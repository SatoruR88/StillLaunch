//! Resident mode: a hidden window with a tray icon that runs profiles on
//! request. It sleeps in GetMessageW and does nothing between requests (no
//! timers, no file watching); the config is read fresh for every request.
//!
//! Requests come from `--profile` via WM_COPYDATA: `dwData` = MAGIC, data =
//! the profile GUID as UTF-16. Each accepted request runs on its own worker
//! thread; with panic=unwind a panicking worker only ends that thread.

use crate::config::{self, Profile};
use crate::run;
use crate::win::{last_error, wide};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, ReleaseMutex, SetProcessWorkingSetSize,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "ProjectLauncher.Resident";
/// Session-local, so each signed-in user can have one resident.
const MUTEX: &str = r"Local\ProjectLauncher.Resident";
/// "PL" + protocol version 2 (version 1 sent JSON).
const MAGIC: usize = 0x504C_0002;
const WM_TRAY: u32 = WM_APP + 1;
const MAX_WORKERS: usize = 16;
const CMD_EDIT: usize = 1;
const CMD_EXIT: usize = 2;
const CMD_PROFILE: usize = 100;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static WORKERS: AtomicUsize = AtomicUsize::new(0);

pub enum Forward {
    NoResident,
    Accepted,
    Rejected,
}

/// Hands a run request to a running resident, if there is one.
pub fn forward(id: &str) -> Forward {
    let class = wide(CLASS);
    let hwnd = unsafe { FindWindowW(class.as_ptr(), null()) };
    if hwnd.is_null() {
        return Forward::NoResident;
    }
    let data = wide(id);
    let cds = COPYDATASTRUCT {
        dwData: MAGIC,
        cbData: (data.len() * 2) as u32,
        lpData: data.as_ptr() as *mut c_void,
    };
    let mut result = 0usize;
    // Must be a synchronous send: `data` only lives until this returns.
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            0,
            &cds as *const COPYDATASTRUCT as LPARAM,
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            3000,
            &mut result,
        )
    };
    if sent != 0 && result != 0 { Forward::Accepted } else { Forward::Rejected }
}

/// Runs until "Exit" is chosen. `Ok(false)` = another resident is running.
pub fn run() -> Result<bool, String> {
    let name = wide(MUTEX);
    let mutex = unsafe { CreateMutexW(null(), 1, name.as_ptr()) };
    if mutex.is_null() {
        return Err(last_error("CreateMutexW"));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return Ok(false);
    }
    let result = unsafe { message_loop() };
    unsafe {
        ReleaseMutex(mutex);
        CloseHandle(mutex);
    }
    result.map(|()| true)
}

unsafe fn message_loop() -> Result<(), String> {
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = wide(CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        if RegisterClassW(&wc) == 0 {
            return Err(last_error("RegisterClassW"));
        }
        // A hidden top-level window rather than a message-only one: only
        // top-level windows receive the TaskbarCreated broadcast, and the
        // tray menu needs a window that can become foreground.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(last_error("CreateWindowExW"));
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
        // May fail before Explorer is up (logon autostart); TaskbarCreated
        // adds the icon later.
        tray(hwnd, NIM_ADD);
        // Idle for hours: hand the start-up pages back to the system.
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

unsafe fn tray(hwnd: HWND, op: NOTIFY_ICON_MESSAGE) {
    unsafe {
        let mut d: NOTIFYICONDATAW = std::mem::zeroed();
        d.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = hwnd;
        d.uID = 1;
        d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        for (dst, src) in d.szTip.iter_mut().zip("StillLaunch".encode_utf16()) {
            *dst = src;
        }
        Shell_NotifyIconW(op, &d);
    }
}

/// Starts `p` on a worker thread. False when too many are running or the
/// thread cannot be created. Never panics (it runs inside the window proc).
fn start(p: Profile) -> bool {
    let claimed = WORKERS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < MAX_WORKERS).then_some(n + 1));
    if claimed.is_err() {
        return false;
    }
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            WORKERS.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let spawned = std::thread::Builder::new().spawn(move || {
        let _slot = Slot; // released even if the profile panics
        let report = run::run_profile(&p);
        eprintln!("[resident] {}", run::summary(&p, &report));
    });
    if spawned.is_err() {
        WORKERS.fetch_sub(1, Ordering::Relaxed);
    }
    spawned.is_ok()
}

fn start_by_id(id: &str) -> bool {
    match config::load_default() {
        Ok(c) => c.find(id).cloned().is_some_and(start),
        Err(e) => {
            eprintln!("[resident] {e}");
            false
        }
    }
}

unsafe fn on_copy_data(lparam: LPARAM) -> LRESULT {
    let cds = lparam as *const COPYDATASTRUCT;
    if cds.is_null() {
        return 0;
    }
    let cds = unsafe { &*cds };
    if cds.dwData != MAGIC || cds.lpData.is_null() || cds.cbData == 0 || cds.cbData > 256 || cds.cbData % 2 != 0 {
        return 0;
    }
    // Copied out before returning, as WM_COPYDATA requires.
    let units = unsafe { std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2) };
    let id = String::from_utf16_lossy(units);
    let id = id.trim_end_matches('\0');
    (config::is_guid(id) && start_by_id(id)) as LRESULT
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let cfg = config::load_default();
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        match &cfg {
            Ok(c) => {
                for (i, p) in c.profiles.iter().enumerate().take(200) {
                    let label = wide(&p.name.replace('&', "&&"));
                    AppendMenuW(menu, MF_STRING, CMD_PROFILE + i, label.as_ptr());
                }
            }
            Err(_) => {
                let label = wide("設定ファイルを読み込めません");
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
            }
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        let (edit, exit) = (wide("編集..."), wide("終了"));
        AppendMenuW(menu, MF_STRING, CMD_EDIT, edit.as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_EXIT, exit.as_ptr());
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, null()) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd {
            CMD_EDIT => open_editor(),
            CMD_EXIT => {
                DestroyWindow(hwnd);
            }
            n if n >= CMD_PROFILE => {
                if let Some(p) = cfg.ok().and_then(|c| c.profiles.into_iter().nth(n - CMD_PROFILE)) {
                    start(p);
                }
            }
            _ => {}
        }
    }
}

fn open_editor() {
    if let Err(e) = crate::win::exe_path().and_then(|exe| run::spawn(&run::command_line(&exe, ""), "")) {
        eprintln!("[resident] cannot open the editor: {e}");
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COPYDATA => on_copy_data(lparam),
            WM_TRAY => {
                match lparam as u32 & 0xFFFF {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONDBLCLK => open_editor(),
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                tray(hwnd, NIM_DELETE);
                PostQuitMessage(0);
                0
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                tray(hwnd, NIM_ADD);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
