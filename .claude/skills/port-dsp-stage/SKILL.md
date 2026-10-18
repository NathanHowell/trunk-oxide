---
name: port-dsp-stage
description: Replace a C++ / GNU Radio / op25 signal-processing stage in trunk-recorder with Rust (trunk-channelizer, trunk-p25), proving it beats the C++ on the golden capture before switching over. Use for any "move X out of C++" or "port the decoder" request.
---

# Porting a DSP stage from C++ to Rust

The bar: the Rust stage must measurably beat the C++ on the same IQ, with
unit tests, typed APIs and metrics. Done this way: conditioner (k55),
P25 control channel (qii), P25 phase 1 voice (nih).

## 1. Map the C++ path

- Read the op25 / trunk-recorder sources end to end and write down what the
  stage *emits* (message types, payload bytes, stream tags, timeouts,
  callbacks): downstream parity is the contract.
- Note op25's weak points to beat (hard decisions, greedy decoders, first
  failure aborts the frame, fixed limits).

## 2. Get reference data

- Golden taps: `/build/trunk-oxide-runs/baseline-tap/taps` (`.out.cf32`
  conditioned IQ, `.dibits`, `.symbols.f32`, `.msgs.jsonl`); new taps with
  `trunk-replay run --tap`. See `crates/trunk-oxide/REPLAY.md`.
- Bit-exact vectors from op25 itself: write a small C++ harness in the
  scratchpad that includes op25's header-only code (`-I lib -I lib/ezpwd`),
  print encodings, and paste them into Rust tests as constants. Never
  truncate harness output when copying vectors.

## 3. Build it in Rust

- Algebraic, typed and `const` where possible: field and polynomial types,
  codes from generator polynomials or parity columns, layout as formulas
  verified against op25's tables; newtypes and enums at the API, the legacy
  wire format only at the FFI edge; fixed-size arrays and const generics.
- Generic crates stay protocol-agnostic (the channelizer hosts decoders via
  `ChannelDecoder`).
- Keep soft information to the last decoder; quantify any acceptance rule's
  false-accept rate (noise must not decode).

## 4. Prove it

- Unit tests with synthetic signals (`trunk_p25::testgen`), including the C++
  algorithm as a baseline the Rust must beat, and clean/noisy round trips.
- An offline scorer example over the taps comparing against op25, plus an
  independent sanity check where there is no ground truth.
- Sweep parameters offline before choosing defaults (timing loop bandwidth
  mattered more than anything for voice).

## 5. Integrate and A/B

- Typed config switch (`recorder.<stage>_decoder = "rust" | "op25"`), Rust
  default, old path kept for A/B.
- Two full replays in parallel, each in its own IPC namespace:
  `unshare --user --map-current-user --ipc ./target/release/trunk-replay run --capture /build/trunk-oxide-iq --out RUN --set key=value --tap`
  (zsh: split multi-arg variables with `${=v}`).
- Compare `summary.json`, `events.jsonl` and tap logs; call-level numbers
  vary ±1 call run to run, decoder outputs must be deterministic.
- Metrics under `trunk.oxide.<stage>.*` in trunk-oxide (counters by outcome,
  quality gauges), polled at the 1 Hz tick.

## 6. Land it

- `cargo test` and `cargo clippy --all-targets` clean on the touched crates
  (don't `cargo fmt` the whole workspace; it rewrites call_manager.rs).
- Update REPLAY.md, the memory notes and the bead; commit without
  attribution lines when the user asks. Delete the old C++ path in its own
  commit once the user approves.
