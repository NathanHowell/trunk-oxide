#ifndef CHANNELIZER_BACKEND_H
#define CHANNELIZER_BACKEND_H

#include <cstddef>
#include <cstdint>
#include <gnuradio/gr_complex.h>
#include <string>

// Channel profiles. A profile fixes a channel's geometry (output rate and
// default filter); every channel, whatever its profile, has its own centre
// frequency and its own filter. All profiles share one forward FFT per source.
enum ChannelProfile : unsigned {
  CHANNEL_PROFILE_NARROW = 0, // digital voice (P25/DMR/Smartnet), ~24 kS/s
  CHANNEL_PROFILE_WIDE = 1,   // analog FM voice, >= 96 kS/s
};

enum ConditioningSquelch : unsigned {
  CONDITIONING_SQUELCH_NONE = 0,
  CONDITIONING_SQUELCH_ANALOG = 1,  // alpha 0.01, 10-sample ramps, zeros while muted
  CONDITIONING_SQUELCH_DIGITAL = 2, // alpha 1e-4, no ramp, muted samples dropped
};

// Per-port conditioning, done in the channelizer: resample
// in_rate -> out_rate when lower, power squelch, RMS AGC, band-edge FLL.
struct ConditioningSpec {
  unsigned sps;
  double excess_bw;
  double in_rate;
  double out_rate;
  ConditioningSquelch squelch;
};

// Decoder a port runs on its conditioned output, inside the channelizer.
enum class ControlDecoder : unsigned {
  None = 0,
  P25Cqpsk = 1,
  P25C4fm = 2,
};

struct ControlDecoderSpec {
  ControlDecoder kind;
  double rate;        // the port's conditioned output rate (S/s)
  std::string system; // short name, for metrics
  double freq_hz;     // control channel frequency, for metrics
  bool primary = true; // its messages drive trunking (else a shadow)
};

// A decoded message in op25's form: type (DUID; 0xffff = timeout) and body
// (NAC, then data), exactly what p25_frame_assembler used to send.
struct DecodedMessage {
  long type;
  std::string payload;
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

  // Condition the port's output (see ConditioningSpec). Persists while the
  // port is disabled; calling it again with a different `sps` (P25 phase
  // 1 <-> 2) or squelch mode changes it without resetting the loops. A
  // resampling or gating port produces fewer than out_per_block() samples.
  virtual void set_conditioning(unsigned port, const ConditioningSpec &spec) = 0;
  virtual void set_squelch_db(unsigned port, double db) = 0;
  virtual bool squelch_open(unsigned port) = 0;
  // Pop the oldest squelch transition: 0 none, 1 opened, 2 closed; pwr_db is
  // the squelch power at the transition.
  virtual int next_squelch_event(unsigned port, double &pwr_db) = 0;
  // FLL frequency error (Hz) and smoothed input power (dB; NaN if none yet) of
  // a conditioned port. Safe to call from any thread.
  virtual int freq_error_hz(unsigned port) = 0;
  virtual double power_db(unsigned port) = 0;

  // Run a decoder on the port's conditioned output (kind None stops it). It
  // persists while the port is disabled and resets itself on a retune.
  virtual void set_decoder(unsigned port, const ControlDecoderSpec &spec) = 0;
  // Pop the port decoder's oldest message; false if none.
  virtual bool next_message(unsigned port, DecodedMessage &msg) = 0;

  // Consume exactly hop() samples. For each active port p < nports, write
  // out_per_block(profile of p) samples to out[p * max_out_per_block()] and set
  // produced[p] to that count; all other produced[] entries are set to 0.
  virtual void process(const gr_complex *in, size_t nports, gr_complex *out, uint32_t *produced) = 0;
};

#endif
