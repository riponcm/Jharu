use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

#[derive(Serialize)]
pub struct JunkItem {
    pub label: String,
    pub path: String,
    pub size_bytes: u64,
    pub stale_days: Option<u64>,
    /// Safe items can be trashed with confidence; others need review.
    pub safe: bool,
    /// Present when the location could not be fully read, so the size shown is
    /// a floor rather than the real total. Without this a location we lack
    /// rights to read looks identical to one that is genuinely empty.
    pub read_error: Option<String>,
}

#[derive(Serialize)]
pub struct JunkGroup {
    pub id: String,
    pub title: String,
    pub note: String,
    /// When false the group is informational only (e.g. the Trash itself).
    pub cleanable: bool,
    pub total_bytes: u64,
    pub items: Vec<JunkItem>,
}

/// Measures a location, keeping hold of the first read failure. Locations like
/// `C:\Windows\Temp` and the Prefetch folder need elevation, and swallowing the
/// error there would report them as 0 B — indistinguishable from empty.
fn dir_size_checked(path: &Path) -> (u64, Option<String>) {
    if path.is_file() {
        return match path.metadata() {
            Ok(m) => (m.len(), None),
            Err(e) => (0, Some(e.to_string())),
        };
    }
    let mut size = 0u64;
    let mut read_error: Option<String> = None;
    for result in jwalk::WalkDir::new(path).skip_hidden(false) {
        match result {
            Ok(entry) => {
                // jwalk hands back a directory it could not open as an Ok entry
                // carrying read_children_error, not as an Err from the iterator,
                // so checking only the Err arm misses every access denial.
                if let Some(e) = &entry.read_children_error {
                    read_error.get_or_insert_with(|| e.to_string());
                }
                if entry.file_type().is_file() {
                    match entry.metadata() {
                        Ok(meta) => size += meta.len(),
                        Err(e) => {
                            read_error.get_or_insert_with(|| e.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                read_error.get_or_insert_with(|| e.to_string());
            }
        }
    }
    (size, read_error)
}

/// Only the macOS-specific scans call this; Windows uses dir_size_checked.
#[allow(dead_code)]
fn dir_size(path: &Path) -> u64 {
    dir_size_checked(path).0
}

fn stale_days(path: &Path) -> Option<u64> {
    let modified = path.metadata().ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok().map(|d| d.as_secs() / 86_400)
}

fn push_group(groups: &mut Vec<JunkGroup>, id: &str, title: &str, note: &str, cleanable: bool, mut items: Vec<JunkItem>) {
    if items.is_empty() {
        return;
    }
    items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    groups.push(JunkGroup {
        id: id.into(),
        title: title.into(),
        note: note.into(),
        cleanable,
        total_bytes: items.iter().map(|i| i.size_bytes).sum(),
        items,
    });
}

const INSTALLER_EXT: &[&str] = &["dmg", "pkg", "iso", "msi", "exe", "appimage", "deb", "rpm"];
const ARCHIVE_EXT: &[&str] = &["zip", "tar", "gz", "tgz", "xz", "7z", "rar"];

fn scan_installers(downloads: &Path) -> (Vec<JunkItem>, Vec<JunkItem>) {
    let mut installers = vec![];
    let mut archives = vec![];
    if let Ok(rd) = std::fs::read_dir(downloads) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            if size < 1_000_000 {
                continue;
            }
            let item = JunkItem {
                label: entry.file_name().to_string_lossy().into_owned(),
                path: p.to_string_lossy().into_owned(),
                size_bytes: size,
                stale_days: stale_days(&p),
                safe: true,
                read_error: None,
            };
            if INSTALLER_EXT.contains(&ext.as_str()) {
                installers.push(item);
            } else if ARCHIVE_EXT.contains(&ext.as_str()) {
                archives.push(JunkItem { safe: false, ..item });
            }
        }
    }
    (installers, archives)
}

fn scan_large_stale(home: &Path) -> Vec<JunkItem> {
    let roots = ["Documents", "Desktop", "Downloads", "Movies", "Music", "Videos"];
    let mut items = vec![];
    for root in roots {
        let base = home.join(root);
        if !base.exists() {
            continue;
        }
        for entry in jwalk::WalkDir::new(&base).skip_hidden(true).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() < 500_000_000 {
                continue;
            }
            let p = entry.path();
            let days = stale_days(&p);
            if days.map_or(false, |d| d >= 180) {
                items.push(JunkItem {
                    label: format!("{} · {}", root, entry.file_name().to_string_lossy()),
                    path: p.to_string_lossy().into_owned(),
                    size_bytes: meta.len(),
                    stale_days: days,
                    safe: false,
                    read_error: None,
                });
            }
        }
    }
    items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    items.truncate(50);
    items
}

#[tauri::command]
pub async fn junk_scan() -> Result<Vec<JunkGroup>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = dirs::home_dir().ok_or("No home directory")?;
        let mut groups: Vec<JunkGroup> = vec![];

        let downloads = dirs::download_dir().unwrap_or_else(|| home.join("Downloads"));
        let (installers, archives) = scan_installers(&downloads);
        push_group(
            &mut groups,
            "installers",
            "Installers in Downloads",
            "Disk images and installer packages already served their purpose once the app is installed.",
            true,
            installers,
        );
        push_group(
            &mut groups,
            "archives",
            "Archives in Downloads",
            "Zip and tar files — check whether you still need the extracted contents first.",
            true,
            archives,
        );

        #[cfg(target_os = "macos")]
        {
            let backups = home.join("Library/Application Support/MobileSync/Backup");
            if let Ok(rd) = std::fs::read_dir(&backups) {
                let items: Vec<JunkItem> = rd
                    .flatten()
                    .map(|e| {
                        let p = e.path();
                        JunkItem {
                            label: format!("Device backup · {}", e.file_name().to_string_lossy()),
                            path: p.to_string_lossy().into_owned(),
                            size_bytes: dir_size(&p),
                            stale_days: stale_days(&p),
                            safe: false,
                            read_error: None,
                        }
                    })
                    .filter(|i| i.size_bytes > 0)
                    .collect();
                push_group(
                    &mut groups,
                    "backups",
                    "iPhone and iPad backups",
                    "Old device backups are often forgotten. Keep the newest one for each device you still own.",
                    true,
                    items,
                );
            }

            let mut log_items = vec![];
            let logs = home.join("Library/Logs");
            if logs.exists() {
                let size = dir_size(&logs);
                if size > 10_000_000 {
                    log_items.push(JunkItem {
                        label: "App logs and crash reports".into(),
                        path: logs.to_string_lossy().into_owned(),
                        size_bytes: size,
                        stale_days: None,
                        safe: true,
                        read_error: None,
                    });
                }
            }
            push_group(
                &mut groups,
                "logs",
                "Logs and crash reports",
                "Diagnostic text files that apps and macOS write over time. Safe to clear.",
                true,
                log_items,
            );

            let mail = home.join("Library/Containers/com.apple.mail/Data/Library/Mail Downloads");
            if mail.exists() {
                let size = dir_size(&mail);
                if size > 10_000_000 {
                    push_group(
                        &mut groups,
                        "mail",
                        "Mail attachment copies",
                        "Copies of attachments you opened from Mail. Originals stay in your mailbox.",
                        true,
                        vec![JunkItem {
                            label: "Mail Downloads".into(),
                            path: mail.to_string_lossy().into_owned(),
                            size_bytes: size,
                            stale_days: None,
                            safe: true,
                            read_error: None,
                        }],
                    );
                }
            }
        }

        #[cfg(target_os = "windows")]
        {
            let local = dirs::data_local_dir().unwrap_or_else(|| home.join("AppData/Local"));
            let windir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let windir = std::path::PathBuf::from(windir);

            let mut temp_items = vec![];
            for (label, path) in [
                ("Your temp folder (%TEMP%)", local.join("Temp")),
                ("System temp folder", windir.join("Temp")),
            ] {
                if path.exists() {
                    let (size, read_error) = dir_size_checked(&path);
                    // Keep a location that we could not fully read even when it
                    // measures small — the size is a floor, not the truth.
                    if size > 1_000_000 || read_error.is_some() {
                        temp_items.push(JunkItem {
                            label: label.into(),
                            path: path.to_string_lossy().into_owned(),
                            size_bytes: size,
                            stale_days: stale_days(&path),
                            safe: true,
                            read_error,
                        });
                    }
                }
            }
            push_group(
                &mut groups,
                "win-temp",
                "Temporary files",
                "Scratch files left behind by installers and applications. Windows never cleans these on its own.",
                true,
                temp_items,
            );

            let mut sys_items = vec![];
            for (label, path, safe) in [
                (
                    "Windows Update downloads",
                    windir.join("SoftwareDistribution/Download"),
                    true,
                ),
                (
                    "Delivery Optimization files",
                    windir.join("ServiceProfiles/NetworkService/AppData/Local/Microsoft/Windows/DeliveryOptimization"),
                    true,
                ),
                ("Crash dumps", local.join("CrashDumps"), true),
                (
                    "Error reporting archive",
                    std::path::PathBuf::from("C:\\ProgramData\\Microsoft\\Windows\\WER"),
                    true,
                ),
                (
                    "Previous Windows installation (Windows.old)",
                    std::path::PathBuf::from("C:\\Windows.old"),
                    false,
                ),
            ] {
                if path.exists() {
                    let (size, read_error) = dir_size_checked(&path);
                    if size > 10_000_000 || read_error.is_some() {
                        sys_items.push(JunkItem {
                            label: label.into(),
                            path: path.to_string_lossy().into_owned(),
                            size_bytes: size,
                            stale_days: stale_days(&path),
                            safe,
                            read_error,
                        });
                    }
                }
            }
            push_group(
                &mut groups,
                "win-system",
                "Windows system leftovers",
                "Update caches, crash dumps, and old installation files. Some need administrator rights to remove.",
                true,
                sys_items,
            );

            // Prefetch is listed for visibility only. Clearing it is a myth:
            // Windows uses it to launch apps faster, it is self-limiting, and
            // deleting it makes the next launches slower until it rebuilds.
            let prefetch = windir.join("Prefetch");
            if prefetch.exists() {
                // Prefetch is unreadable without elevation, so gating purely on
                // size hid this group entirely on a normal user account.
                let (size, read_error) = dir_size_checked(&prefetch);
                if size > 0 || read_error.is_some() {
                    push_group(
                        &mut groups,
                        "win-prefetch",
                        "Prefetch (informational)",
                        "Windows uses these files to start your apps faster, and caps their size automatically. Deleting them frees very little and slows the next launches, so Jharu does not clean them.",
                        false,
                        vec![JunkItem {
                            label: "Prefetch data".into(),
                            path: prefetch.to_string_lossy().into_owned(),
                            size_bytes: size,
                            stale_days: None,
                            safe: false,
                            read_error,
                        }],
                    );
                }
            }
        }

        push_group(
            &mut groups,
            "large",
            "Large files unused for 6+ months",
            "Big files you have not touched in half a year. Review each one — these are yours.",
            true,
            scan_large_stale(&home),
        );

        #[cfg(target_os = "macos")]
        {
            let trash = home.join(".Trash");
            if trash.exists() {
                let size = dir_size(&trash);
                if size > 0 {
                    push_group(
                        &mut groups,
                        "trash",
                        "Trash",
                        "Already in the Trash. Empty it from Finder when you are sure.",
                        false,
                        vec![JunkItem {
                            label: "Trash contents".into(),
                            path: trash.to_string_lossy().into_owned(),
                            size_bytes: size,
                            stale_days: None,
                            safe: false,
                            read_error: None,
                        }],
                    );
                }
            }
        }

        Ok(groups)
    })
    .await
    .map_err(|e| e.to_string())?
}
