#include "p25_voice_synth.h"

#include <cstring>

#include "imbe_vocoder/imbe_vocoder.h"
#include "mbelib.h"
#include "ambe.h"
#include "p25p2_vf.h"
#include "software_imbe_decoder.h"

// The state op25's p25p1_fdma and p25p2_tdma keep for their vocoders.
struct p25_voice_synth::impl {
  bool soft_vocoder;
  // Phase 1.
  software_imbe_decoder p1_software;
  imbe_vocoder p1_vocoder;
  // Phase 2.
  p25p2_vf vf;
  mbe_parms cur_mp, prev_mp, enh_mp;
  mbe_tone tone_mp;
  mbe_errs errs_mp;
  int mbe_err_cnt = 0;
  bool tone_frame = false;
  software_imbe_decoder p2_software;
  imbe_vocoder p2_vocoder;

  explicit impl(bool soft) : soft_vocoder(soft) {
    mbe_initMbeParms(&cur_mp, &prev_mp, &enh_mp);
    mbe_initToneParms(&tone_mp);
    mbe_initErrParms(&errs_mp);
  }
};

p25_voice_synth::p25_voice_synth(bool soft_vocoder) : d(new impl(soft_vocoder)) {}

p25_voice_synth::~p25_voice_synth() = default;

// p25p1_fdma::process_voice, for an unencrypted codeword.
void p25_voice_synth::phase1(const uint32_t u[8], uint32_t e0, uint32_t et, int16_t out[samples_per_frame]) {
  if (d->soft_vocoder) {
    d->p1_software.decode_fullrate(out, u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], e0, et);
  } else {
    int16_t frame_vector[8];
    for (int i = 0; i < 8; i++) {
      frame_vector[i] = u[i] & 0xFFFF;
    }
    frame_vector[7] >>= 1;
    d->p1_vocoder.imbe_decode(frame_vector, out);
  }
}

// p25p2_tdma::handle_voice_frame, without decryption (no keys are loaded,
// so an encrypted frame is never audio).
bool p25_voice_synth::phase2(const uint8_t dibits[36], bool encrypted, int16_t out[samples_per_frame]) {
  std::memset(out, 0, samples_per_frame * sizeof(int16_t));
  int u[4];
  int b[9];
  d->vf.process_vcw(&d->errs_mp, dibits, b, u);
  bool audible = false;
  if (!encrypted) {
    int rc = mbe_dequantizeAmbeTone(&d->tone_mp, &d->errs_mp, u);
    if (rc >= 0) { // tone frame
      if (rc == 0) {
        d->tone_frame = true;
        d->mbe_err_cnt = 0;
      } else if ((++d->mbe_err_cnt < 4) && d->tone_frame) {
        mbe_useLastMbeParms(&d->cur_mp, &d->prev_mp);
        rc = 0;
      } else {
        d->tone_frame = false;
      }
    } else {
      rc = mbe_dequantizeAmbe2250Parms(&d->cur_mp, &d->prev_mp, &d->errs_mp, b);
      if (rc == 0) {
        d->tone_frame = false;
        d->mbe_err_cnt = 0;
      } else if ((++d->mbe_err_cnt < 4) && !d->tone_frame) {
        mbe_useLastMbeParms(&d->cur_mp, &d->prev_mp);
        rc = 0;
      } else {
        d->tone_frame = false;
      }
    }
    if ((rc == 0) && (d->errs_mp.ER <= 0.096)) {
      audible = true;
      if (d->tone_frame) {
        d->p2_software.decode_tone(out, d->tone_mp.ID, d->tone_mp.AD, &d->tone_mp.n);
      } else if (d->soft_vocoder) {
        int K = 12;
        if (d->cur_mp.L <= 36) K = int(float(d->cur_mp.L + 2.0) / 3.0);
        d->p2_software.decode_tap(out, d->cur_mp.L, K, d->cur_mp.w0, &d->cur_mp.Vl[1], &d->cur_mp.Ml[1]);
      } else {
        d->p2_vocoder.decode_tap(out, d->cur_mp.L, d->cur_mp.w0, &d->cur_mp.Vl[1], &d->cur_mp.Ml[1]);
      }
    }
  }
  mbe_moveMbeParms(&d->cur_mp, &d->prev_mp);
  mbe_moveMbeParms(&d->cur_mp, &d->enh_mp);
  return audible || encrypted;
}
