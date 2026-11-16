//! Native API key dialog.
//!
//! The old version was a custom DirectComposition popup that imitated the
//! Windows 11 Fluent dialog. It is now a real Win32 dialog resource with
//! native STATIC / EDIT / BUTTON controls: the system handles the typography,
//! tab order, clipboard, caret, screen reader and keyboard behaviour.
//!
//! Resource `101` lives in `assets/vtdrop.rc` (and `vtdrop-gnu.rc`).

use std::ffi::c_void;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, InvalidateRect, SetBkColor, SetTextColor, HBRUSH,
    HDC, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    IsDlgButtonChecked, SetWindowTheme, BST_CHECKED, EM_GETPASSWORDCHAR, EM_SETLIMITTEXT,
    EM_SETPASSWORDCHAR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    DialogBoxParamW, EndDialog, GetClientRect, GetDlgItem, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, MessageBoxW, SendDlgItemMessageW, SendMessageW,
    SetDlgItemTextW, SetWindowLongPtrW, SetWindowPos, BN_CLICKED, GWLP_USERDATA, MB_ICONWARNING,
    MB_OK, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WM_CLOSE, WM_COMMAND, WM_CTLCOLORBTN,
    WM_CTLCOLORDLG, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_ERASEBKGND, WM_INITDIALOG,
    WM_THEMECHANGED,
};
use zeroize::{Zeroize, Zeroizing};

use super::{is_valid_api_key, save_api_key};
use crate::theme::Theme;
use crate::ui::window;

const DLG_ID: usize = 101;
const ID_TITLE: i32 = 1001;
const ID_DESC: i32 = 1002;
const ID_KEY: i32 = 1003;
const ID_SHOW: i32 = 1004;
const ID_SAVE: i32 = 1;
const ID_CANCEL: i32 = 2;
const MAX_KEY_CHARS: usize = 64;

struct KeyDialogState {
    owner: HWND,
    existing: bool,
    dark: bool,
    background: HBRUSH,
    background_color: COLORREF,
    foreground_color: COLORREF,
    password_char: usize,
    result: Option<Zeroizing<String>>,
}

/// Shows the modal key dialog. `Some(key)` means a new key was saved; `None`
/// means the user cancelled (or kept the current key by leaving the field
/// empty).
pub fn show(owner: HWND, theme: &Theme, existing: bool) -> Option<Zeroizing<String>> {
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let background_color = COLORREF(theme.solid_background().to_colorref());
    let foreground_color = COLORREF(theme.foreground().to_colorref());
    let background = unsafe { CreateSolidBrush(background_color) };

    let mut state = KeyDialogState {
        owner,
        existing,
        dark: theme.dark,
        background,
        background_color,
        foreground_color,
        password_char: 0,
        result: None,
    };

    let result = unsafe {
        DialogBoxParamW(
            Some(HINSTANCE(module.0)),
            PCWSTR(DLG_ID as *const u16),
            Some(owner),
            Some(dialog_proc),
            LPARAM(&mut state as *mut KeyDialogState as isize),
        )
    };

    unsafe {
        let _ = DeleteObject(HGDIOBJ(background.0));
    }

    if result == -1 {
        return None;
    }
    state.result.take()
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    let state = if message == WM_INITDIALOG {
        let pointer = lparam.0 as *mut KeyDialogState;
        if pointer.is_null() {
            let _ = unsafe { EndDialog(hwnd, 0) };
            return 1;
        }
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
        }
        unsafe { &mut *pointer }
    } else {
        let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut KeyDialogState;
        if pointer.is_null() {
            return 0;
        }
        unsafe { &mut *pointer }
    };

    match message {
        WM_INITDIALOG => {
            init_dialog(hwnd, state);
            0
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if id == ID_SHOW && notification == BN_CLICKED {
                toggle_reveal(hwnd, state);
                1
            } else if id == ID_SAVE {
                save_key(hwnd, state);
                1
            } else if id == ID_CANCEL {
                let _ = unsafe { EndDialog(hwnd, 0) };
                1
            } else {
                0
            }
        }
        WM_CTLCOLORDLG => state.background.0 as isize,
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN => {
            let hdc = HDC(wparam.0 as *mut c_void);
            unsafe {
                SetBkColor(hdc, state.background_color);
                SetTextColor(hdc, state.foreground_color);
            }
            state.background.0 as isize
        }
        WM_ERASEBKGND => {
            let hdc = HDC(wparam.0 as *mut c_void);
            let mut rect = RECT::default();
            if unsafe { GetClientRect(hwnd, &mut rect) }.is_ok() {
                unsafe {
                    FillRect(hdc, &rect, state.background);
                }
            }
            1
        }
        WM_THEMECHANGED => {
            apply_dark_controls(hwnd, state);
            0
        }
        WM_CLOSE => {
            let _ = unsafe { EndDialog(hwnd, 0) };
            1
        }
        _ => 0,
    }
}

fn init_dialog(hwnd: HWND, state: &mut KeyDialogState) {
    window::set_dark_mode(hwnd, state.dark);
    window::allow_dark_mode(hwnd, state.dark);
    center_on_owner(hwnd, state.owner);

    let description = if state.existing {
        "Pega una nueva clave. Se guarda cifrada con DPAPI en %APPDATA%\\VTDrop. Si dejas el campo vacio, se conserva la actual."
    } else {
        "Pega tu clave de API de VirusTotal. Se guarda cifrada con DPAPI en %APPDATA%\\VTDrop."
    };
    let description = HSTRING::from(description);
    unsafe {
        let _ = SetDlgItemTextW(hwnd, ID_DESC, &description);
    }

    apply_dark_controls(hwnd, state);

    if let Ok(edit) = unsafe { GetDlgItem(Some(hwnd), ID_KEY) } {
        unsafe {
            let _ = SendMessageW(
                edit,
                EM_SETLIMITTEXT,
                Some(WPARAM(MAX_KEY_CHARS)),
                Some(LPARAM(0)),
            );
            let default_char = SendMessageW(edit, EM_GETPASSWORDCHAR, None, None).0 as usize;
            state.password_char = if default_char == 0 {
                0x25CF
            } else {
                default_char
            };
            let _ = SetFocus(Some(edit));
        }
    }
}

fn apply_dark_controls(hwnd: HWND, state: &KeyDialogState) {
    unsafe {
        for id in [ID_TITLE, ID_DESC, ID_KEY, ID_SHOW, ID_SAVE, ID_CANCEL] {
            if let Ok(control) = GetDlgItem(Some(hwnd), id) {
                window::allow_dark_mode(control, state.dark);
            }
        }
        if state.dark {
            if let Ok(edit) = GetDlgItem(Some(hwnd), ID_KEY) {
                // EDIT controls use their own dark theme name.
                let _ = SetWindowTheme(edit, w!("DarkMode_CFD"), PCWSTR::null());
            }
        }
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

fn toggle_reveal(hwnd: HWND, state: &KeyDialogState) {
    let checked = unsafe { IsDlgButtonChecked(hwnd, ID_SHOW) } == BST_CHECKED.0;
    let mask = if checked { 0 } else { state.password_char };
    unsafe {
        let _ = SendDlgItemMessageW(hwnd, ID_KEY, EM_SETPASSWORDCHAR, WPARAM(mask), LPARAM(0));
        if let Ok(edit) = GetDlgItem(Some(hwnd), ID_KEY) {
            let _ = InvalidateRect(Some(edit), None, true);
        }
    }
}

fn save_key(hwnd: HWND, state: &mut KeyDialogState) {
    let Some(raw) = read_key(hwnd) else {
        return;
    };
    let trimmed = Zeroizing::new(raw.trim().to_owned());

    if trimmed.is_empty() {
        if state.existing {
            let _ = unsafe { EndDialog(hwnd, 1) };
        } else {
            warn(hwnd, "Introduce una API key.");
        }
        return;
    }
    if !is_valid_api_key(trimmed.as_str()) {
        warn(hwnd, "La API key debe tener 64 caracteres hexadecimales.");
        return;
    }
    match save_api_key(trimmed.as_str()) {
        Ok(()) => {
            state.result = Some(Zeroizing::new(trimmed.as_str().to_owned()));
            let _ = unsafe { EndDialog(hwnd, 1) };
        }
        Err(error) => warn(hwnd, &format!("No se pudo guardar la clave: {error}")),
    }
}

fn read_key(hwnd: HWND) -> Option<Zeroizing<String>> {
    let edit = unsafe { GetDlgItem(Some(hwnd), ID_KEY) }.ok()?;
    let length = unsafe { GetWindowTextLengthW(edit) }.max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    let copied = unsafe { GetWindowTextW(edit, &mut buffer) }.max(0) as usize;
    let text = String::from_utf16_lossy(&buffer[..copied]);
    buffer.zeroize();
    Some(Zeroizing::new(text))
}

fn warn(hwnd: HWND, message: &str) {
    let message = HSTRING::from(message);
    unsafe {
        let _ = MessageBoxW(Some(hwnd), &message, w!("VTDrop"), MB_OK | MB_ICONWARNING);
    }
}

fn center_on_owner(hwnd: HWND, owner: HWND) {
    let mut owner_rect = RECT::default();
    let mut dialog_rect = RECT::default();
    unsafe {
        if GetWindowRect(owner, &mut owner_rect).is_err()
            || GetWindowRect(hwnd, &mut dialog_rect).is_err()
        {
            return;
        }
        let width = dialog_rect.right - dialog_rect.left;
        let height = dialog_rect.bottom - dialog_rect.top;
        let x = owner_rect.left + ((owner_rect.right - owner_rect.left) - width) / 2;
        let y = owner_rect.top + ((owner_rect.bottom - owner_rect.top) - height) / 2;
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows::Win32::System::LibraryLoader::{
        FindResourceW, LoadLibraryExW, LoadResource, LockResource, LOAD_LIBRARY_AS_DATAFILE,
        LOAD_LIBRARY_AS_IMAGE_RESOURCE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateDialogIndirectParamW, DestroyWindow, DLGTEMPLATE, RT_DIALOG,
    };

    /// The dialog lives in the application's resource section, not in the test
    /// harness. Load the already-built vtdrop.exe as a data file.
    fn app_module() -> HMODULE {
        let exe = crate::app_exe_path().expect("run `cargo build` before `cargo test`");
        unsafe {
            LoadLibraryExW(
                &HSTRING::from(exe.to_string_lossy().as_ref()),
                None,
                LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
            )
        }
        .expect("load vtdrop.exe as data")
    }

    #[test]
    fn native_dialog_resource_is_linked() {
        let module = app_module();
        let resource =
            unsafe { FindResourceW(Some(module), PCWSTR(DLG_ID as *const u16), RT_DIALOG) };
        unsafe {
            let _ = FreeLibrary(module);
        }
        assert!(!resource.0.is_null(), "dialog resource 101 is missing");
    }

    #[test]
    fn native_dialog_template_is_valid() {
        // Creates the native dialog from the embedded template without showing
        // it. This catches malformed resources and a broken WM_INITDIALOG.
        let module = app_module();
        let resource =
            unsafe { FindResourceW(Some(module), PCWSTR(DLG_ID as *const u16), RT_DIALOG) };
        assert!(!resource.0.is_null(), "dialog resource 101 is missing");
        let loaded = unsafe { LoadResource(Some(module), resource) }.expect("load resource");
        let pointer = unsafe { LockResource(loaded) };
        assert!(!pointer.is_null());

        let mut state = KeyDialogState {
            owner: HWND::default(),
            existing: false,
            dark: true,
            background: HBRUSH::default(),
            background_color: COLORREF(0),
            foreground_color: COLORREF(0),
            password_char: 0,
            result: None,
        };
        let dialog = unsafe {
            CreateDialogIndirectParamW(
                Some(HINSTANCE(module.0)),
                pointer as *const DLGTEMPLATE,
                None,
                Some(dialog_proc),
                LPARAM(&mut state as *mut KeyDialogState as isize),
            )
        }
        .expect("create dialog");
        assert!(!dialog.is_invalid());
        unsafe {
            let _ = DestroyWindow(dialog);
            let _ = FreeLibrary(module);
        }
    }
}
