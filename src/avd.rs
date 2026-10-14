//! The "shard" AVD: `avd\shard.ini` and `avd\shard.avd\config.ini`, written directly.
//!
//! Keys were checked against emulator 37.2.12: it boots this config without rewriting it, and
//! `hardware-qemu.ini` reflects every value. `path.rel` is resolved against the folder above
//! `ANDROID_AVD_HOME` (the Shard root), so the AVD survives a folder move even before `path=` is
//! rewritten; `path=` is still rewritten on every launch.

use std::path::Path;

use anyhow::{Context, Result};
use sha1::Digest;

use crate::config::EmulatorConfig;
use crate::paths::Paths;

pub const AVD_NAME: &str = "shard";
pub const DATA_PARTITION: &str = "8G";

pub fn shard_ini(paths: &Paths) -> String {
    format!(
        "avd.ini.encoding=UTF-8\r\npath={}\r\npath.rel=avd\\shard.avd\r\ntarget=android-33\r\n",
        paths.avd_dir().display()
    )
}

pub fn config_ini(cfg: &EmulatorConfig) -> String {
    let lines = [
        "AvdId=shard".to_string(),
        "avd.ini.displayname=Shard".into(),
        "avd.ini.encoding=UTF-8".into(),
        "PlayStore.enabled=true".into(),
        "abi.type=x86_64".into(),
        format!("disk.dataPartition.size={DATA_PARTITION}"),
        "fastboot.forceColdBoot=no".into(),
        "fastboot.forceFastBoot=yes".into(),
        "hw.accelerometer=yes".into(),
        // No virtual microphone: games don't need it.
        "hw.audioInput=no".into(),
        "hw.battery=yes".into(),
        "hw.camera.back=none".into(),
        "hw.camera.front=none".into(),
        "hw.cpu.arch=x86_64".into(),
        format!("hw.cpu.ncore={}", cfg.cores),
        "hw.dPad=no".into(),
        "hw.gps=yes".into(),
        "hw.gpu.enabled=yes".into(),
        format!("hw.gpu.mode={}", cfg.gpu),
        "hw.initialOrientation=landscape".into(),
        "hw.keyboard=yes".into(),
        format!("hw.lcd.density={}", cfg.dpi),
        format!("hw.lcd.height={}", cfg.height),
        format!("hw.lcd.width={}", cfg.width),
        "hw.mainKeys=no".into(),
        format!("hw.ramSize={}", cfg.ram_mb),
        "hw.sdCard=no".into(),
        "hw.sensors.orientation=yes".into(),
        "hw.sensors.proximity=yes".into(),
        "hw.trackBall=no".into(),
        "image.sysdir.1=system-images\\android-33\\google_apis_playstore\\x86_64\\".into(),
        "showDeviceFrame=no".into(),
        "skin.dynamic=yes".into(),
        format!("skin.name={}x{}", cfg.width, cfg.height),
        "skin.path=_no_skin".into(),
        "tag.display=Google Play".into(),
        "tag.id=google_apis_playstore".into(),
    ];
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// The settings that make an existing Quick Boot snapshot unusable when they change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hardware {
    pub ram_mb: u32,
    pub cores: u32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub gpu: String,
}

impl Hardware {
    pub fn of(cfg: &EmulatorConfig) -> Self {
        Self {
            ram_mb: cfg.ram_mb,
            cores: cfg.cores,
            width: cfg.width,
            height: cfg.height,
            dpi: cfg.dpi,
            gpu: cfg.gpu.clone(),
        }
    }

    /// `cfg` with these hardware values (everything else, like the port, unchanged).
    pub fn apply(&self, cfg: &EmulatorConfig) -> EmulatorConfig {
        EmulatorConfig {
            ram_mb: self.ram_mb,
            cores: self.cores,
            width: self.width,
            height: self.height,
            dpi: self.dpi,
            gpu: self.gpu.clone(),
            ..cfg.clone()
        }
    }

    pub fn hash(&self) -> String {
        let key = format!(
            "ram={};cores={};lcd={}x{}@{};gpu={}",
            self.ram_mb, self.cores, self.width, self.height, self.dpi, self.gpu
        );
        crate::sdk::download::hex(&sha1::Sha1::digest(key.as_bytes()))
    }
}

pub fn hardware_hash(cfg: &EmulatorConfig) -> String {
    Hardware::of(cfg).hash()
}

/// The hardware a Quick Boot snapshot was made with, from the `hardware.ini` the emulator
/// saves next to it ("hw.ramSize = 4096"). A GPU mode Shard does not offer is replaced by
/// `fallback_gpu`.
pub fn parse_snapshot_hardware(ini: &str, fallback_gpu: &str) -> Option<Hardware> {
    let get = |key: &str| {
        ini.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let num = |key: &str| get(key)?.parse::<u32>().ok();
    let gpu = get("hw.gpu.mode")
        .filter(|g| crate::config::GPU_MODES.contains(&g.as_str()))
        .unwrap_or_else(|| fallback_gpu.to_string());
    Some(Hardware {
        ram_mb: num("hw.ramSize")?,
        cores: num("hw.cpu.ncore")?,
        width: num("hw.lcd.width")?,
        height: num("hw.lcd.height")?,
        dpi: num("hw.lcd.density")?,
        gpu,
    })
}

pub fn snapshot_hardware(paths: &Paths, fallback_gpu: &str) -> Option<Hardware> {
    let ini = std::fs::read_to_string(snapshot_dir(paths).join("hardware.ini")).ok()?;
    parse_snapshot_hardware(&ini, fallback_gpu)
}

/// How the next start uses the Quick Boot snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPlan {
    /// The snapshot is about to be discarded (hardware settings changed, or Quick Boot turned
    /// off) and may hold changes Android has not saved yet: boot it with these settings first,
    /// shut Android down cleanly, and only then start as `mode`.
    pub rescue: Option<Hardware>,
    pub mode: crate::emulator::BootMode,
}

/// `saved_hash` is the hardware hash of the last successful start (`state\hardware.txt`);
/// `snapshot` the hardware of the existing snapshot, if its `hardware.ini` could be read.
pub fn plan_start(
    wipe_pending: bool,
    cfg: &EmulatorConfig,
    saved_hash: Option<&str>,
    has_snapshot: bool,
    snapshot: Option<Hardware>,
) -> StartPlan {
    use crate::emulator::BootMode;
    if wipe_pending {
        // The data is reset anyway; nothing in the snapshot is worth keeping.
        return StartPlan {
            rescue: None,
            mode: BootMode::Wipe,
        };
    }
    let mode = if !cfg.quick_boot {
        BootMode::NoSnapshot
    } else if has_snapshot && saved_hash == Some(hardware_hash(cfg).as_str()) {
        BootMode::Quick
    } else {
        BootMode::Cold
    };
    StartPlan {
        rescue: snapshot.filter(|_| has_snapshot && mode != BootMode::Quick),
        mode,
    }
}

/// Whether Stop should shut Android down cleanly instead of saving a snapshot: the next start
/// would discard that snapshot (Quick Boot off, or hardware settings changed since the running
/// emulator started with `running_hash`).
pub fn stop_needs_shutdown(cfg: &EmulatorConfig, running_hash: Option<&str>) -> bool {
    !cfg.quick_boot || running_hash != Some(hardware_hash(cfg).as_str())
}

fn snapshot_dir(paths: &Paths) -> std::path::PathBuf {
    paths.avd_dir().join("snapshots").join("default_boot")
}

/// Deletes the Quick Boot snapshot (it must never be loaded again).
pub fn remove_snapshot(paths: &Paths) -> Result<()> {
    let dir = snapshot_dir(paths);
    if dir.exists() {
        crate::sdk::install::remove_dir_all_retry(&dir)
            .with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// Writes shard.ini (always) and config.ini (only when its content changes). Returns whether
/// config.ini changed.
pub fn write(paths: &Paths, cfg: &EmulatorConfig) -> Result<bool> {
    std::fs::create_dir_all(paths.avd_dir())
        .with_context(|| format!("creating {}", paths.avd_dir().display()))?;
    std::fs::write(paths.avd_ini(), shard_ini(paths)).context("writing avd\\shard.ini")?;
    write_if_changed(&paths.avd_config(), &config_ini(cfg))
}

fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// A Quick Boot snapshot exists for the AVD.
pub fn has_snapshot(paths: &Paths) -> bool {
    snapshot_dir(paths).join("snapshot.pb").is_file()
}
