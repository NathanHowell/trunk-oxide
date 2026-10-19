#ifndef IQ_STREAM_SOURCE_H
#define IQ_STREAM_SOURCE_H

#include <cstddef>
#include <gnuradio/sync_block.h>
#include <memory>

// Complex baseband from somewhere outside GNU Radio: the host (trunk-oxide)
// owns the device and hands its samples over through this interface.
class iq_stream {
public:
  virtual ~iq_stream() = default;
  // Copy up to `n` samples into `out`, waiting a little for the first; returns
  // how many were written. 0 with ended() false means nothing arrived in time.
  virtual size_t read(gr_complex *out, size_t n) = 0;
  // No more samples will ever come.
  virtual bool ended() = 0;
};

// A source block that reads an iq_stream. Finishes (WORK_DONE) when the
// stream ends, so the flowgraph's got_samples() goes false as it would for a
// dongle that stopped.
class iq_stream_source : public gr::sync_block {
public:
  typedef std::shared_ptr<iq_stream_source> sptr;
  static sptr make(std::shared_ptr<iq_stream> stream);

  explicit iq_stream_source(std::shared_ptr<iq_stream> stream);
  int work(int noutput_items, gr_vector_const_void_star &input_items, gr_vector_void_star &output_items) override;

private:
  std::shared_ptr<iq_stream> d_stream;
};

#endif
