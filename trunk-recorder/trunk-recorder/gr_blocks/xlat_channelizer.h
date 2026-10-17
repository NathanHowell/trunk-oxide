#ifndef XLAT_CHANNELIZER_H
#define XLAT_CHANNELIZER_H

#include <boost/log/trivial.hpp>
#include <functional>
#include <memory>

#include "./rms_agc.h"
#include "./callback_pwr_squelch_cc.h"
#include <gnuradio/digital/fll_band_edge_cc.h>
#include <gnuradio/filter/firdes.h>
#include <gnuradio/filter/pfb_arb_resampler_ccf.h>
#include <gnuradio/hier_block2.h>

#include <gnuradio/analog/probe_avg_mag_sqrd_c.h>
#include <gnuradio/blocks/copy.h>
#include <gnuradio/blocks/file_sink.h>
#include <string>

#include "../formatter.h"
#include "./shared_channelizer.h"
#include "../global_structs.h"

// Where a block takes its channel from on a shared_channelizer: the channel's
// sample rate, and how to retune and re-filter it.
struct ChannelTap {
  double rate;
  std::function<void(double)> set_offset;               // offset from source centre, Hz
  std::function<void(double, double)> set_filter;       // cutoff, transition (Hz)
  std::string tap_base;                                 // tap file path prefix; empty = no taps
  ConditioningHooks cond;                               // set = the channelizer conditions the channel
};

class xlat_channelizer : public gr::hier_block2 {
public:
  typedef std::shared_ptr<xlat_channelizer> sptr;

  // The input is already a single channel produced by shared_channelizer at
  // `channel_rate`; this block resamples it to the symbol-rate multiple and runs
  // squelch, AGC and the FLL. Frequency and filter changes are forwarded to the
  // tune and filter hooks (set before the first tune_offset()/set_max_dev()).
  // `external_conditioning`: leave resampling, squelch, AGC and the FLL to
  // the channelizer (see ConditioningSpec); the input is passed through and
  // set_conditioning_hooks() must be called before the graph runs.
  static sptr make(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate, double excess_bw=default_excess_bw, bool external_conditioning=false);
  xlat_channelizer(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate, double excess_bw, bool external_conditioning);

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
    if (d_external) {
      // Kept until the hooks arrive if they are not set yet.
      d_squelch_cb = std::move(cb);
      if (d_cond.set_squelch_callback) d_cond.set_squelch_callback(d_squelch_cb);
      return;
    }
    squelch->set_squelch_callback(std::move(cb));
  }
  void tune_offset(double f);
  // Called with the channel's offset from the source centre (Hz, positive =
  // above centre) whenever tune_offset() is called.
  void set_tune_hook(std::function<void(double)> hook) { tune_hook = std::move(hook); }
  // Called with (cutoff_hz, transition_hz) of this channel's own filter
  // whenever set_max_dev() changes it. Every channel, digital or analog, takes
  // its filter through the same hook.
  void set_filter_hook(std::function<void(double, double)> hook) { filter_hook = std::move(hook); }
  void set_samples_per_symbol(int samples_per_symbol);
  bool external_conditioning() const { return d_external; }
  void set_conditioning_hooks(ConditioningHooks hooks);
  void set_squelch_db(double squelch_db);
  void set_analog_squelch(bool analog_squelch);
  void set_max_dev(double max_dev);
  bool uses_squelch() const { return d_use_squelch; }

  // Offline-baseline taps. enable_tap() adds two file sinks, on the block's
  // input (`.in.cf32`, the shared channelizer's output) and output (`.out.cf32`,
  // after squelch, AGC and FLL); call it before the graph runs. The sinks
  // discard samples until tap_open(base) opens `<base>.in.cf32`,
  // `<base>.out.cf32` and a `<base>.json` sidecar (`meta_json` is spliced in as
  // extra fields); tap_close() stops writing again.
  void enable_tap();
  bool tap_enabled() const { return tap_in != nullptr; }
  void tap_open(const std::string &base, const std::string &meta_json = "");
  void tap_close();

private:
  double d_bandwidth;
  int d_samples_per_symbol;

  bool d_use_squelch;
  bool d_external;
  double d_excess_bw;
  ConditioningSquelch d_squelch_mode;
  ConditioningHooks d_cond;
  std::function<void(bool, double)> d_squelch_cb;
  void send_conditioning();
  std::function<void(double)> tune_hook;
  std::function<void(double, double)> filter_hook;
  std::vector<float> arb_taps;

  callback_pwr_squelch_cc::sptr squelch;
  gr::digital::fll_band_edge_cc::sptr fll_band_edge;
  gr::blocks::rms_agc::sptr rms_agc;

  gr::filter::pfb_arb_resampler_ccf::sptr arb_resampler;
  gr::analog::probe_avg_mag_sqrd_c::sptr pwr_probe;

  double d_channel_rate_in = 0;
  double d_channel_rate_out = 0;
  gr::blocks::copy::sptr passthrough;
  gr::blocks::file_sink::sptr tap_in;
  gr::blocks::file_sink::sptr tap_out;
};

#endif