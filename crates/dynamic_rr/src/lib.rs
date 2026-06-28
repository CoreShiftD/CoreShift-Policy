// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::OpenOptions;
use std::mem::{size_of, MaybeUninit};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::IntoRawFd;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use coreshift_core::android_property::{
    android_property_find, android_property_get, android_property_serial,
    android_property_set, android_property_wait,
};
use coreshift_core::binder::RawBinderService;
use coreshift_core::dex::find_transaction_code;
use coreshift_core::reactor::{Fd, Reactor};
use coreshift_core::spawn::{SpawnOptions, SpawnBackend};
use coreshift_core::{log_info, log_warn};

const TAG: &str = "policy:rr";

const PROP_DYNAMIC: &str = "persist.inoi.refresh.dynamic";
const PROP_LOW:     &str = "persist.inoi.refresh.low";
const PROP_HIGH:    &str = "persist.inoi.refresh.high";

const DEFAULT_IDLE_SEC:   u64 = 5;
const DEFAULT_LOW_RATE:  &str = "60";
const DEFAULT_HIGH_RATE: &str = "120";
const DEFAULT_INPUT:     &str = "/dev/input/event2";

const EV_SYN:     u16 = 0x00;
const SYN_REPORT: u16 = 0x00;
// O_NONBLOCK on Linux/aarch64
const O_NONBLOCK: i32 = 0x800;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    tv_sec:  i64,
    tv_usec: i64,
    type_:   u16,
    code:    u16,
    value:   i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode { Unknown, Low, High }

struct Config {
    idle_ms:       u64,
    default_low:   String,
    default_high:  String,
    input_devices: Vec<String>,
}

struct InputDev {
    path: String,
    fd:   Fd,
}

// ── display binder ────────────────────────────────────────────────────────────

const DISPLAY_JAR:   &str = "/system/framework/framework.jar";
const DISPLAY_STUB:  &str = "Landroid/hardware/display/IDisplayManager$Stub;";
const DISPLAY_FIELD: &str = "TRANSACTION_setRefreshRateSwitchingType";

const SWITCHING_TYPE_NONE:                   i32 = 0;
const SWITCHING_TYPE_ACROSS_AND_WITHIN_GROUPS: i32 = 2;

struct DisplayCtx {
    svc:  RawBinderService,
    code: u32,
}

impl DisplayCtx {
    fn open() -> Option<Self> {
        let code = find_transaction_code(DISPLAY_JAR, DISPLAY_STUB, DISPLAY_FIELD)
            .or_else(|| { log_warn!(TAG, "dex: {DISPLAY_FIELD} not found — using hardcoded 51"); Some(51) })?;
        match RawBinderService::open("display") {
            Ok(svc) => { log_info!(TAG, "display binder ready tx={code}"); Some(Self { svc, code }) }
            Err(e)  => { log_warn!(TAG, "display binder: {e}"); None }
        }
    }

    fn set_switching_type(&self, t: i32) {
        if let Err(e) = self.svc.transact_i32(self.code, t) {
            log_warn!(TAG, "setRefreshRateSwitchingType({t}): {e}");
        }
    }
}

struct Daemon {
    config:    Config,
    low_rate:  String,
    high_rate: String,
    mode:      Mode,
    display:   Option<DisplayCtx>,
}

// ── prop helpers ──────────────────────────────────────────────────────────────

fn prop_get(name: &str) -> String {
    android_property_get(name).unwrap_or_default()
}

fn prop_set_bool(name: &str, val: &str) -> bool {
    android_property_set(name, val).is_ok()
}

fn dynamic_enabled() -> bool {
    matches!(
        prop_get(PROP_DYNAMIC).as_str(),
        "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
    )
}

fn valid_rate(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn rate_int(v: &str) -> Option<i64> {
    v.split('.').next()?.parse::<i64>().ok()
}

fn rate_gt(a: &str, b: &str) -> bool {
    matches!((rate_int(a), rate_int(b)), (Some(a), Some(b)) if a > b)
}

// ── cmd helpers ───────────────────────────────────────────────────────────────

fn run_cmd_output(args: &[&str]) -> Option<String> {
    let argv: Vec<String> = std::iter::once("/system/bin/cmd")
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect();
    let out = SpawnOptions::builder(argv, SpawnBackend::PosixSpawn)
        .capture_stdout()
        .timeout_ms(5000)
        .build().ok()?
        .run().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_cmd(args: &[&str]) -> bool {
    let argv: Vec<String> = std::iter::once("/system/bin/cmd")
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect();
    SpawnOptions::builder(argv, SpawnBackend::PosixSpawn)
        .timeout_ms(5000)
        .build()
        .and_then(|o| o.run())
        .is_ok()
}

fn read_rate_setting(key: &str) -> String {
    run_cmd_output(&["settings", "get", "system", key]).unwrap_or_default()
}

fn set_rate(key: &str, val: &str) -> bool {
    run_cmd(&["settings", "put", "system", key, val])
}

// ── config ────────────────────────────────────────────────────────────────────

fn parse_devices(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn load_config() -> Config {
    let idle_ms = std::env::var("INOI_IDLE_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_IDLE_SEC)
        * 1000;

    let default_low = std::env::var("INOI_DEFAULT_LOW_RATE")
        .ok().filter(|v| valid_rate(v))
        .unwrap_or_else(|| DEFAULT_LOW_RATE.into());

    let default_high = std::env::var("INOI_DEFAULT_HIGH_RATE")
        .ok().filter(|v| valid_rate(v))
        .unwrap_or_else(|| DEFAULT_HIGH_RATE.into());

    let input_devices = parse_devices(
        &std::env::var("INOI_INPUT_DEVICES")
            .unwrap_or_else(|_| DEFAULT_INPUT.into()),
    );

    Config { idle_ms, default_low, default_high, input_devices }
}

// ── input devices ─────────────────────────────────────────────────────────────

fn open_inputs(paths: &[String]) -> Vec<InputDev> {
    let mut out = Vec::new();
    for path in paths {
        match OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(path) {
            Err(e) => log_warn!(TAG, "open {path}: {e}"),
            Ok(file) => {
                let raw = file.into_raw_fd();
                match unsafe { Fd::from_owned_raw_fd(raw, "input_dev") } {
                    Ok(fd)  => out.push(InputDev { path: path.clone(), fd }),
                    Err(e)  => log_warn!(TAG, "wrap {path}: {e}"),
                }
            }
        }
    }
    out
}

fn drain_input(fd: &Fd) -> bool {
    let mut saw = false;
    let mut ev = MaybeUninit::<InputEvent>::uninit();
    let buf = unsafe {
        std::slice::from_raw_parts_mut(ev.as_mut_ptr() as *mut u8, size_of::<InputEvent>())
    };
    loop {
        match fd.read_slice(buf) {
            Ok(Some(n)) if n == size_of::<InputEvent>() => {
                let e = unsafe { ev.assume_init() };
                if e.type_ == EV_SYN && e.code == SYN_REPORT { saw = true; }
            }
            Ok(None) => break,   // EAGAIN — drained
            _ => break,
        }
    }
    saw
}

// ── daemon ────────────────────────────────────────────────────────────────────

impl Daemon {
    fn new(config: Config) -> Self {
        Self {
            config,
            low_rate:  String::new(),
            high_rate: String::new(),
            mode:      Mode::Unknown,
            display:   DisplayCtx::open(),
        }
    }

    fn load_rates(&mut self) -> bool {
        let low = {
            let v = prop_get(PROP_LOW);
            if valid_rate(&v) { v } else { self.config.default_low.clone() }
        };
        let high = {
            let v = prop_get(PROP_HIGH);
            if valid_rate(&v) { v } else { self.config.default_high.clone() }
        };
        if rate_gt(&high, &low) {
            self.low_rate  = low;
            self.high_rate = high;
            true
        } else {
            log_warn!(TAG, "invalid rates low={low} high={high}");
            false
        }
    }

    fn apply_mode(&mut self, mode: Mode) -> bool {
        if mode == Mode::Unknown { return false; }
        if !self.load_rates() { return false; }
        // Low  → cap both at low_rate  (lock to e.g. 60 Hz)
        // High → floor at low_rate, ceiling at high_rate (allow e.g. 120 Hz)
        let (min_r, peak_r) = match mode {
            Mode::Low  => (self.low_rate.as_str(),  self.low_rate.as_str()),
            Mode::High => (self.low_rate.as_str(),  self.high_rate.as_str()),
            Mode::Unknown => unreachable!(),
        };
        if self.mode == mode
            && same_rate(min_r, &read_rate_setting("min_refresh_rate"))
            && same_rate(peak_r, &read_rate_setting("peak_refresh_rate"))
        {
            return true; // already applied
        }
        let ok = set_rate("min_refresh_rate", min_r)
              && set_rate("peak_refresh_rate", peak_r);
        if ok {
            log_info!(TAG, "mode={mode:?} min={min_r} peak={peak_r}");
            self.mode = mode;
            if let Some(d) = &self.display {
                let switching = match mode {
                    Mode::High => SWITCHING_TYPE_ACROSS_AND_WITHIN_GROUPS,
                    Mode::Low  => SWITCHING_TYPE_NONE,
                    Mode::Unknown => unreachable!(),
                };
                d.set_switching_type(switching);
            }
        } else {
            log_warn!(TAG, "set_rate failed");
        }
        ok
    }

    fn set_low(&mut self)  -> bool { self.apply_mode(Mode::Low)  }
    fn set_high(&mut self) -> bool { self.apply_mode(Mode::High) }
}

fn same_rate(a: &str, b: &str) -> bool {
    matches!((rate_int(a), rate_int(b)), (Some(a), Some(b)) if a == b)
}

// ── prop watcher ──────────────────────────────────────────────────────────────

fn spawn_prop_watcher(notify: Arc<Fd>) {
    for name in [PROP_DYNAMIC, PROP_LOW, PROP_HIGH] {
        let notify = notify.clone();
        thread::spawn(move || {
            let info = loop {
                if let Some(i) = android_property_find(name) { break i; }
                thread::sleep(Duration::from_secs(1));
            };
            let mut serial = android_property_serial(info).unwrap_or(0);
            loop {
                match android_property_wait(info, serial, None) {
                    Ok(Some(s)) => { serial = s; let _ = notify.write_u64(1); }
                    Ok(None)    => {}
                    Err(_)      => thread::sleep(Duration::from_millis(200)),
                }
            }
        });
    }
}

// ── public API ────────────────────────────────────────────────────────────────

pub fn run() {
    let config = load_config();
    let mut daemon = Daemon::new(config);
    let mut inputs = open_inputs(&daemon.config.input_devices);

    let prop_efd  = Arc::new(Fd::eventfd(0).expect("prop eventfd"));
    let idle_timer = Fd::timerfd().expect("idle timerfd");
    spawn_prop_watcher(prop_efd.clone());

    let mut reactor = Reactor::new().expect("reactor");
    let prop_tok  = reactor.add(&prop_efd, true, false).expect("add prop_efd");
    let timer_tok = reactor.add(&idle_timer, true, false).expect("add timerfd");

    let mut input_tokens: Vec<(coreshift_core::reactor::Token, usize)> = inputs
        .iter()
        .enumerate()
        .filter_map(|(i, dev)| {
            reactor.add(&dev.fd, true, false).ok().map(|tok| (tok, i))
        })
        .collect();

    log_info!("policy:rr:init", "start idle={}ms inputs={}", daemon.config.idle_ms,
        inputs.iter().map(|d| d.path.as_str()).collect::<Vec<_>>().join(" "));

    if dynamic_enabled() {
        daemon.set_low();
    } else {
        log_info!(TAG, "dynamic disabled — waiting for prop change");
    }

    let mut events = Vec::new();
    loop {
        events.clear();
        let _ = reactor.wait(&mut events, 16, -1);

        let mut saw_input   = false;
        let mut prop_change = false;

        for ev in &events {
            if ev.token == prop_tok {
                let mut buf = [0u8; 8];
                while let Ok(Some(_)) = prop_efd.read_slice(&mut buf) {}
                prop_change = true;
                continue;
            }

            if ev.token == timer_tok {
                let mut buf = [0u8; 8];
                while let Ok(Some(_)) = idle_timer.read_slice(&mut buf) {}
                if daemon.mode == Mode::High {
                    daemon.set_low();
                }
                continue;
            }

            // input device event
            if let Some(&(_, idx)) = input_tokens.iter().find(|(tok, _)| *tok == ev.token) {
                if ev.hangup || ev.error {
                    // device disappeared — remove and retry open next tick
                    if let Some(pos) = input_tokens.iter().position(|(tok, _)| *tok == ev.token) {
                        let (tok, _) = input_tokens.remove(pos);
                        if let Some(dev) = inputs.get(idx) {
                            let _ = reactor.del(&dev.fd);
                            log_warn!(TAG, "input {} lost", dev.path);
                        }
                        let _ = tok; // token invalidated by del
                    }
                    continue;
                }
                if ev.readable {
                    if let Some(dev) = inputs.get(idx) {
                        if drain_input(&dev.fd) { saw_input = true; }
                    }
                }
            }
        }

        // re-open lost devices
        if input_tokens.len() < inputs.len() {
            let fresh = open_inputs(&daemon.config.input_devices);
            for dev in fresh {
                let already = inputs.iter().any(|d| d.path == dev.path);
                if !already {
                    if let Ok(tok) = reactor.add(&dev.fd, true, false) {
                        let idx = inputs.len();
                        input_tokens.push((tok, idx));
                        inputs.push(dev);
                    }
                }
            }
        }

        if prop_change {
            if dynamic_enabled() {
                if daemon.mode == Mode::Unknown {
                    daemon.set_low();
                } else {
                    let cur = daemon.mode;
                    daemon.mode = Mode::Unknown;
                    daemon.apply_mode(cur);
                }
            } else if daemon.mode != Mode::Unknown {
                log_info!(TAG, "dynamic disabled");
                daemon.mode = Mode::Unknown;
            }
        }

        if saw_input && dynamic_enabled() {
            daemon.set_high();
            // arm/rearm idle timer
            let _ = idle_timer.set_timer_oneshot(Some(Duration::from_millis(daemon.config.idle_ms)));
        }
    }
}

pub fn print_status() {
    let config = load_config();
    let mut daemon = Daemon::new(config);
    println!("prop={PROP_DYNAMIC}={}", prop_get(PROP_DYNAMIC));
    println!("prop_low={PROP_LOW}={}", prop_get(PROP_LOW));
    println!("prop_high={PROP_HIGH}={}", prop_get(PROP_HIGH));
    println!("idle_ms={}", daemon.config.idle_ms);
    println!("input_devices={}", daemon.config.input_devices.join(" "));
    println!("current_min_refresh_rate={}",  read_rate_setting("min_refresh_rate"));
    println!("current_peak_refresh_rate={}", read_rate_setting("peak_refresh_rate"));
    if daemon.load_rates() {
        println!("chosen_low_rate={}",  daemon.low_rate);
        println!("chosen_high_rate={}", daemon.high_rate);
    } else {
        println!("chosen_low_rate=none");
        println!("chosen_high_rate=none");
    }
}

pub fn handle_cli() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let subcmd = args.iter().skip(1).find(|a| a.starts_with("dynamic-rr"));
    let Some(_) = subcmd else { return false; };

    let sub = args.get(2).map(String::as_str).unwrap_or("--status");
    match sub {
        "--status" => { print_status(); true }
        "--enable" => {
            if prop_set_bool(PROP_DYNAMIC, "1") { println!("{PROP_DYNAMIC}=1"); true }
            else { eprintln!("failed"); std::process::exit(1); }
        }
        "--disable" => {
            if prop_set_bool(PROP_DYNAMIC, "0") { println!("{PROP_DYNAMIC}=0"); true }
            else { eprintln!("failed"); std::process::exit(1); }
        }
        "--set" => {
            let low  = args.get(3).map(String::as_str).unwrap_or("");
            let high = args.get(4).map(String::as_str).unwrap_or("");
            if !valid_rate(low) || !valid_rate(high) || !rate_gt(high, low) {
                eprintln!("usage: dynamic-rr --set LOW HIGH");
                std::process::exit(2);
            }
            let ok = prop_set_bool(PROP_LOW, low)
                  && prop_set_bool(PROP_HIGH, high)
                  && prop_set_bool(PROP_DYNAMIC, "1");
            if ok { println!("{PROP_LOW}={low}"); println!("{PROP_HIGH}={high}"); println!("{PROP_DYNAMIC}=1"); true }
            else  { eprintln!("failed"); std::process::exit(1); }
        }
        _ => false,
    }
}
