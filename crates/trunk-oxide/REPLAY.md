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
trunk-replay run   --capture DIR --out RUN [--set key=value]... [--limit-s N]
trunk-replay sweep --capture DIR --out DIR [--grid key=v1,v2,..]... [--set key=value]... [-j 4]
trunk-replay compare DIR
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

## Notes

- A conventional channel with no explicit `squelch_db` uses the system squelch,
  so `squelch=` only matters for channels that inherit it.
- Sources not present in the capture are disabled.
- `trunk-replay` is built into the trunk-oxide image
  (`/usr/local/bin/trunk-replay`).
