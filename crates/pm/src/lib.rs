// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

mod preload;

use coreshift_core::uid::proc_stat;
use coreshift_core::reactor::{Event, Reactor};
use coreshift_core::unix_socket::{
    connect_unix_stream_named, UnixConnectResult, UnixSocketAddr,
};
use coreshift_core::{log_error, log_info, log_warn};
use coreshift_foreground::cache::UidCache;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Duration;

const TAG:             &str  = "policy:pm";
const FG_SOCKET:       &[u8] = b"coreshift";
const CONSUMER_SOCKET: &[u8] = b"coreshift_pm_consumer";
const WATCH_CMD:       &[u8] = b"watch";
const TOP_APP_PROCS:   &str  = "/dev/cpuset/top-app/cgroup.procs";
const PKG_XML:         &str  = "/data/system/packages.xml";
const DATA_DIR:        &str  = "/data/local/tmp/Utensil";

pub fn run() {
    log_info!(TAG, "start pid={}", std::process::id());

    let mut uid_cache = UidCache::new(DATA_DIR);
    uid_cache.load_or_refresh(PKG_XML);

    let stream = loop {
        match connect_unix_stream_named(
            UnixSocketAddr::Abstract(FG_SOCKET),
            UnixSocketAddr::Abstract(CONSUMER_SOCKET),
        ) {
            Ok(UnixConnectResult::Connected(s)) => break s,
            Ok(UnixConnectResult::InProgress(_)) => {}
            Err(e) => { log_warn!(TAG, "connect @coreshift: {e} — retry in 2s"); }
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    if stream.fd.write_slice(WATCH_CMD).is_err() {
        log_error!(TAG, "write watch cmd failed");
        return;
    }

    let mut reactor = match Reactor::new() {
        Ok(r)  => r,
        Err(e) => { log_error!(TAG, "reactor: {e}"); return; }
    };
    let fg_tok = match reactor.add(&stream.fd, true, false) {
        Ok(t)  => t,
        Err(e) => { log_error!(TAG, "add fg: {e}"); return; }
    };

    // pkg → set of top-app PIDs at last hint. Same PIDs = pages still warm = skip.
    let mut pid_cache: HashMap<String, HashSet<i32>> = HashMap::new();
    let mut events: Vec<Event> = Vec::new();
    let mut buf = [0u8; 256];
    let mut leftover = String::new();

    log_info!(TAG, "watching @coreshift for foreground changes");

    loop {
        events.clear();
        match reactor.wait(&mut events, 1, -1) {
            Err(_) | Ok(0) => continue,
            Ok(_) => {}
        }

        for ev in &events {
            if ev.token != fg_tok { continue; }

            if ev.hangup || ev.error {
                log_warn!(TAG, "@coreshift disconnected");
                return;
            }

            loop {
                match stream.fd.read_slice(&mut buf) {
                    Ok(Some(0)) | Ok(None) => break,
                    Err(_) => { log_warn!(TAG, "read fg socket"); break; }
                    Ok(Some(n)) => {
                        leftover.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            }

            while let Some(nl) = leftover.find('\n') {
                let pkg = leftover[..nl].trim().to_string();
                leftover.drain(..=nl);
                if pkg.is_empty() { continue; }
                on_foreground(&pkg, &mut pid_cache, &mut uid_cache);
            }
        }
    }
}

fn on_foreground(
    pkg:       &str,
    pid_cache: &mut HashMap<String, HashSet<i32>>,
    uid_cache: &mut UidCache,
) {
    let current_pids = top_app_pids_for_pkg(pkg, uid_cache);

    match pid_cache.get(pkg) {
        Some(cached_pids) if *cached_pids == current_pids => {
            log_info!(TAG, "fg={pkg} pids unchanged — cache valid, skip");
            return;
        }
        _ => {}
    }

    if current_pids.is_empty() {
        log_warn!(TAG, "fg={pkg} no matching pids in top-app");
    } else {
        log_info!(TAG, "fg={pkg} pids={:?} — hint", current_pids);
    }

    preload::hint_package(pkg);
    pid_cache.insert(pkg.to_string(), current_pids);
}

/// Read /dev/cpuset/top-app/cgroup.procs, return PIDs whose UID maps to pkg.
fn top_app_pids_for_pkg(pkg: &str, uid_cache: &mut UidCache) -> HashSet<i32> {
    let content = match fs::read_to_string(TOP_APP_PROCS) {
        Ok(s)  => s,
        Err(e) => { log_warn!(TAG, "read {TOP_APP_PROCS}: {e}"); return HashSet::new(); }
    };

    content
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .filter(|&pid| {
            proc_stat(pid)
                .ok()
                .and_then(|s| uid_cache.get_package(s.uid))
                .map(|p| p == pkg)
                .unwrap_or(false)
        })
        .collect()
}
