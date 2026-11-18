use crate::types::*;
// use crate::ui::state::AppTheme;

// Subclass ID for our centralized theme handler
const SUBCLASS_ID_THEME: usize = 4242;

/// Installs the theme subclass procedure on the window.
pub unsafe fn subclass_control(hwnd: HWND) {
    unsafe { SetWindowSubclass(hwnd, Some(theme_subclass_proc), SUBCLASS_ID_THEME, 0); }
}

/// Removes the theme subclass procedure.
pub unsafe fn unsubclass_control(hwnd: HWND) {
    unsafe { RemoveWindowSubclass(hwnd, Some(theme_subclass_proc), SUBCLASS_ID_THEME); }
}

unsafe extern "system" fn theme_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id_subclass: usize,
    _ref_data: usize,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            unsafe { unsubclass_control(hwnd); }
            // Pass to next purely to behave nicely, though we just removed ourselves.
            // DefSubclassProc might be invalid if we removed? 
            // Docs say: "You don't need to call RemoveWindowSubclass if you handle WM_NCDESTROY"
            // But for WM_DESTROY it's good practice.
            // Let's safe-call DefSubclassProc
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_ERASEBKGND => {
            // Let specific controls handle this or propagate
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_PAINT => {
            // We can intercept paint to enforce dark mode backgrounds on certain standard controls
            // that stubbornly stay light (like some statics or checkboxes in older styles).
            // However, usually WM_CTLCOLOR* handles this.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        // Intercept Theme/Color messages?
        // Actually, the PARENT receives WM_CTLCOLORBTN, etc.
        // This subclass is on the CHILD.
        // So this is useful for OWM_THEMECHANGED or custom painting.
        // For standard controls, theming is often done by the parent responding to WM_CTLCOLOR*.
        // BUT, if we use "Visual Styles" (SetWindowTheme(hwnd, mode)), that's on the child.
        
        WM_KEYDOWN => {
            if unsafe { crate::ui::input::handle_subclass_dispatch(hwnd, msg, wparam) } {
                return 0;
            }
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

/// Applies the visual theme to a control.
pub unsafe fn apply_theme_to_control(hwnd: HWND, is_dark: bool) {
    if hwnd.is_null() { return; }
    let mut cls = [0u16; 48];
    let len = GetClassNameW(hwnd, cls.as_mut_ptr(), cls.len() as i32);
    let name = String::from_utf16_lossy(&cls[..len.max(0) as usize]).to_lowercase();
    let kind = match name.as_str() {
        "button" => match GetWindowLongW(hwnd, GWL_STYLE) as u32 & 0x0f {
            0x07 => crate::ui::theme::ControlType::GroupBox,
            0x02 | 0x03 | 0x05 | 0x06 => crate::ui::theme::ControlType::CheckBox,
            0x04 | 0x09 => crate::ui::theme::ControlType::RadioButton,
            _ => crate::ui::theme::ControlType::Button,
        },
        "combobox" => crate::ui::theme::ControlType::ComboBox,
        "edit" => crate::ui::theme::ControlType::Edit,
        "syslistview32" => crate::ui::theme::ControlType::List,
        _ => {
            InvalidateRect(hwnd, std::ptr::null(), 1);
            return;
        }
    };
    // Use the centralized control-type-specific theme. Passing an empty
    // sub-app string to SetWindowTheme was disabling checkbox visuals and
    // letting Windows draw black checkbox captions on dark backgrounds.
    crate::ui::theme::apply_theme(hwnd, kind, is_dark);
    InvalidateRect(hwnd, std::ptr::null(), 1);
}
