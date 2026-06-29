// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use coreshift_core::android_property::android_property_set;
use coreshift_core::reactor::Fd;
use coreshift_core::{log_info, log_warn};
use utensil_ds::binder_calls::BinderCtx;
use utensil_ds::idle_fsm::{make_cancel, run as run_idle_fsm};
use utensil_ds::prop_wait::ScreenProp;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TAG: &str = "policy:ds";
pub const IDLE_STATE_PROP: &str = "debug.tracing.idle_state";

/// DS thread entry. Watches screen state, spawns/cancels FSM, writes IDLE_STATE_PROP.
pub fn run(ctx: Arc<BinderCtx>) {
    let mut screen = loop {
        match ScreenProp::open() {
            Some(s) => break s,
            None => {
                log_warn!(TAG, "screen_state property not found — retry in 5s");
                thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let _ = android_property_set(IDLE_STATE_PROP, "none");

    let mut cancel: Option<Arc<Fd>>              = None;
    let mut fsm_handle: Option<thread::JoinHandle<()>> = None;

    loop {
        let value = match screen.wait_change() {
            Some(v) => v,
            None    => { log_warn!(TAG, "screen_state wait failed; retry"); continue; }
        };
        let screen_on = value.trim() == "2";
        log_info!(TAG, "screen_state={value:?} on={screen_on}");

        // cancel any running FSM
        if let Some(f) = cancel.take() { let _ = f.write_u64(1); }
        if let Some(h) = fsm_handle.take() { let _ = h.join(); }

        if screen_on {
            let _ = android_property_set(IDLE_STATE_PROP, "none");
        } else {
            let cancel_fd = make_cancel();
            cancel        = Some(cancel_fd.clone());
            let ctx_ref   = ctx.clone();
            fsm_handle = Some(thread::spawn(move || {
                run_idle_fsm(&ctx_ref, cancel_fd, Some(IDLE_STATE_PROP));
            }));
        }
    }
}
