//! Milestone 3: app labels and launch parsing, side-bar key codes, the status pill and the
//! first-run checklist reducer.

use shard::checklist::{Checklist, StepId, StepStatus, Update, CANCELLED};
use shard::control::Key;
use shard::device::{app_label, component_of, parse_package_list};
use shard::sdk::manifest::PackageId;
use shard::state::AppState;
use shard::ui::theme::{pill, Tone};

#[test]
fn app_labels_come_from_the_package_name() {
    for (pkg, label) in [
        ("com.YoStarEN.AzurLane", "Azur Lane"),
        ("com.supercell.clashofclans", "Clashofclans"),
        ("com.kurogame.wutheringwaves.global", "Wutheringwaves"),
        ("com.miHoYo.GenshinImpact", "Genshin Impact"),
        ("jp.co.cygames.umamusume", "Umamusume"),
        ("com.example.my_cool_game", "My Cool Game"),
        ("com.studio.game2048", "Game 2048"),
        ("com.innersloth.spacemafia", "Spacemafia"),
        // Only generic parts: the last one is used anyway.
        ("com.android", "Android"),
    ] {
        assert_eq!(app_label(pkg), label, "{pkg}");
    }
}

#[test]
fn user_app_list_parsing() {
    let out = "package:com.YoStarEN.AzurLane\r\npackage:com.supercell.clashofclans\n\nnot a line\n";
    let apps: Vec<String> = parse_package_list(out).into_iter().collect();
    assert_eq!(
        apps,
        ["com.YoStarEN.AzurLane", "com.supercell.clashofclans"]
    );
}

#[test]
fn launcher_activity_from_resolve_activity() {
    let brief = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\n\
                 com.android.chrome/com.google.android.apps.chrome.Main\n";
    assert_eq!(
        component_of(brief, "com.android.chrome").as_deref(),
        Some("com.android.chrome/com.google.android.apps.chrome.Main")
    );
    assert_eq!(
        component_of(
            "com.YoStarEN.AzurLane/com.manjuu.azurlane.MainActivity",
            "com.YoStarEN.AzurLane"
        )
        .as_deref(),
        Some("com.YoStarEN.AzurLane/com.manjuu.azurlane.MainActivity")
    );
    // Not found, the system chooser, another package, or anything that is not a component.
    assert_eq!(component_of("No activity found\n", "com.a.b"), None);
    assert_eq!(
        component_of(
            "android/com.android.internal.app.ResolverActivity",
            "com.a.b"
        ),
        None
    );
    assert_eq!(component_of("com.x.y/.Main", "com.a.b"), None);
    assert_eq!(component_of("com.a.b/.Main; reboot", "com.a.b"), None);
}

#[test]
fn side_bar_key_codes() {
    assert_eq!(Key::Home.code(), 3);
    assert_eq!(Key::Back.code(), 4);
    assert_eq!(Key::Recents.code(), 187);
    assert_eq!(Key::VolumeUp.code(), 24);
    assert_eq!(Key::VolumeDown.code(), 25);
    assert_eq!(Key::Recents.command(), "input keyevent 187");
}

#[test]
fn status_pill_mapping() {
    assert_eq!(pill(&AppState::SettingUp), ("Downloading", Tone::Busy));
    assert_eq!(pill(&AppState::Booting), ("Booting", Tone::Busy));
    assert_eq!(pill(&AppState::Running), ("Running", Tone::Good));
    assert_eq!(pill(&AppState::Stopped), ("Stopped", Tone::Idle));
    assert_eq!(pill(&AppState::Error("x".into())), ("Error", Tone::Bad));
    assert_eq!(pill(&AppState::NotSetUp).1, Tone::Warn);
}

fn status(c: &Checklist, id: StepId) -> StepStatus {
    c.row(id).unwrap().status.clone()
}

#[test]
fn checklist_starts_from_what_exists() {
    let fresh = Checklist::new(false);
    assert_eq!(fresh.rows.len(), StepId::ALL.len());
    assert!(status(&fresh, StepId::Lock).finished());
    assert_eq!(status(&fresh, StepId::Folder), StepStatus::Pending);
    // Set up already (e.g. the first start was cancelled): only the first-start rows remain.
    let set_up = Checklist::new(true);
    assert!(set_up.setup_done());
    assert_eq!(status(&set_up, StepId::Boot), StepStatus::Pending);
    assert!(!set_up.all_done());
}

#[test]
fn checklist_reducer_tracks_steps_progress_and_cancel() {
    let mut c = Checklist::new(false);
    let emu = StepId::Download(PackageId::Emulator);
    c.apply(Update::Step(StepId::Folder, StepStatus::Running));
    c.apply(Update::Step(
        StepId::Folder,
        StepStatus::Warning("under OneDrive".into()),
    ));
    assert!(status(&c, StepId::Folder).finished());
    c.apply(Update::Step(emu, StepStatus::Running));
    c.apply(Update::Progress {
        label: "Downloading Android Emulator (434 MB)".into(),
        fraction: 0.25,
    });
    let row = c.row(emu).unwrap();
    assert_eq!(row.progress.as_ref().map(|p| p.1), Some(0.25));
    // Progress goes to the running step only, and is clamped.
    c.apply(Update::Progress {
        label: "x".into(),
        fraction: 7.0,
    });
    assert_eq!(
        c.row(emu).unwrap().progress.as_ref().map(|p| p.1),
        Some(1.0)
    );
    assert!(c.row(StepId::Folder).unwrap().progress.is_none());
    // Cancel: the running step shows it was cancelled, and nothing is running any more.
    c.apply(Update::JobFailed {
        cancelled: true,
        message: "setup cancelled".into(),
    });
    assert_eq!(status(&c, emu), StepStatus::Failed(CANCELLED.into()));
    assert!(c.row(emu).unwrap().progress.is_none());
    assert!(!c.running());
    assert_eq!(c.failed().map(|r| r.id), Some(emu));
    // Resume: the step runs again and finishes.
    c.apply(Update::Step(emu, StepStatus::Running));
    c.apply(Update::Step(emu, StepStatus::Done("37.2.12".into())));
    assert_eq!(c.failed(), None);
    assert!(!c.setup_done());
}

#[test]
fn checklist_reducer_records_failures() {
    let mut c = Checklist::new(true);
    c.apply(Update::Step(StepId::Boot, StepStatus::Running));
    c.apply(Update::JobFailed {
        cancelled: false,
        message: "Boot failed: timeout".into(),
    });
    assert_eq!(
        status(&c, StepId::Boot),
        StepStatus::Failed("Boot failed: timeout".into())
    );
    for id in [StepId::Boot, StepId::Debloat, StepId::PlayStore] {
        c.apply(Update::Step(id, StepStatus::Done(String::new())));
    }
    assert!(c.all_done());
}

/// Symbols checked by eye in egui 0.36's bundled fonts (ü and Ü are Latin-1, which Ubuntu Light
/// covers). egui's `has_glyph` reports some that
/// do render (e.g. U+2714, from the emoji font) as missing, so it cannot be the test.
/// Not in the fonts (they draw as an empty box): U+2261, U+2302, U+25CF, U+2713, U+2715.
const RENDERS: &str = "§»…×üÜ⏴▣○⏺⚙⚠✔✖⟳🔉🔊☰🏠";

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every non-ASCII character in the code (outside comments) must be one that egui's bundled
/// fonts draw: step names, log lines and errors from anywhere in `src` end up in the window.
#[test]
fn ui_symbols_are_in_the_bundled_fonts() {
    let mut files = Vec::new();
    rust_files(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
        &mut files,
    );
    let mut unchecked = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let code = line.split("//").next().unwrap_or("");
            for c in code
                .chars()
                .filter(|c| !c.is_ascii() && !RENDERS.contains(*c))
            {
                unchecked.push(format!("{} U+{:04X}", path.display(), c as u32));
            }
        }
    }
    assert!(
        unchecked.is_empty(),
        "symbols not checked to render: {unchecked:#?}"
    );
}
