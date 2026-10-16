#ifndef XLAT_CHANNELIZER_H
#define XLAT_CHANNELIZER_H

#include <boost/log/trivial.hpp>
#include <functional>
#include <iomanip>
#include <memory>
#include <optional>

#include "./rms_agc.h"
#include "./freq_xlating_fft_filter.h"
#include "./callback_pwr_squelch_cc.h"
#include <gnuradio/blocks/copy.h>
#include <gnuradio/digital/fll_band_edge_cc.h>
#include <gnuradio/filter/fft_filter_ccc.h>
#include <gnuradio/filter/fft_filter_ccf.h>
#include <gnuradio/filter/firdes.h>
#include <gnuradio/filter/freq_xlating_fir_filter.h>
#include <gnuradio/filter/pfb_arb_resampler_ccf.h>
#include <gnuradio/hier_block2.h>

#include <gnuradio/analog/probe_avg_mag_sqrd_c.h>
#include <gnuradio/analog/sig_source.h>
#include <gnuradio/blocks/multiply.h>
#include <gnuradio/blocks/multiply_const.h>

#include "../formatter.h"
#include "../global_structs.h"

class xlat_channelizer : public gr::hier_block2 {
public:
  typedef std::shared_ptr<xlat_channelizer> sptr;

  // shared_rate > 0 selects "shared" mode: the input is already a single channel
  // produced by shared_channelizer at that rate (so freq_xlat + channel_lpf are
  // omitted), and tune_offset() is forwarded to the tune hook instead.
  static sptr make(double input_rate, int samples_per_symbol, double symbol_rate, double bandwidth, double center_freq, bool use_squelch, double excess_bw=default_excess_bw, double shared_rate=0.0);
  xlat_channelizer(double input_rate, int samples_per_symbol, double symbol_rate, double bandwidth, double center_freq, bool use_squelch, double excess_bw, double shared_rate);

  struct DecimSettings {
    long decim;
    long decim2;
  };

  static constexpr float default_excess_bw = 0.2;
  static constexpr float smartnet_excess_bw = 0.35;
  static const int smartnet_samples_per_symbol = 5;
  static const int phase1_samples_per_symbol = 5;
  static const int phase2_samples_per_symbol = 4;
  static constexpr double phase1_symbol_rate = 4800;
  static constexpr double phase2_symbol_rate = 6000;
  static constexpr double smartnet_symbol_rate = 3600;
  static constexpr double channel_bandwidth = 12500;

  int get_freq_error();
  bool is_squelched();
  double get_pwr();
  std::vector<gr::block_sptr> get_metric_blocks() const;
  void set_squelch_callback(std::function<void(bool, double)> cb) {
    squelch->set_squelch_callback(std::move(cb));
  }
  void tune_offset(double f);
  bool is_shared() const { return d_shared; }
  // Shared mode only: called with the channel's offset from the source centre
  // (Hz, positive = above centre) whenever tune_offset() is called.
  void set_tune_hook(std::function<void(double)> hook) { tune_hook = std::move(hook); }
  // Shared mode only: called with (cutoff_hz, transition_hz) of this channel's
  // own filter whenever set_max_dev() changes it. Every channel, digital or
  // analog, takes its filter through the same hook.
  void set_filter_hook(std::function<void(double, double)> hook) { filter_hook = std::move(hook); }
  void set_samples_per_symbol(int samples_per_symbol);
  void set_squelch_db(double squelch_db);
  void set_analog_squelch(bool analog_squelch);
  void set_max_dev(double max_dev);
  bool uses_squelch() const { return d_use_squelch; }

  // Shadow validation (see Source::attach_shadow): a shared-mode twin that
  // receives every setter call this channelizer gets, so the two can be
  // compared while this one stays in charge. A twin has the same geometry,
  // squelch and FLL settings; set_mirror() replays the state set so far.
  sptr make_shared_twin(double shared_rate) const;
  void set_mirror(sptr twin);

private:
  bool double_decim;
  long if1;
  long if2;
  double d_center_freq;
  double d_input_rate;
  double d_bandwidth;
  double d_system_channel_rate;
  int d_samples_per_symbol;
  double d_symbol_rate;
  double d_excess_bw;

  // Setter state, kept so a mirror attached later can be brought up to date.
  std::optional<double> d_offset;
  std::optional<double> d_max_dev;
  std::optional<double> d_squelch_db;
  std::optional<bool> d_analog_squelch;
  std::optional<int> d_sps_override;
  sptr d_mirror;

  bool d_use_squelch;
  bool d_shared = false;
  std::function<void(double)> tune_hook;
  std::function<void(double, double)> filter_hook;
  long symbol_rate;
  double initial_rate;
  double squelch_db = 0.0;
  long decim;

  // gr::filter::freq_xlating_fir_filter<gr_complex, gr_complex, float>::sptr freq_xlat;
  freq_xlating_fft_filter_sptr freq_xlat;
  std::vector<float> arb_taps;
  std::vector<gr_complex> bandpass_filter_coeffs;
  std::vector<float> lowpass_filter_coeffs;
  std::vector<float> cutoff_filter_coeffs;

  callback_pwr_squelch_cc::sptr squelch;
  gr::digital::fll_band_edge_cc::sptr fll_band_edge;
  gr::blocks::rms_agc::sptr rms_agc;

  gr::analog::sig_source_c::sptr lo;
  gr::analog::sig_source_c::sptr bfo;
  gr::blocks::multiply_cc::sptr mixer;

  gr::filter::fft_filter_ccc::sptr bandpass_filter;
  gr::filter::fft_filter_ccf::sptr lowpass_filter;
  gr::filter::fft_filter_ccf::sptr channel_lpf;
  gr::filter::fft_filter_ccf::sptr cutoff_filter;

  gr::filter::pfb_arb_resampler_ccf::sptr arb_resampler;
  gr::analog::probe_avg_mag_sqrd_c::sptr pwr_probe;

  static DecimSettings get_decim(long speed);
};

#endif