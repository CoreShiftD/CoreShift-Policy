// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

mod deepsleep;
mod watchdog;

use coreshift_core::reactor::Fd;
use coreshift_core::signal::{SignalRuntime, SIGINT, SIGTERM};
use coreshift_core::{log_error, log_info};
use coreshift_foreground::config::Config;
use coreshift_foreground::daemon::Daemon;
use utensil_ds::binder_calls::BinderCtx;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

const TAG:      &str = "utensil-daemon";
const DATA_DIR: &str = "/data/local/tmp/Utensil";
const PKG_XML:  &str = "/data/system/packages.xml";
const FG_CONF:  &str = "/data/local/tmp/coreshift/coreshift.conf";

fn main() {
    log_info!(TAG, "start pid={}", std::process::id());

    let _ = std::fs::create_dir_all(DATA_DIR);

    // ── shared idle state (prop watcher → WD reactor) ─────────────────────
    let is_deep  = Arc::new(AtomicBool::new(false));
    let idle_efd = Arc::new(Fd::eventfd(0).expect("idle eventfd"));

    // pre-spawn prop watcher — lives forever, never recreated
    watchdog::spawn_prop_watcher(is_deep.clone(), idle_efd.clone());

    // ── foreground daemon (wholesale) ─────────────────────────────────────
    thread::spawn(|| {
        let config = Config::load(FG_CONF);
        let mut daemon = Daemon::new(config);
        if let Err(e) = daemon.run() {
            log_error!("policy:fg", "exited: {e}");
        }
    });

    // ── battery stats daemon (wholesale) ──────────────────────────────────
    thread::spawn(|| {
        utensil_bs::run_daemon();
    });

    // ── DS thread ─────────────────────────────────────────────────────────
    let ctx = Arc::new(BinderCtx::open().unwrap_or_else(|e| {
        log_error!(TAG, "binder init: {e}");
        std::process::exit(1);
    }));
    thread::spawn(move || deepsleep::run(ctx));

    // ── WD thread ─────────────────────────────────────────────────────────
    thread::spawn(move || watchdog::run(DATA_DIR, PKG_XML, is_deep, idle_efd));

    // ── main thread: block on signals ─────────────────────────────────────
    let mask = SignalRuntime::set_with(&[SIGTERM, SIGINT]).unwrap_or_else(|e| {
        log_error!(TAG, "signals: {e}"); std::process::exit(1);
    });
    SignalRuntime::block_current_thread(&mask).ok();
    let signal_fd = SignalRuntime::signalfd_new(&mask).unwrap_or_else(|e| {
        log_error!(TAG, "signalfd: {e}"); std::process::exit(1);
    });

    let mut buf = [0u8; 128];
    loop {
        match signal_fd.read_slice(&mut buf) {
            Ok(Some(_)) => {
                log_info!(TAG, "signal — shutting down");
                std::process::exit(0);
            }
            _ => continue,
        }
    }
}
