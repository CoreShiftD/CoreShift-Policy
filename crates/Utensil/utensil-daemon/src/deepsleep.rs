// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use coreshift_core::android_property::android_property_set;
use coreshift_core::reactor::{Fd, Reactor};
use coreshift_core::unix_socket::{bind_unix_listener, UnixSocketAddr, UnixSocketBindOptions};
use coreshift_core::{log_error, log_info, log_warn};
use utensil_ds::binder_calls::BinderCtx;
use utensil_ds::idle_fsm::{make_cancel, run as run_idle_fsm};
use utensil_ds::screen_source::ScreenSource;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TAG: &str = "policy:ds";
pub const IDLE_STATE_PROP: &str = "debug.tracing.idle_state";
pub const SCREEN_SOCKET: &[u8]  = b"coreshift_screen_state";

// -1 = unknown, 0 = off, 1 = on
static SCREEN_STATE: AtomicI8 = AtomicI8::new(-1);

/// Serve current screen state to any connecting client, then close.
/// Runs in its own thread — non-blocking accept loop via epoll.
fn serve_screen_socket() {
    let listener = match bind_unix_listener(
        UnixSocketAddr::Abstract(SCREEN_SOCKET),
        UnixSocketBindOptions::default(),
    ) {
        Ok(l) => l,
        Err(e) => { log_error!(TAG, "screen socket bind: {e}"); return; }
    };
    let mut reactor = match Reactor::new() {
        Ok(r) => r,
        Err(e) => { log_error!(TAG, "screen socket reactor: {e}"); return; }
    };
    let tok = match reactor.add(&listener.fd, true, false) {
        Ok(t) => t,
        Err(e) => { log_error!(TAG, "screen socket reactor.add: {e}"); return; }
    };
    let mut events = Vec::new();
    loop {
        events.clear();
        match reactor.wait(&mut events, 1, -1) {
            Err(_) | Ok(0) => continue,
            Ok(_) => {}
        }
        for ev in &events {
            if ev.token != tok { continue; }
            while let Ok(Some(stream)) = listener.accept() {
                let msg = match SCREEN_STATE.load(Ordering::Relaxed) {
                    1  => b"on\n"  as &[u8],
                    0  => b"off\n" as &[u8],
                    _  => b"unknown\n" as &[u8],
                };
                let _ = stream.fd.write_slice(msg);
            }
        }
    }
}

/// DS thread entry. Watches screen state, spawns/cancels FSM, writes IDLE_STATE_PROP.
pub fn run(ctx: Arc<BinderCtx>) {
    thread::spawn(serve_screen_socket);

    let mut screen = loop {
        match ScreenSource::open() {
            Some(s) => break s,
            None => {
                log_warn!(TAG, "screen source unavailable — retry in 5s");
                thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let _ = android_property_set(IDLE_STATE_PROP, "none");

    let mut cancel: Option<Arc<Fd>>              = None;
    let mut fsm_handle: Option<thread::JoinHandle<()>> = None;

    loop {
        let screen_on = match screen.wait_screen_on() {
            Some(v) => v,
            None    => { log_warn!(TAG, "screen wait failed; retry"); continue; }
        };
        log_info!(TAG, "screen_on={screen_on}");
        SCREEN_STATE.store(if screen_on { 1 } else { 0 }, Ordering::Relaxed);

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
