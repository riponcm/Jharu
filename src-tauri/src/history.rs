use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One capacity reading for one volume, keyed by whole days so that running
/// Jharu repeatedly in a day does not skew the trend.
#[derive(Serialize, Deserialize, Clone)]
pub struct Snapshot {
    pub day: u64,
    pub mount: String,
    pub total: u64,
    pub used: u64,
}

#[derive(Serialize, Clone)]
pub struct HistoryPoint {
    pub day: u64,
    pub used: u64,
}

#[derive(Serialize, Clone)]
pub struct Forecast {
    pub points: Vec<HistoryPoint>,
    /// None until there are enough readings, or when usage is flat or falling.
    pub days_until_full: Option<u64>,
    /// Bytes per day, positive when the drive is filling up.
    pub bytes_per_day: i64,
}

pub fn today() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

fn store_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("capacity-history.json"))
}

fn load(app: &tauri::AppHandle) -> Vec<Snapshot> {
    let Some(path) = store_path(app) else { return vec![] };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(app: &tauri::AppHandle, snapshots: &[Snapshot]) {
    if let Some(path) = store_path(app) {
        if let Ok(json) = serde_json::to_string(snapshots) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Records today's reading for each volume and returns the per-volume trend.
/// Only one reading per volume per day is kept, and history is capped at two
/// years so the file can never grow without bound.
pub fn record(app: &tauri::AppHandle, current: &[(String, u64, u64)]) -> HashMap<String, Forecast> {
    let mut snapshots = load(app);
    let day = today();

    for (mount, total, used) in current {
        snapshots.retain(|s| !(s.mount == *mount && s.day == day));
        snapshots.push(Snapshot { day, mount: mount.clone(), total: *total, used: *used });
    }

    let cutoff = day.saturating_sub(730);
    snapshots.retain(|s| s.day >= cutoff);
    snapshots.sort_by_key(|s| s.day);
    save(app, &snapshots);

    let mut out = HashMap::new();
    for (mount, total, used) in current {
        let mut points: Vec<HistoryPoint> = snapshots
            .iter()
            .filter(|s| s.mount == *mount)
            .map(|s| HistoryPoint { day: s.day, used: s.used })
            .collect();
        points.sort_by_key(|p| p.day);

        let (bytes_per_day, days_until_full) = project(&points, *total, *used);
        out.insert(mount.clone(), Forecast { points, days_until_full, bytes_per_day });
    }
    out
}

/// Least-squares slope over the readings. Needs at least three days of data
/// spanning a few days — a trend drawn from two adjacent points is noise.
fn project(points: &[HistoryPoint], total: u64, used: u64) -> (i64, Option<u64>) {
    if points.len() < 3 {
        return (0, None);
    }
    let span = points.last().unwrap().day.saturating_sub(points[0].day);
    if span < 3 {
        return (0, None);
    }

    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| p.day as f64).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.used as f64).sum::<f64>() / n;

    let mut num = 0.0;
    let mut den = 0.0;
    for p in points {
        let dx = p.day as f64 - mean_x;
        num += dx * (p.used as f64 - mean_y);
        den += dx * dx;
    }
    if den == 0.0 {
        return (0, None);
    }

    let slope = num / den;
    let bytes_per_day = slope as i64;
    if slope <= 0.0 {
        return (bytes_per_day, None);
    }

    let remaining = total.saturating_sub(used) as f64;
    let days = (remaining / slope).round();
    if days.is_finite() && days >= 0.0 && days < 36_500.0 {
        (bytes_per_day, Some(days as u64))
    } else {
        (bytes_per_day, None)
    }
}
