use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use trunk_radio::{AudioSink, CallId, TrunkMessage, UnitEventType};

pub use trunk_oxide_proto as proto;

use proto::trunk_oxide_server::{TrunkOxide, TrunkOxideServer};

// ── Talkgroup metadata for enrichment ──

#[derive(Debug, Clone)]
pub struct TalkgroupMeta {
    pub alpha_tag: String,
    pub tag: String,
    pub group: String,
    /// CSV/TOML-configured talkgroup priority, for Discord's transcription
    /// order. `None` keeps the call's own.
    pub priority: Option<i32>,
}

// ── Conversion helpers ──

/// A trunk message on the wire: trunk-recorder's flat form, with the fields
/// the message's kind fills in.
#[allow(clippy::too_many_lines)] // one arm per kind
fn proto_trunk_message(m: &TrunkMessage) -> proto::TrunkMessage {
    use proto::MessageType as P;
    use trunk_radio::{Patch, Trunking as K, Voice};
    let mut out = proto::TrunkMessage {
        message_type: P::Unknown as i32,
        meta: String::new(),
        freq: 0.0,
        talkgroup: 0,
        encrypted: false,
        emergency: false,
        duplex: false,
        mode: false,
        priority: 0,
        tdma_slot: 0,
        phase2_tdma: false,
        source: -1,
        sys_id: 0,
        rfss: 0,
        site_id: 0,
        nac: u64::from(m.nac),
        wacn: 0,
        patch_data: None,
        opcode: m.opcode,
    };
    let voice = |out: &mut proto::TrunkMessage, t: P, v: &Voice| {
        out.message_type = t as i32;
        out.freq = v.channel.freq_hz;
        out.phase2_tdma = v.channel.slot.is_some();
        out.tdma_slot = v.channel.slot.map_or(0, i32::from);
        out.talkgroup = i64::from(v.talkgroup);
        out.source = v.source.map_or(-1, i64::from);
        out.emergency = v.options.emergency;
        out.encrypted = v.options.encrypted;
        out.duplex = v.options.duplex;
        out.mode = v.options.packet;
        out.priority = i32::from(v.options.priority);
    };
    let unit = |out: &mut proto::TrunkMessage, t: P, group: u16, unit: u32| {
        out.message_type = t as i32;
        out.talkgroup = i64::from(group);
        out.source = i64::from(unit);
    };
    let patch = |out: &mut proto::TrunkMessage, t: P, p: &Patch| {
        out.message_type = t as i32;
        out.patch_data = Some(proto::PatchData {
            sg: u64::from(p.supergroup),
            ga1: u64::from(p.groups[0]),
            ga2: u64::from(p.groups[1]),
            ga3: u64::from(p.groups[2]),
        });
    };
    match &m.kind {
        K::Grant(v) => voice(&mut out, P::Grant, v),
        K::Update(v) => voice(&mut out, P::Update, v),
        K::UnitGrant(v) => voice(&mut out, P::UuVGrant, v),
        K::UnitUpdate(v) => voice(&mut out, P::UuVUpdate, v),
        K::AnswerRequest(v) => voice(&mut out, P::UuAnsReq, v),
        K::DataGrant {
            freq_hz,
            unit: u,
            options,
        } => {
            out.message_type = P::DataGrant as i32;
            out.freq = *freq_hz;
            out.source = i64::from(*u);
            out.emergency = options.emergency;
            out.encrypted = options.encrypted;
            out.duplex = options.duplex;
            out.mode = options.packet;
        }
        K::Acknowledge { unit: u } => unit(&mut out, P::Acknowledge, 0, *u),
        K::Affiliation { group, unit: u } => unit(&mut out, P::Affiliation, *group, *u),
        K::Location { group, unit: u } => unit(&mut out, P::Location, *group, *u),
        K::Registration { unit: u } => unit(&mut out, P::Registration, 0, *u),
        K::Deregistration { unit: u } => unit(&mut out, P::Deregistration, 0, *u),
        K::ControlChannel { freq_hz } => {
            out.message_type = P::ControlChannel as i32;
            out.freq = *freq_hz;
        }
        K::Site { sys_id, rfss, site } => {
            out.message_type = P::Sysid as i32;
            out.sys_id = u64::from(*sys_id);
            out.rfss = i32::from(*rfss);
            out.site_id = i32::from(*site);
        }
        K::Network {
            freq_hz,
            wacn,
            sys_id,
        } => {
            out.message_type = P::Status as i32;
            out.freq = *freq_hz;
            out.wacn = u64::from(*wacn);
            out.sys_id = u64::from(*sys_id);
        }
        K::PatchAdd(p) => patch(&mut out, P::PatchAdd, p),
        K::PatchDelete(p) => patch(&mut out, P::PatchDelete, p),
        K::Tdulc => out.message_type = P::Tdulc as i32,
        K::Invalid => out.message_type = P::InvalidCcMessage as i32,
        K::Unknown => {}
    }
    out
}

// ── gRPC service implementation ──

struct TrunkOxideService {
    tx: broadcast::Sender<proto::StreamMessage>,
}

#[tonic::async_trait]
impl TrunkOxide for TrunkOxideService {
    type SubscribeStream = ReceiverStream<std::result::Result<proto::StreamMessage, Status>>;

    async fn subscribe(
        &self,
        _request: Request<()>,
    ) -> std::result::Result<Response<Self::SubscribeStream>, Status> {
        tracing::info!("client subscribed to unified stream");
        let mut broadcast_rx = self.tx.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(4096);

        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(msg) => {
                        if tx.send(Ok(msg)).await.is_err() {
                            tracing::info!("subscriber disconnected");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("broadcast channel closed");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ── EventBridge: lock-free event dispatch ──

const DEFAULT_SOCKET_PATH: &str = "/run/service/grpc.sock";

pub struct GrpcConfig {
    pub socket_path: String,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_SOCKET_PATH.to_string(),
        }
    }
}

/// Audio metadata registered by Rust for each active call, keyed by `CallId`.
/// Kept here so the audio path never touches the state machine.
#[derive(Debug, Clone)]
pub struct AudioMeta {
    pub talkgroup: i64,
    pub short_name: Arc<str>,
    pub freq: f64,
    pub sample_rate: i32,
}

/// Bridges the scanner's events to gRPC subscribers via a single broadcast channel.
///
/// All methods take `&self` — no mutex needed. The broadcast sender is
/// inherently `Sync`, talkgroups are read-only after construction, and
/// `unit_aliases`/`audio_meta` use `RwLock` for rare writes.
pub struct EventBridge {
    tx: broadcast::Sender<proto::StreamMessage>,
    /// (`short_name`, `talkgroup_id`) → metadata from config CSVs
    talkgroups: HashMap<(String, i64), TalkgroupMeta>,
    /// (`short_name`, `radio_id`) → OTA-discovered alias
    unit_aliases: RwLock<HashMap<(String, i64), String>>,
    /// Each call's audio metadata, while it is recorded.
    audio_meta: RwLock<HashMap<CallId, AudioMeta>>,
}

impl EventBridge {
    pub fn new(
        talkgroups: HashMap<(String, i64), TalkgroupMeta>,
    ) -> (Self, broadcast::Sender<proto::StreamMessage>) {
        let (tx, _) = broadcast::channel::<proto::StreamMessage>(4096);
        let bridge = Self {
            tx: tx.clone(),
            talkgroups,
            unit_aliases: RwLock::new(HashMap::new()),
            audio_meta: RwLock::new(HashMap::new()),
        };
        (bridge, tx)
    }

    pub fn broadcast_depth(&self) -> usize {
        self.tx.len()
    }

    pub const fn broadcast_capacity(&self) -> usize {
        4096
    }

    /// Fill in what the configuration says about the call's talkgroup.
    fn enrich(&self, mut info: proto::CallInfo) -> proto::CallInfo {
        if let Some(meta) = self
            .talkgroups
            .get(&(info.short_name.clone(), info.talkgroup))
        {
            info.talkgroup_alpha_tag.clone_from(&meta.alpha_tag);
            info.talkgroup_tag.clone_from(&meta.tag);
            info.talkgroup_group.clone_from(&meta.group);
            if let Some(p) = meta.priority {
                info.priority = p;
            }
        }
        info
    }

    fn unit_alias(&self, short_name: &str, unit_id: i64) -> String {
        self.unit_aliases
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(short_name.to_string(), unit_id))
            .cloned()
            .unwrap_or_default()
    }

    fn send_event(&self, event: proto::plugin_event::Event) {
        let msg = proto::StreamMessage {
            message: Some(proto::stream_message::Message::Event(proto::PluginEvent {
                event: Some(event),
            })),
        };
        if let Err(_e) = self.tx.send(msg) {
            tracing::trace!("no subscribers");
        }
    }

    pub fn signal(&self, unit_id: i64, info: proto::CallInfo) {
        let unit_alias = self.unit_alias(&info.short_name, unit_id);
        self.send_event(proto::plugin_event::Event::Signal(proto::SignalEvent {
            unit_id,
            signal_type: proto::SignalType::Normal as i32,
            call: Some(self.enrich(info)),
            unit_alias,
        }));
    }

    /// Register audio metadata for a call. Called when a call starts recording.
    pub fn register_audio_meta(&self, call_id: CallId, meta: AudioMeta) {
        self.audio_meta.write().unwrap().insert(call_id, meta);
    }

    /// Remove audio metadata for a call. Called when a call ends.
    pub fn unregister_audio_meta(&self, call_id: CallId) {
        self.audio_meta.write().unwrap().remove(&call_id);
    }

    /// Update the frequency for a call (e.g. on retune).
    pub fn update_audio_meta_freq(&self, call_id: CallId, freq: f64) {
        if let Some(meta) = self.audio_meta.write().unwrap().get_mut(&call_id) {
            meta.freq = freq;
        }
    }

    pub fn audio_stream(&self, call_id: CallId, samples: &[i16]) {
        let meta = {
            let guard = self.audio_meta.read().unwrap();
            match guard.get(&call_id) {
                Some(m) => m.clone(),
                None => return,
            }
        };
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let msg = proto::AudioSample {
            talkgroup: meta.talkgroup,
            freq: meta.freq,
            samples: bytes,
            sample_rate: meta.sample_rate,
            short_name: meta.short_name.to_string(),
        };
        let stream_msg = proto::StreamMessage {
            message: Some(proto::stream_message::Message::Audio(msg)),
        };
        if let Err(_e) = self.tx.send(stream_msg) {
            tracing::trace!("no audio subscribers");
        }
    }

    /// A call started; `traceparent` is its recorder-side span's.
    pub fn call_start(&self, info: proto::CallInfo, traceparent: String) {
        let call = proto::CallInfo {
            traceparent,
            ..self.enrich(info)
        };
        self.send_event(proto::plugin_event::Event::CallStart(
            proto::CallStartEvent { call: Some(call) },
        ));
    }

    pub fn call_end(&self, call_data: proto::CallData, reason: proto::CallEndReason) {
        self.send_event(proto::plugin_event::Event::CallEnd(proto::CallEndEvent {
            call_data: Some(call_data),
            reason: reason as i32,
        }));
    }

    pub fn calls_active(&self, infos: Vec<proto::CallInfo>) {
        self.send_event(proto::plugin_event::Event::CallsActive(
            proto::CallsActiveEvent {
                calls: infos.into_iter().map(|i| self.enrich(i)).collect(),
            },
        ));
    }

    pub fn setup_system(&self, info: proto::SystemInfo) {
        self.send_event(proto::plugin_event::Event::SetupSystem(
            proto::SetupSystemEvent { system: Some(info) },
        ));
    }

    pub fn setup_config(&self, systems: Vec<proto::SystemInfo>) {
        self.send_event(proto::plugin_event::Event::SetupConfig(
            proto::SetupConfigEvent { systems },
        ));
    }

    pub fn trunk_message(&self, messages: &[TrunkMessage], system: proto::SystemInfo) {
        self.send_event(proto::plugin_event::Event::TrunkMessage(
            proto::TrunkMessageEvent {
                messages: messages.iter().map(proto_trunk_message).collect(),
                system: Some(system),
            },
        ));
    }

    pub fn unit_event(
        &self,
        info: proto::SystemInfo,
        event_type: UnitEventType,
        source_id: i64,
        talkgroup: i64,
    ) {
        let proto_type = match event_type {
            UnitEventType::Registration => proto::UnitEventType::Registration,
            UnitEventType::Deregistration => proto::UnitEventType::Deregistration,
            UnitEventType::Affiliation => proto::UnitEventType::GroupAffiliation,
            UnitEventType::Acknowledge => proto::UnitEventType::AcknowledgeResponse,
            UnitEventType::Location => proto::UnitEventType::Location,
            UnitEventType::DataGrant => proto::UnitEventType::DataGrant,
            UnitEventType::AnswerRequest => proto::UnitEventType::AnswerRequest,
        };
        let source_alias = self.unit_alias(&info.short_name, source_id);
        self.send_event(proto::plugin_event::Event::Unit(proto::UnitEvent {
            r#type: proto_type as i32,
            system: Some(info),
            source_id,
            talkgroup,
            source_alias,
        }));
    }

    pub fn unit_alias_discovered(
        &self,
        short_name: &str,
        radio_id: i64,
        alias: &str,
        source: &str,
        talkgroup_id: i64,
    ) {
        let short_name = short_name.to_owned();
        self.unit_aliases
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((short_name, radio_id), alias.to_string());
        self.send_event(proto::plugin_event::Event::UnitAlias(
            proto::UnitAliasEvent {
                radio_id,
                alias: alias.to_string(),
                source: source.to_string(),
                talkgroup_id,
            },
        ));
    }
}

impl AudioSink for EventBridge {
    fn audio(&self, call_id: CallId, samples: &[i16]) {
        self.audio_stream(call_id, samples);
    }
}

/// Spawn the gRPC server as an async task.
pub async fn serve_grpc(socket_path: String, tx: broadcast::Sender<proto::StreamMessage>) {
    let service = TrunkOxideService { tx };

    let _ = std::fs::remove_file(&socket_path);
    let listener = match tokio::net::UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(path = %socket_path, error = %e, "failed to bind UDS");
            return;
        }
    };
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);

    let (reporter, health_service) = tonic_health::server::health_reporter();
    reporter
        .set_serving::<TrunkOxideServer<TrunkOxideService>>()
        .await;

    tracing::info!(path = %socket_path, "trunk-oxide gRPC server listening");

    if let Err(e) = tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(TrunkOxideServer::new(service))
        .serve_with_incoming(incoming)
        .await
    {
        tracing::error!("gRPC server error: {e}");
    }
}
