//! The settings window: monitor, clip length, microphone volume, start with Windows.
//! Plain Windows controls, created only while the window is open.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::video::{list_monitors, MonitorInfo};
use crate::config::{Config, DURATIONS_SECS, MIC_GAINS};

pub const WM_APP_SETTINGS_DONE: u32 = WM_APP + 2;
pub const WM_APP_QUIT: u32 = WM_APP + 3;

pub struct Pending {
    pub cfg: Config,
    pub autostart: bool,
}

/// Filled when the user presses OK; the main window picks it up.
pub static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// Handle of the open settings window, or 0. Used for tab-key handling and to avoid duplicates.
pub static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;
const ID_MONITOR: i32 = 101;
const ID_LENGTH: i32 = 102;
const ID_MIC: i32 = 103;
const ID_AUTOSTART: i32 = 104;

struct Ui {
    main: isize,
    first_run: bool,
    monitors: Vec<MonitorInfo>,
    initial: Config,
    initial_autostart: bool,
    monitor: HWND,
    length: HWND,
    mic: HWND,
    autostart: HWND,
    font: HFONT,
}

pub fn open(main: HWND, first_run: bool, current: Config, autostart: bool) {
    let existing = SETTINGS_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        unsafe {
            let _ = SetForegroundWindow(HWND(existing as *mut _));
        }
        return;
    }
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("ClipAPCSettings");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as isize as *mut _),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // fails harmlessly if already registered

        let ui = Box::new(Ui {
            main: main.0 as isize,
            first_run,
            monitors: list_monitors(),
            initial: current,
            initial_autostart: autostart,
            monitor: HWND::default(),
            length: HWND::default(),
            mic: HWND::default(),
            autostart: HWND::default(),
            font: HFONT::default(),
        });
        let title = if first_run { "Welcome to clip-a-pc" } else { "clip-a-pc settings" };
        let ptr = Box::into_raw(ui);
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class,
            &HSTRING::from(title),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            400,
            300,
            None,
            None,
            Some(hinst.into()),
            Some(ptr as *const _),
        );
        match hwnd {
            Ok(h) => {
                SETTINGS_HWND.store(h.0 as isize, Ordering::SeqCst);
                let _ = ShowWindow(h, SW_SHOW);
                let _ = SetForegroundWindow(h);
            }
            Err(e) => {
                super::log::log(&format!("could not open settings window: {e}"));
                drop(Box::from_raw(ptr));
            }
        }
    }
}

fn scaled(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

unsafe fn child(parent: HWND, class: PCWSTR, text: &str, style: u32, rect: (i32, i32, i32, i32), id: i32, dpi: u32, font: HFONT) -> HWND {
    let (x, y, w, h) = rect;
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        &HSTRING::from(text),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
        scaled(x, dpi),
        scaled(y, dpi),
        scaled(w, dpi),
        scaled(h, dpi),
        Some(parent),
        Some(HMENU(id as isize as *mut _)),
        None,
        None,
    )
    .unwrap_or_default();
    SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    hwnd
}

unsafe fn add_item(combo: HWND, text: &str) {
    let h = HSTRING::from(text);
    SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(h.as_ptr() as isize)));
}

unsafe fn build(hwnd: HWND, ui: &mut Ui) {
    let dpi = GetDpiForWindow(hwnd).max(96);

    // dialog font, sized for this monitor's DPI
    let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    let _ = SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as *mut _), 0, dpi);
    ui.font = CreateFontIndirectW(&ncm.lfMessageFont);

    let intro = if ui.first_run {
        "Pick the monitor to record and how long a clip should be. You can change this later from the tray icon."
    } else {
        "Changes apply as soon as you press Save."
    };
    child(hwnd, w!("STATIC"), intro, 0, (16, 14, 368, 36), 0, dpi, ui.font);

    child(hwnd, w!("STATIC"), "Monitor", 0, (16, 66, 130, 20), 0, dpi, ui.font);
    ui.monitor = child(hwnd, w!("COMBOBOX"), "", (CBS_DROPDOWNLIST as u32) | WS_TABSTOP.0 | WS_VSCROLL.0, (150, 62, 234, 200), ID_MONITOR, dpi, ui.font);
    child(hwnd, w!("STATIC"), "Clip length", 0, (16, 102, 130, 20), 0, dpi, ui.font);
    ui.length = child(hwnd, w!("COMBOBOX"), "", (CBS_DROPDOWNLIST as u32) | WS_TABSTOP.0 | WS_VSCROLL.0, (150, 98, 234, 200), ID_LENGTH, dpi, ui.font);
    child(hwnd, w!("STATIC"), "Microphone volume", 0, (16, 138, 130, 20), 0, dpi, ui.font);
    ui.mic = child(hwnd, w!("COMBOBOX"), "", (CBS_DROPDOWNLIST as u32) | WS_TABSTOP.0 | WS_VSCROLL.0, (150, 134, 234, 200), ID_MIC, dpi, ui.font);
    ui.autostart = child(hwnd, w!("BUTTON"), "Start with Windows", (BS_AUTOCHECKBOX as u32) | WS_TABSTOP.0, (16, 176, 368, 22), ID_AUTOSTART, dpi, ui.font);
    child(hwnd, w!("BUTTON"), "Save", (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP.0, (210, 220, 84, 28), ID_OK, dpi, ui.font);
    child(hwnd, w!("BUTTON"), "Cancel", (BS_PUSHBUTTON as u32) | WS_TABSTOP.0, (300, 220, 84, 28), ID_CANCEL, dpi, ui.font);

    for m in &ui.monitors {
        let n = list_index_label(m);
        add_item(ui.monitor, &n);
    }
    let sel = ui.monitors.iter().position(|m| m.device_name == ui.initial.monitor).unwrap_or(0);
    SendMessageW(ui.monitor, CB_SETCURSEL, Some(WPARAM(sel)), None);

    for d in DURATIONS_SECS {
        add_item(ui.length, &format!("{d} seconds"));
    }
    let sel = DURATIONS_SECS.iter().position(|&d| d == ui.initial.duration_secs).unwrap_or(1);
    SendMessageW(ui.length, CB_SETCURSEL, Some(WPARAM(sel)), None);

    for g in MIC_GAINS {
        add_item(ui.mic, &format!("{}%", (g * 100.0).round() as i32));
    }
    let sel = MIC_GAINS
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - ui.initial.mic_gain).abs().total_cmp(&(b.1 - ui.initial.mic_gain).abs()))
        .map(|(i, _)| i)
        .unwrap_or(1);
    SendMessageW(ui.mic, CB_SETCURSEL, Some(WPARAM(sel)), None);
    if ui.initial_autostart {
        SendMessageW(ui.autostart, BM_SETCHECK, Some(WPARAM(1)), None);
    }

    // size the window around a 400 x 262 client area and centre it on screen
    let mut r = RECT { left: 0, top: 0, right: scaled(400, dpi), bottom: scaled(262, dpi) };
    let _ = AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU, false, WS_EX_APPWINDOW, dpi);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let (sw, sh) = (GetSystemMetricsForDpi(SM_CXSCREEN, dpi), GetSystemMetricsForDpi(SM_CYSCREEN, dpi));
    let _ = SetWindowPos(hwnd, None, (sw - w) / 2, (sh - h) / 2, w, h, SWP_NOZORDER);
}

fn list_index_label(m: &MonitorInfo) -> String {
    format!("{} x {}{}  ({})", m.width, m.height, if m.primary { "  primary" } else { "" }, m.device_name.trim_start_matches("\\\\.\\"))
}

unsafe fn finish(hwnd: HWND, ui: &Ui, save: bool) {
    if save {
        let sel = |c: HWND| SendMessageW(c, CB_GETCURSEL, None, None).0.max(0) as usize;
        let cfg = Config {
            duration_secs: DURATIONS_SECS[sel(ui.length).min(DURATIONS_SECS.len() - 1)],
            monitor: ui.monitors.get(sel(ui.monitor)).map(|m| m.device_name.clone()).unwrap_or_default(),
            mic_gain: MIC_GAINS[sel(ui.mic).min(MIC_GAINS.len() - 1)],
        };
        let autostart = SendMessageW(ui.autostart, BM_GETCHECK, None, None).0 == 1;
        *PENDING.lock().unwrap() = Some(Pending { cfg, autostart });
        let _ = PostMessageW(Some(HWND(ui.main as *mut _)), WM_APP_SETTINGS_DONE, WPARAM(0), LPARAM(0));
    } else if ui.first_run {
        let _ = PostMessageW(Some(HWND(ui.main as *mut _)), WM_APP_QUIT, WPARAM(0), LPARAM(0));
    }
    let _ = DestroyWindow(hwnd);
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let ui = &mut *ptr;
    match msg {
        WM_CREATE => {
            build(hwnd, ui);
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wp.0 & 0xFFFF) as i32 {
                ID_OK => finish(hwnd, ui, true),
                ID_CANCEL => finish(hwnd, ui, false),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            finish(hwnd, ui, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            SETTINGS_HWND.store(0, Ordering::SeqCst);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DeleteObject(ui.font.into());
            drop(Box::from_raw(ptr));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
