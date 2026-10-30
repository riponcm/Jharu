# Changelog

All notable changes to Jharu are recorded here. Versions follow
[semantic versioning](https://semver.org).

## [1.0.0] — 2026-10-30

The first stable release. Jharu grows from a cache cleaner into a full
storage and speed tool for developers, with a dashboard to start from, and
every Windows download is now signed.

### Added

- **Overview.** The page Jharu opens on: a ring for every drive with when it
  fills up, what Jharu has saved week by week, what can be cleaned right now
  split by safety, live CPU and memory, a map of where the system drive's space
  goes, and the latest cleans.
- **Git worktrees.** Finds the spare checkouts that `git worktree`, Claude Code
  and Codex leave behind. Only worktrees whose commits are already on the main
  branch and that have nothing uncommitted are preselected; branches are never
  deleted.
- **Build folders.** `node_modules`, Rust `target`, `.next`, `dist` and other
  build output, offered only when the project beside it confirms what it is,
  with the command that brings it back.
- **AI tools.** What Claude Code, Codex, Cursor, Windsurf, Copilot, Continue and
  Aider leave behind, tool by tool. Caches are safe once the tool is closed;
  conversations, memory and settings are never preselected.
- **Duplicates.** Identical files and the same model stored by several tools,
  confirmed by BLAKE3 hashes, never by name or date. Hard links and APFS clones
  are recognised as shared space, not duplicates.
- **App sandboxes.** The private AppData copies of Store (MSIX) apps on Windows
  and sandboxed apps on macOS, which most cleaners never show.
- **System space.** The hibernation file, restore points, the component store,
  Time Machine local snapshots and Docker, each with what turning it off costs
  and the exact command Jharu runs.
- **Recycle Bin and Trash.** What is in the bin, rated, with emptying by age and
  a log of everything Jharu moved there with one-click restore.
- **Space history.** Snapshots of where the space is, "what grew since last
  time", a fill forecast per drive, low-space alerts, and optional scheduled
  cleaning of safe caches (off by default).
- **Startup apps.** What starts when you sign in, rated, with the same
  reversible switch Task Manager uses on Windows (listing only on macOS).
- **Speed.** A live view of CPU, memory, disk and the running programs, grouped
  by program and rated, with a graceful Close and a confirmed Force quit. System
  processes, security software (including VPN clients) and drivers are never
  offered.
- **Floating gadget.** An optional always-on-top bar or card with CPU, memory,
  free space and the busiest programs.
- **Deep clean ratings.** Every folder carries a safety rating with the reasons,
  on the list and on the map. Repositories that are also on a remote are told
  apart from ones with local-only work.

### Changed

- A new title bar and a sidebar grouped into Developer, Disk, Manage and
  Insight.
- Sizes on Windows use the units Explorer uses, so Jharu's numbers match it.
- Folders Deep clean cannot fully read are named above the map and badged in
  the list, instead of looking smaller or empty.

### Fixed

- Many Windows fixes, and a full macOS pass: the Trash, iCloud Drive and Library
  containers are known places, APFS clones are counted once, and window
  detection and app quitting work on macOS.

### Security

- Windows installers and programs are signed with Azure Artifact Signing, so
  Windows shows the publisher instead of "Unknown publisher".
- The macOS app is signed with a Developer ID and notarized by Apple, so it
  opens without the "can't be opened" warning or a right-click → Open.

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
