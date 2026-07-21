use serde::Serialize;

#[derive(Serialize)]
pub struct DriveInfo {
    pub name: String,
    pub mount: String,
    pub file_system: String,
    pub kind: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub removable: bool,
    /// e.g. "Verified" on macOS when SMART reports healthy; None if unknown.
    pub smart_status: Option<String>,
}

#[cfg(target_os = "macos")]
fn smart_status(mount: &str) -> Option<String> {
    let out = std::process::Command::new("diskutil")
        .args(["info", "-plist", mount])
        .output()
        .ok()?;
    match plist::Value::from_reader(std::io::Cursor::new(out.stdout)) {
        Ok(plist::Value::Dictionary(d)) => d
            .get("SMARTStatus")
            .and_then(|v| v.as_string())
            .map(|s| s.to_string()),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn smart_status(_mount: &str) -> Option<String> {
    None
}

#[tauri::command]
pub async fn drive_health() -> Result<Vec<DriveInfo>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let mut drives: Vec<DriveInfo> = disks
            .list()
            .iter()
            .filter(|d| {
                let mount = d.mount_point().to_string_lossy();
                // Hide macOS internal APFS helper volumes; keep root and real volumes.
                !mount.starts_with("/System/Volumes") || mount == "/System/Volumes/Data"
            })
            .filter(|d| d.mount_point().to_string_lossy() != "/System/Volumes/Data")
            .map(|d| {
                let mount = d.mount_point().to_string_lossy().into_owned();
                DriveInfo {
                    name: d.name().to_string_lossy().into_owned(),
                    smart_status: smart_status(&mount),
                    mount,
                    file_system: d.file_system().to_string_lossy().into_owned(),
                    kind: format!("{:?}", d.kind()),
                    total_bytes: d.total_space(),
                    available_bytes: d.available_space(),
                    removable: d.is_removable(),
                }
            })
            .collect();

        // Dedupe by mount point (APFS lists snapshots under the same mount).
        drives.sort_by(|a, b| a.mount.cmp(&b.mount));
        drives.dedup_by(|a, b| a.mount == b.mount);
        drives.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
        Ok(drives)
    })
    .await
    .map_err(|e| e.to_string())?
}
