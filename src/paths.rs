//! Every folder Shard uses, resolved from one root (the folder that holds Shard.exe).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// adb server port used by Shard's own adb (5037 is the SDK default, which another SDK may own).
pub const ADB_SERVER_PORT: u16 = 5038;

/// Variables that could point a child at another SDK, AVD set or settings folder.
const INHERITED_VARS_TO_DROP: &[&str] = &[
    "ANDROID_SDK_HOME",
    "ANDROID_PREFS_ROOT",
    "ANDROID_EMULATOR_DISCOVERY_DIR",
    "ANDROID_EMULATOR_FEATURES",
    "ANDROID_SERIAL",
    "ADB_TRACE",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub root: PathBuf,
    pub sdk: PathBuf,
    pub avd: PathBuf,
    pub home: PathBuf,
    pub tmp: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            sdk: root.join("sdk"),
            avd: root.join("avd"),
            home: root.join("home"),
            tmp: root.join("tmp"),
            cache: root.join("cache"),
            state: root.join("state"),
            logs: root.join("logs"),
            root,
        }
    }

    /// The folder holding the running exe, or `root_override` (dev/test only).
    pub fn resolve(root_override: Option<&Path>) -> Result<Self> {
        let root = match root_override {
            Some(dir) => std::path::absolute(dir)
                .with_context(|| format!("resolving --root {}", dir.display()))?,
            None => {
                let exe = std::env::current_exe().context("locating Shard.exe")?;
                exe.parent()
                    .context("Shard.exe has no parent folder")?
                    .to_path_buf()
            }
        };
        Ok(Self::from_root(root))
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.sdk,
            &self.avd,
            &self.home,
            &self.tmp,
            &self.cache,
            &self.state,
            &self.logs,
        ] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("shard.toml")
    }
    pub fn debloat_file(&self) -> PathBuf {
        self.root.join("debloat.txt")
    }
    pub fn exe(&self) -> PathBuf {
        self.root.join("Shard.exe")
    }
    pub fn emulator_dir(&self) -> PathBuf {
        self.sdk.join("emulator")
    }
    pub fn emulator_exe(&self) -> PathBuf {
        self.emulator_dir().join("emulator.exe")
    }
    pub fn qemu_exe(&self) -> PathBuf {
        self.emulator_dir()
            .join("qemu")
            .join("windows-x86_64")
            .join("qemu-system-x86_64.exe")
    }
    pub fn platform_tools_dir(&self) -> PathBuf {
        self.sdk.join("platform-tools")
    }
    pub fn adb_exe(&self) -> PathBuf {
        self.platform_tools_dir().join("adb.exe")
    }
    pub fn sysimg_dir(&self) -> PathBuf {
        self.sdk
            .join("system-images")
            .join("android-33")
            .join("google_apis_playstore")
            .join("x86_64")
    }
    pub fn avd_ini(&self) -> PathBuf {
        self.avd.join("shard.ini")
    }
    pub fn avd_dir(&self) -> PathBuf {
        self.avd.join("shard.avd")
    }
    pub fn avd_config(&self) -> PathBuf {
        self.avd_dir().join("config.ini")
    }
    /// The emulator's own adb key (it generates it and injects its public half into the guest).
    pub fn emulator_adbkey(&self) -> PathBuf {
        self.home.join("adbkey")
    }
    /// Shard's stash of the files the emulator/adb insist on keeping in the user profile.
    pub fn profile_stash(&self) -> PathBuf {
        self.home.join("profile")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.state.join("shard.lock")
    }

    /// The environment every emulator/adb child gets (on that child only, never globally).
    pub fn child_env(&self) -> Vec<(OsString, OsString)> {
        let sdk = self.sdk.clone().into_os_string();
        let home = self.home.clone().into_os_string();
        let tmp = self.tmp.clone().into_os_string();
        vec![
            ("ANDROID_SDK_ROOT".into(), sdk.clone()),
            ("ANDROID_HOME".into(), sdk),
            ("ANDROID_AVD_HOME".into(), self.avd.clone().into_os_string()),
            ("ANDROID_USER_HOME".into(), home.clone()),
            ("ANDROID_EMULATOR_HOME".into(), home),
            ("TEMP".into(), tmp.clone()),
            ("TMP".into(), tmp),
            (
                "ANDROID_ADB_SERVER_PORT".into(),
                ADB_SERVER_PORT.to_string().into(),
            ),
            // Lets Shard's adb server authenticate with the key the emulator injected into the
            // guest, whatever key (if any) the user profile holds.
            (
                "ADB_VENDOR_KEYS".into(),
                self.emulator_adbkey().into_os_string(),
            ),
            // No crash handler process and no crash database writes.
            ("ANDROID_EMU_DISABLE_CRASHHANDLER".into(), "1".into()),
            // No mDNS discovery. Otherwise the adb server binds UDP 5353 on every interface,
            // which is network traffic beyond the SDK downloads and makes Windows Firewall ask
            // about adb.exe (and store a rule for its path when nobody answers).
            ("ADB_MDNS".into(), "0".into()),
        ]
    }

    pub fn apply_child_env(&self, cmd: &mut std::process::Command) {
        for var in INHERITED_VARS_TO_DROP {
            cmd.env_remove(var);
        }
        cmd.envs(self.child_env());
    }

    /// Whether `path` lies inside this root (case-insensitive, as on NTFS).
    pub fn contains(&self, path: &Path) -> bool {
        path_starts_with(path, &self.root)
    }
}

/// Case-insensitive, separator-normalised prefix test for Windows paths.
pub fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    let norm = |p: &Path| {
        let mut s = p.to_string_lossy().replace('/', "\\").to_lowercase();
        if let Some(rest) = s.strip_prefix("\\\\?\\") {
            s = rest.to_string();
        }
        s.trim_end_matches('\\').to_string()
    };
    let (p, pre) = (norm(path), norm(prefix));
    p == pre || p.starts_with(&format!("{pre}\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_case_insensitive_and_component_aware() {
        let root = Path::new(r"D:\Games\Shard");
        assert!(path_starts_with(
            Path::new(r"d:\games\shard\sdk\adb.exe"),
            root
        ));
        assert!(path_starts_with(Path::new(r"D:\Games\Shard"), root));
        assert!(path_starts_with(Path::new(r"\\?\D:\Games\Shard\x"), root));
        assert!(!path_starts_with(Path::new(r"D:\Games\Shard2\sdk"), root));
    }
}
