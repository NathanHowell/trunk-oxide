//! Driver that links against `libtrunk-recorder.so` and drives it via FFI.
//!
//! This inverts the typical plugin relationship: instead of trunk-recorder
//! loading a Rust plugin via dlopen, this crate links against trunk-recorder
//! as a shared library and drives it directly.
//!
//! Events flow: SDR → C++ gnuradio graph → cxx callbacks → Plugin trait impl

use trunk_config::{Config, FrequencyFormat, Modulation, SourceDriver, SystemType};
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

    /// Build from a parsed TOML config using the builder FFI.
    ///
    /// This bypasses JSON config parsing entirely — Rust owns all config
    /// interpretation and drives C++ via structured setter calls.
    ///
    /// # Errors
    ///
    /// Returns an error if an SDR source fails to open or system setup fails.
    pub fn from_config<P: Plugin + 'static>(
        config: &Config,
        plugin: P,
    ) -> anyhow::Result<Self> {
        let plugin_box = trunk_recorder_plugin::into_plugin_box(plugin);
        let mut ctx = ffi::tr_create_from_builder(Box::new(plugin_box))?;

        // ── Config ──
        if let Some(rec) = &config.recorder {
            ffi::tr_config_set_temp_dir(ctx.pin_mut(), &rec.temp_dir);
            ffi::tr_config_set_call_timeout(ctx.pin_mut(), rec.call_timeout);
            ffi::tr_config_set_control_warn_rate(ctx.pin_mut(), rec.control_warn_rate);
            ffi::tr_config_set_control_retune_limit(ctx.pin_mut(), rec.control_retune_limit);
            ffi::tr_config_set_broadcast_signals(ctx.pin_mut(), rec.broadcast_signals);
            ffi::tr_config_set_soft_vocoder(ctx.pin_mut(), rec.soft_vocoder);
            ffi::tr_config_set_record_uu_v_calls(ctx.pin_mut(), rec.record_uu_v_calls);
            ffi::tr_config_set_new_call_from_update(ctx.pin_mut(), rec.new_call_from_update);
            ffi::tr_config_set_frequency_format(
                ctx.pin_mut(),
                match rec.frequency_format {
                    FrequencyFormat::Mhz => 0,
                    FrequencyFormat::Hz => 1,
                },
            );
            ffi::tr_config_set_default_mode(ctx.pin_mut(), &rec.default_mode);
            ffi::tr_config_set_debug_recorder(
                ctx.pin_mut(),
                rec.debug.enabled,
                &rec.debug.address,
                rec.debug.port,
            );
        }

        // ── Sources ──
        for src in &config.source {
            if src.enabled == Some(false) {
                continue;
            }

            let driver_str = match src.driver {
                SourceDriver::Osmosdr => "osmosdr",
                SourceDriver::Usrp => "usrp",
                SourceDriver::Sigmf => "sigmf",
                SourceDriver::Iqfile => "iqfile",
            };

            let idx = ffi::tr_add_source(
                ctx.pin_mut(),
                driver_str,
                &src.device,
                src.center,
                src.rate,
                src.error,
            )?;

            if src.agc {
                ffi::tr_source_set_gain_mode(ctx.pin_mut(), idx, true);
            }
            if src.gain != 0.0 {
                ffi::tr_source_set_gain(ctx.pin_mut(), idx, src.gain);
            }
            if src.ppm != 0.0 {
                ffi::tr_source_set_freq_corr(ctx.pin_mut(), idx, src.ppm);
            }
            if !src.antenna.is_empty() {
                ffi::tr_source_set_antenna(ctx.pin_mut(), idx, &src.antenna);
            }
            if src.silence_frames != 0 {
                ffi::tr_source_set_silence_frames(ctx.pin_mut(), idx, src.silence_frames);
            }
            if let Some(threshold) = src.signal_detector_threshold {
                ffi::tr_source_set_signal_detector_threshold(ctx.pin_mut(), idx, threshold);
            }

            // Named gain stages
            for (name, &value) in &src.gain_settings {
                ffi::tr_source_set_gain_by_name(ctx.pin_mut(), idx, name, value);
            }

            // Individual gain fields (shorthand for common stages)
            let named_gains = [
                ("IF", src.if_gain),
                ("BB", src.bb_gain),
                ("MIX", src.mix_gain),
                ("LNA", src.lna_gain),
                ("TIA", src.tia_gain),
                ("PGA", src.pga_gain),
                ("AMP", src.amp_gain),
                ("VGA", src.vga_gain),
                ("VGA1", src.vga1_gain),
                ("VGA2", src.vga2_gain),
            ];
            for (name, value) in named_gains {
                if value != 0.0 {
                    ffi::tr_source_set_gain_by_name(ctx.pin_mut(), idx, name, value);
                }
            }

            // Recorder slots
            if src.digital_recorders > 0 {
                ffi::tr_source_create_digital_recorders(ctx.pin_mut(), idx, src.digital_recorders);
            }
            if src.analog_recorders > 0 {
                ffi::tr_source_create_analog_recorders(ctx.pin_mut(), idx, src.analog_recorders);
            }
            if src.sigmf_recorders > 0 {
                ffi::tr_source_create_sigmf_recorders(ctx.pin_mut(), idx, src.sigmf_recorders);
            }
        }

        // ── Systems ──
        for (short_name, sys) in &config.system {
            let type_str = match sys.system_type {
                SystemType::Conventional => "conventional",
                SystemType::ConventionalP25 => "conventionalP25",
                SystemType::ConventionalDMR => "conventionalDMR",
                SystemType::ConventionalSigmf => "conventionalSIGMF",
                SystemType::Smartnet => "smartnet",
                SystemType::P25 => "p25",
            };

            let sys_idx = ffi::tr_add_system(ctx.pin_mut(), short_name, type_str)?;

            // Control channels (trunked systems)
            for &freq in &sys.control_channels {
                ffi::tr_system_add_control_channel(ctx.pin_mut(), sys_idx, freq);
            }

            // Modulation
            ffi::tr_system_set_qpsk_mod(
                ctx.pin_mut(),
                sys_idx,
                sys.modulation == Modulation::Qpsk,
            );

            // Audio/filter settings
            ffi::tr_system_set_squelch_db(ctx.pin_mut(), sys_idx, sys.squelch);
            ffi::tr_system_set_analog_levels(ctx.pin_mut(), sys_idx, sys.analog_levels);
            ffi::tr_system_set_digital_levels(ctx.pin_mut(), sys_idx, sys.digital_levels);
            ffi::tr_system_set_tau(ctx.pin_mut(), sys_idx, sys.deemphasis_tau);
            ffi::tr_system_set_max_dev(ctx.pin_mut(), sys_idx, sys.max_dev);
            ffi::tr_system_set_filter_width(ctx.pin_mut(), sys_idx, sys.filter_width);
            ffi::tr_system_set_conversation_mode(ctx.pin_mut(), sys_idx, sys.conversation_mode);
            ffi::tr_system_set_record_unknown(ctx.pin_mut(), sys_idx, sys.record_unknown);

            // Duration limits
            if sys.min_duration > 0.0 {
                ffi::tr_system_set_min_duration(ctx.pin_mut(), sys_idx, sys.min_duration);
            }
            if sys.max_duration > 0.0 {
                ffi::tr_system_set_max_duration(ctx.pin_mut(), sys_idx, sys.max_duration);
            }
            if sys.min_transmission_duration > 0.0 {
                ffi::tr_system_set_min_tx_duration(
                    ctx.pin_mut(),
                    sys_idx,
                    sys.min_transmission_duration,
                );
            }

            // Filtering
            ffi::tr_system_set_hide_encrypted(ctx.pin_mut(), sys_idx, sys.hide_encrypted);
            ffi::tr_system_set_monitor_encrypted(ctx.pin_mut(), sys_idx, sys.monitor_encrypted);
            ffi::tr_system_set_hide_unknown(ctx.pin_mut(), sys_idx, sys.hide_unknown_talkgroups);

            // Decoder options
            ffi::tr_system_set_mdc_enabled(ctx.pin_mut(), sys_idx, sys.decode_mdc);
            ffi::tr_system_set_fsync_enabled(ctx.pin_mut(), sys_idx, sys.decode_fsync);
            ffi::tr_system_set_star_enabled(ctx.pin_mut(), sys_idx, sys.decode_star);
            ffi::tr_system_set_tps_enabled(ctx.pin_mut(), sys_idx, sys.decode_tps);

            // Bandplan
            if let Some(bp) = &sys.bandplan {
                ffi::tr_system_set_bandplan(ctx.pin_mut(), sys_idx, bp);
            }
            if sys.bandplan_base > 0.0 {
                ffi::tr_system_set_bandplan_base(ctx.pin_mut(), sys_idx, sys.bandplan_base);
            }
            if sys.bandplan_high > 0.0 {
                ffi::tr_system_set_bandplan_high(ctx.pin_mut(), sys_idx, sys.bandplan_high);
            }
            if sys.bandplan_spacing > 0.0 {
                ffi::tr_system_set_bandplan_spacing(ctx.pin_mut(), sys_idx, sys.bandplan_spacing);
            }
            if sys.bandplan_offset != 0 {
                ffi::tr_system_set_bandplan_offset(ctx.pin_mut(), sys_idx, sys.bandplan_offset);
            }

            // XOR mask (P25 identity)
            if sys.sys_id != 0 || sys.wacn != 0 || sys.nac != 0 {
                ffi::tr_system_set_xor_mask(
                    ctx.pin_mut(),
                    sys_idx,
                    sys.sys_id,
                    sys.wacn,
                    sys.nac,
                );
            }

            // Multi-site
            if sys.multi_site {
                ffi::tr_system_set_multi_site(ctx.pin_mut(), sys_idx, true);
                ffi::tr_system_set_multi_site_name(
                    ctx.pin_mut(),
                    sys_idx,
                    &sys.multi_site_system_name,
                );
                ffi::tr_system_set_multi_site_number(
                    ctx.pin_mut(),
                    sys_idx,
                    sys.multi_site_system_number as u64,
                );
            }

            // Talkgroups (trunked systems)
            for tg in &sys.talkgroup {
                ffi::tr_system_add_talkgroup(
                    ctx.pin_mut(),
                    sys_idx,
                    tg.id,
                    &tg.mode,
                    tg.priority,
                    tg.preferred_nac,
                );
            }

            // Channels (conventional systems)
            for ch in &sys.channel {
                ffi::tr_system_add_conventional_channel(
                    ctx.pin_mut(),
                    sys_idx,
                    ch.id,
                    ch.freq,
                    ch.tone,
                    ch.squelch_db,
                    ch.signal_detection,
                );
            }

            // Unit tags
            for ut in &sys.unit_tag {
                ffi::tr_system_add_unit_tag(ctx.pin_mut(), sys_idx, &ut.pattern, &ut.tag);
            }
            if let Some(settings) = &sys.unit_tags {
                ffi::tr_system_set_unit_tags_mode(ctx.pin_mut(), sys_idx, &settings.mode);
            }
        }

        // ── Finalize: setup_systems + plugin callbacks ──
        ffi::tr_finalize(ctx.pin_mut())?;

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
