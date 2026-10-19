#ifndef OP25_VOICE_REFERENCE_H
#define OP25_VOICE_REFERENCE_H

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

// op25's P25 voice frame assembler (p25p1_fdma / p25p2_tdma) driven
// directly with dibits, as p25_frame_assembler does in a recorder: the
// reference trunk-p25's call assembler is checked against.
class op25_voice_reference {
public:
  // phase2: TDMA on logical channel `slot`, descrambled with the
  // 2160-dibit `xor_mask`.
  // Vocodes with op25's software IMBE decoder (soft_vocoder).
  op25_voice_reference(bool phase2, int slot, const std::string &xor_mask);
  ~op25_voice_reference();

  // One call of p25_frame_assembler::general_work with these dibits.
  void push(const uint8_t *dibits, size_t n);

  // Audio so far (160 samples per voice frame).
  const std::vector<int16_t> &pcm() const;
  // In-band source units as op25 tags them (one per work call that produced
  // audio after a new one was heard).
  const std::vector<long> &sources() const;
  // Where each terminator fell in pcm().
  const std::vector<long> &terminators() const;
  // JSON messages (encryption, aliases, unit/group), in order.
  const std::vector<std::string> &json() const;
  // Every frame's vocoder input, in order, as records: phase 1
  // {u0..u7, E0, ET, encrypted}, phase 2 {36 dibits, encrypted}.
  const std::vector<uint32_t> &voice_inputs() const;

private:
  struct impl;
  std::unique_ptr<impl> d;
};

#endif
