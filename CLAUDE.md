# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

trunk-oxide is a Rust SDR (Software Defined Radio) scanner that drives trunk-recorder (a C++ library) via FFI, inverting the traditional plugin model. Rust owns configuration and orchestration; C++ handles the GNU Radio signal processing. Events flow from SDR hardware through C++ into Rust via cxx bridge callbacks, then out via gRPC to downstream consumers (Discord bot, speech-to-text service).

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

- **trunk-oxide** — Main binary. Loads TOML config, spawns a dedicated OS thread for C++ FFI (TrContext is !Send), runs tokio for gRPC/timers/signals.
- **trunk-recorder-sys** — cxx bridge to libtrunk-recorder.so. build.rs probes gnuradio/osmosdr/uhd via pkg-config and generates compatibility stubs.
- **trunk-oxide-proto** / **trunk-transcriber-proto** — Protobuf definitions compiled with tonic-prost-build. `trunk-transcriber-proto` defines a generic `Transcriber` gRPC service implemented by both speech-to-text backends.
- **trunk-oxide-grpc** — EventBridge: central hub that receives C++ events and serves them over gRPC (Unix socket).
- **trunk-config** — TOML config parser with CSV overlay support for talkgroups/channels.
- **trunk-database** — SQLite storage with hourly rotating database files and schema versioning via SHA256.
- **trunk-discord** — Discord bot that subscribes to gRPC events, posts call embeds, manages threads, encodes OGG audio.
- **trunk-whisperer** — Speech-to-text service using whisper-rs (whisper.cpp FFI). Optional CUDA feature. Serves `Transcriber`.
- **trunk-parakeet** — Speech-to-text service using parakeet-rs (NVIDIA Parakeet ASR via ONNX Runtime). Drop-in replacement for trunk-whisperer — serves the same `Transcriber` gRPC service on the same Unix socket; backends are swapped at the container/image level, not via trunk-discord config.
- **trunk-whistle** — Speech-to-text service using Cactus Whistle (16.9 MB CPU-only model) via the prebuilt `libneedle.a` C engine (FFI in `needle.rs`, the only `allow(unsafe_code)` outside the sys crates). Same `Transcriber` gRPC service/socket as the other two backends. Not in `default-members`: building needs `NEEDLE_LIB_DIR` (+ libc++ ≥ 21 static libs); use `Dockerfile.whistle` / `--profile whistle`.
- **trunk-solver** — Z3 SMT solver for optimal SDR device frequency/sample-rate/gain assignment.
- **trunk-channelizer** — Shared overlap-save FFT channelizer with per-slot conditioning (squelch, AGC, FLL), slot decoders (`ChannelDecoder`) and the wideband signal detector. The C++ `shared_channelizer` block is a thin wrapper over it.
- **trunk-p25** / **trunk-fm** — P25 control/voice decoders and the analog FM path, run as slot decoders.
- **trunk-rtlsdr-sys** — librtlsdr bindings (the only other `allow(unsafe_code)` crate). **trunk-sdr** — RTL-SDR dongles (`driver = "rtlsdr"`) and IQ files (`driver = "iqfile"`) opened by trunk-oxide itself and streamed into the flowgraph through the `iq_stream_source` block; gr-osmosdr remains for `driver = "osmosdr"`.

## Architecture

**Threading model**: A dedicated OS thread drives TrContext (C++ FFI, !Send). A separate tokio runtime handles gRPC, timers, and I/O. An async channel bridges C++ callbacks to the tokio world via EventNotifier.

**Event flow**: SDR → GNU Radio C++ graph → cxx callbacks → EventBridge (gRPC server) → subscribers (trunk-discord, etc.)

**Configuration hierarchy**: TOML config files with CSV overlays for talkgroups and channels. trunk-solver can auto-generate optimal SDR assignments.

**Deployment**: Docker multi-stage builds produce three services (trunk-oxide, trunk-discord, trunk-whisperer) communicating via gRPC over Unix sockets.

## Key Conventions

- Rust edition 2024, MSRV 1.94, resolver = "3"
- `unsafe_code = "deny"` and `unused_must_use = "deny"` workspace-wide
- clippy::all is denied; pedantic and nursery are warnings
- C++ integration uses cxx (not bindgen) with C++17
- All gRPC services use Unix domain sockets
- Protobuf code is generated at build time in build.rs scripts
- trunk-recorder is a git submodule (forked)


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
