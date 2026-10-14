//! Stash-and-restore for the few things adb and the emulator insist on keeping outside Shard's
//! folder. No variable or flag redirects them (tested with platform-tools 37.0.1 and emulator
//! 37.2.12; both resolve the profile through the shell API, and Qt keeps the emulator's UI
//! settings in the registry):
//!
//! - `%USERPROFILE%\.android\adbkey` and `adbkey.pub` (adb server's user key)
//! - `%USERPROFILE%\.emulator_console_auth_token` (emulator console auth)
//! - `HKCU\Software\Android Open Source Project\Emulator` (emulator UI settings)
//!
//! Shard keeps its own copies in `home\profile\` and `state\emulator-registry.toml`. Before a
//! launch, anything that does not already exist is put in place from Shard's copy and recorded
//! in `state\placed.toml`. After the emulator and adb server stop, changes are copied back and
//! only what Shard placed is removed. Pre-existing items (another SDK's) are used, never touched.
//! A crash leaves `placed.toml` behind and the next start finishes the job.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::winutil::{self, RegKeyDump};

/// Profile-relative files, in placement order.
pub const FILES: &[&str] = &[
    ".android\\adbkey",
    ".android\\adbkey.pub",
    ".emulator_console_auth_token",
];
pub const DIRS: &[&str] = &[".android"];
pub const REG_PARENT: &str = "Software\\Android Open Source Project";
pub const REG_KEY: &str = "Software\\Android Open Source Project\\Emulator";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placed {
    /// Profile folder at placement time (absolute).
    pub profile: PathBuf,
    /// Profile-relative files Shard owns for this run.
    pub files: Vec<String>,
    /// Profile-relative folders Shard created.
    pub dirs: Vec<String>,
    /// HKCU subkey Shard created (the topmost one that did not exist).
    pub reg_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegStash {
    root: String,
    keys: Vec<RegKeyDump>,
}

pub struct Profile {
    pub profile: PathBuf,
    stash: PathBuf,
    placed_file: PathBuf,
    reg_stash: PathBuf,
}

impl Profile {
    pub fn new(paths: &Paths) -> Self {
        Self::with_profile(paths, winutil::profile_dir())
    }

    pub fn with_profile(paths: &Paths, profile: PathBuf) -> Self {
        Self {
            profile,
            stash: paths.profile_stash(),
            placed_file: paths.state.join("placed.toml"),
            reg_stash: paths.state.join("emulator-registry.toml"),
        }
    }

    pub fn pending(&self) -> Option<Placed> {
        let text = std::fs::read_to_string(&self.placed_file).ok()?;
        toml::from_str(&text).ok()
    }

    /// Puts Shard's copies in place for a run. Call before starting adb or the emulator.
    pub fn borrow(&self, log: &mut dyn FnMut(String)) -> Result<()> {
        if self.pending().is_some() {
            // Left over from a run that did not clean up (crash): finish that first.
            self.give_back(log)?;
        }
        let mut placed = Placed {
            profile: self.profile.clone(),
            ..Placed::default()
        };
        for d in DIRS {
            if !self.profile.join(d).exists() {
                placed.dirs.push(d.to_string());
            }
        }
        for f in FILES {
            if !self.profile.join(f).exists() {
                placed.files.push(f.to_string());
            }
        }
        placed.reg_key = if !winutil::hkcu_key_exists(REG_PARENT) {
            Some(REG_PARENT.to_string())
        } else if !winutil::hkcu_key_exists(REG_KEY) {
            Some(REG_KEY.to_string())
        } else {
            None
        };
        // Record ownership before touching anything, so a crash mid-way still cleans up.
        self.write_placed(&placed)?;

        let mut restored = Vec::new();
        for f in &placed.files {
            let src = self.stash.join(f);
            if src.is_file() {
                let dst = self.profile.join(f);
                if let Some(dir) = dst.parent() {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                }
                std::fs::copy(&src, &dst).with_context(|| format!("placing {}", dst.display()))?;
                restored.push(f.to_string());
            }
        }
        if let Some(key) = &placed.reg_key {
            if let Some(stash) = self.load_reg_stash() {
                if stash.root == *key {
                    winutil::hkcu_restore(key, &stash.keys)?;
                    restored.push(format!("HKCU\\{key}"));
                }
            }
        }
        if !placed.files.is_empty() || placed.reg_key.is_some() {
            log(format!(
                "profile items Shard owns for this run and removes afterwards: {}",
                describe(&placed)
            ));
        }
        if !restored.is_empty() {
            log(format!(
                "restored Shard's saved copies: {}",
                restored.join(", ")
            ));
        }
        Ok(())
    }

    /// Copies changes back into Shard's folder and removes what Shard placed. Call only after
    /// the emulator and adb server have exited.
    pub fn give_back(&self, log: &mut dyn FnMut(String)) -> Result<()> {
        let Some(placed) = self.pending() else {
            return Ok(());
        };
        let profile = &placed.profile;
        let mut problems = Vec::new();
        for f in &placed.files {
            let target = profile.join(f);
            if !target.exists() {
                continue;
            }
            let dst = self.stash.join(f);
            if let Some(dir) = dst.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&target, &dst)
                .with_context(|| format!("saving {} into Shard's folder", target.display()))?;
            if let Err(e) =
                crate::sdk::install::retry_locked(&format!("removing {}", target.display()), || {
                    std::fs::remove_file(&target)
                })
            {
                problems.push(format!("{e:#}"));
            }
        }
        for d in placed.dirs.iter().rev() {
            let dir = profile.join(d);
            if dir.is_dir() {
                let leftovers = list_dir(&dir);
                if leftovers.is_empty() {
                    let _ = std::fs::remove_dir(&dir);
                } else {
                    problems.push(format!(
                        "{} is not empty after the run ({}); left in place",
                        dir.display(),
                        leftovers.join(", ")
                    ));
                }
            }
        }
        if let Some(key) = &placed.reg_key {
            if let Some(dump) = winutil::hkcu_dump(key)? {
                self.save_reg_stash(&RegStash {
                    root: key.clone(),
                    keys: dump,
                })?;
                if let Err(e) = winutil::hkcu_delete_tree(key) {
                    problems.push(format!("{e:#}"));
                }
            }
        }
        if !problems.is_empty() {
            anyhow::bail!(
                "could not remove everything Shard placed: {}",
                problems.join("; ")
            );
        }
        std::fs::remove_file(&self.placed_file).ok();
        log(format!(
            "removed Shard's temporary profile items again ({})",
            describe(&placed)
        ));
        Ok(())
    }

    /// Items Shard placed (per `placed.toml`) that still exist. Empty when nothing is pending.
    pub fn leftovers(&self) -> Vec<String> {
        let Some(placed) = self.pending() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for f in &placed.files {
            let p = placed.profile.join(f);
            if p.exists() {
                out.push(p.display().to_string());
            }
        }
        for d in &placed.dirs {
            let p = placed.profile.join(d);
            if p.exists() {
                out.push(p.display().to_string());
            }
        }
        if let Some(k) = &placed.reg_key {
            if winutil::hkcu_key_exists(k) {
                out.push(format!("HKCU\\{k}"));
            }
        }
        out
    }

    fn write_placed(&self, placed: &Placed) -> Result<()> {
        if let Some(dir) = self.placed_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.placed_file, toml::to_string(placed)?)
            .context("writing state\\placed.toml")
    }

    fn load_reg_stash(&self) -> Option<RegStash> {
        toml::from_str(&std::fs::read_to_string(&self.reg_stash).ok()?).ok()
    }

    fn save_reg_stash(&self, stash: &RegStash) -> Result<()> {
        std::fs::write(&self.reg_stash, toml::to_string(stash)?)
            .context("writing state\\emulator-registry.toml")
    }
}

fn describe(p: &Placed) -> String {
    let mut parts: Vec<String> = p.files.clone();
    if let Some(k) = &p.reg_key {
        parts.push(format!("HKCU\\{k}"));
    }
    if parts.is_empty() {
        "nothing".into()
    } else {
        parts.join(", ")
    }
}

fn list_dir(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}
