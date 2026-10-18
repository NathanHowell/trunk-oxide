// Our bridge header (includes trunk-recorder headers and rust/cxx.h)
#include "trunk-recorder-sys/cpp/bridge.h"

// cxx-generated header for the Rust bridge
#include "trunk-recorder-sys/src/lib.rs.h"

#include <cassert>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <map>

#include <gnuradio/block.h>
#include <gnuradio/block_detail.h>
#include <gnuradio/top_block.h>
#include <gnuradio/blocks/head.h>
#include <gnuradio/blocks/skiphead.h>
#include <gnuradio/blocks/vector_sink.h>

#include <cctype>
#include <chrono>
#include <fstream>
#include <sstream>
#include <stdexcept>

#ifdef HAVE_OSMOSDR
#include <osmosdr/source.h>
#endif

#include "trunk-recorder/trunk_context.h"
#include "trunk-recorder/call_conventional.h"
#include "trunk-recorder/recorder_config.h"
#include "trunk-recorder/event_sink.h"
#include "trunk-recorder/formatter.h"
#include "trunk-recorder/recorders/dmr_recorder.h"
#include "trunk-recorder/source.h"
#include "trunk-recorder/systems/p25_parser.h"
#include "trunk-recorder/gr_blocks/symbol_regenerator.h"
#include "trunk-recorder/systems/smartnet_parser.h"
#include "trunk-recorder/systems/system.h"
#include "trunk-recorder/recorders/p25_recorder.h"

#include <boost/log/core.hpp>
#include <boost/log/expressions.hpp>
#include <boost/log/sinks.hpp>
#include <boost/log/trivial.hpp>

// Forward declarations for functions provided by libtrunk-recorder.so
bool setup_systems(Config &config, gr::top_block_sptr &tb,
                   std::vector<std::shared_ptr<Source>> &sources, std::vector<std::shared_ptr<System>> &systems,
                   std::vector<std::shared_ptr<Call>> &calls);

namespace trunk_oxide {

// ── Pre-flight gain calibration probe ──
//
// Opens an SDR source standalone (before the main flowgraph exists) and lets
// Rust sweep gain steps, capturing a block of raw IQ at each. The device stays
// open for the probe's lifetime; each capture builds a short-lived flowgraph
// (source → skiphead(settle) → head(N) → vector_sink) around the persistent
// source. All measurement/selection policy lives in Rust — this is pure
// acquisition. Compiled to throwing stubs when osmosdr is unavailable.

#ifdef HAVE_OSMOSDR
struct TrProbeImpl {
    osmosdr::source::sptr src;
    double rate = 0.0;
};
#else
struct TrProbeImpl {};
#endif

TrProbe::TrProbe() = default;
TrProbe::~TrProbe() = default;
TrProbe::TrProbe(TrProbe &&) noexcept = default;
TrProbe &TrProbe::operator=(TrProbe &&) noexcept = default;

std::unique_ptr<TrProbe> tr_probe_open(const TrProbeConfig &cfg) {
#ifdef HAVE_OSMOSDR
    std::string driver(cfg.driver);
    if (driver != "osmosdr") {
        throw std::runtime_error("gain auto-calibration is only supported for the osmosdr driver");
    }

    std::string dev(cfg.device);
    std::ostringstream args;
    if (!dev.empty() && std::isdigit(static_cast<unsigned char>(dev[0]))) {
        // Bare serial number → default to rtl, matching Source's behavior.
        args << "rtl=" << dev;
    } else {
        args << dev;
    }

    osmosdr::source::sptr src = osmosdr::source::make(args.str());
    src->set_sample_rate(cfg.rate);
    src->set_center_freq(cfg.center + cfg.error, 0);
    if (cfg.ppm != 0.0) {
        src->set_freq_corr(cfg.ppm, 0);
    }
    // Hardware AGC off: gain calibration needs a stationary dB scale.
    src->set_gain_mode(false, 0);
    std::string antenna(cfg.antenna);
    if (!antenna.empty()) {
        src->set_antenna(antenna, 0);
    }

    auto probe = std::make_unique<TrProbe>();
    probe->impl = std::make_unique<TrProbeImpl>();
    probe->impl->src = src;
    probe->impl->rate = src->get_sample_rate();
    return probe;
#else
    (void)cfg;
    throw std::runtime_error("osmosdr support not compiled in; cannot auto-calibrate gain");
#endif
}

rust::Vec<double> tr_probe_gain_steps(const TrProbe &probe) {
    rust::Vec<double> out;
#ifdef HAVE_OSMOSDR
    if (probe.impl && probe.impl->src) {
        osmosdr::gain_range_t range = probe.impl->src->get_gain_range(0);
        for (double g : range.values()) {
            out.push_back(g);
        }
    }
#else
    (void)probe;
#endif
    return out;
}

rust::Vec<float> tr_probe_capture(TrProbe &probe, double gain_db, size_t n_samples, size_t settle_samples) {
    rust::Vec<float> out;
#ifdef HAVE_OSMOSDR
    if (!probe.impl || !probe.impl->src) {
        throw std::runtime_error("probe is not open");
    }
    osmosdr::source::sptr src = probe.impl->src;
    src->set_gain(gain_db, 0);

    gr::top_block_sptr tb = gr::make_top_block("gain_probe_capture");
    auto skip = gr::blocks::skiphead::make(sizeof(gr_complex), settle_samples);
    auto head = gr::blocks::head::make(sizeof(gr_complex), n_samples);
    auto sink = gr::blocks::vector_sink<gr_complex>::make();
    tb->connect(src, 0, skip, 0);
    tb->connect(skip, 0, head, 0);
    tb->connect(head, 0, sink, 0);
    tb->run(); // returns once head has collected n_samples
    tb.reset();

    const std::vector<gr_complex> &data = sink->data();
    out.reserve(data.size() * 2);
    for (const gr_complex &c : data) {
        out.push_back(c.real());
        out.push_back(c.imag());
    }
#else
    (void)probe;
    (void)gain_db;
    (void)n_samples;
    (void)settle_samples;
    throw std::runtime_error("osmosdr support not compiled in; cannot auto-calibrate gain");
#endif
    return out;
}

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
    out.term_events = tx.term_events;
    out.after_term_samples = tx.after_term_samples;
    out.freq = tx.freq;
    return out;
}


// ── CxxEventSink: EventSink subclass for C++-initiated callbacks ──
// Calls Rust trampolines directly on the EventHandlerBox.

class CxxEventSink : public EventSink {
public:
    explicit CxxEventSink(EventHandlerBox *handler) : handler_(handler) {}

    void audio_callback(const std::shared_ptr<Recorder> &recorder,
                        int16_t *samples, int sampleCount) override {
        if (!recorder) return;
        rust::Slice<const int16_t> slice(samples, static_cast<size_t>(sampleCount));
        event_handler_audio_stream(*handler_, recorder->get_rust_call_id(), slice);
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
    // Only set up trunking system decoders here.
    // Conventional channels are set up per-channel from Rust via tr_setup_conventional_channel.
    for (auto &system : ctx.impl->trunk_ctx.systems) {
        if (!is_conventional(system->get_system_type())) {
            system->setup_decoders(ctx.impl->trunk_ctx.tb, ctx.impl->trunk_ctx.sources);
        }
    }
}

// ── Config builder implementations ──

void tr_config_set_temp_dir(TrContext &ctx, rust::Str dir) {
    ctx.impl->trunk_ctx.config.temp_dir = std::string(dir.data(), dir.size());
}

namespace {
// trunk-p25's phase 1 voice regenerator behind the recorder's backend
// interface (symbol_regenerator.h).
class RustVoiceRegenerator : public symbol_regenerator_backend {
public:
    RustVoiceRegenerator(double rate, int recorder) : h_(p25_regenerator_new(rate, recorder)) {}

    void process(const gr_complex *in, size_t n, std::vector<float> &out) override {
        h_->process(rust::Slice<const float>(reinterpret_cast<const float *>(in), 2 * n), out_);
        out.insert(out.end(), out_.begin(), out_.end());
    }
    void start_call(bool qpsk, bool tdma, const std::string &xor_mask, const std::string &system, bool primary) override {
        h_->start_call(qpsk, tdma,
                       rust::Slice<const uint8_t>(reinterpret_cast<const uint8_t *>(xor_mask.data()), xor_mask.size()),
                       rust::Str(system), primary);
    }
    CallCounts take_call_counts() override {
        const TrCallCounts c = h_->take_call_counts();
        return {static_cast<long>(c.errors), static_cast<long>(c.spikes)};
    }

private:
    rust::Box<P25RegeneratorHandle> h_;
    rust::Vec<float> out_;
};
} // namespace

// Recorders get a trunk-p25 regenerator when it decodes some phase, as
// primary or as shadow.
static void update_voice_regenerator(Config &config) {
    auto &f = config.p25_voice_regenerator;
    const bool shadow = config.p25_shadow.phase1 || config.p25_shadow.phase2;
    if (config.p25_rust_phase1 || config.p25_rust_phase2 || shadow) {
        f = [](double rate, int recorder) -> std::shared_ptr<symbol_regenerator_backend> {
            return std::make_shared<RustVoiceRegenerator>(rate, recorder);
        };
    } else {
        f = nullptr;
    }
}

static TrP25Decoder to_tr(P25DecoderImpl d) {
    return d == P25DecoderImpl::Rust ? TrP25Decoder::Rust : TrP25Decoder::Op25;
}

void tr_config_set_p25_voice_decoder(TrContext &ctx, TrP25Decoder phase1, TrP25Decoder phase2) {
    auto &config = ctx.impl->trunk_ctx.config;
    config.p25_rust_phase1 = phase1 == TrP25Decoder::Rust;
    config.p25_rust_phase2 = phase2 == TrP25Decoder::Rust;
    update_voice_regenerator(config);
}

void tr_config_set_p25_shadow(TrContext &ctx, bool control, bool phase1, bool phase2) {
    auto &config = ctx.impl->trunk_ctx.config;
    config.p25_shadow = {control, phase1, phase2};
    update_voice_regenerator(config);
    config.p25_cc_observer = [](const std::string &system, double freq, P25DecoderImpl impl, bool primary, long type,
                                const std::string &payload) {
        p25_observe_cc_message(rust::Str(system), freq, to_tr(impl), primary, static_cast<int64_t>(type),
                               rust::Slice<const uint8_t>(reinterpret_cast<const uint8_t *>(payload.data()), payload.size()));
    };
    config.p25_call_observer = [](const P25CallReport &r) {
        TrCallReport out;
        out.system = rust::String(r.system);
        out.recorder = r.recorder;
        out.talkgroup = r.talkgroup;
        out.freq = r.freq;
        out.phase2 = r.phase2;
        for (const auto &a : r.accounts) {
            TrCallAccount t;
            t.decoder = to_tr(a.impl);
            t.primary = a.primary;
            t.audio_seconds = a.audio_seconds;
            t.transmissions = a.transmissions;
            t.errors = a.errors;
            t.spikes = a.spikes;
            for (long s : a.sources) t.sources.push_back(s);
            out.accounts.push_back(std::move(t));
        }
        p25_observe_call(out);
    };
}

void tr_config_set_p25_decoder(TrContext &ctx, TrP25Decoder decoder) {
    ctx.impl->trunk_ctx.config.p25_decoder =
        decoder == TrP25Decoder::Op25 ? P25DecoderImpl::Op25 : P25DecoderImpl::Rust;
}

void tr_config_set_tap_dir(TrContext &ctx, rust::Str dir) {
    ctx.impl->trunk_ctx.config.tap_dir = std::string(dir.data(), dir.size());
}

void tr_config_set_soft_vocoder(TrContext &ctx, bool enabled) {
    ctx.impl->trunk_ctx.config.soft_vocoder = enabled;
}

void tr_config_set_debug_recorder(TrContext &ctx, bool enabled, rust::Str address, int32_t port) {
    ctx.impl->trunk_ctx.config.debug_recorder = enabled;
    ctx.impl->trunk_ctx.config.debug_recorder_address = std::string(address.data(), address.size());
    ctx.impl->trunk_ctx.config.debug_recorder_port = port;
}

// ── Shared channelizer backend (DSP in Rust: trunk-channelizer) ──

namespace {
class RustChannelizerBackend : public channelizer_backend {
public:
    RustChannelizerBackend(double sample_rate, int32_t source)
        : h_(shared_channelizer_new(sample_rate, source)),
          hop_(h_->hop()),
          stride_(h_->max_out_per_block()),
          profiles_(h_->profile_count()) {}

    size_t hop() const override { return hop_; }
    bool has_profile(unsigned profile) const override { return profile < profiles_; }
    size_t out_per_block(unsigned profile) const override { return h_->out_per_block(profile); }
    size_t max_out_per_block() const override { return stride_; }
    double out_rate(unsigned profile) const override { return h_->out_rate(profile); }
    void set_channel(unsigned port, unsigned profile, double offset_hz) override { h_->set_channel(port, profile, offset_hz); }
    void set_filter(unsigned port, double cutoff_hz, double transition_hz) override { h_->set_filter(port, cutoff_hz, transition_hz); }
    void clear_channel(unsigned port) override { h_->clear_channel(port); }
    void set_conditioning(unsigned port, const ConditioningSpec &s) override {
        h_->set_conditioning(port, s.sps, s.excess_bw, s.in_rate, s.out_rate, static_cast<uint32_t>(s.squelch));
    }
    void set_squelch_db(unsigned port, double db) override { h_->set_squelch_db(port, db); }
    bool squelch_open(unsigned port) override { return h_->squelch_open(port); }
    int next_squelch_event(unsigned port, double &pwr_db) override { return h_->next_squelch_event(port, pwr_db); }
    int freq_error_hz(unsigned port) override { return h_->freq_error_hz(port); }
    double power_db(unsigned port) override { return h_->power_db(port); }
    void set_decoder(unsigned port, const ControlDecoderSpec &s) override {
        TrControlDecoder kind = TrControlDecoder::None;
        switch (s.kind) {
        case ControlDecoder::P25Cqpsk: kind = TrControlDecoder::P25Cqpsk; break;
        case ControlDecoder::P25C4fm: kind = TrControlDecoder::P25C4fm; break;
        case ControlDecoder::None: break;
        }
        h_->set_control_decoder(port, kind, s.rate, rust::Str(s.system), s.freq_hz, s.primary);
    }
    bool next_message(unsigned port, DecodedMessage &msg) override {
        TrDecodedMessage m;
        if (!h_->next_decoded_message(port, m)) return false;
        msg.type = static_cast<long>(m.msg_type);
        msg.payload.assign(reinterpret_cast<const char *>(m.payload.data()), m.payload.size());
        return true;
    }
    void process(const gr_complex *in, size_t nports, gr_complex *out, uint32_t *produced) override {
        h_->process(rust::Slice<const float>(reinterpret_cast<const float *>(in), 2 * hop_),
                    rust::Slice<float>(reinterpret_cast<float *>(out), 2 * nports * stride_),
                    rust::Slice<uint32_t>(produced, nports));
    }

private:
    rust::Box<SharedChannelizerHandle> h_;
    size_t hop_;
    size_t stride_;
    unsigned profiles_;
};
} // namespace

// ── Source builder implementations ──

int32_t tr_add_source_from_config(TrContext &ctx, const TrSourceConfig &cfg) {
    std::string drv(cfg.driver.data(), cfg.driver.size());
    std::string dev(cfg.device.data(), cfg.device.size());
    // "iqfile": `device` is the path to a raw IQ file (cf32 or cu8), paced at
    // the source rate; looped unless `iq_repeat` is false. Used for
    // hardware-free testing (tests/generate_test_iq.py) and offline replay.
    auto source = (drv == "iqfile")
        ? std::make_shared<Source>(dev, cfg.iq_repeat, cfg.center, cfg.rate, ctx.impl->trunk_ctx.config,
                                   std::string(cfg.iq_format.data(), cfg.iq_format.size()))
        : std::make_shared<Source>(cfg.center, cfg.rate, cfg.error, drv, dev,
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
    int32_t idx = static_cast<int32_t>(ctx.impl->trunk_ctx.sources.size());
    source->set_channelizer_factory([idx](double rate) -> std::shared_ptr<channelizer_backend> {
        return std::make_shared<RustChannelizerBackend>(rate, idx);
    });
    if (cfg.digital_recorders > 0) source->create_digital_recorders(tb, cfg.digital_recorders);
    if (cfg.analog_recorders > 0) source->create_analog_recorders(tb, cfg.analog_recorders);
    if (cfg.sigmf_recorders > 0) source->create_sigmf_recorders(tb, cfg.sigmf_recorders);

    ctx.impl->trunk_ctx.sources.push_back(source);
    return idx;
}

rust::String tr_system_short_name(const TrSystem &sys) {
    return rust::String(sys.ptr->get_short_name());
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
    sys->set_analog_levels(cfg.analog_levels);
    sys->set_tau(static_cast<float>(cfg.tau));
    sys->set_max_dev(static_cast<int32_t>(cfg.max_dev));

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

double tr_system_get_control_channel_pwr(TrContext &ctx, int32_t sys_num) {
    auto sys = find_system_by_num(ctx, sys_num);
    if (!sys) return std::numeric_limits<double>::quiet_NaN();
    double pwr = sys->get_control_channel_pwr();
    // DB_UNSET (999.0) means the probe hasn't measured yet (e.g. right after retune)
    if (pwr >= 998.0) return std::numeric_limits<double>::quiet_NaN();
    return pwr;
}

// ── Boost.Log → Rust tracing sink ──

namespace {

class RustTracingSinkBackend
    : public boost::log::sinks::basic_sink_backend<
          boost::log::sinks::concurrent_feeding> {
public:
    void consume(const boost::log::record_view &rec) {
        auto severity = rec[boost::log::trivial::severity];
        uint8_t level = severity ? static_cast<uint8_t>(severity.get()) : 2;

        auto msg = rec[boost::log::expressions::smessage];
        if (msg) {
            const auto &s = msg.get();
            rust_tracing_event(level, rust::Str(s.data(), s.size()));
        }
    }
};

void install_rust_tracing_sink() {
    auto core = boost::log::core::get();
    core->remove_all_sinks();
    using sink_t = boost::log::sinks::synchronous_sink<RustTracingSinkBackend>;
    core->add_sink(boost::make_shared<sink_t>(
        boost::make_shared<RustTracingSinkBackend>()));
    // Let everything through — Rust's EnvFilter handles filtering
    core->set_filter(boost::log::trivial::severity >= boost::log::trivial::trace);
}

} // anonymous namespace

// ── Phase A: Loop Control ──

void tr_start_gnuradio(TrContext &ctx) {
    install_rust_tracing_sink();
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

    // Offline baseline: every raw decoder message, before parsing
    // (Config::tap_dir). One decoder = one GR thread, so no locking.
    std::ofstream raw_log;
    std::chrono::steady_clock::time_point raw_t0 = std::chrono::steady_clock::now();
    uint64_t raw_seq = 0;

    void log_raw(const gr::message::sptr &msg) {
        if (!raw_log.is_open()) return;
        static const char *hex = "0123456789abcdef";
        std::string payload = msg->to_string();
        std::string h;
        h.reserve(payload.size() * 2);
        for (unsigned char ch : payload) {
            h.push_back(hex[ch >> 4]);
            h.push_back(hex[ch & 15]);
        }
        auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - raw_t0).count();
        raw_log << "{\"seq\":" << raw_seq++ << ",\"ms\":" << ms << ",\"type\":" << msg->type()
                << ",\"arg1\":" << msg->arg1() << ",\"arg2\":" << msg->arg2()
                << ",\"hex\":\"" << h << "\"}\n";
        raw_log.flush();
    }

    explicit TrunkDrainState(rust::Box<EventNotifier> n)
        : notifier(std::move(n)) {}
};

void tr_context_wire_event_notifiers(TrContext &ctx) {
    auto &bridge = ctx.impl->event_bridge;
    if (!bridge) return;

    // Phase 2: wire trunk message callbacks — one per decoder for thread safety
    for (auto &system : ctx.impl->trunk_ctx.systems) {
        auto sys_type = system->get_system_type();
        if (sys_type != SYS_SMARTNET && sys_type != SYS_P25) continue;

        for (auto &decoder : system->get_decoders()) {
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
                state->p25_config = std::move(p25_config);
            }

            const std::string &tap_dir = ctx.impl->trunk_ctx.config.tap_dir;
            if (!tap_dir.empty()) {
                state->raw_log.open(tap_dir + "/cc_" + system->get_short_name() + "_" +
                                    std::to_string(static_cast<long long>(decoder->get_freq())) + ".msgs.jsonl");
            }

            decoder->set_msg_callback([state](gr::message::sptr msg) {
                state->log_raw(msg);
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
    }

    // Phase 3: wire squelch callbacks on recorder blocks
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        auto recs = source->get_recorders();
        for (auto &rec : recs) {
            auto notifier = bridge->make_notifier();
            int rec_num = rec->get_num();
            auto sq_notifier = std::make_shared<rust::Box<EventNotifier>>(std::move(notifier));
            rec->set_squelch_callback(
                [sq_notifier, rec_num](bool muted, double pwr_db) {
                    event_notifier_notify_squelch(**sq_notifier, rec_num, muted, pwr_db);
                });
            auto sc_notifier =
                std::make_shared<rust::Box<EventNotifier>>(bridge->make_notifier());
            rec->set_source_change_callback(
                [sc_notifier](uint64_t rust_call_id, long src_id) {
                    event_notifier_notify_source_change(**sc_notifier, rust_call_id, src_id);
                });
            auto cr_notifier =
                std::make_shared<rust::Box<EventNotifier>>(bridge->make_notifier());
            rec->set_crypto_callback(
                [cr_notifier](uint64_t rust_call_id, unsigned algid, unsigned keyid) {
                    event_notifier_notify_crypto(**cr_notifier, rust_call_id, algid, keyid);
                });
        }
    }

    // Phase 4: wire signal detection callbacks on sources
    // The callback receives detected signals directly from the signal detector's
    // work() method and passes them to Rust for frequency matching and routing.
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        auto notifier = bridge->make_notifier();
        int src_num = source->get_num();
        double center = source->get_center();
        source->set_detection_callback(
            [notifier = std::make_shared<rust::Box<EventNotifier>>(std::move(notifier)),
             src_num, center](std::vector<Detected_Signal> signals) {
                rust::Vec<TrDetectedSignal> rust_signals;
                for (const auto &s : signals) {
                    TrDetectedSignal rs;
                    rs.center_freq = s.center_freq;
                    rs.max_rssi = s.max_rssi;
                    rs.threshold = s.threshold;
                    rust_signals.push_back(rs);
                }
                event_notifier_notify_signals_detected(
                    **notifier, src_num, center, std::move(rust_signals));
            });
    }

}

void tr_context_check_sources(TrContext &ctx) {
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        bool samples = source->got_samples();
        if (!samples) {
            BOOST_LOG_TRIVIAL(fatal) << "Source " << source->get_num()
                      << " (" << source->get_device()
                      << ") stopped receiving samples - will exit";
            throw std::runtime_error(
                "Source " + std::to_string(source->get_num()) +
                " (" + source->get_device() +
                ") stopped receiving samples");
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

// ── Call — recorder status (null-guarded) ──

bool call_has_recorder(const TrCall &call) {
    return call.ptr->get_recorder() != nullptr;
}

// ── Call — Rust call ID round-trip ──

void call_set_rust_call_id(TrCall &call, uint64_t id) {
    call.ptr->set_rust_call_id(id);
}

// ── Config conversion (Rust TrRecorderConfig → C++ RecorderConfig) ──

static RecorderConfig to_cpp_config(const TrRecorderConfig &cfg) {
    return RecorderConfig{
        .talkgroup = cfg.talkgroup,
        .call_num = cfg.call_num,
        .freq = cfg.freq,
        .short_name = std::string(cfg.short_name),
        .temp_dir = std::string(cfg.temp_dir),
        .phase2_tdma = cfg.phase2_tdma,
        .tdma_slot = cfg.tdma_slot,
        .xor_mask = std::string(reinterpret_cast<const char*>(cfg.xor_mask.data()), cfg.xor_mask.size()),
        .squelch_db = cfg.squelch_db,
        .digital_levels = cfg.digital_levels,
        .rust_call_id = cfg.rust_call_id,
    };
}

// ── Call — lifecycle ──

void call_restart(TrCall &call, const TrRecorderConfig &cfg) {
    call.ptr->restart_call(to_cpp_config(cfg));
}

void call_start_initial_recording(TrCall &call, const TrRecorderConfig &cfg) {
    auto rec = call.ptr->get_recorder();
    if (rec) {
        rec->start(to_cpp_config(cfg));
    }
}

void call_set_recorder_enabled(TrCall &call, bool enabled) {
    auto rec = call.ptr->get_recorder();
    if (rec) {
        rec->set_enabled(enabled);
    }
}

// ── Conventional channel setup (Rust selects source, C++ creates objects) ──

std::unique_ptr<TrCall> tr_create_conventional_call(
    TrContext &ctx, int32_t sys_num, int32_t source_idx,
    const TrChannelConfig &channel) {

    auto system = find_system_by_num(ctx, sys_num);
    if (!system) return nullptr;

    auto &sources = ctx.impl->trunk_ctx.sources;
    if (source_idx < 0 || source_idx >= static_cast<int32_t>(sources.size()))
        return nullptr;
    auto &source = sources[source_idx];
    auto &tb = ctx.impl->trunk_ctx.tb;

    auto call = std::make_shared<Call_conventional>(system);

    // NOTE: This function must be called BEFORE tb->start(). Conventional
    // recorders are hier_block2 subgraphs; adding them to a running GNU Radio
    // flowgraph via lock/unlock crashes in block_detail::output() during the
    // graph restart. By creating them before start(), they are part of the
    // initial graph topology and no restart is needed.

    std::shared_ptr<Recorder> rec;
    auto sys_type = system->get_system_type();
    if (sys_type == SYS_CONVENTIONAL) {
        auto analog_rec = (channel.tone > 0.0)
            ? source->create_conventional_recorder(tb, static_cast<float>(channel.tone))
            : source->create_conventional_recorder(tb);
        analog_rec->set_tau(system->get_tau());
        rec = analog_rec;
        call->set_recorder(rec);
    } else if (sys_type == SYS_CONVENTIONAL_DMR) {
        rec = source->create_dmr_conventional_recorder(tb);
        call->set_recorder(rec);
    } else if (sys_type == SYS_CONVENTIONAL_P25) {
        rec = source->create_digital_conventional_recorder(tb);
        call->set_recorder(rec);
    } else if (sys_type == SYS_CONVENTIONAL_SIGMF) {
        rec = source->create_sigmf_conventional_recorder(tb);
        call->set_recorder(rec);
    } else {
        return nullptr;
    }

    rec->set_system(system);

    // Wire squelch callback — Phase 3 only covers pre-existing recorders,
    // so conventional recorders created here need it wired explicitly.
    assert(ctx.impl->event_bridge && "event_bridge must be set before creating conventional calls");
    auto notifier = ctx.impl->event_bridge->make_notifier();
    int rec_num = rec->get_num();
    rec->set_squelch_callback(
        [notifier = std::make_shared<rust::Box<EventNotifier>>(std::move(notifier)),
         rec_num](bool muted, double pwr_db) {
            event_notifier_notify_squelch(**notifier, rec_num, muted, pwr_db);
        });
    auto sc_notifier =
        std::make_shared<rust::Box<EventNotifier>>(ctx.impl->event_bridge->make_notifier());
    rec->set_source_change_callback(
        [sc_notifier](uint64_t rust_call_id, long src_id) {
            event_notifier_notify_source_change(**sc_notifier, rust_call_id, src_id);
        });
    auto cr_notifier =
        std::make_shared<rust::Box<EventNotifier>>(ctx.impl->event_bridge->make_notifier());
    rec->set_crypto_callback(
        [cr_notifier](uint64_t rust_call_id, unsigned algid, unsigned keyid) {
            event_notifier_notify_crypto(**cr_notifier, rust_call_id, algid, keyid);
        });

    return std::make_unique<TrCall>(call);
}

// ── Call accessors (audio callback now uses rust_call_id round-trip) ──

// ── Phase C: Call Management — lifecycle operations ──

std::unique_ptr<TrCall> tr_context_create_call(TrContext &ctx, const TrTrunkMessage &msg) {
    auto sys = find_system_by_num(ctx, msg.sys_num);
    if (!sys) {
        throw std::runtime_error("System not found for sys_num");
    }
    auto call = Call::make(sys);
    return std::make_unique<TrCall>(call);
}


int32_t tr_source_available_recorders(const TrContext &ctx, int32_t source_idx, bool is_analog) {
    auto &sources = ctx.impl->trunk_ctx.sources;
    if (source_idx < 0 || source_idx >= static_cast<int32_t>(sources.size())) return 0;
    auto &source = sources[source_idx];
    return is_analog
        ? source->get_num_available_analog_recorders()
        : source->get_num_available_digital_recorders();
}

bool tr_source_start_recording(TrContext &ctx, int32_t source_idx, TrCall &call, bool is_analog, const TrRecorderConfig &cfg) {
    auto &sources = ctx.impl->trunk_ctx.sources;
    if (source_idx < 0 || source_idx >= static_cast<int32_t>(sources.size())) return false;
    auto &source = sources[source_idx];
    auto &config = ctx.impl->trunk_ctx.config;

    std::shared_ptr<Recorder> recorder = is_analog
        ? source->get_analog_recorder()
        : source->get_digital_recorder();

    auto rc = to_cpp_config(cfg);

    if (!recorder) {
        return false;
    }

    recorder->set_system(call.ptr->get_system());

    if (!recorder->start(rc)) {
        return false;
    }

    call.ptr->set_recorder(recorder);

    // Debug recorder
    auto debug_recorder = source->get_debug_recorder();
    if (debug_recorder) {
        debug_recorder->start(rc);
        call.ptr->set_debug_recorder(debug_recorder);
    }

    // SigMF recorder
    auto sigmf_recorder = source->get_sigmf_recorder();
    if (sigmf_recorder) {
        sigmf_recorder->start(rc);
        call.ptr->set_sigmf_recorder(sigmf_recorder);
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
    result.freq_error = rec->get_freq_error();
    result.recorder_pwr = rec->get_pwr();
    result.is_dmr = (call.ptr->get_system()->get_system_type() == SYS_CONVENTIONAL_DMR);

    rec->stop();
    rec->flush_audio();

    if (auto sigmf = call.ptr->get_sigmf_recorder()) {
        sigmf->stop();
    }
    if (auto debug = call.ptr->get_debug_recorder()) {
        debug->stop();
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

void call_set_squelch_db(TrCall &call, double db) {
    auto rec = call.ptr->get_recorder();
    if (rec) rec->set_squelch_db(db);
}

double call_get_pwr_db(const TrCall &call) {
    auto rec = call.ptr->get_recorder();
    if (rec) return rec->get_pwr();
    return 0.0;
}

// ── Phase C: Call Management — call-level recorder accessors ──

double call_recorder_since_last_write_secs(const TrCall &call) {
    auto recorder = call.ptr->get_recorder();
    if (!recorder) {
        return 1e9; // no recorder → effectively infinite (never triggers timeout alone)
    }
    return recorder->since_last_write().count();
}

double call_recorder_since_last_voice_secs(const TrCall &call) {
    auto recorder = call.ptr->get_recorder();
    if (!recorder) {
        return 1e9;
    }
    return recorder->since_last_voice().count();
}

// ── GR block performance metrics ──

static TrGrBlockMetrics collect_block_metrics(const gr::block_sptr &blk) {
    TrGrBlockMetrics m;
    m.block_name = rust::String(blk->name());
    m.block_alias = rust::String(blk->alias());
    m.items_read = 0;
    m.items_written = 0;
    m.work_time_total = 0.0f;
    m.input_buf_full = 0.0f;
    m.output_buf_full = 0.0f;

    auto d = blk->detail();
    if (!d) return m;

    if (d->ninputs() > 0) {
        m.items_read = blk->nitems_read(0);
        m.input_buf_full = blk->pc_input_buffers_full(0);
    }
    if (d->noutputs() > 0) {
        m.items_written = blk->nitems_written(0);
        m.output_buf_full = blk->pc_output_buffers_full(0);
    }
    m.work_time_total = blk->pc_work_time_total();
    return m;
}

rust::Vec<TrRecorderMetrics> tr_collect_recorder_metrics(const TrContext &ctx) {
    rust::Vec<TrRecorderMetrics> result;
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        int src_num = source->get_num();
        auto recorders = source->get_recorders();
        for (auto &rec : recorders) {
            TrRecorderMetrics rm;
            rm.recorder_id = rec->get_num();
            rm.recorder_type = rust::String(rec->get_type_string());
            rm.source_id = src_num;

            for (auto &blk : rec->get_metric_blocks()) {
                rm.blocks.push_back(collect_block_metrics(blk));
            }
            result.push_back(std::move(rm));
        }
    }
    return result;
}

rust::Vec<TrSourceMetrics> tr_collect_source_metrics(const TrContext &ctx) {
    rust::Vec<TrSourceMetrics> result;
    for (auto &source : ctx.impl->trunk_ctx.sources) {
        TrSourceMetrics sm;
        sm.source_id = source->get_num();
        sm.center_freq = source->get_center();
        sm.sample_rate = source->get_rate();

        auto src_block = std::dynamic_pointer_cast<gr::block>(source->get_src_block());
        if (src_block) {
            sm.blocks.push_back(collect_block_metrics(src_block));
        }
        result.push_back(std::move(sm));
    }
    return result;
}

} // namespace trunk_oxide
