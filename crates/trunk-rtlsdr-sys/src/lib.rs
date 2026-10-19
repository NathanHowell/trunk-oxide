//! librtlsdr, as trunk-oxide uses it: open a dongle by serial, tune it, set
//! the tuner gain, and read its 8-bit IQ, synchronously for a short capture
//! or through libusb's asynchronous transfers for a stream.
//!
//! [`Device`] is the only safe type and every unsafe call is behind it. The
//! handle may be shared between threads: librtlsdr's control calls (gain,
//! frequency) are single libusb control transfers, which libusb serialises,
//! and that is how gr-osmosdr changes gain while streaming too.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

#[allow(non_camel_case_types)]
mod sys {
    use super::{c_char, c_int, c_uchar, c_void};

    #[repr(C)]
    pub struct rtlsdr_dev_t {
        _private: [u8; 0],
    }

    pub type rtlsdr_read_async_cb_t =
        Option<unsafe extern "C" fn(buf: *mut c_uchar, len: u32, ctx: *mut c_void)>;

    unsafe extern "C" {
        pub fn rtlsdr_get_device_count() -> u32;
        pub fn rtlsdr_get_device_name(index: u32) -> *const c_char;
        pub fn rtlsdr_get_device_usb_strings(
            index: u32,
            manufact: *mut c_char,
            product: *mut c_char,
            serial: *mut c_char,
        ) -> c_int;
        pub fn rtlsdr_get_index_by_serial(serial: *const c_char) -> c_int;
        pub fn rtlsdr_open(dev: *mut *mut rtlsdr_dev_t, index: u32) -> c_int;
        pub fn rtlsdr_close(dev: *mut rtlsdr_dev_t) -> c_int;
        pub fn rtlsdr_get_usb_strings(
            dev: *mut rtlsdr_dev_t,
            manufact: *mut c_char,
            product: *mut c_char,
            serial: *mut c_char,
        ) -> c_int;
        pub fn rtlsdr_set_center_freq(dev: *mut rtlsdr_dev_t, freq: u32) -> c_int;
        pub fn rtlsdr_get_center_freq(dev: *mut rtlsdr_dev_t) -> u32;
        pub fn rtlsdr_set_freq_correction(dev: *mut rtlsdr_dev_t, ppm: c_int) -> c_int;
        pub fn rtlsdr_get_freq_correction(dev: *mut rtlsdr_dev_t) -> c_int;
        pub fn rtlsdr_get_tuner_type(dev: *mut rtlsdr_dev_t) -> c_int;
        pub fn rtlsdr_get_tuner_gains(dev: *mut rtlsdr_dev_t, gains: *mut c_int) -> c_int;
        pub fn rtlsdr_set_tuner_gain(dev: *mut rtlsdr_dev_t, gain: c_int) -> c_int;
        pub fn rtlsdr_get_tuner_gain(dev: *mut rtlsdr_dev_t) -> c_int;
        pub fn rtlsdr_set_tuner_gain_mode(dev: *mut rtlsdr_dev_t, manual: c_int) -> c_int;
        pub fn rtlsdr_set_sample_rate(dev: *mut rtlsdr_dev_t, rate: u32) -> c_int;
        pub fn rtlsdr_get_sample_rate(dev: *mut rtlsdr_dev_t) -> u32;
        pub fn rtlsdr_set_agc_mode(dev: *mut rtlsdr_dev_t, on: c_int) -> c_int;
        pub fn rtlsdr_set_bias_tee(dev: *mut rtlsdr_dev_t, on: c_int) -> c_int;
        pub fn rtlsdr_reset_buffer(dev: *mut rtlsdr_dev_t) -> c_int;
        pub fn rtlsdr_read_sync(
            dev: *mut rtlsdr_dev_t,
            buf: *mut c_void,
            len: c_int,
            n_read: *mut c_int,
        ) -> c_int;
        pub fn rtlsdr_read_async(
            dev: *mut rtlsdr_dev_t,
            cb: rtlsdr_read_async_cb_t,
            ctx: *mut c_void,
            buf_num: u32,
            buf_len: u32,
        ) -> c_int;
        pub fn rtlsdr_cancel_async(dev: *mut rtlsdr_dev_t) -> c_int;
    }

    pub const USB_STRING_LEN: usize = 256;
}

/// A librtlsdr call that returned an error code (libusb's, usually).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub call: &'static str,
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed with code {}", self.call, self.code)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

const fn check(call: &'static str, code: c_int) -> Result<()> {
    if code < 0 {
        Err(Error { call, code })
    } else {
        Ok(())
    }
}

/// The tuner chip behind the RTL2832U.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tuner {
    Unknown,
    E4000,
    Fc0012,
    Fc0013,
    Fc2580,
    R820T,
    R828D,
}

impl Tuner {
    const fn from_code(code: c_int) -> Self {
        match code {
            1 => Self::E4000,
            2 => Self::Fc0012,
            3 => Self::Fc0013,
            4 => Self::Fc2580,
            5 => Self::R820T,
            6 => Self::R828D,
            _ => Self::Unknown,
        }
    }
}

/// One dongle on the bus, before it is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: String,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
}

fn usb_strings(
    read: impl FnOnce(*mut c_char, *mut c_char, *mut c_char) -> c_int,
) -> Result<(String, String, String)> {
    let mut m = [0 as c_char; sys::USB_STRING_LEN];
    let mut p = [0 as c_char; sys::USB_STRING_LEN];
    let mut s = [0 as c_char; sys::USB_STRING_LEN];
    check(
        "rtlsdr_get_usb_strings",
        read(m.as_mut_ptr(), p.as_mut_ptr(), s.as_mut_ptr()),
    )?;
    let to_string = |buf: &[c_char]| {
        // SAFETY: librtlsdr NUL-terminates each string within the 256 bytes.
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Ok((to_string(&m), to_string(&p), to_string(&s)))
}

/// Every dongle librtlsdr can see.
pub fn devices() -> Vec<DeviceInfo> {
    // SAFETY: plain query; no handle involved.
    let count = unsafe { sys::rtlsdr_get_device_count() };
    (0..count)
        .filter_map(|index| {
            // SAFETY: index < count; the name is a static string.
            let name = unsafe { CStr::from_ptr(sys::rtlsdr_get_device_name(index)) }
                .to_string_lossy()
                .into_owned();
            let (manufacturer, product, serial) = usb_strings(|m, p, s| {
                // SAFETY: three 256-byte buffers, as the API requires.
                unsafe { sys::rtlsdr_get_device_usb_strings(index, m, p, s) }
            })
            .ok()?;
            Some(DeviceInfo {
                index,
                name,
                manufacturer,
                product,
                serial,
            })
        })
        .collect()
}

/// The index of the dongle with this USB serial, if one is plugged in.
pub fn index_by_serial(serial: &str) -> Option<u32> {
    let c = std::ffi::CString::new(serial).ok()?;
    // SAFETY: a NUL-terminated string that outlives the call.
    let index = unsafe { sys::rtlsdr_get_index_by_serial(c.as_ptr()) };
    u32::try_from(index).ok()
}

/// An open dongle. Closed on drop.
pub struct Device {
    dev: *mut sys::rtlsdr_dev_t,
    /// Set while a `read_async` is in progress, so a `cancel_async` from
    /// another thread knows there is something to cancel.
    streaming: AtomicBool,
}

// SAFETY: see the module docs: librtlsdr's calls are individual libusb
// transfers, safe to issue from any thread, and the one blocking call
// (`read_async`) is cancelled from another thread by design.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("streaming", &self.streaming.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Device {
    /// Open the dongle at `index` (see [`devices`], [`index_by_serial`]).
    pub fn open(index: u32) -> Result<Self> {
        let mut dev = std::ptr::null_mut();
        // SAFETY: out-pointer to a null handle; librtlsdr fills it on success.
        check("rtlsdr_open", unsafe {
            sys::rtlsdr_open(&raw mut dev, index)
        })?;
        if dev.is_null() {
            return Err(Error {
                call: "rtlsdr_open",
                code: -1,
            });
        }
        Ok(Self {
            dev,
            streaming: AtomicBool::new(false),
        })
    }

    /// `(manufacturer, product, serial)`.
    pub fn usb_strings(&self) -> Result<(String, String, String)> {
        usb_strings(|m, p, s| {
            // SAFETY: open handle; three 256-byte buffers.
            unsafe { sys::rtlsdr_get_usb_strings(self.dev, m, p, s) }
        })
    }

    pub fn tuner(&self) -> Tuner {
        // SAFETY: open handle.
        Tuner::from_code(unsafe { sys::rtlsdr_get_tuner_type(self.dev) })
    }

    pub fn set_sample_rate(&self, hz: u32) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_set_sample_rate", unsafe {
            sys::rtlsdr_set_sample_rate(self.dev, hz)
        })
    }

    /// The rate the RTL2832U actually resamples to (its divider is coarse).
    pub fn sample_rate(&self) -> u32 {
        // SAFETY: open handle.
        unsafe { sys::rtlsdr_get_sample_rate(self.dev) }
    }

    pub fn set_center_freq(&self, hz: u32) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_set_center_freq", unsafe {
            sys::rtlsdr_set_center_freq(self.dev, hz)
        })
    }

    pub fn center_freq(&self) -> u32 {
        // SAFETY: open handle.
        unsafe { sys::rtlsdr_get_center_freq(self.dev) }
    }

    /// Whole parts per million; the hardware knob.
    pub fn set_freq_correction(&self, ppm: i32) -> Result<()> {
        // SAFETY: open handle.
        let code = unsafe { sys::rtlsdr_set_freq_correction(self.dev, ppm) };
        // librtlsdr answers -2 when the value is already set; not an error.
        if code == -2 {
            return Ok(());
        }
        check("rtlsdr_set_freq_correction", code)
    }

    pub fn freq_correction(&self) -> i32 {
        // SAFETY: open handle.
        unsafe { sys::rtlsdr_get_freq_correction(self.dev) }
    }

    /// The tuner's gain steps, in tenths of a dB, as the tuner orders them.
    pub fn tuner_gains(&self) -> Result<Vec<i32>> {
        // SAFETY: a null buffer asks for the count.
        let count = unsafe { sys::rtlsdr_get_tuner_gains(self.dev, std::ptr::null_mut()) };
        check("rtlsdr_get_tuner_gains", count)?;
        let mut gains = vec![0 as c_int; usize::try_from(count).unwrap_or(0)];
        if !gains.is_empty() {
            // SAFETY: the buffer holds `count` ints, as the first call said.
            check("rtlsdr_get_tuner_gains", unsafe {
                sys::rtlsdr_get_tuner_gains(self.dev, gains.as_mut_ptr())
            })?;
        }
        Ok(gains)
    }

    /// Manual tuner gain (true) or the tuner's own AGC (false).
    pub fn set_tuner_gain_mode(&self, manual: bool) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_set_tuner_gain_mode", unsafe {
            sys::rtlsdr_set_tuner_gain_mode(self.dev, c_int::from(manual))
        })
    }

    /// Tuner gain in tenths of a dB (one of [`Self::tuner_gains`]).
    pub fn set_tuner_gain(&self, tenths_db: i32) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_set_tuner_gain", unsafe {
            sys::rtlsdr_set_tuner_gain(self.dev, tenths_db)
        })
    }

    /// Tuner gain in tenths of a dB.
    pub fn tuner_gain(&self) -> i32 {
        // SAFETY: open handle.
        unsafe { sys::rtlsdr_get_tuner_gain(self.dev) }
    }

    /// The RTL2832U's digital AGC.
    pub fn set_agc_mode(&self, on: bool) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_set_agc_mode", unsafe {
            sys::rtlsdr_set_agc_mode(self.dev, c_int::from(on))
        })
    }

    pub fn set_bias_tee(&self, on: bool) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_set_bias_tee", unsafe {
            sys::rtlsdr_set_bias_tee(self.dev, c_int::from(on))
        })
    }

    /// Flush the USB bulk endpoint. Required once before the first read.
    pub fn reset_buffer(&self) -> Result<()> {
        // SAFETY: open handle.
        check("rtlsdr_reset_buffer", unsafe {
            sys::rtlsdr_reset_buffer(self.dev)
        })
    }

    /// Read raw interleaved unsigned 8-bit IQ into `buf` (a multiple of 512
    /// bytes reads cleanly); returns the bytes read.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize> {
        let len = c_int::try_from(buf.len()).unwrap_or(c_int::MAX);
        let mut n = 0 as c_int;
        // SAFETY: `buf` is valid for `len` bytes for the duration of the call.
        check("rtlsdr_read_sync", unsafe {
            sys::rtlsdr_read_sync(self.dev, buf.as_mut_ptr().cast(), len, &raw mut n)
        })?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Stream raw IQ through `on_buffer` (`buf_num` transfers of `buf_len`
    /// bytes in flight; 0 for librtlsdr's defaults). Blocks until
    /// [`Self::cancel_async`] is called from another thread, the device
    /// fails, or `on_buffer` panics (the stream is then cancelled and the
    /// panic resumed here).
    pub fn read_async<F>(&self, buf_num: u32, buf_len: u32, on_buffer: F) -> Result<()>
    where
        F: FnMut(&[u8]),
    {
        struct Ctx<F> {
            on_buffer: F,
            dev: *mut sys::rtlsdr_dev_t,
            panic: Option<Box<dyn std::any::Any + Send>>,
        }
        unsafe extern "C" fn trampoline<F: FnMut(&[u8])>(
            buf: *mut c_uchar,
            len: u32,
            ctx: *mut c_void,
        ) {
            // SAFETY: `ctx` is the `Ctx<F>` `read_async` passed and outlives
            // the stream; `buf` holds `len` bytes until this returns.
            let ctx = unsafe { &mut *ctx.cast::<Ctx<F>>() };
            if ctx.panic.is_some() {
                return;
            }
            let data = unsafe { std::slice::from_raw_parts(buf, len as usize) };
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| (ctx.on_buffer)(data))) {
                ctx.panic = Some(p);
                // SAFETY: cancelling from within the callback is allowed.
                unsafe { sys::rtlsdr_cancel_async(ctx.dev) };
            }
        }
        let mut ctx = Ctx {
            on_buffer,
            dev: self.dev,
            panic: None,
        };
        self.streaming.store(true, Ordering::SeqCst);
        // SAFETY: `ctx` lives on this stack frame for the whole blocking
        // call, and the trampoline is monomorphised for the same `F`.
        let code = unsafe {
            sys::rtlsdr_read_async(
                self.dev,
                Some(trampoline::<F>),
                (&raw mut ctx).cast(),
                buf_num,
                buf_len,
            )
        };
        self.streaming.store(false, Ordering::SeqCst);
        if let Some(p) = ctx.panic.take() {
            std::panic::resume_unwind(p);
        }
        check("rtlsdr_read_async", code)
    }

    /// Make a [`Self::read_async`] on another thread return.
    pub fn cancel_async(&self) -> Result<()> {
        if !self.streaming.load(Ordering::SeqCst) {
            return Ok(());
        }
        // SAFETY: open handle; librtlsdr allows this from any thread.
        check("rtlsdr_cancel_async", unsafe {
            sys::rtlsdr_cancel_async(self.dev)
        })
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open` and is closed exactly once.
        unsafe { sys::rtlsdr_close(self.dev) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuner_codes_follow_librtlsdr() {
        assert_eq!(Tuner::from_code(5), Tuner::R820T);
        assert_eq!(Tuner::from_code(6), Tuner::R828D);
        assert_eq!(Tuner::from_code(42), Tuner::Unknown);
    }

    #[test]
    fn errors_name_the_call() {
        let e = check("rtlsdr_open", -6).unwrap_err();
        assert_eq!(e.to_string(), "rtlsdr_open failed with code -6");
        assert_eq!(check("ok", 3), Ok(()));
    }

    /// Needs a dongle; passes trivially without one.
    #[test]
    fn lists_devices_without_crashing() {
        for d in devices() {
            assert!(!d.serial.is_empty(), "{d:?}");
        }
    }
}
