//! `shard.toml`: user settings, created with defaults when missing.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// GPU modes the emulator accepts for `-gpu` (37.x). `host` is the default and the fast one.
pub const GPU_MODES: &[&str] = &["host", "auto", "swangle", "swiftshader", "lavapipe"];

/// Resolution presets offered in Settings: (width, height, dpi).
pub const RESOLUTION_PRESETS: &[(u32, u32, u32)] = &[(1280, 720, 240), (1920, 1080, 320)];

/// Even console ports adb discovers on its own (5554..=5584; adb uses port + 1).
pub const PORT_RANGE: std::ops::RangeInclusive<u16> = 5554..=5584;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub emulator: EmulatorConfig,
    pub performance: PerformanceConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmulatorConfig {
    pub ram_mb: u32,
    pub cores: u32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub gpu: String,
    pub quick_boot: bool,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    pub high_priority: bool,
    pub power_plan_switch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub dock_sidebar: bool,
}

impl Default for EmulatorConfig {
    fn default() -> Self {
        Self {
            ram_mb: 4096,
            cores: 4,
            width: 1920,
            height: 1080,
            dpi: 320,
            gpu: "host".into(),
            quick_boot: true,
            port: 5554,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { dock_sidebar: true }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("shard.toml is not valid TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("config always serialises")
    }

    /// Loads `shard.toml`, writing the defaults first if it does not exist.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = Config::default();
            cfg.save(path)?;
            return Ok(cfg);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        std::fs::write(path, self.to_toml()).with_context(|| format!("writing {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        let e = &self.emulator;
        let mut problems = Vec::new();
        if !(2048..=8192).contains(&e.ram_mb) {
            problems.push(format!(
                "emulator.ram_mb = {} (allowed 2048-8192)",
                e.ram_mb
            ));
        }
        if !(1..=8).contains(&e.cores) {
            problems.push(format!("emulator.cores = {} (allowed 1-8)", e.cores));
        }
        if !(640..=3840).contains(&e.width) || !(360..=2160).contains(&e.height) {
            problems.push(format!(
                "emulator resolution {}x{} (allowed 640x360 to 3840x2160)",
                e.width, e.height
            ));
        }
        if e.width <= e.height {
            problems.push(format!(
                "emulator resolution {}x{} must be landscape (width > height)",
                e.width, e.height
            ));
        }
        if !(120..=640).contains(&e.dpi) {
            problems.push(format!("emulator.dpi = {} (allowed 120-640)", e.dpi));
        }
        if !GPU_MODES.contains(&e.gpu.as_str()) {
            problems.push(format!(
                "emulator.gpu = \"{}\" (one of {})",
                e.gpu,
                GPU_MODES.join(", ")
            ));
        }
        if !PORT_RANGE.contains(&e.port) || !e.port.is_multiple_of(2) {
            problems.push(format!(
                "emulator.port = {} (an even port from {} to {})",
                e.port,
                PORT_RANGE.start(),
                PORT_RANGE.end()
            ));
        }
        if !problems.is_empty() {
            bail!("invalid settings: {}", problems.join("; "));
        }
        Ok(())
    }
}
