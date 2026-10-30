use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

#[derive(Serialize)]
pub struct DesktopItem {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub age_days: Option<u64>,
    pub is_screenshot: bool,
}

#[derive(Serialize)]
pub struct DesktopReport {
    pub desktop_path: String,
    pub total_bytes: u64,
    pub file_count: u64,
    pub screenshot_bytes: u64,
    pub screenshot_count: u64,
    /// Where "Archive" moves things — a dated folder inside Pictures.
    pub suggested_archive: String,
    pub items: Vec<DesktopItem>,
}

/// Recognizes the default screenshot filenames macOS and Windows produce,
/// plus the common third-party tools, without touching your other files.
fn is_screenshot(name: &str) -> bool {
    let lower = name.to_lowercase();
    let is_image = [".png", ".jpg", ".jpeg", ".heic", ".gif"]
        .iter()
        .any(|e| lower.ends_with(e));
    if !is_image {
        return false;
    }
    lower.starts_with("screenshot")
        || lower.starts_with("screen shot")
        || lower.starts_with("screen recording")
        || lower.starts_with("cleanshot")
        || lower.starts_with("capture d")
        || lower.starts_with("captura")
        || lower.starts_with("bildschirmfoto")
}

fn age_days(path: &Path) -> Option<u64> {
    let modified = path.metadata().ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok().map(|d| d.as_secs() / 86_400)
}

fn entry_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    crate::walk::walk(path)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[tauri::command]
pub async fn desktop_scan() -> Result<DesktopReport, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = dirs::home_dir().ok_or("No home directory")?;
        let desktop = dirs::desktop_dir().unwrap_or_else(|| home.join("Desktop"));
        let archive = dirs::picture_dir()
            .unwrap_or_else(|| home.join("Pictures"))
            .join("Desktop archive");

        let mut items = vec![];
        if let Ok(rd) = std::fs::read_dir(&desktop) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                // Files only. Folders on the desktop are usually deliberate
                // organisation, not clutter, so Jharu leaves them alone.
                if !path.is_file() {
                    continue;
                }
                let size = entry_size(&path);
                if size == 0 {
                    continue;
                }
                items.push(DesktopItem {
                    is_screenshot: is_screenshot(&name),
                    age_days: age_days(&path),
                    path: path.to_string_lossy().into_owned(),
                    name,
                    size_bytes: size,
                });
            }
        }

        let total_bytes = items.iter().map(|i| i.size_bytes).sum();
        let file_count = items.len() as u64;
        let screenshot_bytes = items.iter().filter(|i| i.is_screenshot).map(|i| i.size_bytes).sum();
        let screenshot_count = items.iter().filter(|i| i.is_screenshot).count() as u64;

        items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

        Ok(DesktopReport {
            desktop_path: desktop.to_string_lossy().into_owned(),
            total_bytes,
            file_count,
            screenshot_bytes,
            screenshot_count,
            suggested_archive: archive.to_string_lossy().into_owned(),
            items,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct MoveOutcome {
    pub moved: u64,
    pub moved_bytes: u64,
    pub errors: Vec<String>,
}

/// Moves files into a folder instead of deleting them — the safe default for
/// a user's own screenshots, which they may still want.
#[tauri::command]
pub async fn move_to_folder(paths: Vec<String>, destination: String) -> Result<MoveOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dest = std::path::PathBuf::from(&destination);
        std::fs::create_dir_all(&dest).map_err(|e| format!("Cannot create {destination}: {e}"))?;

        let mut moved = 0u64;
        let mut moved_bytes = 0u64;
        let mut errors = vec![];

        for p in paths {
            let src = std::path::PathBuf::from(&p);
            if !src.exists() {
                continue;
            }
            let Some(file_name) = src.file_name() else { continue };
            let size = entry_size(&src);

            // Never silently overwrite an existing file in the archive.
            let mut target = dest.join(file_name);
            let mut n = 1;
            while target.exists() {
                let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = src
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                target = dest.join(format!("{stem} ({n}){ext}"));
                n += 1;
            }

            match std::fs::rename(&src, &target) {
                Ok(()) => {
                    moved += 1;
                    moved_bytes += size;
                }
                Err(e) => errors.push(format!("{p}: {e}")),
            }
        }

        Ok(MoveOutcome { moved, moved_bytes, errors })
    })
    .await
    .map_err(|e| e.to_string())?
}
