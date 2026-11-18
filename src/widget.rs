//! Window plumbing.
//!
//! - A hidden "controller" window owns the message loop, the tray icon and the context
//!   menu, and receives broadcasts (TaskbarCreated after an Explorer restart, theme
//!   changes, device changes).
//! - The tray icon (tray.rs) is the only always-visible surface. Left-click opens the
//!   popup (popup.rs) listing every device; right-click opens the menu.
//! - A worker thread scans devices and posts WM_APP_UPDATE to the controller.

use crate::device::{Reading, Status, Tracker};
use crate::{bluetooth, delux, diag, headsetcontrol, init_com, logitech, mchose, notify, playstation, popup, settings, strip, tray, wide, xbox};
use std::cell::RefCell;
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_MENU: u32 = WM_APP + 2;
const TIMER_ENSURE: usize = 1;
const CMD_REFRESH: usize = 1;
const CMD_AUTOSTART: usize = 2;
const CMD_EXIT: usize = 3;
const CMD_ALERTS: usize = 4;
const CMD_COPY_DIAGNOSTICS: usize = 5;
const CMD_MODE_TRAY: usize = 6;
const CMD_MODE_STRIP: usize = 7;
/// WM_POWERBROADCAST: resumed from sleep/hibernate.
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const CMD_DEVICE_BASE: usize = 100;

/// HID++ and the vendor protocols are cheap to poll. Bluetooth reads wake the device's
/// radio and HeadsetControl spawns a process, so poll those less often.
const POLL_FAST: Duration = Duration::from_secs(15);
const POLL_BLUETOOTH: Duration = Duration::from_secs(60);
/// Periodic icon re-add in case Explorer died without a TaskbarCreated broadcast.
const ENSURE: Duration = Duration::from_secs(60);

static READINGS: Mutex<Option<Vec<Reading>>> = Mutex::new(None);
static WAKE: OnceLock<Sender<()>> = OnceLock::new();

struct App {
    controller: HWND,
    popup: HWND,
    icon: HICON,
    state: tray::IconState,
    warnings: tray::Warnings,
    taskbar_created: u32,
    light: bool,
    mode: settings::Display,
}

thread_local! {
    static APP: RefCell<App> = const {
        RefCell::new(App {
            controller: null_mut(),
            popup: null_mut(),
            icon: null_mut(),
            state: tray::IconState::Normal,
            warnings: tray::Warnings::new(),
            taskbar_created: 0,
            light: false,
            mode: settings::Display::Tray,
        })
    };
}

/// Short borrows only; never hold one across a Win32 call that can dispatch messages.
fn with<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

/// Latest scan results, for the popup and the tooltip.
pub(crate) fn readings() -> Vec<Reading> {
    READINGS.lock().unwrap().clone().unwrap_or_default()
}

pub(crate) fn light_theme() -> bool {
    with(|a| a.light)
}

/// The controller window: popup/strip child surfaces post their menu messages here.
pub(crate) fn controller() -> HWND {
    with(|a| a.controller)
}

pub fn run() {
    unsafe {
        let mutex_name = wide(r"Local\taskbar-battery");
        CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return; // already running
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        settings::ensure_autostart();
        settings::register_app_id();
        diag::start();

        let hinst = GetModuleHandleW(null());
        let ctrl_class = wide("TaskbarBatteryController");
        register(hinst, &ctrl_class);
        strip::register(hinst);

        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let controller = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            ctrl_class.as_ptr(),
            ctrl_class.as_ptr(),
            WS_POPUP,
            0, 0, 0, 0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        let light = settings::light_theme();
        let mode = settings::display_mode();
        let icon = tray::make_icon(tray::IconState::Normal, tray::icon_size(), light);
        let popup_wnd = popup::create(hinst);
        with(|a| {
            a.controller = controller;
            a.popup = popup_wnd;
            a.icon = icon;
            a.taskbar_created = taskbar_created;
            a.light = light;
            a.mode = mode;
        });
        match mode {
            settings::Display::Tray => tray::add(controller, icon, &tray::tooltip(&[], &[])),
            settings::Display::Strip => strip::show(controller),
        }

        let (tx, rx) = mpsc::channel();
        let _ = WAKE.set(tx);
        let controller_addr = controller as usize;
        std::thread::spawn(move || worker(controller_addr, rx));

        SetTimer(controller, TIMER_ENSURE, ENSURE.as_millis() as u32, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn register(hinst: HINSTANCE, class: &[u16]) {
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(controller_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);
}

/// Debug builds: `--demo` shows fake readings covering every visual state
/// (1 = critical, 2 = warn band).
pub static DEMO: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn demo_readings() -> Vec<Reading> {
    use crate::device::Kind;
    let warn_only = DEMO.load(std::sync::atomic::Ordering::Relaxed) == 2;
    let r = |name: &str, kind, percent, charging, stale| Reading {
        id: name.into(),
        name: name.into(),
        kind,
        status: Status::Battery { percent, charging, stale },
    };
    vec![
        r("Headset", crate::device::Kind::Headset, 64, true, false),
        r("Earbuds", Kind::Earbuds, if warn_only { 15 } else { 8 }, false, false),
        r("Mouse", Kind::Mouse, if warn_only { 18 } else { 87 }, false, true),
        r("Keyboard", Kind::Keyboard, 45, false, false),
    ]
}

fn worker(controller: usize, rx: Receiver<()>) {
    if DEMO.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        *READINGS.lock().unwrap() = Some(demo_readings());
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        return;
    }
    init_com();
    let mut api = match hidapi::HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            diag::event("hid", format!("hidapi failed to start: {e}"));
            return;
        }
    };
    let mut logi = logitech::Logitech::new();
    let mut delux = delux::Delux::default();
    let mut tracker = Tracker::default();
    let mut alerts = notify::LowBatteryAlerts::default();
    let mut bt = Vec::new();
    let mut last_bt: Option<Instant> = None;
    let mut forced = true;

    loop {
        let _ = api.refresh_devices();
        let mut obs = logi.scan(&api);
        obs.extend(playstation::scan(&api));
        obs.extend(delux.scan(&api));
        obs.extend(xbox::scan());
        obs.extend(mchose::scan(&api));
        if forced || last_bt.is_none_or(|t| t.elapsed() >= POLL_BLUETOOTH) {
            bt = bluetooth::scan();
            if let Some(exe) = headsetcontrol::find() {
                bt.extend(headsetcontrol::scan(&exe, |pid| logitech::reports_pid(&obs, pid)));
            }
            last_bt = Some(Instant::now());
        }
        // Re-feed the last Bluetooth results between Bluetooth scans so they don't look gone.
        obs.extend(bt.iter().cloned());
        let readings = tracker.update(obs);
        alerts.check(&readings);
        *READINGS.lock().unwrap() = Some(readings);
        unsafe { PostMessageW(controller as HWND, WM_APP_UPDATE, 0, 0) };
        trim_working_set();

        forced = match rx.recv_timeout(POLL_FAST) {
            Ok(()) => {
                // Device changes come in bursts; let things settle, then scan once.
                std::thread::sleep(Duration::from_millis(1500));
                while rx.try_recv().is_ok() {}
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
    }
}

/// The app is idle between scans, so hand its pages back to Windows until the next one.
/// They move to the standby list and come back cheaply when touched.
fn trim_working_set() {
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

fn wake() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

/// New scan results (or a theme/visibility change): refresh icon, tooltip and popup —
/// or the taskbar strip when that's the active display mode.
unsafe fn update() {
    let rs = readings();
    let hidden = settings::hidden_devices();
    if with(|a| a.mode) == settings::Display::Strip {
        strip::redraw(controller());
        return;
    }
    with(|a| {
        let state = a.warnings.update(&rs, &hidden);
        let tip = tray::tooltip(&rs, &hidden);
        // Rebuild the icon only when what it shows changed (state, kind, theme).
        if state != a.state || a.icon.is_null() {
            let icon = tray::make_icon(state, tray::icon_size(), a.light);
            if !icon.is_null() {
                if !a.icon.is_null() {
                    DestroyIcon(a.icon);
                }
                a.icon = icon;
                a.state = state;
            }
        }
        tray::update(a.controller, a.icon, &tip);
        popup::refresh(a.popup);
    });
}

unsafe fn show_menu(hwnd: HWND) {
    let devices = readings();
    let hidden = settings::hidden_devices();
    let menu = CreatePopupMenu();
    let mut labels = Vec::new(); // keep the UTF-16 buffers alive while the menu exists

    for (i, r) in devices.iter().enumerate() {
        let label = wide(&format!("{}  —  {}", r.name, r.status));
        let check = if hidden.contains(&r.id) { MF_UNCHECKED } else { MF_CHECKED };
        AppendMenuW(menu, MF_STRING | check, CMD_DEVICE_BASE + i, label.as_ptr());
        labels.push(label);
    }
    if devices.is_empty() {
        let label = wide("No devices found");
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, label.as_ptr());
        labels.push(label);
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let (refresh, autostart, exit) = (wide("Refresh now"), wide("Start with Windows"), wide("Exit"));
    let alerts = wide("Low battery alerts");
    let copy_label = wide("Copy diagnostics");
    let (display, tray_label, strip_label) = (wide("Display"), wide("Tray icon"), wide("Device strip (taskbar)"));
    let checked = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    // Display submenu: the tray icon+popup or the per-device taskbar strip.
    let mode = with(|a| a.mode);
    let sub = CreatePopupMenu();
    AppendMenuW(sub, MF_STRING | checked(mode == settings::Display::Tray), CMD_MODE_TRAY, tray_label.as_ptr());
    AppendMenuW(sub, MF_STRING | checked(mode == settings::Display::Strip), CMD_MODE_STRIP, strip_label.as_ptr());
    AppendMenuW(menu, MF_POPUP, sub as usize, display.as_ptr());
    labels.extend([display, tray_label, strip_label]);
    AppendMenuW(menu, MF_STRING, CMD_REFRESH, refresh.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::alerts_enabled()), CMD_ALERTS, alerts.as_ptr());
    AppendMenuW(menu, MF_STRING | checked(settings::autostart_enabled()), CMD_AUTOSTART, autostart.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, CMD_COPY_DIAGNOSTICS, copy_label.as_ptr());
    AppendMenuW(menu, MF_STRING, CMD_EXIT, exit.as_ptr());

    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);

    match cmd as usize {
        0 => {}
        CMD_REFRESH => wake(),
        CMD_AUTOSTART => settings::set_autostart(!settings::autostart_enabled()),
        CMD_ALERTS => settings::set_alerts(!settings::alerts_enabled()),
        CMD_COPY_DIAGNOSTICS => copy_diagnostics(hwnd),
        CMD_MODE_TRAY => set_mode(hwnd, settings::Display::Tray),
        CMD_MODE_STRIP => set_mode(hwnd, settings::Display::Strip),
        CMD_EXIT => {
            strip::remove();
            tray::remove(hwnd);
            DestroyWindow(hwnd);
        }
        c if c >= CMD_DEVICE_BASE => {
            if let Some(r) = devices.get(c - CMD_DEVICE_BASE) {
                settings::toggle_hidden(&r.id);
                update();
            }
        }
        _ => {}
    }
}

/// Switches between the tray icon (+ popup) and the taskbar strip, live.
unsafe fn set_mode(hwnd: HWND, mode: settings::Display) {
    if with(|a| std::mem::replace(&mut a.mode, mode)) == mode {
        return;
    }
    settings::set_display_mode(mode);
    match mode {
        settings::Display::Strip => {
            tray::remove(hwnd);
            strip::show(hwnd);
        }
        settings::Display::Tray => {
            strip::remove();
            let tip = tray::tooltip(&readings(), &settings::hidden_devices());
            with(|a| tray::add(hwnd, a.icon, &tip));
        }
    }
    update();
}

/// Puts the diagnostics log on the clipboard and confirms with a toast.
unsafe fn copy_diagnostics(hwnd: HWND) {
    let (title, body) = match diag::report() {
        Some(text) if set_clipboard(hwnd, &text) => ("Diagnostics copied", "Paste them into your device report on GitHub."),
        Some(_) => ("Couldn't copy diagnostics", "Another app is using the clipboard. Try again."),
        None => ("No diagnostics yet", "Nothing has been logged so far."),
    };
    // Toasts are WinRT, and COM is only initialized on the worker thread.
    std::thread::spawn(move || {
        init_com();
        let _ = notify::show(title, body);
    });
}

unsafe fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let units: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
    if OpenClipboard(hwnd) == 0 {
        return false;
    }
    let mut ok = false;
    let mem = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
    if !mem.is_null() {
        let dst = GlobalLock(mem) as *mut u16;
        if !dst.is_null() {
            std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
            GlobalUnlock(mem);
            EmptyClipboard();
            ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
        }
        if !ok {
            GlobalFree(mem); // on success the clipboard owns it
        }
    }
    CloseClipboard();
    ok
}

unsafe extern "system" fn controller_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let taskbar_created = with(|a| a.taskbar_created);
    match msg {
        WM_APP_UPDATE => update(),
        WM_TIMER => {
            // Belt and braces: re-add the icon/strip if Explorer died without TaskbarCreated.
            if with(|a| a.mode) == settings::Display::Strip {
                strip::tick(hwnd);
            } else {
                with(|a| {
                    if a.icon.is_null() {
                        a.icon = tray::make_icon(tray::IconState::Normal, tray::icon_size(), a.light);
                    }
                    let rs = readings();
                    let hidden = settings::hidden_devices();
                    tray::add(hwnd, a.icon, &tray::tooltip(&rs, &hidden));
                });
            }
        }
        WM_SETTINGCHANGE => {
            let changed = with(|a| std::mem::replace(&mut a.light, settings::light_theme()) != a.light);
            if changed {
                // Theme flipped: the icon colors invert too, force a rebuild.
                with(|a| {
                    if !a.icon.is_null() {
                        DestroyIcon(a.icon);
                        a.icon = null_mut();
                    }
                });
                update();
            }
        }
        WM_DEVICECHANGE => {
            wake();
            return 1;
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC {
                wake(); // PC woke from sleep: devices may have changed meanwhile
            }
            return 1;
        }
        WM_APP_MENU => show_menu(hwnd),
        tray::WM_APP_TRAY => {
            let popup_wnd = with(|a| a.popup);
            match lp as u32 & 0xFFFF {
                tray::NIN_SELECT | tray::NIN_KEYSELECT => popup::toggle(popup_wnd),
                // Right-click: v4 delivers WM_CONTEXTMENU, older versions WM_RBUTTONUP.
                WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
        }
        WM_DESTROY => {
            strip::remove();
            tray::remove(hwnd);
            PostQuitMessage(0);
        }
        m if m == taskbar_created && m != 0 => {
            // Explorer restarted: the icon/strip died with the old taskbar.
            if with(|a| a.mode) == settings::Display::Strip {
                strip::forget();
                strip::show(hwnd);
            } else {
                with(|a| {
                    if a.icon.is_null() {
                        a.icon = tray::make_icon(tray::IconState::Normal, tray::icon_size(), a.light);
                    }
                    let rs = readings();
                    let hidden = settings::hidden_devices();
                    tray::add(hwnd, a.icon, &tray::tooltip(&rs, &hidden));
                });
            }
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}
