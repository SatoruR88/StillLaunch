//! Windows shortcut (.lnk) creation for profiles.
//!
//! A shortcut targets `ProjectLauncher.exe --profile <GUID>`, so launching it
//! goes through the same code path as the CLI: forward to the resident when
//! one is running, run directly otherwise.
//!
//! windows-sys ships only functions/structs/constants, not COM interface
//! types, so the IShellLinkW / IPersistFile vtables are declared here in
//! `#[repr(C)]` form. The layout is the stable COM ABI from the Windows SDK
//! (shobjidl_core.h, objidl.h); correctness of the field order is exercised
//! end-to-end at runtime by reading a created .lnk back through the shell.

use std::fmt;
use std::iter::once;
use std::path::PathBuf;

/// Where a shortcut .lnk is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutLocation {
    Desktop,
    StartMenu,
}

#[derive(Debug)]
pub enum ShortcutError {
    /// A value cannot be handed to the OS safely (e.g. interior NUL).
    InvalidInput(String),
    /// A COM or shell call failed; `hr` is the raw HRESULT.
    Com { api: &'static str, hr: i32 },
    /// Resolving the running executable's path failed.
    Io(std::io::Error),
    /// The host OS does not provide the required COM API.
    /// Only constructed by the non-Windows stub.
    #[allow(dead_code)]
    UnsupportedPlatform,
}

impl fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShortcutError::InvalidInput(m) => write!(f, "invalid input: {m}"),
            ShortcutError::Com { api, hr } => write!(f, "{api} failed (HRESULT=0x{hr:08X})"),
            ShortcutError::Io(e) => write!(f, "cannot resolve the executable path: {e}"),
            ShortcutError::UnsupportedPlatform => {
                write!(f, "shortcut creation requires Windows")
            }
        }
    }
}

impl std::error::Error for ShortcutError {}

/// Creates `<location>\<profile name>-<id tag>.lnk` pointing at this
/// executable with `--profile <id>` arguments. Returns the written path. An
/// existing file is overwritten so re-running after a rename or exe move
/// repairs the link.
#[cfg(windows)]
pub fn create(
    profile_name: &str,
    profile_id: &str,
    location: ShortcutLocation,
) -> Result<PathBuf, ShortcutError> {
    use std::os::windows::ffi::OsStrExt;

    let exe = std::env::current_exe().map_err(ShortcutError::Io)?;
    let work_dir = exe
        .parent()
        .ok_or_else(|| ShortcutError::InvalidInput("executable has no parent directory".into()))?;
    let dir = known_folder(location)?;
    let lnk = dir.join(shortcut_file_name(profile_name, profile_id));

    let wide_os = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(once(0)).collect() };
    let lnk_w = wide_os(lnk.as_os_str());
    let target_w = wide_os(exe.as_os_str());
    let workdir_w = wide_os(work_dir.as_os_str());
    let args_w = to_wide(&format!("--profile {profile_id}"))?;

    imp::write_lnk(&lnk_w, &target_w, &args_w, &workdir_w, &target_w)?;
    Ok(lnk)
}

/// Non-Windows stub: shortcut creation is Windows-only, kept so the crate
/// still compiles elsewhere for tests.
#[cfg(not(windows))]
pub fn create(
    _profile_name: &str,
    _profile_id: &str,
    _location: ShortcutLocation,
) -> Result<PathBuf, ShortcutError> {
    Err(ShortcutError::UnsupportedPlatform)
}

fn to_wide(s: &str) -> Result<Vec<u16>, ShortcutError> {
    if s.contains('\0') {
        return Err(ShortcutError::InvalidInput(format!(
            "string contains an interior NUL byte: {s:?}"
        )));
    }
    Ok(s.encode_utf16().chain(once(0)).collect())
}

/// Caps the base name so a long profile name cannot push the full path past
/// MAX_PATH (~260 UTF-16 code units); the GUID fallback always fits. Windows
/// measures file names in UTF-16 code units, so the budget is counted with
/// `char::len_utf16`, not `chars().count()` — otherwise a supplementary-plane
/// char would consume two units while counting as one.
const MAX_NAME_CHARS: usize = 100;

/// Builds `<name>-<id tag>.lnk`, mapping characters that are illegal in
/// Windows file names to `_`. The tag is the first eight hex digits of the
/// profile id: sanitizing maps distinct names like "a:b" and "a|b" to the
/// same stem, so without it a later shortcut would silently overwrite the
/// .lnk of a different profile. When the stem is unusable (empty, dots/spaces
/// only, or a reserved device name) the bare GUID digits take over as the
/// whole base name, which is always legal and unique.
fn shortcut_file_name(profile_name: &str, fallback_id: &str) -> String {
    // Find the byte offset where the UTF-16 budget runs out; the cut stays
    // on a char boundary so a surrogate pair is never split.
    let mut units = 0usize;
    let mut end = profile_name.len();
    for (i, c) in profile_name.char_indices() {
        let w = c.len_utf16();
        if units + w > MAX_NAME_CHARS {
            end = i;
            break;
        }
        units += w;
    }
    let sanitized: String = profile_name[..end]
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Trailing dots/spaces cannot be created; trim again after the length cap
    // because truncation can expose one mid-name.
    let trimmed = sanitized.trim_end_matches(['.', ' ']);
    // Hex digits of the id (braces/hyphens dropped) are always a legal file
    // name; a validated profile GUID yields the full 32 of them.
    let id_hex: String = fallback_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if trimmed.is_empty() || is_reserved_name(trimmed) {
        let base = if id_hex.is_empty() {
            fallback_id
        } else {
            id_hex.as_str()
        };
        return format!("{base}.lnk");
    }
    if id_hex.is_empty() {
        // Unreachable for validated profile ids; degrade to the bare stem.
        return format!("{trimmed}.lnk");
    }
    // The stem was capped and trimmed above; the tag is appended after that
    // and is never truncated itself. Byte indexing is safe: hex is ASCII.
    let tag = id_hex[..8.min(id_hex.len())].to_ascii_lowercase();
    format!("{trimmed}-{tag}.lnk")
}

/// Windows reserves these device names up to the first dot, so both `CON.lnk`
/// and a `con.foo.lnk` built from a `con.foo` profile name are invalid. The
/// stem is compared after trimming trailing spaces/dots because the OS
/// strips them while normalizing: "con .foo" resolves to reserved "con".
fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Resolves a per-user known folder. `SHGetKnownFolderPath` with the default
/// flag never creates the folder, which matches read-only expectations here:
/// the folders are guaranteed to exist on a normal Windows install.
#[cfg(windows)]
fn known_folder(location: ShortcutLocation) -> Result<PathBuf, ShortcutError> {
    use std::ffi::c_void;
    use std::slice;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Programs, SHGetKnownFolderPath,
    };

    // Start-menu shortcuts go in Programs so they show up in the app list;
    // the StartMenu root itself is only searched, not enumerated.
    let id = match location {
        ShortcutLocation::Desktop => &FOLDERID_Desktop,
        ShortcutLocation::StartMenu => &FOLDERID_Programs,
    };
    // SAFETY: `id` is a valid GUID constant; `raw` receives a CoTaskMem buffer
    // on success and is freed below on every path.
    unsafe {
        let mut raw: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw);
        if hr < 0 {
            return Err(ShortcutError::Com {
                api: "SHGetKnownFolderPath",
                hr,
            });
        }
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(slice::from_raw_parts(raw, len));
        CoTaskMemFree(raw as *mut c_void);
        Ok(PathBuf::from(s))
    }
}

#[cfg(windows)]
mod imp {
    //! Minimal COM plumbing for IShellLinkW + IPersistFile.
    //!
    //! The vtable field order below is load-bearing and must match the SDK
    //! headers exactly; only the slots we call have real signatures, the rest
    //! keep position with opaque-argument stubs.

    use super::{ShortcutError, hr_err};
    use std::ffi::c_void;
    use std::ptr;
    use windows_sys::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::ShellLink;
    use windows_sys::core::{GUID, HRESULT};

    const IID_ISHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
    const IID_IPERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);

    /// The three IUnknown slots every COM vtable starts with. Any interface
    /// pointer can be read through this to reach QueryInterface/Release.
    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    /// IShellLinkW: IUnknown (3) + 18 methods, order per shobjidl_core.h.
    #[repr(C)]
    struct IShellLinkWVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_path:
            unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> HRESULT,
        get_id_list: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
        set_id_list: unsafe extern "system" fn(*mut c_void, *const c_void) -> HRESULT,
        get_description: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> HRESULT,
        set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_working_directory: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> HRESULT,
        set_working_directory: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_arguments: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> HRESULT,
        set_arguments: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_hotkey: unsafe extern "system" fn(*mut c_void, *mut u16) -> HRESULT,
        set_hotkey: unsafe extern "system" fn(*mut c_void, u16) -> HRESULT,
        get_show_cmd: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
        set_show_cmd: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
        get_icon_location:
            unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut i32) -> HRESULT,
        set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
        set_relative_path: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT,
        resolve: unsafe extern "system" fn(*mut c_void, isize, u32) -> HRESULT,
        set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    }

    /// IPersistFile: IUnknown (3) + IPersist::GetClassID + 5 methods, order
    /// per objidl.h.
    #[repr(C)]
    struct IPersistFileVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_class_id: unsafe extern "system" fn(*mut c_void, *mut GUID) -> HRESULT,
        is_dirty: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT,
        save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
        save_completed: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_cur_file: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
    }

    #[repr(C)]
    pub struct IShellLinkW {
        vtable: *const IShellLinkWVtbl,
    }

    #[repr(C)]
    pub struct IPersistFile {
        vtable: *const IPersistFileVtbl,
    }

    /// Owns a COM pointer and Releases it on drop, including error paths.
    pub struct ComPtr<T> {
        ptr: *mut T,
    }

    impl<T> ComPtr<T> {
        fn ptr(&self) -> *mut T {
            self.ptr
        }
    }

    impl<T> Drop for ComPtr<T> {
        fn drop(&mut self) {
            // SAFETY: every COM object's vtable begins with IUnknown, so the
            // Release slot is at the same position for all interfaces.
            unsafe {
                let vtbl = (*(self.ptr as *mut IUnknownObject)).vtable;
                ((*vtbl).release)(self.ptr as *mut c_void);
            }
        }
    }

    #[repr(C)]
    struct IUnknownObject {
        vtable: *const IUnknownVtbl,
    }

    /// Calls CoInitializeEx on construction and CoUninitialize on drop. Both
    /// S_OK and S_FALSE mean this thread is now initialized and must be
    /// balanced; RPC_E_CHANGED_MODE and real failures propagate as errors.
    struct ComApartment {
        _private: (),
    }

    impl ComApartment {
        fn init() -> Result<Self, ShortcutError> {
            // SAFETY: pvReserved must be NULL; any thread may init COM.
            let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            if hr < 0 {
                return Err(hr_err("CoInitializeEx", hr));
            }
            Ok(Self { _private: () })
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            // SAFETY: balanced against the successful CoInitializeEx in init.
            unsafe { CoUninitialize() }
        }
    }

    /// CoCreateInstance for an in-proc object, wrapped in a ComPtr.
    unsafe fn create_instance<T>(clsid: &GUID, iid: &GUID) -> Result<ComPtr<T>, ShortcutError> {
        let mut ptr: *mut T = ptr::null_mut();
        // SAFETY: ppv receives either a valid interface pointer or NULL; on
        // failure we return before constructing the ComPtr.
        let hr = unsafe {
            CoCreateInstance(
                clsid,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                iid,
                &mut ptr as *mut *mut T as *mut *mut c_void,
            )
        };
        if hr < 0 {
            return Err(hr_err("CoCreateInstance(ShellLink)", hr));
        }
        // A compliant server never returns success with a null pointer, but
        // guard anyway: dereferencing it would be UB, not just a failure.
        if ptr.is_null() {
            return Err(hr_err("CoCreateInstance(ShellLink)", -1));
        }
        Ok(ComPtr { ptr })
    }

    /// QueryInterface via the IUnknown slots shared by every COM object.
    unsafe fn query_interface<T, U>(
        obj: &ComPtr<T>,
        iid: &GUID,
    ) -> Result<ComPtr<U>, ShortcutError> {
        let mut ptr: *mut U = ptr::null_mut();
        // SAFETY: the source pointer is live for the call; per COM rules the
        // interface's first vtable slot is QueryInterface.
        let hr = unsafe {
            let vtbl = (*(obj.ptr() as *mut IUnknownObject)).vtable;
            ((*vtbl).query_interface)(
                obj.ptr() as *mut c_void,
                iid,
                &mut ptr as *mut *mut U as *mut *mut c_void,
            )
        };
        if hr < 0 {
            return Err(hr_err("IShellLinkW::QueryInterface(IPersistFile)", hr));
        }
        if ptr.is_null() {
            return Err(hr_err("IShellLinkW::QueryInterface(IPersistFile)", -1));
        }
        Ok(ComPtr { ptr })
    }

    /// Writes a .lnk file: resolves the target shell link, sets fields, then
    /// persists through IPersistFile.
    pub fn write_lnk(
        lnk_path: &[u16],
        target: &[u16],
        args: &[u16],
        work_dir: &[u16],
        icon: &[u16],
    ) -> Result<(), ShortcutError> {
        // The apartment must outlive every interface we create here, so it is
        // initialized first and dropped last.
        let _com = ComApartment::init()?;

        // SAFETY: all interface pointers come from successful COM calls and
        // are released by ComPtr drops in reverse creation order.
        unsafe {
            let link: ComPtr<IShellLinkW> = create_instance(&ShellLink, &IID_ISHELL_LINK_W)?;
            let vt = (*link.ptr()).vtable;

            let this = link.ptr() as *mut c_void;
            for (api, hr) in [
                (
                    "IShellLinkW::SetPath",
                    ((*vt).set_path)(this, target.as_ptr()),
                ),
                (
                    "IShellLinkW::SetArguments",
                    ((*vt).set_arguments)(this, args.as_ptr()),
                ),
                (
                    "IShellLinkW::SetWorkingDirectory",
                    ((*vt).set_working_directory)(this, work_dir.as_ptr()),
                ),
                (
                    "IShellLinkW::SetIconLocation",
                    ((*vt).set_icon_location)(this, icon.as_ptr(), 0),
                ),
            ] {
                if hr < 0 {
                    return Err(hr_err(api, hr));
                }
            }

            let persist: ComPtr<IPersistFile> = query_interface(&link, &IID_IPERSIST_FILE)?;
            let pvt = (*persist.ptr()).vtable;
            // fRemember=TRUE records the path as the object's current file;
            // harmless for a fresh object and required by convention.
            let hr = ((*pvt).save)(persist.ptr() as *mut c_void, lnk_path.as_ptr(), 1);
            if hr < 0 {
                return Err(hr_err("IPersistFile::Save", hr));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn hr_err(api: &'static str, hr: i32) -> ShortcutError {
    ShortcutError::Com { api, hr }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GUID-shaped ids as `profile.id` can hold them: braced or bare.
    const ID_A: &str = "{11111111-2222-3333-4444-555555555555}";
    const ID_A_BARE: &str = "11111111-2222-3333-4444-555555555555";
    const ID_B: &str = "{66666666-7777-8888-9999-000000000000}";
    /// ID_A reduced to its hex digits — the GUID fallback base name.
    const ID_A_HEX: &str = "11111111222233334444555555555555";

    #[test]
    fn file_name_uses_profile_name() {
        assert_eq!(shortcut_file_name("Dev Env", ID_A), "Dev Env-11111111.lnk");
    }

    #[test]
    fn file_name_replaces_illegal_chars() {
        assert_eq!(
            shortcut_file_name("a/b\\c:d*e?f\"g<h>i|j", ID_A),
            "a_b_c_d_e_f_g_h_i_j-11111111.lnk"
        );
    }

    #[test]
    fn file_name_appends_id_tag() {
        // The tag is the id's first 8 hex digits, lowercased, with braces
        // and hyphens skipped; braced and bare ids give the same name.
        assert_eq!(
            shortcut_file_name("SmokeTest", "{ABCDEF12-3456-7890-abcd-ef1234567890}"),
            "SmokeTest-abcdef12.lnk"
        );
        assert_eq!(
            shortcut_file_name("SmokeTest", ID_A),
            shortcut_file_name("SmokeTest", ID_A_BARE)
        );
    }

    #[test]
    fn file_name_tag_disambiguates_sanitized_collisions() {
        // "a:b" and "a|b" both sanitize to "a_b"; the per-profile tag keeps
        // a later shortcut from overwriting a different profile's .lnk.
        assert_eq!(shortcut_file_name("a:b", ID_A), "a_b-11111111.lnk");
        assert_eq!(shortcut_file_name("a|b", ID_B), "a_b-66666666.lnk");
        // Same name, different profiles: also distinct files.
        assert_ne!(
            shortcut_file_name("same", ID_A),
            shortcut_file_name("same", ID_B)
        );
    }

    #[test]
    fn file_name_trims_trailing_dots_and_spaces() {
        // "name." / "name " cannot be created on Windows; trimming fixes it.
        assert_eq!(shortcut_file_name("name. ", ID_A), "name-11111111.lnk");
        assert_eq!(shortcut_file_name("name...", ID_A), "name-11111111.lnk");
    }

    #[test]
    fn file_name_falls_back_to_guid() {
        let guid = format!("{ID_A_HEX}.lnk");
        assert_eq!(shortcut_file_name("", ID_A), guid);
        assert_eq!(shortcut_file_name("   ", ID_A), guid);
        assert_eq!(shortcut_file_name("...", ID_A), guid);
        assert_eq!(shortcut_file_name("///", ID_A), "___-11111111.lnk");
        // Reserved device names stay reserved even with an extension.
        assert_eq!(shortcut_file_name("con", ID_A), guid);
        assert_eq!(shortcut_file_name("LPT1", ID_A), guid);
        // Windows applies the reservation to the stem before the first dot,
        // so "con.foo.lnk" would be invalid too.
        assert_eq!(shortcut_file_name("con.foo", ID_A), guid);
        assert_eq!(shortcut_file_name("console", ID_A), "console-11111111.lnk");
    }

    #[test]
    fn file_name_reserved_stem_ignores_trailing_space_or_dot() {
        // Windows strips trailing spaces/dots while normalizing, so the stem
        // of "con .foo.lnk" resolves to the reserved "con".
        let guid = format!("{ID_A_HEX}.lnk");
        assert_eq!(shortcut_file_name("con .foo", ID_A), guid);
        assert_eq!(shortcut_file_name("CON ..x", ID_A), guid);
        // An interior " ." stays legal when the stem is not reserved.
        assert_eq!(
            shortcut_file_name("con1 .foo", ID_A),
            "con1 .foo-11111111.lnk"
        );
    }

    #[test]
    fn file_name_caps_long_names() {
        let long = "a".repeat(500);
        let name = shortcut_file_name(&long, ID_A);
        assert_eq!(name.len(), MAX_NAME_CHARS + "-11111111.lnk".len());
        // Truncation can expose a trailing dot/space, which is re-trimmed.
        let with_space = format!("{} .lnk", "a".repeat(MAX_NAME_CHARS - 1));
        assert_eq!(
            shortcut_file_name(&with_space, ID_A),
            format!("{}-11111111.lnk", "a".repeat(MAX_NAME_CHARS - 1))
        );
    }

    #[test]
    fn file_name_cap_counts_utf16_units() {
        // A supplementary-plane char is two UTF-16 units; the cap must drop
        // it as a whole rather than splitting the surrogate pair.
        let name = shortcut_file_name(&"😀".repeat(200), ID_A);
        let stem = name.strip_suffix("-11111111.lnk").unwrap();
        assert_eq!(stem.encode_utf16().count(), MAX_NAME_CHARS);
        assert_eq!(stem.chars().count(), MAX_NAME_CHARS / 2);
        // A pair straddling the cap is excluded, not halved.
        let boundary = format!("{}😀tail", "a".repeat(MAX_NAME_CHARS - 1));
        assert_eq!(
            shortcut_file_name(&boundary, ID_A),
            format!("{}-11111111.lnk", "a".repeat(MAX_NAME_CHARS - 1))
        );
    }

    #[test]
    fn file_name_maps_control_chars() {
        assert_eq!(shortcut_file_name("a\0b\tc", ID_A), "a_b_c-11111111.lnk");
    }
}
