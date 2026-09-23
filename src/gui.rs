//! Native GUI (Phase 5-6): profile list, profile/action editing, test launch,
//! shortcut creation, file/folder pickers, and the autostart toggle.
//!
//! Everything is raw Win32 with the built-in control classes (LISTBOX, EDIT,
//! BUTTON, COMBOBOX, STATIC). There is no manifest and no comctl32 v6, so the
//! controls render in the classic style — an accepted trade-off to keep a
//! single dependency-free executable.
//!
//! The GUI edits `%LOCALAPPDATA%\ProjectLauncher\config.json` directly. A
//! running resident picks changes up on the next run request via the mtime
//! check in `ResidentState::current_config`, so no reload IPC is needed.
//! Both `ProjectLauncher.exe` (no args) and `--settings` open this window.
//!
//! Dialogs are modeless windows driven modally: the owner is disabled and a
//! nested message loop runs until the dialog is destroyed. `lpCreateParams`
//! passes a pointer to the caller's stack context, matching the resident
//! window's GWLP_USERDATA pattern.

#[cfg(not(windows))]
pub fn run() -> Result<(), GuiError> {
    Err(GuiError::UnsupportedPlatform)
}

use crate::config::ConfigError;
use std::fmt;

#[derive(Debug)]
pub enum GuiError {
    /// A Win32 call failed; `code` is GetLastError().
    Win32 {
        api: &'static str,
        code: u32,
    },
    Config(ConfigError),
    /// The host OS does not provide the required Win32 API.
    /// Only constructed by the non-Windows stub.
    #[allow(dead_code)]
    UnsupportedPlatform,
}

impl fmt::Display for GuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GuiError::Win32 { api, code } => write!(f, "{api} failed (GetLastError={code})"),
            GuiError::Config(e) => write!(f, "{e}"),
            GuiError::UnsupportedPlatform => write!(f, "the GUI requires Windows"),
        }
    }
}

impl std::error::Error for GuiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GuiError::Config(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::GuiError;
    use crate::config::{self, Action, ActionKind, CommandShell, Config, ConfigError, Profile};
    use crate::{autostart, ipc, profile, shortcut};
    use std::ffi::c_void;
    use std::iter::once;
    use std::mem::{size_of, zeroed};
    use std::path::PathBuf;
    use std::ptr;
    use std::time::SystemTime;
    use windows_sys::Win32::Foundation::{
        BOOL, GetLastError, HWND, LPARAM, LRESULT, MAX_PATH, RECT, WPARAM,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        COLOR_WINDOW, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, UpdateWindow,
    };
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoCreateGuid, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Controls::BST_CHECKED;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, VK_ESCAPE, VK_RETURN};
    use windows_sys::Win32::UI::Shell::{
        BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW,
        SHGetPathFromIDListW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    use windows_sys::core::GUID;

    const CLASS_MAIN: &str = "ProjectLauncher.MainWindow";
    const CLASS_PROFILE_EDIT: &str = "ProjectLauncher.ProfileEdit";
    const CLASS_ACTION_EDIT: &str = "ProjectLauncher.ActionEdit";

    /// Posted to the main window when a background run finishes; lparam is
    /// a boxed `(ok, String)` the receiver must reclaim, wparam is
    /// POST_MAGIC.
    const WM_LAUNCH_DONE: u32 = WM_APP + 1;
    /// Same payload contract as WM_LAUNCH_DONE, posted to the profile
    /// editor when its test launch finishes.
    const WM_TEST_DONE: u32 = WM_APP + 2;
    /// wparam tag on WM_LAUNCH_DONE / WM_TEST_DONE. Only the worker threads
    /// below attach it, so the handlers can reject forged messages instead
    /// of feeding a crafted lparam to Box::from_raw.
    const POST_MAGIC: WPARAM = 0x504C_4452; // "PLDR"

    // Main window controls.
    const IDC_LIST: i32 = 100;
    const IDC_RUN: i32 = 101;
    const IDC_NEW: i32 = 102;
    const IDC_EDIT: i32 = 103;
    const IDC_DEL: i32 = 104;
    const IDC_SHORTCUT: i32 = 105;
    const IDC_STATUS: i32 = 106;
    const IDC_AUTOSTART: i32 = 107;

    // Profile editor controls.
    const IDC_PE_NAME: i32 = 200;
    const IDC_PE_ID: i32 = 201;
    const IDC_PE_ACTIONS: i32 = 202;
    const IDC_PE_ADD: i32 = 203;
    const IDC_PE_EDIT: i32 = 204;
    const IDC_PE_DEL: i32 = 205;
    const IDC_PE_UP: i32 = 206;
    const IDC_PE_DOWN: i32 = 207;
    const IDC_PE_TEST: i32 = 208;

    // Action editor controls.
    const IDC_AE_ID: i32 = 300;
    const IDC_AE_TYPE: i32 = 301;
    const IDC_AE_DISABLED: i32 = 302;
    const IDC_AE_L1: i32 = 303;
    const IDC_AE_F1: i32 = 304;
    const IDC_AE_L2: i32 = 305;
    const IDC_AE_F2: i32 = 306;
    const IDC_AE_L3: i32 = 307;
    const IDC_AE_F3: i32 = 308;
    const IDC_AE_LS: i32 = 309;
    const IDC_AE_SHELL: i32 = 310;
    const IDC_AE_BROWSE: i32 = 311;

    /// Tray-menu item ids for the shortcut popup.
    const ID_SC_DESKTOP: usize = 1;
    const ID_SC_STARTMENU: usize = 2;
    const ID_SC_BOTH: usize = 3;

    /// The action types in combo order; index <-> ActionKind via
    /// `kind_index`/`default_kind`.
    const TYPE_NAMES: [&str; 6] = ["application", "folder", "file", "url", "command", "wait"];
    const SHELL_NAMES: [&str; 2] = ["cmd", "powershell"];

    // ---------- shared helpers ----------

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(once(0)).collect()
    }

    fn win32(api: &'static str) -> GuiError {
        // SAFETY: called immediately after a failed Win32 call.
        GuiError::Win32 {
            api,
            code: unsafe { GetLastError() },
        }
    }

    /// Parameters for [`control`]; built with [`ctl`] plus struct-update
    /// syntax for `style`/`ex` when needed.
    struct Ctl<'a> {
        class: &'a str,
        text: &'a str,
        style: u32,
        ex: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    }

    fn ctl<'a>(class: &'a str, text: &'a str, x: i32, y: i32, w: i32, h: i32, id: i32) -> Ctl<'a> {
        Ctl {
            class,
            text,
            style: 0,
            ex: 0,
            x,
            y,
            w,
            h,
            id,
        }
    }

    /// Creates a child control and applies the dialog font to it.
    unsafe fn control(parent: HWND, spec: &Ctl, font: HFONT) -> HWND {
        let class = wide(spec.class);
        let text = wide(spec.text);
        // SAFETY: class is a built-in control class name; hInstance may be
        // NULL because system classes do not look it up.
        let hwnd = unsafe {
            CreateWindowExW(
                spec.ex,
                class.as_ptr(),
                text.as_ptr(),
                WS_CHILD | WS_VISIBLE | spec.style,
                spec.x,
                spec.y,
                spec.w,
                spec.h,
                parent,
                spec.id as usize as _,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if !hwnd.is_null() {
            // SAFETY: hwnd is a live control; font is a valid HFONT.
            unsafe { SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1) };
        }
        hwnd
    }

    /// The message-box font (Meiryo UI on Japanese systems) so the GUI does
    /// not fall back to a bitmap font.
    fn message_font() -> HFONT {
        // SAFETY: ncm is sized before the call per SPI_GETNONCLIENTMETRICS
        // contract; on failure we return a null HFONT and controls keep their
        // default font.
        unsafe {
            let mut ncm: NONCLIENTMETRICSW = zeroed();
            ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
            if SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                &mut ncm as *mut _ as *mut c_void,
                0,
            ) == 0
            {
                return ptr::null_mut();
            }
            CreateFontIndirectW(&ncm.lfMessageFont)
        }
    }

    fn set_text(hwnd: HWND, id: i32, text: &str) {
        let w = wide(text);
        // SAFETY: SetDlgItemTextW handles a missing control id by failing.
        unsafe { SetDlgItemTextW(hwnd, id, w.as_ptr()) };
    }

    fn dlg_text(hwnd: HWND, id: i32) -> String {
        // SAFETY: GetDlgItem may return NULL; GetWindowTextLengthW on NULL
        // yields 0 and the empty buffer stays valid.
        unsafe {
            let c = GetDlgItem(hwnd, id);
            if c.is_null() {
                return String::new();
            }
            let len = GetWindowTextLengthW(c) as usize + 1;
            let mut buf = vec![0u16; len];
            let n = GetWindowTextW(c, buf.as_mut_ptr(), len as i32) as usize;
            buf.truncate(n);
            String::from_utf16_lossy(&buf)
        }
    }

    fn set_item_visible(hwnd: HWND, id: i32, visible: bool) {
        unsafe {
            let c = GetDlgItem(hwnd, id);
            if !c.is_null() {
                ShowWindow(c, if visible { SW_SHOW } else { SW_HIDE });
            }
        }
    }

    fn set_item_enabled(hwnd: HWND, id: i32, enabled: bool) {
        unsafe {
            let c = GetDlgItem(hwnd, id);
            if !c.is_null() {
                EnableWindow(c, enabled as BOOL);
            }
        }
    }

    unsafe fn error_box(parent: HWND, message: &str) {
        let text = wide(message);
        let title = wide("Project Launcher");
        // SAFETY: strings are NUL-terminated by `wide`.
        unsafe { MessageBoxW(parent, text.as_ptr(), title.as_ptr(), MB_ICONERROR | MB_OK) };
    }

    unsafe fn info_box(parent: HWND, message: &str) {
        let text = wide(message);
        let title = wide("Project Launcher");
        unsafe {
            MessageBoxW(
                parent,
                text.as_ptr(),
                title.as_ptr(),
                MB_ICONINFORMATION | MB_OK,
            )
        };
    }

    /// Runs a nested message loop while `parent` is disabled, making `hwnd`
    /// effectively modal. IsDialogMessage gets Tab navigation and button
    /// activation; Enter/Escape fall back to OK/Cancel commands.
    unsafe fn modal(parent: HWND, hwnd: HWND) {
        unsafe {
            EnableWindow(parent, 0);
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            let mut msg: MSG = zeroed();
            while IsWindow(hwnd) != 0 {
                let r = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
                if r < 0 {
                    break;
                }
                if r == 0 {
                    // WM_QUIT was pulled off this thread's queue: re-post it
                    // so the outer message loop still sees the quit request
                    // instead of silently swallowing it.
                    PostQuitMessage(0);
                    break;
                }
                if IsDialogMessageW(hwnd, &msg) != 0 {
                    continue;
                }
                if msg.message == WM_KEYDOWN {
                    match msg.wParam {
                        k if k == VK_RETURN as usize => {
                            SendMessageW(hwnd, WM_COMMAND, IDOK as usize, 0);
                            continue;
                        }
                        k if k == VK_ESCAPE as usize => {
                            SendMessageW(hwnd, WM_COMMAND, IDCANCEL as usize, 0);
                            continue;
                        }
                        _ => {}
                    }
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            EnableWindow(parent, 1);
            SetForegroundWindow(parent);
        }
    }

    /// Centers a not-yet-visible window on its owner, or on the primary
    /// display when there is no owner.
    unsafe fn centered_rect(w: i32, h: i32, owner: HWND) -> (i32, i32) {
        unsafe {
            let mut rc: RECT = zeroed();
            if !owner.is_null() && GetWindowRect(owner, &mut rc) != 0 {
                (
                    rc.left + (rc.right - rc.left - w) / 2,
                    rc.top + (rc.bottom - rc.top - h) / 2,
                )
            } else {
                (
                    (GetSystemMetrics(SM_CXSCREEN) - w) / 2,
                    (GetSystemMetrics(SM_CYSCREEN) - h) / 2,
                )
            }
        }
    }

    /// Reads GWLP_USERDATA as a `*mut T`. Null before WM_NCCREATE stores it
    /// and after WM_DESTROY clears it.
    unsafe fn state_of<T>(hwnd: HWND) -> *mut T {
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T }
    }

    /// Loads the CREATESTRUCT context pointer written by every dialog's
    /// WM_NCCREATE handler.
    unsafe fn create_param<T>(lparam: LPARAM) -> *mut T {
        unsafe { (*(lparam as *const CREATESTRUCTW)).lpCreateParams as *mut T }
    }

    fn command_id(wparam: WPARAM) -> i32 {
        (wparam & 0xFFFF) as i32
    }

    fn command_code(wparam: WPARAM) -> u32 {
        ((wparam >> 16) & 0xFFFF) as u32
    }

    /// Selected row in a listbox, or None.
    fn list_sel(hwnd: HWND, id: i32) -> Option<usize> {
        unsafe {
            let lb = GetDlgItem(hwnd, id);
            if lb.is_null() {
                return None;
            }
            let i = SendMessageW(lb, LB_GETCURSEL, 0, 0);
            if i < 0 { None } else { Some(i as usize) }
        }
    }

    fn list_select(hwnd: HWND, id: i32, index: usize) {
        unsafe {
            let lb = GetDlgItem(hwnd, id);
            if !lb.is_null() {
                SendMessageW(lb, LB_SETCURSEL, index, 0);
            }
        }
    }

    fn list_reset(hwnd: HWND, id: i32, items: &[String]) {
        unsafe {
            let lb = GetDlgItem(hwnd, id);
            if lb.is_null() {
                return;
            }
            SendMessageW(lb, LB_RESETCONTENT, 0, 0);
            for item in items {
                let w = wide(item);
                SendMessageW(lb, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
            }
        }
    }

    /// Generates `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` via CoCreateGuid.
    fn new_guid() -> Result<String, GuiError> {
        unsafe {
            let mut g: GUID = zeroed();
            if CoCreateGuid(&mut g) < 0 {
                return Err(win32("CoCreateGuid"));
            }
            Ok(format!(
                "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
                g.data1,
                g.data2,
                g.data3,
                g.data4[0],
                g.data4[1],
                g.data4[2],
                g.data4[3],
                g.data4[4],
                g.data4[5],
                g.data4[6],
                g.data4[7],
            ))
        }
    }

    /// GetOpenFileNameW wrapper. `filter` uses the Win32 NUL-separated
    /// "label\0pattern\0" form; `wide` supplies the final terminator.
    /// Returns None on cancel (also on dialog errors — same UX).
    unsafe fn browse_file(parent: HWND, filter: &str) -> Option<String> {
        let mut buf = vec![0u16; MAX_PATH as usize];
        let filter_w = wide(filter);
        // SAFETY: all pointers are valid for the call; OFN_NOCHANGEDIR keeps
        // the dialog from mutating our process CWD.
        unsafe {
            let mut ofn: OPENFILENAMEW = zeroed();
            ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
            ofn.hwndOwner = parent;
            ofn.lpstrFilter = filter_w.as_ptr();
            ofn.lpstrFile = buf.as_mut_ptr();
            ofn.nMaxFile = buf.len() as u32;
            ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR;
            if GetOpenFileNameW(&mut ofn) == 0 {
                return None;
            }
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// SHBrowseForFolderW wrapper for folder-type paths. The new-dialog-style
    /// flag requires a COM apartment, so one is initialized and balanced per
    /// call (S_FALSE on an existing apartment still needs CoUninitialize).
    unsafe fn browse_folder(parent: HWND, title: &str) -> Option<String> {
        unsafe {
            if CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) < 0 {
                return None;
            }
            let title_w = wide(title);
            let mut display = vec![0u16; MAX_PATH as usize];
            let mut bi: BROWSEINFOW = zeroed();
            bi.hwndOwner = parent;
            bi.pszDisplayName = display.as_mut_ptr();
            bi.lpszTitle = title_w.as_ptr();
            bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
            let pidl = SHBrowseForFolderW(&bi);
            let path = if pidl.is_null() {
                None
            } else {
                let mut buf = vec![0u16; MAX_PATH as usize];
                let ok = SHGetPathFromIDListW(pidl, buf.as_mut_ptr());
                CoTaskMemFree(pidl as *const c_void);
                if ok == 0 {
                    None
                } else {
                    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    Some(String::from_utf16_lossy(&buf[..len]))
                }
            };
            CoUninitialize();
            path
        }
    }

    /// Short one-line summary shown in the action listbox.
    fn action_line(a: &Action) -> String {
        let detail = match &a.kind {
            ActionKind::Application { path, .. } => format!("application: {path}"),
            ActionKind::Folder { path } => format!("folder: {path}"),
            ActionKind::File { path } => format!("file: {path}"),
            ActionKind::Url { url } => format!("url: {url}"),
            ActionKind::Command { shell, command } => {
                let shell = match shell {
                    CommandShell::Cmd => "cmd",
                    CommandShell::PowerShell => "powershell",
                };
                let cmd: String = command.chars().take(40).collect();
                format!("command({shell}): {cmd}")
            }
            ActionKind::Wait { milliseconds } => format!("wait: {milliseconds}ms"),
        };
        let mut line = format!("{}  [{}]", a.id, detail);
        if a.disabled {
            line.push_str("  (無効)");
        }
        line
    }

    /// Runs `profile` on a worker and posts the summary to `hwnd` as
    /// `done_msg` (lparam = boxed `(is_success, String)`, wparam =
    /// POST_MAGIC). Fires exactly once.
    fn spawn_test_launch(hwnd: HWND, profile: Profile, done_msg: u32) {
        // HWND is a raw pointer (not Send); pass it as usize and cast back.
        let hwnd = hwnd as usize;
        std::thread::spawn(move || {
            let hwnd = hwnd as HWND;
            let result = profile::execute_profile(&profile, &mut |a| {
                crate::actions::execute(a).map_err(|e| e.to_string())
            });
            let mut text = format!(
                "プロファイル '{}' のテスト実行が完了しました。\r\n",
                profile.name
            );
            for o in &result.outcomes {
                match &o.status {
                    profile::ActionStatus::Succeeded => {
                        text.push_str(&format!("  [ok]     {}\r\n", o.action_id))
                    }
                    profile::ActionStatus::Failed(e) => {
                        text.push_str(&format!("  [FAILED] {}: {e}\r\n", o.action_id))
                    }
                }
            }
            if result.skipped_disabled > 0 {
                text.push_str(&format!(
                    "  ({} 件の無効アクションをスキップ)\r\n",
                    result.skipped_disabled
                ));
            }
            text.push_str(&format!(
                "{} 件中 {} 件失敗",
                result.outcomes.len(),
                result.failure_count()
            ));
            let boxed = Box::into_raw(Box::new((result.is_success(), text)));
            // SAFETY: posting to a window handle; wparam tags the message
            // as ours so the receiver can reject forged lparams. If the
            // window is already gone the call fails and we reclaim the box
            // instead of leaking it.
            unsafe {
                if PostMessageW(hwnd, done_msg, POST_MAGIC, boxed as LPARAM) == 0 {
                    drop(Box::from_raw(boxed));
                }
            }
        });
    }

    /// The CLI run path shared by the GUI: forward to the resident when one
    /// is running, otherwise execute directly on a worker. `ok` in the
    /// posted payload picks the icon of the completion box — failures and
    /// IPC errors get the error icon, successful/forwarded runs the info
    /// one.
    fn spawn_run_profile(hwnd: HWND, id: String) {
        let hwnd = hwnd as usize;
        std::thread::spawn(move || {
            let hwnd = hwnd as HWND;
            let (ok, text) = match ipc::forward_run_profile(&id) {
                Ok(ipc::ForwardOutcome::Delivered) => (
                    true,
                    format!("プロファイル {id} の実行を Resident に転送しました。"),
                ),
                Ok(ipc::ForwardOutcome::Rejected) => (
                    false,
                    format!("Resident がプロファイル {id} の要求を拒否しました。"),
                ),
                Ok(ipc::ForwardOutcome::NoResident) => match Config::load_default() {
                    Err(e) => (false, format!("config の読み込みに失敗: {e}")),
                    Ok(config) => match profile::find_profile(&config, &id) {
                        None => (false, format!("プロファイル {id} が config にありません。")),
                        Some(p) => {
                            let result = profile::execute_profile(p, &mut |a| {
                                crate::actions::execute(a).map_err(|e| e.to_string())
                            });
                            (
                                result.is_success(),
                                format!(
                                    "プロファイル '{}' を直接実行しました: {} 件中 {} 件失敗。",
                                    p.name,
                                    result.outcomes.len(),
                                    result.failure_count()
                                ),
                            )
                        }
                    },
                },
                Err(e) => (false, format!("Resident への転送に失敗: {e}")),
            };
            let boxed = Box::into_raw(Box::new((ok, text)));
            // SAFETY: same PostMessageW contract as spawn_test_launch.
            unsafe {
                if PostMessageW(hwnd, WM_LAUNCH_DONE, POST_MAGIC, boxed as LPARAM) == 0 {
                    drop(Box::from_raw(boxed));
                }
            }
        });
    }

    // ---------- main window ----------

    struct MainState {
        config: Config,
        config_path: PathBuf,
        /// Mtime of the config file when it was loaded (None when it did
        /// not exist). save_config compares it against the on-disk mtime
        /// before overwriting, so an external edit made while this window
        /// is open is not silently discarded.
        config_mtime: Option<SystemTime>,
        /// Set while a run worker is in flight so the 実行 button and list
        /// double-clicks do not spawn a duplicate run; WM_LAUNCH_DONE
        /// clears it (same role as ProfileEdit::testing).
        running: bool,
        font: HFONT,
    }

    fn fill_profiles(hwnd: HWND, config: &Config, keep_sel: Option<usize>) {
        let items: Vec<String> = config
            .profiles
            .iter()
            .map(|p| format!("{}  ({} actions)", p.name, p.actions.len()))
            .collect();
        list_reset(hwnd, IDC_LIST, &items);
        if let Some(i) = keep_sel
            && i < items.len()
        {
            list_select(hwnd, IDC_LIST, i);
        }
    }

    /// Writes `candidate` to the config file. When the file's mtime differs
    /// from what this window loaded (None when it did not exist — e.g. the
    /// new-config path, or the file having been deleted meanwhile), an
    /// external edit happened while the GUI was open and an unconditional
    /// write would silently discard it, so the user is asked to confirm the
    /// overwrite first. On success the recorded mtime is refreshed. On
    /// failure (or a declined overwrite) false is returned and the error is
    /// shown; callers then keep `state.config` (and the UI) identical to
    /// the on-disk file instead of diverging.
    ///
    /// `state` is a raw pointer, not a &mut: the MessageBoxW below runs a
    /// nested loop where a posted WM_LAUNCH_DONE can write `running`
    /// through its own pointer, so borrows last single statements only.
    unsafe fn save_config(hwnd: HWND, state: *mut MainState, candidate: &Config) -> bool {
        let path = unsafe { &*state }.config_path.clone();
        let known = unsafe { &*state }.config_mtime;
        let disk = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if disk != known {
            let text = wide("設定ファイルが外部で変更されています。上書きしますか?");
            let title = wide("Project Launcher");
            // SAFETY: static strings, NUL-terminated by `wide`.
            let yes = unsafe {
                MessageBoxW(
                    hwnd,
                    text.as_ptr(),
                    title.as_ptr(),
                    MB_ICONWARNING | MB_YESNO,
                )
            };
            if yes != IDYES {
                return false;
            }
        }
        match candidate.save(&path) {
            Ok(()) => {
                // Record the fresh mtime so the next save compares against
                // the file we just wrote, not the previously loaded one.
                unsafe {
                    (*state).config_mtime =
                        std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                }
                true
            }
            Err(e) => {
                unsafe { error_box(hwnd, &format!("config の保存に失敗しました: {e}")) };
                false
            }
        }
    }

    /// Starts the selected profile on a worker. The `running` flag blocks a
    /// second launch while one is already in flight; it is cleared when the
    /// worker's WM_LAUNCH_DONE arrives.
    fn launch_selected(hwnd: HWND, state: *mut MainState) {
        if unsafe { (*state).running } {
            // SAFETY: hwnd is a live window owned by this thread.
            unsafe { info_box(hwnd, "実行中です。") };
            return;
        }
        let Some(i) = list_sel(hwnd, IDC_LIST) else {
            return;
        };
        let pid = unsafe { &*state }.config.profiles[i].id.clone();
        unsafe { (*state).running = true };
        spawn_run_profile(hwnd, pid);
    }

    unsafe fn main_command(hwnd: HWND, id: i32, code: u32) {
        let state = unsafe { state_of::<MainState>(hwnd) };
        if state.is_null() {
            return;
        }
        // `state` stays a raw pointer here: profile_editor, save_config,
        // shortcut_menu and MessageBoxW all run a nested message loop, and
        // a posted WM_LAUNCH_DONE dispatched there writes `running` through
        // its own pointer. A &mut held across such calls would alias that
        // write, so every branch re-borrows for single statements only.
        match id {
            IDC_LIST if code == LBN_DBLCLK => launch_selected(hwnd, state),
            IDC_RUN => launch_selected(hwnd, state),
            IDC_NEW => match new_guid() {
                Ok(guid) => {
                    let p = Profile {
                        id: guid,
                        name: "新しいプロファイル".to_string(),
                        actions: Vec::new(),
                    };
                    if let Some(p) = profile_editor(hwnd, p) {
                        // The modal loop above may have delivered posted
                        // messages; re-read the state pointer afterwards.
                        let state = unsafe { state_of::<MainState>(hwnd) };
                        if state.is_null() {
                            return;
                        }
                        let mut candidate = unsafe { &*state }.config.clone();
                        candidate.profiles.push(p);
                        if unsafe { save_config(hwnd, state, &candidate) } {
                            let state = unsafe { &mut *state };
                            state.config = candidate;
                            let last = state.config.profiles.len() - 1;
                            fill_profiles(hwnd, &state.config, None);
                            list_select(hwnd, IDC_LIST, last);
                        }
                    }
                }
                Err(e) => unsafe { error_box(hwnd, &e.to_string()) },
            },
            IDC_EDIT => {
                if let Some(i) = list_sel(hwnd, IDC_LIST) {
                    let p = unsafe { &*state }.config.profiles[i].clone();
                    if let Some(edited) = profile_editor(hwnd, p) {
                        let state = unsafe { state_of::<MainState>(hwnd) };
                        if state.is_null() {
                            return;
                        }
                        let mut candidate = unsafe { &*state }.config.clone();
                        candidate.profiles[i] = edited;
                        if unsafe { save_config(hwnd, state, &candidate) } {
                            let state = unsafe { &mut *state };
                            state.config = candidate;
                            fill_profiles(hwnd, &state.config, Some(i));
                        }
                    }
                }
            }
            IDC_DEL => {
                if let Some(i) = list_sel(hwnd, IDC_LIST) {
                    let name = unsafe { &*state }.config.profiles[i].name.clone();
                    let text = wide(&format!("プロファイル '{name}' を削除しますか?"));
                    let title = wide("Project Launcher");
                    let yes = unsafe {
                        MessageBoxW(
                            hwnd,
                            text.as_ptr(),
                            title.as_ptr(),
                            MB_ICONWARNING | MB_YESNO,
                        )
                    };
                    if yes == IDYES {
                        let state = unsafe { state_of::<MainState>(hwnd) };
                        if state.is_null() {
                            return;
                        }
                        let mut candidate = unsafe { &*state }.config.clone();
                        candidate.profiles.remove(i);
                        if unsafe { save_config(hwnd, state, &candidate) } {
                            let state = unsafe { &mut *state };
                            state.config = candidate;
                            fill_profiles(hwnd, &state.config, None);
                        }
                    }
                }
            }
            IDC_SHORTCUT => {
                if let Some(i) = list_sel(hwnd, IDC_LIST) {
                    let profile = unsafe { &*state }.config.profiles[i].clone();
                    unsafe { shortcut_menu(hwnd, &profile) };
                }
            }
            IDC_AUTOSTART => {
                let check =
                    unsafe { SendMessageW(GetDlgItem(hwnd, IDC_AUTOSTART), BM_GETCHECK, 0, 0) }
                        == BST_CHECKED as isize;
                if let Err(e) = autostart::set(check) {
                    unsafe {
                        error_box(hwnd, &format!("自動開始の設定に失敗しました: {e}"));
                        // Revert the checkbox to the actual state.
                        SendMessageW(
                            GetDlgItem(hwnd, IDC_AUTOSTART),
                            BM_SETCHECK,
                            if check { 0 } else { BST_CHECKED as usize },
                            0,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Popup under the shortcut button: Desktop / Start Menu / both.
    unsafe fn shortcut_menu(hwnd: HWND, profile: &Profile) {
        unsafe {
            let btn = GetDlgItem(hwnd, IDC_SHORTCUT);
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (id, label) in [
                (ID_SC_DESKTOP, "デスクトップ"),
                (ID_SC_STARTMENU, "スタートメニュー"),
                (ID_SC_BOTH, "両方"),
            ] {
                let w = wide(label);
                AppendMenuW(menu, MF_STRING, id, w.as_ptr());
            }
            let mut rc: RECT = zeroed();
            GetWindowRect(btn, &mut rc);
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                rc.left,
                rc.bottom,
                0,
                hwnd,
                ptr::null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            let locations: &[shortcut::ShortcutLocation] = match cmd as usize {
                ID_SC_DESKTOP => &[shortcut::ShortcutLocation::Desktop],
                ID_SC_STARTMENU => &[shortcut::ShortcutLocation::StartMenu],
                ID_SC_BOTH => &[
                    shortcut::ShortcutLocation::Desktop,
                    shortcut::ShortcutLocation::StartMenu,
                ],
                _ => return,
            };
            let mut text = String::new();
            let mut failed = false;
            for &loc in locations {
                match shortcut::create(&profile.name, &profile.id, loc) {
                    Ok(path) => text.push_str(&format!("作成: {}\r\n", path.display())),
                    Err(e) => {
                        failed = true;
                        text.push_str(&format!("失敗: {e}\r\n"));
                    }
                }
            }
            if failed {
                error_box(hwnd, &text);
            } else {
                info_box(hwnd, &text);
            }
        }
    }

    unsafe extern "system" fn main_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_NCCREATE => {
                    SetWindowLongPtrW(
                        hwnd,
                        GWLP_USERDATA,
                        create_param::<MainState>(lparam) as isize,
                    );
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
                WM_CREATE => {
                    let state = state_of::<MainState>(hwnd);
                    let font = if state.is_null() {
                        ptr::null_mut()
                    } else {
                        (*state).font
                    };
                    control(
                        hwnd,
                        &Ctl {
                            style: LBS_NOTIFY as u32 | WS_VSCROLL | WS_BORDER,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl("LISTBOX", "", 12, 12, 536, 300, IDC_LIST)
                        },
                        font,
                    );
                    let y = 322;
                    control(hwnd, &ctl("BUTTON", "実行", 12, y, 120, 30, IDC_RUN), font);
                    control(hwnd, &ctl("BUTTON", "新規", 142, y, 90, 30, IDC_NEW), font);
                    control(hwnd, &ctl("BUTTON", "編集", 242, y, 90, 30, IDC_EDIT), font);
                    control(hwnd, &ctl("BUTTON", "削除", 342, y, 90, 30, IDC_DEL), font);
                    control(
                        hwnd,
                        &ctl("BUTTON", "ショートカット", 442, y, 106, 30, IDC_SHORTCUT),
                        font,
                    );
                    let autostart = control(
                        hwnd,
                        &Ctl {
                            style: BS_AUTOCHECKBOX as u32,
                            ..ctl(
                                "BUTTON",
                                "Windows 起動時に常駐する",
                                12,
                                360,
                                260,
                                22,
                                IDC_AUTOSTART,
                            )
                        },
                        font,
                    );
                    if matches!(autostart::is_enabled(), Ok(true)) {
                        SendMessageW(autostart, BM_SETCHECK, BST_CHECKED as usize, 0);
                    }
                    if !state.is_null() {
                        let path = (*state).config_path.display().to_string();
                        control(
                            hwnd,
                            &ctl("STATIC", &path, 12, 392, 536, 20, IDC_STATUS),
                            font,
                        );
                        fill_profiles(hwnd, &(*state).config, None);
                    }
                    0
                }
                WM_COMMAND => {
                    main_command(hwnd, command_id(wparam), command_code(wparam));
                    0
                }
                WM_LAUNCH_DONE => {
                    // Ignore forged completions: only our workers know
                    // POST_MAGIC and always pass a non-null box, so anything
                    // else must not reach Box::from_raw.
                    if wparam == POST_MAGIC && lparam != 0 {
                        let state = state_of::<MainState>(hwnd);
                        if !state.is_null() {
                            (*state).running = false;
                        }
                        let (ok, text) = *Box::from_raw(lparam as *mut (bool, String));
                        if ok {
                            info_box(hwnd, &text);
                        } else {
                            error_box(hwnd, &text);
                        }
                    }
                    0
                }
                WM_CLOSE => {
                    DestroyWindow(hwnd);
                    0
                }
                WM_DESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    // ---------- profile editor ----------

    struct ProfileEdit {
        working: Profile,
        accepted: bool,
        /// Set while a test launch worker is in flight so a second click
        /// does not spawn a duplicate run.
        testing: bool,
    }

    fn fill_actions(hwnd: HWND, actions: &[Action], keep_sel: Option<usize>) {
        let items: Vec<String> = actions.iter().map(action_line).collect();
        list_reset(hwnd, IDC_PE_ACTIONS, &items);
        if let Some(i) = keep_sel
            && i < items.len()
        {
            list_select(hwnd, IDC_PE_ACTIONS, i);
        }
    }

    /// The next free `action-N` id inside the working profile.
    fn fresh_action_id(actions: &[Action]) -> String {
        let mut n = 1;
        while actions.iter().any(|a| a.id == format!("action{n}")) {
            n += 1;
        }
        format!("action{n}")
    }

    /// Opens the profile editor modally. Returns the edited profile on OK.
    fn profile_editor(parent: HWND, profile: Profile) -> Option<Profile> {
        let mut edit = ProfileEdit {
            working: profile,
            accepted: false,
            testing: false,
        };
        // SAFETY: `edit` outlives the modal loop; the window stores its
        // pointer in GWLP_USERDATA and clears it on WM_DESTROY.
        let hwnd = unsafe {
            let (x, y) = centered_rect(640, 470, parent);
            CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                wide(CLASS_PROFILE_EDIT).as_ptr(),
                wide("プロファイル編集").as_ptr(),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                x,
                y,
                640,
                470,
                parent,
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                &mut edit as *mut ProfileEdit as *const c_void,
            )
        };
        if hwnd.is_null() {
            return None;
        }
        unsafe { modal(parent, hwnd) };
        edit.accepted.then_some(edit.working)
    }

    unsafe fn pe_on_command(hwnd: HWND, id: i32, code: u32) {
        let state = unsafe { state_of::<ProfileEdit>(hwnd) };
        if state.is_null() {
            return;
        }
        // `state` stays a raw pointer here: action_editor (modal),
        // error_box and info_box all run a nested message loop where a
        // posted WM_TEST_DONE writes `testing` through its own pointer. A
        // &mut held across such calls would alias that write, so borrows
        // last single statements only and GWLP_USERDATA is read again after
        // each modal call.
        match id {
            IDC_PE_ACTIONS if code == LBN_DBLCLK => {
                if let Some(i) = list_sel(hwnd, IDC_PE_ACTIONS) {
                    edit_action(hwnd, i);
                }
            }
            IDC_PE_ADD => {
                let (id, ids) = {
                    let working = unsafe { &(*state).working };
                    (
                        fresh_action_id(&working.actions),
                        working
                            .actions
                            .iter()
                            .map(|a| a.id.clone())
                            .collect::<Vec<String>>(),
                    )
                };
                let mut action = Action {
                    id,
                    disabled: false,
                    kind: ActionKind::Application {
                        path: String::new(),
                        arguments: String::new(),
                        working_directory: None,
                    },
                };
                if action_editor(hwnd, &mut action, &ids, usize::MAX) {
                    // The modal loop may have delivered posted messages;
                    // re-read the state pointer afterwards.
                    let state = unsafe { state_of::<ProfileEdit>(hwnd) };
                    if !state.is_null() {
                        let state = unsafe { &mut *state };
                        state.working.actions.push(action);
                        fill_actions(hwnd, &state.working.actions, None);
                        list_select(hwnd, IDC_PE_ACTIONS, state.working.actions.len() - 1);
                    }
                }
            }
            IDC_PE_EDIT => {
                if let Some(i) = list_sel(hwnd, IDC_PE_ACTIONS) {
                    edit_action(hwnd, i);
                }
            }
            IDC_PE_DEL => {
                if let Some(i) = list_sel(hwnd, IDC_PE_ACTIONS) {
                    let state = unsafe { &mut *state };
                    state.working.actions.remove(i);
                    fill_actions(hwnd, &state.working.actions, None);
                }
            }
            IDC_PE_UP | IDC_PE_DOWN => {
                if let Some(i) = list_sel(hwnd, IDC_PE_ACTIONS) {
                    let state = unsafe { &mut *state };
                    let j = if id == IDC_PE_UP {
                        i.checked_sub(1)
                    } else {
                        (i + 1 < state.working.actions.len()).then_some(i + 1)
                    };
                    if let Some(j) = j {
                        state.working.actions.swap(i, j);
                        fill_actions(hwnd, &state.working.actions, Some(j));
                    }
                }
            }
            IDC_PE_TEST => {
                if unsafe { !(*state).testing } {
                    let profile = unsafe { &*state }.working.clone();
                    unsafe { (*state).testing = true };
                    spawn_test_launch(hwnd, profile, WM_TEST_DONE);
                }
            }
            IDOK => {
                // Write the name and validate first so the &mut ends before
                // the message boxes below spin their own loop.
                let (name_empty, invalid) = {
                    let state = unsafe { &mut *state };
                    state.working.name = dlg_text(hwnd, IDC_PE_NAME);
                    // Reuse full validation by wrapping in a one-profile
                    // config; this catches duplicate action ids and
                    // per-action rules.
                    let check = Config {
                        version: config::CURRENT_CONFIG_VERSION,
                        profiles: vec![state.working.clone()],
                    };
                    (state.working.name.is_empty(), check.validate().err())
                };
                if name_empty {
                    unsafe { error_box(hwnd, "名前を入力してください。") };
                    return;
                }
                if let Some(m) = invalid {
                    unsafe { error_box(hwnd, &format!("保存できません: {m}")) };
                    return;
                }
                let state = unsafe { state_of::<ProfileEdit>(hwnd) };
                if !state.is_null() {
                    unsafe { (*state).accepted = true };
                }
                unsafe { DestroyWindow(hwnd) };
            }
            IDCANCEL => unsafe {
                DestroyWindow(hwnd);
            },
            _ => {}
        }
    }

    /// Opens the action editor for `working.actions[index]`. Reads what the
    /// modal call needs up front, then re-borrows the state afterwards
    /// (see the aliasing note in pe_on_command).
    fn edit_action(hwnd: HWND, index: usize) {
        let state = unsafe { state_of::<ProfileEdit>(hwnd) };
        if state.is_null() {
            return;
        }
        let (ids, mut action) = {
            let working = unsafe { &(*state).working };
            (
                working
                    .actions
                    .iter()
                    .map(|a| a.id.clone())
                    .collect::<Vec<String>>(),
                working.actions[index].clone(),
            )
        };
        if action_editor(hwnd, &mut action, &ids, index) {
            let state = unsafe { state_of::<ProfileEdit>(hwnd) };
            if !state.is_null() {
                let state = unsafe { &mut *state };
                state.working.actions[index] = action;
                fill_actions(hwnd, &state.working.actions, Some(index));
            }
        }
    }

    unsafe extern "system" fn pe_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_NCCREATE => {
                    SetWindowLongPtrW(
                        hwnd,
                        GWLP_USERDATA,
                        create_param::<ProfileEdit>(lparam) as isize,
                    );
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
                WM_CREATE => {
                    // Font is borrowed from the owner window's state: the
                    // profile editor's lpParam is ProfileEdit, so fetch the
                    // font through the main state of the owner instead.
                    let owner = GetWindow(hwnd, GW_OWNER);
                    let font = {
                        let s = state_of::<MainState>(owner);
                        if s.is_null() {
                            ptr::null_mut()
                        } else {
                            (*s).font
                        }
                    };
                    let st = state_of::<ProfileEdit>(hwnd);
                    control(hwnd, &ctl("STATIC", "名前", 12, 14, 60, 20, -1), font);
                    control(
                        hwnd,
                        &Ctl {
                            style: ES_AUTOHSCROLL as u32,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl(
                                "EDIT",
                                if st.is_null() {
                                    ""
                                } else {
                                    &(*st).working.name
                                },
                                80,
                                12,
                                300,
                                24,
                                IDC_PE_NAME,
                            )
                        },
                        font,
                    );
                    control(hwnd, &ctl("STATIC", "ID", 392, 14, 30, 20, -1), font);
                    control(
                        hwnd,
                        &Ctl {
                            style: (ES_AUTOHSCROLL | ES_READONLY) as u32,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl(
                                "EDIT",
                                if st.is_null() { "" } else { &(*st).working.id },
                                424,
                                12,
                                204,
                                24,
                                IDC_PE_ID,
                            )
                        },
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("STATIC", "アクション", 12, 46, 100, 20, -1),
                        font,
                    );
                    control(
                        hwnd,
                        &Ctl {
                            style: LBS_NOTIFY as u32 | WS_VSCROLL | WS_BORDER,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl("LISTBOX", "", 12, 68, 480, 300, IDC_PE_ACTIONS)
                        },
                        font,
                    );
                    let bx = 504;
                    control(
                        hwnd,
                        &ctl("BUTTON", "追加", bx, 68, 124, 28, IDC_PE_ADD),
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("BUTTON", "編集", bx, 102, 124, 28, IDC_PE_EDIT),
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("BUTTON", "削除", bx, 136, 124, 28, IDC_PE_DEL),
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("BUTTON", "上へ", bx, 178, 124, 28, IDC_PE_UP),
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("BUTTON", "下へ", bx, 212, 124, 28, IDC_PE_DOWN),
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("BUTTON", "テスト実行", bx, 254, 124, 28, IDC_PE_TEST),
                        font,
                    );
                    control(hwnd, &ctl("BUTTON", "OK", 384, 396, 120, 30, IDOK), font);
                    control(
                        hwnd,
                        &ctl("BUTTON", "キャンセル", 512, 396, 116, 30, IDCANCEL),
                        font,
                    );
                    if !st.is_null() {
                        fill_actions(hwnd, &(*st).working.actions, None);
                    }
                    0
                }
                WM_COMMAND => {
                    pe_on_command(hwnd, command_id(wparam), command_code(wparam));
                    0
                }
                m if m == WM_TEST_DONE => {
                    // Same forged-message guard as the main window's
                    // WM_LAUNCH_DONE handler.
                    if wparam == POST_MAGIC && lparam != 0 {
                        let st = state_of::<ProfileEdit>(hwnd);
                        if !st.is_null() {
                            (*st).testing = false;
                        }
                        let (ok, text) = *Box::from_raw(lparam as *mut (bool, String));
                        if ok {
                            info_box(hwnd, &text);
                        } else {
                            error_box(hwnd, &text);
                        }
                    }
                    0
                }
                WM_CLOSE => {
                    DestroyWindow(hwnd);
                    0
                }
                WM_DESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    // ---------- action editor ----------

    struct ActionEdit {
        accepted: bool,
        /// Action being built; committed back to the caller on OK.
        action: Action,
        /// Ids already used by other actions in the same profile.
        other_ids: Vec<String>,
    }

    /// Opens the action editor modally; `action` holds the initial value and
    /// receives the result on OK. `other_ids` are the ids the action must not
    /// collide with; `self_index` is its position in the profile (or MAX for
    /// a new action).
    fn action_editor(
        parent: HWND,
        action: &mut Action,
        other_ids: &[String],
        self_index: usize,
    ) -> bool {
        let mut ids = other_ids.to_vec();
        if self_index < ids.len() {
            ids.remove(self_index);
        }
        let mut edit = ActionEdit {
            accepted: false,
            action: action.clone(),
            other_ids: ids,
        };
        // SAFETY: same stack-context pattern as profile_editor.
        let hwnd = unsafe {
            let (x, y) = centered_rect(500, 350, parent);
            CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                wide(CLASS_ACTION_EDIT).as_ptr(),
                wide("アクション編集").as_ptr(),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                x,
                y,
                500,
                350,
                parent,
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                &mut edit as *mut ActionEdit as *const c_void,
            )
        };
        if hwnd.is_null() {
            return false;
        }
        unsafe { modal(parent, hwnd) };
        if edit.accepted {
            *action = edit.action;
        }
        edit.accepted
    }

    fn kind_index(kind: &ActionKind) -> usize {
        match kind {
            ActionKind::Application { .. } => 0,
            ActionKind::Folder { .. } => 1,
            ActionKind::File { .. } => 2,
            ActionKind::Url { .. } => 3,
            ActionKind::Command { .. } => 4,
            ActionKind::Wait { .. } => 5,
        }
    }

    /// Which picker the 参照 button opens for a type, if any.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Browse {
        File,
        Folder,
    }

    /// Which of the three generic fields a type uses, whether the shell
    /// combo is relevant, and which picker the browse button opens. Labels
    /// are set on the static controls.
    fn field_layout(
        type_index: usize,
    ) -> (
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        bool,
        Option<Browse>,
    ) {
        match type_index {
            0 => (
                Some("パス"),
                Some("引数"),
                Some("作業フォルダ (省略可)"),
                false,
                Some(Browse::File),
            ),
            1 => (
                Some("フォルダのパス"),
                None,
                None,
                false,
                Some(Browse::Folder),
            ),
            2 => (
                Some("ファイルのパス"),
                None,
                None,
                false,
                Some(Browse::File),
            ),
            3 => (Some("URL"), None, None, false, None),
            4 => (Some("コマンド"), None, None, true, None),
            5 => (Some("待機時間 (ms)"), None, None, false, None),
            _ => (None, None, None, false, None),
        }
    }

    /// Shows/enables the field set for the selected type and fills it from
    /// the current action when the type still matches.
    unsafe fn ae_apply_type(hwnd: HWND, keep_fields: bool) {
        let type_index =
            unsafe { SendMessageW(GetDlgItem(hwnd, IDC_AE_TYPE), CB_GETCURSEL, 0, 0) } as usize;
        let (l1, l2, l3, shell, browse) = field_layout(type_index);
        for (label_id, field_id, label) in [
            (IDC_AE_L1, IDC_AE_F1, l1),
            (IDC_AE_L2, IDC_AE_F2, l2),
            (IDC_AE_L3, IDC_AE_F3, l3),
        ] {
            match label {
                Some(text) => {
                    set_text(hwnd, label_id, text);
                    set_item_visible(hwnd, label_id, true);
                    set_item_visible(hwnd, field_id, true);
                    set_item_enabled(hwnd, field_id, true);
                }
                None => {
                    set_item_visible(hwnd, label_id, false);
                    set_item_visible(hwnd, field_id, false);
                }
            }
        }
        set_item_visible(hwnd, IDC_AE_LS, shell);
        set_item_visible(hwnd, IDC_AE_SHELL, shell);
        set_item_visible(hwnd, IDC_AE_BROWSE, browse.is_some());
        if !keep_fields {
            for id in [IDC_AE_F1, IDC_AE_F2, IDC_AE_F3] {
                set_text(hwnd, id, "");
            }
        }
    }

    /// Builds the ActionKind for `type_index` from the dialog fields.
    fn build_kind(hwnd: HWND, type_index: usize) -> Result<ActionKind, String> {
        let f1 = dlg_text(hwnd, IDC_AE_F1);
        match type_index {
            0 => Ok(ActionKind::Application {
                path: f1,
                arguments: dlg_text(hwnd, IDC_AE_F2),
                working_directory: {
                    let d = dlg_text(hwnd, IDC_AE_F3);
                    if d.is_empty() { None } else { Some(d) }
                },
            }),
            1 => Ok(ActionKind::Folder { path: f1 }),
            2 => Ok(ActionKind::File { path: f1 }),
            3 => Ok(ActionKind::Url { url: f1 }),
            4 => {
                let shell_index =
                    unsafe { SendMessageW(GetDlgItem(hwnd, IDC_AE_SHELL), CB_GETCURSEL, 0, 0) };
                let shell = if shell_index == 1 {
                    CommandShell::PowerShell
                } else {
                    CommandShell::Cmd
                };
                Ok(ActionKind::Command { shell, command: f1 })
            }
            5 => f1
                .trim()
                .parse::<u64>()
                .map(|milliseconds| ActionKind::Wait { milliseconds })
                .map_err(|_| "待機時間はミリ秒の数値で入力してください。".to_string()),
            _ => Err("不明な種類です。".to_string()),
        }
    }

    unsafe fn ae_on_command(hwnd: HWND, id: i32, code: u32) {
        let state = unsafe { state_of::<ActionEdit>(hwnd) };
        if state.is_null() {
            return;
        }
        let state = unsafe { &mut *state };
        match id {
            IDC_AE_TYPE if code == CBN_SELCHANGE => {
                unsafe { ae_apply_type(hwnd, false) };
            }
            IDC_AE_BROWSE => {
                let type_index =
                    unsafe { SendMessageW(GetDlgItem(hwnd, IDC_AE_TYPE), CB_GETCURSEL, 0, 0) }
                        as usize;
                let picked = match field_layout(type_index).4 {
                    Some(Browse::File) => unsafe {
                        browse_file(
                            hwnd,
                            "実行ファイル (*.exe)\0*.exe\0すべてのファイル (*.*)\0*.*\0",
                        )
                    },
                    Some(Browse::Folder) => unsafe {
                        browse_folder(hwnd, "フォルダを選択してください")
                    },
                    None => None,
                };
                if let Some(path) = picked {
                    set_text(hwnd, IDC_AE_F1, &path);
                }
            }
            IDOK => {
                let id_text = dlg_text(hwnd, IDC_AE_ID);
                if id_text.is_empty() {
                    unsafe { error_box(hwnd, "ID を入力してください。") };
                    return;
                }
                if state.other_ids.contains(&id_text) {
                    unsafe {
                        error_box(
                            hwnd,
                            &format!("ID '{id_text}' はこのプロファイル内で使用済みです。"),
                        )
                    };
                    return;
                }
                let type_index =
                    unsafe { SendMessageW(GetDlgItem(hwnd, IDC_AE_TYPE), CB_GETCURSEL, 0, 0) }
                        as usize;
                let kind = match build_kind(hwnd, type_index) {
                    Ok(k) => k,
                    Err(m) => {
                        unsafe { error_box(hwnd, &m) };
                        return;
                    }
                };
                let action = Action {
                    id: id_text,
                    disabled: unsafe {
                        SendMessageW(GetDlgItem(hwnd, IDC_AE_DISABLED), BM_GETCHECK, 0, 0)
                    } == BST_CHECKED as isize,
                    kind,
                };
                // Per-action rules (empty path, bad URL scheme, wait bound).
                if let Err(m) = config::validate_action(&action) {
                    unsafe { error_box(hwnd, &format!("保存できません: {m}")) };
                    return;
                }
                state.action = action;
                state.accepted = true;
                unsafe { DestroyWindow(hwnd) };
            }
            IDCANCEL => unsafe {
                DestroyWindow(hwnd);
            },
            _ => {}
        }
    }

    unsafe extern "system" fn ae_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_NCCREATE => {
                    SetWindowLongPtrW(
                        hwnd,
                        GWLP_USERDATA,
                        create_param::<ActionEdit>(lparam) as isize,
                    );
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
                WM_CREATE => {
                    let owner = GetWindow(hwnd, GW_OWNER);
                    // The owner is the profile editor; its own owner holds
                    // the font. Walk up once more for the MainState.
                    let grand = if owner.is_null() {
                        ptr::null_mut()
                    } else {
                        GetWindow(owner, GW_OWNER)
                    };
                    let font = {
                        let s = state_of::<MainState>(grand);
                        if s.is_null() {
                            ptr::null_mut()
                        } else {
                            (*s).font
                        }
                    };
                    let st = state_of::<ActionEdit>(hwnd);

                    control(hwnd, &ctl("STATIC", "ID", 12, 14, 60, 20, -1), font);
                    control(
                        hwnd,
                        &Ctl {
                            style: ES_AUTOHSCROLL as u32,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl(
                                "EDIT",
                                if st.is_null() { "" } else { &(*st).action.id },
                                80,
                                12,
                                200,
                                24,
                                IDC_AE_ID,
                            )
                        },
                        font,
                    );
                    control(hwnd, &ctl("STATIC", "種類", 292, 14, 50, 20, -1), font);
                    let combo = control(
                        hwnd,
                        &Ctl {
                            style: CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
                            ..ctl("COMBOBOX", "", 344, 10, 140, 200, IDC_AE_TYPE)
                        },
                        font,
                    );
                    for name in TYPE_NAMES {
                        let w = wide(name);
                        SendMessageW(combo, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
                    }
                    control(
                        hwnd,
                        &Ctl {
                            style: BS_AUTOCHECKBOX as u32,
                            ..ctl(
                                "BUTTON",
                                "無効 (このアクションをスキップ)",
                                12,
                                46,
                                300,
                                22,
                                IDC_AE_DISABLED,
                            )
                        },
                        font,
                    );
                    control(hwnd, &ctl("STATIC", "", 12, 82, 160, 20, IDC_AE_L1), font);
                    control(
                        hwnd,
                        &Ctl {
                            style: ES_AUTOHSCROLL as u32,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl("EDIT", "", 12, 104, 472, 24, IDC_AE_F1)
                        },
                        font,
                    );
                    control(hwnd, &ctl("STATIC", "", 12, 138, 160, 20, IDC_AE_L2), font);
                    control(
                        hwnd,
                        &Ctl {
                            style: ES_AUTOHSCROLL as u32,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl("EDIT", "", 12, 160, 472, 24, IDC_AE_F2)
                        },
                        font,
                    );
                    control(hwnd, &ctl("STATIC", "", 12, 194, 160, 20, IDC_AE_L3), font);
                    control(
                        hwnd,
                        &Ctl {
                            style: ES_AUTOHSCROLL as u32,
                            ex: WS_EX_CLIENTEDGE,
                            ..ctl("EDIT", "", 12, 216, 472, 24, IDC_AE_F3)
                        },
                        font,
                    );
                    // The shell controls sit on the L2/F2 row: that row is
                    // only visible for the application type, which never
                    // shows the shell combo, while the command type that
                    // needs it leaves the row free. Placing them on the
                    // F1 row would overlap the コマンド label.
                    control(
                        hwnd,
                        &ctl("STATIC", "シェル", 12, 138, 60, 20, IDC_AE_LS),
                        font,
                    );
                    control(
                        hwnd,
                        &ctl("BUTTON", "参照...", 370, 80, 114, 22, IDC_AE_BROWSE),
                        font,
                    );
                    let shell_combo = control(
                        hwnd,
                        &Ctl {
                            style: CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
                            ..ctl("COMBOBOX", "", 80, 136, 140, 160, IDC_AE_SHELL)
                        },
                        font,
                    );
                    for name in SHELL_NAMES {
                        let w = wide(name);
                        SendMessageW(shell_combo, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
                    }
                    SendMessageW(shell_combo, CB_SETCURSEL, 0, 0);
                    control(hwnd, &ctl("BUTTON", "OK", 240, 268, 110, 30, IDOK), font);
                    control(
                        hwnd,
                        &ctl("BUTTON", "キャンセル", 360, 268, 110, 30, IDCANCEL),
                        font,
                    );

                    if !st.is_null() {
                        let a = &(*st).action;
                        SendMessageW(combo, CB_SETCURSEL, kind_index(&a.kind), 0);
                        // Fill the generic fields from the current kind.
                        match &a.kind {
                            ActionKind::Application {
                                path,
                                arguments,
                                working_directory,
                            } => {
                                set_text(hwnd, IDC_AE_F1, path);
                                set_text(hwnd, IDC_AE_F2, arguments);
                                if let Some(d) = working_directory {
                                    set_text(hwnd, IDC_AE_F3, d);
                                }
                            }
                            ActionKind::Folder { path } | ActionKind::File { path } => {
                                set_text(hwnd, IDC_AE_F1, path)
                            }
                            ActionKind::Url { url } => set_text(hwnd, IDC_AE_F1, url),
                            ActionKind::Command { shell, command } => {
                                set_text(hwnd, IDC_AE_F1, command);
                                SendMessageW(
                                    shell_combo,
                                    CB_SETCURSEL,
                                    match shell {
                                        CommandShell::Cmd => 0,
                                        CommandShell::PowerShell => 1,
                                    },
                                    0,
                                );
                            }
                            ActionKind::Wait { milliseconds } => {
                                set_text(hwnd, IDC_AE_F1, &milliseconds.to_string())
                            }
                        }
                        if a.disabled {
                            SendMessageW(
                                GetDlgItem(hwnd, IDC_AE_DISABLED),
                                BM_SETCHECK,
                                BST_CHECKED as usize,
                                0,
                            );
                        }
                    }
                    ae_apply_type(hwnd, true);
                    0
                }
                WM_COMMAND => {
                    ae_on_command(hwnd, command_id(wparam), command_code(wparam));
                    0
                }
                WM_CLOSE => {
                    DestroyWindow(hwnd);
                    0
                }
                WM_DESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    // ---------- entry ----------

    fn register_class(name: &str, proc: WNDPROC) -> Result<(), GuiError> {
        // SAFETY: standard class registration; the window proc outlives the
        // class and the background uses the stock window color.
        unsafe {
            let class_name = wide(name);
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: proc,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(ptr::null()),
                hIcon: LoadIconW(ptr::null_mut(), IDI_APPLICATION),
                hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW as usize + 1) as HBRUSH,
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            if RegisterClassW(&wc) == 0 {
                return Err(win32("RegisterClassW"));
            }
            Ok(())
        }
    }

    pub fn run() -> Result<(), GuiError> {
        // Load config; a missing file offers to start from an empty one.
        let config_path = config::config_path().map_err(GuiError::Config)?;
        // Snapshot the mtime before reading so save_config can later detect
        // an external edit; a missing file stays None (the new-config path
        // below is then treated as "file did not exist").
        let config_mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();
        let config = match Config::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                let missing = matches!(
                    &e,
                    ConfigError::Io { source, .. }
                        if source.kind() == std::io::ErrorKind::NotFound
                );
                if !missing {
                    // With windows_subsystem = "windows" there is no console
                    // to print to: returning the Err silently would make an
                    // Explorer double-click look like the app did nothing.
                    let text = wide(&format!("設定ファイルを読み込めません。\r\n{e}"));
                    let title = wide("Project Launcher");
                    // SAFETY: static strings, NUL-terminated by `wide`.
                    unsafe {
                        MessageBoxW(
                            ptr::null_mut(),
                            text.as_ptr(),
                            title.as_ptr(),
                            MB_ICONERROR | MB_OK,
                        )
                    };
                    return Err(GuiError::Config(e));
                }
                let text = wide(&format!(
                    "設定ファイルが見つかりません。\r\n{}\r\n\r\n新規作成しますか?",
                    config_path.display()
                ));
                let title = wide("Project Launcher");
                // SAFETY: static strings, NUL-terminated.
                let r = unsafe {
                    MessageBoxW(
                        ptr::null_mut(),
                        text.as_ptr(),
                        title.as_ptr(),
                        MB_ICONQUESTION | MB_YESNO,
                    )
                };
                if r != IDYES {
                    return Err(GuiError::Config(e));
                }
                Config {
                    version: config::CURRENT_CONFIG_VERSION,
                    profiles: Vec::new(),
                }
            }
        };

        register_class(CLASS_MAIN, Some(main_wnd_proc))?;
        register_class(CLASS_PROFILE_EDIT, Some(pe_wnd_proc))?;
        register_class(CLASS_ACTION_EDIT, Some(ae_wnd_proc))?;

        let font = message_font();
        let mut state = MainState {
            config,
            config_path,
            config_mtime,
            running: false,
            font,
        };

        // SAFETY: `state` lives on this stack frame for the whole message
        // loop; the window clears GWLP_USERDATA on WM_DESTROY.
        let hwnd = unsafe {
            let (x, y) = centered_rect(576, 460, ptr::null_mut());
            CreateWindowExW(
                0,
                wide(CLASS_MAIN).as_ptr(),
                wide("Project Launcher").as_ptr(),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
                x,
                y,
                576,
                460,
                ptr::null_mut(),
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                &mut state as *mut MainState as *const c_void,
            )
        };
        if hwnd.is_null() {
            // The normal DeleteObject below is unreachable on this path, so
            // release the font here instead of leaking the GDI object.
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
            return Err(win32("CreateWindowExW"));
        }

        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            let mut msg: MSG = zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !font.is_null() {
                DeleteObject(font);
            }
            // Balance the RegisterClassW calls above. Failures are ignored:
            // the process is exiting anyway, and no windows remain.
            for name in [CLASS_MAIN, CLASS_PROFILE_EDIT, CLASS_ACTION_EDIT] {
                UnregisterClassW(wide(name).as_ptr(), GetModuleHandleW(ptr::null()));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use imp::run;
