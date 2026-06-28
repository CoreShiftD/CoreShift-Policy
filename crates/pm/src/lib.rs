// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

mod preload;

use coreshift_core::reactor::{Event, Fd, Reactor};
use coreshift_core::unix_socket::{
    connect_unix_stream_named, UnixConnectResult, UnixSocketAddr,
};
use coreshift_core::{log_error, log_info, log_warn};
use std::collections::HashSet;
use std::time::Duration;

const TAG:             &str  = "policy:pm";
const FG_SOCKET:       &[u8] = b"coreshift";
const CONSUMER_SOCKET: &[u8] = b"coreshift_pm_consumer";
const WATCH_CMD:       &[u8] = b"watch";
// Re-hint interval: 30 minutes. Kernel may evict pages under memory pressure;
// the timerfd acts as the cache invalidation boundary.
const REHINT_INTERVAL: Duration = Duration::from_secs(30 * 60);

pub fn run() {
    log_info!(TAG, "start pid={}", std::process::id());

    let stream = loop {
        match connect_unix_stream_named(
            UnixSocketAddr::Abstract(FG_SOCKET),
            UnixSocketAddr::Abstract(CONSUMER_SOCKET),
        ) {
            Ok(UnixConnectResult::Connected(s)) => break s,
            Ok(UnixConnectResult::InProgress(_)) => {}
            Err(e) => {
                log_warn!(TAG, "connect @coreshift: {e} — retry in 2s");
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    // subscribe to foreground change stream
    if stream.fd.write_slice(WATCH_CMD).is_err() {
        log_error!(TAG, "write watch cmd failed");
        return;
    }

    let timer = match Fd::timerfd() {
        Ok(f)  => f,
        Err(e) => { log_error!(TAG, "timerfd: {e}"); return; }
    };
    if let Err(e) = timer.set_timer_oneshot(Some(REHINT_INTERVAL)) {
        log_error!(TAG, "arm timer: {e}"); return;
    }

    let mut reactor = match Reactor::new() {
        Ok(r)  => r,
        Err(e) => { log_error!(TAG, "reactor: {e}"); return; }
    };
    let fg_tok    = match reactor.add(&stream.fd, true, false) {
        Ok(t)  => t,
        Err(e) => { log_error!(TAG, "add fg: {e}"); return; }
    };
    let timer_tok = match reactor.add(&timer, true, false) {
        Ok(t)  => t,
        Err(e) => { log_error!(TAG, "add timer: {e}"); return; }
    };

    let mut cached: HashSet<String> = HashSet::new();
    let mut events: Vec<Event>      = Vec::new();
    let mut buf = [0u8; 256];
    let mut leftover = String::new();

    log_info!(TAG, "watching @coreshift for foreground changes");

    loop {
        events.clear();
        match reactor.wait(&mut events, 2, -1) {
            Err(_) | Ok(0) => continue,
            Ok(_) => {}
        }

        for ev in &events {
            // ── foreground socket readable ────────────────────────────────
            if ev.token == fg_tok {
                if ev.hangup || ev.error {
                    log_warn!(TAG, "@coreshift disconnected — reconnect not implemented");
                    return;
                }
                loop {
                    match stream.fd.read_slice(&mut buf) {
                        Ok(Some(0)) | Ok(None) => break,
                        Err(_) => { log_warn!(TAG, "read fg socket"); break; }
                        Ok(Some(n)) => {
                            leftover.push_str(
                                &String::from_utf8_lossy(&buf[..n])
                            );
                        }
                    }
                }
                // consume complete lines — each is a package name
                while let Some(nl) = leftover.find('\n') {
                    let pkg = leftover[..nl].trim().to_string();
                    leftover.drain(..=nl);
                    if pkg.is_empty() { continue; }
                    if cached.contains(&pkg) {
                        log_info!(TAG, "fg={pkg} (cached, skip)");
                    } else {
                        log_info!(TAG, "fg={pkg} (new, hint)");
                        preload::hint_package(&pkg);
                        cached.insert(pkg);
                    }
                }
            }

            // ── timer fired: promote all cached packages to madvise ───────
            if ev.token == timer_tok {
                let mut drain_buf = [0u8; 8];
                while let Ok(Some(_)) = timer.read_slice(&mut drain_buf) {}
                log_info!(TAG, "promote cycle — {} pkg(s)", cached.len());
                for pkg in &cached {
                    preload::promote_package(pkg);
                }
                // re-arm for next cycle
                if let Err(e) = timer.set_timer_oneshot(Some(REHINT_INTERVAL)) {
                    log_error!(TAG, "re-arm timer: {e}");
                }
            }
        }
    }
}
