//! The Overview page: one look at every drive, what Jharu has saved, what
//! can be cleaned and how safely, and where the system drive's space goes.
//!
//! Nothing here measures anything new. It gathers what other parts already
//! keep or know: Space history's drive readings and forecasts, the Recycle
//! Bin journal (trashlog.rs) for what Jharu moved, Dev clean's ratings, the
//! bin listing and the hibernation file. The quick part answers at once; the
//! cleanable part runs Dev clean's scan and takes a few seconds, so the page
//! asks for it separately.

use crate::rules::{Category, Safety};
use crate::spacehistory;
use crate::trashlog::Entry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DAY: u64 = 86_400;
/// Entries of one source this close together are one clean.
const BATCH_GAP: u64 = 120;

// ---------------------------------------------------------------- quick part

#[derive(Serialize, Clone, Debug)]
pub struct DriveCard {
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub removable: bool,
    pub system: bool,
    pub days_until_full: Option<u64>,
    /// Space history's sentence, or why there is none yet.
    pub forecast: String,
    /// Used bytes, one per day, oldest first (up to 30).
    pub trend: Vec<u64>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Batch {
    /// What it was: the item's name, or "N items".
    pub label: String,
    pub source: String,
    pub bytes: u64,
    pub items: usize,
    pub time: u64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Folder {
    pub name: String,
    pub path: String,
    pub bytes: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct SystemMap {
    pub root: String,
    pub ts: u64,
    pub folders: Vec<Folder>,
    /// Used space not in `folders`: smaller folders and system files.
    pub other: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Prefs {
    /// The GitHub card was closed or the project starred: never shown again.
    pub star_done: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct Quick {
    pub drives: Vec<DriveCard>,
    pub saved_total: u64,
    pub saved_items: usize,
    pub saved_since: Option<u64>,
    /// Bytes saved in each of the last six weeks, oldest first.
    pub saved_weeks: Vec<u64>,
    pub recent: Vec<Batch>,
    pub system_map: Option<SystemMap>,
    pub prefs: Prefs,
}

fn system_mount() -> String {
    #[cfg(target_os = "windows")]
    {
        format!("{}\\", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        "/".into()
    }
}

fn shown_mount(m: &str) -> String {
    let t = m.trim_end_matches(['\\', '/']);
    if t.is_empty() { "/".into() } else { t.into() }
}

fn name_of(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

/// Totals over the whole journal, the last six weeks and the latest cleans.
pub fn savings(entries: &[Entry], now: u64) -> (u64, usize, Option<u64>, Vec<u64>, Vec<Batch>) {
    let total = entries.iter().map(|e| e.size).sum();
    let since = entries.iter().map(|e| e.time).min();
    let mut weeks = vec![0u64; 6];
    for e in entries {
        let age = now.saturating_sub(e.time) / (7 * DAY);
        if age < 6 {
            weeks[5 - age as usize] += e.size;
        }
    }

    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.time);
    let mut batches: Vec<(Batch, u64)> = vec![];
    for e in sorted {
        match batches.last_mut() {
            Some((b, last)) if b.source == e.source && e.time.saturating_sub(*last) <= BATCH_GAP => {
                b.bytes += e.size;
                b.items += 1;
                b.label = format!("{} items", b.items);
                *last = e.time;
            }
            _ => batches.push((
                Batch { label: name_of(&e.path), source: e.source.clone(), bytes: e.size, items: 1, time: e.time },
                e.time,
            )),
        }
    }
    let recent = batches.into_iter().rev().take(5).map(|(b, _)| b).collect();
    (total, entries.len(), since, weeks, recent)
}

/// The system drive's biggest top-level folders from the newest Deep clean
/// of its root, if there has been one.
pub fn system_map(records: &[spacehistory::Record], mount: &str) -> Option<SystemMap> {
    let key = spacehistory::path_key(mount);
    let snap = spacehistory::snapshots(records)
        .into_iter()
        .rev()
        .find(|s| s.root.as_deref().is_some_and(|r| spacehistory::path_key(r) == key) && !s.folders.is_empty())?;
    let mut top: Vec<Folder> = snap
        .folders
        .iter()
        .filter(|f| {
            let p = spacehistory::path_key(&f.path);
            p.strip_prefix(&key).is_some_and(|rest| {
                let rest = rest.trim_start_matches('/');
                !rest.is_empty() && !rest.contains('/')
            })
        })
        .map(|f| Folder { name: name_of(&f.path), path: f.path.clone(), bytes: f.bytes })
        .collect();
    top.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    let used = snap.drives.iter().find(|d| spacehistory::path_key(&d.mount) == key).map(|d| d.used);
    let all: u64 = top.iter().map(|f| f.bytes).sum();
    top.truncate(6);
    let shown: u64 = top.iter().map(|f| f.bytes).sum();
    let other = used.map(|u| u.saturating_sub(shown)).unwrap_or(all - shown);
    Some(SystemMap { root: shown_mount(mount), ts: snap.ts, folders: top, other })
}

fn prefs_file(dir: &Path) -> PathBuf {
    dir.join("overview.json")
}

fn load_prefs(dir: &Path) -> Prefs {
    std::fs::read_to_string(prefs_file(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn quick_impl(dir: Option<&Path>, now: u64) -> Quick {
    let records = dir.map(spacehistory::load).unwrap_or_default();
    let snaps = spacehistory::snapshots(&records);
    let forecasts = spacehistory::forecasts(&records, now);
    let sys_key = spacehistory::path_key(&system_mount());

    let mut drives: Vec<DriveCard> = spacehistory::read_drives()
        .into_iter()
        .map(|d| {
            let key = spacehistory::path_key(&d.mount);
            let f = forecasts.iter().find(|f| spacehistory::path_key(&f.mount) == key);
            let mut trend: Vec<u64> = spacehistory::daily_points(&snaps, &d.mount).iter().map(|p| p.used).collect();
            if trend.len() > 30 {
                trend.drain(..trend.len() - 30);
            }
            DriveCard {
                mount: shown_mount(&d.mount),
                total: d.total,
                used: d.used,
                free: d.free,
                removable: d.removable,
                system: key == sys_key,
                days_until_full: f.and_then(|f| f.days_until_full),
                forecast: match f {
                    Some(f) if f.confidence != "none" => f.summary.clone(),
                    _ => "No forecast yet: it needs a few days of free-space readings.".into(),
                },
                trend,
            }
        })
        .collect();
    // System drive first, then the fullest.
    drives.sort_by(|a, b| b.system.cmp(&a.system).then((b.used * 1000 / b.total.max(1)).cmp(&(a.used * 1000 / a.total.max(1)))));

    let (saved_total, saved_items, saved_since, saved_weeks, recent) = savings(&crate::trashlog::read_all(), now);
    Quick {
        drives,
        saved_total,
        saved_items,
        saved_since,
        saved_weeks,
        recent,
        system_map: system_map(&records, &system_mount()),
        prefs: dir.map(load_prefs).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------- cleanable part

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Source {
    pub id: String,
    pub label: String,
    /// The tab that cleans it.
    pub tab: String,
    pub safe: u64,
    pub caution: u64,
    pub risky: u64,
    pub items: usize,
}

fn add(s: &mut Source, safety: Safety, bytes: u64) {
    match safety {
        Safety::Safe => s.safe += bytes,
        Safety::Caution => s.caution += bytes,
        Safety::Risky => s.risky += bytes,
    }
    s.items += 1;
}

fn source(id: &str, label: &str, tab: &str) -> Source {
    Source { id: id.into(), label: label.into(), tab: tab.into(), safe: 0, caution: 0, risky: 0, items: 0 }
}

fn cleanable_impl() -> Vec<Source> {
    let mut dev = source("dev", "Dev caches", "dev");
    let mut models = source("models", "ML models", "models");
    let mut app = source("appdata", "Tool data", "dev");
    for f in crate::scanner::scan().findings {
        let target = match f.category {
            Category::DevCache => &mut dev,
            Category::MlModels => &mut models,
            Category::AppData => &mut app,
        };
        add(target, f.safety, f.reclaimable_bytes);
    }
    let mut out = vec![dev, models, app];

    #[cfg(target_os = "windows")]
    {
        let hiber = PathBuf::from(system_mount()).join("hiberfil.sys");
        if let Ok(m) = std::fs::metadata(&hiber) {
            let mut s = source("hibernation", "Hibernation file", "system");
            // Turning hibernation off is reversible but changes how the PC
            // starts: never "safe" without the user reading why.
            add(&mut s, Safety::Caution, m.len());
            out.push(s);
        }
    }

    if let Ok(bin) = crate::bin::list_report() {
        let mut s = source("bin", bin.bin_name.as_str(), "bin");
        for i in &bin.items {
            add(&mut s, i.safety, i.size_bytes);
        }
        out.push(s);
    }
    out.retain(|s| s.safe + s.caution + s.risky > 0);
    out
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn overview_quick(app: tauri::AppHandle) -> Result<Quick, String> {
    let dir = spacehistory::data_dir(&app);
    tauri::async_runtime::spawn_blocking(move || quick_impl(dir.as_deref(), spacehistory::now()))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn overview_cleanable() -> Result<Vec<Source>, String> {
    tauri::async_runtime::spawn_blocking(cleanable_impl).await.map_err(|e| e.to_string())
}

/// Remembers that the GitHub card was closed or used.
#[tauri::command]
pub async fn overview_star_done(app: tauri::AppHandle) -> Result<(), String> {
    let dir = spacehistory::data_dir(&app).ok_or("Jharu could not open its data folder.")?;
    let prefs = Prefs { star_done: true };
    std::fs::write(prefs_file(&dir), serde_json::to_string_pretty(&prefs).unwrap_or_default()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spacehistory::{DriveReading, FolderSize, Record, Snapshot};

    fn entry(path: &str, size: u64, time: u64, source: &str) -> Entry {
        Entry { path: path.into(), size, time, source: source.into() }
    }

    #[test]
    fn savings_add_up_per_week_and_group_cleans() {
        let now = 100 * DAY;
        let entries = vec![
            entry("C:/Users/x/AppData/Local/uv/cache", 20, now - 2 * DAY, "Dev clean"),
            entry("C:/Temp/a", 1, now - DAY, "System folders"),
            entry("C:/Temp/b", 2, now - DAY + 30, "System folders"),
            entry("C:/Temp/c", 3, now - DAY + 60, "System folders"),
            entry("C:/old/thing", 7, now - 50 * DAY, "Deep clean"),
        ];
        let (total, items, since, weeks, recent) = savings(&entries, now);
        assert_eq!((total, items), (33, 5));
        assert_eq!(since, Some(now - 50 * DAY));
        assert_eq!(weeks, vec![0, 0, 0, 0, 0, 26], "only the last six weeks, newest last");
        assert_eq!(recent.len(), 3, "the three temp files are one clean");
        assert_eq!(recent[0], Batch { label: "3 items".into(), source: "System folders".into(), bytes: 6, items: 3, time: now - DAY });
        assert_eq!(recent[1].label, "cache");
        assert_eq!(recent[2].source, "Deep clean");
    }

    #[test]
    fn nothing_saved_yet_is_all_zero() {
        let (total, items, since, weeks, recent) = savings(&[], 10 * DAY);
        assert_eq!((total, items, since), (0, 0, None));
        assert_eq!(weeks, vec![0; 6]);
        assert!(recent.is_empty());
    }

    fn snap(ts: u64, root: Option<&str>, folders: &[(&str, u64)], used: u64) -> Record {
        Record::Snapshot(Snapshot {
            ts,
            source: "deep".into(),
            drives: vec![DriveReading { mount: root.unwrap_or("C:\\").into(), total: 1000, used, free: 1000 - used, removable: false }],
            root: root.map(String::from),
            folders: folders.iter().map(|(p, b)| FolderSize { path: p.to_string(), bytes: *b }).collect(),
            floor: 0,
            detail: vec![],
        })
    }

    #[cfg(windows)]
    #[test]
    fn system_map_takes_the_newest_root_scan_and_top_level_folders() {
        let records = vec![
            snap(1, Some("C:\\"), &[("C:\\Old", 900)], 900),
            snap(2, Some("C:\\"), &[("C:\\Users", 500), ("C:\\Users\\x", 400), ("C:\\Windows", 200), ("C:\\Program Files", 100)], 900),
            snap(3, Some("C:\\Users"), &[("C:\\Users\\x", 450)], 900),
            snap(4, None, &[], 950),
        ];
        let m = system_map(&records, "C:\\").unwrap();
        assert_eq!(m.ts, 2);
        assert_eq!(m.root, "C:");
        assert_eq!(m.folders.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Users", "Windows", "Program Files"]);
        assert_eq!(m.other, 100, "used at the time of that scan, minus what is listed");
        assert!(system_map(&[snap(1, None, &[], 10)], "C:\\").is_none(), "no Deep clean of C: yet");
    }

    #[cfg(unix)]
    #[test]
    fn system_map_reads_a_unix_root_scan() {
        let records = vec![
            snap(1, Some("/"), &[("/Users", 500), ("/Users/x", 400), ("/System", 200), ("/Applications", 100)], 900),
            snap(2, Some("/Users"), &[("/Users/x", 450)], 900),
        ];
        let m = system_map(&records, "/").unwrap();
        assert_eq!(m.ts, 1);
        assert_eq!(m.root, "/");
        assert_eq!(m.folders.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["Users", "System", "Applications"]);
        assert_eq!(m.other, 100);
    }

    #[test]
    fn quick_reads_this_machine() {
        let q = quick_impl(None, spacehistory::now());
        assert!(!q.drives.is_empty());
        assert!(q.drives[0].system, "the system drive comes first");
        assert!(q.drives.iter().all(|d| d.free <= d.total));
        assert_eq!(q.saved_weeks.len(), 6);
        assert!(!q.prefs.star_done);
    }
}
