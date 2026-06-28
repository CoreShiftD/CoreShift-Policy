// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

mod preload;

use coreshift_core::reactor::{Event, Reactor};
use coreshift_core::unix_socket::{
    connect_unix_stream_named, UnixConnectResult, UnixSocketAddr,
};
use coreshift_core::{log_error, log_info, log_warn};
use preload::ResolvedTarget;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const TAG:             &str  = "policy:pm";
const FG_SOCKET:       &[u8] = b"coreshift";
const CONSUMER_SOCKET: &[u8] = b"coreshift_pm_consumer";
const WATCH_CMD:       &[u8] = b"watch";

struct PkgInfo {
    install_dir: PathBuf,
    targets:     Vec<ResolvedTarget>,
}

pub fn run() {
    log_info!(TAG, "start pid={}", std::process::id());

    let abis = preload::device_abis();
    log_info!(TAG, "device abis: {}", abis.join(","));

    'reconnect: loop {
        let pkg_map = load_packages(&abis);
        log_info!(TAG, "loaded {} pkg(s)", pkg_map.len());

        let stream = loop {
            match connect_unix_stream_named(
                UnixSocketAddr::Abstract(FG_SOCKET),
                UnixSocketAddr::Abstract(CONSUMER_SOCKET),
            ) {
                Ok(UnixConnectResult::Connected(s)) => break s,
                Ok(UnixConnectResult::InProgress(s)) => {
                    std::thread::sleep(Duration::from_millis(200));
                    match s.finish_connect() {
                        Ok(s)  => break s,
                        Err(e) => { log_warn!(TAG, "connect in-progress: {e}"); }
                    }
                }
                Err(e) => { log_warn!(TAG, "connect @coreshift: {e} — retry in 2s"); }
            }
            std::thread::sleep(Duration::from_secs(2));
        };

        if stream.fd.write_slice(WATCH_CMD).is_err() {
            log_warn!(TAG, "write watch cmd failed — retry in 2s");
            std::thread::sleep(Duration::from_secs(2));
            continue 'reconnect;
        }

        let mut reactor = match Reactor::new() {
            Ok(r)  => r,
            Err(e) => { log_error!(TAG, "reactor: {e}"); return; }
        };
        let fg_tok = match reactor.add(&stream.fd, true, false) {
            Ok(t)  => t,
            Err(e) => { log_error!(TAG, "add fg: {e}"); return; }
        };

        let mut events:   Vec<Event> = Vec::new();
        let mut buf:      [u8; 256]  = [0u8; 256];
        let mut leftover: String     = String::new();

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
                    log_warn!(TAG, "@coreshift disconnected — reconnecting");
                    continue 'reconnect;
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
                    on_foreground(&pkg, &pkg_map);
                }
            }
        }
    }
}

fn on_foreground(pkg: &str, pkg_map: &HashMap<String, PkgInfo>) {
    let info = match pkg_map.get(pkg) {
        Some(i) => i,
        None    => {
            log_info!(TAG, "fg={pkg} not in pkg map, skip");
            return;
        }
    };
    log_info!(TAG, "fg={pkg} — hint");
    preload::hint_resolved(&info.install_dir, &info.targets);
}

// ── package list ──────────────────────────────────────────────────────────────

fn load_packages(abis: &[String]) -> HashMap<String, PkgInfo> {
    let output = match Command::new("/system/bin/cmd")
        .args(["package", "list", "packages", "-f"])
        .output()
    {
        Ok(o)  => o,
        Err(e) => { log_warn!(TAG, "cmd package: {e}"); return HashMap::new(); }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = HashMap::new();

    for line in stdout.lines() {
        if let Some(entry) = parse_package_line(line.trim(), abis) {
            map.insert(entry.0, entry.1);
        }
    }

    map
}

/// Parse `package:/path/to/base.apk=com.pkg` → `(pkg, PkgInfo)`.
fn parse_package_line(line: &str, abis: &[String]) -> Option<(String, PkgInfo)> {
    let pkg_part    = line.split_whitespace().next()?.strip_prefix("package:")?;
    let eq          = pkg_part.rfind('=')?;
    let apk_path    = PathBuf::from(&pkg_part[..eq]);
    let pkg         = pkg_part[eq + 1..].to_string();
    let install_dir = apk_path.parent()?.to_path_buf();
    let targets     = preload::resolve_targets(&apk_path, abis);

    Some((pkg, PkgInfo { install_dir, targets }))
}
