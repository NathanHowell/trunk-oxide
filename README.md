# trunk-oxide

A Rust-based SDR scanner that drives [trunk-recorder](https://github.com/robotastic/trunk-recorder) as a linked library via FFI. Instead of running trunk-recorder as a standalone executable with plugins, trunk-oxide inverts the relationship: Rust owns all configuration, orchestration, and event routing, while trunk-recorder handles GNU Radio signal processing under the hood.

Recorded calls are streamed over gRPC to companion services for transcription and Discord integration.

## Services

**trunk-oxide** -- The core scanner. Reads TOML config, initializes SDR sources and radio systems through the C++ FFI, and exposes a gRPC event stream for call data and audio.

**trunk-discord** -- Discord bot that subscribes to trunk-oxide events. Posts call recordings with metadata (talkgroup, unit IDs, duration) as embeds in organized Discord channels, with optional transcriptions from trunk-whisperer.

**trunk-whisperer** -- Speech-to-text service powered by [whisper.cpp](https://github.com/ggerganov/whisper.cpp). Transcribes call audio on demand via gRPC.

**trunk-solver** -- Offline optimization tool that uses a Z3 SMT solver to compute optimal SDR device assignments (center frequencies, sample rates, gains) to cover all configured channels with minimal hardware.

## Configuration

All services are configured with TOML files. Talkgroup and channel databases can be defined inline or loaded from CSV files (compatible with [RadioReference](https://www.radioreference.com/) exports), with TOML entries acting as overlays to selectively override CSV fields.

Example `config.toml` for trunk-oxide:

```toml
[recorder]
temp_dir = "/dev/shm"
call_timeout = 3.0
soft_vocoder = true

[[source]]
driver = "osmosdr"
device = "rtl=0"
center = 771496500.0
rate = 2496000.0
gain = 40.0
digital_recorders = 6

[system.smco]
type = "p25"
control_channels = [772031250.0, 772406250.0]
modulation = "qpsk"
talkgroup_file = "smco-talkgroups.csv"

[system.conv]
type = "conventional"
squelch = -50.0
channel_file = "conventional-channels.csv"

[grpc]
listen_path = "/run/service/grpc.sock"
```

Supported system types: `p25`, `smartnet`, `conventional`, `dmr`.

## Deployment

The system is designed to run as a set of Docker containers communicating over gRPC via Unix domain sockets:

```
docker compose up --build
```

This starts:
- **trunk-oxide** -- scanner with access to USB SDR devices
- **trunk-whisperer** -- transcription service (auto-downloads the Whisper model on first run)
- **trunk-discord** -- Discord bot (requires a token in `secrets/discord_token`)
- **Jaeger** -- distributed tracing UI at `http://localhost:16686`

Services use health checks to enforce startup ordering and report readiness.

### Environment Variables

| Variable | Description |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OpenTelemetry collector endpoint (enables tracing and metrics) |
| `RUST_LOG` | Log level filter (e.g. `info`, `debug`, `trunk_oxide=debug`) |

## Building from Source

Requires Rust 1.93+ (edition 2024).

```bash
# Most crates build without system dependencies
cargo build

# trunk-oxide requires GNU Radio, osmosdr, and the trunk-recorder shared library
# See Dockerfile.trunk-recorder for the full dependency list
cargo build -p trunk-oxide
```

Building trunk-oxide natively requires:
- GNU Radio 3.10+ development libraries
- gr-osmosdr / librtlsdr (or other SDR driver libraries)
- libfmt, libspdlog
- The trunk-recorder submodule built as a shared library (`libtrunk-recorder.so`)

The Docker build handles all of this automatically.

## Architecture

```
SDR hardware
    |
    v
GNU Radio DSP (C++, via libtrunk-recorder.so)
    |
    v  cxx FFI callbacks
trunk-oxide (Rust)
    |
    v  gRPC over Unix socket
trunk-discord ------> Discord
    |
    v  gRPC over Unix socket
trunk-whisperer ----> whisper.cpp
```

trunk-oxide runs a dedicated OS thread for C++ FFI calls (the GNU Radio context is not thread-safe) and a tokio runtime for everything else. An async channel bridges events from the C++ callback thread into the async world.

## License

This project links against trunk-recorder and GNU Radio, which are licensed under the GPL. See their respective repositories for license details.
