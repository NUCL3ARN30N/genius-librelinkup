//! In-app updates from GitHub releases.
//!
//! Every release must contain `SHA256SUMS.txt`; a download is only installed when its hash
//! matches. Depending on how the app was installed the update is applied differently:
//! * Windows, installed with the setup: the new `*-setup.exe` runs silently and restarts the app.
//! * Windows, portable: the running exe is renamed to `*.old` and replaced by the new one.
//! * Linux `.deb` in `/usr/bin`: installed with `pkexec apt-get install` (graphical password prompt).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const REPO: &str = "NUCL3ARN30N/better-librelinkup";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const SUMS: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches(['v', 'V'])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    WindowsInstaller,
    WindowsPortable,
    LinuxDeb,
    /// Built from source / unknown layout: we can only point to the download page.
    Manual,
}

impl InstallKind {
    pub fn describe(&self) -> &'static str {
        match self {
            InstallKind::WindowsInstaller => "installed version",
            InstallKind::WindowsPortable => "portable version",
            InstallKind::LinuxDeb => "Debian package",
            InstallKind::Manual => "manual install",
        }
    }
}

pub fn install_kind() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else { return InstallKind::Manual };
    if cfg!(debug_assertions) || exe.components().any(|c| c.as_os_str() == "target") {
        return InstallKind::Manual;
    }
    if cfg!(windows) {
        let dir = exe.parent().unwrap_or(Path::new("."));
        if dir.join("unins000.exe").exists() { InstallKind::WindowsInstaller } else { InstallKind::WindowsPortable }
    } else if cfg!(target_os = "linux") && exe.starts_with("/usr/bin") && cfg!(target_arch = "x86_64") {
        InstallKind::LinuxDeb
    } else {
        InstallKind::Manual
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("better-librelinkup/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

/// `1.2.10` > `1.2.9`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> Vec<u64> {
        v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parse(candidate), parse(current));
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Newest published release. `Ok(None)` when there is none yet.
pub async fn latest() -> Result<Option<Release>> {
    let resp = http()
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("cannot reach GitHub")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("GitHub answered {}", resp.status());
    }
    Ok(Some(resp.json().await?))
}

pub fn pick_asset(release: &Release, kind: InstallKind) -> Option<&Asset> {
    let suffix = match kind {
        InstallKind::WindowsInstaller => "-setup.exe",
        InstallKind::WindowsPortable => "-windows-x64.exe",
        InstallKind::LinuxDeb => "_amd64.deb",
        InstallKind::Manual => return None,
    };
    release.assets.iter().find(|a| a.name.ends_with(suffix))
}

fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let hash = parts.next()?.to_lowercase();
            let name = parts.next()?.trim_start_matches('*').to_string();
            (hash.len() == 64).then_some((name, hash))
        })
        .collect()
}

/// Downloads the asset into a temp folder and verifies it against `SHA256SUMS.txt`.
pub async fn download(release: &Release, asset: &Asset, progress: impl Fn(f32)) -> Result<PathBuf> {
    let client = http();
    let sums_asset = release
        .assets
        .iter()
        .find(|a| a.name == SUMS)
        .ok_or_else(|| anyhow!("the release has no {SUMS}; refusing to install an unverified download"))?;
    let sums = client.get(&sums_asset.browser_download_url).send().await?.error_for_status()?.text().await?;
    let expected = parse_sums(&sums)
        .into_iter()
        .find(|(name, _)| name == &asset.name)
        .map(|(_, h)| h)
        .ok_or_else(|| anyhow!("{} is not listed in {SUMS}", asset.name))?;

    let dir = std::env::temp_dir().join("better-librelinkup-update");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&asset.name);
    let mut resp = client.get(&asset.browser_download_url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(asset.size).max(1);
    let mut hasher = Sha256::new();
    let mut data = Vec::with_capacity(total as usize);
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        data.extend_from_slice(&chunk);
        progress((data.len() as f32 / total as f32).min(1.0));
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        bail!("checksum mismatch for {} – the download was not installed", asset.name);
    }
    std::fs::write(&path, &data)?;
    Ok(path)
}

/// Starts the installation. Returns `true` when the app must quit now (the new version is
/// started by the installer / by us).
pub fn install(kind: InstallKind, file: &Path) -> Result<bool> {
    match kind {
        InstallKind::WindowsInstaller => {
            std::process::Command::new(file)
                .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"])
                .spawn()
                .context("starting the installer")?;
            Ok(true)
        }
        InstallKind::WindowsPortable => {
            let exe = std::env::current_exe()?;
            let old = exe.with_extension("exe.old");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&exe, &old).context("cannot replace the running program (is the folder writable?)")?;
            if let Err(e) = std::fs::copy(file, &exe) {
                let _ = std::fs::rename(&old, &exe);
                return Err(e).context("copying the new version");
            }
            std::process::Command::new(&exe).arg("--updated").spawn().context("starting the new version")?;
            Ok(true)
        }
        InstallKind::LinuxDeb => {
            let status = std::process::Command::new("pkexec")
                .args(["apt-get", "install", "-y", "--allow-downgrades"])
                .arg(file)
                .status()
                .context("pkexec is needed to install the package")?;
            if !status.success() {
                bail!("the package installation was cancelled or failed ({status})");
            }
            std::process::Command::new("/usr/bin/better-librelinkup").arg("--updated").spawn()?;
            Ok(true)
        }
        InstallKind::Manual => bail!("this copy was not installed from a release; download the update manually"),
    }
}

/// Removes the `*.old` exe left behind by a portable update.
pub fn cleanup_after_update() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("better-librelinkup-update"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.10", "1.0.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn parses_sha256sums() {
        let s = format!("{}  a.exe\n{} *b.deb\nnonsense\n", "a".repeat(64), "B".repeat(64));
        let p = parse_sums(&s);
        assert_eq!(p[0], ("a.exe".to_string(), "a".repeat(64)));
        assert_eq!(p[1], ("b.deb".to_string(), "b".repeat(64)));
    }

    #[test]
    fn picks_matching_asset() {
        let mk = |n: &str| Asset { name: n.into(), browser_download_url: String::new(), size: 0 };
        let r = Release {
            tag_name: "v0.2.0".into(),
            body: None,
            html_url: String::new(),
            assets: vec![
                mk("better-librelinkup-0.2.0-setup.exe"),
                mk("better-librelinkup-0.2.0-windows-x64.exe"),
                mk("better-librelinkup_0.2.0-1_amd64.deb"),
            ],
        };
        assert_eq!(r.version(), "0.2.0");
        assert!(pick_asset(&r, InstallKind::WindowsInstaller).unwrap().name.ends_with("setup.exe"));
        assert!(pick_asset(&r, InstallKind::WindowsPortable).unwrap().name.ends_with("x64.exe"));
        assert!(pick_asset(&r, InstallKind::LinuxDeb).unwrap().name.ends_with(".deb"));
        assert!(pick_asset(&r, InstallKind::Manual).is_none());
    }
}
