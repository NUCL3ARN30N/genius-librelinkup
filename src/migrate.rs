//! One-time migration from the app's former name "Better LibreLinkUp": moves settings and the
//! glucose log to the new folders, copies keychain entries and replaces the autostart entry.
//! Runs only while the old folders still exist; `.bllu` month files are renamed by the store.

use std::path::Path;

use directories::ProjectDirs;

const LEGACY: &str = "better-librelinkup";

pub fn from_better_librelinkup() {
    let Some(old) = ProjectDirs::from("io", LEGACY, LEGACY) else { return };
    if !old.config_dir().exists() && !old.data_dir().exists() {
        return;
    }
    tracing::info!("migrating data from Better LibreLinkUp");
    let new = crate::config::project_dirs();
    for (from, to) in [
        (old.config_dir(), new.config_dir()),
        (old.data_dir(), new.data_dir()),
        (old.data_local_dir(), new.data_local_dir()),
    ] {
        move_contents(from, to);
    }
    // old app logs are not worth keeping; then drop the emptied folders
    let _ = std::fs::remove_dir_all(old.data_local_dir().join("app-logs"));
    for dir in [old.config_dir(), old.data_dir(), old.data_local_dir()] {
        let mut d = Some(dir);
        while let Some(p) = d.filter(|p| p.to_string_lossy().contains(LEGACY)) {
            if std::fs::remove_dir(p).is_err() && p.exists() {
                break;
            }
            d = p.parent();
        }
    }

    let email = crate::config::Settings::load().email;
    crate::secrets::migrate_from(LEGACY, &email);
    if crate::platform::remove_legacy_entries()
        && let Err(e) = crate::platform::set_autostart(true)
    {
        tracing::warn!("could not register autostart: {e:#}");
    }
}

/// Moves every entry of `from` into `to` unless `to` already has one with that name.
fn move_contents(from: &Path, to: &Path) {
    let Ok(entries) = std::fs::read_dir(from) else { return };
    if let Err(e) = std::fs::create_dir_all(to) {
        tracing::warn!("could not create {}: {e}", to.display());
        return;
    }
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        if target.exists() {
            continue;
        }
        if let Err(e) = std::fs::rename(entry.path(), &target) {
            tracing::warn!("could not move {} to {}: {e}", entry.path().display(), target.display());
        }
    }
}
