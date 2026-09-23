//! Resident core (Phase 2) and integration (Phase 3).
//!
//! The resident owns a Named Mutex so only one instance per session runs, a
//! message-only window, a tray icon with an Exit menu, and a `GetMessage`
//! loop that blocks until Windows delivers something. Blocking in
//! `GetMessage` is what makes the Zero Idle Architecture possible: no
//! polling, no timers, no file watching. A missing tray icon is never
//! fatal: the icon is re-registered whenever Explorer broadcasts
//! TaskbarCreated, which also covers a logon autostart that ran before the
//! taskbar existed.
//!
//! Profiles execute on a temporary worker thread so a Wait action cannot
//! stall the message loop. The trigger is a WM_COPYDATA request (see
//! `ipc.rs`); each accepted request gets its own worker — bounded by
//! `MAX_CONCURRENT_WORKERS` so a burst of requests cannot exhaust threads —
//! so concurrent requests run independently and the only shared state is
//! the immutable `Arc<Config>`. Global hotkeys are intentionally not
//! implemented.
//!
//! The config is re-read only when a run request arrives and the file's
//! mtime changed since the last load — still purely event-driven, so nothing
//! happens while the user is idle.

use crate::actions;
use crate::config::{Action, Config, ConfigError};
use crate::profile::{self, ActionStatus, ProfileResult};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::SystemTime;

/// Identifies the single resident instance. `Local\` scopes the mutex to the
/// current session, so each logged-on user gets their own resident instead of
/// one user blocking another.
#[cfg(windows)]
const MUTEX_NAME: &str = r"Local\ProjectLauncher.Resident";

/// Class name of the message-only window. The `--profile` frontend locates
/// the resident by this class, so it must stay stable and unique to this
/// program.
#[cfg(windows)]
pub(crate) const WINDOW_CLASS: &str = "ProjectLauncher.ResidentMessageWindow";

/// Callback message the tray icon posts back to our window.
#[cfg(windows)]
const WM_TRAYICON: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// Menu command id for the tray's Exit item.
#[cfg(windows)]
const IDM_EXIT: usize = 1;

/// NOTIFYICONDATA.uID for our single tray icon.
#[cfg(windows)]
const TRAY_ICON_ID: u32 = 1;

/// Dynamic message id for "TaskbarCreated", filled in by `run`. Explorer
/// broadcasts it after a restart; on receipt the tray icon is re-registered
/// because the old one died with the old taskbar.
#[cfg(windows)]
static TASKBAR_CREATED_MSG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[derive(Debug)]
pub enum ResidentError {
    /// Another resident already owns the mutex. Not a failure: the desired
    /// state (a resident is running) already holds.
    AlreadyRunning,
    Config(ConfigError),
    /// A Win32 call failed; `code` is GetLastError().
    Win32 {
        api: &'static str,
        code: u32,
    },
    /// The host OS does not provide the required Win32 API.
    #[allow(dead_code)]
    UnsupportedPlatform,
}

impl fmt::Display for ResidentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResidentError::AlreadyRunning => {
                write!(f, "another Project Launcher resident is already running")
            }
            ResidentError::Config(e) => write!(f, "{e}"),
            ResidentError::Win32 { api, code } => {
                write!(f, "{api} failed (GetLastError={code})")
            }
            ResidentError::UnsupportedPlatform => write!(f, "the resident requires Windows"),
        }
    }
}

impl std::error::Error for ResidentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResidentError::Config(e) => Some(e),
            _ => None,
        }
    }
}

/// Everything the resident keeps in memory between messages: the config it
/// loaded and the file mtime it was loaded from. Each run request re-stats
/// the file and reloads only when the mtime changed — event-driven, so the
/// zero-idle guarantee is preserved (nothing happens between requests).
pub struct ResidentState {
    /// Path the config is reloaded from; `None` in tests, which never reload.
    config_path: Option<PathBuf>,
    /// The live config plus the file's mtime at load time.
    inner: RwLock<(Arc<Config>, Option<SystemTime>)>,
}

impl ResidentState {
    /// Test constructor: no path, so `current_config` never reloads.
    #[cfg(test)]
    pub fn new(config: Config) -> Self {
        Self {
            config_path: None,
            inner: RwLock::new((Arc::new(config), None)),
        }
    }

    /// Production constructor: remembers where the config lives so requests
    /// can reload it when the file changed.
    #[cfg(windows)]
    fn with_path(config: Config, path: PathBuf) -> Self {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            config_path: Some(path),
            inner: RwLock::new((Arc::new(config), modified)),
        }
    }

    /// Returns the current config, reloading it first when the file's mtime
    /// changed since the last load. A failed reload keeps the previous
    /// config (a corrupt edit must not brick a running resident) and logs
    /// loudly; the check is a single metadata call per request, never a
    /// timer or watcher.
    pub fn current_config(&self) -> Arc<Config> {
        let Some(path) = &self.config_path else {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            return Arc::clone(&guard.0);
        };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            if mtime == guard.1 {
                return Arc::clone(&guard.0);
            }
        }
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        // Re-check: another request may have reloaded while we waited.
        if mtime != guard.1 {
            match Config::load(path) {
                Ok(config) => {
                    eprintln!("[resident] config reloaded from {}", path.display());
                    *guard = (Arc::new(config), mtime);
                }
                Err(e) => {
                    eprintln!("[resident] config reload failed: {e}; keeping previous config");
                }
            }
        }
        Arc::clone(&guard.0)
    }

    pub fn profile_count(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .profiles
            .len()
    }
}

/// How a worker thread launches one action. A plain `fn` pointer keeps the
/// worker `Send` and lets tests substitute a launcher that never touches the
/// OS.
pub type ActionLauncher = fn(&Action) -> Result<(), String>;

/// The production launcher: hand the action to Windows.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "only called by the Windows IPC path")
)]
pub fn launch_action(action: &Action) -> Result<(), String> {
    actions::execute(action).map_err(|e| e.to_string())
}

/// Starts a profile on a temporary worker thread and returns immediately, so
/// the caller's message loop keeps pumping while the profile runs.
///
/// The join handle yields `None` if the profile id is not in the config.
#[allow(
    dead_code,
    reason = "kept as the testable primitive behind dispatch_profile"
)]
pub fn spawn_profile_run(
    config: Arc<Config>,
    profile_id: String,
    launcher: ActionLauncher,
) -> JoinHandle<Option<ProfileResult>> {
    std::thread::spawn(move || {
        let profile = profile::find_profile(&config, &profile_id)?;
        Some(profile::execute_profile(profile, &mut |action| {
            launcher(action)
        }))
    })
}

/// Upper bound on profile workers running at once. Every accepted IPC
/// request spawns a detached thread; without a cap a flood of requests
/// could exhaust the process's thread budget. Requests beyond the cap are
/// rejected so the sender learns that nothing was started.
const MAX_CONCURRENT_WORKERS: usize = 16;

/// Slots currently held by live workers. `dispatch_profile` claims a slot
/// before spawning; [`WorkerGuard`] returns it when the worker exits.
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Returns a claimed worker slot to [`ACTIVE_WORKERS`] when the worker
/// exits — on normal completion, an early return, or a panic unwinding the
/// thread.
struct WorkerGuard;

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        ACTIVE_WORKERS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Dispatches an IPC run request onto a detached worker thread and logs the
/// outcome to stderr. Returns false without spawning when the profile id is
/// unknown or [`MAX_CONCURRENT_WORKERS`] workers are already running, so
/// the window proc can reject the sender's request.
///
/// The profile's existence is checked before spawning: inside the window
/// proc, work must stay trivial so the sender's SendMessageW returns
/// quickly. The same is why `Builder::spawn` is used instead of
/// `std::thread::spawn`: it reports failure with `Err` rather than
/// panicking, and a panic inside the window proc would unwind across the
/// FFI boundary and abort the resident.
pub fn dispatch_profile(config: &Arc<Config>, profile_id: &str, launcher: ActionLauncher) -> bool {
    if profile::find_profile(config, profile_id).is_none() {
        return false;
    }
    // Claim a slot before spawning so concurrent requests cannot push the
    // worker count past the cap.
    if ACTIVE_WORKERS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |in_flight| {
            (in_flight < MAX_CONCURRENT_WORKERS).then_some(in_flight + 1)
        })
        .is_err()
    {
        eprintln!(
            "[resident] rejected run request: {MAX_CONCURRENT_WORKERS} workers already active"
        );
        return false;
    }
    let config = Arc::clone(config);
    let profile_id = profile_id.to_string();
    let spawned = std::thread::Builder::new()
        .name("profile-worker".to_string())
        .spawn(move || {
            // Held until the closure exits so the slot is released on every
            // path, including a panic unwind.
            let _guard = WorkerGuard;
            // find_profile cannot fail here: the config is immutable and the
            // id was found a moment ago on the caller thread.
            let Some(profile) = profile::find_profile(&config, &profile_id) else {
                return;
            };
            eprintln!(
                "[resident] profile '{}' ({profile_id}) started",
                profile.name
            );
            let result = profile::execute_profile(profile, &mut |a| launcher(a));
            for outcome in &result.outcomes {
                match &outcome.status {
                    ActionStatus::Succeeded => {
                        eprintln!("[resident]   [ok]     {}", outcome.action_id)
                    }
                    ActionStatus::Failed(e) => {
                        eprintln!("[resident]   [FAILED] {}: {e}", outcome.action_id)
                    }
                }
            }
            eprintln!(
                "[resident] profile {profile_id} finished: {} of {} action(s) failed, {} disabled skipped",
                result.failure_count(),
                result.outcomes.len(),
                result.skipped_disabled
            );
        });
    match spawned {
        // The join handle is dropped on purpose: the worker is detached.
        Ok(_worker) => true,
        Err(e) => {
            // No thread started, so hand the claimed slot back.
            ACTIVE_WORKERS.fetch_sub(1, Ordering::Relaxed);
            eprintln!("[resident] failed to spawn a profile worker: {e}");
            false
        }
    }
}

/// Loads the config, claims the single-instance mutex, creates the
/// message-only window, registers the tray icon when the taskbar is ready
/// to accept it, then blocks in the message loop until the window is
/// closed.
#[cfg(windows)]
pub fn run() -> Result<(), ResidentError> {
    let config_path = crate::config::config_path().map_err(ResidentError::Config)?;
    let config = Config::load(&config_path).map_err(ResidentError::Config)?;
    let icon = load_shared_icon()?;
    let context = ResidentContext {
        state: ResidentState::with_path(config, config_path),
        icon,
    };

    // Claimed before the window exists so a second instance cannot race us
    // into creating a duplicate window. Released when `_instance` drops.
    let _instance = SingleInstance::acquire()?;
    let window = MessageWindow::create(&context)?;
    // NIM_ADD legitimately fails when the resident starts before Explorer
    // (e.g. a logon autostart while the taskbar is not up yet). That must
    // not abort startup: the message loop works without a tray icon and
    // the TaskbarCreated broadcast re-adds the icon once Explorer is up.
    // The Option keeps ownership so a registered icon is still removed on
    // drop, including the error paths below.
    let _tray = match TrayIcon::new(window.hwnd, context.icon) {
        Ok(tray) => Some(tray),
        Err(e) => {
            eprintln!("[resident] tray icon registration failed, continuing without it: {e}");
            None
        }
    };
    register_taskbar_created()?;

    // Return the physical pages touched during startup to the system: the
    // resident spends its life idle, so pages it may never touch again should
    // not stay resident. Virtual (private) memory is unchanged; touched pages
    // fault back in on demand.
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }

    println!(
        "Resident started: {} profile(s) loaded. Waiting for messages.",
        context.state.profile_count()
    );
    window.run_message_loop()
}

/// Everything the window proc needs while handling a message. A pointer to
/// this lives in GWLP_USERDATA for the window's lifetime.
#[cfg(windows)]
struct ResidentContext {
    state: ResidentState,
    /// Shared system icon; must not be destroyed (it is not ours).
    icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
}

/// Loads the shared application icon for the tray. The returned handle is
/// owned by the system: callers must never DestroyIcon it.
#[cfg(windows)]
fn load_shared_icon() -> Result<windows_sys::Win32::UI::WindowsAndMessaging::HICON, ResidentError> {
    use std::ptr;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDI_APPLICATION, LoadIconW};

    // SAFETY: a null instance loads a stock system icon, which Windows owns.
    unsafe {
        let icon = LoadIconW(ptr::null_mut(), IDI_APPLICATION);
        if icon.is_null() {
            return Err(ResidentError::Win32 {
                api: "LoadIconW",
                code: GetLastError(),
            });
        }
        Ok(icon)
    }
}

/// Registers for Explorer's TaskbarCreated broadcast so the tray icon can be
/// re-added after an Explorer restart.
#[cfg(windows)]
fn register_taskbar_created() -> Result<(), ResidentError> {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;

    let name = to_wide("TaskbarCreated");
    // SAFETY: `name` is a NUL-terminated buffer that outlives the call.
    // GetLastError is read inside the unsafe block, before anything else can
    // overwrite it.
    let msg = unsafe {
        let msg = RegisterWindowMessageW(name.as_ptr());
        if msg == 0 {
            return Err(ResidentError::Win32 {
                api: "RegisterWindowMessageW",
                code: GetLastError(),
            });
        }
        msg
    };
    TASKBAR_CREATED_MSG.store(msg, Ordering::Relaxed);
    Ok(())
}

#[cfg(not(windows))]
pub fn run() -> Result<(), ResidentError> {
    Err(ResidentError::UnsupportedPlatform)
}

/// Owns the Named Mutex for as long as the resident runs.
#[cfg(windows)]
struct SingleInstance {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl SingleInstance {
    fn acquire() -> Result<Self, ResidentError> {
        use std::ptr;
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name = to_wide(MUTEX_NAME);
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call. GetLastError is read immediately, before any other API call,
        // because CreateMutexW reports an existing mutex that way while
        // still returning a valid handle.
        unsafe {
            let handle = CreateMutexW(ptr::null(), 1, name.as_ptr());
            if handle.is_null() {
                return Err(ResidentError::Win32 {
                    api: "CreateMutexW",
                    code: GetLastError(),
                });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // The handle is valid but owned by the other instance; close
                // ours so we do not leak it.
                CloseHandle(handle);
                return Err(ResidentError::AlreadyRunning);
            }
            Ok(Self { handle })
        }
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;

        // SAFETY: `handle` came from CreateMutexW with initial ownership and
        // is closed exactly once, here.
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

/// A message-only window plus the window class registered for it.
#[cfg(windows)]
struct MessageWindow {
    hwnd: windows_sys::Win32::Foundation::HWND,
    class_name: Vec<u16>,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
}

#[cfg(windows)]
impl MessageWindow {
    /// Creates the window and stores `context` in GWLP_USERDATA so the window
    /// proc can reach the resident state. The caller must keep `context`
    /// alive until after the window is dropped.
    fn create(context: *const ResidentContext) -> Result<Self, ResidentError> {
        use std::mem::zeroed;
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, HWND_MESSAGE, RegisterClassW, UnregisterClassW, WNDCLASSW,
        };

        let class_name = to_wide(WINDOW_CLASS);
        // SAFETY: the class name outlives the window (both live in `Self`).
        // A message-only window needs no icon, cursor or background brush, so
        // those stay null. HWND_MESSAGE as the parent is what keeps the
        // window off-screen and free of paint/input traffic.
        unsafe {
            let instance = GetModuleHandleW(ptr::null());
            if instance.is_null() {
                return Err(ResidentError::Win32 {
                    api: "GetModuleHandleW",
                    code: GetLastError(),
                });
            }
            let mut class: WNDCLASSW = zeroed();
            class.lpfnWndProc = Some(wnd_proc);
            class.hInstance = instance;
            class.lpszClassName = class_name.as_ptr();
            if RegisterClassW(&class) == 0 {
                return Err(ResidentError::Win32 {
                    api: "RegisterClassW",
                    code: GetLastError(),
                });
            }
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                instance,
                ptr::null(),
            );
            if hwnd.is_null() {
                // Read GetLastError first: UnregisterClassW may overwrite
                // it. The class registered above is removed so a failed
                // create leaves nothing behind.
                let code = GetLastError();
                UnregisterClassW(class_name.as_ptr(), instance);
                return Err(ResidentError::Win32 {
                    api: "CreateWindowExW",
                    code,
                });
            }
            // Attach the context so the window proc can reach it. Cleared on
            // WM_DESTROY; the pointed-to struct outlives the window because
            // locals drop in reverse declaration order.
            use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_USERDATA, SetWindowLongPtrW};
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize);
            Ok(Self {
                hwnd,
                class_name,
                instance,
            })
        }
    }

    /// Blocks until WM_QUIT. Zero CPU is used while waiting because
    /// GetMessageW sleeps until a message arrives.
    fn run_message_loop(&self) -> Result<(), ResidentError> {
        use std::mem::zeroed;
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG};

        // SAFETY: `msg` is a valid out-parameter for every iteration.
        // TranslateMessage is intentionally omitted: a message-only window
        // receives no keyboard input, so there is nothing to translate.
        unsafe {
            let mut msg: MSG = zeroed();
            loop {
                match GetMessageW(&mut msg, ptr::null_mut(), 0, 0) {
                    0 => return Ok(()), // WM_QUIT
                    -1 => {
                        return Err(ResidentError::Win32 {
                            api: "GetMessageW",
                            code: GetLastError(),
                        });
                    }
                    _ => {
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
impl Drop for MessageWindow {
    fn drop(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, UnregisterClassW};

        // SAFETY: the window is destroyed before its class is unregistered,
        // as Windows requires. Both are owned by this struct and released
        // exactly once. DestroyWindow on an already-destroyed window (the
        // WM_CLOSE path) fails harmlessly.
        unsafe {
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class_name.as_ptr(), self.instance);
        }
    }
}

/// Sends NIM_ADD for our tray icon. Also used for re-registration after an
/// Explorer restart: per the TaskbarCreated contract the icon must simply be
/// added again.
#[cfg(windows)]
fn add_tray_icon(
    hwnd: windows_sys::Win32::Foundation::HWND,
    icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
) -> Result<(), ResidentError> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NOTIFYICONDATAW, Shell_NotifyIconW,
    };

    // SAFETY: `data` is fully initialized for the fields flagged in uFlags;
    // szTip is always NUL-terminated because the array starts zeroed and
    // the copy below leaves at least one slot untouched.
    unsafe {
        let mut data: NOTIFYICONDATAW = zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ICON_ID;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAYICON;
        data.hIcon = icon;
        let tip = to_wide("Project Launcher");
        // szTip is a fixed [u16; 128] and copy_from_slice panics when the
        // tip does not fit. The literal is far below the limit today; the
        // assert documents that contract while the clamp keeps a future
        // longer tip from panicking — one slot stays zeroed so the field
        // remains NUL-terminated.
        debug_assert!(tip.len() <= data.szTip.len());
        let tip_len = tip.len().min(data.szTip.len() - 1);
        data.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            return Err(ResidentError::Win32 {
                api: "Shell_NotifyIconW(NIM_ADD)",
                code: GetLastError(),
            });
        }
    }
    Ok(())
}

/// Owns the tray icon: removes it on drop, including error paths in `run`.
#[cfg(windows)]
struct TrayIcon {
    hwnd: windows_sys::Win32::Foundation::HWND,
}

#[cfg(windows)]
impl TrayIcon {
    fn new(
        hwnd: windows_sys::Win32::Foundation::HWND,
        icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    ) -> Result<Self, ResidentError> {
        add_tray_icon(hwnd, icon)?;
        Ok(Self { hwnd })
    }
}

#[cfg(windows)]
impl Drop for TrayIcon {
    fn drop(&mut self) {
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::UI::Shell::{NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};

        // SAFETY: only cbSize/hWnd/uID are required to identify the icon to
        // remove. Failure is ignored: the icon disappears with the window
        // anyway.
        unsafe {
            let mut data: NOTIFYICONDATAW = zeroed();
            data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = self.hwnd;
            data.uID = TRAY_ICON_ID;
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

/// Reads the ResidentContext attached to the window. Returns None before
/// GWLP_USERDATA is set and after it is cleared on WM_DESTROY.
///
/// SAFETY: the pointer stored in GWLP_USERDATA refers to a ResidentContext
/// that lives on `run`'s stack frame for the whole window lifetime.
#[cfg(windows)]
unsafe fn context_of(
    hwnd: windows_sys::Win32::Foundation::HWND,
) -> Option<&'static ResidentContext> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_USERDATA, GetWindowLongPtrW};

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ResidentContext;
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the pointer was set by MessageWindow::create from a live
        // reference; WM_DESTROY clears it before the context can die.
        Some(unsafe { &*ptr })
    }
}

/// Handles a WM_COPYDATA run request. Returns nonzero when the request was
/// accepted so the sender can tell "handed off" from "rejected".
#[cfg(windows)]
fn on_copy_data(
    hwnd: windows_sys::Win32::Foundation::HWND,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use crate::config::is_valid_guid;
    use crate::ipc::{self, IpcRequest};
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;

    // SAFETY: per WM_COPYDATA, lparam is a COPYDATASTRUCT valid for the
    // duration of this call; the payload slice is copied out before decode.
    unsafe {
        let cds = lparam as *const COPYDATASTRUCT;
        if cds.is_null() {
            return 0;
        }
        let cds = &*cds;
        if cds.lpData.is_null()
            || cds.cbData == 0
            || !cds.cbData.is_multiple_of(2)
            || cds.cbData > ipc::MAX_IPC_BYTES
        {
            return 0;
        }
        let data = std::slice::from_raw_parts(cds.lpData as *const u16, (cds.cbData / 2) as usize);
        let request = match ipc::decode(cds.dwData, data) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[resident] rejected IPC payload: {e}");
                return 0;
            }
        };
        let IpcRequest::RunProfile { profile } = request;
        if !is_valid_guid(&profile) {
            eprintln!("[resident] rejected IPC request: {profile:?} is not a GUID");
            return 0;
        }
        let Some(ctx) = context_of(hwnd) else {
            return 0;
        };
        let config = ctx.state.current_config();
        if dispatch_profile(&config, &profile, launch_action) {
            1
        } else {
            // Rejected either because the profile id is unknown or because
            // dispatch_profile declined (worker cap or spawn failure, both
            // logged there).
            eprintln!("[resident] rejected IPC run request for profile {profile}");
            0
        }
    }
}

/// Shows the tray popup menu (Exit) at the cursor position.
#[cfg(windows)]
fn show_tray_menu(hwnd: windows_sys::Win32::Foundation::HWND) {
    use std::mem::zeroed;
    use std::ptr;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, DestroyWindow, GetCursorPos, MF_STRING,
        PostMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD,
        TrackPopupMenu, WM_NULL,
    };

    // SAFETY: all handles are created and destroyed within this call. The
    // SetForegroundWindow + WM_NULL pair is the documented workaround that
    // makes a popup menu dismiss correctly for windows that never have
    // focus.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let label = to_wide("終了(&X)");
        AppendMenuW(menu, MF_STRING, IDM_EXIT, label.as_ptr());
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if command == IDM_EXIT as i32 {
            // Same teardown path as WM_CLOSE.
            DestroyWindow(hwnd);
        }
    }
}

/// Window proc for the message-only window. Everything handled here is
/// event-driven; nothing runs on a timer.
#[cfg(windows)]
unsafe extern "system" fn wnd_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, GWLP_USERDATA, PostQuitMessage, SetWindowLongPtrW, WM_CLOSE,
        WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY, WM_RBUTTONUP,
    };

    // SAFETY: called by Windows with a valid hwnd for our class. Messages we
    // do not handle go to the default handler, which is required for correct
    // window behavior.
    unsafe {
        if msg == WM_COPYDATA {
            return on_copy_data(hwnd, lparam);
        }
        if msg == WM_TRAYICON {
            // lparam carries the mouse message for icons without a
            // NOTIFYICON_VERSION_4 registration.
            let event = lparam as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            }
            return 0;
        }
        let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Relaxed);
        if taskbar_created != 0 && msg == taskbar_created {
            // Explorer restarted (or finally came up after we started): the
            // old icon died with the old taskbar, so re-register it. A
            // failure is only logged — a missing icon must not take down
            // the message loop.
            if let Some(ctx) = context_of(hwnd)
                && let Err(e) = add_tray_icon(hwnd, ctx.icon)
            {
                eprintln!("[resident] tray icon re-registration failed: {e}");
            }
            return 0;
        }
        match msg {
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                // Stop dereferencing the context before it goes out of scope.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActionKind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const CONFIG: &str = r#"{
        "version": 1,
        "profiles": [
            {
                "id": "{11111111-2222-3333-4444-555555555555}",
                "name": "Worker",
                "actions": [
                    { "id": "w1", "type": "wait", "milliseconds": 0 },
                    { "id": "w2", "type": "wait", "milliseconds": 0 },
                    { "id": "off", "type": "wait", "milliseconds": 0, "disabled": true }
                ]
            }
        ]
    }"#;

    const PROFILE_ID: &str = "{11111111-2222-3333-4444-555555555555}";

    fn state() -> ResidentState {
        ResidentState::new(Config::parse(CONFIG).unwrap())
    }

    static SLOW_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Stands in for a slow action (e.g. a Wait) without touching the OS.
    fn slow_launcher(_action: &Action) -> Result<(), String> {
        SLOW_CALLS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(150));
        Ok(())
    }

    fn ok_launcher(_action: &Action) -> Result<(), String> {
        Ok(())
    }

    fn failing_launcher(action: &Action) -> Result<(), String> {
        if action.id == "w1" {
            Err("boom".to_string())
        } else {
            Ok(())
        }
    }

    #[test]
    fn state_exposes_config_loaded_once() {
        let state = state();
        assert_eq!(state.profile_count(), 1);
        assert!(profile::find_profile(&state.current_config(), PROFILE_ID).is_some());
    }

    /// Request-time reload: a changed file is picked up, an unchanged one is
    /// served from memory, and a broken rewrite keeps the last valid config.
    #[cfg(windows)]
    #[test]
    fn reloads_config_only_when_the_file_changes() {
        const OTHER_ID: &str = "{99999999-8888-7777-6666-555555555555}";
        let path = std::env::temp_dir().join(format!(
            "project-launcher-test-reload-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, CONFIG).unwrap();
        let state = ResidentState::with_path(Config::load(&path).unwrap(), path.clone());

        // Same mtime -> cached copy.
        let first = state.current_config();
        assert!(profile::find_profile(&first, OTHER_ID).is_none());

        // New mtime -> reload picks up the added profile.
        std::thread::sleep(Duration::from_millis(20));
        let v2 = format!(
            r#"{{"version":1,"profiles":[{},{{"id":"{OTHER_ID}","name":"Added","actions":[]}}]}}"#,
            r#"{"id":"{11111111-2222-3333-4444-555555555555}","name":"Worker","actions":[]}"#
        );
        std::fs::write(&path, v2).unwrap();
        let second = state.current_config();
        assert!(profile::find_profile(&second, OTHER_ID).is_some());
        assert_eq!(second.profiles.len(), 2);

        // Broken rewrite -> last valid config is kept.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "{ not json").unwrap();
        let third = state.current_config();
        assert!(profile::find_profile(&third, OTHER_ID).is_some());

        let _ = std::fs::remove_file(&path);
    }

    /// The whole point of the worker thread: spawning must not block the
    /// caller, which in the resident is the message loop.
    #[test]
    fn spawn_returns_before_the_profile_finishes() {
        let state = state();
        SLOW_CALLS.store(0, Ordering::SeqCst);

        let started = Instant::now();
        let handle = spawn_profile_run(
            state.current_config(),
            PROFILE_ID.to_string(),
            slow_launcher,
        );
        let spawn_elapsed = started.elapsed();

        let result = handle.join().unwrap().unwrap();
        let total_elapsed = started.elapsed();

        assert!(
            spawn_elapsed < Duration::from_millis(100),
            "spawn blocked for {spawn_elapsed:?}"
        );
        // Two enabled actions at 150ms each only add up if the worker ran
        // them, so this also proves the run really happened off-thread.
        assert!(
            total_elapsed >= Duration::from_millis(300),
            "worker finished too fast: {total_elapsed:?}"
        );
        assert_eq!(SLOW_CALLS.load(Ordering::SeqCst), 2);
        assert!(result.is_success());
    }

    #[test]
    fn worker_skips_disabled_and_reports_partial_failure() {
        let state = state();

        let ok = spawn_profile_run(state.current_config(), PROFILE_ID.to_string(), ok_launcher)
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(ok.outcomes.len(), 2);
        assert_eq!(ok.skipped_disabled, 1);
        assert!(ok.is_success());

        let failed = spawn_profile_run(
            state.current_config(),
            PROFILE_ID.to_string(),
            failing_launcher,
        )
        .join()
        .unwrap()
        .unwrap();
        assert_eq!(failed.failure_count(), 1);
        assert!(!failed.is_success());
    }

    #[test]
    fn worker_reports_unknown_profile_id() {
        let state = state();
        let handle = spawn_profile_run(
            state.current_config(),
            "{99999999-8888-7777-6666-555555555555}".to_string(),
            ok_launcher,
        );
        assert!(handle.join().unwrap().is_none());
    }

    static ARRIVED: AtomicUsize = AtomicUsize::new(0);

    /// Each profile run's first action waits for the other run to arrive, so
    /// the two runs can only both succeed if they really execute in parallel.
    fn rendezvous_launcher(_action: &Action) -> Result<(), String> {
        if ARRIVED.fetch_add(1, Ordering::SeqCst) == 0 {
            let deadline = Instant::now() + Duration::from_secs(5);
            while ARRIVED.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if ARRIVED.load(Ordering::SeqCst) < 2 {
                return Err("the other run never started".to_string());
            }
        }
        Ok(())
    }

    /// IPC dispatch policy: every accepted request gets its own worker, so
    /// two runs overlap instead of queueing behind each other.
    #[test]
    fn concurrent_profile_runs_execute_in_parallel() {
        let state = state();
        ARRIVED.store(0, Ordering::SeqCst);
        let h1 = spawn_profile_run(
            state.current_config(),
            PROFILE_ID.to_string(),
            rendezvous_launcher,
        );
        let h2 = spawn_profile_run(
            state.current_config(),
            PROFILE_ID.to_string(),
            rendezvous_launcher,
        );
        assert!(h1.join().unwrap().unwrap().is_success());
        assert!(h2.join().unwrap().unwrap().is_success());
    }

    #[test]
    fn dispatch_rejects_unknown_profile_without_spawning() {
        let state = state();
        assert!(!dispatch_profile(
            &state.current_config(),
            "{99999999-8888-7777-6666-555555555555}",
            ok_launcher
        ));
    }

    /// Flood protection: once MAX_CONCURRENT_WORKERS slots are taken,
    /// dispatch must refuse instead of spawning yet another thread. The
    /// counter is filled directly so the test needs no real workers.
    #[test]
    fn dispatch_rejects_requests_beyond_the_worker_cap() {
        let state = state();
        let saved = ACTIVE_WORKERS.swap(MAX_CONCURRENT_WORKERS, Ordering::SeqCst);
        // Restore before asserting so a failure cannot strand the counter.
        let accepted = dispatch_profile(&state.current_config(), PROFILE_ID, ok_launcher);
        ACTIVE_WORKERS.store(saved, Ordering::SeqCst);
        assert!(!accepted);
    }

    static DISPATCH_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn dispatch_launcher(_action: &Action) -> Result<(), String> {
        DISPATCH_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[test]
    fn dispatch_runs_profile_on_a_detached_worker() {
        let state = state();
        DISPATCH_CALLS.store(0, Ordering::SeqCst);
        assert!(dispatch_profile(
            &state.current_config(),
            PROFILE_ID,
            dispatch_launcher
        ));
        // The worker is detached, so poll until it finishes (bounded). A
        // separate counter from SLOW_CALLS keeps this race-free under the
        // parallel test runner.
        let deadline = Instant::now() + Duration::from_secs(5);
        while DISPATCH_CALLS.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(DISPATCH_CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn production_launcher_runs_a_wait_action() {
        let action = Action {
            id: "w".to_string(),
            disabled: false,
            kind: ActionKind::Wait { milliseconds: 1 },
        };
        assert!(launch_action(&action).is_ok());
    }
}
