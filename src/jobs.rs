//! The worker: one thread runs setup, boot, debloat, wipe and stop and owns the emulator
//! session. The UI and the headless runner send it commands and render only the events it
//! sends back.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::adb::Adb;
use crate::config::Config;
use crate::device::{self, Device, Markers};
use crate::emulator::{self, BootMode, BootOutcome, RunningInfo};
use crate::paths::Paths;
use crate::profile::Profile;
use crate::sdk::download::{self, Cancelled, Downloader, Spec};
use crate::sdk::manifest::{self, PackageId};
use crate::sdk::{self as sdkmod, install, Installed, InstalledPackage};
use crate::state::{AppState, Input};
use crate::{avd, hv, leak, preflight, procs};

const DOWNLOAD_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
/// How often an adopted emulator (no child handle) is checked for having exited.
const ADOPTED_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum Command {
    /// Download and install what is missing, build the AVD. `dev_cache` keeps archives there.
    Setup {
        dev_cache: Option<PathBuf>,
    },
    Boot,
    /// Removes the debloat.txt packages (starting the emulator first if needed; it is then
    /// left running).
    Debloat,
    /// Wipe data on the next start (the emulator must be stopped).
    WipeData,
    Stop,
    /// Ends the worker. A running emulator is left running (the next Shard adopts it);
    /// otherwise the adb server is stopped and Shard's temporary profile items are removed.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Setup,
    Boot,
    Debloat,
    Wipe,
    Stop,
}

#[derive(Debug, Clone)]
pub enum Event {
    State(AppState),
    /// A new step of the current job ("Downloading Android Emulator 37.2.12").
    Step(String),
    Progress {
        label: String,
        done: u64,
        total: u64,
    },
    Log(String),
    Warning(String),
    Running {
        serial: String,
        renderer: Option<String>,
        boot_secs: Option<f64>,
    },
    Finished {
        job: Job,
        result: Result<(), String>,
    },
    ShutDown,
}

/// Sends events and wakes the UI. Cheap to clone into callbacks.
#[derive(Clone)]
pub struct Emitter {
    tx: Sender<Event>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Emitter {
    pub fn emit(&self, ev: Event) {
        match &ev {
            Event::State(s) => tracing::info!("state: {}", s.label()),
            Event::Step(s) => tracing::info!("== {s}"),
            Event::Log(s) => tracing::info!("{s}"),
            Event::Warning(s) => tracing::warn!("{s}"),
            Event::Running {
                serial, renderer, ..
            } => tracing::info!("running: {serial}, renderer {renderer:?}"),
            Event::Finished { job, result } => match result {
                Ok(()) => tracing::info!("{job:?} finished"),
                Err(e) => tracing::error!("{job:?} failed: {e}"),
            },
            Event::Progress { .. } | Event::ShutDown => {}
        }
        let _ = self.tx.send(ev);
        (self.notify)();
    }
    pub fn log(&self, msg: impl Into<String>) {
        self.emit(Event::Log(msg.into()));
    }
    pub fn warn(&self, msg: impl Into<String>) {
        self.emit(Event::Warning(msg.into()));
    }
    pub fn step(&self, msg: impl Into<String>) {
        self.emit(Event::Step(msg.into()));
    }
}

pub struct Engine {
    tx: Sender<Command>,
    pub events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    /// Starts the worker. It first looks for an emulator from this folder that is still running
    /// (adopts it) or for leftovers of a crash (cleans them up), then reports its state.
    pub fn start(paths: Paths, notify: Arc<dyn Fn() + Send + Sync>) -> Engine {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (etx, events) = crossbeam_channel::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            adb: Adb::new(&paths),
            profile: Profile::new(&paths),
            paths,
            state: AppState::NotSetUp,
            session: None,
            cancel: cancel.clone(),
            em: Emitter { tx: etx, notify },
            last_poll: Instant::now(),
            fresh_data: false,
        };
        let thread = std::thread::Builder::new()
            .name("shard-worker".into())
            .spawn(move || worker.run(rx))
            .expect("spawning the worker thread");
        Engine {
            tx,
            events,
            cancel,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: Command) {
        if matches!(
            cmd,
            Command::Setup { .. } | Command::Boot | Command::Debloat
        ) {
            self.cancel.store(false, Ordering::Relaxed);
        }
        let _ = self.tx.send(cmd);
    }

    /// Asks the running setup or boot to stop as soon as it can.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Sends Shutdown and waits up to `wait` for the worker to finish (it may be busy with a
    /// long job; then the process can simply exit and the next start adopts or cleans up).
    pub fn shutdown(mut self, wait: Duration) -> bool {
        let _ = self.tx.send(Command::Shutdown);
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(Event::ShutDown) => break,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        true
    }
}

/// Every package recorded as installed and on disk, and the AVD written.
pub fn is_set_up(paths: &Paths) -> bool {
    Installed::load(paths).complete(paths) && paths.avd_config().is_file()
}

struct Session {
    /// None when adopted from an earlier Shard process.
    child: Option<Child>,
    info: RunningInfo,
}

struct Worker {
    paths: Paths,
    adb: Adb,
    profile: Profile,
    state: AppState,
    session: Option<Session>,
    cancel: Arc<AtomicBool>,
    em: Emitter,
    last_poll: Instant,
    /// The current boot started from fresh user data (new device or wipe).
    fresh_data: bool,
}

impl Worker {
    fn run(mut self, rx: Receiver<Command>) {
        self.init();
        loop {
            let timeout = if self.session.is_some() {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(3600)
            };
            match rx.recv_timeout(timeout) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown();
                    self.em.emit(Event::ShutDown);
                    return;
                }
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => self.watch(),
            }
        }
    }

    fn set(&mut self, input: Input) {
        match self.state.apply(&input) {
            Ok(next) => {
                self.state = next.clone();
                self.em.emit(Event::State(next));
            }
            Err(e) => tracing::warn!("ignored state input: {e}"),
        }
    }

    /// Like [`set`], but an invalid transition is an error for the caller.
    fn begin(&mut self, input: Input, what: &str) -> Result<()> {
        if self.state.apply(&input).is_err() {
            bail!("cannot {what} while Shard is {}", self.state.label());
        }
        self.set(input);
        Ok(())
    }

    fn init(&mut self) {
        let p = self.paths.clone();
        if emulator::qemu_running(&p) {
            if let Some(info) = emulator::find_running(&p) {
                self.adopt(info);
                return;
            }
        }
        // Nothing of ours is running: finish whatever a crash or a kill left behind.
        if !emulator::emulator_processes(&p).is_empty()
            || !procs::under(&p.platform_tools_dir()).is_empty()
        {
            self.em
                .log("cleaning up emulator/adb processes left from an earlier run");
            let _ = emulator::stop(&p, &self.adb, None);
        }
        if self.profile.pending().is_some() {
            let em = self.em.clone();
            if let Err(e) = self.profile.give_back(&mut |m| em.log(m)) {
                self.em.warn(format!("{e:#}"));
            }
        }
        emulator::clear_running(&p);
        self.state = if is_set_up(&p) {
            AppState::Stopped
        } else {
            AppState::NotSetUp
        };
        self.em.emit(Event::State(self.state.clone()));
    }

    fn adopt(&mut self, info: RunningInfo) {
        self.em.log(format!(
            "found the emulator still running ({}); taking it over",
            info.serial
        ));
        if let Err(e) = self.adb.start_server() {
            self.em.warn(format!("{e:#}"));
        }
        let renderer = host_renderer(&self.adb, &info);
        self.session = Some(Session {
            child: None,
            info: info.clone(),
        });
        self.last_poll = Instant::now();
        self.state = AppState::Stopped;
        self.set(Input::Adopted);
        self.em.emit(Event::Running {
            serial: info.serial,
            renderer,
            boot_secs: None,
        });
    }

    fn handle(&mut self, cmd: Command) {
        let (job, result) = match cmd {
            Command::Setup { dev_cache } => (Job::Setup, self.setup(dev_cache.as_deref())),
            Command::Boot => (Job::Boot, self.boot()),
            Command::Debloat => (Job::Debloat, self.debloat()),
            Command::WipeData => (Job::Wipe, self.wipe()),
            Command::Stop => (Job::Stop, self.stop()),
            Command::Shutdown => unreachable!("handled in run"),
        };
        self.em.emit(Event::Finished {
            job,
            result: result.map_err(|e| format!("{e:#}")),
        });
    }

    // -----------------------------------------------------------------------------------------
    // Setup

    fn setup(&mut self, dev_cache: Option<&Path>) -> Result<()> {
        self.begin(Input::SetupStarted, "set up")?;
        match self.do_setup(dev_cache) {
            Ok(()) => {
                self.set(Input::SetupDone);
                Ok(())
            }
            Err(e) if download::is_cancelled(&e) => {
                let set_up = is_set_up(&self.paths);
                self.set(Input::SetupAborted { set_up });
                Err(anyhow!("setup cancelled"))
            }
            Err(e) => {
                self.set(Input::Failed(format!("Setup failed: {e:#}")));
                Err(e)
            }
        }
    }

    fn do_setup(&mut self, dev_cache: Option<&Path>) -> Result<()> {
        let p = self.paths.clone();
        let em = self.em.clone();
        p.ensure_dirs()?;
        // Before any adb or emulator process runs: what the profile looked like without Shard.
        leak::record_baseline(&p)?;

        em.step("Checking the Shard folder");
        for w in preflight::path_warnings(&p) {
            em.warn(w);
        }
        if let Some(w) = preflight::disk_warning(&p) {
            em.warn(w);
        }

        em.step("Checking Windows Hypervisor Platform");
        let whpx = hv::whpx();
        if !whpx.usable() {
            bail!("{}", whpx.describe());
        }
        em.log(whpx.describe());
        if emulator::qemu_running(&p) {
            bail!("the emulator is running; stop it before running setup");
        }

        em.step("Reading Google's SDK manifests");
        let client = download::client()?;
        let plan = manifest::resolve(&client)
            .context("could not read Google's SDK manifests (is the PC online?)")?;
        for id in PackageId::ALL {
            let s = plan.get(id);
            em.log(format!(
                "{}: {} {} ({}, {})",
                id.label(),
                s.path,
                s.revision,
                s.file_name(),
                mb(s.archive.size)
            ));
        }

        let cache_dir = dev_cache.map(Path::to_path_buf).unwrap_or(p.cache.clone());
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating {}", cache_dir.display()))?;
        let mut installed = Installed::load(&p);
        em.log(format!("total download: {}", mb(plan.total_bytes())));
        for id in PackageId::ALL {
            let sel = plan.get(id);
            if installed.has(&p, sel) {
                em.log(format!(
                    "{} {} is already installed",
                    id.label(),
                    sel.revision
                ));
                continue;
            }
            let dest = cache_dir.join(sel.file_name());
            em.step(format!("Downloading {} {}", id.label(), sel.revision));
            {
                let label = format!("Downloading {} ({})", id.label(), mb(sel.archive.size));
                let pem = em.clone();
                let mut throttle = Throttle::new();
                let mut progress = |done: u64, size: u64| {
                    if throttle.ready(done == size) {
                        pem.emit(Event::Progress {
                            label: label.clone(),
                            done,
                            total: size,
                        });
                    }
                };
                let lem = em.clone();
                let mut log = |m: String| lem.log(m);
                let mut dl = Downloader {
                    client: &client,
                    cancel: &self.cancel,
                    progress: &mut progress,
                    log: &mut log,
                    backoff: DOWNLOAD_BACKOFF,
                };
                dl.fetch(&Spec {
                    url: sel.url.clone(),
                    dest: dest.clone(),
                    size: sel.archive.size,
                    checksum: sel.archive.checksum.clone(),
                })?;
            }
            em.step(format!("Installing {} {}", id.label(), sel.revision));
            {
                let label = format!("Extracting {}", id.label());
                let pem = em.clone();
                let mut throttle = Throttle::new();
                let mut progress = |done: u64, n: u64| {
                    if throttle.ready(done == n) {
                        pem.emit(Event::Progress {
                            label: label.clone(),
                            done,
                            total: n,
                        });
                    }
                };
                install::extract(
                    &dest,
                    &sdkmod::staging_dir(&p, id),
                    &sdkmod::target_dir(&p, id),
                    &self.cancel,
                    &mut progress,
                )?;
            }
            installed
                .packages
                .insert(id.key().to_string(), InstalledPackage::from_selected(sel));
            installed.save(&p)?;
            if dev_cache.is_none() {
                let _ = std::fs::remove_file(&dest);
            }
        }
        let _ = install::remove_dir_all_retry(&p.sdk.join(".staging"));

        em.step("Creating the Android device");
        emulator::seed_update_check(&p)?;
        let cfg = Config::load_or_create(&p.config_file())?;
        avd::write(&p, &cfg.emulator)?;
        device::ensure_debloat_file(&p)?;

        em.step("Checking hardware acceleration");
        let accel = hv::accel_check(&p)?;
        if !accel.usable {
            bail!(
                "the emulator cannot use hardware acceleration: {}. {}",
                accel.message,
                hv::ENABLE_WHPX_HELP
            );
        }
        em.log(format!("emulator -accel-check: {}", accel.message));
        if !is_set_up(&p) {
            bail!("setup finished but the SDK or AVD is incomplete");
        }
        em.log("setup complete");
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Boot

    fn boot(&mut self) -> Result<()> {
        if self.state == AppState::Running {
            return Ok(());
        }
        if !is_set_up(&self.paths) {
            bail!("Shard is not set up yet; run setup first");
        }
        if emulator::qemu_running(&self.paths) {
            if let Some(info) = emulator::find_running(&self.paths) {
                self.adopt(info);
                self.after_boot();
                return Ok(());
            }
        }
        self.begin(Input::BootStarted, "start the emulator")?;
        match self.do_boot() {
            Ok(()) => {
                self.after_boot();
                Ok(())
            }
            Err(e) if e.is::<Cancelled>() => {
                self.teardown();
                self.set(Input::StopStarted);
                self.set(Input::Stopped);
                Err(anyhow!("boot cancelled"))
            }
            Err(e) => {
                self.teardown();
                self.set(Input::Failed(format!("Boot failed: {e:#}")));
                Err(e)
            }
        }
    }

    fn do_boot(&mut self) -> Result<()> {
        let p = self.paths.clone();
        let em = self.em.clone();
        let cfg = Config::load_or_create(&p.config_file())?;
        avd::write(&p, &cfg.emulator)?;
        emulator::seed_update_check(&p)?;
        let hw = avd::hardware_hash(&cfg.emulator);
        let mut mode = if device::wipe_pending(&p) {
            BootMode::Wipe
        } else if !cfg.emulator.quick_boot {
            BootMode::NoSnapshot
        } else if load_hw_hash(&p).as_deref() != Some(hw.as_str()) || !avd::has_snapshot(&p) {
            BootMode::Cold
        } else {
            BootMode::Quick
        };

        self.fresh_data = device::prepare_fresh_start(&p)?;

        em.step("Preparing adb");
        self.profile.borrow(&mut |m| em.log(m))?;
        self.adb.start_server()?;

        loop {
            let port = emulator::pick_port(cfg.emulator.port, emulator::port_is_free)
                .ok_or_else(|| anyhow!("no free emulator port between 5554 and 5585"))?;
            em.step(match mode {
                BootMode::Quick => "Starting the emulator (Quick Boot)",
                BootMode::Cold => "Starting the emulator (cold boot)",
                BootMode::NoSnapshot => "Starting the emulator (Quick Boot off)",
                BootMode::Wipe => "Starting the emulator with wiped data",
            });
            let mut launched = emulator::launch(&p, &cfg.emulator, port, mode)?;
            let info = RunningInfo {
                port,
                serial: launched.serial.clone(),
                log: launched.log.clone(),
            };
            emulator::save_running(&p, &info)?;
            em.log(format!(
                "emulator started as {} (log: {})",
                info.serial,
                info.log.display()
            ));
            let outcome =
                emulator::wait_for_boot(&self.adb, &mut launched, &self.cancel, &mut |m| {
                    em.log(m)
                })?;
            match outcome {
                BootOutcome::Booted { elapsed } => {
                    save_hw_hash(&p, &hw)?;
                    if mode == BootMode::Wipe {
                        device::wipe_done(&p)?;
                        em.log("the device's data was reset");
                    }
                    let renderer = host_renderer(&self.adb, &info);
                    em.log(format!("booted in {:.1} s", elapsed.as_secs_f64()));
                    match &renderer {
                        Some(r) if emulator::is_software_renderer(r) => em.warn(format!(
                            "The emulator is drawing in software ({r}); games will be slow."
                        )),
                        Some(r) => em.log(format!("renderer: {r}")),
                        None => em.log("the emulator log does not name the renderer"),
                    }
                    self.session = Some(Session {
                        child: Some(launched.child),
                        info: info.clone(),
                    });
                    self.set(Input::Booted);
                    em.emit(Event::Running {
                        serial: info.serial,
                        renderer,
                        boot_secs: Some(elapsed.as_secs_f64()),
                    });
                    return Ok(());
                }
                BootOutcome::QuickBootStuck { reason } if mode == BootMode::Quick => {
                    em.warn(format!(
                        "The Quick Boot snapshot did not come up ({reason}); cold booting instead."
                    ));
                    // No graceful stop: that would save this broken state as the snapshot.
                    kill_emulator(&p);
                    let _ = launched.child.wait();
                    mode = BootMode::Cold;
                }
                BootOutcome::QuickBootStuck { reason } => {
                    bail!("the emulator did not come up: {reason}")
                }
                BootOutcome::Exited { code, tail } => bail!(
                    "the emulator exited during boot (exit code {}). Last lines of its log:\n{tail}",
                    code.map_or("unknown".into(), |c| c.to_string())
                ),
                BootOutcome::TimedOut { tail } => bail!(
                    "the emulator did not finish booting within {} s. Last lines of its log:\n{tail}",
                    mode.timeout().as_secs()
                ),
                BootOutcome::Cancelled => return Err(Cancelled.into()),
            }
        }
    }

    /// After a failed or cancelled boot: kill what is left, stop adb, give the profile back.
    fn teardown(&mut self) {
        let p = self.paths.clone();
        kill_emulator(&p);
        if let Some(mut s) = self.session.take() {
            if let Some(c) = s.child.as_mut() {
                let _ = c.wait();
            }
        }
        emulator::stop_adb_server(&p, &self.adb);
        emulator::clear_running(&p);
        self.give_back();
    }

    fn give_back(&mut self) -> bool {
        let em = self.em.clone();
        match self.profile.give_back(&mut |m| em.log(m)) {
            Ok(()) => true,
            Err(e) => {
                em.warn(format!("{e:#}"));
                false
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // First boot, debloat, wipe

    fn running_serial(&self) -> Option<String> {
        self.session.as_ref().map(|s| s.info.serial.clone())
    }

    /// Once per device (and again after Wipe data): save the package list, debloat, apply the
    /// tweaks, open the Play Store. Never fails the boot: problems are warnings, and whatever
    /// did not complete runs again on the next start.
    fn after_boot(&mut self) {
        if Markers::load(&self.paths).first_boot_done() {
            return;
        }
        let Some(serial) = self.running_serial() else {
            return;
        };
        let result = self.first_boot(&serial);
        self.fresh_data = false;
        match result {
            Ok(()) => {}
            Err(e) if e.is::<Cancelled>() => self.em.warn(
                "first-boot setup was cancelled; it runs again on the next start".to_string(),
            ),
            Err(e) => self.em.warn(format!(
                "first-boot setup did not finish: {e:#}. It runs again on the next start."
            )),
        }
    }

    fn first_boot(&self, serial: &str) -> Result<()> {
        let p = &self.paths;
        let em = self.em.clone();
        let dev = Device {
            adb: &self.adb,
            serial,
        };
        em.step("Setting up Android (first boot)");
        let launcher = dev.detect().roles()[0].1.clone();
        em.log(format!("waiting for the launcher ({launcher})"));
        if !dev.wait_for_launcher(&launcher, device::LAUNCHER_TIMEOUT, &self.cancel) {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            em.warn(format!(
                "the launcher did not come up within {} s; continuing anyway",
                device::LAUNCHER_TIMEOUT.as_secs()
            ));
        }
        // Only the list from before any debloat (a retry must not overwrite it).
        if self.fresh_data || !device::initial_packages_file(p).is_file() {
            let file = device::save_initial_packages(p, &dev)?;
            em.log(format!("package list saved to {}", file.display()));
        }

        em.step("Removing the apps listed in debloat.txt");
        let report = device::debloat(p, &dev, &self.cancel, &mut |m| em.log(m), &mut |m| {
            em.warn(m)
        })
        .map_err(|e| {
            if self.cancel.load(Ordering::Relaxed) {
                Cancelled.into()
            } else {
                e
            }
        })?;
        em.log(report.summary());
        let mut markers = Markers::load(p);
        if report.failed.is_empty() {
            markers.debloated = Some(crate::logging::stamp());
        }

        em.step("Applying tweaks");
        dev.apply_tweaks()?;
        em.log("animations at 0.5x, stay awake while charging");
        markers.tweaked = Some(crate::logging::stamp());
        markers.save(p)?;

        em.step("Opening the Play Store");
        match dev.open_play_store() {
            Ok(()) => em.log("Play Store opened: sign in there with your Google account"),
            Err(e) => em.warn(format!("{e:#}")),
        }
        if !report.failed.is_empty() {
            bail!("could not remove: {}", report.failed.join(", "));
        }
        Ok(())
    }

    fn debloat(&mut self) -> Result<()> {
        if self.state != AppState::Running {
            self.boot()?;
            self.em
                .log("the emulator was started for this and is left running");
        }
        let serial = self
            .running_serial()
            .ok_or_else(|| anyhow!("the emulator is not running"))?;
        let em = self.em.clone();
        let dev = Device {
            adb: &self.adb,
            serial: &serial,
        };
        em.step("Removing the apps listed in debloat.txt");
        let report = device::debloat(
            &self.paths,
            &dev,
            &self.cancel,
            &mut |m| em.log(m),
            &mut |m| em.warn(m),
        )?;
        em.log(report.summary());
        if !report.failed.is_empty() {
            bail!("could not remove: {}", report.failed.join(", "));
        }
        let mut markers = Markers::load(&self.paths);
        if markers.debloated.is_none() {
            markers.debloated = Some(crate::logging::stamp());
            markers.save(&self.paths)?;
        }
        Ok(())
    }

    /// Wipe data: the next start resets the device to the image's clean user data, then runs
    /// the first-boot setup again.
    fn wipe(&mut self) -> Result<()> {
        let p = self.paths.clone();
        if self.session.is_some() || emulator::qemu_running(&p) {
            bail!("stop the emulator before wiping its data");
        }
        if !is_set_up(&p) {
            bail!("Shard is not set up yet; there is no data to wipe");
        }
        self.em.step("Wiping the device's data");
        device::request_wipe(&p)?;
        // The Quick Boot snapshot holds the old data; it must never be loaded again.
        let snapshot = p.avd_dir().join("snapshots").join("default_boot");
        if snapshot.exists() {
            install::remove_dir_all_retry(&snapshot)
                .with_context(|| format!("removing {}", snapshot.display()))?;
        }
        self.em.log(
            "the device's data is reset on the next start; apps, accounts and settings on it \
             are erased, and the first-boot setup (debloat, tweaks) runs again",
        );
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Stop, crash watch, shutdown

    fn stop(&mut self) -> Result<()> {
        let p = self.paths.clone();
        if self.session.is_none() && !emulator::qemu_running(&p) {
            emulator::stop_adb_server(&p, &self.adb);
            if !self.give_back() {
                bail!("could not remove Shard's temporary profile items");
            }
            return Ok(());
        }
        self.begin(Input::StopStarted, "stop the emulator")?;
        self.em.step("Stopping the emulator");
        let serial = self
            .session
            .as_ref()
            .map(|s| s.info.serial.clone())
            .or_else(|| emulator::find_running(&p).map(|r| r.serial));
        let report = emulator::stop(&p, &self.adb, serial.as_deref());
        if let Some(mut s) = self.session.take() {
            if let Some(c) = s.child.as_mut() {
                let _ = c.wait();
            }
        }
        match &report {
            Ok(r) if r.graceful => self.em.log("the emulator shut down cleanly"),
            Ok(_) => self.em.warn(format!(
                "the emulator did not exit within {} s and was terminated",
                emulator::STOP_TIMEOUT.as_secs()
            )),
            Err(e) => self.em.warn(format!("{e:#}")),
        }
        if let Ok(r) = &report {
            if !r.killed.is_empty() {
                self.em
                    .log(format!("ended leftover processes: {}", r.killed.join(", ")));
            }
        }
        let gave_back = self.give_back();
        self.set(Input::Stopped);
        report?;
        if !gave_back {
            bail!("could not remove Shard's temporary profile items");
        }
        Ok(())
    }

    /// Notices an emulator that went away on its own (window closed, crash).
    fn watch(&mut self) {
        let p = self.paths.clone();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let exit_code: Option<Option<i32>> = match session.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.code()),
                Ok(None) => None,
                Err(_) => Some(None),
            },
            None => {
                if self.last_poll.elapsed() < ADOPTED_POLL {
                    return;
                }
                self.last_poll = Instant::now();
                // Exit code unknown for an adopted emulator; treat a disappearance as a close.
                (!emulator::qemu_running(&p)).then_some(Some(0))
            }
        };
        let Some(code) = exit_code else {
            return;
        };
        let info = self.session.take().map(|s| s.info);
        let _ = emulator::stop(&p, &self.adb, None);
        self.give_back();
        if code == Some(0) {
            self.em.log("the emulator was closed");
            self.set(Input::Stopped);
        } else {
            let tail = info
                .as_ref()
                .map(|i| emulator::tail(&i.log, 30))
                .unwrap_or_default();
            self.set(Input::Failed(format!(
                "The emulator stopped unexpectedly (exit code {}). Last lines of its log:\n{tail}",
                code.map_or("unknown".into(), |c| c.to_string())
            )));
        }
    }

    fn shutdown(&mut self) {
        let p = self.paths.clone();
        if self.session.is_some() || emulator::qemu_running(&p) {
            self.em
                .log("leaving the emulator running; Shard takes it over on its next start");
            return;
        }
        emulator::stop_adb_server(&p, &self.adb);
        self.give_back();
    }
}

/// The host GPU renderer: from the emulator log if it has been flushed yet, else from the
/// guest's SurfaceFlinger (which reports the same translator string).
pub fn host_renderer(adb: &Adb, info: &RunningInfo) -> Option<String> {
    emulator::renderer_from_log(&emulator::read_log(&info.log)).or_else(|| {
        let out = adb
            .shell(
                &info.serial,
                "dumpsys SurfaceFlinger",
                Duration::from_secs(20),
            )
            .ok()?;
        emulator::renderer_from_gles(&emulator::gles_line(&out)?)
    })
}

/// Ends every process from our emulator folder without letting it save state.
fn kill_emulator(paths: &Paths) {
    for _ in 0..3 {
        let left = emulator::emulator_processes(paths);
        if left.is_empty() {
            return;
        }
        procs::kill(&left.iter().map(|p| p.pid).collect::<Vec<_>>());
        procs::wait_until(Duration::from_secs(5), Duration::from_millis(200), || {
            emulator::emulator_processes(paths).is_empty()
        });
    }
}

fn hw_file(paths: &Paths) -> PathBuf {
    paths.state.join("hardware.txt")
}

/// Hardware hash of the settings the current Quick Boot snapshot was made with.
fn load_hw_hash(paths: &Paths) -> Option<String> {
    std::fs::read_to_string(hw_file(paths))
        .ok()
        .map(|s| s.trim().to_string())
}

fn save_hw_hash(paths: &Paths, hash: &str) -> Result<()> {
    std::fs::write(hw_file(paths), hash).context("writing state\\hardware.txt")
}

pub fn mb(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

/// At most ~10 progress events a second (plus the final one).
struct Throttle(Option<Instant>);

impl Throttle {
    fn new() -> Self {
        Self(None)
    }
    fn ready(&mut self, last: bool) -> bool {
        let due = self
            .0
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
        if due || last {
            self.0 = Some(Instant::now());
        }
        due || last
    }
}
