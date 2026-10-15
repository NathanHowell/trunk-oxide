#ifndef CHANNELIZER_BACKEND_H
#define CHANNELIZER_BACKEND_H

#include <cstddef>
#include <cstdint>
#include <gnuradio/gr_complex.h>

// DSP engine behind shared_channelizer. The trunk-recorder tree only sees this
// interface; trunk-oxide implements it in Rust (trunk-channelizer crate).
//
// Threading: set_channel/clear_channel may be called from any thread;
// process() is called only from the GNU Radio thread running the block and
// applies pending set/clear calls between blocks.
class channelizer_backend {
public:
  virtual ~channelizer_backend() = default;

  // Input samples consumed per process() call (source rate).
  virtual size_t hop() const = 0;
  // Output samples produced per active channel per process() call.
  virtual size_t out_per_block() const = 0;
  // Output sample rate of every channel (Hz).
  virtual double out_rate() const = 0;

  // Enable `port` at `offset_hz` from the source centre, or retune it.
  virtual void set_channel(unsigned port, double offset_hz) = 0;
  // Disable `port`: it produces no output from the next block on.
  virtual void clear_channel(unsigned port) = 0;

  // Consume exactly hop() samples. For each active port p < nports, write
  // out_per_block() samples to out[p * out_per_block()] and set active[p]=1;
  // all other active[] entries are set to 0.
  virtual void process(const gr_complex *in, size_t nports, gr_complex *out, uint8_t *active) = 0;
};

#endif
