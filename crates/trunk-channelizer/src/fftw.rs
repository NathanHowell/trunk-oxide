//! Minimal FFTW3 (single precision) FFI: aligned buffers and shareable 1-D complex plans.
//!
//! This is the only `unsafe` in the crate. FFTW is linked as `libfftw3f`.
//!
//! Thread-safety contract (FFTW manual, "Thread safety"): planning and
//! destroying plans are NOT thread-safe, so they are serialised through
//! [`PLAN_LOCK`]; `fftwf_execute_dft` on one plan from many threads with
//! distinct arrays IS thread-safe, which lets all channels share one inverse plan.
#![allow(unsafe_code)]

use std::ffi::{c_int, c_uint, c_void};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Mutex;

use anyhow::{Result, bail};
use num_complex::Complex32;

type RawPlan = *mut c_void;

const FFTW_FORWARD: c_int = -1;
const FFTW_BACKWARD: c_int = 1;
const FFTW_MEASURE: c_uint = 0;
const FFTW_PATIENT: c_uint = 1 << 5;
const FFTW_ESTIMATE: c_uint = 1 << 6;

#[link(name = "fftw3f")]
unsafe extern "C" {
    fn fftwf_malloc(n: usize) -> *mut c_void;
    fn fftwf_free(p: *mut c_void);
    fn fftwf_plan_dft_1d(
        n: c_int,
        input: *mut Complex32,
        output: *mut Complex32,
        sign: c_int,
        flags: c_uint,
    ) -> RawPlan;
    fn fftwf_execute_dft(plan: RawPlan, input: *mut Complex32, output: *mut Complex32);
    fn fftwf_destroy_plan(plan: RawPlan);
}

static PLAN_LOCK: Mutex<()> = Mutex::new(());

/// How hard FFTW searches for a fast algorithm. `Measure`/`Patient` clobber
/// the planning buffer and take noticeable time; reuse via wisdom (wxt.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Estimate,
    Measure,
    Patient,
}

impl Effort {
    const fn flags(self) -> c_uint {
        match self {
            Self::Estimate => FFTW_ESTIMATE,
            Self::Measure => FFTW_MEASURE,
            Self::Patient => FFTW_PATIENT,
        }
    }
}

/// Zero-initialised, SIMD-aligned (`fftwf_malloc`) complex buffer.
pub struct AlignedBuf {
    ptr: NonNull<Complex32>,
    len: usize,
}

// SAFETY: AlignedBuf uniquely owns its allocation, like Vec<Complex32>.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only hands out `&[Complex32]`.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        assert!(len > 0);
        let bytes = len * size_of::<Complex32>();
        // SAFETY: fftwf_malloc returns suitably aligned memory or null.
        let raw = unsafe { fftwf_malloc(bytes) }.cast::<Complex32>();
        let ptr = NonNull::new(raw).expect("fftwf_malloc failed");
        // SAFETY: freshly allocated `len` Complex32s; all-zero bits are a valid Complex32.
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, len) };
        Self { ptr, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [Complex32];
    fn deref(&self) -> &[Complex32] {
        // SAFETY: ptr is valid for `len` initialised elements for our lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [Complex32] {
        // SAFETY: as above, and `&mut self` guarantees uniqueness.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated by fftwf_malloc, freed exactly once.
        unsafe { fftwf_free(self.ptr.as_ptr().cast()) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// In-place 1-D complex FFT plan of fixed size. Unnormalised (like FFTW).
pub struct Plan {
    raw: RawPlan,
    n: usize,
}

// SAFETY: `fftwf_execute_dft` is documented thread-safe for a shared plan
// when called with distinct arrays; creation/destruction are serialised by PLAN_LOCK.
unsafe impl Send for Plan {}
// SAFETY: see above.
unsafe impl Sync for Plan {}

impl Plan {
    pub fn new(n: usize, dir: Direction, effort: Effort) -> Result<Self> {
        let Ok(n_int) = c_int::try_from(n) else {
            bail!("FFT size {n} too large")
        };
        if n == 0 {
            bail!("FFT size must be > 0");
        }
        let sign = match dir {
            Direction::Forward => FFTW_FORWARD,
            Direction::Backward => FFTW_BACKWARD,
        };
        let scratch = AlignedBuf::new(n);
        let p = scratch.ptr.as_ptr();
        let _guard = PLAN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: `scratch` is valid, aligned memory of n elements; in-place plan (in == out).
        let raw = unsafe { fftwf_plan_dft_1d(n_int, p, p, sign, effort.flags()) };
        if raw.is_null() {
            bail!("fftwf_plan_dft_1d({n}) failed");
        }
        Ok(Self { raw, n })
    }

    pub const fn len(&self) -> usize {
        self.n
    }

    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Transform `buf` in place. `buf` must come from [`AlignedBuf`] so its
    /// alignment matches the planning buffer.
    pub fn execute(&self, buf: &mut AlignedBuf) {
        assert_eq!(buf.len, self.n, "buffer/plan size mismatch");
        let p = buf.ptr.as_ptr();
        // SAFETY: plan is in-place for exactly n elements; `buf` is aligned, valid and uniquely borrowed.
        unsafe { fftwf_execute_dft(self.raw, p, p) };
    }
}

impl Drop for Plan {
    fn drop(&mut self) {
        let _guard = PLAN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: plan created by fftwf_plan_dft_1d, destroyed once, under the planner lock.
        unsafe { fftwf_destroy_plan(self.raw) };
    }
}
