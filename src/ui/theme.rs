//! Dark theme with an ice-blue accent. (M3 builds the full look on top of this.)

use eframe::egui::{self, Color32, Theme};

pub const ICE: Color32 = Color32::from_rgb(0x8c, 0xd8, 0xff);
pub const BG: Color32 = Color32::from_rgb(0x0f, 0x13, 0x19);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x1b, 0x23);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x96, 0xa6);
pub const GOOD: Color32 = Color32::from_rgb(0x5f, 0xd3, 0x8d);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xc0, 0x5c);
pub const BAD: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.hyperlink_color = ICE;
    v.selection.bg_fill = ICE.gamma_multiply(0.35);
    v.selection.stroke.color = ICE;
    v.widgets.hovered.bg_stroke.color = ICE;
    v.widgets.active.bg_stroke.color = ICE;
    ctx.set_visuals_of(Theme::Dark, v);
}
