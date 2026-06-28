// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use coreshift_core::fs::{fadvise, mmap_madvise, readahead, FADV_WILLNEED};
use coreshift_core::{log_info, log_warn};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const TAG:  &str      = "policy:pm:preload";
const ROOT: &str      = "/data/app";
const ABIS: [&str; 4] = ["arm64", "arm", "x86_64", "x86"];

#[derive(Clone, Copy)]
enum Method { Fadvise, Readahead }

struct Target { path: PathBuf, method: Method }

// ── public API ────────────────────────────────────────────────────────────────

/// Level 1 hint: fadvise WILLNEED (light, async kernel prefetch).
pub fn hint_package(pkg: &str) {
    apply_package(pkg, false);
}

/// Level 2 hint: mmap_madvise WILLNEED (strong, forces pages in).
/// Called on timer cycle to promote already-hinted packages.
pub fn promote_package(pkg: &str) {
    apply_package(pkg, true);
}

fn apply_package(pkg: &str, promote: bool) {
    let install_dir = match find_install_dir(pkg) {
        Some(d) => d,
        None    => { log_warn!(TAG, "{pkg}: install dir not found"); return; }
    };
    let targets = discover(&install_dir);
    if targets.is_empty() {
        log_warn!(TAG, "{pkg}: no targets in {}", install_dir.display());
        return;
    }
    let mut bytes = 0u64;
    let mut files = 0usize;
    for t in &targets {
        let result = if promote {
            apply_madvise(&t.path)
        } else {
            apply_fadvise(&t.path, t.method)
        };
        match result {
            Ok(n)  => { bytes += n as u64; files += 1; }
            Err(e) => { log_warn!(TAG, "{pkg}: {}: {e}", t.path.display()); }
        }
    }
    let level = if promote { "promote(madvise)" } else { "hint(fadvise)" };
    log_info!(TAG, "{pkg}: {level} {files} file(s) {bytes}B");
}

// ── discovery ─────────────────────────────────────────────────────────────────

fn find_install_dir(pkg: &str) -> Option<PathBuf> {
    // Old layout: /data/app/<pkg>-<N>/base.apk
    // New layout: /data/app/<hash>/<pkg>-<N>/base.apk
    for entry in fs::read_dir(ROOT).ok()?.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if pkg_dir_match(&s, pkg) {
            let p = entry.path();
            if real_dir(&p) && p.join("base.apk").is_file() { return Some(p); }
        }
        if real_dir(&entry.path()) {
            if let Ok(subs) = fs::read_dir(entry.path()) {
                for sub in subs.flatten() {
                    let sn = sub.file_name();
                    let ss = sn.to_string_lossy();
                    if pkg_dir_match(&ss, pkg) {
                        let p = sub.path();
                        if real_dir(&p) && p.join("base.apk").is_file() { return Some(p); }
                    }
                }
            }
        }
    }
    None
}

fn pkg_dir_match(dir: &str, pkg: &str) -> bool {
    dir.starts_with(pkg) && dir[pkg.len()..].starts_with('-')
}

fn real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn real_file(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_file() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn discover(dir: &Path) -> Vec<Target> {
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

    // Native libs: lib/<isa>/*.so → readahead (mmap'd at load time)
    for isa in ABIS {
        if let Ok(entries) = fs::read_dir(dir.join(format!("lib/{isa}"))) {
            for e in entries.flatten() {
                let p = e.path();
                if real_file(&p) && p.extension().map(|x| x == "so").unwrap_or(false) {
                    out.push(Target { path: p, method: Method::Readahead });
                }
            }
        }
    }

    // OAT artifacts: oat/<isa>/*.{odex,vdex,art} — inside install dir, not dalvik-cache
    for isa in ABIS {
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
    }

    out
}

// ── apply ─────────────────────────────────────────────────────────────────────

fn file_len(path: &Path) -> usize {
    fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0)
}

fn apply_fadvise(path: &Path, method: Method) -> Result<usize, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let len  = file_len(path);
    if len == 0 { return Ok(0); }
    match method {
        Method::Fadvise   => fadvise(file.as_raw_fd(), 0, len, FADV_WILLNEED)
                                .map_err(|e| e.to_string())?,
        Method::Readahead => readahead(file.as_raw_fd(), 0, len)
                                .map_err(|e| e.to_string())?,
    }
    Ok(len)
}

fn apply_madvise(path: &Path) -> Result<usize, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let len  = file_len(path);
    if len == 0 { return Ok(0); }
    mmap_madvise(file.as_raw_fd(), 0, len, false).map_err(|e| e.to_string())?;
    Ok(len)
}
