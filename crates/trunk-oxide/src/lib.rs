//! Driver that links against `libtrunk-recorder.so` and drives it via FFI.
//!
//! This inverts the typical plugin relationship: instead of trunk-recorder
//! loading a Rust plugin via dlopen, this crate links against trunk-recorder
//! as a shared library and drives it directly.
//!
//! Events flow: SDR → C++ gnuradio graph → cxx callbacks → Plugin trait impl

use trunk_recorder_plugin::Plugin;
use trunk_recorder_sys::ffi;

/// Configures and initializes a trunk-recorder instance without starting it.
///
/// Call [`start()`](TrunkRecorderBuilder::start) to consume the builder and
/// begin receiving radio traffic.
pub struct TrunkRecorderBuilder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
}

/// A running trunk-recorder instance. Stops automatically on drop.
pub struct TrunkRecorder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
}

impl TrunkRecorderBuilder {
    /// Parse config, open SDR sources, and set up all systems.
    ///
    /// # Errors
    ///
    /// Returns an error if the config is invalid or an SDR source fails to open.
    pub fn new<P: Plugin + 'static>(config_json: &str, plugin: P) -> Result<Self, cxx::Exception> {
        let plugin_box = trunk_recorder_plugin::into_plugin_box(plugin);
        let ctx = ffi::tr_create(config_json, Box::new(plugin_box))?;
        Ok(Self { ctx })
    }

    /// Start the gnuradio flowgraph and monitor thread.
    ///
    /// The plugin's callbacks will be invoked from the C++ monitor thread.
    /// Use a channel-based plugin (e.g. sending events via `tokio::sync::mpsc`)
    /// to bridge callbacks into async Rust.
    pub fn start(mut self) -> TrunkRecorder {
        ffi::tr_start(self.ctx.pin_mut());
        TrunkRecorder { ctx: self.ctx }
    }
}

impl Drop for TrunkRecorder {
    fn drop(&mut self) {
        ffi::tr_stop(self.ctx.pin_mut());
    }
}
