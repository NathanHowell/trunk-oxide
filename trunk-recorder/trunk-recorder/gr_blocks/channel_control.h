#ifndef CHANNEL_CONTROL_H
#define CHANNEL_CONTROL_H

#include <functional>
#include <memory>
#include <string>

#include <gnuradio/blocks/file_sink.h>

#include "./shared_channelizer.h"
#include "../global_structs.h"

// Where a block takes its channel from on a shared_channelizer: the channel's
// sample rate, how to retune and re-filter it, and its conditioning.
struct ChannelTap {
  double rate;
  std::function<void(double)> set_offset;               // offset from source centre, Hz
  std::function<void(double, double)> set_filter;       // cutoff, transition (Hz)
  std::string tap_base;                                 // tap file path prefix; empty = no taps
  ConditioningHooks cond;
  std::string system;                                   // system short name
  P25DecoderImpl p25_decoder = P25DecoderImpl::Rust;
};

// Control side of one shared_channelizer port. The channelizer delivers the
// channel already resampled to `samples_per_symbol * symbol_rate` and
// conditioned (squelch, AGC, FLL; see ConditioningSpec), so callers connect
// the port straight to their demodulator; this object only forwards frequency,
// filter, squelch and conditioning changes, and reads back FLL error, power
// and squelch state. Set the hooks before the first tune_offset() /
// set_max_dev(); set_conditioning_hooks() before the graph runs.
class channel_control {
public:
  typedef std::shared_ptr<channel_control> sptr;

  static sptr make(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate, double excess_bw = default_excess_bw);
  channel_control(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate, double excess_bw);

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
  // Kept until the hooks arrive if they are not set yet.
  void set_squelch_callback(std::function<void(bool, double)> cb);
  void tune_offset(double f);
  // Called with the channel's offset from the source centre (Hz, positive =
  // above centre) whenever tune_offset() is called.
  void set_tune_hook(std::function<void(double)> hook) { tune_hook = std::move(hook); }
  // Called with (cutoff_hz, transition_hz) of this channel's own filter
  // whenever set_max_dev() changes it.
  void set_filter_hook(std::function<void(double, double)> hook) { filter_hook = std::move(hook); }
  void set_samples_per_symbol(int samples_per_symbol);
  void set_conditioning_hooks(ConditioningHooks hooks);
  void set_squelch_db(double squelch_db);
  void set_analog_squelch(bool analog_squelch);
  void set_max_dev(double max_dev);
  bool uses_squelch() const { return d_use_squelch; }

  // Decode the conditioned channel in the channelizer (P25 control channels).
  // Messages arrive on the GR thread through set_message_callback(); both are
  // kept until the hooks arrive if they are not set yet.
  void set_decoder(ControlDecoder kind, const std::string &system, double freq_hz);
  void set_message_callback(std::function<void(const DecodedMessage &)> cb);

  // Offline-baseline taps. enable_tap() returns a file sink for the caller to
  // connect to the conditioned channel; it discards samples until
  // tap_open(base) opens `<base>.out.cf32` and a `<base>.json` sidecar
  // (`meta_json` is spliced in as extra fields); tap_close() stops writing.
  gr::blocks::file_sink::sptr enable_tap();
  bool tap_enabled() const { return tap_out != nullptr; }
  void tap_open(const std::string &base, const std::string &meta_json = "");
  void tap_close();

private:
  double d_bandwidth;
  int d_samples_per_symbol;
  bool d_use_squelch;
  double d_excess_bw;
  ConditioningSquelch d_squelch_mode;
  ConditioningHooks d_cond;
  std::function<void(bool, double)> d_squelch_cb;
  ControlDecoderSpec d_decoder{ControlDecoder::None, 0, "", 0};
  std::function<void(const DecodedMessage &)> d_message_cb;
  void send_conditioning();
  std::function<void(double)> tune_hook;
  std::function<void(double, double)> filter_hook;

  double d_channel_rate_in = 0;
  double d_channel_rate_out = 0;
  gr::blocks::file_sink::sptr tap_out;
};

#endif
