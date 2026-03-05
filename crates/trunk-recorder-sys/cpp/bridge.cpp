// Our bridge header (includes trunk-recorder headers and rust/cxx.h)
#include "trunk-recorder-sys/cpp/bridge.h"

// cxx-generated header for the Rust bridge
#include "trunk-recorder-sys/src/lib.rs.h"

#include <cstdlib>
#include <cstring>
#include <iostream>
#include <map>

#include "trunk-recorder/trunk_context.h"
#include "trunk-recorder/call_conventional.h"
#include "trunk-recorder/event_sink.h"
#include "trunk-recorder/formatter.h"
#include "trunk-recorder/recorders/dmr_recorder.h"
#include "trunk-recorder/source.h"
#include "trunk-recorder/systems/p25_parser.h"
#include "trunk-recorder/systems/smartnet_parser.h"
#include "trunk-recorder/systems/system.h"
#include "trunk-recorder/talkgroup.h"
#include "trunk-recorder/unit_tags_ota.h"
#include "trunk-recorder/autotune.h"
#include "trunk-recorder/recorders/p25_recorder.h"

#include "trunk-recorder/monitor_systems.h"

// Forward declarations for functions provided by libtrunk-recorder.so
bool setup_systems(Config &config, gr::top_block_sptr &tb,
                   std::vector<std::shared_ptr<Source>> &sources, std::vector<std::shared_ptr<System>> &systems,
                   std::vector<std::shared_ptr<Call>> &calls);

namespace trunk_oxide {

// ── Enum conversion helpers ──

static TrMessageType convert_message_type(MessageType mt) {
    switch (mt) {
        case GRANT: return TrMessageType::Grant;
        case STATUS: return TrMessageType::Status;
        case UPDATE: return TrMessageType::Update;
        case CONTROL_CHANNEL: return TrMessageType::ControlChannel;
        case REGISTRATION: return TrMessageType::Registration;
        case DEREGISTRATION: return TrMessageType::Deregistration;
        case AFFILIATION: return TrMessageType::Affiliation;
        case SYSID: return TrMessageType::Sysid;
        case ACKNOWLEDGE: return TrMessageType::Acknowledge;
        case LOCATION: return TrMessageType::Location;
        case PATCH_ADD: return TrMessageType::PatchAdd;
        case PATCH_DELETE: return TrMessageType::PatchDelete;
        case DATA_GRANT: return TrMessageType::DataGrant;
        case UU_ANS_REQ: return TrMessageType::UuAnsReq;
        case UU_V_GRANT: return TrMessageType::UuVGrant;
        case UU_V_UPDATE: return TrMessageType::UuVUpdate;
        case INVALID_CC_MESSAGE: return TrMessageType::InvalidCcMessage;
        case TDULC: return TrMessageType::Tdulc;
        default: return TrMessageType::Unknown;
    }
}

static TrState convert_state(CallState s) {
    switch (s) {
        case MONITORING: return TrState::Monitoring;
        case RECORDING: return TrState::Recording;
        default: return TrState::Monitoring;
    }
}

static TrMonitoringState convert_monitoring_state(MonitoringState ms) {
    switch (ms) {
        case UNSPECIFIED: return TrMonitoringState::Unspecified;
        case UNKNOWN_TG: return TrMonitoringState::UnknownTg;
        case IGNORED_TG: return TrMonitoringState::IgnoredTg;
        case NO_SOURCE: return TrMonitoringState::NoSource;
        case NO_RECORDER: return TrMonitoringState::NoRecorder;
        case ENCRYPTED: return TrMonitoringState::Encrypted;
        case DUPLICATE: return TrMonitoringState::Duplicate;
        case SUPERSEDED: return TrMonitoringState::Superseded;
        default: return TrMonitoringState::Unspecified;
    }
}

// ── EventBridge implementation ──

EventBridge::EventBridge(rust::Box<EventNotifier> n) : prototype_(std::move(n)) {}

rust::Box<EventNotifier> EventBridge::make_notifier() const {
    return event_notifier_clone(*prototype_);
}

// ── Struct conversion helpers ──

TrTrunkMessage convert_trunk_message(const TrunkMessage &msg) {
    TrTrunkMessage out;
    out.message_type = convert_message_type(msg.message_type);
    out.meta = rust::String(msg.meta);
    out.freq = msg.freq;
    out.talkgroup = msg.talkgroup;
    out.encrypted = msg.encrypted;
    out.emergency = msg.emergency;
    out.duplex = msg.duplex;
    out.mode = msg.mode;
    out.priority = msg.priority;
    out.tdma_slot = msg.tdma_slot;
    out.phase2_tdma = msg.phase2_tdma;
    out.source = msg.source;
    out.sys_num = msg.sys_num;
    out.sys_id = msg.sys_id;
    out.sys_rfss = msg.sys_rfss;
    out.sys_site_id = msg.sys_site_id;
    out.nac = msg.nac;
    out.wacn = msg.wacn;
    out.patch_data.sg = msg.patch_data.sg;
    out.patch_data.ga1 = msg.patch_data.ga1;
    out.patch_data.ga2 = msg.patch_data.ga2;
    out.patch_data.ga3 = msg.patch_data.ga3;
    out.opcode = msg.opcode;
    return out;
}

static TrTransmission convert_transmission(const Transmission &tx) {
    TrTransmission out;
    out.source = tx.source;
    out.talkgroup = tx.talkgroup;
    out.slot = tx.slot;
    out.color_code = tx.color_code;
    out.start_time = tx.start_time;
    out.stop_time = tx.stop_time;
    out.sample_count = tx.sample_count;
    out.spike_count = tx.spike_count;
    out.error_count = tx.error_count;
    out.freq = tx.freq;
    out.length = tx.length;
    return out;
}

// ── System info bulk read (replaces 15 individual system_get_* functions) ──

static TrSystemInfo build_system_info(System &sys) {
    TrSystemInfo info{};
    info.sys_num = sys.get_sys_num();
    // Map C++ SystemType enum (0-based) to proto SystemType enum (1-based, 0 = UNSPECIFIED)
    info.system_type = static_cast<int32_t>(sys.get_system_type()) + 1;
    // DSP parameters (read by recorders at start time)
    info.bandplan = rust::String(sys.get_bandplan());
    info.analog_levels = sys.get_analog_levels();
    info.digital_levels = sys.get_digital_levels();
    info.squelch_db = sys.get_squelch_db();
    // Remaining fields (short_name, identity, conversation_mode, rates)
    // are zero-initialized — Rust fills them from its own caches.
    return info;
}


// ── Call accessor implementations (most removed — Rust owns call state via CallEntry) ──

int32_t call_get_freq_error(const TrCall &call) { return call.ptr->get_freq_error(); }
double call_get_current_length(const TrCall &call) {
    // Bypass Call_impl::get_current_length() state check — Rust owns state
    auto rec = call.ptr->get_recorder();
    return rec ? rec->get_current_length() : 0.0;
}

// ── Recorder accessor implementations ──

int32_t recorder_get_num(const TrRecorder &rec) { return rec.ptr->get_num(); }
double recorder_get_freq(const TrRecorder &rec) { return rec.ptr->get_freq(); }
int32_t recorder_get_freq_error(const TrRecorder &rec) { return rec.ptr->get_freq_error(); }
int64_t recorder_get_talkgroup(const TrRecorder &rec) { return rec.ptr->get_talkgroup(); }
int64_t recorder_get_wav_hz(const TrRecorder &rec) { return rec.ptr->get_wav_hz(); }
bool recorder_is_analog(const TrRecorder &rec) { return rec.ptr->is_analog(); }
bool recorder_is_conventional(const TrRecorder &rec) { return rec.ptr->is_conventional(); }
bool recorder_is_active(const TrRecorder &rec) { return rec.ptr->is_active(); }
bool recorder_is_idle(const TrRecorder &rec) { return rec.ptr->is_idle(); }
double recorder_get_current_length(const TrRecorder &rec) { return rec.ptr->get_current_length(); }

// ── Source accessor implementations ──

// ── CxxEventSink: EventSink subclass for the 3 C++-initiated callbacks ──
// Calls Rust trampolines directly on the EventHandlerBox.

class CxxEventSink : public EventSink {
public:
    explicit CxxEventSink(EventHandlerBox *handler) : handler_(handler) {}

    void audio_callback(const std::shared_ptr<Call> &call, const std::shared_ptr<Recorder> &recorder,
                        int16_t *samples, int sampleCount) override {
        TrCall tr_call(call);
        TrRecorder tr_rec(recorder);
        rust::Slice<const int16_t> slice(samples, static_cast<size_t>(sampleCount));
        event_handler_audio_stream(*handler_, call ? &tr_call : nullptr, tr_rec, slice);
    }

    void setup_recorder(const std::shared_ptr<Recorder> &recorder) override {
        TrRecorder tr_rec(recorder);
        event_handler_setup_recorder(*handler_, tr_rec);
    }

    void unit_alias_discovered(const std::shared_ptr<System> &system,
                               const OTAAlias &alias) override {
        TrSystem tr_sys(system);
        event_handler_unit_alias_discovered(
            *handler_, tr_sys, alias.radio_id,
            rust::Str(alias.alias.data(), alias.alias.size()),
            rust::Str(alias.source.data(), alias.source.size()),
            alias.talkgroup_id);
    }

private:
    EventHandlerBox *handler_;
};

// ── TrContext pimpl implementation ──

struct TrContextImpl {
    rust::Box<EventHandlerBox> handler;  // destroyed LAST (declared first)
    TrunkContext trunk_ctx;              // destroyed FIRST

    // Event bridge for GR thread → Rust channel notifications
    std::shared_ptr<EventBridge> event_bridge;

    explicit TrContextImpl(rust::Box<EventHandlerBox> h)
        : handler(std::move(h)) {}
};

TrContext::TrContext() = default;
TrContext::~TrContext() = default;
TrContext::TrContext(TrContext &&) noexcept = default;
TrContext &TrContext::operator=(TrContext &&) noexcept = default;

TrSystemInfo tr_context_get_system_info(TrContext &ctx, int32_t sys_num) {
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        if (sys->get_sys_num() == sys_num) {
            return build_system_info(*sys);
        }
    }
    TrSystemInfo info = {};
    info.sys_num = sys_num;
    return info;
}

// ── Lifecycle function implementations ──

// ── Builder: create empty context ──

std::unique_ptr<TrContext> tr_create_from_builder(rust::Box<EventHandlerBox> handler) {
    auto ctx = std::make_unique<TrContext>();
    ctx->impl = std::make_unique<TrContextImpl>(std::move(handler));
    ctx->impl->trunk_ctx.config.event_sink =
        std::make_shared<CxxEventSink>(&*ctx->impl->handler);
    ctx->impl->trunk_ctx.tb = gr::make_top_block("Trunking");
    return ctx;
}

// ── Builder: finalize (setup_systems only — event callbacks moved to Rust) ──

void tr_finalize(TrContext &ctx) {
    setup_systems(ctx.impl->trunk_ctx.config, ctx.impl->trunk_ctx.tb,
                  ctx.impl->trunk_ctx.sources, ctx.impl->trunk_ctx.systems,
                  ctx.impl->trunk_ctx.calls);

    // Conventional calls remain in trunk_ctx.calls for Rust to drain
    // via tr_context_take_next_conventional_call after finalize.
    // setup_config, setup_system, and start are now fired from Rust.
}

// ── Config builder implementations ──

void tr_config_set_temp_dir(TrContext &ctx, rust::Str dir) {
    ctx.impl->trunk_ctx.config.temp_dir = std::string(dir.data(), dir.size());
}

void tr_config_set_soft_vocoder(TrContext &ctx, bool enabled) {
    ctx.impl->trunk_ctx.config.soft_vocoder = enabled;
}

void tr_config_set_debug_recorder(TrContext &ctx, bool enabled, rust::Str address, int32_t port) {
    ctx.impl->trunk_ctx.config.debug_recorder = enabled;
    ctx.impl->trunk_ctx.config.debug_recorder_address = std::string(address.data(), address.size());
    ctx.impl->trunk_ctx.config.debug_recorder_port = port;
}

// ── Source builder implementations ──

int32_t tr_add_source_from_config(TrContext &ctx, const TrSourceConfig &cfg) {
    std::string drv(cfg.driver.data(), cfg.driver.size());
    std::string dev(cfg.device.data(), cfg.device.size());
    auto source = std::make_shared<Source>(cfg.center, cfg.rate, cfg.error, drv, dev,
                                           ctx.impl->trunk_ctx.config);
    source->init_shared();

    if (cfg.agc) source->set_gain_mode(true);
    if (cfg.gain != 0.0) source->set_gain(cfg.gain);
    if (cfg.ppm != 0.0) source->set_freq_corr(cfg.ppm);
    if (!cfg.antenna.empty()) source->set_antenna(std::string(cfg.antenna.data(), cfg.antenna.size()));
    if (cfg.silence_frames != 0) source->set_silence_frames(cfg.silence_frames);
    if (cfg.has_signal_detector_threshold) source->set_signal_detector_threshold(static_cast<float>(cfg.signal_detector_threshold));

    for (const auto &gs : cfg.gain_stages) {
        source->set_gain_by_name(std::string(gs.name.data(), gs.name.size()), gs.value);
    }

    auto &tb = ctx.impl->trunk_ctx.tb;
    if (cfg.digital_recorders > 0) source->create_digital_recorders(tb, cfg.digital_recorders);
    if (cfg.analog_recorders > 0) source->create_analog_recorders(tb, cfg.analog_recorders);
    if (cfg.sigmf_recorders > 0) source->create_sigmf_recorders(tb, cfg.sigmf_recorders);

    int32_t idx = static_cast<int32_t>(ctx.impl->trunk_ctx.sources.size());
    ctx.impl->trunk_ctx.sources.push_back(source);
    return idx;
}

int32_t tr_add_system_from_config(TrContext &ctx, const TrSystemConfig &cfg) {
    int32_t sys_idx = static_cast<int32_t>(ctx.impl->trunk_ctx.systems.size());
    auto sys = System::make(sys_idx);
    sys->set_short_name(std::string(cfg.short_name.data(), cfg.short_name.size()));
    sys->set_system_type(system_type_from_string(std::string(cfg.system_type.data(), cfg.system_type.size())));

    // Control channels
    for (double freq : cfg.control_channels) {
        sys->add_control_channel(freq);
    }

    // DSP settings
    sys->set_qpsk_mod(cfg.qpsk_mod);
    sys->set_squelch_db(cfg.squelch_db);
    sys->set_analog_levels(cfg.analog_levels);
    sys->set_digital_levels(cfg.digital_levels);
    sys->set_tau(static_cast<float>(cfg.tau));
    sys->set_max_dev(static_cast<int32_t>(cfg.max_dev));
    sys->set_filter_width(cfg.filter_width);

    // Decoder options
    sys->set_mdc_enabled(cfg.decode_mdc);
    sys->set_fsync_enabled(cfg.decode_fsync);
    sys->set_star_enabled(cfg.decode_star);
    sys->set_tps_enabled(cfg.decode_tps);

    // Bandplan
    if (!cfg.bandplan.empty()) sys->set_bandplan(std::string(cfg.bandplan.data(), cfg.bandplan.size()));
    if (cfg.bandplan_base > 0.0) sys->set_bandplan_base(cfg.bandplan_base);
    if (cfg.bandplan_high > 0.0) sys->set_bandplan_high(cfg.bandplan_high);
    if (cfg.bandplan_spacing > 0.0) sys->set_bandplan_spacing(cfg.bandplan_spacing);
    if (cfg.bandplan_offset != 0) sys->set_bandplan_offset(cfg.bandplan_offset);
    if (cfg.bandfreq != 0.0) sys->set_bandfreq(static_cast<int32_t>(cfg.bandfreq));

    // XOR mask
    if (cfg.sys_id != 0 || cfg.wacn != 0 || cfg.nac != 0) {
        sys->set_xor_mask(cfg.sys_id, cfg.wacn, cfg.nac);
    }

    // Talkgroups
    for (const auto &tg : cfg.talkgroups) {
        auto tg_obj = std::make_shared<Talkgroup>(
            sys->get_sys_num(), tg.id,
            std::string(tg.mode.data(), tg.mode.size()),
            tg.priority, tg.preferred_nac);
        sys->add_talkgroup(tg_obj);
    }

    // Conventional channels
    for (const auto &ch : cfg.channels) {
        auto tg_obj = std::make_shared<Talkgroup>(
            sys->get_sys_num(), ch.id, ch.freq, ch.tone, ch.squelch_db, ch.signal_detection);
        sys->add_talkgroup(tg_obj);
        sys->add_channel(ch.freq);
    }

    // Unit tags
    for (const auto &ut : cfg.unit_tags) {
        sys->add_unit_tag(
            std::string(ut.pattern.data(), ut.pattern.size()),
            std::string(ut.tag.data(), ut.tag.size()));
    }
    if (!cfg.unit_tags_mode.empty()) {
        sys->set_unit_tags_mode(std::string(cfg.unit_tags_mode.data(), cfg.unit_tags_mode.size()));
    }

    ctx.impl->trunk_ctx.systems.push_back(sys);
    return sys_idx;
}

// ── Reverse conversion: TrTrunkMessage → C++ TrunkMessage ──

static MessageType convert_message_type_back(TrMessageType mt) {
    switch (mt) {
        case TrMessageType::Grant: return GRANT;
        case TrMessageType::Status: return STATUS;
        case TrMessageType::Update: return UPDATE;
        case TrMessageType::ControlChannel: return CONTROL_CHANNEL;
        case TrMessageType::Registration: return REGISTRATION;
        case TrMessageType::Deregistration: return DEREGISTRATION;
        case TrMessageType::Affiliation: return AFFILIATION;
        case TrMessageType::Sysid: return SYSID;
        case TrMessageType::Acknowledge: return ACKNOWLEDGE;
        case TrMessageType::Location: return LOCATION;
        case TrMessageType::PatchAdd: return PATCH_ADD;
        case TrMessageType::PatchDelete: return PATCH_DELETE;
        case TrMessageType::DataGrant: return DATA_GRANT;
        case TrMessageType::UuAnsReq: return UU_ANS_REQ;
        case TrMessageType::UuVGrant: return UU_V_GRANT;
        case TrMessageType::UuVUpdate: return UU_V_UPDATE;
        case TrMessageType::InvalidCcMessage: return INVALID_CC_MESSAGE;
        case TrMessageType::Tdulc: return TDULC;
        default: return UNKNOWN;
    }
}

static CallState convert_state_back(TrState s) {
    switch (s) {
        case TrState::Recording: return RECORDING;
        default: return MONITORING;
    }
}

static MonitoringState convert_monitoring_state_back(TrMonitoringState ms) {
    switch (ms) {
        case TrMonitoringState::UnknownTg: return UNKNOWN_TG;
        case TrMonitoringState::IgnoredTg: return IGNORED_TG;
        case TrMonitoringState::NoSource: return NO_SOURCE;
        case TrMonitoringState::NoRecorder: return NO_RECORDER;
        case TrMonitoringState::Encrypted: return ENCRYPTED;
        case TrMonitoringState::Duplicate: return DUPLICATE;
        case TrMonitoringState::Superseded: return SUPERSEDED;
        default: return UNSPECIFIED;
    }
}

static TrunkMessage convert_trunk_message_back(const TrTrunkMessage &msg) {
    TrunkMessage out;
    out.message_type = convert_message_type_back(msg.message_type);
    out.meta = std::string(msg.meta.data(), msg.meta.size());
    out.freq = msg.freq;
    out.talkgroup = msg.talkgroup;
    out.encrypted = msg.encrypted;
    out.emergency = msg.emergency;
    out.duplex = msg.duplex;
    out.mode = msg.mode;
    out.priority = msg.priority;
    out.tdma_slot = msg.tdma_slot;
    out.phase2_tdma = msg.phase2_tdma;
    out.source = msg.source;
    out.sys_num = msg.sys_num;
    out.sys_id = msg.sys_id;
    out.sys_rfss = msg.sys_rfss;
    out.sys_site_id = msg.sys_site_id;
    out.nac = msg.nac;
    out.wacn = msg.wacn;
    out.patch_data.sg = msg.patch_data.sg;
    out.patch_data.ga1 = msg.patch_data.ga1;
    out.patch_data.ga2 = msg.patch_data.ga2;
    out.patch_data.ga3 = msg.patch_data.ga3;
    out.opcode = msg.opcode;
    return out;
}

// ── Helper: find system by sys_num ──

static std::shared_ptr<System> find_system_by_num(TrContext &ctx, int sys_num) {
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        if (sys->get_sys_num() == sys_num) {
            return sys;
        }
    }
    return nullptr;
}

// ── Phase B: Message Routing (called from Rust handle_message) ──

void tr_system_add_control_channel_runtime(TrContext &ctx, int32_t sys_num, double freq) {
    auto sys = find_system_by_num(ctx, sys_num);
    if (sys) {
        sys->add_control_channel(freq);
    }
}

void tr_system_retune(TrContext &ctx, int32_t sys_num) {
    auto sys = find_system_by_num(ctx, sys_num);
    if (!sys) return;
    auto &tb = ctx.impl->trunk_ctx.tb;
    auto &sources = ctx.impl->trunk_ctx.sources;
    sys->retune_trunking(tb, sources);
    if (sys->get_source() && sys->get_source()->get_autotune_source() && sys->get_system_type() == SYS_P25) {
        autotune_control_channel(sys, false);
    }
}

bool tr_system_update_sysid(TrContext &ctx, const TrTrunkMessage &msg) {
    auto sys = find_system_by_num(ctx, msg.sys_num);
    if (!sys) return false;
    if (sys->get_system_type() != SYS_P25 && sys->get_system_type() != SYS_CONVENTIONAL_P25) {
        return false;
    }
    auto tm = convert_trunk_message_back(msg);
    return sys->update_sysid(tm);
}

bool tr_system_update_status(TrContext &ctx, const TrTrunkMessage &msg) {
    auto sys = find_system_by_num(ctx, msg.sys_num);
    if (!sys) return false;
    auto tm = convert_trunk_message_back(msg);
    return sys->update_status(tm);
}

// ── Phase B: Decode Rate Checking (called from Rust check_decode_rates) ──

double tr_system_get_current_control_channel(TrContext &ctx, int32_t sys_num) {
    auto sys = find_system_by_num(ctx, sys_num);
    return sys ? sys->get_current_control_channel() : 0.0;
}

// ── Phase A: Loop Control ──

void tr_start_gnuradio(TrContext &ctx) {
    const char *env = std::getenv("RUST_LOG");
    set_logging_level(env ? std::string(env) : "info");
    ctx.impl->trunk_ctx.tb->start();
}

void tr_stop_gnuradio(TrContext &ctx) {
    ctx.impl->trunk_ctx.tb->stop();
    ctx.impl->trunk_ctx.tb->wait();
}

// ── Event channel wiring ──

void tr_context_set_event_notifier(TrContext &ctx, rust::Box<EventNotifier> notifier) {
    ctx.impl->event_bridge = std::make_shared<EventBridge>(std::move(notifier));
}

// Per-system state for trunk message parsing on GR threads
struct TrunkDrainState {
    rust::Box<EventNotifier> notifier;
    std::unique_ptr<SmartnetParser> smartnet_parser;  // non-null for Smartnet
    std::unique_ptr<P25Parser> p25_parser;            // non-null for P25
    P25ParserConfig p25_config;

    explicit TrunkDrainState(rust::Box<EventNotifier> n)
        : notifier(std::move(n)) {}
};

void tr_context_wire_event_notifiers(TrContext &ctx) {
    auto &bridge = ctx.impl->event_bridge;
    if (!bridge) return;

    // Phase 2: wire trunk message callbacks on trunked systems
    for (auto &system : ctx.impl->trunk_ctx.systems) {
        auto sys_type = system->get_system_type();
        if (sys_type != SYS_SMARTNET && sys_type != SYS_P25) continue;

        auto state = std::shared_ptr<TrunkDrainState>(
            new TrunkDrainState(bridge->make_notifier()));

        if (sys_type == SYS_SMARTNET) {
            SmartnetParserConfig config;
            config.sys_num = system->get_sys_num();
            config.bandplan = system->get_bandplan();
            config.bandfreq = system->get_bandfreq();
            config.bandplan_base = system->get_bandplan_base();
            config.bandplan_high = system->get_bandplan_high();
            config.bandplan_spacing = system->get_bandplan_spacing();
            config.bandplan_offset = system->get_bandplan_offset();
            state->smartnet_parser = std::make_unique<SmartnetParser>(std::move(config));
        } else if (sys_type == SYS_P25) {
            state->p25_parser = std::make_unique<P25Parser>();
            P25ParserConfig p25_config;
            p25_config.sys_num = system->get_sys_num();
            p25_config.short_name = system->get_short_name();
            p25_config.custom_freq_table_file = system->has_custom_freq_table_file()
                ? system->get_custom_freq_table_file() : "";
            state->p25_config = std::move(p25_config);
        }

        system->set_msg_callback([state](gr::message::sptr msg) {
            std::vector<TrunkMessage> parsed;
            if (state->smartnet_parser) {
                parsed = state->smartnet_parser->parse_message(msg);
            } else if (state->p25_parser) {
                parsed = state->p25_parser->parse_message(msg, state->p25_config);
            }
            if (parsed.empty()) return;

            rust::Vec<TrTrunkMessage> result;
            for (const auto &m : parsed) {
                result.push_back(convert_trunk_message(m));
            }
            event_notifier_notify_trunk_messages(*state->notifier, std::move(result));
        });
    }

    // Phase 3: wire squelch callbacks on recorder blocks
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        for (auto &rec : source->get_recorders()) {
            auto notifier = bridge->make_notifier();
            int rec_num = rec->get_num();
            rec->set_squelch_callback(
                [notifier = std::make_shared<rust::Box<EventNotifier>>(std::move(notifier)),
                 rec_num](bool muted, double pwr_db) {
                    event_notifier_notify_squelch(**notifier, rec_num, muted, pwr_db);
                });
        }
    }

    // Phase 4: wire signal detection callbacks on sources
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        auto notifier = bridge->make_notifier();
        source->set_detection_callback(
            [notifier = std::make_shared<rust::Box<EventNotifier>>(std::move(notifier))]() {
                event_notifier_notify_signal_detected(**notifier);
            });
    }
}

void tr_context_check_conventional_detection(TrContext &ctx) {
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        source->enable_detected_recorders();
    }
}

void tr_context_check_sources(TrContext &ctx) {
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        if (!source->got_samples()) {
            std::cerr << "Source " << source->get_num() << " has stopped receiving samples - Terminating trunk recorder" << std::endl;
            throw std::runtime_error("Source stopped receiving samples");
        }
    }
}

void tr_context_autotune_control_channels(TrContext &ctx) {
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        if (!is_conventional(sys->get_system_type())) {
            if ((sys->get_source()->get_autotune_source()) && (sys->get_system_type() == SYS_P25)) {
                autotune_control_channel(sys);
            }
        }
    }
}

rust::Vec<TrRecorderStatus> tr_context_get_recorder_status(const TrContext &ctx) {
    rust::Vec<TrRecorderStatus> result;
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        auto recorders = source->get_recorders();
        for (auto &rx : recorders) {
            TrRecorderStatus s;
            s.source_num = source->get_num();
            s.source_center = source->get_center();
            s.source_device = rust::String(source->get_device());
            s.rec_num = rx->get_num();
            s.rec_type = rust::String(rx->get_type_string());
            s.rec_state = static_cast<int32_t>(rx->get_state());
            result.push_back(std::move(s));
        }
    }
    return result;
}

// ── Conventional call management — recorder status (null-guarded) ──

bool call_has_recorder(const TrCall &call) {
    return call.ptr->get_recorder() != nullptr;
}

bool call_get_recorder_is_active(const TrCall &call) {
    auto rec = call.ptr->get_recorder();
    return rec ? rec->is_active() : false;
}

bool call_get_recorder_is_enabled(const TrCall &call) {
    auto rec = call.ptr->get_recorder();
    return rec ? rec->is_enabled() : false;
}

int32_t call_get_recorder_state(const TrCall &call) {
    auto rec = call.ptr->get_recorder();
    return rec ? static_cast<int32_t>(rec->get_state()) : static_cast<int32_t>(REC_INACTIVE);
}

// ── Conventional call management — signal/noise (removed — Rust owns) ──
// ── Conventional call management — system info (removed — Rust owns via CallEntry::max_duration) ──

// ── Conventional call management — lifecycle ──

void call_restart(TrCall &call) {
    call.ptr->restart_call();
}

void call_start_initial_recording(TrCall &call) {
    auto rec = call.ptr->get_recorder();
    if (rec) {
        rec->start(call.ptr);
    }
}

// ── Conventional call management — context-level ──

std::unique_ptr<TrCall> tr_context_take_next_conventional_call(TrContext &ctx) {
    auto &calls = ctx.impl->trunk_ctx.calls;
    if (calls.empty()) {
        return nullptr;
    }
    auto call_ptr = std::move(calls.back());
    calls.pop_back();
    return std::make_unique<TrCall>(call_ptr);
}

// ── Phase C: Call Management — additional call accessors (most removed — Rust owns via CallEntry) ──

// ── Phase C: Call Management — additional recorder accessors ──

double recorder_since_last_write_secs(const TrRecorder &rec) {
    return rec.ptr->since_last_write().count();
}

int32_t recorder_get_type(const TrRecorder &rec) {
    return static_cast<int32_t>(rec.ptr->get_type());
}

// ── Phase C: Call Management — lifecycle operations ──

std::unique_ptr<TrCall> tr_context_create_call(TrContext &ctx, const TrTrunkMessage &msg) {
    auto cpp_msg = convert_trunk_message_back(msg);
    auto sys = find_system_by_num(ctx, msg.sys_num);
    if (!sys) {
        throw std::runtime_error("System not found for sys_num");
    }
    auto call = Call::make(cpp_msg, sys, ctx.impl->trunk_ctx.config);
    return std::make_unique<TrCall>(call);
}

TrCallMetadata call_get_metadata(const TrCall &call) {
    auto *c = call.ptr.get();
    TrCallMetadata m;
    m.call_num = c->get_call_num();
    m.talkgroup = c->get_talkgroup();
    m.freq = c->get_freq();
    m.sys_num = c->get_sys_num();
    m.short_name = rust::String(c->get_short_name());
    m.tdma_slot = c->get_tdma_slot();
    m.phase2_tdma = c->get_phase2_tdma();
    m.mode = c->get_mode();
    m.duplex = c->get_duplex();
    m.emergency = c->get_emergency();
    m.is_conventional = c->is_conventional();
    m.start_time = static_cast<int64_t>(c->get_start_time());
    m.state = convert_state(c->get_state());
    m.monitoring_state = convert_monitoring_state(c->get_monitoring_state());
    m.encrypted = c->get_encrypted();
    m.is_analog = c->get_is_analog();
    m.current_source_id = c->get_current_source_id();
    m.signal = c->get_signal();
    m.noise = c->get_noise();
    m.priority = c->get_priority();
    return m;
}

// State/monitoring/encrypted/is_analog setters removed — Rust owns all call state

int32_t tr_context_source_count(const TrContext &ctx) {
    return static_cast<int32_t>(ctx.impl->trunk_ctx.sources.size());
}

bool tr_source_covers_freq(const TrContext &ctx, int32_t source_idx, double freq) {
    auto &sources = ctx.impl->trunk_ctx.sources;
    if (source_idx < 0 || source_idx >= static_cast<int32_t>(sources.size())) return false;
    auto &source = sources[source_idx];
    return source->get_min_hz() <= freq && freq <= source->get_max_hz();
}

int32_t tr_source_available_recorders(const TrContext &ctx, int32_t source_idx, bool is_analog) {
    auto &sources = ctx.impl->trunk_ctx.sources;
    if (source_idx < 0 || source_idx >= static_cast<int32_t>(sources.size())) return 0;
    auto &source = sources[source_idx];
    return is_analog
        ? source->get_num_available_analog_recorders()
        : source->get_num_available_digital_recorders();
}

bool tr_source_start_recording(TrContext &ctx, int32_t source_idx, TrCall &call, bool is_analog) {
    auto &sources = ctx.impl->trunk_ctx.sources;
    if (source_idx < 0 || source_idx >= static_cast<int32_t>(sources.size())) return false;
    auto &source = sources[source_idx];
    auto &config = ctx.impl->trunk_ctx.config;

    std::shared_ptr<Recorder> recorder = is_analog
        ? source->get_analog_recorder(call.ptr)
        : source->get_digital_recorder(call.ptr);

    if (!recorder || !recorder->start(call.ptr)) {
        return false;
    }

    call.ptr->set_recorder(recorder);
    // State is now owned by Rust — caller sets entry.state = Recording
    config.event_sink->setup_recorder(recorder);

    // Debug recorder
    auto debug_recorder = source->get_debug_recorder();
    if (debug_recorder) {
        debug_recorder->start(call.ptr);
        call.ptr->set_debug_recorder(debug_recorder);
        call.ptr->set_debug_recording(true);
        config.event_sink->setup_recorder(debug_recorder);
    }

    // SigMF recorder
    auto sigmf_recorder = source->get_sigmf_recorder();
    if (sigmf_recorder) {
        sigmf_recorder->start(call.ptr);
        call.ptr->set_sigmf_recorder(sigmf_recorder);
        call.ptr->set_sigmf_recording(true);
        config.event_sink->setup_recorder(sigmf_recorder);
    }

    return true;
}

// ── Phase C: Call Management — conclude recording (recorder mechanics) ──

TrStopRecordingResult call_stop_recording(TrCall &call) {
    TrStopRecordingResult result = {};
    auto rec = call.ptr->get_recorder();
    if (!rec) {
        return result;  // has_recorder = false
    }
    result.has_recorder = true;
    result.final_length = rec->get_current_length();
    result.freq_error = rec->get_freq_error();
    result.recorder_pwr = rec->get_pwr();
    result.recorder_idle = rec->is_idle();
    result.is_dmr = (call.ptr->get_system()->get_system_type() == SYS_CONVENTIONAL_DMR);

    rec->stop();

    if (auto sigmf = call.ptr->get_sigmf_recorder()) {
        if (call.ptr->get_sigmf_recording()) sigmf->stop();
    }
    if (auto debug = call.ptr->get_debug_recorder()) {
        if (call.ptr->get_debug_recording()) debug->stop();
    }
    return result;
}

rust::Vec<TrTransmission> call_get_transmission_list(const TrCall &call) {
    rust::Vec<TrTransmission> result;
    auto rec = call.ptr->get_recorder();
    if (!rec) return result;
    for (const auto &tx : rec->get_transmission_list()) {
        result.push_back(convert_transmission(tx));
    }
    return result;
}

rust::Vec<TrTransmission> call_get_dmr_transmission_list(const TrCall &call, int32_t slot) {
    rust::Vec<TrTransmission> result;
    auto rec = call.ptr->get_recorder();
    if (!rec) return result;
    auto dmr_rec = std::dynamic_pointer_cast<dmr_recorder>(rec);
    if (!dmr_rec) return result;
    for (const auto &tx : dmr_rec->get_transmission_list(slot)) {
        result.push_back(convert_transmission(tx));
    }
    return result;
}

int32_t call_get_recorder_num(const TrCall &call) {
    auto rec = call.ptr->get_recorder();
    return rec ? rec->get_num() : 0;
}

// ── Phase C: Call Management — update source tracking ──
// call_touch and call_set_current_source_id removed — Rust owns timing and source_id

void call_recorder_set_source(TrCall &call, int64_t src) {
    // Rust only calls this when entry.state == Recording
    auto rec = call.ptr->get_recorder();
    if (rec) rec->set_source(src);
}

// ── Phase C: Call Management — call-level recorder accessors ──

double call_recorder_since_last_write_secs(const TrCall &call) {
    auto recorder = call.ptr->get_recorder();
    if (!recorder) {
        return 1e9; // no recorder → effectively infinite (never triggers timeout alone)
    }
    return recorder->since_last_write().count();
}

void call_process_recorder_queue(const TrCall &call) {
    auto recorder = call.ptr->get_recorder();
    if (!recorder) return;
    recorder->process_message_queues();
}

// ── Recorder setup notification (Rust calls this to fire setup_recorder through event_sink) ──
// Needed because Rust doesn't have direct access to the C++ recorder shared_ptr.

void call_notify_setup_recorder(TrContext &ctx, const TrCall &call) {
    auto recorder = call.ptr->get_recorder();
    if (recorder) {
        ctx.impl->trunk_ctx.config.event_sink->setup_recorder(recorder);
    }
}

void tr_recorder_process_queue(const TrRecorder &rec) {
    if (rec.ptr && (rec.ptr->get_type() == P25 || rec.ptr->get_type() == P25C)) {
        auto p25_rec = std::dynamic_pointer_cast<p25_recorder>(rec.ptr);
        if (p25_rec && p25_rec->is_active()) {
            p25_rec->process_message_queues();
        }
    }
}

} // namespace trunk_oxide
