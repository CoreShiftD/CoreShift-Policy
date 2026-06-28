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

/// Ordered list of ISAs to probe, derived once from ro.product.cpu.abilist.
/// Falls back to arm64 only if the property is missing.
pub fn device_abis() -> Vec<String> {
    let raw = android_property_get("ro.product.cpu.abilist")
        .unwrap_or_else(|| android_property_get("ro.product.cpu.abi").unwrap_or_default());
    if raw.is_empty() {
        return vec!["arm64".into()];
    }
    // ABI names map to ISA dir names: arm64-v8a → arm64, armeabi-v7a → arm, etc.
    raw.split(',')
        .filter_map(|abi| match abi.trim() {
            "arm64-v8a"   => Some("arm64"),
            "armeabi-v7a" => Some("arm"),
            "armeabi"     => Some("arm"),
            "x86_64"      => Some("x86_64"),
            "x86"         => Some("x86"),
            _             => None,
        })
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
        .fold(Vec::new(), |mut acc, s| { if !acc.contains(&s) { acc.push(s); } acc })
}

#[derive(Clone, Copy)]
enum Method { Fadvise, Readahead }

struct Target { path: PathBuf, method: Method }

/// Issue fadvise/readahead hints on all relevant files in the install dir.
/// `install_dir` is the parent of base.apk, resolved directly from `cmd package`.
/// `abis` is the ordered device ISA list from `device_abis()`.
pub fn hint_package(install_dir: &Path, abis: &[String]) {
    let targets = discover(install_dir, abis);
    if targets.is_empty() {
        log_warn!(TAG, "no targets in {}", install_dir.display());
        return;
    }
    let mut bytes = 0u64;
    let mut files = 0usize;
    for t in &targets {
        match apply(&t.path, t.method) {
            Ok(n)  => { bytes += n as u64; files += 1; }
            Err(e) => { log_warn!(TAG, "{}: {e}", t.path.display()); }
        }
    }
    log_info!(TAG, "{}: hinted {files} file(s) {bytes}B", install_dir.display());
}

// ── discovery ─────────────────────────────────────────────────────────────────

fn real_file(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn discover(dir: &Path, abis: &[String]) -> Vec<Target> {
    let mut out = Vec::new();

    // APKs + .dm dexmetadata at install root → fadvise (sequential reads)
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !real_file(&p) { continue; }
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".apk") || name.ends_with(".dm") {
                out.push(Target { path: p, method: Method::Fadvise });
            }
        }
    }

    // Native libs: lib/<isa>/*.so — only device-supported ISAs
    for isa in abis {
        if let Ok(entries) = fs::read_dir(dir.join(format!("lib/{isa}"))) {
            for e in entries.flatten() {
                let p = e.path();
                if real_file(&p) && p.extension().map(|x| x == "so").unwrap_or(false) {
                    out.push(Target { path: p, method: Method::Readahead });
                }
            }
        }
    }

    // OAT artifacts: oat/<isa>/*.{odex,vdex,art} — only device-supported ISAs
    // User apps: oat/ lives inside the install dir.
    // System apps (/system, /system_ext, /vendor, /product): oat lives in
    //   /data/dalvik-cache/<isa>/<encoded-path>@classes.{vdex,odex,art}
    //   where the encoded path is the apk path with '/' replaced by '@'.
    let in_data = dir.starts_with("/data/app");
    for isa in abis {
        if in_data {
            // user app — oat inside install dir
            if let Ok(entries) = fs::read_dir(dir.join(format!("oat/{isa}"))) {
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
                    out.push(Target { path: p, method });
                }
            }
        } else {
            // system/vendor/product app — oat in dalvik-cache
            // encode each apk's path and probe all matching artifacts
            let cache_dir = PathBuf::from(format!("/data/dalvik-cache/{isa}"));
            if let Ok(entries) = fs::read_dir(&cache_dir) {
                // collect apk names in this install dir to match against
                let apk_names: Vec<String> = out.iter()
                    .filter(|t| t.path.extension().map(|e| e == "apk").unwrap_or(false))
                    .filter_map(|t| {
                        // encode: strip leading '/', replace '/' with '@'
                        t.path.to_str().map(|s| s.trim_start_matches('/').replace('/', "@"))
                    })
                    .collect();

                for e in entries.flatten() {
                    let p = e.path();
                    if !real_file(&p) { continue; }
                    let fname = p.file_name().unwrap_or_default().to_string_lossy();
                    // match prefix against any of our apk encoded names
                    let matched = apk_names.iter().any(|enc| fname.starts_with(enc.as_str()));
                    if !matched { continue; }
                    let method = if fname.ends_with(".art") {
                        Method::Fadvise
                    } else if fname.ends_with(".odex") || fname.ends_with(".vdex") {
                        Method::Readahead
                    } else {
                        continue;
                    };
                    out.push(Target { path: p, method });
                }
            }
        }
    }

    out
}

// ── apply ─────────────────────────────────────────────────────────────────────

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
