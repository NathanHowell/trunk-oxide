// Our bridge header (includes trunk-recorder headers and rust/cxx.h)
#include "trunk-recorder-sys/cpp/bridge.h"

// cxx-generated header for the Rust bridge
#include "trunk-recorder-sys/src/lib.rs.h"

#include <cstdlib>
#include <cstring>
#include <json.hpp>
using json = nlohmann::json;

#ifdef TR_LIBRARY_MODE
#include "trunk-recorder/trunk_context.h"
#include "trunk-recorder/event_sink.h"
#include <thread>

// Forward declarations for functions provided by libtrunk-recorder.so
bool load_config_from_string(const std::string &json_body, Config &config,
                             gr::top_block_sptr &tb, std::vector<std::shared_ptr<Source>> &sources,
                             std::vector<std::shared_ptr<System>> &systems);
bool setup_systems(Config &config, gr::top_block_sptr &tb,
                   std::vector<std::shared_ptr<Source>> &sources, std::vector<std::shared_ptr<System>> &systems,
                   std::vector<std::shared_ptr<Call>> &calls);
int monitor_messages(TrunkContext &ctx);
#endif

namespace trunk_oxide {

// ── Enum conversion helpers ──

static TrSignalType convert_signal_type(gr::blocks::SignalType st) {
    switch (st) {
        case gr::blocks::Normal: return TrSignalType::Normal;
        case gr::blocks::Emergency: return TrSignalType::Emergency;
        case gr::blocks::EmergencyAck: return TrSignalType::EmergencyAck;
        case gr::blocks::RadioCheck: return TrSignalType::RadioCheck;
        case gr::blocks::RadioCheckAck: return TrSignalType::RadioCheckAck;
        case gr::blocks::RadioStun: return TrSignalType::RadioStun;
        case gr::blocks::RadioStunAck: return TrSignalType::RadioStunAck;
        case gr::blocks::RadioRevive: return TrSignalType::RadioRevive;
        case gr::blocks::RadioReviveAck: return TrSignalType::RadioReviveAck;
        case gr::blocks::NormalPre: return TrSignalType::NormalPre;
        case gr::blocks::EmergencyPre: return TrSignalType::EmergencyPre;
        default: return TrSignalType::Normal;
    }
}

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

static TrState convert_state(State s) {
    switch (s) {
        case MONITORING: return TrState::Monitoring;
        case RECORDING: return TrState::Recording;
        case INACTIVE: return TrState::Inactive;
        case ACTIVE: return TrState::Active;
        case IDLE: return TrState::Idle;
        case STOPPED: return TrState::Stopped;
        case AVAILABLE: return TrState::Available;
        case IGNORE: return TrState::Ignore;
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

static TrCallDataStatus convert_call_data_status(Call_Data_Status s) {
    switch (s) {
        case INITIAL: return TrCallDataStatus::Initial;
        case SUCCESS: return TrCallDataStatus::Success;
        case RETRY: return TrCallDataStatus::Retry;
        case FAILED: return TrCallDataStatus::Failed;
        default: return TrCallDataStatus::Initial;
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

static TrCallSource convert_call_source(const Call_Source &cs) {
    TrCallSource out;
    out.source = cs.source;
    out.position = cs.position;
    out.emergency = cs.emergency;
    out.signal_system = rust::String(cs.signal_system);
    out.tag = rust::String(cs.tag);
    return out;
}

static TrCallError convert_call_error(const Call_Error &ce) {
    TrCallError out;
    out.position = ce.position;
    out.total_len = ce.total_len;
    out.error_count = ce.error_count;
    out.spike_count = ce.spike_count;
    return out;
}

static TrCallData convert_call_data(const Call_Data_t &cd) {
    TrCallData out;
    out.talkgroup = cd.talkgroup;
    out.color_code = cd.color_code;

    rust::Vec<uint64_t> patched;
    for (auto tg : cd.patched_talkgroups) {
        patched.push_back(tg);
    }
    out.patched_talkgroups = std::move(patched);

    out.talkgroup_tag = rust::String(cd.talkgroup_tag);
    out.talkgroup_alpha_tag = rust::String(cd.talkgroup_alpha_tag);
    out.talkgroup_description = rust::String(cd.talkgroup_description);
    out.talkgroup_display = rust::String(cd.talkgroup_display);
    out.talkgroup_group = rust::String(cd.talkgroup_group);
    out.call_num = cd.call_num;
    out.freq = cd.freq;
    out.freq_error = cd.freq_error;
    out.source_num = cd.source_num;
    out.recorder_num = cd.recorder_num;
    out.signal = cd.signal;
    out.noise = cd.noise;
    out.start_time = cd.start_time;
    out.stop_time = cd.stop_time;
    out.error_count = cd.error_count;
    out.spike_count = cd.spike_count;
    out.encrypted = cd.encrypted;
    out.emergency = cd.emergency;
    out.priority = cd.priority;
    out.mode = cd.mode;
    out.duplex = cd.duplex;
    out.sys_num = cd.sys_num;
    out.short_name = rust::String(cd.short_name);
    out.audio_type = rust::String(cd.audio_type);
    out.tdma_slot = cd.tdma_slot;
    out.length = cd.length;
    out.phase2_tdma = cd.phase2_tdma;

    rust::Vec<TrCallSource> sources;
    for (const auto &s : cd.transmission_source_list) {
        sources.push_back(convert_call_source(s));
    }
    out.transmission_source_list = std::move(sources);

    rust::Vec<TrCallError> errors;
    for (const auto &e : cd.transmission_error_list) {
        errors.push_back(convert_call_error(e));
    }
    out.transmission_error_list = std::move(errors);

    rust::Vec<TrTransmission> transmissions;
    for (const auto &t : cd.transmission_list) {
        transmissions.push_back(convert_transmission(t));
    }
    out.transmission_list = std::move(transmissions);

    out.status = convert_call_data_status(cd.status);
    return out;
}

// ── RustPluginBridge: delegates all plugin callbacks to Rust via FFI ──

class RustPluginBridge {
public:
    explicit RustPluginBridge(rust::Box<RustPluginBox> plugin)
        : plugin_(std::move(plugin)) {}

    int parse_config(json config_data) {
        std::string json_str = config_data.dump();
        return rust_plugin_parse_config(*plugin_, rust::Str(json_str.data(), json_str.size()));
    }

    int start() {
        return rust_plugin_start(*plugin_);
    }

    int stop() {
        return rust_plugin_stop(*plugin_);
    }

    int poll_one() {
        return rust_plugin_poll_one(*plugin_);
    }

    int signal(long unitId, const char *signaling_type,
               gr::blocks::SignalType sig_type,
               const std::shared_ptr<Call> &call, const std::shared_ptr<System> &system, const std::shared_ptr<Recorder> &recorder) {
        auto sig_str = signaling_type
            ? rust::Str(signaling_type, std::strlen(signaling_type))
            : rust::Str("", 0);
        TrCall tr_call(call);
        TrSystem tr_sys(system);
        TrRecorder tr_rec(recorder);
        return rust_plugin_signal(
            *plugin_, unitId, sig_str,
            convert_signal_type(sig_type),
            call ? &tr_call : nullptr,
            system ? &tr_sys : nullptr,
            recorder ? &tr_rec : nullptr);
    }

    int audio_stream(const std::shared_ptr<Call> &call, const std::shared_ptr<Recorder> &recorder,
                     int16_t *samples, int sampleCount) {
        TrCall tr_call(call);
        TrRecorder tr_rec(recorder);
        rust::Slice<const int16_t> slice(samples, static_cast<size_t>(sampleCount));
        return rust_plugin_audio_stream(
            *plugin_,
            call ? &tr_call : nullptr,
            tr_rec, slice);
    }

    int call_start(const std::shared_ptr<Call> &call) {
        TrCall tr_call(call);
        return rust_plugin_call_start(*plugin_, tr_call);
    }

    int call_end(Call_Data_t call_info) {
        TrCallData data = convert_call_data(call_info);
        return rust_plugin_call_end(*plugin_, data);
    }

    int calls_active(std::vector<std::shared_ptr<Call>> calls) {
        // Convert vector<shared_ptr<Call>> to vector of TrCall, then pass pointers
        std::vector<TrCall> wrappers;
        wrappers.reserve(calls.size());
        for (auto &c : calls) {
            wrappers.emplace_back(c);
        }
        std::vector<const TrCall *> ptrs;
        ptrs.reserve(wrappers.size());
        for (auto &w : wrappers) {
            ptrs.push_back(&w);
        }
        rust::Slice<const TrCall *const> slice(ptrs.data(), ptrs.size());
        return rust_plugin_calls_active(*plugin_, slice);
    }

    int setup_recorder(const std::shared_ptr<Recorder> &recorder) {
        TrRecorder tr_rec(recorder);
        return rust_plugin_setup_recorder(*plugin_, tr_rec);
    }

    int setup_system(const std::shared_ptr<System> &system) {
        TrSystem tr_sys(system);
        return rust_plugin_setup_system(*plugin_, tr_sys);
    }

    int setup_systems(std::vector<std::shared_ptr<System>> systems) {
        std::vector<TrSystem> wrappers;
        wrappers.reserve(systems.size());
        for (auto &s : systems) {
            wrappers.emplace_back(s);
        }
        std::vector<const TrSystem *> ptrs;
        ptrs.reserve(wrappers.size());
        for (auto &w : wrappers) {
            ptrs.push_back(&w);
        }
        rust::Slice<const TrSystem *const> slice(ptrs.data(), ptrs.size());
        return rust_plugin_setup_systems(*plugin_, slice);
    }

    int setup_sources(std::vector<std::shared_ptr<Source>> /*sources*/) {
        // Source has no virtual methods — we cannot safely call its accessors
        // from a dlopen'd plugin (the symbols aren't exported by trunk-recorder).
        return rust_plugin_setup_sources(*plugin_);
    }

    int setup_config(std::vector<std::shared_ptr<Source>> /*sources*/,
                     std::vector<std::shared_ptr<System>> systems) {
        // Source data omitted (non-virtual, see above); forward systems only.
        std::vector<TrSystem> sys_wrappers;
        sys_wrappers.reserve(systems.size());
        for (auto &s : systems) {
            sys_wrappers.emplace_back(s);
        }
        std::vector<const TrSystem *> sys_ptrs;
        sys_ptrs.reserve(sys_wrappers.size());
        for (auto &w : sys_wrappers) {
            sys_ptrs.push_back(&w);
        }

        rust::Slice<const TrSystem *const> sys_slice(sys_ptrs.data(), sys_ptrs.size());
        return rust_plugin_setup_config(*plugin_, sys_slice);
    }

    int trunk_message(std::vector<TrunkMessage> messages,
                      const std::shared_ptr<System> &system) {
        std::vector<TrTrunkMessage> msgs;
        msgs.reserve(messages.size());
        for (const auto &m : messages) {
            msgs.push_back(convert_trunk_message(m));
        }
        TrSystem tr_sys(system);
        return rust_plugin_trunk_message(*plugin_,
            rust::Slice<const TrTrunkMessage>(msgs.data(), msgs.size()),
            tr_sys);
    }

    int system_rates(std::vector<std::shared_ptr<System>> systems, float timeDiff) {
        std::vector<TrSystem> wrappers;
        wrappers.reserve(systems.size());
        for (auto &s : systems) {
            wrappers.emplace_back(s);
        }
        std::vector<const TrSystem *> ptrs;
        ptrs.reserve(wrappers.size());
        for (auto &w : wrappers) {
            ptrs.push_back(&w);
        }
        rust::Slice<const TrSystem *const> slice(ptrs.data(), ptrs.size());
        return rust_plugin_system_rates(*plugin_, slice, timeDiff);
    }

    int unit_registration(const std::shared_ptr<System> &sys, long source_id) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_registration(*plugin_, tr_sys, source_id);
    }

    int unit_deregistration(const std::shared_ptr<System> &sys, long source_id) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_deregistration(*plugin_, tr_sys, source_id);
    }

    int unit_acknowledge_response(const std::shared_ptr<System> &sys, long source_id) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_acknowledge_response(*plugin_, tr_sys, source_id);
    }

    int unit_group_affiliation(const std::shared_ptr<System> &sys, long source_id,
                                long talkgroup_num) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_group_affiliation(
            *plugin_, tr_sys, source_id, talkgroup_num);
    }

    int unit_data_grant(const std::shared_ptr<System> &sys, long source_id) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_data_grant(*plugin_, tr_sys, source_id);
    }

    int unit_answer_request(const std::shared_ptr<System> &sys, long source_id,
                            long talkgroup) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_answer_request(
            *plugin_, tr_sys, source_id, talkgroup);
    }

    int unit_location(const std::shared_ptr<System> &sys, long source_id,
                      long talkgroup_num) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_location(*plugin_, tr_sys, source_id, talkgroup_num);
    }

private:
    rust::Box<RustPluginBox> plugin_;
};

// ── Call accessor implementations ──

int64_t call_get_talkgroup(const TrCall &call) { return call.ptr->get_talkgroup(); }
int64_t call_get_call_num(const TrCall &call) { return call.ptr->get_call_num(); }
int32_t call_get_sys_num(const TrCall &call) { return call.ptr->get_sys_num(); }
double call_get_freq(const TrCall &call) { return call.ptr->get_freq(); }
double call_get_signal(const TrCall &call) { return call.ptr->get_signal(); }
double call_get_noise(const TrCall &call) { return call.ptr->get_noise(); }
int32_t call_get_freq_error(const TrCall &call) { return call.ptr->get_freq_error(); }
bool call_get_encrypted(const TrCall &call) { return call.ptr->get_encrypted(); }
bool call_get_emergency(const TrCall &call) { return call.ptr->get_emergency(); }
bool call_get_is_analog(const TrCall &call) { return call.ptr->get_is_analog(); }
bool call_is_conventional(const TrCall &call) { return call.ptr->is_conventional(); }
bool call_get_phase2_tdma(const TrCall &call) { return call.ptr->get_phase2_tdma(); }
int32_t call_get_tdma_slot(const TrCall &call) { return call.ptr->get_tdma_slot(); }
int32_t call_get_priority(const TrCall &call) { return call.ptr->get_priority(); }
bool call_get_mode(const TrCall &call) { return call.ptr->get_mode(); }
bool call_get_duplex(const TrCall &call) { return call.ptr->get_duplex(); }
bool call_get_conversation_mode(const TrCall &call) { return call.ptr->get_conversation_mode(); }
int64_t call_get_current_source_id(const TrCall &call) { return call.ptr->get_current_source_id(); }
double call_get_current_length(const TrCall &call) { return call.ptr->get_current_length(); }
int64_t call_get_start_time(const TrCall &call) { return static_cast<int64_t>(call.ptr->get_start_time()); }
int64_t call_elapsed(const TrCall &call) { return static_cast<int64_t>(call.ptr->elapsed().count()); }
rust::String call_get_short_name(const TrCall &call) { return rust::String(call.ptr->get_short_name()); }
rust::String call_get_talkgroup_display(const TrCall &call) { return rust::String(call.ptr->get_talkgroup_display()); }
rust::String call_get_talkgroup_tag(const TrCall &call) { return rust::String(call.ptr->get_talkgroup_tag()); }
TrState call_get_state(const TrCall &call) { return convert_state(call.ptr->get_state()); }
TrMonitoringState call_get_monitoring_state(const TrCall &call) { return convert_monitoring_state(call.ptr->get_monitoring_state()); }

// ── System accessor implementations ──

int32_t system_get_sys_num(const TrSystem &sys) { return sys.ptr->get_sys_num(); }
uint64_t system_get_sys_id(const TrSystem &sys) { return sys.ptr->get_sys_id(); }
rust::String system_get_short_name(const TrSystem &sys) { return rust::String(sys.ptr->get_short_name()); }
int32_t system_get_system_type(const TrSystem &sys) {
    // Map C++ SystemType enum (0-based) to proto SystemType enum (1-based, 0 = UNSPECIFIED)
    return static_cast<int32_t>(sys.ptr->get_system_type()) + 1;
}
uint64_t system_get_wacn(const TrSystem &sys) { return sys.ptr->get_wacn(); }
uint64_t system_get_nac(const TrSystem &sys) { return sys.ptr->get_nac(); }
int32_t system_get_sys_rfss(const TrSystem &sys) { return sys.ptr->get_sys_rfss(); }
int32_t system_get_sys_site_id(const TrSystem &sys) { return sys.ptr->get_sys_site_id(); }
rust::String system_get_bandplan(const TrSystem &sys) { return rust::String(sys.ptr->get_bandplan()); }
double system_get_analog_levels(const TrSystem &sys) { return sys.ptr->get_analog_levels(); }
double system_get_digital_levels(const TrSystem &sys) { return sys.ptr->get_digital_levels(); }
double system_get_squelch_db(const TrSystem &sys) { return sys.ptr->get_squelch_db(); }
bool system_get_conversation_mode(const TrSystem &sys) { return sys.ptr->get_conversation_mode(); }
int32_t system_get_message_count(const TrSystem &sys) { return sys.ptr->get_message_count(); }
int32_t system_get_decode_rate(const TrSystem &sys) { return sys.ptr->get_decode_rate(); }

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

// ── CxxEventSink: EventSink subclass that delegates to RustPluginBridge ──
// Used in library mode to route trunk-recorder events to Rust callbacks.

#ifdef TR_LIBRARY_MODE
class CxxEventSink : public EventSink {
public:
    explicit CxxEventSink(std::shared_ptr<RustPluginBridge> bridge)
        : bridge_(std::move(bridge)) {}

    void audio_callback(const std::shared_ptr<Call> &call, const std::shared_ptr<Recorder> &recorder,
                        int16_t *samples, int sampleCount) override {
        bridge_->audio_stream(call, recorder, samples, sampleCount);
    }

    void poll_one() override {
        bridge_->poll_one();
    }

    int signal(long unitId, const char *signaling_type,
               gr::blocks::SignalType sig_type, const std::shared_ptr<Call> &call,
               const std::shared_ptr<System> &system, const std::shared_ptr<Recorder> &recorder) override {
        return bridge_->signal(unitId, signaling_type, sig_type, call, system, recorder);
    }

    void trunk_message(std::vector<TrunkMessage> messages,
                       const std::shared_ptr<System> &system) override {
        bridge_->trunk_message(std::move(messages), system);
    }

    void call_start(const std::shared_ptr<Call> &call) override {
        bridge_->call_start(call);
    }

    void conclude_call(const std::shared_ptr<Call> &call, const std::shared_ptr<System> &sys, Config /*config*/) override {
        Call_Data_t call_info = {};
        call_info.status = INITIAL;
        call_info.error_count = 0;
        call_info.spike_count = 0;
        call_info.freq = call->get_freq();
        call_info.freq_error = call->get_freq_error();
        call_info.signal = call->get_signal();
        call_info.noise = call->get_noise();
        if (call->get_recorder()) {
            call_info.recorder_num = call->get_recorder()->get_num();
        }
        call_info.encrypted = call->get_encrypted();
        call_info.emergency = call->get_emergency();
        call_info.priority = call->get_priority();
        call_info.mode = call->get_mode();
        call_info.duplex = call->get_duplex();
        call_info.tdma_slot = call->get_tdma_slot();
        call_info.phase2_tdma = call->get_phase2_tdma();
        call_info.transmission_list = call->get_transmissions();
        call_info.sys_num = sys->get_sys_num();
        call_info.short_name = sys->get_short_name();
        call_info.call_num = call->get_call_num();
        call_info.talkgroup = call->get_talkgroup();
        call_info.talkgroup_display = call->get_talkgroup_display();
        call_info.talkgroup_tag = call->get_talkgroup_tag();
        call_info.patched_talkgroups = sys->get_talkgroup_patch(call_info.talkgroup);
        call_info.color_code = 0;
        call_info.start_time = call->get_start_time();
        call_info.stop_time = time(0);
        call_info.length = call->get_current_length();

        if (call->get_is_analog()) {
            call_info.audio_type = "analog";
        } else if (call->get_phase2_tdma()) {
            call_info.audio_type = "digital tdma";
        } else {
            call_info.audio_type = "digital";
        }

        bridge_->call_end(call_info);
    }

    void calls_active(std::vector<std::shared_ptr<Call>> calls) override {
        bridge_->calls_active(std::move(calls));
    }

    void setup_recorder(const std::shared_ptr<Recorder> &recorder) override {
        bridge_->setup_recorder(recorder);
    }

    void setup_system(const std::shared_ptr<System> &system) override {
        bridge_->setup_system(system);
    }

    void setup_config(std::vector<std::shared_ptr<Source>> sources,
                      std::vector<std::shared_ptr<System>> systems) override {
        bridge_->setup_config(std::move(sources), std::move(systems));
    }

    void system_rates(std::vector<std::shared_ptr<System>> systems, float timeDiff) override {
        bridge_->system_rates(std::move(systems), timeDiff);
    }

    void unit_registration(const std::shared_ptr<System> &system, long source_id) override {
        bridge_->unit_registration(system, source_id);
    }

    void unit_deregistration(const std::shared_ptr<System> &system, long source_id) override {
        bridge_->unit_deregistration(system, source_id);
    }

    void unit_acknowledge_response(const std::shared_ptr<System> &system, long source_id) override {
        bridge_->unit_acknowledge_response(system, source_id);
    }

    void unit_group_affiliation(const std::shared_ptr<System> &system, long source_id,
                                long talkgroup_num) override {
        bridge_->unit_group_affiliation(system, source_id, talkgroup_num);
    }

    void unit_data_grant(const std::shared_ptr<System> &system, long source_id) override {
        bridge_->unit_data_grant(system, source_id);
    }

    void unit_answer_request(const std::shared_ptr<System> &system, long source_id,
                             long talkgroup) override {
        bridge_->unit_answer_request(system, source_id, talkgroup);
    }

    void unit_location(const std::shared_ptr<System> &system, long source_id,
                       long talkgroup_num) override {
        bridge_->unit_location(system, source_id, talkgroup_num);
    }

private:
    std::shared_ptr<RustPluginBridge> bridge_;
};
#endif // TR_LIBRARY_MODE

// ── TrContext pimpl implementation ──

#ifdef TR_LIBRARY_MODE
struct TrContextImpl {
    TrunkContext trunk_ctx;
    std::thread monitor_thread;
    std::shared_ptr<RustPluginBridge> bridge;
};
#else
struct TrContextImpl {};
#endif

TrContext::TrContext() : impl(std::make_unique<TrContextImpl>()) {}
TrContext::~TrContext() = default;
TrContext::TrContext(TrContext &&) noexcept = default;
TrContext &TrContext::operator=(TrContext &&) noexcept = default;

// ── Lifecycle function implementations ──

#ifdef TR_LIBRARY_MODE

std::unique_ptr<TrContext> tr_create(rust::Str config_json,
                                     rust::Box<RustPluginBox> plugin) {
    auto ctx = std::make_unique<TrContext>();
    std::string json_str(config_json.data(), config_json.size());

    // Install the event sink before setup_systems(), which calls
    // event_sink->setup_recorder() / call_start() for conventional channels.
    ctx->impl->bridge = std::make_shared<RustPluginBridge>(std::move(plugin));
    ctx->impl->trunk_ctx.config.event_sink = std::make_shared<CxxEventSink>(ctx->impl->bridge);

    ctx->impl->trunk_ctx.tb = gr::make_top_block("Trunking");
    if (!load_config_from_string(json_str, ctx->impl->trunk_ctx.config,
                                 ctx->impl->trunk_ctx.tb,
                                 ctx->impl->trunk_ctx.sources,
                                 ctx->impl->trunk_ctx.systems)) {
        throw std::runtime_error("Failed to load config (source device open failure?)");
    }
    setup_systems(ctx->impl->trunk_ctx.config, ctx->impl->trunk_ctx.tb,
                  ctx->impl->trunk_ctx.sources, ctx->impl->trunk_ctx.systems,
                  ctx->impl->trunk_ctx.calls);

    // Notify the plugin of the current configuration.
    ctx->impl->bridge->setup_config(ctx->impl->trunk_ctx.sources,
                                    ctx->impl->trunk_ctx.systems);
    for (auto &sys : ctx->impl->trunk_ctx.systems) {
        ctx->impl->bridge->setup_system(sys);
    }

    // Start the Rust plugin (spawns the gRPC server).
    ctx->impl->bridge->start();

    return ctx;
}

void tr_start(TrContext &ctx) {
    ctx.impl->trunk_ctx.tb->start();
    ctx.impl->monitor_thread = std::thread([&ctx] {
        ctx.impl->trunk_ctx.exit_code = monitor_messages(ctx.impl->trunk_ctx);
    });
}

void tr_stop(TrContext &ctx) {
    ctx.impl->trunk_ctx.exit_flag = 1;
    if (ctx.impl->monitor_thread.joinable()) {
        ctx.impl->monitor_thread.join();
    }
    ctx.impl->trunk_ctx.tb->stop();
    ctx.impl->trunk_ctx.tb->wait();
}

#else // !TR_LIBRARY_MODE — stubs for dlopen plugin mode

std::unique_ptr<TrContext> tr_create(rust::Str /*config_json*/,
                                     rust::Box<RustPluginBox> /*plugin*/) {
    std::abort();
}

void tr_start(TrContext & /*ctx*/) {
    std::abort();
}

void tr_stop(TrContext & /*ctx*/) {
    std::abort();
}

#endif // TR_LIBRARY_MODE

} // namespace trunk_oxide
