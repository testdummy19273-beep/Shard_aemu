//! Home: what the emulator is doing, Start/Stop, and a tile per app (Play Store first).

use eframe::egui::{self, Align2, FontId, RichText, Sense, Vec2};

use super::app::ShardApp;
use super::sidebar::named;
use super::theme;
use crate::device::{self, PLAY_STORE};
use crate::jobs::Command;
use crate::state::AppState;

const TILE: f32 = 120.0;

impl ShardApp {
    pub(super) fn home(&mut self, ui: &mut egui::Ui) {
        self.banners(ui);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.headline()).size(16.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.actions(ui);
            });
        });
        if let Some((label, f)) = &self.progress {
            ui.add_space(6.0);
            ui.add(egui::ProgressBar::new(*f).text(format!("{label}  {:.0}%", f * 100.0)));
        }
        ui.add_space(14.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| self.tiles(ui));
    }

    fn headline(&self) -> String {
        match (&self.state, &self.launching) {
            (AppState::Booting, Some(pkg)) => {
                format!("Starting the emulator for {}…", name_of(pkg))
            }
            (AppState::Booting, None) if !self.step.is_empty() => self.step.clone(),
            (AppState::Booting, None) => "Starting the emulator…".into(),
            (AppState::Running, Some(pkg)) => format!("Opening {}…", name_of(pkg)),
            (AppState::Running, None) => match &self.running {
                Some((_, Some(r))) => format!("Running on {r}"),
                _ => "Running".into(),
            },
            (AppState::Stopped, _) => "Stopped. Pick an app to start it, or Start.".into(),
            (AppState::Stopping, _) => "Stopping the emulator…".into(),
            (AppState::SettingUp, _) => self.step.clone(),
            (AppState::NotSetUp, _) => "Not set up yet.".into(),
            (AppState::Error(_), _) => "Something went wrong (details above).".into(),
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui) {
        match self.state.clone() {
            AppState::Stopped => {
                if ui.button("Start").clicked() {
                    self.send(Command::Boot);
                }
            }
            AppState::Running => {
                if ui.button("Stop").clicked() {
                    self.send(Command::Stop);
                }
            }
            AppState::Booting | AppState::SettingUp => {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }
            }
            AppState::Stopping => {
                ui.add_enabled(false, egui::Button::new("Stopping…"));
            }
            AppState::Error(_) if self.set_up => {
                if ui.button("Start").clicked() {
                    self.send(Command::Boot);
                }
            }
            AppState::Error(_) | AppState::NotSetUp => {
                if ui.button("Set up").clicked() {
                    self.send(Command::Setup { dev_cache: None });
                }
            }
        }
    }

    fn tiles(&mut self, ui: &mut egui::Ui) {
        // A click while stopped starts the emulator first; while it is stopping or being set
        // up there is nothing to start an app on.
        let usable = self.set_up
            && matches!(
                self.state,
                AppState::Stopped | AppState::Booting | AppState::Running | AppState::Error(_)
            )
            && self.launching.is_none();
        let mut packages = vec![PLAY_STORE.to_string()];
        packages.extend(self.apps.iter().filter(|p| *p != PLAY_STORE).cloned());
        let mut clicked = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(12.0);
            for pkg in &packages {
                if tile(ui, pkg, usable, self.launching.as_deref() == Some(pkg)).clicked() {
                    clicked = Some(pkg.clone());
                }
            }
        });
        if self.apps.is_empty() {
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "Apps you install from the Play Store show up here (the list is read when \
                     the emulator starts and when you come back to Shard).",
                )
                .color(theme::TEXT_DIM),
            );
        }
        if let Some(pkg) = clicked {
            self.send(Command::Launch(pkg));
        }
    }
}

fn name_of(pkg: &str) -> String {
    if pkg == PLAY_STORE {
        "Play Store".into()
    } else {
        device::app_label(pkg)
    }
}

/// A 120x120 tile: a round badge with the first letter, and the app's name under it.
fn tile(ui: &mut egui::Ui, pkg: &str, enabled: bool, busy: bool) -> egui::Response {
    let name = name_of(pkg);
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::splat(TILE),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = enabled && resp.hovered();
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        10.0,
        if hovered {
            theme::TILE.gamma_multiply(1.6)
        } else {
            theme::TILE
        },
    );
    if hovered || busy {
        painter.rect_stroke(
            rect,
            10.0,
            egui::Stroke::new(1.0, theme::ICE),
            egui::StrokeKind::Inside,
        );
    }
    let badge = rect.center() - Vec2::new(0.0, 14.0);
    let accent = if pkg == PLAY_STORE {
        theme::GOOD
    } else {
        theme::ICE
    };
    painter.circle_filled(
        badge,
        26.0,
        accent.gamma_multiply(if enabled { 0.30 } else { 0.12 }),
    );
    let letter: String = name.chars().next().unwrap_or('?').to_uppercase().collect();
    painter.text(
        badge,
        Align2::CENTER_CENTER,
        letter,
        FontId::proportional(26.0),
        if enabled { accent } else { theme::TEXT_DIM },
    );
    let label = if busy {
        "Opening…".to_string()
    } else {
        elide(&name, 15)
    };
    painter.text(
        rect.center_bottom() - Vec2::new(0.0, 18.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(13.5),
        if enabled {
            ui.visuals().strong_text_color()
        } else {
            theme::TEXT_DIM
        },
    );
    let resp = named(resp, &name);
    if pkg == PLAY_STORE {
        resp.on_hover_text("Play Store")
    } else {
        resp.on_hover_text(format!("{name}\n{pkg}"))
    }
}

fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
