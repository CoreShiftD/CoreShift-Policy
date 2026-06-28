// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/

mod deepsleep;
mod watchdog;

use coreshift_core::android_property::android_property_get;
use coreshift_core::process::{
    close_fds_from, fork, redirect_fd_to, redirect_stdio_to_devnull, set_pdeathsig, setsid,
    setpgid, ForkResult,
};
use coreshift_core::reactor::{Fd, Reactor};
use coreshift_core::signal::{signal_ignore, SignalRuntime, SIGHUP, SIGPIPE, SIGINT, SIGTERM};
use coreshift_core::spawn::{ExitStatus, Process};
use coreshift_core::{log_error, log_info};
use coreshift_foreground::config::Config;
use coreshift_foreground::daemon::Daemon;
use utensil_ds::binder_calls::BinderCtx;
use utensil_pm;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const TAG:      &str = "corepolicy";
const DATA_DIR: &str = "/data/local/tmp/Utensil";
const PID_FILE: &str = "/data/local/tmp/Utensil/corepolicy.pid";
const LOG_FILE: &str = "/data/local/tmp/Utensil/corepolicy.log";
const PKG_XML:  &str = "/data/system/packages.xml";
const FG_CONF:  &str = "/data/local/tmp/coreshift/coreshift.conf";

// ── supervisor ────────────────────────────────────────────────────────────────

fn run_supervisor() -> Result<(), Box<dyn std::error::Error>> {
    let pid_file = Path::new(PID_FILE);
    if pid_file.exists() {
        if let Ok(s) = std::fs::read_to_string(pid_file) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                if Process::new(pid).kill(0).is_ok() {
                    println!("daemon already running (pid {pid})");
                    return Ok(());
                }
            }
        }
        // stale PID file
        let _ = std::fs::remove_file(pid_file);
    }

    let _ = std::fs::create_dir_all(DATA_DIR);

    // first fork — parent waits for middle child, then polls PID file
    match unsafe { fork()? } {
        ForkResult::Parent(pid) => {
            let _ = Process::new(pid).wait_blocking();
            let start = Instant::now();
            loop {
                if pid_file.exists() { break; }
                if start.elapsed() > Duration::from_secs(10) {
                    println!("warning: daemon start timed out");
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            return Ok(());
        }
        ForkResult::Child => {}
    }

    // middle child: detach session, then fork supervisor grandchild
    let _ = setsid();
    let _ = setpgid(0, 0);

    match unsafe { fork()? } {
        ForkResult::Parent(_) => std::process::exit(0),
        ForkResult::Child => {}
    }

    // grandchild (supervisor loop)
    unsafe { let _ = redirect_stdio_to_devnull(); }

    let mut crash_count: u64 = 0;
    let mut last_crash_window = Instant::now();

    loop {
        match unsafe { fork()? } {
            ForkResult::Parent(daemon_pid) => {
                let _ = std::fs::write(pid_file, daemon_pid.to_string());
                let status = Process::new(daemon_pid).wait_blocking();
                let _ = std::fs::remove_file(pid_file);

                if let Ok(ExitStatus::Exited(0)) = status {
                    std::process::exit(0);
                }

                crash_count += 1;
                if last_crash_window.elapsed() > Duration::from_secs(10) {
                    crash_count = 1;
                    last_crash_window = Instant::now();
                }
                if crash_count >= 5 {
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(500 * crash_count));
            }
            ForkResult::Child => {
                let _ = set_pdeathsig(SIGTERM);
                unsafe {
                    signal_ignore(SIGHUP);
                    signal_ignore(SIGPIPE);
                }
                close_fds_from(3);

                if let Ok(f) = std::fs::OpenOptions::new()
                    .create(true).append(true).open(LOG_FILE)
                {
                    use std::os::unix::io::IntoRawFd;
                    unsafe { redirect_fd_to(f.into_raw_fd(), 2) };
                }

                run_daemon();
                std::process::exit(0);
            }
        }
    }
}

// ── subcommands ───────────────────────────────────────────────────────────────

fn cmd_stop() -> Result<(), Box<dyn std::error::Error>> {
    let pid_file = Path::new(PID_FILE);
    let s = match std::fs::read_to_string(pid_file) {
        Err(_) => { println!("daemon not running (no PID file)"); return Ok(()); }
        Ok(s) => s,
    };
    let pid: i32 = s.trim().parse()?;
    let process = Process::new(pid);

    if process.kill(0).is_err() {
        // stale PID file — process already dead
        let _ = std::fs::remove_file(pid_file);
        println!("daemon not running (stale PID file removed)");
        return Ok(());
    }

    let _ = process.kill(SIGTERM);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(100));
        // file gone → supervisor cleaned up and exited
        if !pid_file.exists() {
            println!("daemon stopped");
            return Ok(());
        }
        // process dead but file not yet removed (supervisor killed) → clean up
        if Process::new(pid).kill(0).is_err() {
            let _ = std::fs::remove_file(pid_file);
            println!("daemon stopped");
            return Ok(());
        }
    }

    // timeout: force-remove stale file if process is now dead
    if Process::new(pid).kill(0).is_err() {
        let _ = std::fs::remove_file(pid_file);
        println!("daemon stopped (PID file force-removed)");
    } else {
        println!("warning: daemon did not stop within 3s");
    }
    Ok(())
}

fn cmd_status() -> Result<(), Box<dyn std::error::Error>> {
    let pid_file = Path::new(PID_FILE);
    if pid_file.exists() {
        if let Ok(s) = std::fs::read_to_string(pid_file) {
            let pid = s.trim();
            let alive = pid.parse::<i32>().ok()
            .map(|p| Process::new(p).kill(0).is_ok())
            .unwrap_or(false);
            println!("daemon: {} (pid {pid})", if alive { "running" } else { "stale" });
        }
    } else {
        println!("daemon: not running");
    }

    let props = [
        ("screen_state",    "debug.tracing.screen_state"),
        ("idle_state",      "debug.tracing.idle_state"),
        ("charge_state",    "debug.tracing.charge_state"),
        ("watchdog_tick",   "debug.tracing.watchdog_tick"),
    ];
    for (label, prop) in props {
        let val = android_property_get(prop).unwrap_or_else(|| "-".into());
        println!("{label}={val}");
    }

    Ok(())
}

fn print_usage() {
    println!("Usage: corepolicy <command>");
    println!("Commands:");
    println!("  daemon   Start the policy daemon (supervised, detached)");
    println!("  stop     Stop the running daemon");
    println!("  restart  Stop then start the daemon");
    println!("  status   Show daemon state and property snapshot");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { print_usage(); return; }

    let result: Result<(), Box<dyn std::error::Error>> = match args[1].as_str() {
        "daemon"  => run_supervisor(),
        "stop"    => cmd_stop(),
        "restart" => {
            cmd_stop().ok();
            // wait for stale PID file to vanish
            let pid_file = Path::new(PID_FILE);
            let start = Instant::now();
            while pid_file.exists() && start.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(100));
            }
            run_supervisor()
        }
        "status" => cmd_status(),
        _ => { print_usage(); return; }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

// ── daemon core ───────────────────────────────────────────────────────────────

fn run_daemon() {
    log_info!(TAG, "start pid={}", std::process::id());

    let _ = std::fs::create_dir_all(DATA_DIR);

    // Block SIGTERM/SIGINT before spawning any threads so all threads
    // inherit the blocked mask and the signal is only delivered via signalfd.
    let mask = SignalRuntime::set_with(&[SIGTERM, SIGINT]).unwrap_or_else(|e| {
        log_error!(TAG, "signals: {e}"); std::process::exit(1);
    });
    SignalRuntime::block_current_thread(&mask).ok();

    // shared idle state (prop watcher → WD reactor)
    let is_deep  = Arc::new(AtomicBool::new(false));
    let idle_efd = Arc::new(Fd::eventfd(0).expect("idle eventfd"));

    watchdog::spawn_prop_watcher(is_deep.clone(), idle_efd.clone());

    thread::spawn(|| {
        let config = Config::load(FG_CONF);
        let mut daemon = Daemon::new(config);
        if let Err(e) = daemon.run() {
            log_error!("policy:fg", "exited: {e}");
        }
    });

    thread::spawn(|| {
        utensil_bs::run_daemon();
    });

    let ctx = Arc::new(BinderCtx::open().unwrap_or_else(|e| {
        log_error!(TAG, "binder init: {e}");
        std::process::exit(1);
    }));
    thread::spawn(move || deepsleep::run(ctx));

    thread::spawn(move || watchdog::run(DATA_DIR, PKG_XML, is_deep, idle_efd));

    thread::spawn(|| utensil_pm::run());
    let signal_fd = SignalRuntime::signalfd_new(&mask).unwrap_or_else(|e| {
        log_error!(TAG, "signalfd: {e}"); std::process::exit(1);
    });

    let mut sig_reactor = Reactor::new().unwrap_or_else(|e| {
        log_error!(TAG, "sig reactor: {e}"); std::process::exit(1);
    });
    let sig_tok = sig_reactor.add(&signal_fd, true, false).expect("add signalfd");

    let mut buf = [0u8; 128];
    let mut sig_events = Vec::new();
    loop {
        sig_events.clear();
        match sig_reactor.wait(&mut sig_events, 1, -1) {
            Err(_) | Ok(0) => continue,
            Ok(_) => {}
        }
        for ev in &sig_events {
            if ev.token == sig_tok {
                if signal_fd.read_slice(&mut buf).is_ok() {
                    log_info!(TAG, "signal — shutting down");
                    std::process::exit(0);
                }
            }
        }
    }
}
