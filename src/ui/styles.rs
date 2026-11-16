//! Common Controls v6 activation for the MinGW build.
//!
//! Native `BUTTON` / `EDIT` / `STATIC` controls, menus and scrollbars only use
//! the modern Windows theme when the process activates the
//! `Microsoft.Windows.Common-Controls` 6.0 assembly. That dependency normally
//! travels in the application manifest.
//!
//! MinGW-w64's linker always embeds its own default manifest (long paths,
//! `asInvoker`) and ignores a second `RT_MANIFEST/1`, so the dependency never
//! reaches the binary and every control falls back to the classic Windows 2000
//! look. The manifest is therefore embedded a second time as `RT_MANIFEST/2`
//! and activated at runtime with `CreateActCtxW`. Windows only auto-loads
//! `RT_MANIFEST/1`, so the extra copy is inert until we ask for it.
//!
//! MSVC builds already carry the manifest as `RT_MANIFEST/1`; there
//! `IsThemeActive` is true from the start and this module does nothing.
//!
//! Resource id `2` lives in `assets/vtdrop-gnu.rc`.

use std::mem::size_of;
use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::System::ApplicationInstallationAndServicing::{
    ActivateActCtx, CreateActCtxW, ACTCTXW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WindowsProgramming::{
    ACTCTX_FLAG_HMODULE_VALID, ACTCTX_FLAG_RESOURCE_NAME_VALID,
};

const MANIFEST_RESOURCE_ID: usize = 2;

#[allow(dead_code)] // Kept alive for the whole process; the handle must not be released.
struct Activation {
    context: HANDLE,
    cookie: usize,
}

// A process-wide activation context is immutable after creation.
unsafe impl Send for Activation {}
unsafe impl Sync for Activation {}

static ACTIVATION: OnceLock<Option<Activation>> = OnceLock::new();

/// Turns on visual styles (`comctl32` v6) for the whole process.
///
/// Idempotent; safe to call before any window exists. Returns `true` when
/// themed controls are available.
pub fn enable_visual_styles() -> bool {
    let activation = ACTIVATION.get_or_init(|| unsafe { activate() });
    activation.is_some()
}

unsafe fn activate() -> Option<Activation> {
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    activate_from_module(module, MANIFEST_RESOURCE_ID)
}

unsafe fn activate_from_module(module: HMODULE, resource_id: usize) -> Option<Activation> {
    let context = unsafe { create_context(module, resource_id) }?;
    let mut cookie = 0usize;
    unsafe { ActivateActCtx(Some(context), &mut cookie) }.ok()?;
    Some(Activation { context, cookie })
}

unsafe fn create_context(module: HMODULE, resource_id: usize) -> Option<HANDLE> {
    let actctx = ACTCTXW {
        cbSize: size_of::<ACTCTXW>() as u32,
        dwFlags: ACTCTX_FLAG_HMODULE_VALID | ACTCTX_FLAG_RESOURCE_NAME_VALID,
        lpSource: PCWSTR::null(),
        wProcessorArchitecture: 0,
        wLangId: 0,
        lpAssemblyDirectory: PCWSTR::null(),
        lpResourceName: PCWSTR(resource_id as *const u16),
        lpApplicationName: PCWSTR::null(),
        hModule: module,
    };
    unsafe { CreateActCtxW(&actctx) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::{FreeLibrary, HWND};
    use windows::Win32::System::ApplicationInstallationAndServicing::DeactivateActCtx;
    use windows::Win32::System::LibraryLoader::{
        FindResourceW, LoadLibraryExW, LoadResource, LockResource, SizeofResource,
        LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, RT_MANIFEST, WINDOW_EX_STYLE, WS_POPUP,
    };

    /// `comctl32` v6 is only loaded when the process activation context asks
    /// for it. Its absence is the precise reason the dialog used to render
    /// with Windows 2000 buttons.
    fn common_controls_loaded() -> bool {
        unsafe { GetModuleHandleW(w!("comctl32.dll")) }.is_ok()
    }

    /// Creating a native control is what forces the loader to resolve the
    /// theming dependency.
    fn create_hidden_button() -> HWND {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("Button"),
                w!("probe"),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create button")
    }

    /// The resources live in the application binary, not in the test harness.
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

    fn manifest_bytes(module: HMODULE, id: usize) -> Option<Vec<u8>> {
        unsafe {
            let resource = FindResourceW(Some(module), PCWSTR(id as *const u16), RT_MANIFEST);
            if resource.0.is_null() {
                return None;
            }
            let size = SizeofResource(Some(module), resource) as usize;
            let loaded = LoadResource(Some(module), resource).ok()?;
            let pointer = LockResource(loaded) as *const u8;
            Some(std::slice::from_raw_parts(pointer, size).to_vec())
        }
    }

    /// The GNU build must carry the manifest twice: the linker's default one
    /// (`RT_MANIFEST/1`) plus ours with the Common-Controls dependency
    /// (`RT_MANIFEST/2`). Activating the latter is what themes the controls.
    ///
    /// The resource is copied to a manifest file because SxS cannot resolve an
    /// activation context from a module mapped as data.
    #[cfg(target_env = "gnu")]
    #[test]
    fn gnu_manifest_activates_common_controls_v6() {
        let module = app_module();
        let bytes = manifest_bytes(module, MANIFEST_RESOURCE_ID)
            .expect("RT_MANIFEST/2 is missing; controls would look like Windows 2000");
        unsafe {
            let _ = FreeLibrary(module);
        }

        let path = std::env::temp_dir().join("vtdrop-actctx-test.manifest");
        std::fs::write(&path, &bytes).expect("write manifest");
        let source = HSTRING::from(path.to_string_lossy().as_ref());
        let context = unsafe {
            CreateActCtxW(&ACTCTXW {
                cbSize: size_of::<ACTCTXW>() as u32,
                dwFlags: 0,
                lpSource: PCWSTR(source.as_ptr()),
                wProcessorArchitecture: 0,
                wLangId: 0,
                lpAssemblyDirectory: PCWSTR::null(),
                lpResourceName: PCWSTR::null(),
                lpApplicationName: PCWSTR::null(),
                hModule: HMODULE::default(),
            })
        }
        .expect("create activation context from the embedded manifest");

        let mut cookie = 0usize;
        unsafe { ActivateActCtx(Some(context), &mut cookie) }.expect("activate context");
        let button = create_hidden_button();
        let themed = common_controls_loaded();
        unsafe {
            let _ = DestroyWindow(button);
            let _ = DeactivateActCtx(0, cookie);
        }
        let _ = std::fs::remove_file(&path);
        assert!(
            themed,
            "the manifest was activated but comctl32 v6 was not loaded"
        );
    }

    /// MSVC embeds the manifest as `RT_MANIFEST/1`, so Windows applies it at
    /// process start and `enable_visual_styles` has nothing to do.
    #[cfg(not(target_env = "gnu"))]
    #[test]
    fn msvc_manifest_requests_common_controls() {
        let module = app_module();
        let bytes = manifest_bytes(module, 1).expect("RT_MANIFEST/1 is missing");
        unsafe {
            let _ = FreeLibrary(module);
        }
        let manifest = String::from_utf8_lossy(&bytes);
        assert!(
            manifest.contains("Microsoft.Windows.Common-Controls"),
            "the MSVC manifest does not request Common Controls v6"
        );
        assert!(
            manifest.contains("PerMonitorV2"),
            "the MSVC manifest lost per-monitor DPI awareness"
        );
    }
}
