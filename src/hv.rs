//! Virtualization checks: Windows Hypervisor Platform (WHPX) and `emulator -accel-check`.

use std::ffi::c_void;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::paths::Paths;
use crate::procs;
use crate::winutil::wide;

pub const ENABLE_WHPX_HELP: &str = "Turn on \"Windows Hypervisor Platform\": Start > \"Turn Windows features on or off\" > tick \"Windows Hypervisor Platform\" > OK, then restart Windows. Also make sure virtualization (SVM/VT-x) is enabled in the BIOS/UEFI. Shard never changes Windows features itself.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Whpx {
    Present,
    /// The API exists but reports no hypervisor (feature off, or virtualization off in firmware).
    NotPresent,
    /// WinHvPlatform.dll is missing: the Windows feature is not installed.
    FeatureOff,
    Error(String),
}

impl Whpx {
    pub fn usable(&self) -> bool {
        *self == Whpx::Present
    }
    pub fn describe(&self) -> String {
        match self {
            Whpx::Present => "Windows Hypervisor Platform is on".into(),
            Whpx::NotPresent => format!("No hypervisor is running. {ENABLE_WHPX_HELP}"),
            Whpx::FeatureOff => format!(
                "Windows Hypervisor Platform is not installed (WinHvPlatform.dll missing). {ENABLE_WHPX_HELP}"
            ),
            Whpx::Error(e) => format!("Could not query the hypervisor: {e}"),
        }
    }
}

/// `WHvGetCapability(WHvCapabilityCodeHypervisorPresent)`, with the DLL loaded at runtime so
/// Shard.exe still starts on machines without the feature.
pub fn whpx() -> Whpx {
    type WhvGetCapability =
        unsafe extern "system" fn(code: i32, buf: *mut c_void, size: u32, written: *mut u32) -> i32;
    const WHV_CAPABILITY_CODE_HYPERVISOR_PRESENT: i32 = 0;
    let dll = wide("WinHvPlatform.dll");
    // SAFETY: loading a system DLL by name and looking up a documented export.
    unsafe {
        let module = LoadLibraryW(dll.as_ptr());
        if module.is_null() {
            return Whpx::FeatureOff;
        }
        let Some(proc_addr) = GetProcAddress(module, c"WHvGetCapability".as_ptr().cast()) else {
            return Whpx::Error("WHvGetCapability not exported".into());
        };
        let get: WhvGetCapability = std::mem::transmute(proc_addr);
        let mut present: i32 = 0;
        let mut written = 0u32;
        let hr = get(
            WHV_CAPABILITY_CODE_HYPERVISOR_PRESENT,
            &mut present as *mut i32 as *mut c_void,
            std::mem::size_of::<i32>() as u32,
            &mut written,
        );
        if hr < 0 {
            return Whpx::Error(format!("WHvGetCapability failed (HRESULT {hr:#010x})"));
        }
        if present != 0 {
            Whpx::Present
        } else {
            Whpx::NotPresent
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccelCheck {
    pub usable: bool,
    pub message: String,
}

/// Parses `emulator -accel-check` output:
/// ```text
/// accel:
/// 0
/// WHPX(10.0.26100) is installed and usable.
/// accel
/// ```
/// The number is the status code; 0 means usable.
pub fn parse_accel_check(output: &str) -> AccelCheck {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.iter().position(|l| *l == "accel:");
    let (code, message) = match start {
        Some(i) => {
            let code = lines.get(i + 1).and_then(|l| l.parse::<i32>().ok());
            let msg: Vec<&str> = lines
                .iter()
                .skip(i + 2)
                .take_while(|l| **l != "accel")
                .copied()
                .collect();
            (code, msg.join(" "))
        }
        None => (None, lines.join(" ")),
    };
    AccelCheck {
        usable: code == Some(0),
        message: if message.is_empty() {
            "emulator -accel-check gave no result".into()
        } else {
            message
        },
    }
}

pub fn accel_check(paths: &Paths) -> Result<AccelCheck> {
    let mut cmd = Command::new(paths.emulator_exe());
    cmd.arg("-accel-check");
    paths.apply_child_env(&mut cmd);
    let out = procs::run(&mut cmd, Duration::from_secs(60))?;
    Ok(parse_accel_check(&out.combined()))
}
