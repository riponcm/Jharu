# Changelog

All notable changes to Jharu are recorded here. Versions follow
[semantic versioning](https://semver.org).

## [0.2.0] — 2026-07-22

The release that made Jharu understand *which* data matters, not just how much
of it there is. Four new modules, real Windows support, and a visual map of the
disk.

### Added

- **ML models.** Every downloaded model listed individually — Hugging Face repos,
  Ollama models, Whisper and torch checkpoints — with the distinction that
  matters: models you have actually loaded versus models that were downloaded
  and never opened since. Ollama layers shared between models are kept, so
  removing one model never breaks another. Where the filesystem does not record
  access times, Jharu says so instead of guessing.
- **Python environments.** Finds virtualenvs and conda environments, sizes each
  package inside them, and reports what cross-environment duplication costs —
  the "torch is installed eleven times" problem.
- **Desktop.** Lists files on the desktop with screenshot detection, filterable
  by type and age. Archiving to a folder is the default action rather than
  deleting, because these are your own files.
- **Junk files on Windows.** `%TEMP%`, the system temp folder, Windows Update
  downloads, Delivery Optimization, crash dumps, the error reporting archive and
  `Windows.old`.
- **Uninstaller on Windows.** Enumerates installed programs from the registry
  uninstall keys and runs each program's own uninstaller, then sweeps the
  leftovers it can identify.
- **Treemap** in deep clean, with a map and list toggle. Blocks are sized by what
  each folder holds; click one to go inside it.
- **Fill forecast** in drive health. One capacity reading is recorded per day and
  fitted to a trend, projecting when a drive will run out of space.
- **Hardware meters** in drive health, with wear level, temperature, reallocated
  sectors and power-on hours read through `smartctl` when it is installed.
- **A confirmation dialog before every clean**, stating in plain language what the
  consequence actually is — a cache rebuilds itself, a model is re-downloaded, an
  environment has to be recreated, your own files are gone for good.
- **In-app updates** via signed GitHub releases.

### Fixed

- **Findings disappeared from scans at random.** Each rule spawned a thread around
  a `jwalk` walk, but `jwalk` already parallelises internally on rayon, so nesting
  the two starved the pool and some walks returned nothing. `join().unwrap_or_default()`
  then turned that into "no finding". On one run this hid a 16.5 GB Hugging Face
  cache entirely. Walks are now sequential in `scanner`, `deep`, `python` and `apps`.
- **Locations that could not be read vanished instead of being reported.** A folder
  needing elevation measured 0 B and was dropped, which looked identical to a
  folder that was genuinely empty. Unreadable locations are now kept and flagged
  "partly unreadable".
- **The "partly unreadable" badge could never appear.** `jwalk` does not report a
  permission failure as an `Err` from the iterator — it yields the directory as a
  normal entry carrying `read_children_error`. Only the `Err` arm was checked, so
  the badge was unreachable on every platform. Fixed in `scanner`, `junk` and `deep`.
- **The treemap drew every cell black.** The category classes set `background`,
  which an SVG shape ignores; SVG paints with `fill`. The legend looked correct
  because those dots are HTML.
- **Category colours were unreadable for colour-blind users.** "Your files" and
  "apps" measured ΔE 0.3 under deuteranopia and 8.5 for normal vision — effectively
  the same colour. The palette was re-stepped and validated across all pairs in
  both light and dark mode.
- **SMART reported "Not Supported" on APFS.** The synthesised volume cannot report
  SMART; the physical store underneath it can. Only a genuine failure is styled as
  an error now — "not supported" is not a fault.
- **Windows build blockers.** `HKEY` was imported from `winreg::enums` where only
  the constants live, and `raw_arg` was called without `CommandExt`. The
  uninstaller module never compiled, so that path had never run.
- **npm cache was invisible on Windows**, looked up under the roaming profile
  instead of `LocalAppData`.
- **macOS instructions were shown to Windows users** in drive health and deep clean.
- **Junk rows shifted out of alignment** when a badge was absent.

### Changed

- Deep clean, junk files and the scanner now surface read failures rather than
  swallowing them, so an unreadable location is never mistaken for an empty one.
- Drive health always shows capacity and the SMART self-check, with `smartctl`
  detail added on top when available, instead of hiding the whole panel.

## [0.1.0] — 2026-07-20

First release.

### Added

- **Dev clean** across 25+ known developer and ML cache locations, with per-item
  safety ratings and staleness detection.
- **Deep clean**, a full-disk analysis classifying every folder as system,
  applications, your files, app data or developer data, with system locations
  protected from cleaning.
- **Junk files** — installers, archives, device backups, logs and large stale files.
- **Browsers** — per-profile cache breakdown for Chrome, Edge, Brave, Arc, Firefox
  and Safari.
- **Uninstaller** on macOS, listing applications with the data they have
  accumulated and removing their leftovers.
- **Drive health** — capacity and SMART status per mounted drive.
- Everything removed goes to the Trash or Recycle Bin, never a permanent delete.
