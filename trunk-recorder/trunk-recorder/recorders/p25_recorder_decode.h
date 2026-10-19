#ifndef P25_RECORDER_DECODE_H
#define P25_RECORDER_DECODE_H

#include <json.hpp>
#include <gnuradio/block.h>
#include <gnuradio/block_detail.h>
#include <gnuradio/hier_block2.h>
#include <gnuradio/io_signature.h>
#include <atomic>
#include <mutex>

#include <gnuradio/blocks/multiply_const.h>

#include "../gr_blocks/plugin_wrapper.h"
#include "../gr_blocks/headless_sink.h"
#include "recorder.h"

class p25_recorder_decode;

typedef std::shared_ptr<p25_recorder_decode> p25_recorder_decode_sptr;

// Records a P25 call's audio (int16, 8 kHz, tagged with "src_id" where the
// unit talking is heard) and reports its in-band sources and aliases.
p25_recorder_decode_sptr make_p25_recorder_decode(const std::shared_ptr<Recorder> &recorder, const Config &config);

class p25_recorder_decode : public gr::hier_block2 {
  friend p25_recorder_decode_sptr make_p25_recorder_decode(const std::shared_ptr<Recorder> &recorder, const Config &config);

protected:
  void initialize();
  std::shared_ptr<Recorder> d_recorder;
  const Config &d_config;
  std::shared_ptr<System> d_system;
  long d_source_id;
  // Last in-band src reported up to the state machine, to dedup the per-superframe
  // tag re-fires (only a *change* cuts a transmission boundary).
  long d_last_notified_src = -1;
  gr::blocks::multiply_const_ss::sptr levels;
  gr::blocks::headless_sink::sptr wav_sink;
  gr::blocks::plugin_wrapper::sptr plugin_sink;

public:
  p25_recorder_decode(const std::shared_ptr<Recorder> &recorder, const Config &config);
  std::vector<Transmission> get_transmission_list();
  void set_source(long src);
  void set_system(const std::shared_ptr<System> &sys);
  void start(const RecorderConfig &config);
  std::chrono::duration<double> since_last_write();
  std::chrono::duration<double> since_last_voice();
  void stop();
  void flush_plugin();
  // Source units the call in progress (or last call) named in band, in
  // order of first appearance.
  std::vector<long> call_sources() const;
  virtual ~p25_recorder_decode();
  void plugin_callback_handler(int16_t *samples, int sampleCount);
  // An op25-style JSON message: a talker alias.
  void handle_json(const std::string &json);
  void source_id_handler(long src_id);
  double get_output_sample_rate();
  RecorderState get_state();

private:
  void handle_alias_message(const nlohmann::json& j);
  mutable std::mutex d_state_mutex;
  std::vector<long> d_call_sources; // guarded by d_state_mutex
};
#endif