# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

trunk-oxide is a Rust SDR (Software Defined Radio) scanner, all in Rust: it opens RTL-SDR dongles with librtlsdr, channelizes them, decodes P25 control and voice channels and analog FM, runs the call state machine, and publishes calls and audio over gRPC to downstream consumers (Discord bot, speech-to-text service). It began as a Rust driver for trunk-recorder (C++/GNU Radio); that stack was removed in October 2026 (epic trunk-oxide-00p).

## Build Commands

```bash
# Build all workspace members
cargo build

# Build a specific crate
cargo build -p trunk-discord
cargo build -p trunk-solver

# Run clippy (strict: clippy::all = deny, pedantic/nursery = warn)
cargo clippy

# Run tests
cargo test
cargo test -p trunk-config

# Format
cargo fmt
```

## Workspace Structure

- **trunk-oxide** — Main binary. Loads TOML config, calibrates and opens the sources, builds the `trunk-radio::Radio`, and runs the call state machine (`call_state.rs` pure, `call_manager.rs` executor) on a monitor thread inside tokio; gRPC/timers/signals on tokio. Who gets one of the `[recorder] max_channels` channels is `allocation.rs`: one pure rank-prefix function (lower priority wins, holders beat equal newcomers); the state machine emits the diff as `Open`/`Close` effects, a fall-out being an eviction.
- **trunk-radio** — The radio: sources (one DSP thread each) feeding shared channelizers, and the channels opened on their slots. A channel's `Kind` (associated `Params`/`Decoder`, `PROFILE`, tuning: `P25Voice`, `AnalogFm`, `P25Control`, each beside its decoder) is chosen once at config parse as a `Recipe<K>` behind `Arc<dyn Opener>`; `Radio::open::<K>(call, params)` / `close` / `rotate` is all the state machine does with it. `types.rs` holds what the radio and the state machine say to each other (`TrunkMessage`/`Trunking`, `RadioEvent`, `CallId`, `ChannelNum`, `CallData`, …).
- **trunk-oxide-proto** / **trunk-transcriber-proto** — Protobuf definitions compiled with tonic-prost-build. `trunk-transcriber-proto` defines a generic `Transcriber` gRPC service implemented by both speech-to-text backends.
- **trunk-oxide-grpc** — EventBridge: central hub that receives call events and audio and serves them over gRPC (Unix socket).
- **trunk-config** — TOML config parser with CSV overlay support for talkgroups/channels.
- **trunk-database** — SQLite storage with hourly rotating database files and schema versioning via SHA256.
- **trunk-discord** — Discord bot that subscribes to gRPC events, posts call embeds, manages threads, encodes OGG audio.
- **trunk-monitor** — Keeps the #monitoring dashboard (a chart of VictoriaMetrics, drawn with plotters) up to date in Discord. Its own container and its own gRPC health check, which reports NotServing once the dashboard has gone stale (three missed refreshes), and exits for a restart if its loop wedges. Config `config-monitor.toml` (`[monitor]`).
- **trunk-weather** — Streams a Tempest weather station (WeatherFlow cloud WebSocket, personal token in `secrets/tempest_token`) into VictoriaMetrics as `trunk.weather.*` metrics (rain, temperature, wind, lightning) to correlate with decode quality. Its own container and gRPC health check, which reports NotServing once no observation has arrived for ten minutes. No config file: `--tempest-token`, optional `--device-id`.
- **trunk-whisperer** — Speech-to-text service using whisper-rs (whisper.cpp FFI). Optional CUDA feature. Serves `Transcriber`.
- **trunk-parakeet** — Speech-to-text service using parakeet-rs (NVIDIA Parakeet ASR via ONNX Runtime). Drop-in replacement for trunk-whisperer — serves the same `Transcriber` gRPC service on the same Unix socket; backends are swapped at the container/image level, not via trunk-discord config.
- **trunk-whistle** — Speech-to-text service using Cactus Whistle (16.9 MB CPU-only model) via the prebuilt `libneedle.a` C engine (FFI in `needle.rs`, the only `allow(unsafe_code)` outside the sys crates). Same `Transcriber` gRPC service/socket as the other two backends. Not in `default-members`: building needs `NEEDLE_LIB_DIR` (+ libc++ ≥ 21 static libs); use `Dockerfile.whistle` / `--profile whistle`.
- **trunk-solver** — Plans which dongle runs what centre and rate so every channel is covered inside a margin of its band (exact search over contiguous channel groups, `--margin`, uses every dongle that helps).
- **trunk-uls** — Reads the FCC's weekly land-mobile licence file (`l_LMpriv.zip`, cached in `~/.cache/trunk-oxide/uls`, never `/tmp`) for the sites licensed around the receiver: `trunk-uls sites --home LAT,LON` lists each licensee's sites (name, coordinates, tower, ERP, channel set) against the config's P25 channels and names the licensed channels the config does not scan; `trunk-uls reach` ranks those sites by the signal that should reach the antenna over SRTM terrain (4/3 earth, Deygout knife edges, tiles cached beside the licence file). `trunk-uls occupancy` turns an `rtl_power` sweep taken on a free dongle into how often each channel is busy (scanned, licensed but unscanned, or unlicensed); weak readings next to a strong channel can be its skirts, so raise `--threshold-db` to judge them. A band-wide state licence (service SL, e.g. California's WPTZ774 for 769-775/799-805 MHz) has no per-channel rows, so an unlicensed-looking 700 MHz channel in a state like that is not unlicensed. A licence says what a site may transmit, not what it does.
- **trunk-channelizer** — Shared overlap-save FFT channelizer with per-slot conditioning (squelch, AGC, FLL), slot decoders (`ChannelDecoder`) and the wideband signal detector.
- **trunk-p25** / **trunk-fm** — P25 control/voice decoders and the analog FM path, run as slot decoders.
- **trunk-rtlsdr-sys** — librtlsdr bindings (the only other `allow(unsafe_code)` crate). **trunk-sdr** — RTL-SDR dongles (`driver = "rtlsdr"`, by serial) and IQ files (`driver = "iqfile"`) as bounded sample streams.

## Architecture

**Threading model**: Each source has a DSP thread (`source-N`) that reads its stream, runs the shared channelizer and every decoder on its slots, and sends `RadioEvent`s (trunk messages, squelch, source changes, crypto, detections, aliases) over an async channel to the monitor loop; audio goes straight to the EventBridge. The monitor loop (tokio) runs the call state machine and drives `Radio`.

**Event flow**: SDR → trunk-sdr stream → trunk-channelizer → slot decoders (trunk-p25, trunk-fm) → `RadioEvent` → call state machine → EventBridge (gRPC server) → subscribers (trunk-discord, etc.)

**Configuration hierarchy**: TOML config files with CSV overlays for talkgroups and channels. trunk-solver can auto-generate optimal SDR assignments.

**Deployment**: Docker multi-stage builds produce the services (trunk-oxide, trunk-discord, trunk-monitor, trunk-weather, trunk-whisperer) communicating via gRPC over Unix sockets. `just deploy` builds and replaces trunk-oxide, trunk-discord, trunk-monitor and trunk-weather (see the `deploy-stack` skill).

## Key Conventions

- Rust edition 2024, MSRV 1.94, resolver = "3"
- `unsafe_code = "deny"` and `unused_must_use = "deny"` workspace-wide
- clippy::all is denied; pedantic and nursery are warnings. Make a function `const fn` whenever it can be: `missing_const_for_fn` (nursery) names the ones that can, and the workspace is kept at zero of them
- The only native library is librtlsdr (hand-written extern decls in trunk-rtlsdr-sys, no bindgen)
- All gRPC services use Unix domain sockets
- Protobuf code is generated at build time in build.rs scripts
- Smartnet and DMR are not supported until their ports land (beads trunk-oxide-a2m, trunk-oxide-map)


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->
