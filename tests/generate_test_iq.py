"""Generate a synthetic IQ file with an FM-modulated tone for trunk-recorder testing.

Produces gr_complex (interleaved float32 I/Q) format.
A 1 kHz tone is FM-modulated onto a carrier offset from center, producing
a signal that trunk-recorder's analog recorder + squelch can detect.
"""

import struct
import sys

import numpy as np

# Parameters
SAMPLE_RATE = 2_400_000  # 2.4 Msps — must be a multiple of 24000 for trunk-recorder
DURATION_S = 30  # 30 seconds, with repeat=true this loops forever
CENTER_FREQ = 155_000_000  # 155 MHz (VHF)
CHANNEL_FREQ = 155_010_000  # channel 10 kHz above center
TONE_FREQ = 1_000  # 1 kHz audio tone
FM_DEVIATION = 5_000  # 5 kHz deviation (narrowband FM)
AMPLITUDE = 0.8

# Derived
offset = CHANNEL_FREQ - CENTER_FREQ  # 10 kHz offset from center
n_samples = SAMPLE_RATE * DURATION_S
t = np.arange(n_samples, dtype=np.float64) / SAMPLE_RATE

# FM modulate: audio tone -> phase integral -> complex baseband at offset
audio = np.sin(2 * np.pi * TONE_FREQ * t)
phase = 2 * np.pi * offset * t + 2 * np.pi * FM_DEVIATION * np.cumsum(audio) / SAMPLE_RATE
iq = AMPLITUDE * np.exp(1j * phase).astype(np.complex64)

output = sys.argv[1] if len(sys.argv) > 1 else "test.iq"
iq.tofile(output)
n_mb = iq.nbytes / 1_048_576
print(f"Wrote {n_samples} samples ({n_mb:.1f} MB) to {output}")
print(f"  Sample rate: {SAMPLE_RATE} sps")
print(f"  Center freq: {CENTER_FREQ} Hz")
print(f"  Channel at:  {CHANNEL_FREQ} Hz (offset {offset} Hz)")
print(f"  Duration:    {DURATION_S} s")
