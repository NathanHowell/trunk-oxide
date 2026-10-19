
#include "p25_trunking.h"
#include <boost/log/trivial.hpp>
#include <chrono>

p25_trunking_sptr make_p25_trunking(double freq, double center, const ChannelTap &tap, bool qpsk, int sys_num) {
  return gnuradio::get_initial_sptr(new p25_trunking(freq, center, tap, qpsk, sys_num));
}

p25_trunking::p25_trunking(double f, double c, const ChannelTap &tap, bool qpsk, int sys_num)
    : gr::hier_block2("p25_trunking",
                      gr::io_signature::make(1, 1, sizeof(gr_complex)),
                      gr::io_signature::make(0, 0, sizeof(float))) {

  this->sys_num = sys_num;
  chan_freq = f;
  center_freq = c;
  input_rate = static_cast<long>(tap.rate);
  qpsk_mod = qpsk;

  prefilter = channel_control::make(channel_control::phase1_samples_per_symbol, channel_control::phase1_symbol_rate, channel_control::channel_bandwidth, false, tap.rate, channel_control::default_excess_bw);
  prefilter->set_tune_hook(tap.set_offset);
  prefilter->set_filter_hook(tap.set_filter);
  prefilter->set_conditioning_hooks(tap.cond);

  // trunk-p25 demodulates and decodes in the channelizer, on its conditioned
  // output; the port still feeds this block, which only taps or discards it.
  prefilter->set_decoder(qpsk_mod ? ControlDecoder::P25Cqpsk : ControlDecoder::P25C4fm, tap.system, f);
  if (tap.tap_base.empty()) {
    null_sink = gr::blocks::null_sink::make(sizeof(gr_complex));
    connect(self(), 0, null_sink, 0);
  } else {
    connect(self(), 0, prefilter->enable_tap(), 0);
    prefilter->tap_open(tap.tap_base, "\"kind\":\"control_channel\",\"modulation\":\"" + std::string(qpsk_mod ? "qpsk" : "fsk4") +
                                          "\",\"decoder\":\"rust\",\"freq\":" + std::to_string(static_cast<long long>(f)) +
                                          ",\"center\":" + std::to_string(static_cast<long long>(c)));
  }
  tune_freq(chan_freq);
}

p25_trunking::~p25_trunking() {}

void p25_trunking::set_center(double c) {
  center_freq = c;
}

void p25_trunking::set_rate(long s) {
  input_rate = s;
  // TODO: Update/remake blocks that depend on input_rate
}

double p25_trunking::get_pwr() {
  return prefilter->get_pwr();
}

double p25_trunking::get_freq() {
  return chan_freq;
}

void p25_trunking::tune_freq(double f) {
  autotune_offset = 0;
  chan_freq = f;
  int offset_amount = (center_freq - f);
  // The decoder resets itself when its slot is retuned.
  prefilter->tune_offset(offset_amount);
}

int p25_trunking::get_freq_error() { 
  // Get current tuningerror from FLL
  return prefilter->get_freq_error();
}

void p25_trunking::set_msg_callback(std::function<void(gr::message::sptr)> cb) {
  // The messages op25's p25p1_fdma builds: only a timeout carries a time
  // (arg2, wall clock seconds, as log_ts::get_ts).
  prefilter->set_message_callback([cb](const DecodedMessage &m) {
    constexpr long timeout = 0xffff; // get_msg_type(PROTOCOL_P25, M_P25_TIMEOUT)
    const double ts = m.type == timeout
        ? std::chrono::duration<double>(std::chrono::system_clock::now().time_since_epoch()).count()
        : 0.0;
    cb(gr::message::make_from_string(m.payload, m.type, 0, ts));
  });
}

void p25_trunking::finetune_control_freq(double f) {
  // Minor tuning adjustment without resetting costas or phase
  chan_freq = f;
  int offset_amount = (center_freq - f);
  prefilter->tune_offset(offset_amount);
}
