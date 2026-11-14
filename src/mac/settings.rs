//! The settings window: display, clip length, quality, microphone volume, login item, updates.
//! Plain AppKit controls, created when the window opens and dropped when it closes.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSPopUpButton, NSTextField, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::capture::MonitorInfo;
use crate::config::{estimate_memory_mb, Config, Quality, DURATIONS_SECS, MIC_GAINS};

struct Ui {
    window: Retained<NSWindow>,
    monitor: Retained<NSPopUpButton>,
    length: Retained<NSPopUpButton>,
    quality: Retained<NSPopUpButton>,
    mic: Retained<NSPopUpButton>,
    estimate: Retained<NSTextField>,
    login: Retained<NSButton>,
    updates: Retained<NSButton>,
    monitors: Vec<MonitorInfo>,
    first_run: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect { origin: NSPoint { x, y }, size: NSSize { width: w, height: h } }
}

pub fn is_open() -> bool {
    UI.with(|u| u.borrow().is_some())
}

pub fn bring_to_front() {
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref() {
            unsafe { ui.window.makeKeyAndOrderFront(None) };
        }
    });
}

/// Open the window. `target` receives the button and menu actions.
pub fn open(mtm: MainThreadMarker, target: &AnyObject, cfg: &Config, login: bool, monitors: Vec<MonitorInfo>, first_run: bool) {
    if is_open() {
        bring_to_front();
        return;
    }
    let (w, h) = (440.0, 330.0);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(if first_run { "Welcome to clip-a-pc" } else { "clip-a-pc settings" }));
    let content = window.contentView().expect("window has a content view");

    let label = |text: &str, x: f64, y: f64, w: f64| {
        let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        l.setFrame(rect(x, y, w, 20.0));
        content.addSubview(&l);
        l
    };
    let popup = |y: f64, items: Vec<String>, selected: usize, action: Option<objc2::runtime::Sel>| {
        let p = unsafe { NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), rect(170.0, y, 250.0, 26.0), false) };
        for it in &items {
            unsafe { p.addItemWithTitle(&NSString::from_str(it)) };
        }
        unsafe { p.selectItemAtIndex(selected as isize) };
        if let Some(a) = action {
            unsafe {
                p.setTarget(Some(target));
                p.setAction(Some(a));
            }
        }
        content.addSubview(&p);
        p
    };

    let intro = if first_run { "Pick what to record. You can change this later from the menu bar." } else { "Changes apply when you press Save." };
    label(intro, 20.0, h - 38.0, w - 40.0);

    label("Display", 20.0, h - 82.0, 140.0);
    let mon_items: Vec<String> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| format!("Display {}: {} x {}{}", i + 1, m.width, m.height, if m.primary { " (main)" } else { "" }))
        .collect();
    let mon_sel = monitors.iter().position(|m| m.id.to_string() == cfg.monitor).unwrap_or(0);
    let monitor = popup(h - 86.0, if mon_items.is_empty() { vec!["Main display".into()] } else { mon_items }, mon_sel, None);

    label("Clip length", 20.0, h - 120.0, 140.0);
    let length = popup(
        h - 124.0,
        DURATIONS_SECS.iter().map(|d| format!("{d} seconds")).collect(),
        DURATIONS_SECS.iter().position(|&d| d == cfg.duration_secs).unwrap_or(1),
        Some(sel!(estimate:)),
    );

    label("Quality", 20.0, h - 158.0, 140.0);
    let quality = popup(
        h - 162.0,
        Quality::ALL.iter().map(|q| q.label().to_string()).collect(),
        Quality::ALL.iter().position(|&q| q == cfg.quality).unwrap_or(0),
        Some(sel!(estimate:)),
    );

    label("Microphone volume", 20.0, h - 196.0, 140.0);
    let mic_sel = MIC_GAINS
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - cfg.mic_gain).abs().total_cmp(&(b.1 - cfg.mic_gain).abs()))
        .map(|(i, _)| i)
        .unwrap_or(1);
    let mic = popup(h - 200.0, MIC_GAINS.iter().map(|g| format!("{}%", (g * 100.0).round() as i32)).collect(), mic_sel, None);

    let estimate = label("", 20.0, h - 232.0, w - 40.0);

    let check = |text: &str, y: f64, on: bool| {
        let b = unsafe { NSButton::checkboxWithTitle_target_action(&NSString::from_str(text), None, None, mtm) };
        b.setFrame(rect(20.0, y, w - 40.0, 22.0));
        b.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
        content.addSubview(&b);
        b
    };
    let login_box = check("Start when I log in", h - 266.0, login);
    let updates_box = check("Update automatically", h - 292.0, cfg.auto_update);

    let button = |text: &str, x: f64, action: objc2::runtime::Sel, default: bool| {
        let b = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(text), Some(target), Some(action), mtm) };
        b.setFrame(rect(x, 16.0, 90.0, 30.0));
        if default {
            b.setKeyEquivalent(&NSString::from_str("\r"));
        } else {
            b.setKeyEquivalent(&NSString::from_str("\u{1b}"));
        }
        content.addSubview(&b);
    };
    button("Save", w - 110.0, sel!(settingsSave:), true);
    button("Cancel", w - 210.0, sel!(settingsCancel:), false);

    UI.with(|u| {
        *u.borrow_mut() = Some(Ui { window: window.clone(), monitor, length, quality, mic, estimate, login: login_box, updates: updates_box, monitors, first_run });
    });
    refresh_estimate();
    window.center();
    unsafe { window.makeKeyAndOrderFront(None) };
}

/// Show the rough memory cost of the current choices.
pub fn refresh_estimate() {
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref() {
            let (secs, q) = chosen(ui);
            ui.estimate.setStringValue(&NSString::from_str(&format!("Estimated memory use: about {} MB", estimate_memory_mb(secs, q))));
        }
    });
}

fn chosen(ui: &Ui) -> (u32, Quality) {
    let i = |p: &NSPopUpButton| unsafe { p.indexOfSelectedItem() }.max(0) as usize;
    (DURATIONS_SECS[i(&ui.length).min(DURATIONS_SECS.len() - 1)], Quality::ALL[i(&ui.quality).min(Quality::ALL.len() - 1)])
}

/// What the user chose, as (settings, start at login, was this the first-run window).
pub fn read() -> Option<(Config, bool, bool)> {
    UI.with(|u| {
        let b = u.borrow();
        let ui = b.as_ref()?;
        let i = |p: &NSPopUpButton| unsafe { p.indexOfSelectedItem() }.max(0) as usize;
        let (secs, quality) = chosen(ui);
        let cfg = Config {
            duration_secs: secs,
            monitor: ui.monitors.get(i(&ui.monitor)).map(|m| m.id.to_string()).unwrap_or_default(),
            mic_gain: MIC_GAINS[i(&ui.mic).min(MIC_GAINS.len() - 1)],
            quality,
            auto_update: ui.updates.state() == NSControlStateValueOn,
        };
        Some((cfg, ui.login.state() == NSControlStateValueOn, ui.first_run))
    })
}

pub fn close() {
    UI.with(|u| {
        if let Some(ui) = u.borrow_mut().take() {
            ui.window.close();
        }
    });
}
