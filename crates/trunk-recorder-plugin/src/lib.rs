pub use trunk_recorder_sys::ffi::{
    TrCall, TrCallData, TrCallDataStatus, TrCallError, TrCallSource, TrMessageType,
    TrMonitoringState, TrPatchData, TrRecorder, TrSignalType, TrState, TrSystem, TrTransmission,
    TrTrunkMessage,
};

// Re-export sys crate for use by the export macro
pub use trunk_recorder_sys;

/// A signal event delivered to [`Plugin::signal`].
pub struct SignalEvent<'a> {
    pub unit_id: i64,
    pub signaling_type: Option<&'a str>,
    pub signal_type: TrSignalType,
    pub call: &'a TrCall,
    pub system: &'a TrSystem,
    pub recorder: &'a TrRecorder,
}

/// Result type for plugin callbacks.
pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Safe plugin trait. All methods have default no-op implementations so
/// plugins only override what they need.
///
/// Implementors must be `Send` since callbacks may arrive from different threads.
pub trait Plugin: Send {
    fn parse_config(&mut self, _config: &serde_json::Value) -> Result {
        Ok(())
    }
    fn start(&mut self) -> Result {
        Ok(())
    }
    fn stop(&mut self) -> Result {
        Ok(())
    }
    fn poll_one(&mut self) -> Result {
        Ok(())
    }
    fn signal(&mut self, _event: SignalEvent<'_>) -> Result {
        Ok(())
    }
    fn audio_stream(&mut self, _call: &TrCall, _recorder: &TrRecorder, _samples: &[i16]) -> Result {
        Ok(())
    }
    fn call_start(&mut self, _call: &TrCall) -> Result {
        Ok(())
    }
    fn call_end(&mut self, _call_data: &TrCallData) -> Result {
        Ok(())
    }
    fn calls_active(&mut self, _calls: &[&TrCall]) -> Result {
        Ok(())
    }
    fn setup_recorder(&mut self, _recorder: &TrRecorder) -> Result {
        Ok(())
    }
    fn setup_system(&mut self, _system: &TrSystem) -> Result {
        Ok(())
    }
    fn setup_systems(&mut self, _systems: &[&TrSystem]) -> Result {
        Ok(())
    }
    fn setup_sources(&mut self) -> Result {
        Ok(())
    }
    fn setup_config(&mut self, _systems: &[&TrSystem]) -> Result {
        Ok(())
    }
    fn trunk_message(&mut self, _messages: &[TrTrunkMessage], _system: &TrSystem) -> Result {
        Ok(())
    }
    fn system_rates(&mut self, _systems: &[&TrSystem], _time_diff: f32) -> Result {
        Ok(())
    }
    fn unit_registration(&mut self, _system: &TrSystem, _source_id: i64) -> Result {
        Ok(())
    }
    fn unit_deregistration(&mut self, _system: &TrSystem, _source_id: i64) -> Result {
        Ok(())
    }
    fn unit_acknowledge_response(&mut self, _system: &TrSystem, _source_id: i64) -> Result {
        Ok(())
    }
    fn unit_group_affiliation(
        &mut self,
        _system: &TrSystem,
        _source_id: i64,
        _talkgroup: i64,
    ) -> Result {
        Ok(())
    }
    fn unit_data_grant(&mut self, _system: &TrSystem, _source_id: i64) -> Result {
        Ok(())
    }
    fn unit_answer_request(
        &mut self,
        _system: &TrSystem,
        _source_id: i64,
        _talkgroup: i64,
    ) -> Result {
        Ok(())
    }
    fn unit_location(&mut self, _system: &TrSystem, _source_id: i64, _talkgroup: i64) -> Result {
        Ok(())
    }
}

// ── Adapter: wraps a Plugin impl to satisfy RustPluginCallbacks ──

/// Converts between the raw FFI callback interface and the safe Plugin trait.
struct PluginAdapter<P: Plugin> {
    inner: P,
}

impl<P: Plugin> PluginAdapter<P> {
    const fn new(plugin: P) -> Self {
        Self { inner: plugin }
    }

    /// Convert a slice of raw pointers to a vec of references.
    ///
    /// Pointers are provided by the trunk-recorder callback ABI and are expected
    /// to remain valid for the duration of the callback.
    #[allow(unsafe_code)]
    fn ptr_slice_to_refs<T>(ptrs: &[*const T]) -> Vec<&T> {
        ptrs.iter().map(|p| unsafe { &**p }).collect()
    }
}

impl<P: Plugin> trunk_recorder_sys::RustPluginCallbacks for PluginAdapter<P> {
    fn parse_config(&mut self, json_str: &str) -> i32 {
        match serde_json::from_str(json_str) {
            Ok(value) => match self.inner.parse_config(&value) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("trunk-oxide plugin parse_config error: {e}");
                    -1
                }
            },
            Err(e) => {
                eprintln!("trunk-oxide plugin parse_config JSON error: {e}");
                -1
            }
        }
    }

    fn start(&mut self) -> i32 {
        match self.inner.start() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin start error: {e}");
                -1
            }
        }
    }

    fn stop(&mut self) -> i32 {
        match self.inner.stop() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin stop error: {e}");
                -1
            }
        }
    }

    fn poll_one(&mut self) -> i32 {
        match self.inner.poll_one() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin poll_one error: {e}");
                -1
            }
        }
    }

    fn signal(
        &mut self,
        unit_id: i64,
        signaling_type: &str,
        sig_type: TrSignalType,
        call: &TrCall,
        system: &TrSystem,
        recorder: &TrRecorder,
    ) -> i32 {
        let event = SignalEvent {
            unit_id,
            signaling_type: if signaling_type.is_empty() { None } else { Some(signaling_type) },
            signal_type: sig_type,
            call,
            system,
            recorder,
        };
        match self.inner.signal(event) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin signal error: {e}");
                -1
            }
        }
    }

    fn audio_stream(&mut self, call: &TrCall, recorder: &TrRecorder, samples: &[i16]) -> i32 {
        match self.inner.audio_stream(call, recorder, samples) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin audio_stream error: {e}");
                -1
            }
        }
    }

    fn call_start(&mut self, call: &TrCall) -> i32 {
        match self.inner.call_start(call) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin call_start error: {e}");
                -1
            }
        }
    }

    fn call_end(&mut self, call_data: &TrCallData) -> i32 {
        match self.inner.call_end(call_data) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin call_end error: {e}");
                -1
            }
        }
    }

    fn calls_active(&mut self, calls: &[*const TrCall]) -> i32 {
        let refs = Self::ptr_slice_to_refs(calls);
        match self.inner.calls_active(&refs) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin calls_active error: {e}");
                -1
            }
        }
    }

    fn setup_recorder(&mut self, recorder: &TrRecorder) -> i32 {
        match self.inner.setup_recorder(recorder) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin setup_recorder error: {e}");
                -1
            }
        }
    }

    fn setup_system(&mut self, system: &TrSystem) -> i32 {
        match self.inner.setup_system(system) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin setup_system error: {e}");
                -1
            }
        }
    }

    fn setup_systems(&mut self, systems: &[*const TrSystem]) -> i32 {
        let refs = Self::ptr_slice_to_refs(systems);
        match self.inner.setup_systems(&refs) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin setup_systems error: {e}");
                -1
            }
        }
    }

    fn setup_sources(&mut self) -> i32 {
        match self.inner.setup_sources() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin setup_sources error: {e}");
                -1
            }
        }
    }

    fn setup_config(&mut self, systems: &[*const TrSystem]) -> i32 {
        let sys_refs = Self::ptr_slice_to_refs(systems);
        match self.inner.setup_config(&sys_refs) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin setup_config error: {e}");
                -1
            }
        }
    }

    fn trunk_message(&mut self, messages: &[TrTrunkMessage], system: &TrSystem) -> i32 {
        match self.inner.trunk_message(messages, system) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin trunk_message error: {e}");
                -1
            }
        }
    }

    fn system_rates(&mut self, systems: &[*const TrSystem], time_diff: f32) -> i32 {
        let refs = Self::ptr_slice_to_refs(systems);
        match self.inner.system_rates(&refs, time_diff) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin system_rates error: {e}");
                -1
            }
        }
    }

    fn unit_registration(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        match self.inner.unit_registration(system, source_id) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_registration error: {e}");
                -1
            }
        }
    }

    fn unit_deregistration(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        match self.inner.unit_deregistration(system, source_id) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_deregistration error: {e}");
                -1
            }
        }
    }

    fn unit_acknowledge_response(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        match self.inner.unit_acknowledge_response(system, source_id) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_acknowledge_response error: {e}");
                -1
            }
        }
    }

    fn unit_group_affiliation(&mut self, system: &TrSystem, source_id: i64, talkgroup: i64) -> i32 {
        match self
            .inner
            .unit_group_affiliation(system, source_id, talkgroup)
        {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_group_affiliation error: {e}");
                -1
            }
        }
    }

    fn unit_data_grant(&mut self, system: &TrSystem, source_id: i64) -> i32 {
        match self.inner.unit_data_grant(system, source_id) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_data_grant error: {e}");
                -1
            }
        }
    }

    fn unit_answer_request(&mut self, system: &TrSystem, source_id: i64, talkgroup: i64) -> i32 {
        match self.inner.unit_answer_request(system, source_id, talkgroup) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_answer_request error: {e}");
                -1
            }
        }
    }

    fn unit_location(&mut self, system: &TrSystem, source_id: i64, talkgroup: i64) -> i32 {
        match self.inner.unit_location(system, source_id, talkgroup) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("trunk-oxide plugin unit_location error: {e}");
                -1
            }
        }
    }
}

/// Create a [`RustPluginBox`](trunk_recorder_sys::RustPluginBox) from a [`Plugin`] implementation.
pub fn into_plugin_box<P: Plugin + 'static>(plugin: P) -> trunk_recorder_sys::RustPluginBox {
    trunk_recorder_sys::RustPluginBox::new(Box::new(PluginAdapter::new(plugin)))
}

/// Macro to export a Rust plugin as a trunk-recorder shared library plugin.
///
/// Usage:
/// ```ignore
/// struct MyPlugin { /* ... */ }
/// impl trunk_recorder_plugin::Plugin for MyPlugin { /* ... */ }
///
/// trunk_recorder_plugin::export_plugin!(MyPlugin::new());
/// ```
///
/// The expression passed to the macro is evaluated once when the plugin is loaded
/// and must return an instance of a type implementing [`Plugin`].
#[macro_export]
macro_rules! export_plugin {
    ($create_expr:expr) => {
        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_create_plugin() -> *mut $crate::trunk_recorder_sys::RustPluginBox {
            let plugin_box = $crate::into_plugin_box($create_expr);
            Box::into_raw(Box::new(plugin_box))
        }
    };
}
