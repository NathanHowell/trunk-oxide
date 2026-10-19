#include "iq_stream_source.h"
#include <gnuradio/io_signature.h>

iq_stream_source::sptr iq_stream_source::make(std::shared_ptr<iq_stream> stream) {
  return gnuradio::get_initial_sptr(new iq_stream_source(std::move(stream)));
}

iq_stream_source::iq_stream_source(std::shared_ptr<iq_stream> stream)
    : gr::sync_block("iq_stream_source",
                     gr::io_signature::make(0, 0, 0),
                     gr::io_signature::make(1, 1, sizeof(gr_complex))),
      d_stream(std::move(stream)) {}

int iq_stream_source::work(int noutput_items, gr_vector_const_void_star & /*input_items*/, gr_vector_void_star &output_items) {
  gr_complex *out = static_cast<gr_complex *>(output_items[0]);
  size_t n = d_stream->read(out, static_cast<size_t>(noutput_items));
  if (n == 0) {
    return d_stream->ended() ? WORK_DONE : 0;
  }
  return static_cast<int>(n);
}
