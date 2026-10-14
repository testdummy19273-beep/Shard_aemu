//! The side bar's device controls. Its own thread owns one persistent `adb shell` (never a new
//! process per tap), so a click is a line written to that shell; a shell that died is reopened
//! on the next click.

use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};

use crate::adb::{Adb, ShellSession};
use crate::paths::Paths;

/// Android key events the side bar sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Home,
    Back,
    Recents,
    VolumeUp,
    VolumeDown,
}

impl Key {
    /// `KeyEvent.KEYCODE_*`.
    pub fn code(self) -> u32 {
        match self {
            Key::Home => 3,
            Key::Back => 4,
            Key::VolumeUp => 24,
            Key::VolumeDown => 25,
            Key::Recents => 187,
        }
    }

    pub fn command(self) -> String {
        format!("input keyevent {}", self.code())
    }
}

#[derive(Debug, Clone)]
pub enum ControlCmd {
    /// The emulator with this serial is running: open the shell now, so the first click is quick.
    Attach(String),
    /// Not running any more: close the shell.
    Detach,
    Key(Key),
    /// Turns the virtual device by 90° (emulator console `rotate`, like the emulator's own
    /// toolbar button: the window turns, and Android follows when the app in front allows it).
    Rotate,
}

pub struct Control {
    tx: Option<Sender<ControlCmd>>,
    /// Problems worth showing in the log ("could not send Home: ...").
    pub warnings: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl Control {
    pub fn start(paths: &Paths) -> Control {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (wtx, warnings) = crossbeam_channel::unbounded();
        let worker = Worker {
            adb: Adb::new(paths),
            paths: paths.clone(),
            serial: None,
            shell: None,
            warn: wtx,
        };
        let thread = std::thread::Builder::new()
            .name("shard-control".into())
            .spawn(move || worker.run(rx))
            .expect("spawning the control thread");
        Control {
            tx: Some(tx),
            warnings,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: ControlCmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // Closing the channel ends the thread, which closes the shell.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    adb: Adb,
    paths: Paths,
    serial: Option<String>,
    shell: Option<ShellSession>,
    warn: Sender<String>,
}

impl Worker {
    fn run(mut self, rx: Receiver<ControlCmd>) {
        for cmd in rx {
            match cmd {
                ControlCmd::Attach(serial) => {
                    if self.serial.as_deref() != Some(serial.as_str()) {
                        self.shell = None;
                        self.serial = Some(serial);
                    }
                    if self.shell.is_none() {
                        let _ = self.open();
                    }
                }
                ControlCmd::Detach => {
                    self.shell = None;
                    self.serial = None;
                }
                ControlCmd::Key(key) => self.send(&key.command(), &format!("{key:?}")),
                ControlCmd::Rotate => self.rotate(),
            }
        }
    }

    /// Opens the shell only while Shard's adb server runs: an adb client started without one
    /// would start a server of its own outside the worker's control.
    fn open(&mut self) -> bool {
        let Some(serial) = self.serial.clone() else {
            return false;
        };
        if crate::procs::under(&self.paths.platform_tools_dir()).is_empty() {
            return false;
        }
        match self.adb.open_shell(&serial) {
            Ok(s) => {
                self.shell = Some(s);
                true
            }
            Err(e) => {
                let _ = self.warn.send(format!("{e:#}"));
                false
            }
        }
    }

    fn send(&mut self, line: &str, what: &str) {
        if self.serial.is_none() {
            return;
        }
        for _ in 0..2 {
            let alive = self.shell.as_mut().is_some_and(|s| s.alive());
            if !alive && !self.open() {
                break;
            }
            if let Some(shell) = self.shell.as_mut() {
                match shell.send(line) {
                    Ok(()) => return,
                    // Reopen once and try again.
                    Err(_) => self.shell = None,
                }
            }
        }
        let _ = self.warn.send(format!(
            "could not send {what} to the emulator (adb shell is not available)"
        ));
    }

    fn rotate(&mut self) {
        let Some(serial) = self.serial.clone() else {
            return;
        };
        let ok = self
            .adb
            .run(&["-s", &serial, "emu", "rotate"], crate::adb::SHORT)
            .map(|o| o.success() && o.stdout_text().contains("OK"));
        match ok {
            Ok(true) => {}
            Ok(false) => {
                let _ = self.warn.send("the emulator did not accept rotate".into());
            }
            Err(e) => {
                let _ = self.warn.send(format!("rotate: {e:#}"));
            }
        }
    }
}
