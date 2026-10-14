//! Dark theme with an ice-blue accent, and the status pill's wording and colours.

use eframe::egui::{self, Color32, Theme};

use crate::state::AppState;

pub const ICE: Color32 = Color32::from_rgb(0x8c, 0xd8, 0xff);
pub const BG: Color32 = Color32::from_rgb(0x0f, 0x13, 0x19);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x1b, 0x23);
pub const TILE: Color32 = Color32::from_rgb(0x1c, 0x24, 0x2f);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Busy,
    Idle,
    Warn,
    Bad,
}

impl Tone {
    pub fn color(self) -> Color32 {
        match self {
            Tone::Good => GOOD,
            Tone::Busy => ICE,
            Tone::Idle => TEXT_DIM,
            Tone::Warn => WARN,
            Tone::Bad => BAD,
        }
    }
}

/// The status pill: (label, tone). Setup shows as "Downloading" (brief §15).
pub fn pill(state: &AppState) -> (&'static str, Tone) {
    match state {
        AppState::NotSetUp => ("Not set up", Tone::Warn),
        AppState::SettingUp => ("Downloading", Tone::Busy),
        AppState::Stopped => ("Stopped", Tone::Idle),
        AppState::Booting => ("Booting", Tone::Busy),
        AppState::Running => ("Running", Tone::Good),
        AppState::Stopping => ("Stopping", Tone::Busy),
        AppState::Error(_) => ("Error", Tone::Bad),
    }
}
