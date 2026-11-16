//! OLE drop target for dragging files from Explorer.

use std::path::PathBuf;
use std::ptr;

use windows::core::{implement, Ref, Result as WinResult};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{
    IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{
    ReleaseStgMedium, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWLP_USERDATA};

use crate::app::App;

#[implement(windows::Win32::System::Ole::IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
}

impl DropTarget {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd }
    }
}

impl windows::Win32::System::Ole::IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let acceptable = pdataobj
            .as_ref()
            .map(|data| has_hdrop(data))
            .unwrap_or(false)
            && with_app(self.hwnd, |app| app.on_drag_enter()).unwrap_or(false);
        unsafe {
            if let Some(slot) = pdweffect.as_mut() {
                *slot = if acceptable {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                };
            }
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let acceptable = with_app(self.hwnd, |app| app.drag_hover_active()).unwrap_or(false);
        unsafe {
            if let Some(slot) = pdweffect.as_mut() {
                *slot = if acceptable {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                };
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> WinResult<()> {
        with_app(self.hwnd, |app| app.on_drag_leave());
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let mut files = Vec::new();
        if let Some(data) = pdataobj.as_ref() {
            files = extract_paths(data);
        }
        let accepted = if files.is_empty() {
            with_app(self.hwnd, |app| app.on_drag_leave());
            false
        } else {
            with_app(self.hwnd, |app| app.on_files_dropped(files)).unwrap_or(false)
        };
        unsafe {
            if let Some(slot) = pdweffect.as_mut() {
                *slot = if accepted {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                };
            }
        }
        Ok(())
    }
}

fn has_hdrop(data: &IDataObject) -> bool {
    let format = hdrop_format();
    unsafe { data.QueryGetData(&format).is_ok() }
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn extract_paths(data: &IDataObject) -> Vec<PathBuf> {
    let format = hdrop_format();
    let mut medium: STGMEDIUM = match unsafe { data.GetData(&format) } {
        Ok(medium) => medium,
        Err(_) => return Vec::new(),
    };

    let hdrop = unsafe { HDROP(medium.u.hGlobal.0) };
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { DragQueryFileW(hdrop, index, None) };
        if length == 0 {
            continue;
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let written = unsafe { DragQueryFileW(hdrop, index, Some(&mut buffer)) };
        if written > 0 {
            paths.push(PathBuf::from(crate::util::from_wide_lossy(&buffer)));
        }
    }
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    paths
}

fn with_app<R>(hwnd: HWND, f: impl FnOnce(&mut App) -> R) -> Option<R> {
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        pointer.as_mut().map(f)
    }
}
