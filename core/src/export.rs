//! Password-protected exports: an AES-256 ZIP (WinZip AE-2, opens with 7-Zip, WinRAR or Keka) so
//! glucose data can travel by email.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Local, Timelike};

/// Shortest accepted ZIP password (characters).
pub const MIN_PASSWORD_CHARS: usize = 8;

/// Checks the password typed twice in the export dialog.
pub fn check_password(password: &str, repeat: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        bail!("the password needs at least {MIN_PASSWORD_CHARS} characters");
    }
    if password != repeat {
        bail!("the passwords do not match");
    }
    Ok(())
}

/// The ZIP as bytes: every `(name, data)` entry deflated and encrypted with AES-256.
pub fn encrypted_zip(entries: &[(&str, &[u8])], password: &str) -> Result<Vec<u8>> {
    let now = Local::now();
    let modified = zip::DateTime::from_date_and_time(
        now.year().clamp(1980, 2107) as u16,
        now.month() as u8,
        now.day() as u8,
        now.hour() as u8,
        now.minute() as u8,
        now.second() as u8,
    )
    .unwrap_or_default();
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(modified)
        .with_aes_encryption(zip::AesMode::Aes256, password);
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        zip.start_file(*name, options).with_context(|| format!("adding {name}"))?;
        zip.write_all(data)?;
    }
    Ok(zip.finish()?.into_inner())
}

/// Writes [`encrypted_zip`] to `path`.
pub fn write_encrypted_zip(path: &Path, entries: &[(&str, &[u8])], password: &str) -> Result<()> {
    let data = encrypted_zip(entries, password)?;
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn password_rules() {
        assert!(check_password("short", "short").is_err());
        assert!(check_password("long enough", "long enougH").is_err());
        assert!(check_password("long enough", "long enough").is_ok());
    }

    #[test]
    fn opens_with_the_right_password_only() {
        let csv = b"timestamp_utc,local_time,glucose_mg_dl\n2026-01-01T00:00:00Z,x,123\n";
        let zip = encrypted_zip(&[("glucose.csv", csv), ("report.pdf", b"%PDF-1.7")], "correct horse").unwrap();
        // the plaintext must not be visible in the file
        assert!(!zip.windows(13).any(|w| w == b"timestamp_utc"));
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&zip)).unwrap();
        assert_eq!(archive.len(), 2);
        assert!(archive.by_name_decrypt("glucose.csv", b"wrong password").is_err());
        let mut out = Vec::new();
        archive.by_name_decrypt("glucose.csv", b"correct horse").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, csv);
    }
}
