// A window app: no console flashes up when Shard.exe is double-clicked. `--headless` attaches to
// the caller's console (or keeps redirected output) instead.
#![windows_subsystem = "windows"]

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use shard::headless::{self, HeadlessCmd};
use shard::paths::Paths;

#[derive(Parser)]
#[command(
    name = "Shard",
    version,
    about = "Portable launcher for Google's Android Emulator"
)]
struct Cli {
    /// Run without a window (needs a command).
    #[arg(long, global = true)]
    headless: bool,
    /// Keep downloaded SDK archives in DIR and reuse them (development).
    #[arg(long, global = true, value_name = "DIR")]
    dev_cache: Option<PathBuf>,
    /// Use DIR as the Shard folder instead of the exe's folder (development/testing only).
    #[arg(long, global = true, value_name = "DIR", hide = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Download the emulator, platform-tools and system image; create the AVD.
    Setup,
    /// Start the emulator and wait until Android has booted (it keeps running).
    Boot,
    /// Remove the apps listed in debloat.txt (starts the emulator if needed; it keeps running).
    Debloat,
    /// Reset the device's data on its next start (the emulator must be stopped).
    Wipe,
    /// Run the self-checks and print PASS/FAIL per check.
    Verify,
    /// Stop the emulator and the adb server.
    Stop,
    /// Start an app by package name (starts the emulator if needed; it keeps running).
    #[command(hide = true)]
    Launch { package: String },
}

fn main() {
    // Anything on the command line may need to print (results, --help, a parse error).
    if std::env::args_os().len() > 1 {
        shard::winutil::attach_console();
        shard::winutil::std_handles_not_inherited();
    }
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    let paths = match Paths::resolve(cli.root.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };
    let code = match (cli.headless, cli.command) {
        (_, Some(cmd)) => headless::run(
            &paths,
            match cmd {
                Cmd::Setup => HeadlessCmd::Setup,
                Cmd::Boot => HeadlessCmd::Boot,
                Cmd::Debloat => HeadlessCmd::Debloat,
                Cmd::Wipe => HeadlessCmd::Wipe,
                Cmd::Verify => HeadlessCmd::Verify,
                Cmd::Stop => HeadlessCmd::Stop,
                Cmd::Launch { package } => HeadlessCmd::Launch(package),
            },
            cli.dev_cache,
        ),
        (true, None) => {
            eprintln!("--headless needs a command: setup, boot, debloat, wipe, verify or stop");
            2
        }
        (false, None) => match shard::ui::run(paths) {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("{e:#}");
                1
            }
        },
    };
    std::process::exit(code);
}
