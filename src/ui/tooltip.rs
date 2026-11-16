//! Small custom tooltip used for error details.
//!
//! The native tooltip control keeps raw text pointers and interacts poorly with
//! a DirectComposition window that never takes focus, so this window owns its
//! text buffer and GDI resources explicitly.

use std::ptr;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, FrameRect, GetDC, GetMonitorInfoW, MonitorFromPoint, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, SetWindowRgn, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_WORDBREAK, FW_NORMAL,
    HBRUSH, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    GetWindowRect, RegisterClassW, SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_DROPSHADOW,
    GWLP_USERDATA, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::theme::{Rgb, Theme};
use crate::ui::window;

const CLASS_NAME: PCWSTR = w!("VTDropTooltip");
const MAX_WIDTH_DIP: f32 = 360.0;
const PADDING_DIP: f32 = 12.0;

pub struct Tooltip {
    hwnd: HWND,
    owner: HWND,
    theme: Theme,
    text: Vec<u16>,
    bg: HBRUSH,
    border: HBRUSH,
    font: HFONT,
    fg: COLORREF,
}

impl Tooltip {
    pub fn new(owner: HWND, theme: &Theme) -> Option<Self> {
        let hinstance = unsafe { GetModuleHandleW(None).ok() }?;
        let class = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                CLASS_NAME,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                100,
                40,
                Some(owner),
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )
        }
        .ok()?;
        let mut tooltip = Self {
            hwnd,
            owner,
            theme: *theme,
            text: Vec::new(),
            bg: HBRUSH(ptr::null_mut()),
            border: HBRUSH(ptr::null_mut()),
            font: HFONT(ptr::null_mut()),
            fg: COLORREF(0),
        };
        tooltip.apply_theme(theme);
        Some(tooltip)
    }

    pub fn attach(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self as *mut Tooltip as isize);
        }
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = *theme;
        self.apply_theme(theme);
    }

    pub fn show(&mut self, text: &str) {
        if text.is_empty() {
            self.hide();
            return;
        }
        self.text = text.encode_utf16().collect();
        let dpi = unsafe { GetDpiForWindow(self.owner) }.max(96);
        let scale = dpi as f32 / 96.0;
        let padding = (PADDING_DIP * scale).round() as i32;
        let max_width = (MAX_WIDTH_DIP * scale).round() as i32;

        let mut measured = RECT {
            left: 0,
            top: 0,
            right: max_width,
            bottom: 0,
        };
        let hdc = unsafe { GetDC(Some(self.hwnd)) };
        if hdc.is_invalid() {
            return;
        }
        unsafe {
            let old = SelectObject(hdc, HGDIOBJ(self.font.0));
            let mut buffer = self.text.clone();
            DrawTextW(
                hdc,
                &mut buffer,
                &mut measured,
                DT_CALCRECT | DT_WORDBREAK | DT_LEFT | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(Some(self.hwnd), hdc);
        }
        let width = (measured.right - measured.left + padding * 2).max(padding * 2 + 20);
        let height = (measured.bottom - measured.top + padding * 2).max(padding * 2 + 16);

        let mut anchor = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.owner, &mut anchor);
        }
        let (work_left, work_top, work_right, work_bottom) = work_area(anchor);
        let gap = (8.0 * scale).round() as i32;
        let mut x = anchor.right - width;
        let mut y = anchor.top - height - gap;
        if y < work_top + 4 {
            y = anchor.bottom + gap;
        }
        x = x.clamp(work_left + 4, (work_right - width - 4).max(work_left + 4));
        y = y.clamp(work_top + 4, (work_bottom - height - 4).max(work_top + 4));

        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
        self.apply_shape(width, height, dpi);
        unsafe {
            let visible = windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.hwnd);
            if !visible.as_bool() && self.theme.animations_enabled {
                // Native tooltips fade in; AW_BLEND never activates the window.
                let _ = windows::Win32::UI::WindowsAndMessaging::AnimateWindow(
                    self.hwnd,
                    120,
                    windows::Win32::UI::WindowsAndMessaging::AW_BLEND,
                );
            } else {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn apply_theme(&mut self, theme: &Theme) {
        unsafe {
            if !self.bg.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.bg.0));
            }
            if !self.border.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.border.0));
            }
            if !self.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            let bg = if theme.dark {
                Rgb::new(0x20, 0x20, 0x20)
            } else {
                Rgb::new(0xF3, 0xF3, 0xF3)
            };
            let border = if theme.dark {
                Rgb::new(0x50, 0x50, 0x50)
            } else {
                Rgb::new(0xC8, 0xC8, 0xC8)
            };
            self.bg = CreateSolidBrush(COLORREF(bg.to_colorref()));
            self.border = CreateSolidBrush(COLORREF(border.to_colorref()));
            self.fg = COLORREF(theme.foreground().to_colorref());
            let dpi = GetDpiForWindow(self.owner).max(96) as f32 / 96.0;
            self.font = CreateFontW(
                -((11.5 * dpi).round() as i32),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Segoe UI Variable Text"),
            );
            window::set_dark_mode(self.hwnd, theme.dark);
            window::set_rounded_corners(self.hwnd, theme);
            window::set_border_color(self.hwnd, theme, None);
        }
    }

    fn apply_shape(&self, width: i32, height: i32, dpi: u32) {
        if self.theme.is_win11() {
            return;
        }
        let radius = (10.0 * dpi as f32 / 96.0).round() as i32;
        let region =
            unsafe { CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2) };
        if !region.is_invalid() {
            unsafe {
                let _ = SetWindowRgn(self.hwnd, Some(region), true);
            }
        }
    }
}

impl Drop for Tooltip {
    fn drop(&mut self) {
        unsafe {
            if !self.bg.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.bg.0));
            }
            if !self.border.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.border.0));
            }
            if !self.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Tooltip;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let tooltip = unsafe { &mut *pointer };
    match message {
        windows::Win32::UI::WindowsAndMessaging::WM_PAINT => {
            unsafe {
                let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut paint);
                draw(hwnd, hdc, tooltip);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        // AnimateWindow captures the content through WM_PRINTCLIENT.
        windows::Win32::UI::WindowsAndMessaging::WM_PRINTCLIENT => {
            unsafe {
                draw(
                    hwnd,
                    windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut _),
                    tooltip,
                )
            };
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND => LRESULT(1),
        windows::Win32::UI::WindowsAndMessaging::WM_NCHITTEST => {
            LRESULT(windows::Win32::UI::WindowsAndMessaging::HTTRANSPARENT as isize)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn work_area(anchor: RECT) -> (i32, i32, i32, i32) {
    unsafe {
        let monitor = MonitorFromPoint(
            windows::Win32::Foundation::POINT {
                x: anchor.left,
                y: anchor.top,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            (
                info.rcWork.left,
                info.rcWork.top,
                info.rcWork.right,
                info.rcWork.bottom,
            )
        } else {
            (0, 0, 1920, 1080)
        }
    }
}

unsafe fn draw(hwnd: HWND, hdc: windows::Win32::Graphics::Gdi::HDC, tooltip: &Tooltip) {
    unsafe {
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        FillRect(hdc, &client, tooltip.bg);
        FrameRect(hdc, &client, tooltip.border);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, tooltip.fg);
        let old = SelectObject(hdc, HGDIOBJ(tooltip.font.0));
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let inset = (PADDING_DIP * 0.75 * scale).round() as i32;
        let mut rect = RECT {
            left: inset,
            top: inset,
            right: client.right - inset,
            bottom: client.bottom - inset,
        };
        let mut buffer = tooltip.text.clone();
        DrawTextW(
            hdc,
            &mut buffer,
            &mut rect,
            DT_WORDBREAK | DT_LEFT | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    }
}
