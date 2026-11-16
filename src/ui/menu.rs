//! Native Win32 context menu.
//!
//! The widget used to draw its own rounded Fluent popup. Explorer's rounded
//! menu is XAML (not an `HMENU`), so a custom popup was the only way to match
//! that look. This module now uses the real system menu: `CreatePopupMenu` +
//! `TrackPopupMenu`, so the typography, dark mode, keyboard navigation,
//! accessibility and screen-edge handling are exactly Windows'.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, PostMessageW, SetForegroundWindow, TrackPopupMenu,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TPM_TOPALIGN, WM_NULL,
};

use crate::theme::Theme;

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
}

/// Flags for one `AppendMenuW` call.
fn item_flags(item: &MenuItem) -> u32 {
    if item.separator {
        return MF_SEPARATOR.0;
    }
    let mut flags = MF_STRING.0;
    if item.checked {
        flags |= MF_CHECKED.0;
    }
    if !item.enabled {
        flags |= MF_GRAYED.0;
    }
    flags
}

/// Shows a native modal context menu at `anchor` (screen coordinates).
pub fn show(owner: HWND, _theme: Theme, items: Vec<MenuItem>, anchor: POINT) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    for item in &items {
        let flags = windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS(item_flags(item));
        let result = if item.separator {
            unsafe { AppendMenuW(menu, flags, 0, PCWSTR::null()) }
        } else {
            let label = HSTRING::from(item.label.as_str());
            unsafe { AppendMenuW(menu, flags, item.id, &label) }
        };
        if result.is_err() {
            unsafe {
                let _ = DestroyMenu(menu);
            }
            return None;
        }
    }

    let command = unsafe {
        // TrackPopupMenu requires the owner to be foreground for the menu to
        // close correctly when the user clicks elsewhere.
        let _ = SetForegroundWindow(owner);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN,
            anchor.x,
            anchor.y,
            None,
            owner,
            None,
        );
        let _ = DestroyMenu(menu);
        // Classic TrackPopupMenu contract: let the owner finish the menu loop
        // and regain focus.
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        command
    };

    // With TPM_RETURNCMD the BOOL carries the selected command id.
    let id = command.0 as usize;
    (id != 0).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_flags_follow_item_state() {
        let normal = item_flags(&MenuItem::new("Abrir", 4, false));
        assert_eq!(normal, MF_STRING.0);

        let checked = item_flags(&MenuItem::new("Siempre encima", 1, true));
        assert_eq!(checked, MF_STRING.0 | MF_CHECKED.0);

        let disabled = item_flags(&MenuItem::disabled("Nada", 2, false));
        assert_eq!(disabled, MF_STRING.0 | MF_GRAYED.0);

        let separator = item_flags(&MenuItem::separator());
        assert_eq!(separator, MF_SEPARATOR.0);
    }
}
