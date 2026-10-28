# Better LibreLinkUp

Unofficial desktop companion for **LibreLinkUp** (FreeStyle Libre followers), written in Rust with a
Material-styled [Slint](https://slint.dev) UI. Runs on **Windows** and **Linux** (`.deb`).

> ⚠️ Not affiliated with Abbott. It uses the undocumented LibreLinkUp API, which can change at any
> time. Never make treatment decisions based on this app – use your sensor reader / official app.

## Features

- **Tray icon**: the current value (`6.2`, `112`, `LO`/`HI`) with a trend arrow, coloured by range.
  It is rendered with real system fonts at the exact pixel size the tray uses, and the arrows
  snap to whole pixels so they stay readable at 16 px. You can choose the shape (number only /
  drop / circle / square), text colour, font (condensed / regular / mono) and whether the arrow
  is shown. The tooltip adds the trend text and the age of the reading.
- **Customisable UI** (Appearance page, applied live): accent colour, glucose range colours,
  corner style, a filled or outlined value card, a show/hide statistics card, and the graph span
  (3/6/12/24 h) and style (line, dots or both).
- **Dashboard**: current value, trend, a last-12-hours graph with the target band, and 24 h
  time-in-range / average / CV.
- **Notifications** (each with its own toggle): low, high, and "no new data" alerts, with a
  configurable repeat interval.
- **Glucose log** (optional), with LibreView-style reports for 7/14/30/90 days:
  - glucose statistics: % sensor active, average, **GMI**, **estimated HbA1c** (ADAG), CV and SD
  - time in ranges (very low / low / target / high / very high)
  - ambulatory glucose profile (AGP): median, 25–75 % and 5–95 % percentile bands
  - glucose pattern insights (traffic lights per time of day)
  - daily glucose profiles as a calendar (Monday first, only days with data); click a day for
    its 24-hour graph, statistics and time in ranges
- **Encrypted log files** (XChaCha20-Poly1305, Argon2id key derivation) in a **folder you choose**.
  Put it in Nextcloud/OneDrive/Dropbox to share it between machines. Before writing, each monthly
  file is merged with the copy on disk, so two PCs syncing the same folder don't lose readings.
- Changing or removing the log passphrase needs the current passphrase, so an unlocked session
  alone is not enough.
- **Delete all local data** (Settings): removes settings, app/API logs, the keychain entries,
  the autostart entry and optionally the glucose log files (only the app's own files, other files
  in a synced folder stay), then closes the app.
- **Import/export**: settings as JSON, readings as CSV. It can also import LibreView's "Download
  glucose data" CSV to backfill history beyond the 12 hours LibreLinkUp provides.
- **Automatic server discovery**: reads Abbott's live `regionalMap`
  (`/llu/config/country`), follows login redirects to the right region, and caches the result. A
  manual server override is available.
- **Developer mode**: an in-app API log with redacted requests and responses, also written to a
  daily rolling log file.
- **Caching server** ([`server/`](server/README.md)): an always-on `bllu-server` (Docker / LXC)
  polls LibreLinkUp while your computer is off. Paired apps fill the gaps automatically and can
  take their live values from it instead of logging in themselves. The connection is TLS 1.3 with
  mutual authentication. The server certificate is pinned via a one-time pairing code, and every
  computer gets its own key pair plus a rotating token bound to it. Brute-force bans, an IP
  allowlist and an audit log come on top; see [server/README.md](server/README.md#security).
- **Built-in updater**: checks the [GitHub releases](https://github.com/NUCL3ARN30N/better-librelinkup/releases)
  every 12 h (or on demand) and updates in place: a silent installer run, a portable exe swap, or
  `apt` via `pkexec` for the `.deb`. Downloads are only installed when they match the release's
  `SHA256SUMS.txt`.
- Credentials, the log passphrase and the server token are kept in the OS keychain (Windows
  Credential Manager or Secret Service), never in plain files. The app also offers autostart,
  start-minimized, a light/dark/system theme and mmol/L or mg/dL units.

## LibreLinkUp API notes (state: October 2026)

| Topic | Handling |
|---|---|
| Client identity | `product: llu.android`, `version: 5.0.1`, `User-Agent: LibreLinkUp/5.0.1` (configurable). If the server returns status `920`, the app adopts the `minimumVersion` it reports and retries. |
| `Account-Id` header | SHA-256 hex of `user.id`. Required since the 2024/2025 API changes. |
| Token lifetime | About 1 h with the v5 identity. The app re-logs in automatically using the keychain password. |
| Login steps (status `4`) | Terms of Use / Privacy Policy re-acceptance (`data.step.type` = `tou`/`pp`). The app asks you first; only then does it `POST /auth/continue/{type}`. |
| Servers | `api.libreview.io` (global), `api-{us,eu,eu2,de,fr,jp,ap,au,ae,ca,la}.libreview.io` from the live regional map, plus `api.libreview.ru` and `api-cn.myfreestyle.cn`. |
| Rate limits | HTTP/body status `429` is respected (`Retry-After`, bounded to 15 min). The default poll interval is 60 s. |

References: [PaulCoughlin/llu](https://github.com/PaulCoughlin/llu),
[smpurkis/libre-linkup-py](https://github.com/smpurkis/libre-linkup-py),
[nightscout-connect#73](https://github.com/nightscout/nightscout-connect/pull/73),
[nightscout-clock#236](https://github.com/ktomy/nightscout-clock/pull/236) / [#237](https://github.com/ktomy/nightscout-clock/pull/237).

## Usage

```
better-librelinkup              # normal start
better-librelinkup --minimized  # start in the tray (used by autostart)
better-librelinkup --dev        # developer mode (API log)
better-librelinkup --demo       # synthetic data, no account, writes nothing permanent
better-librelinkup --probe      # terminal API check, see below
better-librelinkup --tray-preview <dir>   # write magnified tray icon sheets (16/20/24/32 px)
```

### Testing the API from a terminal

`--probe` runs server discovery, login, connections and graph, then prints a redacted summary.
Credentials come only from environment variables:

```powershell
$env:LLU_EMAIL = "you@example.com"; $env:LLU_PASSWORD = Read-Host -MaskInput "Password"
better-librelinkup --probe | Out-Host     # the pipe makes PowerShell wait for the (GUI) exe's output
Remove-Item Env:LLU_PASSWORD
# optional: $env:LLU_SERVER = "https://api-eu2.libreview.io"; $env:LLU_DUMP = 1 (prints redacted responses)
```

## Data locations

| | Windows | Linux |
|---|---|---|
| Settings | `%APPDATA%\better-librelinkup\better-librelinkup\config\settings.json` | `~/.config/better-librelinkup/settings.json` |
| Glucose log (default) | `%APPDATA%\better-librelinkup\better-librelinkup\data\log\` | `~/.local/share/better-librelinkup/log/` |
| App/API log | `%LOCALAPPDATA%\better-librelinkup\better-librelinkup\data\app-logs\` | `~/.local/share/better-librelinkup/app-logs/` |

Log format: `vault.json` (KDF parameters, salt, check value) plus one `glucose-YYYY-MM.bllu` file per
month: magic `BLLU`, version, cipher byte, then a 24-byte nonce and ciphertext of 11-byte records
(`i64` unix time, `u16` mg/dL, `u8` trend).

Setting `BLLU_HOME=<dir>` keeps settings, log and app logs in that folder and uses a separate
keychain namespace. Use it for portable setups or for test copies next to your normal install.

## Building

The repository is a Cargo workspace:

| Crate | |
|---|---|
| `.` (`better-librelinkup`) | desktop app (Slint UI, tray, updater) |
| `core/` (`bllu-core`) | LibreLinkUp client, encrypted store, sync protocol, demo data |
| `server/` (`bllu-server`) | caching server, see [server/README.md](server/README.md) |

```bash
cargo build --release                  # desktop app: target/release/better-librelinkup(.exe)
cargo build --release -p bllu-server   # caching server (no GUI dependencies)
cargo test --workspace
```

**Linux (.deb)** – needs `libfontconfig1-dev libxkbcommon-dev` (and `libwayland-dev` for Wayland):

```bash
cargo install cargo-deb
cargo deb                        # target/debian/better-librelinkup_*.deb
```

The tray uses the StatusNotifierItem protocol (KDE, XFCE, Cinnamon, …). GNOME needs the
*AppIndicator and KStatusNotifierItem Support* extension. Without a tray the window stays open and
closing it quits the app.

**Windows installer** – [Inno Setup 6](https://jrsoftware.org/isinfo.php):

```powershell
iscc /DAppVersion=0.1.0 packaging\windows\installer.iss   # target/installer/*.exe
```

GitHub Actions (`.github/workflows/build.yml`) builds everything. A `v*` tag publishes a release
with the installer, the portable exe, the `.deb`, the `bllu-server` Linux binary and
`SHA256SUMS.txt` (the updater relies on these asset names), and pushes the server image to
`ghcr.io/nucl3arn30n/bllu-server`.

## License

MIT
