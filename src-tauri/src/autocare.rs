//! Looking after the disk while Jharu is open: low-space alerts, the
//! background space check, and, only if the user turns it on, scheduled
//! cleaning of caches that are safe to clear.
//!
//! A background thread wakes every few minutes and, when the check interval
//! (default 6 hours) has passed, does three things:
//!
//! 1. Stores a drive reading in the space history (cheap, feeds the forecast).
//! 2. Sends a system notification for each drive below the free-space limit,
//!    at most once a day per drive.
//! 3. If scheduled cleaning is on and due, runs it.
//!
//! Scheduled cleaning is deliberately narrow. It considers only Dev clean
//! findings that are rated Safe, whose rule says the tool rebuilds the cache
//! by itself (`Restore::Automatic`), that nothing has touched for a number of
//! days (default 7), and that are big enough to matter. Everything else is
//! left alone, however large. Right before each removal it asks again with
//! fresh facts: still Safe, no program running from the folder, nothing
//! modified since. What it removes goes to the Recycle Bin / Trash, and the
//! run is written to the space history. It never starts while a scan or clean
//! is active, and stops if one starts. It is off by default.

use crate::rules::{all_rules, Restore, Rule, Safety};
use crate::safety::{assess, Facts};
use crate::scanner::Finding;
use crate::spacehistory::{self, path_key, CleanRun, CleanedItem, DriveReading, Forecast, Trend, CANCEL};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const DAY: u64 = 86_400;
/// A cache smaller than this is not worth a scheduled run's attention.
const MIN_CLEAN_BYTES: u64 = 100 << 20;
/// Drives smaller than this (recovery partitions, boot volumes) are full by design.
const MIN_ALERT_DRIVE: u64 = 20 << 30;
/// A drive is mentioned at most this often.
const ALERT_COOLDOWN: u64 = DAY;
/// How often the background thread looks at the clock.
const TICK: Duration = Duration::from_secs(300);
/// Let the app finish starting before the first check.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(45);

#[cfg(target_os = "windows")]
const BIN: &str = "Recycle Bin";
#[cfg(not(target_os = "windows"))]
const BIN: &str = "Trash";

// ---------------------------------------------------------------- activity

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static CLEANING: AtomicBool = AtomicBool::new(false);
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Held by a scan or a clean for as long as it runs. Scheduled cleaning looks
/// at this and stays out of the way.
pub struct Activity;

pub fn activity() -> Activity {
    ACTIVE.fetch_add(1, Ordering::SeqCst);
    Activity
}

impl Drop for Activity {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// True while any scan or clean is running.
pub fn busy() -> bool {
    ACTIVE.load(Ordering::SeqCst) > 0
}

struct CleaningFlag;

impl Drop for CleaningFlag {
    fn drop(&mut self) {
        CLEANING.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- settings

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// How often the background check runs.
    pub check_hours: u32,
    pub alerts_enabled: bool,
    /// Alert when a drive has less free space than this.
    pub low_space_gb: u32,
    /// Scheduled cleaning. Off until the user turns it on.
    pub clean_enabled: bool,
    pub clean_every_days: u32,
    /// A cache must be untouched for this long to be cleaned automatically.
    pub clean_idle_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { check_hours: 6, alerts_enabled: true, low_space_gb: 10, clean_enabled: false, clean_every_days: 7, clean_idle_days: 7 }
    }
}

impl Settings {
    /// Keeps values in a sensible range, whatever a hand-edited file says.
    pub fn sanitized(mut self) -> Settings {
        self.check_hours = self.check_hours.clamp(1, 168);
        self.low_space_gb = self.low_space_gb.clamp(1, 2000);
        self.clean_every_days = self.clean_every_days.clamp(1, 90);
        self.clean_idle_days = self.clean_idle_days.clamp(1, 365);
        self
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct State {
    pub last_check: u64,
    pub last_clean: u64,
    /// When each drive was last mentioned in a notification.
    pub last_alert: HashMap<String, u64>,
}

fn settings_file(dir: &Path) -> PathBuf {
    dir.join("space-settings.json")
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("space-state.json")
}

pub fn load_settings(dir: &Path) -> Settings {
    std::fs::read_to_string(settings_file(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
        .unwrap_or_default()
        .sanitized()
}

pub fn save_settings(dir: &Path, s: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(settings_file(dir), serde_json::to_string_pretty(s).unwrap_or_default())
}

pub fn load_state(dir: &Path) -> State {
    std::fs::read_to_string(state_file(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn update_state(dir: &Path, f: impl FnOnce(&mut State)) {
    let _lock = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = load_state(dir);
    f(&mut st);
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(state_file(dir), serde_json::to_string(&st).unwrap_or_default());
}

// ---------------------------------------------------------------- alerts

/// Drives under the free-space limit that have not been mentioned today.
pub fn low_drives<'a>(readings: &'a [DriveReading], s: &Settings, st: &State, now: u64) -> Vec<&'a DriveReading> {
    if !s.alerts_enabled {
        return vec![];
    }
    let limit = s.low_space_gb as u64 * (1 << 30);
    readings
        .iter()
        .filter(|d| !d.removable && d.total >= MIN_ALERT_DRIVE && d.free < limit)
        .filter(|d| st.last_alert.get(&d.mount).is_none_or(|t| t + ALERT_COOLDOWN <= now))
        .collect()
}

/// The title and body of the notification for a low drive.
pub fn alert_text(d: &DriveReading, forecast: Option<&Forecast>) -> (String, String) {
    let pct = if d.total > 0 { d.free as f64 * 100.0 / d.total as f64 } else { 0.0 };
    let mut body = format!(
        "{} has only {} free ({:.0}% of {}). Open Jharu to see what is taking the space.",
        d.mount,
        spacehistory::fmt_bytes(d.free),
        pct,
        spacehistory::fmt_bytes(d.total)
    );
    if let Some(Forecast { trend: Trend::Filling, days_until_full: Some(days), .. }) = forecast {
        body.push_str(&format!(" At the recent rate it is full in {}.", spacehistory::when(*days)));
    }
    (format!("{} is running low on space", d.mount), body)
}

fn notify(app: &tauri::AppHandle, title: &str, body: &str) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    app.notification().builder().title(title).body(body).show().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- scheduling

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Scheduled cleaning is switched off.
    Off,
    NotDue,
    /// Due, but a scan or clean is running; asked again at the next check.
    Busy,
    Run,
}

pub fn decide_clean(s: &Settings, st: &State, now: u64, busy: bool) -> Decision {
    if !s.clean_enabled {
        Decision::Off
    } else if st.last_clean + s.clean_every_days as u64 * DAY > now {
        Decision::NotDue
    } else if busy {
        Decision::Busy
    } else {
        Decision::Run
    }
}

// ---------------------------------------------------------------- what may be cleaned

/// A Dev clean finding, with what is needed to decide on it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub rule_id: String,
    pub name: String,
    pub path: PathBuf,
    pub safety: Safety,
    pub restore: Restore,
    pub reclaimable: u64,
    pub stale_days: Option<u64>,
    pub complete: bool,
}

pub fn candidates(findings: &[Finding]) -> Vec<Candidate> {
    let rules = all_rules();
    findings
        .iter()
        .filter_map(|f| {
            let rule = rules.iter().find(|r| r.id == f.rule_id)?;
            Some(Candidate {
                rule_id: f.rule_id.clone(),
                name: f.name.clone(),
                path: PathBuf::from(&f.path),
                safety: f.safety,
                restore: rule.restore,
                reclaimable: f.reclaimable_bytes,
                stale_days: f.stale_days,
                complete: f.read_error.is_none(),
            })
        })
        .collect()
}

/// Why a finding is not cleaned automatically. `held_back` is true when it
/// would qualify but for something that may pass (recent use, small size),
/// which is worth telling the user; the rest is simply not this feature's job.
#[derive(Debug, PartialEq, Eq)]
pub struct Skip {
    pub held_back: bool,
    pub why: String,
}

fn rated(s: Safety) -> &'static str {
    match s {
        Safety::Safe => "Safe",
        Safety::Caution => "Caution",
        Safety::Risky => "Risky",
    }
}

/// Whether scheduled cleaning may take this finding. Only Safe findings that
/// the tool rebuilds by itself pass the first two checks; the rest are about
/// this moment.
pub fn gate(c: &Candidate, s: &Settings) -> Result<(), Skip> {
    let skip = |held_back, why: String| Err(Skip { held_back, why });
    if c.safety != Safety::Safe {
        return skip(false, format!("rated {}, not Safe, so it is left for you to decide", rated(c.safety)));
    }
    if c.restore != Restore::Automatic {
        return skip(false, "the tool does not rebuild it by itself, so it is left for you to decide".into());
    }
    if !c.complete {
        return skip(true, "it could not be read completely, so its size and age are unknown".into());
    }
    match c.stale_days {
        None => return skip(true, "its last use is unknown".into()),
        Some(d) if d < s.clean_idle_days as u64 => {
            let used = match d {
                0 => "was used today".to_string(),
                1 => "was used 1 day ago".to_string(),
                _ => format!("was used {d} days ago"),
            };
            return skip(true, format!("it {used}; Jharu waits until it has been idle for {} days", s.clean_idle_days));
        }
        Some(_) => {}
    }
    if c.reclaimable < MIN_CLEAN_BYTES {
        return skip(true, format!("only {} would be freed, too little to bother", spacehistory::fmt_bytes(c.reclaimable)));
    }
    Ok(())
}

/// True when any file under `path` was modified after `cutoff`, or when that
/// cannot be told because something could not be read: a folder that cannot
/// be checked is treated as in use, never as idle. Stops at the first recent
/// file, so a cache in use answers quickly.
fn changed_since(path: &Path, cutoff: SystemTime) -> bool {
    for result in crate::walk::walk(path).skip_hidden(false) {
        let Ok(entry) = result else { return true };
        if entry.read_children_error.is_some() {
            return true;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        match entry.metadata().ok().and_then(|m| m.modified().ok()) {
            Some(m) if m > cutoff => return true,
            Some(_) => {}
            None => return true,
        }
    }
    false
}

fn first_problem(reasons: &[crate::safety::Reason]) -> String {
    reasons
        .iter()
        .find(|r| r.level != crate::safety::Level::Ok)
        .map(|r| r.text.clone())
        .unwrap_or_else(|| "it is no longer rated Safe".into())
}

/// The checks made right before removal, with fresh facts: still where its
/// rule says it is, still rated Safe, no program running from inside it, and
/// nothing modified within the idle period. Ok holds the size it would free.
pub fn recheck(c: &Candidate, rule: &Rule, facts: &Facts, idle_days: u32, now: SystemTime) -> Result<u64, String> {
    if !rule.paths().iter().any(|p| path_key(&p.to_string_lossy()) == path_key(&c.path.to_string_lossy())) {
        return Err("it is not one of the locations Jharu knows for this cache".into());
    }
    let Ok(meta) = c.path.symlink_metadata() else { return Err("it is already gone".into()) };
    if meta.file_type().is_symlink() {
        return Err("it is a link to somewhere else, not a folder of its own".into());
    }
    let (safety, reasons) = assess(rule, &c.path, facts);
    if safety != Safety::Safe {
        return Err(format!("it is now rated {}: {}", rated(safety), first_problem(&reasons)));
    }
    let running = facts.running_from(&c.path);
    if !running.is_empty() {
        return Err(format!("{} is running from inside it", running.join(", ")));
    }
    let cutoff = now.checked_sub(Duration::from_secs(idle_days as u64 * DAY)).unwrap_or(SystemTime::UNIX_EPOCH);
    if changed_since(&c.path, cutoff) {
        return Err(format!("something in it changed within the last {idle_days} days"));
    }
    let size = crate::usage::reclaimable(&c.path);
    if size < MIN_CLEAN_BYTES {
        return Err(format!("only {} would be freed now", spacehistory::fmt_bytes(size)));
    }
    Ok(size)
}

fn remove(c: &Candidate, s: &Settings) -> Result<u64, String> {
    let rule = all_rules().into_iter().find(|r| r.id == c.rule_id).ok_or("its rule no longer exists")?;
    let size = recheck(c, &rule, &Facts::gather(), s.clean_idle_days, SystemTime::now())?;
    match crate::trashlog::to_bin(&[&c.path]) {
        Ok(()) => Ok(size),
        // Windows reports a file held open by another program only as
        // "Some operations were aborted", and then moves nothing.
        Err(e) if c.path.exists() && e.to_string().contains("aborted") => {
            Err("something inside is open in another program, so nothing was moved".into())
        }
        Err(e) => Err(format!("could not move it to the {BIN}: {e}")),
    }
}

/// Scans, picks what qualifies, and (unless `dry_run`) moves it to the
/// Recycle Bin / Trash, checking each folder again first. Stored in the
/// space history unless it is a dry run.
///
/// `only` limits the run to those paths (what the user was shown and agreed
/// to), so a cache that qualified in the meantime is not taken unasked.
pub fn run_clean(
    app: Option<&tauri::AppHandle>,
    dir: &Path,
    s: &Settings,
    trigger: &str,
    dry_run: bool,
    only: Option<&[String]>,
) -> Result<CleanRun, String> {
    if CLEANING.swap(true, Ordering::SeqCst) {
        return Err("An automatic clean is already running.".into());
    }
    let _flag = CleaningFlag;
    if busy() {
        return Err("Jharu is scanning or cleaning something else right now. Try again when that finishes.".into());
    }
    CANCEL.store(false, Ordering::SeqCst);
    let progress = |done: usize, total: usize, current: &str, stage: &str| {
        if let Some(app) = app {
            spacehistory::emit_progress(app, stage, done, total, current);
        }
    };

    progress(0, 0, "", "Looking at your caches");
    let scan = crate::scanner::scan();
    let all = candidates(&scan.findings);
    let mut run = CleanRun { ts: spacehistory::now(), trigger: trigger.into(), freed_bytes: 0, removed: vec![], skipped: vec![], errors: vec![] };

    let wanted: Option<HashSet<String>> = only.map(|paths| paths.iter().map(|p| path_key(p)).collect());
    let mut todo: Vec<Candidate> = vec![];
    for c in all {
        if wanted.as_ref().is_some_and(|w| !w.contains(&path_key(&c.path.to_string_lossy()))) {
            continue;
        }
        match gate(&c, s) {
            Ok(()) => todo.push(c),
            // A scheduled run's log keeps to what was nearly cleaned; a
            // preview explains every location.
            Err(skip) if skip.held_back || dry_run => run.skipped.push(format!("{}: {}", c.name, skip.why)),
            Err(_) => {}
        }
    }

    let total = todo.len();
    for (i, c) in todo.iter().enumerate() {
        progress(i, total, &c.name, "Cleaning safe caches");
        if dry_run {
            run.freed_bytes += c.reclaimable;
            run.removed.push(CleanedItem { name: c.name.clone(), path: c.path.to_string_lossy().into_owned(), bytes: c.reclaimable });
            continue;
        }
        if CANCEL.load(Ordering::Relaxed) {
            run.skipped.push("Stopped before finishing: cancelled.".into());
            break;
        }
        if busy() {
            run.skipped.push("Stopped before finishing: a scan or clean started.".into());
            break;
        }
        match remove(c, s) {
            Ok(bytes) => {
                run.freed_bytes += bytes;
                run.removed.push(CleanedItem { name: c.name.clone(), path: c.path.to_string_lossy().into_owned(), bytes });
            }
            Err(why) => run.skipped.push(format!("{}: {why}", c.name)),
        }
    }
    progress(total, total, "", "Cleaning safe caches");

    if !dry_run {
        let now = spacehistory::now();
        spacehistory::add_run(dir, run.clone(), now);
        update_state(dir, |st| st.last_clean = now);
        // The free-space jump should show on the chart at once.
        let snap = spacehistory::Snapshot { ts: now, source: "background".into(), drives: spacehistory::read_drives(), root: None, folders: vec![], floor: 0, detail: vec![] };
        spacehistory::add_snapshot(dir, snap, now);
    }
    Ok(run)
}

// ---------------------------------------------------------------- background thread

fn check(app: &tauri::AppHandle, dir: &Path, now: u64) {
    let s = load_settings(dir);
    let readings = spacehistory::read_drives();
    spacehistory::record_drives(dir, now);

    let st = load_state(dir);
    let low = low_drives(&readings, &s, &st, now);
    if !low.is_empty() {
        let forecasts = spacehistory::forecasts(&spacehistory::load(dir), now);
        for d in low {
            let forecast = forecasts.iter().find(|f| path_key(&f.mount) == path_key(&d.mount));
            let (title, body) = alert_text(d, forecast);
            if notify(app, &title, &body).is_ok() {
                let mount = d.mount.clone();
                update_state(dir, |st| {
                    st.last_alert.insert(mount, now);
                });
            }
        }
    }

    if decide_clean(&s, &load_state(dir), now, busy()) == Decision::Run {
        if let Ok(run) = run_clean(Some(app), dir, &s, "scheduled", false, None) {
            if run.freed_bytes > 0 {
                let _ = notify(
                    app,
                    "Jharu cleaned your caches",
                    &format!("Moved {} of caches that rebuild themselves to the {BIN}. Open Space history to see what.", spacehistory::fmt_bytes(run.freed_bytes)),
                );
            }
        }
    }
    update_state(dir, |st| st.last_check = now);
}

/// Starts the background thread. Called once, from the app's setup.
pub fn start(app: tauri::AppHandle) {
    let _ = std::thread::Builder::new().name("jharu-autocare".into()).spawn(move || {
        std::thread::sleep(FIRST_CHECK_DELAY);
        loop {
            if let Some(dir) = spacehistory::data_dir(&app) {
                let now = spacehistory::now();
                let s = load_settings(&dir);
                if load_state(&dir).last_check + s.check_hours as u64 * 3600 <= now {
                    check(&app, &dir, now);
                }
            }
            std::thread::sleep(TICK);
        }
    });
}

// ---------------------------------------------------------------- commands

fn dir_of(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    spacehistory::data_dir(app).ok_or_else(|| "Jharu could not open its data folder.".to_string())
}

/// Saves the settings and returns them as stored. Turning scheduled cleaning
/// on starts its clock now: the first run comes after one full period.
#[tauri::command]
pub async fn autocare_set(app: tauri::AppHandle, settings: Settings) -> Result<Settings, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = dir_of(&app)?;
        let before = load_settings(&dir);
        let settings = settings.sanitized();
        save_settings(&dir, &settings).map_err(|e| format!("Could not save the settings: {e}"))?;
        if settings.clean_enabled && !before.clean_enabled {
            let now = spacehistory::now();
            update_state(&dir, |st| st.last_clean = now);
        }
        Ok(settings)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// What an automatic clean would do right now, without doing it. `settings`
/// are the ones on screen, which a preview uses without saving them.
#[tauri::command]
pub async fn autocare_preview(app: tauri::AppHandle, settings: Option<Settings>) -> Result<CleanRun, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = dir_of(&app)?;
        let s = settings.map(Settings::sanitized).unwrap_or_else(|| load_settings(&dir));
        run_clean(Some(&app), &dir, &s, "preview", true, None)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Runs the automatic clean now, once, whatever the schedule says, on the
/// caches in `paths` only: the ones the user was shown and confirmed. Each
/// still has to pass every check again.
#[tauri::command]
pub async fn autocare_run_now(app: tauri::AppHandle, paths: Vec<String>, settings: Option<Settings>) -> Result<CleanRun, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = dir_of(&app)?;
        let s = settings.map(Settings::sanitized).unwrap_or_else(|| load_settings(&dir));
        run_clean(Some(&app), &dir, &s, "manual", false, Some(&paths))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sends a sample notification so the user can see what an alert looks like.
#[tauri::command]
pub fn autocare_test_alert(app: tauri::AppHandle) -> Result<(), String> {
    let d = DriveReading { mount: "C:".into(), total: 500 << 30, used: 495 << 30, free: 5 << 30, removable: false };
    let (_, body) = alert_text(&d, None);
    notify(&app, "Jharu test alert", &format!("This is what a low-space alert looks like. Example: {body}"))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Category;
    use crate::safety::Level;

    const NOW: u64 = 1_760_000_000;

    fn reading(mount: &str, free_gb: u64, total_gb: u64) -> DriveReading {
        DriveReading { mount: mount.into(), total: total_gb << 30, used: (total_gb - free_gb) << 30, free: free_gb << 30, removable: false }
    }

    fn candidate(safety: Safety, restore: Restore) -> Candidate {
        Candidate {
            rule_id: "npm".into(),
            name: "npm cache".into(),
            path: PathBuf::from("C:\\Users\\me\\.npm\\_cacache"),
            safety,
            restore,
            reclaimable: 2 << 30,
            stale_days: Some(30),
            complete: true,
        }
    }

    #[test]
    fn defaults_keep_scheduled_cleaning_off() {
        let s = Settings::default();
        assert!(!s.clean_enabled);
        // A file with only some keys, or junk values, still loads sanely.
        let parsed: Settings = serde_json::from_str(r#"{"clean_enabled":true,"check_hours":0,"clean_idle_days":9999}"#).unwrap();
        let parsed = parsed.sanitized();
        assert!(parsed.clean_enabled);
        assert_eq!(parsed.check_hours, 1);
        assert_eq!(parsed.clean_idle_days, 365);
        assert_eq!(parsed.low_space_gb, 10);
    }

    #[test]
    fn settings_round_trip_through_the_data_folder() {
        let dir = std::env::temp_dir().join(format!("jharu autocare {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_settings(&dir), Settings::default());
        let s = Settings { clean_enabled: true, clean_every_days: 3, ..Settings::default() };
        save_settings(&dir, &s).unwrap();
        assert_eq!(load_settings(&dir), s);
        update_state(&dir, |st| st.last_clean = 42);
        assert_eq!(load_state(&dir).last_clean, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scheduler_gating() {
        let on = Settings { clean_enabled: true, clean_every_days: 7, ..Settings::default() };
        let st = |last_clean| State { last_clean, ..State::default() };
        assert_eq!(decide_clean(&Settings::default(), &st(0), NOW, false), Decision::Off, "off by default, however overdue");
        assert_eq!(decide_clean(&on, &st(NOW - 6 * DAY), NOW, false), Decision::NotDue);
        assert_eq!(decide_clean(&on, &st(NOW - 7 * DAY), NOW, false), Decision::Run);
        assert_eq!(decide_clean(&on, &st(0), NOW, true), Decision::Busy, "never while a scan or clean is active");
        assert_eq!(decide_clean(&on, &st(NOW - DAY), NOW, true), Decision::NotDue);
    }

    #[test]
    fn activity_guard_counts_scans() {
        let before = busy();
        {
            let _a = activity();
            let _b = activity();
            assert!(busy());
        }
        assert_eq!(busy(), before);
    }

    #[test]
    fn only_safe_automatic_idle_big_caches_are_picked() {
        let s = Settings::default();
        assert!(gate(&candidate(Safety::Safe, Restore::Automatic), &s).is_ok());

        // Caution and Risky never, however large and old.
        for rating in [Safety::Caution, Safety::Risky] {
            let skip = gate(&candidate(rating, Restore::Automatic), &s).unwrap_err();
            assert!(!skip.held_back, "not this feature's job, so not worth a line in the log");
            assert!(skip.why.contains("not Safe"));
        }
        // A Safe rating alone is not enough: the tool must rebuild it by itself.
        for restore in [Restore::Download, Restore::Manual("run `x`"), Restore::Never("keys")] {
            assert!(gate(&candidate(Safety::Safe, restore), &s).is_err());
        }
        // Recently used, unknown age, unreadable, tiny: held back, with the reason.
        let mut c = candidate(Safety::Safe, Restore::Automatic);
        c.stale_days = Some(2);
        let skip = gate(&c, &s).unwrap_err();
        assert!(skip.held_back && skip.why.contains("2 days ago"), "{}", skip.why);
        c.stale_days = None;
        assert!(gate(&c, &s).unwrap_err().held_back);
        c.stale_days = Some(30);
        c.complete = false;
        assert!(gate(&c, &s).unwrap_err().held_back);
        c.complete = true;
        c.reclaimable = 10 << 20;
        assert!(gate(&c, &s).unwrap_err().why.contains("too little"));
        // The idle period is the user's setting.
        c.reclaimable = 2 << 30;
        c.stale_days = Some(3);
        assert!(gate(&c, &Settings { clean_idle_days: 3, ..s.clone() }).is_ok());
        assert!(gate(&c, &Settings { clean_idle_days: 4, ..s }).is_err());
    }

    #[test]
    fn candidates_come_from_findings_with_known_rules_only() {
        let finding = |rule_id: &str, safety: Safety, read_error: Option<&str>| Finding {
            rule_id: rule_id.into(),
            name: rule_id.into(),
            description: String::new(),
            category: Category::DevCache,
            safety,
            reasons: vec![],
            path: "C:\\x".into(),
            size_bytes: 5 << 30,
            reclaimable_bytes: 4 << 30,
            reclaimable_exact: true,
            file_count: 10,
            stale_days: Some(20),
            children: vec![],
            read_error: read_error.map(String::from),
        };
        let found = vec![
            finding("npm", Safety::Safe, None),
            finding("docker", Safety::Risky, None),
            finding("playwright", Safety::Caution, None),
            finding("no-such-rule", Safety::Safe, None),
            finding("pip", Safety::Safe, Some("denied")),
        ];
        let cands = candidates(&found);
        assert_eq!(cands.len(), 4, "a finding without a rule is dropped");
        let s = Settings::default();
        let picked: Vec<&str> = cands.iter().filter(|c| gate(c, &s).is_ok()).map(|c| c.rule_id.as_str()).collect();
        assert_eq!(picked, vec!["npm"], "Risky, Caution (manual restore) and unreadable are all left alone");
    }

    fn fixture_rule(tools: &'static [&'static str]) -> Rule {
        // "npm" gives the rule a real location to check the path against.
        Rule { id: "npm", name: "npm cache", description: "", category: Category::DevCache, restore: Restore::Automatic, tools, lock: None }
    }

    #[test]
    fn recheck_refuses_what_changed_since_the_scan() {
        let dir = std::env::temp_dir().join(format!("jharu recheck {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob.bin"), vec![1u8; 4096]).unwrap();

        let rule = fixture_rule(&["npm"]);
        let idle = Facts::default();
        let mut c = candidate(Safety::Safe, Restore::Automatic);

        // A folder that is not the rule's own location is never touched.
        c.path = dir.clone();
        let err = recheck(&c, &rule, &idle, 7, SystemTime::now()).unwrap_err();
        assert!(err.contains("not one of the locations"), "{err}");

        // At a real location of the rule (any path the rule lists, even a
        // missing one), the remaining checks apply. Use the first listed path
        // only for the "already gone" answer: nothing is created or removed.
        if let Some(p) = rule.paths().into_iter().find(|p| !p.exists()) {
            c.path = p;
            assert_eq!(recheck(&c, &rule, &idle, 7, SystemTime::now()).unwrap_err(), "it is already gone");
        }

        // The file checks, on the fixture directly.
        let now = SystemTime::now();
        assert!(changed_since(&dir, now - Duration::from_secs(60)), "a file written a moment ago is recent");
        assert!(!changed_since(&dir, now + Duration::from_secs(60)), "nothing is newer than the future");

        // A program running makes the rating Caution.
        let busy_facts = Facts { running: ["npm".to_string()].into_iter().collect(), exes: vec![], uv_symlink_mode: false };
        let (safety, reasons) = assess(&rule, &dir, &busy_facts);
        assert_eq!(safety, Safety::Caution);
        assert!(first_problem(&reasons).contains("npm is running"));
        assert_eq!(first_problem(&[crate::safety::Reason { level: Level::Ok, text: "fine".into() }]), "it is no longer rated Safe");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What scheduled cleaning would pick on this machine, as a dry run: it
    /// scans and rates, and removes nothing and writes nothing.
    /// `cargo test --lib smoke_autocare_preview -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_autocare_preview() {
        let dir = std::env::temp_dir().join(format!("jharu autocare smoke {}", std::process::id()));
        let run = run_clean(None, &dir, &Settings::default(), "preview", true, None).unwrap();
        println!("would clean {} ({} caches):", spacehistory::fmt_bytes(run.freed_bytes), run.removed.len());
        for i in &run.removed {
            println!("  {:>10}  {}  {}", spacehistory::fmt_bytes(i.bytes), i.name, i.path);
        }
        println!("left alone:");
        for s in &run.skipped {
            println!("  {s}");
        }
        assert!(!dir.exists(), "a dry run writes nothing");
    }

    #[test]
    fn low_space_alerts_respect_threshold_cooldown_and_drive_kind() {
        let s = Settings { low_space_gb: 10, ..Settings::default() };
        let drives = vec![
            reading("C:\\", 8, 476),
            reading("D:\\", 200, 931),
            reading("E:\\", 2, 15),
            DriveReading { removable: true, ..reading("F:\\", 1, 64) },
        ];
        let none = State::default();
        let low = low_drives(&drives, &s, &none, NOW);
        assert_eq!(low.iter().map(|d| d.mount.as_str()).collect::<Vec<_>>(), vec!["C:\\"], "small and removable drives are skipped");

        // Mentioned an hour ago: quiet. A day ago: again.
        let mut st = State::default();
        st.last_alert.insert("C:\\".into(), NOW - 3600);
        assert!(low_drives(&drives, &s, &st, NOW).is_empty());
        st.last_alert.insert("C:\\".into(), NOW - DAY);
        assert_eq!(low_drives(&drives, &s, &st, NOW).len(), 1);

        assert!(low_drives(&drives, &Settings { alerts_enabled: false, ..s.clone() }, &none, NOW).is_empty());
        assert!(low_drives(&drives, &Settings { low_space_gb: 5, ..s }, &none, NOW).is_empty());
    }

    #[test]
    fn alert_text_names_the_drive_and_the_forecast() {
        let d = reading("C:\\", 8, 476);
        let (title, body) = alert_text(&d, None);
        assert!(title.contains("C:\\") && body.contains("C:\\"));
        assert!(!body.contains("full in"));
        let f = Forecast {
            mount: "C:\\".into(),
            trend: Trend::Filling,
            bytes_per_day: 1 << 30,
            days_until_full: Some(8),
            readings: 10,
            span_days: 14,
            confidence: "good",
            summary: String::new(),
        };
        let (_, body) = alert_text(&d, Some(&f));
        assert!(body.contains("full in about 8 days"), "{body}");
    }
}
