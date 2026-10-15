#include "shared_channelizer.h"
#include <cstring>
#include <gnuradio/io_signature.h>

shared_channelizer::sptr shared_channelizer::make(std::shared_ptr<channelizer_backend> backend) {
  return gnuradio::get_initial_sptr(new shared_channelizer(std::move(backend)));
}

shared_channelizer::shared_channelizer(std::shared_ptr<channelizer_backend> backend)
    : gr::block("shared_channelizer",
                gr::io_signature::make(1, 1, sizeof(gr_complex)),
                gr::io_signature::make(1, -1, sizeof(gr_complex))),
      d_backend(std::move(backend)),
      d_hop(d_backend->hop()),
      d_opb(d_backend->out_per_block()) {
  // Every call consumes one hop and emits one block per enabled port.
  set_output_multiple(d_opb);
  // Lets the GR buffer allocator size the source->channelizer buffer for a hop.
  set_relative_rate(static_cast<double>(d_opb) / static_cast<double>(d_hop));
}

shared_channelizer::Port &shared_channelizer::port_locked(unsigned port) {
  if (port >= d_ports.size()) {
    d_ports.resize(port + 1);
  }
  return d_ports[port];
}

void shared_channelizer::set_port_offset(unsigned port, double offset_hz) {
  std::lock_guard<std::mutex> l(d_mutex);
  Port &p = port_locked(port);
  p.offset_hz = offset_hz;
  if (p.enabled) {
    d_backend->set_channel(port, offset_hz);
  }
}

void shared_channelizer::set_port_enabled(unsigned port, bool enabled) {
  std::lock_guard<std::mutex> l(d_mutex);
  Port &p = port_locked(port);
  if (p.enabled == enabled) {
    return;
  }
  p.enabled = enabled;
  if (enabled) {
    d_backend->set_channel(port, p.offset_hz);
  } else {
    d_backend->clear_channel(port);
  }
}

bool shared_channelizer::is_port_enabled(unsigned port) {
  std::lock_guard<std::mutex> l(d_mutex);
  return port < d_ports.size() && d_ports[port].enabled;
}

bool shared_channelizer::got_samples() {
  std::lock_guard<std::mutex> l(d_mutex);
  bool g = d_got_samples;
  d_got_samples = false;
  return g;
}

void shared_channelizer::forecast(int /*noutput_items*/, gr_vector_int &ninput_items_required) {
  ninput_items_required[0] = static_cast<int>(d_hop);
}

int shared_channelizer::general_work(int /*noutput_items*/, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items, gr_vector_void_star &output_items) {
  if (ninput_items[0] < static_cast<int>(d_hop)) {
    return 0;
  }
  const size_t nports = output_items.size();
  d_scratch.resize(nports * d_opb);
  d_active.assign(nports, 0);

  d_backend->process(static_cast<const gr_complex *>(input_items[0]), nports, d_scratch.data(), d_active.data());

  for (size_t p = 0; p < nports; p++) {
    if (d_active[p]) {
      std::memcpy(output_items[p], d_scratch.data() + p * d_opb, d_opb * sizeof(gr_complex));
      produce(p, static_cast<int>(d_opb));
    }
  }
  {
    std::lock_guard<std::mutex> l(d_mutex);
    d_got_samples = true;
  }
  consume_each(static_cast<int>(d_hop));
  return WORK_CALLED_PRODUCE;
}
