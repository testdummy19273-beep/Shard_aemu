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

/// Hash of the settings that make an existing Quick Boot snapshot unusable when they change.
pub fn hardware_hash(cfg: &EmulatorConfig) -> String {
    let key = format!(
        "ram={};cores={};lcd={}x{}@{};gpu={}",
        cfg.ram_mb, cfg.cores, cfg.width, cfg.height, cfg.dpi, cfg.gpu
    );
    crate::sdk::download::hex(&sha1::Sha1::digest(key.as_bytes()))
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
    paths
        .avd_dir()
        .join("snapshots")
        .join("default_boot")
        .join("snapshot.pb")
        .is_file()
}
