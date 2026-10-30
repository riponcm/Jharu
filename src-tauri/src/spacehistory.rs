//! Space history: what grew since last time, and when a drive will be full.
//!
//! Deep clean shows where the space is today. This module remembers it, so the
//! question every full disk raises ("it was fine last month, what happened?")
//! has an answer. Two kinds of snapshot go into one small JSONL file in the
//! app data folder (one record per line, so a damaged line costs one record):
//!
//! - A drive reading (used and free bytes per drive), taken by the background
//!   check every few hours. Cheap, and the input of the forecast.
//! - A folder snapshot, taken after each Deep clean scan of a drive root or
//!   the home folder, or on request: the sizes of the largest folders down to
//!   three levels (at most [`MAX_FOLDERS`] paths). Level one is exactly what
//!   Deep clean measured; the two levels below come from a second walk that
//!   counts files the same way (usage.rs: each physical file once).
//!
//! Comparing two folder snapshots of the same root gives "what grew". Folders
//! that were not among the largest then are not assumed to have been empty:
//! each snapshot records the size below which it stopped listing (`floor`),
//! and a folder missing from the older one is reported as growing by at
//! least its size minus that floor.
//!
//! The forecast fits free space over the last 30 days with the Theil-Sen
//! estimator (the median of all pairwise daily slopes), so one big cleanup or
//! one big download does not swing it the way a least-squares line would.
//! With too little data it says so instead of guessing.
//!
//! Records older than 90 days are dropped; those older than two days are
//! thinned to one folder snapshot per root and day plus the day's last drive
//! reading, so the file stays small however long Jharu runs.

use crate::autocare::{self, Settings, State};
use crate::usage::{Seen, Usage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{Emitter, Manager};

const DAY: u64 = 86_400;
pub const RETENTION_DAYS: u64 = 90;
/// Records newer than this are kept as they are; older ones are thinned.
const FULL_RESOLUTION: u64 = 2 * DAY;
/// Paths kept per folder snapshot.
pub const MAX_FOLDERS: usize = 300;
/// Folders below this are not worth a line in a snapshot.
const MIN_FOLDER_BYTES: u64 = 20 << 20;
/// How many of the largest top-level folders get the two extra levels after a
/// Deep clean scan. The rest stay a single line each.
const MAX_DESCEND: usize = 12;
/// A change smaller than this is noise, not news.
const MIN_CHANGE: u64 = 50 << 20;
const GROWN_SHOWN: usize = 25;
const SHRUNK_SHOWN: usize = 15;
/// The baseline a comparison picks by default.
const DEFAULT_LOOKBACK: u64 = 7 * DAY;
const FORECAST_WINDOW_DAYS: u64 = 30;
const FORECAST_MIN_READINGS: usize = 4;
const FORECAST_MIN_SPAN_DAYS: u64 = 3;
/// A fill date further away than this is reported as "not any time soon".
const FORECAST_HORIZON_DAYS: u64 = 3 * 365;

/// Set by `history_cancel`; checked by snapshot walks and scheduled cleaning.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
static SNAPSHOTTING: AtomicBool = AtomicBool::new(false);
static STORE: Mutex<()> = Mutex::new(());

/// Clears `SNAPSHOTTING` however the thread that set it ends.
struct SnapshottingFlag;

impl Drop for SnapshottingFlag {
    fn drop(&mut self) {
        SNAPSHOTTING.store(false, Ordering::SeqCst);
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------- records

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DriveReading {
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    #[serde(default)]
    pub removable: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FolderSize {
    #[serde(rename = "p")]
    pub path: String,
    #[serde(rename = "b")]
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub ts: u64,
    /// "deep" after a Deep clean scan, "manual", or "background".
    pub source: String,
    pub drives: Vec<DriveReading>,
    /// The folder the sizes below were measured under; None for drive readings.
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub folders: Vec<FolderSize>,
    /// A folder missing from `folders` was at most this big, provided it is
    /// at a level the snapshot looked at (see `detail`).
    #[serde(default)]
    pub floor: u64,
    /// The top-level folders whose two levels below were measured too. A
    /// folder deeper than the first level is only known to be small, or
    /// absent, when its top-level folder is listed here.
    #[serde(default)]
    pub detail: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CleanedItem {
    pub name: String,
    pub path: String,
    pub bytes: u64,
}

/// What one scheduled (or manual) automatic clean did.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CleanRun {
    pub ts: u64,
    /// "scheduled" or "manual".
    pub trigger: String,
    pub freed_bytes: u64,
    pub removed: Vec<CleanedItem>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Snapshot(Snapshot),
    Clean(CleanRun),
}

impl Record {
    fn ts(&self) -> u64 {
        match self {
            Record::Snapshot(s) => s.ts,
            Record::Clean(c) => c.ts,
        }
    }
}

// ---------------------------------------------------------------- store

fn store_file(dir: &Path) -> PathBuf {
    dir.join("space-history.jsonl")
}

/// Every readable record, oldest first. A line that does not parse (a newer
/// version's record, a torn write) is skipped, never fatal.
pub fn load(dir: &Path) -> Vec<Record> {
    let mut records: Vec<Record> = std::fs::read_to_string(store_file(dir))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    records.sort_by_key(Record::ts);
    records
}

fn save(dir: &Path, records: &[Record]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = String::new();
    for r in records {
        if let Ok(line) = serde_json::to_string(r) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    // Written beside the file and renamed over it, so a crash mid-write
    // leaves the previous history intact.
    let tmp = dir.join("space-history.jsonl.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, store_file(dir))
}

fn root_key(root: &Option<String>) -> String {
    root.as_deref().map(path_key).unwrap_or_default()
}

/// Drops what is past retention and thins the rest (see the module docs).
pub fn thin(mut records: Vec<Record>, now: u64) -> Vec<Record> {
    records.retain(|r| r.ts() + RETENTION_DAYS * DAY >= now);
    records.sort_by_key(Record::ts);

    let recent_from = now.saturating_sub(FULL_RESOLUTION);
    // Per old day: the last snapshot of each root that has folders, and the
    // last snapshot of any kind.
    let mut keep: HashSet<usize> = HashSet::new();
    let mut last_any: HashMap<u64, usize> = HashMap::new();
    let mut last_folders: HashMap<(u64, String), usize> = HashMap::new();
    for (i, r) in records.iter().enumerate() {
        match r {
            Record::Clean(_) => {
                keep.insert(i);
            }
            Record::Snapshot(s) if s.ts >= recent_from => {
                keep.insert(i);
            }
            Record::Snapshot(s) => {
                let day = s.ts / DAY;
                last_any.insert(day, i);
                if !s.folders.is_empty() {
                    last_folders.insert((day, root_key(&s.root)), i);
                }
            }
        }
    }
    keep.extend(last_any.values());
    keep.extend(last_folders.values());
    records.into_iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, r)| r).collect()
}

fn update(dir: &Path, now: u64, f: impl FnOnce(&mut Vec<Record>)) {
    let _lock = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = load(dir);
    f(&mut records);
    let _ = save(dir, &thin(records, now));
}

/// Stores a snapshot, replacing one with the same time and root (a Deep clean
/// scan stores its quick version first and the full one when it is done).
pub fn add_snapshot(dir: &Path, snap: Snapshot, now: u64) {
    update(dir, now, |records| {
        records.retain(|r| match r {
            Record::Snapshot(s) => !(s.ts == snap.ts && root_key(&s.root) == root_key(&snap.root)),
            _ => true,
        });
        records.push(Record::Snapshot(snap));
    });
}

pub fn add_run(dir: &Path, run: CleanRun, now: u64) {
    update(dir, now, |records| records.push(Record::Clean(run)));
}

pub fn snapshots(records: &[Record]) -> Vec<&Snapshot> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Snapshot(s) => Some(s),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- paths and text

/// Case-folded where the file system is, with "/" separators and no trailing one.
pub fn path_key(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

fn parent_key(key: &str) -> Option<&str> {
    key.rfind('/').map(|i| &key[..i])
}

/// Sizes in the units the file manager uses (see format.ts).
pub fn fmt_bytes(b: u64) -> String {
    let unit: f64 = if cfg!(target_os = "windows") { 1024.0 } else { 1000.0 };
    let b = b as f64;
    if b >= unit.powi(3) {
        format!("{:.1} GB", b / unit.powi(3))
    } else if b >= unit.powi(2) {
        format!("{:.0} MB", b / unit.powi(2))
    } else {
        format!("{:.0} KB", b / unit)
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

pub(crate) fn when(days: u64) -> String {
    match days {
        0 => "less than a day".into(),
        1 => "about a day".into(),
        2..=13 => format!("about {days} days"),
        14..=59 => format!("about {} weeks", (days as f64 / 7.0).round() as u64),
        60..=729 => format!("about {} months", (days as f64 / 30.4).round() as u64),
        _ => format!("about {:.1} years", days as f64 / 365.0),
    }
}

// ---------------------------------------------------------------- forecast

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    NotEnoughData,
    Steady,
    Filling,
    Freeing,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Forecast {
    pub mount: String,
    pub trend: Trend,
    /// Positive when the drive is filling up.
    pub bytes_per_day: i64,
    /// Only for a filling drive whose fill date is within three years.
    pub days_until_full: Option<u64>,
    pub readings: usize,
    pub span_days: u64,
    /// "none", "low", "fair" or "good": how far to trust the number.
    pub confidence: &'static str,
    /// The sentence to show.
    pub summary: String,
}

/// One day's last reading of a drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub day: u64,
    pub used: u64,
    pub total: u64,
}

/// The last reading of each day for `mount`, oldest first.
pub fn daily_points(snaps: &[&Snapshot], mount: &str) -> Vec<Point> {
    let key = path_key(mount);
    let mut by_day: std::collections::BTreeMap<u64, (u64, Point)> = Default::default();
    for s in snaps {
        let Some(d) = s.drives.iter().find(|d| path_key(&d.mount) == key) else { continue };
        let day = s.ts / DAY;
        let p = Point { day, used: d.used, total: d.total };
        match by_day.get(&day) {
            Some((ts, _)) if *ts > s.ts => {}
            _ => {
                by_day.insert(day, (s.ts, p));
            }
        }
    }
    by_day.into_values().map(|(_, p)| p).collect()
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Fits the last 30 days of `points` and says what it means. `points` must be
/// one per day, oldest first (see [`daily_points`]).
pub fn forecast(mount: &str, points: &[Point], now: u64) -> Forecast {
    let today = now / DAY;
    let window: Vec<Point> =
        points.iter().copied().filter(|p| p.day <= today && p.day + FORECAST_WINDOW_DAYS >= today).collect();
    let span = match (window.first(), window.last()) {
        (Some(a), Some(b)) => b.day - a.day,
        _ => 0,
    };
    let base = |trend, confidence, summary: String| Forecast {
        mount: mount.to_string(),
        trend,
        bytes_per_day: 0,
        days_until_full: None,
        readings: window.len(),
        span_days: span,
        confidence,
        summary,
    };

    if window.len() < FORECAST_MIN_READINGS || span < FORECAST_MIN_SPAN_DAYS {
        let days = plural(window.len() as u64, "day", "days");
        return base(
            Trend::NotEnoughData,
            "none",
            format!(
                "Not enough history to forecast {mount} yet: {days} of readings in the last {FORECAST_WINDOW_DAYS} days, and Jharu needs at least {FORECAST_MIN_READINGS} days spread over {FORECAST_MIN_SPAN_DAYS} or more. It records free space in the background, so this fills in by itself."
            ),
        );
    }

    // A sudden big drop is a cleanup or a deleted folder, not a trend. It
    // cuts the series in two, and only pairs of days on the same side of a
    // cut are compared; otherwise one cleanup would make a filling drive
    // look like it is emptying.
    let cleanup_min = (2u64 << 30).max(window[window.len() - 1].total / 100);
    let mut segment = vec![0usize; window.len()];
    let mut drops: Vec<u64> = vec![];
    for i in 1..window.len() {
        segment[i] = segment[i - 1];
        if window[i].used + cleanup_min < window[i - 1].used {
            segment[i] += 1;
            drops.push(window[i - 1].used - window[i].used);
        }
    }
    // Theil-Sen: the median of the slopes between pairs of days.
    let slopes_of = |same_segment: bool| {
        let mut slopes: Vec<f64> = vec![];
        for (i, a) in window.iter().enumerate() {
            for (j, b) in window.iter().enumerate().skip(i + 1) {
                let dx = (b.day - a.day) as f64;
                if dx > 0.0 && (!same_segment || segment[i] == segment[j]) {
                    slopes.push((b.used as f64 - a.used as f64) / dx);
                }
            }
        }
        slopes.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        slopes
    };
    let mut slopes = slopes_of(true);
    // Fewer than three comparable pairs: every day is its own segment, so
    // the drops are the trend and nothing is left out.
    let ignored = if slopes.len() < 3 {
        slopes = slopes_of(false);
        vec![]
    } else {
        drops
    };
    if slopes.is_empty() {
        return base(Trend::NotEnoughData, "none", format!("Not enough history to forecast {mount} yet."));
    }
    let slope = median(&slopes);
    let agree = slopes.iter().filter(|s| (**s > 0.0) == (slope > 0.0)).count() as f64 / slopes.len() as f64;
    let confidence = if span < 7 || window.len() < 5 {
        "low"
    } else if agree >= 0.75 && span >= 14 {
        "good"
    } else {
        "fair"
    };

    let last = window[window.len() - 1];
    let free = last.total.saturating_sub(last.used);
    let per_day = fmt_bytes(slope.abs() as u64);
    let over = format!("over the last {span} days");
    let cleanups = match ignored.iter().max() {
        None => String::new(),
        Some(biggest) => format!(
            " {} in that time ({} at most) looks like a cleanup and is left out of the rate.",
            if ignored.len() == 1 { "A sudden drop".to_string() } else { format!("{} sudden drops", ignored.len()) },
            fmt_bytes(*biggest)
        ),
    };
    let rough = if confidence == "low" {
        format!(" Only {} of data so far, so treat this as a rough guess.", plural(window.len() as u64, "day", "days"))
    } else {
        String::new()
    };
    let steady_below = (10u64 << 20).max(last.total / 10_000) as f64;

    let mut f = base(Trend::Steady, confidence, String::new());
    f.bytes_per_day = slope as i64;
    if slope.abs() < steady_below {
        f.summary = format!(
            "Used space on {mount} has been steady {over} (changing by less than {} a day), so there is no fill date to predict.{cleanups}",
            fmt_bytes(steady_below as u64)
        );
    } else if slope < 0.0 {
        f.trend = Trend::Freeing;
        f.summary = format!("Free space on {mount} has been growing by about {per_day} a day {over}, so it is not filling up.{cleanups}");
    } else {
        f.trend = Trend::Filling;
        let days = (free as f64 / slope).round() as u64;
        if days > FORECAST_HORIZON_DAYS {
            f.summary = format!(
                "{mount} is filling by about {per_day} a day {over}, but with {} free that would take more than 3 years.{cleanups}{rough}",
                fmt_bytes(free)
            );
        } else {
            f.days_until_full = Some(days);
            f.summary = format!(
                "At this rate {mount} is full in {}: it has been filling by about {per_day} a day {over}, with {} free now.{cleanups}{rough}",
                when(days),
                fmt_bytes(free)
            );
        }
    }
    f
}

/// A forecast for every drive that has a reading.
pub fn forecasts(records: &[Record], now: u64) -> Vec<Forecast> {
    let snaps = snapshots(records);
    let mut mounts: Vec<String> = vec![];
    for s in &snaps {
        for d in &s.drives {
            if !mounts.iter().any(|m| path_key(m) == path_key(&d.mount)) {
                mounts.push(d.mount.clone());
            }
        }
    }
    mounts.sort();
    mounts.iter().map(|m| forecast(m, &daily_points(&snaps, m), now)).collect()
}

// ---------------------------------------------------------------- what grew

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FolderChange {
    pub path: String,
    /// The path below the scanned root, for display.
    pub label: String,
    pub old_bytes: u64,
    pub new_bytes: u64,
    pub delta: i64,
    /// Not among the folders listed in the older snapshot.
    pub is_new: bool,
    /// Not among the folders listed in the newer snapshot.
    pub is_gone: bool,
    /// Most of this change is inside a smaller folder that is listed too, so
    /// that one is the better answer to "what changed".
    pub covered: bool,
}

fn label_for(path: &str, root: &str) -> String {
    match below_root(path, root) {
        Some(rel) => rel.to_string(),
        None => path.to_string(),
    }
}

/// `path` below `root` (without the separator), or None when it is not below it.
fn below_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let r = root.trim_end_matches(['/', '\\']);
    // "\\" and "/" are one byte each, so lengths agree whatever the separators.
    let head = path.get(..r.len())?;
    if path_key(head) != path_key(r) || !path[r.len()..].starts_with(['/', '\\']) {
        return None;
    }
    let rel = path[r.len()..].trim_start_matches(['/', '\\']);
    (!rel.is_empty()).then_some(rel)
}

/// How deep `path` is below `root` (1 for a direct child) and the key of its
/// top-level folder.
fn level_of(path: &str, root: &str) -> Option<(usize, String)> {
    let rel = below_root(path, root)?;
    let mut parts = rel.split(['/', '\\']).filter(|p| !p.is_empty());
    let first = parts.next()?;
    let depth = 1 + parts.count();
    Some((depth, path_key(&format!("{}/{}", root.trim_end_matches(['/', '\\']), first))))
}

/// Whether `snap` would have listed `path` had it been over its floor: deeper
/// levels were only measured under the folders in `detail`.
fn looked_at(snap: &Snapshot, path: &str) -> bool {
    let root = snap.root.as_deref().unwrap_or_default();
    match level_of(path, root) {
        Some((1, _)) => true,
        Some((_, top)) => snap.detail.iter().any(|d| path_key(d) == top),
        None => false,
    }
}

/// Folders whose size changed by at least [`MIN_CHANGE`] between two folder
/// snapshots: (grown, shrunk), the biggest changes first.
pub fn diff_folders(old: &Snapshot, new: &Snapshot) -> (Vec<FolderChange>, Vec<FolderChange>) {
    let old_map: HashMap<String, &FolderSize> = old.folders.iter().map(|f| (path_key(&f.path), f)).collect();
    let new_map: HashMap<String, &FolderSize> = new.folders.iter().map(|f| (path_key(&f.path), f)).collect();
    let root = new.root.clone().unwrap_or_default();

    let mut all: Vec<FolderChange> = vec![];
    for (key, n) in &new_map {
        let o = old_map.get(key);
        // Unlisted then means "at most the floor", so growth is at least this.
        // A folder the older snapshot never looked at (a level it did not
        // measure) says nothing about how big it was: its top-level folder
        // carries the change instead.
        if o.is_none() && !looked_at(old, &n.path) {
            continue;
        }
        let old_bytes = o.map(|o| o.bytes).unwrap_or(old.floor.min(n.bytes));
        all.push(change(&n.path, &root, old_bytes, n.bytes, o.is_none(), false));
    }
    for (key, o) in &old_map {
        if !new_map.contains_key(key) && looked_at(new, &o.path) {
            let new_bytes = new.floor.min(o.bytes);
            all.push(change(&o.path, &root, o.bytes, new_bytes, false, true));
        }
    }
    all.retain(|c| c.delta.unsigned_abs() >= MIN_CHANGE);

    // A change is "covered" when listed folders directly inside it account
    // for most of it.
    let mut inside: HashMap<(String, bool), i64> = HashMap::new();
    for c in &all {
        if let Some(parent) = parent_key(&path_key(&c.path)) {
            *inside.entry((parent.to_string(), c.delta > 0)).or_default() += c.delta;
        }
    }
    for c in &mut all {
        let explained = inside.get(&(path_key(&c.path), c.delta > 0)).copied().unwrap_or(0);
        c.covered = explained.abs() as f64 >= 0.7 * c.delta.abs() as f64;
    }

    let mut grown: Vec<FolderChange> = all.iter().filter(|c| c.delta > 0).cloned().collect();
    let mut shrunk: Vec<FolderChange> = all.into_iter().filter(|c| c.delta < 0).collect();
    grown.sort_by(|a, b| b.delta.cmp(&a.delta).then(a.path.cmp(&b.path)));
    shrunk.sort_by(|a, b| a.delta.cmp(&b.delta).then(a.path.cmp(&b.path)));
    grown.truncate(GROWN_SHOWN * 3);
    shrunk.truncate(SHRUNK_SHOWN * 3);
    (grown, shrunk)
}

fn change(path: &str, root: &str, old_bytes: u64, new_bytes: u64, is_new: bool, is_gone: bool) -> FolderChange {
    FolderChange {
        path: path.to_string(),
        label: label_for(path, root),
        old_bytes,
        new_bytes,
        delta: new_bytes as i64 - old_bytes as i64,
        is_new,
        is_gone,
        covered: false,
    }
}

/// The folder snapshot to compare against by default: the one closest to a
/// week before `latest`, never one taken within the hour before it.
pub fn pick_baseline(candidates: &[u64], latest: u64) -> Option<u64> {
    candidates
        .iter()
        .copied()
        .filter(|ts| *ts + 3600 <= latest)
        .min_by_key(|ts| (latest - ts).abs_diff(DEFAULT_LOOKBACK))
}

#[derive(Serialize, Clone, Debug)]
pub struct DriveDelta {
    pub mount: String,
    pub total: u64,
    pub used_from: u64,
    pub used_to: u64,
    pub free_from: u64,
    pub free_to: u64,
    /// Positive when free space grew.
    pub free_delta: i64,
}

pub fn drive_deltas(old: &Snapshot, new: &Snapshot) -> Vec<DriveDelta> {
    new.drives
        .iter()
        .filter_map(|n| {
            let o = old.drives.iter().find(|o| path_key(&o.mount) == path_key(&n.mount))?;
            Some(DriveDelta {
                mount: n.mount.clone(),
                total: n.total,
                used_from: o.used,
                used_to: n.used,
                free_from: o.free,
                free_to: n.free,
                free_delta: n.free as i64 - o.free as i64,
            })
        })
        .collect()
}

#[derive(Serialize, Clone, Debug)]
pub struct Comparison {
    /// False when there is nothing to compare; `note` says why.
    pub available: bool,
    pub note: Option<String>,
    pub root: Option<String>,
    pub from_ts: u64,
    pub to_ts: u64,
    pub drives: Vec<DriveDelta>,
    pub grown: Vec<FolderChange>,
    pub shrunk: Vec<FolderChange>,
}

fn unavailable(note: &str, root: Option<String>) -> Comparison {
    Comparison {
        available: false,
        note: Some(note.to_string()),
        root,
        from_ts: 0,
        to_ts: 0,
        drives: vec![],
        grown: vec![],
        shrunk: vec![],
    }
}

/// Compares the latest folder snapshot under `root` (default: the root of the
/// latest folder snapshot) with `from_ts` (default: about a week earlier).
pub fn compare(records: &[Record], root: Option<&str>, from_ts: Option<u64>) -> Comparison {
    let snaps = snapshots(records);
    let with_folders: Vec<&Snapshot> = snaps.iter().copied().filter(|s| !s.folders.is_empty()).collect();
    let root_wanted = match root {
        Some(r) => Some(path_key(r)),
        None => with_folders.last().map(|s| root_key(&s.root)),
    };
    let Some(root_wanted) = root_wanted else {
        return unavailable(
            "No folder snapshot yet. Scan a drive or your home folder in Deep clean, or take a snapshot here, and Jharu will start remembering where the space is.",
            None,
        );
    };
    let same_root: Vec<&Snapshot> = with_folders.iter().copied().filter(|s| root_key(&s.root) == root_wanted).collect();
    let Some(latest) = same_root.last().copied() else {
        return unavailable("No folder snapshot of that folder yet.", root.map(String::from));
    };
    if same_root.len() < 2 {
        return unavailable(
            "Only one folder snapshot of this folder so far. Come back after the next Deep clean scan or snapshot and Jharu will show what changed in between.",
            latest.root.clone(),
        );
    }
    let earlier: Vec<u64> = same_root.iter().filter(|s| s.ts < latest.ts).map(|s| s.ts).collect();
    let from = from_ts.filter(|t| earlier.contains(t)).or_else(|| pick_baseline(&earlier, latest.ts)).or_else(|| earlier.last().copied());
    let Some(old) = from.and_then(|t| same_root.iter().copied().find(|s| s.ts == t)) else {
        return unavailable("No earlier snapshot to compare with.", latest.root.clone());
    };

    let (grown, shrunk) = diff_folders(old, latest);
    Comparison {
        available: true,
        note: None,
        root: latest.root.clone(),
        from_ts: old.ts,
        to_ts: latest.ts,
        drives: drive_deltas(old, latest),
        grown,
        shrunk,
    }
}

// ---------------------------------------------------------------- reading the machine

pub fn read_drives() -> Vec<DriveReading> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut out: Vec<DriveReading> = disks
        .list()
        .iter()
        .filter(|d| !d.mount_point().to_string_lossy().starts_with("/System/Volumes") && d.total_space() > 0)
        .map(|d| DriveReading {
            mount: d.mount_point().to_string_lossy().into_owned(),
            total: d.total_space(),
            used: d.total_space().saturating_sub(d.available_space()),
            free: d.available_space(),
            removable: d.is_removable(),
        })
        .collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out.dedup_by(|a, b| a.mount == b.mount);
    out
}

pub fn data_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A drive-only reading, unless one was stored within the last hour.
pub fn record_drives(dir: &Path, now: u64) -> bool {
    let records = load(dir);
    if snapshots(&records).last().is_some_and(|s| s.ts + 3600 > now) {
        return false;
    }
    let snap = Snapshot { ts: now, source: "background".into(), drives: read_drives(), root: None, folders: vec![], floor: 0, detail: vec![] };
    add_snapshot(dir, snap, now);
    true
}

#[derive(Serialize, Clone)]
struct Progress {
    stage: String,
    done: usize,
    total: usize,
    current: String,
}

pub(crate) fn emit_progress(app: &tauri::AppHandle, stage: &str, done: usize, total: usize, current: &str) {
    let _ = app.emit("history-progress", Progress { stage: stage.into(), done, total, current: current.into() });
}

/// What one top-level folder holds, found in a single walk.
struct Levels {
    total: u64,
    /// Folders one and two levels below it, with their sizes.
    below: Vec<(PathBuf, u64)>,
}

/// Walks `folder` once, counting files as Deep clean does, and adds each
/// file's bytes to the folders one and two levels below `folder`.
fn measure_levels(folder: &Path, seen: &mut Seen) -> Option<Levels> {
    let mut usage = Usage::default();
    let mut one: HashMap<OsString, (u64, HashMap<OsString, u64>)> = HashMap::new();
    let walker = crate::walk::walk(folder).skip_hidden(false).process_read_dir(|_, _, _, children| {
        children.retain(|c| c.as_ref().map(|e| !crate::deep::excluded(&e.path())).unwrap_or(true));
    });
    let mut visited = 0u64;
    for entry in walker.into_iter().flatten() {
        visited += 1;
        if visited & 0x0fff == 0 && CANCEL.load(Ordering::Relaxed) {
            return None;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let before = usage.bytes;
        let path = entry.path();
        usage.add_file(&path, &meta, seen);
        let bytes = usage.bytes - before;
        if bytes == 0 {
            continue;
        }
        let Ok(rel) = path.strip_prefix(folder) else { continue };
        let mut parts = rel.components();
        // A file directly in `folder` belongs to no folder below it.
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else { continue };
        let a = a.as_os_str();
        if !one.contains_key(a) {
            one.insert(a.to_owned(), (0, HashMap::new()));
        }
        let slot = one.get_mut(a)?;
        slot.0 += bytes;
        // `b` names a folder only when something follows it.
        if parts.next().is_some() {
            let b = b.as_os_str();
            match slot.1.get_mut(b) {
                Some(v) => *v += bytes,
                None => {
                    slot.1.insert(b.to_owned(), bytes);
                }
            }
        }
    }
    let mut below = vec![];
    for (a, (bytes, children)) in one {
        let a_path = folder.join(&a);
        below.push((a_path.clone(), bytes));
        for (b, bytes) in children {
            below.push((a_path.join(b), bytes));
        }
    }
    Some(Levels { total: usage.bytes, below })
}

/// Keeps the largest [`MAX_FOLDERS`] entries over [`MIN_FOLDER_BYTES`] and
/// reports the size below which nothing is listed.
pub fn finalize(mut folders: Vec<FolderSize>) -> (Vec<FolderSize>, u64) {
    folders.retain(|f| f.bytes >= MIN_FOLDER_BYTES);
    folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    let mut floor = MIN_FOLDER_BYTES;
    if folders.len() > MAX_FOLDERS {
        floor = floor.max(folders[MAX_FOLDERS].bytes);
        folders.truncate(MAX_FOLDERS);
    }
    (folders, floor)
}

/// One top-level folder of the scanned root.
struct Top {
    path: PathBuf,
    /// Size Deep clean measured, when it did.
    known: Option<u64>,
    descend: bool,
}

/// The folders to store, the size below which nothing is listed, and the
/// top-level folders that were walked two levels down.
fn build_folders(app: &tauri::AppHandle, tops: Vec<Top>, stage: &str) -> Option<(Vec<FolderSize>, u64, Vec<String>)> {
    let mut seen = Seen::default();
    let mut out: Vec<FolderSize> = vec![];
    let mut detail: Vec<String> = vec![];
    let total = tops.len();
    for (i, top) in tops.into_iter().enumerate() {
        if CANCEL.load(Ordering::Relaxed) {
            return None;
        }
        let name = top.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        emit_progress(app, stage, i, total, &name);
        if !top.descend && top.known.is_some() {
            out.push(FolderSize { path: top.path.to_string_lossy().into_owned(), bytes: top.known.unwrap_or(0) });
            continue;
        }
        if top.path.is_file() {
            let mut usage = Usage::default();
            if let Ok(meta) = top.path.symlink_metadata() {
                usage.add_file(&top.path, &meta, &mut seen);
            }
            out.push(FolderSize { path: top.path.to_string_lossy().into_owned(), bytes: top.known.unwrap_or(usage.bytes) });
            continue;
        }
        let levels = measure_levels(&top.path, &mut seen)?;
        detail.push(top.path.to_string_lossy().into_owned());
        out.push(FolderSize { path: top.path.to_string_lossy().into_owned(), bytes: top.known.unwrap_or(levels.total) });
        out.extend(levels.below.into_iter().map(|(p, b)| FolderSize { path: p.to_string_lossy().into_owned(), bytes: b }));
    }
    emit_progress(app, stage, total, total, "");
    let (folders, floor) = finalize(out);
    Some((folders, floor, detail))
}

/// Only a drive root or the home folder gets a folder snapshot: anything
/// deeper is a fragment that no later scan would compare with.
pub fn is_snapshot_root(root: &Path) -> bool {
    root.parent().is_none() || dirs::home_dir().is_some_and(|h| path_key(&h.to_string_lossy()) == path_key(&root.to_string_lossy()))
}

/// Called when a Deep clean scan of `root` finishes. Stores what Deep clean
/// measured at once, then adds the two levels below the biggest folders in
/// the background and replaces the snapshot with the full one.
pub fn record_deep(app: &tauri::AppHandle, root: &Path, entries: &[crate::deep::DeepEntry]) {
    if !is_snapshot_root(root) {
        return;
    }
    let Some(dir) = data_dir(app) else { return };
    let ts = now();
    let root_s = root.to_string_lossy().into_owned();
    let drives = read_drives();

    let level_one: Vec<FolderSize> = entries
        .iter()
        .filter(|e| !e.is_virtual && e.size_bytes >= MIN_FOLDER_BYTES)
        .map(|e| FolderSize { path: e.path.clone(), bytes: e.size_bytes })
        .collect();
    let (folders, floor) = finalize(level_one);
    add_snapshot(
        &dir,
        Snapshot { ts, source: "deep".into(), drives: drives.clone(), root: Some(root_s.clone()), folders, floor, detail: vec![] },
        ts,
    );

    let measured: u64 = entries.iter().filter(|e| !e.is_virtual).map(|e| e.size_bytes).sum();
    let mut tops: Vec<Top> = entries
        .iter()
        .filter(|e| !e.is_virtual)
        .map(|e| Top { path: PathBuf::from(&e.path), known: Some(e.size_bytes), descend: false })
        .collect();
    // Descend only into the biggest folders that Jharu can ever clean.
    let big = (measured / 100).max(256 << 20);
    let mut order: Vec<usize> = (0..entries.len())
        .filter(|i| entries[*i].is_dir && !entries[*i].protected && !entries[*i].is_virtual && entries[*i].size_bytes >= big)
        .collect();
    order.sort_by(|a, b| entries[*b].size_bytes.cmp(&entries[*a].size_bytes));
    for i in order.into_iter().take(MAX_DESCEND) {
        tops[i].descend = true;
    }
    if !tops.iter().any(|t| t.descend) || SNAPSHOTTING.swap(true, Ordering::SeqCst) {
        return;
    }

    CANCEL.store(false, Ordering::SeqCst);
    let app = app.clone();
    std::thread::Builder::new()
        .name("jharu-snapshot".into())
        .spawn(move || {
            let _busy = autocare::activity();
            let _flag = SnapshottingFlag;
            if let Some((folders, floor, detail)) = build_folders(&app, tops, "Adding detail to the space history") {
                // Level one keeps the sizes Deep clean showed; entries that
                // were not walked are already in `tops` as they were measured.
                add_snapshot(
                    &dir,
                    Snapshot { ts, source: "deep".into(), drives, root: Some(root_s), folders, floor, detail },
                    ts,
                );
                let _ = app.emit("history-snapshot", ts);
            }
        })
        .ok();
}

/// A full snapshot of `root` measured here, for a user who has not run Deep clean.
fn snapshot_now_impl(app: &tauri::AppHandle, root: &Path) -> Result<u64, String> {
    let dir = data_dir(app).ok_or("Jharu could not open its data folder.")?;
    let mut children: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|e| format!("Cannot read {}: {e}", root.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| !crate::deep::excluded(p) && !p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false))
        .collect();
    children.sort();
    let tops: Vec<Top> = children.into_iter().map(|path| Top { path, known: None, descend: true }).collect();
    let ts = now();
    let drives = read_drives();
    let (folders, floor, detail) = build_folders(app, tops, "Measuring folders").ok_or("Cancelled.")?;
    add_snapshot(
        &dir,
        Snapshot { ts, source: "manual".into(), drives, root: Some(root.to_string_lossy().into_owned()), folders, floor, detail },
        ts,
    );
    Ok(ts)
}

// ---------------------------------------------------------------- commands

#[derive(Serialize)]
pub struct SeriesPoint {
    pub ts: u64,
    pub used: u64,
}

#[derive(Serialize)]
pub struct DriveSeries {
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub removable: bool,
    pub points: Vec<SeriesPoint>,
    pub forecast: Forecast,
}

#[derive(Serialize)]
pub struct SnapshotInfo {
    pub ts: u64,
    pub source: String,
    pub folders: usize,
}

#[derive(Serialize)]
pub struct RootInfo {
    pub root: String,
    /// Newest first.
    pub snapshots: Vec<SnapshotInfo>,
}

#[derive(Serialize)]
pub struct Overview {
    pub now: u64,
    pub drives: Vec<DriveSeries>,
    pub roots: Vec<RootInfo>,
    pub runs: Vec<CleanRun>,
    pub settings: Settings,
    pub state: State,
    pub busy: bool,
    pub retention_days: u64,
}

pub fn overview(dir: &Path, now: u64) -> Overview {
    let records = load(dir);
    let snaps = snapshots(&records);
    let forecasts = forecasts(&records, now);

    let mut drives = vec![];
    for f in forecasts {
        // The latest reading of the drive, wherever it was taken.
        let Some(d) = snaps.iter().rev().find_map(|s| s.drives.iter().find(|d| path_key(&d.mount) == path_key(&f.mount)))
        else {
            continue;
        };
        let points = daily_points(&snaps, &f.mount).into_iter().map(|p| SeriesPoint { ts: p.day * DAY, used: p.used }).collect();
        drives.push(DriveSeries {
            mount: d.mount.clone(),
            total: d.total,
            used: d.used,
            free: d.free,
            removable: d.removable,
            points,
            forecast: f,
        });
    }
    drives.sort_by(|a, b| b.total.cmp(&a.total));

    let mut roots: Vec<RootInfo> = vec![];
    for s in snaps.iter().rev().filter(|s| !s.folders.is_empty()) {
        let Some(root) = &s.root else { continue };
        let info = SnapshotInfo { ts: s.ts, source: s.source.clone(), folders: s.folders.len() };
        match roots.iter_mut().find(|r| path_key(&r.root) == path_key(root)) {
            Some(r) => r.snapshots.push(info),
            None => roots.push(RootInfo { root: root.clone(), snapshots: vec![info] }),
        }
    }

    let mut runs: Vec<CleanRun> = records
        .iter()
        .filter_map(|r| match r {
            Record::Clean(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    runs.reverse();
    runs.truncate(20);

    Overview {
        now,
        drives,
        roots,
        runs,
        settings: autocare::load_settings(dir),
        state: autocare::load_state(dir),
        busy: autocare::busy(),
        retention_days: RETENTION_DAYS,
    }
}

#[tauri::command]
pub async fn history_overview(app: tauri::AppHandle) -> Result<Overview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = data_dir(&app).ok_or("Jharu could not open its data folder.")?;
        Ok(overview(&dir, now()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn history_compare(app: tauri::AppHandle, root: Option<String>, from_ts: Option<u64>) -> Result<Comparison, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = data_dir(&app).ok_or("Jharu could not open its data folder.")?;
        Ok(compare(&load(&dir), root.as_deref(), from_ts))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The forecast of every drive, for the Drive health tab.
#[tauri::command]
pub async fn history_forecasts(app: tauri::AppHandle) -> Result<Vec<Forecast>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = data_dir(&app).ok_or("Jharu could not open its data folder.")?;
        Ok(forecasts(&load(&dir), now()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct PathVerdict {
    pub exists: bool,
    /// System locations: Jharu never offers to remove them.
    pub protected: bool,
    pub hint: String,
    pub verdict: Option<crate::places::Verdict>,
}

/// What Deep clean would say about removing `path`: the same rating, from the
/// same signals, measured now.
#[tauri::command]
pub async fn history_verdict(path: String) -> Result<PathVerdict, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = autocare::activity();
        let path = PathBuf::from(path);
        if !path.exists() {
            return PathVerdict { exists: false, protected: false, hint: "This folder no longer exists.".into(), verdict: None };
        }
        let (_, protected, hint) = crate::deep::classify(&path);
        if protected {
            return PathVerdict { exists: true, protected, hint, verdict: None };
        }
        let home = dirs::home_dir().unwrap_or_default();
        let (_, signals) = crate::deep::measure(&path, &mut Seen::default());
        let facts = crate::safety::Facts::gather();
        PathVerdict { exists: true, protected, hint, verdict: Some(crate::places::verdict(&path, &home, &signals, &facts)) }
    })
    .await
    .map_err(|e| e.to_string())
}

/// Measures `root` (default: the home folder) and stores a folder snapshot.
#[tauri::command]
pub async fn history_snapshot_now(app: tauri::AppHandle, root: Option<String>) -> Result<u64, String> {
    if SNAPSHOTTING.swap(true, Ordering::SeqCst) {
        return Err("A snapshot is already being taken.".into());
    }
    CANCEL.store(false, Ordering::SeqCst);
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _busy = autocare::activity();
        let root = root.map(PathBuf::from).or_else(dirs::home_dir).ok_or("Jharu could not find your home folder.")?;
        snapshot_now_impl(&app, &root)
    })
    .await
    .map_err(|e| e.to_string());
    SNAPSHOTTING.store(false, Ordering::SeqCst);
    result?
}

#[tauri::command]
pub fn history_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_760_000_000;

    fn drive(mount: &str, used_gb: u64, total_gb: u64) -> DriveReading {
        DriveReading { mount: mount.into(), total: total_gb << 30, used: used_gb << 30, free: (total_gb - used_gb) << 30, removable: false }
    }

    fn folder(path: &str, mb: u64) -> FolderSize {
        FolderSize { path: path.into(), bytes: mb << 20 }
    }

    fn snap(ts: u64, root: Option<&str>, used_gb: u64, folders: Vec<FolderSize>) -> Snapshot {
        Snapshot { ts, source: "deep".into(), drives: vec![drive("C:\\", used_gb, 500)], root: root.map(String::from), folders, floor: 20 << 20, detail: vec![] }
    }

    fn points(readings: &[(u64, u64)]) -> Vec<Point> {
        readings.iter().map(|(d, u)| Point { day: *d, used: u << 30, total: 500 << 30 }).collect()
    }

    // ------------------------------------------------------------ diff

    #[test]
    fn diff_names_the_folders_that_grew_and_shrank() {
        let mut old = snap(T0, Some("C:\\"), 300, vec![
            folder("C:\\Users", 100_000),
            folder("C:\\Users\\me", 90_000),
            folder("C:\\Users\\me\\Screen recordings", 20_000),
            folder("C:\\Users\\me\\editor", 5_000),
            folder("C:\\Old projects", 30_000),
            folder("C:\\Windows", 25_000),
        ]);
        let mut new = snap(T0 + 7 * DAY, Some("C:\\"), 318, vec![
            folder("C:\\Users", 118_000),
            folder("C:\\Users\\me", 108_000),
            folder("C:\\Users\\me\\Screen recordings", 32_000),
            folder("C:\\Users\\me\\editor", 11_000),
            folder("C:\\Users\\me\\editor\\target", 6_000),
            folder("C:\\Windows", 25_010),
        ]);
        old.detail = vec!["C:\\Users".into()];
        new.detail = vec!["C:\\Users".into()];
        let (grown, shrunk) = diff_folders(&old, &new);

        let screen = grown.iter().find(|c| c.path.ends_with("Screen recordings")).unwrap();
        assert_eq!(screen.delta, 12_000 << 20);
        assert_eq!(screen.label, "Users\\me\\Screen recordings");
        assert!(!screen.is_new && !screen.covered);

        // Windows changed by 10 MB: below the noise floor, so not listed.
        assert!(grown.iter().all(|c| !c.path.ends_with("Windows")));

        // target is new: it was not listed before, so it grew by at least its
        // size minus the old snapshot's floor.
        let target = grown.iter().find(|c| c.path.ends_with("target")).unwrap();
        assert!(target.is_new);
        assert_eq!(target.delta, (6_000i64 << 20) - (20i64 << 20));

        // Users and Users\me grew only through the folders inside them.
        assert!(grown.iter().find(|c| c.path == "C:\\Users").unwrap().covered);
        assert!(grown.iter().find(|c| c.path == "C:\\Users\\me").unwrap().covered);
        // editor grew +6 GB, of which target explains +5.98: covered.
        assert!(grown.iter().find(|c| c.path.ends_with("editor")).unwrap().covered);

        // Sorted by size of change.
        assert!(grown.windows(2).all(|w| w[0].delta >= w[1].delta));

        // "old projects" is not listed any more: gone, shrunk by at least
        // its size minus the new snapshot's floor.
        let gone = shrunk.iter().find(|c| c.path.ends_with("Old projects")).unwrap();
        assert!(gone.is_gone);
        assert_eq!(gone.delta, -((30_000i64 << 20) - (20i64 << 20)));
    }

    #[test]
    fn diff_does_not_invent_growth_below_levels_the_older_snapshot_never_measured() {
        // The older snapshot is the quick one from a Deep clean scan: top-level
        // folders only. The newer one also measured two levels under Users.
        let old = snap(T0, Some("C:\\"), 300, vec![folder("C:\\Users", 50_000), folder("C:\\Data", 1_000)]);
        let mut new = snap(
            T0 + 7 * DAY,
            Some("C:\\"),
            305,
            vec![
                folder("C:\\Users", 50_100),
                folder("C:\\Users\\me", 40_000),
                folder("C:\\Users\\me\\Videos", 30_000),
                folder("C:\\Data", 6_000),
            ],
        );
        new.detail = vec!["C:\\Users".into()];
        let (grown, shrunk) = diff_folders(&old, &new);
        // Users/me were listed by the newer snapshot only because it looked
        // deeper, not because they appeared: nothing is claimed about them.
        assert!(grown.iter().all(|c| !c.path.contains("me")), "{grown:?}");
        // Level one was always measured, so Data's real growth stays.
        assert_eq!(grown.iter().find(|c| c.path.ends_with("Data")).unwrap().delta, 5_000 << 20);
        // And the other way round: the older, deeper snapshot lists folders the
        // newer, quick one never looked at, so they did not "shrink".
        let (_, shrunk_back) = diff_folders(&new, &old);
        assert!(shrunk_back.iter().all(|c| !c.path.contains("me")), "{shrunk_back:?}");
        assert!(shrunk.is_empty());
    }

    #[test]
    fn paths_below_a_root_and_their_levels() {
        assert_eq!(below_root("C:\\Users\\me", "C:\\"), Some("Users\\me"));
        assert_eq!(below_root("c:/users/me", "C:\\"), if cfg!(any(target_os = "windows", target_os = "macos")) { Some("users/me") } else { None });
        assert_eq!(below_root("C:\\Users", "C:\\Users"), None);
        assert_eq!(below_root("C:\\UsersX", "C:\\Users"), None);
        assert_eq!(below_root("/Users/me", "/"), Some("Users/me"));
        // A folder name whose lower-case form is longer must not panic.
        assert_eq!(below_root("C:\\İİ\\x", "C:\\i"), None);
        assert_eq!(level_of("C:\\Users", "C:\\").unwrap().0, 1);
        assert_eq!(level_of("C:\\Users\\me\\x", "C:\\").unwrap().0, 3);
    }

    #[test]
    fn diff_ignores_case_and_slash_style() {
        let old = snap(T0, Some("C:\\"), 1, vec![folder("C:\\Users\\Me", 1_000)]);
        let new = snap(T0 + DAY, Some("C:\\"), 1, vec![folder("c:/users/me", 1_000)]);
        let (grown, shrunk) = diff_folders(&old, &new);
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert!(grown.is_empty() && shrunk.is_empty());
        }
    }

    #[test]
    fn drive_delta_is_free_space_lost() {
        let old = snap(T0, Some("C:\\"), 300, vec![]);
        let new = snap(T0 + 7 * DAY, Some("C:\\"), 318, vec![]);
        let d = drive_deltas(&old, &new);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].free_delta, -(18i64 << 30));
        assert_eq!(d[0].used_to - d[0].used_from, 18 << 30);
    }

    #[test]
    fn baseline_is_closest_to_a_week_and_never_the_last_hour() {
        let latest = T0 + 30 * DAY;
        let c = [latest - 1800, latest - DAY, latest - 6 * DAY, latest - 20 * DAY];
        assert_eq!(pick_baseline(&c, latest), Some(latest - 6 * DAY));
        assert_eq!(pick_baseline(&[latest - 1800], latest), None);
        assert_eq!(pick_baseline(&[latest - DAY, latest - 3 * DAY], latest), Some(latest - 3 * DAY));
    }

    #[test]
    fn compare_uses_one_root_and_explains_when_it_cannot() {
        let none = compare(&[], None, None);
        assert!(!none.available && none.note.is_some());

        let one = vec![Record::Snapshot(snap(T0, Some("C:\\"), 1, vec![folder("C:\\a", 100)]))];
        assert!(!compare(&one, None, None).available, "one snapshot cannot be compared");

        let recs = vec![
            Record::Snapshot(snap(T0, Some("C:\\"), 300, vec![folder("C:\\a", 1_000)])),
            Record::Snapshot(snap(T0 + DAY, Some("C:\\Users\\me"), 301, vec![folder("C:\\Users\\me\\x", 100)])),
            Record::Snapshot(snap(T0 + 7 * DAY, Some("C:\\"), 310, vec![folder("C:\\a", 3_000)])),
        ];
        let c = compare(&recs, None, None);
        assert!(c.available);
        assert_eq!(c.root.as_deref(), Some("C:\\"));
        assert_eq!((c.from_ts, c.to_ts), (T0, T0 + 7 * DAY), "the home snapshot is not a baseline for C:\\");
        assert_eq!(c.grown.len(), 1);
        assert_eq!(c.grown[0].delta, 2_000 << 20);
        // A baseline that is not one of this root's snapshots falls back to the default.
        assert_eq!(compare(&recs, None, Some(T0 + DAY)).from_ts, T0);
    }

    // ------------------------------------------------------------ thinning

    fn fsnap(ts: u64, root: &str) -> Record {
        Record::Snapshot(snap(ts, Some(root), 1, vec![folder("x", 100)]))
    }

    fn dsnap(ts: u64) -> Record {
        Record::Snapshot(Snapshot { ts, source: "background".into(), drives: vec![drive("C:\\", 1, 2)], root: None, folders: vec![], floor: 0, detail: vec![] })
    }

    #[test]
    fn thinning_keeps_recent_all_and_old_one_per_day() {
        let now = T0 + 60 * DAY;
        let day = |n: u64| now - n * DAY;
        let mut recs = vec![];
        // Today: everything stays.
        for i in 0..5 {
            recs.push(dsnap(now - i * 600));
        }
        // 10 days ago: four drive readings, two folder snapshots of C:\ and one of home.
        for i in 0..4 {
            recs.push(dsnap(day(10) + i * 3600));
        }
        recs.push(fsnap(day(10) + 10_000, "C:\\"));
        recs.push(fsnap(day(10) + 20_000, "C:\\"));
        recs.push(fsnap(day(10) + 15_000, "C:\\Users\\me"));
        // Older than retention.
        recs.push(dsnap(now - 91 * DAY));
        recs.push(fsnap(now - 120 * DAY, "C:\\"));
        // A cleaning record is history too.
        recs.push(Record::Clean(CleanRun { ts: day(30), trigger: "scheduled".into(), freed_bytes: 1, removed: vec![], skipped: vec![], errors: vec![] }));

        let out = thin(recs, now);
        let today: Vec<_> = out.iter().filter(|r| r.ts() >= now - 3600).collect();
        assert_eq!(today.len(), 5, "recent readings are all kept");

        let old: Vec<&Record> = out.iter().filter(|r| r.ts() / DAY == day(10) / DAY && matches!(r, Record::Snapshot(_))).collect();
        // Last folder snapshot of each root, plus the day's last reading of any kind.
        let folder_roots: HashSet<String> = old
            .iter()
            .filter_map(|r| match r {
                Record::Snapshot(s) if !s.folders.is_empty() => s.root.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(folder_roots.len(), 2);
        assert!(old.len() <= 3, "got {}", old.len());
        let c_snaps: Vec<_> = old.iter().filter(|r| matches!(r, Record::Snapshot(s) if s.root.as_deref() == Some("C:\\"))).collect();
        assert_eq!(c_snaps.len(), 1);
        assert_eq!(c_snaps[0].ts(), day(10) + 20_000, "the later of the two");

        assert!(out.iter().all(|r| r.ts() + RETENTION_DAYS * DAY >= now), "retention");
        assert!(out.iter().any(|r| matches!(r, Record::Clean(_))), "clean records survive");
        assert!(out.windows(2).all(|w| w[0].ts() <= w[1].ts()));
    }

    #[test]
    fn a_year_of_four_hourly_readings_stays_small() {
        let now = T0 + 400 * DAY;
        let recs: Vec<Record> = (0..400 * 6).map(|i| dsnap(T0 + i * 4 * 3600)).collect();
        let out = thin(recs, now);
        assert!(out.len() <= (RETENTION_DAYS as usize) + 16, "kept {}", out.len());
    }

    // ------------------------------------------------------------ store

    #[test]
    fn store_round_trips_and_survives_a_bad_line() {
        let dir = std::env::temp_dir().join(format!("jharu history {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = T0;
        add_snapshot(&dir, snap(now - 100, Some("C:\\"), 10, vec![folder("C:\\a", 300)]), now);
        // The same time and root again replaces the record.
        add_snapshot(&dir, snap(now - 100, Some("C:\\"), 10, vec![folder("C:\\a", 300), folder("C:\\b", 40)]), now);
        add_snapshot(&dir, snap(now - 50, Some("C:\\"), 11, vec![]), now);
        let mut text = std::fs::read_to_string(store_file(&dir)).unwrap();
        text.push_str("{this line is damaged\n{\"kind\":\"from_the_future\"}\n");
        std::fs::write(store_file(&dir), text).unwrap();

        let recs = load(&dir);
        let snaps = snapshots(&recs);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].folders.len(), 2);

        let o = overview(&dir, now);
        assert_eq!(o.roots.len(), 1);
        assert_eq!(o.roots[0].snapshots.len(), 1);
        assert_eq!(o.drives.len(), 1);
        assert!(!o.settings.clean_enabled, "scheduled cleaning is off by default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_bounds_the_list_and_records_the_floor() {
        let many: Vec<FolderSize> = (0..500).map(|i| FolderSize { path: format!("C:\\f{i}"), bytes: (30 + i) << 20 }).collect();
        let (kept, floor) = finalize(many);
        assert_eq!(kept.len(), MAX_FOLDERS);
        assert_eq!(kept[0].bytes, 529 << 20);
        assert!(kept.iter().all(|f| f.bytes > floor), "everything listed is above the floor");
        assert_eq!(floor, (30 + 199) << 20);
        let (kept, floor) = finalize(vec![folder("a", 5), folder("b", 40)]);
        assert_eq!((kept.len(), floor), (1, MIN_FOLDER_BYTES));
    }

    #[test]
    fn levels_walk_counts_two_levels_below() {
        let root = std::env::temp_dir().join(format!("jharu levels {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, n: usize| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![1u8; n]).unwrap();
        };
        write("top.bin", 1000);
        write("a/f.bin", 2000);
        write("a/b/g.bin", 3000);
        write("a/b/c/h.bin", 4000);
        write("a/b/c/d/i.bin", 5000);
        write("e space/j.bin", 7000);
        CANCEL.store(false, Ordering::SeqCst);
        let levels = measure_levels(&root, &mut Seen::default()).unwrap();
        let get = |rel: &str| levels.below.iter().find(|(p, _)| *p == root.join(rel)).map(|(_, b)| *b);
        assert!(levels.total >= 22_000);
        let a = get("a").unwrap();
        assert!(a >= 14_000 && a <= levels.total);
        let ab = get("a/b").unwrap();
        assert!(ab >= 12_000 && ab < a, "a/b holds g, h and i but not f");
        assert!(get("a/b/c").is_none(), "only two levels below the folder");
        assert!(get("e space").unwrap() >= 7000, "names with spaces work");
        assert!(levels.below.len() == 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------ forecast

    #[test]
    fn forecast_without_enough_data_says_so() {
        let now = T0;
        let today = now / DAY;
        let none = forecast("C:\\", &[], now);
        assert_eq!(none.trend, Trend::NotEnoughData);
        assert!(none.days_until_full.is_none());
        assert!(none.summary.contains("Not enough history"));

        // Four readings but all within two days.
        let close = forecast("C:\\", &points(&[(today - 1, 100), (today, 101)]), now);
        assert_eq!(close.trend, Trend::NotEnoughData);

        // Plenty of readings, but a month old: the window is relative to now.
        let stale = forecast("C:\\", &points(&[(today - 60, 100), (today - 59, 101), (today - 58, 102), (today - 57, 103)]), now);
        assert_eq!(stale.trend, Trend::NotEnoughData);
        assert_eq!(stale.readings, 0);
    }

    #[test]
    fn forecast_steady_drive_has_no_fill_date() {
        let now = T0;
        let today = now / DAY;
        let pts: Vec<Point> = (0..14).map(|i| Point { day: today - 13 + i, used: (300u64 << 30) + (i % 2) * (1 << 20), total: 500 << 30 }).collect();
        let f = forecast("C:\\", &pts, now);
        assert_eq!(f.trend, Trend::Steady);
        assert!(f.days_until_full.is_none());
        assert!(f.summary.contains("steady"));
    }

    #[test]
    fn forecast_filling_drive_names_the_date_and_ignores_a_cleanup() {
        let now = T0;
        let today = now / DAY;
        // 2 GB a day from 300 GB, with a 40 GB cleanup on day 8.
        let pts: Vec<Point> = (0..20)
            .map(|i| {
                let gb = 300 + 2 * i - if i >= 8 { 40 } else { 0 };
                Point { day: today - 19 + i, used: gb << 30, total: 500 << 30 }
            })
            .collect();
        let f = forecast("C:\\", &pts, now);
        assert_eq!(f.trend, Trend::Filling);
        let gb_per_day = f.bytes_per_day as f64 / (1u64 << 30) as f64;
        assert!((gb_per_day - 2.0).abs() < 0.2, "median slope ignores the one-off drop, got {gb_per_day}");
        // Used 298 GB of 500 after the last day: 202 GB free at 2 GB a day.
        let days = f.days_until_full.unwrap();
        assert!((90..=110).contains(&days), "got {days}");
        assert!(f.summary.contains("months") && f.summary.contains("C:\\"), "{}", f.summary);
        assert_ne!(f.confidence, "low");
        assert!(f.summary.contains("looks like a cleanup"), "{}", f.summary);
    }

    #[test]
    fn forecast_reads_three_weeks() {
        let now = T0;
        let today = now / DAY;
        // 10 GB a day with 210 GB free: 21 days.
        let pts: Vec<Point> = (0..10).map(|i| Point { day: today - 9 + i, used: (200 + 10 * i) << 30, total: 500 << 30 }).collect();
        let f = forecast("D:\\", &pts, now);
        assert_eq!(f.days_until_full, Some(21));
        assert!(f.summary.contains("about 3 weeks"), "{}", f.summary);
        assert_eq!(f.confidence, "fair");
    }

    #[test]
    fn forecast_short_history_is_marked_rough() {
        let now = T0;
        let today = now / DAY;
        let pts = points(&[(today - 4, 100), (today - 3, 105), (today - 2, 110), (today, 120)]);
        let f = forecast("C:\\", &pts, now);
        assert_eq!(f.trend, Trend::Filling);
        assert_eq!(f.confidence, "low");
        assert!(f.summary.contains("rough guess"));
    }

    #[test]
    fn forecast_free_space_growing_is_not_filling() {
        let now = T0;
        let today = now / DAY;
        let pts: Vec<Point> = (0..10).map(|i| Point { day: today - 9 + i, used: (400 - 5 * i) << 30, total: 500 << 30 }).collect();
        let f = forecast("C:\\", &pts, now);
        assert_eq!(f.trend, Trend::Freeing);
        assert!(f.days_until_full.is_none());
        assert!(f.summary.contains("not filling up"));
    }

    #[test]
    fn forecast_very_slow_fill_is_not_a_date() {
        let now = T0;
        let today = now / DAY;
        // 100 MB a day with 400 GB free: 11 years.
        let pts: Vec<Point> = (0..10).map(|i| Point { day: today - 9 + i, used: (100u64 << 30) + i * (100 << 20), total: 500 << 30 }).collect();
        let f = forecast("C:\\", &pts, now);
        assert_eq!(f.trend, Trend::Filling);
        assert!(f.days_until_full.is_none());
        assert!(f.summary.contains("more than 3 years"));
    }

    #[test]
    fn daily_points_take_the_last_reading_of_each_day() {
        let a = Snapshot { drives: vec![drive("C:\\", 10, 100)], ..snap(T0, None, 10, vec![]) };
        let b = Snapshot { drives: vec![drive("C:\\", 12, 100)], ..snap(T0 + 3600, None, 12, vec![]) };
        let c = Snapshot { drives: vec![drive("C:\\", 20, 100)], ..snap(T0 + 5 * DAY, None, 20, vec![]) };
        let pts = daily_points(&[&b, &a, &c], "c:\\");
        assert_eq!(pts.len(), if cfg!(any(target_os = "windows", target_os = "macos")) { 2 } else { 0 });
        if !pts.is_empty() {
            assert_eq!(pts[0].used, 12 << 30);
        }
    }

    #[test]
    fn wording_for_durations() {
        assert_eq!(when(0), "less than a day");
        assert_eq!(when(5), "about 5 days");
        assert_eq!(when(21), "about 3 weeks");
        assert_eq!(when(100), "about 3 months");
        assert_eq!(when(900), "about 2.5 years");
    }

    /// Reads the real drives and prints what the forecast and a snapshot of
    /// the drives look like. Read-only, writes nothing:
    /// `cargo test --lib smoke_space_history -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_space_history() {
        for d in read_drives() {
            println!("{:<10} {} used of {}", d.mount, fmt_bytes(d.used), fmt_bytes(d.total));
        }
    }
}
