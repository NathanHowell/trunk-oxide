#ifndef P25_VOICE_DECODER_H
#define P25_VOICE_DECODER_H

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include <gnuradio/block.h>
#include <gnuradio/gr_complex.h>

// A P25 voice receiver implemented outside this tree (trunk-p25 in
// trunk-oxide), phase 1 or phase 2 TDMA, all the way to audio: it
// demodulates a conditioned channel, decodes and assembles the call, and
// vocodes each frame.
class p25_voice_backend {
public:
  virtual ~p25_voice_backend() = default;

  // Something said in band, at a sample of the audio a process() call
  // appended.
  struct Tag {
    enum Kind { Source = 0, Group = 1, Terminate = 2, Json = 3 };
    uint32_t offset;
    Kind kind;
    long value;
    std::string json; // Json: an op25-style message (talker alias)
  };

  // Decode `n` samples; append audio (8 kHz) and its tags.
  virtual void process(const gr_complex *in, size_t n, std::vector<int16_t> &audio, std::vector<Tag> &tags) = 0;
  // A new call: loops and NAC start over (qpsk = CQPSK, else C4FM; tdma =
  // phase 2 at 6000 baud on logical channel `slot`, descrambled with the
  // 2160-dibit `xor_mask`).
  virtual void start_call(bool qpsk, bool tdma, const std::string &xor_mask, const std::string &system, int slot) = 0;
  // Encryption confirmed on the call's own slot since the last take.
  virtual bool take_crypto(unsigned &algid, unsigned &keyid) = 0;

  // Bits corrected in voice frames and op25-style error spikes since the
  // last call.
  struct CallCounts {
    long errors = 0;
    long spikes = 0;
    int algid = 0x80; // confirmed on the call's slot; 0x80 = clear
  };
  virtual CallCounts take_call_counts() = 0;
};

// Makes a backend for a recorder; `rate` is the conditioned channel rate.
using p25_voice_factory = std::function<std::shared_ptr<p25_voice_backend>(double rate, int recorder)>;

// complex in, audio (int16, 8 kHz) out, tagged as op25's frame assembler
// tags it: "src_id" and "grp_id" where a unit or group is heard,
// "terminate" where a transmission ends.
class p25_voice_decoder : public gr::block {
public:
  typedef std::shared_ptr<p25_voice_decoder> sptr;
  static sptr make(std::shared_ptr<p25_voice_backend> backend, double rate);

  p25_voice_decoder(std::shared_ptr<p25_voice_backend> backend, double rate);

  void start_call(bool qpsk, bool tdma, const std::string &xor_mask, const std::string &system, int slot);
  p25_voice_backend::CallCounts take_call_counts();
  // Called on the GR thread with encryption the backend confirms.
  void set_crypto_callback(std::function<void(unsigned algid, unsigned keyid)> cb) { d_crypto_cb = std::move(cb); }
  // Called on the GR thread with each JSON message (talker aliases).
  void set_json_callback(std::function<void(const std::string &)> cb) { d_json_cb = std::move(cb); }
  // Disabled, input is consumed and dropped without decoding.
  void set_enabled(bool enabled);

  void forecast(int noutput_items, gr_vector_int &ninput_items_required) override;
  int general_work(int noutput_items, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items,
                   gr_vector_void_star &output_items) override;

private:
  struct PendingTag {
    uint64_t at; // absolute output sample
    p25_voice_backend::Tag::Kind kind;
    long value;
  };

  std::shared_ptr<p25_voice_backend> d_backend;
  // Input samples per output sample.
  double d_ratio;
  std::function<void(unsigned, unsigned)> d_crypto_cb;
  std::function<void(const std::string &)> d_json_cb;
  std::atomic<bool> d_enabled{false};
  // Set by start_call; the GR thread drops the last call's pending audio.
  std::atomic<bool> d_new_call{false};
  std::mutex d_mutex; // the backend is driven by the GR thread and by call start/stop
  std::vector<int16_t> d_audio;
  std::vector<p25_voice_backend::Tag> d_tags;
  std::deque<int16_t> d_pending;
  std::deque<PendingTag> d_pending_tags;
};

#endif
