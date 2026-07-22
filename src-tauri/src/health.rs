use crate::history::{self, Forecast};
use serde::Serialize;

#[derive(Serialize)]
pub struct SmartDetail {
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<i64>,
    /// SSD endurance consumed, 0–100. Low is healthy.
    pub percentage_used: Option<i64>,
    pub reallocated_sectors: Option<i64>,
}

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
    /// Detailed attributes, only when smartctl is installed and can read them.
    pub smart_detail: Option<SmartDetail>,
    pub forecast: Option<Forecast>,
}

#[derive(Serialize)]
pub struct HealthReport {
    pub drives: Vec<DriveInfo>,
    /// False when smartctl is not installed, so the UI can explain the gap
    /// instead of showing empty fields.
    pub smart_tool_available: bool,
}

#[cfg(target_os = "macos")]
fn diskutil_info(mount: &str) -> Option<plist::Dictionary> {
    let out = std::process::Command::new("diskutil")
        .args(["info", "-plist", mount])
        .output()
        .ok()?;
    match plist::Value::from_reader(std::io::Cursor::new(out.stdout)) {
        Ok(plist::Value::Dictionary(d)) => Some(d),
        _ => None,
    }
}

/// An APFS volume is synthesised and reports "Not Supported" for SMART — the
/// real answer lives on the physical store underneath it, so follow that link
/// before giving up.
#[cfg(target_os = "macos")]
fn physical_store(d: &plist::Dictionary) -> Option<String> {
    let stores = d.get("APFSPhysicalStores")?.as_array()?;
    let first = stores.first()?.as_dictionary()?;
    first
        .get("APFSPhysicalStore")
        .or_else(|| first.get("DeviceIdentifier"))
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
}

#[cfg(target_os = "macos")]
fn smart_status(mount: &str) -> Option<String> {
    let d = diskutil_info(mount)?;
    let direct = d.get("SMARTStatus").and_then(|v| v.as_string()).map(|s| s.to_string());
    match direct.as_deref() {
        Some("Not Supported") | None => physical_store(&d)
            .and_then(|store| diskutil_info(&store))
            .and_then(|pd| pd.get("SMARTStatus").and_then(|v| v.as_string()).map(|s| s.to_string()))
            .or(direct),
        _ => direct,
    }
}

#[cfg(target_os = "macos")]
fn device_node(mount: &str) -> Option<String> {
    let d = diskutil_info(mount)?;
    if let Some(store) = physical_store(&d) {
        return Some(format!("/dev/{store}"));
    }
    // SMART lives on the whole device, not an individual partition.
    d.get("ParentWholeDisk")
        .and_then(|v| v.as_string())
        .map(|n| format!("/dev/{n}"))
        .or_else(|| d.get("DeviceNode").and_then(|v| v.as_string()).map(|s| s.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn smart_status(_mount: &str) -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
fn device_node(_mount: &str) -> Option<String> {
    None
}

fn smartctl_available() -> bool {
    std::process::Command::new("smartctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reads detailed attributes with smartctl when it is installed. Absent by
/// default on macOS and Windows, so every field is optional and the UI says so
/// rather than showing dashes.
fn smart_detail(device: &str) -> Option<SmartDetail> {
    let out = std::process::Command::new("smartctl")
        .args(["--json", "-A", "-H", device])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;

    let temperature_c = v["temperature"]["current"].as_i64();
    let power_on_hours = v["power_on_time"]["hours"].as_i64();
    let percentage_used = v["nvme_smart_health_information_log"]["percentage_used"]
        .as_i64()
        .or_else(|| {
            v["ata_smart_attributes"]["table"]
                .as_array()?
                .iter()
                .find(|a| a["name"].as_str() == Some("Percent_Lifetime_Remain"))
                .and_then(|a| a["raw"]["value"].as_i64())
        });
    let reallocated_sectors = v["ata_smart_attributes"]["table"]
        .as_array()
        .and_then(|t| {
            t.iter()
                .find(|a| a["name"].as_str() == Some("Reallocated_Sector_Ct"))
                .and_then(|a| a["raw"]["value"].as_i64())
        });

    if temperature_c.is_none()
        && power_on_hours.is_none()
        && percentage_used.is_none()
        && reallocated_sectors.is_none()
    {
        return None;
    }
    Some(SmartDetail { temperature_c, power_on_hours, percentage_used, reallocated_sectors })
}

#[tauri::command]
pub async fn drive_health(app: tauri::AppHandle) -> Result<HealthReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let has_smartctl = smartctl_available();

        let mut drives: Vec<DriveInfo> = disks
            .list()
            .iter()
            .filter(|d| {
                let mount = d.mount_point().to_string_lossy();
                !mount.starts_with("/System/Volumes")
            })
            .map(|d| {
                let mount = d.mount_point().to_string_lossy().into_owned();
                let detail = if has_smartctl {
                    device_node(&mount).and_then(|dev| smart_detail(&dev))
                } else {
                    None
                };
                DriveInfo {
                    name: d.name().to_string_lossy().into_owned(),
                    smart_status: smart_status(&mount),
                    smart_detail: detail,
                    mount,
                    file_system: d.file_system().to_string_lossy().into_owned(),
                    kind: format!("{:?}", d.kind()),
                    total_bytes: d.total_space(),
                    available_bytes: d.available_space(),
                    removable: d.is_removable(),
                    forecast: None,
                }
            })
            .collect();

        drives.sort_by(|a, b| a.mount.cmp(&b.mount));
        drives.dedup_by(|a, b| a.mount == b.mount);

        let readings: Vec<(String, u64, u64)> = drives
            .iter()
            .map(|d| (d.mount.clone(), d.total_bytes, d.total_bytes.saturating_sub(d.available_bytes)))
            .collect();
        let mut forecasts = history::record(&app, &readings);
        for d in &mut drives {
            d.forecast = forecasts.remove(&d.mount);
        }

        drives.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
        Ok(HealthReport { drives, smart_tool_available: has_smartctl })
    })
    .await
    .map_err(|e| e.to_string())?
}
