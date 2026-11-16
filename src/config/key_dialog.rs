//! First-run / reconfigure dialog for the VirusTotal API key.
//!
//! The dialog is custom-drawn with DirectComposition + Direct2D instead of
//! native EDIT/BUTTON children, so it can use the Windows 11 Fluent look:
//! acrylic surface, Segoe UI Variable, rounded text field, accent primary
//! button and translucent hover states. The key is masked, validated locally
//! and stored with DPAPI before the dialog closes.

use std::mem::size_of;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, VK_A,
    VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_SPACE, VK_TAB, VK_UP, VK_V,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, IsWindow, KillTimer, LoadCursorW, PostQuitMessage, RegisterClassW,
    SendMessageW, SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, CS_DROPSHADOW, GWLP_USERDATA, HTCAPTION, IDC_ARROW, IDC_HAND, IDC_IBEAM, MSG,
    SW_SHOW, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_NCLBUTTONDOWN,
    WM_PAINT, WM_SETFOCUS, WM_TIMER, WNDCLASSW, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

use super::{is_valid_api_key, save_api_key};
use crate::config::BackdropMode;
use crate::theme::{Rgb, Theme};
use crate::ui::dialog_render::{control_at, Control, DialogRenderer, DialogView, HEIGHT, WIDTH};
use crate::ui::window::{self, EffectiveBackdrop};
use zeroize::Zeroizing;

const CLASS_NAME: PCWSTR = w!("VTDropKeyDialog");
const TIMER_CARET: usize = 1;
const TITLE_DRAG_HEIGHT: f32 = 48.0;
const MAX_KEY_CHARS: usize = 128;

struct KeyDialog {
    hwnd: HWND,
    renderer: Option<DialogRenderer>,
    scale: f32,
    existing: bool,
    key: Zeroizing<String>,
    cursor: usize,
    select_all: bool,
    focus: Control,
    hovered: Option<Control>,
    pressed: Option<Control>,
    error: String,
    caret_on: bool,
    tracking_leave: bool,
    arrow: Option<windows::Win32::UI::WindowsAndMessaging::HCURSOR>,
    hand: Option<windows::Win32::UI::WindowsAndMessaging::HCURSOR>,
    ibeam: Option<windows::Win32::UI::WindowsAndMessaging::HCURSOR>,
    transparent: bool,
    done: bool,
    result: Option<Zeroizing<String>>,
}

pub fn show(owner: HWND, theme: &Theme, existing: bool) -> Option<Zeroizing<String>> {
    let hinstance = unsafe { GetModuleHandleW(None).ok() }?;
    let class = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(dialog_proc),
        hInstance: HINSTANCE(hinstance.0),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&class);
    }

    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let scale = dpi as f32 / 96.0;
    let width = (WIDTH * scale).round() as i32;
    let height = (HEIGHT * scale).round() as i32;
    let (x, y) = centered_on(owner, width, height);

    let mut state = Box::new(KeyDialog {
        hwnd: HWND::default(),
        renderer: None,
        scale,
        existing,
        key: Zeroizing::new(String::new()),
        cursor: 0,
        select_all: false,
        focus: Control::Input,
        hovered: None,
        pressed: None,
        error: String::new(),
        caret_on: true,
        tracking_leave: false,
        arrow: unsafe { LoadCursorW(None, IDC_ARROW) }.ok(),
        hand: unsafe { LoadCursorW(None, IDC_HAND) }.ok(),
        ibeam: unsafe { LoadCursorW(None, IDC_IBEAM) }.ok(),
        transparent: false,
        done: false,
        result: None,
    });
    let state_ptr = &mut *state as *mut KeyDialog;

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
            CLASS_NAME,
            w!("VTDrop — API key"),
            WS_POPUP,
            x,
            y,
            width,
            height,
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
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    let effective = window::apply_backdrop(hwnd, theme, BackdropMode::Acrylic);
    state.transparent = effective != EffectiveBackdrop::Solid;
    let border = if theme.dark {
        Rgb::new(0x4A, 0x4A, 0x4A)
    } else {
        Rgb::new(0xD0, 0xD0, 0xD0)
    };
    window::set_border_color(hwnd, theme, Some(border));
    window::apply_window_region(hwnd, theme, dpi, effective);

    let renderer = match DialogRenderer::new(hwnd, dpi, *theme) {
        Ok(renderer) => renderer,
        Err(_) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return None;
        }
    };
    state.renderer = Some(renderer);
    state.render();

    unsafe {
        let _ = EnableWindow(owner, false);
        let _ = SetTimer(Some(hwnd), TIMER_CARET, 530, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }

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

    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_CARET);
        let _ = EnableWindow(owner, true);
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = SetForegroundWindow(owner);
        if quit_requested || !IsWindow(Some(owner)).as_bool() {
            PostQuitMessage(0);
        }
    }
    state.result.take()
}

impl KeyDialog {
    fn render(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let title = "API key de VirusTotal";
        let description = if self.existing {
            "Pega una nueva clave para reemplazar la guardada. Se cifra con DPAPI en %APPDATA%\\VTDrop."
        } else {
            "Pega tu clave de API de VirusTotal. Se cifra con DPAPI en %APPDATA%\\VTDrop y nunca se escribe en logs."
        };
        let view = DialogView {
            title,
            description,
            existing: self.existing,
            key: &self.key,
            cursor: self.cursor,
            select_all: self.select_all,
            focus: self.focus,
            hovered: self.hovered,
            pressed: self.pressed,
            error: &self.error,
            caret_on: self.caret_on,
        };
        let _ = renderer.render(&view, self.transparent);
    }

    fn set_focus(&mut self, control: Control) {
        self.focus = control;
        self.caret_on = true;
        self.render();
    }

    fn cycle_focus(&mut self, backwards: bool) {
        const ORDER: [Control; 3] = [Control::Input, Control::Cancel, Control::Save];
        let index = ORDER
            .iter()
            .position(|control| *control == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            (index + ORDER.len() - 1) % ORDER.len()
        } else {
            (index + 1) % ORDER.len()
        };
        self.set_focus(ORDER[next]);
    }

    fn insert_text(&mut self, text: &str) {
        let filtered: Vec<char> = text
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if filtered.is_empty() {
            return;
        }
        if self.select_all {
            self.key.clear();
            self.cursor = 0;
            self.select_all = false;
        }
        for value in filtered {
            if self.key.len() >= MAX_KEY_CHARS {
                break;
            }
            self.key.insert(self.cursor, value);
            self.cursor += 1;
        }
        self.error.clear();
        self.caret_on = true;
        self.render();
    }

    fn backspace(&mut self) {
        if self.select_all {
            self.key.clear();
            self.cursor = 0;
            self.select_all = false;
        } else if self.cursor > 0 {
            self.cursor -= 1;
            self.key.remove(self.cursor);
        }
        self.error.clear();
        self.caret_on = true;
        self.render();
    }

    fn delete(&mut self) {
        if self.select_all {
            self.key.clear();
            self.cursor = 0;
            self.select_all = false;
        } else if self.cursor < self.key.len() {
            self.key.remove(self.cursor);
        }
        self.error.clear();
        self.caret_on = true;
        self.render();
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.select_all {
            self.select_all = false;
            if delta > 0 {
                self.cursor = 0;
            } else {
                self.cursor = self.key.len();
            }
            self.caret_on = true;
            self.render();
            return;
        }
        let target = self.cursor as isize + delta;
        self.cursor = target.clamp(0, self.key.len() as isize) as usize;
        self.caret_on = true;
        self.render();
    }

    fn paste(&mut self) {
        if let Some(text) = clipboard_text(self.hwnd) {
            self.insert_text(&text);
        }
    }

    fn activate(&mut self, control: Control) {
        match control {
            Control::Cancel => self.cancel(),
            Control::Save => self.save_key(),
            Control::Input => self.set_focus(Control::Input),
        }
    }

    fn cancel(&mut self) {
        self.done = true;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    fn save_key(&mut self) {
        if self.key.is_empty() {
            if self.existing {
                self.done = true;
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
            } else {
                self.show_error("Introduce una API key.");
            }
            return;
        }
        if !is_valid_api_key(&self.key) {
            self.show_error("La API key debe tener 64 caracteres hexadecimales.");
            return;
        }
        match save_api_key(&self.key) {
            Ok(()) => {
                self.result = Some(Zeroizing::new(self.key.to_string()));
                self.done = true;
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
            }
            Err(error) => self.show_error(&format!("No se pudo guardar la clave: {error}")),
        }
    }

    fn show_error(&mut self, message: &str) {
        self.error = message.to_owned();
        self.focus = Control::Input;
        self.caret_on = true;
        self.render();
    }

    fn handle(&mut self, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match message {
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ValidateRect(Some(self.hwnd), None);
                }
                self.render();
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = mouse_dip(lparam, self.scale);
                let hovered = control_at(x, y);
                let cursor = match hovered {
                    Some(Control::Input) => self.ibeam,
                    Some(_) => self.hand,
                    None => self.arrow,
                };
                unsafe {
                    let _ = SetCursor(cursor);
                }
                if !self.tracking_leave {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    if unsafe { TrackMouseEvent(&mut track) }.is_ok() {
                        self.tracking_leave = true;
                    }
                }
                if hovered != self.hovered {
                    self.hovered = hovered;
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking_leave = false;
                if self.hovered.is_some() {
                    self.hovered = None;
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = mouse_dip(lparam, self.scale);
                if y < TITLE_DRAG_HEIGHT && control_at(x, y).is_none() {
                    unsafe {
                        let _ = ReleaseCapture();
                        let _ = SendMessageW(
                            self.hwnd,
                            WM_NCLBUTTONDOWN,
                            Some(WPARAM(HTCAPTION as usize)),
                            Some(LPARAM(0)),
                        );
                    }
                    return Some(LRESULT(0));
                }
                if let Some(control) = control_at(x, y) {
                    self.focus = control;
                    self.pressed = Some(control);
                    if control == Control::Input {
                        if let Some(renderer) = self.renderer.as_ref() {
                            self.cursor = renderer.caret_from_point(&self.key, x);
                        }
                        self.select_all = false;
                    } else {
                        unsafe {
                            SetCapture(self.hwnd);
                        }
                    }
                    self.caret_on = true;
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let pressed = self.pressed.take();
                unsafe {
                    let _ = ReleaseCapture();
                }
                if let Some(control) = pressed {
                    if self.hovered == Some(control) {
                        self.activate(control);
                    } else {
                        self.render();
                    }
                }
                Some(LRESULT(0))
            }
            WM_CAPTURECHANGED => {
                if self.pressed.take().is_some() {
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_CHAR => {
                if self.focus == Control::Input {
                    let character = wparam.0 as u32;
                    if character == 0x08 {
                        self.backspace();
                    } else if let Some(value) = char::from_u32(character) {
                        if value.is_ascii_hexdigit() {
                            self.insert_text(&value.to_string());
                        }
                    }
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let key = wparam.0 as u16;
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                if key == VK_TAB.0 {
                    self.cycle_focus(shift);
                } else if key == VK_ESCAPE.0 {
                    self.cancel();
                } else if key == VK_RETURN.0 {
                    if self.focus == Control::Cancel {
                        self.cancel();
                    } else {
                        self.save_key();
                    }
                } else if key == VK_SPACE.0 {
                    if self.focus != Control::Input {
                        self.activate(self.focus);
                    }
                } else if self.focus == Control::Input {
                    if control && key == VK_A.0 {
                        self.select_all = true;
                        self.cursor = self.key.len();
                        self.caret_on = true;
                        self.render();
                    } else if control && key == VK_V.0 {
                        self.paste();
                    } else if key == VK_LEFT.0 {
                        self.move_cursor(-1);
                    } else if key == VK_RIGHT.0 {
                        self.move_cursor(1);
                    } else if key == VK_HOME.0 {
                        self.select_all = false;
                        self.cursor = 0;
                        self.caret_on = true;
                        self.render();
                    } else if key == VK_END.0 {
                        self.select_all = false;
                        self.cursor = self.key.len();
                        self.caret_on = true;
                        self.render();
                    } else if key == VK_DELETE.0 {
                        self.delete();
                    } else if key == VK_UP.0 || key == VK_DOWN.0 {
                        // Single-line field: ignore vertical navigation.
                    }
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                if self.focus == Control::Input && !self.select_all {
                    self.caret_on = !self.caret_on;
                    self.render();
                }
                Some(LRESULT(0))
            }
            WM_SETFOCUS => {
                self.caret_on = true;
                self.render();
                Some(LRESULT(0))
            }
            WM_KILLFOCUS => {
                self.caret_on = false;
                self.render();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.cancel();
                Some(LRESULT(0))
            }
            WM_DESTROY | WM_NCDESTROY => {
                self.done = true;
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

fn mouse_dip(lparam: LPARAM, scale: f32) -> (f32, f32) {
    let x = (lparam.0 as i32 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
    (x as f32 / scale, y as f32 / scale)
}

fn centered_on(owner: HWND, width: i32, height: i32) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(owner, &mut rect);
    }
    let x = rect.left + ((rect.right - rect.left) - width) / 2;
    let y = rect.top + ((rect.bottom - rect.top) - height) / 2;
    clamp_to_work_area(x, y, width, height)
}

fn clamp_to_work_area(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: x + width / 2,
                y: y + height / 2,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return (x, y);
    }
    let margin = 12;
    (
        x.clamp(
            info.rcWork.left + margin,
            (info.rcWork.right - width - margin).max(info.rcWork.left + margin),
        ),
        y.clamp(
            info.rcWork.top + margin,
            (info.rcWork.bottom - height - margin).max(info.rcWork.top + margin),
        ),
    )
}

fn clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return None;
        }
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let memory = HGLOBAL(handle.0);
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                return None;
            }
            let wide = pointer as *const u16;
            let mut len = 0usize;
            while len < 1_000_000 && *wide.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
            let _ = GlobalUnlock(memory);
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut KeyDialog;
    if !pointer.is_null() {
        if let Some(result) = unsafe { (&mut *pointer).handle(message, wparam, lparam) } {
            return result;
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
