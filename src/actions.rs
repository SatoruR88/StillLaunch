//! Action implementations.
//!
//! Launching via CreateProcessW / ShellExecuteW only means "the request was
//! handed to Windows". Whether the started application later exits cleanly is
//! outside the scope of this launcher and is never waited for.
//!
//! The Win32-facing helpers are Windows-only; on other platforms they compile
//! to stubs so the pure logic stays testable in non-Windows environments.

use crate::config::{Action, ActionKind, CommandShell};
use std::fmt;
use std::iter::once;

#[derive(Debug)]
pub enum ActionError {
    /// Input or environment value that cannot be handed to the Win32 API
    /// safely (e.g. interior NUL, implausible system path length).
    InvalidInput(String),
    /// CreateProcessW returned 0; `code` is GetLastError().
    CreateProcess { code: u32 },
    /// ShellExecuteW returned a value <= 32, which is an error code.
    ShellExecute { code: usize },
    /// A Win32 call needed to resolve a system path failed; `code` is
    /// GetLastError().
    SystemPath { api: &'static str, code: u32 },
    /// The host OS does not provide the required Win32 API.
    /// Only constructed by the non-Windows stubs.
    #[allow(dead_code)]
    UnsupportedPlatform,
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::InvalidInput(m) => write!(f, "invalid input: {m}"),
            ActionError::CreateProcess { code } => {
                write!(f, "CreateProcessW failed (GetLastError={code})")
            }
            ActionError::ShellExecute { code } => {
                write!(
                    f,
                    "ShellExecuteW failed ({code}: {})",
                    shell_error_meaning(*code)
                )
            }
            ActionError::SystemPath { api, code } => {
                write!(f, "{api} failed (GetLastError={code})")
            }
            ActionError::UnsupportedPlatform => {
                write!(f, "this action requires Windows")
            }
        }
    }
}

impl std::error::Error for ActionError {}

/// Executes one action. See module docs for the definition of "success".
pub fn execute(action: &Action) -> Result<(), ActionError> {
    match &action.kind {
        ActionKind::Application {
            path,
            arguments,
            working_directory,
        } => {
            let mut command_line = build_command_line(path, arguments)?;
            let cwd = match working_directory {
                Some(dir) => Some(to_wide(dir)?),
                None => None,
            };
            create_process(&mut command_line, cwd.as_deref())
        }
        // NULL verb = the file type's default verb (usually "open"), so file
        // types without an explicit "open" verb still work.
        ActionKind::Folder { path } | ActionKind::File { path } => shell_execute(path),
        ActionKind::Url { url } => shell_execute(url),
        ActionKind::Command { shell, command } => {
            // The command string is appended verbatim. We deliberately do not
            // re-quote it: how quoting reaches the shell is the user's
            // responsibility, and silent re-interpretation is a security risk.
            let exe = shell_executable(*shell)?;
            let prefix = shell.command_line_prefix();
            let mut command_line = build_command_line(&exe, &format!("{prefix}{command}"))?;
            create_process(&mut command_line, None)
        }
        ActionKind::Wait { milliseconds } => {
            std::thread::sleep(std::time::Duration::from_millis(*milliseconds));
            Ok(())
        }
    }
}

fn to_wide(s: &str) -> Result<Vec<u16>, ActionError> {
    if s.contains('\0') {
        return Err(ActionError::InvalidInput(format!(
            "string contains an interior NUL byte: {s:?}"
        )));
    }
    Ok(s.encode_utf16().chain(once(0)).collect())
}

/// Builds the NUL-terminated UTF-16 command line handed to CreateProcessW.
///
/// The executable path is always wrapped in double quotes so paths with
/// spaces cannot be re-parsed as "program arg". `arguments` is appended
/// verbatim after one space; quoting individual arguments is intentionally
/// not done here — the caller owns that contract. The returned buffer is
/// writable because CreateProcessW may modify lpCommandLine in place.
pub fn build_command_line(exe: &str, arguments: &str) -> Result<Vec<u16>, ActionError> {
    if exe.is_empty() {
        return Err(ActionError::InvalidInput(
            "executable path must not be empty".to_string(),
        ));
    }
    if exe.contains('"') {
        return Err(ActionError::InvalidInput(format!(
            "executable path must not contain a quote: {exe:?}"
        )));
    }
    // A trailing separator would escape the closing quote we add below
    // (`"C:\dir\" arg` parses as one token), so reject it instead of
    // producing a silently wrong command line.
    if exe.ends_with('\\') || exe.ends_with('/') {
        return Err(ActionError::InvalidInput(format!(
            "executable path must not end with a path separator: {exe:?}"
        )));
    }
    let mut line = String::with_capacity(exe.len() + arguments.len() + 3);
    line.push('"');
    line.push_str(exe);
    line.push('"');
    if !arguments.is_empty() {
        line.push(' ');
        line.push_str(arguments);
    }
    to_wide(&line)
}

fn shell_error_meaning(code: usize) -> &'static str {
    match code {
        0 => "out of memory or resources",
        2 => "file not found",
        3 => "path not found",
        5 => "access denied",
        8 => "out of memory",
        11 => "invalid executable",
        26 => "sharing violation",
        27 => "incomplete or invalid file association",
        28..=30 => "DDE transaction failed",
        31 => "no application associated with the file type",
        32 => "DLL not found",
        _ => "unknown error",
    }
}

#[cfg(windows)]
fn create_process(
    command_line: &mut [u16],
    working_dir: Option<&[u16]>,
) -> Result<(), ActionError> {
    use std::mem::{size_of, zeroed};
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };

    // SAFETY: `command_line` is a writable, NUL-terminated UTF-16 buffer as
    // CreateProcessW requires. `si` and `pi` are valid out-parameters. On
    // success both returned handles are closed immediately because the
    // launcher never waits on the child.
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let cwd_ptr = working_dir.map_or(ptr::null(), |w| w.as_ptr());
        let ok = CreateProcessW(
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            0,
            ptr::null(),
            cwd_ptr,
            &si,
            &mut pi,
        );
        if ok == 0 {
            // GetLastError must be read before any other API call.
            return Err(ActionError::CreateProcess {
                code: GetLastError(),
            });
        }
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        Ok(())
    }
}

/// Resolves the absolute path of a Command action's shell under the Windows
/// system directory, so neither PATH nor the current directory can redirect
/// `cmd.exe` / `powershell.exe` to a planted executable.
///
/// Called once per Command action. That is a cheap kernel32 call, so it is
/// not cached; revisit if profiles with many Command actions become common.
#[cfg(windows)]
fn shell_executable(shell: CommandShell) -> Result<String, ActionError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::ptr;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    // The system directory is well inside MAX_PATH; a larger length means the
    // API misbehaved, so fail instead of allocating an implausible buffer.
    const MAX_SYSTEM_DIR_CHARS: u32 = 1024;

    // SAFETY: GetSystemDirectoryW is called first with a null buffer to learn
    // the required length (returned including the NUL), then with a buffer of
    // exactly that length. The second call returns the character count
    // excluding the NUL, which is always < the buffer length.
    let dir = unsafe {
        let needed = GetSystemDirectoryW(ptr::null_mut(), 0);
        if needed == 0 {
            return Err(ActionError::SystemPath {
                api: "GetSystemDirectoryW",
                code: GetLastError(),
            });
        }
        if needed > MAX_SYSTEM_DIR_CHARS {
            return Err(ActionError::InvalidInput(format!(
                "GetSystemDirectoryW reported an implausible length of {needed} characters"
            )));
        }
        let mut buf = vec![0u16; needed as usize];
        let written = GetSystemDirectoryW(buf.as_mut_ptr(), needed);
        if written == 0 || written >= needed {
            return Err(ActionError::SystemPath {
                api: "GetSystemDirectoryW",
                code: GetLastError(),
            });
        }
        buf.truncate(written as usize);
        PathBuf::from(OsString::from_wide(&buf))
    };

    let path = dir.join(shell.system_relative_exe());
    path.into_os_string().into_string().map_err(|_| {
        ActionError::InvalidInput("system directory path is not valid Unicode".to_string())
    })
}

#[cfg(windows)]
fn shell_execute(target: &str) -> Result<(), ActionError> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let target = to_wide(target)?;
    // SAFETY: `target` is a NUL-terminated UTF-16 buffer that outlives the
    // call. A NULL lpOperation selects the registered default verb.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            ptr::null(),
            target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns an HINSTANCE; values <= 32 are error codes.
    // Compared as usize so a high pointer bit cannot look like a negative
    // (and therefore failing) value on 64-bit.
    let code = result as usize;
    if code <= 32 {
        Err(ActionError::ShellExecute { code })
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn create_process(
    _command_line: &mut [u16],
    _working_dir: Option<&[u16]>,
) -> Result<(), ActionError> {
    Err(ActionError::UnsupportedPlatform)
}

#[cfg(not(windows))]
fn shell_execute(_target: &str) -> Result<(), ActionError> {
    Err(ActionError::UnsupportedPlatform)
}

#[cfg(not(windows))]
fn shell_executable(_shell: CommandShell) -> Result<String, ActionError> {
    Err(ActionError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_string(buf: &[u16]) -> String {
        String::from_utf16(&buf[..buf.len() - 1]).unwrap()
    }

    #[test]
    fn command_line_quotes_exe_and_appends_args_verbatim() {
        let buf = build_command_line("C:\\My Tools\\app.exe", "-a \"b c\" \\").unwrap();
        assert_eq!(to_string(&buf), "\"C:\\My Tools\\app.exe\" -a \"b c\" \\");
    }

    #[test]
    fn command_line_without_args_is_just_quoted_exe() {
        let buf = build_command_line("app.exe", "").unwrap();
        assert_eq!(to_string(&buf), "\"app.exe\"");
    }

    #[test]
    fn command_line_is_nul_terminated() {
        let buf = build_command_line("app.exe", "-x").unwrap();
        assert_eq!(*buf.last().unwrap(), 0);
    }

    #[test]
    fn rejects_quoted_or_empty_exe_path() {
        assert!(build_command_line("a\"b.exe", "").is_err());
        assert!(build_command_line("", "").is_err());
    }

    /// `"C:\dir\" arg` would escape the closing quote and merge the argument
    /// into the program name.
    #[test]
    fn rejects_exe_path_ending_with_separator() {
        assert!(build_command_line("C:\\Tools\\", "-x").is_err());
        assert!(build_command_line("C:/Tools/", "-x").is_err());
        assert!(build_command_line("C:\\Tools\\app.exe", "-x").is_ok());
    }

    #[test]
    fn rejects_interior_nul() {
        assert!(build_command_line("app.exe", "x\0y").is_err());
        assert!(to_wide("a\0b").is_err());
    }

    /// The shell must be an absolute path under the system directory, never a
    /// bare file name that PATH or the current directory could redirect.
    #[cfg(windows)]
    #[test]
    fn shell_executable_resolves_to_absolute_system_path() {
        for (shell, exe) in [
            (CommandShell::Cmd, "cmd.exe"),
            (CommandShell::PowerShell, "powershell.exe"),
        ] {
            let resolved = shell_executable(shell).unwrap();
            let path = std::path::Path::new(&resolved);
            assert!(path.is_absolute(), "{resolved} is not absolute");
            assert!(
                resolved.ends_with(exe),
                "{resolved} does not end with {exe}"
            );
            assert!(path.is_file(), "{resolved} does not exist");
        }
    }

    #[test]
    fn utf16_conversion_handles_non_bmp() {
        let buf = build_command_line("C:\\日本語\\🦀.exe", "").unwrap();
        assert_eq!(to_string(&buf), "\"C:\\日本語\\🦀.exe\"");
    }
}
