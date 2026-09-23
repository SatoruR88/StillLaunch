//! Windows ログオン時の Resident 自動開始(HKCU Run キー)。
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` に
//! `ProjectLauncher = "<exe>" --resident` を登録する。HKCU なので管理者権限は
//! 不要で、ユーザーごとに独立している。

use std::fmt;

#[derive(Debug)]
pub enum AutostartError {
    /// `std::env::current_exe` が取れない。
    NoExePath(std::io::Error),
    /// exe のパスが UTF-16 に変換できない(サロゲート等)。
    NonUnicodeExePath,
    /// レジストリ API の失敗。
    Registry { api: &'static str, code: u32 },
}

impl fmt::Display for AutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoExePath(e) => write!(f, "cannot resolve the exe path: {e}"),
            Self::NonUnicodeExePath => write!(f, "the exe path is not valid Unicode"),
            Self::Registry { api, code } => {
                write!(f, "{api} failed (WIN32_ERROR={code})")
            }
        }
    }
}

impl std::error::Error for AutostartError {}

/// The Run-key value written on enable: quoted exe path plus `--resident`.
/// Not registry access; kept separate so tests stay side-effect free.
fn run_command() -> Result<String, AutostartError> {
    let exe = std::env::current_exe().map_err(AutostartError::NoExePath)?;
    let exe = exe.to_str().ok_or(AutostartError::NonUnicodeExePath)?;
    Ok(format!("\"{exe}\" --resident"))
}

#[cfg(windows)]
mod imp {
    use super::{AutostartError, run_command};
    use std::iter::once;
    use std::ptr;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE_NAME: &str = "ProjectLauncher";

    /// NUL-terminated UTF-16, matching the codebase's per-module helper.
    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(once(0)).collect()
    }

    fn win32(api: &'static str, code: u32) -> AutostartError {
        AutostartError::Registry { api, code }
    }

    /// RAII wrapper so the opened key is always closed.
    struct RegKey(HKEY);

    impl RegKey {
        fn open(sam: u32) -> Result<Self, AutostartError> {
            let subkey = to_wide(RUN_KEY);
            let mut hkey: HKEY = ptr::null_mut();
            // SAFETY: subkey is NUL-terminated; phkresult is a valid out pointer.
            let rc =
                unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, sam, &mut hkey) };
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(Self(ptr::null_mut()));
            }
            if rc != 0 {
                return Err(win32("RegOpenKeyExW", rc));
            }
            Ok(Self(hkey))
        }

        /// Opens the Run key for `sam` access, creating it when absent. The
        /// key exists on any normal install, but the write path should not
        /// depend on that — RegCreateKeyExW opens-or-creates atomically.
        fn open_or_create(sam: u32) -> Result<Self, AutostartError> {
            let subkey = to_wide(RUN_KEY);
            let mut hkey: HKEY = ptr::null_mut();
            // SAFETY: subkey is NUL-terminated; phkResult is a valid out
            // pointer. lpClass/lpSecurityAttributes NULL give defaults, and
            // lpdwDisposition may be NULL when created-vs-opened is moot.
            let rc = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    0,
                    ptr::null(),
                    0,
                    sam,
                    ptr::null(),
                    &mut hkey,
                    ptr::null_mut(),
                )
            };
            if rc != 0 {
                return Err(win32("RegCreateKeyExW", rc));
            }
            Ok(Self(hkey))
        }
    }

    impl Drop for RegKey {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: hkey came from RegOpenKeyExW/RegCreateKeyExW and is
                // closed exactly once here.
                unsafe { RegCloseKey(self.0) };
            }
        }
    }

    /// Whether the Run key contains a `ProjectLauncher` value.
    pub fn is_enabled() -> Result<bool, AutostartError> {
        let key = RegKey::open(KEY_QUERY_VALUE)?;
        if key.0.is_null() {
            return Ok(false);
        }
        let name = to_wide(VALUE_NAME);
        // SAFETY: key is open for query; name is NUL-terminated. A NULL data
        // pointer only asks for the value's existence/size.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        match rc {
            0 => Ok(true),
            ERROR_FILE_NOT_FOUND => Ok(false),
            other => Err(win32("RegQueryValueExW", other)),
        }
    }

    /// Writes `cmd` as the Run value verbatim. `pub` so tests can restore a
    /// saved registration.
    pub fn write_value(cmd: &str) -> Result<(), AutostartError> {
        // Create-or-open: the Run key is present on a normal install, but a
        // missing one is recreated rather than failing the write.
        let key = RegKey::open_or_create(KEY_SET_VALUE)?;
        let name = to_wide(VALUE_NAME);
        let data = to_wide(cmd);
        // SAFETY: key is open for set; `data` is a NUL-terminated REG_SZ
        // buffer of `len*2` bytes.
        let rc = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        };
        if rc != 0 {
            return Err(win32("RegSetValueExW", rc));
        }
        Ok(())
    }

    /// Removes the Run value; deleting an absent one is a no-op.
    fn delete_value() -> Result<(), AutostartError> {
        let key = RegKey::open(KEY_SET_VALUE)?;
        if key.0.is_null() {
            return Ok(());
        }
        let name = to_wide(VALUE_NAME);
        // SAFETY: key is open for set; deleting an absent value is a benign
        // ERROR_FILE_NOT_FOUND.
        let rc = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
        if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
            return Err(win32("RegDeleteValueW", rc));
        }
        Ok(())
    }

    /// Reads the raw Run value, or None when absent/not REG_SZ. Test-only:
    /// lets tests save and restore the user's real registration verbatim.
    #[cfg(test)]
    pub fn read_value() -> Result<Option<String>, AutostartError> {
        let key = RegKey::open(KEY_QUERY_VALUE)?;
        if key.0.is_null() {
            return Ok(None);
        }
        let name = to_wide(VALUE_NAME);
        let mut ty = 0u32;
        let mut size = 0u32;
        // SAFETY: key is open for query; NULL data asks for type and size.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut ty,
                ptr::null_mut(),
                &mut size,
            )
        };
        match rc {
            0 if ty == REG_SZ => {}
            0 => return Ok(None),
            ERROR_FILE_NOT_FOUND => return Ok(None),
            other => return Err(win32("RegQueryValueExW", other)),
        }
        let mut buf = vec![0u16; (size as usize / 2) + 1];
        // SAFETY: buf has `size` writable bytes plus slack; REG_SZ read.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                &mut size,
            )
        };
        if rc != 0 {
            return Err(win32("RegQueryValueExW", rc));
        }
        let used = (size as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..used]);
        Ok(Some(text.trim_end_matches('\0').to_string()))
    }

    /// Registers or removes the autostart entry.
    pub fn set(enabled: bool) -> Result<(), AutostartError> {
        if enabled {
            write_value(&run_command()?)
        } else {
            delete_value()
        }
    }
}

#[cfg(windows)]
pub use imp::{is_enabled, set};

#[cfg(not(windows))]
mod imp {
    use super::AutostartError;

    pub fn is_enabled() -> Result<bool, AutostartError> {
        Ok(false)
    }

    pub fn set(_enabled: bool) -> Result<(), AutostartError> {
        Ok(())
    }
}

#[cfg(not(windows))]
pub use imp::{is_enabled, set};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_quotes_exe_and_passes_resident() {
        let cmd = run_command().unwrap();
        assert!(cmd.starts_with('"'));
        assert!(cmd.ends_with("\" --resident"));
        assert!(cmd.contains("ProjectLauncher"));
    }

    /// Real HKCU round-trip: enable writes the value, disable removes it and
    /// leaves no residue. Uses the user's own Run key — the same place the
    /// feature manages — and restores the original value string verbatim
    /// (the test binary's `current_exe` is NOT the registered path).
    #[cfg(windows)]
    #[test]
    fn enable_then_disable_round_trips_registry() {
        let original = imp::read_value().unwrap();
        set(true).unwrap();
        assert!(is_enabled().unwrap());
        set(false).unwrap();
        assert!(!is_enabled().unwrap());
        if let Some(original) = original {
            imp::write_value(&original).unwrap();
            assert_eq!(
                imp::read_value().unwrap().as_deref(),
                Some(original.as_str())
            );
        }
    }
}
