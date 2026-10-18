#ifndef SYMBOL_REGENERATOR_H
#define SYMBOL_REGENERATOR_H

#include <cstddef>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include <gnuradio/block.h>
#include <gnuradio/gr_complex.h>

// A P25 phase 1 voice receiver implemented outside this tree (trunk-p25 in
// trunk-oxide). It demodulates a conditioned channel, decodes every frame,
// and re-sends the symbol stream with each decodable frame re-encoded
// without errors (as a repeater regenerates it), at ideal levels (+/-1,
// +/-3). An op25 frame assembler downstream then does the rest unchanged.
class symbol_regenerator_backend {
public:
  virtual ~symbol_regenerator_backend() = default;

  // Demodulate `n` samples; append regenerated symbols to `out`.
  virtual void process(const gr_complex *in, size_t n, std::vector<float> &out) = 0;
  // A new call starts: loops and NAC start over (qpsk = CQPSK, else C4FM).
  virtual void start_call(bool qpsk, const std::string &system) = 0;

  // Bits corrected in IMBE frames and op25-style error spikes since the last
  // call, for the call's transmissions (the frame assembler sees none).
  struct CallCounts {
    long errors = 0;
    long spikes = 0;
  };
  virtual CallCounts take_call_counts() = 0;
};

// Makes a backend for a recorder; `rate` is the conditioned channel rate.
using symbol_regenerator_factory = std::function<std::shared_ptr<symbol_regenerator_backend>(double rate, int recorder)>;

// complex in, float symbols out.
class p25_regenerator : public gr::block {
public:
  typedef std::shared_ptr<p25_regenerator> sptr;
  static sptr make(std::shared_ptr<symbol_regenerator_backend> backend);

  explicit p25_regenerator(std::shared_ptr<symbol_regenerator_backend> backend);

  void start_call(bool qpsk, const std::string &system);
  symbol_regenerator_backend::CallCounts take_call_counts();

  void forecast(int noutput_items, gr_vector_int &ninput_items_required) override;
  int general_work(int noutput_items, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items,
                   gr_vector_void_star &output_items) override;

private:
  std::shared_ptr<symbol_regenerator_backend> d_backend;
  std::mutex d_mutex; // the backend is driven by the GR thread and by call start/stop
  std::vector<float> d_scratch;
  std::deque<float> d_pending;
};

#endif
