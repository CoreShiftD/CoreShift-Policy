// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use coreshift_core::android_property::android_property_get;
use coreshift_core::fs::{fadvise, readahead, FADV_WILLNEED};
use coreshift_core::{log_info, log_warn};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const TAG: &str = "policy:pm:preload";

/// Ordered device ISA list from ro.product.cpu.abilist. Resolved once at startup.
pub fn device_abis() -> Vec<String> {
    let raw = android_property_get("ro.product.cpu.abilist")
        .unwrap_or_else(|| android_property_get("ro.product.cpu.abi").unwrap_or_default());
    if raw.is_empty() {
        return vec!["arm64".into()];
    }
    raw.split(',')
        .filter_map(|abi| match abi.trim() {
            "arm64-v8a"   => Some("arm64"),
            "armeabi-v7a" | "armeabi" => Some("arm"),
            "x86_64"      => Some("x86_64"),
            "x86"         => Some("x86"),
            _             => None,
        })
        .map(str::to_string)
        .fold(Vec::new(), |mut acc, s| {
            if !acc.contains(&s) { acc.push(s); }
            acc
        })
}

#[derive(Clone, Copy, Debug)]
enum Method { Fadvise, Readahead }

pub struct ResolvedTarget {
    path:   PathBuf,
    method: Method,
}

/// Resolve all hintable files for a package at load time (once, with full perms).
/// `apk_path` is the full path to base.apk from `cmd package list packages -f`.
/// Returns empty vec if nothing found (e.g. package not yet installed on disk).
pub fn resolve_targets(apk_path: &Path, abis: &[String]) -> Vec<ResolvedTarget> {
    let install_dir = match apk_path.parent() {
        Some(d) => d,
        None    => return Vec::new(),
    };

    let mut out = Vec::new();

    // APKs + .dm dexmetadata at install root → fadvise
    if let Ok(entries) = fs::read_dir(install_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !real_file(&p) { continue; }
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".apk") || name.ends_with(".dm") {
                out.push(ResolvedTarget { path: p, method: Method::Fadvise });
            }
        }
    }

    // Native libs: lib/<isa>/*.so → readahead
    for isa in abis {
        if let Ok(entries) = fs::read_dir(install_dir.join(format!("lib/{isa}"))) {
            for e in entries.flatten() {
                let p = e.path();
                if real_file(&p) && p.extension().map(|x| x == "so").unwrap_or(false) {
                    out.push(ResolvedTarget { path: p, method: Method::Readahead });
                }
            }
        }
    }

    // OAT artifacts
    if install_dir.starts_with("/data/app") {
        // user app: oat/ lives inside install dir
        for isa in abis {
            if let Ok(entries) = fs::read_dir(install_dir.join(format!("oat/{isa}"))) {
                for e in entries.flatten() {
                    let p = e.path();
                    if !real_file(&p) { continue; }
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    let method = if name.ends_with(".art") {
                        Method::Fadvise
                    } else if name.ends_with(".odex") || name.ends_with(".vdex") {
                        Method::Readahead
                    } else {
                        continue;
                    };
                    out.push(ResolvedTarget { path: p, method });
                }
            }
        }
    } else {
        // system/vendor/product app: OAT in /data/dalvik-cache/<isa>/
        // Path encoding: strip leading '/', replace '/' with '@'.
        // Files: <encoded-apk>@classes.{vdex,odex,art}
        if let Some(apk_str) = apk_path.to_str() {
            let encoded = apk_str.trim_start_matches('/').replace('/', "@");
            for isa in abis {
                let base = PathBuf::from(format!("/data/dalvik-cache/{isa}/{encoded}"));
                for (suffix, method) in &[
                    ("@classes.vdex", Method::Readahead),
                    ("@classes.odex", Method::Readahead),
                    ("@classes.art",  Method::Fadvise),
                ] {
                    let p = PathBuf::from(format!("{}{suffix}", base.display()));
                    if real_file(&p) {
                        log_info!(TAG, "resolved dalvik-cache: {}", p.display());
                        out.push(ResolvedTarget { path: p, method: *method });
                    }
                }
            }
        }
    }

    out
}

/// Apply pre-resolved hints. Called on every foreground change.
pub fn hint_resolved(install_dir: &Path, targets: &[ResolvedTarget]) {
    if targets.is_empty() {
        log_warn!(TAG, "no targets for {}", install_dir.display());
        return;
    }
    let mut bytes = 0u64;
    let mut files = 0usize;
    for t in targets {
        match apply(&t.path, t.method) {
            Ok(n)  => {
                log_info!(TAG, "  hint {:?} {}", t.method, t.path.display());
                bytes += n as u64; files += 1;
            }
            Err(e) => { log_warn!(TAG, "{}: {e}", t.path.display()); }
        }
    }
    log_info!(TAG, "{}: hinted {files} file(s) {bytes}B", install_dir.display());
}

// ── internals ─────────────────────────────────────────────────────────────────

fn real_file(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn apply(path: &Path, method: Method) -> Result<usize, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let len  = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    if len == 0 { return Ok(0); }
    match method {
        Method::Fadvise   => fadvise(file.as_raw_fd(), 0, len, FADV_WILLNEED)
                                .map_err(|e| e.to_string())?,
        Method::Readahead => readahead(file.as_raw_fd(), 0, len)
                                .map_err(|e| e.to_string())?,
    }
    Ok(len)
}
