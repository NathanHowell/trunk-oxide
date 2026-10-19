---
name: port-dsp-stage
description: Replace a C++ / GNU Radio / op25 signal-processing stage in trunk-recorder with Rust (trunk-channelizer, trunk-p25), proving it beats the C++ on the golden capture before switching over. Use for any "move X out of C++" or "port the decoder" request.
---

# Porting a DSP stage from C++ to Rust

The bar: the Rust stage must measurably beat the C++ on the same IQ, with
unit tests, typed APIs and metrics. Done this way: conditioner (k55),
P25 control channel (qii), P25 phase 1 voice (nih), P25 phase 2 TDMA (has).

## 1. Map the C++ path

- Read the op25 / trunk-recorder sources end to end and write down what the
  stage *emits* (message types, payload bytes, stream tags, timeouts,
  callbacks): downstream parity is the contract.
- Note op25's weak points to beat (hard decisions, greedy decoders, first
  failure aborts the frame, fixed limits, known-zero shortened RS symbols
  treated as erasures, sync that drops on weak channels).
- Look for structure behind op25's lookup tables before copying them: the
  ISCH table is an affine code, the DUID table a Hamming code, the scrambler
  a Galois LFSR, interleavers are transposes. Check with a quick script.

## 2. Get reference data

- Golden taps: `/build/trunk-oxide-runs/baseline-tap/taps` (`.out.cf32`
  conditioned IQ, `.dibits`, `.symbols.f32`, `.msgs.jsonl`); new taps with
  `trunk-replay run --tap`. See `crates/trunk-oxide/REPLAY.md`.
- Bit-exact vectors from op25 itself: write a small C++ harness in the
  scratchpad that includes op25's header-only code (`-I lib -I lib/ezpwd`),
  print encodings, and paste them into Rust tests as constants. Never
  truncate harness output when copying vectors.
- Taps from a run with a Rust decoder hold *regenerated* symbols; offline
  scorers that need op25's symbols must use taps from an op25 run.

## 3. Build it in Rust

- Algebraic, typed and `const` where possible: field and polynomial types,
  codes from generator polynomials or parity columns, layout as formulas
  verified against op25's tables; newtypes and enums at the API, the legacy
  wire format only at the FFI edge; fixed-size arrays and const generics.
- Generic crates stay protocol-agnostic (the channelizer hosts decoders via
  `ChannelDecoder`).
- Keep soft information to the last decoder; quantify any acceptance rule's
  false-accept rate (noise must not decode). Derive gates from measured
  noise statistics (e.g. `AmbeFrame::trusted`, `examples/tdma_noise.rs`),
  not hand-picked numbers; gate per burst/frame group when the per-frame
  tail is too heavy.
- When regenerating for op25, rewrite only what is verified and pass the
  rest through untouched, so the result is never worse than op25 alone and
  op25 never locks onto noise.

- Floating point bit for bit (the vocoders): replicate C's types per
  operation (`float`, promoted to `double` by a `double` literal or
  `M_PI`; which `cos` overload the includes pick), round tables as C does
  (literal to `double`, then `float`) by extracting them with a script, and
  build the C oracle with `-ffp-contract=off` (GCC fuses `a*b+c` under
  `-march`; Rust never does). Watch LLVM's libcall rewrites too (`2f32.powf(x)`
  becomes `exp2f`; check the rounding agrees exhaustively), and reproduce
  op25's quirks where they change output (its Golay error counts).

## 4. Prove it

- Unit tests with synthetic signals (`trunk_p25::testgen`), including the C++
  algorithm as a baseline the Rust must beat, and clean/noisy round trips.
- An offline scorer example over the taps comparing against op25: emulate
  op25's own chain (framer, sync, hard decoders) on its symbols as the
  baseline, then score op25 again on the Rust-regenerated stream (the real
  end-to-end number). Flush held output at the end of each tap.
- Ground truth from the signal itself: CRC/RS-verified words, talkgroups
  that match the call, pitch continuity of voice frames.
- For anything audible, a blind listening test beats any metric: cut A/B
  clips only where the candidates diverge most (`vocoder_ab`), post them
  with `trunk-discord ab post`, tally with `ab tally`. Waveform SNR is
  meaningless for a parametric vocoder (one repeat shifts every later
  phase); log-spectral distance is honest but undervalues artifacts, and
  the ears disagreed with it on phase 2.
- Sweep parameters offline before choosing defaults (timing loop bandwidth
  mattered more than anything for voice).

## 5. Integrate and A/B

- Typed config switch (`recorder.<stage>_decoder = "rust" | "op25"`), Rust
  default, old path kept for A/B.
- Two full replays in parallel, each in its own IPC namespace:
  `unshare --user --map-current-user --ipc ./target/release/trunk-replay run --capture /build/trunk-oxide-iq --out RUN --set key=value --tap`
  (zsh: split multi-arg variables with `${=v}`). Wait for runs with
  `while pgrep -x trunk-replay` — `pgrep -f 'trunk-replay run'` matches the
  waiting shell itself and never exits. Don't build (C++ rebuilds) while
  replays run: they are real time.
- Compare `summary.json`, `events.jsonl` and tap logs; call-level numbers
  vary ±1 call run to run, decoder outputs must be deterministic.
- Shadow mode (live A/B): run both implementations on the same IQ, the old
  one only reported, and compare per item (`Both` / `Only(impl)`). The P25
  ports used `recorder.p25_shadow` and `trunk.oxide.shadow.*`; it was
  deleted with the C++ it compared against (git history: 3c56d61..c4086cf),
  so a new port brings its own for as long as both implementations exist.
- An oracle: drive the C++ stage directly from a test (`op25_voice_reference`,
  `op25_vocoder_reference`, the old `tr_p25_reference_parse`) and diff
  outputs on golden taps and on random streams built to reach every branch,
  every
  difference a named divergence; freeze the oracle's output as a fixture
  before deleting the C++ (`p25_parse/tests/reference.rs`).
- Metrics under `trunk.oxide.<stage>.*` in trunk-oxide (counters by outcome,
  quality gauges), polled at the 1 Hz tick; while two implementations run,
  labelled `impl` and `role` (primary | shadow).

## 6. Land it

- `cargo test` (debug too: overflow checks catch what release hides) and
  `cargo clippy --all-targets` clean on the touched crates (don't
  `cargo fmt` the whole workspace or trunk-oxide; it rewrites
  call_manager.rs — `rustfmt` the leaf files instead).
- Update REPLAY.md, the memory notes and the bead; commit without
  attribution lines when the user asks. Delete the old C++ path in its own
  commit once the user approves.
