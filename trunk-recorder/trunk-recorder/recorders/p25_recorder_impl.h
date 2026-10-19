#ifndef P25_RECORDER_IMPL_H
#define P25_RECORDER_IMPL_H

#define _USE_MATH_DEFINES

#include <atomic>
#include <optional>
#include <cstdio>
#include <iostream>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <chrono>
#include <time.h>


#include <gnuradio/filter/firdes.h>
#include <gnuradio/hier_block2.h>
#include <gnuradio/io_signature.h>

#include <gnuradio/analog/pll_freqdet_cf.h>
#include <gnuradio/filter/fft_filter_fff.h>
#include <gnuradio/filter/pfb_arb_resampler_ccf.h>

#include <gnuradio/block.h>
#include <gnuradio/blocks/copy.h>

// #include <gnuradio/blocks/selector.h>

#include <gnuradio/analog/sig_source.h>
#include <gnuradio/blocks/multiply.h>
#include <gnuradio/blocks/multiply_const.h>
#include <gnuradio/filter/fir_filter_blk.h>

#include <gnuradio/block_detail.h>
#include <gnuradio/blocks/file_sink.h>
#include <gnuradio/blocks/head.h>
#include <gnuradio/digital/fll_band_edge_cc.h>
#include <gnuradio/message.h>
#include <gnuradio/runtime_types.h>

#include "../gr_blocks/channel_control.h"
#include "../gr_blocks/selector.h"

// #include <op25_repeater/include/op25_repeater/rmsagc_ff.h>
#include "../../lib/gr-latency-manager/include/latency_manager.h"
#include "../../lib/gr-latency-manager/include/tag_to_msg.h"
#include "../../lib/gr-latency/latency_probe.h"
#include "../../lib/gr-latency/latency_tagger.h"
#include "../call_conventional.h"
#include "p25_recorder.h"
#include "p25_recorder_decode.h"
#include "p25_recorder_fsk4_demod.h"
#include "p25_recorder_qpsk_demod.h"
#include "recorder.h"

class Source;
class p25_recorder;

#include "../gr_blocks/p25_voice_decoder.h"
#include "../gr_blocks/symbol_regenerator.h"

#include "../source.h"

class p25_recorder_impl : public p25_recorder {
  friend p25_recorder_sptr make_p25_recorder(const std::shared_ptr<Source> &src, Recorder_Type type);

protected:
  void initialize(const std::shared_ptr<Source> &src);
  void build_graph();

public:
  p25_recorder_impl(const std::shared_ptr<Source> &src, Recorder_Type type);

  void initialize_qpsk();
  void initialize_fsk4();
  void initialize_p25();
  bool start(const RecorderConfig &config) override;
  void set_channelizer_port(unsigned int port) override;
  void stop() override;
  void flush_audio() override;
  int get_num() const;
  int get_freq_error() const override;
  double get_pwr() const override;
  std::vector<Transmission> get_transmission_list() override;
  void set_tdma(bool phase2);
  void switch_tdma(bool phase2);
  void set_source(long src) override;
  void set_system(const std::shared_ptr<System> &sys) override;
  std::chrono::duration<double> since_last_write() const override;
  std::chrono::duration<double> since_last_voice() const override;
  void set_enabled(bool enabled) override;
  RecorderState get_state() const override;
  void set_squelch_callback(std::function<void(bool, double)> cb) override;
  std::vector<gr::block_sptr> get_metric_blocks() const override;
  std::shared_ptr<Source> get_source();
  void autotune();

protected:
  RecorderState state;
  std::chrono::steady_clock::time_point starttime;
  long talkgroup;
  std::string short_name;
  const Config &config;
  std::shared_ptr<Source> source;
  double chan_freq;
  double center_freq;
  bool qpsk_mod;
  double squelch_db;
  gr::blocks::selector::sptr modulation_selector;

  p25_recorder_fsk4_demod_sptr fsk4_demod;
  p25_recorder_decode_sptr fsk4_p25_decode;
  p25_recorder_qpsk_demod_sptr qpsk_demod;
  p25_recorder_decode_sptr qpsk_p25_decode;
  // trunk-p25's regenerator when Config::p25_voice_regenerator is set:
  // regenerated symbols into a third decode chain. Fed straight from the
  // port (not through the selector) so it can run beside another chain in
  // shadow mode.
  p25_regenerator::sptr rust_regen;
  p25_recorder_decode_sptr rust_p25_decode;
  // trunk-p25 to audio when Config::p25_voice_decoder is set, into a fourth
  // chain that only records (and reports) the audio.
  p25_voice_decoder::sptr rust_voice;
  p25_recorder_decode_sptr rust_voice_decode;
  channel_control::sptr prefilter;
  // Which chain the call in progress uses; the selector feeds op25's.
  enum class Branch { Fsk4 = 0, Qpsk = 1, Regen = 2, Rust = 3 };
  Branch active = Branch::Qpsk;
  // trunk-p25 (regenerator or decoder) is primary on the call in progress:
  // its ESS marks encryption.
  std::atomic<bool> regen_crypto{false};
  std::atomic<bool> voice_crypto{false};
  // Shadow mode: the other implementation's chain, decoding the same call
  // with its audio discarded, reported to Config::p25_call_observer.
  std::optional<Branch> shadow;
  bool call_phase2 = false;
  // trunk-p25's counts for the call just stopped (taken once, at stop()).
  symbol_regenerator_backend::CallCounts rust_counts;
  p25_voice_backend::CallCounts voice_counts;
  Branch branch_of(P25DecoderImpl impl) const;
  static P25DecoderImpl impl_of(Branch b);
  Branch branch_for(bool phase2) const;
  std::optional<Branch> shadow_for(bool phase2) const;
  const p25_recorder_decode_sptr &decode() const;
  const p25_recorder_decode_sptr &decode_of(Branch b) const;
  std::vector<Transmission> transmissions_of(Branch b) const;
  void report_call();
  void disable_op25_chains();
  // Offline taps (enabled by Config::tap_dir): demodulated symbols per chain.
  gr::blocks::file_sink::sptr qpsk_symbol_tap;
  gr::blocks::file_sink::sptr fsk4_symbol_tap;
  gr::blocks::file_sink::sptr rust_symbol_tap;
  // The active chain's symbol tap (none for trunk-p25 to audio).
  gr::blocks::file_sink::sptr symbol_tap() const;
  void tap_begin(const RecorderConfig &config);
  void tap_end();

private:
  void tune_offset(double f);
  void tune_freq(double f);
  void set_tdma_slot(int slot);
  void clear();
  int silence_frames;
  int tdma_slot;
  bool d_phase2_tdma;
  bool d_soft_vocoder;
  long input_rate;
  const int phase1_samples_per_symbol = 5;
  const int phase2_samples_per_symbol = 4;
  const double phase1_symbol_rate = 4800;
  const double phase2_symbol_rate = 6000;

  gr::blocks::multiply_const_ff::sptr rescale;
  void reset_block(gr::basic_block_sptr block);
};

#endif // ifndef P25_RECORDER_H
