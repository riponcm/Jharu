<div align="center">

<img src="assets/brand/jharu-mark-1024.png" alt="Jharu logo" width="110" />

# Jharu

**Storage insight and cleanup for developers**

Jharu (Bengali: ঝাড়ু, Hindi: झाड़ू — "broom") finds the data your tools leave behind — ML model caches, package manager caches, app leftovers, and forgotten junk — and lets you explore and clean it from one professional dashboard.

[![Version](https://img.shields.io/badge/version-0.1.0-EF9F27)](https://github.com)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey)](#installation)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB)](https://tauri.app)

<img src="assets/brand/hero.svg" alt="Animated illustration of Jharu sweeping junk files and reclaiming disk space" width="820" />

</div>

## Why Jharu

Developers and ML practitioners accumulate tens or hundreds of gigabytes of invisible data: Hugging Face models downloaded once for an experiment, conda environments from abandoned projects, npm and pip caches, Docker images, and support files from apps uninstalled long ago. Generic disk analyzers show folder sizes but do not understand what the data is. Jharu ships a knowledge base of known cache and data locations, classifies what it finds, explains it in plain language, and tells you what is safe to remove.

## Features

**Dev clean.**
Scans 25+ known developer and ML locations — Hugging Face, Ollama, PyTorch, Whisper, conda, pip, uv, npm, yarn, pnpm, Cargo, Go, Gradle, Maven, NuGet, Homebrew, Xcode, Docker, JetBrains, Playwright, and more — with per-item safety ratings and staleness detection.

**Deep clean.**
Full-disk analysis with progressive results. Every folder is classified as system, applications, your files, app data, or developer data. System locations are marked protected and can never be cleaned from the app.

**Junk files.**
Finds non-cache junk: installers and archives in Downloads, old phone backups, logs and crash reports, mail attachment copies, and large files untouched for six months or more.

**Browsers.**
Detects installed browsers and itemizes their caches per profile, separating safe-to-clear cache from data that deserves review.

**Uninstaller.**
Lists installed applications with both their bundle size and the data they have accumulated across the system. Removing an app also finds and removes its leftovers, with high-confidence matches preselected.

**Drive health.**
Capacity, file system, and SMART status for every mounted drive, with usage gauges and early warnings as disks approach full.

## Screenshots

<div align="center">
<img src="docs/screenshots/dev-clean.png" alt="Jharu Dev clean dashboard showing reclaimable space, storage breakdown, and cache findings" width="820" />
</div>

## Safety model

- Nothing is ever permanently deleted. Every clean action moves data to the Trash or Recycle Bin, where it can be restored.
- Every finding carries a safety rating: safe to clear, re-downloadable, or review first.
- Operating system locations are protected and excluded from cleaning entirely.
- Every destructive action requires an explicit in-app confirmation.

## Installation

Prebuilt releases for macOS and Windows will be published on the releases page. Until then, build from source.

### Build from source

Prerequisites: [Rust](https://rustup.rs) (stable) and [Node.js](https://nodejs.org) 20+.

```sh
git clone <repository-url>
cd jharu
npm install
npm run tauri dev      # run in development
npm run tauri build    # produce installers for your platform
```

On macOS, grant Full Disk Access (System Settings, Privacy and Security) for complete scan coverage. Jharu detects missing access and offers to open the right settings pane.

## Architecture

| Layer | Technology |
| --- | --- |
| Scanner | Rust with parallel directory walking (`jwalk`) |
| Safety | OS trash integration (`trash`), never permanent deletion |
| Rules | Per-platform knowledge base of cache and data locations |
| UI | React and TypeScript, Tauri 2 WebView |
| Drive info | `sysinfo` with SMART status via platform tools |

## Roadmap

- Windows uninstaller support via registry uninstall keys
- Orphaned-data detection by comparing leftover folders against installed applications
- Treemap visualization for deep clean
- Duplicate file detection
- Localized UI, starting with Bengali and Hindi

## Contributing

Issues and pull requests are welcome. For substantial changes, open an issue first to discuss the approach. New cache locations for the knowledge base are especially appreciated — include the path per platform, what writes it, and whether it regenerates.

## License

Licensed under the [Apache License 2.0](LICENSE).

## Acknowledgements

Jharu draws inspiration from the open source projects ClearDisk, Pearcleaner, PureMac, SquirrelDisk, and kondo.

---

<div align="center">

A [Matily.org](https://matily.org) product. Built with Projectmem memory.

</div>
