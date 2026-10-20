//! Baseline taps (`trunk-replay run --tap`): what a channel's decoder was
//! given, for the offline scorers. Per channel, `<base>.json` (what it is),
//! `<base>.out.cf32` (the slot's conditioned IQ, interleaved f32) and, for
//! voice, `<base>.symbols.f32` (the demodulator's soft symbols).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use num_complex::Complex32;

/// Voice taps are numbered in the order their channels open.
static VOICE_TAPS: AtomicU64 = AtomicU64::new(0);

pub struct Tap {
    base: String,
    iq: BufWriter<File>,
    symbols: Option<BufWriter<File>>,
}

impl std::fmt::Debug for Tap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tap")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Tap {
    /// A control channel's tap at `base`.
    pub fn control(base: &str, meta: &serde_json::Value) -> Option<Self> {
        Self::open(base.to_owned(), meta, false)
    }

    /// The next voice tap in `dir` for `system`'s `talkgroup`. `meta` gains
    /// the symbols file's name.
    pub fn voice(dir: &str, system: &str, talkgroup: u32, meta: serde_json::Value) -> Option<Self> {
        let n = VOICE_TAPS.fetch_add(1, Ordering::Relaxed);
        let name = format!("v_{n}_{system}_{talkgroup}");
        let mut meta = meta;
        meta["symbols_file"] = format!("{name}.symbols.f32").into();
        let base = format!("{dir}/{name}");
        Self::open(base, &meta, true)
    }

    fn open(base: String, meta: &serde_json::Value, symbols: bool) -> Option<Self> {
        let create = |ext: &str| {
            File::create(format!("{base}.{ext}"))
                .map_err(|e| tracing::warn!(base, error = %e, "cannot open a tap"))
                .ok()
        };
        let mut json = create("json")?;
        let _ = writeln!(json, "{meta}");
        Some(Self {
            iq: BufWriter::new(create("out.cf32")?),
            symbols: if symbols {
                Some(BufWriter::new(create("symbols.f32")?))
            } else {
                None
            },
            base,
        })
    }

    pub fn iq(&mut self, iq: &[Complex32]) {
        for s in iq {
            let _ = self.iq.write_all(&s.re.to_le_bytes());
            let _ = self.iq.write_all(&s.im.to_le_bytes());
        }
    }

    pub fn symbols(&mut self, symbols: &[f32]) {
        if let Some(out) = &mut self.symbols {
            for s in symbols {
                let _ = out.write_all(&s.to_le_bytes());
            }
        }
    }
}
