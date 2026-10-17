#ifndef SHARED_CHANNELIZER_H
#define SHARED_CHANNELIZER_H

#include <functional>
#include <gnuradio/block.h>
#include <memory>
#include <mutex>
#include <vector>

#include "channelizer_backend.h"

// What a recorder's xlat_channelizer calls instead of its own GR blocks when
// the channelizer conditions its port (empty `set` = it does not).
struct ConditioningHooks {
  std::function<void(const ConditioningSpec &)> set;
  std::function<int()> freq_error;
  std::function<double()> power_db; // NaN if no signal yet
  std::function<void(double)> set_squelch_db;
  std::function<bool()> squelch_open;
  std::function<void(std::function<void(bool, double)>)> set_squelch_callback; // (muted, pwr_db)
};

// One input (source IQ), N outputs (one per recorder). Each enabled output
// carries that recorder's channel at out_rate(its profile); disabled outputs
// produce nothing (like gr::blocks::selector, so a stalled recorder only
// blocks the flowgraph while it is enabled).
class shared_channelizer : public gr::block {
public:
  typedef std::shared_ptr<shared_channelizer> sptr;
  static sptr make(std::shared_ptr<channelizer_backend> backend);

  shared_channelizer(std::shared_ptr<channelizer_backend> backend);

  bool has_profile(unsigned profile) const { return d_backend->has_profile(profile); }
  double out_rate(unsigned profile) const { return d_backend->out_rate(profile); }

  // Channel profile of the port; set once when the recorder is connected,
  // before the port is first enabled.
  void set_port_profile(unsigned port, unsigned profile);
  // The port's own filter (see channelizer_backend::set_filter). Remembered
  // across enable/disable.
  void set_port_filter(unsigned port, double cutoff_hz, double transition_hz);
  // Offset of the port's channel from the source centre. Takes effect
  // immediately if the port is enabled, else when it is enabled.
  void set_port_offset(unsigned port, double offset_hz);
  // Conditioning of the port's output (see channelizer_backend::set_conditioning).
  void set_port_conditioning(unsigned port, const ConditioningSpec &spec);
  int port_freq_error_hz(unsigned port) { return d_backend->freq_error_hz(port); }
  double port_power_db(unsigned port) { return d_backend->power_db(port); }
  void set_port_squelch_db(unsigned port, double db) { d_backend->set_squelch_db(port, db); }
  bool port_squelch_open(unsigned port) { return d_backend->squelch_open(port); }
  // Called on the GR thread, after the block that saw the transition.
  void set_port_squelch_callback(unsigned port, std::function<void(bool, double)> cb);
  // The hooks an xlat_channelizer on `port` uses.
  ConditioningHooks conditioning_hooks(unsigned port);
  void set_port_enabled(unsigned port, bool enabled);
  bool is_port_enabled(unsigned port);
  // True if samples flowed since the last call.
  bool got_samples();

  void forecast(int noutput_items, gr_vector_int &ninput_items_required) override;
  int general_work(int noutput_items, gr_vector_int &ninput_items, gr_vector_const_void_star &input_items, gr_vector_void_star &output_items) override;

private:
  struct Port {
    double offset_hz = 0.0;
    unsigned profile = CHANNEL_PROFILE_NARROW;
    bool enabled = false;
    std::function<void(bool, double)> squelch_cb;
  };
  Port &port_locked(unsigned port);

  std::shared_ptr<channelizer_backend> d_backend;
  size_t d_hop;
  size_t d_stride; // max_out_per_block: per-port stride in d_scratch
  std::mutex d_mutex;
  std::vector<Port> d_ports;
  std::vector<gr_complex> d_scratch;
  std::vector<uint32_t> d_produced;
  bool d_got_samples = false;
  struct PendingEvent {
    std::function<void(bool, double)> cb;
    bool muted;
    double db;
  };
  std::vector<PendingEvent> d_pending_events;
};

#endif
