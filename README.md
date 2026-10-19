# trunk-oxide

A Rust SDR scanner for P25 trunked and conventional analog radio systems. It opens RTL-SDR dongles with librtlsdr, channelizes each one with a shared FFT channelizer, decodes P25 control and voice channels (trunk-p25) and analog FM (trunk-fm), runs the call state machine, and publishes calls and audio over gRPC. It began as a Rust driver for [trunk-recorder](https://github.com/robotastic/trunk-recorder); the C++/GNU Radio stack has since been replaced entirely.

Recorded calls are streamed over gRPC to companion services for transcription and Discord integration.

## Services

**trunk-oxide** -- The core scanner. Reads TOML config, calibrates and opens the SDR sources, builds the radio (trunk-radio) and exposes a gRPC event stream for call data and audio.

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
max_channels = 8       # channels open at once, across every source (see below)

[[source]]
driver = "rtlsdr"      # an RTL-SDR, opened with librtlsdr ("iqfile" replays a capture)
device = "rtl=00000001" # the dongle's USB serial
center = 771496500.0
rate = 2496000.0
gain = 40.0

[system.smco]
type = "p25"
control_channels = [772031250.0, 772406250.0]
modulation = "qpsk"
talkgroup_file = "smco-talkgroups.csv"
# control_channel_priority = 0   # the control channels' claim (default 0)
# emergency_priority = 0         # an emergency's claim, if better than its talkgroup's
# default_priority = 4           # talkgroups without one (default: after every listed one)

[system.conv]
type = "conventional"
squelch = -50.0
channel_file = "conventional-channels.csv"

[grpc]
listen_path = "/run/service/grpc.sock"
```

Supported system types: `p25`, `conventional`, `conventionalP25` (Smartnet and DMR are not ported yet).

### Channels and priority

Every control channel, trunked call and conventional channel holds one channel while it is open, and there are `max_channels` of them, shared by all sources (`trunk-solver` sizes it). Each claims a channel at a priority, lower numbers first: a talkgroup's from its CSV `Priority` column or TOML overlay, a conventional channel's from its `priority`, a control channel's from `control_channel_priority`. `-1` never records. When every channel is held, a call that outranks a holder evicts it (the holder's call ends as evicted, and Discord marks it); an equal or lower claim waits for one to free, and is admitted when one does. A holder keeps its channel against an equal newcomer.

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
cargo build -p trunk-oxide
```

Building trunk-oxide natively needs librtlsdr (`librtlsdr-dev`, or just the
runtime `librtlsdr0`: the build links `librtlsdr.so.0` directly when the
development package is absent) and protobuf (`protobuf-compiler`,
`libprotobuf-dev`). Everything else is Rust.

## Architecture

```
RTL-SDR dongles (librtlsdr)        IQ captures (replay)
    |                                   |
    v  trunk-sdr: one sample stream per source
trunk-channelizer: shared FFT channelizer per source, one DSP thread each
    |
    v  slot decoders: trunk-p25 (control + voice), trunk-fm (analog)
trunk-radio: channels opened per call (P25 voice, analog FM, P25 control)
    |
    v  RadioEvents + audio
trunk-oxide: call state machine, EventBridge
    |
    v  gRPC over Unix socket
trunk-discord ------> Discord
    |
    v  gRPC over Unix socket
trunk-whisperer / trunk-parakeet ----> speech to text
```

Each source runs its own DSP thread; the call state machine runs on a tokio
monitor loop fed by an async channel of radio events.

## License

The P25 vocoders and the Motorola alias algorithm are ports of op25 and trunk-recorder code (GPL); librtlsdr is GPL-2. See their repositories for license details.
