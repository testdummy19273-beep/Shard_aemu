//! Emulator lifecycle: port selection, launch, boot wait, stop, adoption and log parsing.

use std::ffi::OsString;
use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::adb::Adb;
use crate::config::{EmulatorConfig, PORT_RANGE};
use crate::paths::Paths;
use crate::procs;

pub const COLD_BOOT_TIMEOUT: Duration = Duration::from_secs(240);
pub const QUICK_BOOT_TIMEOUT: Duration = Duration::from_secs(60);
/// A restored snapshot whose adb link stays "offline" this long is treated as unusable.
/// (Seen with emulator 37.2.12: some snapshots restore fine but adbd never answers again.)
pub const QUICK_BOOT_OFFLINE_LIMIT: Duration = Duration::from_secs(20);
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// Emulator features turned off: netsim (Wi-Fi/Bluetooth simulation daemon, its web/CLI UIs)
/// fails at non-ASCII paths and slows boots; the classic network stack is used instead.
const DISABLED_FEATURES: &str = "-WiFiPacketStream,-BluetoothEmulation,-NetsimWebUi,-NetsimCliUi";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    /// Load the Quick Boot snapshot.
    Quick,
    /// Ignore the snapshot (`-no-snapshot-load`), save a fresh one on exit.
    Cold,
    /// Quick Boot turned off in settings (`-no-snapshot`).
    NoSnapshot,
}

impl BootMode {
    pub fn timeout(self) -> Duration {
        match self {
            BootMode::Quick => QUICK_BOOT_TIMEOUT,
            _ => COLD_BOOT_TIMEOUT,
        }
    }
}

/// First even console port from `start` whose console and adb ports (port, port + 1) are free.
pub fn pick_port(start: u16, is_free: impl Fn(u16) -> bool) -> Option<u16> {
    let start = start.max(*PORT_RANGE.start());
    let start = start + start % 2;
    (start..=*PORT_RANGE.end())
        .step_by(2)
        .find(|p| is_free(*p) && is_free(p + 1))
}

pub fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn serial_for(port: u16) -> String {
    format!("emulator-{port}")
}

pub fn args(paths: &Paths, cfg: &EmulatorConfig, port: u16, mode: BootMode) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "-avd".into(),
        crate::avd::AVD_NAME.into(),
        "-gpu".into(),
        cfg.gpu.clone().into(),
        "-no-boot-anim".into(),
        "-port".into(),
        port.to_string().into(),
        // No usage statistics, no crash reports.
        "-no-metrics".into(),
        "-crash-report-mode".into(),
        "disabled".into(),
        "-feature".into(),
        DISABLED_FEATURES.into(),
        // Screenshots/bug reports go to logs\ (without this the emulator probes the Desktop).
        "-save-path".into(),
        paths.logs.clone().into_os_string(),
    ];
    match mode {
        BootMode::Quick => {}
        BootMode::Cold => a.push("-no-snapshot-load".into()),
        BootMode::NoSnapshot => a.push("-no-snapshot".into()),
    }
    a
}

/// The emulator checks Google for updates at most once a day, remembering the last check in
/// `home\emu-update-last-check.ini` under a key derived from its version. Pre-seeding that key
/// with a far-future time (2100-01-01) means it never phones home.
///
/// Key format (reproduces what 37.2.12 writes): the query string
/// `version=<v>&coreVersion=<percent-encoded "qemu2 2.12.0">` percent-encoded again, with every
/// `%` turned into `.`, prefixed with `_`.
pub fn seed_update_check(paths: &Paths) -> Result<()> {
    let version = emulator_build_version(paths)
        .or_else(|| emulator_version(paths).map(|v| format!("{v}.0")))
        .context("cannot tell the emulator version")?;
    let path = paths.home.join("emu-update-last-check.ini");
    let line = format!("{} = 4102444800\n", update_check_key(&version));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(line.as_str()) {
        std::fs::write(&path, line).context("writing home\\emu-update-last-check.ini")?;
    }
    Ok(())
}

pub fn update_check_key(version: &str) -> String {
    let query = format!(
        "version={version}&coreVersion={}",
        percent_encode("qemu2 2.12.0")
    );
    format!("_{}", percent_encode(&query).replace('%', "."))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Pkg.Revision` from `sdk\emulator\source.properties`, e.g. "37.2.12".
pub fn emulator_version(paths: &Paths) -> Option<String> {
    let text = std::fs::read_to_string(paths.emulator_dir().join("source.properties")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("Pkg.Revision="))
        .map(|v| v.trim().to_string())
}

/// The four-part version the update check uses, read from the qemu binary
/// (`version=37.2.12.0&coreVersion=%s`).
fn emulator_build_version(paths: &Paths) -> Option<String> {
    let bin = std::fs::read(paths.qemu_exe()).ok()?;
    let marker = b"&coreVersion=%s";
    let end = bin.windows(marker.len()).position(|w| w == marker)?;
    let start = bin[..end].iter().rposition(|&c| c == b'=')? + 1;
    let v = std::str::from_utf8(&bin[start..end]).ok()?;
    (!v.is_empty() && v.bytes().all(|c| c.is_ascii_digit() || c == b'.')).then(|| v.to_string())
}

pub struct Launched {
    pub child: Child,
    pub port: u16,
    pub serial: String,
    pub log: PathBuf,
    pub mode: BootMode,
}

pub fn launch(paths: &Paths, cfg: &EmulatorConfig, port: u16, mode: BootMode) -> Result<Launched> {
    let log = paths
        .logs
        .join(format!("emulator-{}.log", crate::logging::stamp()));
    let file = File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let err = file.try_clone()?;
    let child = {
        let mut cmd = Command::new(paths.emulator_exe());
        cmd.args(args(paths, cfg, port, mode))
            .current_dir(paths.emulator_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err));
        paths.apply_child_env(&mut cmd);
        procs::hidden(&mut cmd);
        cmd.spawn().context("starting emulator.exe")?
    };
    crate::logging::prune(&paths.logs, "emulator-", 10);
    Ok(Launched {
        child,
        port,
        serial: serial_for(port),
        log,
        mode,
    })
}

#[derive(Debug)]
pub enum BootOutcome {
    Booted {
        elapsed: Duration,
    },
    /// Snapshot restored but adb never came back; caller should cold-boot.
    QuickBootStuck {
        reason: String,
    },
    Exited {
        code: Option<i32>,
        tail: String,
    },
    TimedOut {
        tail: String,
    },
    Cancelled,
}

/// Waits for `adb get-state == device` and `sys.boot_completed == 1`.
pub fn wait_for_boot(
    adb: &Adb,
    launched: &mut Launched,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<BootOutcome> {
    let start = Instant::now();
    let mut mode = launched.mode;
    let mut offline_since: Option<Instant> = None;
    let mut last_state = String::new();
    let mut last_report = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(BootOutcome::Cancelled);
        }
        if let Some(status) = launched.child.try_wait()? {
            // emulator.exe exits once qemu is gone; a quick exit means it failed to start.
            return Ok(BootOutcome::Exited {
                code: status.code(),
                tail: tail(&launched.log, 30),
            });
        }
        let log = read_log(&launched.log);
        let snapshot_loaded = log.contains("Successfully loaded snapshot");
        if mode == BootMode::Quick && !snapshot_loaded && log_says_cold_boot(&log) {
            // The emulator decided to cold boot by itself (e.g. snapshot invalid): allow 240 s.
            mode = BootMode::Cold;
            progress("the emulator is cold booting (Quick Boot snapshot not usable)".into());
        }
        let state = adb
            .state_of(&launched.serial)
            .unwrap_or_else(|| "not listed".into());
        if state == "offline" {
            let since = *offline_since.get_or_insert_with(Instant::now);
            // Faster path when the log does show the restore finished.
            if mode == BootMode::Quick
                && snapshot_loaded
                && since.elapsed() > QUICK_BOOT_OFFLINE_LIMIT
            {
                return Ok(BootOutcome::QuickBootStuck {
                    reason: format!(
                        "adb stayed offline for {}s after the snapshot loaded",
                        since.elapsed().as_secs()
                    ),
                });
            }
        } else {
            offline_since = None;
        }
        if state == "device"
            && adb
                .getprop(&launched.serial, "sys.boot_completed")
                .as_deref()
                == Some("1")
        {
            return Ok(BootOutcome::Booted {
                elapsed: start.elapsed(),
            });
        }
        if state != last_state || last_report.elapsed() > Duration::from_secs(15) {
            progress(format!(
                "waiting for boot: adb state {state}, {}s",
                start.elapsed().as_secs()
            ));
            last_state = state;
            last_report = Instant::now();
        }
        let limit = mode.timeout();
        if start.elapsed() > limit {
            if mode == BootMode::Quick {
                // The emulator's log is block-buffered, so "Successfully loaded snapshot" can show
                // up late or not at all while it runs: a Quick Boot that is not up within 60 s is
                // treated as stuck whether or not the log has said so.
                return Ok(BootOutcome::QuickBootStuck {
                    reason: format!("not booted {} s into Quick Boot", limit.as_secs()),
                });
            }
            return Ok(BootOutcome::TimedOut {
                tail: tail(&launched.log, 30),
            });
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn log_says_cold_boot(log: &str) -> bool {
    log.contains("Failed to load snapshot")
        || log.contains("starting from scratch")
        || log.contains("Cold boot")
}

/// The renderer the emulator picked, from its log, e.g.
/// "Graphics Adapter Android Emulator OpenGL ES Translator (NVIDIA GeForce RTX 5070/PCIe/SSE2)".
/// (The "Graphics Adapter Vendor Google (NVIDIA Corporation)" line just before it is skipped.)
pub fn renderer_from_log(log: &str) -> Option<String> {
    log.lines().find_map(|l| {
        let i = l.find("Graphics Adapter ")?;
        let rest = &l[i + "Graphics Adapter ".len()..];
        if rest.starts_with("Vendor ") {
            return None;
        }
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        (close > open).then(|| rest[open + 1..close].trim().to_string())
    })
}

/// The `GLES:` line of `dumpsys SurfaceFlinger`, e.g. "GLES: Google (NVIDIA Corporation),
/// Android Emulator OpenGL ES Translator (NVIDIA GeForce RTX 5070/PCIe/SSE2), OpenGL ES 3.0 (...)".
pub fn gles_line(dumpsys: &str) -> Option<String> {
    dumpsys
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("GLES:"))
        .map(str::to_string)
}

/// The host renderer named in a `GLES:` line (the part in parentheses after "Translator"; it
/// can itself contain parentheses, e.g. "AMD Radeon(TM) Graphics").
pub fn renderer_from_gles(line: &str) -> Option<String> {
    let start = line.find("Translator (")? + "Translator (".len();
    let mut depth = 1;
    for (i, c) in line[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(line[start..start + i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Vulkan device chosen by the emulator, if logged ("Selecting Vulkan device: NAME, Version...").
pub fn vulkan_device_from_log(log: &str) -> Option<String> {
    log.lines().find_map(|l| {
        let i = l.find("Selecting Vulkan device: ")?;
        let rest = &l[i + "Selecting Vulkan device: ".len()..];
        Some(rest.split(", Version").next()?.trim().to_string())
    })
}

pub fn is_software_renderer(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "swiftshader",
        "llvmpipe",
        "lavapipe",
        "softpipe",
        "warp",
        "basic render",
        "software",
    ]
    .iter()
    .any(|s| n.contains(s))
}

/// A log file as text, empty if unreadable. Lossy: one stray non-UTF-8 byte (a path in the
/// ANSI code page, a driver string) must not hide the whole log.
pub fn read_log(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Last `n` lines of a log file.
pub fn tail(path: &Path, n: usize) -> String {
    let text = read_log(path);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

// ---------------------------------------------------------------------------------------------
// Running-emulator bookkeeping and stop.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningInfo {
    pub port: u16,
    pub serial: String,
    pub log: PathBuf,
}

fn running_file(paths: &Paths) -> PathBuf {
    paths.state.join("running.toml")
}

pub fn save_running(paths: &Paths, info: &RunningInfo) -> Result<()> {
    std::fs::write(running_file(paths), toml::to_string(info)?).context("writing running.toml")
}

pub fn load_running(paths: &Paths) -> Option<RunningInfo> {
    toml::from_str(&std::fs::read_to_string(running_file(paths)).ok()?).ok()
}

pub fn clear_running(paths: &Paths) {
    let _ = std::fs::remove_file(running_file(paths));
}

/// Emulator-side processes (emulator.exe, qemu, netsimd, crashpad, watchdogs) from our sdk.
pub fn emulator_processes(paths: &Paths) -> Vec<procs::ProcInfo> {
    procs::under(&paths.emulator_dir())
}

pub fn qemu_running(paths: &Paths) -> bool {
    emulator_processes(paths)
        .iter()
        .any(|p| p.name_is("qemu-system-x86_64.exe"))
}

/// An emulator from our sdk that is already running (e.g. Shard was closed and left it on).
pub fn find_running(paths: &Paths) -> Option<RunningInfo> {
    let procs = emulator_processes(paths);
    let qemu = procs.iter().find(|p| p.name_is("qemu-system-x86_64.exe"))?;
    let port = port_from_cmd(&qemu.cmd)
        .or_else(|| load_running(paths).map(|r| r.port))
        .unwrap_or(5554);
    let log = load_running(paths).map(|r| r.log).unwrap_or_default();
    Some(RunningInfo {
        port,
        serial: serial_for(port),
        log,
    })
}

pub fn port_from_cmd(cmd: &[String]) -> Option<u16> {
    cmd.windows(2)
        .find(|w| w[0] == "-port")
        .and_then(|w| w[1].parse().ok())
}

#[derive(Debug, Default)]
pub struct StopReport {
    pub graceful: bool,
    pub killed: Vec<String>,
}

/// `adb emu kill`, wait up to 15 s, then terminate whatever of ours is left (including the
/// emulator's `-kill ... -sleep 20` watchdog helpers), then stop the adb server.
pub fn stop(paths: &Paths, adb: &Adb, serial: Option<&str>) -> Result<StopReport> {
    let mut report = StopReport::default();
    if qemu_running(paths) {
        if let Some(serial) = serial {
            let _ = adb.emu_kill(serial);
        }
        report.graceful = procs::wait_until(STOP_TIMEOUT, Duration::from_millis(250), || {
            !qemu_running(paths)
        });
    } else {
        report.graceful = true;
    }
    // emulator.exe exits right after qemu; give it a moment before sweeping helpers.
    procs::wait_until(Duration::from_secs(3), Duration::from_millis(250), || {
        !emulator_processes(paths)
            .iter()
            .any(|p| p.name_is("emulator.exe") && !p.cmd.iter().any(|a| a == "-kill"))
    });
    for round in 0..3 {
        let left = emulator_processes(paths);
        if left.is_empty() {
            break;
        }
        if round == 0 {
            report
                .killed
                .extend(left.iter().map(|p| format!("{} ({})", p.name, p.pid)));
        }
        procs::kill(&left.iter().map(|p| p.pid).collect::<Vec<_>>());
        procs::wait_until(Duration::from_secs(3), Duration::from_millis(200), || {
            emulator_processes(paths).is_empty()
        });
    }
    stop_adb_server(paths, adb);
    clear_running(paths);
    Ok(report)
}

/// `adb kill-server` on Shard's port, then make sure no adb.exe from our sdk is left.
pub fn stop_adb_server(paths: &Paths, adb: &Adb) {
    // Not even a client call when no server of ours runs (the client reads the user profile).
    if procs::under(&paths.platform_tools_dir()).is_empty() {
        return;
    }
    let _ = adb.kill_server();
    let gone = procs::wait_until(Duration::from_secs(5), Duration::from_millis(200), || {
        procs::under(&paths.platform_tools_dir()).is_empty()
    });
    if !gone {
        let left: Vec<u32> = procs::under(&paths.platform_tools_dir())
            .iter()
            .map(|p| p.pid)
            .collect();
        procs::kill(&left);
        procs::wait_until(Duration::from_secs(3), Duration::from_millis(200), || {
            procs::under(&paths.platform_tools_dir()).is_empty()
        });
    }
}
