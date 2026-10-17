#ifndef IQ_FILE_SOURCE_H
#define IQ_FILE_SOURCE_H

#include <gnuradio/blocks/add_const_cc.h>
#include <gnuradio/blocks/deinterleave.h>
#include <gnuradio/blocks/file_source.h>
#include <gnuradio/blocks/float_to_complex.h>
#include <gnuradio/blocks/multiply_const.h>
#include <gnuradio/blocks/throttle.h>
#include <gnuradio/blocks/uchar_to_float.h>
#include <gnuradio/hier_block2.h>

// Replays a raw IQ file as a source, paced at `rate`. `format` is "cf32"
// (interleaved float32, the default) or "cu8" (interleaved unsigned 8-bit as
// written by rtl_sdr, scaled like the osmosdr rtl source: (u - 127.4) / 128).
class iq_file_source : public gr::hier_block2 {
private:
  std::string d_filename;
  double d_rate;
  bool d_repeat;
  gr::blocks::file_source::sptr file_source;
  gr::blocks::throttle::sptr throttle;

public:
  typedef std::shared_ptr<iq_file_source> sptr;
  static sptr make(std::string filename, double rate, bool repeat, std::string format = "cf32");

  iq_file_source(std::string filename, double rate, bool repeat, std::string format);
};

#endif
