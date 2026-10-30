mod access;
mod ai_tools;
mod apps;
mod bin;
mod autocare;
mod browsers;
mod cleaning;
mod codesign;
mod deep;
mod desktop;
mod dupes;
mod gadget;
mod health;
mod history;
mod junk;
mod models;
mod overview;
mod places;
mod processes;
mod protected;
mod projects;
mod python;
mod rules;
mod safety;
mod sandboxes;
mod scanner;
mod trashlog;
mod system_space;
mod spacehistory;
mod startup;
mod usage;
mod walk;
mod worktrees;

use serde::Serialize;

#[tauri::command]
async fn scan_system() -> Result<scanner::ScanResult, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let _busy = autocare::activity();
        scanner::scan()
    })
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
async fn clean_paths(paths: Vec<String>, source: Option<String>) -> Result<CleanOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = autocare::activity();
        let mut freed = 0u64;
        let mut errors = vec![];
        for p in paths {
            let path = std::path::PathBuf::from(&p);
            if !path.exists() {
                continue;
            }
            // Only what deleting really releases: hard-linked files that live
            // on elsewhere (uv packages in virtualenvs) are not freed.
            let size = crate::usage::reclaimable(&path);
            match trash::delete(&path) {
                Ok(()) => {
                    freed += size;
                    crate::trashlog::record(&path, size, source.as_deref().unwrap_or("Jharu"));
                }
                // Windows reports a file held open by another program only as
                // "Some operations were aborted", and then moves nothing.
                Err(e) if path.exists() && e.to_string().contains("aborted") => errors.push(format!(
                    "{p}: something inside is open in another program, so nothing was moved. Close programs that may be using it and try again."
                )),
                Err(e) => errors.push(format!("{p}: {e}")),
            }
        }
        CleanOutcome { freed_bytes: freed, errors }
    })
    .await
    .map_err(|e| e.to_string())
}

/// The exact space deleting `path` would free, checking every file for hard
/// links elsewhere. Slow on large caches, so it is asked for on its own.
#[tauri::command]
async fn reclaimable_size(path: String) -> Result<u64, String> {
    tauri::async_runtime::spawn_blocking(move || crate::usage::reclaimable(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(path).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Started as the elevated helper for system folders: clean and exit
    // without opening a window.
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = cleaning::helper_main(&args) {
        std::process::exit(code);
    }
    if let Some(code) = startup::helper_main(&args) {
        std::process::exit(code);
    }

    // Started as the elevated helper for the system space tab: run one fixed
    // command and exit.
    if let Some(code) = system_space::helper_main(&args) {
        std::process::exit(code);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            autocare::start(app.handle().clone());
            gadget::restore(app.handle());
            // The window draws its own title bar on Windows (TitleBar.tsx).
            // macOS overlays the traffic lights on it (tauri.macos.conf.json);
            // Linux keeps the system bar.
            #[cfg(target_os = "linux")]
            {
                use tauri::Manager;
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.set_decorations(true);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_system,
            clean_paths,
            reclaimable_size,
            reveal_path,
            deep::deep_scan,
            browsers::browser_scan,
            apps::list_apps,
            apps::app_leftovers,
            health::drive_health,
            junk::junk_scan,
            access::check_access,
            access::open_access_settings,
            access::open_url,
            models::model_scan,
            python::python_scan,
            desktop::desktop_scan,
            desktop::move_to_folder,
            apps::run_uninstaller,
            worktrees::worktree_scan,
            worktrees::remove_worktrees,
            cleaning::clean_locations,
            bin::bin_list,
            bin::bin_log,
            bin::bin_purge,
            bin::bin_restore,
            bin::bin_cancel,
            projects::build_scan,
            projects::remove_build_folders,
            projects::cancel_build_work,
            system_space::system_space_scan,
            system_space::system_space_inspect,
            system_space::system_space_run,
            system_space::system_space_cancel,
            dupes::dupes_default_roots,
            dupes::dupes_check_folder,
            dupes::dupes_cancel,
            dupes::dupes_scan,
            dupes::dupes_remove,
            spacehistory::history_overview,
            spacehistory::history_compare,
            spacehistory::history_forecasts,
            spacehistory::history_verdict,
            spacehistory::history_snapshot_now,
            spacehistory::history_cancel,
            autocare::autocare_set,
            autocare::autocare_preview,
            autocare::autocare_run_now,
            autocare::autocare_test_alert,
            sandboxes::sandbox_scan,
            sandboxes::remove_sandbox_items,
            sandboxes::cancel_sandbox_task,
            ai_tools::ai_tools_scan,
            ai_tools::remove_ai_leftovers,
            ai_tools::cancel_ai_removal,
            startup::startup_scan,
            startup::set_startup_enabled,
            gadget::gadget_get,
            gadget::gadget_set,
            gadget::gadget_snapshot,
            gadget::gadget_fit,
            gadget::gadget_open_main,
            overview::overview_quick,
            overview::overview_cleanable,
            overview::overview_star_done,
            processes::process_list,
            processes::process_close,
            processes::process_force_quit,
            processes::process_cancel
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
