//! Profile editor: one fixed-size window. Left: profile list. Right: the
//! selected profile's name, its steps and a form for one step. Plain Win32
//! controls on a light grey background, flat owner-drawn buttons, one
//! accent colour for the primary actions (Run, Save).
//!
//! State lives in a thread-local `RefCell` that is only ever borrowed
//! briefly through `with`, never across a Win32 call that can re-enter the
//! window procedure; `try_borrow_mut` keeps a mistake from panicking inside
//! the window procedure.

use crate::config::{self, Config, Kind, Profile, Step};
use crate::run;
use crate::win::{self, from_wide, wide};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DrawFocusRect,
    DrawTextW, FillRect, FrameRect, HBRUSH, HDC, HGDIOBJ, SelectObject, SetBkColor, SetBkMode,
    SetTextColor, UpdateWindow,
};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, DRAWITEMSTRUCT, EM_SETSEL, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Palette (COLORREF = 0x00BBGGRR).
const BG: u32 = 0x00F8_F7F7; // #F7F7F8 window
const PANEL: u32 = 0x00FF_FFFF; // #FFFFFF fields, lists
const TEXT: u32 = 0x002A_2522; // #22252A
const SUB: u32 = 0x0063_5B57; // #575B63 status line
const LINE: u32 = 0x00E7_E5E5; // #E5E5E7 borders, pressed
const MUTED: u32 = 0x00AD_A8A6; // #A6A8AD disabled text
const ACCENT: u32 = 0x00D6_6C33; // #336CD6 primary buttons
const ACCENT_DOWN: u32 = 0x00B0_592A; // pressed primary

const W: i32 = 720;
const H: i32 = 472;

const ID_OK: i32 = 1; // Enter (sent by IsDialogMessageW)
const ID_PHEAD: i32 = 100;
const ID_PROFILES: i32 = 101;
const ID_NEW: i32 = 102;
const ID_PDEL: i32 = 103;
const ID_RUN: i32 = 104;
const ID_SHORTCUT: i32 = 105;
const ID_NAME: i32 = 110;
const ID_SHEAD: i32 = 111;
const ID_STEPS: i32 = 112;
const ID_KIND: i32 = 113;
const ID_SHELL: i32 = 114;
const ID_OFF: i32 = 115;
const ID_TLABEL: i32 = 116;
const ID_TARGET: i32 = 117;
const ID_BROWSE: i32 = 118;
const ID_ARGS: i32 = 119;
const ID_DIR: i32 = 120;
const ID_ADD: i32 = 121;
const ID_SET: i32 = 122;
const ID_SDEL: i32 = 123;
const ID_UP: i32 = 124;
const ID_DOWN: i32 = 125;
const ID_AUTO: i32 = 130;
const ID_SAVE: i32 = 131;
const ID_STATUS: i32 = 132;

const CLICKED: u32 = 0; // BN_CLICKED
const WM_DONE: u32 = WM_APP + 1;

const BTN: u32 = BS_OWNERDRAW as u32 | WS_TABSTOP;
const EDT: u32 = ES_AUTOHSCROLL as u32 | WS_BORDER | WS_TABSTOP;
const LST: u32 = LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
const CMB: u32 = CBS_DROPDOWNLIST as u32 | WS_VSCROLL | WS_TABSTOP;
const CHK: u32 = BS_AUTOCHECKBOX as u32 | WS_TABSTOP;

/// class, text, style, x, y, w, h, id — in tab order.
type Spec = (&'static str, &'static str, u32, i32, i32, i32, i32, i32);
const CONTROLS: &[Spec] = &[
    ("STATIC", "", 0, 16, 16, 200, 20, ID_PHEAD),
    ("LISTBOX", "", LST, 16, 40, 200, 320, ID_PROFILES),
    ("BUTTON", "新規", BTN, 16, 368, 96, 28, ID_NEW),
    ("BUTTON", "削除", BTN, 120, 368, 96, 28, ID_PDEL),
    ("BUTTON", "実行", BTN, 16, 404, 96, 28, ID_RUN),
    ("BUTTON", "ショートカット", BTN, 120, 404, 96, 28, ID_SHORTCUT),
    ("STATIC", "名前", 0, 236, 16, 76, 20, 0),
    ("EDIT", "", EDT, 316, 12, 388, 24, ID_NAME),
    ("STATIC", "", 0, 236, 48, 300, 20, ID_SHEAD),
    ("LISTBOX", "", LST | LBS_USETABSTOPS as u32, 236, 72, 468, 152, ID_STEPS),
    ("STATIC", "種類", 0, 236, 236, 76, 20, 0),
    ("COMBOBOX", "", CMB, 316, 232, 136, 200, ID_KIND),
    ("COMBOBOX", "", CMB, 460, 232, 120, 120, ID_SHELL),
    ("BUTTON", "無効", CHK, 596, 234, 108, 22, ID_OFF),
    ("STATIC", "", 0, 236, 268, 76, 20, ID_TLABEL),
    ("EDIT", "", EDT, 316, 264, 352, 24, ID_TARGET),
    ("BUTTON", "…", BTN, 676, 264, 28, 24, ID_BROWSE),
    ("STATIC", "引数", 0, 236, 300, 76, 20, 0),
    ("EDIT", "", EDT, 316, 296, 388, 24, ID_ARGS),
    ("STATIC", "作業フォルダ", 0, 236, 332, 76, 20, 0),
    ("EDIT", "", EDT, 316, 328, 388, 24, ID_DIR),
    ("BUTTON", "追加", BTN, 316, 368, 76, 28, ID_ADD),
    ("BUTTON", "更新", BTN, 396, 368, 76, 28, ID_SET),
    ("BUTTON", "削除", BTN, 476, 368, 76, 28, ID_SDEL),
    ("BUTTON", "↑", BTN, 556, 368, 36, 28, ID_UP),
    ("BUTTON", "↓", BTN, 596, 368, 36, 28, ID_DOWN),
    ("BUTTON", "Windows 起動時に常駐", CHK, 236, 408, 240, 22, ID_AUTO),
    ("BUTTON", "保存", BTN, 608, 404, 96, 28, ID_SAVE),
    ("STATIC", "", 0, 16, 444, 688, 20, ID_STATUS),
];

/// Combo labels, in `config::KINDS` order.
const KIND_LABELS: [&str; 6] = ["アプリ", "フォルダ", "ファイル", "URL", "コマンド", "待機"];
const TARGET_LABELS: [&str; 6] = ["プログラム", "フォルダ", "ファイル", "URL", "コマンド", "ミリ秒"];

struct App {
    cfg: Config,
    path: PathBuf,
    /// mtime when loaded/saved, to notice edits made by someone else.
    mtime: Option<SystemTime>,
    dirty: bool,
    cur: Option<usize>,
}

/// GDI handles (as usize so the static is Sync). Freed at process exit.
struct Theme {
    font: usize,
    bg: usize,
    panel: usize,
    line: usize,
    accent: usize,
    accent_down: usize,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}
static THEME: OnceLock<Theme> = OnceLock::new();
/// Summaries from finished run threads, picked up on WM_DONE.
static DONE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        (*guard).as_mut().map(f)
    })
}

pub fn run() -> Result<(), String> {
    let path = config::path()?;
    let cfg = if path.exists() { config::load(&path) } else { Ok(Config::default()) };
    let cfg = cfg.inspect_err(|e| {
        win::message(null_mut(), &format!("設定ファイルを読み込めません。\n{e}"), MB_OK | MB_ICONERROR);
    })?;
    let first = (!cfg.profiles.is_empty()).then_some(0);
    let mtime = config::mtime(&path);
    let shown_path = path.display().to_string();
    APP.with(|a| *a.borrow_mut() = Some(App { cfg, path, mtime, dirty: false, cur: first }));

    unsafe {
        // STA for the folder picker and the shortcut COM objects.
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let font = message_font();
        let theme = THEME.get_or_init(|| Theme {
            font,
            bg: CreateSolidBrush(BG) as usize,
            panel: CreateSolidBrush(PANEL) as usize,
            line: CreateSolidBrush(LINE) as usize,
            accent: CreateSolidBrush(ACCENT) as usize,
            accent_down: CreateSolidBrush(ACCENT_DOWN) as usize,
        });

        let instance = GetModuleHandleW(null());
        let class = wide("StillLaunch.Editor");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        wc.hbrBackground = theme.bg as HBRUSH;
        if RegisterClassW(&wc) == 0 {
            return Err(win::last_error("RegisterClassW"));
        }
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rc = RECT { left: 0, top: 0, right: W, bottom: H };
        AdjustWindowRect(&mut rc, style, 0);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let title = wide(win::TITLE);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            (GetSystemMetrics(SM_CXSCREEN) - w) / 2,
            (GetSystemMetrics(SM_CYSCREEN) - h) / 2,
            w,
            h,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(win::last_error("CreateWindowExW"));
        }
        build(hwnd, font);
        set_check(hwnd, ID_AUTO, win::autostart_enabled());
        refresh_profiles(hwnd);
        show_profile(hwnd);
        kind_changed(hwnd);
        status(hwnd, &shown_path);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // Gives Tab / Shift+Tab / Space navigation between controls.
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}

unsafe fn message_font() -> usize {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        let p = &mut ncm as *mut NONCLIENTMETRICSW as *mut c_void;
        if SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, ncm.cbSize, p, 0) == 0 {
            return 0; // WM_SETFONT(0) = system font
        }
        CreateFontIndirectW(&ncm.lfMessageFont) as usize
    }
}

unsafe fn build(hwnd: HWND, font: usize) {
    for &(class, text, style, x, y, w, h, id) in CONTROLS {
        let (c, t) = (wide(class), wide(text));
        unsafe {
            let ctl = CreateWindowExW(
                0,
                c.as_ptr(),
                t.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                w,
                h,
                hwnd,
                id as usize as HMENU,
                null_mut(),
                null(),
            );
            SendMessageW(ctl, WM_SETFONT, font, 1);
        }
    }
    for k in KIND_LABELS {
        let w = wide(k);
        send(hwnd, ID_KIND, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    for s in ["cmd", "PowerShell"] {
        let w = wide(s);
        send(hwnd, ID_SHELL, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    set_combo(hwnd, ID_KIND, 0);
    set_combo(hwnd, ID_SHELL, 0);
    let stops = [56i32]; // dialog units: kind column, then detail
    send(hwnd, ID_STEPS, LB_SETTABSTOPS, 1, stops.as_ptr() as LPARAM);
}

// ---- control helpers -------------------------------------------------------

fn item(h: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(h, id) }
}

fn send(h: HWND, id: i32, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { SendMessageW(item(h, id), msg, wp, lp) }
}

fn set_text(h: HWND, id: i32, s: &str) {
    let w = wide(s);
    unsafe { SetDlgItemTextW(h, id, w.as_ptr()) };
}

fn text(h: HWND, id: i32) -> String {
    let c = item(h, id);
    let n = unsafe { GetWindowTextLengthW(c) }.max(0) as usize + 1;
    let mut buf = vec![0u16; n];
    unsafe { GetWindowTextW(c, buf.as_mut_ptr(), n as i32) };
    from_wide(&buf)
}

fn status(h: HWND, s: &str) {
    set_text(h, ID_STATUS, s);
}

fn list_sel(h: HWND, id: i32) -> Option<usize> {
    usize::try_from(send(h, id, LB_GETCURSEL, 0, 0)).ok() // LB_ERR = -1
}

fn list_select(h: HWND, id: i32, i: usize) {
    send(h, id, LB_SETCURSEL, i, 0);
}

fn list_fill(h: HWND, id: i32, rows: &[String], sel: Option<usize>) {
    send(h, id, LB_RESETCONTENT, 0, 0);
    for r in rows {
        let w = wide(r);
        send(h, id, LB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    list_select(h, id, sel.unwrap_or(usize::MAX)); // -1 clears
}

fn combo(h: HWND, id: i32) -> usize {
    usize::try_from(send(h, id, CB_GETCURSEL, 0, 0)).unwrap_or(0)
}

fn set_combo(h: HWND, id: i32, i: usize) {
    send(h, id, CB_SETCURSEL, i, 0);
}

fn checked(h: HWND, id: i32) -> bool {
    send(h, id, BM_GETCHECK, 0, 0) == BST_CHECKED as LRESULT
}

fn set_check(h: HWND, id: i32, on: bool) {
    send(h, id, BM_SETCHECK, if on { BST_CHECKED as WPARAM } else { 0 }, 0);
}

fn enable(h: HWND, id: i32, on: bool) {
    unsafe { EnableWindow(item(h, id), on as i32) };
}

// ---- view <-> model --------------------------------------------------------

fn step_row(s: &Step) -> String {
    let detail = match &s.kind {
        Kind::App { path, args, .. } if !args.is_empty() => format!("{path} {args}"),
        Kind::App { path: t, .. } | Kind::Folder(t) | Kind::File(t) | Kind::Url(t) => t.clone(),
        Kind::Cmd { ps, line } => format!("{}> {line}", if *ps { "PS" } else { "cmd" }),
        Kind::Wait(ms) => format!("{ms} ms"),
    };
    let off = if s.off { "［無効］ " } else { "" };
    format!("{}\t{off}{detail}", KIND_LABELS[s.kind.index()])
}

fn refresh_profiles(h: HWND) {
    let Some((rows, cur)) = with(|a| (a.cfg.profiles.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), a.cur))
    else {
        return;
    };
    set_text(h, ID_PHEAD, &format!("プロファイル（{}）", rows.len()));
    list_fill(h, ID_PROFILES, &rows, cur);
}

fn show_profile(h: HWND) {
    let (name, rows) = with(|a| {
        let p = a.cur.and_then(|i| a.cfg.profiles.get(i))?;
        Some((p.name.clone(), p.steps.iter().map(step_row).collect::<Vec<_>>()))
    })
    .flatten()
    .unwrap_or_default();
    set_text(h, ID_NAME, &name);
    set_text(h, ID_SHEAD, &format!("ステップ（{}）", rows.len()));
    list_fill(h, ID_STEPS, &rows, None);
}

/// Copies the name field into the current profile.
fn commit_name(h: HWND) {
    let name = text(h, ID_NAME);
    let changed = with(|a| match a.cur.and_then(|i| a.cfg.profiles.get_mut(i)) {
        Some(p) if p.name != name => {
            p.name = name;
            a.dirty = true;
            true
        }
        _ => false,
    });
    if changed == Some(true) {
        refresh_profiles(h);
    }
}

fn kind_changed(h: HWND) {
    let k = combo(h, ID_KIND).min(5);
    set_text(h, ID_TLABEL, TARGET_LABELS[k]);
    enable(h, ID_ARGS, k == 0);
    enable(h, ID_DIR, k == 0);
    enable(h, ID_SHELL, k == 4);
    enable(h, ID_BROWSE, k <= 2);
}

/// Puts the selected step into the form.
fn load_step(h: HWND) {
    let Some(si) = list_sel(h, ID_STEPS) else { return };
    let step = with(|a| a.cur.and_then(|i| a.cfg.profiles.get(i)).and_then(|p| p.steps.get(si)).cloned());
    let Some(s) = step.flatten() else { return };
    let (k, off) = (s.kind.index(), s.off);
    let (target, args, dir, ps) = match s.kind {
        Kind::App { path, args, dir } => (path, args, dir, false),
        Kind::Folder(t) | Kind::File(t) | Kind::Url(t) => (t, String::new(), String::new(), false),
        Kind::Cmd { ps, line } => (line, String::new(), String::new(), ps),
        Kind::Wait(ms) => (ms.to_string(), String::new(), String::new(), false),
    };
    set_combo(h, ID_KIND, k);
    set_combo(h, ID_SHELL, ps as usize);
    set_text(h, ID_TARGET, &target);
    set_text(h, ID_ARGS, &args);
    set_text(h, ID_DIR, &dir);
    set_check(h, ID_OFF, off);
    kind_changed(h);
}

/// Builds a step from the form.
fn read_step(h: HWND) -> Result<Step, String> {
    let t = text(h, ID_TARGET).trim().to_string();
    let kind = match combo(h, ID_KIND) {
        0 => Kind::App { path: t, args: text(h, ID_ARGS), dir: text(h, ID_DIR).trim().to_string() },
        1 => Kind::Folder(t),
        2 => Kind::File(t),
        3 => Kind::Url(t),
        4 => Kind::Cmd { ps: combo(h, ID_SHELL) == 1, line: t },
        _ => Kind::Wait(t.parse().map_err(|_| "待機時間はミリ秒の整数で入力してください".to_string())?),
    };
    let s = Step { off: checked(h, ID_OFF), kind };
    config::check_step(&s)?;
    Ok(s)
}

/// Applies `f(steps, selected)` to the current profile; `f` returns the row
/// to select afterwards, or None when nothing changed.
fn edit_steps(h: HWND, f: impl FnOnce(&mut Vec<Step>, Option<usize>) -> Option<usize>) {
    let sel = list_sel(h, ID_STEPS);
    let r = with(|a| {
        let p = a.cur.and_then(|i| a.cfg.profiles.get_mut(i))?;
        let n = f(&mut p.steps, sel)?;
        a.dirty = true;
        Some(n)
    });
    if let Some(n) = r.flatten() {
        show_profile(h);
        list_select(h, ID_STEPS, n);
    }
}

// ---- commands --------------------------------------------------------------

fn on_command(h: HWND, id: i32, code: u32) {
    match (id, code) {
        (ID_PROFILES, LBN_SELCHANGE) => {
            let new = list_sel(h, ID_PROFILES);
            commit_name(h);
            with(|a| a.cur = new);
            if let Some(n) = new {
                list_select(h, ID_PROFILES, n);
            }
            show_profile(h);
        }
        (ID_PROFILES, LBN_DBLCLK) => run_selected(h),
        (ID_STEPS, LBN_SELCHANGE) => load_step(h),
        (ID_KIND, CBN_SELCHANGE) => kind_changed(h),
        (_, CLICKED) => match id {
            ID_OK => commit_name(h),
            ID_NEW => new_profile(h),
            ID_PDEL => delete_profile(h),
            ID_RUN => run_selected(h),
            ID_SHORTCUT => shortcut(h),
            ID_BROWSE => browse(h),
            ID_ADD | ID_SET => match read_step(h) {
                Err(e) => status(h, &e),
                Ok(s) if id == ID_ADD => edit_steps(h, |v, _| {
                    v.push(s);
                    Some(v.len() - 1)
                }),
                Ok(s) => edit_steps(h, |v, sel| {
                    let i = sel?;
                    *v.get_mut(i)? = s;
                    Some(i)
                }),
            },
            ID_SDEL => edit_steps(h, |v, sel| {
                let i = sel.filter(|&i| i < v.len())?;
                v.remove(i);
                Some(i.min(v.len().saturating_sub(1)))
            }),
            ID_UP => edit_steps(h, |v, sel| {
                let i = sel.filter(|&i| i > 0 && i < v.len())?;
                v.swap(i, i - 1);
                Some(i - 1)
            }),
            ID_DOWN => edit_steps(h, |v, sel| {
                let i = sel.filter(|&i| i + 1 < v.len())?;
                v.swap(i, i + 1);
                Some(i + 1)
            }),
            ID_AUTO => {
                let on = checked(h, ID_AUTO);
                match win::set_autostart(on) {
                    Ok(()) => status(h, if on { "Windows 起動時に常駐します" } else { "自動起動を解除しました" }),
                    Err(e) => {
                        set_check(h, ID_AUTO, !on);
                        status(h, &e);
                    }
                }
            }
            ID_SAVE => {
                save(h);
            }
            _ => {}
        },
        _ => {}
    }
}

fn new_profile(h: HWND) {
    commit_name(h);
    let id = match win::new_guid() {
        Ok(id) => id,
        Err(e) => return status(h, &e),
    };
    with(|a| {
        a.cfg.profiles.push(Profile { id, name: "新しいプロファイル".into(), steps: Vec::new() });
        a.cur = Some(a.cfg.profiles.len() - 1);
        a.dirty = true;
    });
    refresh_profiles(h);
    show_profile(h);
    unsafe { SetFocus(item(h, ID_NAME)) };
    send(h, ID_NAME, EM_SETSEL, 0, -1);
}

fn delete_profile(h: HWND) {
    commit_name(h);
    let target = with(|a| a.cur.and_then(|i| a.cfg.profiles.get(i).map(|p| (i, p.name.clone()))));
    let Some((i, name)) = target.flatten() else { return };
    if win::message(h, &format!("「{name}」を削除しますか？"), MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }
    with(|a| {
        if i < a.cfg.profiles.len() {
            a.cfg.profiles.remove(i);
        }
        a.cur = None;
        a.dirty = true;
    });
    refresh_profiles(h);
    show_profile(h);
}

/// Runs the profile as shown (saved or not) on a background thread.
fn run_selected(h: HWND) {
    commit_name(h);
    let Some(p) = with(|a| a.cur.and_then(|i| a.cfg.profiles.get(i)).cloned()).flatten() else { return };
    let name = p.name.clone();
    let hwnd = h as usize;
    let spawned = std::thread::Builder::new().spawn(move || {
        let report = run::run_profile(&p);
        DONE.lock().unwrap_or_else(|e| e.into_inner()).push(run::summary(&p, &report));
        unsafe { PostMessageW(hwnd as HWND, WM_DONE, 0, 0) };
    });
    match spawned {
        Ok(_) => status(h, &format!("実行中: {name}")),
        Err(e) => status(h, &e.to_string()),
    }
}

fn shortcut(h: HWND) {
    commit_name(h);
    let Some((p, dirty)) = with(|a| a.cur.and_then(|i| a.cfg.profiles.get(i)).cloned().map(|p| (p, a.dirty))).flatten()
    else {
        return;
    };
    if dirty {
        return status(h, "ショートカットを作る前に保存してください");
    }
    let cmd = unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let (desk, start) = (wide("デスクトップ"), wide("スタートメニュー"));
        AppendMenuW(menu, MF_STRING, 1, desk.as_ptr());
        AppendMenuW(menu, MF_STRING, 2, start.as_ptr());
        let mut rc: RECT = std::mem::zeroed();
        GetWindowRect(item(h, ID_SHORTCUT), &mut rc);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, rc.left, rc.bottom, 0, h, null());
        DestroyMenu(menu);
        cmd
    };
    if cmd != 0 {
        match win::create_shortcut(&p, cmd == 2) {
            Ok(path) => status(h, &format!("作成しました: {}", path.display())),
            Err(e) => status(h, &e),
        }
    }
}

fn browse(h: HWND) {
    let picked = match combo(h, ID_KIND) {
        0 => browse_file(h, "プログラム\0*.exe;*.bat;*.cmd;*.lnk\0すべてのファイル\0*.*\0"),
        1 => browse_folder(h),
        _ => browse_file(h, "すべてのファイル\0*.*\0"),
    };
    if let Some(p) = picked {
        set_text(h, ID_TARGET, &p);
    }
}

fn browse_file(h: HWND, filter: &str) -> Option<String> {
    let filter = wide(filter); // ends in "\0\0" as required
    let mut buf = vec![0u16; 1024];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = h;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR;
    let ok = unsafe { GetOpenFileNameW(&mut ofn) } != 0;
    ok.then(|| from_wide(&buf))
}

fn browse_folder(h: HWND) -> Option<String> {
    let mut display = [0u16; 260];
    let mut path = [0u16; 260];
    unsafe {
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        bi.hwndOwner = h;
        bi.pszDisplayName = display.as_mut_ptr();
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return None;
        }
        let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
        CoTaskMemFree(pidl as *const c_void);
        (ok != 0).then(|| from_wide(&path))
    }
}

/// Saves after checking nobody else changed the file since it was read.
fn save(h: HWND) -> bool {
    commit_name(h);
    let Some((cfg, path, known)) = with(|a| (a.cfg.clone(), a.path.clone(), a.mtime)) else { return false };
    if config::mtime(&path) != known
        && win::message(h, "設定ファイルが外部で変更されています。上書きしますか？", MB_YESNO | MB_ICONWARNING) != IDYES
    {
        return false;
    }
    if let Err(e) = cfg.save(&path) {
        status(h, "保存できませんでした");
        win::message(h, &format!("保存できません。\n{e}"), MB_OK | MB_ICONERROR);
        return false;
    }
    with(|a| {
        a.dirty = false;
        a.mtime = config::mtime(&a.path);
    });
    status(h, "保存しました");
    true
}

fn close(h: HWND) {
    commit_name(h);
    if with(|a| a.dirty) == Some(true) {
        match win::message(h, "変更を保存しますか？", MB_YESNOCANCEL | MB_ICONQUESTION) {
            IDYES if save(h) => {}
            IDNO => {}
            _ => return,
        }
    }
    unsafe { DestroyWindow(h) };
}

// ---- painting --------------------------------------------------------------

unsafe fn draw_button(d: &DRAWITEMSTRUCT) {
    let Some(t) = THEME.get() else { return };
    let primary = d.CtlID == ID_RUN as u32 || d.CtlID == ID_SAVE as u32;
    let down = d.itemState & ODS_SELECTED != 0;
    let (fill, edge, color) = match (primary, down) {
        (true, false) => (t.accent, t.accent, PANEL),
        (true, true) => (t.accent_down, t.accent_down, PANEL),
        (false, false) => (t.panel, t.line, TEXT),
        (false, true) => (t.line, t.line, TEXT),
    };
    let color = if d.itemState & ODS_DISABLED != 0 { MUTED } else { color };
    let mut label = [0u16; 32];
    unsafe {
        FillRect(d.hDC, &d.rcItem, fill as HBRUSH);
        FrameRect(d.hDC, &d.rcItem, edge as HBRUSH);
        let old = SelectObject(d.hDC, t.font as HGDIOBJ);
        SetBkMode(d.hDC, 1); // TRANSPARENT
        SetTextColor(d.hDC, color);
        let n = GetWindowTextW(d.hwndItem, label.as_mut_ptr(), label.len() as i32);
        let mut rc = d.rcItem;
        DrawTextW(d.hDC, label.as_ptr(), n, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        if d.itemState & ODS_FOCUS != 0 {
            let r = &d.rcItem;
            let inner = RECT { left: r.left + 3, top: r.top + 3, right: r.right - 3, bottom: r.bottom - 3 };
            DrawFocusRect(d.hDC, &inner);
        }
        SelectObject(d.hDC, old);
    }
}

unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                on_command(h, (wp & 0xFFFF) as i32, ((wp >> 16) & 0xFFFF) as u32);
                0
            }
            WM_DONE => {
                let done: Vec<String> = DONE.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
                if !done.is_empty() {
                    status(h, &done.join("  |  "));
                }
                0
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                let Some(t) = THEME.get() else { return DefWindowProcW(h, msg, wp, lp) };
                let dc = wp as HDC;
                let field = msg != WM_CTLCOLORSTATIC;
                SetBkColor(dc, if field { PANEL } else { BG });
                SetTextColor(dc, if GetDlgCtrlID(lp as HWND) == ID_STATUS { SUB } else { TEXT });
                (if field { t.panel } else { t.bg }) as LRESULT
            }
            WM_DRAWITEM => {
                if lp != 0 {
                    draw_button(&*(lp as *const DRAWITEMSTRUCT));
                }
                1
            }
            WM_CLOSE => {
                close(h);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}
