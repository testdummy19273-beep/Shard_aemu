//! Shard: a portable launcher for Google's Android Emulator. Everything lives in the folder
//! that holds Shard.exe; see README.md for the few documented exceptions.

pub mod adb;
pub mod avd;
pub mod checklist;
pub mod config;
pub mod control;
pub mod device;
pub mod emulator;
pub mod headless;
pub mod hv;
pub mod instance;
pub mod jobs;
pub mod leak;
pub mod logging;
pub mod paths;
pub mod power;
pub mod preflight;
pub mod procs;
pub mod profile;
pub mod sdk;
pub mod state;
pub mod ui;
pub mod verify;
pub mod winutil;
