//! Glucose log: one file per month (`glucose-YYYY-MM.bllu`) inside a user chosen folder, so the
//! folder can live in Nextcloud/Dropbox/OneDrive. Files are optionally encrypted with
//! XChaCha20-Poly1305 using a key derived from a passphrase with Argon2id (`vault.json` keeps
//! the salt and a check value). Before a month is written the file on disk is merged in, so two
//! machines syncing the same folder do not lose each other's readings.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, Generate, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::api::Reading;

const MAGIC: &[u8; 4] = b"BLLU";
const FORMAT_VERSION: u8 = 1;
const CIPHER_NONE: u8 = 0;
const CIPHER_XCHACHA: u8 = 1;
const RECORD_LEN: usize = 11;
const CHECK_PLAINTEXT: &[u8] = b"better-librelinkup vault check";
pub const MMOL_FACTOR: f32 = 18.0182;

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
        .map_err(|_| anyhow!("wrong passphrase or corrupted file"))
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
                    let pass = passphrase.ok_or_else(|| anyhow!("passphrase required"))?;
                    let salt = hex::decode(&meta.salt)?;
                    let key = derive_key(pass, &salt, meta.m_cost, meta.t_cost, meta.p_cost)?;
                    open_sealed(&key, &hex::decode(&meta.check)?, b"check")
                        .map_err(|_| anyhow!("wrong passphrase"))?;
                    Some(key)
                } else {
                    None
                }
            }
            Err(_) => Self::write_meta(dir, passphrase.filter(|p| !p.is_empty()))?,
        };
        let mut store = Store { dir: dir.to_path_buf(), key, months: BTreeMap::new() };
        store.load_all()?;
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
            bail!("wrong passphrase");
        }
        Ok(())
    }

    fn month_path(&self, (y, m): MonthId) -> PathBuf {
        self.dir.join(format!("glucose-{y:04}-{m:02}.bllu"))
    }

    fn load_all(&mut self) -> Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if let Some(id) = parse_month_name(&name) {
                let readings = self.read_month_file(id)?;
                self.months.entry(id).or_default().readings.extend(readings);
            }
        }
        Ok(())
    }

    fn read_month_file(&self, id: MonthId) -> Result<BTreeMap<i64, (u16, u8)>> {
        let path = self.month_path(id);
        let data = match std::fs::read(&path) {
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
        Ok(())
    }

    /// Re-reads month files from disk (picks up data synced from other machines).
    pub fn reload(&mut self) -> Result<()> {
        self.flush()?;
        self.months.clear();
        self.load_all()
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
        Ok(())
    }

    pub fn export_csv(&self, path: &Path) -> Result<usize> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(f, "timestamp_utc,local_time,glucose_mg_dl,glucose_mmol_l,trend")?;
        let all = self.all();
        for r in &all {
            writeln!(
                f,
                "{},{},{:.0},{:.1},{}",
                r.time.format("%Y-%m-%dT%H:%M:%SZ"),
                r.time.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"),
                r.mg_dl,
                r.mg_dl / MMOL_FACTOR,
                r.trend
            )?;
        }
        f.flush()?;
        Ok(all.len())
    }

    /// Imports our own CSV export or a LibreView "Glucose Data" export. Returns new readings.
    pub fn import_csv(&mut self, path: &Path) -> Result<usize> {
        let text = std::fs::read_to_string(path)?;
        let readings = parse_csv(&text)?;
        let n = self.insert(&readings);
        self.flush()?;
        Ok(n)
    }
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

fn parse_month_name(name: &str) -> Option<MonthId> {
    let rest = name.strip_prefix("glucose-")?.strip_suffix(".bllu")?;
    let (y, m) = rest.split_once('-')?;
    let (y, m) = (y.parse().ok()?, m.parse().ok()?);
    (1..=12).contains(&m).then_some((y, m))
}

fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
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

pub fn parse_csv(text: &str) -> Result<Vec<Reading>> {
    let mut lines = text.lines();
    let mut out = Vec::new();
    // Our own export.
    let first = lines.next().unwrap_or_default();
    if first.starts_with("timestamp_utc") {
        for line in lines {
            let cols = split_csv_line(line);
            if cols.len() < 3 {
                continue;
            }
            let Ok(time) = DateTime::parse_from_rfc3339(&cols[0]) else { continue };
            let Ok(v) = cols[2].parse::<f32>() else { continue };
            let trend = cols.get(4).and_then(|t| t.parse().ok()).unwrap_or(0);
            out.push(Reading { time: time.with_timezone(&Utc), mg_dl: v, trend });
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
        let mmol = h[hist_i].to_lowercase().contains("mmol");
        let Some(time) = cols.get(ts_i).and_then(|s| parse_local_time(s)) else { continue };
        let value = [Some(hist_i), scan_i]
            .into_iter()
            .flatten()
            .filter_map(|i| cols.get(i))
            .find_map(|v| v.replace(',', ".").parse::<f32>().ok());
        if let Some(v) = value {
            let mg = if mmol { v * MMOL_FACTOR } else { v };
            out.push(Reading { time, mg_dl: mg, trend: 0 });
        }
    }
    if header.is_none() {
        bail!("unrecognised CSV format (expected a better-librelinkup or LibreView export)");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bllu-test-{name}-{}", std::process::id()));
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
        let file = std::fs::read(dir.join("glucose-2026-01.bllu")).unwrap();
        assert_eq!(file[5], CIPHER_XCHACHA);
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
        let r = parse_csv(csv).unwrap();
        assert_eq!(r.len(), 2);
        assert!((r[0].mg_dl - 99.1).abs() < 0.2);
    }
}
