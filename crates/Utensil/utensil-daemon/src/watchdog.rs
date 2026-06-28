// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use crate::deepsleep::IDLE_STATE_PROP;
use coreshift_core::android_property::{
    android_property_find, android_property_read, android_property_serial,
    android_property_set, android_property_wait,
};
use coreshift_core::reactor::{Event, Fd, Reactor};
use coreshift_core::unix_socket::{
    connect_unix_stream_named, UnixConnectResult, UnixSocketAddr,
};
use coreshift_core::{log_error, log_info, log_warn};
use coreshift_foreground::blocklist::Blocklist;
use coreshift_foreground::cache::UidCache;
use coreshift_foreground::resolver::Resolver;
use coreshift_foreground::terminal_apps::TerminalApps;
use utensil_wd::proc_scan::scan_user_pids;
use utensil_wd::punish::PunishMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const FG_SOCKET:    &[u8] = b"coreshift";
const WD_CONSUMER:  &[u8] = b"coreshift_wd_consumer";

const TAG:      &str = "policy:wd";
const TICK_PROP: &str = "debug.tracing.watchdog_tick";
const INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Pre-spawned once in main. Watches IDLE_STATE_PROP forever, updates is_deep
/// and writes idle_efd so the WD reactor wakes immediately on state change.
pub fn spawn_prop_watcher(is_deep: Arc<AtomicBool>, idle_efd: Arc<Fd>) {
    thread::spawn(move || {
        let info = match android_property_find(IDLE_STATE_PROP) {
            Some(i) => i,
            None => {
                log_error!("policy:wd:prop", "{IDLE_STATE_PROP} not found; WD won't freeze");
                return;
            }
        };
        let mut serial = android_property_serial(info).unwrap_or(0);

        loop {
            let next = match android_property_wait(info, serial, None) {
                Ok(Some(s)) => s,
                Ok(None) => { log_warn!("policy:wd:prop", "wait returned None"); continue; }
                Err(e)    => { log_warn!("policy:wd:prop", "wait: {e}"); continue; }
            };
            serial = next;

            let value = match android_property_read(info) {
                Ok(v) => v.value,
                Err(_) => continue,
            };
            let deep = value.trim() == "deep";
            is_deep.store(deep, Ordering::Relaxed);
            log_info!("policy:wd:prop", "idle_state={} deep={deep}", value.trim());
            // wake WD reactor immediately
            let _ = idle_efd.write_u64(1);
        }
    });
}

fn do_tick(
    resolver: &mut Resolver,
    punish: &mut PunishMap,
    launcher: &Option<String>,
    prev_live: &mut HashSet<String>,
    tick: &mut u64,
) {
    *tick += 1;
    let _ = android_property_set(TICK_PROP, &tick.to_string());
    log_info!(TAG, "tick={tick}");

    let fg_pkg = resolver.resolve().map(|(pkg, _, _)| pkg);
    log_info!(TAG, "foreground={:?}", fg_pkg);

    if let Some(ref fg) = fg_pkg {
        punish.reverse(fg);
    }

    let mut live_bg: HashSet<String> = HashSet::new();
    for (_, uid) in scan_user_pids() {
        if let Some(pkg) = resolver.cache.get_package(uid) {
            if resolver.blocklist.is_blocked(&pkg) { continue; }
            if launcher.as_deref() == Some(pkg.as_str()) { continue; }
            if fg_pkg.as_deref() == Some(pkg.as_str()) { continue; }
            live_bg.insert(pkg);
        }
    }

    for pkg in &live_bg {
        if prev_live.contains(pkg) {
            punish.escalate(pkg);
        }
    }
    *prev_live = live_bg;
}

fn fg_connect(reactor: &mut Reactor) -> Option<(coreshift_core::unix_socket::UnixStreamFd, coreshift_core::reactor::Token)> {
    let stream = match connect_unix_stream_named(
        UnixSocketAddr::Abstract(FG_SOCKET),
        UnixSocketAddr::Abstract(WD_CONSUMER),
    ) {
        Ok(UnixConnectResult::Connected(s)) => s,
        Ok(UnixConnectResult::InProgress(s)) => {
            std::thread::sleep(Duration::from_millis(200));
            match s.finish_connect() {
                Ok(s)  => s,
                Err(e) => { log_warn!(TAG, "connect in-progress: {e}"); return None; }
            }
        }
        Err(e) => { log_warn!(TAG, "connect @coreshift: {e}"); return None; }
    };
    if stream.fd.write_slice(b"watch").is_err() {
        log_warn!(TAG, "write watch to @coreshift failed");
        return None;
    }
    match reactor.add(&stream.fd, true, false) {
        Ok(tok) => { log_info!(TAG, "connected @coreshift (instant reverse active)"); Some((stream, tok)) }
        Err(e)  => { log_warn!(TAG, "reactor add fg: {e}"); None }
    }
}

/// WD thread entry. Receives pre-spawned watcher's shared state.
pub fn run(data_dir: &str, pkg_xml: &str, is_deep: Arc<AtomicBool>, idle_efd: Arc<Fd>) {
    let wl_conf = format!("{data_dir}/watchdog_whitelist.conf");
    let ta_conf = format!("{data_dir}/terminal_apps.conf");

    let mut cache = UidCache::new(data_dir);
    cache.load_or_refresh(pkg_xml);

    let launcher  = Blocklist::resolve_launcher();
    let defaults  = Blocklist::resolve_defaults();
    let blocklist = Blocklist::load_or_create(&wl_conf, defaults, false);
    let terminal  = TerminalApps::load_or_create(&ta_conf);
    let mut resolver  = Resolver::new(cache, blocklist, terminal, launcher.clone());
    let mut punish    = PunishMap::new();
    let mut prev_live = HashSet::new();
    let mut tick      = 0u64;

    let mut reactor = Reactor::new().unwrap_or_else(|e| {
        log_error!(TAG, "reactor: {e}"); std::process::exit(1);
    });
    let timer = Fd::timerfd().unwrap_or_else(|e| {
        log_error!(TAG, "timerfd: {e}"); std::process::exit(1);
    });
    let timer_tok = reactor.add(&timer, true, false).expect("add timer");
    let idle_tok  = reactor.add(&idle_efd, true, false).expect("add idle_efd");

    // @coreshift connection — optional: instant reverse degrades to tick-only if unavailable
    let mut fg_state: Option<(coreshift_core::unix_socket::UnixStreamFd, coreshift_core::reactor::Token)> = fg_connect(&mut reactor);
    let mut fg_retry = Instant::now();

    // arm initial timer
    if let Err(e) = timer.set_timer_oneshot(Some(INTERVAL)) {
        log_error!(TAG, "arm: {e}"); std::process::exit(1);
    }
    let mut armed_at: Option<Instant> = Some(Instant::now());
    let mut frozen_remaining: Option<Duration> = None;

    let mut events:   Vec<Event> = Vec::new();
    let mut buf:      [u8; 256]  = [0u8; 256];
    let mut leftover: String     = String::new();

    loop {
        events.clear();
        match reactor.wait(&mut events, 8, -1) {
            Err(_) | Ok(0) => {}
            Ok(_) => {}
        }

        // retry @coreshift connection if disconnected (at most once per 5s)
        if fg_state.is_none() && fg_retry.elapsed() >= Duration::from_secs(5) {
            fg_state = fg_connect(&mut reactor);
            fg_retry = Instant::now();
        }

        for i in 0..events.len() {
            let tok = events[i].token;

            // ── foreground changed → instant reverse ──────────────────────
            if let Some((ref stream, fg_tok)) = fg_state {
                if tok == fg_tok {
                    if events[i].hangup || events[i].error {
                        log_warn!(TAG, "@coreshift disconnected — reverting to tick-only");
                        leftover.clear();
                        fg_state = None;
                        fg_retry = Instant::now();
                        continue;
                    }
                    loop {
                        match stream.fd.read_slice(&mut buf) {
                            Ok(Some(0)) | Ok(None) => break,
                            Err(_) => break,
                            Ok(Some(n)) => {
                                leftover.push_str(&String::from_utf8_lossy(&buf[..n]));
                            }
                        }
                    }
                    while let Some(nl) = leftover.find('\n') {
                        let pkg = leftover[..nl].trim().to_string();
                        leftover.drain(..=nl);
                        if pkg.is_empty() { continue; }
                        log_info!(TAG, "fg={pkg} — instant reverse");
                        punish.reverse(&pkg);
                    }
                }
            }

            // ── idle state changed ────────────────────────────────────────
            if tok == idle_tok {
                let mut ibuf = [0u8; 8];
                while let Ok(Some(_)) = idle_efd.read_slice(&mut ibuf) {}

                if is_deep.load(Ordering::Relaxed) {
                    if let Some(at) = armed_at.take() {
                        let elapsed   = at.elapsed();
                        let remaining = INTERVAL.saturating_sub(elapsed);
                        frozen_remaining = Some(remaining);
                        let _ = timer.set_timer_oneshot(None);
                        log_info!(TAG, "timer frozen — {:.1}s remaining",
                                  remaining.as_secs_f32());
                    }
                } else if let Some(remaining) = frozen_remaining.take() {
                    if let Err(e) = timer.set_timer_oneshot(Some(remaining)) {
                        log_error!(TAG, "re-arm: {e}");
                    } else {
                        armed_at = Some(Instant::now());
                        log_info!(TAG, "timer resumed — {:.1}s remaining",
                                  remaining.as_secs_f32());
                    }
                }
            }

            // ── timer fired ───────────────────────────────────────────────
            if tok == timer_tok {
                let mut tbuf = [0u8; 8];
                while let Ok(Some(_)) = timer.read_slice(&mut tbuf) {}

                do_tick(&mut resolver, &mut punish, &launcher, &mut prev_live, &mut tick);

                // only re-arm if not currently frozen
                if !is_deep.load(Ordering::Relaxed) {
                    if let Err(e) = timer.set_timer_oneshot(Some(INTERVAL)) {
                        log_error!(TAG, "re-arm after tick: {e}");
                    } else {
                        armed_at = Some(Instant::now());
                    }
                }
            }
        }
    }
}
