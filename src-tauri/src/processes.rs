//! Task killer: what is running, how heavy it is, and a careful way to close it.
//!
//! This is the second piece of the "speed optimizer", after Startup apps.
//!
//! Processes are grouped by program (the same executable run by the same
//! user is one row, so a browser is one row with its many processes). Each
//! row carries the live numbers (CPU, memory, disk, window, children) and a
//! rating with the reasons for it. The rating errs towards "do not touch":
//!
//! - Safe:    no window, nothing started from it, runs as you, not protected.
//!            It is still never closed automatically.
//! - Caution: it may hold unsaved work (a window, an editor, an office app, a
//!            browser, a terminal, something started from it), a window is
//!            not responding, or Jharu cannot see windows (Linux).
//! - Risky:   system processes, security software, drivers, anything under the
//!            Windows or macOS system folders, Jharu itself and the programs
//!            that run it, other users' processes, and anything whose owner or
//!            location Jharu cannot read. Never offered.
//!
//! Closing is graceful first: Windows posts `WM_CLOSE` to the program's
//! visible windows (the same request as the window's X button), macOS sends
//! SIGTERM. Force quit (`TerminateProcess` / SIGKILL) only runs when the user
//! asks for it explicitly, and loses any unsaved work.
//!
//! Before any signal every process is checked again: same pid, same start
//! time, same executable, and still not protected. A pid that was reused by
//! another program is skipped. On Windows the start time is also compared on
//! the open process handle, so the check and the kill see the same process.
//!
//! macOS: windows come from CoreGraphics' window list (no permission needed),
//! an app's helper processes are listed with the app, and closing sends
//! SIGTERM, or SIGKILL for force quit, through sysinfo.

use crate::protected::{in_system_dir, normalize, starts_word, HARDWARE_DRIVERS, SECURITY_SOFTWARE, WINDOWS_COMPONENTS};
use crate::rules::Safety;
use crate::safety::{Level, Reason};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::Emitter;

/// Gap between the two refreshes that make a CPU percentage real.
const SAMPLE_GAP: Duration = Duration::from_millis(500);
/// A sampler untouched this long starts over with two refreshes.
const STALE_AFTER: Duration = Duration::from_secs(6);
/// How long a graceful close waits for a program to go.
const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// How long a force quit waits for the system to finish ending a process.
const FORCE_WAIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------- names

/// Programs that are part of the operating system. Compared as whole
/// normalized names, so "System" is the Windows kernel process and
/// "System Settings" is not.
const SYSTEM_PROCESSES: &[&str] = &[
    // Windows
    "System", "System Idle Process", "Secure System", "Registry", "Memory Compression", "smss", "csrss", "wininit",
    "winlogon", "services", "lsass", "lsm", "svchost", "dwm", "explorer", "fontdrvhost", "sihost", "ctfmon", "conhost",
    "OpenConsole", "RuntimeBroker", "SearchHost", "SearchIndexer", "StartMenuExperienceHost", "ShellExperienceHost",
    "TextInputHost", "ApplicationFrameHost", "taskhostw", "dllhost", "audiodg", "spoolsv", "MsMpEng", "NisSrv",
    "SecurityHealthService", "SecurityHealthSystray", "wlanext", "WUDFHost", "dasHost", "sppsvc", "smartscreen",
    "LockApp", "WmiPrvSE", "Widgets", "WidgetService",
    // macOS
    "kernel_task", "launchd", "WindowServer", "loginwindow", "Dock", "Finder", "SystemUIServer", "coreaudiod",
    "cfprefsd", "distnoted", "notifyd", "syslogd", "logd", "configd", "diskarbitrationd", "powerd", "securityd",
    "trustd", "usernoted", "UserEventAgent", "universalaccessd", "ControlCenter", "NotificationCenter",
];

/// Prefixes of macOS system helpers (mds, mds_stores, mdworker...).
const SYSTEM_PREFIXES: &[&str] = &["mds", "mdworker"];

const EDITORS: &[&str] = &[
    "code", "code insiders", "cursor", "windsurf", "devenv", "idea", "idea64", "pycharm", "pycharm64", "webstorm",
    "webstorm64", "clion", "clion64", "rider", "rider64", "goland", "goland64", "phpstorm64", "rubymine64",
    "datagrip64", "studio64", "sublime text", "sublime_text", "notepad", "atom", "zed", "vim", "gvim", "nvim", "emacs",
    "xcode", "nova", "textmate", "visual studio code",
];
const OFFICE: &[&str] = &[
    "winword", "excel", "powerpnt", "outlook", "onenote", "msaccess", "mspub", "visio", "soffice", "wps", "wpp",
    "pages", "numbers", "keynote", "obsidian", "notion", "microsoft word", "microsoft excel", "microsoft powerpoint",
];
const BROWSERS: &[&str] = &[
    "chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "safari", "arc", "chromium", "iexplore", "google chrome",
    "microsoft edge",
];
const TERMINALS: &[&str] = &[
    "windowsterminal", "wt", "cmd", "powershell", "pwsh", "bash", "zsh", "wsl", "wslhost", "mintty", "alacritty",
    "wezterm gui", "wezterm", "kitty", "iterm2", "terminal", "hyper", "conemu64", "putty", "tmux",
];

/// Children that say nothing about work being done (a console host appears
/// under every command line program).
const IGNORED_CHILDREN: &[&str] = &["conhost", "openconsole", "wslhost"];

fn is_one_of(list: &[&str], stem: &str) -> bool {
    list.iter().any(|n| normalize(n) == stem)
}

/// What a program does, from its name alone, when that means it may hold work.
fn work_kind(stem: &str) -> Option<&'static str> {
    if is_one_of(EDITORS, stem) {
        Some("an editor or development tool")
    } else if is_one_of(OFFICE, stem) {
        Some("an office or notes app")
    } else if is_one_of(BROWSERS, stem) {
        Some("a web browser, which may have open tabs and forms")
    } else if is_one_of(TERMINALS, stem) {
        Some("a terminal or shell, which may have commands running")
    } else {
        None
    }
}

/// Why a program is off limits, from its name and where it runs from.
/// `stem` is the normalized file name without ".exe".
pub fn protected_kind(stem: &str, exe: Option<&str>) -> Option<&'static str> {
    // Apple's own services are named after their bundle id.
    let prefix = SYSTEM_PREFIXES.iter().any(|p| stem.starts_with(p)) || stem.starts_with("com apple ");
    if prefix || is_one_of(SYSTEM_PROCESSES, stem) {
        return Some("a Windows or macOS system process");
    }
    if SECURITY_SOFTWARE.iter().any(|n| starts_word(stem, n)) {
        return Some("security software");
    }
    if HARDWARE_DRIVERS.iter().any(|n| starts_word(stem, n)) {
        return Some("a hardware driver");
    }
    if WINDOWS_COMPONENTS.iter().any(|n| starts_word(stem, n)) {
        return Some("a Windows or macOS system process");
    }
    if let Some(exe) = exe {
        let raw = exe.trim_start_matches("\\\\?\\").to_lowercase();
        if in_system_dir(&raw) || in_system_dir(&raw.replace('/', "\\")) {
            return Some("a program inside the system folders");
        }
    }
    None
}

// ---------------------------------------------------------------- facts

/// Whose process it is, compared with the user running Jharu.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    You,
    Other,
    /// Windows would not say (a protected or elevated process).
    Unknown,
}

/// One process as read from the system.
#[derive(Clone, Debug)]
pub struct RawProc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub exe: Option<String>,
    /// Unix seconds. Together with the pid it identifies one process.
    pub start: u64,
    /// Share of the whole machine's CPU, 0 to 100.
    pub cpu: f32,
    pub memory: u64,
    pub read_per_sec: u64,
    pub write_per_sec: u64,
    pub owner: Owner,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Totals {
    pub cpu_percent: f32,
    pub memory_used: u64,
    pub memory_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// The sum over processes: it counts every kind of read and write a
    /// program does, not only the disk.
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    pub process_count: usize,
}

pub struct Snapshot {
    pub procs: Vec<RawProc>,
    pub totals: Totals,
    /// The process running Jharu.
    pub self_pid: u32,
    /// Where Jharu runs from (lowercase), so another copy of it is never offered.
    pub self_exe: Option<String>,
}

impl Snapshot {
    fn get(&self, pid: u32) -> Option<&RawProc> {
        self.procs.iter().find(|p| p.pid == pid)
    }
}

fn norm_exe(p: &str) -> String {
    p.trim_start_matches("\\\\?\\").replace('/', "\\").to_lowercase()
}

/// The file name of a program without ".exe", as shown and compared.
fn file_stem(name: &str, exe: Option<&str>) -> String {
    let file = exe
        .and_then(|e| e.rsplit(['\\', '/']).next())
        .filter(|f| !f.is_empty())
        .unwrap_or(name);
    let lower = file.to_lowercase();
    if lower.ends_with(".exe") {
        file[..file.len() - 4].to_string()
    } else {
        file.to_string()
    }
}

fn read_snapshot(sys: &System, elapsed: f64, ncpu: usize, self_pid: u32) -> Snapshot {
    let me = sys.process(Pid::from_u32(self_pid)).and_then(|p| p.user_id());
    let secs = elapsed.max(0.05);
    let procs: Vec<RawProc> = sys
        .processes()
        .values()
        .map(|p| {
            let usage = p.disk_usage();
            RawProc {
                pid: p.pid().as_u32(),
                parent: p.parent().map(|p| p.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                exe: p.exe().map(|e| e.to_string_lossy().into_owned()),
                start: p.start_time(),
                // sysinfo reports per core (a busy 8-core machine can read
                // 800%); the machine's own total is what Task Manager shows.
                cpu: (p.cpu_usage() / ncpu.max(1) as f32).clamp(0.0, 100.0),
                memory: p.memory(),
                read_per_sec: (usage.read_bytes as f64 / secs) as u64,
                write_per_sec: (usage.written_bytes as f64 / secs) as u64,
                owner: match (p.user_id(), me) {
                    (Some(u), Some(m)) if u == m => Owner::You,
                    (Some(_), Some(_)) => Owner::Other,
                    _ => Owner::Unknown,
                },
            }
        })
        .collect();
    let totals = Totals {
        cpu_percent: sys.global_cpu_usage().clamp(0.0, 100.0),
        memory_used: sys.used_memory(),
        memory_total: sys.total_memory(),
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
        disk_read_per_sec: procs.iter().map(|p| p.read_per_sec).sum(),
        disk_write_per_sec: procs.iter().map(|p| p.write_per_sec).sum(),
        process_count: procs.len(),
    };
    let self_exe = std::env::current_exe().ok().map(|e| norm_exe(&e.to_string_lossy()));
    Snapshot { procs, totals, self_pid, self_exe }
}

fn logical_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn live_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet)
}

/// Keeps one `System` alive between polls so the CPU and disk figures are
/// deltas since the previous poll, and a poll costs one refresh.
struct Sampler {
    sys: System,
    last: Option<Instant>,
}

static SAMPLER: Mutex<Option<Sampler>> = Mutex::new(None);

fn sample() -> Snapshot {
    let mut guard = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    let s = guard.get_or_insert_with(|| Sampler { sys: System::new(), last: None });
    let refresh = |sys: &mut System| {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, live_kind());
    };
    let fresh_enough = s.last.is_some_and(|t| t.elapsed() < STALE_AFTER);
    if !fresh_enough {
        refresh(&mut s.sys);
        s.last = Some(Instant::now());
        std::thread::sleep(SAMPLE_GAP);
    } else if let Some(t) = s.last {
        // Two readings closer than this make the CPU figure meaningless.
        let min = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;
        if t.elapsed() < min {
            std::thread::sleep(min - t.elapsed());
        }
    }
    let since = s.last.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.5);
    refresh(&mut s.sys);
    s.last = Some(Instant::now());
    read_snapshot(&s.sys, since, logical_cpus(), std::process::id())
}

/// One reading with no CPU history, for checks right before acting. Reads
/// the executable and owner of every process afresh.
fn fresh() -> Snapshot {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_exe(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    read_snapshot(&sys, 1.0, logical_cpus(), std::process::id())
}

// -------------------------------------------------------------- windows

/// A program's visible top-level windows.
#[derive(Clone, Debug, Default)]
pub struct WinInfo {
    pub hwnds: Vec<isize>,
    pub hung: bool,
}

/// Visible top-level windows by owning pid, or None where Jharu cannot see
/// windows (Linux).
type Windows = Option<HashMap<u32, WinInfo>>;

#[cfg(target_os = "windows")]
fn visible_windows() -> Windows {
    Some(win::visible_windows())
}

#[cfg(target_os = "macos")]
fn visible_windows() -> Windows {
    Some(mac::windows_by_pid())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn visible_windows() -> Windows {
    None
}

#[cfg(target_os = "macos")]
mod mac {
    use super::WinInfo;
    use std::collections::HashMap;
    use std::ffi::c_void;

    type CFRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFRef, index: isize) -> CFRef;
        fn CFDictionaryGetValue(dict: CFRef, key: CFRef) -> CFRef;
        fn CFNumberGetValue(number: CFRef, the_type: isize, out: *mut c_void) -> u8;
        fn CFRelease(cf: CFRef);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFRef;
        static kCGWindowOwnerPID: CFRef;
        static kCGWindowLayer: CFRef;
        static kCGWindowNumber: CFRef;
    }

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    /// `[NSRunningApplication runningApplicationWithProcessIdentifier:pid]`,
    /// then `f` on it, inside an autorelease pool. None when the process is
    /// not an app (a daemon or command line program).
    fn with_running_app<T>(pid: u32, f: impl FnOnce(*mut c_void) -> T) -> Option<T> {
        unsafe {
            let class = objc_getClass(c"NSRunningApplication".as_ptr());
            if class.is_null() {
                return None;
            }
            let pool = objc_autoreleasePoolPush();
            // objc_msgSend must be called through the method's exact type.
            let send: extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
            let app = send(class, sel_registerName(c"runningApplicationWithProcessIdentifier:".as_ptr()), pid as i32);
            let out = (!app.is_null()).then(|| f(app));
            objc_autoreleasePoolPop(pool);
            out
        }
    }

    /// A regular app: one with a Dock icon and menu bar, which can be asked
    /// to quit.
    pub fn is_app(pid: u32) -> bool {
        with_running_app(pid, |app| unsafe {
            let send: extern "C" fn(*mut c_void, *mut c_void) -> isize = std::mem::transmute(objc_msgSend as *const ());
            // NSApplicationActivationPolicyRegular
            send(app, sel_registerName(c"activationPolicy".as_ptr())) == 0
        })
        .unwrap_or(false)
    }

    /// `[app terminate]`: the polite quit, after which the app may ask to
    /// save or decline. True when the request was delivered.
    pub fn ask_app_to_quit(pid: u32) -> bool {
        with_running_app(pid, |app| unsafe {
            let send: extern "C" fn(*mut c_void, *mut c_void) -> i8 = std::mem::transmute(objc_msgSend as *const ());
            send(app, sel_registerName(c"terminate".as_ptr())) != 0
        })
        .unwrap_or(false)
    }

    /// Every window, not only those on screen: a minimized window or one on
    /// another desktop can hold unsaved work too.
    const OPTION_ALL: u32 = 0;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    const CF_NUMBER_SINT64: isize = 4;

    /// App windows (layer 0) by owning pid. Menus, the menu bar, the Dock
    /// and status items sit on other layers. Needs no permission: without
    /// Screen Recording only window titles are withheld.
    pub fn windows_by_pid() -> HashMap<u32, WinInfo> {
        let mut out: HashMap<u32, WinInfo> = HashMap::new();
        unsafe {
            let list = CGWindowListCopyWindowInfo(OPTION_ALL | EXCLUDE_DESKTOP_ELEMENTS, 0);
            if list.is_null() {
                return out;
            }
            let num = |dict: CFRef, key: CFRef| -> Option<i64> {
                let v = CFDictionaryGetValue(dict, key);
                let mut n: i64 = 0;
                (!v.is_null() && CFNumberGetValue(v, CF_NUMBER_SINT64, &mut n as *mut i64 as *mut c_void) != 0).then_some(n)
            };
            for i in 0..CFArrayGetCount(list) {
                let dict = CFArrayGetValueAtIndex(list, i);
                if dict.is_null() || num(dict, kCGWindowLayer) != Some(0) {
                    continue;
                }
                if let (Some(pid), Some(number)) = (num(dict, kCGWindowOwnerPID), num(dict, kCGWindowNumber)) {
                    out.entry(pid as u32).or_default().hwnds.push(number as isize);
                }
            }
            CFRelease(list);
        }
        out
    }
}

// ------------------------------------------------------------- the rating

/// What the rating is worked out from, for one program.
pub struct Facts<'a> {
    /// File name without ".exe".
    pub name: &'a str,
    pub exe: Option<&'a str>,
    pub owner: Owner,
    /// Part of Jharu, or a program that started it.
    pub hosts_jharu: bool,
    /// None where Jharu cannot see windows.
    pub has_window: Option<bool>,
    pub not_responding: bool,
    pub child_count: usize,
    pub child_names: Vec<String>,
    /// Other programs (not system ones) that started this one and are still
    /// running: it may be a part of them.
    pub parent_names: Vec<String>,
    /// Why one of the program's other processes is off limits (macOS, where
    /// an app's helpers are listed with the app).
    pub part_protected: Option<&'static str>,
}

pub struct Rating {
    pub safety: Safety,
    pub summary: String,
    pub reasons: Vec<Reason>,
    /// Whether Jharu will close it at all. False for every Risky row.
    pub offered: bool,
}

/// The rating so far: the worst answer wins, and every answer is kept.
struct Notes {
    safety: Safety,
    reasons: Vec<Reason>,
}

impl Notes {
    fn add(&mut self, level: Level, at_least: Safety, text: impl Into<String>) {
        if at_least > self.safety {
            self.safety = at_least;
        }
        self.reasons.push(Reason { level, text: text.into() });
    }
}

pub fn rate(f: &Facts) -> Rating {
    let stem = normalize(f.name);
    let mut n = Notes { safety: Safety::Safe, reasons: vec![] };
    let mut summary = String::new();
    let mut protect = |n: &mut Notes, what: &str, text: String| {
        n.add(Level::Bad, Safety::Risky, text);
        if summary.is_empty() {
            summary = format!("Protected: {what}");
        }
    };

    // 1: is it off limits?
    if f.hosts_jharu {
        protect(
            &mut n,
            "Jharu or what runs it",
            "This is Jharu itself, or a program that started it. Closing it would close Jharu, so Jharu will not offer it.".into(),
        );
    }
    if let Some(what) = protected_kind(&stem, f.exe).or(f.part_protected) {
        protect(
            &mut n,
            what,
            format!("Looks like {what}. Jharu will not close it: it may protect or run this computer."),
        );
    }
    match f.owner {
        Owner::You => {}
        Owner::Other => protect(
            &mut n,
            "another user's or system program",
            "It runs as another user or as the system, not as you. Jharu only closes your own programs.".into(),
        ),
        Owner::Unknown => protect(
            &mut n,
            "owner unknown",
            "The system would not tell Jharu who runs it, which usually means a protected or elevated program. Jharu only closes programs it can prove are yours.".into(),
        ),
    }
    if f.exe.is_none() {
        protect(
            &mut n,
            "location unreadable",
            "Jharu cannot read where it runs from, so it cannot check it is the same program later. That is usually a protected system process.".into(),
        );
    }

    if n.safety == Safety::Risky {
        return Rating { safety: n.safety, summary, reasons: n.reasons, offered: false };
    }

    n.add(Level::Ok, Safety::Safe, "It runs as you and is not security software, a driver or part of the system.");

    // 2: may it hold work?
    let mut evidence_missing = false;
    match f.has_window {
        Some(true) => n.add(
            Level::Warn,
            Safety::Caution,
            "It has a window open. If that holds unsaved work, closing it makes the program ask you to save, or lose the work if it does not ask.",
        ),
        Some(false) => n.add(Level::Ok, Safety::Safe, "It has no window, so there is nothing on screen to save."),
        None => {
            evidence_missing = true;
            n.add(
                Level::Warn,
                Safety::Caution,
                "Jharu cannot see which programs have windows here, so it cannot tell whether this one holds unsaved work.",
            );
        }
    }
    if f.not_responding {
        n.add(
            Level::Warn,
            Safety::Caution,
            "Windows reports a window of this program as not responding. It may ignore a close request, and force quitting it loses what it has not saved.",
        );
    }
    let kind = work_kind(&stem);
    if let Some(kind) = kind {
        n.add(
            Level::Warn,
            Safety::Caution,
            format!("It looks like {kind}, which often holds work that is not saved yet."),
        );
    }
    if f.child_count > 0 {
        let names = if f.child_names.is_empty() { String::new() } else { format!(" ({})", f.child_names.join(", ")) };
        let one = f.child_count == 1;
        n.add(
            Level::Warn,
            Safety::Caution,
            format!(
                "{} other program{} {} started from it{names}. Closing it may stop {} too.",
                f.child_count,
                if one { "" } else { "s" },
                if one { "was" } else { "were" },
                if one { "that one" } else { "those" },
            ),
        );
    } else {
        n.add(Level::Ok, Safety::Safe, "Nothing else was started from it.");
    }
    if !f.parent_names.is_empty() {
        n.add(
            Level::Warn,
            Safety::Caution,
            format!(
                "It was started by {}, which is still running. It may be part of that program, and closing it can break it.",
                f.parent_names.join(", ")
            ),
        );
    }
    if n.safety == Safety::Safe && !evidence_missing {
        n.add(
            Level::Ok,
            Safety::Safe,
            "No window and nothing started from it; it restarts when needed. Jharu still only closes it when you ask.",
        );
    }

    let summary = match n.safety {
        Safety::Safe => "Background program",
        _ if f.not_responding => "Not responding",
        _ if kind.is_some() || f.has_window == Some(true) => "May hold unsaved work",
        _ if f.child_count > 0 => "Has programs started from it",
        _ if !f.parent_names.is_empty() => "Started by another program",
        _ => "Check first",
    };
    Rating { safety: n.safety, summary: summary.into(), reasons: n.reasons, offered: true }
}

// --------------------------------------------------------------- grouping

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Member {
    pub pid: u32,
    /// Unix seconds the process started at, as listed.
    pub start: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct ProcessGroup {
    /// The program's lowercase path and owner; closing sends only this back
    /// with the members.
    pub id: String,
    pub name: String,
    /// The file name, e.g. chrome.exe.
    pub file: String,
    pub publisher: Option<String>,
    pub exe: Option<String>,
    pub members: Vec<Member>,
    pub process_count: usize,
    /// Programs started from this one that are not part of it.
    pub child_count: usize,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    /// None where Jharu cannot see windows (Linux).
    pub has_window: Option<bool>,
    pub window_count: usize,
    pub not_responding: bool,
    pub owner: Owner,
    pub runs_as_you: bool,
    /// Unix seconds of the earliest member.
    pub started: u64,
    pub safety: Safety,
    pub summary: String,
    pub reasons: Vec<Reason>,
    /// False for every Risky row: Jharu offers no action on it.
    pub offered: bool,
}

fn group_id(p: &RawProc) -> String {
    let owner = match p.owner {
        Owner::You => "you",
        Owner::Other => "other",
        Owner::Unknown => "unknown",
    };
    match &p.exe {
        Some(e) => match app_bundle(e) {
            Some(app) => format!("{}|{owner}", norm_exe(app)),
            None => format!("{}|{owner}", norm_exe(e)),
        },
        None => format!("name:{}|{owner}", p.name.to_lowercase()),
    }
}

/// macOS: the outermost `.app` bundle a program runs from. An app's helpers
/// (renderers, extensions, XPC services) live inside it and are listed, and
/// closed, with the app.
fn app_bundle(exe: &str) -> Option<&str> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let at = exe.find(".app/")?;
    Some(&exe[..at + 4])
}

/// The app's main program, as opposed to one of its helpers.
fn is_main_program(exe: &str) -> bool {
    app_bundle(exe).is_some_and(|app| {
        exe[app.len()..].strip_prefix("/Contents/MacOS/").is_some_and(|rest| !rest.contains('/'))
    })
}

/// The pids of Jharu, of every program that started it, and of every program
/// it started (its web view), so none of them can be offered.
pub fn host_pids(procs: &[RawProc], self_pid: u32) -> HashSet<u32> {
    let by_pid: HashMap<u32, &RawProc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut hosts: HashSet<u32> = HashSet::from([self_pid]);
    // A parent id only counts when that process started first; otherwise it
    // is a recycled id.
    let parent_of = |p: &RawProc| -> Option<u32> {
        let parent = by_pid.get(&p.parent?)?;
        (parent.start <= p.start && parent.pid != p.pid).then_some(parent.pid)
    };
    // Ancestors.
    let mut cur = by_pid.get(&self_pid).copied();
    let mut guard = 0;
    while let Some(p) = cur {
        guard += 1;
        if guard > 64 {
            break;
        }
        match parent_of(p) {
            Some(pp) if hosts.insert(pp) => cur = by_pid.get(&pp).copied(),
            _ => break,
        }
    }
    // Descendants of Jharu itself (not of its ancestors, which would be every
    // program the same shell started), found until nothing new turns up.
    let mut below: HashSet<u32> = HashSet::from([self_pid]);
    loop {
        let before = below.len();
        for p in procs {
            if !below.contains(&p.pid) && parent_of(p).is_some_and(|pp| below.contains(&pp)) {
                below.insert(p.pid);
            }
        }
        if below.len() == before {
            break;
        }
    }
    hosts.extend(below);
    hosts
}

type InfoCache = HashMap<String, (Option<String>, Option<String>)>;
static INFO: Mutex<Option<InfoCache>> = Mutex::new(None);

/// (publisher, description) from the program's file information, remembered
/// because it is read from disk.
fn program_info(exe: &str) -> (Option<String>, Option<String>) {
    let key = norm_exe(exe);
    {
        let guard = INFO.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = guard.as_ref().and_then(|m| m.get(&key)) {
            return hit.clone();
        }
    }
    let (company, description) = crate::startup::file_info(Path::new(exe));
    let mut guard = INFO.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashMap::new).insert(key, (company.clone(), description.clone()));
    (company, description)
}

pub fn build_groups(snap: &Snapshot, wins: &Windows, with_info: bool) -> Vec<ProcessGroup> {
    let hosts = host_pids(&snap.procs, snap.self_pid);
    let mut order: Vec<String> = vec![];
    let mut buckets: HashMap<String, Vec<&RawProc>> = HashMap::new();
    for p in &snap.procs {
        let id = group_id(p);
        if !buckets.contains_key(&id) {
            order.push(id.clone());
        }
        buckets.entry(id).or_default().push(p);
    }
    let mut groups: Vec<ProcessGroup> = vec![];
    for id in order {
        let mut members = buckets[&id].clone();
        // The app's own program first: it names the row.
        members.sort_by_key(|p| !p.exe.as_deref().is_some_and(is_main_program));
        let members = &members;
        let first = members[0];
        let pids: HashSet<u32> = members.iter().map(|p| p.pid).collect();

        // Programs started from this one, other than its own processes.
        let mut child_names: Vec<String> = vec![];
        let mut child_count = 0;
        for c in &snap.procs {
            if pids.contains(&c.pid) {
                continue;
            }
            let Some(parent) = c.parent.filter(|pp| pids.contains(pp)) else { continue };
            let started_after = snap.get(parent).is_none_or(|pp| pp.start <= c.start);
            let stem = normalize(&file_stem(&c.name, c.exe.as_deref()));
            if started_after && !is_one_of(IGNORED_CHILDREN, &stem) {
                child_count += 1;
                let n = file_stem(&c.name, c.exe.as_deref());
                if child_names.len() < 3 && !child_names.contains(&n) {
                    child_names.push(n);
                }
            }
        }

        // Programs that started this one and are still running. System
        // parents (explorer, services) are what launches nearly everything.
        let mut parent_names: Vec<String> = vec![];
        for m in members.iter() {
            let Some(pp) = m.parent.filter(|pp| !pids.contains(pp)).and_then(|pp| snap.get(pp)) else { continue };
            let n = file_stem(&pp.name, pp.exe.as_deref());
            if pp.start <= m.start
                && protected_kind(&normalize(&n), pp.exe.as_deref()).is_none()
                && pp.exe.is_some()
                && !parent_names.contains(&n)
                && parent_names.len() < 3
            {
                parent_names.push(n);
            }
        }

        let (window_count, hung) = match wins {
            Some(w) => members.iter().fold((0, false), |(n, h), p| match w.get(&p.pid) {
                Some(i) => (n + i.hwnds.len(), h || i.hung),
                None => (n, h),
            }),
            None => (0, false),
        };
        let has_window = wins.as_ref().map(|_| window_count > 0);
        // An app's main program may be a generic "Electron"; the app's name
        // is what the user knows.
        let stem = match first.exe.as_deref().and_then(app_bundle) {
            Some(app) => app.trim_end_matches(".app").rsplit('/').next().unwrap_or(app).to_string(),
            None => file_stem(&first.name, first.exe.as_deref()),
        };
        let part_protected = members.iter().skip(1).find_map(|m| {
            protected_kind(&normalize(&file_stem(&m.name, m.exe.as_deref())), m.exe.as_deref())
        });
        // Another copy of Jharu counts too: the same program is never offered.
        let hosts_jharu = members.iter().any(|p| hosts.contains(&p.pid))
            || first.exe.as_deref().is_some_and(|e| snap.self_exe.as_deref() == Some(norm_exe(e).as_str()));
        let rating = rate(&Facts {
            name: &stem,
            exe: first.exe.as_deref(),
            owner: first.owner,
            hosts_jharu,
            has_window,
            not_responding: hung,
            child_count,
            child_names,
            parent_names,
            part_protected,
        });
        let (publisher, description) = match (&first.exe, with_info) {
            (Some(e), true) => program_info(e),
            _ => (None, None),
        };
        let file = first
            .exe
            .as_deref()
            .and_then(|e| e.rsplit(['\\', '/']).next())
            .unwrap_or(&first.name)
            .to_string();
        groups.push(ProcessGroup {
            id,
            name: description.filter(|d| d.len() <= 60).unwrap_or_else(|| stem.clone()),
            file,
            publisher,
            exe: first.exe.clone(),
            members: members.iter().map(|p| Member { pid: p.pid, start: p.start }).collect(),
            process_count: members.len(),
            child_count,
            cpu_percent: members.iter().map(|p| p.cpu).sum::<f32>().min(100.0),
            memory_bytes: members.iter().map(|p| p.memory).sum(),
            disk_read_per_sec: members.iter().map(|p| p.read_per_sec).sum(),
            disk_write_per_sec: members.iter().map(|p| p.write_per_sec).sum(),
            has_window,
            window_count,
            not_responding: hung,
            owner: first.owner,
            runs_as_you: first.owner == Owner::You,
            started: members.iter().map(|p| p.start).min().unwrap_or(0),
            safety: rating.safety,
            summary: rating.summary,
            reasons: rating.reasons,
            offered: rating.offered,
        });
    }
    groups.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.memory_bytes.cmp(&a.memory_bytes))
    });
    groups
}

#[derive(Serialize)]
pub struct ProcessReport {
    pub supported: bool,
    /// Jharu can tell which programs have a window (Windows only).
    pub windows_known: bool,
    pub totals: Totals,
    pub groups: Vec<ProcessGroup>,
    pub notes: Vec<String>,
}

/// Running programs with their live load and ratings. The first call (and
/// any after a pause) takes two readings half a second apart so the CPU
/// figures are real; polls after that take one.
#[tauri::command]
pub async fn process_list() -> Result<ProcessReport, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let supported = cfg!(any(target_os = "windows", target_os = "macos"));
        let snap = sample();
        let wins = visible_windows();
        let mut notes = vec![];
        if wins.is_none() {
            notes.push("Jharu cannot see which programs have windows here, so no program is rated Safe: it cannot tell whether one holds unsaved work.".to_string());
        }
        let groups = build_groups(&snap, &wins, true);
        ProcessReport { supported, windows_known: wins.is_some(), totals: snap.totals, groups, notes }
    })
    .await
    .map_err(|e| e.to_string())
}

// ----------------------------------------------------------- the safeguards

/// The program the user saw, and its processes as listed.
#[derive(Deserialize, Clone, Debug)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub members: Vec<Member>,
}

/// Why a listed process may not be signalled now, or None when it may.
/// `live` is the process with that pid right now. A pid that is gone is not
/// a problem (the work is done), so it is the caller's case to handle.
pub fn guard_member(member: &Member, live: &RawProc, group_id_listed: &str) -> Result<(), String> {
    if live.start != member.start {
        return Err(format!(
            "process {} is no longer the one that was listed (it started at a different time, so the number was reused by another program)",
            member.pid
        ));
    }
    if group_id(live) != group_id_listed {
        return Err(format!(
            "process {} is no longer the same program, or no longer runs as the same user",
            member.pid
        ));
    }
    Ok(())
}

enum Vetted {
    /// Every listed process is already gone.
    Gone,
    /// May be signalled: these are alive and match what was listed.
    Go { alive: Vec<Member>, gone: Vec<u32>, skipped: Vec<String> },
    Refused(String),
}

fn vet(target: &Target, snap: &Snapshot, groups: &[ProcessGroup]) -> Vetted {
    let Some(group) = groups.iter().find(|g| g.id == target.id) else {
        // The program is not running as the listed one any more.
        return if target.members.iter().all(|m| snap.get(m.pid).is_none_or(|p| p.start != m.start)) {
            Vetted::Gone
        } else {
            Vetted::Refused("it is no longer the same program as the one listed, so Jharu left it alone".into())
        };
    };
    // Re-rated just now, whatever the list said.
    if !group.offered {
        return Vetted::Refused(format!("{}. Jharu does not close these.", group.summary));
    }
    let mut alive = vec![];
    let mut gone = vec![];
    let mut skipped = vec![];
    for m in &target.members {
        match snap.get(m.pid) {
            None => gone.push(m.pid),
            Some(live) => match guard_member(m, live, &target.id) {
                // A reused pid belongs to something else: it is not ours to
                // report as exited, nor to touch.
                Err(why) => skipped.push(why),
                Ok(()) => alive.push(*m),
            },
        }
    }
    if alive.is_empty() && skipped.is_empty() {
        Vetted::Gone
    } else {
        Vetted::Go { alive, gone, skipped }
    }
}

// ------------------------------------------------------------- the outcome

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Every process ended.
    Exited,
    /// Asked to close; some processes are still running.
    StillRunning,
    /// Windows only: there was no window to ask. Force quit is the way.
    NoWindow,
    /// Jharu did not touch it, and says why.
    Skipped,
    Failed,
}

#[derive(Serialize, Clone, Debug)]
pub struct ItemResult {
    pub id: String,
    pub name: String,
    pub status: Status,
    pub message: String,
    pub exited: Vec<u32>,
    pub running: Vec<u32>,
}

#[derive(Serialize, Default, Debug)]
pub struct CloseOutcome {
    pub results: Vec<ItemResult>,
    pub cancelled: bool,
}

#[derive(Serialize, Clone)]
struct Progress {
    token: String,
    done: usize,
    total: usize,
    current: String,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Which of these (pid, start) pairs are still the same live process.
fn still_alive(members: &[Member]) -> Vec<u32> {
    if members.is_empty() {
        return vec![];
    }
    let pids: Vec<Pid> = members.iter().map(|m| Pid::from_u32(m.pid)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, ProcessRefreshKind::nothing());
    members
        .iter()
        .filter(|m| sys.process(Pid::from_u32(m.pid)).is_some_and(|p| p.start_time() == m.start))
        .map(|m| m.pid)
        .collect()
}

/// Waits until every member has gone or the time is up (or the user cancels).
fn wait_for_exit(members: &[Member], limit: Duration, cancelled: &dyn Fn() -> bool) -> Vec<u32> {
    let started = Instant::now();
    loop {
        let alive = still_alive(members);
        if alive.is_empty() || started.elapsed() >= limit || cancelled() {
            return alive;
        }
        std::thread::sleep(POLL);
    }
}

/// How one program fared, once its signals are sent and the wait is over.
fn finish_item(
    target: &Target,
    gone: Vec<u32>,
    skipped: Vec<String>,
    waited_on: &[Member],
    limit: Duration,
    cancelled: &dyn Fn() -> bool,
    sent: &str,
) -> ItemResult {
    let alive = wait_for_exit(waited_on, limit, cancelled);
    let mut exited: Vec<u32> = gone;
    exited.extend(waited_on.iter().map(|m| m.pid).filter(|p| !alive.contains(p)));
    let (status, mut message) = if alive.is_empty() {
        (Status::Exited, format!("{} ended.", target.name))
    } else {
        (
            Status::StillRunning,
            format!(
                "{} asked to close, but {} still running after {} seconds. It may be waiting for you to answer a question about unsaved work.",
                sent,
                plural(alive.len(), "process is", "processes are"),
                limit.as_secs()
            ),
        )
    };
    if !skipped.is_empty() {
        message.push_str(&format!(" Skipped: {}.", skipped.join("; ")));
    }
    ItemResult { id: target.id.clone(), name: target.name.clone(), status, message, exited, running: alive }
}

fn skipped_result(target: &Target, why: String) -> ItemResult {
    ItemResult {
        id: target.id.clone(),
        name: target.name.clone(),
        status: Status::Skipped,
        message: format!("Left alone: {why}"),
        exited: vec![],
        running: vec![],
    }
}

fn gone_result(target: &Target) -> ItemResult {
    ItemResult {
        id: target.id.clone(),
        name: target.name.clone(),
        status: Status::Exited,
        message: format!("{} was already closed.", target.name),
        exited: target.members.iter().map(|m| m.pid).collect(),
        running: vec![],
    }
}

// ------------------------------------------------------------- the actions

/// Asks programs to close, the way the window's own close button does.
/// Programs without a window are reported, not touched: use Force quit.
fn close_in(
    targets: &[Target],
    snap: &Snapshot,
    wins: &Windows,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(usize, usize, &str),
    wait: Duration,
) -> CloseOutcome {
    let groups = build_groups(snap, wins, false);
    let mut out = CloseOutcome::default();
    let total = targets.len();
    for (i, t) in targets.iter().enumerate() {
        if cancelled() {
            out.cancelled = true;
            break;
        }
        progress(i, total, &t.name);
        let result = match vet(t, snap, &groups) {
            Vetted::Gone => gone_result(t),
            Vetted::Refused(why) => skipped_result(t, why),
            Vetted::Go { alive, skipped, .. } if alive.is_empty() => skipped_result(t, skipped.join("; ")),
            Vetted::Go { alive, gone, skipped } => match ask_to_close(&alive, wins) {
                Asked::NoWindow => ItemResult {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    status: Status::NoWindow,
                    message: format!(
                        "{} has no window to close, so there is nothing to ask. Use Force quit if you want it stopped; anything it has not saved is lost.",
                        t.name
                    ),
                    exited: gone,
                    running: alive.iter().map(|m| m.pid).collect(),
                },
                Asked::Failed(why) => ItemResult {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    status: Status::Failed,
                    message: why,
                    exited: gone,
                    running: alive.iter().map(|m| m.pid).collect(),
                },
                Asked::Sent(what) => finish_item(t, gone, skipped, &alive, wait, cancelled, &what),
            },
        };
        out.results.push(result);
    }
    progress(total, total, "");
    out
}

/// Ends programs at once. Only for an explicit second click.
fn force_in(
    targets: &[Target],
    snap: &Snapshot,
    wins: &Windows,
    progress: &dyn Fn(usize, usize, &str),
    wait: Duration,
) -> CloseOutcome {
    let groups = build_groups(snap, wins, false);
    let mut out = CloseOutcome::default();
    let total = targets.len();
    for (i, t) in targets.iter().enumerate() {
        progress(i, total, &t.name);
        let result = match vet(t, snap, &groups) {
            Vetted::Gone => gone_result(t),
            Vetted::Refused(why) => skipped_result(t, why),
            Vetted::Go { alive, .. } if alive.is_empty() => {
                skipped_result(t, "the process numbers were reused by other programs".into())
            }
            Vetted::Go { alive, gone, skipped } => {
                let mut errors: Vec<String> = vec![];
                for m in &alive {
                    if let Err(e) = terminate(m) {
                        errors.push(format!("process {}: {e}", m.pid));
                    }
                }
                let still = wait_for_exit(&alive, wait, &|| false);
                let mut exited = gone;
                exited.extend(alive.iter().map(|m| m.pid).filter(|p| !still.contains(p)));
                let (status, mut message) = if still.is_empty() {
                    (Status::Exited, format!("{} was force quit. Anything it had not saved is lost.", t.name))
                } else {
                    (
                        Status::Failed,
                        format!(
                            "{} did not end: {}.",
                            plural(still.len(), "process", "processes"),
                            if errors.is_empty() { "Windows has not finished ending it".to_string() } else { errors.join("; ") }
                        ),
                    )
                };
                if !skipped.is_empty() {
                    message.push_str(&format!(" Skipped: {}.", skipped.join("; ")));
                }
                ItemResult { id: t.id.clone(), name: t.name.clone(), status, message, exited, running: still }
            }
        };
        out.results.push(result);
    }
    progress(total, total, "");
    out
}

enum Asked {
    /// A close request went out; the text says what was sent.
    Sent(String),
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    NoWindow,
    /// macOS only: the signal was not accepted.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    Failed(String),
}

#[cfg(target_os = "windows")]
fn ask_to_close(alive: &[Member], wins: &Windows) -> Asked {
    let Some(map) = wins else { return Asked::NoWindow };
    let mut sent = 0usize;
    for m in alive {
        let Some(info) = map.get(&m.pid) else { continue };
        // Windows can hand a number to another program between the listing
        // and now; the open process must still have the listed start time.
        if win::process_start(m.pid) != Some(m.start) {
            continue;
        }
        sent += win::post_close(&info.hwnds);
    }
    if sent == 0 {
        Asked::NoWindow
    } else {
        Asked::Sent(format!("Jharu sent {}", plural(sent, "close request", "close requests")))
    }
}

#[cfg(not(target_os = "windows"))]
fn ask_to_close(alive: &[Member], _wins: &Windows) -> Asked {
    let mut sent = 0usize;
    let mut sys = System::new();
    let pids: Vec<Pid> = alive.iter().map(|m| Pid::from_u32(m.pid)).collect();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, ProcessRefreshKind::nothing());
    // Same pid and start time as listed, checked on the process just read.
    let live: Vec<&sysinfo::Process> =
        alive.iter().filter_map(|m| sys.process(Pid::from_u32(m.pid)).filter(|p| p.start_time() == m.start)).collect();

    // A Mac app is asked to quit the way the Dock asks, so it can offer to
    // save. SIGTERM would end it without asking, and ending its helpers
    // first could lose what they hold; they exit with the app.
    #[cfg(target_os = "macos")]
    {
        let apps: Vec<u32> = live.iter().map(|p| p.pid().as_u32()).filter(|pid| mac::is_app(*pid)).collect();
        if !apps.is_empty() {
            let asked = apps.iter().filter(|pid| mac::ask_app_to_quit(**pid)).count();
            return if asked == 0 {
                Asked::Failed("macOS did not pass the quit request to the app.".into())
            } else {
                Asked::Sent(format!("Jharu asked the app to quit, as the Dock does ({})", plural(asked, "app", "apps")))
            };
        }
    }
    for p in live {
        if p.kill_with(sysinfo::Signal::Term).unwrap_or(false) {
            sent += 1;
        }
    }
    if sent == 0 {
        Asked::Failed("macOS did not accept the close request (SIGTERM).".into())
    } else {
        Asked::Sent(format!("Jharu sent {}", plural(sent, "close request", "close requests")))
    }
}

#[cfg(target_os = "windows")]
fn terminate(m: &Member) -> Result<(), String> {
    win::terminate(m.pid, m.start)
}

#[cfg(not(target_os = "windows"))]
fn terminate(m: &Member) -> Result<(), String> {
    let mut sys = System::new();
    let pid = Pid::from_u32(m.pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
    match sys.process(pid).filter(|p| p.start_time() == m.start) {
        Some(p) => match p.kill_with(sysinfo::Signal::Kill) {
            Some(true) => Ok(()),
            _ => Err("macOS refused the request (SIGKILL)".into()),
        },
        None => Err("it is not the listed process any more".into()),
    }
}

// -------------------------------------------------------------- Windows API

#[cfg(target_os = "windows")]
mod win {
    //! The few Win32 calls the task killer needs: which programs own a
    //! visible window, asking a window to close, and ending a process.
    use super::WinInfo;
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FILETIME, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsHungAppWindow, IsWindowVisible, PostMessageW, WM_CLOSE,
    };

    struct Found {
        pid: u32,
        hwnd: isize,
        hung: bool,
    }

    unsafe extern "system" fn each_window(hwnd: HWND, lparam: LPARAM) -> i32 {
        // SAFETY: lparam is the address of the Vec that visible_windows owns
        // for the duration of EnumWindows.
        let found = &mut *(lparam as *mut Vec<Found>);
        // Every visible top-level window counts, owned dialogs, tool windows
        // and windows on other desktops included: over-counting only makes a
        // program look like it may hold work, which is the safe mistake.
        if IsWindowVisible(hwnd) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != 0 {
                found.push(Found { pid, hwnd: hwnd as isize, hung: IsHungAppWindow(hwnd) != 0 });
            }
        }
        1
    }

    pub fn visible_windows() -> HashMap<u32, WinInfo> {
        let mut found: Vec<Found> = vec![];
        unsafe {
            EnumWindows(Some(each_window), &mut found as *mut Vec<Found> as LPARAM);
        }
        let mut map: HashMap<u32, WinInfo> = HashMap::new();
        for f in found {
            let info = map.entry(f.pid).or_default();
            info.hwnds.push(f.hwnd);
            info.hung |= f.hung;
        }
        map
    }

    /// Posts WM_CLOSE, the message the window's close button sends. The
    /// program decides what to do with it (ask to save, or refuse).
    pub fn post_close(hwnds: &[isize]) -> usize {
        hwnds
            .iter()
            .filter(|&&h| unsafe { PostMessageW(h as HWND, WM_CLOSE, 0, 0) } != 0)
            .count()
    }

    fn start_unix(h: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut a, mut b, mut c) = (zero, zero, zero, zero);
        if unsafe { GetProcessTimes(h, &mut created, &mut a, &mut b, &mut c) } == 0 {
            return None;
        }
        let ft = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        // Same conversion as sysinfo, so the two agree to the second.
        (ft / 10_000_000).checked_sub(11_644_473_600)
    }

    /// When the process with this pid started, read from the process itself.
    pub fn process_start(pid: u32) -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let start = start_unix(h);
            CloseHandle(h);
            start
        }
    }

    /// Ends the process, after checking on the open handle that it is the one
    /// that started at `expected_start`.
    pub fn terminate(pid: u32, expected_start: u64) -> Result<(), String> {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return Err(match GetLastError() {
                    5 => "Windows denied access (it is protected, or runs with more rights than Jharu)".to_string(),
                    87 => "it has already ended".to_string(),
                    e => format!("could not open it (Windows error {e})"),
                });
            }
            if start_unix(h) != Some(expected_start) {
                CloseHandle(h);
                return Err("it is not the listed process any more".into());
            }
            let ok = TerminateProcess(h, 1);
            let err = GetLastError();
            CloseHandle(h);
            if ok == 0 {
                Err(format!("Windows refused to end it (error {err})"))
            } else {
                Ok(())
            }
        }
    }
}

// ----------------------------------------------------------------- commands

static CANCELLED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn is_cancelled(token: &str) -> bool {
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|t| t == token)
}

/// Stops the wait of a close in progress. Close requests already sent are
/// not taken back.
#[tauri::command]
pub fn process_cancel(token: String) {
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).push(token);
}

fn forget(token: &str) {
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).retain(|t| t != token);
}

/// Asks the chosen programs to close and reports which ended.
#[tauri::command]
pub async fn process_close(app: tauri::AppHandle, items: Vec<Target>, token: String) -> Result<CloseOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let snap = fresh();
        let wins = visible_windows();
        let out = close_in(
            &items,
            &snap,
            &wins,
            &|| is_cancelled(&token),
            &|done, total, current| {
                let _ = app.emit("process-progress", Progress { token: token.clone(), done, total, current: current.to_string() });
            },
            CLOSE_WAIT,
        );
        forget(&token);
        out
    })
    .await
    .map_err(|e| e.to_string())
}

/// Ends the chosen programs at once; anything they have not saved is lost.
/// The window asks for a second, explicit click before it calls this.
#[tauri::command]
pub async fn process_force_quit(items: Vec<Target>) -> Result<CloseOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let snap = fresh();
        let wins = visible_windows();
        force_in(&items, &snap, &wins, &|_, _, _| {}, FORCE_WAIT)
    })
    .await
    .map_err(|e| e.to_string())
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(name: &'a str, exe: Option<&'a str>) -> Facts<'a> {
        Facts {
            name,
            exe,
            owner: Owner::You,
            hosts_jharu: false,
            has_window: Some(false),
            not_responding: false,
            child_count: 0,
            child_names: vec![],
            parent_names: vec![],
            part_protected: None,
        }
    }

    fn user_exe(name: &str) -> String {
        format!("C:\\Users\\me\\AppData\\Local\\{name}\\{name}.exe")
    }

    #[test]
    fn a_quiet_program_of_yours_is_safe_with_the_evidence_shown() {
        let exe = user_exe("sync");
        let r = rate(&facts("sync", Some(&exe)));
        assert_eq!(r.safety, Safety::Safe);
        assert!(r.offered);
        assert!(r.reasons.iter().any(|x| x.text.contains("restarts when needed")));
        assert!(r.reasons.iter().all(|x| x.level == Level::Ok));
    }

    #[test]
    fn safe_needs_evidence_of_no_window() {
        // Where windows cannot be seen, nothing is Safe.
        let exe = user_exe("sync");
        let mut f = facts("sync", Some(&exe));
        f.has_window = None;
        let r = rate(&f);
        assert_eq!(r.safety, Safety::Caution);
        assert!(r.offered);
        assert!(r.reasons.iter().any(|x| x.text.contains("cannot see which programs have windows")));
        assert!(!r.reasons.iter().any(|x| x.text.contains("restarts when needed")));
    }

    #[test]
    fn a_window_makes_it_caution() {
        let exe = user_exe("player");
        let mut f = facts("player", Some(&exe));
        f.has_window = Some(true);
        let r = rate(&f);
        assert_eq!(r.safety, Safety::Caution);
        assert_eq!(r.summary, "May hold unsaved work");
        assert!(r.offered);
    }

    #[test]
    fn editors_browsers_terminals_and_office_are_caution_even_without_a_window() {
        for name in ["Code", "chrome", "WINWORD", "pwsh", "idea64", "Obsidian", "firefox", "Notepad++"] {
            let exe = user_exe(name);
            let r = rate(&facts(name, Some(&exe)));
            assert_eq!(r.safety, Safety::Caution, "{name}");
            assert!(r.offered, "{name}");
        }
    }

    #[test]
    fn a_program_with_children_is_caution_and_names_them() {
        let exe = user_exe("launcher");
        let mut f = facts("launcher", Some(&exe));
        f.child_count = 2;
        f.child_names = vec!["node".into(), "python".into()];
        let r = rate(&f);
        assert_eq!(r.safety, Safety::Caution);
        assert_eq!(r.summary, "Has programs started from it");
        assert!(r.reasons.iter().any(|x| x.text.contains("node, python")));
    }

    #[test]
    fn a_helper_started_by_another_running_program_is_caution() {
        let exe = user_exe("helper");
        let mut f = facts("helper", Some(&exe));
        f.parent_names = vec!["Claude".into()];
        let r = rate(&f);
        assert_eq!(r.safety, Safety::Caution);
        assert_eq!(r.summary, "Started by another program");
        assert!(r.reasons.iter().any(|x| x.text.contains("started by Claude")));
    }

    #[test]
    fn only_non_system_parents_count_and_only_when_they_started_first() {
        let child = "C:\\w\\webview.exe";
        let snap = snapshot(
            vec![
                proc(1, None, "explorer.exe", Some("C:\\Windows\\explorer.exe"), 10),
                proc(2, Some(1), "app.exe", Some("C:\\a\\app.exe"), 20),
                proc(3, Some(2), "webview.exe", Some(child), 30),
                // Its listed parent is newer than it: a recycled id, so no parent.
                proc(4, Some(2), "lone.exe", Some("C:\\l\\lone.exe"), 5),
                proc(5, Some(1), "fromstart.exe", Some("C:\\f\\fromstart.exe"), 40),
            ],
            9999,
        );
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        let g = |f: &str| groups.iter().find(|g| g.file == f).unwrap();
        assert_eq!(g("webview.exe").safety, Safety::Caution);
        assert_eq!(g("webview.exe").summary, "Started by another program");
        // Started from Explorer, like nearly everything: no news there.
        assert_eq!(g("fromstart.exe").safety, Safety::Safe);
        assert_eq!(g("lone.exe").safety, Safety::Safe);
    }

    #[test]
    fn another_copy_of_jharu_is_never_offered() {
        let mut snap = snapshot(vec![proc(8, None, "Jharu.exe", Some("C:\\Apps\\Jharu\\Jharu.exe"), 100)], 9999);
        snap.self_exe = Some("c:\\apps\\jharu\\jharu.exe".into());
        let g = &build_groups(&snap, &Some(HashMap::new()), false)[0];
        assert!(!g.offered);
        assert!(g.summary.contains("Jharu"));
    }

    #[test]
    fn a_window_that_is_not_responding_is_caution_and_says_so() {
        let exe = user_exe("stuck");
        let mut f = facts("stuck", Some(&exe));
        f.has_window = Some(true);
        f.not_responding = true;
        let r = rate(&f);
        assert_eq!(r.safety, Safety::Caution);
        assert_eq!(r.summary, "Not responding");
        assert!(r.reasons.iter().any(|x| x.text.contains("not responding")));
    }

    #[test]
    fn system_processes_are_risky_and_not_offered() {
        let names = [
            "System", "Registry", "smss", "csrss", "wininit", "winlogon", "services", "lsass", "svchost", "dwm",
            "explorer", "fontdrvhost", "sihost", "ctfmon", "conhost", "RuntimeBroker", "SearchHost",
            "StartMenuExperienceHost", "ShellExperienceHost", "audiodg", "spoolsv", "MsMpEng", "SecurityHealthService",
            "kernel_task", "launchd", "WindowServer", "loginwindow", "Dock", "Finder", "SystemUIServer", "mds",
            "mds_stores", "mdworker_shared", "coreaudiod",
        ];
        for name in names {
            // Even when the path looks harmless: the name decides.
            let exe = user_exe(name);
            let r = rate(&facts(name, Some(&exe)));
            assert_eq!(r.safety, Safety::Risky, "{name}");
            assert!(!r.offered, "{name}");
            assert!(r.summary.starts_with("Protected"), "{name}");
        }
    }

    #[test]
    fn security_software_and_drivers_come_from_the_shared_lists() {
        for name in ["AVGSvc", "ESET Service", "avastui", "NortonUI", "RtkNGUI64", "NVDisplay.Container", "SynTPEnh"] {
            let exe = user_exe(name);
            let r = rate(&facts(name, Some(&exe)));
            assert_eq!(r.safety, Safety::Risky, "{name}");
            assert!(!r.offered, "{name}");
        }
    }

    #[test]
    fn word_matching_does_not_protect_lookalikes() {
        // "preset" must not match "eset"; "System Settings" is not "System".
        for name in ["Preset Manager", "System Settings", "Dockerfile watcher", "Reset Tool"] {
            let exe = user_exe(name);
            assert!(rate(&facts(name, Some(&exe))).offered, "{name}");
        }
        assert_eq!(protected_kind("preset manager", None), None);
        assert_eq!(protected_kind("system settings", None), None);
        // A name that starts a word with a protected name is protected.
        assert!(protected_kind("eset service", None).is_some());
    }

    #[test]
    fn anything_under_the_system_folders_is_risky() {
        let r = rate(&facts("notepad", Some("C:\\Windows\\System32\\notepad.exe")));
        assert_eq!(r.safety, Safety::Risky);
        assert!(!r.offered);
        let r = rate(&facts("tool", Some("C:\\WINDOWS\\SysWOW64\\tool.exe")));
        assert!(!r.offered);
        let r = rate(&facts("Safari", Some("/System/Volumes/Preboot/Safari")));
        assert!(!r.offered);
        let r = rate(&facts("x", Some("/usr/libexec/x")));
        assert!(!r.offered);
        let r = rate(&facts("x", Some("\\\\?\\C:\\Windows\\x.exe")));
        assert!(!r.offered);
    }

    #[test]
    fn jharu_and_what_runs_it_are_never_offered() {
        let exe = user_exe("jharu");
        let mut f = facts("jharu", Some(&exe));
        f.hosts_jharu = true;
        let r = rate(&f);
        assert_eq!(r.safety, Safety::Risky);
        assert!(!r.offered);
        assert!(r.summary.contains("Jharu"));
    }

    #[test]
    fn other_users_and_unknown_owners_are_not_offered() {
        let exe = user_exe("sync");
        let mut f = facts("sync", Some(&exe));
        f.owner = Owner::Other;
        assert!(!rate(&f).offered);
        f.owner = Owner::Unknown;
        let r = rate(&f);
        assert!(!r.offered);
        assert_eq!(r.safety, Safety::Risky);
    }

    #[test]
    fn a_program_whose_location_cannot_be_read_is_not_offered() {
        let r = rate(&facts("mystery", None));
        assert_eq!(r.safety, Safety::Risky);
        assert!(!r.offered);
    }

    #[test]
    fn risky_is_never_downgraded_by_good_news() {
        // No window, no children, yours — and still protected.
        let r = rate(&facts("svchost", Some("C:\\Windows\\System32\\svchost.exe")));
        assert_eq!(r.safety, Safety::Risky);
        assert!(r.reasons.iter().all(|x| x.level == Level::Bad));
    }

    // ------------------------------------------------------- grouping

    fn proc(pid: u32, parent: Option<u32>, name: &str, exe: Option<&str>, start: u64) -> RawProc {
        RawProc {
            pid,
            parent,
            name: name.into(),
            exe: exe.map(String::from),
            start,
            cpu: 1.0,
            memory: 10,
            read_per_sec: 1,
            write_per_sec: 2,
            owner: Owner::You,
        }
    }

    fn snapshot(procs: Vec<RawProc>, self_pid: u32) -> Snapshot {
        let totals = Totals { process_count: procs.len(), ..Totals::default() };
        Snapshot { procs, totals, self_pid, self_exe: None }
    }

    #[test]
    fn one_program_is_one_row_with_summed_numbers() {
        let chrome = "C:\\Program Files\\Chrome\\chrome.exe";
        let snap = snapshot(
            vec![
                proc(10, None, "chrome.exe", Some(chrome), 100),
                proc(11, Some(10), "chrome.exe", Some(chrome), 101),
                proc(12, Some(10), "chrome.exe", Some(&chrome.to_uppercase()), 102),
                proc(20, None, "other.exe", Some("C:\\x\\other.exe"), 100),
            ],
            9999,
        );
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        assert_eq!(groups.len(), 2);
        let g = groups.iter().find(|g| g.file == "chrome.exe").unwrap();
        assert_eq!(g.process_count, 3);
        assert_eq!(g.memory_bytes, 30);
        assert_eq!(g.disk_write_per_sec, 6);
        assert_eq!(g.child_count, 0, "its own processes are not children");
        assert_eq!(g.started, 100);
        assert_eq!(g.has_window, Some(false));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_apps_helpers_are_one_row_named_after_the_app() {
        let app = "/Applications/Visual Studio Code.app";
        let main = format!("{app}/Contents/MacOS/Electron");
        let helper = format!("{app}/Contents/Frameworks/Code Helper (Renderer).app/Contents/MacOS/Code Helper (Renderer)");
        let ext = "/Applications/AdBlock.app/Contents/PlugIns/SafariMenu.appex/Contents/MacOS/SafariMenu";
        let snap = snapshot(
            vec![
                proc(11, Some(10), "Code Helper (Renderer)", Some(&helper), 101),
                proc(10, None, "Electron", Some(&main), 100),
                proc(20, None, "SafariMenu", Some(ext), 100),
                proc(30, None, "AdBlock", Some("/Applications/AdBlock.app/Contents/MacOS/AdBlock"), 100),
            ],
            9999,
        );
        let mut wins = HashMap::new();
        wins.insert(10u32, WinInfo { hwnds: vec![1], hung: false });
        let groups = build_groups(&snap, &Some(wins), false);
        assert_eq!(groups.len(), 2, "{:?}", groups.iter().map(|g| &g.name).collect::<Vec<_>>());
        let code = groups.iter().find(|g| g.name == "Visual Studio Code").expect("named after the app");
        assert_eq!((code.process_count, code.child_count, code.file.as_str()), (2, 0, "Electron"));
        assert_eq!((code.has_window, code.safety), (Some(true), Safety::Caution));
        let adblock = groups.iter().find(|g| g.name == "AdBlock").expect("the extension is part of its app");
        assert_eq!(adblock.process_count, 2);
        assert!(is_main_program(&main) && !is_main_program(&helper));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_regular_apps_are_asked_to_quit_like_the_dock_does() {
        // Read-only: nothing is asked to quit here.
        assert!(!mac::is_app(std::process::id()), "a command line program is no app");
        assert!(!mac::is_app(1), "launchd");
        assert!(!mac::is_app(u32::MAX >> 1), "no such process");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn smoke_finder_is_a_regular_app() {
        let sys = System::new_all();
        let finder = sys.processes().values().find(|p| p.name() == "Finder").expect("Finder runs");
        assert!(mac::is_app(finder.pid().as_u32()));
    }

    #[test]
    fn apple_services_and_vpn_agents_are_protected() {
        for (name, exe) in [
            ("com.apple.CoreSimulator.CoreSimulatorService", "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/Versions/A/XPCServices/com.apple.CoreSimulator.CoreSimulatorService.xpc/Contents/MacOS/com.apple.CoreSimulator.CoreSimulatorService"),
            ("XProtectPluginService", "/Library/Apple/System/Library/CoreServices/XProtect.app/Contents/MacOS/XProtectPluginService"),
            ("PanGPA", "/Applications/GlobalProtect.app/Contents/MacOS/PanGPA"),
        ] {
            let r = rate(&facts(name, Some(exe)));
            assert_eq!((r.safety, r.offered), (Safety::Risky, false), "{name}");
        }
        // A protected helper protects the whole app it is part of.
        let mut f = facts("Some App", Some("/Applications/Some App.app/Contents/MacOS/Some App"));
        f.part_protected = Some("security software");
        assert!(!rate(&f).offered);
    }

    #[test]
    fn the_same_program_run_by_another_user_is_its_own_risky_row() {
        let exe = "C:\\Apps\\sync.exe";
        let mut other = proc(11, None, "sync.exe", Some(exe), 100);
        other.owner = Owner::Other;
        let snap = snapshot(vec![proc(10, None, "sync.exe", Some(exe), 100), other], 9999);
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        assert_eq!(groups.len(), 2);
        let mine = groups.iter().find(|g| g.owner == Owner::You).unwrap();
        let theirs = groups.iter().find(|g| g.owner == Owner::Other).unwrap();
        assert!(mine.offered);
        assert!(!theirs.offered);
    }

    #[test]
    fn children_are_counted_but_console_hosts_and_recycled_parents_are_not() {
        let term = "C:\\Apps\\term.exe";
        let snap = snapshot(
            vec![
                proc(10, None, "term.exe", Some(term), 100),
                proc(11, Some(10), "node.exe", Some("C:\\n\\node.exe"), 105),
                proc(12, Some(10), "conhost.exe", Some("C:\\Windows\\System32\\conhost.exe"), 105),
                // Started before its "parent": the id was recycled.
                proc(13, Some(10), "old.exe", Some("C:\\o\\old.exe"), 50),
            ],
            9999,
        );
        let g = build_groups(&snap, &Some(HashMap::new()), false).into_iter().find(|g| g.file == "term.exe").unwrap();
        assert_eq!(g.child_count, 1);
        assert_eq!(g.safety, Safety::Caution);
    }

    #[test]
    fn windows_make_a_row_caution_and_hung_windows_are_reported() {
        let exe = "C:\\Apps\\paint.exe";
        let snap = snapshot(vec![proc(10, None, "paint.exe", Some(exe), 100)], 9999);
        let mut w = HashMap::new();
        w.insert(10, WinInfo { hwnds: vec![1, 2], hung: true });
        let g = &build_groups(&snap, &Some(w), false)[0];
        assert_eq!(g.window_count, 2);
        assert_eq!(g.has_window, Some(true));
        assert!(g.not_responding);
        assert_eq!(g.safety, Safety::Caution);
    }

    #[test]
    fn without_window_detection_nothing_is_safe() {
        let snap = snapshot(vec![proc(10, None, "sync", Some("/Applications/Sync.app/sync"), 100)], 9999);
        let g = &build_groups(&snap, &None, false)[0];
        assert_eq!(g.has_window, None);
        assert_eq!(g.safety, Safety::Caution);
    }

    #[test]
    fn jharu_its_ancestors_and_descendants_are_hosts() {
        let snap = snapshot(
            vec![
                proc(1, None, "explorer.exe", Some("C:\\Windows\\explorer.exe"), 10),
                proc(2, Some(1), "node.exe", Some("C:\\n\\node.exe"), 20),
                proc(3, Some(2), "jharu.exe", Some("C:\\j\\jharu.exe"), 30),
                proc(4, Some(3), "msedgewebview2.exe", Some("C:\\w\\msedgewebview2.exe"), 40),
                proc(5, Some(4), "msedgewebview2.exe", Some("C:\\w\\msedgewebview2.exe"), 41),
                proc(6, Some(1), "unrelated.exe", Some("C:\\u\\unrelated.exe"), 25),
                // Claims Jharu as parent but started before it: recycled id.
                proc(7, Some(3), "stale.exe", Some("C:\\s\\stale.exe"), 5),
            ],
            3,
        );
        let hosts = host_pids(&snap.procs, 3);
        for pid in [1, 2, 3, 4, 5] {
            assert!(hosts.contains(&pid), "{pid}");
        }
        assert!(!hosts.contains(&6));
        assert!(!hosts.contains(&7));
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        for file in ["node.exe", "jharu.exe", "msedgewebview2.exe"] {
            let g = groups.iter().find(|g| g.file == file).unwrap();
            assert!(!g.offered, "{file}");
        }
        assert!(groups.iter().find(|g| g.file == "unrelated.exe").unwrap().offered);
    }

    #[test]
    fn parent_cycles_do_not_hang() {
        let snap = snapshot(
            vec![proc(1, Some(2), "a.exe", Some("C:\\a.exe"), 10), proc(2, Some(1), "b.exe", Some("C:\\b.exe"), 10)],
            1,
        );
        let hosts = host_pids(&snap.procs, 1);
        assert!(hosts.contains(&1));
    }

    // ----------------------------------------------------- pid reuse guard

    #[test]
    fn a_reused_pid_is_refused() {
        let exe = "C:\\Apps\\a.exe";
        let live = proc(10, None, "a.exe", Some(exe), 500);
        let id = group_id(&live);
        assert!(guard_member(&Member { pid: 10, start: 500 }, &live, &id).is_ok());
        let why = guard_member(&Member { pid: 10, start: 100 }, &live, &id).unwrap_err();
        assert!(why.contains("reused"), "{why}");
    }

    #[test]
    fn a_different_program_on_the_same_pid_is_refused() {
        let listed = proc(10, None, "a.exe", Some("C:\\Apps\\a.exe"), 500);
        let live = proc(10, None, "b.exe", Some("C:\\Apps\\b.exe"), 500);
        assert!(guard_member(&Member { pid: 10, start: 500 }, &live, &group_id(&listed)).is_err());
        // Same program, but now run by someone else.
        let mut other = listed.clone();
        other.owner = Owner::Other;
        assert!(guard_member(&Member { pid: 10, start: 500 }, &other, &group_id(&listed)).is_err());
    }

    fn target_for(snap: &Snapshot, pid: u32) -> Target {
        let p = snap.get(pid).unwrap();
        Target { id: group_id(p), name: p.name.clone(), members: vec![Member { pid, start: p.start }] }
    }

    #[test]
    fn vet_refuses_risky_rows_even_when_the_request_names_them() {
        let snap = snapshot(
            vec![
                proc(4, None, "System", None, 1),
                proc(10, None, "svchost.exe", Some("C:\\Windows\\System32\\svchost.exe"), 5),
                proc(11, None, "app.exe", Some("C:\\Apps\\app.exe"), 5),
            ],
            9999,
        );
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        for pid in [4, 10] {
            match vet(&target_for(&snap, pid), &snap, &groups) {
                Vetted::Refused(why) => assert!(why.contains("Protected") || why.contains("Jharu"), "{why}"),
                _ => panic!("pid {pid} was not refused"),
            }
        }
        assert!(matches!(vet(&target_for(&snap, 11), &snap, &groups), Vetted::Go { .. }));
    }

    #[test]
    fn vet_skips_a_process_whose_pid_was_reused_and_reports_one_that_is_gone() {
        let snap = snapshot(vec![proc(11, None, "app.exe", Some("C:\\Apps\\app.exe"), 900)], 9999);
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        let id = group_id(snap.get(11).unwrap());
        // Listed at start 100, now pid 11 started at 900: reused.
        let t = Target { id: id.clone(), name: "app".into(), members: vec![Member { pid: 11, start: 100 }] };
        match vet(&t, &snap, &groups) {
            Vetted::Go { alive, skipped, .. } => {
                assert!(alive.is_empty());
                assert_eq!(skipped.len(), 1);
            }
            _ => panic!("expected the reused pid to be skipped"),
        }
        // A pid that no longer exists counts as already closed.
        let t = Target { id, name: "app".into(), members: vec![Member { pid: 77, start: 100 }, Member { pid: 11, start: 900 }] };
        match vet(&t, &snap, &groups) {
            Vetted::Go { alive, gone, .. } => {
                assert_eq!(alive, vec![Member { pid: 11, start: 900 }]);
                assert_eq!(gone, vec![77]);
            }
            _ => panic!("expected one alive and one gone"),
        }
    }

    #[test]
    fn vet_ignores_new_members_that_were_not_in_the_request() {
        let exe = "C:\\Apps\\app.exe";
        let snap = snapshot(
            vec![proc(11, None, "app.exe", Some(exe), 900), proc(12, None, "app.exe", Some(exe), 901)],
            9999,
        );
        let groups = build_groups(&snap, &Some(HashMap::new()), false);
        match vet(&target_for(&snap, 11), &snap, &groups) {
            Vetted::Go { alive, .. } => assert_eq!(alive.len(), 1, "only what was listed is signalled"),
            _ => panic!(),
        }
    }

    // -------------------------------------------- real processes we start

    /// A throwaway program of our own that sleeps with no window: a copy of
    /// this test executable under a name nothing else uses, run so that the
    /// helper test below sleeps. It is the only thing these tests signal.
    struct Sleeper {
        child: std::process::Child,
        copy: std::path::PathBuf,
    }

    impl Sleeper {
        fn start(tag: &str) -> Sleeper {
            let me = std::env::current_exe().unwrap();
            let copy = std::env::temp_dir().join(format!("jharu-test-sleeper-{tag}-{}.exe", std::process::id()));
            std::fs::copy(&me, &copy).unwrap();
            let mut cmd = std::process::Command::new(&copy);
            cmd.args(["--exact", "processes::tests::sleeper_helper", "--nocapture", "--test-threads=1"])
                .env("JHARU_TEST_SLEEPER", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let child = cmd.spawn().unwrap();
            let s = Sleeper { child, copy };
            // Wait until the system lists it.
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(10) {
                if fresh().get(s.child.id()).is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            s
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        /// A snapshot in which Jharu is some other process, since this child
        /// descends from the test process and would otherwise count as part
        /// of Jharu.
        fn snapshot(&self) -> Snapshot {
            let mut snap = fresh();
            snap.self_pid = u32::MAX;
            snap
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            // Our own child, ended in any case so nothing is left behind.
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.copy);
        }
    }

    /// The helper the sleeper runs. In a normal test run it returns at once.
    #[test]
    fn sleeper_helper() {
        if std::env::var("JHARU_TEST_SLEEPER").is_ok() {
            std::thread::sleep(Duration::from_secs(120));
        }
    }

    #[test]
    fn closing_a_program_with_no_window_says_so_and_leaves_it_running() {
        let sleeper = Sleeper::start("close");
        let snap = sleeper.snapshot();
        let wins = visible_windows();
        let t = target_for(&snap, sleeper.pid());
        let out = close_in(&[t.clone()], &snap, &wins, &|| false, &|_, _, _| {}, Duration::from_millis(600));
        assert_eq!(out.results.len(), 1);
        let r = &out.results[0];
        if cfg!(target_os = "windows") {
            assert_eq!(r.status, Status::NoWindow, "{}", r.message);
            assert!(r.message.contains("no window"), "{}", r.message);
            assert!(r.message.contains("Force quit"), "{}", r.message);
            assert_eq!(r.running, vec![sleeper.pid()]);
            // Nothing was signalled, so it is still there.
            assert!(!still_alive(&t.members).is_empty());
        }
    }

    #[test]
    fn force_quit_ends_a_program_we_started_and_nothing_else() {
        let sleeper = Sleeper::start("force");
        let snap = sleeper.snapshot();
        let t = target_for(&snap, sleeper.pid());
        assert_eq!(still_alive(&t.members), vec![sleeper.pid()]);
        let out = force_in(&[t.clone()], &snap, &visible_windows(), &|_, _, _| {}, Duration::from_secs(5));
        let r = &out.results[0];
        assert_eq!(r.status, Status::Exited, "{}", r.message);
        assert!(r.message.contains("not saved"), "{}", r.message);
        assert!(still_alive(&t.members).is_empty());
    }

    #[test]
    fn force_quit_skips_a_reused_pid_and_leaves_the_real_process_alone() {
        let sleeper = Sleeper::start("reuse");
        let snap = sleeper.snapshot();
        let mut t = target_for(&snap, sleeper.pid());
        // The list says this pid started long ago: as if it were reused.
        t.members[0].start -= 1000;
        let out = force_in(&[t], &snap, &visible_windows(), &|_, _, _| {}, Duration::from_millis(500));
        assert_eq!(out.results[0].status, Status::Skipped, "{}", out.results[0].message);
        let live = [Member { pid: sleeper.pid(), start: snap.get(sleeper.pid()).unwrap().start }];
        assert_eq!(still_alive(&live), vec![sleeper.pid()]);
    }

    #[test]
    fn jharu_itself_is_refused_without_signalling() {
        // Looks only: vet decides, nothing is sent.
        let snap = fresh();
        let groups = build_groups(&snap, &visible_windows(), false);
        let me = snap.get(snap.self_pid).unwrap();
        let t = Target { id: group_id(me), name: "test".into(), members: vec![Member { pid: me.pid, start: me.start }] };
        assert!(matches!(vet(&t, &snap, &groups), Vetted::Refused(_)));
    }

    /// Prints the real list, read only. `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smoke_prints_the_real_list() {
        let snap = sample();
        let wins = visible_windows();
        println!("windows visible: {}", wins.as_ref().map(|w| w.len().to_string()).unwrap_or("unknown".into()));
        println!(
            "cpu {:.1}%  mem {}/{}  swap {}/{}  read {}/s write {}/s  procs {}",
            snap.totals.cpu_percent,
            snap.totals.memory_used,
            snap.totals.memory_total,
            snap.totals.swap_used,
            snap.totals.swap_total,
            snap.totals.disk_read_per_sec,
            snap.totals.disk_write_per_sec,
            snap.totals.process_count
        );
        for g in build_groups(&snap, &wins, true) {
            println!(
                "{:<8?} {:<7} {:>5.1}% {:>9} KB  win={:?} n={} kids={}  {} ({})  [{}]",
                g.safety,
                if g.offered { "offered" } else { "refused" },
                g.cpu_percent,
                g.memory_bytes / 1024,
                g.has_window,
                g.process_count,
                g.child_count,
                g.name,
                g.file,
                g.summary
            );
        }
    }
}
