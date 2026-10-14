//! `Shard.exe --headless setup|boot|debloat|wipe|verify|stop`: the same worker as the window, with
//! its events printed as lines. Exit codes: 0 ok, 1 failed, 2 another Shard owns this folder.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::instance::{self, Acquire};
use crate::jobs::{Command, Engine, Event, Job};
use crate::paths::Paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessCmd {
    Setup,
    Boot,
    Debloat,
    Wipe,
    Verify,
    Stop,
}

pub fn run(paths: &Paths, cmd: HeadlessCmd, dev_cache: Option<PathBuf>) -> i32 {
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("error: {e:#}");
        return 1;
    }
    let lock = match instance::acquire(paths) {
        Ok(Acquire::Acquired(lock)) => lock,
        Ok(Acquire::AlreadyRunning(pid)) => {
            eprintln!(
                "Shard is already running{}",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            );
            return 2;
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            return 1;
        }
    };
    crate::logging::init(&paths.logs);
    tracing::info!(
        "Shard {} --headless {cmd:?} in {}",
        env!("CARGO_PKG_VERSION"),
        paths.root.display()
    );
    let code = match cmd {
        HeadlessCmd::Setup => run_job(paths, Command::Setup { dev_cache }, Job::Setup),
        HeadlessCmd::Boot => run_job(paths, Command::Boot, Job::Boot),
        HeadlessCmd::Stop => run_job(paths, Command::Stop, Job::Stop),
        HeadlessCmd::Debloat => run_job(paths, Command::Debloat, Job::Debloat),
        HeadlessCmd::Wipe => run_job(paths, Command::WipeData, Job::Wipe),
        HeadlessCmd::Verify => crate::verify::run(paths, lock),
    };
    tracing::info!("exit code {code}");
    code
}

fn run_job(paths: &Paths, cmd: Command, job: Job) -> i32 {
    let engine = Engine::start(paths.clone(), Arc::new(|| {}));
    let mut printer = Printer::new("");
    let (result, _) = run_on(&engine, cmd, job, &mut printer);
    // A booted emulator is left running; otherwise this stops adb and tidies the profile.
    engine.shutdown(Duration::from_secs(60));
    match result {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// What a `Running` event reported.
#[derive(Debug, Clone)]
pub struct RunningSeen {
    pub serial: String,
    pub renderer: Option<String>,
    pub boot_secs: Option<f64>,
}

/// Sends `cmd` and prints events until `job` finishes.
pub fn run_on(
    engine: &Engine,
    cmd: Command,
    job: Job,
    printer: &mut Printer,
) -> (Result<(), String>, Option<RunningSeen>) {
    engine.send(cmd);
    let mut running = None;
    loop {
        match engine.events.recv() {
            Ok(Event::Finished { job: j, result }) if j == job => {
                printer.finished(&result);
                return (result, running);
            }
            Ok(ev) => {
                if let Event::Running {
                    serial,
                    renderer,
                    boot_secs,
                } = &ev
                {
                    running = Some(RunningSeen {
                        serial: serial.clone(),
                        renderer: renderer.clone(),
                        boot_secs: *boot_secs,
                    });
                }
                printer.print(&ev);
            }
            Err(_) => return (Err("the worker stopped unexpectedly".into()), running),
        }
    }
}

pub struct Printer {
    indent: &'static str,
    label: String,
    pct: u64,
}

impl Printer {
    pub fn new(indent: &'static str) -> Self {
        Self {
            indent,
            label: String::new(),
            pct: 0,
        }
    }

    pub fn print(&mut self, ev: &Event) {
        let i = self.indent;
        match ev {
            Event::State(s) => println!("{i}state: {}", s.label()),
            Event::Step(s) => println!("{i}==> {s}"),
            Event::Progress { label, done, total } => {
                let pct = if *total == 0 { 100 } else { done * 100 / total };
                if *label != self.label || pct >= self.pct + 5 || (pct == 100 && self.pct != 100) {
                    println!("{i}    {label}: {pct}%");
                    self.label = label.clone();
                    self.pct = pct;
                }
            }
            Event::Log(m) => println!("{i}    {m}"),
            Event::Warning(w) => println!("{i}WARNING: {w}"),
            Event::Running {
                serial,
                renderer,
                boot_secs,
            } => println!(
                "{i}running: {serial}{}{}",
                boot_secs
                    .map(|s| format!(", booted in {s:.1} s"))
                    .unwrap_or_default(),
                renderer
                    .as_ref()
                    .map(|r| format!(", renderer {r}"))
                    .unwrap_or_default()
            ),
            Event::Finished { .. } | Event::ShutDown => {}
        }
        let _ = std::io::stdout().flush();
    }

    pub fn finished(&mut self, result: &Result<(), String>) {
        match result {
            Ok(()) => println!("{}done", self.indent),
            Err(e) => println!("{}FAILED: {e}", self.indent),
        }
        let _ = std::io::stdout().flush();
    }
}
