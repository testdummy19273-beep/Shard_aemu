//! debloat.txt parsing, keep-list enforcement and parsing of device output (samples captured
//! from the Android 13 Play image). No device needed.

use std::collections::BTreeSet;

use shard::device::{
    self, is_valid_package, keep_list, missing_keep, package_of_component, package_of_ime,
    parse_debloat, parse_package_list, plan, top_resumed_package, webview_from_dumpsys, Detected,
    KEEP_FIXED,
};

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn image_detected() -> Detected {
    Detected {
        launcher: Some("com.google.android.apps.nexuslauncher".into()),
        keyboard: Some("com.google.android.inputmethod.latin".into()),
        installer: Some("com.google.android.packageinstaller".into()),
        permission_controller: Some("com.google.android.permissioncontroller".into()),
        webview: Some("com.google.android.webview".into()),
    }
}

#[test]
fn package_names_are_validated() {
    for ok in [
        "com.google.android.youtube",
        "com.android.camera2",
        "a.b",
        "com.example_app.x_1",
    ] {
        assert!(is_valid_package(ok), "{ok}");
    }
    for bad in [
        "",
        "youtube",
        "com..google",
        ".com.google",
        "com.google.",
        "1com.google",
        "com.1google",
        "com.google; reboot",
        "com.google&&reboot",
        "com.google android",
        "com.google/x",
        "com.goo$(x)",
        "com.gööle",
    ] {
        assert!(!is_valid_package(bad), "{bad}");
    }
}

#[test]
fn debloat_txt_parsing() {
    let text = "\u{feff}# header\r\n\
                com.google.android.youtube   # YouTube\r\n\
                \r\n\
                   com.google.android.gm\t\r\n\
                com.google.android.youtube\r\n\
                com.google.android.gm; reboot\r\n\
                youtube\r\n\
                # com.google.android.videos\r\n\
                com.android.camera2#no space before comment";
    let list = parse_debloat(text);
    assert_eq!(
        list.packages,
        [
            "com.google.android.youtube",
            "com.google.android.gm",
            "com.android.camera2"
        ]
    );
    assert_eq!(list.problems.len(), 3, "{:?}", list.problems);
    assert!(list.problems[0].starts_with("line 5:") && list.problems[0].contains("twice"));
    assert!(list.problems[1].starts_with("line 6:") && list.problems[1].contains("reboot"));
    assert!(list.problems[2].starts_with("line 7:"));
}

#[test]
fn shipped_debloat_txt_is_clean_and_respects_the_keep_list() {
    let list = parse_debloat(device::DEFAULT_DEBLOAT);
    assert!(list.problems.is_empty(), "{:?}", list.problems);
    assert_eq!(list.packages.len(), 11);
    assert!(list
        .packages
        .contains(&"com.google.android.googlequicksearchbox".to_string()));
    let keep = keep_list(&image_detected());
    for p in &list.packages {
        assert!(!keep.contains(p), "{p} is on the keep-list");
    }
}

#[test]
fn keep_list_wins_over_debloat_txt() {
    let detected = image_detected();
    let keep = keep_list(&detected);
    for p in KEEP_FIXED {
        assert!(keep.contains(*p));
    }
    assert!(keep.contains("com.google.android.apps.nexuslauncher"));
    assert!(keep.contains("com.google.android.inputmethod.latin"));
    assert!(keep.contains("com.google.android.webview"));
    assert!(keep.contains("com.android.webview"));
    let listed: Vec<String> = [
        "com.android.vending",
        "com.google.android.youtube",
        "com.google.android.apps.nexuslauncher",
        "com.google.android.gm",
        "com.android.webview",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let installed = set(&[
        "com.android.vending",
        "com.google.android.youtube",
        "com.google.android.apps.nexuslauncher",
        "com.google.android.webview",
    ]);
    let p = plan(&listed, &keep, &installed);
    assert_eq!(p.remove, ["com.google.android.youtube"]);
    assert_eq!(
        p.kept,
        [
            "com.android.vending",
            "com.google.android.apps.nexuslauncher",
            "com.android.webview"
        ]
    );
    assert_eq!(p.absent, ["com.google.android.gm"]);
}

#[test]
fn a_detected_launcher_is_kept_even_if_not_the_default() {
    let mut d = image_detected();
    d.launcher = Some("com.example.launcher".into());
    let keep = keep_list(&d);
    assert!(keep.contains("com.example.launcher"));
    // Undetected roles fall back to the image's defaults.
    let fallback = keep_list(&Detected::default());
    assert!(fallback.contains("com.google.android.apps.nexuslauncher"));
    assert!(fallback.contains("com.google.android.permissioncontroller"));
    assert!(Detected::default().roles().iter().all(|(_, _, fb)| *fb));
}

#[test]
fn webview_counts_as_present_if_either_provider_is() {
    let keep = keep_list(&image_detected());
    let mut installed = keep.clone();
    installed.remove("com.android.webview");
    assert!(missing_keep(&keep, &installed).is_empty());
    installed.remove("com.google.android.webview");
    assert_eq!(
        missing_keep(&keep, &installed),
        ["com.google.android.webview or com.android.webview"]
    );
    installed.insert("com.google.android.webview".into());
    installed.remove("com.android.chrome");
    assert_eq!(missing_keep(&keep, &installed), ["com.android.chrome"]);
}

#[test]
fn package_list_output() {
    let out =
        "package:android\r\npackage:com.android.camera2\npackage:com.google.android.gm\n\nnoise\n";
    assert_eq!(
        parse_package_list(out),
        set(&["com.android.camera2", "com.google.android.gm"])
    );
}

#[test]
fn resolve_activity_output() {
    // Real output on the image (preceded by a "priority=..." block on some versions).
    assert_eq!(
        package_of_component(
            "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\n\
             com.google.android.apps.nexuslauncher/.NexusLauncherActivity\n"
        )
        .as_deref(),
        Some("com.google.android.apps.nexuslauncher")
    );
    assert_eq!(
        package_of_component(
            "com.google.android.packageinstaller/com.android.packageinstaller.InstallStart"
        )
        .as_deref(),
        Some("com.google.android.packageinstaller")
    );
    // Several home apps: the system chooser is not an answer.
    assert_eq!(
        package_of_component("android/com.android.internal.app.ResolverActivity"),
        None
    );
    assert_eq!(package_of_component("No activity found\n"), None);
    assert_eq!(package_of_component(""), None);
}

#[test]
fn ime_output() {
    assert_eq!(
        package_of_ime(
            "com.google.android.inputmethod.latin/com.android.inputmethod.latin.LatinIME\n"
        )
        .as_deref(),
        Some("com.google.android.inputmethod.latin")
    );
    assert_eq!(package_of_ime("null\n"), None);
}

#[test]
fn webview_dumpsys_output() {
    let out =
        "Current WebView package (name, version): (com.google.android.webview, 109.0.5414.123)\n";
    assert_eq!(
        webview_from_dumpsys(out).as_deref(),
        Some("com.google.android.webview")
    );
    assert_eq!(webview_from_dumpsys("nothing here"), None);
}

#[test]
fn top_resumed_activity_output() {
    let out = "      topResumedActivity=ActivityRecord{406dd7f u0 com.google.android.apps.nexuslauncher/.NexusLauncherActivity} t8}\n";
    assert_eq!(
        top_resumed_package(out).as_deref(),
        Some("com.google.android.apps.nexuslauncher")
    );
    let old =
        "  mResumedActivity: ActivityRecord{1 u0 com.android.vending/.AssetBrowserActivity t3}";
    assert_eq!(
        top_resumed_package(old).as_deref(),
        Some("com.android.vending")
    );
    assert_eq!(top_resumed_package("topResumedActivity=null"), None);
}

#[test]
fn package_save_wait() {
    use shard::device::{package_save_remaining, PACKAGE_SAVE_DELAY};
    use std::time::{Duration, Instant};
    assert_eq!(package_save_remaining(None), Duration::ZERO);
    let now = package_save_remaining(Some(Instant::now()));
    assert!(now > PACKAGE_SAVE_DELAY - Duration::from_secs(1) && now <= PACKAGE_SAVE_DELAY);
    if let Some(old) = Instant::now().checked_sub(PACKAGE_SAVE_DELAY + Duration::from_secs(1)) {
        assert_eq!(package_save_remaining(Some(old)), Duration::ZERO);
    }
}
