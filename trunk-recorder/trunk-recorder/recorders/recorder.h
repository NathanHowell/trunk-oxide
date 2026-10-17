#ifndef RECORDER_H
#define RECORDER_H

#include <chrono>
#include <functional>
#include <memory>
#include <string>
#include <vector>

#include <gnuradio/runtime_types.h>

#include "../global_structs.h"
#include "../recorder_config.h"
#include "../state.h"

class Source;
class System;
class xlat_channelizer;

class Recorder {

public:
  struct DecimSettings {
    long decim;
    long decim2;
  };

  int rec_num;
  int rssi=0;
  static int rec_counter;
  std::string get_type_string();
  bool conventional;
  unsigned int channelizer_port;

  virtual void set_channelizer_port(unsigned int port) {channelizer_port = port;}
  Recorder(Recorder_Type  type);
  int get_num() const { return rec_num; };

  virtual bool start(const RecorderConfig &config) = 0;
  virtual void stop() = 0;
  virtual int get_freq_error() const = 0;
  virtual double get_pwr() const = 0;
  virtual std::vector<Transmission> get_transmission_list() = 0;
  virtual void set_source(long src) = 0;
  virtual RecorderState get_state() const = 0;
  virtual void set_enabled(bool enabled) = 0;
  virtual std::chrono::duration<double> since_last_write() const = 0;
  /// Time since the recorder last wrote non-silent audio (since_last_write()
  /// also advances on the decoder's silence padding). Defaults to
  /// since_last_write() for recorders that cannot tell.
  virtual std::chrono::duration<double> since_last_voice() const { return since_last_write(); }
  virtual void set_system(const std::shared_ptr<System> &) = 0;
  virtual void set_squelch_callback(std::function<void(bool, double)> cb) = 0;
  virtual void flush_audio() = 0;
  virtual void set_squelch_db(double /*db*/) {};

  /// Return key internal gr::block instances for performance-counter collection.
  virtual std::vector<gr::block_sptr> get_metric_blocks() const = 0;

  uint64_t get_rust_call_id() const { return rust_call_id; }

  // In-band P25 source (unit) change, decoded by op25 on the voice channel and
  // reported by the decode block. Wired (like the squelch callback) to the
  // Rust EventNotifier so the state machine can cut per-speaker transmission
  // boundaries. Concrete on the base — every recorder type inherits it; only
  // the P25 decode path actually calls notify_source_change().
  void set_source_change_callback(std::function<void(uint64_t, long)> cb) {
    source_change_callback = std::move(cb);
  }
  void notify_source_change(long src_id) {
    if (source_change_callback) {
      source_change_callback(rust_call_id, src_id);
    }
  }

  // In-band P25 encryption parameters (ALGID / KeyID) decoded from the ESS on
  // the voice channel, reported by the decode block when a call is encrypted
  // (and we hold no key). Wired like the source-change callback.
  void set_crypto_callback(std::function<void(uint64_t, unsigned, unsigned)> cb) {
    crypto_callback = std::move(cb);
  }
  void notify_crypto(unsigned algid, unsigned keyid) {
    if (crypto_callback) {
      crypto_callback(rust_call_id, algid, keyid);
    }
  }

protected:
  Recorder_Type  type;
  int autotune_offset = 0;
  uint64_t rust_call_id = 0;
  std::function<void(uint64_t, long)> source_change_callback;
  std::function<void(uint64_t, unsigned, unsigned)> crypto_callback;
};

typedef std::shared_ptr<Recorder> Recorder_sptr;

#endif
