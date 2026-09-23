//! Project Launcher.
//!
//! Exit codes:
//!   0  all enabled actions were handed to Windows successfully, a profile
//!      run request was forwarded to the resident, the resident exited
//!      normally, or a resident was already running
//!   1  one or more actions failed (partial or total profile failure)
//!   2  usage error, config error, resident/IPC failure, or shortcut error

// GUI subsystem: double-clicking the exe must not open a console window.
// Output (println!/eprintln!) still reaches the terminal when launched from
// cmd/PowerShell, because the parent's std handles are inherited regardless
// of subsystem. Test builds keep the console subsystem for the test harness.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod actions;
mod args;
mod autostart;
mod config;
mod gui;
mod ipc;
mod profile;
mod resident;
mod shortcut;

use args::{Mode, ShortcutLocation};
use profile::ActionStatus;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mode = match args::parse(std::env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match mode {
        Mode::Gui | Mode::Settings => run_gui(),
        Mode::Resident => run_resident(),
        Mode::Profile(id) => run_profile(&id),
        Mode::Shortcut { id, location } => run_shortcut(&id, location),
    }
}

fn run_gui() -> ExitCode {
    match gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Runs the resident until its window is closed. A second instance is not an
/// error: the desired state (one resident running) already holds.
fn run_resident() -> ExitCode {
    match resident::run() {
        Ok(()) => {
            println!("Resident stopped.");
            ExitCode::SUCCESS
        }
        Err(resident::ResidentError::AlreadyRunning) => {
            println!("A Project Launcher resident is already running.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Creates .lnk shortcuts for a profile. All requested locations are
/// attempted so one failure still leaves the other location written.
fn run_shortcut(id: &str, location: ShortcutLocation) -> ExitCode {
    let config = match config::Config::load_default() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let Some(profile) = profile::find_profile(&config, id) else {
        eprintln!("error: no profile with id {id:?} in the config");
        return ExitCode::from(2);
    };

    let locations: &[shortcut::ShortcutLocation] = match location {
        ShortcutLocation::Desktop => &[shortcut::ShortcutLocation::Desktop],
        ShortcutLocation::StartMenu => &[shortcut::ShortcutLocation::StartMenu],
        ShortcutLocation::Both => &[
            shortcut::ShortcutLocation::Desktop,
            shortcut::ShortcutLocation::StartMenu,
        ],
    };

    let mut failed = false;
    for &loc in locations {
        match shortcut::create(&profile.name, &profile.id, loc) {
            Ok(path) => println!("Created {}", path.display()),
            Err(e) => {
                eprintln!("error: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

fn run_profile(id: &str) -> ExitCode {
    // If a resident is running it owns profile execution: forward the
    // request and exit. No resident means this process runs the profile
    // directly (headless use stays fully supported).
    #[cfg(windows)]
    match ipc::forward_run_profile(id) {
        Ok(ipc::ForwardOutcome::Delivered) => {
            println!("Forwarded the run request for profile {id} to the resident.");
            return ExitCode::SUCCESS;
        }
        Ok(ipc::ForwardOutcome::Rejected) => {
            eprintln!(
                "error: the resident rejected the request for profile {id} \
                 (unknown profile id, or blocked by UIPI integrity levels)"
            );
            return ExitCode::from(2);
        }
        Ok(ipc::ForwardOutcome::NoResident) => {}
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    }
    run_profile_direct(id)
}

fn run_profile_direct(id: &str) -> ExitCode {
    let config = match config::Config::load_default() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let profile = match profile::find_profile(&config, id) {
        Some(profile) => profile,
        None => {
            eprintln!("error: no profile with id {id:?} in the config");
            return ExitCode::from(2);
        }
    };

    println!("Running profile '{}' ({})", profile.name, profile.id);
    let result = profile::execute_profile(profile, &mut |action| {
        actions::execute(action).map_err(|e| e.to_string())
    });

    for outcome in &result.outcomes {
        match &outcome.status {
            ActionStatus::Succeeded => println!("  [ok]     {}", outcome.action_id),
            ActionStatus::Failed(e) => println!("  [FAILED] {}: {e}", outcome.action_id),
        }
    }
    if result.skipped_disabled > 0 {
        println!("  ({} disabled action(s) skipped)", result.skipped_disabled);
    }

    if result.is_success() {
        println!(
            "Profile finished: all {} action(s) succeeded.",
            result.outcomes.len()
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "Profile finished: {} of {} action(s) failed.",
            result.failure_count(),
            result.outcomes.len()
        );
        ExitCode::from(1)
    }
}
