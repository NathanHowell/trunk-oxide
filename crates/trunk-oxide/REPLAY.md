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
| `.in.cf32` | conditioner input (`in_rate`: 24 kS/s digital, ~101 kS/s analog). Only in baselines from before the conditioner moved into the Rust channelizer (k55), e.g. the golden capture's | `examples/condcheck.rs` input |
| `.out.cf32` | the channelizer's conditioned output: after resample, squelch, RMS AGC and FLL (`out_rate`) | input of a Rust demod / P25 decoder; `condcheck` expected output |
| `.dibits` | control channels, only in taps from before op25's control channel chain was deleted: its slicer output, one byte (0..3) per dibit. trunk-p25 decodes inside the channelizer and writes none | input of a frame assembler |
| `.symbols.f32` | voice calls: demodulator output (soft symbols) | input of a Rust voice frame decoder |
| `.msgs.jsonl` | control channels: every raw decoder message in op25's form (`type`, `arg1`, `arg2` = a timeout's wall clock time, `hex` payload, `ms` since start) | golden output of the decoder |

Voice taps cover the span the recorder was active. An analog channel gets one
tap per *enabled window* (`a_<recorder>_w<n>_<system>_<tg>`): a conventional
channel is only enabled while signal detection sees a carrier, so there are
gaps between windows and squelch gating is inside `.out.cf32`. The decoded WAVs
and `events.jsonl` from the same run are the end-to-end golden output.

The taps are deterministic: two replays of the same capture produce
byte-identical `.out.cf32` / `.dibits` and identical raw message
sequences (only the tail length differs, by a few blocks, because the run is
stopped by wall clock). Call-level results (`events.jsonl`, `summary.json`) vary
slightly run to run — ±1 call, ~1 s of audio — because the state machine is
wall-clock driven, so compare those with a tolerance. Taps are big (~4 GB per
10 minutes); `recorder.tap_dir` enables them outside replay.

## P25 decoders

trunk-p25 decodes all P25 in Rust: control channels, and voice all the way
to audio — demodulator, decoders, the call assembler (`trunk_p25::assemble`)
and the vocoders (`trunk_p25::vocoder`: op25's software IMBE decoder and
mbelib's AMBE+2 dequantiser, ported sample for sample). op25's chains were
deleted once shadow mode had compared them live: control channels
(trunk-p25 decoding everything op25 did and ~40% more), then the regenerator
into op25's frame assembler against trunk-p25's assembler (c4086cf). The
vocoders replaced op25's once they matched it exactly. `recorder.soft_vocoder`
now only chooses DMR's vocoder.

Offline, without a replay:
- `cargo run --release -p trunk-p25 --example golden -- RUN/taps` decodes the
  control channels' `.out.cf32` (and `.dibits`, if any) and scores the TSBKs
  against `.msgs.jsonl` (op25's in the golden capture's taps, trunk-p25's own
  in newer ones);
- `cargo run --release -p trunk-p25 --example voice_golden -- RUN/taps`
  decodes every phase 1 call's `.symbols.f32` and `.out.cf32`, and compares
  op25's decoding algorithms with trunk-p25's on the same frames;
- `cargo run --release -p trunk-p25 --example tdma_golden -- RUN/taps
  [wacn=0xbee00 sysid=0x5e4 nac=0x5e1]` does the same for phase 2 calls:
  an emulation of op25's TDMA chain on its own symbols, trunk-p25 on those
  symbols and on the IQ, and op25 again on trunk-p25's regenerated stream
  (the defaults are svrcs's scrambling parameters);
- `TRUNK_P25_TAPS=RUN/taps cargo test -p trunk-recorder-sys --release --lib
  voice_capture -- --ignored --nocapture` runs every voice tap's IQ through
  op25's frame assembler (driven directly, `op25_voice_reference`) after
  trunk-p25's regenerator, and through trunk-p25's own path, and aligns the
  frames each vocoded: phase 1 must be op25's sample for sample; phase 2 may
  differ only at the edges of lock (`LockEdges`: op25 frames from its first
  S-ISCH to ten bursts past its last, trunk-p25 while its flywheel holds).
  Every call's vocoder inputs as op25 framed them must make op25's audio
  through trunk-p25's vocoders, sample for sample.

The vocoders are also checked frame by frame against op25's
(`op25_vocoder_reference`) on random streams that reach every pitch, repeat,
mute, tone and erasure (`the_imbe_vocoder_is_op25s`,
`the_ambe_vocoder_is_op25s`). That holds only while op25's
`software_imbe_decoder.cc` and `ambe.c` are built with `-ffp-contract=off`
(op25's CMakeLists): GCC fuses multiply-adds under `-march=znver2`, Rust
never does.

Voice taps hold the conditioned IQ (`.out.cf32`) only; `.symbols.f32` is in
taps from before the op25 chains were deleted (op25's demodulator output, or
with the regenerator its symbols — the sidecar's `"decoder"` says which).
The `voice_golden` and `tdma_golden` scorers need op25's own symbols, from
such older taps.

Calls are counted in `trunk.oxide.p25call.{calls,audio,transmissions,
bit_errors,spikes,encrypted}{system,phase}`; the decoder stage metrics are
`p25cc`, `p25voice` and `p25tdma`. A replay writes the call tallies and the
control channel parser's counts to `summary.json` under `decoders`.

## Control channel parser

trunk-p25's parser (`trunk_p25::trunking`, `trunk_recorder_sys::p25_parse`)
turns control channel messages into trunking messages. It shares channel
plans across a system's control channels and fixes trunk-recorder's
misreadings (the named divergences in `p25_parse.rs`); `summary.json`
`decoders.parser` has its per-system counts (packet kinds; channels resolved
by the control channel's own plans, another one's, or not at all).
trunk-recorder's `P25Parser` is deleted. Before it went it matched the Rust
parser on a 98k-message capture except for the named divergences, and its
output for the unit-test corpus is kept in `p25_parse/tests/reference.rs`,
which `matches_trunk_recorder_except_where_noted` still checks against.

## Notes

- A conventional channel with no explicit `squelch_db` uses the system squelch,
  so `squelch=` only matters for channels that inherit it.
- Sources not present in the capture are disabled.
- `trunk-replay` is built into the trunk-oxide image
  (`/usr/local/bin/trunk-replay`).
