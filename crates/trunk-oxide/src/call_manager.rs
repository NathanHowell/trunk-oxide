use std::pin::Pin;

use cxx::UniquePtr;
use trunk_recorder_sys::ffi::{
    self, TrCall, TrContext, TrMessageType, TrMonitoringState, TrState, TrTrunkMessage,
};

/// Configuration extracted from trunk-config for call management decisions.
#[derive(Clone)]
pub struct CallManagerConfig {
    pub call_timeout: f64,
    pub new_call_from_update: bool,
    pub record_uu_v_calls: bool,
}

/// A single active call tracked by the Rust CallManager.
struct CallEntry {
    call: UniquePtr<TrCall>,
}

/// Manages trunked call lifecycle: creation, update, timeout, and conclusion.
///
/// Conventional calls remain in C++ and are managed by `tr_context_tick_conventional`.
pub struct CallManager {
    calls: Vec<CallEntry>,
    config: CallManagerConfig,
}

struct MultiSiteResult {
    is_duplicate: bool,
    is_superseding: bool,
    /// Index into `calls` of the original call (valid when `is_duplicate` is true).
    original_idx: Option<usize>,
}

impl CallManager {
    pub fn new(config: CallManagerConfig) -> Self {
        Self {
            calls: Vec::new(),
            config,
        }
    }

    // ── Message dispatch ──

    pub fn handle_message(&mut self, ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        match msg.message_type {
            TrMessageType::Grant => self.handle_grant(ctx, msg, true),
            TrMessageType::Update => {
                if self.config.new_call_from_update {
                    self.handle_grant(ctx, msg, false);
                } else {
                    self.handle_update(ctx, msg);
                }
            }
            TrMessageType::UuVGrant => {
                if self.config.record_uu_v_calls {
                    self.handle_grant(ctx, msg, true);
                }
            }
            TrMessageType::UuVUpdate => {
                if self.config.record_uu_v_calls {
                    self.handle_update(ctx, msg);
                }
            }
            // All other message types (CONTROL_CHANNEL, REGISTRATION, PATCH_*, etc.)
            // are handled inside C++ by tr_context_drain_messages.
            _ => {}
        }
    }

    // ── Grant handling ──

    fn handle_grant(
        &mut self,
        mut ctx: Pin<&mut TrContext>,
        msg: &TrTrunkMessage,
        is_grant_message: bool,
    ) {
        // Check for exact match on existing calls
        let mut call_found = false;
        for entry in &mut self.calls {
            let call = &*entry.call;
            if ffi::call_get_talkgroup(call) == msg.talkgroup
                && ffi::call_get_sys_num(call) == msg.sys_num
                && ffi::call_get_freq(call) == msg.freq
                && ffi::call_get_tdma_slot(call) == msg.tdma_slot
                && ffi::call_get_phase2_tdma(call) == msg.phase2_tdma
            {
                call_found = true;
                let source_updated = ffi::tr_call_update(entry.call.pin_mut(), msg);
                if source_updated {
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &*entry.call);
                }
            }
        }

        if call_found {
            return;
        }

        // New call
        let multisite = self.check_multisite(&*ctx, msg.talkgroup, msg.sys_num, msg);
        let mut new_call = ffi::tr_context_create_call(ctx.as_mut(), msg);

        if multisite.is_superseding {
            let recording_started =
                ffi::tr_context_try_start_recording(ctx.as_mut(), new_call.pin_mut(), msg);

            if recording_started {
                if let Some(orig_idx) = multisite.original_idx {
                    let orig = &mut self.calls[orig_idx];
                    ffi::tr_call_set_state(orig.call.pin_mut(), TrState::Monitoring);
                    ffi::tr_call_set_monitoring_state(
                        orig.call.pin_mut(),
                        TrMonitoringState::Superseded,
                    );
                    ffi::tr_call_conclude(orig.call.pin_mut());
                }
            }
        } else if multisite.is_duplicate {
            ffi::tr_call_set_state(new_call.pin_mut(), TrState::Monitoring);
            ffi::tr_call_set_monitoring_state(new_call.pin_mut(), TrMonitoringState::Duplicate);
        } else {
            let recording_started =
                ffi::tr_context_try_start_recording(ctx.as_mut(), new_call.pin_mut(), msg);
            if recording_started && !is_grant_message {
                tracing::info!(
                    talkgroup = msg.talkgroup,
                    sys_num = msg.sys_num,
                    "this was an UPDATE"
                );
            }
        }

        self.calls.push(CallEntry {
            call: new_call,
        });

        let last = self.calls.last().unwrap();
        ffi::tr_context_fire_call_start(ctx.as_mut(), &*last.call);

        let ptrs: Vec<*const TrCall> = self.calls.iter().map(|e| &*e.call as *const TrCall).collect();
        ffi::tr_context_fire_calls_active(ctx, &ptrs);
    }

    // ── Update handling ──

    fn handle_update(&mut self, mut ctx: Pin<&mut TrContext>, msg: &TrTrunkMessage) {
        for entry in &mut self.calls {
            let call = &*entry.call;
            if ffi::call_get_talkgroup(call) == msg.talkgroup
                && ffi::call_get_sys_num(call) == msg.sys_num
                && ffi::call_get_freq(call) == msg.freq
                && ffi::call_get_tdma_slot(call) == msg.tdma_slot
                && ffi::call_get_phase2_tdma(call) == msg.phase2_tdma
            {
                if msg.encrypted {
                    ffi::tr_call_set_encrypted(entry.call.pin_mut(), true);
                }

                let source_updated = ffi::tr_call_update(entry.call.pin_mut(), msg);
                if source_updated {
                    ffi::tr_context_fire_call_start(ctx.as_mut(), &*entry.call);
                }
            }
        }
    }

    // ── Multisite duplicate detection ──

    fn check_multisite(&self, ctx: &TrContext, talkgroup: i64, sys_num: i32, msg: &TrTrunkMessage) -> MultiSiteResult {
        let mut result = MultiSiteResult {
            is_duplicate: false,
            is_superseding: false,
            original_idx: None,
        };

        for (idx, entry) in self.calls.iter().enumerate() {
            let call = &*entry.call;

            if ffi::call_get_talkgroup(call) != talkgroup {
                continue;
            }
            if ffi::call_get_sys_num(call) == sys_num {
                continue;
            }
            if !ffi::call_get_system_multi_site(call) {
                continue;
            }
            if ffi::call_get_state(call) != TrState::Recording {
                continue;
            }

            let call_wacn = ffi::call_get_system_wacn(call);
            if call_wacn == 0 {
                continue;
            }

            let call_multi_site_name = ffi::call_get_system_multi_site_name(call);

            // Default mode: match WACN, different RFSS/Site, no multiSiteSystemName
            if call_multi_site_name.is_empty() {
                if call_wacn != msg.wacn {
                    continue;
                }

                let call_rfss = ffi::call_get_system_rfss(call);
                let call_site_id = ffi::call_get_system_site_id(call);
                let call_rfss_site = call_rfss as u64 * 10000 + call_site_id as u64;
                let msg_rfss_site = msg.sys_rfss as u64 * 10000 + msg.sys_site_id as u64;

                if msg_rfss_site == call_rfss_site {
                    continue;
                }

                result.is_duplicate = true;
                result.original_idx = Some(idx);

                // Supersede check: the C++ code compares both the existing call's
                // and the incoming system's preferred NAC. We only have accessors
                // for the existing call's system, not the incoming system. The
                // message carries nac but not the system's preferred NAC for this
                // talkgroup. We can only check the RFSS/Site-based supersede path.
                let call_preferred_nac = ffi::call_system_get_preferred_nac(call, talkgroup);
                if call_preferred_nac != call_rfss_site && msg.nac == msg_rfss_site {
                    result.is_superseding = true;
                }
            }
            // Secondary mode: match multiSiteSystemName.
            else {
                let msg_multi_site_name = ffi::tr_context_get_system_multi_site_name(ctx, sys_num);
                if !msg_multi_site_name.is_empty() && call_multi_site_name == msg_multi_site_name {
                    result.is_duplicate = true;
                    result.original_idx = Some(idx);
                }
            }
        }

        result
    }

    // ── Periodic tick (call timeout management) ──

    pub fn tick(&mut self, mut ctx: Pin<&mut TrContext>) {
        let mut ended_call = false;
        let timeout = self.config.call_timeout;

        self.calls.retain_mut(|entry| {
            let call = &*entry.call;
            let state = ffi::call_get_state(call);
            let since_update = ffi::call_since_last_update_secs(call);

            match state {
                TrState::Monitoring => {
                    if since_update > timeout {
                        ffi::tr_call_conclude(entry.call.pin_mut());
                        ended_call = true;
                        return false; // remove
                    }
                }
                TrState::Recording => {
                    // Check both recorder idle time and control channel update time
                    let since_write = ffi::call_recorder_since_last_write_secs(call);
                    if since_write > timeout || since_update > timeout {
                        ffi::tr_call_conclude(entry.call.pin_mut());
                        ffi::tr_context_fire_setup_recorder(ctx.as_mut(), &*entry.call);
                        ended_call = true;
                        return false;
                    }
                }
                _ => {
                    if since_update > timeout {
                        ffi::tr_call_conclude(entry.call.pin_mut());
                        ended_call = true;
                        return false;
                    }
                }
            }

            true // keep
        });

        if ended_call {
            let ptrs: Vec<*const TrCall> =
                self.calls.iter().map(|e| &*e.call as *const TrCall).collect();
            ffi::tr_context_fire_calls_active(ctx, &ptrs);
        }
    }

    // ── Process P25 recorder message queues ──

    pub fn process_recorder_queues(&self) {
        for entry in &self.calls {
            ffi::call_process_recorder_queue(&*entry.call);
        }
    }

    // ── Shutdown ──

    pub fn conclude_all(&mut self, mut ctx: Pin<&mut TrContext>) {
        for entry in &mut self.calls {
            if ffi::call_get_state(&*entry.call) != TrState::Monitoring {
                ffi::tr_call_conclude(entry.call.pin_mut());
            }
        }
        self.calls.clear();

        let ptrs: Vec<*const TrCall> = Vec::new();
        ffi::tr_context_fire_calls_active(ctx.as_mut(), &ptrs);
    }
}
