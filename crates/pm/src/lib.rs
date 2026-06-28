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
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const TAG:             &str  = "policy:pm";
const FG_SOCKET:       &[u8] = b"coreshift";
const CONSUMER_SOCKET: &[u8] = b"coreshift_pm_consumer";
const WATCH_CMD:       &[u8] = b"watch";
const TOP_APP_PROCS:   &str  = "/dev/cpuset/top-app/cgroup.procs";

struct PkgInfo {
    uid:         u32,
    install_dir: PathBuf,
}

pub fn run() {
    log_info!(TAG, "start pid={}", std::process::id());

    // pkg → (uid, install_dir) for third-party apps only
    let pkg_map = load_third_party_packages();
    log_info!(TAG, "loaded {} third-party pkg(s)", pkg_map.len());

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

    // pkg → set of top-app PIDs at last hint
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
                on_foreground(&pkg, &pkg_map, &mut pid_cache);
            }
        }
    }
}

fn on_foreground(
    pkg:       &str,
    pkg_map:   &HashMap<String, PkgInfo>,
    pid_cache: &mut HashMap<String, HashSet<i32>>,
) {
    let info = match pkg_map.get(pkg) {
        Some(i) => i,
        None    => {
            log_info!(TAG, "fg={pkg} not third-party, skip");
            return;
        }
    };

    let current_pids = top_app_pids_for_uid(info.uid);

    match pid_cache.get(pkg) {
        Some(cached) if *cached == current_pids => {
            log_info!(TAG, "fg={pkg} pids unchanged — skip");
            return;
        }
        _ => {}
    }

    log_info!(TAG, "fg={pkg} uid={} pids={:?} — hint", info.uid, current_pids);
    preload::hint_package(&info.install_dir);
    pid_cache.insert(pkg.to_string(), current_pids);
}

/// Read top-app PIDs and return those belonging to the given UID.
fn top_app_pids_for_uid(uid: u32) -> HashSet<i32> {
    let content = match fs::read_to_string(TOP_APP_PROCS) {
        Ok(s)  => s,
        Err(e) => { log_warn!(TAG, "read {TOP_APP_PROCS}: {e}"); return HashSet::new(); }
    };
    content
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .filter(|&pid| {
            proc_stat(pid).ok().map(|s| s.uid == uid).unwrap_or(false)
        })
        .collect()
}

// ── package list ──────────────────────────────────────────────────────────────

/// Run `cmd package list packages -f -U -3` and parse into pkg → PkgInfo.
/// Output line format: `package:<apk_path>=<pkg> uid:<uid>`
fn load_third_party_packages() -> HashMap<String, PkgInfo> {
    let output = match Command::new("cmd")
        .args(["package", "list", "packages", "-f", "-U", "-3"])
        .output()
    {
        Ok(o)  => o,
        Err(e) => { log_warn!(TAG, "cmd package: {e}"); return HashMap::new(); }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = HashMap::new();

    for line in stdout.lines() {
        if let Some(info) = parse_package_line(line.trim()) {
            map.insert(info.0, info.1);
        }
    }

    map
}

/// Parse `package:/path/to/base.apk=com.pkg uid:10234` → `(pkg, PkgInfo)`.
fn parse_package_line(line: &str) -> Option<(String, PkgInfo)> {
    // split on space: ["package:<path>=<pkg>", "uid:<uid>"]
    let mut parts = line.splitn(2, ' ');
    let pkg_part = parts.next()?.strip_prefix("package:")?;
    let uid_part = parts.next()?.strip_prefix("uid:")?;

    let uid: u32 = uid_part.trim().parse().ok()?;

    // pkg_part = "<path>=<pkg>" — split on last '='
    let eq = pkg_part.rfind('=')?;
    let apk_path = PathBuf::from(&pkg_part[..eq]);
    let pkg      = pkg_part[eq + 1..].to_string();

    // install_dir = parent of base.apk
    let install_dir = apk_path.parent()?.to_path_buf();

    Some((pkg, PkgInfo { uid, install_dir }))
}
