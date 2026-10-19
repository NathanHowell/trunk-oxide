
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
  if (type == P25C) {
    conventional = true;
  } else {
    conventional = false;
  }
  initialize(src);
}

void p25_recorder_impl::initialize(const std::shared_ptr<Source> &src) {
  source = src;
  chan_freq = source->get_center();
  center_freq = source->get_center();
  d_soft_vocoder = config.soft_vocoder;
  input_rate = source->get_rate();
  qpsk_mod = true;
  silence_frames = source->get_silence_frames();
  squelch_db = 0;
  talkgroup = 0;
  d_phase2_tdma = false;
  rec_num = rec_counter++;

  state = REC_INACTIVE;

  starttime = std::chrono::steady_clock::now();

  // Input is one channel already extracted by the source's shared channelizer.
  auto chz = source->get_channelizer();
  prefilter = channel_control::make(channel_control::phase1_samples_per_symbol, channel_control::phase1_symbol_rate, channel_control::channel_bandwidth, conventional, chz->out_rate(CHANNEL_PROFILE_NARROW));
  prefilter->set_tune_hook([this](double offset_hz) { source->channelizer_set_offset(channelizer_port, offset_hz); });
  prefilter->set_filter_hook([this](double cutoff_hz, double transition_hz) { source->channelizer_set_filter(channelizer_port, cutoff_hz, transition_hz); });
  if (!config.tap_dir.empty()) prefilter->enable_tap();

  modulation_selector = gr::blocks::selector::make(sizeof(gr_complex), 0, 0);
  qpsk_demod = make_p25_recorder_qpsk_demod();
  fsk4_demod = make_p25_recorder_fsk4_demod();
}

void p25_recorder_impl::set_channelizer_port(unsigned int port) {
  Recorder::set_channelizer_port(port);
  prefilter->set_conditioning_hooks(source->channelizer_conditioning(port));
}

void p25_recorder_impl::build_graph() {
  auto self_recorder = std::dynamic_pointer_cast<Recorder>(shared_from_this());
  qpsk_p25_decode = make_p25_recorder_decode(self_recorder, config, silence_frames, d_soft_vocoder);
  fsk4_p25_decode = make_p25_recorder_decode(self_recorder, config, silence_frames, d_soft_vocoder);
  if (config.p25_voice_regenerator) {
    rust_regen = p25_regenerator::make(config.p25_voice_regenerator(channel_control::phase1_samples_per_symbol * channel_control::phase1_symbol_rate, rec_num));
    // With trunk-p25 primary, its ESS (confirmed on the call's own slot) is
    // what marks a call encrypted; op25's is only compared.
    rust_regen->set_crypto_callback([this](unsigned algid, unsigned keyid) {
      if (regen_crypto) notify_crypto(algid, keyid);
    });
    rust_p25_decode = make_p25_recorder_decode(self_recorder, config, silence_frames, d_soft_vocoder);
  }
  if (config.p25_voice_decoder) {
    const double rate = channel_control::phase1_samples_per_symbol * channel_control::phase1_symbol_rate;
    rust_voice = p25_voice_decoder::make(config.p25_voice_decoder(rate, rec_num), rate);
    rust_voice->set_crypto_callback([this](unsigned algid, unsigned keyid) {
      if (voice_crypto) notify_crypto(algid, keyid);
    });
    rust_voice_decode = make_p25_recorder_decode(self_recorder, config, silence_frames, d_soft_vocoder, true);
    rust_voice->set_json_callback([this](const std::string &json) { rust_voice_decode->handle_json(json); });
  }

  connect(self(), 0, modulation_selector, 0);
  connect(modulation_selector, 0, fsk4_demod, 0);
  connect(fsk4_demod, 0, fsk4_p25_decode, 0);
  connect(modulation_selector, 1, qpsk_demod, 0);
  connect(qpsk_demod, 0, qpsk_p25_decode, 0);
  disable_op25_chains();
  if (rust_regen) {
    connect(self(), 0, rust_regen, 0);
    connect(rust_regen, 0, rust_p25_decode, 0);
  }
  if (rust_voice) {
    connect(self(), 0, rust_voice, 0);
    connect(rust_voice, 0, rust_voice_decode, 0);
  }

  if (prefilter->tap_enabled()) {
    connect(self(), 0, prefilter->enable_tap(), 0);
    qpsk_symbol_tap = gr::blocks::file_sink::make(sizeof(float), "/dev/null");
    fsk4_symbol_tap = gr::blocks::file_sink::make(sizeof(float), "/dev/null");
    qpsk_symbol_tap->close();
    fsk4_symbol_tap->close();
    connect(qpsk_demod, 0, qpsk_symbol_tap, 0);
    connect(fsk4_demod, 0, fsk4_symbol_tap, 0);
    if (rust_regen) {
      rust_symbol_tap = gr::blocks::file_sink::make(sizeof(float), "/dev/null");
      rust_symbol_tap->close();
      connect(rust_regen, 0, rust_symbol_tap, 0);
    }
  }
}

p25_recorder_impl::Branch p25_recorder_impl::branch_of(P25DecoderImpl impl) const {
  if (impl == P25DecoderImpl::Rust && rust_voice) return Branch::Rust;
  if (impl == P25DecoderImpl::Regen && rust_regen) return Branch::Regen;
  return qpsk_mod ? Branch::Qpsk : Branch::Fsk4;
}

P25DecoderImpl p25_recorder_impl::impl_of(Branch b) {
  switch (b) {
  case Branch::Rust: return P25DecoderImpl::Rust;
  case Branch::Regen: return P25DecoderImpl::Regen;
  case Branch::Fsk4:
  case Branch::Qpsk: break;
  }
  return P25DecoderImpl::Op25;
}

p25_recorder_impl::Branch p25_recorder_impl::branch_for(bool phase2) const {
  return branch_of(phase2 ? config.p25_phase2 : config.p25_phase1);
}

std::optional<p25_recorder_impl::Branch> p25_recorder_impl::shadow_for(bool phase2) const {
  if (!(phase2 ? config.p25_shadow.phase2 : config.p25_shadow.phase1)) return std::nullopt;
  const Branch b = branch_of(p25_shadow_of(impl_of(branch_for(phase2))));
  if (b == branch_for(phase2)) return std::nullopt; // what it needs is not built
  return b;
}

const p25_recorder_decode_sptr &p25_recorder_impl::decode() const {
  return decode_of(active);
}

const p25_recorder_decode_sptr &p25_recorder_impl::decode_of(Branch b) const {
  switch (b) {
  case Branch::Rust: return rust_voice_decode;
  case Branch::Regen: return rust_p25_decode;
  case Branch::Fsk4: return fsk4_p25_decode;
  case Branch::Qpsk: break;
  }
  return qpsk_p25_decode;
}

gr::blocks::file_sink::sptr p25_recorder_impl::symbol_tap() const {
  switch (active) {
  case Branch::Rust: return nullptr;
  case Branch::Regen: return rust_symbol_tap;
  case Branch::Fsk4: return fsk4_symbol_tap;
  case Branch::Qpsk: break;
  }
  return qpsk_symbol_tap;
}

void p25_recorder_impl::tap_begin(const RecorderConfig &rc) {
  if (!prefilter->tap_enabled()) return;
  const std::string base = config.tap_dir + "/v_" + std::to_string(rc.rust_call_id) + "_" + rc.short_name + "_" + std::to_string(rc.talkgroup);
  prefilter->tap_open(base, "\"kind\":\"voice\",\"system\":\"" + rc.short_name + "\",\"talkgroup\":" + std::to_string(rc.talkgroup) +
                                ",\"freq\":" + std::to_string(static_cast<long long>(rc.freq)) +
                                ",\"center\":" + std::to_string(static_cast<long long>(center_freq)) +
                                ",\"tdma\":" + (rc.phase2_tdma ? "true" : "false") + ",\"slot\":" + std::to_string(rc.tdma_slot) +
                                ",\"call_id\":" + std::to_string(rc.rust_call_id) + ",\"recorder\":" + std::to_string(rec_num) +
                                ",\"decoder\":\"" + (active == Branch::Rust ? "rust" : active == Branch::Regen ? "regen" : "op25") + "\"" +
                                (symbol_tap() ? ",\"symbols_file\":\"" + base + ".symbols.f32\"" : std::string()));
  if (auto tap = symbol_tap()) tap->open((base + ".symbols.f32").c_str());
}

void p25_recorder_impl::tap_end() {
  if (!prefilter->tap_enabled()) return;
  prefilter->tap_close();
  qpsk_symbol_tap->close();
  fsk4_symbol_tap->close();
  if (rust_symbol_tap) rust_symbol_tap->close();
}

void p25_recorder_impl::switch_tdma(bool phase2) {
  if (phase2) {
    d_phase2_tdma = true;
    prefilter->set_samples_per_symbol(phase2_samples_per_symbol);
  } else {
    d_phase2_tdma = false;
    prefilter->set_samples_per_symbol(phase1_samples_per_symbol);
  }

  if (qpsk_mod) {
    qpsk_p25_decode->switch_tdma(phase2);
    qpsk_demod->switch_tdma(phase2);
  }
  if (rust_p25_decode) rust_p25_decode->switch_tdma(phase2);
  if (rust_voice_decode) rust_voice_decode->switch_tdma(phase2);
}

void p25_recorder_impl::set_tdma(bool phase2) {
  if (phase2 != d_phase2_tdma) {
    switch_tdma(phase2);
  }
}

void p25_recorder_impl::reset_block(gr::basic_block_sptr block) {
  gr::block_detail_sptr detail;
  gr::block_sptr grblock = cast_to_block_sptr(block);
  detail = grblock->detail();
  detail->reset_nitem_counters();
}
void p25_recorder_impl::clear() {
  // This lead to weird SegFaults, but the goal was to clear out buffers inbetween transmissions.
  /*
  if (double_decim) {
    //reset_block(bandpass_filter);
    //reset_block(bfo);
  } else {
  //reset_block(lo);
  }
  reset_block(lowpass_filter);
  reset_block(mixer);

  if (arb_rate != 1.0) {
  reset_block(arb_resampler);
  }

  reset_block(cutoff_filter);
  reset_block(squelch);
  //reset_block(rms_agc); // RMS AGC cant be made into a basic block
  reset_block(fll_band_edge);
  reset_block(modulation_selector);


  //reset_block(qpsk_demod); // bad - Seg Faults
  //reset_block(qpsk_p25_decode); // bad - Seg Faults
  //reset_block(fsk4_demod); // bad - Seg Faults
  //reset_block(fsk4_p25_decode);  // bad - Seg Faults

  */
  qpsk_demod->reset();
  qpsk_p25_decode->reset();
  fsk4_demod->reset();
  fsk4_p25_decode->reset();
  if (rust_p25_decode) rust_p25_decode->reset();
  if (rust_voice_decode) rust_voice_decode->reset();
}

void p25_recorder_impl::autotune() {
  /*if (!qpsk_mod) {
    gr::message::sptr msg;
    msg = tune_queue->delete_head_nowait();

    if (msg != 0) {
      BOOST_LOG_TRIVIAL(info) << "p25_recorder.cc: Freq:\t" << format_freq(chan_freq) << "\t Tune: " << msg->arg1();

      // tune_offset(freq + (msg->arg1()*100));
      tune_queue->flush();
    }
  }*/
}

void p25_recorder_impl::tune_offset(double f) { prefilter->tune_offset(f); }

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
  return decode()->since_last_write();
}

std::chrono::duration<double> p25_recorder_impl::since_last_voice() const {
  return decode()->since_last_voice();
}

RecorderState p25_recorder_impl::get_state() const {
  return decode()->get_state();
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

void p25_recorder_impl::tune_freq(double f) {
  chan_freq = f;
  float freq = (center_freq - f);
  prefilter->tune_offset(freq);
}

void p25_recorder_impl::set_source(long src) {
  return decode()->set_source(src);
}

void p25_recorder_impl::set_system(const std::shared_ptr<System> &sys) {
  qpsk_p25_decode->set_system(sys);
  fsk4_p25_decode->set_system(sys);
  if (rust_p25_decode) rust_p25_decode->set_system(sys);
  if (rust_voice_decode) rust_voice_decode->set_system(sys);
}

std::vector<Transmission> p25_recorder_impl::get_transmission_list() {
  return transmissions_of(active);
}

std::vector<Transmission> p25_recorder_impl::transmissions_of(Branch b) const {
  std::vector<Transmission> list = decode_of(b)->get_transmission_list();
  if (b != Branch::Rust && b != Branch::Regen) return list;
  // The decode errors are trunk-p25's (the regenerator's frame assembler
  // only saw error-free frames), shared out by audio length.
  const long total_errors = b == Branch::Rust ? voice_counts.errors : rust_counts.errors;
  const long total_spikes = b == Branch::Rust ? voice_counts.spikes : rust_counts.spikes;
  long total_samples = 0;
  for (const auto &t : list) total_samples += t.sample_count;
  long errors = total_errors, spikes = total_spikes;
  for (size_t i = 0; i < list.size(); i++) {
    const bool last = i + 1 == list.size();
    const double share = total_samples > 0 ? static_cast<double>(list[i].sample_count) / total_samples : 1.0 / list.size();
    list[i].error_count = last ? errors : static_cast<long>(total_errors * share);
    list[i].spike_count = last ? spikes : static_cast<long>(total_spikes * share);
    errors -= list[i].error_count;
    spikes -= list[i].spike_count;
  }
  return list;
}

void p25_recorder_impl::disable_op25_chains() {
  modulation_selector->set_port_enabled(static_cast<unsigned>(Branch::Fsk4), false);
  modulation_selector->set_port_enabled(static_cast<unsigned>(Branch::Qpsk), false);
}

void p25_recorder_impl::report_call() {
  if (!config.p25_call_observer) return;
  P25CallReport r{short_name, rec_num, talkgroup, chan_freq, call_phase2, {}};
  auto account = [this](Branch b, bool primary) {
    P25CallAccount a{impl_of(b), primary, 0.0, 0, 0, 0, {}, 0x80};
    for (const auto &t : transmissions_of(b)) {
      a.audio_seconds += static_cast<double>(t.sample_count) / 8000.0;
      a.transmissions++;
      a.errors += t.error_count;
      a.spikes += t.spike_count;
    }
    a.sources = decode_of(b)->call_sources();
    a.algid = b == Branch::Rust ? voice_counts.algid : b == Branch::Regen ? rust_counts.algid : decode_of(b)->call_algid();
    return a;
  };
  r.accounts.push_back(account(active, true));
  if (shadow) r.accounts.push_back(account(*shadow, false));
  config.p25_call_observer(r);
}

void p25_recorder_impl::stop() {
  if (state == REC_ACTIVE) {
    state = REC_INACTIVE;
    regen_crypto = false;
    voice_crypto = false;
    set_enabled(false);
    tap_end();

    clear();
    decode()->stop();
    if (shadow) decode_of(*shadow)->stop();
    if (rust_regen) {
      rust_regen->set_enabled(false);
      if (active == Branch::Regen || shadow == Branch::Regen) rust_counts = rust_regen->take_call_counts();
    }
    if (rust_voice) {
      rust_voice->set_enabled(false);
      if (active == Branch::Rust || shadow == Branch::Rust) voice_counts = rust_voice->take_call_counts();
    }
    disable_op25_chains();
    report_call();
  } else {
    BOOST_LOG_TRIVIAL(error) << "p25_recorder.cc: Trying to Stop an Inactive Logger!!!";
  }
}

void p25_recorder_impl::flush_audio() {
  decode()->flush_plugin();
}

void p25_recorder_impl::set_tdma_slot(int slot) {
  decode()->set_tdma_slot(slot);
  if (shadow) decode_of(*shadow)->set_tdma_slot(slot);
  tdma_slot = slot;
}

bool p25_recorder_impl::start(const RecorderConfig &config) {
  if (state == REC_INACTIVE) {
    active = branch_for(config.phase2_tdma);
    shadow = shadow_for(config.phase2_tdma);
    call_phase2 = config.phase2_tdma;
    rust_counts = {};
    voice_counts = {};
    set_tdma(config.phase2_tdma);
    if (config.phase2_tdma) {
      if (!qpsk_mod) {
        BOOST_LOG_TRIVIAL(error) << "Error - Modulation is FSK4 but receiving Phase 2 call, this will not work";
        return false;
      }
      set_tdma_slot(config.tdma_slot);

      if (!config.xor_mask.empty()) {
        qpsk_p25_decode->set_xor_mask(config.xor_mask);
        if (rust_p25_decode) rust_p25_decode->set_xor_mask(config.xor_mask);
      } else {
        BOOST_LOG_TRIVIAL(info) << "Error - can't set XOR Mask for TDMA";
        return false;
      }
    } else {
      set_tdma_slot(0);
    }

    starttime = std::chrono::steady_clock::now();

    talkgroup = config.talkgroup;
    short_name = config.short_name;
    chan_freq = config.freq;
    rust_call_id = config.rust_call_id;

    int offset_amount = (center_freq - chan_freq);

    prefilter->tune_offset(offset_amount);
    tap_begin(config);

    const int slot = config.phase2_tdma ? config.tdma_slot : 0;
    if (active == Branch::Regen || shadow == Branch::Regen) {
      rust_regen->start_call(qpsk_mod, config.phase2_tdma, config.xor_mask, config.short_name, active == Branch::Regen, slot);
      rust_regen->take_call_counts(); // anything before this call is not its
      rust_regen->set_enabled(true);
    }
    if (active == Branch::Rust || shadow == Branch::Rust) {
      rust_voice->start_call(qpsk_mod, config.phase2_tdma, config.xor_mask, config.short_name, active == Branch::Rust, slot);
      rust_voice->take_call_counts();
      rust_voice->set_enabled(true);
    }
    // The selector feeds whichever op25 chain runs, primary or shadow.
    const auto is_op25 = [](Branch b) { return b == Branch::Fsk4 || b == Branch::Qpsk; };
    if (is_op25(active)) {
      modulation_selector->set_output_index(static_cast<int>(active)); // enables that port only
    } else if (shadow && is_op25(*shadow)) {
      modulation_selector->set_output_index(static_cast<int>(*shadow));
    }
    decode()->set_shadow(false);
    // trunk-p25 confirms encryption on the call's own slot; op25's ESS
    // reports are then only recorded.
    decode()->set_forward_crypto(is_op25(active));
    regen_crypto = active == Branch::Regen;
    voice_crypto = active == Branch::Rust;
    decode()->start(config);
    if (shadow) {
      decode_of(*shadow)->set_shadow(true);
      decode_of(*shadow)->start(config);
    }
    state = REC_ACTIVE;

    squelch_db = config.squelch_db;
    if (!conventional) {
      set_enabled(true);
    }
    prefilter->set_squelch_db(squelch_db);

  } else {
    BOOST_LOG_TRIVIAL(error) << "p25_recorder.cc: Trying to Start an already Active Logger!!!";
    return false;
  }
  return true;
}

std::vector<gr::block_sptr> p25_recorder_impl::get_metric_blocks() const {
  std::vector<gr::block_sptr> blocks;
  if (modulation_selector) blocks.push_back(modulation_selector);
  return blocks;
}

int Recorder::rec_counter = 0;
