#include "symbol_regenerator.h"

#include <algorithm>
#include <gnuradio/io_signature.h>

p25_regenerator::sptr p25_regenerator::make(std::shared_ptr<symbol_regenerator_backend> backend) {
  return gnuradio::get_initial_sptr(new p25_regenerator(std::move(backend)));
}

p25_regenerator::p25_regenerator(std::shared_ptr<symbol_regenerator_backend> backend)
    : gr::block("p25_regenerator",
                gr::io_signature::make(1, 1, sizeof(gr_complex)),
                gr::io_signature::make(1, 1, sizeof(float))),
      d_backend(std::move(backend)) {
  set_relative_rate(1.0 / d_samples_per_symbol);
}

void p25_regenerator::start_call(bool qpsk, bool tdma, const std::string &xor_mask, const std::string &system, bool primary, int slot) {
  std::lock_guard<std::mutex> l(d_mutex);
  d_samples_per_symbol = tdma ? 4 : 5;
  d_backend->start_call(qpsk, tdma, xor_mask, system, primary, slot);
  d_new_call = true;
}

void p25_regenerator::set_enabled(bool enabled) {
  d_enabled = enabled;
}

symbol_regenerator_backend::CallCounts p25_regenerator::take_call_counts() {
  std::lock_guard<std::mutex> l(d_mutex);
  return d_backend->take_call_counts();
}

void p25_regenerator::forecast(int noutput_items, gr_vector_int &ninput_items_required) {
  // Symbols already regenerated need no input; the rest a symbol period of
  // samples each (frames are released in bursts, so this is only a hint).
  const int missing = std::max(0, noutput_items - static_cast<int>(d_pending.size()));
  ninput_items_required[0] = missing * d_samples_per_symbol;
}

int p25_regenerator::general_work(int noutput_items, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items,
                                  gr_vector_void_star &output_items) {
  const auto *in = static_cast<const gr_complex *>(input_items[0]);
  auto *out = static_cast<float *>(output_items[0]);
  const int n_in = ninput_items[0];
  // Symbols regenerated for the last call but not yet sent came from its
  // channel: sent now, they would open this call with them (and, on a TDMA
  // recorder moved to the other slot, with that slot's encryption sync).
  if (d_new_call.exchange(false)) {
    d_pending.clear();
  }
  if (!d_enabled) {
    consume_each(n_in);
    d_pending.clear();
    return 0;
  }
  if (n_in > 0) {
    unsigned algid = 0, keyid = 0;
    bool crypto = false;
    {
      std::lock_guard<std::mutex> l(d_mutex);
      d_scratch.clear();
      d_backend->process(in, static_cast<size_t>(n_in), d_scratch);
      d_pending.insert(d_pending.end(), d_scratch.begin(), d_scratch.end());
      crypto = d_backend->take_crypto(algid, keyid);
    }
    if (crypto && d_crypto_cb) d_crypto_cb(algid, keyid);
  }
  consume_each(n_in);
  const int n_out = std::min(noutput_items, static_cast<int>(d_pending.size()));
  std::copy_n(d_pending.begin(), n_out, out);
  d_pending.erase(d_pending.begin(), d_pending.begin() + n_out);
  return n_out;
}
