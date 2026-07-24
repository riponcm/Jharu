<div align="center">

<img src="assets/brand/jharu-mark-1024.png" alt="Jharu disk cleaner logo" width="110" />

# Jharu

**Free, open source disk cleaner and storage analyzer for developers — macOS and Windows**

Jharu finds and safely removes the gigabytes that development tools leave behind: machine learning model caches, package manager caches, Docker data, leftovers from uninstalled apps, and forgotten junk files — all from one professional dashboard.

[![Website](https://img.shields.io/badge/website-jharu.matily.org-D85A30)](https://jharu.matily.org)
[![GitHub stars](https://img.shields.io/github/stars/riponcm/Jharu?style=flat&color=EF9F27)](https://github.com/riponcm/Jharu/stargazers)
[![Downloads](https://img.shields.io/github/downloads/riponcm/Jharu/total?color=1D9E75)](https://github.com/riponcm/Jharu/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey)](#installation)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB)](https://tauri.app)

<img src="assets/brand/hero.svg" alt="Animation of Jharu sweeping junk files away and reclaiming disk space" width="820" />

### Download

[![Download for macOS](https://img.shields.io/badge/Download_for-macOS-111111?style=for-the-badge&logo=apple&logoColor=white)](https://github.com/riponcm/Jharu/releases/download/v0.2.0/Jharu_0.2.0_universal.dmg)
&nbsp;
[![Download for Windows](https://img.shields.io/badge/Download_for-Windows-0078D6?style=for-the-badge&logo=windows&logoColor=white)](https://github.com/riponcm/Jharu/releases/download/v0.2.0/Jharu_0.2.0_x64-setup.exe)

macOS installer is universal (Apple Silicon and Intel). Windows installer is 64-bit.
<br />
Current version: **v0.2.0** · [All releases and release notes](https://github.com/riponcm/Jharu/releases)

</div>

## What is Jharu?

Jharu is a desktop disk cleanup app built specifically for software developers and machine learning engineers. Generic disk space analyzers show you folder sizes; Jharu understands what the data actually is. It ships a knowledge base of known cache and data locations, classifies everything it finds, explains each item in plain language, rates how safe it is to delete, and moves removed data to the Trash or Recycle Bin — never a permanent delete.

Typical problems Jharu solves:

- Disk full after downloading machine learning models with Hugging Face, Ollama, PyTorch, or Whisper
- Gigabytes hidden in npm, pip, uv, yarn, pnpm, conda, Cargo, Go, Gradle, Maven, and NuGet caches
- Leftover settings, caches, and support files from apps you uninstalled long ago
- Old installers, phone backups, logs, and large files you have not touched in months
- Not knowing what is actually using your disk space

## Watch the demo

See a scan, a plain-English breakdown, and a click to clean — with the real numbers from a real machine.

<div align="center">

[![Watch the Jharu demo — scanning and cleaning developer and ML caches](https://i.ytimg.com/vi/pF8etchkChI/maxresdefault.jpg)](https://www.youtube.com/watch?v=pF8etchkChI)

<em>▶ Watch on YouTube — reclaiming gigabytes in under a minute</em>

</div>

## Screenshots

<div align="center">

<img src="docs/screenshots/dev-clean.png" alt="Jharu dashboard showing reclaimable disk space, a storage breakdown bar, and developer cache findings with safety ratings" width="820" />

<em>Dev clean — every known cache location, sized and rated</em>

<br /><br />

<img src="docs/screenshots/ml-models.png" alt="Jharu ML models tab listing individual models with badges showing which were downloaded but never loaded" width="820" />

<em>ML models — 21.1 GB of the 21.7 GB here was downloaded and never loaded once</em>

<br /><br />

<img src="docs/screenshots/deep-clean-treemap.png" alt="Jharu deep clean treemap with folders sized by disk usage and a notice naming folders that could not be read" width="820" />

<em>Deep clean — folders sized by what they hold, and an honest notice about what could not be read</em>

</div>

## Features

**ML models — keep what you use, remove what you never loaded.**
Every downloaded model listed individually, across Hugging Face, Ollama, Whisper, PyTorch and Keras. Jharu distinguishes models you have actually loaded from models that were downloaded once and never opened since, so you can reclaim the second kind without touching the first. Ollama layers shared between models are kept, so removing one model never breaks another. No other cleaner makes this distinction.

**Dev clean.**
Scans 25+ known developer and machine learning cache locations — Hugging Face, Ollama, PyTorch, Whisper, LM Studio, conda, pip, uv, npm, yarn, pnpm, Cargo, Go modules, Gradle, Maven, NuGet, Homebrew, Xcode DerivedData, Docker, JetBrains, Playwright, Puppeteer — with per-item safety ratings and staleness detection.

**Python environments.**
Finds every virtualenv and conda environment, sizes the packages inside them, and shows what duplication costs across environments — the "torch is installed eleven times" problem, quantified.

**Deep clean.**
Full-disk analysis as a treemap or a list. Folders are sized by what they hold and classified as system, applications, your files, app data or developer data. Click a block to go inside it. System locations are protected and can never be cleaned from the app.

**Junk files.**
Non-cache junk: installers and archives in Downloads, old device backups, logs and crash reports, mail attachment copies, large files untouched for six months, and on Windows the temp folders, Windows Update cache, Delivery Optimization files and crash dumps.

**Desktop.**
Screenshots and clutter on your desktop, filterable by type and age. Because these are your own files, archiving them to a folder is the default action rather than deleting.

**Browsers.**
Detects Chrome, Edge, Brave, Arc, Firefox and Safari, and itemizes their caches per profile, separating safe-to-clear cache from data that deserves review.

**Uninstaller.**
Lists installed applications with both their own size and the data they have accumulated across the system. On Windows it reads the registry and runs each program's own uninstaller; on macOS it removes the bundle. Either way it then sweeps the leftovers, with high-confidence matches preselected.

**Drive health.**
Capacity and SMART status per drive, plus a fill forecast built from a daily capacity reading that projects when a drive will run out of space. Wear level, temperature and power-on hours are shown when `smartctl` is available.

**In-app updates.**
Jharu checks GitHub releases for new versions and downloads and installs them from inside the app — signed, verified, and with a one-click restart.

## How Jharu compares

Generic cleaners are built to look busy and sell a subscription. Jharu does one thing deeply: the developer and AI data no general-purpose cleaner understands.

| | Jharu | CleanMyMac | CCleaner |
| --- | --- | --- | --- |
| Price | **Free forever** | $40+/yr | Freemium |
| Open source | **Yes · Apache 2.0** | No | No |
| Understands ML model caches | **Yes** | No | No |
| Used vs never-loaded detection | **Yes** | No | No |
| Telemetry / tracking | **None** | Yes | Yes |
| macOS and Windows | **Both** | macOS only | Both |
| Registry "cleaner" | **Never (it does not work)** | — | Yes |

## What can Jharu clean?

| Category | Examples |
| --- | --- |
| ML model caches | Hugging Face hub (`~/.cache/huggingface`), Ollama models (`~/.ollama/models`), PyTorch hub, Whisper, Keras, LM Studio |
| Python | pip cache, uv cache, conda package cache, stale conda environments |
| JavaScript | npm cache (`~/.npm/_cacache`), yarn cache, pnpm store, Puppeteer and Playwright browser downloads |
| Build tools | Xcode DerivedData and archives, Gradle caches, Maven repository, Cargo registry, Go module cache, NuGet packages |
| Containers | Docker VM data and image storage |
| IDEs | JetBrains caches, editor cache directories |
| App leftovers | Support files, caches, preferences, and logs from uninstalled applications |
| Junk | Installers in Downloads, old device backups, crash reports, mail attachment copies, large stale files |

## Safety model

- Nothing is ever permanently deleted. Every clean action moves data to the Trash or Recycle Bin, where it can be restored.
- Every finding carries a safety rating: safe to clear, re-downloadable, or review first.
- Operating system locations are protected and excluded from cleaning entirely.
- Every clean is confirmed by a dialog that states the actual consequence — a cache rebuilds itself, a model is re-downloaded, an environment must be recreated, your own files are gone for good.
- **A location Jharu cannot read is reported, never hidden.** A folder needing elevated rights measures 0 B, which looks exactly like an empty one; Jharu flags these as partly unreadable instead of quietly leaving them out. A cleaner that hides what it cannot see is worse than one that admits it.
- No registry cleaner, and Windows prefetch is listed but never cleaned. Both are widely marketed and neither does anything useful; prefetch is how Windows starts your apps quickly, and clearing it makes the next launches slower.

## Installation

Prebuilt installers for macOS (Apple Silicon and Intel) and Windows are published on the [releases page](https://github.com/riponcm/Jharu/releases). Once installed, Jharu updates itself in-app.

### Build from source

Prerequisites: [Rust](https://rustup.rs) (stable) and [Node.js](https://nodejs.org) 20+.

```sh
git clone https://github.com/riponcm/Jharu.git
cd Jharu
npm install
npm run tauri dev      # run in development
npm run tauri build    # produce installers for your platform
```

On macOS, grant Full Disk Access (System Settings, Privacy and Security) for complete scan coverage. Jharu detects missing access and offers to open the right settings pane.

### Opening Jharu the first time

Jharu is not yet signed with a paid developer certificate (see [Support Jharu](#support-jharu) below), so your operating system shows a one-time warning the first time you open it. This is expected for open source apps and does not mean anything is wrong — the download is verified by a cryptographic signature on every in-app update.

- **macOS:** if you see "Jharu cannot be opened because Apple cannot verify it", right-click (or Control-click) the app and choose **Open**, then confirm. You only do this once.
- **Windows:** if SmartScreen shows "Windows protected your PC", click **More info**, then **Run anyway**.

## Frequently asked questions

**How do I clear the Hugging Face model cache?**
Open Jharu's Dev clean tab. It lists your Hugging Face hub cache (`~/.cache/huggingface`) with its exact size and lets you explore it or move it to the Trash. Models are re-downloaded automatically the next time a library requests them.

**Is it safe to delete npm, pip, or other package manager caches?**
Yes. These caches only speed up reinstalls and are rebuilt automatically on the next install. Jharu marks them "Safe to clear". Deleting an active project's `node_modules` or a conda environment you still use is different — Jharu rates those "Review first".

**Does Jharu permanently delete my files?**
No. Every clean action moves data to the macOS Trash or Windows Recycle Bin, where it can be restored. Jharu never bypasses the Trash.

**Is Jharu a free alternative to CleanMyMac or CCleaner?**
Yes. Jharu is free and open source under the Apache 2.0 license, with no subscription, telemetry, or upsell. Its focus is narrower and deeper: developer and machine learning data that general-purpose cleaners do not understand.

**How do I find out what is taking up disk space on my Mac or Windows PC?**
Jharu's Deep clean tab analyzes the whole disk and shows every folder's real size, classified by what it is — system files, applications, your documents, app data, or developer data — so you can tell junk from data that matters.

**How do I completely uninstall an app, including leftover files?**
Jharu's Uninstaller lists every installed app together with the data it has accumulated. Uninstalling removes the app and its leftover support files, caches, and preferences in one step.

## Architecture

| Layer | Technology |
| --- | --- |
| Scanner | Rust with parallel directory walking (`jwalk`) |
| Safety | OS trash integration (`trash`), never permanent deletion |
| Rules | Per-platform knowledge base of cache and data locations |
| UI | React and TypeScript, Tauri 2 WebView |
| Updates | `tauri-plugin-updater` with signed GitHub release artifacts |
| Drive info | `sysinfo`, with SMART via `diskutil` and optional `smartctl` |
| Windows | `winreg` for the uninstall keys; per-platform paths throughout |
| Trend | A daily capacity reading stored locally, fitted by least squares |

## Roadmap

- Orphaned-data detection by comparing leftover folders against installed applications
- Duplicate file detection
- Detailed drive health on Windows, which currently reports capacity only
- Localized UI, starting with Bengali and Hindi

Shipped in 0.2.0: per-model ML intelligence, Python environment analysis, the
desktop cleaner, Windows uninstaller and junk rules, the deep clean treemap and
the fill forecast. See the [changelog](CHANGELOG.md).

## Support Jharu

Jharu is free, open source, and has no ads, telemetry, or paid tier — and it always will. But shipping a desktop app that users can install without scary security warnings costs real money every year, and that is the one thing an individual developer cannot absorb alone:

| What it unlocks | Cost |
| --- | --- |
| **Apple notarization** — removes the "Apple cannot verify this app" warning on every Mac | 99 USD / year (Apple Developer Program) |
| **Windows code signing** — removes the "Windows protected your PC" SmartScreen warning | ~100–400 USD / year (code signing certificate) |

Every star and every sponsorship goes directly toward these costs. If Jharu reclaimed gigabytes of your disk, here is how you can pay it forward in seconds:

- **Star the repository.** It is free, it takes one click, and it is the single biggest thing that helps other developers discover Jharu.
- **Sponsor the project.** Even a one-time contribution the size of a coffee helps cover the signing certificates that make Jharu safe and frictionless for everyone to install. Use the **Sponsor** button at the top of this repository.
- **Spread the word.** Share Jharu with a teammate whose disk is always full, or post about it — reach is what turns a side project into a maintained tool.

Once the signing costs are covered, every release will install cleanly on macOS and Windows with no warnings at all. Thank you for helping keep Jharu free and independent.

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
