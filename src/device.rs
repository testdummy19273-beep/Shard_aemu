//! Device-side work: package list, debloat/restore, tweaks, opening the Play Store, and the
//! markers that make the first-boot work run once per device (again after Wipe data).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::adb::Adb;
use crate::paths::Paths;

/// The `debloat.txt` written next to Shard.exe when there is none.
pub const DEFAULT_DEBLOAT: &str = include_str!("../debloat.txt");

/// Never removed, whatever debloat.txt says.
pub const KEEP_FIXED: &[&str] = &[
    "com.android.vending",
    "com.google.android.gms",
    "com.google.android.gsf",
    "com.android.systemui",
    "com.android.settings",
    // Game logins may open a browser.
    "com.android.chrome",
];
/// Android System WebView: one of these is the provider, depending on the image.
pub const WEBVIEWS: &[&str] = &["com.google.android.webview", "com.android.webview"];

/// What the Android 13 Play image uses, for when detection fails.
const FALLBACK_LAUNCHER: &str = "com.google.android.apps.nexuslauncher";
const FALLBACK_KEYBOARD: &str = "com.google.android.inputmethod.latin";
const FALLBACK_INSTALLER: &str = "com.google.android.packageinstaller";
const FALLBACK_PERMISSIONS: &str = "com.google.android.permissioncontroller";

pub const PLAY_STORE: &str = "com.android.vending";
const PACKAGES_INITIAL: &str = "packages-initial.txt";

/// Animation scales set by the tweaks, and "stay awake while charging" for AC, USB and wireless.
const TWEAKS: &[(&str, &str)] = &[
    ("window_animation_scale", "0.5"),
    ("transition_animation_scale", "0.5"),
    ("animator_duration_scale", "0.5"),
    ("stay_on_while_plugged_in", "7"),
];

const PM_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the first boot waits for the launcher before working anyway.
pub const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------------------------
// debloat.txt

/// An Android package name: dot-separated Java identifiers, at least two segments. Anything
/// else is refused before it reaches `adb shell` (which joins its arguments into one command
/// line for the device's shell).
pub fn is_valid_package(name: &str) -> bool {
    let mut segments = 0;
    for seg in name.split('.') {
        let mut chars = seg.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_ascii_alphabetic() || first == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return false;
        }
        segments += 1;
    }
    segments >= 2 && name.len() <= 255
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebloatList {
    /// Valid package names in file order, without duplicates.
    pub packages: Vec<String>,
    /// Lines that were ignored, with the reason ("line 4: ...").
    pub problems: Vec<String>,
}

/// Parses debloat.txt: one package per line, `#` starts a comment (whole line or after the
/// name), blank lines ignored. Tolerates a UTF-8 BOM and CRLF line ends (Notepad).
pub fn parse_debloat(text: &str) -> DebloatList {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut list = DebloatList::default();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let n = i + 1;
        if !is_valid_package(line) {
            list.problems.push(format!(
                "line {n}: \"{line}\" is not a package name; ignored"
            ));
        } else if list.packages.iter().any(|p| p == line) {
            list.problems
                .push(format!("line {n}: {line} is listed twice; ignored"));
        } else {
            list.packages.push(line.to_string());
        }
    }
    list
}

/// Reads debloat.txt next to Shard.exe, writing the default one first if there is none.
pub fn load_debloat(paths: &Paths) -> Result<DebloatList> {
    ensure_debloat_file(paths)?;
    let file = paths.debloat_file();
    let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    Ok(parse_debloat(&String::from_utf8_lossy(&bytes)))
}

/// Writes the default debloat.txt if the folder has none (never overwrites the user's).
pub fn ensure_debloat_file(paths: &Paths) -> Result<()> {
    let file = paths.debloat_file();
    if !file.exists() {
        std::fs::write(&file, DEFAULT_DEBLOAT.replace('\n', "\r\n"))
            .with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Keep-list

/// Packages detected on the device that must stay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    pub launcher: Option<String>,
    pub keyboard: Option<String>,
    pub installer: Option<String>,
    pub permission_controller: Option<String>,
    pub webview: Option<String>,
}

impl Detected {
    /// (role, detected package or fallback, whether it was a fallback)
    pub fn roles(&self) -> [(&'static str, String, bool); 4] {
        let pick = |d: &Option<String>, fb: &str| match d {
            Some(p) => (p.clone(), false),
            None => (fb.to_string(), true),
        };
        let (l, lf) = pick(&self.launcher, FALLBACK_LAUNCHER);
        let (k, kf) = pick(&self.keyboard, FALLBACK_KEYBOARD);
        let (i, i_f) = pick(&self.installer, FALLBACK_INSTALLER);
        let (p, pf) = pick(&self.permission_controller, FALLBACK_PERMISSIONS);
        [
            ("launcher", l, lf),
            ("keyboard", k, kf),
            ("package installer", i, i_f),
            ("permission controller", p, pf),
        ]
    }
}

/// Every package that debloat must leave alone.
pub fn keep_list(detected: &Detected) -> BTreeSet<String> {
    let mut keep: BTreeSet<String> = KEEP_FIXED.iter().map(|s| s.to_string()).collect();
    keep.extend(WEBVIEWS.iter().map(|s| s.to_string()));
    keep.extend(detected.webview.iter().cloned());
    keep.extend(detected.roles().into_iter().map(|(_, p, _)| p));
    keep
}

/// Keep-list packages that should be installed but are not. WebView counts as present when
/// either provider is (only one exists on an image).
pub fn missing_keep(keep: &BTreeSet<String>, installed: &BTreeSet<String>) -> Vec<String> {
    let webview_ok = WEBVIEWS.iter().any(|w| installed.contains(*w));
    let mut missing: Vec<String> = keep
        .iter()
        .filter(|p| !WEBVIEWS.contains(&p.as_str()))
        .filter(|p| !installed.contains(*p))
        .cloned()
        .collect();
    if !webview_ok {
        missing.push(WEBVIEWS.join(" or "));
    }
    missing
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebloatPlan {
    /// Listed and installed for user 0: to remove.
    pub remove: Vec<String>,
    /// Listed but on the keep-list: left alone, with a warning.
    pub kept: Vec<String>,
    /// Listed but not installed (already removed, or not on this image).
    pub absent: Vec<String>,
}

pub fn plan(list: &[String], keep: &BTreeSet<String>, installed: &BTreeSet<String>) -> DebloatPlan {
    let mut p = DebloatPlan::default();
    for pkg in list {
        if keep.contains(pkg) {
            p.kept.push(pkg.clone());
        } else if installed.contains(pkg) {
            p.remove.push(pkg.clone());
        } else {
            p.absent.push(pkg.clone());
        }
    }
    p
}

// ---------------------------------------------------------------------------------------------
// Parsing device output

/// `pm list packages` output ("package:<name>" lines) as a set.
pub fn parse_package_list(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .map(str::trim)
        .filter(|p| is_valid_package(p))
        .map(str::to_string)
        .collect()
}

/// The package of a component name ("pkg/.Activity" or "pkg/pkg.Activity"), taken from the
/// last non-empty line of `cmd package resolve-activity --brief` output. The system chooser
/// (`android/...ResolverActivity`, when several apps could handle the intent) is not an answer.
pub fn package_of_component(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let (pkg, _) = line.split_once('/')?;
    (pkg != "android" && is_valid_package(pkg)).then(|| pkg.to_string())
}

/// The component ("pkg/.Activity") from `cmd package resolve-activity --brief` output, if it
/// belongs to `pkg` (so the system chooser or "No activity found" are not answers).
pub fn component_of(output: &str, pkg: &str) -> Option<String> {
    let line = output.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let (p, activity) = line.split_once('/')?;
    let ok = p == pkg
        && !activity.is_empty()
        && activity
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._$".contains(c));
    ok.then(|| line.to_string())
}

/// A readable name for a package, for its tile. Android's shell has no command that prints app
/// labels (only resource IDs), so this is made from the package name: the last part that says
/// something ("com.YoStarEN.AzurLane" → "Azur Lane", "com.kurogame.wutheringwaves.global" →
/// "Wutheringwaves").
pub fn app_label(pkg: &str) -> String {
    const GENERIC: &[&str] = &[
        "android", "app", "apps", "game", "games", "client", "global", "mobile", "free", "full",
        "gp", "google", "play", "main", "release", "prod", "en", "us", "jp", "kr", "tw", "cn",
        "glb", "intl", "online", "aos",
    ];
    let segments: Vec<&str> = pkg.split('.').collect();
    let pick = segments
        .iter()
        .skip(1)
        .rev()
        .find(|s| s.len() > 1 && !GENERIC.contains(&s.to_ascii_lowercase().as_str()))
        .or(segments.last())
        .copied()
        .unwrap_or(pkg);
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in pick.chars() {
        let boundary = match prev {
            _ if c == '_' || c == '-' => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                prev = None;
                continue;
            }
            Some(p) => {
                (p.is_ascii_lowercase() && c.is_ascii_uppercase())
                    || (p.is_ascii_alphabetic() && c.is_ascii_digit())
                    || (p.is_ascii_digit() && c.is_ascii_alphabetic())
            }
            None => false,
        };
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c);
        prev = Some(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .iter()
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The package of `settings get secure default_input_method` ("pkg/.Service").
pub fn package_of_ime(output: &str) -> Option<String> {
    package_of_component(output)
}

/// The WebView provider from `dumpsys webviewupdate`
/// ("Current WebView package (name, version): (com.google.android.webview, 109.0...)").
pub fn webview_from_dumpsys(output: &str) -> Option<String> {
    let line = output
        .lines()
        .find(|l| l.contains("Current WebView package"))?;
    let inner = line.rsplit_once("): (")?.1;
    let pkg = inner.split(',').next()?.trim();
    is_valid_package(pkg).then(|| pkg.to_string())
}

/// The package of the top resumed activity in `dumpsys activity activities`
/// ("topResumedActivity=ActivityRecord{406dd7f u0 com.google.android.apps.nexuslauncher/...").
pub fn top_resumed_package(output: &str) -> Option<String> {
    let line = output
        .lines()
        .find(|l| l.contains("topResumedActivity=") || l.contains("mResumedActivity:"))?;
    line.split_whitespace()
        .find_map(|w| w.split_once('/').map(|(p, _)| p))
        .filter(|p| is_valid_package(p))
        .map(str::to_string)
}

// ---------------------------------------------------------------------------------------------
// Device commands (always `adb -s <serial>`, so a phone on USB is never touched)

pub struct Device<'a> {
    pub adb: &'a Adb,
    pub serial: &'a str,
}

impl Device<'_> {
    fn sh(&self, command: &str) -> Result<String> {
        self.adb.shell(self.serial, command, PM_TIMEOUT)
    }

    /// Packages installed for user 0.
    pub fn packages(&self) -> Result<BTreeSet<String>> {
        let out = self.sh("pm list packages --user 0")?;
        let set = parse_package_list(&out);
        if set.is_empty() {
            bail!("pm list packages returned nothing");
        }
        Ok(set)
    }

    pub fn detect(&self) -> Detected {
        let resolve = |args: &str| {
            self.sh(&format!("cmd package resolve-activity --brief {args}"))
                .ok()
                .and_then(|o| package_of_component(&o))
        };
        Detected {
            launcher: resolve("-a android.intent.action.MAIN -c android.intent.category.HOME"),
            keyboard: self
                .sh("settings get secure default_input_method")
                .ok()
                .and_then(|o| package_of_ime(&o)),
            installer: resolve(
                "-a android.intent.action.INSTALL_PACKAGE -d content://shard/a.apk \
                 -t application/vnd.android.package-archive",
            ),
            permission_controller: resolve("-a android.intent.action.MANAGE_PERMISSIONS"),
            webview: self
                .sh("dumpsys webviewupdate")
                .ok()
                .and_then(|o| webview_from_dumpsys(&o)),
        }
    }

    /// `pm uninstall --user 0 <pkg>`: removes the app for user 0 only (reversible).
    pub fn remove(&self, pkg: &str) -> Result<()> {
        if !is_valid_package(pkg) {
            bail!("\"{pkg}\" is not a package name");
        }
        let out = self
            .sh(&format!("pm uninstall --user 0 {pkg}"))
            .with_context(|| format!("removing {pkg}"))?;
        if !out.contains("Success") {
            bail!("removing {pkg}: {}", out.trim());
        }
        Ok(())
    }

    /// `cmd package install-existing --user 0 <pkg>`: brings back a removed system app.
    pub fn restore(&self, pkg: &str) -> Result<()> {
        if !is_valid_package(pkg) {
            bail!("\"{pkg}\" is not a package name");
        }
        let out = self
            .sh(&format!("cmd package install-existing --user 0 {pkg}"))
            .with_context(|| format!("restoring {pkg}"))?;
        if !out.contains("installed for user") {
            bail!("restoring {pkg}: {}", out.trim());
        }
        Ok(())
    }

    /// Animation scales 0.5 and stay-awake while charging; every value is read back.
    pub fn apply_tweaks(&self) -> Result<()> {
        let mut script = String::new();
        for (key, value) in TWEAKS {
            script.push_str(&format!("settings put global {key} {value}; "));
        }
        for (key, _) in TWEAKS {
            script.push_str(&format!("echo {key}=$(settings get global {key}); "));
        }
        let out = self.sh(&script).context("applying the tweaks")?;
        for (key, value) in TWEAKS {
            let want = format!("{key}={value}");
            if !out.lines().any(|l| l.trim() == want) {
                bail!("setting {key} to {value} did not stick: {}", out.trim());
            }
        }
        Ok(())
    }

    /// Turns Android's Bluetooth off; `bluetooth_on=0` persists across boots. The emulator runs
    /// without Bluetooth emulation (netsim is off), so with Bluetooth on the guest's stack finds
    /// no HAL on every cold boot and crashes about 7 times before Android gives up.
    pub fn disable_bluetooth(&self) -> Result<()> {
        let out = self
            .sh("cmd bluetooth_manager disable; echo bluetooth_on=$(settings get global bluetooth_on)")
            .context("turning Bluetooth off")?;
        if !out.lines().any(|l| l.trim() == "bluetooth_on=0") {
            bail!("turning Bluetooth off did not stick: {}", out.trim());
        }
        Ok(())
    }

    /// User-installed apps (`pm list packages -3`), sorted by their tile label.
    pub fn user_apps(&self) -> Result<Vec<String>> {
        let out = self.sh("pm list packages -3 --user 0")?;
        let mut apps: Vec<String> = parse_package_list(&out).into_iter().collect();
        apps.sort_by_key(|p| app_label(p).to_lowercase());
        Ok(apps)
    }

    /// Starts an app's launcher activity (`resolve-activity`, then `am start -n`), falling back
    /// to monkey with the LAUNCHER category.
    pub fn launch(&self, pkg: &str) -> Result<()> {
        if !is_valid_package(pkg) {
            bail!("\"{pkg}\" is not a package name");
        }
        let resolved = self
            .sh(&format!(
                "cmd package resolve-activity --brief -a android.intent.action.MAIN \
                 -c android.intent.category.LAUNCHER {pkg}"
            ))
            .ok()
            .and_then(|o| component_of(&o, pkg));
        if let Some(component) = resolved {
            let out = self.sh(&format!("am start -n {component}"))?;
            if !out.contains("Error") && !out.contains("Exception") {
                return Ok(());
            }
            tracing::warn!("am start -n {component}: {}", out.trim());
        }
        let out = self.sh(&format!(
            "monkey -p {pkg} -c android.intent.category.LAUNCHER 1"
        ))?;
        if out.contains("Events injected: 1") {
            Ok(())
        } else {
            bail!("could not start {pkg}: {}", out.trim())
        }
    }

    /// Opens the Play Store (the user signs in themselves).
    pub fn open_play_store(&self) -> Result<()> {
        let out = self
            .sh("am start -a android.intent.action.MAIN -c android.intent.category.APP_MARKET")?;
        if !out.contains("Error") && !out.contains("Exception") {
            return Ok(());
        }
        let out = self.sh(&format!(
            "monkey -p {PLAY_STORE} -c android.intent.category.LAUNCHER 1"
        ))?;
        if out.contains("Events injected: 1") {
            Ok(())
        } else {
            bail!("could not open the Play Store: {}", out.trim())
        }
    }

    /// Waits until `launcher` is the top resumed activity (boot_completed comes before the
    /// launcher has drawn). False on timeout or cancel.
    pub fn wait_for_launcher(
        &self,
        launcher: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout && !cancel.load(Ordering::Relaxed) {
            let top = self
                .adb
                .shell(
                    self.serial,
                    "dumpsys activity activities | grep -E 'topResumedActivity=|mResumedActivity:'",
                    Duration::from_secs(15),
                )
                .ok()
                .and_then(|o| top_resumed_package(&o));
            if top.as_deref() == Some(launcher) {
                return true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        false
    }
}

// ---------------------------------------------------------------------------------------------
// Debloat

#[derive(Debug, Clone, Default)]
pub struct DebloatReport {
    pub listed: usize,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    pub absent: Vec<String>,
    pub failed: Vec<String>,
    /// When the last package was removed (see [`wait_for_package_save`]).
    pub last_removal: Option<Instant>,
}

/// Android saves a `pm uninstall --user 0` to disk about 10 s after the change. Shard's stop
/// ends Android without a shutdown, so a stop within that time loses the removal whenever the
/// next start is a cold boot (measured: an uninstall followed by an immediate stop came back;
/// after 12 s it stuck). `install-existing` is saved at once.
pub const PACKAGE_SAVE_DELAY: Duration = Duration::from_secs(15);

/// How long until a package change made at `since` is on disk.
pub fn package_save_remaining(since: Option<Instant>) -> Duration {
    since.map_or(Duration::ZERO, |t| {
        PACKAGE_SAVE_DELAY.saturating_sub(t.elapsed())
    })
}

/// Waits until a package change made at `since` is on disk. False if cancelled.
pub fn wait_for_package_save(since: Option<Instant>, cancel: &AtomicBool) -> bool {
    while !package_save_remaining(since).is_zero() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    true
}

impl DebloatReport {
    pub fn summary(&self) -> String {
        format!(
            "{} listed: removed {}, already not installed {}, kept (keep-list) {}, failed {}",
            self.listed,
            self.removed.len(),
            self.absent.len(),
            self.kept.len(),
            self.failed.len()
        )
    }
}

/// Removes every installed debloat.txt package that is not on the keep-list.
pub fn debloat(
    paths: &Paths,
    dev: &Device,
    cancel: &AtomicBool,
    log: &mut dyn FnMut(String),
    warn: &mut dyn FnMut(String),
) -> Result<DebloatReport> {
    let list = load_debloat(paths)?;
    for p in &list.problems {
        warn(format!("debloat.txt {p}"));
    }
    let detected = dev.detect();
    for (role, pkg, fallback) in detected.roles() {
        if fallback {
            warn(format!(
                "could not detect the {role}; keeping {pkg} (the image's default)"
            ));
        } else {
            log(format!("{role}: {pkg} (kept)"));
        }
    }
    let keep = keep_list(&detected);
    let installed = dev.packages()?;
    let plan = plan(&list.packages, &keep, &installed);
    for k in &plan.kept {
        warn(format!(
            "debloat.txt lists {k}, which Shard always keeps; left installed"
        ));
    }
    let mut report = DebloatReport {
        listed: list.packages.len(),
        kept: plan.kept,
        absent: plan.absent,
        ..Default::default()
    };
    for pkg in plan.remove {
        if cancel.load(Ordering::Relaxed) {
            bail!("debloat cancelled");
        }
        match dev.remove(&pkg) {
            Ok(()) => {
                log(format!("removed {pkg}"));
                report.removed.push(pkg);
                report.last_removal = Some(Instant::now());
            }
            Err(e) => {
                warn(format!("{e:#}"));
                report.failed.push(pkg);
            }
        }
    }
    Ok(report)
}

/// Saves `pm list packages` (all, then system and user-installed counts) to
/// `logs\packages-initial.txt`.
pub fn initial_packages_file(paths: &Paths) -> PathBuf {
    paths.logs.join(PACKAGES_INITIAL)
}

pub fn save_initial_packages(paths: &Paths, dev: &Device) -> Result<PathBuf> {
    let all = dev.sh("pm list packages")?;
    let third = dev.sh("pm list packages -3").unwrap_or_default();
    let mut names: Vec<&str> = all
        .lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .collect();
    names.sort_unstable();
    let mut text = format!(
        "# pm list packages on the device's first boot ({}); {} packages, {} user-installed\r\n",
        crate::logging::stamp(),
        names.len(),
        parse_package_list(&third).len()
    );
    for n in names {
        text.push_str(n);
        text.push_str("\r\n");
    }
    let file = initial_packages_file(paths);
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

// ---------------------------------------------------------------------------------------------
// Markers

/// `state\device.toml`: what has been done to the current device's data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Markers {
    /// When the first-boot debloat ran (local time stamp).
    pub debloated: Option<String>,
    /// When the tweaks were applied.
    pub tweaked: Option<String>,
    /// When Android's Bluetooth was turned off (added after milestone 2; a device set up
    /// before that gets only this step on its next start).
    pub bluetooth_off: Option<String>,
}

fn markers_file(paths: &Paths) -> PathBuf {
    paths.state.join("device.toml")
}

fn wipe_file(paths: &Paths) -> PathBuf {
    paths.state.join("wipe-pending")
}

impl Markers {
    /// The markers (none while a wipe is pending).
    pub fn load(paths: &Paths) -> Self {
        if wipe_pending(paths) {
            return Self::default();
        }
        std::fs::read_to_string(markers_file(paths))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string(self).context("serialising device markers")?;
        std::fs::write(markers_file(paths), text).context("writing state\\device.toml")
    }

    pub fn first_boot_done(&self) -> bool {
        self.debloated.is_some() && self.tweaked.is_some() && self.bluetooth_off.is_some()
    }

    /// Debloat and tweaks are done, only the Bluetooth step is missing.
    pub fn only_bluetooth_missing(&self) -> bool {
        self.debloated.is_some() && self.tweaked.is_some() && self.bluetooth_off.is_none()
    }

    pub fn clear(paths: &Paths) -> Result<()> {
        match std::fs::remove_file(markers_file(paths)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).context("removing state\\device.toml")
            }
            _ => Ok(()),
        }
    }
}

/// Called before the emulator starts: if the device has no user data yet (a new or rebuilt
/// AVD, or a wipe is pending), the markers of an earlier device are dropped. Returns whether
/// this start creates fresh user data. (Checked before launch: the emulator creates
/// `userdata-qemu.img` within seconds of starting.)
pub fn prepare_fresh_start(paths: &Paths) -> Result<bool> {
    let fresh = !has_user_data(paths) || wipe_pending(paths);
    if fresh {
        Markers::clear(paths)?;
    }
    Ok(fresh)
}

/// Whether the emulator has created the device's user data partition yet.
pub fn has_user_data(paths: &Paths) -> bool {
    std::fs::read_dir(paths.avd_dir())
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("userdata-qemu.img")
            })
        })
        .unwrap_or(false)
}

pub fn wipe_pending(paths: &Paths) -> bool {
    wipe_file(paths).is_file()
}

/// Wipe data: the next boot starts the device from the image's clean user data
/// (`-wipe-data`), and the first-boot work runs again.
pub fn request_wipe(paths: &Paths) -> Result<()> {
    Markers::clear(paths)?;
    std::fs::write(wipe_file(paths), crate::logging::stamp()).context("writing state\\wipe-pending")
}

pub fn wipe_done(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(wipe_file(paths)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("removing state\\wipe-pending")
        }
        _ => Ok(()),
    }
}
