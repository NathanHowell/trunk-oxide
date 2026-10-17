#ifndef SOURCE_H
#define SOURCE_H
#include <atomic>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <thread>
#include "./global_structs.h"
#include "./gr_blocks/selector.h"
#include "./gr_blocks/shared_channelizer.h"
#include "./gr_blocks/signal_detector_cvf.h"
#include "recorders/analog_recorder.h"
#include "recorders/debug_recorder.h"
#include "recorders/dmr_recorder.h"
#include "recorders/p25_recorder.h"
#include "recorders/sigmf_recorder.h"
#include "sources/iq_file_source.h"
#include <gnuradio/basic_block.h>
#include <gnuradio/top_block.h>
#include <gnuradio/uhd/usrp_source.h>
#include <iostream>
#include <numeric>
#ifdef HAVE_OSMOSDR
#include <osmosdr/source.h>
#endif

#include <json.hpp>

struct Gain_Stage_t {
  std::string stage_name;
  double value;
};

class Source : public std::enable_shared_from_this<Source> {

  int src_num;
  double min_hz;
  double max_hz;
  double center;
  double rate;
  double actual_rate;
  double error;
  double ppm;
  bool attached_detector;
  bool gain_mode;
  double gain;
  double bb_gain;
  double if_gain;
  double lna_gain;
  double tia_gain;
  double pga_gain;
  double mix_gain;
  double vga1_gain;
  double vga2_gain;
  int max_digital_recorders;
  int max_debug_recorders;
  int max_sigmf_recorders;
  int max_analog_recorders;
  int debug_recorder_port;
  int silence_frames;
  const Config &config;

  std::vector<p25_recorder_sptr> digital_recorders;
  std::vector<p25_recorder_sptr> digital_conv_recorders;
  std::vector<debug_recorder_sptr> debug_recorders;
  std::vector<sigmf_recorder_sptr> sigmf_recorders;
  std::vector<sigmf_recorder_sptr> sigmf_conv_recorders;
  std::vector<analog_recorder_sptr> analog_recorders;
  std::vector<analog_recorder_sptr> analog_conv_recorders;
  std::vector<dmr_recorder_sptr> dmr_conv_recorders;
  std::vector<Gain_Stage_t> gain_stages;
  std::string driver;
  std::string device;
  std::string antenna;
  gr::basic_block_sptr source_block;
  // Optional shared FFT channelizer feeding the voice recorders, digital and analog (see
  // gr_blocks/shared_channelizer.h). Created lazily when a factory is set.
  shared_channelizer::sptr channelizer;
  std::function<std::shared_ptr<channelizer_backend>(double)> channelizer_factory;
  unsigned int next_channelizer_port = 0;
  signal_detector_cvf::sptr signal_detector;

  template <typename Rec> void wire_voice_recorder(gr::top_block_sptr tb, const Rec &log, unsigned profile);

  void add_gain_stage(std::string stage_name, double value);

public:
  int get_num();
  const Config &get_config();
  Source(double c, double r, double e, std::string driver, std::string device, const Config &cfg);
  Source(std::string sigmf_meta, std::string sigmf_data, bool repeat, const Config &cfg);
  Source(std::string iq_file, bool repeat, double center, double rate, const Config &cfg, std::string iq_format = "cf32");
  void init_shared();
  void set_iq_source(std::string iq_file, bool repeat, double center, double rate, std::string iq_format = "cf32");
  gr::basic_block_sptr get_src_block();
  void attach_detector(gr::top_block_sptr tb);
  double get_min_hz();
  double get_max_hz();
  void set_min_max();

  void set_silence_frames(int m);
  int get_silence_frames();

  double get_center();
  double get_rate();
  bool got_samples();
  std::string get_driver();
  std::string get_device();
  void set_antenna(std::string ant);
  std::string get_antenna();
  void set_error(double e);
  double get_error();
  void set_freq_corr(double p);

  /* -- Gain -- */
  void set_if_gain(double i);
  double get_if_gain();
  void set_gain_mode(bool m);
  bool get_gain_mode();
  void set_gain(double r);
  std::vector<Gain_Stage_t> get_gain_stages();
  int get_gain_by_name(std::string name);
  void set_gain_by_name(std::string name, double r);
  double get_gain();
  double get_bb_gain();
  double get_mix_gain();
  double get_lna_gain();
  double get_tia_gain();
  double get_pga_gain();
  double get_vga1_gain();
  double get_vga2_gain();

  /* -- Recorders -- */
  void print_recorders();
  int get_debug_recorder_port();
  int get_num_available_analog_recorders();
  int get_num_available_digital_recorders();
  void set_signal_detector_threshold(float t);
  void set_detection_callback(std::function<void(std::vector<Detected_Signal>)> cb);
  void set_power_callback(std::function<void(const float*, unsigned, double)> cb);
  // Shared channelizer: set before create_*_recorders(). Every voice recorder
  // takes its channel from it.
  void set_channelizer_factory(std::function<std::shared_ptr<channelizer_backend>(double)> f) { channelizer_factory = std::move(f); }
  shared_channelizer::sptr get_channelizer() const { return channelizer; }
  void attach_channelizer(gr::top_block_sptr tb);
  void channelizer_set_enabled(unsigned int port, bool enabled);
  void channelizer_set_offset(unsigned int port, double offset_hz);
  void channelizer_set_filter(unsigned int port, double cutoff_hz, double transition_hz);
  // Attach the shared channelizer a voice recorder of `profile` will take its
  // channel from. Throws if there is no channelizer factory or it cannot
  // provide `profile` at this sample rate. Call before constructing the recorder.
  void prepare_voice_input(gr::top_block_sptr tb, unsigned profile);
  bool channelizer_supports(unsigned profile) const { return channelizer && channelizer->has_profile(profile); }
  unsigned int add_channelizer_port(unsigned profile);
  void create_debug_recorder(gr::top_block_sptr tb, int source_num);
  void create_sigmf_recorders(gr::top_block_sptr tb, int r);
  void create_analog_recorders(gr::top_block_sptr tb, int r);
  void create_digital_recorders(gr::top_block_sptr tb, int r);

  analog_recorder_sptr create_conventional_recorder(gr::top_block_sptr tb);
  analog_recorder_sptr create_conventional_recorder(gr::top_block_sptr tb, float tone_freq);
  sigmf_recorder_sptr create_sigmf_conventional_recorder(gr::top_block_sptr tb);
  p25_recorder_sptr create_digital_conventional_recorder(gr::top_block_sptr tb);
  dmr_recorder_sptr create_dmr_conventional_recorder(gr::top_block_sptr tb);

  std::shared_ptr<Recorder> get_digital_recorder();
  std::shared_ptr<Recorder> get_analog_recorder();
  std::shared_ptr<Recorder> get_debug_recorder();
  std::shared_ptr<Recorder> get_sigmf_recorder();
  std::vector<std::shared_ptr<Recorder>> get_recorders();

#ifdef HAVE_OSMOSDR
  inline osmosdr::source::sptr cast_to_osmo_sptr(gr::basic_block_sptr p) {
    return std::dynamic_pointer_cast<osmosdr::source, gr::basic_block>(p);
  }
#endif
  inline gr::uhd::usrp_source::sptr cast_to_usrp_sptr(gr::basic_block_sptr p) {
    return std::dynamic_pointer_cast<gr::uhd::usrp_source, gr::basic_block>(p);
  }
};
#endif
