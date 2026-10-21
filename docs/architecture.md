# Architecture

How trunk-oxide is put together and where to change it. It covers the Rust workspace under `crates/`: the radio (sources, channelizer, P25 and analog FM decoders), the call state machine that drives it, and the services that consume its gRPC stream (Discord bot, speech-to-text backends). It does not repeat field-level detail: for configuration see the [README](../README.md#configuration) and [`config.toml.example`](../config.toml.example); for the wire schema see [`trunk_oxide.proto`](../crates/trunk-oxide-proto/proto/trunk_oxide.proto) and [`transcriber.proto`](../crates/trunk-transcriber-proto/proto/transcriber.proto); for offline replay see [`REPLAY.md`](../crates/trunk-oxide/REPLAY.md).

Two facts frame everything below. trunk-oxide writes no audio or metadata files: a call leaves the process as `AudioSample` and `CallEnd` messages on a gRPC stream, and persistence belongs to the consumers. And Smartnet, DMR and SigMF systems parse in the config but are refused at startup; the supported system types are `p25`, `conventional` (analog FM) and `conventionalP25`.

## 1. The crates

```text
 trunk-oxide (lib; bins trunk-oxide, trunk-replay)
   │  call_state.rs, allocation.rs  pure policy        call_manager.rs  executor
   ├──► trunk-radio ─────┬──► trunk-channelizer ┐
   │      sources,       ├──► trunk-p25         ├─ platform-independent DSP and protocol:
   │      Radio, kinds   ├──► trunk-fm          ┘  samples in, frames / messages / PCM out
   │                     └──► trunk-sdr ──► trunk-rtlsdr-sys ──► librtlsdr   (FFI)
   ├──► trunk-oxide-grpc (EventBridge) ──┬──► trunk-radio
   │                                     └──► trunk-oxide-proto  (trunk_oxide.proto)
   ├──► trunk-sdr
   └──► trunk-config  (TOML + CSV overlay)

 trunk-discord ──┬──► trunk-oxide-proto
                 ├──► trunk-transcriber-proto  (transcriber.proto)
                 ├──► trunk-database ──► trunk-oxide-proto      (SQLite)
                 └──► trunk-config

 trunk-whisperer | trunk-parakeet | trunk-whistle ──► trunk-transcriber-proto
 trunk-solver ──┬──► trunk-channelizer  (passband rule only)
                └──► trunk-config
 trunk-sh        no dependencies; a stand-in /bin/sh for the distroless images
```

The layering rule: `trunk-p25`, `trunk-fm` and `trunk-channelizer` take samples (or soft symbols) in and give frames, messages and PCM out. They open no files or sockets, make no decision from a clock, and `trunk-p25` and `trunk-fm` start no threads (the channelizer times its own blocks with `Instant` for `Counters`, and has a rayon pool only if `Config::threads` is non-zero; `Config::standard` leaves it 0, so everything runs on the caller). The only `OpenTelemetry` code is in `trunk-oxide` and the services. Everything with a thread, a socket or a timestamp lives above them: `trunk-sdr` owns the reader threads, `trunk-radio` owns the per-source DSP threads and stamps recordings with `pace::now`, and `trunk-oxide` decides what happens when.

The second rule: the call state machine (`crates/trunk-oxide/src/call_state.rs`, `TrunkStateMachine`) is pure. It takes messages and ticks in and returns `Effects` (a `#[must_use]` list, so `unused_must_use = "deny"` makes dropping one a compile error). `CallManager` in `call_manager.rs` is the only code that applies them to the `Radio` and the `EventBridge`. The state machine reads time only through `Clock`, so the same logic runs live on the system clock and in `trunk-replay` on a manual clock that follows the capture.

Workspace lints (`Cargo.toml`): `unsafe_code = "deny"` and `unused_must_use = "deny"` everywhere; `clippy::all` denied, `pedantic` and `nursery` warn. `#![allow(unsafe_code)]` appears in three places: `trunk-rtlsdr-sys` (hand-written `extern` declarations, no bindgen), `trunk-whistle/src/needle.rs` (the C engine) and `trunk-channelizer/src/fftw.rs` (behind the `fftw` feature, kept only so the bench can compare against rustfft). `default-members` lists the proto, gRPC, database, config, Discord, solver and `whisperer` / `parakeet` crates; `trunk-oxide`, `trunk-whistle` and `trunk-sh` are built with `-p`. The scanner needs librtlsdr at link time, and so does anything that depends on `trunk-radio`, `trunk-oxide-grpc` included.

| Crate | Main entry points |
|---|---|
| `trunk-oxide` | `ScannerBuilder::from_config` / `from_config_paced`, `Scanner::run_monitor` / `run_monitor_with` (`src/lib.rs`); `TrunkStateMachine`, `Effect`, `CallEndReason` (`call_state.rs`); `CallManager`, `process_effects` (`call_manager.rs`); `allocation::allocate` / `diff`; `Clock`; `calibrate::calibrate`, `calibrate::recheck`; `cpu_weight::require`; binary `trunk-replay` (`src/bin/trunk-replay/`) |
| `trunk-radio` | `Radio` (`open::<K>`, `close`, `rotate`, `reading`, `add_source`, `add_system`), `Kind`, `Recipe<K>`, `Opener`, `P25Voice`, `P25Control`, `AnalogFm`, `Tape`, `Hold`, `RadioEvent`, `Trunking`, `TrunkMessage`, `P25TrunkParser`, `Pacer`, `Source` |
| `trunk-channelizer` | `Config::standard`, `SharedChannelizer` (`set_slot`, `set_slot_conditioning`, `set_slot_decoder`, `run`), `ChannelDecoder`, `Channelizer`, `Conditioner`, `SignalDetector`, `passband` |
| `trunk-p25` | `ControlChannelDecoder`, `VoiceDecoder`, `TdmaDecoder`, `Demodulator`, `Framer`, `Phase1Assembler`, `Phase2Assembler`, `ImbeVocoder`, `AmbeVocoder`, `Osp`, `SystemPlans`, `Scrambler` |
| `trunk-fm` | `FmDemod`, `Detector`, `Gate`, `Denoiser` (`denoise`) |
| `trunk-sdr` | `Rtl::open` / `stream`, `DeviceSpec`, `Stream`, `stream::Producer`, `Running`, `file::play`, `streams()` |
| `trunk-rtlsdr-sys` | `Device`, `devices`, `index_by_serial` |
| `trunk-oxide-grpc` | `EventBridge`, `serve_grpc`, `AudioMeta`, `TalkgroupMeta` |
| `trunk-oxide-proto` | generated `trunk_oxide` module; `types.rs` newtypes (`Talkgroup`, `SysNum`, `CallNum`, `SourceId`, `RecorderId`, `CallRowId`, `Freq`, `SignalDb`) |
| `trunk-transcriber-proto` | generated `Transcriber` service |
| `trunk-config` | `Config::load`, `SourceConfig`, `SystemConfig`, `TalkgroupConfig`, `ChannelConfig`, `RecorderConfig`, `OxideConfig`, `AudioConfig`, `DiscordConfig` |
| `trunk-database` | `Database::open_hourly`, `insert_call`, `update_call`, `get_thread` / `set_thread` |
| `trunk-discord` | `DispatchPump`, `DispatcherState`, `CallHandler`, `ChannelWriters`, `Aggregator`, `TranscriberConnection`, `OggOpusEncoder` |
| `trunk-monitor` | `Dashboard` (refresh, recover), `StatsClient` (the chart), `Channel` (the Discord message), `health::watchdog` |
| `trunk-whisperer`, `trunk-parakeet`, `trunk-whistle` | a `Transcriber` gRPC service each, behind a `PrioritySemaphore` |
| `trunk-solver` | `solve`, `max_channels`, `SolverConfig` (binary `trunk-solver`) |
| `trunk-sh` | binary `sh` (`sh -c COMMAND` only) |

## 2. From the antenna to the wire

```text
 RTL-SDR dongle                                    IQ file (trunk-replay, tests)
   │ librtlsdr read_async, 15 x 256 KiB transfers    │
   ▼                                                 ▼
 thread "rtl-<serial>"  (trunk-sdr::rtl::reader)    thread "iqfile-<name>"  (trunk-sdr::file::player)
   skip 1st transfer, u8 -> Complex32 via Lut,       cu8 or cf32, paced to rate (or unpaced under a Pacer)
   count clipped samples, cut 32 768-sample blocks
   │ Producer::push: queue full => drop whole block   │ Producer::push_wait: queue full => wait
   ▼                                                 ▼
 ╔═ Stream queue ═══ Mutex<VecDeque<block>> + 2 Condvars; capacity max(rate x 1 s, 4 x 32 768) samples ═╗
 ╚═════════════════════════════════════════════════════════════════════════════════════════════════════╝
   │ Stream::read, waits 100 ms (READ_WAIT) per attempt, fills one `hop` of samples
   ▼
 thread "source-N"   (trunk-radio::source::Thread::run: one per source; all DSP below runs here)
   │  SharedChannelizer::run(hop)             lock order: mailbox (drained and released) then `dsp` for the whole block
   │   1. drain mailbox  <──────────────  Radio::open/close/rotate/set_source  (monitor thread, Mutex<Vec<Cmd>>)
   │   2. SignalDetector on raw input (only after Radio::detect_signals)
   │   3. one forward FFT of n samples (overlap-save), shared by every slot
   │   4. per active slot, in slot order: bin shift + channel filter + k-point inverse FFT
   │        -> Conditioner (resample, power squelch, AGC, band-edge FLL) -> Reading {power_db, freq_error_hz, squelch_open}
   │        -> ChannelDecoder::process(iq, reading, &mut Decoded)
   │             ┌───────────────────────┬─────────────────────────────────────┬────────────────────────────────────┐
   │             │ P25Control            │ P25VoiceChannel                     │ AnalogFmChannel                    │
   │             │ profile NARROW 24 kS/s│ profile NARROW 24 kS/s              │ profile WIDE -> 96 kS/s            │
   │             │ ControlChannelDecoder │ phase 1: VoiceDecoder + Phase1Assembler│ FmDemod -> Detector -> Gate       │
   │             │ P25TrunkParser        │ phase 2: TdmaDecoder + Phase2Assembler│ [-> Denoiser] -> 16 kHz PCM       │
   │             │                       │ ImbeVocoder / AmbeVocoder -> 8 kHz PCM│                                   │
   │             │ no audio              │ Tape (Recording + Hold)  [Carrier squelch on conventionalP25]             │
   │             └───────────┬───────────┴───────────────┬─────────────────────┴──────────────┬─────────────────────┘
   │                         │ events                    │ audio + events                     │ audio + events
   │                         ▼                           ▼                                    ▼
   │                      Decoded { audio: Vec<(CallId, Vec<i16>)>, events: Vec<RadioEvent> }
   │   5. Thread::route:  audio -> AudioSink::audio (EventBridge::audio_stream)      events -> EventSender::send (try_send)
   │                      detections -> RadioEvent::SignalsDetected
   ▼                                  │                                                  │
                                      │                                                  ▼
                                      │                          tokio mpsc<RadioEvent>, 256 (EVENT_QUEUE); full => event dropped, warn
                                      │                                                  │
                                      │                                                  ▼
                                      │       thread "monitor": runtime.block_on(Scanner::run_monitor_with)  -- select! over
                                      │         ctrl_c | shutdown Notify | event_rx.recv() | 1 s tick | 200 s status
                                      │         TrunkMessages -> control_heard -> TimedDedup (500 ms) -> bridge.trunk_message
                                      │                       -> CallManager::handle_message -> TrunkStateMachine::on_message
                                      │         Squelch / SourceChange / Crypto / SignalsDetected / UnitAlias -> state machine or bridge
                                      │                       -> Effects -> CallManager::process_effects
                                      │                          Open/Close/Rotate/SetSource -> Radio (Mutex<Inner>) -> mailbox
                                      │                          EmitCallStart/End/Signal/CallsActive/UnitEvent/SetupSystem -> EventBridge
                                      ▼                                                  │
 ╔═ EventBridge ═══ tokio broadcast::channel<StreamMessage>(4096); audio_meta RwLock<HashMap<CallId, AudioMeta>> ═╗
 ╚═══════════════════════════════════════════════════════════════════════════════════════════════════════════════╝
   │ per subscriber: task + mpsc(4096) -> tonic server stream `TrunkOxide.Subscribe`, Unix socket (listen_path)
   │ a subscriber that lags the broadcast skips messages (warn "subscriber lagged"); no subscribers => dropped
   ▼
 trunk-discord  ──► DispatchPump -> per-call CallHandler (mpsc 1024) -> resample to 16 kHz -> Transcriber gRPC (Unix socket)
   │                                                                   -> OggOpusEncoder -> ChannelWriters (Discord REST) + SQLite
   └──► trunk-whisperer | trunk-parakeet | trunk-whistle  (one of them owns the socket)
```

Three kinds of channel hang off the same slots, reached by four routes; the choice is made once at config parse as a `Recipe<K>` behind `Arc<dyn Opener>` (`ScannerBuilder::from_config_paced`):

| Branch | Where it comes from | `Kind` / `Params` |
|---|---|---|
| control channel | each `control_channels` entry of a `p25` system | `P25Control` / `()` |
| trunked voice | a grant on a `p25` system; `AnalogFm` when the talkgroup's `mode` is `A` (or `default_mode = "analog"` for unlisted talkgroups) | `P25Voice` / `Keying::Trunked`, or `AnalogFm` / `AnalogParams::default()` |
| conventional analog | a `conventional` system's `channel` rows | `AnalogFm` / `AnalogParams::with_tone_hz(ch.tone)` |
| conventional P25 | a `conventionalP25` system's `channel` rows | `P25Voice` / `Keying::Carrier { squelch_db }` |

### 2.1 Sources

`trunk-sdr` supports two sources: `driver = "rtlsdr"` (an RTL-SDR dongle found by USB serial through librtlsdr, `rtl::Rtl`) and `driver = "iqfile"` (`file::play`, formats `cu8` and `cf32`, optionally looping). The config enum `SourceDriver` also lists `osmosdr` and `usrp`; `ScannerBuilder::from_config_paced` rejects both with an error. There is no driver trait: the seam is `Stream` (the consumer's end of a queue) plus `Running` (`stop()`, kept alive by the radio), and `Source` in `trunk-radio` accepts any pair.

Samples are `Complex32`. The dongle's unsigned 8-bit I/Q goes through `convert::Lut` (`(u - 127.4) / 128`, the gr-osmosdr scale, so thresholds tuned against the old GNU Radio source still hold). `rtl.rs` mirrors gr-osmosdr's set-up: manual tuner gain with the RTL2832U AGC off (`Gain::Manual`; `Gain::Auto` when the source sets `agc = true`), the first USB transfer discarded (`BUF_SKIP`), `BUF_NUM` = 15 transfers of `BUF_LEN` = 256 KiB in flight.

Back-pressure is in `trunk-sdr/src/stream.rs`. A dongle's reader thread calls `Producer::push`, which drops the whole block and counts it (`StreamStats::dropped`, `drops`) when the queue would pass its capacity; the reader never blocks, because librtlsdr's callback must return. The capacity is `max(rate x STREAM_QUEUE_SECS, 4 x STREAM_BLOCK)` samples with `STREAM_BLOCK` = 32 768 and `STREAM_QUEUE_SECS` = 1.0 (`crates/trunk-oxide/src/lib.rs`). A file player calls `push_wait` and sleeps until there is room, so a replay loses nothing. A drop is not repaired: the DSP thread sees a gap as a splice, and the only visible trace is the `trunk.oxide.source.dropped_samples` and `trunk.oxide.source.drops` counters (polled each tick by `CallManager::poll_sources`). `clipped` counts samples with I or Q on an ADC rail.

There is no reconnect or back-off in the scanner. A reader that ends (`producer.end()`) lets the DSP thread fall out of its loop when the queue drains; the monitor's 1 s tick calls `Radio::check_sources`, which fails if a source delivered no samples since the previous check, and `on_tick` then calls `std::process::exit(1)` so the container's restart policy (`restart: unless-stopped` in `docker-compose.yml`) brings everything back, gain calibration included. `trunk-replay` turns the check off (`MonitorOptions::check_sources`) because a finite file ends. (`trunk-discord` is the component that reconnects with back-off; see section 8.)

Before any source streams, `calibrate_sources_parallel` (in `lib.rs`) opens every `gain = "auto"` dongle on its own thread, sweeps the tuner's gain steps and picks one (`calibrate::calibrate`; model in `calibrate/select.rs`: a fit of the ADC-plus-front-end noise floor, pick the lowest gain whose ADC penalty is within `CalibrationPolicy::penalty_budget_db`, vetoing gains that clip). The last pick is cached per device in `<state_dir>/gain-calibration.json` (`calibrate::cache`, `ALGORITHM` = 3) and re-checked on the next start (`calibrate::recheck`) instead of sweeping again. If calibration fails the source falls back to the cached gain, else `DEFAULT_GAIN_DB` = 40.2.

### 2.2 Channelization

`trunk-channelizer` is a shared overlap-save FFT channelizer (a Borgerding multiband filter bank, as in ka9q-radio). One forward FFT of `n` points is shared by every channel of a source. Each channel (a slot) selects the bins around its centre, multiplies by its filter's frequency response, and runs a small inverse FFT of `k = n / decim` points; the first `overlap_in / decim` outputs of each block are time-aliased and discarded, so each block yields `hop / decim` samples per channel. A slot's residual offset from its bin centre is removed by a mixer at the output rate.

`Config::standard(rate)` picks the geometry: two profiles on one FFT, `NARROW` (`ProfileSpec::NARROW`: 12.5 kHz channel, 24 kS/s target, filter cutoff 6.25 kHz, transition 3.125 kHz, 60 dB) for P25, and `WIDE` (`ProfileSpec::WIDE`: 96 kS/s target up to 288 kS/s, cutoff 6 kHz, transition 3 kHz) for analog FM. Each profile's decimation is an integer giving a rate between its target and its maximum (`candidate_decims`); the combination and FFT size are the cheapest per input sample (`fft_cost`, `plan`): a multiple of the profiles' common multiple that gives the narrow profile `k >= 256` and leaves room in the overlap for every channel filter's impulse response. If the source rate is too low for the wide profile, `standard` falls back to `p25` (narrow only), and `Source::offers` then refuses `AnalogFm` channels there. Worked values (from the unit test `standard_keeps_the_narrow_geometry_and_adds_a_wide_profile`):

| Source rate | `n` | `hop` | Narrow `k` | Wide `k` @ rate |
|---|---|---|---|---|
| 2 496 000 S/s | 26 624 | 19 968 (8 ms) | 256 | 1 024 @ 96 000 |
| 2 400 000 S/s | 25 600 | 19 200 | 256 | 1 024 @ 96 000 |
| 1 872 000 S/s | 19 968 | 14 976 | 256 | 1 536 @ 144 000 |
| 912 000 S/s | 10 944 | 8 208 | 288 | 1 216 @ 101 333 |

The source thread feeds the channelizer exactly `hop` samples at a time, so a block's real-time budget is `hop / sample_rate`; `Counters::overruns` counts blocks that took longer (`block_budget_ns`). There is no pre-roll: a slot enabled by `set_slot` is valid from the first block after the mailbox drain, and a decoder sees nothing of the audio before the block in which its channel opened. The slot's conditioner survives release (`release_slot` keeps it) so a channel reopened on the same kind carries its AGC and FLL on.

What reuses the channelizer's output or input:

| Consumer | What it reads | Where |
|---|---|---|
| `Conditioner` | per-slot power probe (smoothed power), power squelch (`SquelchMode::ANALOG` or `DIGITAL`), RMS AGC (`agc_alpha` 0.45, `agc_reference` 0.85), band-edge FLL; port of the GNU Radio chain | `conditioner.rs` |
| `Reading` | `power_db`, `freq_error_hz` (FLL, scaled to 24 kHz as `xlat_channelizer` did), `squelch_open` | `service.rs`, `SlotStats` |
| `SignalDetector` | an FFT of 1 024 contiguous raw samples every 0.1 s, threshold found per spectrum; makes a `signal_detection` conventional channel wake | `detector.rs`, `Radio::detect_signals` |
| Clock correction | FLL errors of closed channels steer the source's software offset (`Correction`: `ALPHA` 0.2, `GAIN` 0.3, `DEADBAND_HZ` 30, `MAX_STEP_HZ` 50, `MAX_CORRECTION_HZ` 5 000), when `adaptive_freq_correction` is set | `trunk-radio/src/correction.rs`, `Inner::track` |
| Live noise floor | a raw snapshot of every source every `calibrate::live::EVERY` = 60 s, measured in `trunk-oxide` as the gain sweep measures | `trunk-radio/src/snapshot.rs`, `calibrate/live.rs` |
| Placement | `passband::USABLE` = 0.85 of Nyquist; `Inner::deepest` (in `radio.rs`) picks the covering source a channel sits deepest in; `warn_on_placement` warns at startup; `trunk-solver` plans with the same constant | `passband.rs` |

There is no waterfall or spectrum output in the process.

### 2.3 Receivers

All of a source's decoders run on its `source-N` thread, called by `SharedChannelizer::run_block` with the slot's conditioned IQ.

| Receiver | Modulation | Symbol rate | Used for | Notes |
|---|---|---|---|---|
| `ControlChannelDecoder` (`trunk-p25/src/control.rs`) | C4FM or CQPSK (`Modulation`, per system `modulation = "qpsk"` or `"fsk4"`) | 4800 baud, 5 samples/symbol at 24 kS/s | P25 phase 1 control channels (`P25Control`) | `Demodulator` + `Framer` + soft Viterbi trellis; a block that fails its CRC gets a list decode (`list_size` 8, `list_max_extra` 12.0) |
| `VoiceDecoder` (`trunk-p25/src/voice/mod.rs`) | C4FM or CQPSK | 4800 baud | P25 phase 1 voice | same `Demodulator`; configured from the system's `modulation` |
| `TdmaDecoder` (`trunk-p25/src/tdma/mod.rs`) | H-DQPSK, read as CQPSK (`DemodConfig::phase2`) | 6000 baud, 4 samples/symbol | P25 phase 2 voice (`slot` set on the channel) | `BurstFramer` flywheel; needs the system's `Scrambler` (WACN, system ID, NAC) or opening fails with `OpenError::NoScramblingMask` |
| `FmDemod` + `Detector` + `Gate` (`trunk-fm`) | narrowband FM | n/a | analog conventional and analog talkgroups (`AnalogFm`) | quadrature demod, 75 us de-emphasis, decimate to 16 kHz, 300-3250 Hz band-pass; carrier detect by noise "quieting" above the voice band; optional CTCSS tone; optional WebRTC noise suppression |

Tricks in `Demodulator` (`trunk-p25/src/demod.rs`): Gardner timing recovery with a proportional-integral loop (damping 0.707, `timing_bandwidth` 0.0025) and four-point cubic interpolation; for CQPSK the detector runs on complex samples, for C4FM on the per-symbol phase change; differential detection in deviation units (+-1, +-3, one unit = pi/4); optional decision-feedback reference (`reference_memory`, 0 by default, so plain differential detection is what runs); a decision-directed tracker removes the constant phase bias a residual frequency offset leaves (`bias_gain` 0.02, never past pi/8); a CQPSK-only receive low-pass (`RECEIVE_CUTOFF_HZ` = 5 000, `RECEIVE_TRANSITION_HZ` = 2 000). Soft symbols are kept all the way into the error-correcting decoders (the framers, trellis, Golay/Hamming/Reed-Solomon all take reliabilities); the older op25 chain made hard decisions earlier. There is no diversity combining and no adaptive equaliser.

### 2.4 Framers and decoders

| Protocol / channel | Framing unit | Decoding steps |
|---|---|---|
| P25 phase 1 control (`control.rs`, `framer.rs`, `trellis.rs`) | frame sync (48 bits, `FRAME_SYNC`, <= 6 bit errors by search, <= 12 where the previous frame predicts it) then NID; data units `Duid::Tsdu` (TSBK) and `Duid::Pdu` (multi-block trunking, MBT) | NID: hard BCH(63,16,23) (`bch.rs`) until two agreeing NIDs teach the NAC, then maximum-likelihood against the seven valid codewords for that NAC; status symbols removed (every 36th dibit); 98-dibit blocks de-interleaved and soft-Viterbi decoded (rate-1/2 trellis); CRC-16 on a TSBK/header (`crc.rs`), CRC-32 on an MBT; list decode on failure; `Osp::from_tsbk` / `from_mbt` give a typed packet |
| P25 phase 1 voice (`voice/`) | HDU (396 dibits), LDU1 and LDU2 (864 dibits), TDU, TDULC | IMBE: Golay(23,12) and Hamming(15,11) vectors, u0 chosen jointly with the runner-up; link control (LDU1, TDULC), encryption sync (LDU2) and header are Reed-Solomon words over GF(64) (`fec/rs.rs`) with the inner symbols' margins as erasure reliabilities; `EncryptionTracker` confirms an ALGID only when the same ALGID/key ID decodes twice running; no decryption (an encrypted frame plays as silence, `Vocode::Silence`) |
| P25 phase 2 voice (`tdma/`) | 180-dibit burst, 12 per 360 ms superframe, two logical channels; 40-bit ISCH ahead of each burst | ISCH maximum-likelihood against the words the superframe position allows (`isch.rs`); DUID (8,4,4) over the burst types a voice channel uses (`duid.rs`); payload descrambled with the system's 4320-bit sequence (`scramble.rs`: x^44 LFSR seeded from WACN, system ID and NAC); AMBE+2 voice codewords (`ambe.rs`: extended Golay(24,12), Golay(23,12), 49 bits in 72, 18 x 4 interleaver); FACCH, SACCH and ESS are shortened (FACCH and SACCH also punctured) RS(63,35,29) words, MAC PDUs end in a CRC-12 (`mac.rs`) |
| Vocoders (`vocoder/`) | one 20 ms frame = 160 samples at 8 kHz | op25's `software_imbe_decoder` synthesiser and mbelib's AMBE+2 dequantiser, ported operation for operation; repeat and mute rules per BABA/BABA-1 on the bits the decoder corrected; a muted frame plays as `ComfortNoise` |
| Analog FM (`trunk-fm`) | 100 ms hops of demodulated audio | `Measurement` (quieting, CTCSS) -> `Gate` (attack/release over hops, a delay line so the opening hop is not lost) -> optional `Denoiser` |
| Control messages -> trunking (`trunk-radio/src/p25_parse.rs`) | one decoded `Message` | `P25TrunkParser::parse_message` gives `Vec<TrunkMessage>`; channel numbers resolve through the system's shared `SystemPlans` (IDEN_UP, IDEN_UP_VU, IDEN_UP_TDMA); a grant on a channel no plan covers yet is deferred and re-sent when the plan arrives |

Not in the live pipeline: `voice::regenerate` and `tdma::regenerate` (`TdmaRegenerator`) re-encode a corrected channel for op25's decoder and are exercised only by tests and the `tdma_golden` example; the live code uses only `tdma::op25_view`, `tdma::regenerate::verified` and `voice::regenerate::SpikeDetector`, so the vocoders see what op25's would. TDMA control channels (LCCH bursts) are read as `BurstContent::Unread`; only FDMA control channels are followed. Over-the-air unit aliases (Motorola, Harris) arrive as `VoiceTagKind::Alias` and are decoded in `trunk-radio/src/alias.rs`.

### 2.5 Trunking and call management

The engine is `CallManager` (executor) around `TrunkStateMachine` (policy). The state machine owns:

| Field (`TrunkStateMachine`) | What it is |
|---|---|
| `calls: Vec<CallEntry>` | every call, trunked or conventional, in any `CallState` (`Recording`, `Idle`, `Disabled`, `Waiting`, `Rejected(RejectReason)`) |
| `controls`, `hunts` | each control channel's standing claim (`ControlChannel`) and each trunked system's `ControlHunt` |
| `wants`, `held` | who wants one of the `max_channels` channels, and who has one |
| `patch_tables` | talkgroups patched into supergroups, per P25 system (`PatchTable`; entries stale after 10 s) |
| `system_identities` | per system `sys_id`, `wacn`, `nac`, `rfss`, `site_id` as learned from `Trunking::Site` and `Trunking::Network` |
| `known_alg_ids` | last ESS ALGID per `(sys_num, talkgroup)` |
| `config: CallManagerConfig` | `call_timeout`, `new_call_from_update`, `record_uu_v_calls`, `max_channels`, `systems: HashMap<i32, SystemPolicy>` |

The hierarchy is engine, system, call, transmission. There is no site object: one `[system.<short_name>]` is one site's control channels plus its `SystemIdentity`. Several configured systems that share talkgroups across sites are tied together by `multi_site` (below). A call is one channel holding an ordered list of transmissions, one continuously keyed speaker each; a transmission boundary is a `CallEnd` / `CallStart` pair on the wire (`Cut::Speaker`, `Cut::KeyUp`, `Cut::Carrier`) with the channel and its recording running on (trunked) or rotating (conventional, via `Radio::rotate`).

A grant becomes a call as follows (`on_message` -> `on_grant`):

1. The control channel decoder emits `RadioEvent::TrunkMessages { control_hz, messages }`. The monitor first calls `control_heard` (so each control channel is judged on what it decoded itself), then drops copies with `TimedDedup` (500 ms; keyed by `TrunkMsgKey`: kind, opcode, channel, talkgroup, unit, system). Only the first admission starts the window, so a control channel's own repeats, a grant update about once a second, still pass. The survivors are published as a `TrunkMessageEvent` per system, then fed to `CallManager::handle_message`.
2. `Radio::update_sysid` / `update_status` note a site and a system identity once each (the latter sets the phase 2 scrambler).
3. `end_reassigned_calls` ends calls whose channel (and TDMA slot) a grant gives to another talkgroup, unless the two are patched together (`CallEndReason::ChannelReassigned`).
4. A talkgroup has one call per system. A grant for an existing call is a source update, a new key-up (`MIN_TRANSMISSION` = 700 ms floor; another handset named, or the voice channel ended a transmission since: `ptt_ended`, set by `note_ptt_ends` from the terminator counts), or a retune to another channel or slot (`retune`).
5. A new call is checked for multi-site duplication (`check_multisite`): another system's recording call on the same talkgroup, with `multi_site` set and a known WACN, makes this one a duplicate when the WACN matches and the RFSS/site differs (or, if `multi_site_system_name` is set, when the names match). A duplicate is `Rejected(Duplicate)`, unless it supersedes the original: its `nac` equals its own `rfss * 10000 + site_id` while the original's site is not the talkgroup's `preferred_nac`; the original then ends as `Superseded`.
6. `assess` admits or rejects it: unknown talkgroup unless `record_unknown` or patched with a known one (`UnknownTg`), encrypted unless `monitor_encrypted` (`Encrypted`), priority `-1` (`IgnoredTg`). The claim's priority is the talkgroup's (or `default_priority`), lowered to the best of its patch members and to `emergency_priority` for an emergency; its `recipe` is the talkgroup's, else the system's `default_recipe`.
7. `claim` registers a `Want`. `settle` runs `allocation::allocate` and `diff` and turns the difference into `Effect::Close` and `Effect::Open`. A call that loses its place is evicted (`evict`, `CallEndReason::Evicted`); one that enters is admitted (`admit`).
8. `CallManager::exec_open` takes the call's `Opener` and calls `Radio::open::<K>`: pick the covering source the channel sits deepest in whose channelizer offers `K::PROFILE`, take a free slot (one that last held the same kind first), build the decoder with a `Tape`, configure the slot (`set_slot_conditioning`, `set_slot_filter`, `set_slot_squelch_db`, `set_slot_decoder`, `set_slot`), all through the mailbox. The decoder runs from the next block. The result goes back to the state machine as `on_opened`; a failure (`OpenError`) becomes `Rejected(NoSource)` or `Rejected(NoChannel)`.
9. `EmitCallStart` opens the call's tracing span, serialises its W3C `traceparent` into `CallInfo.traceparent` and publishes `CallStartEvent`.

`allocation::allocate` is one pure function: sort wants by lower `Priority` first, then holders before newcomers (so nothing is evicted for an equal), then the longest-waiting, then the lowest `CallId`; the first `max_channels` win. Because a holder only gains rank and a newcomer only loses it, allocating again from the result changes nothing, and one want added, removed or changed moves at most one call in and one out. Control channels, trunked calls and conventional channels all compete in it.

Control-channel handling per system type:

| System type | Control channel behaviour |
|---|---|
| `p25` | `ControlHunt`: every configured `control_channels` entry is opened as a candidate. When a candidate decodes `ACQUIRE_MESSAGES` = 5 in one tick it becomes `live` and the others are closed; when the live one decodes nothing for `LOST_TICKS` = 3 ticks, every candidate (configured plus announced via `Trunking::ControlChannel` / `Network`) is opened again. A candidate no source covers is dropped from later hunts (`unreachable`). The control channel's claim is `control_channel_priority`. |
| `conventional` | none; each `channel` row is a standing claim at its own priority, keyed by the transmission detector: trunk-fm's `Gate`, reported as `RadioEvent::Squelch`. With `signal_detection` the claim exists only after the wideband detector has seen a signal within 30 kHz (`on_signals_detected`). |
| `conventionalP25` | the same, with `Keying::Carrier { squelch_db }`: the conditioner's power squelch, held `HOLD_S` = 0.25 s (`carrier.rs`) before a transmission is called over. |
| `smartnet`, `conventionalDMR`, `conventionalSIGMF` | parse, then refused in `from_config_paced` ("not supported by the Rust radio"). |

Order in which a block is processed on the source thread: mailbox commands, signal detector, forward FFT, then each active slot in ascending slot order (conditioner, then decoder), then `route`: all audio goes to the sink first, then events in decode order, then detections. On the monitor thread each event is handled to completion, effects included, before the next is taken. A conventional transmission's life is `Squelch(Open)` -> `Recording` -> `Squelch(Close)` -> `Cut::Carrier` (`Effect::Rotate`, `EmitCallEnd`, `NextTransmission`); a trunked call's is grant -> `Recording` -> updates refresh `last_update` -> a timeout, a `ChannelReassigned`, an eviction or shutdown.

### 2.6 Time

Three clocks, kept apart.

- **Sample clock.** The DSP threads count samples, not time. Audio duration is sample count over the audio rate (8 kHz P25, 16 kHz analog). Nothing resynchronises after a dropped block or a USB hiccup; a drop shortens the audio and the counters above are the only trace. `Counters::overruns` says when the channelizer itself could not keep up.
- **Monotonic clock for decisions.** The state machine and executor read `Clock::now()` (`crates/trunk-oxide/src/clock.rs`), a monotonic `Instant`. Recordings stamp `crate::pace::now()` (`trunk-radio`), the same monotonic clock live. Timeouts are in this domain: `call_timeout` (default 3.0 s) ages a trunked call since the control channel last mentioned it (`GrantTimeout` when it was recording, `IdleTimeout` otherwise, in `TrunkStateMachine::tick`) and, separately, since its channel last wrote audio (`WriteTimeout`, checked from the radio in `CallManager::tick`). A conventional channel idles out after `call_timeout`, past `max_duration`, or when it has been `Recording` with no squelch close for `STUCK_RECORDING_BACKSTOP` = 30 s.
- **Wall clock at the edge.** `Clock::wall(Instant)` maps an instant to wall time through one anchor taken when the clock was made, so reported times never step with NTP. Wall time appears only in `CallInfo.start_time` (a `Timestamp`), `CallData.start_time` / `stop_time` (Unix seconds), `CallSegment.start_time` and `Transmission.start_time` / `stop_time`.

A source that goes quiet is detected by the 1 s tick (process exit, section 2.1). A control channel that goes quiet is detected by the hunt (3 ticks). A voice channel that goes quiet ends its call by `WriteTimeout` or `GrantTimeout`. A replay faster than real time swaps the clock: `Clock::manual` plus a `Pacer` (`trunk-radio/src/pace.rs`) that runs one source block at a time in sample-time order, with the monitor moving the clock to the capture time (`RadioEvent::Paced`) before the next block; `pace::now` / `wall_now` then follow capture time as well.

### 2.7 Output

Everything the process publishes goes through `EventBridge` (`trunk-oxide-grpc`) onto one stream of `StreamMessage`:

| Message | Sent by | When |
|---|---|---|
| `AudioSample` | `EventBridge::audio_stream`, from the source thread (through `AudioSink`), and from `Radio::close` / `rotate` for a held tail | per decoded block, once the transmission has proved audible; `int16` LE PCM at `sample_rate` (8000 for `P25Voice`, 16000 for `AnalogFm`) |
| `CallStartEvent` | `Effect::EmitCallStart` | a call starts, and at every transmission boundary of a trunked call |
| `CallEndEvent` (`CallData`, `CallEndReason`) | `Effect::EmitCallEnd` | a call or one of its transmissions ends; always after the last `AudioSample` of that transmission |
| `CallsActiveEvent` | `Effect::EmitCallsActive` | the set of calls changed |
| `SignalEvent` | `Effect::EmitSignal` | a unit started talking (`unit_alias` filled if learned) |
| `TrunkMessageEvent` | the monitor loop | every non-duplicate control message batch, per system |
| `UnitEvent` | `Effect::EmitUnitEvent` | registration, deregistration, affiliation, acknowledge, location, data grant, answer request |
| `SetupSystemEvent`, `SetupConfigEvent` | `bridge.setup_system`, `setup_config` | startup, and when the first site and network status messages arrive (later repeats stop in `handle_message`) |
| `SetupRecorderEvent` | `Effect::Close` | a channel closed, with the FLL error it closed on |
| `UnitAliasEvent` | `RadioEvent::UnitAlias` | an over-the-air alias was decoded |

A finished call is a `CallData`. It carries `talkgroup`, `patched_talkgroups`, `call_num`, `freq`, `channel`, `signal`, `noise`, `start_time`, `stop_time` (Unix seconds), `encrypted`, `emergency`, `priority`, `mode`, `duplex`, `short_name`, `kind`, `tdma_slot`, `phase2_tdma`, `is_conventional`, `length` (recording seconds), `error_count`, `spike_count` (the decoder's errored and spiked voice codewords, for this transmission's slice), `segments` (`CallSegment`: frequency, start, duration, channel), and, on a call's final end only, `freq_error` and `transmissions` (`Transmission`: the audio the recordings actually delivered). A call that never had a channel (rejected, or still waiting) ends with a bare `CallData` and no `length`. `Transmission` has per-recording `sample_count`, `spike_count`, `error_count`; the end-of-transmission cause counts (`Ended`: `tdu`, `tdulc`, `end_ptt`, `hangtime`) go to metrics and the call span, not to the wire.

There is no trimming, loudness normalisation or file naming. The audio is what the decoder delivers, multiplied by the system's `digital_levels` or `analog_levels`, and gated by `audible::Hold`: a transmission's audio is held until some 0.5 s window reaches RMS 0.005 (about -46 dBFS, `AUDIBLE_RMS`, `AUDIBLE_WINDOW_S`), then everything held is released; a transmission that ends without such a window (a final partial window can still qualify it) is dropped at the source and logged ("dropping a transmission with no audible audio"). The recording's `Accounting` tracks `last_write` and `last_voice` (a sample at or above `VOICE_THRESHOLD` = 16) for the timeouts and the stop-age metrics.

### 2.8 Adding a protocol

The most recently added receiver is analog FM (`AnalogFm`, `trunk-fm`, by git history); use it as the model. Steps, in dependency order:

1. **Decoder crate.** Put the DSP in a crate with no I/O and no threads (as `trunk-fm`): input is the slot's conditioned `Complex32` at a fixed rate, output is PCM or messages. Add it to `[workspace.members]` and `[workspace.dependencies]` in the root `Cargo.toml`, and a dependency edge from `trunk-radio`.
2. **Channelizer profile.** If the rate or filter does not fit `NARROW` or `WIDE`, add a `ProfileSpec` in `crates/trunk-channelizer/src/config.rs` and extend `Config::standard`. `Source::offers(profile)` and `Config::with_profiles` already pick geometry per source; also check `passband::USABLE` and `CHANNEL_HALF_WIDTH_HZ` still describe the new channel width, since `trunk-solver` plans with them.
3. **Kind.** Add `crates/trunk-radio/src/<name>.rs` with an empty `enum` implementing `Kind` (`Params`, `Decoder`, `NAME`, `PROFILE`, `AUDIO_RATE`, `tuning`, `decoder`, optionally `finished`), a `ChannelDecoder<Output = Decoded, Command = Cmd>` for the decoder, and re-export it from `crates/trunk-radio/src/lib.rs`. `Radio` needs no change: `Radio::open::<K>` is generic.
4. **Counters.** If the decoder has metrics, register them through `trunk-radio/src/registry.rs` (as `fm.rs` does) and export them in `crates/trunk-oxide/src/<name>_metrics.rs`, polled from `CallManager::tick`.
5. **Config.** Add a `SystemType` variant or a `TalkgroupMode` and any per-system options in `crates/trunk-config/src/system.rs`; if the solver needs to size it, handle the type in `crates/trunk-solver/src/config.rs` (`SolverConfig::channels`, `Signal`).
6. **Recipe.** In `ScannerBuilder::from_config_paced` (`crates/trunk-oxide/src/lib.rs`) accept the type in the `matches!(sys.system_type, ...)` guard and build `Recipe::<NewKind>::opener(params)` into `TalkgroupPolicy::recipe`, `ConventionalChannel::recipe`, or `SystemPolicy::control_recipe` / `default_recipe`. Extend `SystemSetup` if the decoder needs system data.
7. **Wire.** Map the type in `proto_system_type` (`call_manager.rs`); `Kind::NAME` already travels as `CallInfo.kind` and `CallData.kind`. A new proto field or enum value goes in `trunk_oxide.proto` (never reuse a field number; see `reserved`), and the consumers are updated in the same change.
8. **Consumers.** `trunk-discord` keys on `short_name` and `talkgroup`; check `call_handler.rs` for anything keyed on `kind` or the sample rate (`CallMessage::Audio` carries `sample_rate`; the transcription `Resampler` takes it as input).
9. **Tests.** The decoder gets unit tests and, where a reference exists, a golden test or example (`crates/trunk-p25/tests/golden.rs`, `examples/*_golden.rs`); `Radio` has a test that opens one of each kind (`crates/trunk-radio/src/radio.rs`, `tests`).

What does not change: the state machine and `allocation.rs` (they see `Arc<dyn Opener>` and an `audio_rate`), `Radio::open` / `close` / `rotate`, `EventBridge`, the gRPC service, the transcribers.

## 3. The orchestration layer

`crates/trunk-oxide/src/lib.rs` is the layer between the radio and the process. `ScannerBuilder::from_config` builds a `Radio` and does, in order: read `[recorder] max_channels` (required); calibrate gains; open and add each source; `warn_on_placement`; add systems and their policies (systems are numbered in `short_name` order); publish `SystemInfo` for each system to the bridge. `start()` yields a `Scanner`, whose `run_monitor_with(MonitorOptions)` is the loop: it creates the `CallManager`, opens the control channels (`CallManager::start`), adds each conventional channel's call (`add_conventional_call`), then selects over the events below. `Scanner` stops the radio on drop.

The monitor owns two timers. A 1 s `TICK` runs `on_tick`: `CallManager::tick` (poll source and channelizer counters, the decoders' registries; time out write-stalled calls; `TrunkStateMachine::tick`), `clear_stale_patches`, and the source check. A 200 s `STATUS` prints call, patch and channel tables. In a paced replay both fall due on capture time instead.

It consumes `RadioEvent` (section 2) and emits through `EventBridge`. Outbound traffic is not per-client: the bridge has one broadcast channel and every subscriber sees every message (section 7). The persistence hooks of this layer are small: the gain-calibration cache and the live noise floor (`calibrate::report`, read by metric callbacks). `ScannerBuilder::with_clock` and `from_config_paced` are the seams `trunk-replay` uses; `MonitorOptions::shutdown` is how it stops the loop.

## 4. Metrics and traces

```text
  source threads / monitor thread
     │ atomics, Mutex<snapshot>, registries              (never an OpenTelemetry call from the DSP path)
     ▼
  CallManager::tick (1 s)  poll_sources, poll_channelizers, P25Metrics::poll, FmMetrics::poll
  OTel observable gauges in main.rs (event queue depth, broadcast utilization, CPU from /proc,
     conventional channels, calibration/live floor)
     │ OTLP/HTTP, PeriodicReader        OTLP/HTTP spans, batch exporter
     ▼                                          ▼
  VictoriaMetrics  ◄── trunk-monitor chart (`[monitor] victoriametrics_url`; PNG to #monitoring)      Jaeger
```

Exporters turn on from the environment: `OTEL_EXPORTER_OTLP_ENDPOINT` enables traces and metrics; `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` metrics alone (`main.rs`). With neither, instruments are no-ops. The scanner keeps no history; the only stores are VictoriaMetrics and Jaeger. Metric names are `trunk.oxide.*` (`calls_started`, `calls_ended`, `channelizer.*`, `source.*`, `channel_waits`, `channel_evictions`, `channels_held`, `event_channel.depth`, `broadcast.utilization`, `conventional.*`, plus the `p25` and `fm` decoders'). Each call has a span opened at `EmitCallStart` and closed in `exec_conclude_call` with the stop reason, audio and voice age, channelizer overruns during the call, and decoder counts; its `traceparent` rides `CallInfo` so `trunk-discord` and the transcriber join one trace. `trunk-discord`'s `reactions.rs` closes the loop for audio quality: a reaction emoji on a call's embed becomes a "flagged" metric and a log line carrying the call's trace id.

## 5. The process

The `trunk-oxide` process (`crates/trunk-oxide/src/main.rs`):

```text
 process (pid1::Pid1Settings::launch first, so it behaves as PID 1 in a container)
 ├─ main thread        parse args, init tracing + OTel, Config::load, cpu_weight::require,
 │                      ScannerBuilder::from_config(...).start(), then join("monitor")
 ├─ tokio runtime (Runtime::new, multi-thread workers)
 │    └─ task: serve_grpc(listen_path)         tonic over UnixListener, health service
 │         └─ per Subscribe: task  broadcast::Receiver ──► mpsc(4096) ──► client
 ├─ thread "monitor"    rt.handle().block_on(Scanner::run_monitor)   (state machine + Radio calls)
 ├─ thread "source-N"   one per source: channelizer + every decoder on its slots; sends RadioEvent
 │       │  mpsc<RadioEvent>(256)  ───────────────► "monitor"
 │       └─ AudioSink -> EventBridge ─ broadcast(4096) ─► subscriber tasks
 ├─ thread "rtl-<serial>"  (or "iqfile-<name>")   librtlsdr callback -> Stream queue
 └─ short-lived: calibration threads, one per `gain = "auto"` dongle, joined before sources open
```

The monitor runs on its own OS thread so a slow tick never holds a runtime worker that is serving gRPC; `Handle::block_on` still shares the runtime's reactor, timers and signal handler.

Shared state: `Radio` holds `Mutex<Inner>` (sources, systems, slots, open channels), taken by the monitor for `open` / `close` / `rotate` / `reading` / status and never by a DSP thread. The DSP thread and the monitor meet at these places: the channelizer's `mailbox` and `dsp` mutexes (control calls only append to the mailbox; the DSP thread drains it between blocks), a channel's `Tape` (`Arc<Mutex<Option<Recording>>>`: the decoder writes, the monitor reads accounting and closes), the `SlotStats` atomics, `EventBridge`'s `audio_meta` `RwLock` (read per audio block, written by the monitor at call create and end), and the per-decoder counter registries (`trunk-radio/src/registry.rs`). `RadioEvent` is the only way back from a DSP thread to the state machine; the state machine never blocks a DSP thread, and a full event queue drops the event rather than the sample stream. The radio's and channelizer's mutexes are taken with `PoisonError::into_inner`, so a panicked holder does not wedge them.

Lifecycle: a call's state is changed only by the state machine (`CallState`); a channel's open state only by `Radio::open` / `close` through `CallManager`. A control channel is a claim like any call (`controls`). Config is read once; nothing reloads.

Shutdown: `ctrl_c` (or `MonitorOptions::shutdown`) in the monitor's `select!` calls `CallManager::conclude_all`, which unwants every call (closing its channel and sending its held tail), emits `CallEnd` with `CallEndReason::Shutdown` for live ones, removes them and emits `CallsActive`; the loop returns, `Scanner` is dropped (`Radio::stop` stops each `Source`: set `stopping`, `Running::stop`, join its thread), `main` shuts down the OTel providers and returns, dropping the runtime.

Fatal exits: a source that stops delivering (`std::process::exit(1)`); `cpu_weight::require` failing at startup (the scanner refuses to run unless some cgroup it is in, below the root, has `cpu.weight` of at least `min_cpu_weight`, default 1000; the host side is `deploy/systemd/user.slice.d/50-trunk-oxide-cpu.conf`).

Persisted: `<state_dir>/gain-calibration.json` (default `state_dir` `/var/lib/trunk-oxide`), written with `std::fs::write` (not atomic: no temp file and rename; a torn file is discarded by `Cache::load`, which falls back to sweeping). Optional taps under `tap_dir` for `trunk-replay --tap`. Nothing else is written by this process. `temp_dir` is parsed into `CallManagerConfig` but nothing reads it.

Logging: `tracing` with the `fmt` layer (its default writer, stdout), filtered by `RUST_LOG` (`EnvFilter::from_default_env`), plus the OTel layer when tracing is enabled. `trunk-discord` additionally keeps `serenity`'s spans out of exported traces.

The consumers are separate processes, joined by Unix sockets on shared volumes (`oxide-socket`, `whisperer-socket` in `docker-compose.yml`): `trunk-discord` (`main.rs`, `dispatcher.rs`), one transcriber (profiles `whisper`, `parakeet`, `whistle`), and the Jaeger and VictoriaMetrics containers the metrics and traces go to. Images are built by `Dockerfile.trunk-recorder` (the scanner), `Dockerfile.discord`, `Dockerfile.whisperer`, `Dockerfile.parakeet`, `Dockerfile.whistle`; CI (`.github/workflows/docker.yml`) builds the first three.

## 6. Other binaries

There is no web, WASM or mobile build. `.cargo/config.toml` sets `target-cpu=znver2` for `x86_64-unknown-linux-gnu`; `proc_stats.rs` (per-thread CPU) reads `/proc` and reports nothing elsewhere, and `cpu_weight.rs` reads cgroup files. The other executables:

| Binary | Purpose | Limits |
|---|---|---|
| `trunk-replay` (`crates/trunk-oxide/src/bin/trunk-replay/`) | runs the real pipeline on recorded IQ (`run`, `sweep`, `compare`, `taps`); see [`REPLAY.md`](../crates/trunk-oxide/REPLAY.md) | `iqfile` sources only, cu8 or cf32; speed `1x` is wall-clock and must be a release build to keep up, `max` is deterministic and uses `Pacer` + `Clock::manual` |
| `trunk-solver` | Z3-based plan of device centres and rates covering all configured channels; prints `[recorder] max_channels` | knows `rtl` and `airspy` device templates (`device.rs`), though the scanner drives RTL-SDRs only; plans with `passband::USABLE` |
| `trunk-discord ab` | blind A/B listening posts and tallies (`ab.rs`) | needs a Discord guild |
| `examples/` in `trunk-p25`, `trunk-fm`, `trunk-channelizer`, `trunk-sdr`, `trunk-discord` | golden scorers, benchmarks, probes | development tools |

## 7. The gRPC protocol

Source of truth: [`trunk_oxide.proto`](../crates/trunk-oxide-proto/proto/trunk_oxide.proto), compiled by `trunk-oxide-proto/build.rs` with `tonic_prost_build`. The only RPC is `TrunkOxide.Subscribe(Empty) returns (stream StreamMessage)`; a `StreamMessage` is an `AudioSample` or a `PluginEvent` (`oneof`). `PluginEvent` is only the event wrapper; nothing loads plugins. Removed fields stay as `reserved` lines, including the old `sys_num` fields: consumers key on `short_name`.

Checks: the unit tests in `trunk-oxide-proto/src/lib.rs` assert that `encoded_len()` equals the encoded size for the messages they build. Producer and consumers all compile against the generated module, so a renamed or removed field fails the build of whichever side uses it; there is no separate schema-parity test.

Direction: server to client only. Ordering guarantee (in the proto comment): `CallEnd` follows the last `AudioSample` of its call; this holds because `Radio::close` and `rotate` send a held tail to the sink before the executor emits `EmitCallEnd`, and `RemoveCall` unregisters the call's `AudioMeta` last. Every subscriber receives every message (no topics, no per-client filter). A lagging subscriber is not disconnected: the broadcast channel (4096) overwrites its oldest, the subscriber task logs `subscriber lagged` with the skip count and goes on, and `AudioSample`s in that window are lost.

| Message | Content | When |
|---|---|---|
| `AudioSample` | `talkgroup`, `freq`, PCM `samples`, `sample_rate`, `short_name` | see section 2.7 |
| `SetupConfigEvent` / `SetupSystemEvent` | `SystemInfo` (`short_name`, `system_type`, `sys_id`, `wacn`, `nac`, `rfss`, `site_id`, `bandplan`, levels, squelch, `conversation_mode`) | startup; site and network status |
| `CallStartEvent`, `CallEndEvent`, `CallsActiveEvent` | `CallInfo` / `CallData` | call lifecycle |
| `SignalEvent` | unit and `CallInfo` | a unit begins talking |
| `TrunkMessageEvent` | `TrunkMessage` list and `SystemInfo` | control messages |
| `UnitEvent`, `UnitAliasEvent` | unit and system / alias | unit activity |
| `SetupRecorderEvent` | `RecorderInfo` (`id` = channel number, `freq_error`) | a channel closed |

`CallInfo.call_state` is the `CallState` enum (`CALL_STATE_RECORDING`, `CALL_STATE_IDLE`, `CALL_STATE_WAITING`, and the `CALL_STATE_REJECTED_*` reasons); `CallEnd` carries a `CallEndReason`. Talkgroup `alpha_tag`, `tag`, `group` and `priority` are added by `EventBridge::enrich` from `TalkgroupMeta` (built by `talkgroup_meta_map` from the config).

Access control: none beyond the socket's filesystem permissions. The service is on a Unix domain socket at `[grpc] listen_path` (default `/run/service/grpc.sock`); `tonic_health` serves `grpc.health.v1` on the same socket, which the compose healthcheck probes with `grpc_health_probe`. There are no HTTP routes and no TLS. The same socket has two names: the scanner listens on `[grpc] listen_path` (`/run/service/grpc.sock`) and `trunk-discord` dials `[oxide] socket_path` (`/run/trunk-oxide/grpc.sock`); `docker-compose.yml` mounts the `oxide-socket` volume at `/run/service` in the scanner and at `/run/trunk-oxide` in `trunk-discord` (likewise `whisperer-socket` at `/run/service` in a transcriber and `/run/trunk-whisperer` in `trunk-discord`). `trunk-discord` itself serves only the health service, on its own `[grpc] listen_path`.

Second protocol: `Transcriber.Transcribe(TranscribeRequest) returns (TranscribeResponse)` (`transcriber.proto`): `audio` is little-endian `f32` mono at 16 kHz, with `short_name`, `is_conventional`, `talkgroup` (span attribute only) and `priority` (lower is more important; 0 or unset is least) as admission hints; the response is `text` plus optional start and end times of the recognised text. Trace context rides gRPC metadata.

## 8. Downstream consumers

trunk-oxide has no plugin system; it has consumers of its stream.

**trunk-discord** (`crates/trunk-discord/`). It dials `[oxide] socket_path` with `grpc_oxide::connect` (exponential back-off, 1 s doubling to a 10 s cap, bumping a heartbeat so the watchdog does not mistake an absent upstream for a stall) and, when the stream ends, ends the calls it carried (`upstream_lost`) and subscribes again. `DispatchPump` is a single-owner loop over `DispatcherState` (no mutex): a call is keyed by `CallKey { system, talkgroup }`, each live call has a `CallHandler` task fed by an `mpsc` of 1 024 `CallMessage`s (`Audio`, `Event`, `End`, `PttEnd`, `PttStart`), and a send that cannot complete in `CALL_SEND_TIMEOUT` = 30 s drops that handler rather than freeze the pump. Per call: audio is resampled to 16 kHz (`resampler.rs`, rubato), cut per PTT transmission (`audio.max_segment_ms` = 120 000 as the safety cap), sent to the transcriber with a per-request timeout (`audio.transcription_timeout_s` = 30; a timeout is treated as empty), encoded to Ogg Opus at 16 kbit/s (`ogg_encoder.rs`) and posted with an embed. `ChannelWriters` serialises Discord REST per channel in two lanes: an edit lane that coalesces to the latest embed, and a create lane bounded at `CREATE_LANE_CAPACITY` = 128 that sheds audio attaches when full. Persistence is `trunk-database`: hourly SQLite files `trunk-discord-<YYYY-MM-DDTHH>-<schema hash>.db` in `--db-path` (`journal_mode=WAL`), tables `calls`, `thread_cache`, `activity` (`sql/schema.sql`); the hash is the first 8 hex digits of the schema's SHA-256, so a schema change starts a new file. At each hour the pump opens the next file and copies the day's thread cache into it (`rotate_db`). The liveness watchdog (`health_watchdog`) reports `NotServing` after 20 s without a pump heartbeat and exits the process after 90 s.

**Transcribers.** `trunk-whisperer` (whisper.cpp, optional `cuda` feature; `gpu_count` 2, `max_concurrent` 4, `beam_size` 5 by default), `trunk-parakeet` (NVIDIA Parakeet through ONNX Runtime; `max_concurrent` 1) and `trunk-whistle` (Cactus Whistle through the C engine `libneedle.a`, which holds one process-global model; `max_concurrent` 1) each serve `Transcriber` on a Unix socket, one at a time on the shared socket volume; swapping backends is a container choice, not a `trunk-discord` setting. Each admits requests through its own copy of `priority_semaphore.rs` (`PrioritySemaphore`): when saturated, the waiter with the lowest `priority` number is woken first, FIFO within a priority, so a dispatch call is not stuck behind transit chatter. Failure handling is the client's: a request that errors or times out becomes an empty transcript and the call still posts.

Install: `docker compose up --build` starts the scanner, `trunk-discord`, Jaeger and VictoriaMetrics; a transcriber is a compose profile (`--profile whisper`, `parakeet` or `whistle`), each with a `fetch-*-model` service that downloads its model into a volume. `trunk-discord` has no `depends_on`: it retries the scanner's socket and resubscribes whenever the stream drops, so either side restarts alone. `trunk-whistle` is not in `default-members` and needs `NEEDLE_LIB_DIR` plus libc++ 21 static libraries to build outside Docker.

## 9. Identifiers that cross boundaries

| Identifier | Type | What it indexes | Covers | Used in | Lifetime |
|---|---|---|---|---|---|
| `short_name` | `String` (the `[system.<name>]` key) | a system | all configured systems | wire (`short_name` on every message that names a system), SQLite keys (`thread_cache`), metric labels, `CallKey` in `trunk-discord`, `TalkgroupMeta` keys | stable across restarts |
| `sys_num` | `i32` | a system, by position in `short_name` order; `Radio.systems` index | all configured systems | `TrunkMessage.sys_num`, `SystemPolicy` map, `Radio::add_system`; never on the wire (`reserved`) | derived from the config's set of names: stable only while no name is added or removed before it |
| source index | `i32` (`Source.idx`) | an enabled source, in config order | enabled sources only | `Radio`, `RadioEvent::SignalsDetected.source_num`, `ChannelReport.source`, `source` metric labels | per config |
| config source index | `usize` | a `[[source]]` entry, enabled or not | all `[[source]]` entries | `calibrate::report::SourceGain.source` and the `trunk.oxide.source.gain_db` / `noise_floor_db` labels | differs from the source index when a source has `enabled = false` |
| `ChannelNum` | `u32` | a channelizer slot the radio has used, numbered in first-use order | grows; never shrinks | `Radio::open` result, `RecorderInfo.id`, `CallData.channel`, `CallSegment.channel` | per run |
| slot port | `usize` | a slot of one source's channelizer (`Source::alloc_port`) | per source | inside `trunk-radio` | per run |
| `CallId` | `u64` (from 1) | a call or a control-channel claim | every call the state machine created, rejected ones included | state machine, `Radio::open` / `close`, `RadioEvent` routing, `AudioMeta` key | per run; never on the wire |
| `call_num` | `i64` (from 0) | one transmission of a call; a new one at every boundary | per run | `CallInfo.call_num`, `CallData.call_num` | restarts at 0 |
| talkgroup | `u32` in the radio, `i64` on the wire | a talkgroup, or the called unit of a unit-to-unit call | as configured, or as granted | `TalkgroupConfig.id`, `Talkgroup` newtypes, `CallKey`, SQLite | stable |
| unit id | `u32` / `i64` | a radio | as heard | `Voice.source`, `SignalEvent.unit_id`, `UnitAlias.radio_id` | stable |
| `sys_id`, `wacn`, `nac` | `u16`, `u32`, `u16` on air; `u64` in config and `SystemInfo` | a P25 system and network | as configured, replaced by what the control channel announces | `SystemIdentity`, `Scrambler` seed | stable |
| `rfss`, `site_id` | `i32` | a site within a system | learned from `RFSS_STS_BCST` | `check_multisite`, `SystemInfo` | stable |
| TDMA slot | `u8` 0-based in `Channel.slot` (`Option`); `i32` on the wire with `phase2_tdma` | a logical channel of a phase 2 carrier | TDMA calls | `CallInfo.tdma_slot`, `Phase2Assembler.channel` | stable |
| `Priority` | `i32` | a claim on a channel; lower wins; `Priority::UNSET` = `i32::MAX` | per talkgroup, channel, control channel | `allocation.rs`, `TalkgroupMeta.priority`, `TranscribeRequest.priority` | config |
| `CallRowId` | `i64` | a row in the hourly `calls` table | one database file | `trunk-database` only | per file |

Other conventions: frequencies are Hz as `f64` in config and on the wire (`Freq(u64)` truncates to whole Hz inside the state machine); sample rates are S/s; gains and powers are dB (powers relative to full scale); time is `Instant` inside, Unix seconds or `Timestamp` at the edge; audio is `int16` PCM, 8 kHz from P25, 16 kHz from analog, 16 kHz `f32` to transcribers; priorities are lower-wins, with `-1` rejecting a trunked talkgroup in `assess`; TDMA slots are 0-based; the hardware `ppm` is a whole number (`SourceConfig::ppm` rounded in `rtl_tuning`) and the fractional correction is `freq_correction_hz`.

## 10. Where things live

| Concern | File(s) |
|---|---|
| Workspace members, lints, MSRV | `Cargo.toml`, `rust-toolchain.toml`, `.cargo/config.toml` |
| Config schema and CSV overlay | `crates/trunk-config/src/lib.rs` (`Config::load`), `system.rs`, `source.rs`, `recorder.rs`, `service.rs`, `discord.rs`; examples `config.toml.example`, `config.toml` |
| Talkgroup and channel tables | `smco-talkgroups.csv`, `svrcs-talkgroups.csv`, `conventional-channels.csv` (RadioReference-style CSV) |
| Startup, source and system wiring | `crates/trunk-oxide/src/main.rs`, `src/lib.rs` |
| Call state machine, allocation, effects | `crates/trunk-oxide/src/call_state.rs`, `allocation.rs`, `call_manager.rs` |
| Message dedup, clock, CPU weight | `dedup.rs`, `clock.rs`, `cpu_weight.rs` |
| Gain calibration | `crates/trunk-oxide/src/calibrate/` (`select.rs`, `measure.rs`, `cache.rs`, `live.rs`, `rtl.rs`, `report.rs`) |
| Metrics | `crates/trunk-oxide/src/main.rs` (gauges), `call_manager.rs`, `p25_metrics.rs`, `fm_metrics.rs`, `proc_stats.rs`; `crates/trunk-radio/src/registry.rs`, `p25_calls.rs`, `fm.rs` |
| The radio, slots, kinds | `crates/trunk-radio/src/radio.rs`, `kind.rs`, `source.rs`, `types.rs` |
| Recording, audibility hold | `crates/trunk-radio/src/recording.rs`, `audible.rs`, `carrier.rs` |
| Clock correction, replay pacing, taps | `crates/trunk-radio/src/correction.rs`, `pace.rs`, `tap.rs`, `snapshot.rs` |
| SDR drivers and queue | `crates/trunk-sdr/src/rtl.rs`, `file.rs`, `stream.rs`, `convert.rs`; `crates/trunk-rtlsdr-sys/src/lib.rs`, `build.rs` |
| Channelizer | `crates/trunk-channelizer/src/config.rs`, `channelizer.rs`, `service.rs`, `conditioner.rs`, `detector.rs`, `passband.rs`, `resampler.rs`, `design.rs` |
| P25 control: demod, framer, trellis, FEC | `crates/trunk-p25/src/demod.rs`, `framer.rs`, `trellis.rs`, `control.rs`, `bch.rs`, `crc.rs`, `gf64.rs`, `symbols.rs` |
| P25 control: messages and channel plans | `crates/trunk-p25/src/trunking/` (`osp.rs`, `iden.rs`); `crates/trunk-radio/src/p25.rs`, `p25_parse.rs` |
| P25 phase 1 voice | `crates/trunk-p25/src/voice/`; `crates/trunk-radio/src/p25_voice.rs` |
| P25 phase 2 voice | `crates/trunk-p25/src/tdma/` |
| Call assembly (who talks, end, encryption, aliases) | `crates/trunk-p25/src/assemble.rs`; `crates/trunk-radio/src/alias.rs` |
| Vocoders | `crates/trunk-p25/src/vocoder/` |
| Analog FM | `crates/trunk-fm/src/`; `crates/trunk-radio/src/analog.rs` |
| gRPC server and bridge | `crates/trunk-oxide-grpc/src/lib.rs` |
| Protocol definitions | `crates/trunk-oxide-proto/proto/trunk_oxide.proto`, `crates/trunk-transcriber-proto/proto/transcriber.proto` |
| Discord bot | `crates/trunk-discord/src/` (`dispatcher.rs`, `dispatcher_state.rs`, `call_handler.rs`, `channel_writer.rs`, `grpc_oxide.rs`, `grpc_transcriber.rs`, `ogg_encoder.rs`, `tone_detect.rs`) |
| Database | `crates/trunk-database/src/lib.rs`, `sql/schema.sql` |
| Transcribers | `crates/trunk-whisperer/`, `crates/trunk-parakeet/`, `crates/trunk-whistle/` |
| Source planning | `crates/trunk-solver/src/` |
| Replay and sweeps | `crates/trunk-oxide/src/bin/trunk-replay/`, `crates/trunk-oxide/REPLAY.md` |
| Golden and noise tools | `crates/trunk-p25/tests/golden.rs`, `crates/trunk-p25/examples/`, `crates/trunk-channelizer/examples/`, `crates/trunk-fm/examples/`, `tests/generate_test_iq.py` |
| Deployment | `docker-compose.yml`, `Dockerfile.*`, `deploy/systemd/user.slice.d/50-trunk-oxide-cpu.conf`, `rtl-sdr.rules`, `jaeger-config.yaml`, `.github/workflows/docker.yml` |
| Per-service configs | `config-discord.toml`, `config-whisperer.toml`, `config-parakeet.toml`, `config-whistle.toml` |
