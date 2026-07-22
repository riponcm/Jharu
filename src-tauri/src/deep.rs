use jwalk::WalkDir;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::Emitter;

#[derive(Serialize, Clone)]
pub struct DeepEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub file_count: u64,
    /// system | applications | user_data | app_data | dev | other
    pub kind: String,
    /// Protected locations can be explored but never cleaned from the app.
    pub protected: bool,
    pub hint: String,
}

#[derive(Serialize, Clone)]
struct DeepEvent {
    token: String,
    entry: DeepEntry,
}

pub fn default_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    return PathBuf::from("C:\\");
    #[cfg(not(target_os = "windows"))]
    return PathBuf::from("/");
}

/// Paths that must never be descended into when measuring: separate volumes,
/// virtual filesystems, and the APFS firmlink that would double-count data.
fn excluded(path: &Path) -> bool {
    let s = path.to_string_lossy();
    #[cfg(target_os = "macos")]
    {
        return s == "/System/Volumes"
            || s.starts_with("/System/Volumes/")
            || s == "/Volumes"
            || s.starts_with("/Volumes/")
            || s == "/dev"
            || s == "/Network";
    }
    #[cfg(not(target_os = "macos"))]
    {
        return s.contains("$Recycle.Bin")
            || s.ends_with("\\System Volume Information")
            || s == "/proc"
            || s == "/sys"
            || s == "/dev";
    }
}

fn classify(path: &Path) -> (String, bool, String) {
    let home = dirs::home_dir().unwrap_or_default();
    let s = path.to_string_lossy().to_string();
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

    #[cfg(target_os = "macos")]
    {
        for sys in ["/System", "/usr", "/bin", "/sbin", "/cores", "/private", "/etc", "/var", "/opt/homebrew/Cellar"] {
            if s == *sys || (s.starts_with(sys) && !s.starts_with(&format!("{}/", home.to_string_lossy()))) {
                if s == "/private" || s.starts_with("/private") {
                    return ("system".into(), true, "System temp files, logs, and swap. Managed by macOS.".into());
                }
                return ("system".into(), true, "Operating system files. Cannot be cleaned from here.".into());
            }
        }
        if s == "/Library" || (s.starts_with("/Library/") && !s.starts_with(&home.to_string_lossy().to_string())) {
            return ("system".into(), true, "System-wide support files shared by all users.".into());
        }
        if s == "/Applications" || s.starts_with("/Applications/") {
            return ("applications".into(), false, "Installed apps — use the Uninstaller tab to remove one cleanly.".into());
        }
    }
    #[cfg(target_os = "windows")]
    {
        let low = s.to_lowercase();
        if low.starts_with("c:\\windows") || name.eq_ignore_ascii_case("pagefile.sys") || name.eq_ignore_ascii_case("hiberfil.sys") {
            return ("system".into(), true, "Operating system files. Cannot be cleaned from here.".into());
        }
        if low.starts_with("c:\\program files") {
            return ("applications".into(), false, "Installed programs — uninstall via the Uninstaller tab.".into());
        }
        if low.starts_with("c:\\programdata") {
            return ("app_data".into(), false, "Shared application data. Review before deleting.".into());
        }
    }

    if path.starts_with(&home) {
        let user_dirs = ["Documents", "Desktop", "Downloads", "Pictures", "Movies", "Music", "Videos", "Public"];
        if let Ok(rel) = path.strip_prefix(&home) {
            let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
            if user_dirs.contains(&first.as_str()) {
                return ("user_data".into(), false, "Your personal files. Review carefully before deleting anything.".into());
            }
            if first == "Library" || first == "AppData" {
                return ("app_data".into(), false, "Settings, caches, and data stored by your apps.".into());
            }
            if first.starts_with('.') {
                return ("dev".into(), false, "Configuration or developer data (hidden folder).".into());
            }
        }
    }
    if name.starts_with('.') {
        return ("dev".into(), false, "Hidden configuration or developer data.".into());
    }
    ("other".into(), false, "Unrecognized — explore to see what's inside.".into())
}

fn measure(path: &Path) -> (u64, u64) {
    if path.is_file() {
        return (path.metadata().map(|m| m.len()).unwrap_or(0), 1);
    }
    let mut size = 0u64;
    let mut files = 0u64;
    let walker = WalkDir::new(path).skip_hidden(false).process_read_dir(|_, _, _, children| {
        children.retain(|c| c.as_ref().map(|e| !excluded(&e.path())).unwrap_or(true));
    });
    for entry in walker.into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                size += meta.len();
                files += 1;
            }
        }
    }
    (size, files)
}

#[tauri::command]
pub async fn deep_scan(
    app: tauri::AppHandle,
    path: Option<String>,
    token: String,
) -> Result<Vec<DeepEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = path.map(PathBuf::from).unwrap_or_else(default_root);
        let mut children: Vec<PathBuf> = std::fs::read_dir(&root)
            .map_err(|e| format!("Cannot read {}: {e}", root.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                !excluded(p)
                    && !p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false)
            })
            .collect();
        children.sort();

        // Measured one at a time: jwalk already parallelises each walk, and
        // nesting a thread per child starves rayon's pool, which makes walks
        // return empty. Entries still stream to the UI as each one finishes.
        let mut entries: Vec<DeepEntry> = Vec::with_capacity(children.len());
        for child in children {
            let (size_bytes, file_count) = measure(&child);
            let (kind, protected, hint) = classify(&child);
            let entry = DeepEntry {
                name: child
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| child.to_string_lossy().into_owned()),
                path: child.to_string_lossy().into_owned(),
                is_dir: child.is_dir(),
                size_bytes,
                file_count,
                kind,
                protected,
                hint,
            };
            let _ = app.emit(
                "deep-entry",
                DeepEvent { token: token.clone(), entry: entry.clone() },
            );
            entries.push(entry);
        }

        entries.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        Ok(entries)
    })
    .await
    .map_err(|e| e.to_string())?
}
