#include "p25_voice_decoder.h"

#include <algorithm>
#include <cmath>
#include <gnuradio/io_signature.h>
#include <pmt/pmt.h>

p25_voice_decoder::sptr p25_voice_decoder::make(std::shared_ptr<p25_voice_backend> backend, double rate) {
  return gnuradio::get_initial_sptr(new p25_voice_decoder(std::move(backend), rate));
}

p25_voice_decoder::p25_voice_decoder(std::shared_ptr<p25_voice_backend> backend, double rate)
    : gr::block("p25_voice_decoder",
                gr::io_signature::make(1, 1, sizeof(gr_complex)),
                gr::io_signature::make(1, 1, sizeof(int16_t))),
      d_backend(std::move(backend)),
      d_ratio(rate / 8000.0) {
  set_relative_rate(1.0 / d_ratio);
}

void p25_voice_decoder::start_call(bool qpsk, bool tdma, const std::string &xor_mask, const std::string &system, bool primary, int slot) {
  std::lock_guard<std::mutex> l(d_mutex);
  d_backend->start_call(qpsk, tdma, xor_mask, system, primary, slot);
  d_new_call = true;
}

void p25_voice_decoder::set_enabled(bool enabled) {
  d_enabled = enabled;
}

p25_voice_backend::CallCounts p25_voice_decoder::take_call_counts() {
  std::lock_guard<std::mutex> l(d_mutex);
  return d_backend->take_call_counts();
}

void p25_voice_decoder::forecast(int noutput_items, gr_vector_int &ninput_items_required) {
  // Audio already decoded needs no input; the rest about d_ratio samples
  // each (frames come out 160 samples at a time, so this is only a hint).
  const int missing = std::max(0, noutput_items - static_cast<int>(d_pending.size()));
  ninput_items_required[0] = static_cast<int>(std::ceil(missing * d_ratio));
}

int p25_voice_decoder::general_work(int noutput_items, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items,
                                    gr_vector_void_star &output_items) {
  static const pmt::pmt_t src_id = pmt::intern("src_id");
  static const pmt::pmt_t grp_id = pmt::intern("grp_id");
  static const pmt::pmt_t terminate = pmt::intern("terminate");
  const pmt::pmt_t me = pmt::intern(alias());

  const auto *in = static_cast<const gr_complex *>(input_items[0]);
  auto *out = static_cast<int16_t *>(output_items[0]);
  const int n_in = ninput_items[0];
  // Audio decoded for the last call but not yet sent is its: sent now, it
  // would open this call.
  if (d_new_call.exchange(false)) {
    d_pending.clear();
    d_pending_tags.clear();
  }
  if (!d_enabled) {
    consume_each(n_in);
    d_pending.clear();
    d_pending_tags.clear();
    return 0;
  }
  if (n_in > 0) {
    unsigned algid = 0, keyid = 0;
    bool crypto = false;
    {
      std::lock_guard<std::mutex> l(d_mutex);
      d_audio.clear();
      d_tags.clear();
      d_backend->process(in, static_cast<size_t>(n_in), d_audio, d_tags);
      crypto = d_backend->take_crypto(algid, keyid);
    }
    const uint64_t base = nitems_written(0) + d_pending.size();
    for (const auto &t : d_tags) {
      if (t.kind == p25_voice_backend::Tag::Json) {
        if (d_json_cb) d_json_cb(t.json);
      } else {
        d_pending_tags.push_back({base + t.offset, t.kind, t.value});
      }
    }
    d_pending.insert(d_pending.end(), d_audio.begin(), d_audio.end());
    // A terminator after the last audio rides on one silent sample, as op25
    // sends it (else it would wait for the next call's audio).
    if (!d_pending_tags.empty() && d_pending_tags.back().kind == p25_voice_backend::Tag::Terminate &&
        d_pending_tags.back().at >= nitems_written(0) + d_pending.size()) {
      d_pending_tags.back().at = nitems_written(0) + d_pending.size();
      d_pending.push_back(0);
    }
    if (crypto && d_crypto_cb) d_crypto_cb(algid, keyid);
  }
  consume_each(n_in);
  const int n_out = std::min(noutput_items, static_cast<int>(d_pending.size()));
  std::copy_n(d_pending.begin(), n_out, out);
  d_pending.erase(d_pending.begin(), d_pending.begin() + n_out);
  // Tags on the samples sent.
  const uint64_t end = nitems_written(0) + n_out;
  while (!d_pending_tags.empty() && d_pending_tags.front().at < end) {
    const auto &t = d_pending_tags.front();
    const pmt::pmt_t key = t.kind == p25_voice_backend::Tag::Source ? src_id : t.kind == p25_voice_backend::Tag::Group ? grp_id : terminate;
    add_item_tag(0, t.at, key, pmt::from_long(t.value), me);
    d_pending_tags.pop_front();
  }
  return n_out;
}
