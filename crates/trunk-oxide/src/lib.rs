//! Driver that links against `libtrunk-recorder.so` and drives it via FFI.
//!
//! This inverts the typical plugin relationship: instead of trunk-recorder
//! loading a Rust plugin via dlopen, this crate links against trunk-recorder
//! as a shared library and drives it directly.
//!
//! Events flow: SDR → C++ gnuradio graph → cxx callbacks → Plugin trait impl

use trunk_recorder_plugin::Plugin;
use trunk_recorder_sys::ffi;

/// Drives a trunk-recorder instance via direct FFI calls.
///
/// The driver owns the C++ `TrContext` which encapsulates the gnuradio top
/// block, all sources/systems/recorders, and the monitor thread.
pub struct TrunkRecorderDriver {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    started: bool,
}

impl TrunkRecorderDriver {
    /// Create a new driver instance from a trunk-recorder JSON config string
    /// and a plugin to receive events.
    ///
    /// This parses the config, installs the event sink, creates SDR sources,
    /// and sets up all systems (trunking control channels, conventional
    /// channels, etc.) but does not start the gnuradio flowgraph yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the config is invalid or an SDR source fails to open.
    pub fn new<P: Plugin + 'static>(config_json: &str, plugin: P) -> Result<Self, cxx::Exception> {
        let plugin_box = trunk_recorder_plugin::into_plugin_box(plugin);
        let ctx = ffi::tr_create(config_json, Box::new(plugin_box))?;
        Ok(Self {
            ctx,
            started: false,
        })
    }

    /// Start the gnuradio flowgraph and monitor thread.
    ///
    /// The plugin's callbacks will be invoked from the C++ monitor thread.
    /// Use a channel-based plugin (e.g. sending events via `tokio::sync::mpsc`)
    /// to bridge callbacks into async Rust.
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub fn start(&mut self) {
        assert!(!self.started, "TrunkRecorderDriver::start called twice");
        ffi::tr_start(self.ctx.pin_mut());
        self.started = true;
    }

    /// Stop the engine, joining the monitor thread and stopping the flowgraph.
    pub fn stop(&mut self) {
        if self.started {
            ffi::tr_stop(self.ctx.pin_mut());
            self.started = false;
        }
    }
}

impl Drop for TrunkRecorderDriver {
    fn drop(&mut self) {
        self.stop();
    }
}
