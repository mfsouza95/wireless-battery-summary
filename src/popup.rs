//! The popup: a small borderless tool window listing every device — icon, name,
//! percentage and a level bar — anchored next to the tray icon.
//!
//! Opens on left-click (NIN_SELECT) and closes when it loses activation, like the
//! system volume/battery flyouts. Content is repainted from the latest readings on
//! WM_PAINT; `refresh` is called after every scan so an open popup stays live and
//! re-anchors if the row count changed.

use crate::device::{Reading, Status};
use crate::{render, settings, tray, wide};
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WIDTH: i32 = 264;
const ROW_H: i32 = 36;
const PAD: i32 = 10;

// Open animation: ~120 ms slide in from the tray icon's direction.
const ANIM_TIMER: usize = 1;
const ANIM_MS: u32 = 15;
const ANIM_STEPS: u32 = 8;
const ANIM_SLIDE: i32 = 14;

#[derive(Clone, Copy, Default)]
struct Anim {
    active: bool,
    tick: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

thread_local! {
    static ANIM: Cell<Anim> = const { Cell::new(Anim { active: false, tick: 0, x0: 0, y0: 0, x1: 0, y1: 0 }) };
}

fn s(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

/// Creates the hidden popup window.
pub unsafe fn create(hinst: HINSTANCE) -> HWND {
    let class = wide("TaskbarBatteryPopup");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(proc),
        hInstance: hinst,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszClassName: class.as_ptr(),
        ..zeroed()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class.as_ptr(),
        class.as_ptr(),
        WS_POPUP,
        0, 0, 0, 0,
        null_mut(),
        null_mut(),
        hinst,
        null_mut(),
    );
    // Round the corners like a Win11 flyout (no-op where unsupported).
    let pref: i32 = DWMWCP_ROUND;
    DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &pref as *const i32 as *const c_void, size_of::<i32>() as u32);
    hwnd
}

/// Popup (w, h) in physical pixels for `rows` device rows at `dpi`.
fn size_for(rows: usize, dpi: u32) -> (i32, i32) {
    let rows = rows.max(1) as i32;
    (s(WIDTH, dpi), s(PAD * 2, dpi) + rows * s(ROW_H, dpi))
}

/// Where the popup goes given the tray icon rect and the usable work area.
fn position(icon: RECT, work: RECT, w: i32, h: i32, margin: i32) -> (i32, i32) {
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
    let cx = (icon.left + icon.right) / 2;
    let cy = (icon.top + icon.bottom) / 2;
    // The icon hugs the taskbar edge: which work-area edge is it closest to?
    if cy > work.bottom - margin * 4 {
        (clamp(cx - w / 2, work.left + margin, work.right - w - margin), icon.top - h - margin)
    } else if cy < work.top + margin * 4 {
        (clamp(cx - w / 2, work.left + margin, work.right - w - margin), icon.bottom + margin)
    } else if cx > work.right - margin * 4 {
        (icon.left - w - margin, clamp(cy - h / 2, work.top + margin, work.bottom - h - margin))
    } else {
        (icon.right + margin, clamp(cy - h / 2, work.top + margin, work.bottom - h - margin))
    }
}

/// Honors Windows' "window animations" accessibility toggle.
unsafe fn animations_enabled() -> bool {
    let mut ai: ANIMATIONINFO = zeroed();
    ai.cbSize = size_of::<ANIMATIONINFO>() as u32;
    SystemParametersInfoW(SPI_GETANIMATION, size_of::<ANIMATIONINFO>() as u32, &mut ai as *mut ANIMATIONINFO as *mut c_void, 0) != 0
        && ai.iMinAnimate != 0
}

/// Shows the popup anchored to the tray icon (or toggles it closed if already up).
pub unsafe fn toggle(hwnd: HWND) {
    if IsWindowVisible(hwnd) != 0 {
        KillTimer(hwnd, ANIM_TIMER);
        ShowWindow(hwnd, SW_HIDE);
        return;
    }
    place_and_show(hwnd, true);
}

/// Repositions and (re)shows. Called on open (animated slide from the icon) and
/// whenever readings change while open (snaps straight to place).
unsafe fn place_and_show(hwnd: HWND, animate: bool) {
    let dpi = GetDpiForWindow(hwnd).max(96);
    let (w, h) = size_for(rows().len(), dpi);
    let margin = s(8, dpi);
    let mut work: RECT = zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as *mut c_void, 0);
    let icon = tray::icon_rect();
    let (x, y) = match icon {
        Some(icon) => position(icon, work, w, h, margin),
        // Icon unreachable (shouldn't happen): bottom-right corner of the work area.
        None => (work.right - w - margin, work.bottom - h - margin),
    };

    KillTimer(hwnd, ANIM_TIMER);
    ANIM.with(|a| a.set(Anim::default()));
    if animate && IsWindowVisible(hwnd) == 0 && animations_enabled() {
        // Start offset toward the icon's center, then slide to the final
        // position on the timer — it reads as growing out of the icon.
        let slide = s(ANIM_SLIDE, dpi) as f64;
        let (px, py) = (x + w / 2, y + h / 2);
        let (ix, iy) = match icon {
            Some(r) => ((r.left + r.right) / 2, (r.top + r.bottom) / 2),
            None => (px, py + slide as i32), // fall back to sliding up
        };
        let (dx, dy) = (ix - px, iy - py);
        let len = ((dx * dx + dy * dy) as f64).sqrt().max(1.0);
        let (x0, y0) = (x + (dx as f64 * slide / len) as i32, y + (dy as f64 * slide / len) as i32);
        SetWindowPos(hwnd, HWND_TOPMOST, x0, y0, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        ANIM.with(|a| a.set(Anim { active: true, tick: 0, x0, y0, x1: x, y1: y }));
        SetTimer(hwnd, ANIM_TIMER, ANIM_MS, None);
    } else {
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
    SetForegroundWindow(hwnd);
    InvalidateRect(hwnd, null(), 1);
}

/// After a scan: keep an open popup live, or do nothing when it's hidden.
pub unsafe fn refresh(hwnd: HWND) {
    if IsWindowVisible(hwnd) != 0 {
        place_and_show(hwnd, false);
    }
}

/// Visible rows: devices the user didn't hide. NotConnected devices stay — the popup
/// is the one place they're still listed (greyed out).
fn rows() -> Vec<Reading> {
    let hidden = settings::hidden_devices();
    crate::widget::readings().into_iter().filter(|r| !hidden.contains(&r.id)).collect()
}

/// Paints the whole window (double buffered).
unsafe fn paint(hwnd: HWND, light: bool) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let dpi = GetDpiForWindow(hwnd).max(96);

    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp);

    let (bg, fg, muted) = if light { (0xF9F9F9, 0x151515, 0x707070) } else { (0x202020, 0xF5F5F5, 0x9A9A9A) };
    let (amber, green, red) = if light { (0xC07C00, 0x0F7B0F, 0xC42B1C) } else { (0xD89B00, 0x6CCB5F, 0xFF6B6B) };
    let track = if light { 0xE0E0E0 } else { 0x383838 };
    let accent = if light { 0x0067C0 } else { 0x4CC2FF };

    let bg_brush = CreateSolidBrush(bg);
    FillRect(mem, &rc, bg_brush);
    DeleteObject(bg_brush);

    let text_font = render::font("Segoe UI", -s(12, dpi));
    let icon_font = render::font("Segoe Fluent Icons", -s(15, dpi));
    SetBkMode(mem, TRANSPARENT as _);

    let items = rows();
    if items.is_empty() {
        SelectObject(mem, text_font);
        SetTextColor(mem, muted);
        let t = wide("No devices found");
        let mut r = rc;
        DrawTextW(mem, t.as_ptr(), t.len() as i32 - 1, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    for (i, r) in items.iter().enumerate() {
        let top = s(PAD, dpi) + i as i32 * s(ROW_H, dpi);
        let icon_x = s(PAD, dpi);
        let text_x = icon_x + s(26, dpi);
        let right = w - s(PAD, dpi);
        let (name_c, status_c, bar_c) = match r.status {
            Status::Battery { percent, charging, stale } => {
                let sev = if charging {
                    accent
                } else if percent <= tray::CRIT_PCT {
                    red
                } else if percent <= tray::WARN_PCT {
                    amber
                } else {
                    green
                };
                if stale {
                    (muted, muted, muted)
                } else {
                    (fg, sev, sev)
                }
            }
            Status::NotConnected => (muted, muted, track),
        };

        // Kind glyph, vertically centered in the row.
        SelectObject(mem, icon_font);
        SetTextColor(mem, name_c);
        let glyph = wide(render::icon(r.kind));
        let mut gz: SIZE = zeroed();
        GetTextExtentPoint32W(mem, glyph.as_ptr(), glyph.len() as i32 - 1, &mut gz);
        TextOutW(mem, icon_x, top + (s(ROW_H, dpi) - gz.cy) / 2, glyph.as_ptr(), glyph.len() as i32 - 1);

        // Name (ellipsized) top-left, status right-aligned on the same line.
        SelectObject(mem, text_font);
        SetTextColor(mem, name_c);
        let mut nr = RECT { left: text_x, top, right: right - s(120, dpi), bottom: top + s(20, dpi) };
        let name = wide(&r.name);
        DrawTextW(mem, name.as_ptr(), name.len() as i32 - 1, &mut nr, DT_LEFT | DT_END_ELLIPSIS | DT_SINGLELINE);
        SetTextColor(mem, status_c);
        let mut sr = RECT { left: right - s(120, dpi), top, right, bottom: top + s(20, dpi) };
        let st = wide(&r.status.to_string());
        DrawTextW(mem, st.as_ptr(), st.len() as i32 - 1, &mut sr, DT_RIGHT | DT_SINGLELINE);

        // Level bar under the text: track + fill.
        if r.status != Status::NotConnected {
            let (bx, by, bw, bh) = (text_x, top + s(24, dpi), right - text_x, s(4, dpi));
            let mut br = RECT { left: bx, top: by, right: bx + bw, bottom: by + bh };
            let track_brush = CreateSolidBrush(track);
            FillRect(mem, &br, track_brush);
            DeleteObject(track_brush);
            if let Status::Battery { percent, .. } = r.status {
                br.right = bx + bw * percent.min(100) as i32 / 100;
                if br.right > br.left {
                    let fill_brush = CreateSolidBrush(bar_c);
                    FillRect(mem, &br, fill_brush);
                    DeleteObject(fill_brush);
                }
            }
        }
    }

    BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteObject(text_font);
    DeleteObject(icon_font);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

/// The popup window procedure. Closes when the user clicks away.
pub unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, _lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd, crate::widget::light_theme());
            0
        }
        WM_TIMER if wp == ANIM_TIMER => {
            let a = ANIM.with(|c| c.get());
            if !a.active {
                KillTimer(hwnd, ANIM_TIMER);
                return 0;
            }
            let t = ((a.tick + 1) as f64 / ANIM_STEPS as f64).min(1.0);
            let e = 1.0 - (1.0 - t).powi(3); // ease-out cubic
            let x = a.x0 + ((a.x1 - a.x0) as f64 * e) as i32;
            let y = a.y0 + ((a.y1 - a.y0) as f64 * e) as i32;
            SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            if t >= 1.0 {
                KillTimer(hwnd, ANIM_TIMER);
                SetWindowPos(hwnd, HWND_TOPMOST, a.x1, a.y1, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
                ANIM.with(|c| c.set(Anim::default()));
            } else {
                ANIM.with(|c| c.set(Anim { tick: a.tick + 1, ..a }));
            }
            0
        }
        WM_ACTIVATE => {
            if wp == WA_INACTIVE as usize {
                KillTimer(hwnd, ANIM_TIMER);
                ShowWindow(hwnd, SW_HIDE);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, _lp),
    }
}
