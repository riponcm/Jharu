//! The floating gadget: a small always-on-top window with the numbers that
//! matter (CPU, memory, free space on the system drive and when it runs out,
//! the busiest programs) and one safe action. Off until the user turns it on.
//!
//! It shows as a slim bar or an expanded card. It must cost less than it
//! saves, so it never walks the disk: each refresh reads CPU, memory and the
//! process table (a few milliseconds), and the forecast comes from the
//! readings Space history already keeps. Its one action, cleaning safe
//! caches, goes through autocare's preview and run, the same checks and the
//! same Recycle Bin as everywhere else.

use crate::spacehistory;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "gadget";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Bar,
    Card,
}

impl Mode {
    /// Starting window size in logical pixels. The page then measures
    /// itself and asks for its exact size (gadget_fit), so the rounded
    /// corners meet the window's edges and nothing is cut off.
    fn size(self) -> (f64, f64) {
        match self {
            Mode::Bar => (470.0, 44.0),
            Mode::Card => (276.0, 360.0),
        }
    }

    /// The shell's corner radius in gadget.css (.g-bar, .g-card).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn radius(self) -> f64 {
        match self {
            Mode::Bar => 22.0,
            Mode::Card => 16.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Settings {
    /// Off by default: the gadget only exists when the user asks for it.
    pub enabled: bool,
    pub mode: Mode,
    /// Where the user last left it: `right` is its right edge, which stays
    /// put when it takes its content's width (gadget_fit), and `y` its top.
    /// Physical pixels, or points on macOS, where screens of different
    /// scales share one space of points. None puts it in the top-right
    /// corner of the main screen. Older versions saved the left edge as
    /// `x`; that is ignored, so the gadget starts top-right once.
    pub right: Option<i32>,
    pub y: Option<i32>,
}

fn settings_file(dir: &Path) -> PathBuf {
    dir.join("gadget.json")
}

pub fn load(dir: &Path) -> Settings {
    std::fs::read_to_string(settings_file(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save(dir: &Path, s: &Settings) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(settings_file(dir), serde_json::to_string_pretty(s).unwrap_or_default())
        .map_err(|e| format!("Could not save the gadget settings: {e}"))
}

fn dir_of(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    spacehistory::data_dir(app).ok_or_else(|| "Jharu could not open its data folder.".to_string())
}

// ---------------------------------------------------------------- window

/// A position that is still on one of the screens, so a gadget left on a
/// monitor that has since been unplugged does not open out of sight.
fn on_screen(app: &tauri::AppHandle, x: i32, y: i32) -> bool {
    app.available_monitors().unwrap_or_default().iter().any(|m| {
        let (p, s) = (m.position(), m.size());
        // Saved positions are in points on macOS (see Settings).
        let k = if cfg!(target_os = "macos") { m.scale_factor() } else { 1.0 };
        let (px, py) = ((p.x as f64 / k) as i32, (p.y as f64 / k) as i32);
        let (sw, sh) = ((s.width as f64 / k) as i32, (s.height as f64 / k) as i32);
        x >= px - 20 && y >= py - 20 && x < px + sw - 40 && y < py + sh - 30
    })
}

/// Opens the gadget, or brings it back and gives it the size of `s.mode`.
fn show(app: &tauri::AppHandle, s: &Settings) -> Result<(), String> {
    let (w, h) = s.mode.size();
    if let Some(win) = app.get_webview_window(LABEL) {
        resize_keep_right(&win, w, h)?;
        #[cfg(target_os = "macos")]
        mac::round_corners(&win, s.mode.radius());
        win.show().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html?view=gadget".into()))
        .title("Jharu gadget")
        .inner_size(w, h)
        .decorations(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        // Windows draws its window shadow as a square box, which showed
        // behind the rounded corners. The page draws its own edge instead.
        .shadow(false)
        .focused(false)
        .visible(false);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.transparent(true);
    let win = builder.build().map_err(|e| format!("Could not open the gadget: {e}"))?;
    #[cfg(target_os = "macos")]
    mac::round_corners(&win, s.mode.radius());

    match (s.right, s.y) {
        (Some(right), Some(y)) if on_screen(app, right - 40, y) => {
            let _ = if cfg!(target_os = "macos") {
                win.set_position(tauri::LogicalPosition::new(right as f64 - w, y as f64))
            } else {
                let scale = win.scale_factor().unwrap_or(1.0);
                win.set_position(tauri::PhysicalPosition::new(right - (w * scale).round() as i32, y))
            };
        }
        _ => {
            // Top-right of the main screen, below where a maximized
            // window keeps its title bar buttons.
            if let Ok(Some(m)) = win.primary_monitor() {
                let scale = m.scale_factor();
                // macOS places windows in points across all screens, so with
                // two screens at different scales a physical position lands
                // on the wrong one.
                if cfg!(target_os = "macos") {
                    let (mx, my) = (m.position().x as f64 / scale, m.position().y as f64 / scale);
                    let x = mx + m.size().width as f64 / scale - (w + 24.0);
                    let _ = win.set_position(tauri::LogicalPosition::new(x, my + 64.0));
                } else {
                    let right = m.position().x + m.size().width as i32;
                    let x = right - ((w + 24.0) * scale) as i32;
                    let y = m.position().y + (64.0 * scale) as i32;
                    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
                }
            }
        }
    }

    // Remember where the user drags it. Moves arrive many times a second
    // while dragging, so only the last one is written, half a second later.
    let handle = app.clone();
    win.on_window_event(move |e| {
        if let tauri::WindowEvent::Moved(p) = e {
            let Some(w) = handle.get_webview_window(LABEL) else { return };
            let width = w.outer_size().map(|s| s.width as i32).unwrap_or(0);
            let right = tauri::PhysicalPosition::new(p.x + width, p.y);
            if cfg!(target_os = "macos") {
                // The scale of the screen it is on now.
                let l = right.to_logical::<f64>(w.scale_factor().unwrap_or(1.0));
                remember_position(&handle, l.x.round() as i32, l.y.round() as i32);
            } else {
                remember_position(&handle, right.x, right.y);
            }
        }
    });
    win.show().map_err(|e| e.to_string())?;
    Ok(())
}

fn remember_position(app: &tauri::AppHandle, x: i32, y: i32) {
    static PENDING: Mutex<Option<(i32, i32)>> = Mutex::new(None);
    let first = PENDING.lock().map(|mut p| p.replace((x, y)).is_none()).unwrap_or(false);
    if !first {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let Some((x, y)) = PENDING.lock().ok().and_then(|mut p| p.take()) else { return };
        if let Ok(dir) = dir_of(&app) {
            let mut s = load(&dir);
            s.right = Some(x);
            s.y = Some(y);
            let _ = save(&dir, &s);
        }
    });
}

fn hide(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.close();
    }
}

/// Called once from the app's setup: brings the gadget back if it was on.
pub fn restore(app: &tauri::AppHandle) {
    if let Ok(dir) = dir_of(app) {
        let s = load(&dir);
        if s.enabled {
            let _ = show(app, &s);
        }
    }
}

// ---------------------------------------------------------------- snapshot

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Program {
    pub name: String,
    /// Share of the whole machine, 0 to 100.
    pub cpu: f32,
    pub memory: u64,
    pub processes: usize,
}

#[derive(Serialize, Clone, Debug)]
pub struct Drive {
    pub mount: String,
    pub free: u64,
    pub total: u64,
    pub days_until_full: Option<u64>,
    /// Space history's sentence, or why there is no forecast yet.
    pub forecast: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct Snapshot {
    pub cpu: f32,
    pub memory_used: u64,
    pub memory_total: u64,
    pub drive: Option<Drive>,
    /// Busiest programs by CPU, then memory. Jharu's own windows excluded.
    pub top: Vec<Program>,
    pub processes: usize,
}

/// One System kept between refreshes: CPU use is measured as the change
/// since the last refresh, so a fresh System would always read 0 %.
fn system() -> &'static Mutex<sysinfo::System> {
    static SYS: std::sync::OnceLock<Mutex<sysinfo::System>> = std::sync::OnceLock::new();
    SYS.get_or_init(|| Mutex::new(sysinfo::System::new()))
}

/// Groups processes by name and returns the busiest `n`. Idle pseudo
/// processes and this app itself are left out.
pub fn busiest(rows: &[(String, f32, u64)], cpus: usize, n: usize) -> Vec<Program> {
    let mut by: Vec<Program> = vec![];
    for (name, cpu, mem) in rows {
        let lower = name.to_lowercase();
        let key = lower.strip_suffix(".exe").unwrap_or(&lower);
        if matches!(key, "system idle process" | "idle" | "jharu" | "msedgewebview2" | "kernel_task") {
            continue;
        }
        let pretty = name.strip_suffix(".exe").unwrap_or(name).to_string();
        match by.iter_mut().find(|p| p.name.eq_ignore_ascii_case(&pretty)) {
            Some(p) => {
                p.cpu += cpu;
                p.memory += mem;
                p.processes += 1;
            }
            None => by.push(Program { name: pretty, cpu: *cpu, memory: *mem, processes: 1 }),
        }
    }
    // sysinfo gives each process up to 100 % per core.
    let cores = cpus.max(1) as f32;
    for p in &mut by {
        p.cpu = (p.cpu / cores).min(100.0);
    }
    by.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.memory.cmp(&a.memory)));
    by.truncate(n);
    by
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

fn drive(dir: Option<&Path>) -> Option<Drive> {
    let mount = system_mount();
    let key = spacehistory::path_key(&mount);
    let reading = spacehistory::read_drives().into_iter().find(|d| spacehistory::path_key(&d.mount) == key)?;
    let forecast = dir.and_then(|dir| {
        spacehistory::forecasts(&spacehistory::load(dir), spacehistory::now())
            .into_iter()
            .find(|f| spacehistory::path_key(&f.mount) == key)
    });
    // "C:\" reads as "C:"; the root "/" stays "/".
    let shown = reading.mount.trim_end_matches(['\\', '/']);
    Some(Drive {
        mount: if shown.is_empty() { "/".into() } else { shown.into() },
        free: reading.free,
        total: reading.total,
        days_until_full: forecast.as_ref().and_then(|f| f.days_until_full),
        forecast: match forecast {
            // Space history's own sentence explains the readings it still
            // needs at length; the gadget has room for one line.
            Some(f) if f.confidence != "none" => f.summary,
            _ => "No forecast yet: it needs a few days of free-space readings.".into(),
        },
    })
}

fn snapshot_impl(dir: Option<&Path>) -> Snapshot {
    let mut sys = system().lock().unwrap_or_else(|e| e.into_inner());
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let rows: Vec<(String, f32, u64)> =
        sys.processes().values().map(|p| (p.name().to_string_lossy().into_owned(), p.cpu_usage(), p.memory())).collect();
    Snapshot {
        cpu: sys.global_cpu_usage(),
        memory_used: sys.used_memory(),
        memory_total: sys.total_memory(),
        drive: drive(dir),
        top: busiest(&rows, sys.cpus().len(), 3),
        processes: rows.len(),
    }
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn gadget_get(app: tauri::AppHandle) -> Result<Settings, String> {
    Ok(load(&dir_of(&app)?))
}

/// Turns the gadget on or off and sets its size. Position is kept.
#[tauri::command]
pub async fn gadget_set(app: tauri::AppHandle, enabled: bool, mode: Mode) -> Result<Settings, String> {
    let dir = dir_of(&app)?;
    let mut s = load(&dir);
    s.enabled = enabled;
    s.mode = mode;
    save(&dir, &s)?;
    // Windows are created and closed on the main thread.
    let (handle, shown) = (app.clone(), s.clone());
    app.run_on_main_thread(move || {
        if shown.enabled {
            let _ = show(&handle, &shown);
        } else {
            hide(&handle);
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(s)
}

/// Sizes the gadget window to its content, as measured by the page.
#[tauri::command]
pub async fn gadget_fit(app: tauri::AppHandle, width: f64, height: f64) -> Result<(), String> {
    let Some(win) = app.get_webview_window(LABEL) else { return Ok(()) };
    let (w, h) = (width.clamp(40.0, 800.0).ceil(), height.clamp(40.0, 800.0).ceil());
    resize_keep_right(&win, w, h)
}

/// Resizes the gadget with its right edge where it is: it opens at the
/// top-right with a starting size, then takes its content's size, and a
/// switch between bar and card should not push it off the screen.
fn resize_keep_right(win: &tauri::WebviewWindow, w: f64, h: f64) -> Result<(), String> {
    let before = (win.outer_position(), win.outer_size(), win.scale_factor());
    win.set_size(tauri::LogicalSize::new(w, h)).map_err(|e| e.to_string())?;
    if let (Ok(pos), Ok(size), Ok(scale)) = before {
        let right = pos.x + size.width as i32;
        let _ = win.set_position(tauri::PhysicalPosition::new(right - (w * scale).round() as i32, pos.y));
    }
    Ok(())
}

/// The numbers the gadget shows. Cheap: no disk walk.
#[tauri::command]
pub async fn gadget_snapshot(app: tauri::AppHandle) -> Result<Snapshot, String> {
    let dir = spacehistory::data_dir(&app);
    tauri::async_runtime::spawn_blocking(move || snapshot_impl(dir.as_deref())).await.map_err(|e| e.to_string())
}

/// Brings Jharu's main window forward, re-opening it if it was closed while
/// the gadget stayed up.
#[tauri::command]
pub async fn gadget_open_main(app: tauri::AppHandle) -> Result<(), String> {
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Some(win) = handle.get_webview_window("main") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
        } else if let Some(cfg) = handle.config().app.windows.first().cloned() {
            if let Ok(win) = WebviewWindowBuilder::from_config(&handle, &cfg).and_then(|b| b.build()) {
                let _ = win.set_focus();
            }
        }
    })
    .map_err(|e| e.to_string())
}

/// macOS: rounded corners without the private transparency API. The window
/// gets a clear background and its content view is clipped to the shell's
/// radius, all with public AppKit calls. Runs on the main thread (show()).
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void};

    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    type Id = *mut c_void;

    pub fn round_corners(win: &tauri::WebviewWindow, radius: f64) {
        let Ok(window) = win.ns_window() else { return };
        // objc_msgSend must be called through each method's exact type.
        unsafe {
            let get: extern "C" fn(Id, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
            let set_bool: extern "C" fn(Id, Id, i8) = std::mem::transmute(objc_msgSend as *const ());
            let set_id: extern "C" fn(Id, Id, Id) = std::mem::transmute(objc_msgSend as *const ());
            let set_f64: extern "C" fn(Id, Id, f64) = std::mem::transmute(objc_msgSend as *const ());
            let sel = |s: &std::ffi::CStr| sel_registerName(s.as_ptr());

            let window = window as Id;
            set_bool(window, sel(c"setOpaque:"), 0);
            let clear = get(objc_getClass(c"NSColor".as_ptr()), sel(c"clearColor"));
            set_id(window, sel(c"setBackgroundColor:"), clear);
            let view = get(window, sel(c"contentView"));
            if view.is_null() {
                return;
            }
            set_bool(view, sel(c"setWantsLayer:"), 1);
            let layer = get(view, sel(c"layer"));
            if !layer.is_null() {
                set_f64(layer, sel(c"setCornerRadius:"), radius);
                set_bool(layer, sel(c"setMasksToBounds:"), 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_off_and_survive_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("jharu gadget {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), Settings::default());
        assert!(!load(&dir).enabled, "the gadget is opt-in");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(settings_file(&dir), "{ not json").unwrap();
        assert_eq!(load(&dir), Settings::default());

        let s = Settings { enabled: true, mode: Mode::Card, right: Some(10), y: Some(-5) };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), s);
        // The left edge an older version saved as `x` is not read as the
        // right edge: the gadget starts top-right instead of a width off.
        std::fs::write(settings_file(&dir), r#"{"enabled":true,"x":1935,"y":39}"#).unwrap();
        assert_eq!(load(&dir), Settings { enabled: true, y: Some(39), ..Settings::default() });
        // A file from an older version without some fields still loads.
        std::fs::write(settings_file(&dir), r#"{"enabled":true}"#).unwrap();
        assert_eq!(load(&dir), Settings { enabled: true, ..Settings::default() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn busiest_groups_by_program_and_leaves_out_idle_and_itself() {
        let rows = vec![
            ("chrome.exe".to_string(), 40.0, 300),
            ("chrome.exe".to_string(), 20.0, 200),
            ("System Idle Process".to_string(), 700.0, 0),
            ("Jharu.exe".to_string(), 90.0, 50),
            ("code.exe".to_string(), 10.0, 900),
            ("node.exe".to_string(), 10.0, 100),
        ];
        let top = busiest(&rows, 4, 3);
        assert_eq!(top.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["chrome", "code", "node"]);
        assert_eq!(top[0].processes, 2);
        assert_eq!(top[0].memory, 500);
        assert!((top[0].cpu - 15.0).abs() < 0.01, "60 % of one core on 4 cores is 15 % of the machine");
        // Ties on CPU go to the one using more memory.
        assert!(top[1].memory > top[2].memory);
    }

    #[test]
    fn snapshot_reads_the_machine() {
        let s = snapshot_impl(None);
        assert!(s.memory_total > 0 && s.memory_used <= s.memory_total);
        assert!(s.processes > 0);
        assert!(s.top.len() <= 3);
        let d = s.drive.expect("the system drive has a reading");
        assert!(d.total > 0 && d.free <= d.total);
        assert!(d.days_until_full.is_none(), "no history given, so no forecast");
    }
}
