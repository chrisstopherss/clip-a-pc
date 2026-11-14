//! The menu-bar app: a status item, three global hotkeys, the settings window,
//! and the recorder running in the background.
//!
//! Hotkeys are Control+Option+C, X and V. Plain Option shortcuts are blocked
//! by macOS 15, so Control is added.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSImage, NSMenu, NSMenuItem, NSSound, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, NSTimer};

use super::capture::list_monitors;
use super::engine::{Engine, EngineCfg, Finished};
use super::settings;
use super::store::{autostart_enabled, load_config, save_config, set_autostart};
use super::updater::{self, Outcome, VERSION};
use super::util::{log, log_path, memory_mb, notify, now_100ns, open, trim_log, videos_dir};
use crate::config::{Config, DURATIONS_SECS};

struct State {
    engine: Option<Arc<Engine>>,
    cfg: Config,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static SAVING: AtomicBool = AtomicBool::new(false);
static UPDATE_READY: Mutex<Option<String>> = Mutex::new(None);

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State { engine: None, cfg: Config::default() }))
}

/// Everything that must stay alive on the main thread.
struct Ui {
    item: Retained<NSStatusItem>,
    controller: Retained<Controller>,
    _hotkeys: Option<GlobalHotKeyManager>,
    timer: Option<Retained<NSTimer>>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

// ------------------------------------------------------------------ the object menus and buttons talk to

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ClipAPCController"]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    impl Controller {
        #[unsafe(method(saveClip:))]
        fn save_clip_action(&self, _sender: Option<&AnyObject>) {
            save_clip(None);
        }

        #[unsafe(method(saveLength:))]
        fn save_length_action(&self, sender: Option<&AnyObject>) {
            if let Some(s) = sender {
                let tag: isize = unsafe { msg_send![s, tag] };
                if tag > 0 {
                    save_clip(Some(tag as u32));
                }
            }
        }

        #[unsafe(method(toggleRecording:))]
        fn toggle_action(&self, _sender: Option<&AnyObject>) {
            toggle_recording();
        }

        #[unsafe(method(openSettings:))]
        fn settings_action(&self, _sender: Option<&AnyObject>) {
            open_settings(false);
        }

        #[unsafe(method(openFolder:))]
        fn folder_action(&self, _sender: Option<&AnyObject>) {
            let dir = videos_dir().join("clip-a-pc");
            let _ = std::fs::create_dir_all(&dir);
            open(&dir.to_string_lossy());
        }

        #[unsafe(method(openLog:))]
        fn log_action(&self, _sender: Option<&AnyObject>) {
            open(&log_path().to_string_lossy());
        }

        #[unsafe(method(applyUpdate:))]
        fn update_action(&self, _sender: Option<&AnyObject>) {
            apply_update();
        }

        #[unsafe(method(quit:))]
        fn quit_action(&self, _sender: Option<&AnyObject>) {
            quit();
        }

        #[unsafe(method(estimate:))]
        fn estimate_action(&self, _sender: Option<&AnyObject>) {
            settings::refresh_estimate();
        }

        #[unsafe(method(settingsSave:))]
        fn settings_save_action(&self, _sender: Option<&AnyObject>) {
            if let Some((cfg, login, first)) = settings::read() {
                settings::close();
                apply_settings(cfg, login, first);
            }
        }

        #[unsafe(method(settingsCancel:))]
        fn settings_cancel_action(&self, _sender: Option<&AnyObject>) {
            let first = settings::read().map(|(_, _, f)| f).unwrap_or(false);
            settings::close();
            if first {
                quit();
            }
        }

        #[unsafe(method(tick:))]
        fn tick_action(&self, _sender: Option<&AnyObject>) {
            on_tick();
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// ------------------------------------------------------------------ helpers

fn with_ui<T>(f: impl FnOnce(&mut Ui) -> T) -> Option<T> {
    UI.with(|u| u.borrow_mut().as_mut().map(f))
}

fn controller() -> Option<Retained<Controller>> {
    with_ui(|ui| ui.controller.clone())
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("must run on the main thread")
}

/// Run on the main thread, whichever thread asked.
fn on_main(f: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(f);
}

fn play(name: &str) {
    if let Some(s) = NSSound::soundNamed(&NSString::from_str(name)) {
        s.play();
    }
}

fn clock(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn engine() -> Option<Arc<Engine>> {
    state().lock().unwrap().engine.clone()
}

// ------------------------------------------------------------------ engine

fn start_engine(cfg: &Config) -> Option<Arc<Engine>> {
    match Engine::start(EngineCfg { secs: cfg.duration_secs, display_id: cfg.monitor.parse().ok(), mic_gain: cfg.mic_gain, quality: cfg.quality }) {
        Ok(e) => {
            let e = Arc::new(e);
            log(&format!("recording started: {} s clips at {}, mic {}x", cfg.duration_secs, cfg.quality.label(), cfg.mic_gain));
            let watcher = e.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                log(&format!("video: {}", watcher.shared.video_info.lock().unwrap()));
                log(&format!("audio: {}", watcher.shared.audio_info.lock().unwrap()));
                loop {
                    for _ in 0..300 {
                        if watcher.is_stopped() {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    let (span, used) = watcher.buffer_status();
                    log(&format!("status: buffered {span:.0} s ({:.0} MB), memory {:.0} MB", used as f64 / 1048576.0, memory_mb()));
                }
            });
            Some(e)
        }
        Err(e) => {
            log(&format!("could not start recording: {e}"));
            notify("clip-a-pc could not start recording", &e.to_string());
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
        report_finished(e.take_finished());
    }
    let new = start_engine(&cfg);
    state().lock().unwrap().engine = new;
}

fn save_clip(secs: Option<u32>) {
    if SAVING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(engine) = engine() else {
        SAVING.store(false, Ordering::SeqCst);
        notify("Nothing to save", "Recording is not running.");
        return;
    };
    std::thread::spawn(move || {
        let t = Instant::now();
        let result = match secs {
            Some(n) => engine.save_secs(n),
            None => engine.save(),
        };
        match result {
            Ok(s) => {
                on_main(|| play("Glass"));
                let name = s.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                notify("Clip saved", &format!("{:.0} s, {:.1} MB  ({name})", s.seconds, s.bytes as f64 / 1048576.0));
                log(&format!("clip saved: {} ({:.1} s, {:.1} MB) in {} ms, memory {:.0} MB", s.path.display(), s.seconds, s.bytes as f64 / 1048576.0, t.elapsed().as_millis(), memory_mb()));
            }
            Err(e) => {
                notify("Could not save the clip", &e.to_string());
                log(&format!("save failed: {e}"));
            }
        }
        SAVING.store(false, Ordering::SeqCst);
    });
}

// ------------------------------------------------------------------ manual recording

fn set_title(text: &str) {
    with_ui(|ui| {
        if let Some(b) = ui.item.button(mtm()) {
            b.setTitle(&NSString::from_str(text));
        }
    });
}

fn start_timer() {
    let Some(ctrl) = controller() else { return };
    let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(1.0, &ctrl, sel!(tick:), None, true) };
    with_ui(|ui| ui.timer = Some(timer));
}

fn stop_timer() {
    with_ui(|ui| {
        if let Some(t) = ui.timer.take() {
            unsafe { t.invalidate() };
        }
    });
}

/// Tell the user a recording ended and put the menu bar back to normal.
fn report_finished(f: Option<Result<Finished, String>>) {
    stop_timer();
    set_title("");
    rebuild_menu();
    let Some(f) = f else { return };
    match f {
        Ok(f) => {
            play("Pop");
            let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let why = if f.note.is_empty() { String::new() } else { format!(" Stopped because {}.", f.note) };
            notify("Recording saved", &format!("{}, {:.0} MB  ({name}).{why}", clock(f.seconds), f.bytes as f64 / 1048576.0));
            log(&format!("recording saved: {} ({}, {:.1} MB) {}", f.path.display(), clock(f.seconds), f.bytes as f64 / 1048576.0, f.note));
        }
        Err(e) => {
            notify("The recording could not be saved", &e);
            log(&format!("recording failed: {e}"));
        }
    }
}

fn toggle_recording() {
    let Some(engine) = engine() else {
        notify("Nothing to record", "Recording is not running.");
        return;
    };
    if engine.recording_elapsed().is_some() {
        let r = engine.stop_recording().map_err(|e| e.to_string());
        report_finished(Some(r));
    } else {
        match engine.start_recording() {
            Ok(path) => {
                play("Hero");
                notify("Recording started", "Press Control+Option+V again to stop and save.");
                log(&format!("recording started: {}", path.display()));
                set_title(" REC 0:00");
                rebuild_menu();
                start_timer();
            }
            Err(e) => {
                notify("Could not start recording", &e.to_string());
                log(&format!("could not start recording: {e}"));
            }
        }
    }
}

/// Once a second while recording: keep the menu bar timer current and notice a recording that ended by itself.
fn on_tick() {
    let Some(engine) = engine() else {
        report_finished(None);
        return;
    };
    match engine.recording_elapsed() {
        Some(d) => set_title(&format!(" REC {}", clock(d.as_secs_f64()))),
        None => report_finished(engine.take_finished()),
    }
}

// ------------------------------------------------------------------ menus

fn add_item(menu: &NSMenu, ctrl: &Controller, title: &str, action: objc2::runtime::Sel) {
    let item = NSMenuItem::new(mtm());
    item.setTitle(&NSString::from_str(title));
    unsafe {
        item.setAction(Some(action));
        item.setTarget(Some(ctrl));
    }
    menu.addItem(&item);
}

fn rebuild_menu() {
    let recording = engine().map(|e| e.recording_elapsed().is_some()).unwrap_or(false);
    let update = UPDATE_READY.lock().unwrap().clone();
    with_ui(|ui| {
        let m = mtm();
        let menu = NSMenu::new(m);
        let c = &ui.controller;
        add_item(&menu, c, "Save clip now", sel!(saveClip:));
        add_item(&menu, c, if recording { "Stop recording" } else { "Start recording" }, sel!(toggleRecording:));
        add_item(&menu, c, "Settings...", sel!(openSettings:));
        add_item(&menu, c, "Open clips folder", sel!(openFolder:));
        add_item(&menu, c, "Open log", sel!(openLog:));
        menu.addItem(&NSMenuItem::separatorItem(m));
        if let Some(v) = update {
            add_item(&menu, c, &format!("Restart to update to {v}"), sel!(applyUpdate:));
        }
        add_item(&menu, c, "Quit clip-a-pc", sel!(quit:));
        ui.item.setMenu(Some(&menu));
    });
}

/// The shortcut pop-up: a small menu at the pointer asking how many seconds to save.
fn show_length_menu() {
    let configured = state().lock().unwrap().cfg.duration_secs;
    let Some(ctrl) = controller() else { return };
    let m = mtm();
    let menu = NSMenu::new(m);
    let head = NSMenuItem::new(m);
    head.setTitle(&NSString::from_str("Save the last..."));
    head.setEnabled(false);
    menu.addItem(&head);
    for d in DURATIONS_SECS.iter().filter(|&&d| d <= configured) {
        let item = NSMenuItem::new(m);
        item.setTitle(&NSString::from_str(&format!("{d} seconds")));
        item.setTag(*d as isize);
        unsafe {
            item.setAction(Some(sel!(saveLength:)));
            item.setTarget(Some(&ctrl));
        }
        menu.addItem(&item);
    }
    if DURATIONS_SECS.iter().any(|&d| d > configured) {
        menu.addItem(&NSMenuItem::separatorItem(m));
        let hint = NSMenuItem::new(m);
        hint.setTitle(&NSString::from_str("Longer needs a longer Clip length in Settings"));
        hint.setEnabled(false);
        menu.addItem(&hint);
    }
    NSApplication::sharedApplication(m).activate();
    menu.popUpMenuPositioningItem_atLocation_inView(None, NSEvent::mouseLocation(), None);
}

// ------------------------------------------------------------------ settings

fn open_settings(first_run: bool) {
    let Some(ctrl) = controller() else { return };
    let cfg = state().lock().unwrap().cfg.clone();
    let monitors = list_monitors().unwrap_or_default();
    NSApplication::sharedApplication(mtm()).activate();
    settings::open(mtm(), &ctrl, &cfg, autostart_enabled(), monitors, first_run);
}

fn apply_settings(cfg: Config, login: bool, first_run: bool) {
    if let Err(e) = save_config(&cfg) {
        log(&format!("could not save settings: {e}"));
    }
    set_autostart(login);
    restart_engine(cfg);
    rebuild_menu();
    if first_run {
        notify("clip-a-pc is running", "Press Control+Option+C any time to save a clip.");
    }
}

// ------------------------------------------------------------------ updates

fn spawn_updater() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        let mut first = true;
        loop {
            let enabled = state().lock().unwrap().cfg.auto_update;
            let waiting = UPDATE_READY.lock().unwrap().is_some();
            if enabled && !waiting {
                match updater::check_and_download() {
                    Outcome::Ready(v) => {
                        log(&format!("update {v} downloaded and verified"));
                        *UPDATE_READY.lock().unwrap() = Some(v.clone());
                        on_main(rebuild_menu);
                        let busy = SAVING.load(Ordering::SeqCst) || engine().map(|e| e.recording_elapsed().is_some()).unwrap_or(false);
                        if first && !busy {
                            on_main(apply_update);
                        } else {
                            notify("clip-a-pc update ready", &format!("Version {v} is ready. Choose Restart to update in the menu bar menu."));
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

fn apply_update() {
    // finish any recording first, then swap the app and relaunch
    let engine = state().lock().unwrap().engine.take();
    if let Some(e) = engine {
        e.stop();
    }
    match updater::apply_and_restart() {
        Ok(()) => {
            log("restarting into the new version");
            NSApplication::sharedApplication(mtm()).terminate(None);
        }
        Err(e) => {
            log(&format!("update could not be installed: {e}"));
            notify("Update could not be installed", &e);
            let cfg = state().lock().unwrap().cfg.clone();
            let new = start_engine(&cfg);
            state().lock().unwrap().engine = new;
        }
    }
}

fn quit() {
    let engine = state().lock().unwrap().engine.take();
    if let Some(e) = engine {
        e.stop(); // finishes a recording in progress
    }
    log("clip-a-pc closed");
    NSApplication::sharedApplication(mtm()).terminate(None);
}

// ------------------------------------------------------------------ entry

fn register_hotkeys() -> Option<GlobalHotKeyManager> {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            log(&format!("hotkeys unavailable: {e}"));
            return None;
        }
    };
    let mods = Some(Modifiers::CONTROL | Modifiers::ALT);
    let keys = [(Code::KeyC, "Control+Option+C"), (Code::KeyX, "Control+Option+X"), (Code::KeyV, "Control+Option+V")];
    let mut ids = Vec::new();
    let mut taken = Vec::new();
    for (code, name) in keys {
        let hk = HotKey::new(mods, code);
        match manager.register(hk) {
            Ok(()) => ids.push((hk.id(), code)),
            Err(e) => {
                log(&format!("could not register {name}: {e}"));
                taken.push(name);
            }
        }
    }
    if !taken.is_empty() {
        notify("A shortcut is already in use", &format!("{} belongs to another program, so clip-a-pc cannot use it.", taken.join(", ")));
    }
    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
        if e.state != HotKeyState::Pressed {
            return;
        }
        let Some((_, code)) = ids.iter().find(|(id, _)| *id == e.id) else { return };
        let code = *code;
        on_main(move || match code {
            Code::KeyC => save_clip(None),
            Code::KeyX => show_length_menu(),
            Code::KeyV => toggle_recording(),
            _ => {}
        });
    }));
    Some(manager)
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("clip-a-pc {VERSION}");
        return; // used by the updater to check a download starts at all
    }
    let m = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(m);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    trim_log();
    std::panic::set_hook(Box::new(|info| log(&format!("crash: {info}"))));
    log(&format!("clip-a-pc {VERSION} started"));

    let controller = Controller::new(m);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(m) {
        let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("record.circle"), Some(&NSString::from_str("clip-a-pc")));
        if let Some(icon) = icon {
            button.setImage(Some(&icon));
        } else {
            button.setTitle(&NSString::from_str("clip"));
        }
    }
    let hotkeys = register_hotkeys();
    UI.with(|u| *u.borrow_mut() = Some(Ui { item, controller, _hotkeys: hotkeys, timer: None }));
    rebuild_menu();

    match load_config() {
        Some(cfg) => {
            state().lock().unwrap().cfg = cfg.clone();
            let e = start_engine(&cfg);
            state().lock().unwrap().engine = e;
            rebuild_menu();
        }
        None => open_settings(true),
    }
    spawn_updater();
    let _ = now_100ns();
    app.run();
}
