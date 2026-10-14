//! The 48 px side bar on the right edge: ☰ (volume, rotate), Home, Recents, Back. Clicks go to
//! the control thread's persistent adb shell, so they never wait on the worker.

use eframe::egui::{self, RichText, Vec2};

use super::app::ShardApp;
use super::theme;
use crate::control::{ControlCmd, Key};
use crate::state::AppState;

pub const WIDTH: f32 = 48.0;
const BUTTON: f32 = 40.0;

/// Gives an icon button a readable accessibility name (screen readers, UI Automation).
pub(super) fn named(resp: egui::Response, name: &str) -> egui::Response {
    let enabled = resp.enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
    resp
}

impl ShardApp {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let on = self.state == AppState::Running;
        let off = |what: &str| format!("{what} (when the emulator is running)");
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let menu = ui.add_enabled_ui(on, |ui| {
                ui.menu_button(RichText::new("☰").size(22.0), |ui| {
                    // Left open when the emulator stopped: there is nothing to send to.
                    if !on {
                        ui.close();
                    }
                    let item = |ui: &mut egui::Ui, text: &str, cmd: ControlCmd| {
                        if ui.button(text).clicked() {
                            self.control.send(cmd);
                        }
                    };
                    item(ui, "🔊  Volume up", ControlCmd::Key(Key::VolumeUp));
                    item(ui, "🔉  Volume down", ControlCmd::Key(Key::VolumeDown));
                    item(ui, "⟳  Rotate", ControlCmd::Rotate);
                })
                .response
            });
            named(menu.inner, "Quick menu")
                .on_hover_text("Volume, rotate")
                .on_disabled_hover_text(off("Volume, rotate"));
            ui.add_space(6.0);
            for (glyph, key, name) in [
                ("🏠", Key::Home, "Home"),
                ("▣", Key::Recents, "Recent apps"),
                ("⏴", Key::Back, "Back"),
            ] {
                let button = egui::Button::new(RichText::new(glyph).size(20.0).color(if on {
                    theme::ICE
                } else {
                    theme::TEXT_DIM
                }))
                .min_size(Vec2::splat(BUTTON));
                let resp = named(ui.add_enabled(on, button), name)
                    .on_hover_text(name)
                    .on_disabled_hover_text(off(name));
                if resp.clicked() {
                    self.control.send(ControlCmd::Key(key));
                }
            }
        });
    }
}
