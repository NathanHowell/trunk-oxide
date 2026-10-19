#ifndef P25_RECORDER_IMPL_H
#define P25_RECORDER_IMPL_H

#include <atomic>
#include <chrono>
#include <memory>
#include <string>
#include <vector>

#include <gnuradio/hier_block2.h>
#include <gnuradio/io_signature.h>

#include "../gr_blocks/channel_control.h"
#include "../gr_blocks/p25_voice_decoder.h"
#include "p25_recorder.h"
#include "p25_recorder_decode.h"
#include "recorder.h"

class Source;
class p25_recorder;

#include "../source.h"

// A P25 voice recorder: trunk-p25 decodes the channel to audio
// (p25_voice_decoder, Config::p25_voice_decoder), which p25_recorder_decode
// records and reports.
class p25_recorder_impl : public p25_recorder {
  friend p25_recorder_sptr make_p25_recorder(const std::shared_ptr<Source> &src, Recorder_Type type);

protected:
  void initialize(const std::shared_ptr<Source> &src);
  void build_graph();

public:
  p25_recorder_impl(const std::shared_ptr<Source> &src, Recorder_Type type);

  bool start(const RecorderConfig &config) override;
  void set_channelizer_port(unsigned int port) override;
  void stop() override;
  void flush_audio() override;
  int get_num() const;
  int get_freq_error() const override;
  double get_pwr() const override;
  std::vector<Transmission> get_transmission_list() override;
  void set_tdma(bool phase2) override;
  void switch_tdma(bool phase2) override;
  void set_source(long src) override;
  void set_system(const std::shared_ptr<System> &sys) override;
  std::chrono::duration<double> since_last_write() const override;
  std::chrono::duration<double> since_last_voice() const override;
  void set_enabled(bool enabled) override;
  RecorderState get_state() const override;
  void set_squelch_callback(std::function<void(bool, double)> cb) override;
  std::shared_ptr<Source> get_source() override;
  std::vector<gr::block_sptr> get_metric_blocks() const override;

protected:
  RecorderState state;
  long talkgroup;
  std::string short_name;
  const Config &config;
  std::shared_ptr<Source> source;
  double chan_freq;
  double center_freq;
  bool qpsk_mod;
  double squelch_db;
  channel_control::sptr prefilter;
  p25_voice_decoder::sptr voice;
  p25_recorder_decode_sptr decode;
  // Its ESS (confirmed on the call's own slot) marks the call encrypted.
  std::atomic<bool> forward_crypto{false};
  bool call_phase2 = false;
  // The decoder's counts for the call just stopped (taken once, at stop()).
  p25_voice_backend::CallCounts counts;
  void report_call();
  // Offline taps (Config::tap_dir): the conditioned channel of each call.
  void tap_begin(const RecorderConfig &config);
  void tap_end();

private:
  bool d_phase2_tdma;
  const int phase1_samples_per_symbol = 5;
  const int phase2_samples_per_symbol = 4;
};

#endif // ifndef P25_RECORDER_IMPL_H
