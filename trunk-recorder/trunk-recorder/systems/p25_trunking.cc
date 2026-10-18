
#include "p25_trunking.h"
#include <boost/log/trivial.hpp>
#include <chrono>

p25_trunking_sptr make_p25_trunking(double freq, double center, const ChannelTap &tap, bool qpsk, int sys_num) {
  return gnuradio::get_initial_sptr(new p25_trunking(freq, center, tap, qpsk, sys_num));
}

void p25_trunking::initialize_fsk4() {

  double phase1_channel_rate = phase1_symbol_rate * phase1_samples_per_symbol;
  system_channel_rate = phase1_symbol_rate * phase1_samples_per_symbol;
  samples_per_symbol = phase1_samples_per_symbol;
  symbol_rate = phase1_symbol_rate;
  // double phase2_channel_rate = phase2_symbol_rate * phase2_samples_per_symbol;
  const double pi = M_PI;

  // FSK4: Phase Loop Lock - can only be Phase 1, so locking at that rate.
  double freq_to_norm_radians = pi / (phase1_channel_rate / 2.0);
  double fc = 0.0;
  double fd = 600.0;
  double pll_demod_gain = 1.0 / (fd * freq_to_norm_radians);
  pll_freq_lock = gr::analog::pll_freqdet_cf::make((phase1_symbol_rate / 2.0 * 1.2) * freq_to_norm_radians, (fc + (3 * fd * 1.9)) * freq_to_norm_radians, (fc + (-3 * fd * 1.9)) * freq_to_norm_radians);
  pll_amp = gr::blocks::multiply_const_ff::make(pll_demod_gain * 1.0); // source->get_());

  // FSK4: noise filter - can only be Phase 1, so locking at that rate.
  baseband_noise_filter_taps = gr::filter::firdes::low_pass_2(1.0, phase1_channel_rate, phase1_symbol_rate / 2.0 * 1.175, phase1_symbol_rate / 2.0 * 0.125, 20.0, gr::fft::window::WIN_KAISER, 6.76);

  noise_filter = gr::filter::fft_filter_fff::make(1.0, baseband_noise_filter_taps);

  // FSK4: Symbol Taps
  double symbol_decim = 1;
  for (int i = 0; i < samples_per_symbol; i++) {
    sym_taps.push_back(1.0 / samples_per_symbol);
  }
  sym_filter = gr::filter::fir_filter_fff::make(symbol_decim, sym_taps);

  // FSK4: FSK4 Demod - locked at Phase 1 rates, since it can only be Phase 1
  tune_queue = gr::msg_queue::make(20);
  fsk4_demod = gr::op25_repeater::fsk4_demod_ff::make(tune_queue, phase1_channel_rate, phase1_symbol_rate);

  connect(self(), 0, pll_freq_lock, 0);
  connect(pll_freq_lock, 0, pll_amp, 0);
  connect(pll_amp, 0, noise_filter, 0);
  connect(noise_filter, 0, sym_filter, 0);
  connect(sym_filter, 0, fsk4_demod, 0);
  connect(fsk4_demod, 0, slicer, 0);
}

void p25_trunking::initialize_qpsk() {
  const double pi = M_PI;

  agc = gr::analog::feedforward_agc_cc::make(16, 1.0);
  system_channel_rate = phase1_symbol_rate * phase1_samples_per_symbol;
  samples_per_symbol = phase1_samples_per_symbol;
  symbol_rate = phase1_symbol_rate;
  // Gardner Costas Clock
  double gain_mu = 0.025; // 0.025
  double costas_alpha = 0.008;
  double omega = double(system_channel_rate) / symbol_rate; // set to 6000 for TDMA, should be symbol_rate
  double gain_omega = 0.1 * gain_mu * gain_mu;
  double fmax = 3000; // Hz
  fmax = 2 * pi * fmax / double(system_channel_rate);

  costas = gr::op25_repeater::costas_loop_cc::make(costas_alpha,  4, (2 * pi)/4 ); 
  clock = gr::op25_repeater::gardner_cc::make(omega, gain_mu, gain_omega);

  // QPSK: Perform Differential decoding on the constellation
  diffdec = gr::digital::diff_phasor_cc::make();

  // QPSK: take angle of the difference (in radians)
  to_float = gr::blocks::complex_to_arg::make();

  // QPSK: convert from radians such that signal is in -3/-1/+1/+3
  rescale = gr::blocks::multiply_const_ff::make((1 / (pi / 4)));




  connect(self(), 0, clock, 0);
  connect(clock, 0, diffdec, 0);
  connect(diffdec, 0, costas,0);
  connect(costas,0, to_float, 0);
  connect(to_float, 0, rescale, 0);
  connect(rescale, 0, slicer, 0);
}

void p25_trunking::initialize_p25() {
  // OP25 Slicer
  const float l[] = {-2.0, 0.0, 2.0, 4.0};
  const int msgq_id = 0;
  const int debug = 0;
  std::vector<float> slices(l, l + sizeof(l) / sizeof(l[0]));

  slicer = gr::op25_repeater::fsk4_slicer_fb::make(msgq_id, debug, slices);

  // OP25 Frame Assembler
  traffic_queue = gr::msg_queue::make(2);
  tune_queue = gr::msg_queue::make(2);
  gr::msg_queue::sptr rx_queue = gr::msg_queue::make(1);

  int udp_port = 0;
  int verbosity = 0;
  const char *wireshark_host = "0";
  bool do_imbe = 0;
  int silence_frames = 0;
  bool do_output = 0;
  bool do_msgq = 1;
  bool do_audio_output = 0;
  bool do_tdma = 0;
  bool do_nocrypt = 1;
  bool soft_vocoder = false;
  op25_frame_assembler = gr::op25_repeater::p25_frame_assembler::make(silence_frames, soft_vocoder, wireshark_host, udp_port, verbosity, do_imbe, do_output, do_msgq, rx_queue, do_audio_output, do_tdma, do_nocrypt);
  autotune_offset = 0;

  connect(slicer, 0, op25_frame_assembler, 0);
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
  rust_decoder = tap.p25_decoder == P25DecoderImpl::Rust;
  run_rust = rust_decoder || tap.p25_shadow;
  run_op25 = !rust_decoder || tap.p25_shadow;
  observe = tap.observe;

  prefilter = channel_control::make(channel_control::phase1_samples_per_symbol, channel_control::phase1_symbol_rate, channel_control::channel_bandwidth, false, tap.rate, channel_control::default_excess_bw);
  prefilter->set_tune_hook(tap.set_offset);
  prefilter->set_filter_hook(tap.set_filter);
  prefilter->set_conditioning_hooks(tap.cond);

  if (run_rust) {
    // Demodulated and decoded in the channelizer, on its conditioned output;
    // the port still feeds this block, which only taps or discards it.
    prefilter->set_decoder(qpsk_mod ? ControlDecoder::P25Cqpsk : ControlDecoder::P25C4fm, tap.system, f, rust_decoder);
    if (!run_op25 && tap.tap_base.empty()) {
      null_sink = gr::blocks::null_sink::make(sizeof(gr_complex));
      connect(self(), 0, null_sink, 0);
    }
  }
  if (run_op25) {
    initialize_p25();
    if (!qpsk_mod) {
      initialize_fsk4();
    } else {
      initialize_qpsk();
    }
  }
  BOOST_LOG_TRIVIAL(info) << "\t P25 control channel decoder: " << (rust_decoder ? "trunk-p25" : "op25")
                          << (tap.p25_shadow ? (rust_decoder ? " (op25 shadow)" : " (trunk-p25 shadow)") : "");
  if (!tap.tap_base.empty()) {
    connect(self(), 0, prefilter->enable_tap(), 0);
    prefilter->tap_open(tap.tap_base, "\"kind\":\"control_channel\",\"modulation\":\"" + std::string(qpsk_mod ? "qpsk" : "fsk4") +
                                          "\",\"decoder\":\"" + std::string(rust_decoder ? "rust" : "op25") +
                                          "\",\"freq\":" + std::to_string(static_cast<long long>(f)) +
                                          ",\"center\":" + std::to_string(static_cast<long long>(c)) +
                                          (!run_op25 ? std::string() : ",\"dibits_file\":\"" + tap.tap_base + ".dibits\""));
    if (run_op25) {
      dibit_sink = gr::blocks::file_sink::make(1, (tap.tap_base + ".dibits").c_str());
      connect(slicer, 0, dibit_sink, 0);
    }
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
  prefilter->tune_offset(offset_amount);
  // The Rust decoder resets itself when its slot is retuned.
  if (run_op25 && qpsk_mod) {
    costas->set_phase(0);
    costas->set_frequency(0);
  }
}

int p25_trunking::get_freq_error() { 
  // Get current tuningerror from FLL
  return prefilter->get_freq_error();
}

void p25_trunking::set_msg_callback(std::function<void(gr::message::sptr)> cb) {
  // Only the primary's messages reach trunking; every decoder's are observed.
  if (run_rust) {
    // The messages op25's p25p1_fdma builds: only a timeout carries a time
    // (arg2, wall clock seconds, as log_ts::get_ts).
    prefilter->set_message_callback([cb, primary = rust_decoder, observe = observe](const DecodedMessage &m) {
      if (observe) observe(P25DecoderImpl::Rust, primary, m.type, m.payload);
      if (!primary) return;
      constexpr long timeout = 0xffff; // get_msg_type(PROTOCOL_P25, M_P25_TIMEOUT)
      const double ts = m.type == timeout
          ? std::chrono::duration<double>(std::chrono::system_clock::now().time_since_epoch()).count()
          : 0.0;
      cb(gr::message::make_from_string(m.payload, m.type, 0, ts));
    });
  }
  if (run_op25 && op25_frame_assembler) {
    op25_frame_assembler->set_msg_callback([cb, primary = !rust_decoder, observe = observe](gr::message::sptr msg) {
      if (observe) observe(P25DecoderImpl::Op25, primary, msg->type(), msg->to_string());
      if (primary) cb(std::move(msg));
    });
  }
}

void p25_trunking::finetune_control_freq(double f) {
  // Minor tuning adjustment without resetting costas or phase
  chan_freq = f;
  int offset_amount = (center_freq - f);
  prefilter->tune_offset(offset_amount);
}
