# trunk-replay: offline IQ replay and parameter sweeps

`trunk-replay` runs the real trunk-oxide pipeline (shared channelizer, Rust
recorders and decoders, call state machine) against recorded IQ instead of
dongles, and records what came out. Use it to tune squelch / tuning, to compare
builds, and as the baseline for replacing C++ decoders.

Replay runs in **real time** by default (`--speed 1x`), on the wall clock as
live does, so a 10 minute capture takes 10 minutes; parallel variants run side
by side. Always use a release build — a debug build cannot keep up and the
summary shows `overruns > 0` when a run was not faithful.

`--speed max` runs as fast as it decodes (10-20x on this box), and comes out
the same every time. The files play unpaced and the sources keep the time
(`trunk_radio::pace`): one block at a time, the earliest source's first, and
the monitor loop handles everything a block caused, its clock moved to the
capture time, before the next block runs; the 1 s tick and 200 s status fall
due on capture time, and recording ages (write and grant timeouts) are capture
time too. A source also waits while the event recorder is behind, so nothing is
dropped. No source runs a block until the monitor has opened the control and
conventional channels (`Pacer::start`). Channelizer overruns mean nothing at
max speed. Against 1x, expect the same calls and audio but for the races 1x
has with its wall clock (a timeout that fires a few ms either side of a new
grant, say), and the first block or so of each channel, which at 1x may play
before it opens. Use max for A/B comparisons and iteration; use 1x to check a
build keeps up in real time.

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
trunk-replay run   --capture DIR --out RUN [--set key=value]... [--limit-s N] [--tap] [--speed 1x|max]
trunk-replay sweep --capture DIR --out DIR [--grid key=v1,v2,..]... [--set key=value]... [-j 4] [--speed 1x|max]
trunk-replay compare DIR
trunk-replay taps  RUN/taps
```

`--set` edits the config TOML before it is parsed: `source.2.freq_correction_hz=0`,
`system.conv.squelch=-48`, `system.conv.channel.0.squelch_db=-45`,
`recorder.call_timeout=5`. Two aliases fan out: `squelch` (every conventional
system) and `freq_correction_hz` (every source). `--grid` takes comma-separated
values and runs the cartesian product. Adaptive ppm correction is off in replay
unless you set `recorder.adaptive_freq_correction=true`.

## Output (per run)

- `events.jsonl` — one JSON object per control-channel message, unit event,
  call start and call end, with `t` = seconds since the flowgraph started.
- `audio/` — one WAV per call (`<t>_<system>_<tg>_<MHz>.wav`).
- `summary.json` — totals and per-system / per-talkgroup counts, mean
  |freq error|, signal / noise, decode errors, channelizer overruns.
- `config.toml` — the exact config that was replayed; `run.log` — process log.

`comparison.md` in a sweep directory tabulates every variant's summary.

## Decoder baselines (`--tap`)

`run --tap` also writes `RUN/taps/` — what each decoder was given, for
testing decoder changes offline and faster than real time (no wall-clock
logic involved), byte-identical from run to run. `trunk-replay taps RUN/taps`
writes `index.json` and prints a table. Per tap (`<base>` =
`cc_<system>_<freq Hz>` for a control channel, `v_<n>_<system>_<tg>` for a
P25 voice channel, numbered in the order channels open), with a
`<base>.json` sidecar describing it. Analog channels are not tapped since
GNU Radio went (`a_` taps are in older baselines only):

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
stopped by wall clock). At 1x, call-level results (`events.jsonl`,
`summary.json`) vary slightly run to run — ±1 call, ~1 s of audio — because the
state machine is wall-clock driven, so compare those with a tolerance. At
`--speed max` they do not: two runs give the same events in the same order
(only `t` in `events.jsonl` differs by a few ms, as the recorder stamps events
as it reads them), and the same `summary.json` but for `wall_seconds`. Taps are big (~4 GB per
10 minutes); `recorder.tap_dir` enables them outside replay.

## P25 decoders

trunk-p25 decodes all P25 in Rust: control channels, and voice all the way
to audio — demodulator, decoders, the call assembler (`trunk_p25::assemble`)
and the vocoders (`trunk_p25::vocoder`: op25's software IMBE decoder and
mbelib's AMBE+2 dequantiser, ported sample for sample, then taking the
standards' way where they won a vote, below). op25's chains were
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
- `TRUNK_P25_TAPS=RUN/taps cargo test -p trunk-radio --release --lib
  voice_capture -- --ignored --nocapture` runs every voice tap's IQ through
  trunk-p25's voice path and reports each call; with `VOICE_CORPUS=<dir>`
  it writes the speech corpus the vocoder examples use. While op25's frame
  assembler was in the tree this test drove it directly as an oracle: phase
  1 was op25's sample for sample on 375 calls; phase 2 differed only at the
  edges of lock (`LockEdges`: op25 framed from its first S-ISCH to ten
  bursts past its last, trunk-p25 while its flywheel holds), never in what
  an aligned frame carried, and trunk-p25 made sound of more frames.

The vocoders' policy (`trunk_p25::vocoder::Policy`, what to play, repeat
or mute) was measured against an alternative built on trunk-p25's soft
decisions: `VOCODER_CORPUS=<dir> cargo run --release -p trunk-p25 --example
vocoder_noise` scores both on tapped speech (the corpus is written by the
`voice_capture` test with `VOICE_CORPUS=<dir>`) under injected noise, by
decision class and log-spectral distance; `--example vocoder_ab -- <out>`
cuts blind A/B clips from the calls where they diverge most, and
`trunk-discord ab post <out>` / `ab tally <out>` puts them to a vote in the
hourly thread. Verdict (2026-10-19, one listener): op25's policy 5, soft 1;
phase 1 unanimous. Soft was not shipped; the apparatus stays for the next idea.
Also tried and dropped (bead q7f.7): taking AMBE+2's unprotected quantiser
values as expectations under the bits' posteriors; by log-spectral distance
never better than playing the soft decode as received.

Where op25 departs from TIA-102.BABA and BABA-1 (beads 4hj, 13y), the
standards' way was put to the same vote, twice (2026-10-20). The first
round (7 s clips, two listeners) put every phase 2 vote on the right-hand
clip and split phase 1; the second (`vocoder_ab` tests `u7`, `rules`,
`p2`: two-second clips where the two differ, each posted both ways round,
with a can't-tell vote) was mostly can't-tell, the standard's phase 1
error rules preferred 2 pairs to 1. Heard as close, the standards are
followed: phase 1 reads u7 as sent (op25 reads b̂0's low bits one bit low;
read as sent, the pitch on the tapped corpus moves 29% less from frame to
frame) and repeats, mutes and smooths by the standard on the bits the
decoder here corrected (op25 was given a regenerated frame and counted
none); phase 2 predicts past silence frames from the last voice frame,
knows a tone by u0 alone and plays it at its log level.

The vocoders were checked frame by frame against op25's on random streams
that reach every pitch, repeat, mute, tone and erasure, 16k frames a phase,
and on every tapped call's frames, and the C++ oracles are gone. Their
audio is frozen as hashes in the trunk-radio `p25_voice` tests
(`the_imbe_vocoder_is_frozen`, `the_ambe_vocoder_is_frozen`, the
clean-call tests), op25's until the votes and the standards' since. The match
with op25 needed its vocoder sources built with `-ffp-contract=off` (GCC
fuses multiply-adds under `-march=znver2`, Rust never does).

Voice taps hold the conditioned IQ (`.out.cf32`) only; `.symbols.f32` is in
taps from before the op25 chains were deleted (op25's demodulator output, or
with the regenerator its symbols — the sidecar's `"decoder"` says which).
The `voice_golden` and `tdma_golden` scorers need op25's own symbols, from
such older taps.

Calls are counted in `trunk.oxide.p25call.{calls,audio,transmissions,
bit_errors,spikes,encrypted,repeated,muted}{system,phase}` (`repeated` and
`muted` are the voice frames the vocoder would not play as received: what
the listener heard of a bad channel); the decoder stage metrics are
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
