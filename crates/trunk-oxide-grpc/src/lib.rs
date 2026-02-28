use std::collections::HashMap;

use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use trunk_recorder_sys::ffi::{
    TrCall, TrCallData, TrRecorder, TrSignalType, TrSystem, TrTrunkMessage,
};
use trunk_recorder_sys::RustPluginCallbacks;

pub use trunk_oxide_proto as proto;

use proto::trunk_oxide_server::{TrunkOxide, TrunkOxideServer};

// ── Talkgroup metadata for enrichment ──

#[derive(Debug, Clone)]
pub struct TalkgroupMeta {
    pub alpha_tag: String,
    pub tag: String,
    pub group: String,
}

// ── Conversion helpers ──

fn system_info(sys: &TrSystem) -> proto::SystemInfo {
    use trunk_recorder_sys::ffi::{
        system_get_analog_levels, system_get_bandplan,
        system_get_conversation_mode, system_get_decode_rate, system_get_digital_levels,
        system_get_message_count, system_get_nac, system_get_short_name, system_get_squelch_db,
        system_get_sys_id, system_get_sys_num, system_get_sys_rfss, system_get_sys_site_id,
        system_get_system_type, system_get_wacn,
    };
    proto::SystemInfo {
        sys_num: system_get_sys_num(sys),
        sys_id: system_get_sys_id(sys),
        short_name: system_get_short_name(sys),
        system_type: system_get_system_type(sys),
        wacn: system_get_wacn(sys),
        nac: system_get_nac(sys),
        rfss: system_get_sys_rfss(sys),
        site_id: system_get_sys_site_id(sys),
        bandplan: system_get_bandplan(sys),
        analog_levels: system_get_analog_levels(sys),
        digital_levels: system_get_digital_levels(sys),
        squelch_db: system_get_squelch_db(sys),
        conversation_mode: system_get_conversation_mode(sys),
        message_count: system_get_message_count(sys),
        decode_rate: system_get_decode_rate(sys),
    }
}

fn recorder_info(rec: &TrRecorder) -> proto::RecorderInfo {
    use trunk_recorder_sys::ffi::{
        recorder_get_current_length, recorder_get_freq, recorder_get_freq_error, recorder_get_num,
        recorder_get_talkgroup, recorder_get_wav_hz, recorder_is_active, recorder_is_analog,
        recorder_is_conventional, recorder_is_idle,
    };
    proto::RecorderInfo {
        id: recorder_get_num(rec),
        r#type: String::new(),
        freq: recorder_get_freq(rec),
        freq_error: recorder_get_freq_error(rec),
        talkgroup: recorder_get_talkgroup(rec),
        wav_hz: recorder_get_wav_hz(rec),
        is_analog: recorder_is_analog(rec),
        is_conventional: recorder_is_conventional(rec),
        is_active: recorder_is_active(rec),
        is_idle: recorder_is_idle(rec),
        current_length: recorder_get_current_length(rec),
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
        signal: if cd.signal >= 998.0 { None } else { Some(cd.signal) },
        noise: if cd.noise >= 998.0 { None } else { Some(cd.noise) },
        start_time: cd.start_time,
        stop_time: cd.stop_time,
        error_count: cd.error_count,
        spike_count: cd.spike_count,
        encrypted: cd.encrypted,
        emergency: cd.emergency,
        priority: cd.priority,
        mode: cd.mode,
        duplex: cd.duplex,
        sys_num: cd.sys_num,
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
                length: t.length,
            })
            .collect(),
        status: i32::from(cd.status.repr),
    }
}

fn convert_signal_type(st: TrSignalType) -> i32 {
    match st {
        TrSignalType::Normal => proto::SignalType::Normal as i32,
        TrSignalType::Emergency => proto::SignalType::Emergency as i32,
        TrSignalType::EmergencyAck => proto::SignalType::EmergencyAck as i32,
        TrSignalType::RadioCheck => proto::SignalType::RadioCheck as i32,
        TrSignalType::RadioCheckAck => proto::SignalType::RadioCheckAck as i32,
        TrSignalType::RadioStun => proto::SignalType::RadioStun as i32,
        TrSignalType::RadioStunAck => proto::SignalType::RadioStunAck as i32,
        TrSignalType::RadioRevive => proto::SignalType::RadioRevive as i32,
        TrSignalType::RadioReviveAck => proto::SignalType::RadioReviveAck as i32,
        TrSignalType::NormalPre => proto::SignalType::NormalPre as i32,
        TrSignalType::EmergencyPre => proto::SignalType::EmergencyPre as i32,
        other => {
            tracing::warn!("unknown signal type: {:?}", other);
            proto::SignalType::Normal as i32
        }
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
            sys_num: m.sys_num,
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

/// Convert a slice of raw pointers to a vec of references.
///
/// # Safety
/// Pointers must be valid for the duration of the call.
#[allow(unsafe_code)]
fn ptr_slice_to_refs<T>(ptrs: &[*const T]) -> Vec<&T> {
    ptrs.iter().map(|p| unsafe { &**p }).collect()
}

// ── gRPC service implementation ──

struct TrunkOxideService {
    audio_tx: broadcast::Sender<proto::AudioSample>,
    event_tx: broadcast::Sender<proto::PluginEvent>,
}

#[tonic::async_trait]
impl TrunkOxide for TrunkOxideService {
    type SubscribeEventsStream = ReceiverStream<std::result::Result<proto::PluginEvent, Status>>;
    type SubscribeAudioStream = ReceiverStream<std::result::Result<proto::AudioSample, Status>>;

    async fn subscribe_events(
        &self,
        _request: Request<()>,
    ) -> std::result::Result<Response<Self::SubscribeEventsStream>, Status> {
        tracing::info!("client subscribed to events");
        let mut broadcast_rx = self.event_tx.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(4096);

        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event)).await.is_err() {
                            tracing::info!("event subscriber disconnected");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "event subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::warn!("event broadcast channel closed");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn subscribe_audio(
        &self,
        _request: Request<()>,
    ) -> std::result::Result<Response<Self::SubscribeAudioStream>, Status> {
        tracing::info!("client subscribed to audio");
        let mut broadcast_rx = self.audio_tx.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(4096);

        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(sample) => {
                        if tx.send(Ok(sample)).await.is_err() {
                            tracing::info!("audio subscriber disconnected");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "audio subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::warn!("audio broadcast channel closed");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ── Plugin implementation ──

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

pub struct GrpcPlugin {
    config: GrpcConfig,
    runtime: Option<tokio::runtime::Runtime>,
    audio_tx: Option<broadcast::Sender<proto::AudioSample>>,
    event_tx: Option<broadcast::Sender<proto::PluginEvent>>,
    /// (short_name, talkgroup_id) → metadata from config CSVs
    talkgroups: HashMap<(String, i64), TalkgroupMeta>,
    /// (sys_num, radio_id) → OTA-discovered alias
    unit_aliases: HashMap<(i32, i64), String>,
}

impl GrpcPlugin {
    pub fn new() -> Self {
        Self {
            config: GrpcConfig::default(),
            runtime: None,
            audio_tx: None,
            event_tx: None,
            talkgroups: HashMap::new(),
            unit_aliases: HashMap::new(),
        }
    }

    /// Populate talkgroup metadata lookup from config data.
    pub fn set_talkgroups(&mut self, talkgroups: HashMap<(String, i64), TalkgroupMeta>) {
        self.talkgroups = talkgroups;
    }

    fn call_info(&self, call: &TrCall) -> proto::CallInfo {
        use trunk_recorder_sys::ffi::{
            call_elapsed, call_get_call_num, call_get_conversation_mode, call_get_current_length,
            call_get_current_source_id, call_get_duplex, call_get_emergency, call_get_encrypted,
            call_get_freq, call_get_freq_error, call_get_is_analog, call_get_mode,
            call_get_monitoring_state, call_get_noise, call_get_phase2_tdma, call_get_priority,
            call_get_short_name, call_get_signal, call_get_start_time, call_get_state,
            call_get_sys_num, call_get_talkgroup, call_get_tdma_slot, call_is_conventional,
        };
        let short_name = call_get_short_name(call);
        let talkgroup = call_get_talkgroup(call);
        let meta = self.talkgroups.get(&(short_name.clone(), talkgroup));
        proto::CallInfo {
            talkgroup,
            call_num: call_get_call_num(call),
            sys_num: call_get_sys_num(call),
            freq: call_get_freq(call),
            signal: { let s = call_get_signal(call); if s >= 998.0 { None } else { Some(s) } },
            noise: { let n = call_get_noise(call); if n >= 998.0 { None } else { Some(n) } },
            freq_error: call_get_freq_error(call),
            encrypted: call_get_encrypted(call),
            emergency: call_get_emergency(call),
            is_analog: call_get_is_analog(call),
            is_conventional: call_is_conventional(call),
            phase2_tdma: call_get_phase2_tdma(call),
            tdma_slot: call_get_tdma_slot(call),
            priority: call_get_priority(call),
            mode: call_get_mode(call),
            duplex: call_get_duplex(call),
            conversation_mode: call_get_conversation_mode(call),
            current_source_id: call_get_current_source_id(call),
            current_length: call_get_current_length(call),
            start_time: call_get_start_time(call),
            elapsed: call_elapsed(call),
            short_name,
            talkgroup_alpha_tag: meta.map_or_else(String::new, |m| m.alpha_tag.clone()),
            talkgroup_tag: meta.map_or_else(String::new, |m| m.tag.clone()),
            talkgroup_group: meta.map_or_else(String::new, |m| m.group.clone()),
            state: i32::from(call_get_state(call).repr),
            monitoring_state: i32::from(call_get_monitoring_state(call).repr),
        }
    }

    fn unit_alias(&self, sys_num: i32, unit_id: i64) -> String {
        self.unit_aliases
            .get(&(sys_num, unit_id))
            .cloned()
            .unwrap_or_default()
    }

    fn send_event(&self, event: proto::plugin_event::Event) {
        if let Some(tx) = &self.event_tx {
            let msg = proto::PluginEvent { event: Some(event) };
            let _ = tx.send(msg);
        }
    }

    fn send_unit_event(
        &self,
        event_type: proto::UnitEventType,
        system: &TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) {
        use trunk_recorder_sys::ffi::system_get_sys_num;
        let sys_num = system_get_sys_num(system);
        self.send_event(proto::plugin_event::Event::Unit(proto::UnitEvent {
            r#type: event_type as i32,
            system: Some(system_info(system)),
            source_id,
            talkgroup,
            source_alias: self.unit_alias(sys_num, source_id),
        }));
    }
}

impl RustPluginCallbacks for GrpcPlugin {
    fn start(&mut self) -> i32 {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("trunk-oxide gRPC plugin start error: {e}");
                return -1;
            }
        };

        let (audio_tx, _) = broadcast::channel::<proto::AudioSample>(4096);
        let (event_tx, _) = broadcast::channel::<proto::PluginEvent>(4096);

        let service = TrunkOxideService {
            audio_tx: audio_tx.clone(),
            event_tx: event_tx.clone(),
        };

        let socket_path = self.config.socket_path.clone();

        rt.spawn(async move {
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
        });

        self.audio_tx = Some(audio_tx);
        self.event_tx = Some(event_tx);
        self.runtime = Some(rt);

        0
    }

    fn stop(&mut self) -> i32 {
        self.audio_tx.take();
        self.event_tx.take();

        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(5));
        }
        0
    }

    fn poll_one(&mut self) -> i32 {
        0
    }

    fn signal(
        &mut self,
        unit_id: i64,
        signaling_type: &str,
        sig_type: TrSignalType,
        call: Option<&TrCall>,
        system: Option<&TrSystem>,
        recorder: Option<&TrRecorder>,
    ) -> i32 {
        use trunk_recorder_sys::ffi::system_get_sys_num;
        let sys_num = system.map_or(0, system_get_sys_num);
        self.send_event(proto::plugin_event::Event::Signal(proto::SignalEvent {
            unit_id,
            signaling_type: signaling_type.to_string(),
            signal_type: convert_signal_type(sig_type),
            call: call.map(|c| self.call_info(c)),
            system: system.map(system_info),
            recorder: recorder.map(recorder_info),
            unit_alias: self.unit_alias(sys_num, unit_id),
        }));
        0
    }

    fn audio_stream(&mut self, call: Option<&TrCall>, recorder: &TrRecorder, samples: &[i16]) -> i32 {
        use trunk_recorder_sys::ffi::{
            call_get_freq, call_get_sys_num, call_get_talkgroup, recorder_get_wav_hz,
        };
        let Some(call) = call else {
            return 0;
        };
        if let Some(tx) = &self.audio_tx {
            let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            let sample_rate_i64 = recorder_get_wav_hz(recorder);
            let Ok(sample_rate) = i32::try_from(sample_rate_i64) else {
                tracing::warn!(
                    sample_rate = sample_rate_i64,
                    "sample rate outside i32 range; dropping audio chunk"
                );
                return 0;
            };
            let msg = proto::AudioSample {
                talkgroup: call_get_talkgroup(call),
                sys_num: call_get_sys_num(call),
                freq: call_get_freq(call),
                samples: bytes,
                sample_rate,
            };
            let _ = tx.send(msg);
        }
        0
    }

    fn call_start(&mut self, call: &TrCall) -> i32 {
        self.send_event(proto::plugin_event::Event::CallStart(
            proto::CallStartEvent {
                call: Some(self.call_info(call)),
            },
        ));
        0
    }

    fn call_end(&mut self, call_data: &TrCallData) -> i32 {
        self.send_event(proto::plugin_event::Event::CallEnd(proto::CallEndEvent {
            call_data: Some(convert_call_data(call_data)),
        }));
        0
    }

    fn calls_active(&mut self, calls: &[*const TrCall]) -> i32 {
        let refs = ptr_slice_to_refs(calls);
        self.send_event(proto::plugin_event::Event::CallsActive(
            proto::CallsActiveEvent {
                calls: refs.iter().map(|c| self.call_info(c)).collect(),
            },
        ));
        0
    }

    fn setup_recorder(&mut self, recorder: &TrRecorder) -> i32 {
        self.send_event(proto::plugin_event::Event::SetupRecorder(
            proto::SetupRecorderEvent {
                recorder: Some(recorder_info(recorder)),
            },
        ));
        0
    }

    fn setup_system(&mut self, system: &TrSystem) -> i32 {
        self.send_event(proto::plugin_event::Event::SetupSystem(
            proto::SetupSystemEvent {
                system: Some(system_info(system)),
            },
        ));
        0
    }

    fn setup_systems(&mut self, systems: &[*const TrSystem]) -> i32 {
        let refs = ptr_slice_to_refs(systems);
        self.send_event(proto::plugin_event::Event::SetupSystems(
            proto::SetupSystemsEvent {
                systems: refs.iter().map(|s| system_info(s)).collect(),
            },
        ));
        0
    }

    fn setup_sources(&mut self) -> i32 {
        self.send_event(proto::plugin_event::Event::SetupSources(
            proto::SetupSourcesEvent {
                sources: vec![],
            },
        ));
        0
    }

    fn setup_config(&mut self, systems: &[*const TrSystem]) -> i32 {
        let refs = ptr_slice_to_refs(systems);
        self.send_event(proto::plugin_event::Event::SetupConfig(
            proto::SetupConfigEvent {
                sources: vec![],
                systems: refs.iter().map(|s| system_info(s)).collect(),
            },
        ));
        0
    }

    fn trunk_message(&mut self, messages: &[TrTrunkMessage], system: &TrSystem) -> i32 {
        self.send_event(proto::plugin_event::Event::TrunkMessage(
            proto::TrunkMessageEvent {
                messages: convert_trunk_messages(messages),
                system: Some(system_info(system)),
            },
        ));
        0
    }

    fn system_rates(&mut self, systems: &[*const TrSystem], time_diff: f32) -> i32 {
        let refs = ptr_slice_to_refs(systems);
        self.send_event(proto::plugin_event::Event::SystemRates(
            proto::SystemRatesEvent {
                systems: refs.iter().map(|s| system_info(s)).collect(),
                time_diff,
            },
        ));
        0
    }

    fn unit_registration(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        self.send_unit_event(proto::UnitEventType::Registration, system, source_id, 0);
        0
    }

    fn unit_deregistration(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        self.send_unit_event(proto::UnitEventType::Deregistration, system, source_id, 0);
        0
    }

    fn unit_acknowledge_response(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        self.send_unit_event(
            proto::UnitEventType::AcknowledgeResponse,
            system,
            source_id,
            0,
        );
        0
    }

    fn unit_group_affiliation(
        &mut self,
        system: &TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) -> i32 {
        self.send_unit_event(
            proto::UnitEventType::GroupAffiliation,
            system,
            source_id,
            talkgroup,
        );
        0
    }

    fn unit_data_grant(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        self.send_unit_event(proto::UnitEventType::DataGrant, system, source_id, 0);
        0
    }

    fn unit_answer_request(&mut self, system: &TrSystem, source_id: i64, talkgroup: i64) -> i32 {
        self.send_unit_event(
            proto::UnitEventType::AnswerRequest,
            system,
            source_id,
            talkgroup,
        );
        0
    }

    fn unit_location(&mut self, system: &TrSystem, source_id: i64, talkgroup: i64) -> i32 {
        self.send_unit_event(proto::UnitEventType::Location, system, source_id, talkgroup);
        0
    }

    fn unit_alias_discovered(
        &mut self,
        system: &TrSystem,
        radio_id: i64,
        alias: &str,
        source: &str,
        talkgroup_id: i64,
    ) -> i32 {
        use trunk_recorder_sys::ffi::system_get_sys_num;
        let sys_num = system_get_sys_num(system);
        self.unit_aliases
            .insert((sys_num, radio_id), alias.to_string());
        self.send_event(proto::plugin_event::Event::UnitAlias(
            proto::UnitAliasEvent {
                system: Some(system_info(system)),
                radio_id,
                alias: alias.to_string(),
                source: source.to_string(),
                talkgroup_id,
            },
        ));
        0
    }
}
