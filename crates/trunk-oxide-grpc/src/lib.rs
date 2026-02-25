use tokio::sync::mpsc;
use tonic::transport::Channel;
use trunk_recorder_plugin::{
    Plugin, Result, SignalEvent, TrCall, TrCallData, TrRecorder, TrSource, TrSystem,
    TrTrunkMessage,
};

mod proto {
    tonic::include_proto!("trunk_oxide");
}

use proto::trunk_recorder_plugin_client::TrunkRecorderPluginClient;

// ── Conversion helpers ──

fn call_info(call: &TrCall) -> proto::CallInfo {
    use trunk_recorder_sys::ffi::*;
    proto::CallInfo {
        talkgroup: call_get_talkgroup(call),
        call_num: call_get_call_num(call),
        sys_num: call_get_sys_num(call),
        freq: call_get_freq(call),
        signal: call_get_signal(call),
        noise: call_get_noise(call),
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
        short_name: call_get_short_name(call),
        talkgroup_display: call_get_talkgroup_display(call),
        talkgroup_tag: call_get_talkgroup_tag(call),
        state: call_get_state(call).repr as i32,
        monitoring_state: call_get_monitoring_state(call).repr as i32,
    }
}

fn system_info(sys: &TrSystem) -> proto::SystemInfo {
    use trunk_recorder_sys::ffi::*;
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
        compress_wav: system_get_compress_wav(sys),
        conversation_mode: system_get_conversation_mode(sys),
        message_count: system_get_message_count(sys),
        decode_rate: system_get_decode_rate(sys),
    }
}

fn recorder_info(rec: &TrRecorder) -> proto::RecorderInfo {
    use trunk_recorder_sys::ffi::*;
    proto::RecorderInfo {
        id: recorder_get_num(rec),
        r#type: recorder_get_type_string(rec),
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

fn source_info(src: &TrSource) -> proto::SourceInfo {
    use trunk_recorder_sys::ffi::*;
    proto::SourceInfo {
        src_num: source_get_num(src),
        center_freq: source_get_center(src),
        rate: source_get_rate(src),
        min_hz: source_get_min_hz(src),
        max_hz: source_get_max_hz(src),
        driver: source_get_driver(src),
        device: source_get_device(src),
        antenna: source_get_antenna(src),
        if_gain: source_get_if_gain(src),
        lna_gain: source_get_lna_gain(src),
        pga_gain: source_get_pga_gain(src),
        vga1_gain: source_get_vga1_gain(src),
        vga2_gain: source_get_vga2_gain(src),
        analog_recorders: source_get_analog_recorder_count(src),
        digital_recorders: source_get_digital_recorder_count(src),
        debug_recorders: source_get_debug_recorder_count(src),
        sigmf_recorders: source_get_sigmf_recorder_count(src),
    }
}

fn convert_call_data(cd: &TrCallData) -> proto::CallData {
    proto::CallData {
        talkgroup: cd.talkgroup,
        color_code: cd.color_code,
        patched_talkgroups: cd.patched_talkgroups.clone(),
        talkgroup_tag: cd.talkgroup_tag.clone(),
        talkgroup_alpha_tag: cd.talkgroup_alpha_tag.clone(),
        talkgroup_description: cd.talkgroup_description.clone(),
        talkgroup_display: cd.talkgroup_display.clone(),
        talkgroup_group: cd.talkgroup_group.clone(),
        call_num: cd.call_num,
        freq: cd.freq,
        freq_error: cd.freq_error,
        source_num: cd.source_num,
        recorder_num: cd.recorder_num,
        signal: cd.signal,
        noise: cd.noise,
        start_time: cd.start_time,
        stop_time: cd.stop_time,
        error_count: cd.error_count,
        spike_count: cd.spike_count,
        encrypted: cd.encrypted,
        emergency: cd.emergency,
        priority: cd.priority,
        mode: cd.mode,
        duplex: cd.duplex,
        audio_archive: cd.audio_archive,
        transmission_archive: cd.transmission_archive,
        call_log: cd.call_log,
        compress_wav: cd.compress_wav,
        filename: cd.filename.clone(),
        status_filename: cd.status_filename.clone(),
        converted: cd.converted.clone(),
        sys_num: cd.sys_num,
        short_name: cd.short_name.clone(),
        upload_script: cd.upload_script.clone(),
        audio_type: cd.audio_type.clone(),
        tdma_slot: cd.tdma_slot,
        length: cd.length,
        phase2_tdma: cd.phase2_tdma,
        sources: cd
            .transmission_source_list
            .iter()
            .map(|s| proto::CallSource {
                source: s.source,
                time: s.time,
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
                time: e.time,
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
                filename: t.filename.clone(),
            })
            .collect(),
        status: cd.status.repr as i32,
        process_call_time: cd.process_call_time,
        retry_attempt: cd.retry_attempt,
    }
}

fn convert_signal_type(
    st: trunk_recorder_plugin::TrSignalType,
) -> i32 {
    use trunk_recorder_plugin::TrSignalType;
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
    use trunk_recorder_plugin::TrMessageType;
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

// ── Plugin implementation ──

#[derive(Default)]
struct GrpcConfig {
    server_address: String,
}

struct GrpcPlugin {
    config: GrpcConfig,
    runtime: Option<tokio::runtime::Runtime>,
    audio_tx: Option<mpsc::Sender<proto::AudioSample>>,
    event_tx: Option<mpsc::Sender<proto::PluginEvent>>,
}

impl GrpcPlugin {
    fn new() -> Self {
        Self {
            config: GrpcConfig::default(),
            runtime: None,
            audio_tx: None,
            event_tx: None,
        }
    }

    fn send_event(&self, event: proto::plugin_event::Event) {
        if let Some(tx) = &self.event_tx {
            let msg = proto::PluginEvent { event: Some(event) };
            if tx.try_send(msg).is_err() {
                tracing::warn!("event channel full or closed, dropping event");
            }
        }
    }

    fn send_unit_event(
        &self,
        event_type: proto::UnitEventType,
        system: &TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) {
        self.send_event(proto::plugin_event::Event::Unit(proto::UnitEvent {
            r#type: event_type as i32,
            system: Some(system_info(system)),
            source_id,
            talkgroup,
        }));
    }
}

impl Plugin for GrpcPlugin {
    fn parse_config(&mut self, config: &serde_json::Value) -> Result {
        self.config.server_address = config
            .get("server")
            .and_then(|v| v.as_str())
            .unwrap_or("http://127.0.0.1:50051")
            .to_string();
        Ok(())
    }

    fn start(&mut self) -> Result {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;

        let (audio_tx, audio_rx) = mpsc::channel::<proto::AudioSample>(4096);
        let (event_tx, event_rx) = mpsc::channel::<proto::PluginEvent>(4096);

        let addr = self.config.server_address.clone();

        rt.spawn(async move {
            if let Err(e) = run_streams(addr, audio_rx, event_rx).await {
                tracing::error!("gRPC stream error: {e}");
            }
        });

        self.audio_tx = Some(audio_tx);
        self.event_tx = Some(event_tx);
        self.runtime = Some(rt);

        Ok(())
    }

    fn stop(&mut self) -> Result {
        // Drop senders to close streams
        self.audio_tx.take();
        self.event_tx.take();

        // Shutdown the runtime
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(5));
        }
        Ok(())
    }

    fn signal(&mut self, event: SignalEvent<'_>) -> Result {
        self.send_event(proto::plugin_event::Event::Signal(proto::SignalEvent {
            unit_id: event.unit_id,
            signaling_type: event.signaling_type.to_string(),
            signal_type: convert_signal_type(event.signal_type),
            call: Some(call_info(event.call)),
            system: Some(system_info(event.system)),
            recorder: Some(recorder_info(event.recorder)),
        }));
        Ok(())
    }

    fn audio_stream(
        &mut self,
        call: &TrCall,
        recorder: &TrRecorder,
        samples: &[i16],
    ) -> Result {
        use trunk_recorder_sys::ffi::*;
        if let Some(tx) = &self.audio_tx {
            // Convert i16 samples to bytes (little-endian)
            let bytes: Vec<u8> = samples
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            let msg = proto::AudioSample {
                talkgroup: call_get_talkgroup(call),
                sys_num: call_get_sys_num(call),
                freq: call_get_freq(call),
                samples: bytes,
                sample_rate: recorder_get_wav_hz(recorder) as i32,
            };
            if tx.try_send(msg).is_err() {
                tracing::warn!("audio channel full or closed, dropping samples");
            }
        }
        Ok(())
    }

    fn call_start(&mut self, call: &TrCall) -> Result {
        self.send_event(proto::plugin_event::Event::CallStart(
            proto::CallStartEvent {
                call: Some(call_info(call)),
            },
        ));
        Ok(())
    }

    fn call_end(&mut self, call_data: &TrCallData) -> Result {
        self.send_event(proto::plugin_event::Event::CallEnd(proto::CallEndEvent {
            call_data: Some(convert_call_data(call_data)),
        }));
        Ok(())
    }

    fn calls_active(&mut self, calls: &[&TrCall]) -> Result {
        self.send_event(proto::plugin_event::Event::CallsActive(
            proto::CallsActiveEvent {
                calls: calls.iter().map(|c| call_info(c)).collect(),
            },
        ));
        Ok(())
    }

    fn setup_recorder(&mut self, recorder: &TrRecorder) -> Result {
        self.send_event(proto::plugin_event::Event::SetupRecorder(
            proto::SetupRecorderEvent {
                recorder: Some(recorder_info(recorder)),
            },
        ));
        Ok(())
    }

    fn setup_system(&mut self, system: &TrSystem) -> Result {
        self.send_event(proto::plugin_event::Event::SetupSystem(
            proto::SetupSystemEvent {
                system: Some(system_info(system)),
            },
        ));
        Ok(())
    }

    fn setup_systems(&mut self, systems: &[&TrSystem]) -> Result {
        self.send_event(proto::plugin_event::Event::SetupSystems(
            proto::SetupSystemsEvent {
                systems: systems.iter().map(|s| system_info(s)).collect(),
            },
        ));
        Ok(())
    }

    fn setup_sources(&mut self, sources: &[&TrSource]) -> Result {
        self.send_event(proto::plugin_event::Event::SetupSources(
            proto::SetupSourcesEvent {
                sources: sources.iter().map(|s| source_info(s)).collect(),
            },
        ));
        Ok(())
    }

    fn setup_config(&mut self, sources: &[&TrSource], systems: &[&TrSystem]) -> Result {
        self.send_event(proto::plugin_event::Event::SetupConfig(
            proto::SetupConfigEvent {
                sources: sources.iter().map(|s| source_info(s)).collect(),
                systems: systems.iter().map(|s| system_info(s)).collect(),
            },
        ));
        Ok(())
    }

    fn trunk_message(&mut self, messages: &[TrTrunkMessage], system: &TrSystem) -> Result {
        self.send_event(proto::plugin_event::Event::TrunkMessage(
            proto::TrunkMessageEvent {
                messages: convert_trunk_messages(messages),
                system: Some(system_info(system)),
            },
        ));
        Ok(())
    }

    fn system_rates(&mut self, systems: &[&TrSystem], time_diff: f32) -> Result {
        self.send_event(proto::plugin_event::Event::SystemRates(
            proto::SystemRatesEvent {
                systems: systems.iter().map(|s| system_info(s)).collect(),
                time_diff,
            },
        ));
        Ok(())
    }

    fn unit_registration(&mut self, system: &TrSystem, source_id: i64) -> Result {
        self.send_unit_event(proto::UnitEventType::Registration, system, source_id, 0);
        Ok(())
    }

    fn unit_deregistration(&mut self, system: &TrSystem, source_id: i64) -> Result {
        self.send_unit_event(proto::UnitEventType::Deregistration, system, source_id, 0);
        Ok(())
    }

    fn unit_acknowledge_response(&mut self, system: &TrSystem, source_id: i64) -> Result {
        self.send_unit_event(
            proto::UnitEventType::AcknowledgeResponse,
            system,
            source_id,
            0,
        );
        Ok(())
    }

    fn unit_group_affiliation(
        &mut self,
        system: &TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) -> Result {
        self.send_unit_event(
            proto::UnitEventType::GroupAffiliation,
            system,
            source_id,
            talkgroup,
        );
        Ok(())
    }

    fn unit_data_grant(&mut self, system: &TrSystem, source_id: i64) -> Result {
        self.send_unit_event(proto::UnitEventType::DataGrant, system, source_id, 0);
        Ok(())
    }

    fn unit_answer_request(
        &mut self,
        system: &TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) -> Result {
        self.send_unit_event(
            proto::UnitEventType::AnswerRequest,
            system,
            source_id,
            talkgroup,
        );
        Ok(())
    }

    fn unit_location(
        &mut self,
        system: &TrSystem,
        source_id: i64,
        talkgroup: i64,
    ) -> Result {
        self.send_unit_event(proto::UnitEventType::Location, system, source_id, talkgroup);
        Ok(())
    }
}

// ── Background stream tasks ──

async fn run_streams(
    addr: String,
    audio_rx: mpsc::Receiver<proto::AudioSample>,
    event_rx: mpsc::Receiver<proto::PluginEvent>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let channel = Channel::from_shared(addr)?.connect().await?;
    let mut client = TrunkRecorderPluginClient::new(channel.clone());
    let mut audio_client = TrunkRecorderPluginClient::new(channel);

    let audio_handle = tokio::spawn(async move {
        let stream = tokio_stream::wrappers::ReceiverStream::new(audio_rx);
        if let Err(e) = audio_client.stream_audio(stream).await {
            tracing::error!("StreamAudio error: {e}");
        }
    });

    let event_handle = tokio::spawn(async move {
        let stream = tokio_stream::wrappers::ReceiverStream::new(event_rx);
        if let Err(e) = client.stream_events(stream).await {
            tracing::error!("StreamEvents error: {e}");
        }
    });

    let _ = tokio::try_join!(audio_handle, event_handle);
    Ok(())
}

// ── Plugin export ──

trunk_recorder_plugin::export_plugin!(GrpcPlugin::new());
