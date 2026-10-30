use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct BrowserItem {
    pub label: String,
    pub path: String,
    pub size_bytes: u64,
    /// true = pure cache, regenerates; false = review first (may hold site data)
    pub safe: bool,
}

#[derive(Serialize)]
pub struct BrowserReport {
    pub name: String,
    pub total_bytes: u64,
    pub items: Vec<BrowserItem>,
}

struct Spec {
    name: &'static str,
    /// (label, relative path from home, safe)
    fixed: Vec<(&'static str, String, bool)>,
    /// profile container relative to home; scans its profile subdirs
    profiles_root: Option<String>,
}

fn chromium_profile_items(profile: &PathBuf) -> Vec<(String, PathBuf, bool)> {
    let name = profile.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    [
        ("Cache", true),
        ("Code Cache", true),
        ("GPUCache", true),
        ("Service Worker/CacheStorage", true),
        ("Service Worker/ScriptCache", true),
    ]
    .iter()
    .map(|(sub, safe)| (format!("{name} · {sub}"), profile.join(sub), *safe))
    .collect()
}

fn specs() -> Vec<Spec> {
    #[cfg(target_os = "macos")]
    {
        vec![
            Spec {
                name: "Google Chrome",
                fixed: vec![("System cache", "Library/Caches/Google/Chrome".into(), true)],
                profiles_root: Some("Library/Application Support/Google/Chrome".into()),
            },
            Spec {
                name: "Brave",
                fixed: vec![("System cache", "Library/Caches/BraveSoftware".into(), true)],
                profiles_root: Some("Library/Application Support/BraveSoftware/Brave-Browser".into()),
            },
            Spec {
                name: "Microsoft Edge",
                fixed: vec![("System cache", "Library/Caches/Microsoft Edge".into(), true)],
                profiles_root: Some("Library/Application Support/Microsoft Edge".into()),
            },
            Spec {
                name: "Arc",
                fixed: vec![("System cache", "Library/Caches/Arc".into(), true)],
                profiles_root: Some("Library/Application Support/Arc/User Data".into()),
            },
            Spec {
                name: "Firefox",
                fixed: vec![("Cache", "Library/Caches/Firefox/Profiles".into(), true)],
                profiles_root: None,
            },
            Spec {
                name: "Safari",
                fixed: vec![("Cache", "Library/Caches/com.apple.Safari".into(), true)],
                profiles_root: None,
            },
        ]
    }
    #[cfg(target_os = "windows")]
    {
        vec![
            Spec {
                name: "Google Chrome",
                fixed: vec![],
                profiles_root: Some("AppData/Local/Google/Chrome/User Data".into()),
            },
            Spec {
                name: "Microsoft Edge",
                fixed: vec![],
                profiles_root: Some("AppData/Local/Microsoft/Edge/User Data".into()),
            },
            Spec {
                name: "Brave",
                fixed: vec![],
                profiles_root: Some("AppData/Local/BraveSoftware/Brave-Browser/User Data".into()),
            },
            Spec {
                name: "Firefox",
                fixed: vec![("Cache", "AppData/Local/Mozilla/Firefox/Profiles".into(), true)],
                profiles_root: None,
            },
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            Spec {
                name: "Google Chrome",
                fixed: vec![("Cache", ".cache/google-chrome".into(), true)],
                profiles_root: Some(".config/google-chrome".into()),
            },
            Spec {
                name: "Firefox",
                fixed: vec![("Cache", ".cache/mozilla/firefox".into(), true)],
                profiles_root: None,
            },
        ]
    }
}

fn dir_size(path: &std::path::Path) -> u64 {
    crate::walk::walk(path)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn is_profile_dir(name: &str) -> bool {
    name == "Default" || name.starts_with("Profile ")
}

#[tauri::command]
pub async fn browser_scan() -> Result<Vec<BrowserReport>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = dirs::home_dir().ok_or("No home directory")?;
        let mut reports = vec![];

        for spec in specs() {
            let mut items: Vec<BrowserItem> = vec![];

            for (label, rel, safe) in &spec.fixed {
                let p = home.join(rel);
                if p.exists() {
                    let size = dir_size(&p);
                    if size > 0 {
                        items.push(BrowserItem {
                            label: label.to_string(),
                            path: p.to_string_lossy().into_owned(),
                            size_bytes: size,
                            safe: *safe,
                        });
                    }
                }
            }

            if let Some(root) = &spec.profiles_root {
                let root = home.join(root);
                if let Ok(rd) = std::fs::read_dir(&root) {
                    for entry in rd.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if entry.path().is_dir() && is_profile_dir(&name) {
                            for (label, path, safe) in chromium_profile_items(&entry.path()) {
                                if path.exists() {
                                    let size = dir_size(&path);
                                    if size > 0 {
                                        items.push(BrowserItem {
                                            label,
                                            path: path.to_string_lossy().into_owned(),
                                            size_bytes: size,
                                            safe,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !items.is_empty() {
                items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
                reports.push(BrowserReport {
                    name: spec.name.to_string(),
                    total_bytes: items.iter().map(|i| i.size_bytes).sum(),
                    items,
                });
            }
        }

        reports.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
        Ok(reports)
    })
    .await
    .map_err(|e| e.to_string())?
}
