mod access;
mod apps;
mod browsers;
mod deep;
mod health;
mod junk;
mod rules;
mod scanner;

use serde::Serialize;

#[tauri::command]
async fn scan_system() -> Result<scanner::ScanResult, String> {
    tauri::async_runtime::spawn_blocking(scanner::scan)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct CleanOutcome {
    freed_bytes: u64,
    errors: Vec<String>,
}

/// Moves paths to the OS trash / recycle bin — never a permanent delete.
#[tauri::command]
async fn clean_paths(paths: Vec<String>) -> Result<CleanOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut freed = 0u64;
        let mut errors = vec![];
        for p in paths {
            let path = std::path::PathBuf::from(&p);
            if !path.exists() {
                continue;
            }
            let size = dir_size(&path);
            match trash::delete(&path) {
                Ok(()) => freed += size,
                Err(e) => errors.push(format!("{p}: {e}")),
            }
        }
        CleanOutcome { freed_bytes: freed, errors }
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(path).map_err(|e| e.to_string())
}

fn dir_size(path: &std::path::Path) -> u64 {
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            scan_system,
            clean_paths,
            reveal_path,
            deep::deep_scan,
            browsers::browser_scan,
            apps::list_apps,
            apps::app_leftovers,
            health::drive_health,
            junk::junk_scan,
            access::check_access,
            access::open_access_settings,
            access::open_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
