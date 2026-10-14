//! The window: top bar with the status pill and gear, Home tiles, the side bar, the first-run
//! checklist, error banners and a log panel.

pub mod app;
mod firstrun;
mod home;
mod settings;
mod sidebar;
pub mod theme;

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::instance::{self, Acquire};
use crate::paths::Paths;

pub fn run(paths: Paths) -> Result<()> {
    paths.ensure_dirs()?;
    let lock = match instance::acquire(&paths)? {
        Acquire::Acquired(lock) => lock,
        Acquire::AlreadyRunning(pid) => {
            // One Shard per folder: bring the open one to the front instead.
            if let Some(pid) = pid {
                crate::winutil::focus_window_of(pid);
            }
            return Ok(());
        }
    };
    crate::logging::init(&paths.logs);
    tracing::info!(
        "Shard {} started in {}",
        env!("CARGO_PKG_VERSION"),
        paths.root.display()
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Shard")
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Shard",
        options,
        Box::new(move |cc| Ok(Box::new(app::ShardApp::new(cc, paths, lock)))),
    )
    .map_err(|e| anyhow!("could not open the window: {e}"))
}
