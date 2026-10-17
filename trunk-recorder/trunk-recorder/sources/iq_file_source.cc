#include "iq_file_source.h"

#include <stdexcept>

iq_file_source::sptr
iq_file_source::make(std::string filename, double rate, bool repeat, std::string format) {
  return gnuradio::get_initial_sptr(new iq_file_source(filename, rate, repeat, format));
}

iq_file_source::iq_file_source(std::string filename, double rate, bool repeat, std::string format)
    : gr::hier_block2("iq_file_source",
                      gr::io_signature::make(0, 0, 0),
                      gr::io_signature::make(1, 1, sizeof(gr_complex))),
      d_filename(filename),
      d_rate(rate),
      d_repeat(repeat) {

  throttle = gr::blocks::throttle::make(sizeof(gr_complex), rate);

  if (format == "cf32" || format.empty()) {
    file_source = gr::blocks::file_source::make(sizeof(gr_complex), filename.c_str(), repeat);
    connect(file_source, 0, throttle, 0);
  } else if (format == "cu8") {
    // bytes -> floats -> split I/Q -> complex -> (u - 127.4) / 128
    file_source = gr::blocks::file_source::make(sizeof(unsigned char), filename.c_str(), repeat);
    auto to_float = gr::blocks::uchar_to_float::make();
    auto split = gr::blocks::deinterleave::make(sizeof(float), 1);
    auto to_complex = gr::blocks::float_to_complex::make();
    auto offset = gr::blocks::add_const_cc::make(gr_complex(-127.4f, -127.4f));
    auto scale = gr::blocks::multiply_const_cc::make(gr_complex(1.0f / 128.0f, 0.0f));
    connect(file_source, 0, to_float, 0);
    connect(to_float, 0, split, 0);
    connect(split, 0, to_complex, 0);
    connect(split, 1, to_complex, 1);
    connect(to_complex, 0, offset, 0);
    connect(offset, 0, scale, 0);
    connect(scale, 0, throttle, 0);
  } else {
    throw std::runtime_error("iq_file_source: unsupported format '" + format + "' (expected cf32 or cu8)");
  }
  connect(throttle, 0, self(), 0);
}
