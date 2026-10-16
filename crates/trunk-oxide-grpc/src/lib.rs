use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use trunk_recorder_sys::ffi::{TrCallData, TrCallInfo, TrSystem, TrTrunkMessage, TrUnitEventType};
use trunk_recorder_sys::{CxxCallbacks, SystemInfo};

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

fn convert_call_data(cd: &TrCallData) -> proto::CallData {
    proto::CallData {
        talkgroup: cd.talkgroup,
        color_code: cd.color_code,
        patched_talkgroups: cd.patched_talkgroups.clone(),
        call_num: cd.call_num,
        freq: cd.freq,
        freq_error: cd.freq_error,
        source_num: cd.source_num,
        recorder_num: cd.recorder_num,
        signal: trunk_recorder_sys::signal_from_ffi(cd.signal),
        noise: trunk_recorder_sys::signal_from_ffi(cd.noise),
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
        status: i32::from(cd.status.repr),
        segments: Vec::new(),   // populated by caller if applicable
        is_conventional: false, // set by caller
    }
}

fn convert_trunk_messages(messages: &[TrTrunkMessage]) -> Vec<proto::TrunkMessage> {
    use trunk_recorder_sys::ffi::TrMessageType;
    messages
        .iter()
        .map(|m| proto::TrunkMessage {
            message_type: match m.message_type {
                TrMessageType::Grant => proto::MessageType::Grant as i32,
                TrMessageType::Status => proto::MessageType::Status as i32,
                TrMessageType::Update => proto::MessageType::Update as i32,
                TrMessageType::ControlChannel => proto::MessageType::ControlChannel as i32,
                TrMessageType::Registration => proto::MessageType::Registration as i32,
                TrMessageType::Deregistration => proto::MessageType::Deregistration as i32,
                TrMessageType::Affiliation => proto::MessageType::Affiliation as i32,
                TrMessageType::Sysid => proto::MessageType::Sysid as i32,
                TrMessageType::Acknowledge => proto::MessageType::Acknowledge as i32,
                TrMessageType::Location => proto::MessageType::Location as i32,
                TrMessageType::PatchAdd => proto::MessageType::PatchAdd as i32,
                TrMessageType::PatchDelete => proto::MessageType::PatchDelete as i32,
                TrMessageType::DataGrant => proto::MessageType::DataGrant as i32,
                TrMessageType::UuAnsReq => proto::MessageType::UuAnsReq as i32,
                TrMessageType::UuVGrant => proto::MessageType::UuVGrant as i32,
                TrMessageType::UuVUpdate => proto::MessageType::UuVUpdate as i32,
                TrMessageType::InvalidCcMessage => proto::MessageType::InvalidCcMessage as i32,
                TrMessageType::Tdulc => proto::MessageType::Tdulc as i32,
                TrMessageType::Unknown => proto::MessageType::Unknown as i32,
                other => {
                    tracing::warn!("unknown message type: {:?}", other);
                    proto::MessageType::Unknown as i32
                }
            },
            meta: m.meta.clone(),
            freq: m.freq,
            talkgroup: m.talkgroup,
            encrypted: m.encrypted,
            emergency: m.emergency,
            duplex: m.duplex,
            mode: m.mode,
            priority: m.priority,
            tdma_slot: m.tdma_slot,
            phase2_tdma: m.phase2_tdma,
            source: m.source,
            sys_id: m.sys_id,
            rfss: m.sys_rfss,
            site_id: m.sys_site_id,
            nac: m.nac,
            wacn: m.wacn,
            patch_data: Some(proto::PatchData {
                sg: m.patch_data.sg,
                ga1: m.patch_data.ga1,
                ga2: m.patch_data.ga2,
                ga3: m.patch_data.ga3,
            }),
            opcode: m.opcode,
        })
        .collect()
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
    /// `rust_call_id` → audio metadata (set by Rust, read by GR audio callback)
    audio_meta: RwLock<HashMap<u64, AudioMeta>>,
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

    fn call_info_from(&self, info: &TrCallInfo) -> proto::CallInfo {
        let meta = self
            .talkgroups
            .get(&(info.short_name.clone(), info.talkgroup));
        proto::CallInfo {
            talkgroup: info.talkgroup,
            call_num: info.call_num,
            freq: info.freq,
            signal: trunk_recorder_sys::signal_from_ffi(info.signal),
            noise: trunk_recorder_sys::signal_from_ffi(info.noise),
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

    pub fn signal(&self, unit_id: i64, info: &TrCallInfo) {
        self.send_event(proto::plugin_event::Event::Signal(proto::SignalEvent {
            unit_id,
            signaling_type: String::new(),
            signal_type: proto::SignalType::Normal as i32,
            call: Some(self.call_info_from(info)),
            unit_alias: self.unit_alias(&info.short_name, unit_id),
        }));
    }

    /// Register audio metadata for a call. Called when a call starts recording.
    pub fn register_audio_meta(&self, rust_call_id: u64, meta: AudioMeta) {
        self.audio_meta.write().unwrap().insert(rust_call_id, meta);
    }

    /// Remove audio metadata for a call. Called when a call ends.
    pub fn unregister_audio_meta(&self, rust_call_id: u64) {
        self.audio_meta.write().unwrap().remove(&rust_call_id);
    }

    /// Update the frequency for a call (e.g. on retune).
    pub fn update_audio_meta_freq(&self, rust_call_id: u64, freq: f64) {
        if let Some(meta) = self.audio_meta.write().unwrap().get_mut(&rust_call_id) {
            meta.freq = freq;
        }
    }

    pub fn audio_stream(&self, rust_call_id: u64, samples: &[i16]) {
        if rust_call_id == 0 {
            return;
        }
        let meta = {
            let guard = self.audio_meta.read().unwrap();
            match guard.get(&rust_call_id) {
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

    pub fn call_start(&self, info: &TrCallInfo, traceparent: String) {
        let mut call = self.call_info_from(info);
        call.traceparent = traceparent;
        self.send_event(proto::plugin_event::Event::CallStart(
            proto::CallStartEvent { call: Some(call) },
        ));
    }

    pub fn call_end(
        &self,
        call_data: &TrCallData,
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

    pub fn calls_active(&self, infos: &[TrCallInfo]) {
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

    pub fn trunk_message(&self, messages: &[TrTrunkMessage], info: &SystemInfo) {
        self.send_event(proto::plugin_event::Event::TrunkMessage(
            proto::TrunkMessageEvent {
                messages: convert_trunk_messages(messages),
                system: Some(system_info_from(info)),
            },
        ));
    }

    pub fn unit_event(
        &self,
        info: &SystemInfo,
        event_type: TrUnitEventType,
        source_id: i64,
        talkgroup: i64,
    ) {
        let proto_type = match event_type {
            TrUnitEventType::Registration => proto::UnitEventType::Registration,
            TrUnitEventType::Deregistration => proto::UnitEventType::Deregistration,
            TrUnitEventType::Affiliation => proto::UnitEventType::GroupAffiliation,
            TrUnitEventType::Acknowledge => proto::UnitEventType::AcknowledgeResponse,
            TrUnitEventType::Location => proto::UnitEventType::Location,
            TrUnitEventType::DataGrant => proto::UnitEventType::DataGrant,
            TrUnitEventType::AnswerRequest => proto::UnitEventType::AnswerRequest,
            _ => {
                tracing::warn!("unknown unit event type: {:?}", event_type);
                return;
            }
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
        system: &TrSystem,
        radio_id: i64,
        alias: &str,
        source: &str,
        talkgroup_id: i64,
    ) {
        let short_name = trunk_recorder_sys::ffi::tr_system_short_name(system);
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

impl CxxCallbacks for EventBridge {
    fn audio_stream(&self, rust_call_id: u64, samples: &[i16]) {
        Self::audio_stream(self, rust_call_id, samples);
    }

    fn unit_alias_discovered(
        &self,
        system: &TrSystem,
        radio_id: i64,
        alias: &str,
        source: &str,
        talkgroup_id: i64,
    ) {
        Self::unit_alias_discovered(self, system, radio_id, alias, source, talkgroup_id);
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
