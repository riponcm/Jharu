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
    /// Windows only: the command that runs the program's own uninstaller.
    pub uninstall_command: Option<String>,
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

/// Windows records every installed program under the uninstall registry keys,
/// including the command that removes it cleanly.
#[cfg(target_os = "windows")]
fn list_windows_apps(home: &Path) -> Vec<AppInfo> {
    use winreg::enums::*;
    use winreg::RegKey;

    const ROOTS: [(isize, &str); 3] = [
        (HKEY_LOCAL_MACHINE as isize, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE as isize, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER as isize, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];

    let index = build_index(home);
    let mut apps: Vec<AppInfo> = vec![];
    let mut seen: HashSet<String> = HashSet::new();

    for (hive, path) in ROOTS {
        let root = RegKey::predef(hive as HKEY);
        let Ok(uninstall) = root.open_subkey_with_flags(path, KEY_READ) else { continue };

        for key_name in uninstall.enum_keys().flatten() {
            let Ok(key) = uninstall.open_subkey_with_flags(&key_name, KEY_READ) else { continue };

            let name: String = match key.get_value("DisplayName") {
                Ok(n) => n,
                Err(_) => continue,
            };
            // Skip patches and updates, which are not user-uninstallable entries.
            if key.get_value::<u32, _>("SystemComponent").unwrap_or(0) == 1
                || key.get_value::<String, _>("ParentKeyName").is_ok()
                || !seen.insert(name.to_lowercase())
            {
                continue;
            }

            let version: Option<String> = key.get_value("DisplayVersion").ok();
            let uninstall_string: Option<String> = key
                .get_value("QuietUninstallString")
                .or_else(|_| key.get_value("UninstallString"))
                .ok();
            let install_location: Option<String> = key.get_value("InstallLocation").ok();

            // EstimatedSize is in KB; fall back to measuring the install folder.
            let estimated_kb: u32 = key.get_value("EstimatedSize").unwrap_or(0);
            let size_bytes = if estimated_kb > 0 {
                estimated_kb as u64 * 1024
            } else {
                install_location
                    .as_deref()
                    .map(|p| dir_size(Path::new(p)))
                    .unwrap_or(0)
            };

            let norm = normalize(&name);
            let data_bytes: u64 = index
                .iter()
                .filter(|e| matches(e, None, &norm).is_some())
                .map(|e| dir_size(&e.path))
                .sum();

            apps.push(AppInfo {
                name,
                path: install_location.unwrap_or_default(),
                size_bytes,
                data_bytes,
                bundle_id: None,
                version,
                last_used_days: None,
                uninstall_command: uninstall_string,
            });
        }
    }

    apps.sort_by(|a, b| (b.size_bytes + b.data_bytes).cmp(&(a.size_bytes + a.data_bytes)));
    apps
}

/// Runs the program's own uninstaller. Windows requires this rather than
/// deleting files, so the program can unregister itself properly.
#[tauri::command]
pub async fn run_uninstaller(command: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", "/wait"])
                .raw_arg(&command)
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = command;
        Err("Running an uninstaller command is only supported on Windows.".to_string())
    }
}

#[tauri::command]
pub async fn list_apps() -> Result<Vec<AppInfo>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        #[cfg(target_os = "windows")]
        {
            let home = dirs::home_dir().ok_or("No home directory")?;
            return Ok(list_windows_apps(&home));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            return Err("App listing is available on macOS and Windows.".to_string());
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

            // Measure every unique matched data dir once. Sequential on
            // purpose — see the note in scanner.rs about nesting threads
            // around jwalk walks.
            let sizes: HashMap<usize, u64> = unique_paths
                .iter()
                .map(|&i| (i, dir_size(&index[i].path)))
                .collect();

            let mut apps: Vec<AppInfo> = basics
                .iter()
                .zip(matched_per_app.iter())
                .map(|(b, hits)| {
                    let data_bytes: u64 = hits.iter().filter_map(|i| sizes.get(i)).sum();
                    AppInfo {
                        size_bytes: dir_size(&b.bundle),
                        last_used_days: last_used_days(&b.bundle),
                        path: b.bundle.to_string_lossy().into_owned(),
                        name: b.name.clone(),
                        data_bytes,
                        bundle_id: b.bundle_id.clone(),
                        version: b.version.clone(),
                        uninstall_command: None,
                    }
                })
                .collect();

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
