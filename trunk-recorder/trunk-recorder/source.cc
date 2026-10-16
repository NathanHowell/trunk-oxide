#include "source.h"
#include "formatter.h"
#include <chrono>
#include <cmath>
#include <stdexcept>
#include <gnuradio/blocks/null_sink.h>

using json = nlohmann::json;

static int src_counter = 0;

int Source::get_num() {
  return src_num;
};

gr::basic_block_sptr Source::get_src_block() {
  return source_block;
}

void Source::init_shared() {
}

const Config &Source::get_config() {
  return config;
}

void Source::set_min_max() {
  long s = rate;
  long if_freqs[] = {24000, 25000, 32000};
  long decim = 24000;
  for (int i = 0; i < 3; i++) {
    long if_freq = if_freqs[i];
    if (s % if_freq != 0) {
      continue;
    }
    long q = s / if_freq;
    if (q & 1) {
      continue;
    }

    if ((q >= 40) && ((q & 3) == 0)) {
      decim = q / 4;
    } else {
      decim = q / 2;
    }
  }
  long if1 = rate / decim;
  min_hz = center - ((rate / 2) - (if1 / 2));
  max_hz = center + ((rate / 2) - (if1 / 2));
}

Source::Source(double c, double r, double e, std::string drv, std::string dev, const Config &cfg)
    : config(cfg) {
  rate = r;
  center = c;
  error = e;
  set_min_max();
  driver = drv;
  device = dev;
  gain = 0;
  lna_gain = 0;
  tia_gain = 0;
  pga_gain = 0;
  mix_gain = 0;
  if_gain = 0;
  src_num = src_counter++;
  max_digital_recorders = 0;
  max_debug_recorders = 0;
  max_sigmf_recorders = 0;
  max_analog_recorders = 0;
  debug_recorder_port = 0;
  attached_detector = false;
  attached_selector = false;
  next_selector_port = 0;

  recorder_selector = gr::blocks::selector::make(sizeof(gr_complex), 0, 0);

  // parameters for signal_detector_cvf
  float threshold_sensitivity = 0.9;
  bool auto_threshold = true;
  float threshold = -45;
  int fft_len = 1024;
  float average = 0.8;
  float quantization = 0.01;
  float min_bw = 0.0;
  float max_bw = 50000;

  signal_detector = signal_detector_cvf::make(rate, fft_len, 0, threshold, threshold_sensitivity, auto_threshold, average, quantization, min_bw, max_bw, "");
  BOOST_LOG_TRIVIAL(info) << "Made the Signal Detector";

#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    osmosdr::source::sptr osmo_src;
    std::vector<std::string> gain_names;
    if (dev == "") {
      BOOST_LOG_TRIVIAL(info) << "Source Device not specified";
      osmo_src = osmosdr::source::make();
    } else {
      std::ostringstream msg;

      if (isdigit(dev[0])) {  // Assume this is a serial number and fail back
                              // to using rtl as default
        msg << "rtl=" << dev; // <<  ",buflen=32764,buffers=8";
        BOOST_LOG_TRIVIAL(info) << "Source device name missing, defaulting to rtl device";
      } else {
        msg << dev; // << ",buflen=32764,buffers=8";
      }
      BOOST_LOG_TRIVIAL(info) << "Source Device: " << msg.str();
      osmo_src = osmosdr::source::make(msg.str());
    }
    BOOST_LOG_TRIVIAL(info) << "SOURCE TYPE OSMOSDR (osmosdr)";
    BOOST_LOG_TRIVIAL(info) << "Setting sample rate to: " << FormatSamplingRate(rate);
    osmo_src->set_sample_rate(rate);
    actual_rate = osmo_src->get_sample_rate();
    rate = round(actual_rate);
    BOOST_LOG_TRIVIAL(info) << "Actual sample rate: " << FormatSamplingRate(actual_rate);
    BOOST_LOG_TRIVIAL(info) << "Tuning to " << format_freq(center + error);
    osmo_src->set_center_freq(center + error, 0);
    gain_names = osmo_src->get_gain_names();
    std::string gain_list;
    for (std::vector<std::string>::iterator it = gain_names.begin(); it != gain_names.end(); it++) {
      std::string gain_name = *it;
      osmosdr::gain_range_t range = osmo_src->get_gain_range(gain_name);
      std::vector<double> gains = range.values();
      std::string gain_opt_str;
      for (std::vector<double>::iterator gain_it = gains.begin(); gain_it != gains.end(); gain_it++) {
        double gain_opt = *gain_it;
        std::ostringstream ss;
        // gain_opt = floor(gain_opt * 10) / 10;
        ss << gain_opt << " ";

        gain_opt_str += ss.str();
      }
      BOOST_LOG_TRIVIAL(info) << "Gain Stage: " << gain_name << " supported values: " << gain_opt_str;
    }

    source_block = osmo_src;
  }
#endif

  if (driver == "usrp") {
    gr::uhd::usrp_source::sptr usrp_src;
    usrp_src = gr::uhd::usrp_source::make(device, uhd::stream_args_t("fc32"));

    BOOST_LOG_TRIVIAL(info) << "SOURCE TYPE USRP (UHD)";

    BOOST_LOG_TRIVIAL(info) << "Setting sample rate to: " << FormatSamplingRate(rate);
    usrp_src->set_samp_rate(rate);
    actual_rate = usrp_src->get_samp_rate();
    BOOST_LOG_TRIVIAL(info) << "Actual sample rate: " << FormatSamplingRate(actual_rate);
    BOOST_LOG_TRIVIAL(info) << "Tuning to " << format_freq(center + error);
    usrp_src->set_center_freq(center + error, 0);

    source_block = usrp_src;
  }
}

void Source::set_iq_source(std::string iq_file, bool repeat, double center, double rate) {
  this->rate = rate;
  this->center = center;
  error = 0;
  min_hz = center - ((rate / 2));
  max_hz = center + ((rate / 2));
  driver = "iqfile";
  device = "";
  gain = 0;
  lna_gain = 0;
  tia_gain = 0;
  pga_gain = 0;
  mix_gain = 0;
  if_gain = 0;
  src_num = src_counter++;
  max_digital_recorders = 0;
  max_debug_recorders = 0;
  max_sigmf_recorders = 0;
  max_analog_recorders = 0;
  debug_recorder_port = 0;
  attached_detector = false;
  attached_selector = false;
  next_selector_port = 0;

  recorder_selector = gr::blocks::selector::make(sizeof(gr_complex), 0, 0);

  // Same signal detector as the hardware ctor; conventional recorders attach it.
  signal_detector = signal_detector_cvf::make(rate, 1024, 0, -45, 0.9, true, 0.8, 0.01, 0.0, 50000, "");

  iq_file_source::sptr iq_file_src;
  iq_file_src = iq_file_source::make(iq_file, this->rate, repeat);

  BOOST_LOG_TRIVIAL(info) << "SOURCE TYPE IQ FILE";
  BOOST_LOG_TRIVIAL(info) << "Setting Center to: " << FormatSamplingRate(center);
  BOOST_LOG_TRIVIAL(info) << "Setting sample rate to: " << FormatSamplingRate(rate);

  source_block = iq_file_src;
}

Source::Source(std::string sigmf_meta, std::string sigmf_data, bool repeat, const Config &cfg)
    : config(cfg) {
  json data;
  std::cout << sigmf_meta << std::endl;
  try {
    std::ifstream f(sigmf_meta);
    data = json::parse(f);
  } catch (const json::parse_error &e) {
    // output exception information
    BOOST_LOG_TRIVIAL(error) << "JSON parse error in source config: " << e.what()
                            << " (id: " << e.id << ", byte: " << e.byte << ")";
    throw;
  }

  std::cout << data.dump(4) << std::endl;
  json global = data["global"];
  this->rate = global["core:sample_rate"];

  json capture = data["captures"][0];
  this->center = capture["core:frequency"];
  std::cout << "Rate: " << rate << "Center: " << center << std::endl;
  set_iq_source(sigmf_data, repeat, center, rate);
}

Source::Source(std::string iq_file, bool repeat, double center, double rate, const Config &cfg)
    : config(cfg) {
  set_iq_source(iq_file, repeat, center, rate);
}

void Source::set_selector_port_enabled(unsigned int port, bool enabled) {
  recorder_selector->set_port_enabled(port, enabled);
  auto it = shadow_by_selector_port.find(port);
  if (it != shadow_by_selector_port.end()) {
    channelizer->set_port_enabled(it->second->twin_port, enabled);
    it->second->active_since_ns = enabled ? std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() : 0;
  }
}

bool Source::is_selector_port_enabled(unsigned int port) {
  return recorder_selector->is_port_enabled(port);
}

void Source::attach_selector(gr::top_block_sptr tb) {
  if (!attached_selector) {
    attached_selector = true;
    recorder_selector = gr::blocks::selector::make(sizeof(gr_complex), 0, 0);
    tb->connect(source_block, 0, recorder_selector, 0);
  }
}

void Source::attach_channelizer(gr::top_block_sptr tb) {
  if (!channelizer && channelizer_factory) {
    channelizer = shared_channelizer::make(channelizer_factory(rate));
    tb->connect(source_block, 0, channelizer, 0);
    BOOST_LOG_TRIVIAL(info) << "Source " << src_num << ": shared channelizer attached, narrow channel rate " << channelizer->out_rate(CHANNEL_PROFILE_NARROW) << " Hz"
                            << (channelizer->has_profile(CHANNEL_PROFILE_WIDE) ? ", wide channel rate " + std::to_string(channelizer->out_rate(CHANNEL_PROFILE_WIDE)) + " Hz" : ", no wide (analog) profile at this sample rate");
  }
}

void Source::channelizer_set_enabled(unsigned int port, bool enabled) {
  channelizer->set_port_enabled(port, enabled);
}

void Source::channelizer_set_offset(unsigned int port, double offset_hz) {
  channelizer->set_port_offset(port, offset_hz);
}

void Source::channelizer_set_filter(unsigned int port, double cutoff_hz, double transition_hz) {
  channelizer->set_port_filter(port, cutoff_hz, transition_hz);
}

unsigned int Source::add_channelizer_port(unsigned profile) {
  unsigned int port = next_channelizer_port++;
  channelizer->set_port_profile(port, profile);
  return port;
}

bool Source::prepare_voice_input(gr::top_block_sptr tb, unsigned profile) {
  if (channelizer_mode == CHANNELIZER_LEGACY || !channelizer_factory) {
    attach_selector(tb);
    return false;
  }
  attach_channelizer(tb);
  if (channelizer_mode == CHANNELIZER_SHARED) {
    if (!channelizer_supports(profile)) {
      throw std::runtime_error("Source " + std::to_string(src_num) + ": the shared channelizer cannot provide the " +
                               (profile == CHANNEL_PROFILE_WIDE ? "wide (analog)" : "narrow (digital)") + " channel profile at a sample rate of " + std::to_string(rate) +
                               " S/s; raise the source rate or use channelizer = \"legacy\"");
    }
    return true;
  }
  // Shadow: the legacy path stays in charge; attach_shadow() adds the twin.
  attach_selector(tb);
  return false;
}

template <typename Rec>
void Source::wire_voice_recorder(gr::top_block_sptr tb, const Rec &log, bool shared, unsigned profile) {
  if (shared) {
    unsigned int port = add_channelizer_port(profile);
    log->set_selector_port(port);
    tb->connect(channelizer, port, log, 0);
  } else {
    log->set_selector_port(next_selector_port);
    tb->connect(recorder_selector, next_selector_port, log, 0);
    next_selector_port++;
    attach_shadow(tb, log, profile);
  }
}

void Source::attach_shadow(gr::top_block_sptr tb, const std::shared_ptr<Recorder> &rec, unsigned profile) {
  if (channelizer_mode != CHANNELIZER_SHADOW || !channelizer) {
    return;
  }
  auto primary = rec->prefilter_block();
  if (!primary) {
    return;
  }
  if (!channelizer_supports(profile)) {
    BOOST_LOG_TRIVIAL(warning) << "Source " << src_num << ": shadow channelizer cannot provide profile " << profile << " at " << rate << " S/s; recorder " << rec->get_num() << " is not shadowed";
    return;
  }
  auto entry = std::make_shared<ShadowEntry>();
  entry->primary = primary;
  entry->profile = profile;
  entry->twin_port = add_channelizer_port(profile);
  entry->twin = primary->make_shared_twin(channelizer->out_rate(profile));
  unsigned twin_port = entry->twin_port;
  entry->twin->set_tune_hook([this, twin_port](double offset_hz) { channelizer_set_offset(twin_port, offset_hz); });
  entry->twin->set_filter_hook([this, twin_port](double cutoff_hz, double transition_hz) { channelizer_set_filter(twin_port, cutoff_hz, transition_hz); });
  tb->connect(channelizer, twin_port, entry->twin, 0);
  tb->connect(entry->twin, 0, gr::blocks::null_sink::make(sizeof(gr_complex)), 0);
  primary->set_mirror(entry->twin);

  shadow_by_selector_port[rec->selector_port] = entry;
  shadow_entries.push_back(entry);
  if (!shadow_thread.joinable()) {
    shadow_thread = std::thread([this] { shadow_poll_loop(); });
  }
}

void Source::shadow_poll_loop() {
  using namespace std::chrono;
  constexpr auto kPoll = milliseconds(50);
  // Let both paths' power averages (alpha 1e-4, ~0.4 s at 24 kS/s) and FLLs
  // settle after a recorder is enabled before comparing them.
  constexpr long long kSettleNs = 2'000'000'000LL;
  while (!shadow_stop) {
    std::this_thread::sleep_for(kPoll);
    long long now = duration_cast<nanoseconds>(steady_clock::now().time_since_epoch()).count();
    std::lock_guard<std::mutex> lk(shadow_mutex);
    for (auto &e : shadow_entries) {
      long long since = e->active_since_ns;
      if (since == 0 || now - since < kSettleNs) {
        continue;
      }
      ShadowStats &st = shadow_stats[e->profile < 2 ? e->profile : 0];
      st.polls++;
      if (e->primary->uses_squelch()) {
        bool a = e->primary->is_squelched();
        bool b = e->twin->is_squelched();
        st.squelch_polls++;
        if (a != b) {
          st.squelch_disagree++;
        }
        if (a || b) {
          continue; // no signal on at least one path; power and FLL are not comparable
        }
      }
      double pa = e->primary->get_pwr();
      double pb = e->twin->get_pwr();
      if (pa != DB_UNSET && pb != DB_UNSET) {
        double d = pb - pa;
        double ad = std::fabs(d);
        st.power_polls++;
        st.power_abs_sum_db += ad;
        st.power_bias_sum_db += d;
        st.power_abs_max_db = std::max(st.power_abs_max_db, ad);
        if (ad > 1.0) st.power_gt_1db++;
        if (ad > 3.0) st.power_gt_3db++;
        double fe = std::fabs(static_cast<double>(e->twin->get_freq_error() - e->primary->get_freq_error()));
        st.ferr_polls++;
        st.ferr_abs_sum_hz += fe;
        st.ferr_abs_max_hz = std::max(st.ferr_abs_max_hz, fe);
      }
    }
  }
}

std::vector<ShadowSnapshot> Source::collect_shadow_stats() {
  std::vector<ShadowSnapshot> out;
  std::lock_guard<std::mutex> lk(shadow_mutex);
  for (unsigned profile = 0; profile < 2; profile++) {
    unsigned n = 0;
    for (auto &e : shadow_entries) {
      if (e->profile == profile) n++;
    }
    if (n == 0) continue;
    out.push_back({profile, n, shadow_stats[profile]});
  }
  return out;
}

Source::~Source() {
  shadow_stop = true;
  if (shadow_thread.joinable()) {
    shadow_thread.join();
  }
}

void Source::attach_detector(gr::top_block_sptr tb) {
  if (!attached_detector) {
    attached_detector = true;
    tb->connect(source_block, 0, signal_detector, 0);
  }
}

void Source::set_antenna(std::string ant) {
  antenna = ant;

#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    cast_to_osmo_sptr(source_block)->set_antenna(antenna, 0);
    BOOST_LOG_TRIVIAL(info) << "Setting antenna to [" << cast_to_osmo_sptr(source_block)->get_antenna() << "]";
  }
#endif

  if (driver == "usrp") {
    BOOST_LOG_TRIVIAL(info) << "Setting antenna to [" << antenna << "]";
    cast_to_usrp_sptr(source_block)->set_antenna(antenna, 0);
  }
}

std::string Source::get_antenna() {
  return antenna;
}

void Source::set_silence_frames(int m) {
  silence_frames = m;
}

int Source::get_silence_frames() {
  return silence_frames;
}

double Source::get_min_hz() {
  return min_hz;
}

double Source::get_max_hz() {
  return max_hz;
}

double Source::get_center() {
  return center;
}

double Source::get_rate() {
  return rate;
}

bool Source::got_samples() {
  if (!attached_selector && !channelizer) {
    return true;
  }
  // Query both (each call resets its flag); the source is alive if either block ran.
  bool got = false;
  if (attached_selector) {
    got = recorder_selector->got_samples() || got;
  }
  if (channelizer) {
    got = channelizer->got_samples() || got;
  }
  return got;
}

std::string Source::get_driver() {
  return driver;
}

std::string Source::get_device() {
  return device;
}

void Source::set_freq_corr(double p) {
  ppm = p;

#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    cast_to_osmo_sptr(source_block)->set_freq_corr(ppm);
    BOOST_LOG_TRIVIAL(info) << "PPM set to: " << cast_to_osmo_sptr(source_block)->get_freq_corr();
  }
#endif
}

void Source::set_error(double e) {
  error = e;
}

double Source::get_error() {
  return error;
}

/* -- Gain -- */

void Source::set_gain(double r) {
#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    gain = r;
    cast_to_osmo_sptr(source_block)->set_gain(gain);
    double current_gain = cast_to_osmo_sptr(source_block)->get_gain();
    if (current_gain != gain) {
      BOOST_LOG_TRIVIAL(error) << "Requested Gain of " << gain << " not supported, driver using: " << current_gain;
    }
    BOOST_LOG_TRIVIAL(info) << "Gain set to: " << current_gain;
  }
#endif

  if (driver == "usrp") {
    gain = r;
    cast_to_usrp_sptr(source_block)->set_gain(gain);
  }
}

void Source::add_gain_stage(std::string stage_name, double value) {
  Gain_Stage_t stage = {stage_name, value};
  gain_stages.push_back(stage);
}

std::vector<Gain_Stage_t> Source::get_gain_stages() {
  return gain_stages;
}

void Source::set_gain_by_name(std::string name, double new_gain) {
#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    cast_to_osmo_sptr(source_block)->set_gain(new_gain, name);
    double current_gain = cast_to_osmo_sptr(source_block)->get_gain(name);
    if (current_gain != new_gain) {
      BOOST_LOG_TRIVIAL(error) << "Requested " << name << " Gain of " << new_gain << " not supported, driver using: " << current_gain;
    }
    BOOST_LOG_TRIVIAL(info) << name << " Gain set to: " << current_gain;
    add_gain_stage(name, new_gain);
  } else
#endif
  {
    BOOST_LOG_TRIVIAL(error) << "Unable to set Gain by Name for SDR driver: " << driver;
  }
}

int Source::get_gain_by_name(std::string name) {
#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    try {
      return cast_to_osmo_sptr(source_block)->get_gain(name, 0);
    } catch (std::exception &e) {
      BOOST_LOG_TRIVIAL(error) << name << " Gain unsupported or other error: " << e.what();
    }
  } else
#endif
  {
    BOOST_LOG_TRIVIAL(error) << "Unable to get Gain by Name for SDR drive: " << driver;
  }
  return -1;
}

double Source::get_gain() {
  return gain;
}

void Source::set_gain_mode(bool m) {
#ifdef HAVE_OSMOSDR
  if (driver == "osmosdr") {
    gain_mode = m;
    cast_to_osmo_sptr(source_block)->set_gain_mode(gain_mode);
    if (cast_to_osmo_sptr(source_block)->get_gain_mode()) {
      BOOST_LOG_TRIVIAL(info) << "Auto gain control is ON";
    } else {
      BOOST_LOG_TRIVIAL(info) << "Auto gain control is OFF";
    }
  }
#endif
}

double Source::get_if_gain() {
  return if_gain;
}

double Source::get_bb_gain() {
  return bb_gain;
}

double Source::get_mix_gain() {
  return mix_gain;
}

double Source::get_lna_gain() {
  return lna_gain;
}

double Source::get_tia_gain() {
  return tia_gain;
}

double Source::get_pga_gain() {
  return pga_gain;
}

double Source::get_vga1_gain() {
  return vga1_gain;
}

double Source::get_vga2_gain() {
  return vga2_gain;
}

/* -- Recorders -- */

void Source::set_detection_callback(std::function<void(std::vector<Detected_Signal>)> cb) {
  if (signal_detector) {
    signal_detector->set_detection_callback(std::move(cb));
  }
}

void Source::set_power_callback(std::function<void(const float*, unsigned, double)> cb) {
  if (signal_detector) {
    signal_detector->set_power_callback(std::move(cb));
  }
}

void Source::set_signal_detector_threshold(float threshold) {
BOOST_LOG_TRIVIAL(info) << " - Setting Signal Detector Threshold to: " << threshold;
  signal_detector->set_threshold(threshold);
}

void Source::create_analog_recorders(gr::top_block_sptr tb, int r) {
  bool shared = r > 0 && prepare_voice_input(tb, CHANNEL_PROFILE_WIDE);
  max_analog_recorders = r;

  for (int i = 0; i < max_analog_recorders; i++) {
    analog_recorder_sptr log = make_analog_recorder(shared_from_this(), ANALOG);
    analog_recorders.push_back(log);
    wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_WIDE);
  }
}

void Source::create_digital_recorders(gr::top_block_sptr tb, int r) {
  bool shared = r > 0 && prepare_voice_input(tb, CHANNEL_PROFILE_NARROW);
  max_digital_recorders = r;

  for (int i = 0; i < max_digital_recorders; i++) {
    p25_recorder_sptr log = make_p25_recorder(shared_from_this(), P25);
    digital_recorders.push_back(log);
    wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_NARROW);
  }
}

void Source::create_sigmf_recorders(gr::top_block_sptr tb, int r) {
  max_sigmf_recorders = r;
  bool shared = r > 0 && prepare_voice_input(tb, CHANNEL_PROFILE_NARROW);

  for (int i = 0; i < max_sigmf_recorders; i++) {
    sigmf_recorder_sptr log = make_sigmf_recorder(shared_from_this(), SIGMF);

    sigmf_recorders.push_back(log);
    wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_NARROW);
  }
}

analog_recorder_sptr Source::create_conventional_recorder(gr::top_block_sptr tb, float tone_freq) {
  // Not adding it to the vector of analog_recorders. We don't want it to be available for trunk recording.
  // Conventional recorders are tracked seperately in analog_conv_recorders
  attach_detector(tb);
  bool shared = prepare_voice_input(tb, CHANNEL_PROFILE_WIDE);

  analog_recorder_sptr log = make_analog_recorder(shared_from_this(), ANALOGC, tone_freq);
  analog_conv_recorders.push_back(log);
  wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_WIDE);
  return log;
}

analog_recorder_sptr Source::create_conventional_recorder(gr::top_block_sptr tb) {
  // Not adding it to the vector of analog_recorders. We don't want it to be available for trunk recording.
  // Conventional recorders are tracked seperately in analog_conv_recorders
  attach_detector(tb);
  bool shared = prepare_voice_input(tb, CHANNEL_PROFILE_WIDE);

  analog_recorder_sptr log = make_analog_recorder(shared_from_this(), ANALOGC);
  analog_conv_recorders.push_back(log);
  wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_WIDE);
  return log;
}

sigmf_recorder_sptr Source::create_sigmf_conventional_recorder(gr::top_block_sptr tb) {
  // Not adding it to the vector of digital_recorders. We don't want it to be available for trunk recording.
  // Conventional recorders are tracked seperately in digital_conv_recorders
  attach_detector(tb);
  bool shared = prepare_voice_input(tb, CHANNEL_PROFILE_NARROW);
  sigmf_recorder_sptr log = make_sigmf_recorder(shared_from_this(), SIGMFC);
  sigmf_conv_recorders.push_back(log);
  wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_NARROW);
  return log;
}

p25_recorder_sptr Source::create_digital_conventional_recorder(gr::top_block_sptr tb) {
  // Not adding it to the vector of digital_recorders. We don't want it to be available for trunk recording.
  // Conventional recorders are tracked seperately in digital_conv_recorders
  attach_detector(tb);
  bool shared = prepare_voice_input(tb, CHANNEL_PROFILE_NARROW);

  p25_recorder_sptr log = make_p25_recorder(shared_from_this(), P25C);
  digital_conv_recorders.push_back(log);
  wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_NARROW);
  return log;
}

dmr_recorder_sptr Source::create_dmr_conventional_recorder(gr::top_block_sptr tb) {
  // Not adding it to the vector of digital_recorders. We don't want it to be available for trunk recording.
  // Conventional recorders are tracked seperately in digital_conv_recorders
  attach_detector(tb);
  bool shared = prepare_voice_input(tb, CHANNEL_PROFILE_NARROW);

  dmr_recorder_sptr log = make_dmr_recorder(shared_from_this(), DMR);
  dmr_conv_recorders.push_back(log);
  wire_voice_recorder(tb, log, shared, CHANNEL_PROFILE_NARROW);
  return log;
}

void Source::create_debug_recorder(gr::top_block_sptr tb, int source_num) {
  max_debug_recorders = 1;
  debug_recorder_port = config.debug_recorder_port + source_num;
  debug_recorder_sptr log = make_debug_recorder(shared_from_this(), config.debug_recorder_address, debug_recorder_port);
  debug_recorders.push_back(log);
  tb->connect(source_block, 0, log, 0);
}

std::shared_ptr<Recorder> Source::get_analog_recorder() {
  for (auto &rx : analog_recorders) {
    if (rx->get_state() == REC_AVAILABLE) {
      return rx;
    }
  }
  BOOST_LOG_TRIVIAL(error) << "[ " << device << " ] No Analog Recorders Available.";
  return nullptr;
}

std::shared_ptr<Recorder> Source::get_digital_recorder() {
  for (auto &rx : digital_recorders) {
    if (rx->get_state() == REC_AVAILABLE) {
      return rx;
    }
  }
  BOOST_LOG_TRIVIAL(error) << "[ " << device << " ] No Digital Recorders Available.";

  for (auto &rx : digital_recorders) {
    BOOST_LOG_TRIVIAL(info) << "[ " << rx->get_num() << " ] State: " << format_state(rx->get_state());
  }
  return nullptr;
}

std::shared_ptr<Recorder> Source::get_debug_recorder() {
  for (auto &rx : debug_recorders) {
    if (rx->get_state() == REC_INACTIVE) {
      return rx;
    }
  }
  return nullptr;
}

int Source::get_debug_recorder_port() {
  return debug_recorder_port;
}

std::shared_ptr<Recorder> Source::get_sigmf_recorder() {
  for (auto &rx : sigmf_recorders) {
    if (rx->get_state() == REC_INACTIVE) {
      return rx;
    }
  }
  return nullptr;
}

void Source::print_recorders() {
  BOOST_LOG_TRIVIAL(info) << "[ Source " << src_num << ": " << format_freq(center) << " ] " << device;

  for (std::vector<p25_recorder_sptr>::iterator it = digital_recorders.begin();
       it != digital_recorders.end(); it++) {
    p25_recorder_sptr rx = *it;

    BOOST_LOG_TRIVIAL(info) << "\t[ " << std::setw(2) << rx->get_num() << " ] " << rx->get_type_string() << "\tState: " << format_state(rx->get_state());
  }

  for (std::vector<p25_recorder_sptr>::iterator it = digital_conv_recorders.begin();
       it != digital_conv_recorders.end(); it++) {
    p25_recorder_sptr rx = *it;

    BOOST_LOG_TRIVIAL(info) << "\t[ " << std::setw(2) << rx->get_num() << " ] " << rx->get_type_string() << "\tState: " << format_state(rx->get_state());
  }

  for (std::vector<dmr_recorder_sptr>::iterator it = dmr_conv_recorders.begin();
       it != dmr_conv_recorders.end(); it++) {
    dmr_recorder_sptr rx = *it;

    BOOST_LOG_TRIVIAL(info) << "\t[ " << std::setw(2) << rx->get_num() << " ] " << rx->get_type_string() << "\tState: " << format_state(rx->get_state());
  }

  for (std::vector<analog_recorder_sptr>::iterator it = analog_recorders.begin();
       it != analog_recorders.end(); it++) {
    analog_recorder_sptr rx = *it;

    BOOST_LOG_TRIVIAL(info) << "\t[ " << std::setw(2) << rx->get_num() << " ] " << rx->get_type_string() << "\tState: " << format_state(rx->get_state());
  }

  for (std::vector<analog_recorder_sptr>::iterator it = analog_conv_recorders.begin();
       it != analog_conv_recorders.end(); it++) {
    analog_recorder_sptr rx = *it;

    BOOST_LOG_TRIVIAL(info) << "\t[ " << std::setw(2) << rx->get_num() << " ] " << rx->get_type_string() << "\tState: " << format_state(rx->get_state());
  }
}

int Source::get_num_available_digital_recorders() {
  int num_available_recorders = 0;

  for (std::vector<p25_recorder_sptr>::iterator it = digital_recorders.begin();
       it != digital_recorders.end(); it++) {
    p25_recorder_sptr rx = *it;

    if (rx->get_state() == REC_AVAILABLE) {
      num_available_recorders++;
    }
  }
  return num_available_recorders;
}

int Source::get_num_available_analog_recorders() {
  int num_available_recorders = 0;

  for (std::vector<analog_recorder_sptr>::iterator it = analog_recorders.begin(); it != analog_recorders.end(); it++) {
    analog_recorder_sptr rx = *it;

    if (rx->get_state() == REC_AVAILABLE) {
      num_available_recorders++;
    }
  }
  return num_available_recorders;
}

std::vector<std::shared_ptr<Recorder>> Source::get_recorders() {

  std::vector<std::shared_ptr<Recorder>> recorders;

  for (auto &rx : digital_recorders) recorders.push_back(rx);
  for (auto &rx : digital_conv_recorders) recorders.push_back(rx);
  for (auto &rx : dmr_conv_recorders) recorders.push_back(rx);
  for (auto &rx : analog_recorders) recorders.push_back(rx);
  for (auto &rx : analog_conv_recorders) recorders.push_back(rx);
  for (auto &rx : debug_recorders) recorders.push_back(rx);
  for (auto &rx : sigmf_recorders) recorders.push_back(rx);

  return recorders;
}
