//! The first-run screen (brief §9): the checklist with a status and progress bar per step, a
//! Cancel button, and Resume/Retry that continue where things stopped.

use eframe::egui::{self, RichText};

use super::app::{ShardApp, View};
use super::theme;
use crate::checklist::{StepId, StepStatus};
use crate::jobs::Command;
use crate::state::AppState;

impl ShardApp {
    pub(super) fn first_run(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Setting up Shard").size(20.0).strong());
        ui.label(
            RichText::new(
                "Shard downloads the Android Emulator, platform-tools and the Android 13 Google \
                 Play image (about 2 GB) from Google into this folder, builds the device, \
                 starts it once and removes the apps in debloat.txt. Cancel any time; it \
                 continues where it stopped.",
            )
            .color(theme::TEXT_DIM),
        );
        ui.add_space(8.0);
        self.banners(ui);
        ui.add_space(4.0);
        self.first_run_actions(ui);
        ui.add_space(10.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in self.checklist.rows.clone() {
                    step_row(ui, row.id, &row.status, row.progress.as_ref());
                }
            });
    }

    fn first_run_actions(&mut self, ui: &mut egui::Ui) {
        let busy = matches!(
            self.state,
            AppState::SettingUp | AppState::Booting | AppState::Stopping
        );
        let started = self
            .checklist
            .rows
            .iter()
            .any(|r| r.id != StepId::Lock && r.status != StepStatus::Pending);
        let boot_done = self
            .checklist
            .row(StepId::Boot)
            .is_some_and(|r| r.status.finished());
        ui.horizontal(|ui| {
            if busy {
                if self.state != AppState::Stopping && ui.button("Cancel").clicked() {
                    self.cancel();
                }
                ui.spinner();
            } else if boot_done && !self.checklist.running() {
                ui.label(
                    RichText::new("Done. Sign in to the Play Store in the emulator window.")
                        .color(theme::GOOD),
                );
                if ui.button("Open Home").clicked() {
                    self.view = View::Home;
                }
            } else {
                let label = if !started {
                    "Start setup"
                } else if self.checklist.failed().is_some() {
                    "Resume"
                } else {
                    "Continue"
                };
                if ui.button(label).clicked() {
                    if self.set_up {
                        self.send(Command::Boot);
                    } else {
                        self.send(Command::Setup { dev_cache: None });
                    }
                }
            }
        });
    }
}

fn step_row(ui: &mut egui::Ui, id: StepId, status: &StepStatus, progress: Option<&(String, f32)>) {
    let (mark, color, detail) = match status {
        StepStatus::Pending => ("○", theme::TEXT_DIM, ""),
        StepStatus::Running => ("⏺", theme::ICE, ""),
        StepStatus::Done(d) => ("✔", theme::GOOD, d.as_str()),
        StepStatus::Warning(d) => ("⚠", theme::WARN, d.as_str()),
        StepStatus::Failed(d) => ("✖", theme::BAD, d.as_str()),
    };
    ui.horizontal(|ui| {
        ui.add_sized(
            [22.0, 18.0],
            egui::Label::new(RichText::new(mark).color(color).size(15.0)),
        );
        let text = RichText::new(id.label());
        ui.label(if *status == StepStatus::Pending {
            text.color(theme::TEXT_DIM)
        } else {
            text
        });
        if *status == StepStatus::Running {
            ui.spinner();
        }
    });
    if !detail.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(30.0);
            ui.add(egui::Label::new(RichText::new(detail).color(color).size(12.5)).wrap());
        });
    }
    if let Some((label, f)) = progress {
        ui.horizontal(|ui| {
            ui.add_space(30.0);
            ui.add(
                egui::ProgressBar::new(*f)
                    .desired_width(ui.available_width().min(520.0))
                    .text(format!("{label}  {:.0}%", f * 100.0)),
            );
        });
    }
    ui.add_space(3.0);
}
