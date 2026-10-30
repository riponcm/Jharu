//! System space hogs: big things the operating system keeps that no folder
//! scan can offer to clean, because they are not ordinary files.
//!
//! Windows: the hibernation file, System Restore points, the component store
//! (WinSxS), the page file (information only) and Docker. macOS: Time Machine
//! local snapshots, the sleep image (information only) and Docker.
//!
//! The rules that shape this module:
//!
//! - Nothing here goes to the Recycle Bin, because none of it is a file the
//!   bin could hold. Every action says so, says whether it can be undone, and
//!   shows the exact command Jharu will run. A command is never assembled
//!   from text the user or a file supplied: the elevated helper takes one of
//!   a fixed list of action ids and builds its commands itself, from absolute
//!   paths under `System32`.
//! - Looking is separate from changing. The scan only reads. What needs
//!   administrator rights to read (restore points, the component store) waits
//!   for a click, and a mutating command only ever runs from an explicit
//!   action, after the state is checked again.
//! - Windows prints its tools' results in the system language. Where a locale
//!   independent source exists (the registry, file sizes, WMI through
//!   PowerShell, Docker's JSON format) that is what gets parsed. DISM has no
//!   such output; its report is parsed by its English labels and, when they
//!   are not found, shown as it came.

// Parts of this module (restore points, DISM, hibernation) exist only for
// Windows, and the Time Machine parts only for macOS.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::rules::Safety;
use crate::safety::{Level, Reason};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Emitter;

/// How long read-only questions to other programs may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(20);
/// How much command output is kept for the user to read.
const OUTPUT_TAIL: usize = 1500;
/// Names quoted in an explanation (Docker volumes).
const NAME_LIMIT: usize = 6;
/// A fully sized hibernation file is this share of RAM by default; Windows'
/// reduced type is 40%.
const HIBER_FULL_PERCENT: u64 = 75;
const HIBER_REDUCED_PERCENT: u64 = 40;

/// The command-line flag that starts Jharu as the elevated system helper.
pub const HELPER_FLAG: &str = "--jharu-elevated-system";

// ---------------------------------------------------------------- model

#[derive(Serialize, Clone)]
pub struct Fact {
    pub label: String,
    pub text: String,
}

fn fact(label: &str, text: impl Into<String>) -> Fact {
    Fact { label: label.into(), text: text.into() }
}

/// A read-only look that is too slow or too privileged for the scan itself.
#[derive(Serialize, Clone)]
pub struct Inspect {
    pub label: String,
    pub command: String,
    pub admin: bool,
    pub slow: bool,
    /// Already looked at in this session; the button then reads "Check again".
    pub done: bool,
}

#[derive(Serialize, Clone)]
pub struct SpaceAction {
    pub id: String,
    pub label: String,
    /// Every command Jharu will run for this action, exactly as run.
    pub commands: Vec<String>,
    /// What turning it off or cleaning it costs you.
    pub cost: String,
    pub safety: Safety,
    pub reasons: Vec<Reason>,
    pub admin: bool,
    /// Cannot be undone: it does not go to the Recycle Bin and there is no
    /// command that brings it back.
    pub permanent: bool,
    /// How to reverse it, when it can be reversed.
    pub undo: Option<String>,
    pub slow: bool,
    /// False once started as an administrator command: stopping DISM or
    /// vssadmin half way is not something Jharu will do to the system.
    pub cancellable: bool,
    pub est_bytes: Option<u64>,
    pub est_note: Option<String>,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct SpaceItem {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub size_bytes: Option<u64>,
    /// Why there is no size, or what the size means.
    pub size_note: Option<String>,
    pub facts: Vec<Fact>,
    /// Something the user should know first (not running, needs a click).
    pub notice: Option<String>,
    /// For information-only items: how to change it yourself.
    pub how_to: Option<String>,
    pub info_only: bool,
    pub inspect: Option<Inspect>,
    pub actions: Vec<SpaceAction>,
}

#[derive(Serialize)]
pub struct SpaceReport {
    pub platform: &'static str,
    pub is_admin: bool,
    pub items: Vec<SpaceItem>,
}

#[derive(Serialize, Default)]
pub struct InspectOutcome {
    pub admin_declined: bool,
    pub error: Option<String>,
    /// What the command printed, shown when it could not be read.
    pub output: String,
}

#[derive(Serialize, Default)]
pub struct RunOutcome {
    pub action_id: String,
    pub ok: bool,
    /// The re-check found it should not run, and why.
    pub skipped: Option<String>,
    pub freed_bytes: Option<u64>,
    pub message: String,
    pub output: String,
    pub admin_declined: bool,
    pub cancelled: bool,
    pub errors: Vec<String>,
}

#[derive(Serialize, Clone)]
struct Progress {
    token: String,
    stage: String,
}

fn ok(text: impl Into<String>) -> Reason {
    Reason { level: Level::Ok, text: text.into() }
}
fn warn(text: impl Into<String>) -> Reason {
    Reason { level: Level::Warn, text: text.into() }
}
fn bad(text: impl Into<String>) -> Reason {
    Reason { level: Level::Bad, text: text.into() }
}

/// Sizes in the units the file manager uses, as `src/format.ts` does.
fn human(n: u64) -> String {
    let unit: f64 = if cfg!(windows) { 1024.0 } else { 1000.0 };
    let v = n as f64;
    if v >= unit.powi(3) {
        format!("{:.1} GB", v / unit.powi(3))
    } else if v >= unit.powi(2) {
        format!("{:.1} MB", v / unit.powi(2))
    } else if v >= unit {
        format!("{:.0} KB", v / unit)
    } else {
        format!("{n} B")
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn days_ago(unix: u64) -> u64 {
    now_secs().saturating_sub(unix) / 86_400
}

fn age_text(unix: u64) -> String {
    match days_ago(unix) {
        0 => "today".into(),
        1 => "1 day ago".into(),
        d => format!("{d} days ago"),
    }
}

fn tail(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= OUTPUT_TAIL {
        return t.to_string();
    }
    let skip = t.chars().count() - OUTPUT_TAIL;
    format!("…{}", t.chars().skip(skip).collect::<String>())
}

// ---------------------------------------------------------------- state kept between clicks

/// What the last administrator look found. Restore points and the component
/// store cannot be read without a permission prompt, so a rescan reuses the
/// answer rather than asking again, and says how old it is.
#[derive(Default)]
struct Looked {
    restore: Option<(RestoreState, u64)>,
    store: Option<(ComponentStore, u64)>,
}

fn looked() -> &'static Mutex<Looked> {
    static L: OnceLock<Mutex<Looked>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Looked::default()))
}

fn cancel_flags() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static C: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_cancel(token: &str) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if let Ok(mut m) = cancel_flags().lock() {
        m.insert(token.to_string(), flag.clone());
    }
    flag
}

fn unregister_cancel(token: &str) {
    if let Ok(mut m) = cancel_flags().lock() {
        m.remove(token);
    }
}

// ---------------------------------------------------------------- running programs

struct CmdOut {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    cancelled: bool,
}

impl CmdOut {
    fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }
    fn combined(&self) -> String {
        let mut s = self.stdout.trim().to_string();
        if !self.stderr.trim().is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(self.stderr.trim());
        }
        s
    }
}

/// `pmset -g` prints "hibernatemode        3"; say what the number means.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn hibernate_mode_text(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    if words.next()? != "hibernatemode" {
        return None;
    }
    let n = words.next()?;
    let meaning = match n {
        "0" => "memory is not written to disk (the default on desktop Macs)",
        "3" => "memory stays powered and is also written to disk (the default on laptops)",
        "25" => "memory is written to disk and powered off",
        _ => "",
    };
    Some(if meaning.is_empty() { format!("hibernatemode {n}") } else { format!("hibernatemode {n}: {meaning}") })
}

fn command(program: &Path) -> Command {
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Runs a program to its end with output captured, killing it when `timeout`
/// passes or `cancel` is set. Output is read on threads so a chatty program
/// cannot fill a pipe and stall.
fn run_cmd(mut cmd: Command, timeout: Option<Duration>, cancel: Option<&AtomicBool>) -> Result<CmdOut, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut out_pipe = child.stdout.take().ok_or("no stdout")?;
    let mut err_pipe = child.stderr.take().ok_or("no stderr")?;
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let (mut code, mut timed_out, mut cancelled) = (None, false, false);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                code = status.code();
                break;
            }
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            cancelled = true;
            break;
        }
        if timeout.is_some_and(|t| start.elapsed() > t) {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    let stdout = String::from_utf8_lossy(&out_thread.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_thread.join().unwrap_or_default()).into_owned();
    Ok(CmdOut { code, stdout, stderr, timed_out, cancelled })
}

// ---------------------------------------------------------------- parsers

/// "9.74 GB", "427.45 MB": the units DISM prints, counted in 1024s.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_binary_size(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let number: f64 = parts.next()?.replace(',', ".").parse().ok()?;
    let mult: f64 = match parts.next()?.to_ascii_uppercase().as_str() {
        "B" | "BYTES" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * mult) as u64)
}

/// "1.234GB", "12.3kB", "0B", "1.2GB (45%)": Docker's decimal units.
fn parse_docker_size(text: &str) -> Option<u64> {
    let token = text.split_whitespace().next()?;
    let split = token.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = token.split_at(split);
    let number: f64 = num.parse().ok()?;
    let mult: f64 = match unit.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        _ => return None,
    };
    Some((number * mult) as u64)
}

#[derive(Debug, Clone, PartialEq, Default)]
struct DockerRow {
    kind: String,
    total: u64,
    active: u64,
    size_bytes: u64,
    reclaimable_bytes: u64,
}

fn json_u64(v: &serde_json::Value) -> u64 {
    match v {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// `docker system df --format "{{json .}}"`: one JSON object per line.
fn parse_docker_df(text: &str) -> Vec<DockerRow> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .filter_map(|v| {
            let kind = v.get("Type")?.as_str()?.to_string();
            let size_of = |key: &str| v.get(key).and_then(|s| s.as_str()).and_then(parse_docker_size).unwrap_or(0);
            Some(DockerRow {
                kind,
                total: v.get("TotalCount").map(json_u64).unwrap_or(0),
                active: v.get("Active").map(json_u64).unwrap_or(0),
                size_bytes: size_of("Size"),
                reclaimable_bytes: size_of("Reclaimable"),
            })
        })
        .collect()
}

/// "Total reclaimed space: 1.234GB" from the end of a prune.
fn parse_reclaimed(text: &str) -> Option<u64> {
    text.lines()
        .rev()
        .find(|l| l.to_ascii_lowercase().contains("reclaimed space"))
        .and_then(|l| l.split_once(':'))
        .and_then(|(_, v)| parse_docker_size(v))
}

/// What `DISM /Online /Cleanup-Image /AnalyzeComponentStore` reported.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ComponentStore {
    pub explorer_bytes: Option<u64>,
    pub actual_bytes: Option<u64>,
    pub shared_bytes: Option<u64>,
    pub backups_bytes: Option<u64>,
    pub cache_bytes: Option<u64>,
    pub last_cleanup: Option<String>,
    pub reclaimable_packages: Option<u32>,
    pub cleanup_recommended: Option<bool>,
}

/// Reads the English labels of the DISM report. Returns None when none of
/// the sizes are found, as on a Windows in another language.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_dism_analysis(text: &str) -> Option<ComponentStore> {
    let mut s = ComponentStore::default();
    for line in text.lines() {
        let Some((label, value)) = line.split_once(" : ") else { continue };
        let (label, value) = (label.trim(), value.trim());
        if label.starts_with("Windows Explorer Reported Size") {
            s.explorer_bytes = parse_binary_size(value);
        } else if label.starts_with("Actual Size of Component Store") {
            s.actual_bytes = parse_binary_size(value);
        } else if label.starts_with("Shared with Windows") {
            s.shared_bytes = parse_binary_size(value);
        } else if label.starts_with("Backups and Disabled Features") {
            s.backups_bytes = parse_binary_size(value);
        } else if label.starts_with("Cache and Temporary Data") {
            s.cache_bytes = parse_binary_size(value);
        } else if label.starts_with("Date of Last Cleanup") {
            s.last_cleanup = Some(value.to_string());
        } else if label.starts_with("Number of Reclaimable Packages") {
            s.reclaimable_packages = value.parse().ok();
        } else if label.starts_with("Component Store Cleanup Recommended") {
            s.cleanup_recommended = match value.to_ascii_lowercase().as_str() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            };
        }
    }
    (s.actual_bytes.is_some() || s.explorer_bytes.is_some()).then_some(s)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RestorePoint {
    /// The shadow copy's id, `{GUID}`.
    pub id: String,
    pub created: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RestoreState {
    /// System Restore points on the system drive, oldest first.
    pub points: Vec<RestorePoint>,
    pub used: Option<u64>,
    pub allocated: Option<u64>,
    /// None when the limit is unbounded.
    pub max: Option<u64>,
}

/// Anything from here up is how WMI reports "no limit".
const UNBOUNDED: u64 = 1 << 62;

fn as_list(v: Option<&serde_json::Value>) -> Vec<&serde_json::Value> {
    match v {
        Some(serde_json::Value::Array(a)) => a.iter().collect(),
        Some(serde_json::Value::Null) | None => vec![],
        Some(other) => vec![other],
    }
}

/// The JSON the restore point query prints (see `RESTORE_SCRIPT`).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_restore_json(text: &str) -> Result<RestoreState, String> {
    let text = text.trim().trim_start_matches('\u{feff}');
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("unreadable restore point list: {e}"))?;
    let mut points: Vec<RestorePoint> = as_list(v.get("points"))
        .into_iter()
        .filter_map(|p| {
            Some(RestorePoint { id: p.get("id")?.as_str()?.to_string(), created: p.get("created").map(json_u64)? })
        })
        .collect();
    points.sort_by_key(|p| p.created);
    let storage = as_list(v.get("storage"));
    let first = storage.first();
    let field = |key: &str| -> Option<u64> {
        let v = first?.get(key)?;
        let n = match v {
            serde_json::Value::String(s) => s.trim().parse::<u64>().ok()?,
            other => other.as_u64()?,
        };
        Some(n)
    };
    let max = field("max").filter(|m| *m < UNBOUNDED);
    Ok(RestoreState { points, used: field("used"), allocated: field("allocated"), max })
}

/// `{3D6A7F2E-0000-4C1B-8A00-1234567890AB}`: the only shape of shadow copy id
/// allowed into a command line.
fn valid_shadow_id(id: &str) -> bool {
    let b = id.as_bytes();
    id.len() == 38
        && b[0] == b'{'
        && b[37] == b'}'
        && id[1..37].chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// `com.apple.TimeMachine.2026-10-28-101530.local` lines of
/// `tmutil listlocalsnapshots /`, as the dates `tmutil` deletes by.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_tm_snapshots(text: &str) -> Vec<String> {
    let mut dates: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("com.apple.TimeMachine."))
        .map(|rest| rest.strip_suffix(".local").unwrap_or(rest))
        .filter(|d| valid_tm_date(d))
        .map(String::from)
        .collect();
    dates.sort();
    dates.dedup();
    dates
}

/// `YYYY-MM-DD-HHMMSS`, the only shape of snapshot name allowed into a
/// command line.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn valid_tm_date(d: &str) -> bool {
    let parts: Vec<&str> = d.split('-').collect();
    parts.len() == 4
        && [4, 2, 2, 6].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_digit()))
}

// ---------------------------------------------------------------- administrator actions (Windows commands)

/// The only things the elevated helper will do. It receives one of these ids
/// and nothing else; it works out every command line itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdminAction {
    ListRestorePoints,
    AnalyzeComponentStore,
    HibernateOff,
    HibernateReduced,
    RestoreDeleteOldest,
    RestoreKeepNewest,
    ComponentCleanup,
}

const ADMIN_ACTIONS: [AdminAction; 7] = [
    AdminAction::ListRestorePoints,
    AdminAction::AnalyzeComponentStore,
    AdminAction::HibernateOff,
    AdminAction::HibernateReduced,
    AdminAction::RestoreDeleteOldest,
    AdminAction::RestoreKeepNewest,
    AdminAction::ComponentCleanup,
];

impl AdminAction {
    pub fn id(self) -> &'static str {
        match self {
            AdminAction::ListRestorePoints => "list-restore-points",
            AdminAction::AnalyzeComponentStore => "analyze-component-store",
            AdminAction::HibernateOff => "hibernate-off",
            AdminAction::HibernateReduced => "hibernate-reduced",
            AdminAction::RestoreDeleteOldest => "restore-delete-oldest",
            AdminAction::RestoreKeepNewest => "restore-keep-newest",
            AdminAction::ComponentCleanup => "component-cleanup",
        }
    }

    pub fn from_id(id: &str) -> Option<AdminAction> {
        ADMIN_ACTIONS.iter().copied().find(|a| a.id() == id)
    }

    /// Deleting restore points takes one more argument: the id of the point
    /// the user was shown (the oldest for "delete the oldest", the newest for
    /// "keep only the newest"). The helper deletes only if the list it reads
    /// now still has that point in that place.
    pub fn takes_point(self) -> bool {
        matches!(self, AdminAction::RestoreDeleteOldest | AdminAction::RestoreKeepNewest)
    }
}

/// One program run by the helper: a bare file name looked up only in
/// `%SystemRoot%\System32`, never on PATH.
#[derive(Debug, Clone, PartialEq)]
struct Step {
    exe: &'static str,
    shown: &'static str,
    args: Vec<String>,
}

/// The programs the helper may start at all.
const ALLOWED_EXES: [&str; 3] = ["powercfg.exe", "dism.exe", "vssadmin.exe"];

impl Step {
    fn new(exe: &'static str, shown: &'static str, args: &[&str]) -> Step {
        Step { exe, shown, args: args.iter().map(|a| a.to_string()).collect() }
    }
    fn display(&self) -> String {
        format!("{} {}", self.shown, self.args.join(" "))
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn fixed_steps(action: AdminAction) -> Vec<Step> {
    match action {
        AdminAction::HibernateOff => vec![Step::new("powercfg.exe", "powercfg", &["/hibernate", "off"])],
        AdminAction::HibernateReduced => vec![Step::new("powercfg.exe", "powercfg", &["/h", "/type", "reduced"])],
        AdminAction::AnalyzeComponentStore => {
            vec![Step::new("dism.exe", "DISM", &["/Online", "/Cleanup-Image", "/AnalyzeComponentStore"])]
        }
        AdminAction::ComponentCleanup => {
            vec![Step::new("dism.exe", "DISM", &["/Online", "/Cleanup-Image", "/StartComponentCleanup"])]
        }
        // These two decide what to delete from a fresh list; see `delete_steps`.
        AdminAction::ListRestorePoints | AdminAction::RestoreDeleteOldest | AdminAction::RestoreKeepNewest => vec![],
    }
}

/// Which restore points an action removes, from the list as it is now.
/// Oldest first in, oldest first out.
fn victims(action: AdminAction, points: &[RestorePoint]) -> Vec<&RestorePoint> {
    match action {
        AdminAction::RestoreDeleteOldest => points.iter().take(1).collect(),
        AdminAction::RestoreKeepNewest => points.iter().take(points.len().saturating_sub(1)).collect(),
        _ => vec![],
    }
}

/// The point the user is shown for a deletion, and the helper must find in
/// the same place: the oldest for "delete the oldest", the newest (the one that
/// stays) for "keep only the newest".
fn shown_point(action: AdminAction, points: &[RestorePoint]) -> Option<&RestorePoint> {
    match action {
        AdminAction::RestoreDeleteOldest => points.first(),
        AdminAction::RestoreKeepNewest => points.last(),
        _ => None,
    }
}

/// `vssadmin delete shadows /shadow={id} /quiet` for each point. An id that
/// is not a plain GUID is refused rather than put on a command line.
fn delete_steps(points: &[&RestorePoint]) -> Result<Vec<Step>, String> {
    points
        .iter()
        .map(|p| {
            if !valid_shadow_id(&p.id) {
                return Err(format!("refused shadow copy id {:?}", p.id));
            }
            Ok(Step {
                exe: "vssadmin.exe",
                shown: "vssadmin",
                args: vec!["delete".into(), "shadows".into(), format!("/shadow={}", p.id), "/quiet".into()],
            })
        })
        .collect()
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct StepLog {
    pub command: String,
    pub exit_code: Option<i32>,
    pub output: String,
}

/// What the elevated helper reports back to Jharu.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct HelperResult {
    pub ok: bool,
    /// The helper looked again and found nothing to do.
    pub skipped: Option<String>,
    pub restore: Option<RestoreState>,
    pub store: Option<ComponentStore>,
    pub deleted: u32,
    pub log: Vec<StepLog>,
    pub errors: Vec<String>,
}

/// Entry point when Jharu was started as the elevated system helper:
/// `Jharu.exe --jharu-elevated-system <result.json> <action id> [<shadow id>]`.
/// The shadow id is required for the two restore point deletions and refused
/// for everything else.
/// Returns the process exit code, or None for a normal app start.
pub fn helper_main(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(HELPER_FLAG) {
        return None;
    }
    let Some(result_path) = args.get(2) else { return Some(2) };
    let mut result = HelperResult::default();
    // Exactly one known id; a raw command, a path or anything else is refused.
    // The restore point deletions also carry a strictly validated GUID.
    let action = args.get(3).and_then(|id| AdminAction::from_id(id));
    let point = args.get(4).map(String::as_str);
    match (args.len(), action) {
        (4, Some(action)) if !action.takes_point() => result = run_admin_action(action, None, None),
        (5, Some(action)) if action.takes_point() && point.is_some_and(valid_shadow_id) => {
            result = run_admin_action(action, point, None)
        }
        _ => result.errors.push("Refused: not a known system space action".into()),
    }
    let json = serde_json::to_vec(&result).unwrap_or_default();
    // create_new refuses to follow a file or link planted at this path, so the
    // elevated process cannot be tricked into overwriting something else.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(result_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &json));
    Some(if written.is_ok() { 0 } else { 3 })
}

#[cfg(not(target_os = "windows"))]
fn run_admin_action(_action: AdminAction, _point: Option<&str>, _cancel: Option<&AtomicBool>) -> HelperResult {
    HelperResult { errors: vec!["These actions are only implemented on Windows".into()], ..Default::default() }
}

#[cfg(target_os = "windows")]
fn run_admin_action(action: AdminAction, point: Option<&str>, cancel: Option<&AtomicBool>) -> HelperResult {
    win::run_admin_action(action, point, cancel)
}

// ---------------------------------------------------------------- Windows

#[cfg(target_os = "windows")]
mod win {
    use super::*;

    pub fn sysroot() -> PathBuf {
        PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()))
    }

    /// `C:\`, or wherever Windows is installed.
    pub fn system_drive() -> PathBuf {
        let d = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{d}\\"))
    }

    pub fn system32(exe: &str) -> PathBuf {
        sysroot().join("System32").join(exe)
    }

    fn powershell() -> PathBuf {
        system32("WindowsPowerShell/v1.0/powershell.exe")
    }

    /// No double quotes in these scripts: they travel as one command-line
    /// argument, where quotes are the first thing to go wrong.
    const PAGEFILE_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
$pf = @(Get-CimInstance Win32_PageFileUsage | ForEach-Object { [pscustomobject]@{ name = $_.Name; allocated_mb = [string]$_.AllocatedBaseSize; used_mb = [string]$_.CurrentUsage; peak_mb = [string]$_.PeakUsage } }); \
$auto = [bool](Get-CimInstance Win32_ComputerSystem).AutomaticManagedPagefile; \
[pscustomobject]@{ files = $pf; auto = $auto } | ConvertTo-Json -Compress -Depth 4";

    /// System Restore points are the client-accessible shadow copies of the
    /// system volume. Both classes need administrator rights to read.
    pub const RESTORE_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
$d = $env:SystemDrive; \
$vol = (Get-CimInstance Win32_Volume | Where-Object { $_.DriveLetter -eq $d } | Select-Object -First 1).DeviceID; \
if (-not $vol) { throw 'system volume not found' }; \
$guid = [regex]::Match($vol, '\\{[0-9a-fA-F-]+\\}').Value; \
$pts = @(Get-CimInstance Win32_ShadowCopy | Where-Object { $_.VolumeName -like ('*' + $guid + '*') -and $_.ClientAccessible } | ForEach-Object { [pscustomobject]@{ id = $_.ID; created = ([DateTimeOffset]$_.InstallDate).ToUnixTimeSeconds() } }); \
$st = @(Get-CimInstance Win32_ShadowStorage | Where-Object { (([string]$_.Volume.DeviceID) + ([string]$_.Volume)) -like ('*' + $guid + '*') } | ForEach-Object { [pscustomobject]@{ used = [string]$_.UsedSpace; allocated = [string]$_.AllocatedSpace; max = [string]$_.MaxSpace } }); \
[pscustomobject]@{ points = $pts; storage = $st } | ConvertTo-Json -Compress -Depth 4";

    pub fn run_ps(script: &str, timeout: Option<Duration>, cancel: Option<&AtomicBool>) -> Result<CmdOut, String> {
        let mut cmd = command(&powershell());
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        run_cmd(cmd, timeout, cancel)
    }

    fn reg_u32(subkey: &str, name: &str) -> Option<u32> {
        use winreg::enums::*;
        use winreg::RegKey;
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(subkey, KEY_READ).ok()?.get_value::<u32, _>(name).ok()
    }

    // ---- hibernation

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum HiberMode {
        Full,
        Reduced,
    }

    /// `HiberFileSizePercent` is 0 for the default (full) size; the reduced
    /// type is stored as 40.
    pub fn hiber_mode(percent: u32) -> HiberMode {
        if (1..=50).contains(&percent) {
            HiberMode::Reduced
        } else {
            HiberMode::Full
        }
    }

    /// What reducing the file would free, from its size and current percent.
    pub fn reduce_estimate(size: u64, percent: u32) -> u64 {
        let current = if percent == 0 { HIBER_FULL_PERCENT } else { percent as u64 };
        if current <= HIBER_REDUCED_PERCENT {
            return 0;
        }
        size - size * HIBER_REDUCED_PERCENT / current
    }

    pub fn hiberfile() -> PathBuf {
        system_drive().join("hiberfil.sys")
    }

    pub fn hiber_percent_now() -> u32 {
        reg_u32(r"SYSTEM\CurrentControlSet\Control\Power", "HiberFileSizePercent").unwrap_or(0)
    }

    fn fast_startup() -> Option<bool> {
        reg_u32(r"SYSTEM\CurrentControlSet\Control\Session Manager\Power", "HiberbootEnabled").map(|v| v != 0)
    }

    pub fn hibernation_item() -> SpaceItem {
        let file = hiberfile();
        let meta = std::fs::metadata(&file);
        let size = meta.as_ref().ok().map(|m| m.len());
        let unreadable = matches!(&meta, Err(e) if e.kind() != std::io::ErrorKind::NotFound);
        let percent = hiber_percent_now();
        let mode = hiber_mode(percent);
        let fast = fast_startup();

        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let ram = sys.total_memory();

        let mut facts = vec![fact("File", file.display().to_string())];
        if ram > 0 {
            facts.push(fact("Memory", format!("{} of RAM; the file holds a compressed copy of it", human(ram))));
        }
        if size.is_some() {
            facts.push(fact(
                "Type",
                match mode {
                    HiberMode::Full => "Full: hibernate and Fast Startup both work",
                    HiberMode::Reduced => "Reduced: only Fast Startup works, hibernate does not",
                },
            ));
        }
        facts.push(fact(
            "Fast Startup",
            match fast {
                Some(true) => "On: shutdown saves a hibernation image to speed up the next start",
                Some(false) => "Off",
                None => "Unknown",
            },
        ));

        let mut actions = vec![];
        if let Some(size) = size {
            let fast_reason = match fast {
                Some(true) => warn("Fast Startup is on right now and will stop working. Shutdown becomes a full shutdown, so starting up can take a few seconds longer."),
                Some(false) => ok("Fast Startup is already off, so the only feature lost is hibernate."),
                None => warn("Jharu could not tell whether Fast Startup is on. If it is, it stops working."),
            };
            actions.push(SpaceAction {
                id: AdminAction::HibernateOff.id().into(),
                label: "Turn off hibernation".into(),
                commands: fixed_steps(AdminAction::HibernateOff).iter().map(Step::display).collect(),
                cost: "Hibernate and Fast Startup stop working. Windows deletes the file immediately; no restart is needed.".into(),
                safety: Safety::Caution,
                reasons: vec![
                    warn("Turns off hibernate: a laptop can no longer save your open programs to disk and power off completely."),
                    fast_reason,
                    ok("Nothing of yours is deleted. The file only holds a saved copy of memory that Windows rewrites whenever it hibernates."),
                    ok("Reversible: powercfg /hibernate on creates the file again (it takes the same space back)."),
                ],
                admin: true,
                permanent: false,
                undo: Some("Run `powercfg /hibernate on`; the file comes back at the same size.".into()),
                slow: false,
                cancellable: false,
                est_bytes: Some(size),
                est_note: None,
                enabled: true,
                disabled_reason: None,
            });
            if mode == HiberMode::Full {
                let est = reduce_estimate(size, percent);
                actions.push(SpaceAction {
                    id: AdminAction::HibernateReduced.id().into(),
                    label: "Shrink to the reduced type".into(),
                    commands: fixed_steps(AdminAction::HibernateReduced).iter().map(Step::display).collect(),
                    cost: "Hibernate stops working, Fast Startup keeps working. The file shrinks to about 40% of RAM.".into(),
                    safety: Safety::Caution,
                    reasons: vec![
                        warn("Turns off hibernate (the reduced file is too small to hold a full session) but keeps Fast Startup."),
                        ok("Nothing of yours is deleted."),
                        ok("Reversible: powercfg /h /type full restores the full size."),
                        warn("The saving is an estimate from the usual sizes (75% and 40% of RAM); Windows reports the exact size afterwards."),
                    ],
                    admin: true,
                    permanent: false,
                    undo: Some("Run `powercfg /h /type full` to get the full size back.".into()),
                    slow: false,
                    cancellable: false,
                    est_bytes: Some(est),
                    est_note: Some("Estimate from the usual sizes (75% and 40% of RAM); Windows reports the exact figure afterwards.".into()),
                    enabled: true,
                    disabled_reason: None,
                });
            }
        }

        SpaceItem {
            id: "hibernation".into(),
            title: "Hibernation file".into(),
            summary: "hiberfil.sys keeps a copy of memory so Windows can hibernate and start fast. It is as large as a good part of your RAM, all the time.".into(),
            size_bytes: size,
            size_note: match (size, unreadable) {
                (None, true) => Some("Jharu could not read the file's size".into()),
                (None, false) => Some("Hibernation is already off: there is no file".into()),
                _ => None,
            },
            facts,
            notice: None,
            how_to: None,
            info_only: false,
            inspect: None,
            actions,
        }
    }

    // ---- page file (information only)

    pub fn pagefile_item() -> SpaceItem {
        #[derive(Deserialize, Default)]
        struct Files {
            #[serde(default)]
            files: Vec<serde_json::Value>,
            #[serde(default)]
            auto: bool,
        }
        let parsed: Option<Files> = run_ps(PAGEFILE_SCRIPT, Some(QUERY_TIMEOUT), None)
            .ok()
            .filter(CmdOut::success)
            .and_then(|o| serde_json::from_str(o.stdout.trim().trim_start_matches('\u{feff}')).ok());

        let mut facts = vec![];
        let mut size = 0u64;
        let mut known = false;
        if let Some(p) = &parsed {
            for f in &p.files {
                let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("pagefile.sys");
                let mb = |k: &str| f.get(k).map(json_u64).unwrap_or(0) * 1024 * 1024;
                // The file's real length, which can exceed the base size once
                // Windows has grown it; the base size when it cannot be read.
                let on_disk = std::fs::metadata(name).map(|m| m.len()).unwrap_or_else(|_| mb("allocated_mb"));
                size += on_disk;
                known = true;
                facts.push(fact(
                    "Page file",
                    format!(
                        "{name}: {} on disk, {} in use now, {} at the peak since start",
                        human(on_disk),
                        human(mb("used_mb")),
                        human(mb("peak_mb"))
                    ),
                ));
            }
            facts.push(fact(
                "Managed by",
                if p.auto { "Windows decides its size automatically" } else { "A size you set by hand" },
            ));
        }
        if !known {
            // No answer from WMI: fall back to the usual place.
            if let Ok(m) = std::fs::metadata(system_drive().join("pagefile.sys")) {
                size = m.len();
                known = true;
                facts.push(fact("Page file", format!("{}: {}", system_drive().join("pagefile.sys").display(), human(size))));
            }
        }

        SpaceItem {
            id: "pagefile".into(),
            title: "Page file".into(),
            summary: "pagefile.sys is the overflow for memory: Windows moves what programs are not using into it, and it is where crash dumps start.".into(),
            size_bytes: known.then_some(size),
            size_note: (!known).then(|| "No page file found, or Windows would not say".into()),
            facts,
            notice: None,
            how_to: Some(
                "Jharu never changes this. To change it yourself: press Win+R, run sysdm.cpl, open Advanced, then Performance Settings, Advanced, Virtual memory, Change. Untick the automatic option to set a size, or move it to another drive. A page file that is too small or missing makes programs fail with out-of-memory errors and stops crash dumps, so most people are better off leaving it automatic."
                    .into(),
            ),
            info_only: true,
            inspect: None,
            actions: vec![],
        }
    }

    // ---- restore points

    pub fn restore_item() -> SpaceItem {
        let cached = looked().lock().ok().and_then(|l| l.restore.clone());
        let list_cmd = "powershell Get-CimInstance Win32_ShadowCopy, Win32_ShadowStorage  (read-only query)".to_string();
        let mut item = SpaceItem {
            id: "restore-points".into(),
            title: "System Restore points".into(),
            summary: "Snapshots Windows takes before installs and updates so it can roll back. They share a reserved slice of the system drive.".into(),
            size_bytes: None,
            size_note: None,
            facts: vec![],
            notice: None,
            how_to: None,
            info_only: false,
            inspect: Some(Inspect {
                label: "Check restore points".into(),
                command: list_cmd,
                admin: true,
                slow: false,
                done: cached.is_some(),
            }),
            actions: vec![],
        };
        let Some((state, checked)) = cached else {
            item.size_note = Some("Windows only lets administrators see restore points, so Jharu asks first. Looking changes nothing.".into());
            return item;
        };

        item.size_bytes = state.used;
        item.size_note = Some("Shadow storage in use for the system drive".into());
        item.facts.push(fact("Checked", format!("{} (click Check again to refresh)", age_text(checked))));
        item.facts.push(fact("Restore points", state.points.len().to_string()));
        if let (Some(first), Some(last)) = (state.points.first(), state.points.last()) {
            item.facts.push(fact("Oldest", age_text(first.created)));
            item.facts.push(fact("Newest", age_text(last.created)));
        }
        if let Some(used) = state.used {
            item.facts.push(fact("Storage in use", human(used)));
        }
        if let Some(a) = state.allocated {
            item.facts.push(fact("Storage allocated", human(a)));
        }
        item.facts.push(fact(
            "Storage limit",
            state.max.map(human).unwrap_or_else(|| "No limit".into()),
        ));
        if state.points.is_empty() {
            item.notice = Some("There are no restore points on the system drive: System Protection may be off, or none was made yet.".into());
            return item;
        }

        let oldest = &state.points[0];
        let newest = state.points.last().unwrap();
        let common = |extra: Vec<Reason>| -> Vec<Reason> {
            let mut r = vec![
                bad("You lose the way back: restore points are how Windows undoes a bad driver, update or install."),
                bad("Permanent. Restore points are not files, so they cannot go to the Recycle Bin and nothing brings them back."),
                warn("The list is every shadow copy of the system drive that programs can browse. System Restore stores its points that way, but backup software can make the same kind, and deleting one removes it for that software too."),
                ok("Your documents, apps and settings are not touched; System Restore never held your files."),
            ];
            r.extend(extra);
            r
        };
        let est_note = Some("Windows does not report how much each point holds; the storage figure above shows the change afterwards.".to_string());

        actions_push_restore(
            &mut item,
            AdminAction::RestoreDeleteOldest,
            "Delete the oldest restore point",
            delete_steps(&victims(AdminAction::RestoreDeleteOldest, &state.points)).unwrap_or_default(),
            format!("You can no longer roll back to {} (the oldest point).", age_text(oldest.created)),
            common(vec![ok(format!(
                "Every newer point stays, including the newest ({}).",
                age_text(newest.created)
            ))]),
            true,
            est_note.clone(),
        );
        let many = state.points.len() > 1;
        actions_push_restore(
            &mut item,
            AdminAction::RestoreKeepNewest,
            "Keep only the newest restore point",
            delete_steps(&victims(AdminAction::RestoreKeepNewest, &state.points)).unwrap_or_default(),
            format!(
                "{} are deleted; only the newest ({}) is kept.",
                plural(state.points.len().saturating_sub(1), "restore point", "restore points"),
                age_text(newest.created)
            ),
            common(vec![
                ok(format!("The newest point ({}) is kept, so you can still roll back to it.", age_text(newest.created))),
                if newest.created + 14 * 86_400 < now_secs() {
                    warn("Even the newest point is more than two weeks old, so it would not undo anything recent.")
                } else {
                    ok("The newest point is recent.")
                },
            ]),
            many,
            est_note,
        );
        item
    }

    #[allow(clippy::too_many_arguments)]
    fn actions_push_restore(
        item: &mut SpaceItem,
        action: AdminAction,
        label: &str,
        steps: Vec<Step>,
        cost: String,
        reasons: Vec<Reason>,
        enabled: bool,
        est_note: Option<String>,
    ) {
        item.actions.push(SpaceAction {
            id: action.id().into(),
            label: label.into(),
            commands: steps.iter().map(Step::display).collect(),
            cost,
            safety: Safety::Risky,
            reasons,
            admin: true,
            permanent: true,
            undo: None,
            slow: false,
            cancellable: false,
            est_bytes: None,
            est_note,
            enabled: enabled && !steps.is_empty(),
            disabled_reason: (!(enabled && !steps.is_empty())).then(|| "There is only one restore point, and it is the one that stays".to_string()),
        });
    }

    // ---- component store

    pub fn store_item() -> SpaceItem {
        let cached = looked().lock().ok().and_then(|l| l.store.clone());
        let analyze = fixed_steps(AdminAction::AnalyzeComponentStore)[0].display();
        let mut item = SpaceItem {
            id: "winsxs".into(),
            title: "Windows component store (WinSxS)".into(),
            summary: "The folder where Windows keeps every version of its own files, for updates, repair and optional features. Explorer overstates it, because most files are hard links shared with the real system folders.".into(),
            size_bytes: None,
            size_note: None,
            facts: vec![],
            notice: None,
            how_to: None,
            info_only: false,
            inspect: Some(Inspect {
                label: "Analyze component store".into(),
                command: analyze,
                admin: true,
                slow: true,
                done: cached.is_some(),
            }),
            actions: vec![],
        };
        let Some((store, checked)) = cached else {
            item.size_note = Some("Walking the folder would count shared files twice, so Jharu asks Windows. The analysis needs administrator rights and takes a minute or two. Looking changes nothing.".into());
            return item;
        };

        item.size_bytes = store.actual_bytes.or(store.explorer_bytes);
        item.size_note = Some("Actual size, as Windows counts it (shared files once)".into());
        item.facts.push(fact("Checked", format!("{} (click Analyze again to refresh)", age_text(checked))));
        let mut size_fact = |label: &str, v: Option<u64>| {
            if let Some(v) = v {
                item.facts.push(fact(label, human(v)));
            }
        };
        size_fact("Explorer reports", store.explorer_bytes);
        size_fact("Actual size", store.actual_bytes);
        size_fact("Shared with Windows", store.shared_bytes);
        size_fact("Backups and disabled features", store.backups_bytes);
        size_fact("Cache and temporary data", store.cache_bytes);
        if let Some(d) = &store.last_cleanup {
            item.facts.push(fact("Last cleanup", d.clone()));
        }
        if let Some(n) = store.reclaimable_packages {
            item.facts.push(fact("Reclaimable packages", n.to_string()));
        }

        let recommendation = match (store.cleanup_recommended, store.reclaimable_packages) {
            (Some(true), n) => ok(format!(
                "Windows' own analysis says a cleanup is recommended{}.",
                n.map(|n| format!(" ({})", plural(n as usize, "reclaimable package", "reclaimable packages"))).unwrap_or_default()
            )),
            (Some(false), _) => warn("Windows' own analysis says a cleanup is not recommended right now, so expect little to be freed."),
            (None, _) => warn("The analysis did not say whether a cleanup is recommended."),
        };
        item.actions.push(SpaceAction {
            id: AdminAction::ComponentCleanup.id().into(),
            label: "Clean up the component store".into(),
            commands: fixed_steps(AdminAction::ComponentCleanup).iter().map(Step::display).collect(),
            cost: "Takes a few minutes, sometimes much longer, and Windows must stay on. Older versions of updated components can no longer be used to repair, but installed updates can still be uninstalled.".into(),
            safety: Safety::Safe,
            reasons: vec![
                ok("This is the cleanup Microsoft documents for the component store, and the same one Windows runs itself on a schedule."),
                ok("/ResetBase is not used, so every installed update can still be uninstalled."),
                recommendation,
                warn("Do not shut down or sleep while it runs. Jharu cannot stop it half way."),
            ],
            admin: true,
            permanent: false,
            undo: None,
            slow: true,
            cancellable: false,
            est_bytes: None,
            est_note: Some("Windows does not say how much a cleanup will free; Jharu reports the change in actual size afterwards.".into()),
            enabled: true,
            disabled_reason: None,
        });
        item
    }

    // ---- running the helper's actions

    pub enum AdminError {
        Declined,
        Failed(String),
    }

    /// Runs an allowed action as an administrator: in this process when Jharu
    /// is already elevated, otherwise in a relaunched, elevated copy.
    pub fn run_admin(action: AdminAction, point: Option<&str>, cancel: Option<&AtomicBool>) -> Result<HelperResult, AdminError> {
        if crate::cleaning::is_elevated() {
            return Ok(run_admin_action(action, point, cancel));
        }
        let mut args = vec![action.id().to_string()];
        if let Some(p) = point {
            // Only a plain GUID may ride on the helper's command line.
            if !valid_shadow_id(p) {
                return Err(AdminError::Failed("refused: not a restore point id".into()));
            }
            args.push(p.to_string());
        }
        let helper = crate::cleaning::run_helper(HELPER_FLAG, &args);
        if helper.declined {
            return Err(AdminError::Declined);
        }
        match helper.result {
            Some(bytes) => {
                let mut r: HelperResult = serde_json::from_slice(&bytes)
                    .map_err(|e| AdminError::Failed(format!("Unreadable result from the administrator helper: {e}")))?;
                r.errors.extend(helper.errors);
                Ok(r)
            }
            None => Err(AdminError::Failed(helper.errors.join("; "))),
        }
    }

    fn run_step(step: &Step, log: &mut Vec<StepLog>, cancel: Option<&AtomicBool>) -> bool {
        // Defence in depth: the table above is the allowlist.
        if !ALLOWED_EXES.contains(&step.exe) {
            log.push(StepLog { command: step.display(), exit_code: None, output: "refused: program not allowed".into() });
            return false;
        }
        let mut cmd = command(&system32(step.exe));
        cmd.args(&step.args);
        match run_cmd(cmd, None, cancel) {
            Ok(out) => {
                let success = out.success();
                log.push(StepLog { command: step.display(), exit_code: out.code, output: tail(&out.combined()) });
                success
            }
            Err(e) => {
                log.push(StepLog { command: step.display(), exit_code: None, output: e });
                false
            }
        }
    }

    fn query_restore(result: &mut HelperResult) -> Option<RestoreState> {
        let out = match run_ps(RESTORE_SCRIPT, Some(Duration::from_secs(120)), None) {
            Ok(o) => o,
            Err(e) => {
                result.errors.push(format!("Could not start PowerShell: {e}"));
                return None;
            }
        };
        if !out.success() {
            result.errors.push(format!("Could not read restore points: {}", tail(&out.combined())));
            return None;
        }
        match parse_restore_json(&out.stdout) {
            Ok(s) => Some(s),
            Err(e) => {
                result.errors.push(e);
                None
            }
        }
    }

    fn analyze(result: &mut HelperResult, cancel: Option<&AtomicBool>) {
        let step = &fixed_steps(AdminAction::AnalyzeComponentStore)[0];
        let mut log = vec![];
        let success = run_step(step, &mut log, cancel);
        let text = log.first().map(|l| l.output.clone()).unwrap_or_default();
        result.store = parse_dism_analysis(&text);
        if success && result.store.is_none() {
            result.errors.push(
                "DISM finished, but its report was not in English, so Jharu cannot read the figures. Its own output is shown below.".into(),
            );
        }
        result.log.extend(log);
    }

    pub fn run_admin_action(action: AdminAction, point: Option<&str>, cancel: Option<&AtomicBool>) -> HelperResult {
        let mut result = HelperResult::default();
        match action {
            AdminAction::ListRestorePoints => {
                result.restore = query_restore(&mut result);
                result.ok = result.restore.is_some();
            }
            AdminAction::AnalyzeComponentStore => {
                analyze(&mut result, cancel);
                result.ok = result.store.is_some();
            }
            AdminAction::HibernateOff | AdminAction::HibernateReduced => {
                let mut ok = true;
                for step in fixed_steps(action) {
                    ok &= run_step(&step, &mut result.log, cancel);
                }
                result.ok = ok;
            }
            AdminAction::ComponentCleanup => {
                let mut ok = true;
                for step in fixed_steps(action) {
                    ok &= run_step(&step, &mut result.log, cancel);
                }
                result.ok = ok;
                // What the store looks like now, for the figure freed.
                if ok {
                    analyze(&mut result, cancel);
                }
            }
            AdminAction::RestoreDeleteOldest | AdminAction::RestoreKeepNewest => {
                let Some(want) = point.filter(|p| valid_shadow_id(p)) else {
                    result.errors.push("Refused: no restore point id was given".into());
                    return result;
                };
                let Some(before) = query_restore(&mut result) else { return result };
                // The point the user was shown must still be where it was: the
                // oldest, or the newest. A point made or removed since they
                // looked changes what the action would delete, so nothing is
                // deleted and they look again.
                if !shown_point(action, &before.points).is_some_and(|p| p.id.eq_ignore_ascii_case(want)) {
                    result.skipped = Some("The restore points changed since you checked them, so nothing was deleted. Check again and review the list.".into());
                    result.restore = Some(before);
                    result.ok = true;
                    return result;
                }
                // Decided here, from the list as it is at this moment.
                let doomed = victims(action, &before.points);
                if doomed.is_empty() {
                    result.skipped = Some(match action {
                        AdminAction::RestoreDeleteOldest => "There are no restore points any more.".into(),
                        _ => "Only one restore point is left, and it is the one that stays.".into(),
                    });
                    result.restore = Some(before);
                    result.ok = true;
                    return result;
                }
                let steps = match delete_steps(&doomed) {
                    Ok(s) => s,
                    Err(e) => {
                        result.errors.push(e);
                        return result;
                    }
                };
                let wanted = steps.len();
                for step in &steps {
                    if run_step(step, &mut result.log, cancel) {
                        result.deleted += 1;
                    }
                }
                result.ok = result.deleted as usize == wanted;
                result.restore = query_restore(&mut result).or(Some(before));
            }
        }
        result
    }
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod mac {
    use super::*;

    const TMUTIL: &str = "/usr/bin/tmutil";

    pub fn snapshots() -> Result<Vec<String>, String> {
        let mut cmd = command(Path::new(TMUTIL));
        cmd.args(["listlocalsnapshots", "/"]);
        let out = run_cmd(cmd, Some(QUERY_TIMEOUT), None)?;
        if !out.success() {
            return Err(tail(&out.combined()));
        }
        Ok(parse_tm_snapshots(&out.stdout))
    }

    pub fn tm_item() -> SpaceItem {
        let snaps = snapshots();
        let list_cmd = format!("{TMUTIL} listlocalsnapshots /");
        let mut item = SpaceItem {
            id: "tm-snapshots".into(),
            title: "Time Machine local snapshots".into(),
            summary: "Hourly snapshots macOS keeps on this disk between backups. macOS removes them itself when space runs low, but they can hold tens of gigabytes until then.".into(),
            size_bytes: None,
            size_note: Some("macOS does not report how large local snapshots are; they show up as purgeable space".into()),
            facts: vec![fact("Listed with", list_cmd)],
            notice: None,
            how_to: None,
            info_only: false,
            inspect: None,
            actions: vec![],
        };
        let dates = match snaps {
            Ok(d) => d,
            Err(e) => {
                item.notice = Some(format!("Jharu could not list the snapshots: {e}"));
                return item;
            }
        };
        item.facts.push(fact("Snapshots", dates.len().to_string()));
        if let (Some(first), Some(last)) = (dates.first(), dates.last()) {
            item.facts.push(fact("Oldest", first.clone()));
            item.facts.push(fact("Newest", last.clone()));
        }
        if dates.is_empty() {
            item.notice = Some("There are no local snapshots on this disk.".into());
            return item;
        }
        let reasons = vec![
            warn("You can no longer restore files from those local snapshots. Backups on your Time Machine disk are not touched."),
            bad("Permanent: snapshots are not files, so nothing goes to the Trash and nothing brings them back."),
            ok("macOS makes new snapshots by itself while Time Machine is on, and deletes old ones whenever it needs the space."),
            warn("Needs your password; macOS asks for it."),
        ];
        let mut push = |id: &str, label: &str, chosen: Vec<&String>, cost: String| {
            item.actions.push(SpaceAction {
                id: id.into(),
                label: label.into(),
                commands: chosen.iter().map(|d| format!("{TMUTIL} deletelocalsnapshots {d}")).collect(),
                cost,
                safety: Safety::Caution,
                reasons: reasons.clone(),
                admin: true,
                permanent: true,
                undo: None,
                slow: false,
                cancellable: false,
                est_bytes: None,
                est_note: Some("macOS does not report the size of snapshots; compare free space afterwards.".into()),
                enabled: !chosen.is_empty(),
                disabled_reason: chosen.is_empty().then(|| "No snapshot to delete".to_string()),
            });
        };
        push(
            "tm-delete-oldest",
            "Delete the oldest local snapshot",
            dates.iter().take(1).collect(),
            "You lose the oldest local restore point; the others stay.".into(),
        );
        push(
            "tm-delete-all",
            "Delete all local snapshots",
            dates.iter().collect(),
            format!("You lose {}.", plural(dates.len(), "local restore point", "local restore points")),
        );
        item
    }

    pub fn sleepimage_item() -> SpaceItem {
        let path = Path::new("/private/var/vm/sleepimage");
        let size = std::fs::metadata(path).ok().map(|m| m.len());
        let mut facts = vec![fact("File", path.display().to_string())];
        let mut pmset = command(Path::new("/usr/bin/pmset"));
        pmset.arg("-g");
        let mode = run_cmd(pmset, Some(QUERY_TIMEOUT), None)
            .ok()
            .and_then(|o| o.stdout.lines().find_map(|l| hibernate_mode_text(l)));
        if let Some(m) = mode {
            facts.push(fact("Power setting", m));
        }
        SpaceItem {
            id: "sleepimage".into(),
            title: "Sleep image".into(),
            summary: "macOS writes memory to /private/var/vm/sleepimage so it can restore your session after the battery runs out during sleep.".into(),
            size_bytes: size,
            size_note: size.is_none().then(|| "No sleep image found, or macOS would not let Jharu see it".to_string()),
            facts,
            notice: None,
            how_to: Some(
                "Jharu never changes this. macOS recreates the file at its set size whenever the Mac sleeps, so deleting it frees nothing for long, and changing hibernatemode with pmset is not recommended on current Macs."
                    .into(),
            ),
            info_only: true,
            inspect: None,
            actions: vec![],
        }
    }

    /// Deletes snapshots by date through one administrator prompt. Every date
    /// was just read from `tmutil` and is checked against the one allowed
    /// shape before it reaches the script.
    pub fn delete_snapshots(dates: &[String]) -> Result<CmdOut, String> {
        if dates.is_empty() || !dates.iter().all(|d| valid_tm_date(d)) {
            return Err("refused: not a Time Machine snapshot date".into());
        }
        let shell = dates.iter().map(|d| format!("{TMUTIL} deletelocalsnapshots {d}")).collect::<Vec<_>>().join("; ");
        let script = format!("do shell script \"{shell}\" with administrator privileges");
        let mut cmd = command(Path::new("/usr/bin/osascript"));
        cmd.arg("-e").arg(script);
        run_cmd(cmd, None, None)
    }
}

// ---------------------------------------------------------------- Docker

/// The Docker command line client, wherever this platform keeps it. A
/// Finder-launched app gets a minimal PATH, so the usual places are tried.
fn docker_binary() -> Option<PathBuf> {
    let mut explicit: Vec<PathBuf> = vec![];
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramW6432"] {
            if let Ok(pf) = std::env::var(var) {
                explicit.push(PathBuf::from(pf).join(r"Docker\Docker\resources\bin\docker.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for p in [
            "/usr/local/bin/docker",
            "/opt/homebrew/bin/docker",
            "/Applications/Docker.app/Contents/Resources/bin/docker",
        ] {
            explicit.push(PathBuf::from(p));
        }
        if let Some(h) = dirs::home_dir() {
            explicit.push(h.join(".docker/bin/docker"));
        }
    }
    if let Some(found) = explicit.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    // On PATH: asking it for its version is the only way to know.
    let path = PathBuf::from("docker");
    let mut cmd = command(&path);
    cmd.args(["--version"]);
    run_cmd(cmd, Some(Duration::from_secs(5)), None).ok().filter(CmdOut::success).map(|_| path)
}

fn docker_cmd(bin: &Path, args: &[&str], timeout: Option<Duration>, cancel: Option<&AtomicBool>) -> Result<CmdOut, String> {
    let mut cmd = command(bin);
    cmd.args(args);
    run_cmd(cmd, timeout, cancel)
}

struct DockerState {
    rows: Vec<DockerRow>,
    /// None when Docker would not answer the count: unknown is not "none".
    dangling_images: Option<usize>,
    stopped: Option<usize>,
    unused_volumes: Option<Vec<String>>,
}

impl DockerState {
    fn row(&self, kind: &str) -> DockerRow {
        self.rows.iter().find(|r| r.kind == kind).cloned().unwrap_or_default()
    }
}

fn lines(out: &CmdOut) -> Vec<String> {
    out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Docker's own account of its disk use, or the reason it gave none.
fn docker_state(bin: &Path) -> Result<DockerState, String> {
    let df = docker_cmd(bin, &["system", "df", "--format", "{{json .}}"], Some(QUERY_TIMEOUT), None)?;
    if df.timed_out {
        return Err("Docker did not answer in time".into());
    }
    if !df.success() {
        let why = df.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        return Err(if why.is_empty() { "Docker did not answer".into() } else { why });
    }
    let rows = parse_docker_df(&df.stdout);
    if rows.is_empty() {
        return Err("Docker's report could not be read".into());
    }
    let count = |args: &[&str]| docker_cmd(bin, args, Some(QUERY_TIMEOUT), None).ok().filter(CmdOut::success).map(|o| lines(&o));
    let dangling_images = count(&["image", "ls", "--filter", "dangling=true", "--format", "{{.ID}}"]).map(|l| l.len());
    let stopped = count(&["ps", "-a", "--filter", "status=exited", "--filter", "status=created", "--format", "{{.ID}}"]).map(|l| l.len());
    let unused_volumes = count(&["volume", "ls", "--filter", "dangling=true", "--format", "{{.Name}}"]);
    Ok(DockerState { rows, dangling_images, stopped, unused_volumes })
}

fn docker_item() -> Option<SpaceItem> {
    let bin = docker_binary()?;
    let mut item = SpaceItem {
        id: "docker".into(),
        title: "Docker".into(),
        summary: "Images, containers, networks and build cache kept by Docker. They live inside Docker's own disk image, so ordinary folder scans see one huge file.".into(),
        size_bytes: None,
        size_note: None,
        facts: vec![],
        notice: None,
        how_to: None,
        info_only: false,
        inspect: None,
        actions: vec![],
    };
    let state = match docker_state(&bin) {
        Ok(s) => s,
        Err(why) => {
            item.notice = Some(format!(
                "Docker is installed but is not answering ({why}). Start Docker Desktop (or the Docker service), then rescan."
            ));
            item.actions = docker_actions(None);
            return Some(item);
        }
    };
    item.size_bytes = Some(state.rows.iter().map(|r| r.size_bytes).sum());
    item.size_note = Some("As `docker system df` counts it".into());
    for r in &state.rows {
        item.facts.push(fact(
            &r.kind,
            format!(
                "{} ({} active) · {} · {} reclaimable",
                plural(r.total as usize, "item", "items"),
                r.active,
                human(r.size_bytes),
                human(r.reclaimable_bytes)
            ),
        ));
    }
    item.actions = docker_actions(Some(&state));
    Some(item)
}

fn docker_actions(state: Option<&DockerState>) -> Vec<SpaceAction> {
    let (containers, images, cache, volumes) = match state {
        Some(s) => (s.row("Containers"), s.row("Images"), s.row("Build Cache"), s.row("Local Volumes")),
        None => Default::default(),
    };
    let est = containers.reclaimable_bytes + images.reclaimable_bytes + cache.reclaimable_bytes;
    let gone = state.is_none();
    let off = || gone.then(|| "Docker is not running".to_string());

    let plain = vec![
        match state.and_then(|s| s.stopped) {
            Some(0) => ok("There are no stopped containers to remove."),
            Some(n) => warn(format!(
                "Removes {}: anything written inside them, outside a volume, is lost.",
                plural(n, "stopped container", "stopped containers")
            )),
            // Not counted: say what the command does, not that nothing is there.
            None => warn("Removes any stopped containers: anything written inside them, outside a volume, is lost."),
        },
        match state.and_then(|s| s.dangling_images) {
            Some(n) if n > 0 => ok(format!("Removes {} (no name or tag). Images with a tag stay.", plural(n, "untagged image", "untagged images"))),
            _ => ok("Only untagged (dangling) images are removed; images with a tag stay."),
        },
        ok("Volumes are not touched, so databases and other volume data stay."),
        ok("Running containers are not touched."),
        warn("Unused build cache goes too, so the next image build is slower once."),
        warn("The space is freed inside Docker's disk image first; the image file on your drive may not shrink until Docker Desktop compacts it."),
    ];
    let mut with_volumes = vec![
        bad(match state.and_then(|s| s.unused_volumes.clone()) {
            Some(v) if !v.is_empty() => {
                let shown: Vec<&str> = v.iter().take(NAME_LIMIT).map(String::as_str).collect();
                format!(
                    "Deletes volumes no container uses right now: {}{}. A stopped database keeps its data in a volume.",
                    shown.join(", "),
                    if v.len() > NAME_LIMIT { format!(" and {} more", v.len() - NAME_LIMIT) } else { String::new() }
                )
            }
            _ => "Deletes volumes no container uses right now. A stopped database keeps its data in a volume.".to_string(),
        }),
        bad("Permanent: volume data exists nowhere else, and nothing brings it back."),
        warn("Newer Docker versions keep named volumes with this option and older ones delete them too; Jharu cannot tell which of yours hold data you care about."),
    ];
    with_volumes.extend(plain.clone());

    vec![
        SpaceAction {
            id: "docker-prune".into(),
            label: "Prune unused Docker data".into(),
            commands: vec!["docker system prune -f".into()],
            cost: "Stopped containers, unused networks, untagged images and unused build cache are removed. Volumes are not included.".into(),
            safety: Safety::Caution,
            reasons: plain,
            admin: false,
            permanent: true,
            undo: Some("Images can be pulled or built again; data that lived only inside a removed container cannot come back.".into()),
            slow: false,
            cancellable: true,
            est_bytes: state.map(|_| est),
            est_note: state.map(|_| "At most; Docker reports the exact amount afterwards.".into()),
            enabled: !gone,
            disabled_reason: off(),
        },
        SpaceAction {
            id: "docker-prune-volumes".into(),
            label: "Prune unused Docker data and volumes".into(),
            commands: vec!["docker system prune -f --volumes".into()],
            cost: "Everything above, and the volumes no container uses. Volume data cannot be recovered.".into(),
            safety: Safety::Risky,
            reasons: with_volumes,
            admin: false,
            permanent: true,
            undo: None,
            slow: false,
            cancellable: true,
            est_bytes: state.map(|_| est + volumes.reclaimable_bytes),
            est_note: state.map(|_| "At most; Docker reports the exact amount afterwards.".into()),
            enabled: !gone,
            disabled_reason: off(),
        },
    ]
}

// ---------------------------------------------------------------- scan

pub fn scan() -> SpaceReport {
    // Docker and PowerShell each take a second or so; ask them together.
    let (docker, mut items) = std::thread::scope(|s| {
        let docker = s.spawn(docker_item);
        #[allow(unused_mut)]
        let mut items: Vec<SpaceItem> = vec![];
        #[cfg(target_os = "windows")]
        {
            let page = s.spawn(win::pagefile_item);
            items.push(win::hibernation_item());
            items.push(win::restore_item());
            items.push(win::store_item());
            if let Ok(p) = page.join() {
                items.push(p);
            }
        }
        #[cfg(target_os = "macos")]
        {
            items.push(mac::tm_item());
            items.push(mac::sleepimage_item());
        }
        (docker.join().ok().flatten(), items)
    });
    items.extend(docker);
    SpaceReport {
        platform: if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "other"
        },
        is_admin: cfg!(windows) && crate::cleaning::is_elevated(),
        items,
    }
}

/// Looks at something that needs administrator rights, on request.
fn inspect(item_id: &str, cancel: &AtomicBool) -> InspectOutcome {
    #[cfg(target_os = "windows")]
    {
        let action = match item_id {
            "restore-points" => AdminAction::ListRestorePoints,
            "winsxs" => AdminAction::AnalyzeComponentStore,
            _ => return InspectOutcome { error: Some(format!("Nothing to check for {item_id}")), ..Default::default() },
        };
        return match win::run_admin(action, None, Some(cancel)) {
            Err(win::AdminError::Declined) => InspectOutcome { admin_declined: true, ..Default::default() },
            Err(win::AdminError::Failed(e)) => InspectOutcome { error: Some(e), ..Default::default() },
            Ok(r) => {
                if let Ok(mut l) = looked().lock() {
                    if let Some(s) = r.restore.clone() {
                        l.restore = Some((s, now_secs()));
                    }
                    if let Some(s) = r.store.clone() {
                        l.store = Some((s, now_secs()));
                    }
                }
                let output = tail(&r.log.iter().map(|l| format!("> {}\n{}", l.command, l.output)).collect::<Vec<_>>().join("\n"));
                let error = (!r.ok).then(|| {
                    if r.errors.is_empty() {
                        "The command did not succeed.".to_string()
                    } else {
                        tail(&r.errors.join("; "))
                    }
                });
                InspectOutcome { admin_declined: false, error, output }
            }
        };
    }
    #[allow(unreachable_code)]
    {
        let _ = cancel;
        InspectOutcome { error: Some(format!("Nothing to check for {item_id} on this system")), ..Default::default() }
    }
}

// ---------------------------------------------------------------- running an action

struct Ctx<'a> {
    progress: &'a dyn Fn(&str),
    cancel: &'a AtomicBool,
}

fn skipped(id: &str, why: impl Into<String>) -> RunOutcome {
    RunOutcome { action_id: id.into(), skipped: Some(why.into()), ..Default::default() }
}

fn run_docker(id: &str, volumes: bool, ctx: &Ctx) -> RunOutcome {
    let Some(bin) = docker_binary() else { return skipped(id, "The Docker command line client is no longer installed.") };
    // State can change after the scan; look again now.
    if let Err(why) = docker_state(&bin) {
        return skipped(id, format!("Docker is not answering ({why}), so nothing was removed."));
    }
    (ctx.progress)("Running docker system prune…");
    let args: Vec<&str> = if volumes { vec!["system", "prune", "-f", "--volumes"] } else { vec!["system", "prune", "-f"] };
    let mut out = RunOutcome { action_id: id.into(), ..Default::default() };
    match docker_cmd(&bin, &args, None, Some(ctx.cancel)) {
        Err(e) => out.errors.push(format!("Could not run docker: {e}")),
        Ok(o) if o.cancelled => {
            out.cancelled = true;
            out.message = "Stopped waiting for Docker. It may finish what it had started; rescan to see.".into();
            out.output = tail(&o.combined());
        }
        Ok(o) => {
            out.output = tail(&o.combined());
            out.ok = o.success();
            out.freed_bytes = parse_reclaimed(&o.stdout);
            if out.ok {
                out.message = match out.freed_bytes {
                    Some(b) => format!("Docker reports {} reclaimed inside its disk image.", human(b)),
                    None => "Docker finished.".into(),
                };
            } else {
                out.errors.push(format!("docker system prune failed: {}", out.output));
            }
        }
    }
    out
}

#[cfg(target_os = "windows")]
fn run_windows(id: &str, ctx: &Ctx) -> RunOutcome {
    use win::AdminError;
    let Some(action) = AdminAction::from_id(id) else { return skipped(id, "Unknown action.") };
    let mut out = RunOutcome { action_id: id.into(), ..Default::default() };

    // For the restore point deletions: the point the user was shown.
    let mut point: Option<String> = None;

    // Re-check, now, what the scan saw.
    let hiber_before = win::hiberfile();
    let hiber_size = std::fs::metadata(&hiber_before).ok().map(|m| m.len());
    match action {
        AdminAction::HibernateOff if hiber_size.is_none() => {
            return skipped(id, "Hibernation is already off: the file no longer exists.");
        }
        AdminAction::HibernateReduced => {
            if hiber_size.is_none() {
                return skipped(id, "Hibernation is off: there is no file to shrink.");
            }
            let pct = win::hiber_percent_now();
            if win::hiber_mode(pct) == win::HiberMode::Reduced {
                return skipped(id, "The hibernation file is already the reduced size.");
            }
        }
        AdminAction::RestoreDeleteOldest | AdminAction::RestoreKeepNewest => {
            let known = looked().lock().ok().and_then(|l| l.restore.clone());
            match known {
                None => return skipped(id, "Check the restore points first, so you can see what would be deleted."),
                Some((s, _)) if victims(action, &s.points).is_empty() => {
                    return skipped(id, "There is nothing to delete: the list has no restore point outside the one that stays.");
                }
                Some((s, _)) => {
                    // The helper reads the list again and deletes only if this
                    // point is still the oldest (or the newest) there.
                    point = shown_point(action, &s.points).map(|p| p.id.clone());
                }
            }
        }
        AdminAction::ComponentCleanup => {
            let busy = crate::safety::Facts::gather().running;
            let running: Vec<&str> = ["tiworker", "trustedinstaller", "wusa"].into_iter().filter(|n| busy.contains(*n)).collect();
            if !running.is_empty() {
                return skipped(
                    id,
                    format!(
                        "Windows is installing or servicing updates right now ({} is running). Try again when it has finished.",
                        running.join(", ")
                    ),
                );
            }
            if looked().lock().ok().and_then(|l| l.store.clone()).is_none() {
                return skipped(id, "Analyze the component store first, so you can see whether a cleanup is worth it.");
            }
        }
        AdminAction::ListRestorePoints | AdminAction::AnalyzeComponentStore | AdminAction::HibernateOff => {}
    }

    (ctx.progress)(if crate::cleaning::is_elevated() {
        "Running the Windows command…"
    } else {
        "Waiting for the Windows permission prompt…"
    });
    // Only the read-only look may be stopped: a half-run DISM or powercfg is
    // not something Jharu does to the system, in this process or the helper.
    let stoppable = matches!(action, AdminAction::ListRestorePoints | AdminAction::AnalyzeComponentStore);
    let result = match win::run_admin(action, point.as_deref(), stoppable.then_some(ctx.cancel)) {
        Err(AdminError::Declined) => {
            out.admin_declined = true;
            out.message = "You declined the Windows permission prompt, so nothing was changed.".into();
            return out;
        }
        Err(AdminError::Failed(e)) => {
            out.errors.push(e);
            return out;
        }
        Ok(r) => r,
    };
    out.output = tail(&result.log.iter().map(|l| format!("> {}\n{}", l.command, l.output)).collect::<Vec<_>>().join("\n"));
    out.errors.extend(result.errors.iter().cloned());
    if let Some(why) = &result.skipped {
        out.skipped = Some(why.clone());
    }
    out.ok = result.ok;

    match action {
        AdminAction::HibernateOff | AdminAction::HibernateReduced => {
            let after = std::fs::metadata(&hiber_before).ok().map(|m| m.len());
            let before = hiber_size.unwrap_or(0);
            if result.ok {
                let freed = before.saturating_sub(after.unwrap_or(0));
                out.freed_bytes = Some(freed);
                out.message = match action {
                    AdminAction::HibernateOff => format!("Hibernation is off; {} freed.", human(freed)),
                    _ => format!("The hibernation file was reduced; {} freed.", human(freed)),
                };
            }
        }
        AdminAction::RestoreDeleteOldest | AdminAction::RestoreKeepNewest => {
            let previous = looked().lock().ok().and_then(|l| l.restore.clone()).map(|(s, _)| s);
            if let Some(after) = result.restore.clone() {
                if result.skipped.is_none() {
                    if let (Some(b), Some(a)) = (previous.as_ref().and_then(|p| p.used), after.used) {
                        out.freed_bytes = Some(b.saturating_sub(a));
                    }
                }
                if let Ok(mut l) = looked().lock() {
                    l.restore = Some((after, now_secs()));
                }
            }
            if result.skipped.is_none() {
                out.message = format!(
                    "Deleted {}.{}",
                    plural(result.deleted as usize, "restore point", "restore points"),
                    out.freed_bytes.map(|b| format!(" Shadow storage in use dropped by {}.", human(b))).unwrap_or_default()
                );
            }
        }
        AdminAction::ComponentCleanup => {
            let previous = looked().lock().ok().and_then(|l| l.store.clone()).map(|(s, _)| s);
            if let Some(after) = result.store.clone() {
                if let (Some(b), Some(a)) = (previous.as_ref().and_then(|p| p.actual_bytes), after.actual_bytes) {
                    out.freed_bytes = Some(b.saturating_sub(a));
                }
                if let Ok(mut l) = looked().lock() {
                    l.store = Some((after, now_secs()));
                }
            }
            if result.ok {
                out.message = match out.freed_bytes {
                    Some(b) => format!("Component store cleaned up; its actual size dropped by {}.", human(b)),
                    None => "Component store cleaned up. Windows did not report a new size.".into(),
                };
            }
        }
        AdminAction::ListRestorePoints | AdminAction::AnalyzeComponentStore => {}
    }
    if !out.ok && out.errors.is_empty() && out.skipped.is_none() {
        out.errors.push("The Windows command did not succeed. Its output is shown below.".into());
    }
    out
}

#[cfg(target_os = "macos")]
fn run_macos(id: &str, ctx: &Ctx) -> RunOutcome {
    let mut out = RunOutcome { action_id: id.into(), ..Default::default() };
    // Re-list, and delete only what is there now.
    let dates = match mac::snapshots() {
        Ok(d) => d,
        Err(e) => {
            out.errors.push(format!("Could not list the snapshots: {e}"));
            return out;
        }
    };
    let chosen: Vec<String> = match id {
        "tm-delete-oldest" => dates.iter().take(1).cloned().collect(),
        "tm-delete-all" => dates.clone(),
        _ => return skipped(id, "Unknown action."),
    };
    if chosen.is_empty() {
        return skipped(id, "There are no local snapshots any more.");
    }
    (ctx.progress)("Waiting for the macOS password prompt…");
    match mac::delete_snapshots(&chosen) {
        Err(e) => out.errors.push(e),
        Ok(o) if o.stderr.contains("User canceled") || o.stderr.contains("-128") => {
            out.admin_declined = true;
            out.message = "You declined the password prompt, so nothing was changed.".into();
        }
        Ok(o) => {
            out.output = tail(&o.combined());
            let left = mac::snapshots().map(|d| chosen.iter().filter(|c| d.contains(c)).count()).unwrap_or(0);
            out.ok = o.success() && left == 0;
            out.message = format!("Deleted {}.", plural(chosen.len() - left, "local snapshot", "local snapshots"));
            if !out.ok {
                out.errors.push(format!("{left} snapshot(s) are still there: {}", out.output));
            }
        }
    }
    out
}

fn run_action(id: &str, ctx: &Ctx) -> RunOutcome {
    if ctx.cancel.load(Ordering::Relaxed) {
        return RunOutcome { action_id: id.into(), cancelled: true, message: "Cancelled before anything ran.".into(), ..Default::default() };
    }
    match id {
        "docker-prune" => run_docker(id, false, ctx),
        "docker-prune-volumes" => run_docker(id, true, ctx),
        #[cfg(target_os = "windows")]
        _ => run_windows(id, ctx),
        #[cfg(target_os = "macos")]
        _ => run_macos(id, ctx),
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        _ => skipped(id, "This action is not available on this system."),
    }
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn system_space_scan() -> Result<SpaceReport, String> {
    tauri::async_runtime::spawn_blocking(scan).await.map_err(|e| e.to_string())
}

/// Read-only look at something that needs administrator rights.
#[tauri::command]
pub async fn system_space_inspect(app: tauri::AppHandle, item_id: String, token: String) -> Result<InspectOutcome, String> {
    let cancel = register_cancel(&token);
    let t = token.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _ = app.emit("system-space-progress", Progress { token: t, stage: "Looking…".into() });
        inspect(&item_id, &cancel)
    })
    .await
    .map_err(|e| e.to_string());
    unregister_cancel(&token);
    result
}

/// Runs one action from the fixed list, after checking it again.
#[tauri::command]
pub async fn system_space_run(app: tauri::AppHandle, action_id: String, token: String) -> Result<RunOutcome, String> {
    let cancel = register_cancel(&token);
    let t = token.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let progress = |stage: &str| {
            let _ = app.emit("system-space-progress", Progress { token: t.clone(), stage: stage.to_string() });
        };
        run_action(&action_id, &Ctx { progress: &progress, cancel: &cancel })
    })
    .await
    .map_err(|e| e.to_string());
    unregister_cancel(&token);
    result
}

/// Stops a running action that can be stopped (Docker). Windows commands
/// run to their end.
#[tauri::command]
pub fn system_space_cancel(token: String) {
    if let Ok(m) = cancel_flags().lock() {
        if let Some(flag) = m.get(&token) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hibernate_mode_says_what_it_means() {
        assert_eq!(hibernate_mode_text(" hibernatemode        3").as_deref(), Some("hibernatemode 3: memory stays powered and is also written to disk (the default on laptops)"));
        assert_eq!(hibernate_mode_text("hibernatemode 7").as_deref(), Some("hibernatemode 7"));
        assert_eq!(hibernate_mode_text(" hibernatefile        /var/vm/sleepimage"), None);
    }

    const DOCKER_DF: &str = r#"{"Active":"2","Reclaimable":"1.234GB (45%)","Size":"2.7GB","TotalCount":"5","Type":"Images"}
{"Active":"1","Reclaimable":"12.3kB (100%)","Size":"12.3kB","TotalCount":"3","Type":"Containers"}
{"Active":"0","Reclaimable":"512MB (100%)","Size":"512MB","TotalCount":"2","Type":"Local Volumes"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Build Cache"}
"#;

    const DISM: &str = "\r\nDeployment Image Servicing and Management tool\r\nVersion: 10.0.26100.1150\r\n\r\nImage Version: 10.0.26100.4349\r\n\r\n[===========================99.0%==========================] \r\n\r\nComponent Store (WinSxS) information:\r\n\r\nWindows Explorer Reported Size of Component Store : 9.74 GB\r\n\r\nActual Size of Component Store : 9.38 GB\r\n\r\n    Shared with Windows : 5.89 GB\r\n    Backups and Disabled Features : 3.07 GB\r\n    Cache and Temporary Data : 427.45 MB\r\n\r\nDate of Last Cleanup : 2026-10-20 12:32:07\r\n\r\nNumber of Reclaimable Packages : 3\r\nComponent Store Cleanup Recommended : Yes\r\n\r\nThe operation completed successfully.\r\n";

    const TMUTIL: &str = "Snapshots for volume group containing disk /:\ncom.apple.TimeMachine.2026-10-28-101530.local\ncom.apple.TimeMachine.2026-10-27-221004.local\ncom.apple.TimeMachine.2018-01-05-123456\nnot a snapshot line\n";

    #[test]
    fn docker_sizes_use_decimal_units() {
        assert_eq!(parse_docker_size("0B"), Some(0));
        assert_eq!(parse_docker_size("12.3kB"), Some(12_300));
        assert_eq!(parse_docker_size("1.234GB (45%)"), Some(1_234_000_000));
        assert_eq!(parse_docker_size("512MB"), Some(512_000_000));
        assert_eq!(parse_docker_size("garbage"), None);
    }

    #[test]
    fn docker_df_json_lines() {
        let rows = parse_docker_df(DOCKER_DF);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            DockerRow { kind: "Images".into(), total: 5, active: 2, size_bytes: 2_700_000_000, reclaimable_bytes: 1_234_000_000 }
        );
        assert_eq!(rows[2].kind, "Local Volumes");
        assert_eq!(rows[2].reclaimable_bytes, 512_000_000);
        // Plain error text from a stopped daemon yields nothing, not a panic.
        assert!(parse_docker_df("failed to connect to the docker API at npipe:////./pipe/x").is_empty());
    }

    #[test]
    fn prune_total_is_read_from_the_last_line() {
        let out = "Deleted Containers:\nabc\n\nTotal reclaimed space: 1.5GB\n";
        assert_eq!(parse_reclaimed(out), Some(1_500_000_000));
        assert_eq!(parse_reclaimed("Total reclaimed space: 0B"), Some(0));
        assert_eq!(parse_reclaimed("nothing here"), None);
    }

    #[test]
    fn dism_analysis_is_parsed_by_label() {
        let s = parse_dism_analysis(DISM).expect("report");
        assert_eq!(s.explorer_bytes, parse_binary_size("9.74 GB"));
        assert_eq!(s.actual_bytes, Some((9.38 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(s.shared_bytes, parse_binary_size("5.89 GB"));
        assert_eq!(s.backups_bytes, parse_binary_size("3.07 GB"));
        assert_eq!(s.cache_bytes, parse_binary_size("427.45 MB"));
        assert_eq!(s.last_cleanup.as_deref(), Some("2026-10-20 12:32:07"));
        assert_eq!(s.reclaimable_packages, Some(3));
        assert_eq!(s.cleanup_recommended, Some(true));

        let no = DISM.replace("Recommended : Yes", "Recommended : No");
        assert_eq!(parse_dism_analysis(&no).unwrap().cleanup_recommended, Some(false));
        // Another language: the labels are not found, and that is said, not guessed.
        assert_eq!(parse_dism_analysis("Taille réelle du magasin de composants : 9,38 Go"), None);
        assert_eq!(parse_dism_analysis(""), None);
    }

    #[test]
    fn restore_points_json_sorted_and_unbounded_limit_dropped() {
        let one = r#"{"points":[{"id":"{AAAAAAAA-0000-0000-0000-000000000002}","created":2000},{"id":"{AAAAAAAA-0000-0000-0000-000000000001}","created":1000}],"storage":[{"used":"4831838208","allocated":"5000000000","max":"18446744073709551615"}]}"#;
        let s = parse_restore_json(one).unwrap();
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.points[0].created, 1000, "oldest first");
        assert_eq!(s.used, Some(4_831_838_208));
        assert_eq!(s.max, None, "unbounded");

        // PowerShell collapses single-element lists in some versions.
        let single = r#"{"points":{"id":"{AAAAAAAA-0000-0000-0000-000000000001}","created":5},"storage":{"used":"10","allocated":"20","max":"100"}}"#;
        let s = parse_restore_json(single).unwrap();
        assert_eq!((s.points.len(), s.used, s.allocated, s.max), (1, Some(10), Some(20), Some(100)));

        let none = parse_restore_json("\u{feff}{\"points\":[],\"storage\":[]}").unwrap();
        assert!(none.points.is_empty() && none.used.is_none());
        assert!(parse_restore_json("Access is denied").is_err());
    }

    #[test]
    fn time_machine_snapshots() {
        let dates = parse_tm_snapshots(TMUTIL);
        assert_eq!(dates, vec!["2018-01-05-123456", "2026-10-27-221004", "2026-10-28-101530"]);
        assert!(valid_tm_date("2026-10-28-101530"));
        assert!(!valid_tm_date("2026-10-28-101530; rm -rf /"));
        assert!(!valid_tm_date("../etc"));
    }

    #[test]
    fn shadow_ids_must_be_plain_guids() {
        assert!(valid_shadow_id("{3D6A7F2E-0000-4C1B-8A00-1234567890AB}"));
        assert!(!valid_shadow_id("{3D6A7F2E-0000-4C1B-8A00-1234567890AB} & calc"));
        assert!(!valid_shadow_id("3D6A7F2E-0000-4C1B-8A00-1234567890AB"));
        assert!(!valid_shadow_id("{3D6A7F2E-0000-4C1B-8A00-1234567890A}"));
        assert!(!valid_shadow_id("{3D6A7F2E_0000-4C1B-8A00-1234567890AB}"));
        assert!(!valid_shadow_id(""));
    }

    fn points(n: u64) -> Vec<RestorePoint> {
        (1..=n).map(|i| RestorePoint { id: format!("{{AAAAAAAA-0000-0000-0000-{i:012}}}"), created: i * 1000 }).collect()
    }

    #[test]
    fn restore_actions_pick_the_right_points() {
        let pts = points(4);
        let oldest = victims(AdminAction::RestoreDeleteOldest, &pts);
        assert_eq!(oldest.len(), 1);
        assert_eq!(oldest[0].created, 1000);
        let rest = victims(AdminAction::RestoreKeepNewest, &pts);
        assert_eq!(rest.len(), 3);
        assert!(rest.iter().all(|p| p.created < 4000), "the newest is never chosen");
        assert!(victims(AdminAction::RestoreKeepNewest, &points(1)).is_empty());
        assert!(victims(AdminAction::RestoreDeleteOldest, &[]).is_empty());
        assert!(victims(AdminAction::HibernateOff, &pts).is_empty());

        let steps = delete_steps(&rest).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].display(), "vssadmin delete shadows /shadow={AAAAAAAA-0000-0000-0000-000000000001} /quiet");

        // The point shown is the oldest, or the newest (the one that stays).
        assert_eq!(shown_point(AdminAction::RestoreDeleteOldest, &pts).map(|p| p.created), Some(1000));
        assert_eq!(shown_point(AdminAction::RestoreKeepNewest, &pts).map(|p| p.created), Some(4000));
        assert!(shown_point(AdminAction::HibernateOff, &pts).is_none());
        assert!(shown_point(AdminAction::RestoreKeepNewest, &[]).is_none());

        let evil = [RestorePoint { id: "{x} & del C:\\*".into(), created: 1 }];
        let refs: Vec<&RestorePoint> = evil.iter().collect();
        assert!(delete_steps(&refs).is_err());
    }

    #[test]
    fn helper_only_knows_its_fixed_ids_and_commands() {
        for a in ADMIN_ACTIONS {
            assert_eq!(AdminAction::from_id(a.id()), Some(a));
            for step in fixed_steps(a) {
                assert!(ALLOWED_EXES.contains(&step.exe), "{}", step.display());
                assert!(!step.exe.contains(['\\', '/', ':']), "a bare file name only: {}", step.exe);
            }
        }
        for bad in ["", "powercfg /h off", "hibernate-off & calc", "C:\\Windows\\System32\\cmd.exe", "HIBERNATE-OFF"] {
            assert_eq!(AdminAction::from_id(bad), None, "{bad:?}");
        }
        // The exact commands shown to the user.
        let shown = |a| fixed_steps(a).iter().map(Step::display).collect::<Vec<_>>();
        assert_eq!(shown(AdminAction::HibernateOff), vec!["powercfg /hibernate off"]);
        assert_eq!(shown(AdminAction::HibernateReduced), vec!["powercfg /h /type reduced"]);
        assert_eq!(shown(AdminAction::AnalyzeComponentStore), vec!["DISM /Online /Cleanup-Image /AnalyzeComponentStore"]);
        assert_eq!(shown(AdminAction::ComponentCleanup), vec!["DISM /Online /Cleanup-Image /StartComponentCleanup"]);
        assert!(!shown(AdminAction::ComponentCleanup)[0].contains("ResetBase"));
    }

    #[test]
    fn helper_refuses_anything_else_and_writes_once() {
        let dir = std::env::temp_dir().join(format!("jharu system space {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let result = dir.join("result.json");
        let run = |extra: &[&str]| -> Option<i32> {
            let mut args: Vec<String> = vec!["Jharu.exe".into(), HELPER_FLAG.into(), result.to_string_lossy().into_owned()];
            args.extend(extra.iter().map(|s| s.to_string()));
            helper_main(&args)
        };
        // None of these is a known id, so nothing is executed.
        assert_eq!(run(&["powercfg /hibernate off"]), Some(0));
        let out: HelperResult = serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
        assert!(!out.ok);
        assert_eq!(out.errors.len(), 1);
        assert!(out.log.is_empty() && out.deleted == 0);
        // The file is written once only.
        assert_eq!(run(&["nonsense"]), Some(3));
        // Extra arguments after a valid-looking id are refused too.
        let _ = std::fs::remove_file(&result);
        assert_eq!(run(&["nonsense", "extra"]), Some(0));
        // The restore point deletions need a plain GUID, and the other ids
        // refuse one; none of these runs anything.
        for extra in [
            &["restore-delete-oldest"][..],
            &["restore-keep-newest", "{x} & calc"],
            &["restore-keep-newest", "not-a-guid"],
        ] {
            let _ = std::fs::remove_file(&result);
            assert_eq!(run(extra), Some(0), "{extra:?}");
            let out: HelperResult = serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
            assert!(!out.ok && out.errors.len() == 1 && out.log.is_empty() && out.deleted == 0, "{extra:?}");
        }
        assert!(AdminAction::RestoreDeleteOldest.takes_point() && !AdminAction::HibernateOff.takes_point());
        // Not for this helper at all.
        assert_eq!(helper_main(&["Jharu.exe".into()]), None);
        assert_eq!(helper_main(&["Jharu.exe".into(), "--jharu-elevated-clean".into()]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_sizes() {
        assert_eq!(parse_binary_size("1 KB"), Some(1024));
        assert_eq!(parse_binary_size("427.45 MB"), Some((427.45 * 1048576.0) as u64));
        assert_eq!(parse_binary_size("10 bytes"), Some(10));
        assert_eq!(parse_binary_size("9,38 GB"), Some((9.38 * 1073741824.0) as u64));
        assert_eq!(parse_binary_size("12"), None);
    }

    #[test]
    fn docker_actions_are_rated_and_labelled() {
        let state = DockerState {
            rows: parse_docker_df(DOCKER_DF),
            dangling_images: Some(2),
            stopped: Some(2),
            unused_volumes: Some((1..=9).map(|i| format!("vol{i}")).collect()),
        };
        let a = docker_actions(Some(&state));
        assert_eq!(a[0].commands, vec!["docker system prune -f"]);
        assert_eq!(a[0].safety, Safety::Caution);
        assert!(!a[0].commands[0].contains("--volumes"), "the plain option never touches volumes");
        assert_eq!(a[1].commands, vec!["docker system prune -f --volumes"]);
        assert_eq!(a[1].safety, Safety::Risky);
        assert!(a[1].permanent && a[1].cancellable);
        assert!(a[1].reasons.iter().any(|r| r.level == Level::Bad && r.text.contains("vol1") && r.text.contains("3 more")));
        // Images + containers + cache; the volumes figure only on the second.
        assert_eq!(a[0].est_bytes, Some(1_234_000_000 + 12_300));
        assert_eq!(a[1].est_bytes, Some(1_234_000_000 + 12_300 + 512_000_000));
        // Without a running Docker both are listed but cannot run.
        let off = docker_actions(None);
        assert!(off.iter().all(|a| !a.enabled && a.disabled_reason.is_some()));
        // A count Docker would not give is never reported as "none".
        let unknown = docker_actions(Some(&DockerState { rows: parse_docker_df(DOCKER_DF), dangling_images: None, stopped: None, unused_volumes: None }));
        assert!(!unknown[0].reasons.iter().any(|r| r.text.contains("no stopped containers")));
    }

    #[test]
    fn command_runner_captures_cancels_and_times_out() {
        #[cfg(windows)]
        let shell = |script: &str| {
            let mut c = command(Path::new("powershell.exe"));
            c.args(["-NoProfile", "-Command", script]);
            c
        };
        #[cfg(not(windows))]
        let shell = |script: &str| {
            let mut c = command(Path::new("sh"));
            c.args(["-c", script]);
            c
        };
        let out = run_cmd(shell("echo hello; exit 3"), None, None).unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout.trim(), "hello");
        assert!(!out.success());

        let started = Instant::now();
        let out = run_cmd(shell("sleep 30"), Some(Duration::from_millis(300)), None).unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));

        let flag = AtomicBool::new(true);
        let out = run_cmd(shell("sleep 30"), None, Some(&flag)).unwrap();
        assert!(out.cancelled);
        assert!(run_cmd(command(Path::new("jharu-no-such-program")), None, None).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn hibernation_sizes() {
        use win::{hiber_mode, reduce_estimate, HiberMode};
        assert_eq!(hiber_mode(0), HiberMode::Full);
        assert_eq!(hiber_mode(75), HiberMode::Full);
        assert_eq!(hiber_mode(40), HiberMode::Reduced);
        // Default full file (75% of RAM) shrinks to 40%: saves 8/15 of it.
        assert_eq!(reduce_estimate(15_000, 0), 7_000);
        assert_eq!(reduce_estimate(15_000, 75), 7_000);
        assert_eq!(reduce_estimate(8_000, 40), 0, "already reduced");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_scripts_have_no_double_quotes() {
        // They travel inside one command-line argument.
        assert!(!win::RESTORE_SCRIPT.contains('"'));
    }

    /// Read-only look at this machine:
    /// `cargo test --lib smoke_system_space -- --ignored --nocapture`
    /// Runs only queries (file sizes, registry reads, CIM reads, docker df,
    /// tmutil list); restore points and DISM need administrator rights and
    /// are not asked for here.
    #[test]
    #[ignore]
    fn smoke_system_space() {
        let start = Instant::now();
        let report = scan();
        println!("scanned in {:?} (admin: {})", start.elapsed(), report.is_admin);
        for item in &report.items {
            println!(
                "\n{} [{}] size={:?}{}",
                item.title,
                item.id,
                item.size_bytes.map(human),
                item.size_note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default()
            );
            for f in &item.facts {
                println!("    {}: {}", f.label, f.text);
            }
            if let Some(n) = &item.notice {
                println!("    notice: {n}");
            }
            if let Some(i) = &item.inspect {
                println!("    inspect: {} -> {} (admin {})", i.label, i.command, i.admin);
            }
            for a in &item.actions {
                println!(
                    "    action {:<24} {:?} admin={} permanent={} est={:?} enabled={}",
                    a.id,
                    a.safety,
                    a.admin,
                    a.permanent,
                    a.est_bytes.map(human),
                    a.enabled
                );
                for c in &a.commands {
                    println!("        $ {c}");
                }
            }
        }
    }
}
