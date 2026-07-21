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

fn dir_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    jwalk::WalkDir::new(path)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
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
