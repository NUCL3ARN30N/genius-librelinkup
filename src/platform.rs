//! Small OS integrations: autostart and (Windows) toast notification registration.

use anyhow::Result;

#[cfg(windows)]
pub const WINDOWS_APP_ID: &str = "io.genius-librelinkup";

/// Registers an AppUserModelID so unpackaged toasts show our name instead of PowerShell's.
#[cfg(windows)]
pub fn register_notifications() {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let res = (|| -> std::io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(format!(r"Software\Classes\AppUserModelId\{WINDOWS_APP_ID}"))?;
        key.set_value("DisplayName", &"Genius LibreLinkUp")?;
        if let Ok(exe) = std::env::current_exe() {
            let icon = exe.with_file_name("genius-librelinkup.ico");
            if !icon.exists() {
                let _ = std::fs::write(&icon, crate::icon::encode_ico(&[crate::icon::app_icon(64)]));
            }
            key.set_value("IconUri", &icon.to_string_lossy().to_string())?;
        }
        Ok(())
    })();
    if let Err(e) = res {
        tracing::warn!("could not register notification app id: {e}");
    }
}

/// Removes what [`register_notifications`] created (used by "delete all local data").
#[cfg(windows)]
pub fn unregister_notifications() {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(format!(r"Software\Classes\AppUserModelId\{WINDOWS_APP_ID}"));
}

#[cfg(not(windows))]
pub fn register_notifications() {}

#[cfg(not(windows))]
pub fn unregister_notifications() {}

#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let run = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_SET_VALUE)?;
    if enabled {
        let exe = std::env::current_exe()?;
        run.set_value("GeniusLibreLinkUp", &format!("\"{}\" --minimized", exe.display()))?;
    } else {
        let _ = run.delete_value("GeniusLibreLinkUp");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn set_autostart(enabled: bool) -> Result<()> {
    let dir = directories::BaseDirs::new()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .config_dir()
        .join("autostart");
    let file = dir.join("genius-librelinkup.desktop");
    if enabled {
        std::fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()?;
        std::fs::write(
            &file,
            format!(
                "[Desktop Entry]\nType=Application\nName=Genius LibreLinkUp\nExec=\"{}\" --minimized\n\
                 Icon=genius-librelinkup\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            ),
        )?;
    } else if file.exists() {
        std::fs::remove_file(file)?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn set_autostart(_enabled: bool) -> Result<()> {
    Ok(())
}

/// Removes the autostart entry and notification id of the app's former name. Returns whether
/// autostart was enabled there.
#[cfg(windows)]
pub fn remove_legacy_entries() -> bool {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(r"Software\Classes\AppUserModelId\io.better-librelinkup");
    hkcu.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_SET_VALUE)
        .is_ok_and(|run| run.delete_value("BetterLibreLinkUp").is_ok())
}

#[cfg(target_os = "linux")]
pub fn remove_legacy_entries() -> bool {
    directories::BaseDirs::new()
        .is_some_and(|b| std::fs::remove_file(b.config_dir().join("autostart/better-librelinkup.desktop")).is_ok())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn remove_legacy_entries() -> bool {
    false
}

/// Opens a web page in the default browser (https only).
pub fn open_url(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    #[cfg(windows)]
    let result = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = result {
        tracing::warn!("could not open {url}: {e}");
    }
}

/// Opens a folder in the system file manager.
pub fn open_path(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(not(windows))]
    let cmd = "xdg-open";
    if let Err(e) = std::process::Command::new(cmd).arg(path).spawn() {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}
