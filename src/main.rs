//! StillLaunch — run groups of apps, folders, files, URLs and commands from
//! saved profiles (Windows).
//!
//! Exit codes: 0 success, 1 some steps failed, 2 usage/config/system error.

// No console window on double-click; output still reaches a parent console.
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![cfg_attr(not(windows), allow(dead_code))]

mod config;
mod json;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod resident;
#[cfg(windows)]
mod run;
#[cfg(windows)]
mod win;

use std::process::ExitCode;

const USAGE: &str = "Usage:
  ProjectLauncher.exe                         open the editor (also: --settings)
  ProjectLauncher.exe --resident              start the tray resident
  ProjectLauncher.exe --profile <GUID>        run a profile (via the resident if running)
  ProjectLauncher.exe --shortcut <GUID> [desktop|startmenu|both]";

#[derive(Debug, PartialEq)]
enum Mode {
    Editor,
    Resident,
    Profile(String),
    /// (GUID, desktop, start menu)
    Shortcut(String, bool, bool),
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut it = args.iter().map(String::as_str);
    let mode = match it.next() {
        None | Some("--settings") => Mode::Editor,
        Some("--resident") => Mode::Resident,
        Some(a) if flag(a, "--profile") => Mode::Profile(guid_value(a, "--profile", &mut it)?),
        Some(a) if flag(a, "--shortcut") => {
            let id = guid_value(a, "--shortcut", &mut it)?;
            match it.next() {
                None | Some("desktop") => Mode::Shortcut(id, true, false),
                Some("startmenu") => Mode::Shortcut(id, false, true),
                Some("both") => Mode::Shortcut(id, true, true),
                Some(x) => return Err(format!("unknown shortcut location {x:?}")),
            }
        }
        Some(a) => return Err(format!("unknown argument {a:?}")),
    };
    match it.next() {
        Some(extra) => Err(format!("unexpected argument {extra:?}")),
        None => Ok(mode),
    }
}

/// `--name` or `--name=...`
fn flag(arg: &str, name: &str) -> bool {
    arg == name || arg.strip_prefix(name).is_some_and(|r| r.starts_with('='))
}

fn guid_value<'a>(arg: &'a str, name: &str, it: &mut impl Iterator<Item = &'a str>) -> Result<String, String> {
    let v = match arg.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
        Some(v) => v,
        None => it.next().unwrap_or(""),
    };
    if config::is_guid(v) {
        Ok(v.to_string())
    } else {
        Err(format!("{name} expects a GUID like {{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}}, got {v:?}"))
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("StillLaunch runs on Windows only.");
    ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
    let args: Result<Vec<String>, _> = std::env::args_os().skip(1).map(|a| a.into_string()).collect();
    let mode = match args.map_err(|_| "arguments must be valid Unicode".to_string()).and_then(|a| parse_args(&a)) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match mode {
        Mode::Editor => gui::run().map(|()| 0),
        Mode::Resident => resident::run().map(|started| {
            if !started {
                println!("A resident is already running.");
            }
            0
        }),
        Mode::Profile(id) => run_profile(&id),
        Mode::Shortcut(id, desktop, start_menu) => make_shortcuts(&id, desktop, start_menu),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Hands the request to the resident when one is running, otherwise runs
/// the profile in this process.
#[cfg(windows)]
fn run_profile(id: &str) -> Result<u8, String> {
    match resident::forward(id) {
        resident::Forward::Accepted => {
            println!("Sent to the resident.");
            return Ok(0);
        }
        resident::Forward::Rejected => {
            return Err("the resident rejected the request (unknown profile, busy, or a different user/integrity level)".into());
        }
        resident::Forward::NoResident => {}
    }
    let cfg = config::load_default()?;
    let p = cfg.find(id).ok_or_else(|| format!("no profile with id {id}"))?;
    let report = run::run_profile(p);
    println!("{}", run::summary(p, &report));
    Ok(if report.iter().all(|(_, r)| r.is_ok()) { 0 } else { 1 })
}

#[cfg(windows)]
fn make_shortcuts(id: &str, desktop: bool, start_menu: bool) -> Result<u8, String> {
    let cfg = config::load_default()?;
    let p = cfg.find(id).ok_or_else(|| format!("no profile with id {id}"))?;
    let mut failed = false;
    for (wanted, in_start_menu) in [(desktop, false), (start_menu, true)] {
        if wanted {
            match win::create_shortcut(p, in_start_menu) {
                Ok(path) => println!("Created {}", path.display()),
                Err(e) => {
                    eprintln!("error: {e}");
                    failed = true;
                }
            }
        }
    }
    Ok(if failed { 2 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "{11111111-2222-3333-4444-555555555555}";

    fn p(a: &[&str]) -> Result<Mode, String> {
        parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_modes() {
        assert_eq!(p(&[]).unwrap(), Mode::Editor);
        assert_eq!(p(&["--settings"]).unwrap(), Mode::Editor);
        assert_eq!(p(&["--resident"]).unwrap(), Mode::Resident);
        assert_eq!(p(&["--profile", G]).unwrap(), Mode::Profile(G.into()));
        assert_eq!(p(&[&format!("--profile={G}")]).unwrap(), Mode::Profile(G.into()));
        assert_eq!(p(&["--shortcut", G]).unwrap(), Mode::Shortcut(G.into(), true, false));
        assert_eq!(p(&["--shortcut", G, "startmenu"]).unwrap(), Mode::Shortcut(G.into(), false, true));
        assert_eq!(p(&[&format!("--shortcut={G}"), "both"]).unwrap(), Mode::Shortcut(G.into(), true, true));
    }

    #[test]
    fn rejects_bad_args() {
        for bad in [
            &["--profile"][..],
            &["--profile", "abc"],
            &["--profile="],
            &["--profilex", G],
            &["--bogus"],
            &["--resident", "--settings"],
            &["--shortcut", G, "somewhere"],
            &["--shortcut", G, "desktop", "--resident"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }
}
