//! Windows checkbox caption rendering.
//!
//! Theming a BS_AUTOCHECKBOX via "DarkMode_Explorer" changes its glyph, but
//! Windows' themed BUTTON painter still uses black caption text in many
//! Windows 11 builds. WM_CTLCOLORBTN is ignored by that painter. Keep the
//! native checkbox (mouse, keyboard, focus, BN_CLICKED and BM_GETCHECK) and
//! only repaint the caption in our app foreground colour after native paint.
//! This is intentionally shared by every checkbox built through ControlBuilder
//! and is updated when switching between light and dark themes.

#![allow(unsafe_op_in_unsafe_fn)]
use crate::types::*;
use std::ptr;

const CHECKBOX_CAPTION_SUBCLASS: usize = 0xC0C0_1024;
const WM_NCDESTROY: u32 = 0x0082;
const WM_PRINTCLIENT: u32 = 0x0318;
const WM_GETFONT: u32 = 0x0031;
const DT_NOPREFIX: u32 = 0x0800;
const WS_DISABLED: u32 = 0x0800_0000;
const COLOR_LIGHT_CAPTION: u32 = 0x00E8E8E8;  // COLORREF: pale gray/white
const COLOR_DISABLED_CAPTION: u32 = 0x00909090;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(hwnd: HWND) -> HDC;
    fn ReleaseDC(hwnd: HWND, hdc: HDC) -> i32;
    fn GetFocus() -> HWND;
    fn DrawFocusRect(hdc: HDC, rc: *const RECT) -> BOOL;
}

unsafe fn paint_caption(hwnd: HWND, hdc: HDC) {
    if hdc.is_null() { return; }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 { return; }
    let mut text = vec![0u16; len as usize + 1];
    let actual = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
    if actual <= 0 { return; }

    let mut rect: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rect);
    // Leave the native checkbox glyph completely untouched. The left edge
    // coincides with the native BUTTON text margin at normal and HiDPI scales.
    let gap = crate::ui::theme::scale(19);
    let text_left = gap.min(rect.right);
    if text_left >= rect.right { return; }
    rect.left = text_left;

    // The native BUTTON painter has already drawn dark glyph + black text.
    // Replace only the caption region using the *same* dark panel background.
    FillRect(hdc, &rect, crate::ui::theme::get_dark_brush());
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0) as HFONT;
    let saved = if font.is_null() {
        ptr::null_mut()
    } else {
        SelectObject(hdc, font as HGDIOBJ)
    };
    SetBkMode(hdc, TRANSPARENT as i32);
    let disabled = (GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_DISABLED) != 0;
    SetTextColor(hdc, if disabled { COLOR_DISABLED_CAPTION } else { COLOR_LIGHT_CAPTION });
    let mut caption = rect;
    DrawTextW(hdc, text.as_ptr(), actual, &mut caption, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);

    // Preserve keyboard accessibility: the native focus rectangle was
    // covered along with the dark caption and must be restored.
    if GetFocus() == hwnd {
        let mut focus = rect;
        focus.left = (focus.left + 1).min(focus.right);
        DrawFocusRect(hdc, &focus);
    }
    if !saved.is_null() { SelectObject(hdc, saved); }
}

unsafe extern "system" fn checkbox_caption_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    dark: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(checkbox_caption_proc), CHECKBOX_CAPTION_SUBCLASS);
        return DefSubclassProc(hwnd, msg, wparam, lparam);
    }
    if msg == WM_PAINT || msg == WM_PRINTCLIENT {
        // Run the native renderer FIRST, preserving focus, check, hover,
        // disabled and accessibility behaviour of BS_AUTOCHECKBOX.
        let result = DefSubclassProc(hwnd, msg, wparam, lparam);
        if dark != 0 {
            if msg == WM_PRINTCLIENT {
                paint_caption(hwnd, wparam as HDC);
            } else {
                let hdc = GetDC(hwnd);
                paint_caption(hwnd, hdc);
                if !hdc.is_null() { ReleaseDC(hwnd, hdc); }
            }
        }
        return result;
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Called by the centralized theme handler on creation AND on theme changes.
/// Re-registering a subclass with the same ID updates its dark/light userdata.
pub unsafe fn apply(hwnd: HWND, is_dark: bool) {
    if hwnd.is_null() { return; }
    SetWindowSubclass(
        hwnd, Some(checkbox_caption_proc), CHECKBOX_CAPTION_SUBCLASS,
        usize::from(is_dark),
    );
    InvalidateRect(hwnd, ptr::null(), 1);
}

#[cfg(feature = "shell-broker-smoke")]
pub unsafe fn verify_dark_caption_pixels() -> bool {
    // This is a real native Windows GDI paint test, not a unit test that merely
    // checks our chosen foreground constant. It runs on windows-2022 CI.
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetPixel(hdc: HDC, x: i32, y: i32) -> u32;
    }
    let parent = CreateWindowExW(
        0, crate::w!("STATIC").as_ptr(), crate::w!("").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        100, 100, 340, 120, ptr::null_mut(), ptr::null_mut(),
        GetModuleHandleW(ptr::null()), ptr::null_mut(),
    );
    if parent.is_null() { return false; }
    let child = crate::ui::builder::ControlBuilder::new(parent, 5001)
        .checkbox().text("Case Sensitive").dark_mode(true)
        .pos(12, 15).size(245, 32).build();
    if child.is_null() {
        DestroyWindow(parent);
        return false;
    }

    SendMessageW(child, BM_SETCHECK, 1, 0);
    let toggled = SendMessageW(child, BM_GETCHECK, 0, 0) == 1;
    InvalidateRect(child, ptr::null(), 1);
    UpdateWindow(child);

    // Scan only the CAPTION rectangle. A dark native caption contains almost
    // no bright pixels; our explicit pale gray text produces hundreds.
    let dc = GetDC(child);
    let mut bright = 0usize;
    if !dc.is_null() {
        for y in 0..32 {
            for x in 22..200 {
                let c = GetPixel(dc, x, y);
                if c != u32::MAX && (c & 0xff) >= 170
                    && ((c >> 8) & 0xff) >= 170 && ((c >> 16) & 0xff) >= 170 {
                    bright += 1;
                }
            }
        }
        ReleaseDC(child, dc);
    }
    crate::shell_batch::record_mock(&format!("DARK_CHECKBOX_BRIGHT_PIXELS|{}",bright));

    // Verify theme changes can reach the same existing checkbox HWND.
    crate::ui::theme::apply_theme(child, crate::ui::theme::ControlType::CheckBox, false);
    UpdateWindow(child);
    crate::ui::theme::apply_theme(child, crate::ui::theme::ControlType::CheckBox, true);
    UpdateWindow(child);
    let state_after_switch = SendMessageW(child, BM_GETCHECK, 0, 0) == 1;
    DestroyWindow(parent);
    toggled && state_after_switch && bright >= 30
}
