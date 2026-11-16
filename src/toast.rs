//! Windows toast notification for multi-file summaries.
//!
//! Unpackaged desktop apps need an AppUserModelID and a Start Menu shortcut that
//! carries the same AUMID. The shortcut is created lazily; failures are silent
//! because the expanded pill still shows the worst result.

use std::path::PathBuf;

use windows::core::{Interface, BSTR, GUID, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, SetCurrentProcessExplicitAppUserModelID};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

use crate::model::ScanResult;

const AUMID: &str = "VTDrop.Desktop";
const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 5,
};

pub fn init() {
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID));
    }
    if let Some(path) = shortcut_path() {
        let _ = create_shortcut(&path);
    }
}

pub fn show_summary(results: &[ScanResult], errors: usize, skipped: usize) -> bool {
    if results.is_empty() && errors == 0 && skipped == 0 {
        return false;
    }
    if shortcut_path().is_none() {
        return false;
    }

    let mut lines = Vec::new();
    lines.push(format!("Análisis completado: {} archivo(s)", results.len()));
    for result in results.iter().take(8) {
        lines.push(format!("{} — {}", result.file_name(), result.ratio_text()));
    }
    if results.len() > 8 {
        lines.push(format!("… y {} más", results.len() - 8));
    }
    if errors > 0 {
        lines.push(format!("{errors} con error"));
    }
    if skipped > 0 {
        lines.push(format!("{skipped} omitido(s)"));
    }

    let mut xml = String::from("<toast><visual><binding template=\"ToastGeneric\">");
    for line in lines {
        xml.push_str("<text>");
        xml.push_str(&escape_xml(&line));
        xml.push_str("</text>");
    }
    xml.push_str("</binding></visual></toast>");

    let result = (|| -> windows::core::Result<()> {
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let notification = ToastNotification::CreateToastNotification(&document)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        notifier.Show(&notification)?;
        Ok(())
    })();
    result.is_ok()
}

fn shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("VTDrop.lnk"),
    )
}

fn create_shortcut(path: &std::path::Path) -> windows::core::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string())
        })?;
    }
    let exe = std::env::current_exe().map_err(|error| {
        windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string())
    })?;
    let exe = exe.to_string_lossy().into_owned();
    let path = path.to_string_lossy().into_owned();

    unsafe {
        let link: IShellLinkW = CoCreateInstance(
            &CLSID_SHELL_LINK,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )?;
        link.SetPath(&BSTR::from(exe.as_str()))?;
        link.SetDescription(&BSTR::from("VTDrop"))?;
        link.SetIconLocation(&BSTR::from(exe.as_str()), 0)?;
        link.SetArguments(&BSTR::from(""))?;

        let store: IPropertyStore = link.cast()?;
        let value = PROPVARIANT::from(AUMID);
        store.SetValue(&PKEY_APP_USER_MODEL_ID, &value)?;
        store.Commit()?;

        let file: IPersistFile = link.cast()?;
        file.Save(&HSTRING::from(path.as_str()), true)?;
    }
    Ok(())
}

fn escape_xml(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(character),
        }
    }
    output
}
