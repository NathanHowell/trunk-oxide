// Our bridge header (includes trunk-recorder headers and rust/cxx.h)
#include "trunk-recorder-sys/cpp/bridge.h"

// cxx-generated header for the Rust bridge
#include "trunk-recorder-sys/src/lib.rs.h"

#include <cstdlib>
#include <cstring>
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

// Forward declarations for functions provided by libtrunk-recorder.so
bool setup_systems(Config &config, gr::top_block_sptr &tb,
                   std::vector<std::shared_ptr<Source>> &sources, std::vector<std::shared_ptr<System>> &systems,
                   std::vector<std::shared_ptr<Call>> &calls);
void set_logging_level(std::string log_level);

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

// ── Struct conversion helpers ──

static TrTrunkMessage convert_trunk_message(const TrunkMessage &msg) {
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
    TrSystemInfo info;
    info.sys_num = sys.get_sys_num();
    info.sys_id = sys.get_sys_id();
    info.short_name = rust::String(sys.get_short_name());
    // Map C++ SystemType enum (0-based) to proto SystemType enum (1-based, 0 = UNSPECIFIED)
    info.system_type = static_cast<int32_t>(sys.get_system_type()) + 1;
    info.wacn = sys.get_wacn();
    info.nac = sys.get_nac();
    info.rfss = sys.get_sys_rfss();
    info.site_id = sys.get_sys_site_id();
    info.bandplan = rust::String(sys.get_bandplan());
    info.analog_levels = sys.get_analog_levels();
    info.digital_levels = sys.get_digital_levels();
    info.squelch_db = sys.get_squelch_db();
    info.conversation_mode = sys.get_conversation_mode();
    info.message_count = 0;  // Rust owns message count
    info.decode_rate = 0;    // Rust owns decode rate
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

    // Parsers — created eagerly during tr_finalize
    std::map<int, std::unique_ptr<SmartnetParser>> smartnet_parsers;
    std::unique_ptr<P25Parser> p25_parser;

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

    // Eagerly create parsers based on configured system types
    for (auto &system : ctx.impl->trunk_ctx.systems) {
        if (system->get_system_type() == SYS_SMARTNET) {
            ctx.impl->smartnet_parsers[system->get_sys_num()] =
                std::make_unique<SmartnetParser>(system);
        }
        if (system->get_system_type() == SYS_P25 && !ctx.impl->p25_parser) {
            ctx.impl->p25_parser = std::make_unique<P25Parser>();
        }
    }

    // Conventional calls remain in trunk_ctx.calls for Rust to drain
    // via tr_context_take_next_conventional_call after finalize.
    // setup_config, setup_system, and start are now fired from Rust.
}

// ── Config builder implementations ──

void tr_config_set_temp_dir(TrContext &ctx, rust::Str dir) {
    ctx.impl->trunk_ctx.config.temp_dir = std::string(dir.data(), dir.size());
}

void tr_config_set_call_timeout(TrContext &ctx, double seconds) {
    ctx.impl->trunk_ctx.config.call_timeout = std::chrono::duration<double>(seconds);
}

void tr_config_set_control_warn_rate(TrContext &ctx, int32_t rate) {
    ctx.impl->trunk_ctx.config.control_message_warn_rate = rate;
}

void tr_config_set_control_retune_limit(TrContext &ctx, int32_t limit) {
    ctx.impl->trunk_ctx.config.control_retune_limit = limit;
}

void tr_config_set_broadcast_signals(TrContext &ctx, bool enabled) {
    ctx.impl->trunk_ctx.config.broadcast_signals = enabled;
}

void tr_config_set_soft_vocoder(TrContext &ctx, bool enabled) {
    ctx.impl->trunk_ctx.config.soft_vocoder = enabled;
}

void tr_config_set_record_uu_v_calls(TrContext &ctx, bool enabled) {
    ctx.impl->trunk_ctx.config.record_uu_v_calls = enabled;
}

void tr_config_set_new_call_from_update(TrContext &ctx, bool enabled) {
    ctx.impl->trunk_ctx.config.new_call_from_update = enabled;
}

void tr_config_set_frequency_format(TrContext &ctx, int32_t format) {
    ctx.impl->trunk_ctx.config.frequency_format = format;
}

void tr_config_set_default_mode(TrContext &ctx, rust::Str mode) {
    ctx.impl->trunk_ctx.config.default_mode = std::string(mode.data(), mode.size());
}

void tr_config_set_debug_recorder(TrContext &ctx, bool enabled, rust::Str address, int32_t port) {
    ctx.impl->trunk_ctx.config.debug_recorder = enabled;
    ctx.impl->trunk_ctx.config.debug_recorder_address = std::string(address.data(), address.size());
    ctx.impl->trunk_ctx.config.debug_recorder_port = port;
}

// ── Source builder implementations ──

int32_t tr_add_source(TrContext &ctx, rust::Str driver, rust::Str device,
                      double center, double rate, double error) {
    std::string drv(driver.data(), driver.size());
    std::string dev(device.data(), device.size());
    auto source = std::make_shared<Source>(center, rate, error, drv, dev, ctx.impl->trunk_ctx.config);
    source->init_shared();
    int32_t idx = static_cast<int32_t>(ctx.impl->trunk_ctx.sources.size());
    ctx.impl->trunk_ctx.sources.push_back(source);
    return idx;
}

void tr_source_set_gain(TrContext &ctx, int32_t idx, double gain) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_gain(gain);
}

void tr_source_set_gain_by_name(TrContext &ctx, int32_t idx, rust::Str name, double gain) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_gain_by_name(
        std::string(name.data(), name.size()), gain);
}

void tr_source_set_antenna(TrContext &ctx, int32_t idx, rust::Str antenna) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_antenna(
        std::string(antenna.data(), antenna.size()));
}

void tr_source_set_freq_corr(TrContext &ctx, int32_t idx, double ppm) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_freq_corr(ppm);
}

void tr_source_set_gain_mode(TrContext &ctx, int32_t idx, bool agc) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_gain_mode(agc);
}

void tr_source_set_silence_frames(TrContext &ctx, int32_t idx, int32_t frames) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_silence_frames(frames);
}

void tr_source_set_signal_detector_threshold(TrContext &ctx, int32_t idx, float threshold) {
    ctx.impl->trunk_ctx.sources.at(idx)->set_signal_detector_threshold(threshold);
}

void tr_source_create_digital_recorders(TrContext &ctx, int32_t idx, int32_t count) {
    ctx.impl->trunk_ctx.sources.at(idx)->create_digital_recorders(ctx.impl->trunk_ctx.tb, count);
}

void tr_source_create_analog_recorders(TrContext &ctx, int32_t idx, int32_t count) {
    ctx.impl->trunk_ctx.sources.at(idx)->create_analog_recorders(ctx.impl->trunk_ctx.tb, count);
}

void tr_source_create_sigmf_recorders(TrContext &ctx, int32_t idx, int32_t count) {
    ctx.impl->trunk_ctx.sources.at(idx)->create_sigmf_recorders(ctx.impl->trunk_ctx.tb, count);
}

// ── System builder implementations ──

int32_t tr_add_system(TrContext &ctx, rust::Str short_name, rust::Str system_type) {
    int32_t sys_idx = static_cast<int32_t>(ctx.impl->trunk_ctx.systems.size());
    auto system = System::make(sys_idx);
    system->set_short_name(std::string(short_name.data(), short_name.size()));
    std::string type_str(system_type.data(), system_type.size());
    system->set_system_type(system_type_from_string(type_str));
    ctx.impl->trunk_ctx.systems.push_back(system);
    return sys_idx;
}

void tr_system_add_control_channel(TrContext &ctx, int32_t sys_idx, double freq) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->add_control_channel(freq);
}

void tr_system_add_channel(TrContext &ctx, int32_t sys_idx, double freq) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->add_channel(freq);
}

void tr_system_set_qpsk_mod(TrContext &ctx, int32_t sys_idx, bool qpsk) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_qpsk_mod(qpsk);
}

void tr_system_set_squelch_db(TrContext &ctx, int32_t sys_idx, double db) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_squelch_db(db);
}

void tr_system_set_analog_levels(TrContext &ctx, int32_t sys_idx, double level) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_analog_levels(level);
}

void tr_system_set_digital_levels(TrContext &ctx, int32_t sys_idx, double level) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_digital_levels(level);
}

void tr_system_set_tau(TrContext &ctx, int32_t sys_idx, float tau) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_tau(tau);
}

void tr_system_set_max_dev(TrContext &ctx, int32_t sys_idx, int32_t max_dev) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_max_dev(max_dev);
}

void tr_system_set_filter_width(TrContext &ctx, int32_t sys_idx, double width) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_filter_width(width);
}

void tr_system_set_conversation_mode(TrContext &ctx, int32_t sys_idx, bool mode) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_conversation_mode(mode);
}

void tr_system_set_record_unknown(TrContext &ctx, int32_t sys_idx, bool record) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_record_unknown(record);
}

void tr_system_set_min_duration(TrContext &ctx, int32_t sys_idx, double duration) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_min_duration(duration);
}

void tr_system_set_max_duration(TrContext &ctx, int32_t sys_idx, double duration) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_max_duration(duration);
}

void tr_system_set_min_tx_duration(TrContext &ctx, int32_t sys_idx, double duration) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_min_tx_duration(duration);
}

void tr_system_set_hide_encrypted(TrContext &ctx, int32_t sys_idx, bool hide) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_hideEncrypted(hide);
}

void tr_system_set_monitor_encrypted(TrContext &ctx, int32_t sys_idx, bool monitor) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_monitorEncrypted(monitor);
}

void tr_system_set_hide_unknown(TrContext &ctx, int32_t sys_idx, bool hide) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_hideUnknown(hide);
}

void tr_system_set_mdc_enabled(TrContext &ctx, int32_t sys_idx, bool enabled) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_mdc_enabled(enabled);
}

void tr_system_set_fsync_enabled(TrContext &ctx, int32_t sys_idx, bool enabled) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_fsync_enabled(enabled);
}

void tr_system_set_star_enabled(TrContext &ctx, int32_t sys_idx, bool enabled) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_star_enabled(enabled);
}

void tr_system_set_tps_enabled(TrContext &ctx, int32_t sys_idx, bool enabled) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_tps_enabled(enabled);
}

void tr_system_set_bandplan(TrContext &ctx, int32_t sys_idx, rust::Str bandplan) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_bandplan(
        std::string(bandplan.data(), bandplan.size()));
}

void tr_system_set_bandfreq(TrContext &ctx, int32_t sys_idx, int32_t freq) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_bandfreq(freq);
}

void tr_system_set_bandplan_base(TrContext &ctx, int32_t sys_idx, double base) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_bandplan_base(base);
}

void tr_system_set_bandplan_high(TrContext &ctx, int32_t sys_idx, double high) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_bandplan_high(high);
}

void tr_system_set_bandplan_spacing(TrContext &ctx, int32_t sys_idx, double spacing) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_bandplan_spacing(spacing);
}

void tr_system_set_bandplan_offset(TrContext &ctx, int32_t sys_idx, int32_t offset) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_bandplan_offset(offset);
}

void tr_system_set_xor_mask(TrContext &ctx, int32_t sys_idx, uint64_t sys_id,
                            uint64_t wacn, uint64_t nac) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_xor_mask(sys_id, wacn, nac);
}

void tr_system_set_multi_site(TrContext &ctx, int32_t sys_idx, bool multi_site) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_multiSite(multi_site);
}

void tr_system_set_multi_site_name(TrContext &ctx, int32_t sys_idx, rust::Str name) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_multiSiteSystemName(
        std::string(name.data(), name.size()));
}

void tr_system_set_multi_site_number(TrContext &ctx, int32_t sys_idx, uint64_t number) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_multiSiteSystemNumber(number);
}

// ── Talkgroup/Channel builder implementations ──

void tr_system_add_talkgroup(TrContext &ctx, int32_t sys_idx,
                             int64_t id, rust::Str mode, int32_t priority, uint64_t preferred_nac) {
    auto &sys = ctx.impl->trunk_ctx.systems.at(sys_idx);
    std::string mode_str(mode.data(), mode.size());
    auto tg = std::make_shared<Talkgroup>(sys->get_sys_num(), id, mode_str, priority, preferred_nac);
    sys->add_talkgroup(tg);
}

void tr_system_add_conventional_channel(TrContext &ctx, int32_t sys_idx,
                                        int64_t tg_number, double freq, double tone,
                                        double squelch_db, bool signal_detection) {
    auto &sys = ctx.impl->trunk_ctx.systems.at(sys_idx);
    auto tg = std::make_shared<Talkgroup>(sys->get_sys_num(), tg_number, freq, tone, squelch_db, signal_detection);
    sys->add_talkgroup(tg);
    sys->add_channel(freq);
}

// ── Unit tag builder implementations ──

void tr_system_add_unit_tag(TrContext &ctx, int32_t sys_idx,
                            rust::Str pattern, rust::Str tag) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->add_unit_tag(
        std::string(pattern.data(), pattern.size()),
        std::string(tag.data(), tag.size()));
}

void tr_system_set_unit_tags_mode(TrContext &ctx, int32_t sys_idx, rust::Str mode) {
    ctx.impl->trunk_ctx.systems.at(sys_idx)->set_unit_tags_mode(
        std::string(mode.data(), mode.size()));
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

int32_t tr_system_control_channel_count(TrContext &ctx, int32_t sys_num) {
    auto sys = find_system_by_num(ctx, sys_num);
    return sys ? sys->control_channel_count() : 0;
}

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

// ── Phase B: Message Draining ──

rust::Vec<TrTrunkMessage> tr_context_drain_messages(TrContext &ctx) {
    rust::Vec<TrTrunkMessage> result;
    auto &systems = ctx.impl->trunk_ctx.systems;
    auto &config = ctx.impl->trunk_ctx.config;

    for (auto &system : systems) {
        if (system->get_system_type() != SYS_P25 && system->get_system_type() != SYS_SMARTNET) {
            continue;
        }

        gr::message::sptr msg;
        msg = system->get_msg_queue()->delete_head_nowait();
        while (msg != nullptr) {
            std::vector<TrunkMessage> trunk_messages;

            if (system->get_system_type() == SYS_SMARTNET) {
                auto &smartnet_parser = ctx.impl->smartnet_parsers[system->get_sys_num()];
                trunk_messages = smartnet_parser->parse_message(msg, system);
            } else if (system->get_system_type() == SYS_P25) {
                trunk_messages = ctx.impl->p25_parser->parse_message(msg, system);
            }

            // trunk_message event is now fired from Rust after drain returns (Step 3)

            // Convert all messages to Rust types for return
            for (const auto &m : trunk_messages) {
                result.push_back(convert_trunk_message(m));
            }

            if (msg->type() == -1) {
                BOOST_LOG_TRIVIAL(error) << "[" << system->get_short_name() << "]\t process_data_unit timeout";
            }

            msg.reset();
            msg = system->get_msg_queue()->delete_head_nowait();
        }
    }

    return result;
}

void tr_context_process_conventional_queues(TrContext &ctx) {
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        for (auto &ar : sys->get_conventional_recorders()) {
            ar->process_message_queues();
        }
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
            BOOST_LOG_TRIVIAL(error) << "Source " << source->get_num() << " has stopped receiving samples - Terminating trunk recorder";
            throw std::runtime_error("Source stopped receiving samples");
        }
    }
}

void tr_context_print_status(TrContext &ctx, rust::Slice<const TrCall *const> calls) {
    auto &sources = ctx.impl->trunk_ctx.sources;
    auto &systems = ctx.impl->trunk_ctx.systems;

    BOOST_LOG_TRIVIAL(info) << "Active Calls: " << calls.size();
    for (auto *tr_call : calls) {
        auto &call = tr_call->ptr;
        auto recorder = call->get_recorder();
        std::string loghdr = log_header(call->get_short_name(), call->get_call_num(), call->get_talkgroup(), call->get_freq());
        if (call->get_state() == MONITORING) {
            BOOST_LOG_TRIVIAL(info) << loghdr << "Elapsed: " << std::setw(4) << call->elapsed().count() << " State: " << format_state(call->get_state(), call->get_monitoring_state());
        } else {
            if (call->is_conventional()) {
                bool is_enabled = call->get_recorder()->is_enabled();
                BOOST_LOG_TRIVIAL(info) << loghdr << "Elapsed: " << std::setw(4) << call->elapsed().count() << " State: " << format_state(call->get_state()) << " Enabled: " << is_enabled;
            } else {
                BOOST_LOG_TRIVIAL(info) << loghdr << "Elapsed: " << std::setw(4) << call->elapsed().count() << " State: " << format_state(call->get_state());
            }
        }
        if (recorder) {
            BOOST_LOG_TRIVIAL(info) << "\t[ " << std::setw(2) << recorder->get_num() << " ] State: " << format_state(recorder->get_state());
        }
    }

    BOOST_LOG_TRIVIAL(info) << "Active Patches: ";
    for (auto &sys : systems) {
        sys->print_active_talkgroup_patches();
    }

    BOOST_LOG_TRIVIAL(info) << "Control Channel Decode Rates: ";
    for (auto &sys : systems) {
        if (!is_conventional(sys->get_system_type())) {
            BOOST_LOG_TRIVIAL(info) << "[" << sys->get_short_name() << "]\t" << format_freq(sys->get_current_control_channel()) << "\t" << sys->get_decode_rate() << " msg/sec";
            if ((sys->get_source()->get_autotune_source()) && (sys->get_system_type() == SYS_P25)) {
                autotune_control_channel(sys);
            }
        }
    }

    BOOST_LOG_TRIVIAL(info) << "Recorders: ";
    for (auto &source : sources) {
        source->print_recorders();
    }
}

// ── Conventional call management — recorder status (null-guarded) ──

bool call_has_recorder(const TrCall &call) {
    return call.ptr->get_recorder() != nullptr;
}

bool call_get_recorder_is_active(const TrCall &call) {
    auto rec = call.ptr->get_recorder();
    return rec ? rec->is_active() : false;
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

// ── Conventional call management — squelch event drain ──

rust::Vec<TrSquelchEvent> call_drain_squelch_events(const TrCall &call) {
    rust::Vec<TrSquelchEvent> result;
    auto rec = call.ptr->get_recorder();
    if (!rec) return result;
    auto events = rec->drain_squelch_events();
    for (const auto &ev : events) {
        TrSquelchEvent out;
        out.call_num = call.ptr->get_call_num();
        out.muted = ev.muted;
        out.pwr_db = ev.pwr_db;
        result.push_back(out);
    }
    return result;
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

uint64_t call_system_get_preferred_nac(const TrCall &call, int64_t talkgroup) {
    auto tg = call.ptr->get_system()->find_talkgroup(talkgroup);
    if (tg) {
        return tg->get_preferredNAC();
    }
    return 0;
}

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
    auto sys = c->get_system();
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
    m.multi_site = sys ? sys->get_multiSite() : false;
    m.wacn = sys ? sys->get_wacn() : 0;
    m.rfss = sys ? sys->get_sys_rfss() : 0;
    m.site_id = sys ? sys->get_sys_site_id() : 0;
    m.multi_site_name = rust::String(sys ? sys->get_multiSiteSystemName() : "");
    m.conversation_mode = sys ? sys->get_conversation_mode() : false;
    m.max_duration = sys ? sys->get_max_duration() : 0.0;
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
    if (recorder->get_type() == P25 || recorder->get_type() == P25C) {
        auto p25_rec = std::dynamic_pointer_cast<p25_recorder>(recorder);
        if (p25_rec && p25_rec->is_active()) {
            p25_rec->process_message_queues();
        }
    }
}

// ── Phase C: Call Management — system lookup by sys_num ──

rust::String tr_context_get_system_multi_site_name(const TrContext &ctx, int32_t sys_num) {
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        if (sys->get_sys_num() == sys_num) {
            return rust::String(sys->get_multiSiteSystemName());
        }
    }
    return rust::String("");
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
