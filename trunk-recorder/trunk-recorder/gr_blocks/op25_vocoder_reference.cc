#include "op25_vocoder_reference.h"

#include <cstring>

#include "mbelib.h"
#include "ambe.h"
#include "p25p2_vf.h"
#include "software_imbe_decoder.h"

// The state op25's p25p1_fdma and p25p2_tdma keep for their vocoders, with
// soft_vocoder set (its software IMBE decoder).
struct op25_vocoder_reference::impl {
  // Phase 1.
  software_imbe_decoder p1_software;
  // Phase 2.
  p25p2_vf vf;
  mbe_parms cur_mp, prev_mp, enh_mp;
  mbe_tone tone_mp;
  mbe_errs errs_mp;
  int mbe_err_cnt = 0;
  bool tone_frame = false;
  software_imbe_decoder p2_software;

  impl() {
    mbe_initMbeParms(&cur_mp, &prev_mp, &enh_mp);
    mbe_initToneParms(&tone_mp);
    mbe_initErrParms(&errs_mp);
  }
};

op25_vocoder_reference::op25_vocoder_reference() : d(new impl()) {}

op25_vocoder_reference::~op25_vocoder_reference() = default;

// p25p1_fdma::process_voice, for an unencrypted codeword.
void op25_vocoder_reference::phase1(const uint32_t u[8], uint32_t e0, uint32_t et, int16_t out[samples_per_frame]) {
  d->p1_software.decode_fullrate(out, u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], e0, et);
}

// p25p2_tdma::handle_voice_frame, without decryption (no keys are loaded,
// so an encrypted frame is never audio).
bool op25_vocoder_reference::phase2(const uint8_t dibits[36], bool encrypted, int16_t out[samples_per_frame]) {
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
      } else {
        int K = 12;
        if (d->cur_mp.L <= 36) K = int(float(d->cur_mp.L + 2.0) / 3.0);
        d->p2_software.decode_tap(out, d->cur_mp.L, K, d->cur_mp.w0, &d->cur_mp.Vl[1], &d->cur_mp.Ml[1]);
      }
    }
  }
  mbe_moveMbeParms(&d->cur_mp, &d->prev_mp);
  mbe_moveMbeParms(&d->cur_mp, &d->enh_mp);
  return audible || encrypted;
}
