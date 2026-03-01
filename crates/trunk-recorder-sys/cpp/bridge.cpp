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

    int unit_alias_discovered(const std::shared_ptr<System> &sys, const OTAAlias &alias) {
        TrSystem tr_sys(sys);
        return rust_plugin_unit_alias_discovered(
            *plugin_, tr_sys, alias.radio_id,
            rust::Str(alias.alias.data(), alias.alias.size()),
            rust::Str(alias.source.data(), alias.source.size()),
            alias.talkgroup_id);
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

    void unit_alias_discovered(const std::shared_ptr<System> &system,
                               const OTAAlias &alias) override {
        bridge_->unit_alias_discovered(system, alias);
    }

private:
    std::shared_ptr<RustPluginBridge> bridge_;
};

// ── TrContext pimpl implementation ──

struct TrContextImpl {
    TrunkContext trunk_ctx;
    std::shared_ptr<RustPluginBridge> bridge;

    // Parsers — created eagerly during tr_finalize
    std::map<int, std::unique_ptr<SmartnetParser>> smartnet_parsers;
    std::unique_ptr<P25Parser> p25_parser;

};

TrContext::TrContext() : impl(std::make_unique<TrContextImpl>()) {}
TrContext::~TrContext() = default;
TrContext::TrContext(TrContext &&) noexcept = default;
TrContext &TrContext::operator=(TrContext &&) noexcept = default;

// ── Lifecycle function implementations ──

// ── Builder: create empty context ──

std::unique_ptr<TrContext> tr_create_from_builder(rust::Box<RustPluginBox> plugin) {
    auto ctx = std::make_unique<TrContext>();
    ctx->impl->bridge = std::make_shared<RustPluginBridge>(std::move(plugin));
    ctx->impl->trunk_ctx.config.event_sink = std::make_shared<CxxEventSink>(ctx->impl->bridge);
    ctx->impl->trunk_ctx.tb = gr::make_top_block("Trunking");
    return ctx;
}

// ── Builder: finalize (setup_systems + plugin callbacks) ──

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

    ctx.impl->bridge->setup_config(ctx.impl->trunk_ctx.sources,
                                   ctx.impl->trunk_ctx.systems);
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        ctx.impl->bridge->setup_system(sys);
    }
    ctx.impl->bridge->start();
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
    auto &sources = ctx.impl->trunk_ctx.sources;
    auto &tb = ctx.impl->trunk_ctx.tb;

    for (auto &system : systems) {
        if (system->get_system_type() != SYS_P25 && system->get_system_type() != SYS_SMARTNET) {
            continue;
        }

        gr::message::sptr msg;
        msg = system->get_msg_queue()->delete_head_nowait();
        while (msg != nullptr) {
            system->set_message_count(system->get_message_count() + 1);

            std::vector<TrunkMessage> trunk_messages;

            if (system->get_system_type() == SYS_SMARTNET) {
                auto &smartnet_parser = ctx.impl->smartnet_parsers[system->get_sys_num()];
                trunk_messages = smartnet_parser->parse_message(msg, system);
            } else if (system->get_system_type() == SYS_P25) {
                trunk_messages = ctx.impl->p25_parser->parse_message(msg, system);
            }

            // Handle C++-internal message types and fire plugin notifications
            for (auto &message : trunk_messages) {
                switch (message.message_type) {
                case CONTROL_CHANNEL:
                    system->add_control_channel(message.freq);
                    break;
                case PATCH_ADD:
                    system->update_active_talkgroup_patches(message.patch_data);
                    break;
                case PATCH_DELETE:
                    system->delete_talkgroup_patch(message.patch_data);
                    break;
                case TDULC:
                    system->retune_trunking(tb, sources);
                    if (system->get_source() && system->get_source()->get_autotune_source() && system->get_system_type() == SYS_P25) {
                        autotune_control_channel(system, false);
                    }
                    break;
                case INVALID_CC_MESSAGE:
                {
                    int msg_count = system->get_message_count();
                    if (msg_count > 1) {
                        system->set_message_count(msg_count - 1);
                    }
                    break;
                }
                case REGISTRATION:
                    config.event_sink->unit_registration(system, message.source);
                    break;
                case DEREGISTRATION:
                    config.event_sink->unit_deregistration(system, message.source);
                    break;
                case AFFILIATION:
                    config.event_sink->unit_group_affiliation(system, message.source, message.talkgroup);
                    break;
                case ACKNOWLEDGE:
                    config.event_sink->unit_acknowledge_response(system, message.source);
                    break;
                case LOCATION:
                    config.event_sink->unit_location(system, message.source, message.talkgroup);
                    break;
                case DATA_GRANT:
                    config.event_sink->unit_data_grant(system, message.source);
                    break;
                case UU_ANS_REQ:
                    config.event_sink->unit_answer_request(system, message.source, message.talkgroup);
                    break;
                case SYSID:
                    if ((system->get_system_type() == SYS_P25) || (system->get_system_type() == SYS_CONVENTIONAL_P25)) {
                        if (system->update_sysid(message)) {
                            config.event_sink->setup_system(system);
                        }
                    }
                    break;
                case STATUS:
                    if (system->update_status(message)) {
                        config.event_sink->setup_system(system);
                    }
                    break;
                default:
                    break;
                }
            }

            // Fire trunk_message plugin notification for all messages
            config.event_sink->trunk_message(trunk_messages, system);

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

void tr_context_check_decode_rates(TrContext &ctx, float time_diff) {
    auto &config = ctx.impl->trunk_ctx.config;
    auto &tb = ctx.impl->trunk_ctx.tb;
    auto &sources = ctx.impl->trunk_ctx.sources;
    auto &systems = ctx.impl->trunk_ctx.systems;

    config.event_sink->setup_config(sources, systems);
    config.event_sink->system_rates(systems, time_diff);

    for (auto &sys : systems) {
        if (!is_conventional(sys->get_system_type())) {
            int msgs_decoded_per_second = std::floor(sys->get_message_count() / time_diff);
            sys->set_decode_rate(msgs_decoded_per_second);

            if (msgs_decoded_per_second < 2) {
                if (config.control_retune_limit > 0) {
                    sys->set_retune_attempts(sys->get_retune_attempts() + 1);
                    if (sys->get_retune_attempts() > config.control_retune_limit) {
                        BOOST_LOG_TRIVIAL(error) << "[" << sys->get_short_name() << "]\t"
                                                 << "Control channel retune limit exceeded after " << sys->get_retune_attempts() << " tries - Terminating trunk recorder";
                        throw std::runtime_error("Control channel retune limit exceeded");
                    }
                }
                if (sys->control_channel_count() > 1) {
                    sys->retune_trunking(tb, sources);
                    if (sys->get_source() && sys->get_source()->get_autotune_source() && sys->get_system_type() == SYS_P25) {
                        autotune_control_channel(sys, false);
                    }
                } else {
                    BOOST_LOG_TRIVIAL(error) << "[" << sys->get_short_name() << "]\tThere is only one control channel defined";
                }
            } else {
                sys->set_retune_attempts(0);
            }

            if (msgs_decoded_per_second < config.control_message_warn_rate) {
                BOOST_LOG_TRIVIAL(error) << "[" << sys->get_short_name() << "]\tfreq: " << format_freq(sys->get_current_control_channel()) << "\tControl Channel Message Decode Rate: " << msgs_decoded_per_second << "/sec, count:  " << sys->get_message_count();
            } else if (config.control_message_warn_rate == -1) {
                BOOST_LOG_TRIVIAL(info) << "[" << sys->get_short_name() << "]\tfreq: " << format_freq(sys->get_current_control_channel()) << "\tControl Channel Message Decode Rate: " << msgs_decoded_per_second << "/sec, count:  " << sys->get_message_count();
            }
        }
        sys->set_message_count(0);
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

void tr_context_clear_stale_patches(TrContext &ctx) {
    for (auto &system : ctx.impl->trunk_ctx.systems) {
        if (system->get_system_type() == SYS_P25) {
            system->clear_stale_talkgroup_patches();
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

// ── Conventional call management — signal/noise ──

void call_set_signal(TrCall &call, double s) {
    call.ptr->set_signal(s);
}

void call_set_noise(TrCall &call, double n) {
    call.ptr->set_noise(n);
}

// ── Conventional call management — system info ──

double call_get_system_max_duration(const TrCall &call) {
    return call.ptr->get_system()->get_max_duration();
}

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

// ── Phase C: Call Management — additional call accessors ──

double call_since_last_update_secs(const TrCall &call) {
    return call.ptr->since_last_update().count();
}

bool call_get_system_multi_site(const TrCall &call) {
    return call.ptr->get_system()->get_multiSite();
}

uint64_t call_get_system_wacn(const TrCall &call) {
    return call.ptr->get_system()->get_wacn();
}

uint64_t call_get_system_nac(const TrCall &call) {
    return call.ptr->get_system()->get_nac();
}

int32_t call_get_system_rfss(const TrCall &call) {
    return call.ptr->get_system()->get_sys_rfss();
}

int32_t call_get_system_site_id(const TrCall &call) {
    return call.ptr->get_system()->get_sys_site_id();
}

rust::String call_get_system_multi_site_name(const TrCall &call) {
    return rust::String(call.ptr->get_system()->get_multiSiteSystemName());
}

uint64_t call_get_system_multi_site_number(const TrCall &call) {
    return call.ptr->get_system()->get_multiSiteSystemNumber();
}

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

bool tr_call_update(TrCall &call, const TrTrunkMessage &msg) {
    auto cpp_msg = convert_trunk_message_back(msg);
    return call.ptr->update(cpp_msg);
}

void tr_call_set_state(TrCall &call, TrState state) {
    call.ptr->set_state(convert_state_back(state));
}

void tr_call_set_monitoring_state(TrCall &call, TrMonitoringState state) {
    call.ptr->set_monitoring_state(convert_monitoring_state_back(state));
}

void tr_call_set_encrypted(TrCall &call, bool encrypted) {
    call.ptr->set_encrypted(encrypted);
}

void call_set_is_analog(TrCall &call, bool is_analog) {
    call.ptr->set_is_analog(is_analog);
}

rust::Vec<int64_t> tr_context_get_talkgroup_patch(
    const TrContext &ctx, int32_t sys_num, int64_t talkgroup) {
    rust::Vec<int64_t> result;
    for (auto &sys : ctx.impl->trunk_ctx.systems) {
        if (sys->get_sys_num() == sys_num) {
            for (auto &tg_id : sys->get_talkgroup_patch(talkgroup)) {
                result.push_back(tg_id);
            }
            break;
        }
    }
    return result;
}

TrAcquireResult tr_context_acquire_recorder(TrContext &ctx, TrCall &call, bool is_analog, int32_t priority) {
    auto &config = ctx.impl->trunk_ctx.config;
    auto &sources = ctx.impl->trunk_ctx.sources;

    for (auto &source : sources) {
        if (source->get_min_hz() > call.ptr->get_freq() ||
            source->get_max_hz() < call.ptr->get_freq()) {
            continue;
        }

        // Source covers this frequency
        int32_t available = is_analog
            ? source->get_num_available_analog_recorders()
            : source->get_num_available_digital_recorders();

        // Priority check: -1 means disabled, priority > available means insufficient recorders
        if (priority == -1 || priority > available) {
            TrAcquireResult res;
            res.started = false;
            res.available = available;
            return res;
        }

        // Allocate a recorder
        std::shared_ptr<Recorder> recorder = is_analog
            ? source->get_analog_recorder(call.ptr)
            : source->get_digital_recorder(call.ptr);

        if (!recorder) {
            TrAcquireResult res;
            res.started = false;
            res.available = available;
            return res;
        }

        if (!recorder->start(call.ptr)) {
            TrAcquireResult res;
            res.started = false;
            res.available = available;
            return res;
        }

        call.ptr->set_recorder(recorder);
        call.ptr->set_state(RECORDING);
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

        TrAcquireResult res;
        res.started = true;
        res.available = available;
        return res;
    }

    // No source covers this frequency
    TrAcquireResult res;
    res.started = false;
    res.available = -1;
    return res;
}

void tr_call_conclude(TrCall &call) {
    call.ptr->conclude_call();
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

// ── Phase C: Call Management — event firing ──

void tr_context_fire_call_start(TrContext &ctx, const TrCall &call) {
    ctx.impl->trunk_ctx.config.event_sink->call_start(call.ptr);
}

void tr_context_fire_calls_active(TrContext &ctx, rust::Slice<const TrCall *const> calls) {
    std::vector<std::shared_ptr<Call>> call_ptrs;
    call_ptrs.reserve(calls.size());
    for (auto *c : calls) {
        call_ptrs.push_back(c->ptr);
    }
    ctx.impl->trunk_ctx.config.event_sink->calls_active(std::move(call_ptrs));
}

void tr_context_fire_setup_recorder(TrContext &ctx, const TrCall &call) {
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
