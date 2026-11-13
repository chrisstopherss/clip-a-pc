//! The tray app: a hidden window that owns the tray icon and the Alt+C hotkey,
//! starts the recorder, and shows the settings window when asked.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_NOREPEAT};
use windows::Win32::UI::Shell::{ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::engine::{Engine, EngineCfg};
use super::log::{log, log_path, trim};
use super::settings_ui::{self, Pending, PENDING, SETTINGS_HWND, WM_APP_QUIT, WM_APP_SETTINGS_DONE};
use super::store::{autostart_enabled, load_config, save_config, set_autostart};
use super::toast;
use super::updater::{self, Outcome, VERSION};
use super::util::{memory_mb, videos_dir};
use super::video::list_monitors;
use crate::config::Config;

const WM_TRAY: u32 = WM_APP + 1;
const ID_SAVE: usize = 1001;
const ID_SETTINGS: usize = 1002;
const ID_FOLDER: usize = 1003;
const ID_LOG: usize = 1004;
const ID_QUIT: usize = 1005;
const ID_UPDATE: usize = 1006;
const WM_APP_APPLY_UPDATE: u32 = WM_APP + 4;
const HOTKEY_ID: i32 = 1;

struct State {
    engine: Option<Arc<Engine>>,
    cfg: Config,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static SAVING: AtomicBool = AtomicBool::new(false);
static FIRST_RUN: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static ICON: AtomicIsize = AtomicIsize::new(0);
/// Version of a downloaded, verified update that is waiting to be installed.
static UPDATE_READY: Mutex<Option<String>> = Mutex::new(None);

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State { engine: None, cfg: Config::default() }))
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

// ------------------------------------------------------------------ engine

fn start_engine(cfg: &Config) -> Option<Arc<Engine>> {
    let monitors = list_monitors();
    let idx = monitors.iter().position(|m| m.device_name == cfg.monitor).unwrap_or(0);
    match Engine::start(EngineCfg { secs: cfg.duration_secs, monitor: idx, mic_gain: cfg.mic_gain }) {
        Ok(e) => {
            let e = Arc::new(e);
            log(&format!("recording started: {} s clips, monitor {idx}, mic {}x", cfg.duration_secs, cfg.mic_gain));
            let watcher = e.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                log(&format!("video: {}", watcher.shared.video_info.lock().unwrap()));
                log(&format!("audio: {}", watcher.shared.audio_info.lock().unwrap()));
                // one status line every 5 minutes while this engine runs
                loop {
                    for _ in 0..300 {
                        if watcher.is_stopped() {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    let (span, used) = watcher.buffer_status();
                    let (p, w) = memory_mb();
                    log(&format!("status: buffered {span:.0} s ({:.0} MB), memory private {p:.0} MB, working set {w:.0} MB", used as f64 / 1048576.0));
                }
            });
            Some(e)
        }
        Err(e) => {
            log(&format!("could not start recording: {e}"));
            toast::notify("clip-a-pc could not start recording", &e.to_string());
            None
        }
    }
}

fn restart_engine(cfg: Config) {
    let old = {
        let mut st = state().lock().unwrap();
        st.cfg = cfg.clone();
        st.engine.take()
    };
    if let Some(e) = old {
        e.stop();
    }
    let new = start_engine(&cfg);
    state().lock().unwrap().engine = new;
}

fn save_clip() {
    if SAVING.swap(true, Ordering::SeqCst) {
        return;
    }
    let engine = state().lock().unwrap().engine.clone();
    let Some(engine) = engine else {
        SAVING.store(false, Ordering::SeqCst);
        toast::notify("Nothing to save", "Recording is not running.");
        return;
    };
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let t = Instant::now();
        let (p0, _) = memory_mb();
        match engine.save() {
            Ok(s) => {
                toast::chime();
                let name = s.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                toast::notify("Clip saved", &format!("{:.0} s, {:.1} MB  ({name})", s.seconds, s.bytes as f64 / 1048576.0));
                let (p1, w1) = memory_mb();
                log(&format!(
                    "clip saved: {} ({:.1} s, {:.1} MB) in {} ms, private memory {p0:.0} -> {p1:.0} MB, working set {w1:.0} MB",
                    s.path.display(),
                    s.seconds,
                    s.bytes as f64 / 1048576.0,
                    t.elapsed().as_millis()
                ));
            }
            Err(e) => {
                toast::notify("Could not save the clip", &e.to_string());
                log(&format!("save failed: {e}"));
            }
        }
        SAVING.store(false, Ordering::SeqCst);
    });
}

// ------------------------------------------------------------------ updates

/// Checks once shortly after start, then every 6 hours. The first check installs
/// right away (the buffer is nearly empty then); later ones wait for the user, so a
/// restart never throws away footage they might still want.
fn spawn_updater(hwnd: isize) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        updater::cleanup_old();
        let mut first = true;
        loop {
            let enabled = state().lock().unwrap().cfg.auto_update;
            let waiting = UPDATE_READY.lock().unwrap().is_some();
            if enabled && !waiting {
                match updater::check_and_download() {
                    Outcome::Ready(v) => {
                        log(&format!("update {v} downloaded and verified"));
                        *UPDATE_READY.lock().unwrap() = Some(v.clone());
                        let busy = SAVING.load(Ordering::SeqCst) || SETTINGS_HWND.load(Ordering::SeqCst) != 0 || FIRST_RUN.load(Ordering::SeqCst);
                        if first && !busy {
                            unsafe {
                                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_APPLY_UPDATE, WPARAM(0), LPARAM(0));
                            }
                        } else {
                            toast::notify("clip-a-pc update ready", &format!("Version {v} is ready. Right-click the tray icon and choose Restart to update."));
                        }
                    }
                    Outcome::UpToDate => log(&format!("update check: version {VERSION} is current")),
                    Outcome::Failed(e) => log(&format!("update check failed: {e}")),
                }
            }
            first = false;
            std::thread::sleep(Duration::from_secs(6 * 3600));
        }
    });
}

fn apply_update(hwnd: HWND) {
    match updater::apply_and_restart() {
        Ok(()) => {
            log("restarting into the new version");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        Err(e) => {
            log(&format!("update could not be installed: {e}"));
            toast::notify("Update could not be installed", &e);
        }
    }
}

// ------------------------------------------------------------------ tray

/// A red record dot, drawn at runtime so the exe needs no icon file.
unsafe fn make_icon() -> HICON {
    const N: i32 = 32;
    let mut px = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 + 0.5 - N as f32 / 2.0, y as f32 + 0.5 - N as f32 / 2.0);
            let d = (dx * dx + dy * dy).sqrt();
            let alpha = (13.5 - d).clamp(0.0, 1.0);
            let white = (5.0 - d).clamp(0.0, 1.0);
            let (r, g, b) = (229.0 * (1.0 - white) + 255.0 * white, 57.0 * (1.0 - white) + 255.0 * white, 53.0 * (1.0 - white) + 255.0 * white);
            let i = ((y * N + x) * 4) as usize;
            // premultiplied BGRA
            px[i] = (b * alpha) as u8;
            px[i + 1] = (g * alpha) as u8;
            px[i + 2] = (r * alpha) as u8;
            px[i + 3] = (alpha * 255.0) as u8;
        }
    }
    let mask = vec![0u8; (N * N / 8) as usize];
    let color = CreateBitmap(N, N, 1, 32, Some(px.as_ptr() as *const _));
    let mono = CreateBitmap(N, N, 1, 1, Some(mask.as_ptr() as *const _));
    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mono, hbmColor: color };
    let icon = CreateIconIndirect(&info).unwrap_or_default();
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mono.into());
    icon
}

unsafe fn tray_add(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: HICON(ICON.load(Ordering::Relaxed) as *mut _),
        ..Default::default()
    };
    for (i, u) in format!("clip-a-pc {VERSION}  (Alt+C saves a clip)").encode_utf16().take(127).enumerate() {
        nid.szTip[i] = u;
    }
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn tray_remove(hwnd: HWND) {
    let nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

fn open_path(hwnd: HWND, path: &std::path::Path) {
    unsafe {
        ShellExecuteW(Some(hwnd), w!("open"), &HSTRING::from(path.as_os_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn open_settings(hwnd: HWND) {
    let cfg = state().lock().unwrap().cfg.clone();
    settings_ui::open(hwnd, false, cfg, autostart_enabled());
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, ID_SAVE, w!("Save clip now"));
    let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("Settings..."));
    let _ = AppendMenuW(menu, MF_STRING, ID_FOLDER, w!("Open clips folder"));
    let _ = AppendMenuW(menu, MF_STRING, ID_LOG, w!("Open log"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    if let Some(v) = UPDATE_READY.lock().unwrap().clone() {
        let _ = AppendMenuW(menu, MF_STRING, ID_UPDATE, &HSTRING::from(format!("Restart to update to {v}")));
    }
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_SAVE => save_clip(),
        ID_SETTINGS => open_settings(hwnd),
        ID_FOLDER => {
            let dir = videos_dir().join("clip-a-pc");
            let _ = std::fs::create_dir_all(&dir);
            open_path(hwnd, &dir);
        }
        ID_LOG => open_path(hwnd, &log_path()),
        ID_UPDATE => apply_update(hwnd),
        ID_QUIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

fn apply_pending(hwnd: HWND) {
    let Some(Pending { cfg, autostart }) = PENDING.lock().unwrap().take() else { return };
    if let Err(e) = save_config(&cfg) {
        log(&format!("could not save settings: {e}"));
    }
    set_autostart(autostart);
    restart_engine(cfg);
    if FIRST_RUN.swap(false, Ordering::SeqCst) {
        toast::notify("clip-a-pc is running", "Press Alt+C any time to save a clip.");
    }
    let _ = hwnd;
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            save_clip();
            LRESULT(0)
        }
        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                x if x == WM_RBUTTONUP || x == WM_CONTEXTMENU => show_menu(hwnd),
                x if x == WM_LBUTTONDBLCLK => open_settings(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_SETTINGS_DONE => {
            apply_pending(hwnd);
            LRESULT(0)
        }
        WM_APP_APPLY_UPDATE => {
            apply_update(hwnd);
            LRESULT(0)
        }
        WM_APP_QUIT => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            tray_remove(hwnd);
            let engine = state().lock().unwrap().engine.take();
            if let Some(e) = engine {
                e.stop();
            }
            log("clip-a-pc closed");
            PostQuitMessage(0);
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            tray_add(hwnd); // Explorer restarted
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ------------------------------------------------------------------ entry

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        return; // used by the updater to check a download starts at all
    }
    let after_update = args.iter().any(|a| a == "--after-update");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // one copy at a time. After an update the old copy is still shutting down, so wait for it.
        let mut _mutex = CreateMutexW(None, true, w!("Local\\clip-a-pc-single-instance"));
        if after_update {
            for _ in 0..75 {
                if GetLastError() != ERROR_ALREADY_EXISTS {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
                _mutex = CreateMutexW(None, true, w!("Local\\clip-a-pc-single-instance"));
            }
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            MessageBoxW(None, w!("clip-a-pc is already running. Look for its icon near the clock."), w!("clip-a-pc"), MB_OK | MB_ICONINFORMATION);
            return;
        }
        trim();
        std::panic::set_hook(Box::new(|info| log(&format!("crash: {info}"))));
        log(&format!("clip-a-pc {VERSION} started{}", if after_update { " (after an update)" } else { "" }));
        toast::register();

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        ICON.store(make_icon().0 as isize, Ordering::Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        let class = w!("ClipAPCMain");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("clip-a-pc"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(hinst.into()), None) else {
            log("could not create the main window");
            return;
        };
        tray_add(hwnd);

        if RegisterHotKey(Some(hwnd), HOTKEY_ID, MOD_ALT | MOD_NOREPEAT, 'C' as u32).is_err() {
            log("could not register Alt+C: another program already uses it");
            toast::notify("Alt+C is already in use", "Another program has this shortcut, so clip-a-pc cannot use it.");
        }

        match load_config() {
            Some(cfg) => {
                state().lock().unwrap().cfg = cfg.clone();
                let e = start_engine(&cfg);
                state().lock().unwrap().engine = e;
            }
            None => {
                FIRST_RUN.store(true, Ordering::SeqCst);
                settings_ui::open(hwnd, true, Config::default(), false);
            }
        }

        spawn_updater(hwnd.0 as isize);

        // test helper: save one clip after N seconds, then quit
        if let Some(after) = arg_value(&args, "--autosave").and_then(|v| v.parse::<u64>().ok()) {
            let hw = hwnd.0 as isize;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(after));
                save_clip();
                std::thread::sleep(Duration::from_secs(4));
                let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_QUIT, WPARAM(0), LPARAM(0));
            });
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let sw = SETTINGS_HWND.load(Ordering::SeqCst);
            if sw != 0 && IsDialogMessageW(HWND(sw as *mut _), &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
