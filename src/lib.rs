#![allow(clippy::too_many_arguments)]

pub mod app;
pub mod config;
pub mod drop_target;
pub mod hash;
pub mod model;
pub mod theme;
pub mod toast;
pub mod ui;
pub mod util;
pub mod vt;
pub mod worker;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use windows::core::{w, Result as WinResult, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW, GetWindowLongPtrW,
    LoadCursorW, LoadIconW, MessageBoxW, PostMessageW, RegisterClassW, SetWindowLongPtrW,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, MB_ICONERROR, MB_OK, MSG,
    WM_NCDESTROY, WNDCLASSW, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::app::{App, WM_APP_SHOW};
use crate::config::Config;
use crate::theme::Theme;
use crate::worker::WorkerHandle;

const CLASS_NAME: windows::core::PCWSTR = w!("VTDropWidget");
const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\VTDrop-7F1E2B9C-SingleInstance");

/// Location of the built application binary, for tests that need its resource
/// section (the test harness itself only carries the icon-less default
/// manifest, so resources must be read from the real `.exe`).
#[cfg(test)]
pub(crate) fn app_exe_path() -> Option<std::path::PathBuf> {
    let mut dir = std::env::current_exe().ok()?;
    loop {
        dir = dir.parent()?.to_path_buf();
        if dir.file_name().is_some_and(|name| name == "build") {
            // target/<profile>/build -> target/<profile>/vtdrop.exe
            let candidate = dir.parent()?.join("vtdrop.exe");
            return candidate.exists().then_some(candidate);
        }
    }
}

pub fn run() -> WinResult<()> {
    // Must happen before the first window: comctl32 v6 decides how every
    // native control, menu and scrollbar is drawn.
    crate::ui::styles::enable_visual_styles();
    unsafe {
        // Safe no-op when the MSVC manifest already set PerMonitorV2.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let mutex = CreateMutexW(None, true, MUTEX_NAME)?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            if let Ok(existing) = FindWindowW(CLASS_NAME, None) {
                if !existing.is_invalid() {
                    let _ = PostMessageW(Some(existing), WM_APP_SHOW, WPARAM(0), LPARAM(0));
                }
            }
            return Ok(());
        }

        OleInitialize(None)?;
        crate::toast::init();

        let result = run_inner();
        OleUninitialize();
        let _ = CloseHandle(mutex);
        result
    }
}

fn run_inner() -> WinResult<()> {
    let config = Config::load();
    let theme = Theme::detect();
    // uxtheme only honours the dark menu preference before the first window.
    crate::theme::apply_native_menu_theme(theme.dark);
    let events = Arc::new(Mutex::new(VecDeque::new()));

    let hinstance = unsafe { GetModuleHandleW(None)? };
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW)? };
    let icon = unsafe { LoadIconW(Some(HINSTANCE(hinstance.0)), PCWSTR(1 as *const u16)) }.ok();

    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(hinstance.0),
        hIcon: icon.unwrap_or_default(),
        hCursor: cursor,
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_thread());
    }

    // No redirection bitmap: the composition visual tree is the only content,
    // so the window is transparent outside the circle/capsule.
    let mut ex_style = WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP;
    if config.always_on_top {
        ex_style |= WS_EX_TOPMOST;
    }

    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            CLASS_NAME,
            w!("VTDrop"),
            WS_POPUP,
            0,
            0,
            64,
            64,
            None,
            None,
            Some(HINSTANCE(hinstance.0)),
            None,
        )?
    };

    let api_key = config::load_api_key().ok().flatten();
    let worker = WorkerHandle::spawn(hwnd, events.clone(), api_key, config.clone());
    let mut app = Box::new(App::new(hwnd, config, theme, events));
    app.set_worker(worker);
    let pointer = Box::into_raw(app);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
        if let Err(error) = (&mut *pointer).initialize() {
            let message = HSTRING::from(format!(
                "No se pudo inicializar DirectComposition/Direct2D.\n\n{error}"
            ));
            MessageBoxW(None, &message, w!("VTDrop"), MB_OK | MB_ICONERROR);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            drop(Box::from_raw(pointer));
            return Err(error);
        }
        // `vtdrop.exe <files…>` scans them right away ("Abrir con", scripts).
        // ponytail: only on a fresh start; forward via WM_COPYDATA to a running
        // instance if "Enviar a" support is wanted.
        let files: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
        if !files.is_empty() {
            (&mut *pointer).on_files_dropped(files);
        }
    }

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message as *const MSG);
            DispatchMessageW(&message as *const MSG);
        }
    }

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(pointer));
    }
    Ok(())
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if !pointer.is_null() {
        let app = unsafe { &mut *pointer };
        if let Some(result) = app.handle_message(message, wparam, lparam) {
            if message == WM_NCDESTROY {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
            }
            return result;
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
