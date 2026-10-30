//! Startup apps: what starts by itself when you sign in, how heavy it
//! probably is, and a way to switch it off and on again.
//!
//! This is the first piece of the "speed optimizer", which makes the computer
//! feel faster under the same safety rules as the cleaner.
//!
//! Windows lists the Run keys (user, machine, 32-bit machine; RunOnce is only
//! listed), the user and common Startup folders, and scheduled tasks that
//! start at logon (listed only). Whether an entry is on comes from the same
//! `StartupApproved` flags Task Manager reads and writes, so switching an
//! entry off here shows as Disabled in Task Manager and the other way round.
//! The Run value or shortcut itself is never deleted: undo is always the same
//! flag flipped back.
//!
//! macOS lists LaunchAgents, LaunchDaemons and login items, read-only. That
//! code has not been compiled or run on a Mac.
//!
//! The rating is deliberately narrow, and errs towards "do not touch":
//!
//! - Safe:    the program it starts is gone, so the entry does nothing.
//! - Caution: everything else that may be switched off (updaters and
//!            helpers, Microsoft apps, programs Jharu does not recognise).
//! - Risky:   security software, hardware drivers, Windows and Apple system
//!            components. Jharu refuses to switch these off.
//!
//! Publisher names come from the program's file information, not a verified
//! signature, and a program can claim anything there. That can only make an
//! entry look more protected, never less, so it is safe to rely on.

// The Windows parsers are compiled and tested everywhere, but only called
// on Windows.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::protected::{in_system_dir, normalize, starts_word, HARDWARE_DRIVERS, SECURITY_SOFTWARE, WINDOWS_COMPONENTS};
use crate::rules::Safety;
use crate::safety::{Level, Reason};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
const MB: f64 = 1024.0 * 1024.0;
#[cfg(not(target_os = "windows"))]
const MB: f64 = 1000.0 * 1000.0;

/// Running memory above which a startup program counts as a heavy one.
const HIGH_MEMORY: u64 = 200 * 1024 * 1024;
const MEDIUM_MEMORY: u64 = 50 * 1024 * 1024;
/// A program this big on disk is "medium" even when it is not running now.
const LARGE_PROGRAM: u64 = 150 * 1024 * 1024;

/// The command-line flag that starts Jharu as the elevated startup helper.
pub const HELPER_FLAG: &str = "--jharu-elevated-startup";

// ---------------------------------------------------------------- types

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Run,
    /// The 32-bit view of the machine Run key (WOW6432Node).
    Run32,
    RunOnce,
    StartupFolder,
    ScheduledTask,
    LaunchAgent,
    LaunchDaemon,
    LoginItem,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Run => "Run key",
            Kind::Run32 => "Run key (32-bit)",
            Kind::RunOnce => "RunOnce key",
            Kind::StartupFolder => "Startup folder",
            Kind::ScheduledTask => "Scheduled task at logon",
            Kind::LaunchAgent => "Launch agent",
            Kind::LaunchDaemon => "Launch daemon",
            Kind::LoginItem => "Login item",
        }
    }

    /// Why Jharu cannot switch this kind on or off, or None when it can.
    fn read_only_note(self) -> Option<&'static str> {
        match self {
            Kind::Run | Kind::Run32 | Kind::StartupFolder => None,
            Kind::RunOnce => Some("Windows runs a RunOnce entry once and then removes it, and has no off switch for it. Jharu only lists these."),
            Kind::ScheduledTask => Some("Jharu only lists scheduled tasks. To turn one off, open Task Scheduler."),
            Kind::LaunchAgent | Kind::LaunchDaemon | Kind::LoginItem => {
                Some("Jharu only lists these on macOS. Change them in System Settings, General, Login Items.")
            }
        }
    }

    /// The StartupApproved sub-key that holds this kind's on/off flags.
    fn approved_table(self) -> Option<&'static str> {
        match self {
            Kind::Run => Some("Run"),
            Kind::Run32 => Some("Run32"),
            Kind::StartupFolder => Some("StartupFolder"),
            _ => None,
        }
    }

    /// The short word used in ids and in the elevated helper's arguments.
    fn token(self) -> Option<&'static str> {
        match self {
            Kind::Run => Some("run"),
            Kind::Run32 => Some("run32"),
            Kind::StartupFolder => Some("folder"),
            _ => None,
        }
    }

    fn from_token(t: &str) -> Option<Kind> {
        match t {
            "run" => Some(Kind::Run),
            "run32" => Some(Kind::Run32),
            "folder" => Some(Kind::StartupFolder),
            _ => None,
        }
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Starts for this user only.
    User,
    /// Starts for everyone who signs in on this computer.
    Machine,
}

impl Scope {
    fn token(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Machine => "machine",
        }
    }

    fn from_token(t: &str) -> Option<Scope> {
        match t {
            "user" => Some(Scope::User),
            "machine" => Some(Scope::Machine),
            _ => None,
        }
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ImpactLevel {
    /// Nothing starts: it is switched off, or its program is gone.
    None,
    Low,
    Medium,
    High,
    Unknown,
}

#[derive(Serialize, Clone, Debug)]
pub struct Impact {
    pub level: ImpactLevel,
    pub text: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct StartupEntry {
    /// `<kind>|<scope>|<name>`; switching an entry sends only this back.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// From the program's file information (or the label on macOS); not a
    /// verified signature.
    pub publisher: Option<String>,
    pub command: String,
    pub target: Option<String>,
    /// None when Jharu could not work out which file this starts.
    pub target_exists: Option<bool>,
    pub kind: Kind,
    pub kind_label: String,
    pub scope: Scope,
    pub location: String,
    /// None when Windows holds a state value Jharu does not recognise.
    pub enabled: Option<bool>,
    /// When it was switched off, as Unix seconds, if Windows recorded it.
    pub disabled_at: Option<u64>,
    pub can_toggle: bool,
    pub needs_admin: bool,
    pub read_only_note: Option<String>,
    pub size_bytes: Option<u64>,
    pub running: bool,
    pub memory_bytes: Option<u64>,
    pub impact: Impact,
    pub safety: Safety,
    pub summary: String,
    pub reasons: Vec<Reason>,
}

#[derive(Serialize)]
pub struct StartupReport {
    pub supported: bool,
    /// Jharu itself is running as administrator.
    pub elevated: bool,
    pub entries: Vec<StartupEntry>,
    /// Scheduled tasks under \Microsoft\Windows left out of the list.
    pub hidden_system_tasks: usize,
    pub notes: Vec<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct SkippedItem {
    pub id: String,
    pub name: String,
    pub why: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct StartupOutcome {
    /// Ids whose state was changed.
    pub changed: Vec<String>,
    pub skipped: Vec<SkippedItem>,
    /// Administrator permission was needed and the user declined it.
    pub admin_declined: bool,
    pub errors: Vec<String>,
}

impl StartupOutcome {
    fn merge(&mut self, other: StartupOutcome) {
        self.changed.extend(other.changed);
        self.skipped.extend(other.skipped);
        self.admin_declined |= other.admin_declined;
        self.errors.extend(other.errors);
    }
}

/// An entry as found in its source, before it is rated.
struct Raw {
    id: String,
    name: String,
    kind: Kind,
    scope: Scope,
    command: String,
    location: String,
    enabled: Option<bool>,
    disabled_at: Option<u64>,
    target: Option<PathBuf>,
    target_exists: Option<bool>,
    /// A publisher worked out without reading the file (macOS labels).
    publisher_hint: Option<String>,
}

pub fn make_id(kind: Kind, scope: Scope, name: &str) -> String {
    format!("{}|{}|{}", kind.token().unwrap_or("other"), scope.token(), name)
}

/// The kind, scope and name in an id. The name may itself contain `|`.
pub fn parse_id(id: &str) -> Option<(Kind, Scope, String)> {
    let mut parts = id.splitn(3, '|');
    let kind = Kind::from_token(parts.next()?)?;
    let scope = Scope::from_token(parts.next()?)?;
    let name = parts.next()?;
    (!name.is_empty()).then(|| (kind, scope, name.to_string()))
}

// ---------------------------------------------------- StartupApproved

/// What a StartupApproved value says. The value is 12 bytes: a flag in the
/// first byte, then (when disabled) the Windows FILETIME of the moment it
/// was switched off. Task Manager writes 02 for on and 03 for off; 06 and 07
/// appear for entries an installer approved.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Approved {
    Enabled,
    /// FILETIME of when it was switched off; 0 when not recorded.
    Disabled(u64),
    /// A flag Jharu does not know. Shown as unclear, never guessed at.
    Unknown(u8),
}

pub fn parse_approved(bytes: &[u8]) -> Approved {
    let Some(&flag) = bytes.first() else { return Approved::Unknown(0) };
    match flag {
        0x02 | 0x06 => Approved::Enabled,
        0x03 | 0x07 => {
            let ft = bytes
                .get(4..12)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
                .unwrap_or(0);
            Approved::Disabled(ft)
        }
        other => Approved::Unknown(other),
    }
}

/// The 12 bytes Task Manager writes: 02 and zeros for on, 03 and the
/// FILETIME of now for off.
pub fn approved_bytes(enable: bool, now_filetime: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    if enable {
        b[0] = 0x02;
    } else {
        b[0] = 0x03;
        b[4..12].copy_from_slice(&now_filetime.to_le_bytes());
    }
    b
}

const FILETIME_UNIX_OFFSET: u64 = 11_644_473_600;

fn filetime_to_unix(ft: u64) -> Option<u64> {
    (ft > 0).then(|| (ft / 10_000_000).saturating_sub(FILETIME_UNIX_OFFSET))
}

fn now_filetime() -> u64 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    (secs + FILETIME_UNIX_OFFSET) * 10_000_000
}

fn state_of(a: Option<Approved>) -> (Option<bool>, Option<u64>) {
    match a {
        // No flag at all means Windows has never been told otherwise.
        None | Some(Approved::Enabled) => (Some(true), None),
        Some(Approved::Disabled(ft)) => (Some(false), filetime_to_unix(ft)),
        Some(Approved::Unknown(_)) => (None, None),
    }
}

// ------------------------------------------------- command to program

/// `%NAME%` references replaced from the environment; unknown ones stay.
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || p.starts_with('/')
}

fn is_unc(p: &str) -> bool {
    p.starts_with("\\\\") || p.starts_with("//")
}

const PROGRAM_EXTS: &[&str] = &["exe", "com", "bat", "cmd", "msc", "lnk", "cpl", "jar", "ps1", "vbs", "js", "app"];

fn has_program_ext(p: &str) -> bool {
    Path::new(p)
        .extension()
        .map(|e| PROGRAM_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}

/// The program a command line starts: `(path, exists)`. `exists` is None
/// when the file cannot be worked out (a bare name not found on the path, or
/// a network share, which Jharu does not touch because a dead share can
/// stall a check for a long time).
///
/// An unquoted path may contain spaces (`C:\Program Files\X\x.exe /silent`),
/// so every prefix ending at a space is tried and the first real file wins.
pub fn resolve_command(
    command: &str,
    is_file: &dyn Fn(&Path) -> bool,
    search: &dyn Fn(&str) -> Option<PathBuf>,
) -> (Option<PathBuf>, Option<bool>) {
    let cmd = expand_env(command.trim());
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return (None, None);
    }
    let candidates: Vec<String> = if let Some(rest) = cmd.strip_prefix('"') {
        vec![rest.split('"').next().unwrap_or("").to_string()]
    } else {
        let mut c: Vec<String> = cmd
            .char_indices()
            .filter(|(_, ch)| *ch == ' ')
            .map(|(i, _)| cmd[..i].to_string())
            .collect();
        c.push(cmd.to_string());
        c
    };
    let candidates: Vec<String> = candidates.into_iter().filter(|c| !c.is_empty()).collect();
    if candidates.is_empty() {
        return (None, None);
    }
    if candidates.iter().any(|c| is_unc(c)) {
        return (Some(PathBuf::from(&candidates[0])), None);
    }

    // A bare name such as OneDrive.exe or cmd: look where Windows would.
    if !candidates[0].contains(['\\', '/']) {
        let first = &candidates[0];
        let found = search(first).or_else(|| if has_program_ext(first) { None } else { search(&format!("{first}.exe")) });
        return match found {
            Some(p) => (Some(p), Some(true)),
            None => (Some(PathBuf::from(first)), None),
        };
    }

    for c in &candidates {
        if is_absolute(c) {
            let p = PathBuf::from(c);
            if is_file(&p) {
                return (Some(p), Some(true));
            }
            // A command with no extension may name an .exe.
            if !has_program_ext(c) {
                let with_exe = PathBuf::from(format!("{c}.exe"));
                if is_file(&with_exe) {
                    return (Some(with_exe), Some(true));
                }
            }
        }
    }
    // Nothing exists. Report the longest candidate that looks like a program.
    let shown = candidates
        .iter()
        .rev()
        .find(|c| has_program_ext(c))
        .unwrap_or(&candidates[0])
        .clone();
    let missing = if is_absolute(&shown) { Some(false) } else { None };
    (Some(PathBuf::from(shown)), missing)
}

/// Looks for a bare program name in System32 and on PATH.
fn search_path(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![];
    if let Ok(root) = std::env::var("SystemRoot") {
        dirs.push(Path::new(&root).join("System32"));
        dirs.push(PathBuf::from(root));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Whether Jharu may look at this path: not a network drive (a dead share can
/// stall a check for a long time) and not a drive letter that is not there
/// right now (an unplugged disk is not a deleted program).
#[cfg(target_os = "windows")]
fn drive_is_local(p: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return true;
    }
    let root = wide(&format!("{}:\\", b[0] as char));
    // 0 unknown, 1 no such drive, 4 network: all of them "do not look".
    !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 0 | 1 | 4)
}

#[cfg(not(target_os = "windows"))]
fn drive_is_local(_p: &Path) -> bool {
    true
}

/// Like `resolve_command` against the real disk. "Not there" is only
/// reported when Windows said so (file not found). A permission error, an
/// unplugged or network drive, or any other failure leaves it unknown, so a
/// program Jharu merely could not see is never rated as gone.
fn resolve_on_disk(command: &str) -> (Option<PathBuf>, Option<bool>) {
    let unsure = std::cell::Cell::new(false);
    let is_file = |p: &Path| {
        if !drive_is_local(p) {
            unsure.set(true);
            return false;
        }
        match std::fs::metadata(p) {
            Ok(m) => m.is_file(),
            Err(e) => {
                if !matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput) {
                    unsure.set(true);
                }
                false
            }
        }
    };
    let (target, exists) = resolve_command(command, &is_file, &search_path);
    if exists == Some(false) && unsure.get() {
        return (target, None);
    }
    (target, exists)
}

// ------------------------------------------------------ shortcut files

/// The local path a Windows shortcut (.lnk) points to, read straight from
/// the file. None when the shortcut has no plain path (a store app, a
/// network location).
pub fn parse_lnk(b: &[u8]) -> Option<String> {
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()) as usize);
    if u32_at(0)? != 0x4C {
        return None;
    }
    let flags = u32_at(0x14)?;
    let mut pos = 0x4C;
    if flags & 1 != 0 {
        let size = b.get(pos..pos + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)?;
        pos += 2 + size;
    }
    if flags & 2 == 0 {
        return None;
    }
    let info = b.get(pos..)?;
    let at = |o: usize| info.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()) as usize);
    let header = at(4)?;
    if at(8)? & 1 == 0 {
        return None; // no local base path, only a network one
    }
    if header >= 0x24 {
        let off = at(28)?;
        let units: Vec<u16> = info
            .get(off..)?
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        if !units.is_empty() {
            return Some(String::from_utf16_lossy(&units));
        }
    }
    let off = at(16)?;
    let bytes: Vec<u8> = info.get(off..)?.iter().copied().take_while(|&c| c != 0).collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------- scheduled tasks

/// CSV as `schtasks /fo csv` writes it: quoted fields, doubled quotes, and
/// line breaks inside a quoted field (task comments have them).
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = vec![];
    let mut row: Vec<String> = vec![];
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TaskRow {
    /// Full path, `\Folder\Name`.
    pub path: String,
    pub command: String,
    pub enabled: bool,
    pub run_as: String,
}

#[derive(Debug, Default)]
pub struct TaskParse {
    /// Tasks with a logon trigger, each once.
    pub tasks: Vec<TaskRow>,
    /// Some row had a schedule type Jharu recognises as English. When rows
    /// exist but none do, the listing is probably in another language and
    /// "at logon" cannot be told apart.
    pub english: bool,
}

// Column positions of `schtasks /query /fo csv /v`. The order is the same in
// every language; only the header text and the schedule words change.
const COL_TASK_NAME: usize = 1;
const COL_STATUS: usize = 3;
const COL_COMMAND: usize = 8;
const COL_STATE: usize = 11;
const COL_RUN_AS: usize = 14;
const COL_SCHEDULE_TYPE: usize = 18;
const MIN_COLUMNS: usize = 19;

pub fn parse_schtasks(text: &str) -> TaskParse {
    let mut out = TaskParse::default();
    for row in parse_csv(text) {
        // Verbose output repeats the header for each task folder, and a task
        // with several triggers appears once per trigger.
        if row.len() < MIN_COLUMNS || row[COL_TASK_NAME] == "TaskName" || row[0] == "HostName" {
            continue;
        }
        let schedule = row[COL_SCHEDULE_TYPE].to_lowercase();
        if ["daily", "weekly", "monthly", "one time", "at logon", "at system", "on idle", "on event", "at log on"]
            .iter()
            .any(|w| schedule.contains(w))
        {
            out.english = true;
        }
        if !(schedule.contains("at logon") || schedule.contains("at log on")) {
            continue;
        }
        let path = row[COL_TASK_NAME].clone();
        let state_off = row[COL_STATE].eq_ignore_ascii_case("disabled") || row[COL_STATUS].eq_ignore_ascii_case("disabled");
        if out.tasks.iter().any(|t| t.path == path) {
            continue;
        }
        out.tasks.push(TaskRow {
            path,
            command: row[COL_COMMAND].clone(),
            enabled: !state_off,
            run_as: row[COL_RUN_AS].clone(),
        });
    }
    out
}

/// A task in the \Microsoft\Windows folder: Windows' own housekeeping.
fn is_windows_system_task(path: &str) -> bool {
    path.to_lowercase().starts_with("\\microsoft\\windows\\")
}

// ------------------------------------------------------ macOS plists

/// What a LaunchAgent / LaunchDaemon plist says: (label, program, runs at
/// load, disabled). Parsing works anywhere; only finding the files is macOS.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_launch_plist(bytes: &[u8]) -> Option<LaunchItem> {
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    let dict = value.as_dictionary()?;
    let label = dict.get("Label")?.as_string()?.to_string();
    let args: Vec<String> = dict
        .get("ProgramArguments")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_string().map(String::from)).collect())
        .unwrap_or_default();
    let program = dict
        .get("Program")
        .and_then(|v| v.as_string())
        .map(String::from)
        .or_else(|| args.first().cloned());
    let truthy = |key: &str| match dict.get(key) {
        Some(plist::Value::Boolean(b)) => *b,
        Some(plist::Value::Dictionary(d)) => !d.is_empty(),
        _ => false,
    };
    Some(LaunchItem {
        label,
        command: if args.is_empty() { program.clone().unwrap_or_default() } else { args.join(" ") },
        program,
        starts_at_load: truthy("RunAtLoad") || truthy("KeepAlive"),
        disabled: truthy("Disabled"),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct LaunchItem {
    pub label: String,
    pub command: String,
    pub program: Option<String>,
    pub starts_at_load: bool,
    pub disabled: bool,
}

/// `com.google.keystone.agent` -> "google", the nearest thing to a
/// publisher without reading a code signature.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn publisher_from_label(label: &str) -> Option<String> {
    let mut parts = label.split('.');
    let tld = parts.next()?;
    if !["com", "org", "net", "io", "app", "co"].contains(&tld) {
        return None;
    }
    let owner = parts.next()?;
    Some(owner.to_string())
}

// ------------------------------------------------------- the rating

pub struct Cls<'a> {
    pub name: &'a str,
    pub target: Option<&'a str>,
    pub target_exists: Option<bool>,
    pub publisher: Option<&'a str>,
    pub description: Option<&'a str>,
    pub enabled: Option<bool>,
    pub kind: Kind,
    pub running: bool,
    pub memory: Option<u64>,
}

const UPDATERS_AND_HELPERS: &[&str] = &["update", "updater", "autoupdate", "helper", "keystone", "notifier", "gc invoker", "maintenance"];

/// Folders a legitimate program rarely starts from at sign-in.
const ODD_PLACES: &[&str] = &["\\appdata\\local\\temp\\", "\\windows\\temp\\", "\\downloads\\", "/tmp/", "/downloads/", "/private/var/folders/"];

/// What makes an entry one Jharu will not switch off, if anything.
fn protected_kind(c: &Cls) -> Option<&'static str> {
    let name = normalize(c.name);
    let file = c
        .target
        .and_then(|t| Path::new(t).file_stem().map(|s| normalize(&s.to_string_lossy())))
        .unwrap_or_default();
    let desc = c.description.map(normalize).unwrap_or_default();
    let publisher = c.publisher.map(normalize).unwrap_or_default();
    let target_lower = c.target.map(|t| t.to_lowercase().replace('/', "\\")).unwrap_or_default();
    let raw_target = c.target.unwrap_or("").to_lowercase();
    let hit = |list: &[&str]| {
        list.iter()
            .any(|n| [&name, &file, &desc].iter().any(|h| starts_word(h, n)))
    };
    if hit(SECURITY_SOFTWARE) || starts_word(&publisher, "malwarebytes") || starts_word(&publisher, "kaspersky") {
        return Some("security software");
    }
    if hit(HARDWARE_DRIVERS) || ["realtek", "nvidia", "synaptics"].iter().any(|p| starts_word(&publisher, p)) {
        return Some("a hardware driver");
    }
    if c.kind == Kind::LaunchAgent || c.kind == Kind::LaunchDaemon || c.kind == Kind::LoginItem {
        if normalize(c.name).starts_with("com apple ") || in_system_dir(&raw_target) {
            return Some("an Apple system component");
        }
    }
    if in_system_dir(&target_lower) || in_system_dir(&raw_target) || hit(WINDOWS_COMPONENTS) {
        return Some("a Windows or Apple system component");
    }
    None
}

fn is_updater(c: &Cls) -> bool {
    let name = normalize(c.name);
    let file = c
        .target
        .and_then(|t| Path::new(t).file_stem().map(|s| normalize(&s.to_string_lossy())))
        .unwrap_or_default();
    UPDATERS_AND_HELPERS.iter().any(|n| name.contains(n) || file.contains(n))
}

fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / MB)
}

pub fn classify(c: &Cls) -> (Safety, String, Vec<Reason>) {
    let mut safety = Safety::Safe;
    let summary: String;
    let mut reasons: Vec<Reason> = vec![];
    let mut note = |level: Level, at_least: Safety, text: String| {
        if at_least > safety {
            safety = at_least;
        }
        reasons.push(Reason { level, text });
    };
    let protected = protected_kind(c);

    // 1: does the entry still do anything?
    match c.target_exists {
        Some(false) => note(
            Level::Ok,
            Safety::Safe,
            format!(
                "The program it starts is not there any more ({}), so this entry does nothing and switching it off only tidies the list.",
                c.target.unwrap_or("path unknown")
            ),
        ),
        Some(true) => {}
        None => note(
            Level::Warn,
            Safety::Caution,
            "Jharu could not work out which file this starts, so it cannot tell whether the entry still does anything.".into(),
        ),
    }

    // 2: what is it?
    if let Some(what) = protected {
        note(
            Level::Bad,
            Safety::Risky,
            format!("Looks like {what}. Jharu will not switch it off: it may protect or run this computer."),
        );
        summary = format!("Protected: {what}");
    } else if c.target_exists == Some(false) {
        summary = "Missing target".into();
    } else if is_updater(c) {
        note(
            Level::Warn,
            Safety::Caution,
            "Looks like an updater or helper. Switching it off usually means that program stops checking for updates, or loses a tray feature, until you open it yourself.".into(),
        );
        summary = "Updater or helper".into();
    } else {
        let microsoft = c.publisher.is_some_and(|p| p.to_lowercase().contains("microsoft"));
        let text = if microsoft {
            "A Microsoft program outside the Windows folder, such as OneDrive or Teams. It is optional, but read what it does before turning it off."
        } else {
            "Jharu does not recognise this program. Check the publisher and location, then decide. Switching it off only stops it starting at sign-in."
        };
        note(Level::Warn, Safety::Caution, text.into());
        summary = if microsoft { "Microsoft app".into() } else { "Not recognised".into() };
    }

    // 3: where does it start from?
    if c.target_exists != Some(false) {
        let t = c.target.unwrap_or("").to_lowercase().replace('/', "\\");
        if ODD_PLACES.iter().any(|p| t.contains(&p.replace('/', "\\"))) {
            note(
                Level::Warn,
                Safety::Caution,
                "It starts from a temporary or Downloads folder, which is unusual for software that is meant to stay installed.".into(),
            );
        }
    }

    // 4: state and use.
    if c.enabled == Some(false) {
        note(Level::Ok, Safety::Safe, "Already switched off: it does not start at sign-in. You can switch it back on.".into());
    }
    if c.running {
        let mem = c.memory.map(|m| format!("; its processes use about {}", mb(m))).unwrap_or_default();
        note(
            Level::Ok,
            Safety::Safe,
            format!("Running now{mem}. Switching it off only changes the next sign-in; Jharu does not close it."),
        );
    }
    if c.kind.approved_table().is_some() {
        note(
            Level::Ok,
            Safety::Safe,
            "Reversible: Jharu only flips the on/off flag Task Manager uses. The entry itself is never deleted.".into(),
        );
    }
    drop(note);
    (safety, summary, reasons)
}

/// An honest estimate: what is running now and how big the program is. It is
/// not a measured start-up time, and the wording says so.
pub fn estimate_impact(enabled: Option<bool>, exists: Option<bool>, size: Option<u64>, running: bool, memory: Option<u64>) -> Impact {
    let (level, text) = if enabled == Some(false) {
        (ImpactLevel::None, "Switched off: nothing starts at sign-in.".to_string())
    } else if exists == Some(false) {
        (ImpactLevel::None, "Nothing to start: the program is missing.".to_string())
    } else if exists.is_none() {
        (ImpactLevel::Unknown, "Cannot be estimated: Jharu could not find the program.".to_string())
    } else if running {
        let m = memory.unwrap_or(0);
        let level = if m >= HIGH_MEMORY {
            ImpactLevel::High
        } else if m >= MEDIUM_MEMORY {
            ImpactLevel::Medium
        } else {
            ImpactLevel::Low
        };
        (level, format!("Running now; its processes use about {} of memory.", mb(m)))
    } else if size.is_some_and(|s| s >= LARGE_PROGRAM) {
        (ImpactLevel::Medium, format!("Not running now, but a large program ({}) to load at sign-in.", mb(size.unwrap_or(0))))
    } else {
        (ImpactLevel::Low, "Not running now: it may quit again soon after sign-in or only start on demand.".to_string())
    };
    Impact { level, text: format!("Estimated: {text}") }
}

// -------------------------------------------------------- processes

struct ProcInfo {
    /// Lowercase path with backslashes, when Windows allowed reading it.
    exe: Option<String>,
    name: String,
    memory: u64,
}

pub struct Procs {
    list: Vec<ProcInfo>,
}

fn norm_path(p: &str) -> String {
    p.trim_start_matches("\\\\?\\").replace('/', "\\").to_lowercase()
}

impl Procs {
    fn gather() -> Procs {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always).with_memory(),
        );
        let list = sys
            .processes()
            .values()
            .map(|p| ProcInfo {
                exe: p.exe().map(|e| norm_path(&e.to_string_lossy())),
                name: p.name().to_string_lossy().to_lowercase(),
                memory: p.memory(),
            })
            .collect();
        Procs { list }
    }

    /// Whether the program is running, and the memory of all its processes.
    fn lookup(&self, target: Option<&Path>) -> (bool, Option<u64>) {
        let Some(target) = target else { return (false, None) };
        let want = norm_path(&target.to_string_lossy());
        // Split by hand: the path may use either separator, whatever the host.
        let file = want.rsplit('\\').next().unwrap_or_default().to_string();
        // A macOS login item names the .app bundle, while the running
        // process is the program inside it.
        let bundle = want.ends_with(".app").then(|| format!("{want}\\"));
        let mut found = false;
        let mut total = 0u64;
        for p in &self.list {
            let same = match &p.exe {
                Some(e) => *e == want || bundle.as_ref().is_some_and(|b| e.starts_with(b.as_str())),
                // Windows hides the path of some processes; the name is all
                // there is to go on.
                None => !file.is_empty() && p.name == file,
            };
            if same {
                found = true;
                total += p.memory;
            }
        }
        (found, found.then_some(total))
    }
}

// ---------------------------------------------------- file information

#[cfg(target_os = "windows")]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// (company, description) from the program's version resource.
#[cfg(target_os = "windows")]
pub(crate) fn file_info(path: &Path) -> (Option<String>, Option<String>) {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};

    let w = wide(&path.to_string_lossy());
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(w.as_ptr(), &mut handle);
        if size == 0 {
            return (None, None);
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(w.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
            return (None, None);
        }
        // The version resource is keyed by language and code page.
        let mut langs: Vec<String> = vec![];
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let q = wide("\\VarFileInfo\\Translation");
        if VerQueryValueW(buf.as_ptr() as *const _, q.as_ptr(), &mut p, &mut len) != 0 && len >= 4 && !p.is_null() {
            let bytes = std::slice::from_raw_parts(p as *const u8, len as usize);
            for pair in bytes.chunks_exact(4) {
                langs.push(format!("{:04x}{:04x}", u16::from_le_bytes([pair[0], pair[1]]), u16::from_le_bytes([pair[2], pair[3]])));
            }
        }
        langs.extend(["040904b0".to_string(), "040904e4".to_string(), "000004b0".to_string()]);

        let read = |field: &str| -> Option<String> {
            for lang in &langs {
                let q = wide(&format!("\\StringFileInfo\\{lang}\\{field}"));
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                let mut len = 0u32;
                if VerQueryValueW(buf.as_ptr() as *const _, q.as_ptr(), &mut p, &mut len) != 0 && len > 1 && !p.is_null() {
                    let bytes = std::slice::from_raw_parts(p as *const u8, len as usize * 2);
                    let units: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .take_while(|&u| u != 0)
                        .collect();
                    let text = String::from_utf16_lossy(&units).trim().to_string();
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
            None
        };
        (read("CompanyName"), read("FileDescription"))
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn file_info(_path: &Path) -> (Option<String>, Option<String>) {
    (None, None)
}

// ---------------------------------------------------------- building

fn finish(raw: Raw, procs: &Procs, elevated: bool) -> StartupEntry {
    let on_disk = raw.target.as_ref().filter(|_| raw.target_exists == Some(true));
    let size = on_disk.and_then(|p| std::fs::metadata(p).ok()).filter(|m| m.is_file()).map(|m| m.len());
    let (company, description) = on_disk.map(|p| file_info(p)).unwrap_or((None, None));
    let publisher = company.or(raw.publisher_hint);
    let (running, memory) = procs.lookup(raw.target.as_deref());
    let target = raw.target.as_ref().map(|p| p.to_string_lossy().into_owned());

    let (safety, summary, reasons) = classify(&Cls {
        name: &raw.name,
        target: target.as_deref(),
        target_exists: raw.target_exists,
        publisher: publisher.as_deref(),
        description: description.as_deref(),
        enabled: raw.enabled,
        kind: raw.kind,
        running,
        memory,
    });
    let impact = estimate_impact(raw.enabled, raw.target_exists, size, running, memory);
    let read_only_note = raw.kind.read_only_note().map(String::from);
    StartupEntry {
        id: raw.id,
        name: raw.name,
        description,
        publisher,
        command: raw.command,
        target,
        target_exists: raw.target_exists,
        kind_label: raw.kind.label().into(),
        kind: raw.kind,
        scope: raw.scope,
        location: raw.location,
        enabled: raw.enabled,
        disabled_at: raw.disabled_at,
        can_toggle: read_only_note.is_none(),
        needs_admin: raw.scope == Scope::Machine && !elevated && raw.kind.approved_table().is_some(),
        read_only_note,
        size_bytes: size,
        running,
        memory_bytes: memory,
        impact,
        safety,
        summary,
        reasons,
    }
}

fn impact_rank(l: ImpactLevel) -> u8 {
    match l {
        ImpactLevel::High => 4,
        ImpactLevel::Medium => 3,
        ImpactLevel::Low => 2,
        ImpactLevel::Unknown => 1,
        ImpactLevel::None => 0,
    }
}

pub fn scan() -> StartupReport {
    let procs = Procs::gather();
    let elevated = elevation::is_elevated();
    let mut notes = vec![];
    let mut hidden = 0usize;
    let raws = collect(true, &mut notes, &mut hidden);
    let mut entries: Vec<StartupEntry> = raws.into_iter().map(|r| finish(r, &procs, elevated)).collect();
    // Heaviest first, then by name.
    entries.sort_by(|a, b| {
        impact_rank(b.impact.level)
            .cmp(&impact_rank(a.impact.level))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    StartupReport {
        supported: cfg!(any(target_os = "windows", target_os = "macos")),
        elevated,
        entries,
        hidden_system_tasks: hidden,
        notes,
    }
}

/// Every entry a switch could apply to, found again from scratch. No
/// scheduled tasks: those are never switched, and listing them is slow.
fn switchable_entries() -> Vec<StartupEntry> {
    let procs = Procs::gather();
    let elevated = elevation::is_elevated();
    let (mut notes, mut hidden) = (vec![], 0usize);
    collect(false, &mut notes, &mut hidden)
        .into_iter()
        .map(|r| finish(r, &procs, elevated))
        .collect()
}

fn collect(include_tasks: bool, notes: &mut Vec<String>, hidden: &mut usize) -> Vec<Raw> {
    #[cfg(target_os = "windows")]
    return win::collect(include_tasks, notes, hidden);
    #[cfg(target_os = "macos")]
    return mac::collect(include_tasks, notes, hidden);
    #[allow(unreachable_code)]
    {
        let _ = (include_tasks, notes, hidden);
        vec![]
    }
}

// ------------------------------------------------------ switching

/// Whether `entry` may be switched to `enable` now. The same check runs in
/// the app and again in the elevated helper.
pub fn gate(entry: &StartupEntry, enable: bool) -> Result<(), String> {
    if let Some(why) = &entry.read_only_note {
        return Err(why.clone());
    }
    match (entry.enabled, enable) {
        (Some(true), true) => return Err("It is already on.".into()),
        (Some(false), false) => return Err("It is already off.".into()),
        _ => {}
    }
    // Switching a protected entry back on is allowed (that is undo); only
    // switching it off is refused.
    if !enable && entry.safety == Safety::Risky {
        return Err(format!("Protected: {}. Jharu does not switch these off.", entry.summary.trim_start_matches("Protected: ")));
    }
    Ok(())
}

struct Op {
    kind: Kind,
    scope: Scope,
    name: String,
    enable: bool,
}

fn encode_op(op: &Op) -> String {
    let hex: String = op.name.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{}:{}:{}", if op.enable { "enable" } else { "disable" }, op.kind.token().unwrap_or("none"), hex)
}

/// Strict: exactly `enable|disable`, a switchable kind and hex for the name.
fn decode_op(token: &str) -> Option<Op> {
    let mut parts = token.split(':');
    let enable = match parts.next()? {
        "enable" => true,
        "disable" => false,
        _ => return None,
    };
    let kind = Kind::from_token(parts.next()?)?;
    let hex = parts.next()?;
    if parts.next().is_some() || hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = (0..hex.len()).step_by(2).filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
    let name = String::from_utf8(bytes).ok()?;
    // Only machine-wide entries are ever handed to the elevated helper.
    Some(Op { kind, scope: Scope::Machine, name, enable })
}

/// Finds the entry fresh, runs the gate, then writes the flag. `allow`
/// limits which scope this process may touch.
fn apply(op: &Op, entries: &[StartupEntry], out: &mut StartupOutcome) -> bool {
    let id = make_id(op.kind, op.scope, &op.name);
    let Some(entry) = entries.iter().find(|e| e.id == id) else {
        out.skipped.push(SkippedItem {
            id,
            name: op.name.clone(),
            why: "It no longer exists. It may have been removed since the scan.".into(),
        });
        return false;
    };
    if let Err(why) = gate(entry, op.enable) {
        out.skipped.push(SkippedItem { id, name: entry.name.clone(), why });
        return false;
    }
    match write_flag(op) {
        Ok(()) => {
            out.changed.push(id);
            true
        }
        Err(e) => {
            out.errors.push(format!("{}: {e}", entry.name));
            false
        }
    }
}

#[cfg(target_os = "windows")]
fn write_flag(op: &Op) -> std::io::Result<()> {
    win::write_flag(op)
}

#[cfg(not(target_os = "windows"))]
fn write_flag(_op: &Op) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "switching startup entries is only implemented on Windows"))
}

/// Switches the given entries on or off the way Task Manager does. Each is
/// found again and checked right before it is changed.
pub fn set_enabled(ids: Vec<String>, enable: bool) -> StartupOutcome {
    let mut out = StartupOutcome::default();
    let entries = switchable_entries();
    let elevated = elevation::is_elevated();
    let mut for_helper: Vec<Op> = vec![];
    for id in ids {
        let Some((kind, scope, name)) = parse_id(&id) else {
            out.skipped.push(SkippedItem { name: id.clone(), id, why: "Jharu can only switch Run keys and Startup folder entries.".into() });
            continue;
        };
        let op = Op { kind, scope, name, enable };
        if scope == Scope::Machine && !elevated {
            // Check now so the permission prompt is only shown for entries
            // that will really be changed; the helper checks again.
            let eid = make_id(kind, scope, &op.name);
            match entries.iter().find(|e| e.id == eid) {
                None => out.skipped.push(SkippedItem { id: eid, name: op.name.clone(), why: "It no longer exists. It may have been removed since the scan.".into() }),
                Some(e) => match gate(e, enable) {
                    Ok(()) => for_helper.push(op),
                    Err(why) => out.skipped.push(SkippedItem { id: eid, name: e.name.clone(), why }),
                },
            }
            continue;
        }
        let done = apply(&op, &entries, &mut out);
        if done && scope == Scope::Machine {
            override_for_user(&op);
        }
    }
    if !for_helper.is_empty() {
        let tokens: Vec<String> = for_helper.iter().map(encode_op).collect();
        let mut result = elevation::run_elevated(&tokens);
        // The helper reports ids it changed; carry the per-user override
        // along for those, as that part needs no administrator rights.
        for op in &for_helper {
            if result.changed.contains(&make_id(op.kind, op.scope, &op.name)) {
                override_for_user(op);
            }
        }
        std::mem::swap(&mut out, &mut result);
        out.merge(result);
    }
    out
}

/// A user's own StartupApproved flag for a machine-wide entry beats the
/// machine's, so when one exists it is kept in step.
fn override_for_user(op: &Op) {
    #[cfg(target_os = "windows")]
    win::sync_user_override(op);
    #[cfg(not(target_os = "windows"))]
    let _ = op;
}

#[tauri::command]
pub async fn startup_scan() -> Result<StartupReport, String> {
    tauri::async_runtime::spawn_blocking(scan).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_startup_enabled(ids: Vec<String>, enable: bool) -> Result<StartupOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || set_enabled(ids, enable))
        .await
        .map_err(|e| e.to_string())
}

/// Entry point when Jharu was started as the elevated helper:
/// `Jharu.exe --jharu-elevated-startup <result.json> <op>...`, where each op
/// is `enable|disable:run|run32|folder:<hex of the name>`. The helper only
/// accepts that grammar, only for machine-wide entries that exist right now,
/// and runs the same checks as the app (nothing protected is switched off).
/// Returns the exit code, or None for a normal app start.
pub fn helper_main(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(HELPER_FLAG) {
        return None;
    }
    let Some(result_path) = args.get(2) else { return Some(2) };
    let mut out = StartupOutcome::default();
    let entries = switchable_entries();
    for token in &args[3..] {
        match decode_op(token) {
            Some(op) => {
                apply(&op, &entries, &mut out);
            }
            None => out.errors.push(format!("Refused unknown operation {token}")),
        }
    }
    let json = serde_json::to_vec(&out).unwrap_or_default();
    // create_new refuses to follow a file or link planted at this path, so
    // the elevated process cannot be tricked into overwriting something else.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(result_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &json));
    Some(if written.is_ok() { 0 } else { 3 })
}

// ------------------------------------------------------------ Windows

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use winreg::enums::*;
    use winreg::types::FromRegValue;
    use winreg::{RegKey, RegValue};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
    const RUN_32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
    pub const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

    fn hive(scope: Scope) -> RegKey {
        RegKey::predef(match scope {
            Scope::User => HKEY_CURRENT_USER,
            Scope::Machine => HKEY_LOCAL_MACHINE,
        })
    }

    fn hive_label(scope: Scope) -> &'static str {
        match scope {
            Scope::User => "HKCU",
            Scope::Machine => "HKLM",
        }
    }

    /// The string values of a Run key: (name, command).
    pub fn run_values(key: &RegKey) -> Vec<(String, String)> {
        key.enum_values()
            .flatten()
            .filter(|(name, _)| !name.is_empty())
            .filter_map(|(name, v)| match v.vtype {
                REG_SZ | REG_EXPAND_SZ => String::from_reg_value(&v).ok().map(|s| (name, s)),
                _ => None,
            })
            .collect()
    }

    pub fn read_approved(base: &RegKey, table: &str, name: &str) -> Option<Approved> {
        let key = base.open_subkey_with_flags(table, KEY_READ).ok()?;
        let v = key.get_raw_value(name).ok()?;
        Some(parse_approved(&v.bytes))
    }

    /// Writes the flag exactly as Task Manager does. Only the flag changes.
    pub fn write_approved(base: &RegKey, table: &str, name: &str, enable: bool) -> std::io::Result<()> {
        let (key, _) = base.create_subkey_with_flags(table, KEY_SET_VALUE | KEY_READ)?;
        key.set_raw_value(
            name,
            &RegValue { bytes: approved_bytes(enable, now_filetime()).to_vec(), vtype: REG_BINARY },
        )
    }

    fn approved_base(scope: Scope, write: bool) -> std::io::Result<RegKey> {
        let flags = if write { KEY_READ | KEY_SET_VALUE | KEY_CREATE_SUB_KEY } else { KEY_READ };
        hive(scope).open_subkey_with_flags(APPROVED, flags).or_else(|e| {
            // The key is missing on a profile nothing was ever switched on.
            if write && e.kind() == std::io::ErrorKind::NotFound {
                hive(scope).create_subkey_with_flags(APPROVED, flags).map(|(k, _)| k)
            } else {
                Err(e)
            }
        })
    }

    pub fn write_flag(op: &Op) -> std::io::Result<()> {
        let table = op.kind.approved_table().ok_or_else(|| std::io::Error::other("not a switchable entry"))?;
        let base = approved_base(op.scope, true)?;
        write_approved(&base, table, &op.name, op.enable)
    }

    /// Keeps a user's own flag for a machine-wide entry in step, if it has one.
    pub fn sync_user_override(op: &Op) {
        let Some(table) = op.kind.approved_table() else { return };
        let Ok(base) = approved_base(Scope::User, true) else { return };
        if read_approved(&base, table, &op.name).is_some() {
            let _ = write_approved(&base, table, &op.name, op.enable);
        }
    }

    /// The state that applies: this user's own flag first, then the machine's.
    fn effective(user: Option<&RegKey>, machine: Option<&RegKey>, scope: Scope, table: &str, name: &str) -> Option<Approved> {
        let own = user.and_then(|b| read_approved(b, table, name));
        match scope {
            Scope::User => own,
            Scope::Machine => own.or_else(|| machine.and_then(|b| read_approved(b, table, name))),
        }
    }

    /// Entries of one Run key. Separate from the real hives so a test can
    /// point it at a temporary key.
    pub fn run_entries(
        run: &RegKey,
        kind: Kind,
        scope: Scope,
        location: &str,
        user_approved: Option<&RegKey>,
        machine_approved: Option<&RegKey>,
    ) -> Vec<Raw> {
        let table = kind.approved_table().unwrap_or("Run");
        run_values(run)
            .into_iter()
            .map(|(name, command)| {
                let (enabled, disabled_at) = if kind.approved_table().is_some() {
                    state_of(effective(user_approved, machine_approved, scope, table, &name))
                } else {
                    (Some(true), None)
                };
                let (target, target_exists) = resolve_on_disk(&command);
                Raw {
                    id: make_id(kind, scope, &name),
                    name,
                    kind,
                    scope,
                    command,
                    location: location.to_string(),
                    enabled,
                    disabled_at,
                    target,
                    target_exists,
                    publisher_hint: None,
                }
            })
            .collect()
    }

    pub fn folder_entries(
        dir: &Path,
        scope: Scope,
        user_approved: Option<&RegKey>,
        machine_approved: Option<&RegKey>,
    ) -> Vec<Raw> {
        let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
        rd.flatten()
            .filter(|e| e.path().is_file())
            .filter(|e| !e.file_name().to_string_lossy().eq_ignore_ascii_case("desktop.ini"))
            .map(|e| {
                let path = e.path();
                let file = e.file_name().to_string_lossy().into_owned();
                let is_lnk = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"));
                let (command, target, exists) = if is_lnk {
                    // A real shortcut is a few KB; never read a huge file.
                    let small = std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1024 * 1024);
                    match small.then(|| std::fs::read(&path).ok()).flatten().and_then(|b| parse_lnk(&b)) {
                        Some(t) => {
                            let (target, exists) = resolve_on_disk(&format!("\"{t}\""));
                            (t, target, exists)
                        }
                        // A store app or a network target: nothing to check.
                        None => (path.to_string_lossy().into_owned(), None, None),
                    }
                } else {
                    (path.to_string_lossy().into_owned(), Some(path.clone()), Some(true))
                };
                let (enabled, disabled_at) = state_of(effective(user_approved, machine_approved, scope, "StartupFolder", &file));
                Raw {
                    id: make_id(Kind::StartupFolder, scope, &file),
                    name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| file.clone()),
                    kind: Kind::StartupFolder,
                    scope,
                    command,
                    location: dir.to_string_lossy().into_owned(),
                    enabled,
                    disabled_at,
                    target,
                    target_exists: exists,
                    publisher_hint: None,
                }
            })
            .collect()
    }

    pub fn collect(include_tasks: bool, notes: &mut Vec<String>, hidden: &mut usize) -> Vec<Raw> {
        let mut raws = vec![];
        let user_ap = approved_base(Scope::User, false).ok();
        let machine_ap = approved_base(Scope::Machine, false).ok();
        let (u, m) = (user_ap.as_ref(), machine_ap.as_ref());

        for (scope, path, kind) in [
            (Scope::User, RUN, Kind::Run),
            (Scope::Machine, RUN, Kind::Run),
            (Scope::Machine, RUN_32, Kind::Run32),
            (Scope::User, RUN_ONCE, Kind::RunOnce),
            (Scope::Machine, RUN_ONCE, Kind::RunOnce),
        ] {
            let key = match hive(scope).open_subkey_with_flags(path, KEY_READ) {
                Ok(k) => k,
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        notes.push(format!("Could not read {}\\{path}: {e}", hive_label(scope)));
                    }
                    continue;
                }
            };
            let location = format!("{}\\{}", hive_label(scope), path);
            raws.extend(run_entries(&key, kind, scope, &location, u, m));
        }

        let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
        let user_folder = dirs::data_dir().map(|d| d.join(r"Microsoft\Windows\Start Menu\Programs\Startup"));
        let common_folder = PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
        for (dir, scope) in user_folder.into_iter().map(|d| (d, Scope::User)).chain([(common_folder, Scope::Machine)]) {
            match std::fs::read_dir(&dir) {
                Ok(_) => raws.extend(folder_entries(&dir, scope, u, m)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => notes.push(format!("Could not read the Startup folder {}: {e}", dir.display())),
            }
        }

        if include_tasks {
            raws.extend(scheduled_tasks(notes, hidden));
        }
        raws
    }

    fn scheduled_tasks(notes: &mut Vec<String>, hidden: &mut usize) -> Vec<Raw> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = std::process::Command::new("schtasks")
            .args(["/query", "/fo", "csv", "/v"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let text = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            Ok(_) | Err(_) => {
                notes.push("Scheduled tasks could not be listed (schtasks failed), so tasks that start at sign-in are not shown.".into());
                return vec![];
            }
        };
        let parsed = parse_schtasks(&text);
        if !parsed.english && text.lines().count() > 3 {
            notes.push("Scheduled tasks are recognised by their English schedule names, so tasks that start at sign-in may be missing on a Windows set to another language.".into());
        }
        let mut raws = vec![];
        for t in parsed.tasks {
            if is_windows_system_task(&t.path) {
                *hidden += 1;
                continue;
            }
            let (folder, name) = t.path.rsplit_once('\\').unwrap_or(("", &t.path));
            // schtasks writes in the console code page, which Jharu reads as
            // UTF-8, so a non-ASCII path may be garbled. A garbled path would
            // look like a missing program, so those are left unknown.
            let (target, exists) = if t.command.is_empty() || t.command.eq_ignore_ascii_case("com handler") || t.command == "N/A" {
                (None, None)
            } else if !t.command.is_ascii() {
                (resolve_on_disk(&t.command).0, None)
            } else {
                resolve_on_disk(&t.command)
            };
            raws.push(Raw {
                id: format!("task|machine|{}", t.path),
                name: name.to_string(),
                kind: Kind::ScheduledTask,
                scope: Scope::Machine,
                command: t.command,
                location: if folder.is_empty() { "\\".into() } else { folder.to_string() },
                enabled: Some(t.enabled),
                disabled_at: None,
                target,
                target_exists: exists,
                publisher_hint: None,
            });
        }
        raws
    }
}

// -------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod mac {
    //! Read-only listing. Not compiled or run on a Mac yet.
    use super::*;

    pub fn collect(_include_tasks: bool, notes: &mut Vec<String>, _hidden: &mut usize) -> Vec<Raw> {
        let mut raws = vec![];
        let mut on_demand = 0usize;
        let mut dirs_to_read: Vec<(Kind, Scope, PathBuf)> = vec![
            (Kind::LaunchAgent, Scope::Machine, PathBuf::from("/Library/LaunchAgents")),
            (Kind::LaunchDaemon, Scope::Machine, PathBuf::from("/Library/LaunchDaemons")),
        ];
        if let Some(home) = dirs::home_dir() {
            dirs_to_read.insert(0, (Kind::LaunchAgent, Scope::User, home.join("Library/LaunchAgents")));
        }
        for (kind, scope, dir) in dirs_to_read {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let path = e.path();
                if path.extension().map_or(true, |x| x != "plist") {
                    continue;
                }
                let Some(item) = std::fs::read(&path).ok().and_then(|b| parse_launch_plist(&b)) else { continue };
                if !item.starts_at_load {
                    on_demand += 1;
                    continue;
                }
                let (target, exists) = match &item.program {
                    Some(p) => (Some(PathBuf::from(p)), Some(Path::new(p).exists())),
                    None => (None, None),
                };
                raws.push(Raw {
                    id: format!("plist|{}|{}", scope.token(), path.display()),
                    name: item.label.clone(),
                    kind,
                    scope,
                    command: item.command,
                    location: dir.to_string_lossy().into_owned(),
                    enabled: Some(!item.disabled),
                    disabled_at: None,
                    target,
                    target_exists: exists,
                    publisher_hint: publisher_from_label(&item.label),
                });
            }
        }
        if on_demand > 0 {
            notes.push(format!("{on_demand} launch items that only run on demand, not at sign-in, are left out."));
        }

        // Login items. Asking System Events may show a one-time macOS
        // Automation permission prompt.
        let out = std::process::Command::new("osascript")
            .args(["-e", "tell application \"System Events\" to get the path of every login item"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                for p in String::from_utf8_lossy(&o.stdout).trim().split(", ").filter(|p| !p.is_empty()) {
                    let name = Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string());
                    raws.push(Raw {
                        id: format!("login|user|{p}"),
                        name,
                        kind: Kind::LoginItem,
                        scope: Scope::User,
                        command: p.to_string(),
                        location: "System Settings, General, Login Items".into(),
                        enabled: Some(true),
                        disabled_at: None,
                        target: Some(PathBuf::from(p)),
                        target_exists: Some(Path::new(p).exists()),
                        publisher_hint: None,
                    });
                }
            }
            _ => notes.push("Login items could not be read. macOS may have blocked Jharu from asking System Events.".into()),
        }
        raws
    }
}

// ----------------------------------------------------------- elevation

#[cfg(target_os = "windows")]
mod elevation {
    //! The same relaunch-as-administrator idiom as cleaning::elevation,
    //! kept in this file so the two modules merge independently.
    use super::{StartupOutcome, HELPER_FLAG};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    const ERROR_CANCELLED: u32 = 1223;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn is_elevated() -> bool {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }

    /// Relaunches Jharu elevated to switch the given operations (Windows
    /// shows its permission prompt) and waits for it to finish.
    pub fn run_elevated(ops: &[String]) -> StartupOutcome {
        let mut out = StartupOutcome::default();
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                out.errors.push(format!("Could not locate Jharu to elevate: {e}"));
                return out;
            }
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let result_path = std::env::temp_dir().join(format!("jharu-startup-{}-{nonce}.json", std::process::id()));
        // Ops are lowercase letters, digits and colons, so they need no quoting.
        let params = format!("{HELPER_FLAG} \"{}\" {}", result_path.display(), ops.join(" "));

        let verb = wide(std::ffi::OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let params_w = wide(std::ffi::OsStr::new(&params));
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params_w.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                let code = GetLastError();
                if code == ERROR_CANCELLED {
                    out.admin_declined = true;
                } else {
                    out.errors.push(format!("Could not start the administrator helper (error {code})"));
                }
                return out;
            }
            if info.hProcess.is_null() {
                out.errors.push("The administrator helper did not start".into());
                return out;
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut exit = 0u32;
            GetExitCodeProcess(info.hProcess, &mut exit);
            CloseHandle(info.hProcess);
            if exit != 0 {
                out.errors.push(format!("The administrator helper failed (exit code {exit})"));
            }
        }
        match std::fs::read(&result_path) {
            Ok(bytes) => {
                let _ = std::fs::remove_file(&result_path);
                match serde_json::from_slice::<StartupOutcome>(&bytes) {
                    Ok(o) => out.merge(o),
                    Err(e) => out.errors.push(format!("Unreadable result from the administrator helper: {e}")),
                }
            }
            Err(e) => out.errors.push(format!("No result from the administrator helper: {e}")),
        }
        out
    }
}

#[cfg(not(target_os = "windows"))]
mod elevation {
    use super::StartupOutcome;

    pub fn is_elevated() -> bool {
        true
    }

    pub fn run_elevated(_ops: &[String]) -> StartupOutcome {
        StartupOutcome {
            errors: vec!["Switching startup entries is only implemented on Windows".into()],
            ..Default::default()
        }
    }
}

// -------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn cls<'a>(name: &'a str, target: Option<&'a str>, exists: Option<bool>, publisher: Option<&'a str>) -> Cls<'a> {
        Cls { name, target, target_exists: exists, publisher, description: None, enabled: Some(true), kind: Kind::Run, running: false, memory: None }
    }

    fn rate(c: &Cls) -> Safety {
        classify(c).0
    }

    // ---- StartupApproved layout

    #[test]
    fn approved_layout_matches_what_task_manager_writes() {
        // Captured from a real profile: 03 + FILETIME is off, 02 + zeros is on.
        let off = [0x03, 0, 0, 0, 0x3F, 0x9E, 0x27, 0xFF, 0xB8, 0x1B, 0xDD, 0x01];
        assert_eq!(parse_approved(&off), Approved::Disabled(0x01DD_1BB8_FF27_9E3F));
        let on = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_approved(&on), Approved::Enabled);
        assert_eq!(parse_approved(&[0x06, 0, 0, 0]), Approved::Enabled);
        assert_eq!(parse_approved(&[0x07, 0, 0, 0]), Approved::Disabled(0));
        // A flag nobody has documented is reported, not guessed at.
        assert_eq!(parse_approved(&[0x01, 0, 0, 0]), Approved::Unknown(1));
        assert_eq!(parse_approved(&[]), Approved::Unknown(0));
        // 2024-ish, in Unix seconds, from the captured FILETIME.
        let unix = filetime_to_unix(0x01DD_1BB8_FF27_9E3F).unwrap();
        assert!(unix > 1_700_000_000, "{unix}");
        assert_eq!(filetime_to_unix(0), None);
    }

    #[test]
    fn approved_bytes_round_trip() {
        let on = approved_bytes(true, 123);
        assert_eq!(on, [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let ft = now_filetime();
        let off = approved_bytes(false, ft);
        assert_eq!(off.len(), 12);
        assert_eq!(off[0], 3);
        assert_eq!(parse_approved(&off), Approved::Disabled(ft));
        assert_eq!(state_of(None), (Some(true), None));
        assert_eq!(state_of(Some(Approved::Unknown(1))), (None, None));
    }

    // ---- ids and the helper's operation grammar

    #[test]
    fn ids_and_ops_round_trip() {
        let id = make_id(Kind::Run, Scope::Machine, "Weird|Name \"x\"");
        assert_eq!(parse_id(&id), Some((Kind::Run, Scope::Machine, "Weird|Name \"x\"".to_string())));
        assert_eq!(parse_id("task|machine|\\x"), None, "tasks are never switchable");
        assert_eq!(parse_id("run|everyone|x"), None);

        let op = Op { kind: Kind::StartupFolder, scope: Scope::Machine, name: "Ünï cöde.lnk".into(), enable: false };
        let token = encode_op(&op);
        let back = decode_op(&token).unwrap();
        assert_eq!((back.kind, back.scope, back.name.as_str(), back.enable), (Kind::StartupFolder, Scope::Machine, "Ünï cöde.lnk", false));
        for bad in ["", "delete:run:6162", "enable:runonce:6162", "enable:run:", "enable:run:6", "enable:run:zz", "enable:run:6162:extra", "enable:task:6162"] {
            assert!(decode_op(bad).is_none(), "{bad}");
        }
    }

    // ---- commands to programs

    fn fake_exists(files: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p| files.iter().any(|f| Path::new(f) == p)
    }

    #[test]
    fn resolves_quoted_unquoted_and_missing_commands() {
        let files = &[r"C:\Program Files\Mozilla Firefox\firefox.exe", r"C:\Program Files (x86)\Avro Keyboard\Avro Keyboard.exe"];
        let exists = fake_exists(files);
        let search = |n: &str| (n.eq_ignore_ascii_case("onedrive.exe")).then(|| PathBuf::from(r"C:\Windows\System32\OneDrive.exe"));

        let (t, e) = resolve_command(r#""C:\Program Files\Mozilla Firefox\firefox.exe" -os-autostart"#, &exists, &search);
        assert_eq!((t.unwrap(), e), (PathBuf::from(files[0]), Some(true)));

        // Unquoted, with spaces and arguments.
        let (t, e) = resolve_command(r"C:\Program Files (x86)\Avro Keyboard\Avro Keyboard.exe /min", &exists, &search);
        assert_eq!((t.unwrap(), e), (PathBuf::from(files[1]), Some(true)));

        let (t, e) = resolve_command(r#""C:\Program Files\Gone\gone.exe" --x"#, &exists, &search);
        assert_eq!((t.unwrap(), e), (PathBuf::from(r"C:\Program Files\Gone\gone.exe"), Some(false)));

        let (t, e) = resolve_command(r"C:\Program Files\Gone App\gone.exe /silent", &exists, &search);
        assert_eq!((t.unwrap(), e), (PathBuf::from(r"C:\Program Files\Gone App\gone.exe"), Some(false)));

        // A bare name is looked up; one that is not found is unknown, not broken.
        let (t, e) = resolve_command("OneDrive.exe /background", &exists, &search);
        assert_eq!((t.unwrap(), e), (PathBuf::from(r"C:\Windows\System32\OneDrive.exe"), Some(true)));
        let (_, e) = resolve_command("mystery.exe", &exists, &search);
        assert_eq!(e, None);

        // A network share is never touched.
        let (_, e) = resolve_command(r"\\server\share\tool.exe", &exists, &search);
        assert_eq!(e, None);
        assert_eq!(resolve_command("   ", &exists, &search), (None, None));
    }

    #[test]
    fn expands_environment_variables() {
        std::env::set_var("JHARU_TEST_DIR", r"C:\Some Dir");
        assert_eq!(expand_env(r"%JHARU_TEST_DIR%\app.exe"), r"C:\Some Dir\app.exe");
        assert_eq!(expand_env(r"%JHARU_NOT_SET_ANYWHERE%\x"), r"%JHARU_NOT_SET_ANYWHERE%\x");
        assert_eq!(expand_env("100% sure"), "100% sure");
        assert_eq!(expand_env("%%"), "%%");
    }

    // ---- shortcut files

    /// A minimal .lnk: header, LinkInfo with a local base path in both forms.
    fn sample_lnk(path: &str, unicode: bool) -> Vec<u8> {
        let mut b = vec![0u8; 0x4C];
        b[0] = 0x4C;
        b[0x14] = 0x02; // HasLinkInfo only
        let ansi = format!("{path}\0");
        let uni: Vec<u8> = path.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect();
        let header = if unicode { 0x24usize } else { 0x1C };
        let ansi_off = header;
        let uni_off = header + ansi.len();
        let size = header + ansi.len() + if unicode { uni.len() } else { 0 };
        let mut info = vec![0u8; header];
        info[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        info[4..8].copy_from_slice(&(header as u32).to_le_bytes());
        info[8..12].copy_from_slice(&1u32.to_le_bytes());
        info[16..20].copy_from_slice(&(ansi_off as u32).to_le_bytes());
        if unicode {
            info[28..32].copy_from_slice(&(uni_off as u32).to_le_bytes());
        }
        info.extend(ansi.as_bytes());
        if unicode {
            info.extend(uni);
        }
        b.extend(info);
        b
    }

    #[test]
    fn reads_the_target_of_a_shortcut() {
        let p = r"C:\Program Files\Some App\app.exe";
        assert_eq!(parse_lnk(&sample_lnk(p, false)).as_deref(), Some(p));
        assert_eq!(parse_lnk(&sample_lnk(p, true)).as_deref(), Some(p));
        assert_eq!(parse_lnk(b"not a shortcut"), None);
        assert_eq!(parse_lnk(&[]), None);
        let mut no_info = sample_lnk(p, false);
        no_info[0x14] = 0;
        assert_eq!(parse_lnk(&no_info), None);
    }

    // ---- schtasks

    /// A captured `schtasks /query /fo csv /v` excerpt: the header repeats
    /// per folder, one task has two triggers, one comment spans lines and has
    /// quotes, and one task is disabled.
    const SCHTASKS: &str = r#""HostName","TaskName","Next Run Time","Status","Logon Mode","Last Run Time","Last Result","Author","Task To Run","Start In","Comment","Scheduled Task State","Idle Time","Power Management","Run As User","Delete Task If Not Rescheduled","Stop Task If Runs X Hours and X Mins","Schedule","Schedule Type","Start Time","Start Date","End Date","Days","Months","Repeat: Every","Repeat: Until: Time","Repeat: Until: Duration","Repeat: Stop If Still Running"
"PC","\Adobe Acrobat Update Task","N/A","Ready","Interactive/Background","1/1/2026 9:00:00 AM","0","Adobe","C:\Program Files (x86)\Common Files\Adobe\ARM\1.0\AdobeARM.exe /ClientID","N/A","Keeps Adobe ""Reader"" up to date.
Second line of the comment.","Enabled","Disabled","Stop On Battery Mode","me","Disabled","72:00:00","Scheduling data is not available in this format.","At logon time","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A"
"PC","\Adobe Acrobat Update Task","N/A","Ready","Interactive/Background","1/1/2026 9:00:00 AM","0","Adobe","C:\Program Files (x86)\Common Files\Adobe\ARM\1.0\AdobeARM.exe /ClientID","N/A","","Enabled","Disabled","Stop On Battery Mode","me","Disabled","72:00:00","Scheduling data is not available in this format.","Daily ","9:00:00 AM","1/1/2026","N/A","Every 1 day(s)","N/A","Disabled","Disabled","Disabled","Disabled"
"HostName","TaskName","Next Run Time","Status","Logon Mode","Last Run Time","Last Result","Author","Task To Run","Start In","Comment","Scheduled Task State","Idle Time","Power Management","Run As User","Delete Task If Not Rescheduled","Stop Task If Runs X Hours and X Mins","Schedule","Schedule Type","Start Time","Start Date","End Date","Days","Months","Repeat: Every","Repeat: Until: Time","Repeat: Until: Duration","Repeat: Stop If Still Running"
"PC","\Microsoft\Windows\Shell\FamilySafetyMonitor","N/A","Ready","Interactive/Background","N/A","0","Microsoft","%systemroot%\system32\wsqmcons.exe","N/A","","Enabled","Disabled","Stop On Battery Mode","SYSTEM","Disabled","72:00:00","Scheduling data is not available in this format.","At logon time","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A"
"PC","\Old Updater","N/A","Disabled","Interactive only","N/A","1","me","C:\Gone\upd.exe","N/A","","Disabled","Disabled","Stop On Battery Mode","me","Disabled","72:00:00","Scheduling data is not available in this format.","At logon time","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A"
"PC","\Weekly Thing","N/A","Ready","Interactive only","N/A","0","me","C:\x\w.exe","N/A","","Enabled","Disabled","Stop On Battery Mode","me","Disabled","72:00:00","Scheduling data is not available in this format.","Weekly","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A","N/A"
"#;

    #[test]
    fn parses_captured_schtasks_csv() {
        let rows = parse_csv(SCHTASKS);
        assert!(rows.iter().all(|r| r.len() == 28), "multi-line quoted field stays in one row");
        assert!(rows[1][10].contains("\"Reader\"") && rows[1][10].contains('\n'));

        let parsed = parse_schtasks(SCHTASKS);
        assert!(parsed.english);
        let names: Vec<&str> = parsed.tasks.iter().map(|t| t.path.as_str()).collect();
        // Two rows for the Adobe task collapse to one, the weekly one is not a logon task.
        assert_eq!(names, ["\\Adobe Acrobat Update Task", "\\Microsoft\\Windows\\Shell\\FamilySafetyMonitor", "\\Old Updater"]);
        assert!(parsed.tasks[0].enabled);
        assert!(parsed.tasks[0].command.ends_with("AdobeARM.exe /ClientID"));
        assert!(!parsed.tasks[2].enabled);
        assert_eq!(parsed.tasks[1].run_as, "SYSTEM");
        assert!(is_windows_system_task(&parsed.tasks[1].path));
        assert!(!is_windows_system_task(&parsed.tasks[0].path));
    }

    #[test]
    fn schtasks_in_another_language_is_noticed_not_misread() {
        // German schedule names: nothing matches, and `english` says so.
        let german = SCHTASKS.replace("At logon time", "Bei Anmeldung").replace("Daily ", "Täglich").replace("Weekly", "Wöchentlich");
        let parsed = parse_schtasks(&german);
        assert!(parsed.tasks.is_empty());
        assert!(!parsed.english);
        assert!(parse_schtasks("").tasks.is_empty());
        assert!(parse_schtasks("garbage,not,a,table\n").tasks.is_empty());
    }

    // ---- launch plists (macOS format, parsed anywhere)

    #[test]
    fn parses_launch_plists() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.google.keystone.agent</string>
<key>ProgramArguments</key><array><string>/Library/Google/GoogleSoftwareUpdate/agent</string><string>--run</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>"#;
        let item = parse_launch_plist(xml).unwrap();
        assert_eq!(item.label, "com.google.keystone.agent");
        assert_eq!(item.program.as_deref(), Some("/Library/Google/GoogleSoftwareUpdate/agent"));
        assert_eq!(item.command, "/Library/Google/GoogleSoftwareUpdate/agent --run");
        assert!(item.starts_at_load && !item.disabled);
        assert_eq!(publisher_from_label(&item.label).as_deref(), Some("google"));
        assert_eq!(publisher_from_label("Homebrew"), None);
        assert!(parse_launch_plist(b"not a plist").is_none());
    }

    // ---- the rating

    #[test]
    fn missing_target_is_the_only_safe() {
        let c = cls("Old Tool", Some(r"C:\Program Files\Old\old.exe"), Some(false), Some("Old Corp"));
        let (s, summary, reasons) = classify(&c);
        assert_eq!(s, Safety::Safe);
        assert_eq!(summary, "Missing target");
        assert!(reasons.iter().any(|r| r.level == Level::Ok && r.text.contains("not there any more")));
        // An existing, unknown program is never Safe.
        assert_eq!(rate(&cls("Spotify", Some(r"C:\Users\x\AppData\Roaming\Spotify\Spotify.exe"), Some(true), Some("Spotify AB"))), Safety::Caution);
        // Nor one whose program could not be found.
        assert_eq!(rate(&cls("Mystery", None, None, None)), Safety::Caution);
    }

    #[test]
    fn critical_software_is_risky_even_when_broken() {
        for (name, target, publisher) in [
            ("SecurityHealth", r"C:\Windows\System32\SecurityHealthSystray.exe", Some("Microsoft Corporation")),
            ("Windows Defender", r"C:\Program Files\Windows Defender\MSASCui.exe", None),
            ("RtkAudUService", r"C:\Windows\System32\RtkAudUService64.exe", Some("Realtek Semiconductor")),
            ("NvBackend", r"C:\Program Files (x86)\NVIDIA Corporation\Update Core\NvBackend.exe", Some("NVIDIA Corporation")),
            ("IgfxTray", r"C:\Windows\System32\igfxtray.exe", Some("Intel Corporation")),
            ("AvastUI", r"C:\Program Files\Avast Software\Avast\AvastUI.exe", Some("AVAST Software")),
            ("ESET Security", r"D:\x\egui.exe", None),
            ("Synaptics Pointing Device Driver", r"D:\x\SynTPEnh.exe", None),
            ("Some Tool", r"C:\Windows\System32\helper.exe", None),
        ] {
            let (s, summary, _) = classify(&cls(name, Some(target), Some(true), publisher));
            assert_eq!(s, Safety::Risky, "{name}");
            assert!(summary.starts_with("Protected"), "{name}: {summary}");
        }
        // Missing, but still protected: Jharu does not offer to disable it.
        let (s, _, reasons) = classify(&cls("SecurityHealth", Some(r"C:\Gone\x.exe"), Some(false), None));
        assert_eq!(s, Safety::Risky);
        assert!(reasons.iter().any(|r| r.level == Level::Bad));
    }

    #[test]
    fn word_matching_does_not_protect_lookalikes() {
        for name in ["Reset Tool", "Preset Manager", "Savage Notes", "Davgrid"] {
            let s = rate(&cls(name, Some(r"C:\Apps\x.exe"), Some(true), None));
            assert_eq!(s, Safety::Caution, "{name}");
        }
        assert!(starts_word("eset security", "eset"));
        assert!(!starts_word("preset manager", "eset"));
        assert_eq!(normalize("Intel(R) Graphics  Command"), "intel r graphics command");
    }

    #[test]
    fn updaters_and_helpers_are_caution() {
        for (name, target) in [
            ("GoogleUpdateTaskMachine", r"C:\Program Files (x86)\Google\Update\GoogleUpdate.exe"),
            ("Adobe Acrobat Synchronizer", r"C:\Program Files\Adobe\AdobeCollabSync.exe"),
            ("Steam Client Helper", r"C:\Games\steam.exe"),
            ("Foo", r"C:\Foo\FooUpdater.exe"),
        ] {
            let (s, summary, _) = classify(&cls(name, Some(target), Some(true), None));
            if name == "Adobe Acrobat Synchronizer" {
                assert_eq!(summary, "Not recognised");
            } else {
                assert_eq!(summary, "Updater or helper", "{name}");
            }
            assert_eq!(s, Safety::Caution);
        }
    }

    #[test]
    fn microsoft_apps_outside_windows_are_caution_system_files_are_risky() {
        let (s, summary, _) = classify(&cls("OneDrive", Some(r"C:\Program Files\Microsoft OneDrive\OneDrive.exe"), Some(true), Some("Microsoft Corporation")));
        assert_eq!((s, summary.as_str()), (Safety::Caution, "Microsoft app"));
        assert_eq!(rate(&cls("Whatever", Some(r"c:\windows\system32\whatever.exe"), Some(true), Some("Microsoft Corporation"))), Safety::Risky);
    }

    #[test]
    fn apple_components_are_risky() {
        let mut c = cls("com.apple.metadata.mds", Some("/System/Library/Frameworks/CoreServices.framework/mds"), Some(true), None);
        c.kind = Kind::LaunchDaemon;
        assert_eq!(rate(&c), Safety::Risky);
        let mut c = cls("com.google.keystone.agent", Some("/Library/Google/GoogleSoftwareUpdate/agent"), Some(true), Some("google"));
        c.kind = Kind::LaunchAgent;
        assert_eq!(rate(&c), Safety::Caution);
    }

    #[test]
    fn explains_state_location_and_use() {
        let mut c = cls("Tool", Some(r"C:\Users\x\Downloads\tool.exe"), Some(true), None);
        c.enabled = Some(false);
        c.running = true;
        c.memory = Some(300 * 1024 * 1024);
        let (_, _, reasons) = classify(&c);
        let text: String = reasons.iter().map(|r| r.text.clone()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Downloads folder"));
        assert!(text.contains("Already switched off"));
        assert!(text.contains("Running now") && text.contains("does not close it"));
        assert!(text.contains("never deleted"));
    }

    #[test]
    fn impact_is_an_estimate_and_says_so() {
        let mem = |mb: u64| Some(mb * 1024 * 1024);
        assert_eq!(estimate_impact(Some(false), Some(true), None, true, mem(900)).level, ImpactLevel::None);
        assert_eq!(estimate_impact(Some(true), Some(false), None, false, None).level, ImpactLevel::None);
        assert_eq!(estimate_impact(Some(true), None, None, false, None).level, ImpactLevel::Unknown);
        assert_eq!(estimate_impact(Some(true), Some(true), Some(1), true, mem(400)).level, ImpactLevel::High);
        assert_eq!(estimate_impact(Some(true), Some(true), Some(1), true, mem(80)).level, ImpactLevel::Medium);
        assert_eq!(estimate_impact(Some(true), Some(true), Some(1), true, mem(10)).level, ImpactLevel::Low);
        assert_eq!(estimate_impact(Some(true), Some(true), mem(400), false, None).level, ImpactLevel::Medium);
        assert_eq!(estimate_impact(Some(true), Some(true), mem(2), false, None).level, ImpactLevel::Low);
        assert!(estimate_impact(Some(true), Some(true), None, true, mem(10)).text.starts_with("Estimated"));
    }

    // ---- processes

    #[test]
    fn matches_running_programs_by_path_or_by_name_when_the_path_is_hidden() {
        let procs = Procs {
            list: vec![
                ProcInfo { exe: Some(norm_path(r"C:\Program Files\Chat\chat.exe")), name: "chat.exe".into(), memory: 100 },
                ProcInfo { exe: Some(norm_path(r"C:\Program Files\Chat\chat.exe")), name: "chat.exe".into(), memory: 50 },
                ProcInfo { exe: None, name: "hidden.exe".into(), memory: 7 },
                ProcInfo { exe: Some(norm_path(r"D:\other\sync.exe")), name: "sync.exe".into(), memory: 9 },
            ],
        };
        assert_eq!(procs.lookup(Some(Path::new(r"c:/program files/chat/CHAT.exe"))), (true, Some(150)));
        assert_eq!(procs.lookup(Some(Path::new(r"C:\x\hidden.exe"))), (true, Some(7)));
        // Same file name, different place: not the same program.
        assert_eq!(procs.lookup(Some(Path::new(r"C:\x\sync.exe"))), (false, None));
        assert_eq!(procs.lookup(None), (false, None));
    }

    #[test]
    fn a_login_item_app_bundle_matches_the_program_inside_it() {
        let procs = Procs {
            list: vec![
                ProcInfo { exe: Some(norm_path("/Applications/Chat.app/Contents/MacOS/Chat")), name: "chat".into(), memory: 100 },
                ProcInfo { exe: Some(norm_path("/Applications/Chat.app/Contents/Frameworks/Helper.app/Contents/MacOS/Helper")), name: "helper".into(), memory: 20 },
                ProcInfo { exe: Some(norm_path("/Applications/Chat Beta.app/Contents/MacOS/Chat")), name: "chat".into(), memory: 5 },
            ],
        };
        assert_eq!(procs.lookup(Some(Path::new("/Applications/Chat.app"))), (true, Some(120)));
        assert_eq!(procs.lookup(Some(Path::new("/Applications/Other.app"))), (false, None));
    }

    // ---- the switch

    fn entry(kind: Kind, enabled: Option<bool>, safety: Safety) -> StartupEntry {
        StartupEntry {
            id: make_id(kind, Scope::User, "x"),
            name: "x".into(),
            description: None,
            publisher: None,
            command: String::new(),
            target: None,
            target_exists: Some(true),
            kind,
            kind_label: kind.label().into(),
            scope: Scope::User,
            location: String::new(),
            enabled,
            disabled_at: None,
            can_toggle: kind.read_only_note().is_none(),
            needs_admin: false,
            read_only_note: kind.read_only_note().map(String::from),
            size_bytes: None,
            running: false,
            memory_bytes: None,
            impact: estimate_impact(enabled, Some(true), None, false, None),
            safety,
            summary: if safety == Safety::Risky { "Protected: security software".into() } else { "Not recognised".into() },
            reasons: vec![],
        }
    }

    #[test]
    fn gate_refuses_protected_read_only_and_no_ops() {
        assert!(gate(&entry(Kind::Run, Some(true), Safety::Caution), false).is_ok());
        assert!(gate(&entry(Kind::StartupFolder, Some(false), Safety::Safe), true).is_ok());
        assert!(gate(&entry(Kind::Run, None, Safety::Caution), false).is_ok(), "unclear state can still be switched");
        assert!(gate(&entry(Kind::Run, Some(true), Safety::Caution), true).unwrap_err().contains("already on"));
        assert!(gate(&entry(Kind::Run, Some(false), Safety::Caution), false).unwrap_err().contains("already off"));
        // Protected: cannot be switched off, but can be switched back on (undo).
        let defender = entry(Kind::Run, Some(true), Safety::Risky);
        assert!(gate(&defender, false).unwrap_err().contains("Protected"));
        assert!(gate(&entry(Kind::Run, Some(false), Safety::Risky), true).is_ok());
        assert!(gate(&entry(Kind::ScheduledTask, Some(true), Safety::Safe), false).unwrap_err().contains("Task Scheduler"));
        assert!(gate(&entry(Kind::RunOnce, Some(true), Safety::Safe), false).is_err());
    }

    #[test]
    fn helper_refuses_bad_operations_and_writes_once() {
        let dir = std::env::temp_dir().join(format!("jharu startup helper {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let result = dir.join("result.json");
        // The last op is well formed but names an entry that does not exist,
        // so it is refused after a read-only look; nothing is ever written.
        let ghost = encode_op(&Op { kind: Kind::Run, scope: Scope::Machine, name: format!("JharuTestNoSuchEntry{}", std::process::id()), enable: false });
        let args: Vec<String> = ["Jharu.exe", HELPER_FLAG, &result.to_string_lossy(), "disable:../../etc:6162", "format:run:6162", &ghost]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(helper_main(&args), Some(0));
        let out: StartupOutcome = serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
        assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
        assert!(out.changed.is_empty());
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].why.contains("no longer exists"));
        // A second run must not overwrite an existing file.
        assert_eq!(helper_main(&args), Some(3));
        assert_eq!(helper_main(&["Jharu.exe".to_string()]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the registry, in a throwaway key (never the real Run keys)

    #[cfg(target_os = "windows")]
    mod registry {
        use super::super::win::*;
        use super::super::*;
        use winreg::enums::*;
        use winreg::{RegKey, RegValue};

        /// A scratch tree under HKCU\Software\JharuTest, deleted on drop.
        struct Scratch {
            root: String,
            key: RegKey,
        }

        impl Scratch {
            fn new(tag: &str) -> Scratch {
                let root = format!(r"Software\JharuTest\{tag}-{}", std::process::id());
                let hkcu = RegKey::predef(HKEY_CURRENT_USER);
                let _ = hkcu.delete_subkey_all(&root);
                let (key, _) = hkcu.create_subkey(&root).unwrap();
                Scratch { root, key }
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let hkcu = RegKey::predef(HKEY_CURRENT_USER);
                let _ = hkcu.delete_subkey_all(&self.root);
                // Remove the parent too when nothing else is using it.
                let _ = hkcu.delete_subkey(r"Software\JharuTest");
            }
        }

        fn binary(bytes: &[u8]) -> RegValue {
            RegValue { bytes: bytes.to_vec(), vtype: REG_BINARY }
        }

        #[test]
        fn run_key_listing_state_and_toggle_never_touch_the_run_value() {
            let dir = std::env::temp_dir().join(format!("jharu startup reg {}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let exe = dir.join("real app.exe");
            std::fs::write(&exe, vec![0u8; 2048]).unwrap();

            let s = Scratch::new("run");
            let (run, _) = s.key.create_subkey("Run").unwrap();
            let (approved, _) = s.key.create_subkey("StartupApproved").unwrap();
            run.set_value("Present", &format!("\"{}\" --background", exe.display())).unwrap();
            run.set_value("Gone", &format!("\"{}\"", dir.join("deleted.exe").display())).unwrap();
            run.set_value("Off", &format!("\"{}\"", exe.display())).unwrap();
            run.set_value("Unclear", &exe.to_string_lossy().to_string()).unwrap();
            run.set_raw_value("Binary", &binary(&[1, 2, 3])).unwrap(); // not a string: skipped
            let (table, _) = approved.create_subkey("Run").unwrap();
            table.set_raw_value("Off", &binary(&approved_bytes(false, now_filetime()))).unwrap();
            table.set_raw_value("Unclear", &binary(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).unwrap();

            let mut entries = run_entries(&run, Kind::Run, Scope::User, "scratch", Some(&approved), None);
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let by = |n: &str| entries.iter().find(|e| e.name == n).unwrap();
            assert_eq!(entries.len(), 4, "the binary value is not an entry");
            assert_eq!((by("Present").enabled, by("Present").target_exists), (Some(true), Some(true)));
            assert_eq!(by("Gone").target_exists, Some(false));
            assert_eq!(by("Off").enabled, Some(false));
            assert!(by("Off").disabled_at.is_some());
            assert_eq!(by("Unclear").enabled, None);
            assert_eq!(by("Present").id, "run|user|Present");

            // Switch Present off, as Task Manager would, then on again.
            write_approved(&approved, "Run", "Present", false).unwrap();
            let raw = approved.open_subkey("Run").unwrap().get_raw_value("Present").unwrap();
            assert_eq!(raw.vtype, REG_BINARY);
            assert_eq!(raw.bytes.len(), 12);
            assert_eq!(&raw.bytes[..4], &[3, 0, 0, 0]);
            assert!(matches!(read_approved(&approved, "Run", "Present"), Some(Approved::Disabled(ft)) if ft > 0));
            write_approved(&approved, "Run", "Present", true).unwrap();
            let raw = approved.open_subkey("Run").unwrap().get_raw_value("Present").unwrap();
            assert_eq!(raw.bytes, vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(read_approved(&approved, "Run", "Present"), Some(Approved::Enabled));

            // The Run value itself is untouched by all of it.
            let cmd: String = run.get_value("Present").unwrap();
            assert!(cmd.contains("--background"));
            assert_eq!(run_values(&run).len(), 4);

            // A table that does not exist yet is created on first write.
            write_approved(&approved, "StartupFolder", "thing.lnk", false).unwrap();
            assert!(matches!(read_approved(&approved, "StartupFolder", "thing.lnk"), Some(Approved::Disabled(_))));

            drop(table);
            drop(run);
            drop(approved);
            drop(s);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_users_own_flag_beats_the_machine_flag() {
            let s = Scratch::new("override");
            let (user, _) = s.key.create_subkey("user").unwrap();
            let (machine, _) = s.key.create_subkey("machine").unwrap();
            write_approved(&machine, "Run", "App", false).unwrap();
            // Only the machine says it is off.
            let run = s.key.create_subkey("Run").unwrap().0;
            run.set_value("App", &"C:\\Nowhere\\app.exe").unwrap();
            let e = run_entries(&run, Kind::Run, Scope::Machine, "x", Some(&user), Some(&machine));
            assert_eq!(e[0].enabled, Some(false));
            // The user's own switch-on wins.
            write_approved(&user, "Run", "App", true).unwrap();
            let e = run_entries(&run, Kind::Run, Scope::Machine, "x", Some(&user), Some(&machine));
            assert_eq!(e[0].enabled, Some(true));
            // For a user-scope entry the machine flag is not consulted at all.
            let e = run_entries(&run, Kind::Run, Scope::User, "x", None, Some(&machine));
            assert_eq!(e[0].enabled, Some(true));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_program_on_a_drive_that_is_not_there_is_unknown_not_missing() {
        let letter = ('D'..='Z').rev().find(|c| !drive_is_local(Path::new(&format!(r"{c}:\x.exe"))));
        let Some(letter) = letter else { return };
        let (_, e) = resolve_on_disk(&format!(r#""{letter}:\Tools\tool.exe" --x"#));
        assert_eq!(e, None, "an unplugged or network drive must not read as a deleted program");
        // A real local file is still found, and a real local miss is still a miss.
        let (_, e) = resolve_on_disk(r"C:\Windows\System32\cmd.exe");
        assert_eq!(e, Some(true));
        let (_, e) = resolve_on_disk(r"C:\Windows\JharuNoSuchFolder\nothing.exe");
        assert_eq!(e, Some(false));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn startup_folder_lists_shortcuts_and_files() {
        let dir = std::env::temp_dir().join(format!("jharu startup folder {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("my tool.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        std::fs::write(dir.join("Live.lnk"), sample_lnk(&exe.to_string_lossy(), true)).unwrap();
        std::fs::write(dir.join("Dead.lnk"), sample_lnk(&dir.join("gone.exe").to_string_lossy(), true)).unwrap();
        std::fs::write(dir.join("desktop.ini"), b"[x]").unwrap();
        std::fs::write(dir.join("run me.bat"), b"echo").unwrap();

        let mut entries = super::win_folder_entries_for_test(&dir);
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        // desktop.ini is skipped; a plain program in the folder is an entry too.
        assert_eq!(ids, ["folder|user|Dead.lnk", "folder|user|Live.lnk", "folder|user|my tool.exe", "folder|user|run me.bat"]);
        assert_eq!(entries[0].target_exists, Some(false));
        assert_eq!(entries[1].target_exists, Some(true));
        assert_eq!(entries[3].target_exists, Some(true));
        assert_eq!(entries[1].name, "Live");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read-only look at this machine:
    /// `cargo test --lib smoke_startup -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_startup() {
        let report = scan();
        println!("elevated={} hidden system tasks={} notes={:?}", report.elevated, report.hidden_system_tasks, report.notes);
        for e in &report.entries {
            println!(
                "{:<10} {:<8} {:<28} {:?} on={:?} exists={:?} pub={:?} run={} | {} | {}",
                e.kind_label, e.scope.token(), e.name, e.safety, e.enabled, e.target_exists, e.publisher, e.running, e.summary, e.impact.text
            );
        }
        assert!(report.supported);
    }
}

#[cfg(all(test, target_os = "windows"))]
fn win_folder_entries_for_test(dir: &Path) -> Vec<StartupEntry> {
    let procs = Procs { list: vec![] };
    win::folder_entries(dir, Scope::User, None, None)
        .into_iter()
        .map(|r| finish(r, &procs, false))
        .collect()
}
