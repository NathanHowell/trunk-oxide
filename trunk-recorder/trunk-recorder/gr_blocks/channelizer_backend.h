#ifndef CHANNELIZER_BACKEND_H
#define CHANNELIZER_BACKEND_H

#include <cstddef>
#include <cstdint>
#include <gnuradio/gr_complex.h>

// Channel profiles. A profile fixes a channel's geometry (output rate and
// default filter); every channel, whatever its profile, has its own centre
// frequency and its own filter. All profiles share one forward FFT per source.
enum ChannelProfile : unsigned {
  CHANNEL_PROFILE_NARROW = 0, // digital voice (P25/DMR/Smartnet), ~24 kS/s
  CHANNEL_PROFILE_WIDE = 1,   // analog FM voice, >= 96 kS/s
};

// DSP engine behind shared_channelizer. The trunk-recorder tree only sees this
// interface; trunk-oxide implements it in Rust (trunk-channelizer crate).
//
// Threading: set_channel/set_filter/clear_channel may be called from any
// thread; process() is called only from the GNU Radio thread running the block
// and applies pending calls between blocks.
class channelizer_backend {
public:
  virtual ~channelizer_backend() = default;

  // Input samples consumed per process() call (source rate).
  virtual size_t hop() const = 0;
  // False if the source rate is too low for this profile.
  virtual bool has_profile(unsigned profile) const = 0;
  // Output samples produced per active channel of `profile` per process() call.
  virtual size_t out_per_block(unsigned profile) const = 0;
  // Largest out_per_block() over all profiles (the per-port stride).
  virtual size_t max_out_per_block() const = 0;
  // Output sample rate of every channel of `profile` (Hz).
  virtual double out_rate(unsigned profile) const = 0;

  // Enable `port` as a `profile` channel at `offset_hz` from the source
  // centre, or retune it.
  virtual void set_channel(unsigned port, unsigned profile, double offset_hz) = 0;
  // Give `port` its own filter (cutoff = 6 dB point, one-sided; Hz). It
  // persists while the port is disabled. Never called => profile default.
  virtual void set_filter(unsigned port, double cutoff_hz, double transition_hz) = 0;
  // Disable `port`: it produces no output from the next block on.
  virtual void clear_channel(unsigned port) = 0;

  // Consume exactly hop() samples. For each active port p < nports, write
  // out_per_block(profile of p) samples to out[p * max_out_per_block()] and set
  // produced[p] to that count; all other produced[] entries are set to 0.
  virtual void process(const gr_complex *in, size_t nports, gr_complex *out, uint32_t *produced) = 0;
};

#endif
