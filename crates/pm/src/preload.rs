// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

use coreshift_core::fs::{fadvise, readahead, FADV_WILLNEED};
use coreshift_core::{log_info, log_warn};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const TAG:  &str      = "policy:pm:preload";
const ABIS: [&str; 4] = ["arm64", "arm", "x86_64", "x86"];

#[derive(Clone, Copy)]
enum Method { Fadvise, Readahead }

struct Target { path: PathBuf, method: Method }

/// Issue fadvise/readahead hints on all relevant files in the install dir.
/// `install_dir` is the parent of base.apk, resolved directly from `cmd package`.
pub fn hint_package(install_dir: &Path) {
    let targets = discover(install_dir);
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

    // OAT artifacts: oat/<isa>/*.{odex,vdex,art} — inside install dir
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
