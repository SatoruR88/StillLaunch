//! Profiles: data model, validation, load/save of
//! `%LOCALAPPDATA%\ProjectLauncher\config.json`.
//!
//! The file format is the one used before the rewrite (version 1), so
//! existing files load unchanged. Unknown fields are rejected so a typo
//! cannot silently change what a profile does. The per-action `"id"` field
//! is still accepted on read but no longer written.

use crate::json::{self, Json};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Largest config file that will be read (1 MiB).
const MAX_BYTES: u64 = 1 << 20;
/// Longest allowed wait step (24 h).
pub const MAX_WAIT_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Stable GUID; shortcuts and `--profile` refer to it.
    pub id: String,
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub off: bool,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// `args` is appended verbatim; `dir` empty = inherit.
    App { path: String, args: String, dir: String },
    Folder(String),
    File(String),
    Url(String),
    /// `ps` = PowerShell, otherwise cmd. `line` is passed verbatim.
    Cmd { ps: bool, line: String },
    Wait(u64),
}

/// JSON `"type"` names, in the order the editor lists them.
pub const KINDS: [&str; 6] = ["application", "folder", "file", "url", "command", "wait"];

impl Kind {
    pub fn index(&self) -> usize {
        match self {
            Kind::App { .. } => 0,
            Kind::Folder(_) => 1,
            Kind::File(_) => 2,
            Kind::Url(_) => 3,
            Kind::Cmd { .. } => 4,
            Kind::Wait(_) => 5,
        }
    }
}

impl Config {
    pub fn find(&self, id: &str) -> Option<&Profile> {
        let key = guid_key(id);
        self.profiles.iter().find(|p| guid_key(&p.id) == key)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (i, p) in self.profiles.iter().enumerate() {
            if !is_guid(&p.id) {
                return Err(format!("profile id {:?} is not a GUID", p.id));
            }
            if self.profiles[..i].iter().any(|q| guid_key(&q.id) == guid_key(&p.id)) {
                return Err(format!("duplicate profile id {:?}", p.id));
            }
            if p.name.trim().is_empty() {
                return Err(format!("profile {:?} has an empty name", p.id));
            }
            for (n, s) in p.steps.iter().enumerate() {
                check_step(s).map_err(|e| format!("{} #{}: {e}", p.name, n + 1))?;
            }
        }
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let root = json::parse(text.strip_prefix('\u{feff}').unwrap_or(text))?;
        let f = fields(&root, "config", &["version", "profiles"])?;
        if get(f, "version") != Some(&Json::Num(1)) {
            return Err("unsupported or missing \"version\" (expected 1)".into());
        }
        let profiles = match get(f, "profiles") {
            None => Vec::new(),
            Some(Json::Arr(a)) => a.iter().map(parse_profile).collect::<Result<_, _>>()?,
            Some(_) => return Err("\"profiles\" must be an array".into()),
        };
        let c = Config { profiles };
        c.validate()?;
        Ok(c)
    }

    pub fn to_json(&self) -> String {
        let mut o = String::from("{\n  \"version\": 1,\n  \"profiles\": [");
        for (i, p) in self.profiles.iter().enumerate() {
            o.push_str(if i == 0 { "\n    {\n      \"id\": " } else { ",\n    {\n      \"id\": " });
            json::write_str(&mut o, &p.id);
            o.push_str(",\n      \"name\": ");
            json::write_str(&mut o, &p.name);
            o.push_str(",\n      \"actions\": [");
            for (j, s) in p.steps.iter().enumerate() {
                o.push_str(if j == 0 { "\n        " } else { ",\n        " });
                write_step(&mut o, s);
            }
            o.push_str(if p.steps.is_empty() { "]\n    }" } else { "\n      ]\n    }" });
        }
        o.push_str(if self.profiles.is_empty() { "]\n}\n" } else { "\n  ]\n}\n" });
        o
    }

    /// Validates, then writes through a temp file + rename so a crash
    /// cannot leave a truncated config behind.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.to_json()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }
}

/// Checks one step. Shared by `Config::validate` and the editor.
pub fn check_step(s: &Step) -> Result<(), String> {
    match &s.kind {
        Kind::App { path, dir, .. } => {
            if path.is_empty() {
                return Err("path is empty".into());
            }
            // The path is wrapped in quotes at launch; a quote or a trailing
            // separator (`"C:\dir\"`) would break that.
            if path.contains('"') || path.ends_with(['\\', '/']) {
                return Err(format!("invalid program path {path:?}"));
            }
            if !dir.is_empty() && !is_absolute(dir) {
                return Err(format!("working directory must be absolute: {dir:?}"));
            }
        }
        Kind::Folder(t) | Kind::File(t) | Kind::Cmd { line: t, .. } => {
            if t.is_empty() {
                return Err("value is empty".into());
            }
        }
        Kind::Url(u) => {
            // A real scheme has 2+ chars, so `C:\x` is not taken for one.
            let scheme = u.split_once(':').map_or("", |(s, _)| s);
            let ok = scheme.len() > 1
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
            if !ok {
                return Err(format!("URL needs a scheme such as https: {u:?}"));
            }
        }
        Kind::Wait(ms) => {
            if *ms > MAX_WAIT_MS {
                return Err(format!("wait is longer than {MAX_WAIT_MS} ms"));
            }
        }
    }
    Ok(())
}

pub fn path() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("%LOCALAPPDATA% is not set")?;
    Ok(PathBuf::from(base).join("ProjectLauncher").join("config.json"))
}

pub fn load(path: &Path) -> Result<Config, String> {
    let at = |e: String| format!("{}: {e}", path.display());
    let size = std::fs::metadata(path).map_err(|e| at(e.to_string()))?.len();
    if size > MAX_BYTES {
        return Err(at(format!("file is larger than {MAX_BYTES} bytes")));
    }
    let text = std::fs::read_to_string(path).map_err(|e| at(e.to_string()))?;
    Config::parse(&text).map_err(at)
}

pub fn load_default() -> Result<Config, String> {
    load(&path()?)
}

pub fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Upper-case GUID without one pair of surrounding braces, so `{X}` and `x`
/// name the same profile.
fn guid_key(s: &str) -> String {
    s.strip_prefix('{').and_then(|t| t.strip_suffix('}')).unwrap_or(s).to_ascii_uppercase()
}

/// `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, optionally in braces.
pub fn is_guid(s: &str) -> bool {
    let k = guid_key(s);
    k.len() == 36
        && k.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) { b == b'-' } else { b.is_ascii_hexdigit() }
        })
}

/// `X:\...` or `\\server\...`.
fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with("\\\\")
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
}

/// File name for a profile's .lnk: the name with characters Windows forbids
/// replaced, plus the first 8 GUID digits so two profiles never collide.
pub fn shortcut_file_name(p: &Profile) -> String {
    let mut name: String = p
        .name
        .chars()
        .take(80)
        .map(|c| if c < ' ' || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    name.truncate(name.trim_end_matches(['.', ' ']).len());
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let sb = stem.as_bytes();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (sb.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && (b'1'..=b'9').contains(&sb[3]));
    if name.is_empty() || reserved {
        name.insert(0, '_');
    }
    let tag: String = guid_key(&p.id).chars().take(8).collect::<String>().to_ascii_lowercase();
    format!("{name}-{tag}.lnk")
}

// ---- JSON -> model -------------------------------------------------------

type Fields = [(String, Json)];

/// The object's fields, after checking that every key is known and unique.
fn fields<'a>(v: &'a Json, what: &str, allowed: &[&str]) -> Result<&'a Fields, String> {
    let Json::Obj(f) = v else {
        return Err(format!("{what} must be an object"));
    };
    for (i, (k, _)) in f.iter().enumerate() {
        if !allowed.contains(&k.as_str()) {
            return Err(format!("{what}: unknown field {k:?}"));
        }
        if f[..i].iter().any(|(k2, _)| k2 == k) {
            return Err(format!("{what}: duplicate field {k:?}"));
        }
    }
    Ok(f)
}

fn get<'a>(f: &'a Fields, key: &str) -> Option<&'a Json> {
    f.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// String field; `Ok(None)` when absent or null.
fn opt_str(f: &Fields, key: &str) -> Result<Option<String>, String> {
    match get(f, key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("\"{key}\" must be a string")),
    }
}

fn req_str(f: &Fields, key: &str) -> Result<String, String> {
    opt_str(f, key)?.ok_or_else(|| format!("missing \"{key}\""))
}

fn parse_profile(v: &Json) -> Result<Profile, String> {
    let f = fields(v, "profile", &["id", "name", "actions"])?;
    let id = req_str(f, "id")?;
    let name = req_str(f, "name")?;
    let steps = match get(f, "actions") {
        None => Vec::new(),
        Some(Json::Arr(a)) => a
            .iter()
            .map(parse_step)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("profile {name:?}: {e}"))?,
        Some(_) => return Err(format!("profile {name:?}: \"actions\" must be an array")),
    };
    Ok(Profile { id, name, steps })
}

fn parse_step(v: &Json) -> Result<Step, String> {
    let Json::Obj(raw) = v else {
        return Err("action must be an object".into());
    };
    let ty = req_str(raw, "type")?;
    let extra: &[&str] = match ty.as_str() {
        "application" => &["path", "arguments", "working_directory"],
        "folder" | "file" => &["path"],
        "url" => &["url"],
        "command" => &["shell", "command"],
        "wait" => &["milliseconds"],
        _ => return Err(format!("unknown action type {ty:?}")),
    };
    let allowed: Vec<&str> = ["id", "type", "disabled"].iter().chain(extra).copied().collect();
    let f = fields(v, &format!("{ty} action"), &allowed)?;
    if get(f, "id").is_some_and(|id| !matches!(id, Json::Str(_))) {
        return Err("\"id\" must be a string".into());
    }
    let off = match get(f, "disabled") {
        None => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err("\"disabled\" must be true or false".into()),
    };
    let kind = match ty.as_str() {
        "application" => Kind::App {
            path: req_str(f, "path")?,
            args: opt_str(f, "arguments")?.unwrap_or_default(),
            dir: opt_str(f, "working_directory")?.unwrap_or_default(),
        },
        "folder" => Kind::Folder(req_str(f, "path")?),
        "file" => Kind::File(req_str(f, "path")?),
        "url" => Kind::Url(req_str(f, "url")?),
        "command" => Kind::Cmd {
            ps: match req_str(f, "shell")?.as_str() {
                "cmd" => false,
                "powershell" => true,
                s => return Err(format!("unknown shell {s:?} (cmd or powershell)")),
            },
            line: req_str(f, "command")?,
        },
        _ => match get(f, "milliseconds") {
            Some(Json::Num(n)) => Kind::Wait(*n),
            _ => return Err("\"milliseconds\" must be a non-negative integer".into()),
        },
    };
    Ok(Step { off, kind })
}

fn write_step(o: &mut String, s: &Step) {
    o.push_str("{ \"type\": ");
    json::write_str(o, KINDS[s.kind.index()]);
    match &s.kind {
        Kind::App { path, args, dir } => {
            kv(o, "path", path);
            if !args.is_empty() {
                kv(o, "arguments", args);
            }
            if !dir.is_empty() {
                kv(o, "working_directory", dir);
            }
        }
        Kind::Folder(p) | Kind::File(p) => kv(o, "path", p),
        Kind::Url(u) => kv(o, "url", u),
        Kind::Cmd { ps, line } => {
            kv(o, "shell", if *ps { "powershell" } else { "cmd" });
            kv(o, "command", line);
        }
        Kind::Wait(ms) => o.push_str(&format!(", \"milliseconds\": {ms}")),
    }
    if s.off {
        o.push_str(", \"disabled\": true");
    }
    o.push_str(" }");
}

fn kv(o: &mut String, k: &str, v: &str) {
    o.push_str(", \"");
    o.push_str(k);
    o.push_str("\": ");
    json::write_str(o, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "{11111111-2222-3333-4444-555555555555}";

    fn one(action: &str) -> Result<Config, String> {
        Config::parse(&format!(
            r#"{{"version":1,"profiles":[{{"id":"{ID}","name":"x","actions":[{action}]}}]}}"#
        ))
    }

    #[test]
    fn loads_the_example_file() {
        let c = Config::parse(include_str!("../config.example.json")).unwrap();
        assert_eq!(c.profiles[0].steps.len(), 5);
        assert!(c.profiles[0].steps[3].off);
        assert_eq!(c.profiles[0].steps[2].kind, Kind::Wait(500));
    }

    #[test]
    fn loads_old_files_with_action_ids_and_bom() {
        let c = Config::parse(&format!(
            "\u{feff}{{\"version\":1,\"profiles\":[{{\"id\":\"{}\",\"name\":\"x\",\"actions\":[\
             {{\"id\":\"a1\",\"type\":\"application\",\"path\":\"C:\\\\a.exe\",\"working_directory\":null}}]}}]}}",
            ID.replace('{', "").replace('}', "")
        ))
        .unwrap();
        assert_eq!(
            c.profiles[0].steps[0].kind,
            Kind::App { path: "C:\\a.exe".into(), args: String::new(), dir: String::new() }
        );
        assert!(c.find(ID).is_some());
    }

    #[test]
    fn roundtrips_every_kind() {
        let c = Config {
            profiles: vec![
                Profile {
                    id: ID.into(),
                    name: "Dev \"q\" 日本".into(),
                    steps: vec![
                        Step { off: false, kind: Kind::App { path: "C:\\a b\\x.exe".into(), args: "-v \"1\"".into(), dir: "D:\\w".into() } },
                        Step { off: true, kind: Kind::Folder("C:\\f".into()) },
                        Step { off: false, kind: Kind::File("C:\\f\\n.txt".into()) },
                        Step { off: false, kind: Kind::Url("https://example.com/?a=1&b=2".into()) },
                        Step { off: false, kind: Kind::Cmd { ps: true, line: "Get-Date\n".into() } },
                        Step { off: false, kind: Kind::Wait(250) },
                    ],
                },
                Profile { id: "aaaaaaaa-2222-3333-4444-555555555555".into(), name: "Empty".into(), steps: vec![] },
            ],
        };
        assert_eq!(Config::parse(&c.to_json()).unwrap(), c);
        assert_eq!(Config::parse(&Config::default().to_json()).unwrap(), Config::default());
    }

    #[test]
    fn rejects_bad_configs() {
        for (bad, why) in [
            (r#"{"version":2}"#.to_string(), "version"),
            (r#"{"version":1,"extra":1}"#.to_string(), "unknown field"),
            (r#"{"version":1,"profiles":[{"id":"nope","name":"x"}]}"#.to_string(), "GUID"),
            (format!(r#"{{"version":1,"profiles":[{{"id":"{ID}","name":" "}}]}}"#), "empty name"),
            (
                format!(r#"{{"version":1,"profiles":[{{"id":"{ID}","name":"a"}},{{"id":"11111111-2222-3333-4444-555555555555","name":"b"}}]}}"#),
                "duplicate profile",
            ),
        ] {
            let e = Config::parse(&bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        for (action, why) in [
            (r#"{"type":"reboot"}"#, "unknown action type"),
            (r#"{"type":"wait","milliseconds":1,"bogus":2}"#, "unknown field"),
            (r#"{"type":"wait","milliseconds":99999999999}"#, "longer than"),
            (r#"{"type":"application","path":""}"#, "empty"),
            (r#"{"type":"application","path":"C:\\a\"b.exe"}"#, "invalid program path"),
            (r#"{"type":"application","path":"C:\\Tools\\"}"#, "invalid program path"),
            (r#"{"type":"application","path":"a.exe","working_directory":"rel"}"#, "absolute"),
            (r#"{"type":"url","url":"example.com"}"#, "scheme"),
            (r#"{"type":"url","url":"C:\\work"}"#, "scheme"),
            (r#"{"type":"command","shell":"bash","command":"ls"}"#, "unknown shell"),
            (r#"{"type":"file","path":"a","path":"b"}"#, "duplicate field"),
        ] {
            let e = one(action).unwrap_err();
            assert!(e.contains(why), "{action}: {e}");
        }
        one(r#"{"type":"url","url":"mailto:a@b.c"}"#).unwrap();
        one(r#"{"type":"application","path":"a.exe","working_directory":"\\\\srv\\share"}"#).unwrap();
    }

    #[test]
    fn guids() {
        assert!(is_guid("11111111-2222-3333-4444-555555555555"));
        assert!(is_guid("{abcdefab-0000-ffff-9999-000000000000}"));
        assert!(!is_guid("{11111111-2222-3333-4444-555555555555"));
        assert!(!is_guid("11111111-2222-3333-4444-55555555555g"));
        assert!(!is_guid(""));
    }

    #[test]
    fn shortcut_names() {
        let p = |name: &str| Profile { id: ID.into(), name: name.into(), steps: vec![] };
        assert_eq!(shortcut_file_name(&p("Dev Env")), "Dev Env-11111111.lnk");
        assert_eq!(shortcut_file_name(&p("a/b:c*\t")), "a_b_c__-11111111.lnk");
        assert_eq!(shortcut_file_name(&p("name. ")), "name-11111111.lnk");
        assert_eq!(shortcut_file_name(&p("con.foo")), "_con.foo-11111111.lnk");
        assert_eq!(shortcut_file_name(&p("LPT1")), "_LPT1-11111111.lnk");
        assert_eq!(shortcut_file_name(&p("console")), "console-11111111.lnk");
        assert_eq!(shortcut_file_name(&p("...")), "_-11111111.lnk");
    }

    #[test]
    fn save_then_load() {
        let dir = std::env::temp_dir().join(format!("stilllaunch-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let c = Config::parse(include_str!("../config.example.json")).unwrap();
        c.save(&path).unwrap();
        assert_eq!(load(&path).unwrap(), c);
        let mut bad = c.clone();
        bad.profiles[0].id = "x".into();
        assert!(bad.save(&path).is_err());
        assert_eq!(load(&path).unwrap(), c);
        std::fs::remove_dir_all(&dir).ok();
    }
}
