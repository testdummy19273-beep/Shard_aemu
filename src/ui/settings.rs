//! The gear's device page: debloat again, wipe data (with a confirmation), open the folder,
//! repair. (Milestone 4 turns this into the full Settings tab.)

use eframe::egui::{self, RichText};

use super::app::ShardApp;
use super::theme;
use crate::jobs::Command;
use crate::state::AppState;

impl ShardApp {
    pub(super) fn device_page(&mut self, ctx: &egui::Context) {
        if self.device_page {
            let modal = egui::Modal::new(egui::Id::new("device-page")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(RichText::new("Device").size(18.0).strong());
                ui.add_space(8.0);
                let idle = matches!(self.state, AppState::Stopped | AppState::Error(_));
                let can_debloat = idle || self.state == AppState::Running;

                row(
                    ui,
                    "Debloat again",
                    "Remove the apps listed in debloat.txt (starts the emulator if needed).",
                    can_debloat,
                    || {
                        self.send(Command::Debloat);
                        self.device_page = false;
                    },
                );
                row(
                    ui,
                    "Wipe data…",
                    if idle {
                        "Reset Android to a fresh device on its next start. Everything installed \
                         and every account on it is erased."
                    } else {
                        "Stop the emulator first."
                    },
                    idle,
                    || self.confirm_wipe = true,
                );
                row(
                    ui,
                    "Open Shard folder",
                    &self.paths.root.display().to_string(),
                    true,
                    || {
                        crate::winutil::open_path(&self.paths.root);
                    },
                );
                row(
                    ui,
                    "Repair",
                    if idle {
                        "Check Google's SDK manifests and download again whatever is missing or \
                         damaged."
                    } else {
                        "Stop the emulator first."
                    },
                    idle,
                    || {
                        self.send(Command::Setup { dev_cache: None });
                        self.device_page = false;
                    },
                );
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        self.device_page = false;
                    }
                });
            });
            if modal.should_close() && !self.confirm_wipe {
                self.device_page = false;
            }
        }
        if self.confirm_wipe {
            let modal = egui::Modal::new(egui::Id::new("confirm-wipe")).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.label(RichText::new("Wipe the device's data?").size(17.0).strong());
                ui.add_space(6.0);
                ui.label(
                    "Android is reset to a fresh device on its next start: apps you installed, \
                     your Google account and all settings on it are erased. Shard then removes \
                     the debloat.txt apps and opens the Play Store again.",
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let wipe = egui::Button::new(RichText::new("Wipe data").color(theme::BAD));
                    if ui.add(wipe).clicked() {
                        self.send(Command::WipeData);
                        self.confirm_wipe = false;
                        self.device_page = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_wipe = false;
                    }
                });
            });
            if modal.should_close() {
                self.confirm_wipe = false;
            }
        }
    }
}

fn row(ui: &mut egui::Ui, button: &str, help: &str, enabled: bool, on_click: impl FnOnce()) {
    ui.horizontal(|ui| {
        let clicked = ui
            .add_enabled(
                enabled,
                egui::Button::new(button).min_size([150.0, 0.0].into()),
            )
            .clicked();
        ui.add(egui::Label::new(RichText::new(help).color(theme::TEXT_DIM).size(12.5)).wrap());
        if clicked {
            on_click();
        }
    });
    ui.add_space(6.0);
}
