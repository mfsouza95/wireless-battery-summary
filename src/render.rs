//! Shared bits for the tray icon and the popup: the Segoe Fluent Icons glyph table
//! and the GDI font helper. Rendering lives with its users (tray.rs, popup.rs).

use crate::device::Kind;
use crate::wide;
use std::mem::zeroed;
use std::ptr::null_mut;
use std::sync::OnceLock;
use windows_sys::Win32::Graphics::Gdi::*;

/// Segoe Fluent Icons glyph per device kind.
pub(crate) unsafe fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Headset => "\u{E7F6}",
        Kind::Earbuds => earbud(),
        Kind::Mouse => "\u{E962}",
        Kind::Keyboard => "\u{E765}",
        Kind::Gamepad => "\u{E7FC}",
        Kind::Other => "\u{E83F}", // battery
    }
}

/// Newer single-earbud glyph (clearer at small sizes); older Segoe Fluent Icons
/// versions only have the pair.
const EARBUD_SINGLE: &str = "\u{F27C}";
const EARBUD_PAIR: &str = "\u{F4C0}";
static EARBUD: OnceLock<&'static str> = OnceLock::new();

unsafe fn earbud() -> &'static str {
    *EARBUD.get_or_init(|| {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);
        let f = font("Segoe Fluent Icons", -20);
        SelectObject(dc, f);
        let single = wide(EARBUD_SINGLE);
        let mut glyph = 0u16;
        let missing = GetGlyphIndicesW(dc, single.as_ptr(), 1, &mut glyph, GGI_MARK_NONEXISTING_GLYPHS) == GDI_ERROR as u32 || glyph == 0xFFFF;
        DeleteObject(f);
        DeleteDC(dc);
        if missing { EARBUD_PAIR } else { EARBUD_SINGLE }
    })
}

pub(crate) unsafe fn font(face: &str, height: i32) -> HFONT {
    let mut lf: LOGFONTW = zeroed();
    lf.lfHeight = height;
    lf.lfWeight = FW_NORMAL as _;
    lf.lfQuality = ANTIALIASED_QUALITY as _; // grayscale AA: coverage maps cleanly to alpha
    for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *dst = src;
    }
    CreateFontIndirectW(&lf)
}
