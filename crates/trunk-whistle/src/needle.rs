//! Safe wrapper over the Cactus `needle` C engine (`libneedle.a`, `needle.h`).
//!
//! The engine keeps one process-global, non-thread-safe speech model, so
//! [`Needle`] is a singleton and every call needs `&mut self`.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_ulonglong};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};

const NEEDLE_SPEECH: c_int = 2;
/// Hard limit of one `needle_transcribe` pass.
const MAX_CHUNK_SAMPLES: usize = 30 * 16_000;
/// JSON result buffer; 30 s of speech is far below this.
const OUT_CAPACITY: usize = 64 * 1024;

unsafe extern "C" {
    fn needle_load(cact: *const c_uchar, n: c_ulonglong) -> c_int;
    fn needle_models() -> c_int;
    fn needle_last_error() -> *const c_char;
    fn needle_transcribe(
        pcm: *const f32,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        word_timestamps: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
}

static LOADED: AtomicBool = AtomicBool::new(false);

pub struct Needle {
    // Kept alive for the process lifetime in case the engine borrows the buffer
    // rather than copying it (needle.h doesn't say).
    _weights: Vec<u8>,
    language: Option<CString>,
    keywords: Option<CString>,
}

#[derive(serde::Deserialize)]
struct Output {
    text: String,
}

fn last_error() -> String {
    // SAFETY: returns a NUL-terminated string owned by the runtime, valid until
    // the next API call; we copy it out immediately.
    unsafe {
        let p = needle_last_error();
        if p.is_null() {
            "unknown error".to_string()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

impl Needle {
    pub fn load(path: &str, language: Option<&str>, keywords: &[String]) -> Result<Self> {
        if LOADED.swap(true, Ordering::SeqCst) {
            bail!("needle engine already loaded in this process");
        }
        let weights = std::fs::read(path).with_context(|| format!("reading {path}"))?;
        // SAFETY: pointer/length describe `weights`, which outlives the call and
        // is retained in `Self`.
        let rc = unsafe { needle_load(weights.as_ptr(), weights.len() as c_ulonglong) };
        if rc < 0 {
            bail!("needle_load failed ({rc}): {}", last_error());
        }
        // SAFETY: no arguments.
        if unsafe { needle_models() } & NEEDLE_SPEECH == 0 {
            bail!("{path} did not load a speech model (is it whistle.cact?)");
        }
        let keywords = keywords.join("\n");
        Ok(Self {
            _weights: weights,
            language: language.map(CString::new).transpose()?,
            keywords: (!keywords.is_empty())
                .then(|| CString::new(keywords))
                .transpose()?,
        })
    }

    /// Transcribes 16 kHz mono f32 PCM of any length, splitting into evenly
    /// sized passes of at most 30 s.
    pub fn transcribe(&mut self, pcm: &[f32]) -> Result<String> {
        if pcm.is_empty() {
            return Ok(String::new());
        }
        let passes = pcm.len().div_ceil(MAX_CHUNK_SAMPLES);
        let chunk_len = pcm.len().div_ceil(passes);
        let mut parts: Vec<String> = Vec::with_capacity(passes);
        for chunk in pcm.chunks(chunk_len) {
            let text = self.transcribe_chunk(chunk)?;
            let text = text.trim();
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
        Ok(parts.join(" "))
    }

    // `&mut self` encodes that the engine is not thread-safe.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn transcribe_chunk(&mut self, chunk: &[f32]) -> Result<String> {
        let mut out = vec![0u8; OUT_CAPACITY];
        let samples = c_int::try_from(chunk.len())?;
        let language = self
            .language
            .as_ref()
            .map_or(std::ptr::null(), |s| s.as_ptr());
        let keywords = self
            .keywords
            .as_ref()
            .map_or(std::ptr::null(), |s| s.as_ptr());
        // SAFETY: `chunk` and `out` are valid for the lengths passed; language
        // and keywords are NUL-terminated or null; `&mut self` guarantees the
        // non-thread-safe engine isn't re-entered.
        let rc = unsafe {
            needle_transcribe(
                chunk.as_ptr(),
                samples,
                language,
                keywords,
                0,
                out.as_mut_ptr().cast::<c_char>(),
                c_int::try_from(out.len())?,
            )
        };
        if rc < 0 {
            bail!("needle_transcribe failed ({rc}): {}", last_error());
        }
        let json = CStr::from_bytes_until_nul(&out).context("unterminated needle output")?;
        let parsed: Output = serde_json::from_slice(json.to_bytes())
            .with_context(|| format!("bad needle output: {}", json.to_string_lossy()))?;
        Ok(parsed.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test against the real engine. Skipped unless `WHISTLE_MODEL` points
    /// at whistle.cact; `WHISTLE_TEST_PCM` optionally names a raw f32-LE 16 kHz
    /// mono file whose transcript is printed. (One test fn: the engine is a
    /// process-global singleton.)
    #[test]
    fn engine_smoke() {
        let Ok(model) = std::env::var("WHISTLE_MODEL") else {
            eprintln!("WHISTLE_MODEL unset; skipping");
            return;
        };
        let mut n = Needle::load(&model, Some("en"), &[]).unwrap();
        assert_eq!(n.transcribe(&[]).unwrap(), "");
        // Silence yields an empty transcript, including across the 30 s split.
        assert_eq!(n.transcribe(&vec![0.0; 16_000 * 3]).unwrap(), "");
        assert_eq!(n.transcribe(&vec![0.0; 16_000 * 65]).unwrap(), "");
        if let Ok(path) = std::env::var("WHISTLE_TEST_PCM") {
            let bytes = std::fs::read(path).unwrap();
            let (c, _) = bytes.as_chunks::<4>();
            let pcm: Vec<f32> = c.iter().map(|b| f32::from_le_bytes(*b)).collect();
            eprintln!("transcript: {:?}", n.transcribe(&pcm).unwrap());
        }
    }
}
