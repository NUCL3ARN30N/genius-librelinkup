//! Persistent settings (JSON in the platform config directory). Secrets live in the OS keyring.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::api::Identity;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnitPref {
    /// Use whatever the LibreLinkUp account is configured for.
    #[default]
    Auto,
    MmolL,
    MgDl,
}

/// How the tray icon looks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TrayStyle {
    /// 0 none (coloured number), 1 drop, 2 circle, 3 rounded square
    pub shape: u8,
    pub arrow: bool,
    /// 0 auto, 1 white, 2 black
    pub text_color: u8,
    /// 0 condensed, 1 regular, 2 monospace
    pub font: u8,
}

impl Default for TrayStyle {
    fn default() -> Self {
        Self { shape: 3, arrow: true, text_color: 0, font: 0 }
    }
}

/// Glucose range colours as `#rrggbb`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RangeColors {
    pub very_low: String,
    pub low: String,
    pub target: String,
    pub high: String,
    pub very_high: String,
}

impl Default for RangeColors {
    fn default() -> Self {
        Self {
            very_low: "#8e0e0e".into(),
            low: "#d32f2f".into(),
            target: "#2e7d32".into(),
            high: "#f9a825".into(),
            very_high: "#ef6c00".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Appearance {
    pub accent: String,
    pub colors: RangeColors,
    /// 0 square, 1 rounded, 2 round
    pub corners: u8,
    /// hours shown in the dashboard graph
    pub graph_hours: u32,
    /// 0 line, 1 dots, 2 line + dots
    pub graph_style: u8,
    /// Fill the current value card with the range colour.
    pub colored_card: bool,
    pub show_stats: bool,
    pub tray: TrayStyle,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            accent: "#1d5fa8".into(),
            colors: RangeColors::default(),
            corners: 1,
            graph_hours: 12,
            graph_style: 0,
            colored_card: true,
            show_stats: true,
            tray: TrayStyle::default(),
        }
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
pub fn parse_hex(s: &str) -> Option<[u8; 4]> {
    let h = s.trim().trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    match h.len() {
        3 => {
            let v: Vec<u8> = h.chars().map(|c| c.to_digit(16).map(|d| d as u8 * 17)).collect::<Option<_>>()?;
            Some([v[0], v[1], v[2], 255])
        }
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateSettings {
    pub auto_check: bool,
    /// A version the user chose to skip.
    pub skipped: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self { auto_check: true, skipped: String::new() }
    }
}

/// Pairing with a `gllu-server` caching server. The bearer token lives in the keyring.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SyncSettings {
    pub url: String,
    /// pinned SHA-256 of the server certificate; empty = public CA validation
    pub fingerprint: String,
    pub client_id: String,
    pub server_name: String,
    /// Read live values from the server instead of polling LibreLinkUp from this computer.
    pub use_as_source: bool,
    /// patient id → unix time up to which the local log holds the server's data
    pub synced_until: std::collections::BTreeMap<String, i64>,
}

impl SyncSettings {
    pub fn paired(&self) -> bool {
        !self.url.is_empty() && !self.client_id.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub email: String,
    pub remember_password: bool,
    /// Empty = automatic server discovery.
    pub server_override: String,
    /// Last server the account resolved to (cache for faster logins).
    pub resolved_server: String,
    pub identity: Identity,
    pub patient_id: String,

    pub units: UnitPref,
    pub poll_seconds: u32,
    /// Target range in mg/dL (also used for statistics and the graph band).
    pub target_low: f32,
    pub target_high: f32,

    pub notify_low: bool,
    pub low_threshold: f32,
    pub notify_high: bool,
    pub high_threshold: f32,
    pub notify_stale: bool,
    pub stale_minutes: u32,
    pub notify_repeat_minutes: u32,

    pub log_enabled: bool,
    /// Folder holding the (optionally encrypted) monthly log files. Empty = default data dir.
    pub log_dir: String,
    pub log_encrypted: bool,
    pub remember_passphrase: bool,

    pub dev_mode: bool,
    pub start_minimized: bool,
    pub autostart: bool,
    pub dark_mode: Option<bool>,
    pub appearance: Appearance,
    pub updates: UpdateSettings,
    pub sync: SyncSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            email: String::new(),
            remember_password: true,
            server_override: String::new(),
            resolved_server: String::new(),
            identity: Identity::default(),
            patient_id: String::new(),
            units: UnitPref::Auto,
            poll_seconds: 60,
            target_low: 70.0,
            target_high: 180.0,
            notify_low: true,
            low_threshold: 70.0,
            notify_high: false,
            high_threshold: 250.0,
            notify_stale: false,
            stale_minutes: 20,
            notify_repeat_minutes: 15,
            log_enabled: true,
            log_dir: String::new(),
            log_encrypted: true,
            remember_passphrase: true,
            dev_mode: false,
            start_minimized: false,
            autostart: false,
            dark_mode: None,
            appearance: Appearance::default(),
            updates: UpdateSettings::default(),
            sync: SyncSettings::default(),
        }
    }
}

pub fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("io", "genius-librelinkup", "genius-librelinkup")
        .expect("no home directory found")
}

/// `GLLU_HOME=<dir>` keeps settings, data and logs in one folder (isolated test copies,
/// portable setups). Secrets then use a separate keychain namespace, see `secrets`.
pub fn home_override() -> Option<PathBuf> {
    std::env::var_os("GLLU_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn config_path() -> PathBuf {
    match home_override() {
        Some(h) => h.join("settings.json"),
        None => project_dirs().config_dir().join("settings.json"),
    }
}

pub fn default_log_dir() -> PathBuf {
    match home_override() {
        Some(h) => h.join("log"),
        None => project_dirs().data_dir().join("log"),
    }
}

pub fn app_log_dir() -> PathBuf {
    match home_override() {
        Some(h) => h.join("app-logs"),
        None => project_dirs().data_local_dir().join("app-logs"),
    }
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from(&config_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut s: Settings = serde_json::from_str(&text).context("parsing settings")?;
        s.sanitize();
        Ok(s)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Settings meant for sharing/backup: drops account specific caches.
    pub fn exportable(&self) -> Settings {
        Settings {
            resolved_server: String::new(),
            patient_id: String::new(),
            // a pairing belongs to this computer (its token is in this keyring)
            sync: SyncSettings::default(),
            ..self.clone()
        }
    }

    pub fn log_dir(&self) -> PathBuf {
        if self.log_dir.trim().is_empty() { default_log_dir() } else { PathBuf::from(self.log_dir.trim()) }
    }

    pub fn sanitize(&mut self) {
        self.poll_seconds = self.poll_seconds.clamp(30, 900);
        self.target_low = self.target_low.clamp(40.0, 150.0);
        self.target_high = self.target_high.clamp(self.target_low + 10.0, 400.0);
        self.low_threshold = self.low_threshold.clamp(40.0, 200.0);
        self.high_threshold = self.high_threshold.clamp(80.0, 450.0);
        self.stale_minutes = self.stale_minutes.clamp(5, 240);
        self.notify_repeat_minutes = self.notify_repeat_minutes.clamp(0, 240);
        let a = &mut self.appearance;
        a.graph_hours = match a.graph_hours {
            0..=4 => 3,
            5..=8 => 6,
            9..=16 => 12,
            _ => 24,
        };
        a.corners = a.corners.min(2);
        a.graph_style = a.graph_style.min(2);
        a.tray.shape = a.tray.shape.min(3);
        a.tray.text_color = a.tray.text_color.min(2);
        a.tray.font = a.tray.font.min(2);
        let defaults = RangeColors::default();
        for (value, default) in [
            (&mut a.colors.very_low, defaults.very_low),
            (&mut a.colors.low, defaults.low),
            (&mut a.colors.target, defaults.target),
            (&mut a.colors.high, defaults.high),
            (&mut a.colors.very_high, defaults.very_high),
        ] {
            if parse_hex(value).is_none() {
                *value = default;
            }
        }
        if parse_hex(&a.accent).is_none() {
            a.accent = Appearance::default().accent;
        }
        if self.identity.product.trim().is_empty() || self.identity.version.trim().is_empty() {
            self.identity = Identity::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults_for_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"email":"x","poll_seconds":5}"#).unwrap();
        assert_eq!(s.email, "x");
        assert_eq!(s.target_high, 180.0);
        let mut s2 = s.clone();
        s2.sanitize();
        assert_eq!(s2.poll_seconds, 30);
        assert_eq!(s2.appearance.graph_hours, 12);
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex("#2e7d32"), Some([0x2e, 0x7d, 0x32, 255]));
        assert_eq!(parse_hex("fff"), Some([255, 255, 255, 255]));
        assert_eq!(parse_hex("#12345"), None);
    }
}
