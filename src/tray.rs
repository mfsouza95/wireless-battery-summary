//! The single notification-area icon.
//!
//! `Shell_NotifyIconW` with a runtime-generated HICON rendered from the same Segoe Fluent
//! Icons glyphs the popup uses. Icon state:
//!
//! - normal: neutral battery glyph (white on dark theme, black on light)
//! - warning: any visible device at or below `WARN_PCT` -> amber badge on the glyph
//! - critical: any visible device at or below `CRIT_PCT` -> that device's own icon in red
//!
//! Warnings only look at devices that report a battery and are not charging (a charging
//! device is on its way back up). Stale readings of sleeping devices still count — a
//! mouse that fell asleep at 9% stays a warning. Thresholds clear `HYSTERESIS` points
//! above the entry level so a reading hovering near a boundary doesn't flicker the icon.
//!
//! The icon identity is a fixed GUID, so `Shell_NotifyIconGetRect` can find it for popup
//! anchoring and Windows keeps its overflow/pin state across restarts.

use crate::device::{Kind, Reading, Status};
use crate::{render, wide};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// uCallbackMessage on the controller window.
pub const WM_APP_TRAY: u32 = WM_APP + 3;

// NOTIFYICON_VERSION_4 events arrive in LOWORD(lParam).
pub const NIN_SELECT: u32 = 0x400;
pub const NIN_KEYSELECT: u32 = 0x401;

/// Any visible, non-charging device at or below this level adds the warning badge.
pub const WARN_PCT: u8 = 20;
/// ... and at or below this level the icon becomes that device's own icon in red.
pub const CRIT_PCT: u8 = 10;
/// A warning clears this many points above its threshold (22% / 12%).
const HYSTERESIS: u8 = 2;

const GUID_ICON: GUID = GUID::from_u128(0x7f3b9d21_4c8a_4e5f_9b1d_2a8c6e0d4f71);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconState {
    Normal,
    /// Number of low devices, shown inside the badge.
    Warn(u8),
    /// The lowest device's kind: its glyph is shown in red.
    Critical(Kind),
}

/// Tracks the current icon state with hysteresis.
pub struct Warnings {
    pub state: IconState,
}

impl Warnings {
    pub const fn new() -> Self {
        Self { state: IconState::Normal }
    }

    /// The worst visible device decides the icon. Returns the new state.
    pub fn update(&mut self, readings: &[Reading], hidden: &[String]) -> IconState {
        // While already warned, devices count up to the recovery ceiling so the
        // badge number doesn't collapse during hysteresis.
        let warned = self.state != IconState::Normal;
        let warn_ceiling = WARN_PCT + if warned { HYSTERESIS } else { 0 };
        let mut worst: Option<(u8, Kind)> = None;
        let mut low = 0u8;
        for r in readings {
            if r.is_hidden(hidden) {
                continue;
            }
            if let Status::Battery { percent, charging: false, .. } = r.status {
                if percent <= warn_ceiling {
                    low += 1;
                }
                if worst.map_or(true, |(p, _)| percent < p) {
                    worst = Some((percent, r.kind));
                }
            }
        }
        self.state = match worst {
            None => IconState::Normal,
            Some((p, kind)) if p <= CRIT_PCT || (self.state == IconState::Critical(kind) && p <= CRIT_PCT + HYSTERESIS) => IconState::Critical(kind),
            Some((p, _)) if p <= warn_ceiling => IconState::Warn(low.max(1)),
            _ => IconState::Normal,
        };
        self.state
    }
}

unsafe fn icon_data(hwnd: HWND, icon: HICON, tip: &[u16]) -> NOTIFYICONDATAW {
    let mut d: NOTIFYICONDATAW = zeroed();
    d.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.guidItem = GUID_ICON;
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID | NIF_SHOWTIP;
    d.uCallbackMessage = WM_APP_TRAY;
    d.hIcon = icon;
    let n = tip.len().min(d.szTip.len()) - 1; // keep one NUL
    d.szTip[..n].copy_from_slice(&tip[..n]);
    d
}

/// Adds the icon (also re-adds after an Explorer restart) and selects version 4 events.
pub unsafe fn add(hwnd: HWND, icon: HICON, tip: &[u16]) {
    let mut d = icon_data(hwnd, icon, tip);
    Shell_NotifyIconW(NIM_ADD, &d);
    d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    Shell_NotifyIconW(NIM_SETVERSION, &d);
}

/// Swaps the icon and/or tooltip (NIM_MODIFY).
pub unsafe fn update(hwnd: HWND, icon: HICON, tip: &[u16]) {
    Shell_NotifyIconW(NIM_MODIFY, &icon_data(hwnd, icon, tip));
}

pub unsafe fn remove(hwnd: HWND) {
    let mut d: NOTIFYICONDATAW = zeroed();
    d.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.guidItem = GUID_ICON;
    d.uFlags = NIF_GUID;
    Shell_NotifyIconW(NIM_DELETE, &d);
}

/// On-screen rect of our tray icon, for popup anchoring. None when the icon isn't up.
pub unsafe fn icon_rect() -> Option<RECT> {
    let mut id: NOTIFYICONIDENTIFIER = zeroed();
    id.cbSize = size_of::<NOTIFYICONIDENTIFIER>() as u32;
    id.guidItem = GUID_ICON;
    let mut rc: RECT = zeroed();
    (Shell_NotifyIconGetRect(&id, &mut rc) == 0).then_some(rc)
}

/// One line per visible device for the tray tooltip (127 chars, one line).
pub fn tooltip(readings: &[Reading], hidden: &[String]) -> Vec<u16> {
    let text = readings
        .iter()
        .filter(|r| !r.is_hidden(hidden))
        .map(|r| format!("{} · {}", r.name, r.status))
        .collect::<Vec<_>>()
        .join("  |  ");
    wide(&if text.is_empty() { "Taskbar Battery".into() } else { text })
}

/// Builds the HICON for a state: neutral glyph, glyph + badge, or device glyph in red.
/// `size` is the icon edge in physical pixels (SM_CXSMICON at the system DPI).
pub unsafe fn make_icon(state: IconState, size: i32, light: bool) -> HICON {
    let fg = if light { 0x000000 } else { 0xFFFFFF };
    let red = if light { 0xC42B1C } else { 0xFF6B6B };
    let glyph = render::icon(match state {
        IconState::Critical(k) => k,
        _ => Kind::Other, // battery glyph
    });
    let color = match state {
        IconState::Critical(_) => red,
        _ => fg,
    };

    struct Seg {
        text: Vec<u16>,
        font: HFONT,
        color: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    let glyph_font = render::font("Segoe Fluent Icons", -size * 7 / 8);

    let measure = |dc: HDC, font: HFONT, text: &str| -> (Vec<u16>, i32, i32) {
        let t = wide(text);
        SelectObject(dc, font);
        let mut sz: SIZE = zeroed();
        GetTextExtentPoint32W(dc, t.as_ptr(), t.len() as i32 - 1, &mut sz);
        (t, sz.cx, sz.cy)
    };

    let (text, w, h) = measure(dc, glyph_font, glyph);
    let segs = [Seg { text, font: glyph_font, color, x: (size - w) / 2, y: (size - h) / 2, w, h }];

    // White-on-black text; coverage becomes alpha. (Same trick as the old taskbar renderer.)
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size,
        biHeight: -size,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB as _,
        ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteObject(glyph_font);
        DeleteDC(dc);
        return null_mut();
    }
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as _);
    SetTextColor(dc, 0x00FF_FFFF);
    for s in &segs {
        SelectObject(dc, s.font);
        TextOutW(dc, s.x, s.y, s.text.as_ptr(), s.text.len() as i32 - 1);
    }
    GdiFlush();

    // Per-pixel color from the segment boxes (later segs win), coverage -> premultiplied BGRA.
    let mut paint = vec![(fg, 255u32); (size * size) as usize];
    for s in &segs {
        for y in s.y.max(0)..(s.y + s.h).min(size) {
            for x in s.x.max(0)..(s.x + s.w).min(size) {
                paint[(y * size + x) as usize] = (s.color, 255);
            }
        }
    }
    // Warn badge: solid disc filling the top-right quadrant, drawn per-pixel
    // (a font glyph can't rasterize a clean circle at 16px). Anti-aliased edge.
    let badge = matches!(state, IconState::Warn(_));
    let (bcx, bcy, brad) = (size as f32 * 0.75, size as f32 * 0.25, size as f32 / 4.0);
    let (sr, sg, sb) = (((red >> 16) & 0xFF) as f32, ((red >> 8) & 0xFF) as f32, (red & 0xFF) as f32);
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
    for (i, p) in px.iter_mut().enumerate() {
        let (color, opacity) = paint[i];
        let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
        let a = cov * opacity / 255;
        let ch = |shift: u32| ((color >> shift) & 0xFF) * a / 255;
        *p = (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0);
        if badge {
            let d = ((i as i32 % size) as f32 + 0.5 - bcx).hypot((i as i32 / size) as f32 + 0.5 - bcy);
            let sa = (brad - d + 0.5).clamp(0.0, 1.0);
            if sa > 0.0 {
                // Source-over: premultiplied src over the (premultiplied) glyph pixel.
                let inv = 1.0 - sa;
                let (da, dr, dg, db) = ((*p >> 24) as f32 / 255.0, ((*p >> 16) & 0xFF) as f32, ((*p >> 8) & 0xFF) as f32, (*p & 0xFF) as f32);
                let (oa, or_, og, ob) = (sa + da * inv, sr * sa + dr * inv, sg * sa + dg * inv, sb * sa + db * inv);
                *p = ((oa * 255.0) as u32) << 24 | ((or_ as u32) << 16) | ((og as u32) << 8) | ob as u32;
            }
        }
    }

    // 1bpp AND mask, all zero: transparency comes from the color bitmap's alpha.
    let mask = {
        let stride = (size + 15) / 16 * 2;
        let zeros = vec![0u8; (stride * size) as usize];
        CreateBitmap(size, size, 1, 1, zeros.as_ptr() as *const c_void)
    };
    let icon = CreateIconIndirect(&ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: bmp });

    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteObject(mask);
    DeleteObject(glyph_font);
    DeleteDC(dc);
    icon
}

/// Icon edge in pixels for the current system DPI (notification icons are small icons).
pub unsafe fn icon_size() -> i32 {
    GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind: Kind, percent: u8, charging: bool, stale: bool) -> Reading {
        Reading { id: percent.to_string(), ids: vec![percent.to_string()], name: "dev".into(), kind, status: Status::Battery { percent, charging, stale }, direct: None }
    }

    #[test]
    fn warns_at_20_and_below() {
        let mut w = Warnings::new();
        assert_eq!(w.update(&[r(Kind::Mouse, 21, false, false)], &[]), IconState::Normal);
        assert_eq!(w.update(&[r(Kind::Mouse, 20, false, false)], &[]), IconState::Warn(1));
    }

    #[test]
    fn badge_counts_every_low_device() {
        let mut w = Warnings::new();
        let rs = [r(Kind::Mouse, 15, false, false), r(Kind::Headset, 18, false, false), r(Kind::Keyboard, 60, false, false)];
        assert_eq!(w.update(&rs, &[]), IconState::Warn(2));
    }

    #[test]
    fn critical_at_10_shows_lowest_devices_kind() {
        let mut w = Warnings::new();
        let rs = [r(Kind::Mouse, 15, false, false), r(Kind::Headset, 8, false, false)];
        assert_eq!(w.update(&rs, &[]), IconState::Critical(Kind::Headset));
    }

    #[test]
    fn hysteresis_clears_above_threshold() {
        let mut w = Warnings::new();
        w.update(&[r(Kind::Mouse, 15, false, false)], &[]);
        assert_eq!(w.update(&[r(Kind::Mouse, 22, false, false)], &[]), IconState::Warn(1)); // still warn
        assert_eq!(w.update(&[r(Kind::Mouse, 23, false, false)], &[]), IconState::Normal);
    }

    #[test]
    fn charging_and_hidden_and_not_connected_never_warn() {
        let mut w = Warnings::new();
        assert_eq!(w.update(&[r(Kind::Mouse, 5, true, false)], &[]), IconState::Normal);
        assert_eq!(w.update(&[r(Kind::Mouse, 5, false, false)], &["5".into()]), IconState::Normal);
        let mut nc = r(Kind::Mouse, 5, false, false);
        nc.status = Status::NotConnected;
        assert_eq!(w.update(&[nc], &[]), IconState::Normal);
    }

    #[test]
    fn stale_readings_still_warn() {
        let mut w = Warnings::new();
        assert_eq!(w.update(&[r(Kind::Mouse, 9, false, true)], &[]), IconState::Critical(Kind::Mouse));
    }
}
