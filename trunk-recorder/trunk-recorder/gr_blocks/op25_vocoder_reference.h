#ifndef OP25_VOCODER_REFERENCE_H
#define OP25_VOCODER_REFERENCE_H

#include <cstdint>
#include <memory>

// op25's P25 vocoders, one 20 ms frame at a time: what its frame assembler
// does with each voice codeword once it has framed and decoded it. The
// reference trunk-p25's vocoders (crates/trunk-p25 src/vocoder) are checked
// against.
//
// Kept across frames as op25 keeps its own (the synthesisers carry pitch,
// phase and error rate from frame to frame).
class op25_vocoder_reference {
public:
  static constexpr int samples_per_frame = 160; // 8 kHz

  op25_vocoder_reference();
  ~op25_vocoder_reference();

  // Phase 1: one IMBE frame's eight parameter vectors as op25 holds them
  // (u7 shifted left one bit), and the bits corrected in c0 (e0) and in all
  // (et).
  void phase1(const uint32_t u[8], uint32_t e0, uint32_t et, int16_t out[samples_per_frame]);

  // Phase 2: one AMBE+2 frame, its 36 descrambled dibits as received.
  // Encrypted frames are silent but still update the running error rate.
  // Returns false if the frame was muted (silence for errors or repeats).
  bool phase2(const uint8_t dibits[36], bool encrypted, int16_t out[samples_per_frame]);

private:
  struct impl;
  std::unique_ptr<impl> d;
};

#endif
