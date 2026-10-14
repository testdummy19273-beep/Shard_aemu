//! Config, state machine, AVD files, ports and log/output parsing. No emulator needed.

use std::path::{Path, PathBuf};

use shard::config::{Config, EmulatorConfig};
use shard::emulator::{self, BootMode};
use shard::paths::Paths;
use shard::state::{AppState, Input};
use shard::{adb, avd, hv, sdk};

fn scratch(name: &str) -> PathBuf {
    // Inside target\, never %TEMP%.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- config ---------------------------------------------------------------------------------

#[test]
fn config_defaults_match_the_brief() {
    let c = Config::default();
    let e = &c.emulator;
    assert_eq!(
        (e.ram_mb, e.cores, e.width, e.height, e.dpi),
        (4096, 4, 1920, 1080, 320)
    );
    assert_eq!(e.gpu, "host");
    assert!(e.quick_boot);
    assert_eq!(e.port, 5554);
    assert!(!c.performance.high_priority);
    assert!(!c.performance.power_plan_switch);
    assert!(c.ui.dock_sidebar);
    c.validate().unwrap();
    assert_eq!(Config::parse(&c.to_toml()).unwrap(), c);
}

#[test]
fn partial_config_fills_defaults() {
    let c = Config::parse("[emulator]\nram_mb = 6144\n").unwrap();
    assert_eq!(c.emulator.ram_mb, 6144);
    assert_eq!(c.emulator.cores, 4);
    assert!(c.ui.dock_sidebar);
}

#[test]
fn config_validation_lists_every_problem() {
    let err = Config::parse(
        "[emulator]\nram_mb = 1024\ncores = 9\nwidth = 720\nheight = 1280\ngpu = \"vulkan\"\nport = 5555\n",
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    for want in ["ram_mb", "cores", "landscape", "gpu", "port"] {
        assert!(msg.contains(want), "{want} missing in: {msg}");
    }
    assert!(Config::parse("[emulator]\nram_mb = 8192\ncores = 1\n").is_ok());
    assert!(Config::parse("[emulator]\nram_mb = 8193\n").is_err());
    assert!(Config::parse("not toml = = =").is_err());
}

#[test]
fn missing_config_is_created_with_defaults() {
    let dir = scratch("config-create");
    let file = dir.join("shard.toml");
    let c = Config::load_or_create(&file).unwrap();
    assert_eq!(c, Config::default());
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("ram_mb = 4096"));
}

// --- state machine ----------------------------------------------------------------------------

#[test]
fn normal_lifecycle() {
    let mut s = AppState::NotSetUp;
    for (input, want) in [
        (Input::SetupStarted, AppState::SettingUp),
        (Input::SetupDone, AppState::Stopped),
        (Input::BootStarted, AppState::Booting),
        (Input::Booted, AppState::Running),
        (Input::StopStarted, AppState::Stopping),
        (Input::Stopped, AppState::Stopped),
    ] {
        s = s.apply(&input).unwrap();
        assert_eq!(s, want);
    }
}

#[test]
fn invalid_transitions_are_rejected() {
    assert!(AppState::NotSetUp.apply(&Input::BootStarted).is_err());
    assert!(AppState::Running.apply(&Input::SetupStarted).is_err());
    assert!(AppState::SettingUp.apply(&Input::BootStarted).is_err());
    assert!(AppState::Stopped.apply(&Input::Booted).is_err());
}

#[test]
fn failures_cancels_crashes_and_adoption() {
    assert!(matches!(
        AppState::Booting.apply(&Input::Failed("x".into())),
        Ok(AppState::Error(_))
    ));
    assert_eq!(
        AppState::SettingUp
            .apply(&Input::SetupAborted { set_up: false })
            .unwrap(),
        AppState::NotSetUp
    );
    assert_eq!(
        AppState::SettingUp
            .apply(&Input::SetupAborted { set_up: true })
            .unwrap(),
        AppState::Stopped
    );
    // Window closed by the user while running.
    assert_eq!(
        AppState::Running.apply(&Input::Stopped).unwrap(),
        AppState::Stopped
    );
    assert_eq!(
        AppState::Stopped.apply(&Input::Adopted).unwrap(),
        AppState::Running
    );
    // Retry from an error.
    assert_eq!(
        AppState::Error("x".into())
            .apply(&Input::BootStarted)
            .unwrap(),
        AppState::Booting
    );
}

// --- AVD files and paths --------------------------------------------------------------------

#[test]
fn shard_ini_uses_the_current_root() {
    let p = Paths::from_root(r"D:\Games\Shard Moved ü");
    let ini = avd::shard_ini(&p);
    assert_eq!(
        ini,
        "avd.ini.encoding=UTF-8\r\npath=D:\\Games\\Shard Moved ü\\avd\\shard.avd\r\npath.rel=avd\\shard.avd\r\ntarget=android-33\r\n"
    );
}

#[test]
fn config_ini_reflects_settings() {
    let cfg = EmulatorConfig {
        ram_mb: 6144,
        cores: 6,
        width: 1280,
        height: 720,
        dpi: 240,
        ..EmulatorConfig::default()
    };
    let ini = avd::config_ini(&cfg);
    for line in [
        "hw.ramSize=6144",
        "hw.cpu.ncore=6",
        "hw.lcd.width=1280",
        "hw.lcd.height=720",
        "hw.lcd.density=240",
        "hw.gpu.mode=host",
        "hw.initialOrientation=landscape",
        "PlayStore.enabled=true",
        "image.sysdir.1=system-images\\android-33\\google_apis_playstore\\x86_64\\",
    ] {
        assert!(ini.lines().any(|l| l == line), "{line} missing");
    }
    assert_ne!(
        avd::hardware_hash(&cfg),
        avd::hardware_hash(&EmulatorConfig::default())
    );
    // quick_boot and port do not invalidate a snapshot.
    let other = EmulatorConfig {
        quick_boot: false,
        port: 5556,
        ..cfg.clone()
    };
    assert_eq!(avd::hardware_hash(&cfg), avd::hardware_hash(&other));
}

#[test]
fn path_is_rewritten_after_a_folder_move() {
    let base = scratch("avd-move");
    let first = base.join("Shard Dev");
    let moved = base.join("Shard Moved ü");
    let p = Paths::from_root(&first);
    let cfg = EmulatorConfig::default();
    assert!(avd::write(&p, &cfg).unwrap());
    assert!(
        !avd::write(&p, &cfg).unwrap(),
        "unchanged config.ini rewritten"
    );
    std::fs::rename(&first, &moved).unwrap();
    let m = Paths::from_root(&moved);
    assert!(!avd::write(&m, &cfg).unwrap());
    let ini = std::fs::read_to_string(m.avd_ini()).unwrap();
    assert!(ini.contains(&format!("path={}\r\n", m.avd_dir().display())));
    assert!(!ini.contains("Shard Dev"));
}

#[test]
fn child_env_points_everything_into_the_folder() {
    let p = Paths::from_root(r"D:\Shard");
    let env: std::collections::HashMap<String, String> = p
        .child_env()
        .into_iter()
        .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
        .collect();
    assert_eq!(env["ANDROID_SDK_ROOT"], r"D:\Shard\sdk");
    assert_eq!(env["ANDROID_HOME"], r"D:\Shard\sdk");
    assert_eq!(env["ANDROID_AVD_HOME"], r"D:\Shard\avd");
    assert_eq!(env["ANDROID_USER_HOME"], r"D:\Shard\home");
    assert_eq!(env["ANDROID_EMULATOR_HOME"], r"D:\Shard\home");
    assert_eq!(env["TEMP"], r"D:\Shard\tmp");
    assert_eq!(env["TMP"], r"D:\Shard\tmp");
    assert_eq!(env["ANDROID_ADB_SERVER_PORT"], "5038");
    assert_eq!(env["ADB_MDNS"], "0");
}

#[test]
fn emulator_args() {
    let p = Paths::from_root(r"D:\Shard");
    let cfg = EmulatorConfig::default();
    let a: Vec<String> = emulator::args(&p, &cfg, 5556, BootMode::Cold)
        .into_iter()
        .map(|s| s.into_string().unwrap())
        .collect();
    let joined = a.join(" ");
    assert!(joined.starts_with("-avd shard -gpu host -no-boot-anim -port 5556"));
    assert!(joined.contains("-no-metrics"));
    assert!(joined.contains(r"-save-path D:\Shard\logs"));
    assert!(a.last().unwrap() == "-no-snapshot-load");
    let q: Vec<String> = emulator::args(&p, &cfg, 5554, BootMode::Quick)
        .into_iter()
        .map(|s| s.into_string().unwrap())
        .collect();
    assert!(!q.iter().any(|s| s.starts_with("-no-snapshot")));
    let args = |cfg: &EmulatorConfig| -> Vec<String> {
        emulator::args(&p, cfg, 5554, BootMode::Wipe)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect()
    };
    // Wipe data never loads the old snapshot; with Quick Boot off it saves none either.
    assert!(args(&cfg).ends_with(&["-wipe-data".into(), "-no-snapshot-load".into()]));
    let no_qb = EmulatorConfig {
        quick_boot: false,
        ..cfg.clone()
    };
    assert!(args(&no_qb).ends_with(&["-wipe-data".into(), "-no-snapshot".into()]));
}

#[test]
fn device_markers_follow_user_data_and_wipe() {
    use shard::device::{self, Markers};
    let root = scratch("device-markers");
    let p = Paths::from_root(&root);
    std::fs::create_dir_all(&p.state).unwrap();
    std::fs::create_dir_all(p.avd_dir()).unwrap();
    let done = Markers {
        debloated: Some("20261015-120000".into()),
        tweaked: Some("20261015-120001".into()),
    };
    let userdata = p.avd_dir().join("userdata-qemu.img.qcow2");
    // Markers left from an earlier device, AVD rebuilt (no user data). In boot order: the
    // check before launch, then the emulator creates user data, then the markers are read.
    done.save(&p).unwrap();
    assert!(device::prepare_fresh_start(&p).unwrap());
    std::fs::write(&userdata, b"").unwrap();
    assert!(
        !Markers::load(&p).first_boot_done(),
        "stale markers of a rebuilt device must not count"
    );
    // An existing device whose first boot was done.
    done.save(&p).unwrap();
    assert!(!device::prepare_fresh_start(&p).unwrap());
    assert!(Markers::load(&p).first_boot_done());
    // Wipe: markers gone at once and at the next start, until the first-boot work re-saves them.
    device::request_wipe(&p).unwrap();
    assert!(device::wipe_pending(&p));
    assert!(!Markers::load(&p).first_boot_done());
    assert!(device::prepare_fresh_start(&p).unwrap());
    device::wipe_done(&p).unwrap();
    assert!(!device::wipe_pending(&p));
    assert!(
        !Markers::load(&p).first_boot_done(),
        "a wipe clears the markers"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

// --- ports ------------------------------------------------------------------------------------

#[test]
fn port_selection() {
    assert_eq!(emulator::pick_port(5554, |_| true), Some(5554));
    // Odd start rounds up; console and adb port must both be free.
    assert_eq!(emulator::pick_port(5555, |_| true), Some(5556));
    assert_eq!(emulator::pick_port(5554, |p| p != 5555), Some(5556));
    assert_eq!(emulator::pick_port(5554, |p| p >= 5560), Some(5560));
    assert_eq!(emulator::pick_port(5554, |_| false), None);
    assert_eq!(emulator::pick_port(5584, |p| p != 5584), None);
    assert_eq!(emulator::serial_for(5556), "emulator-5556");
    let cmd: Vec<String> = ["qemu", "-avd", "shard", "-port", "5558"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(emulator::port_from_cmd(&cmd), Some(5558));
}

// --- parsing ----------------------------------------------------------------------------------

#[test]
fn accel_check_output() {
    let ok = hv::parse_accel_check("accel:\n0\nWHPX(10.0.26100) is installed and usable.\naccel\n");
    assert!(ok.usable);
    assert_eq!(ok.message, "WHPX(10.0.26100) is installed and usable.");
    let bad = hv::parse_accel_check(
        "accel:\n10\nWHPX is not installed or not enabled. Please enable it.\naccel\n",
    );
    assert!(!bad.usable);
    assert!(bad.message.contains("WHPX"));
    assert!(!hv::parse_accel_check("").usable);
}

#[test]
fn renderer_from_emulator_log() {
    let log = "INFO | Graphics backend: gfxstream\n\
INFO | Graphics Adapter Vendor Google (NVIDIA Corporation)\n\
INFO | Graphics Adapter Android Emulator OpenGL ES Translator (NVIDIA GeForce RTX 5070/PCIe/SSE2)\n\
INFO | Selecting Vulkan device: NVIDIA GeForce RTX 5070, Version: 1.4.312\n";
    assert_eq!(
        emulator::renderer_from_log(log).as_deref(),
        Some("NVIDIA GeForce RTX 5070/PCIe/SSE2")
    );
    assert_eq!(
        emulator::vulkan_device_from_log(log).as_deref(),
        Some("NVIDIA GeForce RTX 5070")
    );
    assert!(!emulator::is_software_renderer(
        "NVIDIA GeForce RTX 5070/PCIe/SSE2"
    ));
    assert!(emulator::is_software_renderer(
        "Google SwiftShader (Vulkan 1.3)"
    ));
    assert!(emulator::is_software_renderer(
        "llvmpipe (LLVM 15.0.7, 256 bits)"
    ));
    assert_eq!(emulator::renderer_from_log("nothing here"), None);
}

#[test]
fn renderer_from_surfaceflinger() {
    let dump = "Display 4619827259835644672 HWC layers:\n\
GLES: Google (NVIDIA Corporation), Android Emulator OpenGL ES Translator (NVIDIA GeForce RTX 5070/PCIe/SSE2), OpenGL ES 3.0 (4.5.0 NVIDIA 617.14)\n\
other";
    let line = emulator::gles_line(dump).unwrap();
    assert!(line.starts_with("GLES: Google"));
    assert_eq!(
        emulator::renderer_from_gles(&line).as_deref(),
        Some("NVIDIA GeForce RTX 5070/PCIe/SSE2")
    );
    let amd = "GLES: Google (ATI Technologies Inc.), Android Emulator OpenGL ES Translator (AMD Radeon(TM) Graphics), OpenGL ES 3.0 (4.6.0)";
    assert_eq!(
        emulator::renderer_from_gles(amd).as_deref(),
        Some("AMD Radeon(TM) Graphics")
    );
    assert_eq!(emulator::gles_line("no gles here"), None);
}

#[test]
fn firewall_rules_inside_the_folder_are_found() {
    let root = Path::new(r"D:\Games\Shard Dev");
    // As Windows stores the "Query User" rule it adds when adb listens on the network.
    let rule = r"v2.33|Action=Block|Active=TRUE|Dir=In|Protocol=17|Profile=Private|Profile=Public|App=D:\Games\shard dev\sdk\platform-tools\adb.exe|Name=adb|Desc=adb|Defer=User|";
    assert_eq!(
        shard::winutil::describe_firewall_rule(rule, root).as_deref(),
        Some(r"adb: Block In UDP (D:\Games\shard dev\sdk\platform-tools\adb.exe)")
    );
    let elsewhere = r"v2.33|Action=Allow|Dir=In|Protocol=6|App=C:\tools\scrcpy\adb.exe|Name=adb|";
    assert_eq!(
        shard::winutil::describe_firewall_rule(elsewhere, root),
        None
    );
    let sibling = r"v2.33|Action=Block|Dir=In|App=D:\Games\Shard Dev2\adb.exe|";
    assert_eq!(shard::winutil::describe_firewall_rule(sibling, root), None);
    assert_eq!(
        shard::winutil::describe_firewall_rule("v2.33|Action=Block|Dir=In|", root),
        None
    );
}

#[test]
fn bound_sockets_include_a_test_listener() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let pid = std::process::id();
    let found = shard::winutil::bound_sockets()
        .into_iter()
        .any(|s| s.pid == pid && !s.udp && s.addr.port() == port && s.addr.ip().is_loopback());
    assert!(found, "listener on 127.0.0.1:{port} not reported");
}

#[test]
fn update_check_key_matches_what_the_emulator_writes() {
    assert_eq!(
        emulator::update_check_key("37.2.12.0"),
        "_version.3D37.2.12.0.26coreVersion.3Dqemu2.25202.12.0"
    );
}

#[test]
fn adb_devices_output() {
    let text = "* daemon started successfully\nList of devices attached\nemulator-5554\tdevice\nemulator-5556\toffline\n\n";
    assert_eq!(
        adb::parse_devices(text),
        vec![
            ("emulator-5554".to_string(), "device".to_string()),
            ("emulator-5556".to_string(), "offline".to_string())
        ]
    );
    assert!(adb::parse_devices("List of devices attached\n\n").is_empty());
}

#[test]
fn archive_top_folder_is_stripped() {
    use sdk::install::strip_top;
    assert_eq!(
        strip_top(Path::new("emulator/lib64/x.dll")),
        Some(PathBuf::from("lib64/x.dll"))
    );
    assert_eq!(strip_top(Path::new("emulator/")), None);
    assert_eq!(strip_top(Path::new("x86_64")), None);
}

#[test]
fn screenshot_brightness() {
    fn png(w: u32, h: u32, lit: usize) -> Vec<u8> {
        let mut data = vec![[0u8, 0, 0, 255]; (w * h) as usize];
        for px in data.iter_mut().take(lit) {
            *px = [200, 220, 255, 255];
        }
        let data: Vec<u8> = data.concat();
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&data).unwrap();
        }
        out
    }
    let (f, w, h) = shard::verify::lit_fraction(&png(100, 50, 0)).unwrap();
    assert_eq!((f, w, h), (0.0, 100, 50));
    let (f, _, _) = shard::verify::lit_fraction(&png(100, 50, 2500)).unwrap();
    assert!((f - 0.5).abs() < 1e-9);
    assert!(shard::verify::lit_fraction(b"not a png").is_err());
}
