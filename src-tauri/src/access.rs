use serde::Serialize;

#[derive(Serialize)]
pub struct AccessStatus {
    /// True when the app can read TCC-protected locations (macOS Full Disk
    /// Access). Always true on platforms without an equivalent gate.
    pub full_disk: bool,
    pub needs_prompt: bool,
}

#[tauri::command]
pub fn check_access() -> AccessStatus {
    #[cfg(target_os = "macos")]
    {
        // These directories are readable only with Full Disk Access.
        let probes = [
            "/Library/Application Support/com.apple.TCC",
            &format!(
                "{}/Library/Safari",
                dirs::home_dir().unwrap_or_default().to_string_lossy()
            ),
        ];
        let full_disk = probes.iter().any(|p| std::fs::read_dir(p).is_ok());
        AccessStatus { full_disk, needs_prompt: !full_disk }
    }
    #[cfg(not(target_os = "macos"))]
    {
        AccessStatus { full_disk: true, needs_prompt: false }
    }
}

#[tauri::command]
pub fn open_access_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        tauri_plugin_opener::open_url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
            None::<String>,
        )
        .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}
