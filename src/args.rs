//! Launch mode and command-line argument parsing.
//!
//! Supported forms:
//!   ProjectLauncher.exe                        -> GUI
//!   ProjectLauncher.exe --resident             -> Resident
//!   ProjectLauncher.exe --settings             -> same GUI
//!   ProjectLauncher.exe --profile <GUID>       -> run one profile and exit
//!   ProjectLauncher.exe --shortcut <GUID> [loc]-> create a .lnk

use crate::config::is_valid_guid;
use std::ffi::OsString;
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Gui,
    Resident,
    Settings,
    Profile(String),
    Shortcut {
        id: String,
        location: ShortcutLocation,
    },
}

/// Where `--shortcut` writes the .lnk file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutLocation {
    Desktop,
    StartMenu,
    Both,
}

#[derive(Debug)]
pub struct ArgsError {
    pub message: String,
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n\n{USAGE}", self.message)
    }
}

impl std::error::Error for ArgsError {}

pub const USAGE: &str = "Usage:
  ProjectLauncher.exe                                open the profile manager window
  ProjectLauncher.exe --resident                     start the resident process
  ProjectLauncher.exe --settings                     open the profile manager window
  ProjectLauncher.exe --profile <GUID>               run a profile (via the resident when running)
  ProjectLauncher.exe --shortcut <GUID> [location]   create a profile shortcut
                                                     location: desktop (default) | startmenu | both";

fn err(message: impl Into<String>) -> ArgsError {
    ArgsError {
        message: message.into(),
    }
}

/// Parses argv (excluding argv[0]). Arguments must be valid Unicode; profile
/// GUIDs are ASCII anyway, so non-Unicode input is rejected rather than
/// silently mangled.
pub fn parse<I, S>(args: I) -> Result<Mode, ArgsError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut mode: Option<Mode> = None;
    // Collected up front so the optional location word after --shortcut can
    // be inspected through peek().
    let mut iter = args
        .into_iter()
        .map(Into::into)
        .collect::<Vec<OsString>>()
        .into_iter()
        .peekable();
    while let Some(raw) = iter.next() {
        let arg = raw
            .to_str()
            .ok_or_else(|| err("argument is not valid Unicode"))?;
        let next_mode = match arg {
            "--resident" => Mode::Resident,
            "--settings" => Mode::Settings,
            "--profile" => {
                let value = take_value(&mut iter, "--profile")?;
                Mode::Profile(validate_guid("--profile", &value)?)
            }
            _ if arg.starts_with("--profile=") => {
                Mode::Profile(validate_guid("--profile", &arg["--profile=".len()..])?)
            }
            "--shortcut" => {
                let value = take_value(&mut iter, "--shortcut")?;
                Mode::Shortcut {
                    id: validate_guid("--shortcut", &value)?,
                    location: take_location(&mut iter),
                }
            }
            _ if arg.starts_with("--shortcut=") => Mode::Shortcut {
                id: validate_guid("--shortcut", &arg["--shortcut=".len()..])?,
                location: take_location(&mut iter),
            },
            _ => return Err(err(format!("unknown argument {arg:?}"))),
        };
        if mode.replace(next_mode).is_some() {
            return Err(err("only one mode may be given"));
        }
    }
    Ok(mode.unwrap_or(Mode::Gui))
}

fn take_value(
    iter: &mut std::iter::Peekable<std::vec::IntoIter<OsString>>,
    flag: &str,
) -> Result<String, ArgsError> {
    iter.next()
        .and_then(|v| v.into_string().ok())
        .ok_or_else(|| err(format!("{flag} requires a GUID value")))
}

/// Consumes the optional `--shortcut` location word. Only a known value is
/// consumed; anything else stays so e.g. `--shortcut G --resident` still
/// reports "only one mode" instead of swallowing the flag.
fn take_location(iter: &mut std::iter::Peekable<std::vec::IntoIter<OsString>>) -> ShortcutLocation {
    let location = match iter.peek().and_then(|v| v.to_str()) {
        Some("desktop") => ShortcutLocation::Desktop,
        Some("startmenu") => ShortcutLocation::StartMenu,
        Some("both") => ShortcutLocation::Both,
        _ => return ShortcutLocation::Desktop,
    };
    iter.next();
    location
}

fn validate_guid(flag: &str, value: &str) -> Result<String, ArgsError> {
    if is_valid_guid(value) {
        Ok(value.to_string())
    } else {
        Err(err(format!(
            "{flag} expects a GUID like {{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}}, got {value:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Mode, ArgsError> {
        parse(args.iter().copied())
    }

    #[test]
    fn no_args_is_gui() {
        assert_eq!(parse_strs(&[]).unwrap(), Mode::Gui);
    }

    #[test]
    fn parses_modes() {
        assert_eq!(parse_strs(&["--resident"]).unwrap(), Mode::Resident);
        assert_eq!(parse_strs(&["--settings"]).unwrap(), Mode::Settings);
        assert_eq!(
            parse_strs(&["--profile", "{11111111-2222-3333-4444-555555555555}"]).unwrap(),
            Mode::Profile("{11111111-2222-3333-4444-555555555555}".to_string())
        );
        assert_eq!(
            parse_strs(&["--profile=11111111-2222-3333-4444-555555555555"]).unwrap(),
            Mode::Profile("11111111-2222-3333-4444-555555555555".to_string())
        );
    }

    #[test]
    fn rejects_bad_guid() {
        assert!(parse_strs(&["--profile", "abc"]).is_err());
        assert!(parse_strs(&["--profile="]).is_err());
    }

    #[test]
    fn rejects_missing_value_and_unknown_args() {
        assert!(parse_strs(&["--profile"]).is_err());
        assert!(parse_strs(&["--bogus"]).is_err());
        assert!(parse_strs(&["file.txt"]).is_err());
    }

    #[test]
    fn rejects_multiple_modes() {
        assert!(parse_strs(&["--resident", "--settings"]).is_err());
        // The location word after --shortcut must not hide a second mode.
        assert!(
            parse_strs(&[
                "--shortcut",
                "{11111111-2222-3333-4444-555555555555}",
                "--resident"
            ])
            .is_err()
        );
    }

    const GUID: &str = "{11111111-2222-3333-4444-555555555555}";

    #[test]
    fn parses_shortcut_default_desktop() {
        assert_eq!(
            parse_strs(&["--shortcut", GUID]).unwrap(),
            Mode::Shortcut {
                id: GUID.to_string(),
                location: ShortcutLocation::Desktop
            }
        );
        assert_eq!(
            parse_strs(&["--shortcut=11111111-2222-3333-4444-555555555555"]).unwrap(),
            Mode::Shortcut {
                id: "11111111-2222-3333-4444-555555555555".to_string(),
                location: ShortcutLocation::Desktop
            }
        );
        // The = form accepts the same optional location word.
        assert_eq!(
            parse_strs(&[
                "--shortcut=11111111-2222-3333-4444-555555555555",
                "startmenu"
            ])
            .unwrap(),
            Mode::Shortcut {
                id: "11111111-2222-3333-4444-555555555555".to_string(),
                location: ShortcutLocation::StartMenu
            }
        );
    }

    #[test]
    fn parses_shortcut_locations() {
        for (word, loc) in [
            ("desktop", ShortcutLocation::Desktop),
            ("startmenu", ShortcutLocation::StartMenu),
            ("both", ShortcutLocation::Both),
        ] {
            assert_eq!(
                parse_strs(&["--shortcut", GUID, word]).unwrap(),
                Mode::Shortcut {
                    id: GUID.to_string(),
                    location: loc
                }
            );
        }
    }

    #[test]
    fn rejects_bad_shortcut_args() {
        assert!(parse_strs(&["--shortcut"]).is_err());
        assert!(parse_strs(&["--shortcut", "abc"]).is_err());
        assert!(parse_strs(&["--shortcut="]).is_err());
        // An unknown word after the GUID is not a location and must error.
        assert!(parse_strs(&["--shortcut", GUID, "somewhere"]).is_err());
    }
}
