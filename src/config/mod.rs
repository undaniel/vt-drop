//! Persistent configuration and DPAPI-protected API key.
//!
//! Layout:
//!   %APPDATA%\VTDrop\config.json  -> non-secret settings (UTF-8 JSON)
//!   %APPDATA%\VTDrop\config       -> DPAPI-protected API key (binary)
//!
//! The API key is never serialized, logged or written in plain text.

use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::slice;

use serde::{Deserialize, Serialize};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use zeroize::{Zeroize, Zeroizing};

use crate::util::{wide, wide_path};

pub mod key_dialog;

const CONFIG_DIR: &str = "VTDrop";
const CONFIG_FILE: &str = "config.json";
const SECRET_FILE: &str = "config";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "VTDrop";
const DPAPI_ENTROPY: &[u8] = b"VTDrop/config/v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackdropMode {
    /// Undocumented `ACCENT_ENABLE_ACRYLICBLURBEHIND`, stays blurred without focus.
    #[default]
    Acrylic,
    /// DWM system transient backdrop (Win11 22621+), solid when inactive.
    System,
    /// Opaque theme color, used as a fallback.
    Solid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Physical top-left of the compact widget.
    pub position: Option<[i32; 2]>,
    pub always_on_top: bool,
    /// Detections at or above this value use amber (1..threshold_red-1).
    pub threshold_amber: u32,
    /// Detections at or above this value use red.
    pub threshold_red: u32,
    /// Seconds before an expanded result collapses. Zero keeps it open.
    pub auto_close_secs: u64,
    /// Explicit opt-in; off by default.
    pub auto_upload: bool,
    /// Enumerate dropped folders recursively instead of ignoring them.
    pub scan_folders: bool,
    /// API requests per minute (public VirusTotal limit is 4).
    pub requests_per_minute: u32,
    pub version: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: None,
            always_on_top: true,
            threshold_amber: 1,
            threshold_red: 4,
            auto_close_secs: 8,
            auto_upload: false,
            scan_folders: false,
            requests_per_minute: 4,
            version: 1,
        }
    }
}

impl Config {
    /// Loads settings, falling back to defaults on absence or corruption.
    pub fn load() -> Self {
        let path = config_path();
        let mut config = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Config>(&bytes).unwrap_or_default(),
            Err(_) => Config::default(),
        };
        config.normalize();
        config
    }

    /// Saves settings atomically (temp + MoveFileEx).
    pub fn save(&self) -> io::Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let path = config_path();
        let temp = path.with_extension("json.tmp");
        let mut json = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        json.push(b'\n');
        std::fs::write(&temp, &json)?;

        let from = wide_path(&temp);
        let to = wide_path(&path);
        let result = unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING,
            )
        };
        if result.is_err() {
            // Extremely unlikely; do not lose the settings if a filter blocks the move.
            std::fs::remove_file(&path).ok();
            std::fs::rename(&temp, &path)?;
        }
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.threshold_amber = self.threshold_amber.max(1);
        if self.threshold_red <= self.threshold_amber {
            self.threshold_red = self.threshold_amber.saturating_add(1);
        }
        self.requests_per_minute = self.requests_per_minute.clamp(1, 60);
        if self.auto_close_secs > 86_400 {
            self.auto_close_secs = 8;
        }
        self.version = 1;
    }
}

/// `%APPDATA%\VTDrop`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR)
}

pub fn config_path() -> PathBuf {
    data_dir().join(CONFIG_FILE)
}

fn secret_path() -> PathBuf {
    data_dir().join(SECRET_FILE)
}

/// True when the string looks like a public VirusTotal API key.
pub fn is_valid_api_key(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Loads and decrypts the API key with DPAPI.
pub fn load_api_key() -> io::Result<Option<Zeroizing<String>>> {
    let path = secret_path();
    let blob = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if blob.is_empty() {
        return Ok(None);
    }

    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    let result = unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    result.map_err(|error| io::Error::other(format!("DPAPI decrypt failed: {error}")))?;

    let plain = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let text = String::from_utf8(plain.to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "API key is not UTF-8"));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void)));
    }

    let text = Zeroizing::new(text?);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(Zeroizing::new(trimmed.to_owned())))
}

/// Encrypts and stores the API key with DPAPI. An empty value removes it.
pub fn save_api_key(value: &str) -> io::Result<()> {
    if value.is_empty() {
        return match std::fs::remove_file(secret_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }

    std::fs::create_dir_all(data_dir())?;
    let mut plain = value.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    let result = unsafe {
        CryptProtectData(
            &input,
            w!("VTDrop API key"),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    plain.zeroize();

    result.map_err(|error| io::Error::other(format!("DPAPI encrypt failed: {error}")))?;

    let encrypted =
        unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void)));
    }

    let temp = secret_path().with_extension("tmp");
    std::fs::write(&temp, &encrypted)?;
    let from = wide_path(&temp);
    let to = wide_path(&secret_path());
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    }
    .map_err(|error| io::Error::other(format!("could not commit API key: {error}")))?;
    Ok(())
}

/// Reads a HKCU DWORD, returning `None` if missing or of the wrong type.
pub fn read_hkcu_dword(subkey: &str, value: &str) -> Option<u32> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            windows::Win32::System::Registry::RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

pub fn read_hkcu_string(subkey: &str, value: &str) -> Option<String> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut buffer = [0u16; 2048];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || size < 2 {
        return None;
    }
    Some(crate::util::from_wide_lossy(&buffer))
}

fn set_startup_command(command: Option<&str>) -> io::Result<()> {
    let subkey = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let status = match command {
        Some(command) => {
            let mut data = wide(command);
            let status = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey.as_ptr()),
                    PCWSTR(name.as_ptr()),
                    REG_SZ.0,
                    Some(data.as_ptr() as *const c_void),
                    (data.len() * 2) as u32,
                )
            };
            data.zeroize();
            status
        }
        None => unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                PCWSTR(name.as_ptr()),
            )
        },
    };

    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(io::Error::other(format!("registry error {}", status.0)))
    }
}

/// Enables or disables auto-start for the current executable.
pub fn set_start_with_windows(enabled: bool) -> io::Result<()> {
    if !enabled {
        return set_startup_command(None);
    }
    let exe = std::env::current_exe()?;
    let command = format!("\"{}\"", exe.display());
    set_startup_command(Some(&command))
}

pub fn is_start_with_windows() -> bool {
    match read_hkcu_string(RUN_KEY, RUN_VALUE) {
        Some(value) => {
            let value = value.trim();
            let exe = std::env::current_exe().ok();
            match exe {
                Some(exe) => {
                    let exe = exe.to_string_lossy();
                    value.trim_matches('"').eq_ignore_ascii_case(exe.as_ref())
                }
                None => !value.is_empty(),
            }
        }
        None => false,
    }
}

/// Deletes a corrupt config so the next start is clean. Used by diagnostics.
pub fn quarantine_config() {
    let path = config_path();
    if path.exists() {
        let _ = std::fs::rename(&path, path.with_extension("json.bad"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn api_key_validation() {
        assert!(is_valid_api_key(&"a".repeat(64)));
        assert!(!is_valid_api_key(&"z".repeat(64)));
        assert!(!is_valid_api_key("abc"));
    }

    #[test]
    fn defaults_are_sane() {
        let mut config = Config::default();
        config.normalize();
        assert_eq!(config.threshold_amber, 1);
        assert_eq!(config.threshold_red, 4);
        assert!(!config.auto_upload);
        assert!(!config.scan_folders);
    }

    #[test]
    fn thresholds_are_normalized() {
        let mut config = Config {
            threshold_amber: 0,
            threshold_red: 0,
            requests_per_minute: 0,
            ..Config::default()
        };
        config.normalize();
        assert_eq!(config.threshold_amber, 1);
        assert_eq!(config.threshold_red, 2);
        assert_eq!(config.requests_per_minute, 1);
    }

    #[test]
    fn config_roundtrips_as_json() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.threshold_red, 4);
    }

    #[test]
    fn data_dir_is_under_appdata() {
        if std::env::var_os("APPDATA").is_some() {
            assert!(data_dir().ends_with("VTDrop"));
        }
    }

    #[test]
    fn path_helpers_do_not_panic_with_spaces() {
        let _ = wide_path(Path::new(r"C:\Program Files\VTDrop\vtdrop.exe"));
    }
}
