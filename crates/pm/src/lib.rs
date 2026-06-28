// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

mod preload;

use coreshift_core::reactor::{Event, Reactor};
use coreshift_core::unix_socket::{
    connect_unix_stream_named, UnixConnectResult, UnixSocketAddr,
};
use coreshift_core::spawn::{SpawnOptions, SpawnBackend};
use coreshift_core::{log_error, log_info, log_warn};
use preload::ResolvedTarget;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const TAG:             &str  = "policy:pm";
const FG_SOCKET:       &[u8] = b"coreshift";
const CONSUMER_SOCKET: &[u8] = b"coreshift_pm_consumer";
const WATCH_CMD:       &[u8] = b"watch";

struct PkgInfo {
    apk_path: PathBuf,
    targets:  Option<Vec<ResolvedTarget>>,
}

pub fn run() {
    log_info!(TAG, "start pid={}", std::process::id());

    let abis = preload::device_abis();
    log_info!(TAG, "device abis: {}", abis.join(","));

    let mut pkg_map: HashMap<String, PkgInfo> = HashMap::new();
    refresh_packages(&mut pkg_map);

    'reconnect: loop {

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
                    refresh_packages(&mut pkg_map);
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
                    on_foreground(&pkg, &mut pkg_map, &abis);
                }
            }
        }
    }
}

fn on_foreground(pkg: &str, pkg_map: &mut HashMap<String, PkgInfo>, abis: &[String]) {
    let info = match pkg_map.get_mut(pkg) {
        Some(i) => i,
        None    => {
            log_info!(TAG, "fg={pkg} not in pkg map, skip");
            return;
        }
    };
    if info.targets.is_none() {
        info.targets = Some(preload::resolve_targets(&info.apk_path, abis));
    }
    let targets = info.targets.as_ref().unwrap();
    let install_dir = info.apk_path.parent().unwrap_or(&info.apk_path);
    log_info!(TAG, "fg={pkg} — hint");
    preload::hint_resolved(install_dir, targets);
}

// ── package list ──────────────────────────────────────────────────────────────

fn refresh_packages(map: &mut HashMap<String, PkgInfo>) {
    let argv = vec![
        "/system/bin/cmd".to_string(),
        "package".to_string(), "list".to_string(),
        "packages".to_string(), "-f".to_string(),
    ];
    let out = match SpawnOptions::builder(argv, SpawnBackend::PosixSpawn)
        .capture_stdout()
        .timeout_ms(10_000)
        .build()
        .and_then(|o| o.run())
    {
        Ok(o)  => o,
        Err(e) => { log_warn!(TAG, "cmd package: {e}"); return; }
    };

    let stdout = match std::str::from_utf8(&out.stdout) {
        Ok(s)  => s,
        Err(_) => { log_warn!(TAG, "cmd package: non-utf8 output"); return; }
    };

    let mut seen = 0usize;
    let mut added = 0usize;
    for line in stdout.lines() {
        seen += 1;
        if let Some((pkg, info)) = parse_package_line(line.trim()) {
            if !map.contains_key(&pkg) {
                map.insert(pkg, info);
                added += 1;
            }
        }
    }
    log_info!(TAG, "pkg refresh: {seen} seen, {added} new, {} total", map.len());
}

/// Parse `package:/path/to/base.apk=com.pkg` → `(pkg, PkgInfo)`.
fn parse_package_line(line: &str) -> Option<(String, PkgInfo)> {
    let pkg_part = line.split_whitespace().next()?.strip_prefix("package:")?;
    let eq       = pkg_part.rfind('=')?;
    let apk_path = PathBuf::from(&pkg_part[..eq]);
    let pkg      = pkg_part[eq + 1..].to_string();
    Some((pkg, PkgInfo { apk_path, targets: None }))
}
