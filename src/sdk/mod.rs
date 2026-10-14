//! SDK packages: which ones, where they go, and what is installed.

pub mod download;
pub mod install;
pub mod manifest;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use manifest::{PackageId, Selected};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub packages: BTreeMap<String, InstalledPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub path: String,
    pub revision: String,
    pub archive: String,
    pub checksum: String,
}

impl InstalledPackage {
    pub fn from_selected(sel: &Selected) -> Self {
        Self {
            path: sel.path.clone(),
            revision: sel.revision.to_string(),
            archive: sel.file_name(),
            checksum: sel.archive.checksum.hex.clone(),
        }
    }
}

fn state_file(paths: &Paths) -> PathBuf {
    paths.state.join("sdk.toml")
}

impl Installed {
    pub fn load(paths: &Paths) -> Self {
        std::fs::read_to_string(state_file(paths))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string(self).context("serialising sdk state")?;
        std::fs::write(state_file(paths), text).context("writing state\\sdk.toml")
    }
    /// Installed, at the same archive, and its key file is still on disk.
    pub fn has(&self, paths: &Paths, sel: &Selected) -> bool {
        self.packages.get(sel.id.key()) == Some(&InstalledPackage::from_selected(sel))
            && key_file(paths, sel.id).is_file()
    }
    /// Every package recorded and present (used when offline: no manifest to compare with).
    pub fn complete(&self, paths: &Paths) -> bool {
        PackageId::ALL
            .iter()
            .all(|id| self.packages.contains_key(id.key()) && key_file(paths, *id).is_file())
    }
}

pub fn target_dir(paths: &Paths, id: PackageId) -> PathBuf {
    match id {
        PackageId::Emulator => paths.emulator_dir(),
        PackageId::PlatformTools => paths.platform_tools_dir(),
        PackageId::SystemImage => paths.sysimg_dir(),
    }
}

/// One file whose presence shows the package is extracted.
pub fn key_file(paths: &Paths, id: PackageId) -> PathBuf {
    match id {
        PackageId::Emulator => paths.emulator_exe(),
        PackageId::PlatformTools => paths.adb_exe(),
        PackageId::SystemImage => paths.sysimg_dir().join("system.img"),
    }
}

pub fn staging_dir(paths: &Paths, id: PackageId) -> PathBuf {
    paths.sdk.join(".staging").join(id.key())
}
