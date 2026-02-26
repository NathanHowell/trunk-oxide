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
    /// Create a new driver instance from a trunk-recorder JSON config string.
    ///
    /// This parses the config, creates SDR sources, and sets up all systems
    /// (trunking control channels, conventional channels, etc.) but does not
    /// start the gnuradio flowgraph yet.
    #[must_use]
    pub fn new(config_json: &str) -> Self {
        let ctx = ffi::tr_create(config_json);
        Self {
            ctx,
            started: false,
        }
    }

    /// Register a [`Plugin`] to receive events and start the engine.
    ///
    /// The plugin's callbacks will be invoked from the C++ monitor thread.
    /// Use a channel-based plugin (e.g. sending events via `tokio::sync::mpsc`)
    /// to bridge callbacks into async Rust.
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub fn start<P: Plugin + 'static>(&mut self, plugin: P) {
        assert!(!self.started, "TrunkRecorderDriver::start called twice");
        let plugin_box = trunk_recorder_plugin::into_plugin_box(plugin);
        ffi::tr_set_plugin(self.ctx.pin_mut(), Box::new(plugin_box));
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
