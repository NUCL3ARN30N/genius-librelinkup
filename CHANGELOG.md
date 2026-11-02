# Changelog

The app shows the entry of its own version under **About**. Release notes on GitHub are taken
from here as well.

## [1.1.0] – 2026-11-02

### Added
- Export on the Reports page: the new **Export…** button saves the readings and notes of the selected report period (or of all data) as a CSV file, or packs them into a ZIP file encrypted with AES-256 and attaches it to a new email in your mail program (Outlook, Thunderbird and other programs that support Simple MAPI on Windows, the desktop's mail program on Linux). Tell the recipient the password another way; the file opens with 7-Zip, WinRAR or Keka.
- Treatments in notes: a note can record rapid-acting or long-acting insulin, or a treatment you name yourself, with the units given (a decimal comma works too). Treatments are shown in the graph tooltips, included in the CSV export and import, and read from LibreView exports ("Rapid-Acting Insulin" and "Long-Acting Insulin" columns).

### Changed
- Exporting readings moved from Settings → Import & export to the Reports page.
- Notes written by a newer version keep their extra information when they are edited here.

### Fixed
- Counts read correctly in the singular ("1 note" instead of "1 notes").

## [1.0.0] – 2026-10-31

First stable release. Includes everything from 0.2.0 below.

## [0.2.0] – 2026-10-29

### Added
- Notes: add a note at any time (meal, insulin, exercise, …) with the new "Add note" button or by clicking the graph. Notes appear as markers on the dashboard graph, in the day details and in the daily profile calendar, and pointing at a marker shows the note. Notes are stored next to the glucose log (encrypted together with it), merged between computers that share a synced log folder, and included in the CSV export and import – LibreView exports bring their notes along too.
- Hover the glucose graph to see the time and value under the pointer, in every graph style (line, dots, line + dots), in the day details and in the daily glucose profiles calendar.
- The trend arrow can be shown as a separate, second tray icon so the number keeps the full icon width and stays readable (Appearance → Tray icon, on by default).
- About page with the version, what's new, and links to the source code, issues, releases, documentation and licence.

### Fixed
- Windows autostart: when the app starts at sign-in before the taskbar is ready, the tray icon could fail to register and never appeared, so the app ran invisibly. The icon is now re-created until it is shown. Turning autostart on in the app also clears a "disabled" switch left in Task Manager → Startup apps.
- The value in the tray icon is centred on the actual glyphs of the installed font instead of an estimated width, horizontally and vertically.

## [0.1.0] – 2026-10-29

First release of Genius LibreLinkUp: tray readout with trend arrow, alerts, dashboard graph, encrypted glucose log with LibreView-style reports, caching server, built-in updater.
