//! Shard's adb: always `sdk\platform-tools\adb.exe`, with the child env from `paths.rs`
//! (server port 5038, so another SDK's adb on 5037 is never touched).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::paths::Paths;
use crate::procs::{self, Output};

pub const SHORT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Adb {
    paths: Paths,
}

impl Adb {
    pub fn new(paths: &Paths) -> Self {
        Self {
            paths: paths.clone(),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.paths.adb_exe());
        self.paths.apply_child_env(&mut cmd);
        cmd
    }

    pub fn run(&self, args: &[&str], timeout: Duration) -> Result<Output> {
        let mut cmd = self.command();
        cmd.args(args);
        procs::run(&mut cmd, timeout).with_context(|| format!("adb {}", args.join(" ")))
    }

    /// Starts the server with no inherited pipes: the daemon outlives this call, and would
    /// otherwise keep our output pipes open.
    pub fn start_server(&self) -> Result<()> {
        let mut cmd = self.command();
        cmd.arg("start-server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        procs::hidden(&mut cmd);
        let mut child = cmd.spawn().context("starting the adb server")?;
        let (code, timed_out) = procs::wait_timeout(&mut child, Duration::from_secs(30))?;
        if timed_out || code != Some(0) {
            bail!("adb start-server failed (exit {code:?})");
        }
        Ok(())
    }

    pub fn kill_server(&self) -> Result<()> {
        if !self.paths.adb_exe().is_file() {
            return Ok(());
        }
        self.run(&["kill-server"], SHORT)?;
        Ok(())
    }

    /// `adb devices` as (serial, state) pairs.
    pub fn devices(&self) -> Result<Vec<(String, String)>> {
        let out = self.run(&["devices"], SHORT)?;
        Ok(parse_devices(&out.stdout_text()))
    }

    pub fn state_of(&self, serial: &str) -> Option<String> {
        self.devices()
            .ok()?
            .into_iter()
            .find(|(s, _)| s == serial)
            .map(|(_, st)| st)
    }

    pub fn shell(&self, serial: &str, command: &str, timeout: Duration) -> Result<String> {
        let out = self.run(&["-s", serial, "shell", command], timeout)?;
        if !out.success() {
            bail!("adb shell {command} failed: {}", out.combined());
        }
        Ok(out.stdout_text())
    }

    pub fn getprop(&self, serial: &str, prop: &str) -> Option<String> {
        let out = self
            .run(&["-s", serial, "shell", "getprop", prop], SHORT)
            .ok()?;
        out.success().then(|| out.stdout_text().trim().to_string())
    }

    pub fn emu_kill(&self, serial: &str) -> Result<Output> {
        self.run(&["-s", serial, "emu", "kill"], SHORT)
    }

    /// PNG bytes of the current screen.
    pub fn screencap(&self, serial: &str) -> Result<Vec<u8>> {
        let out = self.run(
            &["-s", serial, "exec-out", "screencap", "-p"],
            Duration::from_secs(30),
        )?;
        if !out.success() || out.stdout.is_empty() {
            bail!("screencap failed: {}", out.stderr_text().trim());
        }
        Ok(out.stdout)
    }
}

/// One long-lived `adb -s <serial> shell`. Without a terminal it runs each line written to its
/// stdin as a command; measured here at 10-25 ms per `input keyevent` (a new `adb shell` per
/// command costs about 35 ms plus process start-up).
pub struct ShellSession {
    child: Child,
    stdin: ChildStdin,
}

impl Adb {
    pub fn open_shell(&self, serial: &str) -> Result<ShellSession> {
        let mut cmd = self.command();
        cmd.args(["-s", serial, "shell"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        procs::hidden(&mut cmd);
        let mut child = cmd.spawn().context("starting adb shell")?;
        let stdin = child.stdin.take().context("adb shell has no stdin")?;
        // Drained on their own threads so a full pipe never blocks the shell (or our writes).
        let out: [Option<Box<dyn Read + Send>>; 2] = [
            child
                .stdout
                .take()
                .map(|o| Box::new(o) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|e| Box::new(e) as Box<dyn Read + Send>),
        ];
        for stream in out.into_iter().flatten() {
            std::thread::Builder::new()
                .name("adb-shell-output".into())
                .spawn(move || {
                    for line in BufReader::new(stream).lines() {
                        match line {
                            Ok(l) if !l.trim().is_empty() => tracing::debug!("adb shell: {l}"),
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                })
                .context("starting the adb shell reader")?;
        }
        Ok(ShellSession { child, stdin })
    }
}

impl ShellSession {
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Writes one command line. Only Shard's own fixed commands go through here.
    pub fn send(&mut self, line: &str) -> Result<()> {
        if !self.alive() {
            bail!("adb shell has exited");
        }
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|()| self.stdin.flush())
            .context("writing to adb shell")
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"exit\n");
        let _ = self.stdin.flush();
        if procs::wait_timeout(&mut self.child, Duration::from_secs(1))
            .map_or(true, |(_, timed_out)| timed_out)
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn parse_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}
