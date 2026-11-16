//! Window appearance: DWM attributes, acrylic and fallbacks.

use std::ffi::c_void;
use std::mem::size_of;

use windows::core::{w, Result as WinResult, BOOL, PCSTR, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW,
    DWMWA_BORDER_COLOR, DWMWA_COLOR_DEFAULT, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_HOSTBACKDROPBRUSH, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER,
};

use crate::config::BackdropMode;
use crate::theme::{Rgb, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveBackdrop {
    Acrylic,
    System,
    Solid,
}

#[repr(C)]
struct AccentPolicy {
    accent_state: i32,
    accent_flags: i32,
    gradient_color: u32,
    animation_id: i32,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: i32,
    data: *mut c_void,
    size: usize,
}

const WCA_ACCENT_POLICY: i32 = 19;
const ACCENT_DISABLED: i32 = 0;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: i32 = 4;

windows::core::link!("user32.dll" "system" fn SetWindowCompositionAttribute(hwnd: HWND, data: *mut WindowCompositionAttributeData) -> BOOL);

/// Enables the extended DWM frame so the acrylic/system backdrop can show through.
pub fn extend_frame(hwnd: HWND) {
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

pub fn set_dark_mode(hwnd: HWND, dark: bool) {
    let value = BOOL::from(dark);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &value as *const BOOL as *const c_void,
            size_of::<BOOL>() as u32,
        );
    }
}

/// Best-effort dark-mode opt-in for native menus and child controls.
pub fn allow_dark_mode(hwnd: HWND, dark: bool) {
    unsafe {
        let Ok(module) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(allow) = GetProcAddress(module, PCSTR(133 as *const u8)) {
            let allow: unsafe extern "system" fn(HWND, BOOL) -> BOOL = std::mem::transmute(allow);
            let _ = allow(hwnd, BOOL::from(dark));
        }
        let _ = SetWindowTheme(
            hwnd,
            if dark {
                w!("DarkMode_Explorer")
            } else {
                w!("Explorer")
            },
            PCWSTR::null(),
        );
    }
}

pub fn set_rounded_corners(hwnd: HWND, theme: &Theme) {
    if !theme.is_win11() {
        return;
    }
    let preference = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
    }
}

pub fn set_border_color(hwnd: HWND, theme: &Theme, accent: Option<Rgb>) {
    if !theme.is_win11() {
        return;
    }
    let color = accent
        .map(|rgb| rgb.to_colorref())
        .unwrap_or(DWMWA_COLOR_DEFAULT);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
    }
}

/// No DWM corners or border: the window draws its own non-rectangular shape.
/// Both attributes are ignored before Windows 11.
pub fn make_frameless(hwnd: HWND) {
    let corners = windows::Win32::Graphics::Dwm::DWMWCP_DONOTROUND;
    let border = windows::Win32::Graphics::Dwm::DWMWA_COLOR_NONE;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corners as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
    }
}

pub fn set_topmost(hwnd: HWND, topmost: bool) {
    let insert_after = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Applies the configured backdrop and returns what was actually available.
///
/// Acrylic uses the undocumented user32 accent policy because it stays blurred
/// when the widget is inactive. System acrylic uses the documented DWM backdrop
/// and is accepted to become solid while the window is not focused.
pub fn apply_backdrop(hwnd: HWND, theme: &Theme, mode: BackdropMode) -> EffectiveBackdrop {
    extend_frame(hwnd);
    set_dark_mode(hwnd, theme.dark);
    set_rounded_corners(hwnd, theme);

    // Clear previous state first so mode switches do not stack.
    disable_accent(hwnd);
    unsafe {
        let none = DWMSBT_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &none as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
    }

    if theme.effective_solid() || mode == BackdropMode::Solid {
        return EffectiveBackdrop::Solid;
    }

    if mode == BackdropMode::System && theme.supports_system_backdrop() {
        let backdrop = DWMSBT_TRANSIENTWINDOW;
        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &backdrop as *const _ as *const c_void,
                size_of::<i32>() as u32,
            )
        };
        if result.is_ok() {
            return EffectiveBackdrop::System;
        }
    }

    if enable_acrylic(hwnd, theme.acrylic_tint()) {
        EffectiveBackdrop::Acrylic
    } else {
        EffectiveBackdrop::Solid
    }
}

pub fn apply_window_region(hwnd: HWND, theme: &Theme, dpi: u32, backdrop: EffectiveBackdrop) {
    // Win11 DWM rounds the acrylic window itself. On Win10, acrylic would
    // otherwise paint a square blur, so clip it with a rounded region.
    if theme.is_win11() || backdrop != EffectiveBackdrop::Acrylic {
        unsafe {
            let _ = SetWindowRgn(hwnd, None, true);
        }
        return;
    }
    let mut rect = windows::Win32::Foundation::RECT::default();
    if unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return;
    }
    let radius = (18.0 * dpi as f32 / 96.0).round() as i32;
    let region = unsafe { CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2) };
    if !region.is_invalid() {
        unsafe {
            let _ = SetWindowRgn(hwnd, Some(region), true);
        }
    }
}

fn enable_acrylic(hwnd: HWND, gradient_color: u32) -> bool {
    let mut policy = AccentPolicy {
        accent_state: ACCENT_ENABLE_ACRYLICBLURBEHIND,
        accent_flags: 0,
        gradient_color,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut c_void,
        size: size_of::<AccentPolicy>(),
    };
    unsafe { SetWindowCompositionAttribute(hwnd, &mut data).as_bool() }
}

fn disable_accent(hwnd: HWND) {
    let mut policy = AccentPolicy {
        accent_state: ACCENT_DISABLED,
        accent_flags: 0,
        gradient_color: 0,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut c_void,
        size: size_of::<AccentPolicy>(),
    };
    unsafe {
        let _ = SetWindowCompositionAttribute(hwnd, &mut data);
    }
}

/// Starts a normal foreground drag. Used when the widget is clicked.
pub fn bring_to_front(hwnd: HWND, topmost: bool) {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
    }
    if topmost {
        set_topmost(hwnd, true);
    } else {
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Host backdrop for the `Windows.UI.Composition` visual layer.
///
/// `DWMWA_USE_HOSTBACKDROPBRUSH` gives a `CreateHostBackdropBrush()` visual
/// access to the blurred desktop behind the window. It is Windows 11 only;
/// on Windows 10 the caller keeps the opaque tint instead.
pub fn set_host_backdrop(hwnd: HWND, enabled: bool) {
    // The accent policy would blur a rectangle, not the composition shape.
    disable_accent(hwnd);
    unsafe {
        let none = DWMSBT_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &none as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
        let value = BOOL::from(enabled);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_HOSTBACKDROPBRUSH,
            &value as *const BOOL as *const c_void,
            size_of::<BOOL>() as u32,
        );
    }
}

/// DPI-scaled compact size in physical pixels.
pub fn compact_px(dpi: u32) -> (i32, i32) {
    let scale = dpi.max(96) as f32 / 96.0;
    let size = (64.0 * scale).round() as i32;
    (size, size)
}

pub fn dip_to_px(value: f32, dpi: u32) -> i32 {
    (value * dpi as f32 / 96.0).round() as i32
}

pub fn px_to_dip(value: i32, dpi: u32) -> f32 {
    value as f32 * 96.0 / dpi.max(96) as f32
}

pub fn resize_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> WinResult<()> {
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) }
}
