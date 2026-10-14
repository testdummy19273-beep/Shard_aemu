//! Shard's adb: always `sdk\platform-tools\adb.exe`, with the child env from `paths.rs`
//! (server port 5038, so another SDK's adb on 5037 is never touched).

use std::process::{Command, Stdio};
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
