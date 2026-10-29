# Changelog

The app shows the entry of its own version under **About**. Release notes on GitHub are taken
from here as well.

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

First release as Genius LibreLinkUp (formerly Better LibreLinkUp 0.1.1): tray readout with trend arrow, alerts, dashboard graph, encrypted glucose log with LibreView-style reports, caching server, built-in updater. Existing Better LibreLinkUp installations are migrated on the first start.
