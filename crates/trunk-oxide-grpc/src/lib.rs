use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use trunk_radio::{AudioSink, CallData, CallId, CallInfo, SystemInfo, TrunkMessage, UnitEventType};

pub use trunk_oxide_proto as proto;

use proto::trunk_oxide_server::{TrunkOxide, TrunkOxideServer};

// ── Talkgroup metadata for enrichment ──

#[derive(Debug, Clone)]
pub struct TalkgroupMeta {
    pub alpha_tag: String,
    pub tag: String,
    pub group: String,
    /// CSV/TOML-configured talkgroup priority. `None` for conventional
    /// channels (no priority field), which fall back to the FFI value.
    pub priority: Option<i32>,
}

// ── Conversion helpers ──

fn system_info_from(info: &SystemInfo) -> proto::SystemInfo {
    proto::SystemInfo {
        sys_id: info.sys_id,
        short_name: info.short_name.clone(),
        system_type: info.system_type,
        wacn: info.wacn,
        nac: info.nac,
        rfss: info.rfss,
        site_id: info.site_id,
        bandplan: info.bandplan.clone(),
        analog_levels: info.analog_levels,
        digital_levels: info.digital_levels,
        squelch_db: info.squelch_db,
        conversation_mode: info.conversation_mode,
        message_count: info.message_count,
        decode_rate: info.decode_rate,
    }
}

const fn recorder_info(rec_num: i32) -> proto::RecorderInfo {
    proto::RecorderInfo {
        id: rec_num,
        freq_error: 0,
    }
}

fn convert_call_data(cd: &CallData) -> proto::CallData {
    proto::CallData {
        talkgroup: cd.talkgroup,
        color_code: cd.color_code,
        patched_talkgroups: cd.patched_talkgroups.clone(),
        call_num: cd.call_num,
        freq: cd.freq,
        freq_error: cd.freq_error,
        source_num: cd.source_num,
        recorder_num: cd.recorder_num,
        signal: trunk_radio::signal_from_ffi(cd.signal),
        noise: trunk_radio::signal_from_ffi(cd.noise),
        start_time: cd.start_time,
        stop_time: cd.stop_time,
        error_count: cd.error_count,
        spike_count: cd.spike_count,
        encrypted: cd.encrypted,
        emergency: cd.emergency,
        priority: cd.priority,
        mode: cd.mode,
        duplex: cd.duplex,
        short_name: cd.short_name.clone(),
        audio_type: cd.audio_type.clone(),
        tdma_slot: cd.tdma_slot,
        length: cd.length,
        phase2_tdma: cd.phase2_tdma,
        sources: cd
            .transmission_source_list
            .iter()
            .map(|s| proto::CallSource {
                source: s.source,
                position: s.position,
                emergency: s.emergency,
                signal_system: s.signal_system.clone(),
                tag: s.tag.clone(),
            })
            .collect(),
        errors: cd
            .transmission_error_list
            .iter()
            .map(|e| proto::CallError {
                position: e.position,
                total_len: e.total_len,
                error_count: e.error_count,
                spike_count: e.spike_count,
            })
            .collect(),
        transmissions: cd
            .transmission_list
            .iter()
            .map(|t| proto::Transmission {
                source: t.source,
                talkgroup: t.talkgroup,
                slot: t.slot,
                color_code: t.color_code,
                start_time: t.start_time,
                stop_time: t.stop_time,
                sample_count: t.sample_count,
                spike_count: t.spike_count,
                error_count: t.error_count,
                freq: t.freq,
            })
            .collect(),
        status: i32::from(cd.status.repr()),
        segments: Vec::new(),   // populated by caller if applicable
        is_conventional: false, // set by caller
    }
}

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
        K::Acknowledge { group, unit: u } => unit(&mut out, P::Acknowledge, *group, *u),
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
/// This avoids FFI round-trips in the hot audio callback path.
#[derive(Debug, Clone)]
pub struct AudioMeta {
    pub talkgroup: i64,
    pub short_name: Arc<str>,
    pub freq: f64,
    pub sample_rate: i32,
}

/// Bridges trunk-recorder events to gRPC subscribers via a single broadcast channel.
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

    fn call_info_from(&self, info: &CallInfo) -> proto::CallInfo {
        let meta = self
            .talkgroups
            .get(&(info.short_name.clone(), info.talkgroup));
        proto::CallInfo {
            talkgroup: info.talkgroup,
            call_num: info.call_num,
            freq: info.freq,
            signal: trunk_radio::signal_from_ffi(info.signal),
            noise: trunk_radio::signal_from_ffi(info.noise),
            freq_error: info.freq_error,
            encrypted: info.encrypted,
            emergency: info.emergency,
            is_analog: info.is_analog,
            is_conventional: info.is_conventional,
            phase2_tdma: info.phase2_tdma,
            tdma_slot: info.tdma_slot,
            priority: meta.and_then(|m| m.priority).unwrap_or(info.priority),
            mode: info.mode,
            duplex: info.duplex,
            conversation_mode: info.conversation_mode,
            current_source_id: info.current_source_id,
            start_time: Some(prost_types::Timestamp {
                seconds: info.start_time,
                nanos: 0,
            }),
            elapsed: {
                let d = std::time::Duration::from_secs_f64(info.elapsed);
                Some(prost_types::Duration {
                    seconds: d.as_secs().try_into().unwrap_or(i64::MAX),
                    nanos: d.subsec_nanos().try_into().unwrap_or(0),
                })
            },
            short_name: info.short_name.clone(),
            talkgroup_alpha_tag: meta.map_or_else(String::new, |m| m.alpha_tag.clone()),
            talkgroup_tag: meta.map_or_else(String::new, |m| m.tag.clone()),
            talkgroup_group: meta.map_or_else(String::new, |m| m.group.clone()),
            call_state: info.call_state,
            // Populated only by `call_start` (the recorder-side call span's
            // context); empty for CallsActive/Signal snapshots.
            traceparent: String::new(),
        }
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

    pub fn signal(&self, unit_id: i64, info: &CallInfo) {
        self.send_event(proto::plugin_event::Event::Signal(proto::SignalEvent {
            unit_id,
            signaling_type: String::new(),
            signal_type: proto::SignalType::Normal as i32,
            call: Some(self.call_info_from(info)),
            unit_alias: self.unit_alias(&info.short_name, unit_id),
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

    pub fn call_start(&self, info: &CallInfo, traceparent: String) {
        let mut call = self.call_info_from(info);
        call.traceparent = traceparent;
        self.send_event(proto::plugin_event::Event::CallStart(
            proto::CallStartEvent { call: Some(call) },
        ));
    }

    pub fn call_end(
        &self,
        call_data: &CallData,
        segments: &[proto::CallSegment],
        is_conventional: bool,
    ) {
        let mut data = convert_call_data(call_data);
        data.segments = segments.to_vec();
        data.is_conventional = is_conventional;
        self.send_event(proto::plugin_event::Event::CallEnd(proto::CallEndEvent {
            call_data: Some(data),
        }));
    }

    pub fn calls_active(&self, infos: &[CallInfo]) {
        self.send_event(proto::plugin_event::Event::CallsActive(
            proto::CallsActiveEvent {
                calls: infos.iter().map(|i| self.call_info_from(i)).collect(),
            },
        ));
    }

    pub fn setup_recorder(&self, rec_num: i32) {
        self.send_event(proto::plugin_event::Event::SetupRecorder(
            proto::SetupRecorderEvent {
                recorder: Some(recorder_info(rec_num)),
            },
        ));
    }

    pub fn setup_system(&self, info: &SystemInfo) {
        self.send_event(proto::plugin_event::Event::SetupSystem(
            proto::SetupSystemEvent {
                system: Some(system_info_from(info)),
            },
        ));
    }

    pub fn setup_config(&self, infos: &[SystemInfo]) {
        self.send_event(proto::plugin_event::Event::SetupConfig(
            proto::SetupConfigEvent {
                systems: infos.iter().map(system_info_from).collect(),
            },
        ));
    }

    pub fn trunk_message(&self, messages: &[TrunkMessage], info: &SystemInfo) {
        self.send_event(proto::plugin_event::Event::TrunkMessage(
            proto::TrunkMessageEvent {
                messages: messages.iter().map(proto_trunk_message).collect(),
                system: Some(system_info_from(info)),
            },
        ));
    }

    pub fn unit_event(
        &self,
        info: &SystemInfo,
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
        self.send_event(proto::plugin_event::Event::Unit(proto::UnitEvent {
            r#type: proto_type as i32,
            system: Some(system_info_from(info)),
            source_id,
            talkgroup,
            source_alias: self.unit_alias(&info.short_name, source_id),
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
