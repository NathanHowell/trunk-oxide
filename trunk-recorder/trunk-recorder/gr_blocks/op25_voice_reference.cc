#include "op25_voice_reference.h"

#include <deque>

#include <gnuradio/msg_queue.h>

#include "log_ts.h"
#include "op25_audio.h"
#include "p25p1_fdma.h"
#include "p25p2_tdma.h"

using gr::op25_repeater::p25p1_fdma;

// What p25_frame_assembler_impl holds, minus the GNU Radio block.
struct op25_voice_reference::impl {
  bool phase2;
  op25_audio audio{"127.0.0.1", 0, 0}; // port 0: no UDP, audio to the queue
  log_ts logts;
  std::deque<int16_t> output_queue;
  gr::msg_queue::sptr queue = gr::msg_queue::make(1);
  p25p1_fdma p1;
  p25p2_tdma p2;
  std::vector<int16_t> pcm;
  std::vector<long> sources;
  std::vector<long> terminators;
  std::vector<std::string> json;
  std::vector<uint32_t> inputs;

  impl(bool phase2, int slot, bool soft)
      : phase2(phase2),
        p1(audio, logts, 0, true, true, true, queue, output_queue, true, soft),
        p2(audio, logts, slot, 0, true, queue, output_queue, true, soft) {
    auto cb = [this](gr::message::sptr msg) {
      if (msg->type() == -3) json.push_back(msg->to_string());
    };
    p1.set_msg_callback(cb);
    p2.set_msg_callback(cb);
    p1.voice_tap = [this](const uint32_t *u, uint32_t e0, uint32_t et, bool encrypted) {
      inputs.insert(inputs.end(), u, u + 8);
      inputs.push_back(e0);
      inputs.push_back(et);
      inputs.push_back(encrypted);
    };
    p2.voice_tap = [this](const uint8_t *dibits, bool encrypted) {
      inputs.insert(inputs.end(), dibits, dibits + 36);
      inputs.push_back(encrypted);
    };
  }
};

op25_voice_reference::op25_voice_reference(bool phase2, int slot, const std::string &xor_mask, bool soft_vocoder)
    : d(new impl(phase2, slot, soft_vocoder)) {
  if (phase2) {
    if (xor_mask.size() >= 2160) d->p2.set_xormask(xor_mask.data());
    d->p2.call_start();
  }
}

op25_voice_reference::~op25_voice_reference() = default;

// p25_frame_assembler_impl::general_work, with silence_frames 0.
void op25_voice_reference::push(const uint8_t *dibits, size_t n) {
  bool terminated = false;
  d->p1.rx_sym(dibits, static_cast<int>(n));
  if (d->phase2) {
    for (size_t i = 0; i < n; i++) {
      if (d->p2.rx_sym(dibits[i])) {
        int rc = d->p2.handle_frame();
        if (d->p2.get_call_terminated().first) {
          terminated = true;
          d->p2.reset_call_terminated();
        }
        if (rc > -1) d->p1.reset_timer();
      }
    }
  } else if (d->p1.get_call_terminated().first) {
    terminated = true;
    d->p1.reset_call_terminated();
  }
  if (!d->output_queue.empty()) {
    d->pcm.insert(d->pcm.end(), d->output_queue.begin(), d->output_queue.end());
    d->output_queue.clear();
    const long p2_src = d->p2.get_ptt_src_id();
    const long p1_src = d->p1.get_curr_src_id();
    d->p2.get_ptt_grp_id();
    d->p1.get_curr_grp_id();
    if (p1_src > 0) d->sources.push_back(p1_src);
    if (p2_src > 0) d->sources.push_back(p2_src);
  }
  if (terminated) d->terminators.push_back(static_cast<long>(d->pcm.size()));
}

const std::vector<int16_t> &op25_voice_reference::pcm() const { return d->pcm; }
const std::vector<long> &op25_voice_reference::sources() const { return d->sources; }
const std::vector<long> &op25_voice_reference::terminators() const { return d->terminators; }
const std::vector<std::string> &op25_voice_reference::json() const { return d->json; }
const std::vector<uint32_t> &op25_voice_reference::voice_inputs() const { return d->inputs; }
