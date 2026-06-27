// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use coreshift_core::android_property::{
    android_property_find, android_property_read, android_property_serial,
    android_property_set, android_property_wait,
};
use coreshift_core::reactor::{Event, Fd, Reactor, Token};
use coreshift_core::{log_error, log_info, log_warn};
use utensil_ds::binder_calls::{BinderCtx, IDLE_DEEP, IDLE_LIGHT};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TAG: &str = "policy:ds";
pub const IDLE_STATE_PROP: &str = "debug.tracing.idle_state";

fn wait_fd(
    reactor: &mut Reactor,
    target: Token,
    cancel: Token,
    events: &mut Vec<Event>,
) -> bool {
    loop {
        events.clear();
        match reactor.wait(events, 2, -1) {
            Err(_) | Ok(0) => continue,
            Ok(_) => {}
        }
        for ev in events.iter() {
            if ev.token == cancel { return false; }
            if ev.token == target  { return true; }
        }
    }
}

fn drain(fd: &Fd) {
    let mut buf = [0u8; 8];
    while let Ok(Some(_)) = fd.read_slice(&mut buf) {}
}

fn run_fsm(ctx: Arc<BinderCtx>, cancel: Arc<Fd>) {
    let mut reactor = match Reactor::new() {
        Ok(r) => r,
        Err(e) => { log_error!(TAG, "reactor: {e}"); return; }
    };
    let timer = match Fd::timerfd() {
        Ok(f) => f,
        Err(e) => { log_error!(TAG, "timerfd: {e}"); return; }
    };
    let cancel_tok = match reactor.add(&cancel, true, false) {
        Ok(t) => t,
        Err(e) => { log_error!(TAG, "add cancel: {e}"); return; }
    };
    let timer_tok = match reactor.add(&timer, true, false) {
        Ok(t) => t,
        Err(e) => { log_error!(TAG, "add timer: {e}"); return; }
    };

    let mut events = Vec::new();

    // Phase 1: 90s → light sleep
    log_info!(TAG, "screen off — light idle in 90s");
    if let Err(e) = timer.set_timer_oneshot(Some(Duration::from_secs(90))) {
        log_error!(TAG, "arm 90s: {e}"); return;
    }
    if !wait_fd(&mut reactor, timer_tok, cancel_tok, &mut events) {
        log_info!(TAG, "cancelled (light wait)"); return;
    }
    drain(&timer);
    ctx.note_idle(IDLE_LIGHT);
    let _ = android_property_set(IDLE_STATE_PROP, "light");
    log_info!(TAG, "light idle entered");

    // Phase 2: 360s → deep sleep
    if let Err(e) = timer.set_timer_oneshot(Some(Duration::from_secs(360))) {
        log_error!(TAG, "arm 360s: {e}"); return;
    }
    if !wait_fd(&mut reactor, timer_tok, cancel_tok, &mut events) {
        log_info!(TAG, "cancelled (deep wait)"); return;
    }
    drain(&timer);
    ctx.note_idle(IDLE_DEEP);
    let _ = android_property_set(IDLE_STATE_PROP, "deep");
    log_info!(TAG, "deep idle entered — WD will pause");
}

/// DS thread entry. Watches screen state, spawns/cancels FSM, writes IDLE_STATE_PROP.
pub fn run(ctx: Arc<BinderCtx>) {
    let info = match android_property_find("debug.tracing.screen_state") {
        Some(i) => i,
        None => {
            log_error!(TAG, "screen_state property not found");
            std::process::exit(1);
        }
    };
    let mut serial = match android_property_serial(info) {
        Ok(s) => s,
        Err(e) => { log_error!(TAG, "serial: {e}"); std::process::exit(1); }
    };

    let _ = android_property_set(IDLE_STATE_PROP, "none");

    let mut cancel: Option<Arc<Fd>>         = None;
    let mut fsm_handle: Option<thread::JoinHandle<()>> = None;

    loop {
        let next_serial = match android_property_wait(info, serial, None) {
            Ok(Some(s)) => s,
            Ok(None) => { log_warn!(TAG, "wait returned None; retry"); continue; }
            Err(e)    => { log_warn!(TAG, "wait: {e}; retry"); continue; }
        };
        serial = next_serial;

        let value = match android_property_read(info) {
            Ok(v) => v.value,
            Err(e) => { log_warn!(TAG, "read: {e}"); continue; }
        };
        let screen_on = value.trim() == "2";
        log_info!(TAG, "screen_state={value:?} on={screen_on}");

        // cancel any running FSM
        if let Some(f) = cancel.take() { let _ = f.write_u64(1); }
        if let Some(h) = fsm_handle.take() { let _ = h.join(); }

        if screen_on {
            let _ = android_property_set(IDLE_STATE_PROP, "none");
        } else {
            let cancel_fd    = Arc::new(Fd::eventfd(0).expect("ds cancel eventfd"));
            cancel           = Some(cancel_fd.clone());
            let ctx_ref      = ctx.clone();
            fsm_handle = Some(thread::spawn(move || run_fsm(ctx_ref, cancel_fd)));
        }
    }
}
