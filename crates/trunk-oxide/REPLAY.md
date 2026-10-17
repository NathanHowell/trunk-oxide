# trunk-replay: offline IQ replay and parameter sweeps

`trunk-replay` runs the real trunk-oxide pipeline (shared channelizer, C++
recorders and decoders, call state machine) against recorded IQ instead of
dongles, and records what came out. Use it to tune squelch / tuning, to compare
builds, and as the baseline for replacing C++ decoders.

Replay is **real time** (the state machine uses wall-clock timers), so a 10
minute capture takes 10 minutes; parallel variants run side by side. Always use
a release build — a debug build cannot keep up and the summary shows
`overruns > 0` when a run was not faithful.

## Capture directory

```
capture.json   what was recorded (serial, center_hz, rate_sps, gain_db, file)
config.toml    the live config at capture time (systems, recorders, squelch)
*.csv          the talkgroup / channel files it references
<serial>_<center>_<rate>.cu8   raw rtl_sdr output (interleaved unsigned 8-bit I/Q)
```

Make one by stopping trunk-oxide (it holds the dongles) and running one
`rtl_sdr -d <serial> -f <center> -s <rate> -g <gain> -n <rate*seconds>` per
dongle at the same time. Use the gains auto-calibration picked
(`gain auto-calibration complete` in the trunk-oxide log). `rtl_sdr` does not
apply `freq_correction_hz`; replay does, through the normal tune path.

## Commands

```
trunk-replay run   --capture DIR --out RUN [--set key=value]... [--limit-s N] [--tap]
trunk-replay sweep --capture DIR --out DIR [--grid key=v1,v2,..]... [--set key=value]... [-j 4]
trunk-replay compare DIR
trunk-replay taps  RUN/taps
```

`--set` edits the config TOML before it is parsed: `source.2.freq_correction_hz=0`,
`system.conv.squelch=-48`, `system.conv.channel.0.squelch_db=-45`,
`recorder.call_timeout=5`. Two aliases fan out: `squelch` (every conventional
system) and `freq_correction_hz` (every source). `--grid` takes comma-separated
values and runs the cartesian product. Adaptive ppm correction is off in replay
unless you set `recorder.adaptive_freq_correction=true`.

Each sweep child runs in its own IPC namespace (`unshare`): GNU Radio uses ~900
SysV shared-memory segments per process and the host allows 4096.

## Output (per run)

- `events.jsonl` — one JSON object per control-channel message, unit event,
  call start and call end, with `t` = seconds since the flowgraph started.
- `audio/` — one WAV per call (`<t>_<system>_<tg>_<MHz>.wav`).
- `summary.json` — totals and per-system / per-talkgroup counts, mean
  |freq error|, signal / noise, decode errors, channelizer overruns.
- `config.toml` — the exact config that was replayed; `run.log` — process log.

`comparison.md` in a sweep directory tabulates every variant's summary.

## Decoder baselines (`--tap`)

`run --tap` also writes `RUN/taps/` — the inputs and outputs of the C++ signal
chain, for testing Rust replacements offline and faster than real time (no
wall-clock logic involved). `trunk-replay taps RUN/taps` writes `index.json`
and prints a table. Per tap (`<base>` = `cc_<system>_<freq Hz>` for a control
channel, `v_<call id>_<system>_<tg>` for a P25 voice call, `a_<call id>_<system>_<tg>`
for an analog channel), with a `<base>.json` sidecar describing it:

| file | what | use |
|---|---|---|
| `.in.cf32` | conditioner input: the Rust shared channelizer's output (`in_rate`: 24 kS/s digital, ~101 kS/s analog) | input of a Rust conditioner (bead k55) |
| `.out.cf32` | conditioner output: after resample, squelch, RMS AGC and FLL (`out_rate`) | input of a Rust demod / P25 decoder; expected output of the conditioner |
| `.dibits` | control channels: slicer output, one byte (0..3) per dibit | input of a Rust frame assembler |
| `.symbols.f32` | voice calls: demodulator output (soft symbols) | input of a Rust voice frame decoder |
| `.msgs.jsonl` | control channels: every raw decoder message (`type`, `arg1`, `arg2`, `hex` payload, `ms` since start) | golden output of the decoder |

Voice taps cover the span the recorder was active. An analog channel gets one
tap per *enabled window* (`a_<recorder>_w<n>_<system>_<tg>`): a conventional
channel is only enabled while signal detection sees a carrier, so there are
gaps between windows and squelch gating is inside `.out.cf32`. The decoded WAVs
and `events.jsonl` from the same run are the end-to-end golden output.

The taps are deterministic: two replays of the same capture produce
byte-identical `.in.cf32` / `.out.cf32` / `.dibits` and identical raw message
sequences (only the tail length differs, by a few blocks, because the run is
stopped by wall clock). Call-level results (`events.jsonl`, `summary.json`) vary
slightly run to run — ±1 call, ~1 s of audio — because the state machine is
wall-clock driven, so compare those with a tolerance. Taps are big (~4 GB per
10 minutes); `recorder.tap_dir` enables them outside replay.

## Notes

- A conventional channel with no explicit `squelch_db` uses the system squelch,
  so `squelch=` only matters for channels that inherit it.
- Sources not present in the capture are disabled.
- `trunk-replay` is built into the trunk-oxide image
  (`/usr/local/bin/trunk-replay`).
