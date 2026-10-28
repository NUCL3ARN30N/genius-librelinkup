//! Secrets (LibreLinkUp password, log passphrase) stored in the OS credential store:
//! Windows Credential Manager / freedesktop Secret Service (GNOME Keyring, KWallet).

use std::sync::atomic::{AtomicBool, Ordering};

const SERVICE: &str = "better-librelinkup";
const PASSPHRASE_USER: &str = "log-passphrase";

/// Demo mode must never read or write the real keychain.
static DISABLED: AtomicBool = AtomicBool::new(false);

pub fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

fn service() -> String {
    match crate::config::home_override() {
        Some(home) => format!("{SERVICE}@{}", home.display()),
        None => SERVICE.to_string(),
    }
}

fn entry(user: &str) -> Option<keyring::Entry> {
    if DISABLED.load(Ordering::Relaxed) {
        return None;
    }
    match keyring::Entry::new(&service(), user) {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!("credential store unavailable: {e}");
            None
        }
    }
}

fn get(user: &str) -> Option<String> {
    entry(user)?.get_password().ok().filter(|s| !s.is_empty())
}

fn set(user: &str, secret: &str) -> bool {
    match entry(user).map(|e| e.set_password(secret)) {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            tracing::warn!("could not store secret: {e}");
            false
        }
        None => false,
    }
}

fn delete(user: &str) {
    if let Some(e) = entry(user) {
        let _ = e.delete_credential();
    }
}

fn account_user(email: &str) -> String {
    format!("llu:{}", email.trim().to_lowercase())
}

pub fn password(email: &str) -> Option<String> {
    get(&account_user(email))
}

pub fn store_password(email: &str, password: &str) -> bool {
    set(&account_user(email), password)
}

pub fn forget_password(email: &str) {
    delete(&account_user(email))
}

pub fn passphrase() -> Option<String> {
    get(PASSPHRASE_USER)
}

pub fn store_passphrase(passphrase: &str) -> bool {
    set(PASSPHRASE_USER, passphrase)
}

pub fn forget_passphrase() {
    delete(PASSPHRASE_USER)
}

const SYNC_USER: &str = "sync-credentials";
const LEGACY_SYNC_USER: &str = "sync-token";

/// Token + TLS client key of the caching-server pairing (never written to settings files).
pub fn sync_credentials() -> Option<bllu_core::sync::SyncCredentials> {
    serde_json::from_str(&get(SYNC_USER)?).ok()
}

pub fn store_sync_credentials(c: &bllu_core::sync::SyncCredentials) -> bool {
    serde_json::to_string(c).is_ok_and(|s| set(SYNC_USER, &s))
}

pub fn forget_sync_credentials() {
    delete(SYNC_USER);
    delete(LEGACY_SYNC_USER);
}
