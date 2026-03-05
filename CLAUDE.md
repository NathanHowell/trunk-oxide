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

## Workspace Structure (10 crates)

- **trunk-oxide** — Main binary. Loads TOML config, spawns a dedicated OS thread for C++ FFI (TrContext is !Send), runs tokio for gRPC/timers/signals.
- **trunk-recorder-sys** — cxx bridge to libtrunk-recorder.so. build.rs probes gnuradio/osmosdr/uhd via pkg-config and generates compatibility stubs.
- **trunk-oxide-proto** / **trunk-whisperer-proto** — Protobuf definitions compiled with tonic-prost-build.
- **trunk-oxide-grpc** — EventBridge: central hub that receives C++ events and serves them over gRPC (Unix socket).
- **trunk-config** — TOML config parser with CSV overlay support for talkgroups/channels.
- **trunk-database** — SQLite storage with hourly rotating database files and schema versioning via SHA256.
- **trunk-discord** — Discord bot that subscribes to gRPC events, posts call embeds, manages threads, encodes OGG audio.
- **trunk-whisperer** — Speech-to-text service using whisper-rs (whisper.cpp FFI). Optional CUDA feature.
- **trunk-solver** — Z3 SMT solver for optimal SDR device frequency/sample-rate/gain assignment.

## Architecture

**Threading model**: A dedicated OS thread drives TrContext (C++ FFI, !Send). A separate tokio runtime handles gRPC, timers, and I/O. An async channel bridges C++ callbacks to the tokio world via EventNotifier.

**Event flow**: SDR → GNU Radio C++ graph → cxx callbacks → EventBridge (gRPC server) → subscribers (trunk-discord, etc.)

**Configuration hierarchy**: TOML config files with CSV overlays for talkgroups and channels. trunk-solver can auto-generate optimal SDR assignments.

**Deployment**: Docker multi-stage builds produce three services (trunk-oxide, trunk-discord, trunk-whisperer) communicating via gRPC over Unix sockets.

## Key Conventions

- Rust edition 2024, MSRV 1.93, resolver = "3"
- `unsafe_code = "deny"` and `unused_must_use = "deny"` workspace-wide
- clippy::all is denied; pedantic and nursery are warnings
- C++ integration uses cxx (not bindgen) with C++17
- All gRPC services use Unix domain sockets
- Protobuf code is generated at build time in build.rs scripts
- trunk-recorder is a git submodule (forked)
