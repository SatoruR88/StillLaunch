//! Config model, loading and validation.
//!
//! The config file lives at `%LOCALAPPDATA%\ProjectLauncher\config.json`.
//! Profiles are referenced by a stable GUID, never by name. Unknown fields
//! are rejected on purpose so typos in the config cannot silently change
//! behavior.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

pub const CURRENT_CONFIG_VERSION: u32 = 1;
/// Upper bound for Wait actions (24h). Anything larger is almost certainly a
/// misconfiguration, so it is rejected at validation time.
pub const MAX_WAIT_MS: u64 = 86_400_000;
/// Upper bound for the config file size (1 MiB). The file is meant to be
/// hand-edited JSON of a few KB at most; the cap keeps `Config::load` from
/// reading an unbounded amount when a stray huge file sits at the config
/// path.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Stable GUID that identifies this profile (braced or bare form).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Action {
    /// Stable ID that identifies this action inside the profile.
    pub id: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(flatten)]
    pub kind: ActionKind,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionKind {
    /// Launches an executable via CreateProcessW. `arguments` is appended to
    /// the command line verbatim; quoting it is the user's responsibility.
    Application {
        path: String,
        #[serde(default)]
        arguments: String,
        working_directory: Option<String>,
    },
    /// Opens a folder through the Windows Shell association.
    Folder { path: String },
    /// Opens a file through the Windows Shell association.
    File { path: String },
    /// Opens a URL through the Windows Shell association.
    Url { url: String },
    /// Runs a user-supplied command string through the selected shell.
    /// The command is appended verbatim after `/C` / `-Command`; quoting is
    /// the user's responsibility.
    Command {
        shell: CommandShell,
        command: String,
    },
    /// Sleeps for the given number of milliseconds.
    Wait { milliseconds: u64 },
}

/// Shells supported by the Command action. PowerShell 7 and others can be
/// added later without changing the data model.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandShell {
    Cmd,
    #[serde(rename = "powershell")]
    PowerShell,
}

impl CommandShell {
    /// Path of the shell executable relative to the Windows system
    /// directory. Resolving it against the system directory rather than
    /// relying on PATH/CWD prevents a planted `cmd.exe` from being run.
    pub fn system_relative_exe(self) -> &'static str {
        match self {
            CommandShell::Cmd => "cmd.exe",
            CommandShell::PowerShell => r"WindowsPowerShell\v1.0\powershell.exe",
        }
    }

    /// Arguments that precede the user's command string, including the
    /// trailing space.
    pub fn command_line_prefix(self) -> &'static str {
        match self {
            CommandShell::Cmd => "/C ",
            CommandShell::PowerShell => "-NoProfile -Command ",
        }
    }
}

/// Why a config document failed to parse: broken JSON, or well-formed JSON
/// whose values fail validation.
#[derive(Debug)]
pub enum ParseError {
    Json(serde_json::Error),
    Validation(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Json(e) => write!(f, "{e}"),
            ParseError::Validation(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseError::Json(e) => Some(e),
            ParseError::Validation(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    /// %LOCALAPPDATA% is not set; the config location cannot be resolved.
    NoLocalAppData,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is larger than `MAX_CONFIG_BYTES`; reading is refused
    /// before allocating, so a corrupt path cannot exhaust memory.
    TooLarge {
        path: PathBuf,
        size: u64,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Serialization itself failed; effectively unreachable for this model
    /// but reported instead of panicking.
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    Validation {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoLocalAppData => {
                write!(f, "%LOCALAPPDATA% is not set; cannot locate config.json")
            }
            ConfigError::Io { path, source } => {
                write!(f, "cannot read config file {}: {source}", path.display())
            }
            ConfigError::TooLarge { path, size } => {
                write!(
                    f,
                    "config file {} is too large ({size} bytes; maximum is {MAX_CONFIG_BYTES})",
                    path.display()
                )
            }
            ConfigError::Parse { path, source } => {
                write!(
                    f,
                    "config file {} is not valid JSON: {source}",
                    path.display()
                )
            }
            ConfigError::Serialize { path, source } => {
                write!(
                    f,
                    "cannot serialize config for {}: {source}",
                    path.display()
                )
            }
            ConfigError::Validation { path, message } => {
                write!(f, "config file {} is invalid: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
            ConfigError::Serialize { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Normalizes a GUID for comparison by removing one surrounding brace pair
/// — only when both braces are present — and upper-casing. Shared by
/// `is_valid_guid`, the duplicate check in `Config::validate` and
/// `profile::find_profile` so all three agree on which strings name the
/// same profile.
pub(crate) fn guid_key(s: &str) -> String {
    s.strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or(s)
        .to_ascii_uppercase()
}

/// Returns true for `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, with or without
/// one pair of surrounding braces. Mismatched braces can never pass: a
/// brace kept by `guid_key` is neither a hex digit nor a `-` separator.
pub fn is_valid_guid(s: &str) -> bool {
    let inner = guid_key(s);
    let bytes = inner.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        let is_separator = matches!(i, 8 | 13 | 18 | 23);
        if is_separator != (b == b'-') {
            return false;
        }
        if !is_separator && !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn validate_no_nul(field: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        Err(format!("{field} contains an interior NUL byte"))
    } else {
        Ok(())
    }
}

/// Validates a single action. Shared by `Config::validate` and the GUI's
/// action editor, which needs per-action errors before the whole profile is
/// assembled.
pub(crate) fn validate_action(action: &Action) -> Result<(), String> {
    if action.id.is_empty() {
        return Err("action id must not be empty".to_string());
    }
    match &action.kind {
        ActionKind::Application {
            path,
            arguments,
            working_directory,
        } => {
            if path.is_empty() {
                return Err("application path must not be empty".to_string());
            }
            // Mirror `actions::build_command_line`: the path is wrapped in
            // double quotes at launch time, so a stored path that breaks
            // that quoting could never run. Rejecting it here keeps a
            // guaranteed-to-fail setting out of the config.
            if path.contains('"') {
                return Err(format!(
                    "application path must not contain a quote: {path:?}"
                ));
            }
            if path.ends_with('\\') || path.ends_with('/') {
                return Err(format!(
                    "application path must not end with a path separator: {path:?}"
                ));
            }
            validate_no_nul("application path", path)?;
            validate_no_nul("application arguments", arguments)?;
            if let Some(dir) = working_directory {
                if dir.is_empty() {
                    return Err("working_directory must not be empty".to_string());
                }
                // A relative working directory would be resolved against
                // the launcher's own current directory at run time, which
                // is almost never what the user intended to store.
                if !is_absolute_windows_path(dir) {
                    return Err(format!(
                        "working_directory must be an absolute path (e.g. C:\\dir or \\\\server\\share): {dir:?}"
                    ));
                }
                validate_no_nul("working_directory", dir)?;
            }
        }
        ActionKind::Folder { path } | ActionKind::File { path } => {
            if path.is_empty() {
                return Err("path must not be empty".to_string());
            }
            validate_no_nul("path", path)?;
        }
        ActionKind::Url { url } => {
            if url.is_empty() {
                return Err("url must not be empty".to_string());
            }
            validate_no_nul("url", url)?;
            if !has_uri_scheme(url) {
                return Err(format!("url {url:?} has no scheme (e.g. https:)"));
            }
        }
        ActionKind::Command { command, .. } => {
            if command.is_empty() {
                return Err("command must not be empty".to_string());
            }
            validate_no_nul("command", command)?;
        }
        ActionKind::Wait { milliseconds } => {
            if *milliseconds > MAX_WAIT_MS {
                return Err(format!(
                    "wait of {milliseconds}ms exceeds the maximum of {MAX_WAIT_MS}ms"
                ));
            }
        }
    }
    Ok(())
}

/// True when `url` starts with a real URI scheme per RFC 3986
/// (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`).
///
/// Single-letter schemes are rejected on purpose: `C:\dir` would otherwise
/// look like scheme `C`, so a local path pasted into a URL action would be
/// handed to ShellExecuteW as a protocol. Such paths belong in a
/// `file`/`folder` action instead.
fn has_uri_scheme(url: &str) -> bool {
    match url.find(':') {
        Some(0) | Some(1) | None => false,
        Some(i) => url[..i].chars().enumerate().all(|(n, c)| {
            c.is_ascii_alphabetic()
                || (n > 0 && (c.is_ascii_digit() || matches!(c, '+' | '-' | '.')))
        }),
    }
}

/// True for `X:\...` / `X:/...` with an ASCII drive letter, or a `\\...`
/// UNC path. `C:foo` (drive-relative) and plain relative paths return
/// false on purpose: they would resolve against whatever current
/// directory the launcher happens to have at run time.
fn is_absolute_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.starts_with(b"\\\\") {
        return true;
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

impl Config {
    /// Parses and validates a config document.
    pub fn parse(json: &str) -> Result<Config, ParseError> {
        let config: Config = serde_json::from_str(json).map_err(ParseError::Json)?;
        config.validate().map_err(ParseError::Validation)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != CURRENT_CONFIG_VERSION {
            return Err(format!(
                "unsupported config version {} (supported: {CURRENT_CONFIG_VERSION})",
                self.version
            ));
        }
        let mut profile_ids = HashSet::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            if !is_valid_guid(&profile.id) {
                return Err(format!("profile id {:?} is not a valid GUID", profile.id));
            }
            if !profile_ids.insert(guid_key(&profile.id)) {
                return Err(format!("duplicate profile id {:?}", profile.id));
            }
            if profile.name.trim().is_empty() {
                return Err(format!("profile {:?} has an empty name", profile.id));
            }
            let mut action_ids = HashSet::with_capacity(profile.actions.len());
            for action in &profile.actions {
                validate_action(action).map_err(|m| format!("profile {:?}: {m}", profile.id))?;
                if !action_ids.insert(&action.id) {
                    return Err(format!(
                        "profile {:?}: duplicate action id {:?}",
                        profile.id, action.id
                    ));
                }
            }
        }
        Ok(())
    }

    /// Loads and validates the config from an explicit path.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        // Check the size before reading so a bogus huge file at the
        // config path cannot be slurped into memory unbounded.
        let size = std::fs::metadata(path)
            .map_err(|source| ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        if size > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge {
                path: path.to_path_buf(),
                size,
            });
        }
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Tolerate a UTF-8 BOM: Notepad's "UTF-8 with BOM" save adds one
        // and serde_json refuses to parse it.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        Self::parse(text).map_err(|e| match e {
            ParseError::Json(source) => ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            },
            ParseError::Validation(message) => ConfigError::Validation {
                path: path.to_path_buf(),
                message,
            },
        })
    }

    /// Loads and validates the config from the default location.
    pub fn load_default() -> Result<Config, ConfigError> {
        Self::load(&config_path()?)
    }

    /// Validates and writes the config to `path`. The write goes through a
    /// sibling temp file renamed over the target so a crash mid-write cannot
    /// leave a truncated config; `fs::rename` replaces the destination on
    /// Windows.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate().map_err(|message| ConfigError::Validation {
            path: path.to_path_buf(),
            message,
        })?;
        let text = serde_json::to_string_pretty(self).map_err(|source| ConfigError::Serialize {
            path: path.to_path_buf(),
            source,
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ConfigError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        if let Err(source) = std::fs::rename(&tmp, path) {
            // Best-effort cleanup so a failed save does not leave a stray
            // config.json.tmp next to the real config; the rename error is
            // the one worth reporting.
            let _ = std::fs::remove_file(&tmp);
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
        Ok(())
    }
}

/// `%LOCALAPPDATA%\ProjectLauncher\config.json`
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or(ConfigError::NoLocalAppData)?;
    Ok(PathBuf::from(base)
        .join("ProjectLauncher")
        .join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "version": 1,
        "profiles": [
            {
                "id": "{11111111-2222-3333-4444-555555555555}",
                "name": "Dev",
                "actions": [
                    { "id": "a1", "type": "wait", "milliseconds": 100 },
                    { "id": "a2", "type": "url", "url": "https://example.com", "disabled": true },
                    { "id": "a3", "type": "application", "path": "C:\\Tools\\app.exe", "arguments": "-x 1" },
                    { "id": "a4", "type": "command", "shell": "powershell", "command": "Get-Date" }
                ]
            }
        ]
    }"#;

    #[test]
    fn parses_valid_config() {
        let config = Config::parse(VALID).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].actions.len(), 4);
        assert!(config.profiles[0].actions[1].disabled);
    }

    #[test]
    fn rejects_unknown_version() {
        let err = Config::parse(r#"{"version": 99, "profiles": []}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported config version"), "{err}");
    }

    #[test]
    fn rejects_broken_json() {
        assert!(Config::parse("{ not json").is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = Config::parse(r#"{"version": 1, "profiles": [], "extra": 1}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rejects_unknown_action_type() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[{"id":"a","type":"reboot"}]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown variant"), "{err}");
    }

    #[test]
    fn rejects_unknown_action_field() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[{"id":"a","type":"wait","milliseconds":1,"bogus":2}]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rejects_invalid_profile_guid() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"not-a-guid","name":"x","actions":[]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a valid GUID"), "{err}");
    }

    #[test]
    fn rejects_duplicate_profile_ids_case_insensitive() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[
                {"id":"AAAAAAAA-2222-3333-4444-555555555555","name":"x","actions":[]},
                {"id":"aaaaaaaa-2222-3333-4444-555555555555","name":"y","actions":[]}
            ]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate profile id"), "{err}");
    }

    #[test]
    fn rejects_duplicate_action_ids() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {"id":"a","type":"wait","milliseconds":1},
                {"id":"a","type":"wait","milliseconds":2}
            ]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate action id"), "{err}");
    }

    #[test]
    fn rejects_empty_application_path() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {"id":"a","type":"application","path":""}
            ]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn rejects_huge_wait() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {"id":"a","type":"wait","milliseconds":99999999999}
            ]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exceeds the maximum"), "{err}");
    }

    #[test]
    fn rejects_url_without_scheme() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {"id":"a","type":"url","url":"example.com"}
            ]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no scheme"), "{err}");
    }

    /// `C:\dir` must not be mistaken for scheme `C`; such paths belong in a
    /// file/folder action.
    #[test]
    fn rejects_windows_path_as_url() {
        for path in ["C:\\work", "C:/work", "D:"] {
            let json = format!(
                r#"{{"version":1,"profiles":[{{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                    {{"id":"a","type":"url","url":"{}"}}
                ]}}]}}"#,
                path.replace('\\', "\\\\")
            );
            let err = Config::parse(&json).unwrap_err().to_string();
            assert!(err.contains("no scheme"), "{path}: {err}");
        }
    }

    #[test]
    fn accepts_mailto_and_non_http_schemes() {
        Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {"id":"a","type":"url","url":"mailto:a@b.c"}
            ]}]}"#,
        )
        .unwrap();
    }

    #[test]
    fn rejects_interior_nul() {
        let err = Config::parse(
            "{\"version\":1,\"profiles\":[{\"id\":\"11111111-2222-3333-4444-555555555555\",\"name\":\"x\",\"actions\":[{\"id\":\"a\",\"type\":\"file\",\"path\":\"C:\\\\a\\u0000b.txt\"}]}]}",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("NUL"), "{err}");
    }

    #[test]
    fn guid_validation() {
        assert!(is_valid_guid("11111111-2222-3333-4444-555555555555"));
        assert!(is_valid_guid("{11111111-2222-3333-4444-555555555555}"));
        assert!(is_valid_guid("abcdefab-0000-ffff-9999-000000000000"));
        assert!(!is_valid_guid("{11111111-2222-3333-4444-555555555555"));
        assert!(!is_valid_guid("11111111-2222-3333-4444-55555555555"));
        assert!(!is_valid_guid("11111111-2222-3333-4444-55555555555g"));
        assert!(!is_valid_guid("11111111222233334444555555555555"));
        assert!(!is_valid_guid(""));
    }

    #[test]
    fn save_roundtrips_and_refuses_invalid() {
        let dir = std::env::temp_dir().join(format!("pl-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let config = Config::parse(VALID).unwrap();

        config.save(&path).unwrap();
        // The saved file parses back to an equivalent config.
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.profiles.len(), config.profiles.len());
        assert_eq!(reloaded.profiles[0].id, config.profiles[0].id);
        assert_eq!(
            reloaded.profiles[0].actions.len(),
            config.profiles[0].actions.len()
        );

        // An invalid config must be refused before anything is written.
        let mut bad = config.clone();
        bad.profiles[0].id = "not-a-guid".to_string();
        assert!(bad.save(&path).is_err());
        // ...and the previous file is still intact.
        assert_eq!(Config::load(&path).unwrap().profiles.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `{X}` and `X` name the same profile for `find_profile`, so the
    /// duplicate check must catch them too — otherwise the second entry
    /// would be silently unreachable.
    #[test]
    fn rejects_duplicate_profile_ids_braced_vs_bare() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[
                {"id":"{AAAAAAAA-2222-3333-4444-555555555555}","name":"x","actions":[]},
                {"id":"aaaaaaaa-2222-3333-4444-555555555555","name":"y","actions":[]}
            ]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate profile id"), "{err}");
    }

    #[test]
    fn rejects_whitespace_only_profile_name() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"   ","actions":[]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty name"), "{err}");
    }

    /// Same rule as `actions::build_command_line`: the path is wrapped in
    /// quotes at launch, so an embedded quote could never work.
    #[test]
    fn rejects_quoted_application_path() {
        let err = Config::parse(
            r#"{"version":1,"profiles":[{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {"id":"a","type":"application","path":"C:\\Tools\"bad\\app.exe"}
            ]}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("must not contain a quote"), "{err}");
    }

    /// `"C:\dir\" arg` would escape the closing quote added at launch.
    #[test]
    fn rejects_application_path_ending_with_separator() {
        for path in ["C:\\Tools\\", "C:/Tools/"] {
            let json = format!(
                r#"{{"version":1,"profiles":[{{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                    {{"id":"a","type":"application","path":"{}"}}
                ]}}]}}"#,
                path.replace('\\', "\\\\")
            );
            let err = Config::parse(&json).unwrap_err().to_string();
            assert!(
                err.contains("must not end with a path separator"),
                "{path}: {err}"
            );
        }
    }

    fn config_with_working_dir(dir: &str) -> String {
        format!(
            r#"{{"version":1,"profiles":[{{"id":"11111111-2222-3333-4444-555555555555","name":"x","actions":[
                {{"id":"a","type":"application","path":"C:\\Tools\\app.exe","working_directory":"{}"}}
            ]}}]}}"#,
            dir.replace('\\', "\\\\")
        )
    }

    /// A relative working directory resolves against the launcher's own
    /// CWD at run time, so it is rejected at validation time.
    #[test]
    fn rejects_relative_working_directory() {
        for dir in ["Tools", ".\\Tools", "sub\\dir", "C:Tools"] {
            let err = Config::parse(&config_with_working_dir(dir))
                .unwrap_err()
                .to_string();
            assert!(err.contains("must be an absolute path"), "{dir}: {err}");
        }
    }

    #[test]
    fn accepts_absolute_working_directory() {
        for dir in ["C:\\Tools", "D:/Tools", "\\\\server\\share"] {
            Config::parse(&config_with_working_dir(dir)).unwrap_or_else(|e| panic!("{dir}: {e}"));
        }
    }

    /// Notepad's "UTF-8 with BOM" save prepends U+FEFF; accept it.
    #[test]
    fn load_accepts_utf8_bom() {
        let dir = std::env::temp_dir().join(format!("pl-test-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, format!("\u{feff}{VALID}")).unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.profiles.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_oversized_file() {
        let dir = std::env::temp_dir().join(format!("pl-test-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        // A file just over the cap; its contents are never read.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        drop(file);

        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::TooLarge { .. }), "{err:?}");
        assert!(err.to_string().contains("too large"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
