//! `--headless verify`: the brief's checks a–i, one `PASS|FAIL|SKIP <id> <detail>` line each,
//! exit code 1 if any check failed.
//!
//! Execution order differs from the printed order: the emulator is booted (c, d, e) and stopped
//! first, then the folder is moved (h), and only then are leftovers (b) and processes (i)
//! checked, when nothing of Shard's should be running any more.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::adb::Adb;
use crate::config::Config;
use crate::emulator;
use crate::headless::{run_on, Printer, RunningSeen};
use crate::instance::{self, Acquire, InstanceLock};
use crate::jobs::{self, Command, Engine, Job};
use crate::leak;
use crate::paths::{path_starts_with, Paths, ADB_SERVER_PORT};
use crate::procs;
use crate::profile::Profile;
use crate::sdk::{self, manifest::PackageId};
use crate::winutil;

/// Sibling folder name used by check h (space and non-ASCII on purpose).
pub const MOVED_NAME: &str = "Shard Moved ü";
const SCREENCAP_FILE: &str = "verify-screencap.png";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
    Skip,
}

struct Check {
    status: Status,
    detail: String,
}

fn pass(d: impl Into<String>) -> Check {
    Check {
        status: Status::Pass,
        detail: d.into(),
    }
}
fn fail(d: impl Into<String>) -> Check {
    Check {
        status: Status::Fail,
        detail: d.into(),
    }
}
fn skip(d: impl Into<String>) -> Check {
    Check {
        status: Status::Skip,
        detail: d.into(),
    }
}

pub fn run(paths: &Paths, lock: InstanceLock) -> i32 {
    let mut lock = Some(lock);
    let mut checks: BTreeMap<char, Check> = BTreeMap::new();
    println!("Shard verify in {}", paths.root.display());

    let start = leak::take();
    let baseline = leak::load_baseline(paths);
    let was_running = emulator::qemu_running(paths);
    let rules_at_start = firewall_rules(paths).unwrap_or_default();
    let mut exposure: Option<Vec<String>> = None;

    checks.insert('a', check_folders(paths));

    if jobs::is_set_up(paths) {
        let engine = Engine::start(paths.clone(), Arc::new(|| {}));
        let mut printer = Printer::new("    ");
        println!("--- boot");
        let (boot, running) = run_on(&engine, Command::Boot, Job::Boot, &mut printer);
        let adb = Adb::new(paths);
        match (&boot, &running) {
            (Ok(()), Some(r)) => {
                checks.insert('c', check_device(&adb, r));
                checks.insert('d', check_gpu(paths, &adb, r));
                checks.insert('e', check_screen(paths, &adb, &r.serial));
                exposure = Some(network_exposure(paths));
            }
            _ => {
                let why = boot
                    .clone()
                    .err()
                    .unwrap_or("no running emulator reported".into());
                checks.insert('c', fail(format!("boot failed: {why}")));
                checks.insert('d', fail("not checked: the emulator did not boot"));
                checks.insert('e', fail("not checked: the emulator did not boot"));
            }
        }
        println!("--- stop");
        let (stopped, _) = run_on(&engine, Command::Stop, Job::Stop, &mut printer);
        engine.shutdown(Duration::from_secs(60));
        if let Err(e) = stopped {
            println!("    stop failed: {e}");
        }
        checks.insert('h', check_move(paths, &mut lock));
    } else {
        let msg = "Shard is not set up in this folder (run Shard.exe --headless setup first)";
        checks.insert('c', fail(msg));
        checks.insert('d', fail(msg));
        checks.insert('e', fail(msg));
        checks.insert('h', skip(msg));
    }
    checks.insert('f', skip("debloat arrives in milestone 2"));
    checks.insert('g', skip("restore arrives in milestone 2"));
    checks.insert('i', check_processes(paths));
    checks.insert(
        'b',
        check_leaks(
            paths,
            &start,
            baseline.as_ref(),
            was_running,
            &rules_at_start,
            exposure.as_deref(),
        ),
    );
    drop(lock);

    println!("--- results");
    let mut failed = 0;
    for (id, c) in &checks {
        let tag = match c.status {
            Status::Pass => "PASS",
            Status::Fail => {
                failed += 1;
                "FAIL"
            }
            Status::Skip => "SKIP",
        };
        println!("{tag} {id} {}", c.detail);
        tracing::info!("verify {tag} {id} {}", c.detail);
    }
    let passed = checks.values().filter(|c| c.status == Status::Pass).count();
    let skipped = checks.values().filter(|c| c.status == Status::Skip).count();
    println!("verify: {passed} passed, {failed} failed, {skipped} skipped");
    if failed > 0 {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------------------------

fn check_folders(paths: &Paths) -> Check {
    let mut missing = Vec::new();
    for (name, dir) in [
        ("sdk", &paths.sdk),
        ("avd", &paths.avd),
        ("home", &paths.home),
        ("tmp", &paths.tmp),
        ("cache", &paths.cache),
        ("state", &paths.state),
        ("logs", &paths.logs),
    ] {
        if !dir.is_dir() {
            missing.push(format!("{name}\\"));
        }
    }
    if !paths.config_file().is_file() {
        missing.push("shard.toml".into());
    }
    for id in PackageId::ALL {
        if !sdk::key_file(paths, id).is_file() {
            missing.push(id.label().to_string());
        }
    }
    for f in [paths.avd_ini(), paths.avd_config()] {
        if !f.is_file() {
            missing.push(rel(paths, &f));
        }
    }
    if missing.is_empty() {
        pass("folders, shard.toml, emulator, platform-tools, system image and AVD are in place")
    } else {
        fail(format!("missing: {}", missing.join(", ")))
    }
}

fn check_device(adb: &Adb, r: &RunningSeen) -> Check {
    match adb.state_of(&r.serial) {
        Some(s) if s == "device" => pass(format!(
            "{} is listed as device{}",
            r.serial,
            r.boot_secs
                .map(|s| format!(" (booted in {s:.1} s)"))
                .unwrap_or_else(|| " (was already running)".into())
        )),
        other => fail(format!(
            "adb lists {} as {}",
            r.serial,
            other.unwrap_or_else(|| "missing".into())
        )),
    }
}

fn check_gpu(paths: &Paths, adb: &Adb, r: &RunningSeen) -> Check {
    let mode = Config::load_or_create(&paths.config_file())
        .map(|c| c.emulator.gpu)
        .unwrap_or_else(|_| "?".into());
    // The emulator log is block-buffered, so its renderer line may not be written yet; the
    // guest's SurfaceFlinger reports the same host renderer.
    let from_log = emulator::load_running(paths)
        .and_then(|info| emulator::renderer_from_log(&emulator::read_log(&info.log)));
    let gles = adb
        .shell(&r.serial, "dumpsys SurfaceFlinger", Duration::from_secs(20))
        .ok()
        .and_then(|out| emulator::gles_line(&out));
    let from_gles = gles.as_deref().and_then(emulator::renderer_from_gles);
    let names: Vec<&str> = [from_log.as_deref(), from_gles.as_deref(), gles.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let detail = format!(
        "-gpu {mode}: emulator log: {} | SurfaceFlinger {}",
        from_log
            .as_deref()
            .unwrap_or("(renderer line not written yet)"),
        gles.as_deref().unwrap_or("(no GLES line)")
    );
    if from_log.is_none() && from_gles.is_none() {
        fail(format!("no host renderer reported; {detail}"))
    } else if names.iter().any(|n| emulator::is_software_renderer(n)) {
        fail(format!("software rendering; {detail}"))
    } else {
        pass(detail)
    }
}

fn check_screen(paths: &Paths, adb: &Adb, serial: &str) -> Check {
    match screen_ok(paths, adb, serial) {
        Ok(d) => pass(d),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// Screenshot saved to `logs\verify-screencap.png`; more than 1% of pixels must be non-black.
fn screen_ok(paths: &Paths, adb: &Adb, serial: &str) -> Result<String> {
    let file = paths.logs.join(SCREENCAP_FILE);
    let mut last = String::from("no screenshot taken");
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        match adb.screencap(serial) {
            Ok(png) => {
                let _ = std::fs::write(&file, &png);
                match lit_fraction(&png) {
                    Ok((f, w, h)) if f > 0.01 => {
                        return Ok(format!(
                            "{:.0}% of {w}x{h} pixels are not black (logs\\{SCREENCAP_FILE})",
                            f * 100.0
                        ))
                    }
                    Ok((f, w, h)) => {
                        last = format!("screen is black ({:.2}% of {w}x{h} lit)", f * 100.0)
                    }
                    Err(e) => last = format!("screenshot unreadable: {e:#}"),
                }
            }
            Err(e) => last = format!("{e:#}"),
        }
    }
    bail!("{last}")
}

/// (fraction of pixels brighter than near-black, width, height)
pub fn lit_fraction(png_bytes: &[u8]) -> Result<(f64, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().context("not a PNG")?;
    let size = reader
        .output_buffer_size()
        .context("PNG too large to decode")?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).context("decoding the PNG")?;
    if info.bit_depth != png::BitDepth::Eight {
        bail!("unexpected PNG bit depth {:?}", info.bit_depth);
    }
    let samples = info.color_type.samples();
    let pixels = info.width as usize * info.height as usize;
    if pixels == 0 {
        bail!("empty image");
    }
    let lit = buf[..info.buffer_size()]
        .chunks_exact(samples)
        .filter(|px| {
            let v = if samples >= 3 {
                px[0].max(px[1]).max(px[2])
            } else {
                px[0]
            };
            v > 16
        })
        .count();
    Ok((lit as f64 / pixels as f64, info.width, info.height))
}

// ---------------------------------------------------------------------------------------------
// h: move the whole folder, boot and stop from the new place, move it back.

fn check_move(paths: &Paths, lock: &mut Option<InstanceLock>) -> Check {
    let exe_inside = std::env::current_exe()
        .map(|e| paths.contains(&e))
        .unwrap_or(true);
    let cwd_inside = std::env::current_dir()
        .map(|c| paths.contains(&c))
        .unwrap_or(true);
    if exe_inside || cwd_inside {
        return skip(format!(
            "Windows cannot rename a folder that holds the running exe or the current directory; \
             run from outside it: <another copy of Shard.exe> --root \"{}\" --headless verify",
            paths.root.display()
        ));
    }
    if !paths.exe().is_file() {
        return fail(format!(
            "no Shard.exe in {} to run after the move",
            paths.root.display()
        ));
    }
    let Some(parent) = paths.root.parent() else {
        return fail("the folder has no parent to move within");
    };
    let moved_root = parent.join(MOVED_NAME);
    if moved_root.exists() {
        return fail(format!("{} already exists", moved_root.display()));
    }

    // Hold nothing inside the folder while it is renamed.
    drop(lock.take());
    crate::logging::close();
    println!(
        "--- move {} -> {}",
        paths.root.display(),
        moved_root.display()
    );
    let moved = Paths::from_root(&moved_root);
    let result = move_test(paths, &moved, parent);

    // Whatever happened, stop anything left in the moved folder and move it back.
    let back = if moved_root.exists() {
        if !procs::under(&moved_root).is_empty() {
            let _ = run_child(&moved.exe(), &["--headless", "stop"], parent, 120);
            let left: Vec<u32> = procs::under(&moved_root).iter().map(|p| p.pid).collect();
            procs::kill(&left);
        }
        procs::wait_until(Duration::from_secs(30), Duration::from_millis(250), || {
            procs::under(&moved_root).is_empty()
        });
        println!(
            "--- move {} -> {}",
            moved_root.display(),
            paths.root.display()
        );
        crate::sdk::install::retry_locked("moving the folder back", || {
            std::fs::rename(&moved_root, &paths.root)
        })
    } else {
        Ok(())
    };
    crate::logging::open(&paths.logs.join("shard.log"));
    if let Ok(Acquire::Acquired(l)) = instance::acquire(paths) {
        *lock = Some(l);
    }

    match (result, back) {
        (Ok(detail), Ok(())) => pass(format!("{detail}; moved back")),
        (Err(e), Ok(())) => fail(format!("{e:#} (folder moved back)")),
        (r, Err(e)) => fail(format!(
            "{}; could not move the folder back: {e:#}. It is now at {}",
            r.err()
                .map(|e| format!("{e:#}"))
                .unwrap_or("move test passed".into()),
            moved_root.display()
        )),
    }
}

fn move_test(paths: &Paths, moved: &Paths, cwd: &Path) -> Result<String> {
    crate::sdk::install::retry_locked("moving the folder", || {
        std::fs::rename(&paths.root, &moved.root)
    })?;
    let boot = run_child(&moved.exe(), &["--headless", "boot"], cwd, 420)?;
    if !boot.success() {
        bail!(
            "Shard.exe --headless boot from the moved folder failed (exit {:?})",
            boot.code
        );
    }
    let info = emulator::load_running(moved)
        .context("the moved Shard did not record a running emulator")?;
    let ini = std::fs::read_to_string(moved.avd_ini()).context("reading the moved shard.ini")?;
    let want = format!("path={}", moved.avd_dir().display());
    if !ini.lines().any(|l| l == want) {
        bail!("avd\\shard.ini was not rewritten for the new location");
    }
    let adb = Adb::new(moved);
    let state = adb.state_of(&info.serial);
    if state.as_deref() != Some("device") {
        bail!(
            "after the move adb lists {} as {}",
            info.serial,
            state.unwrap_or("missing".into())
        );
    }
    let screen = screen_ok(moved, &adb, &info.serial).context("screen check after the move")?;
    let stop = run_child(&moved.exe(), &["--headless", "stop"], cwd, 120)?;
    if !stop.success() {
        bail!(
            "Shard.exe --headless stop in the moved folder failed (exit {:?})",
            stop.code
        );
    }
    Ok(format!(
        "booted from \"{}\": {} is device, shard.ini path rewritten, {screen}; stopped",
        moved.root.display(),
        info.serial
    ))
}

fn run_child(exe: &Path, args: &[&str], cwd: &Path, timeout_s: u64) -> Result<procs::Output> {
    println!("    $ \"{}\" {}", exe.display(), args.join(" "));
    let mut cmd = Process::new(exe);
    cmd.args(args).current_dir(cwd);
    let out = procs::run(&mut cmd, Duration::from_secs(timeout_s))
        .with_context(|| format!("running {}", exe.display()))?;
    for line in out.combined().lines() {
        println!("    | {line}");
    }
    if out.timed_out {
        bail!("{} {} timed out", exe.display(), args.join(" "));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------

fn check_processes(paths: &Paths) -> Check {
    let moved_sdk = paths.root.parent().map(|p| p.join(MOVED_NAME).join("sdk"));
    let all = procs::list();
    let ours: Vec<String> = all
        .iter()
        .filter(|p| {
            p.exe.as_deref().is_some_and(|e| {
                path_starts_with(e, &paths.sdk)
                    || moved_sdk.as_deref().is_some_and(|m| path_starts_with(e, m))
            })
        })
        .map(|p| format!("{} ({})", p.name, p.pid))
        .collect();
    let others: Vec<String> = all
        .iter()
        .filter(|p| {
            ["emulator.exe", "qemu-system-x86_64.exe", "adb.exe"]
                .iter()
                .any(|n| p.name_is(n))
        })
        .filter(|p| {
            !p.exe
                .as_deref()
                .is_some_and(|e| path_starts_with(e, &paths.sdk))
        })
        .map(|p| format!("{} ({})", p.name, p.pid))
        .collect();
    let addr = SocketAddr::from(([127, 0, 0, 1], ADB_SERVER_PORT));
    let adb_listening = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok();
    let info = if others.is_empty() {
        String::new()
    } else {
        format!(" (INFO: not Shard's, left alone: {})", others.join(", "))
    };
    if ours.is_empty() && !adb_listening {
        pass(format!(
            "no emulator, qemu or adb from Shard's sdk running; nothing listening on {ADB_SERVER_PORT}{info}"
        ))
    } else {
        let mut problems = Vec::new();
        if !ours.is_empty() {
            problems.push(format!("still running: {}", ours.join(", ")));
        }
        if adb_listening {
            problems.push(format!("something still listens on port {ADB_SERVER_PORT}"));
        }
        fail(format!("{}{info}", problems.join("; ")))
    }
}

/// Windows Firewall rules for programs in the Shard folder or its check-h location.
fn firewall_rules(paths: &Paths) -> Result<Vec<String>> {
    let mut rules = winutil::firewall_rules_for(&paths.root)?;
    if let Some(parent) = paths.root.parent() {
        rules.extend(winutil::firewall_rules_for(&parent.join(MOVED_NAME))?);
    }
    Ok(rules)
}

/// While the emulator runs: sockets of Shard's processes that are reachable from the network.
/// TCP listeners must be loopback-only and adb must hold no non-loopback UDP socket (mDNS).
/// qemu's UDP sockets carry the guest's own traffic (DNS and so on) and are not counted.
fn network_exposure(paths: &Paths) -> Vec<String> {
    let ours: BTreeMap<u32, String> = procs::under(&paths.root)
        .into_iter()
        .map(|p| (p.pid, p.name))
        .collect();
    winutil::bound_sockets()
        .into_iter()
        .filter(|s| !s.addr.ip().is_loopback())
        .filter_map(|s| {
            let name = ours.get(&s.pid)?;
            let counts = !s.udp || name.eq_ignore_ascii_case("adb.exe");
            counts.then(|| {
                format!(
                    "{name} ({}) {} {}",
                    s.pid,
                    if s.udp { "UDP" } else { "TCP listen" },
                    s.addr
                )
            })
        })
        .collect()
}

fn check_leaks(
    paths: &Paths,
    start: &leak::Snapshot,
    baseline: Option<&leak::Snapshot>,
    was_running: bool,
    rules_at_start: &[String],
    exposure: Option<&[String]>,
) -> Check {
    let end = leak::take();
    let profile = Profile::new(paths);
    let mut problems = Vec::new();
    match firewall_rules(paths) {
        Ok(rules) if !rules.is_empty() => {
            let added = rules.iter().filter(|r| !rules_at_start.contains(r)).count();
            problems.push(format!(
                "Windows Firewall holds {} rule(s) for programs in Shard's folder ({added} added \
                 during this run): {}",
                rules.len(),
                rules.join("; ")
            ));
        }
        Ok(_) => {}
        Err(e) => problems.push(format!("{e:#}")),
    }
    if let Some(open) = exposure {
        if !open.is_empty() {
            problems.push(format!(
                "reachable from the network while running: {}",
                open.join(", ")
            ));
        }
    }
    let left = profile.leftovers();
    if !left.is_empty() {
        problems.push(format!("Shard left behind: {}", left.join(", ")));
    } else if profile.pending().is_some() {
        problems.push("state\\placed.toml still lists items to remove".into());
    }
    if !was_running {
        let d = leak::diff(start, &end);
        if !d.is_empty() {
            problems.push(format!("changed during verify: {}", d.describe()));
        }
    }
    let mut baseline_note = "";
    match baseline {
        Some(base) => {
            // Against the first-setup baseline only Shard's own targets count: the Desktop and
            // the name scan can legitimately change between setup and now (other software).
            let mut d = leak::diff(base, &end);
            d.added.clear();
            d.changed.retain(|(t, _, _)| !t.starts_with("Desktop"));
            if !d.is_empty() {
                problems.push(format!("differs from before setup: {}", d.describe()));
            }
        }
        None => baseline_note = " (no setup baseline in state\\; compared with verify start only)",
    }
    if problems.is_empty() {
        let targets: Vec<&str> = end.targets.keys().map(String::as_str).collect();
        let net = match exposure {
            Some(_) => {
                "; no firewall rules; while running, no TCP listener beyond 127.0.0.1 and no adb UDP"
            }
            None => "; no firewall rules",
        };
        pass(format!(
            "unchanged: {}; no new Android/emulator names in %USERPROFILE%, %LOCALAPPDATA%, %APPDATA%, %TEMP%, HKCU\\Software{net}{baseline_note}",
            targets.join(", ")
        ))
    } else {
        fail(problems.join("; "))
    }
}

fn rel(paths: &Paths, p: &Path) -> String {
    p.strip_prefix(&paths.root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}
