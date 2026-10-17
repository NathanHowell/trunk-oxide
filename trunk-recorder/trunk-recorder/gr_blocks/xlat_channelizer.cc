#include "xlat_channelizer.h"
#include <cmath>
#include <fstream>
#include <stdexcept>

xlat_channelizer::sptr xlat_channelizer::make(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate, double excess_bw, bool external_conditioning) {

  return gnuradio::get_initial_sptr(new xlat_channelizer(samples_per_symbol, symbol_rate, bandwidth, use_squelch, channel_rate, excess_bw, external_conditioning));
}

const int xlat_channelizer::smartnet_samples_per_symbol;
const int xlat_channelizer::phase1_samples_per_symbol;
const int xlat_channelizer::phase2_samples_per_symbol;
const double xlat_channelizer::phase1_symbol_rate;
const double xlat_channelizer::phase2_symbol_rate;
const double xlat_channelizer::smartnet_symbol_rate;

xlat_channelizer::xlat_channelizer(int samples_per_symbol, double symbol_rate, double bandwidth, bool use_squelch, double channel_rate_in, double excess_bw, bool external_conditioning)
    : gr::hier_block2("xlat_channelizer_ccf",
                      gr::io_signature::make(1, 1, sizeof(gr_complex)),
                      gr::io_signature::make(1, 1, sizeof(gr_complex))),
      d_bandwidth(bandwidth),
      d_samples_per_symbol(samples_per_symbol),
      d_use_squelch(use_squelch),
      d_external(external_conditioning),
      d_excess_bw(excess_bw),
      d_squelch_mode(use_squelch ? CONDITIONING_SQUELCH_DIGITAL : CONDITIONING_SQUELCH_NONE) {

  long channel_rate = symbol_rate * d_samples_per_symbol;
  d_channel_rate_in = channel_rate_in;
  d_channel_rate_out = channel_rate;
  // long if_rate = 12500;

  const float pi = M_PI;

  // shared_channelizer already delivered this channel at channel_rate_in.
  const double resampled_rate = channel_rate_in;
  BOOST_LOG_TRIVIAL(info) << "\t Xlating Channelizer (shared) - input rate: " << channel_rate_in;
  // ARB Resampler
  double arb_rate = channel_rate / resampled_rate;

  double arb_size = 32;
  double arb_atten = 30; // was originally 100
  // Create a filter that covers the full bandwidth of the output signal

  // If rate >= 1, we need to prevent images in the output,
  // so we have to filter it to less than half the channel
  // width of 0.5.  If rate < 1, we need to filter to less
  // than half the output signal's bw to avoid aliasing, so
  // the half-band here is 0.5*rate.
  double percent = 0.80;

  if (arb_rate < 1) {
    double halfband = 0.5 * arb_rate;
    double bw = percent * halfband;
    double tb = (percent / 2.0) * halfband;

// BOOST_LOG_TRIVIAL(info) << "Arb Rate: " << arb_rate << " Half band: " << halfband << " bw: " << bw << " tb: " <<
// tb;

// As we drop the bw factor, the optfir filter has a harder time converging;
// using the firdes method here for better results.
    arb_taps = gr::filter::firdes::low_pass_2(arb_size, arb_size, bw, tb, arb_atten, gr::fft::window::WIN_BLACKMAN_HARRIS);
    BOOST_LOG_TRIVIAL(info) << "\t Channelizer ARB - Symbol Rate: " << channel_rate << " Resampled Rate: " << resampled_rate << " ARB Rate: " << arb_rate << " ARB Taps: " << arb_taps.size() << " BW: " << bw << " TB: " << tb;
    arb_resampler = gr::filter::pfb_arb_resampler_ccf::make(arb_rate, arb_taps);
  } else if (arb_rate > 1) {
    throw std::runtime_error("xlat_channelizer: resampling rate too low (arb_rate > 1)");
  }


  // Squelch DB
  // on a trunked network where you know you will have good signal, a carrier
  // power squelch works well. real FM receviers use a noise squelch, where
  // the received audio is high-passed above the cutoff and then fed to a
  // reverse squelch. If the power is then BELOW a threshold, open the squelch.

  squelch = callback_pwr_squelch_cc::make(0.0, 0.0001, 0, true);

  if (d_external) {
    // The channelizer resamples and conditions the channel; nothing left to do here.
    BOOST_LOG_TRIVIAL(info) << "\t Conditioning (" << (arb_rate < 1 ? "resampler, " : "") << (d_use_squelch ? "squelch, " : "")
                            << "AGC, FLL) in the channelizer";
    // TODO: a hier_block2 cannot wire its input straight to its output; this
    // goes away when the callers connect around the xlat_channelizer.
    passthrough = gr::blocks::copy::make(sizeof(gr_complex));
    connect(self(), 0, passthrough, 0);
    connect(passthrough, 0, self(), 0);
    return;
  }

  // Power probe: taps off the filtered signal to measure average magnitude
  // without affecting the main signal path. alpha=0.0001 for a slow average.
  pwr_probe = gr::analog::probe_avg_mag_sqrd_c::make(-200.0, 0.0001);

  rms_agc = gr::blocks::rms_agc::make(0.45, 0.85);
  // Legacy FLL loop-filter bandwidth (OP25 historically uses 350 instead of 250).
  double fll_loop_bw_legacy = (2.0 * pi) / d_samples_per_symbol / 250;
// NOTE: GNURADIO_VERSION comes from CMake as digit-pairs (3.10.12 -> 0x031012), not
// plain hex (0x030a0c), so these thresholds are 3.10.13 -> 0x031013 and 3.11.0 -> 0x031100.
// Plain-hex thresholds (0x030a0d/0x030b00) are always true for 3.10.x and select the
// improved-filter loop bandwidth against the legacy FLL, freezing it.
#if GNURADIO_VERSION >= 0x031013
  // gnuradio PR #7890 reworked the FLL loop filter. The improved filter expects
  // bandwidth as BL*T (loop noise bandwidth * symbol period). Convert the legacy
  // gain so closed-loop dynamics roughly match the prior behavior.
  double fll_loop_bw = (fll_loop_bw_legacy * fll_loop_bw_legacy * d_samples_per_symbol) /
                       ((1.0 + M_SQRT2 * fll_loop_bw_legacy + fll_loop_bw_legacy * fll_loop_bw_legacy) * 2.0 * pi);
#if GNURADIO_VERSION >= 0x031100
  // 3.11+: the improved filter is the only behavior; the opt-in bool was removed.
  fll_band_edge = gr::digital::fll_band_edge_cc::make(d_samples_per_symbol, excess_bw, 2 * d_samples_per_symbol + 1, fll_loop_bw);
#else
  // 3.10 backport: opt in via the new `improved_loop_filter` bool to silence the legacy warning.
  fll_band_edge = gr::digital::fll_band_edge_cc::make(d_samples_per_symbol, excess_bw, 2 * d_samples_per_symbol + 1, fll_loop_bw, true);
#endif
#else
  fll_band_edge = gr::digital::fll_band_edge_cc::make(d_samples_per_symbol, excess_bw, 2 * d_samples_per_symbol + 1, fll_loop_bw_legacy);
#endif

  // The block's own input is the single channel.
  gr::basic_block_sptr head = self();
  if (d_use_squelch) {
    BOOST_LOG_TRIVIAL(info) << "Conventional - with Squelch";
    if (arb_rate == 1.0) {
      connect(head, 0, squelch, 0);
    } else {
      connect(head, 0, arb_resampler, 0);
      connect(arb_resampler, 0, squelch, 0);
    }
    connect(squelch, 0, rms_agc, 0);
  } else {
    if (arb_rate == 1.0) {
      connect(head, 0, rms_agc, 0);
    } else {
      connect(head, 0, arb_resampler, 0);
      connect(arb_resampler, 0, rms_agc, 0);
    }
  }

  // Tap the power probe off the channel filter output (before resampling/squelch)
  connect(head, 0, pwr_probe, 0);

  connect(rms_agc, 0, fll_band_edge, 0);
  connect(fll_band_edge, 0, self(), 0);
}

int xlat_channelizer::get_freq_error() {
  if (d_external) {
    return d_cond.freq_error ? d_cond.freq_error() : 0;
  } // get frequency error from FLL and convert to Hz
  const float pi = M_PI;
  long if_rate = 24000;
  return int((fll_band_edge->get_frequency() / (2 * pi)) * if_rate);
}

bool xlat_channelizer::is_squelched() {
  if (d_external) {
    return d_cond.squelch_open && !d_cond.squelch_open();
  }
  return !squelch->unmuted();
}

double xlat_channelizer::get_pwr() {
  if (d_external) {
    const double db = d_cond.power_db ? d_cond.power_db() : std::nan("");
    return std::isnan(db) ? DB_UNSET : db;
  }
  double level = pwr_probe->level();
  if (level > 0.0) {
    return 10.0 * std::log10(level);
  }
  return DB_UNSET;
}

void xlat_channelizer::tune_offset(double f) {
  // f is (source centre - channel); the channelizer wants channel - centre.
  if (tune_hook) {
    tune_hook(-f);
  }
}

void xlat_channelizer::set_max_dev(double max_dev) {
  // The channel filter lives in the shared channelizer: cutoff max_dev,
  // transition bandwidth/2, applied to this channel only.
  if (filter_hook) {
    filter_hook(max_dev, d_bandwidth / 2);
  }
}

void xlat_channelizer::set_squelch_db(double squelch_db) {
  if (d_external) {
    if (d_cond.set_squelch_db) d_cond.set_squelch_db(squelch_db);
    return;
  }
  squelch->set_threshold(squelch_db);
}

void xlat_channelizer::set_analog_squelch(bool analog_squelch) {
  if (d_external) {
    if (d_use_squelch) {
      d_squelch_mode = analog_squelch ? CONDITIONING_SQUELCH_ANALOG : CONDITIONING_SQUELCH_DIGITAL;
      send_conditioning();
    }
    return;
  }
  if (analog_squelch) {
    squelch->set_alpha(0.01);
    squelch->set_ramp(10);
    squelch->set_gate(false);
  } else {
    squelch->set_alpha(0.0001);
    squelch->set_ramp(0);
    squelch->set_gate(true);
  }
}

void xlat_channelizer::send_conditioning() {
  if (d_cond.set) {
    d_cond.set({static_cast<unsigned>(d_samples_per_symbol), d_excess_bw, d_channel_rate_in, d_channel_rate_out, d_squelch_mode});
  }
}

void xlat_channelizer::set_conditioning_hooks(ConditioningHooks hooks) {
  d_cond = std::move(hooks);
  send_conditioning();
  if (d_squelch_cb && d_cond.set_squelch_callback) d_cond.set_squelch_callback(d_squelch_cb);
}

void xlat_channelizer::set_samples_per_symbol(int samples_per_symbol) {
  if (d_external) {
    d_samples_per_symbol = samples_per_symbol;
    send_conditioning();
    return;
  }
  fll_band_edge->set_samples_per_symbol(samples_per_symbol);
}

std::vector<gr::block_sptr> xlat_channelizer::get_metric_blocks() const {
  std::vector<gr::block_sptr> blocks;
  if (d_external) return blocks;
  if (arb_resampler) blocks.push_back(arb_resampler);
  if (fll_band_edge) blocks.push_back(fll_band_edge);
  return blocks;
}

void xlat_channelizer::enable_tap() {
  if (tap_in) return;
  // file_sink opens its file on construction; point it at /dev/null and close
  // it so the sink drops samples until tap_open().
  tap_in = gr::blocks::file_sink::make(sizeof(gr_complex), "/dev/null");
  tap_out = gr::blocks::file_sink::make(sizeof(gr_complex), "/dev/null");
  tap_in->close();
  tap_out->close();
  connect(self(), 0, tap_in, 0);
  if (d_external) {
    connect(passthrough, 0, tap_out, 0); // conditioned in the channelizer: input is already the output
  } else {
    connect(fll_band_edge, 0, tap_out, 0);
  }
}

void xlat_channelizer::tap_open(const std::string &base, const std::string &meta_json) {
  if (!tap_in) return;
  tap_in->open((base + ".in.cf32").c_str());
  tap_out->open((base + ".out.cf32").c_str());
  std::ofstream meta(base + ".json");
  meta << "{\"format\":\"cf32\",\"in_rate\":" << d_channel_rate_in
       << ",\"out_rate\":" << d_channel_rate_out
       << ",\"samples_per_symbol\":" << d_samples_per_symbol
       << ",\"squelch\":" << (d_use_squelch ? "true" : "false");
  if (!meta_json.empty()) meta << "," << meta_json;
  meta << "}\n";
}

void xlat_channelizer::tap_close() {
  if (!tap_in) return;
  tap_in->close();
  tap_out->close();
}
