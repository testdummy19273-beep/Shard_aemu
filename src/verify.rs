//! `--headless verify`: the brief's checks a-i plus j (a hardware-setting change), one
//! `PASS|FAIL|SKIP <id> <detail>` line each, exit code 1 if any check failed.
//!
//! Execution order differs from the printed order: the emulator is booted (c, d, e), debloated
//! (f), a package restored and removed again (g), taken through two hardware-setting changes
//! (j) and stopped first, then the folder is moved (h), and only then are leftovers (b) and
//! processes (i) checked, when nothing of Shard's should be running any more.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::adb::Adb;
use crate::config::Config;
use crate::device::{self, Device};
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
                let dev = Device {
                    adb: &adb,
                    serial: &r.serial,
                };
                let before = listed_installed(paths, &dev);
                println!("--- debloat");
                let (debloat, _) = run_on(&engine, Command::Debloat, Job::Debloat, &mut printer);
                checks.insert('f', check_debloat(paths, &dev, before, debloat));
                println!("--- restore");
                checks.insert('g', check_restore(paths, &dev));
                exposure = Some(network_exposure(paths));
                println!("--- settings change");
                checks.insert('j', check_settings_change(paths, &engine, &mut printer));
            }
            _ => {
                let why = boot
                    .clone()
                    .err()
                    .unwrap_or("no running emulator reported".into());
                checks.insert('c', fail(format!("boot failed: {why}")));
                for id in ['d', 'e', 'f', 'g', 'j'] {
                    checks.insert(id, fail("not checked: the emulator did not boot"));
                }
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
        for id in ['c', 'd', 'e', 'f', 'g', 'j'] {
            checks.insert(id, fail(msg));
        }
        checks.insert('h', skip(msg));
    }
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
// f, g: debloat and restore.

/// debloat.txt packages (not keep-listed) installed for user 0, before verify's own debloat.
/// On a device whose first boot did its work this is empty.
fn listed_installed(paths: &Paths, dev: &Device) -> Option<Vec<String>> {
    let list = device::load_debloat(paths).ok()?;
    let keep = device::keep_list(&dev.detect());
    let installed = dev.packages().ok()?;
    Some(device::plan(&list.packages, &keep, &installed).remove)
}

fn check_debloat(
    paths: &Paths,
    dev: &Device,
    before: Option<Vec<String>>,
    job: Result<(), String>,
) -> Check {
    if let Err(e) = job {
        return fail(format!("debloat failed: {e}"));
    }
    let list = match device::load_debloat(paths) {
        Ok(l) => l,
        Err(e) => return fail(format!("{e:#}")),
    };
    let detected = dev.detect();
    let keep = device::keep_list(&detected);
    let installed = match dev.packages() {
        Ok(i) => i,
        Err(e) => return fail(format!("{e:#}")),
    };
    let plan = device::plan(&list.packages, &keep, &installed);
    let missing = device::missing_keep(&keep, &installed);
    let mut problems = Vec::new();
    if !plan.remove.is_empty() {
        problems.push(format!(
            "still installed for user 0: {}",
            plan.remove.join(", ")
        ));
    }
    if !missing.is_empty() {
        problems.push(format!(
            "keep-list packages missing: {}",
            missing.join(", ")
        ));
    }
    if !problems.is_empty() {
        return fail(problems.join("; "));
    }
    let before = match before {
        Some(b) if b.is_empty() => "none (the first boot had removed them)".to_string(),
        Some(b) => format!("{} ({})", b.len(), b.join(", ")),
        None => "unknown".to_string(),
    };
    let roles: Vec<String> = detected
        .roles()
        .into_iter()
        .map(|(role, pkg, fallback)| {
            format!("{role} {pkg}{}", if fallback { " (fallback)" } else { "" })
        })
        .collect();
    pass(format!(
        "{} debloat.txt packages: none installed for user 0 (installed before verify's debloat: \
         {before}; keep-listed {}); keep-list present: {} packages incl. {}",
        list.packages.len(),
        plan.kept.len(),
        keep.iter().filter(|p| installed.contains(*p)).count(),
        roles.join(", ")
    ))
}

/// Restores one debloat.txt package, checks it is back, removes it again.
fn check_restore(paths: &Paths, dev: &Device) -> Check {
    let list = match device::load_debloat(paths) {
        Ok(l) => l,
        Err(e) => return fail(format!("{e:#}")),
    };
    let keep = device::keep_list(&dev.detect());
    let on_image = dev
        .adb
        .shell(
            dev.serial,
            "pm list packages -u --user 0",
            Duration::from_secs(60),
        )
        .map(|o| device::parse_package_list(&o))
        .unwrap_or_default();
    let Some(pkg) = list
        .packages
        .iter()
        .find(|p| !keep.contains(*p) && on_image.contains(*p))
    else {
        return fail("debloat.txt lists no removable package that exists on this image");
    };
    let installed = |dev: &Device| dev.packages().map(|s| s.contains(pkg));
    let restored = dev.restore(pkg).and_then(|()| match installed(dev)? {
        true => Ok(()),
        false => bail!("{pkg} is not listed for user 0 after the restore"),
    });
    // Remove it again whatever happened, so the device ends as debloat left it.
    let removed = dev.remove(pkg).and_then(|()| match installed(dev)? {
        false => Ok(()),
        true => bail!("{pkg} is still listed for user 0 after removing it again"),
    });
    // Before the stop that follows, or a later cold boot brings it back.
    device::wait_for_package_save(Some(Instant::now()), &AtomicBool::new(false));
    match (restored, removed) {
        (Ok(()), Ok(())) => pass(format!(
            "restored {pkg} (install-existing), it was installed for user 0; removed it again"
        )),
        (Err(e), Ok(())) => fail(format!("{e:#} ({pkg} removed again)")),
        (r, Err(e)) => fail(format!(
            "{}; removing it again failed: {e:#}",
            r.err()
                .map(|e| format!("{e:#}"))
                .unwrap_or(format!("restored {pkg}"))
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// j: a hardware setting changes while there are changes only the Quick Boot snapshot holds.

const RESCUED: &str = "Android shut down cleanly; the last session is saved";
const SHUT_DOWN: &str = "Android shut down cleanly";

/// Two rounds, each with an app removal Android has not written to disk yet (so it lives only
/// in the running device and the snapshot): (1) stop with a snapshot, change `cores`, start;
/// (2) change `cores` back while running, stop, start. After each start the removal must have
/// survived and the guest must have the new core count. shard.toml is restored whatever happens.
fn check_settings_change(paths: &Paths, engine: &Engine, printer: &mut Printer) -> Check {
    let file = paths.config_file();
    let original = match std::fs::read(&file) {
        Ok(b) => b,
        Err(e) => return fail(format!("reading shard.toml: {e}")),
    };
    let cfg = match Config::load_or_create(&file) {
        Ok(c) => c,
        Err(e) => return fail(format!("{e:#}")),
    };
    if !cfg.emulator.quick_boot {
        return skip("Quick Boot is off in shard.toml (there is no snapshot to lose)");
    }
    let mut probe = None;
    let result = settings_change_test(paths, engine, printer, &cfg, &mut probe);
    let restored = std::fs::write(&file, &original);
    // Leave the device as debloat left it.
    let tidy = probe.map(|pkg| -> Result<()> {
        let serial = emulator::load_running(paths)
            .map(|r| r.serial)
            .context("no emulator running to remove it on")?;
        let adb = Adb::new(paths);
        let dev = Device {
            adb: &adb,
            serial: &serial,
        };
        if dev.packages()?.contains(&pkg) {
            dev.remove(&pkg)?;
            device::wait_for_package_save(Some(Instant::now()), &AtomicBool::new(false));
        }
        Ok(())
    });
    let mut problems = Vec::new();
    if let Err(e) = restored {
        problems.push(format!("could not restore shard.toml: {e}"));
    }
    if let Some(Err(e)) = tidy {
        problems.push(format!("could not remove the probe app again: {e:#}"));
    }
    match result {
        Ok(d) if problems.is_empty() => pass(format!("{d}; shard.toml restored")),
        Ok(d) => fail(format!("{d}; but {}", problems.join("; "))),
        Err(e) => {
            problems.insert(0, format!("{e:#}"));
            fail(format!("{} (shard.toml restored)", problems.join("; ")))
        }
    }
}

fn settings_change_test(
    paths: &Paths,
    engine: &Engine,
    printer: &mut Printer,
    cfg: &Config,
    probe: &mut Option<String>,
) -> Result<String> {
    let adb = Adb::new(paths);
    let serial = || {
        emulator::load_running(paths)
            .map(|r| r.serial)
            .context("no running emulator recorded")
    };
    let pkg = {
        let s = serial()?;
        probe_package(
            paths,
            &Device {
                adb: &adb,
                serial: &s,
            },
        )?
    };
    *probe = Some(pkg.clone());
    let cores = cfg.emulator.cores;
    let other = if cores > 1 { cores - 1 } else { 2 };
    let job = |cmd: Command, job: Job, printer: &mut Printer| -> Result<usize> {
        let mark = printer.lines.len();
        let (r, _) = run_on(engine, cmd, job, printer);
        r.map_err(|e| anyhow::anyhow!("{job:?} failed: {e}"))?;
        Ok(mark)
    };
    let saw = |printer: &Printer, mark: usize, line: &str| {
        printer.lines[mark..].iter().any(|l| l == line)
    };

    // Round 1: the emulator is stopped with a snapshot when the setting changes.
    let s = serial()?;
    unsaved_removal(&adb, &s, &pkg)?;
    job(Command::Stop, Job::Stop, printer)?;
    if !crate::avd::has_snapshot(paths) {
        bail!("Stop with Quick Boot on left no snapshot");
    }
    set_cores(paths, cfg, other)?;
    let mark = job(Command::Boot, Job::Boot, printer)?;
    let s = serial()?;
    expect_after_start(&adb, &s, &pkg, other, "stopped with a snapshot")?;
    if !saw(printer, mark, RESCUED) {
        bail!("the start did not report saving the last session from the snapshot");
    }

    // Round 2: the setting changes back while the emulator runs.
    unsaved_removal(&adb, &s, &pkg)?;
    set_cores(paths, cfg, cores)?;
    let mark = job(Command::Stop, Job::Stop, printer)?;
    let clean = saw(printer, mark, SHUT_DOWN);
    job(Command::Boot, Job::Boot, printer)?;
    let s = serial()?;
    expect_after_start(&adb, &s, &pkg, cores, "running")?;
    if !clean {
        bail!("Stop did not shut Android down cleanly after the settings changed");
    }
    Ok(format!(
        "an unsaved removal of {pkg} survived cores {cores}->{other} while stopped with a \
         snapshot (started from it, shut down, cold booted) and {other}->{cores} while running \
         (clean shutdown on stop); the guest had {other}, then {cores} CPUs"
    ))
}

/// A debloat.txt package that exists on the image and is not keep-listed.
fn probe_package(paths: &Paths, dev: &Device) -> Result<String> {
    let list = device::load_debloat(paths)?;
    let keep = device::keep_list(&dev.detect());
    let on_image = device::parse_package_list(&dev.adb.shell(
        dev.serial,
        "pm list packages -u --user 0",
        Duration::from_secs(60),
    )?);
    list.packages
        .into_iter()
        .find(|p| !keep.contains(p) && on_image.contains(p))
        .context("debloat.txt lists no removable package that exists on this image")
}

/// Restores `pkg` and waits until that is on disk, then removes it and does not wait: the
/// removal exists only in the running device (Android saves it about 10 s later).
fn unsaved_removal(adb: &Adb, serial: &str, pkg: &str) -> Result<()> {
    let dev = Device { adb, serial };
    if !dev.packages()?.contains(pkg) {
        dev.restore(pkg)?;
        device::wait_for_package_save(Some(Instant::now()), &AtomicBool::new(false));
    }
    dev.remove(pkg)
}

fn set_cores(paths: &Paths, cfg: &Config, cores: u32) -> Result<()> {
    let mut c = cfg.clone();
    c.emulator.cores = cores;
    c.save(&paths.config_file())
}

fn expect_after_start(adb: &Adb, serial: &str, pkg: &str, cores: u32, when: &str) -> Result<()> {
    let dev = Device { adb, serial };
    if dev.packages()?.contains(pkg) {
        bail!("{pkg} is installed again after the start (settings changed while {when}): the removal was lost");
    }
    let n: u32 = adb
        .shell(serial, "nproc", Duration::from_secs(10))?
        .trim()
        .parse()
        .context("reading the guest's CPU count")?;
    if n != cores {
        bail!("the guest has {n} CPUs after the start, expected {cores} (settings changed while {when})");
    }
    Ok(())
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
