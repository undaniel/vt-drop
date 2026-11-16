//! System theme, accent color and accessibility settings.

use std::ffi::c_void;
use std::mem::size_of;

use windows::core::{w, BOOL, PCSTR};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Graphics::Dwm::{DwmGetColorizationColor, DwmIsCompositionEnabled};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::config::read_hkcu_dword;

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const DWM_KEY: &str = r"Software\Microsoft\Windows\DWM";
const FALLBACK_ACCENT: Rgb = Rgb {
    r: 0x00,
    g: 0x67,
    b: 0xC0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_colorref(self) -> u32 {
        self.r as u32 | ((self.g as u32) << 8) | ((self.b as u32) << 16)
    }

    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8;
        Rgb::new(
            channel(self.r, other.r),
            channel(self.g, other.g),
            channel(self.b, other.b),
        )
    }

    pub fn with_alpha(self, alpha: f32) -> (f32, f32, f32, f32) {
        (
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            alpha,
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub accent: Rgb,
    pub transparency_enabled: bool,
    pub animations_enabled: bool,
    pub composition_enabled: bool,
    pub build: u32,
}

impl Theme {
    pub fn detect() -> Self {
        // Apps follow "app mode", not the taskbar's "Windows mode".
        let light = read_hkcu_dword(PERSONALIZE, "AppsUseLightTheme")
            .or_else(|| read_hkcu_dword(PERSONALIZE, "SystemUsesLightTheme"));
        let dark = light.map(|value| value == 0).unwrap_or(false);
        let transparency_enabled = read_hkcu_dword(PERSONALIZE, "EnableTransparency")
            .map(|value| value != 0)
            .unwrap_or(true);
        let composition_enabled = unsafe { DwmIsCompositionEnabled() }
            .map(|value| value.as_bool())
            .unwrap_or(true);
        let build = windows_build();
        let accent = read_accent_color().unwrap_or(FALLBACK_ACCENT);

        Self {
            dark,
            accent,
            transparency_enabled,
            animations_enabled: client_area_animations(),
            composition_enabled,
            build,
        }
    }

    pub fn is_win11(&self) -> bool {
        self.build >= 22_000
    }

    /// `DWMWA_SYSTEMBACKDROP_TYPE` with `DWMSBT_TRANSIENTWINDOW` requires 22621.
    pub fn supports_system_backdrop(&self) -> bool {
        self.build >= 22_621
    }

    pub fn effective_solid(&self) -> bool {
        !self.transparency_enabled || !self.composition_enabled
    }

    /// RGB background used when acrylic/system backdrops are unavailable.
    pub fn solid_background(&self) -> Rgb {
        if self.dark {
            Rgb::new(0x20, 0x20, 0x20)
        } else {
            Rgb::new(0xF3, 0xF3, 0xF3)
        }
    }

    pub fn foreground(&self) -> Rgb {
        if self.dark {
            Rgb::new(0xF5, 0xF5, 0xF5)
        } else {
            Rgb::new(0x1A, 0x1A, 0x1A)
        }
    }

    pub fn secondary(&self) -> Rgb {
        if self.dark {
            Rgb::new(0xB8, 0xB8, 0xB8)
        } else {
            Rgb::new(0x5C, 0x5C, 0x5C)
        }
    }

    pub fn muted(&self) -> Rgb {
        if self.dark {
            Rgb::new(0x8A, 0x8A, 0x8A)
        } else {
            Rgb::new(0x7A, 0x7A, 0x7A)
        }
    }

    pub fn border_tint(&self) -> (f32, f32, f32, f32) {
        if self.dark {
            (1.0, 1.0, 1.0, 0.10)
        } else {
            (0.0, 0.0, 0.0, 0.10)
        }
    }

    pub fn green(&self) -> Rgb {
        if self.dark {
            Rgb::new(0x4C, 0xC3, 0x8A)
        } else {
            Rgb::new(0x1E, 0x8E, 0x5A)
        }
    }

    pub fn amber(&self) -> Rgb {
        if self.dark {
            Rgb::new(0xF0, 0xB4, 0x29)
        } else {
            Rgb::new(0xA8, 0x6A, 0x00)
        }
    }

    pub fn red(&self) -> Rgb {
        if self.dark {
            Rgb::new(0xFF, 0x6B, 0x6B)
        } else {
            Rgb::new(0xC4, 0x2B, 0x2B)
        }
    }

    pub fn grey(&self) -> Rgb {
        if self.dark {
            Rgb::new(0xA0, 0xA0, 0xA0)
        } else {
            Rgb::new(0x6E, 0x6E, 0x6E)
        }
    }

    pub fn acrylic_tint(&self) -> u32 {
        // GradientColor is 0xAABBGGRR.
        if self.dark {
            0x99202020
        } else {
            0x99F2F2F2
        }
    }
}

fn windows_build() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // GetVersionExW lies without a supportedOS manifest (the GNU build uses the
    // MinGW default manifest), so ask the kernel directly first.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.0 < 0 {
        let mut fallback = OSVERSIONINFOW {
            dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        unsafe { GetVersionExW(&mut fallback) }.ok();
        return fallback.dwBuildNumber;
    }
    info.dwBuildNumber
}

fn client_area_animations() -> bool {
    let mut enabled = BOOL(1);
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut BOOL as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if result.is_err() {
        return true;
    }
    enabled.as_bool()
}

fn read_accent_color() -> Option<Rgb> {
    if let Some(value) = read_hkcu_dword(DWM_KEY, "AccentColor") {
        // 0xAABBGGRR
        let rgb = Rgb::new(
            (value & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            ((value >> 16) & 0xFF) as u8,
        );
        if rgb.r != 0 || rgb.g != 0 || rgb.b != 0 {
            return Some(rgb);
        }
    }

    let mut colorization = 0u32;
    let mut opaque = BOOL(0);
    unsafe { DwmGetColorizationColor(&mut colorization, &mut opaque) }.ok()?;
    // ColorizationColor is 0xAARRGGBB.
    let rgb = Rgb::new(
        ((colorization >> 16) & 0xFF) as u8,
        ((colorization >> 8) & 0xFF) as u8,
        (colorization & 0xFF) as u8,
    );
    (rgb.r != 0 || rgb.g != 0 || rgb.b != 0).then_some(rgb)
}

/// Opts native Win32 menus into the dark/light theme on Windows 10 1809+.
///
/// `SetPreferredAppMode` and `FlushMenuThemes` are undocumented uxtheme exports,
/// so this is intentionally best-effort and cannot fail the app.
pub fn apply_native_menu_theme(dark: bool) {
    unsafe {
        let Ok(module) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        let set_mode = GetProcAddress(module, PCSTR(135 as *const u8));
        let flush = GetProcAddress(module, PCSTR(136 as *const u8));

        if let Some(set_mode) = set_mode {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            // 0 = Default, 1 = AllowDark, 2 = ForceDark, 3 = ForceLight
            let _ = set_mode(if dark { 2 } else { 1 });
        }
        if let Some(flush) = flush {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_abgr_to_rgb() {
        let value = 0xFF24_8240u32; // A=FF, B=24, G=82, R=40
        let rgb = Rgb::new(
            (value & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            ((value >> 16) & 0xFF) as u8,
        );
        assert_eq!(rgb, Rgb::new(0x40, 0x82, 0x24));
    }

    #[test]
    fn colorref_is_bgr() {
        assert_eq!(Rgb::new(0x12, 0x34, 0x56).to_colorref(), 0x0056_3412);
    }

    #[test]
    fn win11_checks() {
        let mut theme = Theme {
            dark: true,
            accent: FALLBACK_ACCENT,
            transparency_enabled: true,
            animations_enabled: true,
            composition_enabled: true,
            build: 22_621,
        };
        assert!(theme.is_win11());
        assert!(theme.supports_system_backdrop());
        theme.build = 19_045;
        assert!(!theme.supports_system_backdrop());
        theme.transparency_enabled = false;
        assert!(theme.effective_solid());
    }
}
