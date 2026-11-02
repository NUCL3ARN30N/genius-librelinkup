//! Glucose log: one file per month (`glucose-YYYY-MM.gllu`) inside a user chosen folder, so the
//! folder can live in Nextcloud/Dropbox/OneDrive. Files are optionally encrypted with
//! XChaCha20-Poly1305 using a key derived from a passphrase with Argon2id (`vault.json` keeps
//! the salt and a check value). Before a month is written the file on disk is merged in, so two
//! machines syncing the same folder do not lose each other's readings. Notes live next to the
//! months in `notes.gllu` (same container, JSON payload) and are merged the same way.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, Generate, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::api::Reading;
use crate::tr;

// The format constants (magic, check value, AAD prefix) keep the original "Better LibreLinkUp"
// identifiers so logs written before the rename stay readable.
const MAGIC: &[u8; 4] = b"BLLU";
const FORMAT_VERSION: u8 = 1;
const CIPHER_NONE: u8 = 0;
const CIPHER_XCHACHA: u8 = 1;
const RECORD_LEN: usize = 11;
const CHECK_PLAINTEXT: &[u8] = b"better-librelinkup vault check";
pub const MMOL_FACTOR: f32 = 18.0182;
const EXT: &str = ".gllu";
/// Month files written before the rename to Genius LibreLinkUp.
const LEGACY_EXT: &str = ".bllu";
const NOTES_FILE: &str = "notes.gllu";
/// Longest note text that is kept (characters).
pub const NOTE_MAX_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultState {
    /// No vault yet – will be created.
    New,
    Plain,
    Encrypted,
}

#[derive(Serialize, Deserialize)]
struct VaultMeta {
    version: u8,
    encrypted: bool,
    #[serde(default)]
    kdf: String,
    #[serde(default)]
    m_cost: u32,
    #[serde(default)]
    t_cost: u32,
    #[serde(default)]
    p_cost: u32,
    #[serde(default)]
    salt: String,
    #[serde(default)]
    check: String,
}

type Key = [u8; 32];
type MonthId = (i32, u32);

pub struct Store {
    dir: PathBuf,
    key: Option<Key>,
    months: BTreeMap<MonthId, Month>,
    /// id → note, including deleted ones (tombstones)
    notes: BTreeMap<String, Note>,
    notes_dirty: bool,
    /// Why `notes.gllu` could not be read; notes are then read-only so the file is not replaced.
    notes_error: Option<String>,
}

/// Longest custom treatment name (characters).
pub const TREATMENT_NAME_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreatmentKind {
    RapidActing,
    LongActing,
    Custom,
}

/// Insulin (or another medication) given with a note.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Treatment {
    pub kind: TreatmentKind,
    /// only for [`TreatmentKind::Custom`]
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<f32>,
}

impl Treatment {
    /// Validates and normalises: a custom treatment needs a name, units must be 0 < u ≤ 1000
    /// (rounded to 2 decimals).
    pub fn new(kind: TreatmentKind, name: &str, units: Option<f32>) -> Result<Treatment> {
        let name = if kind == TreatmentKind::Custom { clean_note_text(name) } else { String::new() };
        if kind == TreatmentKind::Custom {
            if name.is_empty() {
                bail!(tr!("enter a name for the custom treatment"));
            }
            if name.chars().count() > TREATMENT_NAME_MAX_CHARS {
                bail!(tr!("the treatment name is longer than {} characters", TREATMENT_NAME_MAX_CHARS));
            }
        }
        let units = match units {
            Some(u) if !u.is_finite() || u <= 0.0 || u > 1000.0 => bail!(tr!("units must be between 0 and 1000")),
            Some(u) => Some((u * 100.0).round() / 100.0),
            None => None,
        };
        Ok(Treatment { kind, name, units })
    }

    pub fn label(&self) -> &str {
        match self.kind {
            TreatmentKind::RapidActing => "Rapid-acting insulin",
            TreatmentKind::LongActing => "Long-acting insulin",
            TreatmentKind::Custom => &self.name,
        }
    }

    /// Inverse of [`Treatment::label`] (case-insensitive); any other text is a custom treatment.
    pub fn from_label(label: &str, units: Option<f32>) -> Result<Treatment> {
        let kind = [TreatmentKind::RapidActing, TreatmentKind::LongActing]
            .into_iter()
            .find(|k| Treatment { kind: *k, name: String::new(), units: None }.label().eq_ignore_ascii_case(label.trim()))
            .unwrap_or(TreatmentKind::Custom);
        Treatment::new(kind, label, units)
    }

    /// The label in the current language (custom names stay as typed).
    pub fn display_label(&self) -> &str {
        match self.kind {
            TreatmentKind::Custom => &self.name,
            _ => crate::i18n::tr(match self.kind {
                TreatmentKind::RapidActing => "Rapid-acting insulin",
                _ => "Long-acting insulin",
            }),
        }
    }

    /// "Rapid-acting insulin: 4.5 u", or the label alone without units (current language).
    pub fn describe(&self) -> String {
        match self.units {
            Some(u) => tr!("{}: {} u", self.display_label(), crate::i18n::numbers(&fmt_units(u))),
            None => self.display_label().to_string(),
        }
    }
}

/// Validates carbohydrates: 0 < g ≤ 1000, rounded to one decimal.
pub fn check_carbs(grams: Option<f32>) -> Result<Option<f32>> {
    match grams {
        Some(g) if !g.is_finite() || g <= 0.0 || g > 1000.0 => bail!(tr!("carbs must be between 0 and 1000 g")),
        Some(g) => Ok(Some((g * 10.0).round() / 10.0)),
        None => Ok(None),
    }
}

/// Units without trailing zeros: 4.5, 12, 0.25.
pub fn fmt_units(u: f32) -> String {
    let s = format!("{u:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A free-text note at a point in time (meal, insulin, exercise, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    /// unix seconds the note refers to
    pub time: i64,
    pub text: String,
    /// unix milliseconds of the last change; the newest version wins when merging
    pub updated: i64,
    /// kept as a tombstone so a copy on another device does not bring the note back
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treatment: Option<Treatment>,
    /// carbohydrates in grams
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carbs: Option<f32>,
    /// fields written by newer versions, kept so saving here does not drop them
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Note {
    pub fn at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.time, 0).unwrap_or_default()
    }

    /// Treatment, carbs and text in one line, e.g. "Rapid-acting insulin: 4.5 u · 45 g carbs · Breakfast".
    pub fn summary(&self) -> String {
        let carbs = self.carbs.map(|g| tr!("{} g carbs", crate::i18n::numbers(&fmt_units(g))));
        let parts = [self.treatment.as_ref().map(Treatment::describe), carbs, Some(self.text.clone())];
        parts.into_iter().flatten().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
    }
}

/// Single line, trimmed and limited to [`NOTE_MAX_CHARS`].
pub fn clean_note_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(NOTE_MAX_CHARS).collect()
}

/// Adds `other` to `into`; per id the newer version wins, a deletion wins a tie.
fn merge_notes(into: &mut BTreeMap<String, Note>, other: BTreeMap<String, Note>) {
    for (id, n) in other {
        match into.get(&id) {
            Some(cur) if (cur.updated, cur.deleted) >= (n.updated, n.deleted) => {}
            _ => {
                into.insert(id, n);
            }
        }
    }
}

/// A note found in a CSV file.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvNote {
    pub time: DateTime<Utc>,
    pub text: String,
    pub treatment: Option<Treatment>,
    pub carbs: Option<f32>,
}

/// What a CSV file contained.
#[derive(Debug, Default)]
pub struct CsvData {
    pub readings: Vec<Reading>,
    pub notes: Vec<CsvNote>,
}

/// Result of [`Store::import_csv`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    pub readings: usize,
    pub notes: usize,
}

#[derive(Default)]
struct Month {
    /// unix seconds → (mg/dL, trend)
    readings: BTreeMap<i64, (u16, u8)>,
    dirty: bool,
}

fn meta_path(dir: &Path) -> PathBuf {
    dir.join("vault.json")
}

pub fn vault_state(dir: &Path) -> VaultState {
    match std::fs::read_to_string(meta_path(dir)).ok().and_then(|t| serde_json::from_str::<VaultMeta>(&t).ok()) {
        None => VaultState::New,
        Some(m) if m.encrypted => VaultState::Encrypted,
        Some(_) => VaultState::Plain,
    }
}

fn derive_key(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Key> {
    let params = argon2::Params::new(m, t, p, Some(32)).map_err(|e| anyhow!("argon2 params: {e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("argon2: {e}"))?;
    Ok(key)
}

fn seal(key: &Key, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XNonce::generate();
    let ct = cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad })
        .map_err(|_| anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open_sealed(key: &Key, data: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 24 + 16 {
        bail!("ciphertext too short");
    }
    let nonce = XNonce::try_from(&data[..24]).map_err(|_| anyhow!("bad nonce"))?;
    XChaCha20Poly1305::new(key.into())
        .decrypt(&nonce, Payload { msg: &data[24..], aad })
        .map_err(|_| anyhow!(tr!("wrong passphrase or corrupted file")))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::fill(&mut b[..]);
    b
}

impl Store {
    /// Opens (or creates) the log in `dir`. `passphrase` is required for encrypted vaults and,
    /// for a new vault, decides whether it will be encrypted.
    pub fn open(dir: &Path, passphrase: Option<&str>) -> Result<Store> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let key = match std::fs::read_to_string(meta_path(dir)) {
            Ok(text) => {
                let meta: VaultMeta = serde_json::from_str(&text).context("parsing vault.json")?;
                if meta.encrypted {
                    let pass = passphrase.ok_or_else(|| anyhow!(tr!("passphrase required")))?;
                    let salt = hex::decode(&meta.salt)?;
                    let key = derive_key(pass, &salt, meta.m_cost, meta.t_cost, meta.p_cost)?;
                    open_sealed(&key, &hex::decode(&meta.check)?, b"check")
                        .map_err(|_| anyhow!(tr!("wrong passphrase")))?;
                    Some(key)
                } else {
                    None
                }
            }
            Err(_) => Self::write_meta(dir, passphrase.filter(|p| !p.is_empty()))?,
        };
        let mut store = Store {
            dir: dir.to_path_buf(),
            key,
            months: BTreeMap::new(),
            notes: BTreeMap::new(),
            notes_dirty: false,
            notes_error: None,
        };
        store.migrate_legacy_files()?;
        store.load_all()?;
        store.load_notes();
        Ok(store)
    }

    fn write_meta(dir: &Path, passphrase: Option<&str>) -> Result<Option<Key>> {
        let (meta, key) = match passphrase {
            Some(pass) => {
                let salt = random_bytes::<16>();
                let (m, t, p) = (64 * 1024, 3, 1);
                let key = derive_key(pass, &salt, m, t, p)?;
                let check = seal(&key, CHECK_PLAINTEXT, b"check")?;
                (
                    VaultMeta {
                        version: 1,
                        encrypted: true,
                        kdf: "argon2id".into(),
                        m_cost: m,
                        t_cost: t,
                        p_cost: p,
                        salt: hex::encode(salt),
                        check: hex::encode(check),
                    },
                    Some(key),
                )
            }
            None => (
                VaultMeta {
                    version: 1,
                    encrypted: false,
                    kdf: String::new(),
                    m_cost: 0,
                    t_cost: 0,
                    p_cost: 0,
                    salt: String::new(),
                    check: String::new(),
                },
                None,
            ),
        };
        atomic_write(&meta_path(dir), serde_json::to_string_pretty(&meta)?.as_bytes())?;
        Ok(key)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_encrypted(&self) -> bool {
        self.key.is_some()
    }

    /// Confirms that `passphrase` is the current one (always true for an unencrypted log). Used
    /// before the passphrase is changed or removed, so an unlocked session alone is not enough.
    pub fn verify_passphrase(&self, passphrase: &str) -> Result<()> {
        let Some(current) = self.key else { return Ok(()) };
        let meta: VaultMeta = serde_json::from_str(&std::fs::read_to_string(meta_path(&self.dir))?)?;
        let key = derive_key(passphrase, &hex::decode(&meta.salt)?, meta.m_cost, meta.t_cost, meta.p_cost)?;
        if key != current {
            bail!(tr!("wrong passphrase"));
        }
        Ok(())
    }

    fn month_path(&self, (y, m): MonthId) -> PathBuf {
        self.dir.join(format!("glucose-{y:04}-{m:02}{EXT}"))
    }

    /// Renames `.bllu` month files to `.gllu`; when both exist (another machine already
    /// migrated the synced folder) the legacy file is merged in first.
    fn migrate_legacy_files(&self) -> Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().to_string();
            let Some(id) = parse_month_name(&name, LEGACY_EXT) else { continue };
            let (legacy, path) = (self.dir.join(&name), self.month_path(id));
            if path.exists() {
                let mut readings = self.read_file(&path, id)?;
                for (ts, v) in self.read_file(&legacy, id)? {
                    readings.entry(ts).or_insert(v);
                }
                self.write_month_file(id, &readings)?;
                std::fs::remove_file(&legacy)?;
            } else {
                std::fs::rename(&legacy, &path).with_context(|| format!("renaming {}", legacy.display()))?;
            }
        }
        Ok(())
    }

    fn load_all(&mut self) -> Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if let Some(id) = parse_month_name(&name, EXT) {
                let readings = self.read_month_file(id)?;
                self.months.entry(id).or_default().readings.extend(readings);
            }
        }
        Ok(())
    }

    fn read_month_file(&self, id: MonthId) -> Result<BTreeMap<i64, (u16, u8)>> {
        self.read_file(&self.month_path(id), id)
    }

    fn read_file(&self, path: &Path, id: MonthId) -> Result<BTreeMap<i64, (u16, u8)>> {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e.into()),
        };
        if data.len() < 6 || &data[..4] != MAGIC || data[4] != FORMAT_VERSION {
            bail!("{} is not a glucose log file", path.display());
        }
        let aad = month_aad(id);
        let payload = match data[5] {
            CIPHER_NONE => data[6..].to_vec(),
            CIPHER_XCHACHA => {
                let key = self.key.as_ref().ok_or_else(|| anyhow!("{} is encrypted", path.display()))?;
                open_sealed(key, &data[6..], &aad).with_context(|| path.display().to_string())?
            }
            c => bail!("unknown cipher {c} in {}", path.display()),
        };
        Ok(payload
            .chunks_exact(RECORD_LEN)
            .map(|c| {
                let ts = i64::from_le_bytes(c[..8].try_into().unwrap());
                (ts, (u16::from_le_bytes([c[8], c[9]]), c[10]))
            })
            .collect())
    }

    fn write_month_file(&self, id: MonthId, readings: &BTreeMap<i64, (u16, u8)>) -> Result<()> {
        let mut payload = Vec::with_capacity(readings.len() * RECORD_LEN);
        for (ts, (v, t)) in readings {
            payload.extend_from_slice(&ts.to_le_bytes());
            payload.extend_from_slice(&v.to_le_bytes());
            payload.push(*t);
        }
        let mut out = Vec::with_capacity(payload.len() + 64);
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        match &self.key {
            Some(key) => {
                out.push(CIPHER_XCHACHA);
                out.extend_from_slice(&seal(key, &payload, &month_aad(id))?);
            }
            None => {
                out.push(CIPHER_NONE);
                out.extend_from_slice(&payload);
            }
        }
        atomic_write(&self.month_path(id), &out)
    }

    /// Adds readings, returns how many were new.
    pub fn insert(&mut self, readings: &[Reading]) -> usize {
        let mut added = 0;
        for r in readings {
            let ts = r.time.timestamp();
            let month = self.months.entry(month_of(r.time)).or_default();
            let value = (r.mg_dl.round().clamp(0.0, 1000.0) as u16, r.trend);
            if month.readings.insert(ts, value).is_none() {
                added += 1;
                month.dirty = true;
            }
        }
        added
    }

    /// Writes all changed months (merging with what another device may have synced meanwhile).
    pub fn flush(&mut self) -> Result<()> {
        let dirty: Vec<MonthId> = self.months.iter().filter(|(_, m)| m.dirty).map(|(id, _)| *id).collect();
        for id in dirty {
            let on_disk = self.read_month_file(id).unwrap_or_else(|e| {
                tracing::warn!("could not merge existing month file: {e:#}");
                BTreeMap::new()
            });
            let month = self.months.get_mut(&id).unwrap();
            for (ts, v) in on_disk {
                month.readings.entry(ts).or_insert(v);
            }
            let readings = month.readings.clone();
            self.write_month_file(id, &readings)?;
            self.months.get_mut(&id).unwrap().dirty = false;
        }
        if self.notes_dirty && self.notes_error.is_none() {
            match self.read_notes_file() {
                Ok(on_disk) => merge_notes(&mut self.notes, on_disk),
                Err(e) => tracing::warn!("could not merge existing notes file: {e:#}"),
            }
            self.write_notes_file()?;
            self.notes_dirty = false;
        }
        Ok(())
    }

    /// Re-reads month files and notes from disk (picks up data synced from other machines).
    pub fn reload(&mut self) -> Result<()> {
        self.flush()?;
        self.months.clear();
        self.load_all()?;
        self.load_notes();
        Ok(())
    }

    pub fn range(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Reading> {
        let (f, t) = (from.timestamp(), to.timestamp());
        self.months
            .range(month_of(from)..=month_of(to))
            .flat_map(|(_, m)| m.readings.range(f..=t))
            .filter_map(|(ts, (v, trend))| {
                Some(Reading { time: DateTime::from_timestamp(*ts, 0)?, mg_dl: *v as f32, trend: *trend })
            })
            .collect()
    }

    pub fn all(&self) -> Vec<Reading> {
        self.range(DateTime::from_timestamp(0, 0).unwrap(), Utc::now() + chrono::Duration::days(1))
    }

    pub fn len(&self) -> usize {
        self.months.values().map(|m| m.readings.len()).sum()
    }

    pub fn first_time(&self) -> Option<DateTime<Utc>> {
        self.months
            .values()
            .find_map(|m| m.readings.keys().next())
            .and_then(|ts| DateTime::from_timestamp(*ts, 0))
    }

    pub fn last_time(&self) -> Option<DateTime<Utc>> {
        self.months
            .values()
            .rev()
            .find_map(|m| m.readings.keys().next_back())
            .and_then(|ts| DateTime::from_timestamp(*ts, 0))
    }

    /// Changes (or removes, with `None`) the passphrase and re-encrypts every month file.
    pub fn rekey(&mut self, passphrase: Option<&str>) -> Result<()> {
        self.flush()?;
        let ids: Vec<MonthId> = self.months.keys().copied().collect();
        self.key = Self::write_meta(&self.dir, passphrase.filter(|p| !p.is_empty()))?;
        for id in ids {
            let readings = self.months[&id].readings.clone();
            self.write_month_file(id, &readings)?;
        }
        if self.notes_error.is_none() && !self.notes.is_empty() {
            self.write_notes_file()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ notes

    fn notes_path(&self) -> PathBuf {
        self.dir.join(NOTES_FILE)
    }

    fn read_notes_file(&self) -> Result<BTreeMap<String, Note>> {
        let path = self.notes_path();
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e.into()),
        };
        if data.len() < 6 || &data[..4] != MAGIC || data[4] != FORMAT_VERSION {
            bail!("{} is not a notes file", path.display());
        }
        let payload = match data[5] {
            CIPHER_NONE => data[6..].to_vec(),
            CIPHER_XCHACHA => {
                let key = self.key.as_ref().ok_or_else(|| anyhow!("{} is encrypted", path.display()))?;
                open_sealed(key, &data[6..], NOTES_AAD).with_context(|| path.display().to_string())?
            }
            c => bail!("unknown cipher {c} in {}", path.display()),
        };
        let notes: Vec<Note> = serde_json::from_slice(&payload).with_context(|| format!("parsing {}", path.display()))?;
        Ok(notes.into_iter().map(|n| (n.id.clone(), n)).collect())
    }

    fn write_notes_file(&self) -> Result<()> {
        let payload = serde_json::to_vec(&self.notes.values().collect::<Vec<_>>())?;
        let mut out = Vec::with_capacity(payload.len() + 64);
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        match &self.key {
            Some(key) => {
                out.push(CIPHER_XCHACHA);
                out.extend_from_slice(&seal(key, &payload, NOTES_AAD)?);
            }
            None => {
                out.push(CIPHER_NONE);
                out.extend_from_slice(&payload);
            }
        }
        atomic_write(&self.notes_path(), &out)
    }

    /// (Re)loads `notes.gllu`. A file that cannot be read (e.g. re-encrypted by an older app
    /// version) does not stop the log from opening; notes just stay read-only.
    fn load_notes(&mut self) {
        match self.read_notes_file() {
            Ok(notes) => {
                merge_notes(&mut self.notes, notes);
                self.notes_error = None;
            }
            Err(e) => {
                tracing::warn!("could not read notes: {e:#}");
                self.notes_error = Some(format!("{e:#}"));
            }
        }
    }

    fn notes_writable(&self) -> Result<()> {
        match &self.notes_error {
            Some(e) => bail!(tr!("the notes file could not be read ({})", e)),
            None => Ok(()),
        }
    }

    /// Notes in `[from, to]`, oldest first, without deleted ones.
    pub fn notes(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Note> {
        let (f, t) = (from.timestamp(), to.timestamp());
        let mut out: Vec<Note> =
            self.notes.values().filter(|n| !n.deleted && (f..=t).contains(&n.time)).cloned().collect();
        out.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
        out
    }

    pub fn all_notes(&self) -> Vec<Note> {
        let mut out: Vec<Note> = self.notes.values().filter(|n| !n.deleted).cloned().collect();
        out.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
        out
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.notes.get(id).filter(|n| !n.deleted)
    }

    /// Set when `notes.gllu` could not be read; notes are read-only then.
    pub fn notes_error(&self) -> Option<&str> {
        self.notes_error.as_deref()
    }

    pub fn note_count(&self) -> usize {
        self.notes.values().filter(|n| !n.deleted).count()
    }

    /// Adds a note and returns its id. Call [`Store::flush`] to write it.
    pub fn add_note(&mut self, time: DateTime<Utc>, text: &str, treatment: Option<Treatment>, carbs: Option<f32>) -> Result<String> {
        self.notes_writable()?;
        let text = clean_note_text(text);
        let carbs = check_carbs(carbs)?;
        if text.is_empty() && treatment.is_none() && carbs.is_none() {
            bail!(tr!("the note is empty"));
        }
        let id = hex::encode(random_bytes::<8>());
        let note = Note {
            id: id.clone(),
            time: time.timestamp(),
            text,
            updated: now_millis(),
            deleted: false,
            treatment,
            carbs,
            extra: Default::default(),
        };
        self.notes.insert(id.clone(), note);
        self.notes_dirty = true;
        Ok(id)
    }

    /// Changes the time, text, treatment and carbs of an existing note.
    pub fn edit_note(
        &mut self,
        id: &str,
        time: DateTime<Utc>,
        text: &str,
        treatment: Option<Treatment>,
        carbs: Option<f32>,
    ) -> Result<()> {
        self.notes_writable()?;
        let text = clean_note_text(text);
        let carbs = check_carbs(carbs)?;
        if text.is_empty() && treatment.is_none() && carbs.is_none() {
            bail!(tr!("the note is empty"));
        }
        let note = self.notes.get_mut(id).filter(|n| !n.deleted).ok_or_else(|| anyhow!("note not found"))?;
        note.time = time.timestamp();
        note.text = text;
        note.treatment = treatment;
        note.carbs = carbs;
        note.updated = now_millis().max(note.updated + 1);
        self.notes_dirty = true;
        Ok(())
    }

    pub fn delete_note(&mut self, id: &str) -> Result<()> {
        self.notes_writable()?;
        let note = self.notes.get_mut(id).filter(|n| !n.deleted).ok_or_else(|| anyhow!("note not found"))?;
        note.deleted = true;
        note.text.clear();
        note.treatment = None;
        note.carbs = None;
        // the tombstone keeps no content, not even fields of newer versions
        note.extra.clear();
        note.updated = now_millis().max(note.updated + 1);
        self.notes_dirty = true;
        Ok(())
    }

    /// Readings and notes in `range` (inclusive; everything with `None`).
    fn export_data(&self, range: Option<(DateTime<Utc>, DateTime<Utc>)>) -> (Vec<Reading>, Vec<Note>) {
        match range {
            Some((from, to)) => (self.range(from, to), self.notes(from, to)),
            None => (self.all(), self.all_notes()),
        }
    }

    /// The CSV export of `range` (everything with `None`) as bytes, with the number of readings
    /// and notes it contains.
    pub fn csv(&self, range: Option<(DateTime<Utc>, DateTime<Utc>)>) -> Result<(Vec<u8>, usize, usize)> {
        let (readings, notes) = self.export_data(range);
        let mut out = Vec::new();
        write_csv(&mut out, &readings, &notes)?;
        Ok((out, readings.len(), notes.len()))
    }

    /// Writes the readings and notes of `range` (everything with `None`), ordered by time. Note
    /// rows leave the glucose columns empty. Returns (readings, notes).
    pub fn export_csv(&self, path: &Path, range: Option<(DateTime<Utc>, DateTime<Utc>)>) -> Result<(usize, usize)> {
        let (data, readings, notes) = self.csv(range)?;
        let mut f = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        f.write_all(&data)?;
        f.flush()?;
        Ok((readings, notes))
    }

    /// Imports our own CSV export or a LibreView "Glucose Data" export (including its notes and
    /// insulin). Returns what was new.
    pub fn import_csv(&mut self, path: &Path) -> Result<Imported> {
        let text = std::fs::read_to_string(path)?;
        let data = parse_csv(&text)?;
        let readings = self.insert(&data.readings);
        let mut notes = 0;
        for n in &data.notes {
            let clean = clean_note_text(&n.text);
            let exists = self.notes.values().any(|o| {
                !o.deleted && o.time == n.time.timestamp() && o.text == clean && o.treatment == n.treatment && o.carbs == n.carbs
            });
            if !exists && self.add_note(n.time, &clean, n.treatment.clone(), n.carbs).is_ok() {
                notes += 1;
            }
        }
        self.flush()?;
        Ok(Imported { readings, notes })
    }
}

const NOTES_AAD: &[u8] = b"gllu:notes";

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// Quotes a CSV field when needed.
fn csv_field(s: &str) -> String {
    if s.contains([',', ';', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

pub const CSV_HEADER: &str = "timestamp_utc,local_time,glucose_mg_dl,glucose_mmol_l,trend,note,treatment,units,carbs_g";

fn write_csv(f: &mut impl Write, readings: &[Reading], notes: &[Note]) -> Result<()> {
    writeln!(f, "{CSV_HEADER}")?;
    let stamp = |t: DateTime<Utc>| {
        (t.format("%Y-%m-%dT%H:%M:%SZ").to_string(), t.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
    };
    let note_row = |f: &mut dyn Write, n: &Note| -> std::io::Result<()> {
        let (utc, local) = stamp(n.at());
        let (treatment, units) = match &n.treatment {
            Some(t) => (csv_field(t.label()), t.units.map(fmt_units).unwrap_or_default()),
            None => (String::new(), String::new()),
        };
        let carbs = n.carbs.map(fmt_units).unwrap_or_default();
        writeln!(f, "{utc},{local},,,,{},{treatment},{units},{carbs}", csv_field(&n.text))
    };
    let mut notes = notes.iter().peekable();
    for r in readings {
        while let Some(n) = notes.next_if(|n| n.time < r.time.timestamp()) {
            note_row(f, n)?;
        }
        let (utc, local) = stamp(r.time);
        writeln!(f, "{utc},{local},{:.0},{:.1},{},,,,", r.mg_dl, r.mg_dl / MMOL_FACTOR, r.trend)?;
    }
    for n in notes {
        note_row(f, n)?;
    }
    Ok(())
}

/// A number that may use a decimal comma; empty or invalid text is `None`.
fn parse_number(s: &str) -> Option<f32> {
    s.trim().replace(',', ".").parse::<f32>().ok().filter(|v| v.is_finite())
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn month_of(t: DateTime<Utc>) -> MonthId {
    (t.year(), t.month())
}

fn month_aad((y, m): MonthId) -> Vec<u8> {
    format!("bllu:{y:04}-{m:02}").into_bytes()
}

fn parse_month_name(name: &str, ext: &str) -> Option<MonthId> {
    let rest = name.strip_prefix("glucose-")?.strip_suffix(ext)?;
    let (y, m) = rest.split_once('-')?;
    let (y, m) = (y.parse().ok()?, m.parse().ok()?);
    (1..=12).contains(&m).then_some((y, m))
}

fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // "" inside a quoted field is a literal quote
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

fn parse_local_time(s: &str) -> Option<DateTime<Utc>> {
    const FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%d-%m-%Y %H:%M",
        "%m-%d-%Y %I:%M %p",
        "%d.%m.%Y %H:%M",
        "%d/%m/%Y %H:%M",
        "%m/%d/%Y %I:%M %p",
        "%m/%d/%Y %H:%M",
    ];
    FORMATS.iter().find_map(|f| {
        let n = NaiveDateTime::parse_from_str(s, f).ok()?;
        Local.from_local_datetime(&n).earliest().map(|t| t.with_timezone(&Utc))
    })
}

pub fn parse_csv(text: &str) -> Result<CsvData> {
    let mut lines = text.lines();
    let mut out = CsvData::default();
    // Our own export (the note column was added in 0.2.0, treatment, units and carbs in 1.1.0).
    let first = lines.next().unwrap_or_default().trim_start_matches('\u{feff}');
    if first.starts_with("timestamp_utc") {
        for line in lines {
            let cols = split_csv_line(line);
            if cols.len() < 3 {
                continue;
            }
            let Ok(time) = DateTime::parse_from_rfc3339(&cols[0]) else { continue };
            let time = time.with_timezone(&Utc);
            let col = |i: usize| cols.get(i).map(String::as_str).unwrap_or_default();
            let treatment = (!col(6).is_empty()).then(|| Treatment::from_label(col(6), parse_number(col(7))).ok()).flatten();
            let carbs = check_carbs(parse_number(col(8))).ok().flatten();
            if !col(5).is_empty() || treatment.is_some() || carbs.is_some() {
                out.notes.push(CsvNote { time, text: col(5).to_string(), treatment, carbs });
            }
            let Ok(v) = cols[2].parse::<f32>() else { continue };
            let trend = cols.get(4).and_then(|t| t.parse().ok()).unwrap_or(0);
            out.readings.push(Reading { time, mg_dl: v, trend });
        }
        return Ok(out);
    }
    // LibreView export: a metadata line, then a header with "Device Timestamp" and
    // "Historic Glucose mg/dL" (or mmol/L) and "Scan Glucose …" columns.
    let mut header: Option<Vec<String>> = None;
    for line in std::iter::once(first).chain(lines) {
        let cols = split_csv_line(line);
        let Some(h) = &header else {
            if cols.iter().any(|c| c.to_lowercase().contains("historic glucose")) {
                header = Some(cols);
            }
            continue;
        };
        let find = |needle: &str| h.iter().position(|c| c.to_lowercase().contains(needle));
        let (Some(ts_i), Some(hist_i)) = (find("timestamp"), find("historic glucose")) else {
            bail!("unsupported CSV header");
        };
        let scan_i = find("scan glucose");
        let notes_i = h.iter().position(|c| c.trim().eq_ignore_ascii_case("notes"));
        // "Rapid-Acting Insulin (units)", "Long-Acting Insulin Value (units)" – not the
        // "Non-numeric …" columns next to them
        let insulin = |needle: &str| {
            h.iter().position(|c| {
                let c = c.to_lowercase();
                c.contains(needle) && !c.starts_with("non-numeric")
            })
        };
        let insulin_cols = [(TreatmentKind::RapidActing, insulin("rapid-acting insulin")), (TreatmentKind::LongActing, insulin("long-acting insulin"))];
        let carbs_i = insulin("carbohydrates");
        let mmol = h[hist_i].to_lowercase().contains("mmol");
        let Some(time) = cols.get(ts_i).and_then(|s| parse_local_time(s)) else { continue };
        let mut text = notes_i.and_then(|i| cols.get(i)).cloned().unwrap_or_default();
        let mut row_notes = 0;
        for (kind, i) in insulin_cols {
            let Some(units) = i.and_then(|i| cols.get(i)).and_then(|v| parse_number(v)) else { continue };
            if let Ok(t) = Treatment::new(kind, "", Some(units)) {
                // the row's note text goes with its first treatment
                out.notes.push(CsvNote { time, text: std::mem::take(&mut text), treatment: Some(t), carbs: None });
                row_notes += 1;
            }
        }
        let carbs = carbs_i.and_then(|i| cols.get(i)).and_then(|v| check_carbs(parse_number(v)).ok().flatten());
        if row_notes == 0 && (!text.is_empty() || carbs.is_some()) {
            out.notes.push(CsvNote { time, text, treatment: None, carbs });
        } else if let Some(first) = carbs.and(out.notes.len().checked_sub(row_notes)) {
            // carbs in the same row as insulin go with the first treatment, like the text
            out.notes[first].carbs = carbs;
        }
        let value = [Some(hist_i), scan_i]
            .into_iter()
            .flatten()
            .filter_map(|i| cols.get(i))
            .find_map(|v| v.replace(',', ".").parse::<f32>().ok());
        if let Some(v) = value {
            let mg = if mmol { v * MMOL_FACTOR } else { v };
            out.readings.push(Reading { time, mg_dl: mg, trend: 0 });
        }
    }
    if header.is_none() {
        bail!("unrecognised CSV format (expected a genius-librelinkup or LibreView export)");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gllu-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn reading(ts: i64, v: f32) -> Reading {
        Reading { time: DateTime::from_timestamp(ts, 0).unwrap(), mg_dl: v, trend: 3 }
    }

    #[test]
    fn encrypted_roundtrip_and_wrong_passphrase() {
        let dir = tmpdir("enc");
        {
            let mut s = Store::open(&dir, Some("secret")).unwrap();
            assert!(s.is_encrypted());
            assert_eq!(s.insert(&[reading(1_767_225_600, 100.0), reading(1_767_225_660, 105.0)]), 2);
            assert_eq!(s.insert(&[reading(1_767_225_600, 100.0)]), 0);
            s.flush().unwrap();
        }
        assert_eq!(vault_state(&dir), VaultState::Encrypted);
        assert!(Store::open(&dir, Some("wrong")).is_err());
        let s = Store::open(&dir, Some("secret")).unwrap();
        assert_eq!(s.len(), 2);
        let file = std::fs::read(dir.join("glucose-2026-01.gllu")).unwrap();
        assert_eq!(file[5], CIPHER_XCHACHA);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_legacy_month_files() {
        let dir = tmpdir("legacy");
        let path = |month: &str, ext: &str| dir.join(format!("glucose-2026-{month}.{ext}"));
        {
            let mut s = Store::open(&dir, Some("secret")).unwrap();
            s.insert(&[reading(1_767_225_600, 100.0), reading(1_769_904_000, 110.0)]);
            s.flush().unwrap();
        }
        // January only exists as .bllu; February gets a different reading in each file
        std::fs::rename(path("01", "gllu"), path("01", "bllu")).unwrap();
        std::fs::rename(path("02", "gllu"), dir.join("feb-legacy")).unwrap();
        {
            let mut s = Store::open(&dir, Some("secret")).unwrap();
            s.insert(&[reading(1_769_904_060, 115.0)]);
            s.flush().unwrap();
        }
        std::fs::rename(dir.join("feb-legacy"), path("02", "bllu")).unwrap();

        let s = Store::open(&dir, Some("secret")).unwrap();
        assert_eq!(s.len(), 3);
        for month in ["01", "02"] {
            assert!(path(month, "gllu").exists() && !path(month, "bllu").exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merges_with_file_written_by_other_device() {
        let dir = tmpdir("merge");
        let mut a = Store::open(&dir, None).unwrap();
        let mut b = Store::open(&dir, None).unwrap();
        a.insert(&[reading(1_767_225_600, 100.0)]);
        a.flush().unwrap();
        b.insert(&[reading(1_767_225_900, 120.0)]);
        b.flush().unwrap();
        let c = Store::open(&dir, None).unwrap();
        assert_eq!(c.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_plain_to_encrypted() {
        let dir = tmpdir("rekey");
        let mut s = Store::open(&dir, None).unwrap();
        s.insert(&[reading(1_767_225_600, 90.0)]);
        s.flush().unwrap();
        s.rekey(Some("pw")).unwrap();
        assert!(Store::open(&dir, None).is_err());
        assert_eq!(Store::open(&dir, Some("pw")).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verifies_current_passphrase() {
        let dir = tmpdir("verify");
        let s = Store::open(&dir, Some("right")).unwrap();
        assert!(s.verify_passphrase("right").is_ok());
        assert!(s.verify_passphrase("wrong").is_err());
        assert!(s.verify_passphrase("").is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let plain = Store::open(&tmpdir("verify-plain"), None).unwrap();
        assert!(plain.verify_passphrase("anything").is_ok());
        let _ = std::fs::remove_dir_all(tmpdir("verify-plain"));
    }

    #[test]
    fn parses_libreview_csv() {
        let csv = "Glucose Data,Generated on,10-28-2026 10:00 AM UTC,Generated by,x\n\
            Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mmol/L,Scan Glucose mmol/L\n\
            FreeStyle LibreLink,ABC,27-10-2026 08:15,0,5.5,\n\
            FreeStyle LibreLink,ABC,27-10-2026 08:20,1,,6.1\n";
        let r = parse_csv(csv).unwrap().readings;
        assert_eq!(r.len(), 2);
        assert!((r[0].mg_dl - 99.1).abs() < 0.2);
    }

    #[test]
    fn parses_libreview_notes() {
        let csv = "Glucose Data,Generated on,10-28-2026 10:00 AM UTC,Generated by,x\n\
            Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,Scan Glucose mg/dL,Notes\n\
            FreeStyle LibreLink,ABC,27-10-2026 08:15,0,110,,\n\
            FreeStyle LibreLink,ABC,27-10-2026 08:30,6,,,\"Breakfast, 2 slices\"\n";
        let d = parse_csv(csv).unwrap();
        assert_eq!(d.readings.len(), 1);
        assert_eq!(d.notes.len(), 1);
        assert_eq!(d.notes[0].text, "Breakfast, 2 slices");
    }

    #[test]
    fn notes_roundtrip_merge_and_delete() {
        let dir = tmpdir("notes");
        let t = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        let (id_a, id_b);
        {
            let mut a = Store::open(&dir, Some("pw")).unwrap();
            let mut b = Store::open(&dir, Some("pw")).unwrap();
            id_a = a.add_note(t, "  Pizza \n large ", None, None).unwrap();
            a.flush().unwrap();
            // b never saw a's note but must not drop it when writing its own
            id_b = b.add_note(t + chrono::Duration::minutes(5), "Walk", None, None).unwrap();
            b.flush().unwrap();
        }
        let mut s = Store::open(&dir, Some("pw")).unwrap();
        assert_eq!(s.note_count(), 2);
        assert_eq!(s.note(&id_a).unwrap().text, "Pizza large");
        assert!(s.add_note(t, "   ", None, None).is_err());

        // a device holding an old copy must not bring a deleted note back
        let mut stale = Store::open(&dir, Some("pw")).unwrap();
        s.delete_note(&id_a).unwrap();
        s.edit_note(&id_b, t, "Long walk", None, None).unwrap();
        s.flush().unwrap();
        stale.add_note(t, "Other", None, None).unwrap();
        stale.flush().unwrap();
        let s = Store::open(&dir, Some("pw")).unwrap();
        assert!(s.note(&id_a).is_none());
        assert_eq!(s.note(&id_b).unwrap().text, "Long walk");
        assert_eq!(s.note_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_notes_stay_read_only() {
        let dir = tmpdir("notes-bad");
        let mut s = Store::open(&dir, None).unwrap();
        std::fs::write(dir.join(NOTES_FILE), b"garbage").unwrap();
        s.reload().unwrap();
        assert!(s.add_note(Utc::now(), "x", None, None).is_err());
        assert_eq!(std::fs::read(dir.join(NOTES_FILE)).unwrap(), b"garbage");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn csv_roundtrip_with_notes() {
        let dir = tmpdir("csv");
        let mut s = Store::open(&dir, None).unwrap();
        s.insert(&[reading(1_767_225_600, 100.0), reading(1_767_225_900, 120.0)]);
        s.add_note(DateTime::from_timestamp(1_767_225_700, 0).unwrap(), "Juice, \"small\"", None, None).unwrap();
        s.add_note(DateTime::from_timestamp(1_767_226_000, 0).unwrap(), "Bed", None, None).unwrap();
        s.flush().unwrap();
        let file = dir.join("export.csv");
        assert_eq!(s.export_csv(&file, None).unwrap(), (2, 2));
        let text = std::fs::read_to_string(&file).unwrap();
        let data = parse_csv(&text).unwrap();
        assert_eq!(data.readings.len(), 2);
        assert_eq!(data.notes.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["Juice, \"small\"", "Bed"]);

        let other = tmpdir("csv-import");
        let mut t = Store::open(&other, None).unwrap();
        assert_eq!(t.import_csv(&file).unwrap(), Imported { readings: 2, notes: 2 });
        assert_eq!(t.import_csv(&file).unwrap(), Imported::default());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn treatment_validation_and_describe() {
        use TreatmentKind::*;
        let t = Treatment::new(RapidActing, "ignored", Some(4.504)).unwrap();
        assert_eq!((t.name.as_str(), t.units), ("", Some(4.5)));
        assert_eq!(t.describe(), "Rapid-acting insulin: 4.5 u");
        assert_eq!(Treatment::new(LongActing, "", None).unwrap().describe(), "Long-acting insulin");
        assert_eq!(Treatment::new(Custom, "  Metformin ", Some(500.0)).unwrap().describe(), "Metformin: 500 u");
        assert!(Treatment::new(Custom, "  ", None).is_err());
        assert!(Treatment::new(Custom, &"x".repeat(61), None).is_err());
        for bad in [0.0, -1.0, 1000.5, f32::NAN, f32::INFINITY] {
            assert!(Treatment::new(RapidActing, "", Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(Treatment::from_label("long-ACTING insulin", Some(12.0)).unwrap().kind, LongActing);
        assert_eq!(Treatment::from_label("Glucagon", None).unwrap().label(), "Glucagon");
        assert_eq!((fmt_units(12.0), fmt_units(0.25), fmt_units(4.5)), ("12".into(), "0.25".into(), "4.5".into()));

        let mut n = Note {
            id: "a".into(),
            time: 0,
            text: "Breakfast".into(),
            updated: 0,
            deleted: false,
            treatment: Some(t),
            carbs: None,
            extra: Default::default(),
        };
        assert_eq!(n.summary(), "Rapid-acting insulin: 4.5 u · Breakfast");
        n.text.clear();
        assert_eq!(n.summary(), "Rapid-acting insulin: 4.5 u");
        n.carbs = Some(45.0);
        n.text = "Breakfast".into();
        assert_eq!(n.summary(), "Rapid-acting insulin: 4.5 u · 45 g carbs · Breakfast");
        n.treatment = None;
        n.text.clear();
        assert_eq!(n.summary(), "45 g carbs");
        assert_eq!(check_carbs(Some(12.34)).unwrap(), Some(12.3));
        assert!(check_carbs(Some(0.0)).is_err() && check_carbs(Some(1001.0)).is_err());
    }

    #[test]
    fn notes_with_treatment_only() {
        let dir = tmpdir("treatment-only");
        let mut s = Store::open(&dir, None).unwrap();
        let t = Some(Treatment::new(TreatmentKind::LongActing, "", Some(18.0)).unwrap());
        let id = s.add_note(Utc::now(), " ", t.clone(), None).unwrap();
        assert_eq!(s.note(&id).unwrap().treatment, t);
        assert!(s.edit_note(&id, Utc::now(), "", None, None).is_err());
        s.delete_note(&id).unwrap();
        assert!(s.notes.get(&id).is_some_and(|n| n.treatment.is_none() && n.deleted));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_libreview_insulin() {
        let csv = "Glucose Data,Generated on,10-28-2026 10:00 AM UTC,Generated by,x\n\
            Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,Scan Glucose mg/dL,\
            Non-numeric Rapid-Acting Insulin,Rapid-Acting Insulin (units),Non-numeric Food,Carbohydrates (grams),\
            Non-numeric Long-Acting Insulin,Long-Acting Insulin Value (units),Notes\n\
            FreeStyle LibreLink,ABC,27-10-2026 08:15,0,110,,,,,,,,\n\
            FreeStyle LibreLink,ABC,27-10-2026 08:30,4,,,,\"4,5\",,,,,Breakfast\n\
            FreeStyle LibreLink,ABC,27-10-2026 22:00,4,,,,2,,,,18,\n\
            FreeStyle LibreLink,ABC,27-10-2026 23:00,4,,,X,,,,,,\n\
            FreeStyle LibreLink,ABC,27-10-2026 12:00,5,,,,,Pasta,\"60,5\",,,Lunch\n\
            FreeStyle LibreLink,ABC,27-10-2026 12:05,4,,,,3,,40,,,\n";
        let d = parse_csv(csv).unwrap();
        assert_eq!(d.readings.len(), 1);
        let got: Vec<String> = d.notes.iter().map(|n| Note {
            id: String::new(),
            time: 0,
            text: n.text.clone(),
            updated: 0,
            deleted: false,
            treatment: n.treatment.clone(),
            carbs: n.carbs,
            extra: Default::default(),
        }.summary()).collect();
        assert_eq!(
            got,
            [
                "Rapid-acting insulin: 4.5 u · Breakfast",
                "Rapid-acting insulin: 2 u",
                "Long-acting insulin: 18 u",
                "60.5 g carbs · Lunch",
                "Rapid-acting insulin: 3 u · 40 g carbs",
            ]
        );
    }

    #[test]
    fn csv_roundtrip_with_custom_treatment() {
        let dir = tmpdir("csv-treatment");
        let mut s = Store::open(&dir, None).unwrap();
        s.insert(&[reading(1_767_225_600, 100.0)]);
        let custom = Treatment::new(TreatmentKind::Custom, "Inhaled, \"fast\"", Some(8.25)).unwrap();
        s.add_note(DateTime::from_timestamp(1_767_225_700, 0).unwrap(), "", Some(custom.clone()), Some(30.0)).unwrap();
        s.add_note(DateTime::from_timestamp(1_767_225_500, 0).unwrap(), "Walk", None, None).unwrap();
        let file = dir.join("export.csv");
        s.export_csv(&file, None).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(CSV_HEADER));
        let data = parse_csv(&text).unwrap();
        assert_eq!(data.notes.len(), 2);
        assert_eq!(data.notes[1].treatment.as_ref(), Some(&custom));
        assert_eq!((data.notes[0].carbs, data.notes[1].carbs), (None, Some(30.0)));
        // files of 0.2.0 without the treatment columns still parse
        let old = "timestamp_utc,local_time,glucose_mg_dl,glucose_mmol_l,trend,note\n\
            2026-01-01T00:00:00Z,x,100,5.5,3,\n2026-01-01T00:01:00Z,x,,,,Bed\n";
        let data = parse_csv(old).unwrap();
        assert_eq!((data.readings.len(), data.notes[0].text.as_str()), (1, "Bed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_unknown_note_fields() {
        let dir = tmpdir("note-extra");
        {
            let s = Store::open(&dir, None).unwrap();
            let json = r#"[{"id":"n1","time":1767225600,"text":"Run","updated":1,"mood":"good"}]"#;
            let mut out = MAGIC.to_vec();
            out.extend_from_slice(&[FORMAT_VERSION, CIPHER_NONE]);
            out.extend_from_slice(json.as_bytes());
            std::fs::write(s.notes_path(), out).unwrap();
        }
        let mut s = Store::open(&dir, None).unwrap();
        s.edit_note("n1", DateTime::from_timestamp(1_767_225_600, 0).unwrap(), "Long run", None, None).unwrap();
        s.flush().unwrap();
        let s = Store::open(&dir, None).unwrap();
        let n = s.note("n1").unwrap();
        assert_eq!((n.text.as_str(), n.extra.get("mood")), ("Long run", Some(&serde_json::json!("good"))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
