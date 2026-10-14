//! The M1 window: renders the worker's events; every action is a command to the worker.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, Margin, RichText};

use super::theme;
use crate::instance::InstanceLock;
use crate::jobs::{self, Command, Engine, Event};
use crate::paths::Paths;
use crate::state::AppState;

const LOG_LINES: usize = 400;

pub struct ShardApp {
    paths: Paths,
    _lock: InstanceLock,
    engine: Option<Engine>,
    state: AppState,
    set_up: bool,
    step: String,
    progress: Option<(String, f32)>,
    warnings: Vec<String>,
    log: VecDeque<String>,
    running: Option<(String, Option<String>)>,
    last_error: Option<String>,
}

impl ShardApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths, lock: InstanceLock) -> Self {
        theme::apply(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(paths.clone(), Arc::new(move || ctx.request_repaint()));
        let set_up = jobs::is_set_up(&paths);
        let mut warnings = Vec::new();
        if !set_up {
            // First run: say up front if the folder is a poor place for ~15 GB of emulator.
            warnings.extend(crate::preflight::path_warnings(&paths));
            warnings.extend(crate::preflight::disk_warning(&paths));
        }
        Self {
            paths,
            _lock: lock,
            engine: Some(engine),
            state: AppState::NotSetUp,
            set_up,
            step: String::new(),
            progress: None,
            warnings,
            log: VecDeque::new(),
            running: None,
            last_error: None,
        }
    }

    fn push_log(&mut self, line: String) {
        for l in line.lines() {
            if self.log.len() == LOG_LINES {
                self.log.pop_front();
            }
            self.log.push_back(l.to_string());
        }
    }

    fn pump(&mut self) {
        let events: Vec<Event> = match &self.engine {
            Some(e) => e.events.try_iter().collect(),
            None => return,
        };
        for ev in events {
            match ev {
                Event::State(s) => {
                    if !matches!(
                        s,
                        AppState::SettingUp | AppState::Booting | AppState::Stopping
                    ) {
                        self.progress = None;
                        self.step.clear();
                    }
                    if s != AppState::Running {
                        self.running = None;
                    }
                    self.set_up = jobs::is_set_up(&self.paths);
                    self.state = s;
                }
                Event::Step(s) => {
                    self.push_log(format!("» {s}"));
                    self.step = s;
                    self.progress = None;
                }
                Event::Progress { label, done, total } => {
                    let f = if total == 0 {
                        1.0
                    } else {
                        done as f32 / total as f32
                    };
                    self.progress = Some((label, f));
                }
                Event::Log(m) => self.push_log(m),
                Event::Warning(w) => {
                    self.push_log(format!("warning: {w}"));
                    if !self.warnings.contains(&w) {
                        self.warnings.push(w);
                    }
                }
                Event::Running {
                    serial, renderer, ..
                } => self.running = Some((serial, renderer)),
                Event::Finished { result, .. } => {
                    self.last_error = result.err();
                }
                Event::ShutDown => {}
            }
        }
    }

    fn send(&mut self, cmd: Command) {
        self.last_error = None;
        if let Some(e) = &self.engine {
            e.send(cmd);
        }
    }

    fn cancel(&self) {
        if let Some(e) = &self.engine {
            e.cancel();
        }
    }

    fn main(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        if let AppState::Error(msg) = &self.state {
            banner(ui, theme::BAD, msg);
        } else if let Some(e) = &self.last_error {
            banner(ui, theme::BAD, e);
        }
        for w in &self.warnings {
            banner(ui, theme::WARN, w);
        }
        ui.add_space(8.0);

        let headline = match &self.state {
            AppState::NotSetUp => "Not set up yet. Setup downloads about 2.1 GB from Google: the Android Emulator, platform-tools and the Android 13 Google Play image.".to_string(),
            AppState::SettingUp | AppState::Booting => self.step.clone(),
            AppState::Stopped => "Ready.".into(),
            AppState::Running => match &self.running {
                Some((serial, Some(r))) => format!("Running ({serial}) on {r}"),
                Some((serial, None)) => format!("Running ({serial})"),
                None => "Running".into(),
            },
            AppState::Stopping => "Stopping the emulator…".into(),
            AppState::Error(_) => "Something went wrong; details above and in logs\\shard.log.".into(),
        };
        ui.label(RichText::new(headline).size(16.0));
        if let Some((label, f)) = &self.progress {
            ui.add_space(6.0);
            ui.add(egui::ProgressBar::new(*f).text(format!("{label}  {:.0}%", f * 100.0)));
        }
        ui.add_space(12.0);

        let state = self.state.clone();
        ui.horizontal(|ui| match state {
            AppState::NotSetUp => {
                if ui.button("Set up").clicked() {
                    self.send(Command::Setup { dev_cache: None });
                }
            }
            AppState::SettingUp | AppState::Booting => {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }
            }
            AppState::Stopped => {
                if ui.button("Start").clicked() {
                    self.send(Command::Boot);
                }
                if ui
                    .button("Repair")
                    .on_hover_text(
                        "Check Google's SDK manifests and re-download anything missing or damaged",
                    )
                    .clicked()
                {
                    self.send(Command::Setup { dev_cache: None });
                }
            }
            AppState::Running => {
                if ui.button("Stop").clicked() {
                    self.send(Command::Stop);
                }
            }
            AppState::Stopping => {
                ui.add_enabled(false, egui::Button::new("Stopping…"));
            }
            AppState::Error(_) => {
                if self.set_up {
                    if ui.button("Start").clicked() {
                        self.send(Command::Boot);
                    }
                    if ui.button("Repair").clicked() {
                        self.send(Command::Setup { dev_cache: None });
                    }
                } else if ui.button("Set up").clicked() {
                    self.send(Command::Setup { dev_cache: None });
                }
            }
        });
    }
}

impl eframe::App for ShardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Shard").size(22.0).strong().color(theme::ICE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        pill(ui, &self.state);
                    });
                });
            });
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(200.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| {
                ui.label(RichText::new("Log").color(theme::TEXT_DIM));
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(RichText::new(line).monospace().size(12.0));
                        }
                    });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| self.main(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // A running emulator stays up; otherwise adb is stopped and the profile tidied.
        if let Some(engine) = self.engine.take() {
            engine.shutdown(Duration::from_secs(10));
        }
    }
}

fn pill(ui: &mut egui::Ui, state: &AppState) {
    let color = match state {
        AppState::Running => theme::GOOD,
        AppState::Error(_) => theme::BAD,
        AppState::NotSetUp => theme::WARN,
        AppState::SettingUp | AppState::Booting | AppState::Stopping => theme::ICE,
        AppState::Stopped => theme::TEXT_DIM,
    };
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(state.label()).color(color).strong());
        });
}

fn banner(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(color));
        });
    ui.add_space(4.0);
}
