use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct AppInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    /// Data the app has accumulated in library/appdata locations.
    pub data_bytes: u64,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub last_used_days: Option<u64>,
}

#[derive(Serialize)]
pub struct LeftoverEntry {
    pub label: String,
    pub path: String,
    pub size_bytes: u64,
    /// true when matched by bundle id (high confidence) vs name (lower)
    pub confident: bool,
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

fn search_dirs(home: &Path) -> Vec<(&'static str, PathBuf)> {
    #[cfg(target_os = "macos")]
    return vec![
        ("Application Support", home.join("Library/Application Support")),
        ("Caches", home.join("Library/Caches")),
        ("Preferences", home.join("Library/Preferences")),
        ("Logs", home.join("Library/Logs")),
        ("Containers", home.join("Library/Containers")),
        ("Group Containers", home.join("Library/Group Containers")),
        ("Saved State", home.join("Library/Saved Application State")),
        ("WebKit", home.join("Library/WebKit")),
        ("HTTP Storage", home.join("Library/HTTPStorages")),
    ];
    #[cfg(target_os = "windows")]
    return vec![
        ("AppData Roaming", home.join("AppData/Roaming")),
        ("AppData Local", home.join("AppData/Local")),
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    return vec![
        ("Config", home.join(".config")),
        ("Cache", home.join(".cache")),
        ("Data", home.join(".local/share")),
    ];
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

struct IndexEntry {
    label: String,
    path: PathBuf,
    lower_name: String,
    norm_name: String,
}

fn build_index(home: &Path) -> Vec<IndexEntry> {
    let mut index = vec![];
    for (label, dir) in search_dirs(home) {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            index.push(IndexEntry {
                label: format!("{label} · {name}"),
                path: entry.path(),
                lower_name: name.to_lowercase(),
                norm_name: normalize(&name),
            });
        }
    }
    index
}

fn matches(entry: &IndexEntry, bundle_id: Option<&str>, norm_name: &str) -> Option<bool> {
    if let Some(b) = bundle_id {
        if entry.lower_name.contains(b) {
            return Some(true);
        }
    }
    // Name matching only for distinctive names, to avoid false positives.
    if norm_name.len() >= 5 && entry.norm_name.contains(norm_name) {
        return Some(false);
    }
    None
}

#[cfg(target_os = "macos")]
fn read_bundle_info(app: &Path) -> (Option<String>, Option<String>) {
    let info = app.join("Contents/Info.plist");
    match plist::Value::from_file(&info) {
        Ok(plist::Value::Dictionary(d)) => {
            let get = |k: &str| d.get(k).and_then(|v| v.as_string()).map(|s| s.to_string());
            (get("CFBundleIdentifier"), get("CFBundleShortVersionString"))
        }
        _ => (None, None),
    }
}

/// Days since the app was last opened, via Spotlight metadata.
#[cfg(target_os = "macos")]
fn last_used_days(app: &Path) -> Option<u64> {
    let out = std::process::Command::new("mdls")
        .args(["-name", "kMDItemLastUsedDate", "-raw"])
        .arg(app)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let date = s.trim().split(' ').next()?;
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    let days = days_from_civil(y, m, d);
    let today = days_from_civil_now()?;
    Some((today - days).max(0) as u64)
}

/// Howard Hinnant's civil-date algorithm: days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_from_civil_now() -> Option<i64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((secs / 86_400) as i64)
}

#[tauri::command]
pub async fn list_apps() -> Result<Vec<AppInfo>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        #[cfg(not(target_os = "macos"))]
        {
            return Err("App listing is implemented on macOS first; Windows registry support is planned.".to_string());
        }
        #[cfg(target_os = "macos")]
        {
            let home = dirs::home_dir().ok_or("No home directory")?;
            let roots = [PathBuf::from("/Applications"), home.join("Applications")];
            let bundles: Vec<PathBuf> = roots
                .iter()
                .filter_map(|r| std::fs::read_dir(r).ok())
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |e| e == "app"))
                .collect();

            // Basic info first (cheap), sizes in parallel after.
            struct Basic {
                bundle: PathBuf,
                name: String,
                bundle_id: Option<String>,
                version: Option<String>,
            }
            let basics: Vec<Basic> = bundles
                .into_iter()
                .map(|bundle| {
                    let (bundle_id, version) = read_bundle_info(&bundle);
                    Basic {
                        name: bundle.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        bundle,
                        bundle_id,
                        version,
                    }
                })
                .collect();

            // One index of all app-data locations, matched per app by name.
            let index = build_index(&home);
            let mut matched_per_app: Vec<Vec<usize>> = vec![];
            let mut unique_paths: HashSet<usize> = HashSet::new();
            for b in &basics {
                let bid = b.bundle_id.as_deref().map(|s| s.to_lowercase());
                let norm = normalize(&b.name);
                let mut hits = vec![];
                for (i, e) in index.iter().enumerate() {
                    if matches(e, bid.as_deref(), &norm).is_some() {
                        hits.push(i);
                        unique_paths.insert(i);
                    }
                }
                matched_per_app.push(hits);
            }

            // Measure every unique matched data dir once, in parallel.
            let sizes: HashMap<usize, u64> = std::thread::scope(|s| {
                let handles: Vec<_> = unique_paths
                    .iter()
                    .map(|&i| {
                        let path = index[i].path.clone();
                        s.spawn(move || (i, dir_size(&path)))
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });

            // Bundle sizes + last-used in parallel.
            let mut apps: Vec<AppInfo> = std::thread::scope(|s| {
                let handles: Vec<_> = basics
                    .iter()
                    .zip(matched_per_app.iter())
                    .map(|(b, hits)| {
                        let data_bytes: u64 = hits.iter().filter_map(|i| sizes.get(i)).sum();
                        let bundle = b.bundle.clone();
                        let name = b.name.clone();
                        let bundle_id = b.bundle_id.clone();
                        let version = b.version.clone();
                        s.spawn(move || AppInfo {
                            size_bytes: dir_size(&bundle),
                            last_used_days: last_used_days(&bundle),
                            path: bundle.to_string_lossy().into_owned(),
                            name,
                            data_bytes,
                            bundle_id,
                            version,
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });

            apps.sort_by(|a, b| (b.size_bytes + b.data_bytes).cmp(&(a.size_bytes + a.data_bytes)));
            Ok(apps)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn app_leftovers(bundle_id: Option<String>, name: String) -> Result<Vec<LeftoverEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or("No home directory")?;
        let norm_name = normalize(&name);
        let bid = bundle_id.map(|b| b.to_lowercase());

        let mut found = vec![];
        for entry in build_index(&home) {
            if let Some(confident) = matches(&entry, bid.as_deref(), &norm_name) {
                found.push(LeftoverEntry {
                    size_bytes: dir_size(&entry.path),
                    path: entry.path.to_string_lossy().into_owned(),
                    label: entry.label,
                    confident,
                });
            }
        }
        found.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        found.truncate(40);
        Ok(found)
    })
    .await
    .map_err(|e| e.to_string())?
}
