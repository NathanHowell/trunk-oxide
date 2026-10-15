#ifndef SHARED_CHANNELIZER_H
#define SHARED_CHANNELIZER_H

#include <gnuradio/block.h>
#include <memory>
#include <mutex>
#include <vector>

#include "channelizer_backend.h"

// One input (source IQ), N outputs (one per recorder). Each enabled output
// carries that recorder's channel at backend->out_rate(); disabled outputs
// produce nothing (like gr::blocks::selector, so a stalled recorder only
// blocks the flowgraph while it is enabled).
class shared_channelizer : public gr::block {
public:
  typedef std::shared_ptr<shared_channelizer> sptr;
  static sptr make(std::shared_ptr<channelizer_backend> backend);

  shared_channelizer(std::shared_ptr<channelizer_backend> backend);

  double out_rate() const { return d_backend->out_rate(); }

  // Offset of the port's channel from the source centre. Takes effect
  // immediately if the port is enabled, else when it is enabled.
  void set_port_offset(unsigned port, double offset_hz);
  void set_port_enabled(unsigned port, bool enabled);
  bool is_port_enabled(unsigned port);
  // True if samples flowed since the last call.
  bool got_samples();

  void forecast(int noutput_items, gr_vector_int &ninput_items_required) override;
  int general_work(int noutput_items, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items, gr_vector_void_star &output_items) override;

private:
  struct Port {
    double offset_hz = 0.0;
    bool enabled = false;
  };
  Port &port_locked(unsigned port);

  std::shared_ptr<channelizer_backend> d_backend;
  size_t d_hop;
  size_t d_opb;
  std::mutex d_mutex;
  std::vector<Port> d_ports;
  std::vector<gr_complex> d_scratch;
  std::vector<uint8_t> d_active;
  bool d_got_samples = false;
};

#endif
