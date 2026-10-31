# Genius LibreLinkUp

[![GitHub stars](https://img.shields.io/github/stars/NUCL3ARN30N/genius-librelinkup?style=for-the-badge)](https://github.com/NUCL3ARN30N/genius-librelinkup/stargazers)
[![GitHub forks](https://img.shields.io/github/forks/NUCL3ARN30N/genius-librelinkup?style=for-the-badge)](https://github.com/NUCL3ARN30N/genius-librelinkup/network/members)
[![GitHub issues](https://img.shields.io/github/issues/NUCL3ARN30N/genius-librelinkup?style=for-the-badge)](https://github.com/NUCL3ARN30N/genius-librelinkup/issues)
[![GitHub license](https://img.shields.io/github/license/NUCL3ARN30N/genius-librelinkup?style=for-the-badge)](LICENSE)

![Rust](https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white)
![Slint](https://img.shields.io/badge/Slint-2379F4?style=for-the-badge&logo=slint&logoColor=white)
![Windows](https://img.shields.io/badge/Windows-0078D6?style=for-the-badge&logo=windows&logoColor=white)
![Linux](https://img.shields.io/badge/Linux-FCC624?style=for-the-badge&logo=linux&logoColor=black)
![Docker](https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white)

> Unofficial desktop companion for LibreLinkUp (FreeStyle Libre followers) — tray glucose readout, alerts, encrypted log and LibreView-style reports

> ⚠️ Not affiliated with Abbott. It uses the undocumented LibreLinkUp API, which can change at any
> time. Never make treatment decisions based on this app – use your sensor reader / official app.

## Quick Start

Download the installer, portable exe or `.deb` from the
[latest release](https://github.com/NUCL3ARN30N/genius-librelinkup/releases), sign in with your
LibreLinkUp account, and the current value appears in the tray.

```
genius-librelinkup              # normal start
genius-librelinkup --minimized  # start in the tray (used by autostart)
genius-librelinkup --dev        # developer mode (API log)
genius-librelinkup --demo       # synthetic data, no account, writes nothing permanent
genius-librelinkup --probe      # terminal API check, see below
genius-librelinkup --tray-preview <dir>   # write magnified tray icon sheets (16/20/24/32 px)
```

## Features

| #  | Feature                        | Description                                                                                                                                                                                                                                                                                                                                                                                                              |
| -- | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1  | **Tray icon**                  | The current value (`6.2`, `112`, `LO`/`HI`) with a trend arrow, coloured by range. Rendered with real system fonts at the exact pixel size the tray uses; arrows snap to whole pixels so they stay readable at 16 px. Choose the shape (number only / drop / circle / square), text colour, font (condensed / regular / mono) and whether the arrow is shown – by default as a second tray icon, so the number keeps the full width. The value is centred on the actual glyphs of the installed font. The tooltip adds the trend text and the age of the reading |
| 2  | **Customisable UI**            | Appearance page, applied live: accent colour, glucose range colours, corner style, filled or outlined value card, show/hide statistics card, graph span (3/6/12/24 h) and style (line, dots or both)                                                                                                                                                                                                                     |
| 3  | **Dashboard**                  | Current value, trend, a last-12-hours graph with the target band, and 24 h time-in-range / average / CV. Point at the graph (any style) for the time and value of a reading                                                                                                                                                                                                                                             |
| 3a | **Notes**                      | Add a note at any time – with "Add note" or by clicking the graph. Notes show as markers on the dashboard graph, in the day details and in the daily-profile calendar; pointing at a marker shows the text, clicking it edits or deletes the note. Stored in the log folder (encrypted with the log, merged between computers), and part of the CSV export/import                                                           |
| 4  | **Notifications**              | Low, high, and "no new data" alerts, each with its own toggle and a configurable repeat interval                                                                                                                                                                                                                                                                                                                         |
| 5  | **Glucose log**                | Optional, with LibreView-style reports for 7/14/30/90 days (see [Reports](#reports))                                                                                                                                                                                                                                                                                                                                     |
| 6  | **Encrypted log files**        | XChaCha20-Poly1305 with Argon2id key derivation, in a **folder you choose**. Put it in Nextcloud/OneDrive/Dropbox to share it between machines — each monthly file is merged with the copy on disk before writing, so two PCs syncing the same folder don't lose readings                                                                                                                                                |
| 7  | **Passphrase protection**      | Changing or removing the log passphrase needs the current passphrase, so an unlocked session alone is not enough                                                                                                                                                                                                                                                                                                         |
| 8  | **Delete all local data**      | Removes settings, app/API logs, the keychain entries, the autostart entry and optionally the glucose log files (only the app's own files — other files in a synced folder stay), then closes the app                                                                                                                                                                                                                     |
| 9  | **Import/export**              | Settings as JSON, readings and notes as CSV. Imports LibreView's "Download glucose data" CSV (including its notes) to backfill history beyond the 12 hours LibreLinkUp provides                                                                                                                                                                                                                                         |
| 10 | **Automatic server discovery** | Reads Abbott's live `regionalMap` (`/llu/config/country`), follows login redirects to the right region, and caches the result. Manual server override available                                                                                                                                                                                                                                                          |
| 11 | **Developer mode**             | In-app API log with redacted requests and responses, also written to a daily rolling log file                                                                                                                                                                                                                                                                                                                            |
| 12 | **Caching server**             | See [Caching Server](#caching-server)                                                                                                                                                                                                                                                                                                                                                                                    |
| 13 | **Built-in updater**           | Checks the [GitHub releases](https://github.com/NUCL3ARN30N/genius-librelinkup/releases) every 12 h (or on demand) and updates in place: silent installer run, portable exe swap, or `apt` via `pkexec` for the `.deb`. Downloads are only installed when they match the release's `SHA256SUMS.txt`                                                                                                                      |
| 14 | **OS keychain**                | Credentials, the log passphrase and the server token live in Windows Credential Manager or Secret Service, never in plain files                                                                                                                                                                                                                                                                                          |
| 15 | **Desktop niceties**           | Autostart, start-minimized, light/dark/system theme, mmol/L or mg/dL units                                                                                                                                                                                                                                                                                                                                               |
| 16 | **About page**                 | Version, what's new (from [CHANGELOG.md](CHANGELOG.md)) and links to this repository, issues, releases, documentation and the licence                                                                                                                                                                                                                                                                                    |

## Reports

| Report                         | Contents                                                                                                       |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------- |
| **Glucose statistics**         | % sensor active, average, **GMI**, **estimated HbA1c** (ADAG), CV and SD                                       |
| **Time in ranges**             | Very low / low / target / high / very high                                                                     |
| **Ambulatory glucose profile** | AGP median, 25–75 % and 5–95 % percentile bands                                                                |
| **Pattern insights**           | Traffic lights per time of day                                                                                 |
| **Daily profiles**             | Calendar (Monday first, only days with data); point at a day to read values and notes, click it for its 24-hour graph, statistics, time in ranges and notes |

## Caching Server

An always-on `gllu-server` ([`server/`](server/README.md), Docker / LXC) polls LibreLinkUp while your
computer is off. Paired apps fill the gaps automatically and can take their live values from it
instead of logging in themselves.

| Layer         | Protection                                                                                             |
| ------------- | ------------------------------------------------------------------------------------------------------ |
| **Transport** | TLS 1.3 with mutual authentication                                                                     |
| **Pairing**   | Server certificate pinned via a one-time pairing code                                                  |
| **Clients**   | Every computer gets its own key pair plus a rotating token bound to it                                 |
| **Hardening** | Brute-force bans, an IP allowlist and an audit log — see [server/README.md](server/README.md#security) |

## LibreLinkUp API Notes

State: October 2026.

| Topic                        | Handling                                                                                                                                                                                 |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Client identity**          | `product: llu.android`, `version: 5.0.1`, `User-Agent: LibreLinkUp/5.0.1` (configurable). If the server returns status `920`, the app adopts the `minimumVersion` it reports and retries |
| **`Account-Id` header**      | SHA-256 hex of `user.id`. Required since the 2024/2025 API changes                                                                                                                       |
| **Token lifetime**           | About 1 h with the v5 identity. The app re-logs in automatically using the keychain password                                                                                             |
| **Login steps (status `4`)** | Terms of Use / Privacy Policy re-acceptance (`data.step.type` = `tou`/`pp`). The app asks you first; only then does it `POST /auth/continue/{type}`                                      |
| **Servers**                  | `api.libreview.io` (global), `api-{us,eu,eu2,de,fr,jp,ap,au,ae,ca,la}.libreview.io` from the live regional map, plus `api.libreview.ru` and `api-cn.myfreestyle.cn`                      |
| **Rate limits**              | HTTP/body status `429` is respected (`Retry-After`, bounded to 15 min). The default poll interval is 60 s                                                                                |

References: [PaulCoughlin/llu](https://github.com/PaulCoughlin/llu),
[smpurkis/libre-linkup-py](https://github.com/smpurkis/libre-linkup-py),
[nightscout-connect#73](https://github.com/nightscout/nightscout-connect/pull/73),
[nightscout-clock#236](https://github.com/ktomy/nightscout-clock/pull/236) / [#237](https://github.com/ktomy/nightscout-clock/pull/237).

### Testing the API from a Terminal

`--probe` runs server discovery, login, connections and graph, then prints a redacted summary.
Credentials come only from environment variables:

```powershell
$env:LLU_EMAIL = "you@example.com"; $env:LLU_PASSWORD = Read-Host -MaskInput "Password"
genius-librelinkup --probe | Out-Host     # the pipe makes PowerShell wait for the (GUI) exe's output
Remove-Item Env:LLU_PASSWORD
# optional: $env:LLU_SERVER = "https://api-eu2.libreview.io"; $env:LLU_DUMP = 1 (prints redacted responses)
```

## Data Locations

| Data                      | Windows                                                                                                               | Linux                                        |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| **Settings**              | `%APPDATA%\genius-librelinkup\genius-librelinkup\config\settings.json`                                                | `~/.config/genius-librelinkup/settings.json` |
| **Glucose log (default)** | `%APPDATA%\genius-librelinkup\genius-librelinkup\data\log\` | `~/.local/share/genius-librelinkup/log/`                |                                              |
| **App/API log**           | `%LOCALAPPDATA%\genius-librelinkup\genius-librelinkup\data\app-logs\` | `~/.local/share/genius-librelinkup/app-logs/` |                                              |

Log format: `vault.json` (KDF parameters, salt, check value) plus one `glucose-YYYY-MM.gllu` file per
month: magic `BLLU` (kept from the former name), version, cipher byte, then a 24-byte nonce and ciphertext of 11-byte records
(`i64` unix time, `u16` mg/dL, `u8` trend). Notes live in `notes.gllu` in the same container with a JSON
payload (`id`, `time`, `text`, `updated`, `deleted`); deletions are kept as tombstones so a synced copy
on another computer does not bring a note back.

CSV export: `timestamp_utc,local_time,glucose_mg_dl,glucose_mmol_l,trend,note`, ordered by time. Note
rows leave the glucose columns empty.

Setting `GLLU_HOME=<dir>` keeps settings, log and app logs in that folder and uses a separate
keychain namespace. Use it for portable setups or for test copies next to your normal install.

**Windows autostart:** the entry is `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\GeniusLibreLinkUp`
and is rewritten on every start. If the app is switched off under Task Manager → Startup apps,
Windows skips it; turning *Start when I sign in* off and on in the app clears that switch.

### Upgrading from Better LibreLinkUp

The app was called **Better LibreLinkUp** (`better-librelinkup`) up to v0.1.1. Updating from it,
in the app or with the new setup, migrates everything on the first start: settings and the
glucose log move to the folders above, keychain entries and the autostart entry are carried
over, and `.bllu` month files are renamed to `.gllu` (also inside a custom, synced log folder).
Old `bllu1:` pairing codes keep working. Update every computer that shares a synced log folder.

## Building

The repository is a Cargo workspace:

| Crate                          | Contents                                                      |
| ------------------------------ | ------------------------------------------------------------- |
| **`.`** (`genius-librelinkup`) | Desktop app (Slint UI, tray, updater)                         |
| **`core/`** (`gllu-core`)      | LibreLinkUp client, encrypted store, sync protocol, demo data |
| **`server/`** (`gllu-server`)  | Caching server, see [server/README.md](server/README.md)      |

```bash
cargo build --release                  # desktop app: target/release/genius-librelinkup(.exe)
cargo build --release -p gllu-server   # caching server (no GUI dependencies)
cargo test --workspace
```

**Linux (.deb)** – needs `libfontconfig1-dev libxkbcommon-dev` (and `libwayland-dev` for Wayland):

```bash
cargo install cargo-deb
cargo deb                        # target/debian/genius-librelinkup_*.deb
```

**Windows installer** – [Inno Setup 6](https://jrsoftware.org/isinfo.php):

```powershell
iscc /DAppVersion=1.0.0 packaging\windows\installer.iss   # target/installer/*.exe
```

GitHub Actions (`.github/workflows/build.yml`) builds everything. A `v*` tag publishes a release
with the installer, the portable exe, the `.deb`, the `gllu-server` Linux binary and
`SHA256SUMS.txt` (the updater relies on these asset names), and pushes the server image to
`ghcr.io/nucl3arn30n/gllu-server`. The release notes are the version's section of
[CHANGELOG.md](CHANGELOG.md), which the app also shows under *About*.

## Requirements

**OS:** Windows, or Linux with a StatusNotifierItem tray (KDE, XFCE, Cinnamon, …)  
**GNOME:** Needs the *AppIndicator and KStatusNotifierItem Support* extension. Without a tray the window stays open and closing it quits the app  
**Account:** A LibreLinkUp follower account  
**Keychain:** Windows Credential Manager or Secret Service  
**Build:** Rust (edition 2024), Slint 1.18

---

Badges from [envoy1084/awesome-badges](https://github.com/envoy1084/awesome-badges)
