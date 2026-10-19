
#include "p25_recorder_impl.h"
#include "../formatter.h"
#include "p25_recorder.h"
#include <boost/log/trivial.hpp>

p25_recorder_sptr make_p25_recorder(const std::shared_ptr<Source> &src, Recorder_Type type) {
  auto sptr = gnuradio::get_initial_sptr(new p25_recorder_impl(src, type));
  // shared_from_this() is now available — build the decode blocks and wire the graph.
  static_cast<p25_recorder_impl *>(sptr.get())->build_graph();
  return sptr;
}

p25_recorder_impl::p25_recorder_impl(const std::shared_ptr<Source> &src, Recorder_Type type)
    : gr::hier_block2("p25_recorder",
                      gr::io_signature::make(1, 1, sizeof(gr_complex)),
                      gr::io_signature::make(0, 0, sizeof(float))),
      Recorder(type),
      config(src->get_config()) {
  conventional = type == P25C;
  initialize(src);
}

void p25_recorder_impl::initialize(const std::shared_ptr<Source> &src) {
  source = src;
  chan_freq = source->get_center();
  center_freq = source->get_center();
  qpsk_mod = true;
  squelch_db = 0;
  talkgroup = 0;
  d_phase2_tdma = false;
  rec_num = rec_counter++;
  state = REC_INACTIVE;

  // Input is one channel already extracted by the source's shared channelizer.
  auto chz = source->get_channelizer();
  prefilter = channel_control::make(channel_control::phase1_samples_per_symbol, channel_control::phase1_symbol_rate, channel_control::channel_bandwidth, conventional, chz->out_rate(CHANNEL_PROFILE_NARROW));
  prefilter->set_tune_hook([this](double offset_hz) { source->channelizer_set_offset(channelizer_port, offset_hz); });
  prefilter->set_filter_hook([this](double cutoff_hz, double transition_hz) { source->channelizer_set_filter(channelizer_port, cutoff_hz, transition_hz); });
  if (!config.tap_dir.empty()) prefilter->enable_tap();
}

void p25_recorder_impl::set_channelizer_port(unsigned int port) {
  Recorder::set_channelizer_port(port);
  prefilter->set_conditioning_hooks(source->channelizer_conditioning(port));
}

void p25_recorder_impl::build_graph() {
  if (!config.p25_voice_decoder) {
    throw std::runtime_error("P25 recorder needs Config::p25_voice_decoder (trunk-p25)");
  }
  auto self_recorder = std::dynamic_pointer_cast<Recorder>(shared_from_this());
  const double rate = channel_control::phase1_samples_per_symbol * channel_control::phase1_symbol_rate;
  voice = p25_voice_decoder::make(config.p25_voice_decoder(rate, rec_num), rate);
  voice->set_crypto_callback([this](unsigned algid, unsigned keyid) {
    if (forward_crypto) notify_crypto(algid, keyid);
  });
  decode = make_p25_recorder_decode(self_recorder, config);
  voice->set_json_callback([this](const std::string &json) { decode->handle_json(json); });

  connect(self(), 0, voice, 0);
  connect(voice, 0, decode, 0);
  if (prefilter->tap_enabled()) {
    connect(self(), 0, prefilter->enable_tap(), 0);
  }
}

void p25_recorder_impl::tap_begin(const RecorderConfig &rc) {
  if (!prefilter->tap_enabled()) return;
  const std::string base = config.tap_dir + "/v_" + std::to_string(rc.rust_call_id) + "_" + rc.short_name + "_" + std::to_string(rc.talkgroup);
  prefilter->tap_open(base, "\"kind\":\"voice\",\"system\":\"" + rc.short_name + "\",\"talkgroup\":" + std::to_string(rc.talkgroup) +
                                ",\"freq\":" + std::to_string(static_cast<long long>(rc.freq)) +
                                ",\"center\":" + std::to_string(static_cast<long long>(center_freq)) +
                                ",\"tdma\":" + (rc.phase2_tdma ? "true" : "false") + ",\"slot\":" + std::to_string(rc.tdma_slot) +
                                ",\"call_id\":" + std::to_string(rc.rust_call_id) + ",\"recorder\":" + std::to_string(rec_num) +
                                ",\"decoder\":\"rust\"");
}

void p25_recorder_impl::tap_end() {
  if (prefilter->tap_enabled()) prefilter->tap_close();
}

void p25_recorder_impl::switch_tdma(bool phase2) {
  d_phase2_tdma = phase2;
  prefilter->set_samples_per_symbol(phase2 ? phase2_samples_per_symbol : phase1_samples_per_symbol);
}

void p25_recorder_impl::set_tdma(bool phase2) {
  if (phase2 != d_phase2_tdma) {
    switch_tdma(phase2);
  }
}

int p25_recorder_impl::get_freq_error() const { // get frequency error from FLL and convert to Hz
  return prefilter->get_freq_error();
}

std::shared_ptr<Source> p25_recorder_impl::get_source() {
  return source;
}

int p25_recorder_impl::get_num() const {
  return rec_num;
}

std::chrono::duration<double> p25_recorder_impl::since_last_write() const {
  return decode->since_last_write();
}

std::chrono::duration<double> p25_recorder_impl::since_last_voice() const {
  return decode->since_last_voice();
}

RecorderState p25_recorder_impl::get_state() const {
  return decode->get_state();
}

void p25_recorder_impl::set_enabled(bool enabled) {
  source->channelizer_set_enabled(channelizer_port, enabled);
}

double p25_recorder_impl::get_pwr() const {
  return prefilter->get_pwr();
}

void p25_recorder_impl::set_squelch_callback(std::function<void(bool, double)> cb) {
  prefilter->set_squelch_callback(std::move(cb));
}

void p25_recorder_impl::set_source(long src) {
  decode->set_source(src);
}

void p25_recorder_impl::set_system(const std::shared_ptr<System> &sys) {
  decode->set_system(sys);
}

std::vector<Transmission> p25_recorder_impl::get_transmission_list() {
  std::vector<Transmission> list = decode->get_transmission_list();
  // The call's decode errors, shared out by audio length.
  long total_samples = 0;
  for (const auto &t : list) total_samples += t.sample_count;
  long errors = counts.errors, spikes = counts.spikes;
  for (size_t i = 0; i < list.size(); i++) {
    const bool last = i + 1 == list.size();
    const double share = total_samples > 0 ? static_cast<double>(list[i].sample_count) / total_samples : 1.0 / list.size();
    list[i].error_count = last ? errors : static_cast<long>(counts.errors * share);
    list[i].spike_count = last ? spikes : static_cast<long>(counts.spikes * share);
    errors -= list[i].error_count;
    spikes -= list[i].spike_count;
  }
  return list;
}

void p25_recorder_impl::report_call() {
  if (!config.p25_call_observer) return;
  P25CallReport r{short_name, rec_num, talkgroup, chan_freq, call_phase2, 0.0, 0, 0, 0, decode->call_sources(), counts.algid, counts.repeated, counts.muted};
  for (const auto &t : get_transmission_list()) {
    r.audio_seconds += static_cast<double>(t.sample_count) / 8000.0;
    r.transmissions++;
    r.errors += t.error_count;
    r.spikes += t.spike_count;
  }
  config.p25_call_observer(r);
}

void p25_recorder_impl::stop() {
  if (state == REC_ACTIVE) {
    state = REC_INACTIVE;
    forward_crypto = false;
    set_enabled(false);
    tap_end();
    decode->stop();
    voice->set_enabled(false);
    counts = voice->take_call_counts();
    report_call();
  } else {
    BOOST_LOG_TRIVIAL(error) << "p25_recorder.cc: Trying to Stop an Inactive Logger!!!";
  }
}

void p25_recorder_impl::flush_audio() {
  decode->flush_plugin();
}

bool p25_recorder_impl::start(const RecorderConfig &config) {
  if (state != REC_INACTIVE) {
    BOOST_LOG_TRIVIAL(error) << "p25_recorder.cc: Trying to Start an already Active Logger!!!";
    return false;
  }
  if (config.phase2_tdma && config.xor_mask.empty()) {
    BOOST_LOG_TRIVIAL(info) << "Error - can't set XOR Mask for TDMA";
    return false;
  }
  call_phase2 = config.phase2_tdma;
  counts = {};
  set_tdma(config.phase2_tdma);

  talkgroup = config.talkgroup;
  short_name = config.short_name;
  chan_freq = config.freq;
  rust_call_id = config.rust_call_id;

  const int offset_amount = (center_freq - chan_freq); // whole Hz, as before
  prefilter->tune_offset(offset_amount);
  tap_begin(config);

  voice->start_call(qpsk_mod, config.phase2_tdma, config.xor_mask, config.short_name, config.phase2_tdma ? config.tdma_slot : 0);
  voice->take_call_counts(); // anything before this call is not its
  voice->set_enabled(true);
  forward_crypto = true;
  decode->start(config);
  state = REC_ACTIVE;

  squelch_db = config.squelch_db;
  if (!conventional) {
    set_enabled(true);
  }
  prefilter->set_squelch_db(squelch_db);
  return true;
}

std::vector<gr::block_sptr> p25_recorder_impl::get_metric_blocks() const {
  return {voice};
}

int Recorder::rec_counter = 0;
