//! Shared Win32 helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// Converts a string to a NUL-terminated UTF-16 buffer.
pub fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Converts a path to a NUL-terminated UTF-16 buffer.
pub fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Converts a NUL-terminated UTF-16 slice to a `String`.
pub fn from_wide_lossy(value: &[u16]) -> String {
    let end = value.iter().position(|&c| c == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

/// Formats a SHA-256 digest as lowercase hexadecimal.
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Clamps a value without requiring `Ord` on references.
pub fn clamp_f32(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_complete() {
        assert_eq!(hex_lower(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn wide_is_nul_terminated() {
        assert_eq!(wide("ab"), vec![b'a' as u16, b'b' as u16, 0]);
    }
}

