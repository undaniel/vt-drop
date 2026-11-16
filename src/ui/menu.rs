//! Custom Windows 11-style context menu.
//!
//! USER32's `TrackPopupMenu` always paints the classic square menu. Explorer's
//! rounded Fluent menu is XAML/WinUI, not an `HMENU`, so this module implements
//! a small Win32 popup with DWM acrylic, rounded corners, Segoe UI Variable and
//! keyboard navigation. Rendering goes through DirectComposition so every frame
//! starts transparent and hover states do not leave stale pixels behind.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_RETURN, VK_SPACE,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, IsWindow, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, CS_DROPSHADOW, GWLP_USERDATA,
    HWND_TOPMOST, MSG, SWP_NOACTIVATE, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSW,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::config::BackdropMode;
use crate::theme::{Rgb, Theme};
use crate::ui::menu_render::{menu_height, MenuRenderer};
use crate::ui::window::{self, EffectiveBackdrop};

const CLASS_NAME: PCWSTR = w!("VTDropPopupMenu");
const MENU_WIDTH: f32 = 240.0;
const ITEM_HEIGHT: f32 = 32.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
const PADDING: f32 = 6.0;

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub(crate) label: String,
    pub(crate) id: usize,
    pub(crate) checked: bool,
    pub(crate) enabled: bool,
    pub(crate) separator: bool,
}

impl MenuItem {
    pub fn new(label: &str, id: usize, checked: bool) -> Self {
        Self {
            label: label.to_owned(),
            id,
            checked,
            enabled: true,
            separator: false,
        }
    }

    pub fn disabled(label: &str, id: usize, checked: bool) -> Self {
        Self {
            label: label.to_owned(),
            id,
            checked,
            enabled: false,
            separator: false,
        }
    }

    pub fn separator() -> Self {
        Self {
            label: String::new(),
            id: 0,
            checked: false,
            enabled: false,
            separator: true,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn is_separator(&self) -> bool {
        self.separator
    }

    pub fn is_checked(&self) -> bool {
        self.checked
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

struct MenuState {
    hwnd: HWND,
    items: Vec<MenuItem>,
    scale: f32,
    renderer: Option<MenuRenderer>,
    selected: Option<usize>,
    result: Option<usize>,
    done: bool,
    shown: bool,
    transparent: bool,
}

pub fn show(owner: HWND, theme: Theme, items: Vec<MenuItem>, anchor: POINT) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
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

    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let scale = dpi as f32 / 96.0;
    let height_dip = menu_height(&items);
    let width_px = (MENU_WIDTH * scale).round() as i32;
    let height_px = (height_dip * scale).round() as i32;
    let (work_left, work_top, work_right, work_bottom) = work_area(anchor);
    let mut state = Box::new(MenuState {
        hwnd: HWND::default(),
        items,
        scale,
        renderer: None,
        selected: None,
        result: None,
        done: false,
        shown: false,
        transparent: false,
    });

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
            CLASS_NAME,
            PCWSTR::null(),
            WS_POPUP,
            anchor.x,
            anchor.y,
            10,
            10,
            Some(owner),
            None,
            Some(HINSTANCE(hinstance.0)),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        return None;
    };
    state.hwnd = hwnd;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *state as *mut MenuState as isize);
    }

    let effective = window::apply_backdrop(hwnd, &theme, BackdropMode::Acrylic);
    state.transparent = effective != EffectiveBackdrop::Solid;
    let border = if theme.dark {
        Rgb::new(0x4A, 0x4A, 0x4A)
    } else {
        Rgb::new(0xD0, 0xD0, 0xD0)
    };
    window::set_border_color(hwnd, &theme, Some(border));

    let mut x = anchor.x;
    let mut y = anchor.y;
    if x + width_px > work_right - 4 {
        x = work_right - width_px - 4;
    }
    if y + height_px > work_bottom - 4 {
        y = work_bottom - height_px - 4;
    }
    x = x.clamp(
        work_left + 4,
        (work_right - width_px - 4).max(work_left + 4),
    );
    y = y.clamp(
        work_top + 4,
        (work_bottom - height_px - 4).max(work_top + 4),
    );

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            width_px,
            height_px,
            SWP_NOACTIVATE,
        );
        window::apply_window_region(hwnd, &theme, dpi, effective);
    }

    let renderer = match MenuRenderer::new(hwnd, dpi, MENU_WIDTH, height_dip, theme) {
        Ok(renderer) => renderer,
        Err(_) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return None;
        }
    };
    if theme.animations_enabled {
        renderer.animate_in();
    }
    state.renderer = Some(renderer);
    state.render();

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        let _ = SetCapture(hwnd);
    }
    state.shown = true;

    let mut message = MSG::default();
    let mut quit_requested = false;
    while !state.done {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            quit_requested = true;
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message as *const MSG);
            DispatchMessageW(&message as *const MSG);
        }
    }
    let owner_alive = unsafe { IsWindow(Some(owner)).as_bool() };
    unsafe {
        let _ = ReleaseCapture();
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = SetForegroundWindow(owner);
        // A nested popup loop must not swallow WM_QUIT intended for the main
        // message loop. Re-post it so the app can finish shutting down.
        if quit_requested || !owner_alive {
            PostQuitMessage(0);
        }
    }
    state.result.take()
}

impl MenuState {
    fn render(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            let _ = renderer.render(&self.items, self.selected, self.transparent);
        }
    }

    fn hit_test(&self, x_px: i32, y_px: i32) -> Option<usize> {
        let x = x_px as f32 / self.scale;
        let y = y_px as f32 / self.scale;
        let mut top = PADDING;
        for (index, item) in self.items.iter().enumerate() {
            let height = if item.is_separator() {
                SEPARATOR_HEIGHT
            } else {
                ITEM_HEIGHT
            };
            if !item.is_separator()
                && item.is_enabled()
                && x >= PADDING
                && x <= MENU_WIDTH - PADDING
                && y >= top
                && y < top + height
            {
                return Some(index);
            }
            top += height;
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.items.len() as isize;
        if count == 0 {
            return;
        }
        let mut position = self.selected.map(|index| index as isize).unwrap_or(-1);
        for _ in 0..count {
            position = (position + delta).rem_euclid(count);
            let index = position as usize;
            let item = &self.items[index];
            if !item.is_separator() && item.is_enabled() {
                self.selected = Some(index);
                self.render();
                return;
            }
        }
    }

    fn first_selection(&mut self) {
        self.selected = None;
        self.move_selection(1);
    }

    fn last_selection(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = Some(0);
        self.move_selection(-1);
    }

    fn finish(&mut self, result: Option<usize>) {
        if self.done {
            return;
        }
        self.done = true;
        self.result = result;
        unsafe {
            let _ = ReleaseCapture();
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
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut MenuState;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let state = unsafe { &mut *pointer };
    match message {
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            state.render();
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let x = (lparam.0 as i32 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
            let selected = state.hit_test(x, y);
            if selected != state.selected {
                state.selected = selected;
                state.render();
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 as i32 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
            let mut client = RECT::default();
            unsafe {
                let _ = GetClientRect(hwnd, &mut client);
            }
            if x < 0 || y < 0 || x >= client.right || y >= client.bottom {
                state.finish(None);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 as i32 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
            if let Some(index) = state.hit_test(x, y) {
                if let Some(item) = state.items.get(index) {
                    let id = item.id;
                    state.finish(Some(id));
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            state.finish(None);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wparam.0 as u16;
            if key == VK_ESCAPE.0 {
                state.finish(None);
            } else if key == VK_DOWN.0 {
                if state.selected.is_none() {
                    state.first_selection();
                } else {
                    state.move_selection(1);
                }
            } else if key == VK_UP.0 {
                if state.selected.is_none() {
                    state.last_selection();
                } else {
                    state.move_selection(-1);
                }
            } else if key == VK_HOME.0 {
                state.first_selection();
            } else if key == VK_END.0 {
                state.last_selection();
            } else if key == VK_RETURN.0 || key == VK_SPACE.0 {
                if let Some(index) = state.selected {
                    if let Some(item) = state.items.get(index) {
                        if item.enabled && !item.separator {
                            let id = item.id;
                            state.finish(Some(id));
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            if state.shown {
                state.finish(None);
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_DESTROY | WM_NCDESTROY => {
            state.done = true;
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn work_area(point: POINT) -> (i32, i32, i32, i32) {
    unsafe {
        let monitor = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            point,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut info = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        if windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut info).as_bool() {
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
