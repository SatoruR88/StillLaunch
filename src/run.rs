//! Running steps. "Success" means Windows accepted the request; launched
//! programs are never waited for (use a wait step for delays).

use crate::config::{Kind, Profile, Step};
use crate::win::{last_error, wide};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Step number (1-based) and outcome for every enabled step, in order. A
/// failing step does not stop the ones after it.
pub type Report = Vec<(usize, Result<(), String>)>;

pub fn run_profile(p: &Profile) -> Report {
    p.steps
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.off)
        .map(|(i, s)| (i + 1, run_step(s)))
        .collect()
}

/// One line, e.g. `Dev: 4/5 OK — #3: file not found`.
pub fn summary(p: &Profile, r: &Report) -> String {
    let fails: Vec<String> = r
        .iter()
        .filter_map(|(n, x)| x.as_ref().err().map(|e| format!("#{n}: {e}")))
        .collect();
    let mut s = format!("{}: {}/{} OK", p.name, r.len() - fails.len(), r.len());
    if !fails.is_empty() {
        s.push_str(" — ");
        s.push_str(&fails.join(" / "));
    }
    s
}

pub fn run_step(s: &Step) -> Result<(), String> {
    match &s.kind {
        Kind::App { path, args, dir } => spawn(&command_line(path, args), dir),
        Kind::Folder(t) | Kind::File(t) | Kind::Url(t) => open(t),
        Kind::Cmd { ps, line } => {
            // Resolved under the system directory so PATH or the current
            // directory cannot substitute another cmd.exe / powershell.exe.
            let (exe, pre) = if *ps {
                (r"WindowsPowerShell\v1.0\powershell.exe", "-NoProfile -Command ")
            } else {
                ("cmd.exe", "/C ")
            };
            spawn(&command_line(&format!("{}\\{exe}", system_dir()?), &format!("{pre}{line}")), "")
        }
        Kind::Wait(ms) => {
            std::thread::sleep(std::time::Duration::from_millis(*ms));
            Ok(())
        }
    }
}

/// `"exe" args` — the program is always quoted, the arguments are verbatim.
pub fn command_line(exe: &str, args: &str) -> String {
    if args.is_empty() { format!("\"{exe}\"") } else { format!("\"{exe}\" {args}") }
}

/// CreateProcessW without waiting; `dir` empty = inherit.
pub fn spawn(cmd: &str, dir: &str) -> Result<(), String> {
    let mut line = wide(cmd); // CreateProcessW may write to this buffer.
    let dir_w = wide(dir);
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            0,
            null(),
            if dir.is_empty() { null() } else { dir_w.as_ptr() },
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(last_error("CreateProcessW"));
        }
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    Ok(())
}

/// ShellExecuteW with the default verb (folders, files, URLs).
fn open(target: &str) -> Result<(), String> {
    let t = wide(target);
    let code = unsafe { ShellExecuteW(null_mut(), null(), t.as_ptr(), null(), null(), SW_SHOWNORMAL) } as usize;
    if code > 32 {
        return Ok(());
    }
    let why = match code {
        2 => "file not found",
        3 => "path not found",
        5 => "access denied",
        31 => "no application is associated",
        _ => "ShellExecuteW failed",
    };
    Err(format!("{why} (code {code})"))
}

fn system_dir() -> Result<String, String> {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(last_error("GetSystemDirectoryW"));
    }
    Ok(String::from_utf16_lossy(&buf[..n]))
}
