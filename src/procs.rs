//! Child processes: run-with-timeout, and finding/killing processes that live under our folder.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::paths::path_starts_with;

/// No console window for console-subsystem children (adb, emulator.exe).
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).replace('\r', "")
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).replace('\r', "")
    }
    /// stdout + stderr, trimmed, for error messages.
    pub fn combined(&self) -> String {
        let mut s = self.stdout_text();
        let e = self.stderr_text();
        if !e.trim().is_empty() {
            if !s.trim().is_empty() {
                s.push('\n');
            }
            s.push_str(&e);
        }
        s.trim().to_string()
    }
}

pub fn hidden(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

/// Runs `cmd` to completion or until `timeout`, capturing stdout/stderr.
///
/// Reader threads are not joined unconditionally: a child that starts a daemon (adb does) can
/// leave the daemon holding the pipes open, so output is collected with a short grace period.
pub fn run(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    hidden(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {:?}", cmd.get_program()))?;
    let out_rx = reader(child.stdout.take());
    let err_rx = reader(child.stderr.take());
    let (code, timed_out) = wait_timeout(&mut child, timeout)?;
    let grace = Duration::from_secs(2);
    Ok(Output {
        code,
        stdout: out_rx.recv_timeout(grace).unwrap_or_default(),
        stderr: err_rx.recv_timeout(grace).unwrap_or_default(),
        timed_out,
    })
}

fn reader<R: Read + Send + 'static>(src: Option<R>) -> crossbeam_channel::Receiver<Vec<u8>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    if let Some(mut src) = src {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = src.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

/// Waits for `child`; kills it after `timeout`. Returns (exit code, timed out).
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<(Option<i32>, bool)> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), false));
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((None, true));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub cmd: Vec<String>,
}

impl ProcInfo {
    pub fn name_is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// All processes (with exe path and command line where Windows lets us read them).
pub fn list() -> Vec<ProcInfo> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    sys.processes()
        .iter()
        .map(|(pid, p)| ProcInfo {
            pid: pid.as_u32(),
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(Path::to_path_buf),
            cmd: p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        })
        .collect()
}

/// Processes whose image lives under `dir`.
pub fn under(dir: &Path) -> Vec<ProcInfo> {
    list()
        .into_iter()
        .filter(|p| p.exe.as_deref().is_some_and(|e| path_starts_with(e, dir)))
        .collect()
}

pub fn is_alive(pid: u32) -> bool {
    let mut sys = System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(pid).is_some()
}

pub fn kill(pids: &[u32]) {
    let mut sys = System::new();
    let ids: Vec<sysinfo::Pid> = pids.iter().map(|p| sysinfo::Pid::from_u32(*p)).collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&ids),
        true,
        ProcessRefreshKind::nothing(),
    );
    for id in ids {
        if let Some(p) = sys.process(id) {
            p.kill();
        }
    }
}

/// Polls until `pred` is true or `timeout` passes. Returns whether `pred` became true.
pub fn wait_until(timeout: Duration, step: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(step);
    }
}
