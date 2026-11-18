//! Alternative display mode: a layered child window embedded in the taskbar, showing
//! every device's glyph + percentage side by side — the upstream project's approach.
//!
//! Unlike the tray icon this relies on undocumented taskbar internals (a child window
//! of Shell_TrayWnd parked next to TrayNotifyWnd), so a Windows update can break its
//! placement. That's why it lives behind a menu option instead of being the default.

use crate::device::{Kind, Status};
use crate::{render, settings, wide};
use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const LOW_BATTERY: u8 = 15;
const BOLT: &str = "\u{E945}";
/// Opacity for the last known value of a sleeping device.
const STALE_OPACITY: u32 = 130;

/// Right-click anywhere on the strip -> the shared context menu on the controller.
const WM_APP_MENU: u32 = WM_APP + 2;

struct Strip {
    widget: HWND,
    tooltip: HWND,
    size: (i32, i32),
    layout: Option<Layout>,
}

thread_local! {
    static STRIP: RefCell<Strip> = const {
        RefCell::new(Strip { widget: null_mut(), tooltip: null_mut(), size: (0, 0), layout: None })
    };
}

fn with<R>(f: impl FnOnce(&mut Strip) -> R) -> R {
    STRIP.with(|s| f(&mut s.borrow_mut()))
}

/// Registers the strip's window class. Idempotent.
pub unsafe fn register(hinst: HINSTANCE) {
    let class = wide("TaskbarBatteryWidget");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(proc),
        hInstance: hinst,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: class.as_ptr(),
        ..zeroed()
    };
    RegisterClassW(&wc);
}

/// Creates the strip inside the taskbar if it doesn't exist yet, then (re)draws it.
/// `controller` receives the right-click menu message.
pub unsafe fn show(controller: HWND) {
    if !with(|s| s.widget).is_null() && IsWindow(with(|s| s.widget)) != 0 {
        redraw(controller);
        return;
    }
    let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
    if tray.is_null() {
        return; // Explorer not up yet; the next timer tick retries.
    }
    let class = wide("TaskbarBatteryWidget");
    let widget = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        null(),
        WS_CHILD | WS_CLIPSIBLINGS,
        0, 0, 1, 1,
        tray,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    let old_tip = with(|s| s.tooltip);
    if !old_tip.is_null() {
        DestroyWindow(old_tip); // its tool was the previous (dead) widget
    }
    let tip = create_tooltip(controller, widget);
    with(|s| {
        s.widget = widget;
        s.tooltip = tip;
        s.size = (0, 0);
        s.layout = None;
    });
    redraw(controller);
}

/// Destroys the strip (mode switched away, or the app is exiting).
pub unsafe fn remove() {
    let (w, tip) = with(|s| (s.widget, s.tooltip));
    if !tip.is_null() {
        DestroyWindow(tip);
    }
    if !w.is_null() && IsWindow(w) != 0 {
        DestroyWindow(w);
    }
    with(|s| {
        s.widget = null_mut();
        s.tooltip = null_mut();
        s.size = (0, 0);
        s.layout = None;
    });
}

/// Explorer restarted: the child window died with the old taskbar. Forget it so the
/// next `show` recreates it.
pub unsafe fn forget() {
    with(|s| {
        s.widget = null_mut();
        s.tooltip = null_mut();
    });
}

/// Periodic housekeeping: create the strip if missing and keep it anchored.
pub unsafe fn tick(controller: HWND) {
    show(controller);
    place(controller);
}

/// Repaint from the latest readings (scan finished, or theme/visibility changed).
pub unsafe fn redraw(controller: HWND) {
    let widget = with(|s| s.widget);
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
    if tray.is_null() {
        return;
    }
    let layout = taskbar_layout(tray);
    let dpi = GetDpiForWindow(tray).max(96);
    let hidden = settings::hidden_devices();
    // Disconnected devices drop off the bar (they stay listed in the right-click menu).
    let shown: Vec<_> = crate::widget::readings()
        .into_iter()
        .filter(|r| !hidden.contains(&r.id) && r.status != Status::NotConnected)
        .collect();
    let tip = with(|s| s.tooltip);
    if !tip.is_null() {
        let mut text = wide(&shown.iter().map(|r| format!("{}: {}", r.name, r.status)).collect::<Vec<_>>().join("\r\n"));
        let ti = tool_info(widget, controller, &mut text);
        SendMessageW(tip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
    }
    let items: Vec<_> = shown.iter().map(|r| (r.kind, r.status)).collect();
    let light = crate::widget::light_theme();
    let size = if items.is_empty() { (0, 0) } else { draw(widget, &items, dpi, layout, light) };
    with(|s| {
        s.size = size;
        s.layout = Some(layout);
    });
    place(controller);
}

/// Hover tooltip listing each device's full name and status.
unsafe fn create_tooltip(owner: HWND, widget: HWND) -> HWND {
    let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
    InitCommonControlsEx(&icc);
    let tip = CreateWindowExW(
        WS_EX_TOPMOST,
        wide("tooltips_class32").as_ptr(),
        null(),
        WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if tip.is_null() {
        return tip;
    }
    let mut text = wide("");
    let mut ti = tool_info(widget, owner, &mut text);
    SendMessageW(tip, TTM_ADDTOOLW, 0, &mut ti as *mut _ as LPARAM);
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 600); // enables multi-line text
    tip
}

unsafe fn tool_info(widget: HWND, owner: HWND, text: &mut [u16]) -> TTTOOLINFOW {
    let mut ti: TTTOOLINFOW = zeroed();
    ti.cbSize = size_of::<TTTOOLINFOW>() as u32;
    ti.uFlags = TTF_IDISHWND | TTF_SUBCLASS;
    ti.hwnd = owner;
    ti.uId = widget as usize;
    ti.lpszText = text.as_mut_ptr();
    ti
}

/// A taskbar docked left/right is taller than wide; lay the strip out as a column there.
unsafe fn taskbar_layout(tray: HWND) -> Layout {
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let (w, h) = (tr.right - tr.left, tr.bottom - tr.top);
    if h > w {
        Layout::Column { width: w }
    } else {
        Layout::Row { height: h }
    }
}

/// Keeps the strip next to the notification area (left of it on a horizontal taskbar,
/// above it on a vertical one) and above the taskbar's XAML content.
unsafe fn place(_controller: HWND) {
    let (widget, (width, height), rendered) = with(|s| (s.widget, s.size, s.layout));
    if widget.is_null() || IsWindow(widget) == 0 {
        return;
    }
    let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
    if tray.is_null() {
        return;
    }
    // Taskbar moved to another edge or resized: the bitmap was drawn for the old shape.
    if rendered != Some(taskbar_layout(tray)) {
        return; // next redraw() picks up the new layout, then places
    }
    if width == 0 {
        ShowWindow(widget, SW_HIDE); // nothing to show
        return;
    }
    let mut tr: RECT = zeroed();
    GetWindowRect(tray, &mut tr);
    let mut nr: RECT = zeroed();
    let notify = FindWindowExW(tray, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
    let has_notify = !notify.is_null() && GetWindowRect(notify, &mut nr) != 0 && nr.right > nr.left;
    let margin = 4 * GetDpiForWindow(tray).max(96) as i32 / 96;
    let (x, y) = match rendered {
        Some(Layout::Column { .. }) => {
            let bottom = if has_notify { nr.top } else { tr.bottom };
            (0, bottom - tr.top - height - margin)
        }
        _ => {
            let right = if has_notify { nr.left } else { tr.right };
            (right - tr.left - width - margin, 0)
        }
    };

    let mut cur: RECT = zeroed();
    GetWindowRect(widget, &mut cur);
    let moved = cur.left - tr.left != x
        || cur.top - tr.top != y
        || cur.right - cur.left != width
        || cur.bottom - cur.top != height;
    let on_top = GetWindow(tray, GW_CHILD) == widget;
    if moved || !on_top || IsWindowVisible(widget) == 0 {
        SetWindowPos(widget, HWND_TOP, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

/// Strip window proc: right-click opens the shared menu, clicks never steal focus.
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, _wp: WPARAM, _lp: LPARAM) -> LRESULT {
    match msg {
        WM_RBUTTONUP => {
            PostMessageW(crate::widget::controller(), WM_APP_MENU, 0, 0);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, _wp, _lp),
    }
}

/// How the strip is laid out on the taskbar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layout {
    /// Horizontal taskbar of this height: devices in a row.
    Row { height: i32 },
    /// Vertical (left/right) taskbar of this width: devices stacked, icon above the label.
    Column { width: i32 },
}

struct Seg {
    text: Vec<u16>,
    icon: bool,
    color: u32, // 0xRRGGBB
    opacity: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    gap_after: i32,
    /// Index of the device this segment belongs to.
    item: usize,
}

/// Draws the strip into a per-pixel-alpha bitmap and pushes it with UpdateLayeredWindow,
/// so text blends cleanly over the (translucent) Windows 11 taskbar.
/// Returns its (width, height) in physical pixels.
unsafe fn draw(hwnd: HWND, items: &[(Kind, Status)], dpi: u32, layout: Layout, light: bool) -> (i32, i32) {
    let s = |v: i32| v * dpi as i32 / 96;
    let fg = if light { 0x000000 } else { 0xFFFFFF };
    let red = if light { 0xC42B1C } else { 0xFF6B6B };
    let green = if light { 0x0F7B0F } else { 0x6CCB5F };

    let mut segs = Vec::new();
    let seg = |item: usize, text: &str, icon: bool, color: u32, opacity: u32, gap_after: i32| Seg {
        text: wide(text),
        icon,
        color,
        opacity,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        gap_after,
        item,
    };
    for (i, (kind, status)) in items.iter().enumerate() {
        let gap = if i == items.len() - 1 { 0 } else { s(12) };
        let (label, color, opacity, charging) = match *status {
            Status::Battery { percent, charging, stale } => {
                let color = if percent <= LOW_BATTERY && !charging { red } else { fg };
                (format!("{percent}%"), color, if stale { STALE_OPACITY } else { 255 }, charging)
            }
            Status::NotConnected => ("—".to_string(), fg, 110, false),
        };
        segs.push(seg(i, render::icon(*kind), true, color, opacity, s(5)));
        if charging {
            segs.push(seg(i, &label, false, color, opacity, s(2)));
            segs.push(seg(i, BOLT, true, green, opacity, gap));
        } else {
            segs.push(seg(i, &label, false, color, opacity, gap));
        }
    }

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    ReleaseDC(null_mut(), screen);
    let text_font = render::font("Segoe UI", -s(12));
    let icon_font = render::font("Segoe Fluent Icons", -s(15));

    // Measure.
    for seg in &mut segs {
        SelectObject(dc, if seg.icon { icon_font } else { text_font });
        let mut size: SIZE = zeroed();
        GetTextExtentPoint32W(dc, seg.text.as_ptr(), seg.text.len() as i32 - 1, &mut size);
        seg.w = size.cx;
        seg.h = size.cy;
    }

    // Layout.
    let pad = s(6);
    let (width, height) = match layout {
        Layout::Row { height } => {
            let mut x = pad;
            for seg in &mut segs {
                seg.x = x;
                seg.y = (height - seg.h) / 2;
                x += seg.w + seg.gap_after;
            }
            (x + pad, height)
        }
        Layout::Column { width } => {
            // Per device: the icon on one line, the label (and charging bolt) centered below it.
            let mut y = pad;
            for i in 0..items.len() {
                let (first, rest) = {
                    let mut it = segs.iter_mut().filter(|s| s.item == i);
                    (it.next(), it.collect::<Vec<_>>())
                };
                if let Some(icon) = first {
                    icon.x = (width - icon.w) / 2;
                    icon.y = y;
                    y += icon.h + s(1);
                }
                let line_w: i32 = rest.iter().map(|s| s.w).sum::<i32>()
                    + rest.iter().rev().skip(1).map(|s| s.gap_after).sum::<i32>();
                let line_h = rest.iter().map(|s| s.h).max().unwrap_or(0);
                let mut x = (width - line_w) / 2;
                for seg in rest {
                    seg.x = x;
                    seg.y = y + (line_h - seg.h) / 2;
                    x += seg.w + seg.gap_after;
                }
                y += line_h;
                if i + 1 < items.len() {
                    y += s(8);
                }
            }
            (width, y + pad)
        }
    };

    // White-on-black text into a 32bpp DIB; coverage becomes alpha below.
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB as _,
        ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteObject(text_font);
        DeleteObject(icon_font);
        DeleteDC(dc);
        return (width, height);
    }
    let old_bmp = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as _);
    SetTextColor(dc, 0x00FF_FFFF);
    for seg in &segs {
        SelectObject(dc, if seg.icon { icon_font } else { text_font });
        TextOutW(dc, seg.x, seg.y, seg.text.as_ptr(), seg.text.len() as i32 - 1);
    }
    GdiFlush();

    // Per-pixel color/opacity from each segment's box, then convert coverage -> premultiplied BGRA.
    let mut paint = vec![(fg, 255u32); (width * height) as usize];
    for seg in &segs {
        let (x0, x1) = (seg.x.max(0), (seg.x + seg.w).min(width));
        let (y0, y1) = (seg.y.max(0), (seg.y + seg.h).min(height));
        for y in y0..y1 {
            for x in x0..x1 {
                paint[(y * width + x) as usize] = (seg.color, seg.opacity);
            }
        }
    }
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
    for (i, p) in px.iter_mut().enumerate() {
        let (color, opacity) = paint[i];
        let cov = ((*p >> 16) & 0xFF).max((*p >> 8) & 0xFF).max(*p & 0xFF);
        let a = cov * opacity / 255;
        // Alpha 1 keeps the whole rect clickable while staying invisible.
        *p = if a == 0 {
            0x0100_0000
        } else {
            let ch = |shift: u32| ((color >> shift) & 0xFF) * a / 255;
            (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
        };
    }

    let size = SIZE { cx: width, cy: height };
    let origin = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(hwnd, null_mut(), null(), &size, dc, &origin, 0, &blend, ULW_ALPHA);

    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteObject(text_font);
    DeleteObject(icon_font);
    DeleteDC(dc);
    (width, height)
}
