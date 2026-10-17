#include "channel_control.h"
#include <boost/log/trivial.hpp>
#include <cmath>
#include <fstream>
#include <stdexcept>

channel_control::sptr channel_control::make(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate, double excess_bw) {
  return std::make_shared<channel_control>(samples_per_symbol, symbol_rate, bandwidth, use_squelch, channel_rate, excess_bw);
}

const int channel_control::smartnet_samples_per_symbol;
const int channel_control::phase1_samples_per_symbol;
const int channel_control::phase2_samples_per_symbol;
const double channel_control::phase1_symbol_rate;
const double channel_control::phase2_symbol_rate;
const double channel_control::smartnet_symbol_rate;

channel_control::channel_control(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate_in, double excess_bw)
    : d_bandwidth(bandwidth),
      d_samples_per_symbol(samples_per_symbol),
      d_use_squelch(use_squelch),
      d_excess_bw(excess_bw),
      d_squelch_mode(use_squelch ? CONDITIONING_SQUELCH_DIGITAL : CONDITIONING_SQUELCH_NONE),
      d_channel_rate_in(channel_rate_in),
      d_channel_rate_out(symbol_rate * samples_per_symbol) {
  if (d_channel_rate_out > d_channel_rate_in) {
    throw std::runtime_error("channel_control: channel rate " + std::to_string(d_channel_rate_in) + " is below the symbol-rate multiple " + std::to_string(d_channel_rate_out));
  }
  BOOST_LOG_TRIVIAL(info) << "\t Channel " << d_channel_rate_in << " -> " << d_channel_rate_out << " S/s, conditioned in the channelizer"
                          << (d_use_squelch ? " (with squelch)" : "");
}

int channel_control::get_freq_error() {
  return d_cond.freq_error ? d_cond.freq_error() : 0;
}

bool channel_control::is_squelched() {
  return d_cond.squelch_open && !d_cond.squelch_open();
}

double channel_control::get_pwr() {
  const double db = d_cond.power_db ? d_cond.power_db() : std::nan("");
  return std::isnan(db) ? DB_UNSET : db;
}

void channel_control::set_squelch_callback(std::function<void(bool, double)> cb) {
  d_squelch_cb = std::move(cb);
  if (d_cond.set_squelch_callback) d_cond.set_squelch_callback(d_squelch_cb);
}

void channel_control::tune_offset(double f) {
  // f is (source centre - channel); the channelizer wants channel - centre.
  if (tune_hook) {
    tune_hook(-f);
  }
}

void channel_control::set_max_dev(double max_dev) {
  // The channel filter lives in the shared channelizer: cutoff max_dev,
  // transition bandwidth/2, applied to this channel only.
  if (filter_hook) {
    filter_hook(max_dev, d_bandwidth / 2);
  }
}

void channel_control::set_squelch_db(double squelch_db) {
  if (d_cond.set_squelch_db) d_cond.set_squelch_db(squelch_db);
}

void channel_control::set_analog_squelch(bool analog_squelch) {
  if (d_use_squelch) {
    d_squelch_mode = analog_squelch ? CONDITIONING_SQUELCH_ANALOG : CONDITIONING_SQUELCH_DIGITAL;
    send_conditioning();
  }
}

void channel_control::send_conditioning() {
  if (d_cond.set) {
    d_cond.set({static_cast<unsigned>(d_samples_per_symbol), d_excess_bw, d_channel_rate_in, d_channel_rate_out, d_squelch_mode});
  }
}

void channel_control::set_conditioning_hooks(ConditioningHooks hooks) {
  d_cond = std::move(hooks);
  send_conditioning();
  if (d_squelch_cb && d_cond.set_squelch_callback) d_cond.set_squelch_callback(d_squelch_cb);
}

void channel_control::set_samples_per_symbol(int samples_per_symbol) {
  d_samples_per_symbol = samples_per_symbol;
  send_conditioning();
}

gr::blocks::file_sink::sptr channel_control::enable_tap() {
  if (!tap_out) {
    // file_sink opens its file on construction; point it at /dev/null and
    // close it so the sink drops samples until tap_open().
    tap_out = gr::blocks::file_sink::make(sizeof(gr_complex), "/dev/null");
    tap_out->close();
  }
  return tap_out;
}

void channel_control::tap_open(const std::string &base, const std::string &meta_json) {
  if (!tap_out) return;
  tap_out->open((base + ".out.cf32").c_str());
  std::ofstream meta(base + ".json");
  meta << "{\"format\":\"cf32\",\"in_rate\":" << d_channel_rate_in
       << ",\"out_rate\":" << d_channel_rate_out
       << ",\"samples_per_symbol\":" << d_samples_per_symbol
       << ",\"squelch\":" << (d_use_squelch ? "true" : "false");
  if (!meta_json.empty()) meta << "," << meta_json;
  meta << "}\n";
}

void channel_control::tap_close() {
  if (!tap_out) return;
  tap_out->close();
}
