//! Small Win32 helpers shared by the other modules: UTF-16 strings, message
//! boxes, GUIDs, the autostart registry value and .lnk shortcuts.

use crate::config::{self, Profile};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, GetLastError, HWND};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateGuid, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_ANY, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, SHGetKnownFolderPath, ShellLink,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MESSAGEBOX_STYLE, MessageBoxW};
use windows_sys::core::{GUID, HRESULT};

pub const TITLE: &str = "StillLaunch";

/// NUL-terminated UTF-16. Config strings never contain NUL (the JSON reader
/// refuses `\u0000`), and window text cannot, so nothing is cut short.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Text up to the first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

pub fn last_error(api: &str) -> String {
    format!("{api} failed (error {})", unsafe { GetLastError() })
}

pub fn message(owner: HWND, text: &str, style: MESSAGEBOX_STYLE) -> i32 {
    let (t, c) = (wide(text), wide(TITLE));
    unsafe { MessageBoxW(owner, t.as_ptr(), c.as_ptr(), style) }
}

pub fn exe_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.to_str().map(str::to_owned).ok_or_else(|| "the program path is not valid Unicode".into())
}

pub fn new_guid() -> Result<String, String> {
    let mut g: GUID = unsafe { std::mem::zeroed() };
    if unsafe { CoCreateGuid(&mut g) } < 0 {
        return Err("CoCreateGuid failed".into());
    }
    let d = g.data4;
    Ok(format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    ))
}

// ---- autostart: HKCU\...\Run\ProjectLauncher = "<exe>" --resident -------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "ProjectLauncher";

pub fn autostart_enabled() -> bool {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_ANY, null_mut(), null_mut(), null_mut())
    };
    rc == 0
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    let rc = if on {
        let data = wide(&format!("\"{}\" --resident", exe_path()?));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_SZ,
                data.as_ptr() as *const c_void,
                (data.len() * 2) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => 0,
            rc => rc,
        }
    };
    if rc == 0 { Ok(()) } else { Err(format!("registry update failed (error {rc})")) }
}

// ---- shortcuts -------------------------------------------------------------

/// Writes `<Desktop or Start menu\Programs>\<name>-<id8>.lnk` that runs
/// `"<exe>" --profile <GUID>`.
pub fn create_shortcut(p: &Profile, start_menu: bool) -> Result<PathBuf, String> {
    let exe = exe_path()?;
    let dir = known_folder(if start_menu { &FOLDERID_Programs } else { &FOLDERID_Desktop })?;
    let lnk = dir.join(config::shortcut_file_name(p));
    let lnk_s = lnk.to_str().ok_or("the shortcut path is not valid Unicode")?;
    let work = exe.rsplit_once('\\').map_or(exe.as_str(), |(d, _)| d);
    write_lnk(&wide(lnk_s), &wide(&exe), &wide(&format!("--profile {}", p.id)), &wide(work), &wide(&p.name))?;
    Ok(lnk)
}

fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    let mut raw: *mut u16 = null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut raw) };
    if hr < 0 || raw.is_null() {
        return Err(format!("SHGetKnownFolderPath failed (0x{hr:08X})"));
    }
    // SAFETY: on success `raw` is a NUL-terminated string we must free.
    let path = unsafe {
        let mut n = 0;
        while *raw.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(raw, n));
        CoTaskMemFree(raw as *const c_void);
        s
    };
    Ok(PathBuf::from(path))
}

const IID_ISHELLLINKW: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const IID_IPERSISTFILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);

type FnRelease = unsafe extern "system" fn(*mut c_void) -> u32;
type FnQuery = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type FnSetStr = unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT;
type FnSetStrInt = unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT;

/// Slot `i` of a COM object's vtable. Slot numbers follow the interface
/// declarations in ShObjIdl_core.h / ObjIdl.h (IUnknown = 0..2).
unsafe fn slot(obj: *mut c_void, i: usize) -> *const c_void {
    unsafe { *(*(obj as *const *const *const c_void)).add(i) }
}

/// Releases a COM pointer on drop.
struct Com(*mut c_void);

impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            let release: FnRelease = std::mem::transmute(slot(self.0, 2));
            release(self.0);
        }
    }
}

fn check(api: &str, hr: HRESULT) -> Result<(), String> {
    if hr < 0 { Err(format!("{api} failed (0x{hr:08X})")) } else { Ok(()) }
}

fn write_lnk(lnk: &[u16], target: &[u16], args: &[u16], dir: &[u16], desc: &[u16]) -> Result<(), String> {
    // S_FALSE (already initialised) still needs a matching CoUninitialize;
    // RPC_E_CHANGED_MODE leaves COM usable in the existing mode.
    let init = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
    let result = unsafe {
        let mut obj: *mut c_void = null_mut();
        let hr = CoCreateInstance(&ShellLink, null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELLLINKW, &mut obj);
        check("CoCreateInstance(ShellLink)", hr).and_then(|()| {
            let link = Com(obj);
            // IShellLinkW: 7 SetDescription, 9 SetWorkingDirectory,
            // 11 SetArguments, 17 SetIconLocation, 20 SetPath.
            let set = |i: usize, s: &[u16]| -> HRESULT {
                let f: FnSetStr = std::mem::transmute(slot(link.0, i));
                f(link.0, s.as_ptr())
            };
            check("SetPath", set(20, target))?;
            check("SetArguments", set(11, args))?;
            check("SetWorkingDirectory", set(9, dir))?;
            check("SetDescription", set(7, desc))?;
            let set_icon: FnSetStrInt = std::mem::transmute(slot(link.0, 17));
            check("SetIconLocation", set_icon(link.0, target.as_ptr(), 0))?;
            let query: FnQuery = std::mem::transmute(slot(link.0, 0));
            let mut pf: *mut c_void = null_mut();
            check("QueryInterface(IPersistFile)", query(link.0, &IID_IPERSISTFILE, &mut pf))?;
            let file = Com(pf);
            // IPersistFile: 6 Save(pszFileName, fRemember).
            let save: FnSetStrInt = std::mem::transmute(slot(file.0, 6));
            check("IPersistFile::Save", save(file.0, lnk.as_ptr(), 1))
        })
    };
    if init >= 0 {
        unsafe { CoUninitialize() };
    }
    result
}
