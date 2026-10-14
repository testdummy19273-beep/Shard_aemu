//! The window: renders the worker's events; every action is a command to the worker (or, for
//! the side bar, to the control thread).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Margin, RichText};

use super::theme;
use crate::checklist::{Checklist, Update};
use crate::control::{Control, ControlCmd};
use crate::device::Markers;
use crate::instance::InstanceLock;
use crate::jobs::{self, Command, Engine, Event, Job};
use crate::paths::Paths;
use crate::state::AppState;

const LOG_LINES: usize = 400;
/// The app list is refreshed when Shard's window gets the focus back, at most this often.
const APPS_REFRESH_MIN: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum View {
    FirstRun,
    Home,
}

pub struct ShardApp {
    pub(super) paths: Paths,
    _lock: InstanceLock,
    engine: Option<Engine>,
    pub(super) control: Control,
    pub(super) state: AppState,
    pub(super) set_up: bool,
    pub(super) step: String,
    pub(super) progress: Option<(String, f32)>,
    pub(super) warnings: Vec<String>,
    pub(super) log: VecDeque<String>,
    pub(super) show_log: bool,
    pub(super) running: Option<(String, Option<String>)>,
    pub(super) last_error: Option<String>,
    /// What Retry sends again.
    pub(super) last_cmd: Option<Command>,
    pub(super) apps: Vec<String>,
    apps_asked: Option<Instant>,
    was_focused: bool,
    pub(super) view: View,
    pub(super) checklist: Checklist,
    /// The app a Launch job is starting.
    pub(super) launching: Option<String>,
    pub(super) device_page: bool,
    pub(super) confirm_wipe: bool,
}

impl ShardApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths, lock: InstanceLock) -> Self {
        theme::apply(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(paths.clone(), Arc::new(move || ctx.request_repaint()));
        let control = Control::start(&paths);
        let set_up = jobs::is_set_up(&paths);
        let first_run = !set_up || !Markers::load(&paths).first_boot_done();
        Self {
            apps: jobs::load_apps(&paths),
            control,
            _lock: lock,
            engine: Some(engine),
            state: AppState::NotSetUp,
            set_up,
            step: String::new(),
            progress: None,
            warnings: Vec::new(),
            log: VecDeque::new(),
            show_log: first_run,
            running: None,
            last_error: None,
            last_cmd: None,
            apps_asked: None,
            was_focused: true,
            view: if first_run {
                View::FirstRun
            } else {
                View::Home
            },
            checklist: Checklist::new(set_up),
            launching: None,
            device_page: false,
            confirm_wipe: false,
            paths,
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
        let warnings: Vec<String> = self.control.warnings.try_iter().collect();
        for w in warnings {
            self.push_log(format!("warning: {w}"));
        }
        let events: Vec<Event> = match &self.engine {
            Some(e) => e.events.try_iter().collect(),
            None => return,
        };
        for ev in events {
            self.on_event(ev);
        }
    }

    fn on_event(&mut self, ev: Event) {
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
                    self.control.send(ControlCmd::Detach);
                }
                self.set_up = jobs::is_set_up(&self.paths);
                self.state = s;
            }
            Event::Step(s) => {
                self.push_log(format!("» {s}"));
                self.step = s;
                self.progress = None;
            }
            Event::Check { step, status } => self.checklist.apply(Update::Step(step, status)),
            Event::Progress { label, done, total } => {
                let f = if total == 0 {
                    1.0
                } else {
                    done as f32 / total as f32
                };
                self.checklist.apply(Update::Progress {
                    label: label.clone(),
                    fraction: f,
                });
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
            } => {
                self.control.send(ControlCmd::Attach(serial.clone()));
                self.running = Some((serial, renderer));
            }
            Event::Apps(apps) => self.apps = apps,
            Event::Finished { job, result } => {
                if let Err(e) = &result {
                    self.checklist.apply(Update::JobFailed {
                        cancelled: e.ends_with("cancelled"),
                        message: e.clone(),
                    });
                }
                if job == Job::Launch {
                    self.launching = None;
                }
                // First run: setup is followed by the device's first start.
                if job == Job::Setup && result.is_ok() && self.view == View::FirstRun {
                    self.send(Command::Boot);
                }
                self.last_error = result.err().filter(|e| !e.ends_with("cancelled"));
            }
            Event::ShutDown => {}
        }
    }

    pub(super) fn send(&mut self, cmd: Command) {
        self.last_error = None;
        if let Command::Launch(pkg) = &cmd {
            self.launching = Some(pkg.clone());
        }
        if matches!(
            cmd,
            Command::Setup { .. } | Command::Boot | Command::Launch(_) | Command::Debloat
        ) {
            self.last_cmd = Some(cmd.clone());
        }
        if let Some(e) = &self.engine {
            e.send(cmd);
        }
    }

    pub(super) fn cancel(&self) {
        if let Some(e) = &self.engine {
            e.cancel();
        }
    }

    pub(super) fn retry(&mut self) {
        match self.last_cmd.clone() {
            Some(cmd) => self.send(cmd),
            None if self.set_up => self.send(Command::Boot),
            None => self.send(Command::Setup { dev_cache: None }),
        }
    }

    /// Back in Shard's window: the user may have installed or removed apps meanwhile.
    fn refresh_on_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.focused);
        let regained = focused && !self.was_focused;
        self.was_focused = focused;
        if regained
            && self.state == AppState::Running
            && self
                .apps_asked
                .is_none_or(|t| t.elapsed() >= APPS_REFRESH_MIN)
        {
            self.apps_asked = Some(Instant::now());
            if let Some(e) = &self.engine {
                e.send(Command::RefreshApps);
            }
        }
    }

    pub(super) fn open_log(&self) {
        crate::winutil::open_path(&self.paths.logs.join("shard.log"));
    }

    /// Error (or last failed job) banner with Open log and Retry, plus warnings.
    pub(super) fn banners(&mut self, ui: &mut egui::Ui) {
        let error = match &self.state {
            AppState::Error(msg) => Some(msg.clone()),
            _ => self.last_error.clone(),
        };
        if let Some(msg) = error {
            let (first, rest) = msg.split_once('\n').unwrap_or((msg.as_str(), ""));
            let mut open_log = false;
            let mut retry = false;
            banner_frame(theme::BAD).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(first).color(theme::BAD));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let busy = matches!(
                            self.state,
                            AppState::SettingUp | AppState::Booting | AppState::Stopping
                        );
                        retry = ui.add_enabled(!busy, egui::Button::new("Retry")).clicked();
                        open_log = ui.button("Open log").clicked();
                    });
                });
                if !rest.trim().is_empty() {
                    egui::CollapsingHeader::new("Details")
                        .id_salt("error-details")
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    ui.label(RichText::new(rest).monospace().size(11.5));
                                });
                        });
                }
            });
            ui.add_space(4.0);
            if open_log {
                self.open_log();
            }
            if retry {
                self.retry();
            }
        }
        let mut dismiss = None;
        for (i, w) in self.warnings.iter().enumerate() {
            banner_frame(theme::WARN).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(w).color(theme::WARN)).wrap());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                            dismiss = Some(i);
                        }
                    });
                });
            });
            ui.add_space(4.0);
        }
        if let Some(i) = dismiss {
            self.warnings.remove(i);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Shard").size(22.0).strong().color(theme::ICE));
            ui.add_space(8.0);
            pill(ui, &self.state);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let gear = super::sidebar::named(
                    ui.add_enabled(
                        self.set_up,
                        egui::Button::new(RichText::new("⚙").size(18.0)).frame(false),
                    ),
                    "Device settings",
                )
                .on_hover_text("Device: debloat again, wipe data, open folder");
                if gear.clicked() {
                    self.device_page = true;
                }
                let log = if self.show_log {
                    "Hide log"
                } else {
                    "Show log"
                };
                if ui
                    .add(egui::Button::new(RichText::new(log).color(theme::TEXT_DIM)).frame(false))
                    .clicked()
                {
                    self.show_log = !self.show_log;
                }
            });
        });
    }

    fn log_panel(&self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Log").color(theme::TEXT_DIM));
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.log {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
    }
}

impl eframe::App for ShardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        let ctx = ui.ctx().clone();
        self.refresh_on_focus(&ctx);
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::right("sidebar")
            .resizable(false)
            .exact_size(super::sidebar::WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(4, 10)),
            )
            .show(ui, |ui| self.sidebar(ui));
        if self.show_log {
            egui::Panel::bottom("log")
                .resizable(true)
                .default_size(180.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme::BG)
                        .inner_margin(Margin::symmetric(16, 8)),
                )
                .show(ui, |ui| self.log_panel(ui));
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| match self.view {
                View::FirstRun => self.first_run(ui),
                View::Home => self.home(ui),
            });
        self.device_page(&ctx);
        // The worker wakes the UI for its events; the focus check needs no polling.
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.control.send(ControlCmd::Detach);
        // A running emulator stays up; otherwise adb is stopped and the profile tidied.
        if let Some(engine) = self.engine.take() {
            engine.shutdown(Duration::from_secs(10));
        }
    }
}

fn pill(ui: &mut egui::Ui, state: &AppState) {
    let (label, tone) = theme::pill(state);
    let color = tone.color();
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(label).color(color).strong());
        });
}

pub(super) fn banner_frame(color: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(6)
        .inner_margin(Margin::same(8))
}
