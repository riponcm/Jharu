//! Cleaning folders that the operating system or running programs keep using:
//! temp folders, crash dumps, update caches, logs.
//!
//! Two lessons from the first Windows test round shape this module:
//!
//! - Moving such a folder as a whole never works. Some program always has a
//!   file open inside `%TEMP%`, and Windows then aborts the entire move, so
//!   nothing was cleaned at all. Even if it had worked, the folder itself
//!   would be gone, and programs expect it to exist. So only the folder's
//!   contents are moved, one entry at a time, and entries still in use are
//!   left behind and counted.
//! - `C:\Windows\Temp`, the Windows Update cache and the error reporting
//!   archive belong to the system. Jharu does not run as administrator;
//!   instead it relaunches itself elevated for just those locations, which
//!   shows one Windows permission prompt.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Entries changed more recently than this are left alone: an installer or a
/// running program may still be using them.
const MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Entries handed to the Recycle Bin per call. One call per entry is slow when
/// a temp folder holds thousands; one call for all hides which ones failed.
const BATCH: usize = 50;
/// How many names of entries left in place to quote back.
const NAME_LIMIT: usize = 5;

/// The command-line flag that starts Jharu as the elevated cleaning helper.
pub const HELPER_FLAG: &str = "--jharu-elevated-clean";

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct LocationOutcome {
    pub freed_bytes: u64,
    pub moved: u64,
    /// Entries that could not be moved, nearly always because a running
    /// program has them open.
    pub left: u64,
    pub left_names: Vec<String>,
    /// Entries skipped because they changed within the last day.
    pub recent: u64,
    /// Administrator permission was needed and the user declined it.
    pub admin_declined: bool,
    pub errors: Vec<String>,
}

impl LocationOutcome {
    fn merge(&mut self, other: LocationOutcome) {
        self.freed_bytes += other.freed_bytes;
        self.moved += other.moved;
        self.left += other.left;
        for n in other.left_names {
            if self.left_names.len() < NAME_LIMIT {
                self.left_names.push(n);
            }
        }
        self.recent += other.recent;
        self.admin_declined |= other.admin_declined;
        self.errors.extend(other.errors);
    }
}

/// A folder whose contents Jharu may clean, by a fixed id. The elevated
/// helper only accepts these ids and works out the paths itself, so nothing
/// it is handed can point it at an arbitrary folder.
pub struct Location {
    pub roots: Vec<PathBuf>,
    pub admin: bool,
}

pub fn location(id: &str) -> Option<Location> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "windows")]
    {
        let local = dirs::data_local_dir().unwrap_or_else(|| home.join("AppData/Local"));
        let windir = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()));
        let wer = PathBuf::from("C:\\ProgramData\\Microsoft\\Windows\\WER");
        let delivery = windir.join("ServiceProfiles/NetworkService/AppData/Local/Microsoft/Windows/DeliveryOptimization");
        let (roots, admin) = match id {
            "user-temp" => (vec![local.join("Temp")], false),
            "crash-dumps" => (vec![local.join("CrashDumps")], false),
            "system-temp" => (vec![windir.join("Temp")], true),
            "update-downloads" => (vec![windir.join("SoftwareDistribution/Download")], true),
            // Only the cache and logs; the State folder is the service's own
            // database, not downloaded data.
            "delivery-optimization" => (vec![delivery.join("Cache"), delivery.join("Logs")], true),
            // The report folders, not WER itself, whose layout the error
            // reporting service expects to find.
            "error-reports" => (vec![wer.join("ReportArchive"), wer.join("ReportQueue"), wer.join("Temp")], true),
            _ => return None,
        };
        return Some(Location { roots, admin });
    }
    #[cfg(target_os = "macos")]
    {
        let roots = match id {
            "mac-logs" => vec![home.join("Library/Logs")],
            "mail-downloads" => vec![home.join("Library/Containers/com.apple.mail/Data/Library/Mail Downloads")],
            _ => return None,
        };
        return Some(Location { roots, admin: false });
    }
    #[allow(unreachable_code)]
    {
        let _ = (home, id);
        None
    }
}

/// Size of an entry and the newest modification time anywhere inside it. A
/// folder's own time only changes when its direct children do, so a browser
/// profile in use can look days old from the outside.
fn entry_stats(path: &Path) -> (u64, Option<SystemTime>) {
    let own = path.symlink_metadata().ok();
    let mut newest = own.as_ref().and_then(|m| m.modified().ok());
    if !path.is_dir() {
        return (own.map(|m| m.len()).unwrap_or(0), newest);
    }
    let mut size = 0u64;
    for entry in crate::walk::walk(path).skip_hidden(false).into_iter().flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if entry.file_type().is_file() {
            size += meta.len();
        }
        if let Ok(m) = meta.modified() {
            if newest.map_or(true, |n| m > n) {
                newest = Some(m);
            }
        }
    }
    (size, newest)
}

/// Moves everything inside `root` to the Trash / Recycle Bin, keeping `root`
/// itself. Entries with anything changed within `min_age`, and entries in
/// use, stay where they are. `progress` hears (done, total) per batch.
pub fn clean_contents(root: &Path, min_age: Duration, out: &mut LocationOutcome, progress: &mut dyn FnMut(usize, usize)) {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            out.errors.push(format!("{}: {e}", root.display()));
            return;
        }
    };
    let now = SystemTime::now();
    // Finder's view settings are not junk, and moving anything to the Trash
    // through Finder writes a fresh one into this folder, so trashing it only
    // trades one for another on every clean.
    let entries: Vec<PathBuf> = rd.flatten().filter(|e| e.file_name() != ".DS_Store").map(|e| e.path()).collect();
    let total = entries.len();
    let mut candidates: Vec<(PathBuf, u64)> = vec![];
    for path in entries {
        let (size, newest) = entry_stats(&path);
        let old_enough = newest
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= min_age);
        if old_enough {
            candidates.push((path, size));
        } else {
            out.recent += 1;
        }
    }
    let mut done = total - candidates.len();
    progress(done, total);

    for batch in candidates.chunks(BATCH) {
        let paths: Vec<&PathBuf> = batch.iter().map(|(p, _)| p).collect();
        // Windows moves what it can and reports the rest as "aborted", so the
        // truth is whatever is no longer there afterwards.
        let result = crate::trashlog::to_bin(&paths);
        if result.is_err() && batch.len() > 1 && batch.iter().all(|(p, _)| p.symlink_metadata().is_ok()) {
            // The batch failed before moving anything (one bad path can stop
            // it up front); give every entry its own chance.
            for (p, _) in batch {
                let _ = crate::trashlog::to_bin(&[p]);
            }
        }
        for (p, size) in batch {
            if p.symlink_metadata().is_err() {
                out.moved += 1;
                out.freed_bytes += size;
                crate::trashlog::record(p, *size, "System folders");
            } else {
                out.left += 1;
                if out.left_names.len() < NAME_LIMIT {
                    out.left_names.push(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                }
            }
        }
        done += batch.len();
        progress(done, total);
    }
}

fn clean_location(loc: &Location, out: &mut LocationOutcome, progress: &mut dyn FnMut(usize, usize)) {
    for root in &loc.roots {
        clean_contents(root, MIN_AGE, out, progress);
    }
}

#[derive(Serialize, Clone)]
struct CleanProgress {
    token: String,
    /// Which folder is being worked on, for the progress line.
    label: String,
    done: usize,
    total: usize,
}

#[tauri::command]
pub async fn clean_locations(
    app: tauri::AppHandle,
    ids: Vec<String>,
    token: String,
) -> Result<LocationOutcome, String> {
    use tauri::Emitter;
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = crate::autocare::activity();
        let mut out = LocationOutcome::default();
        let mut needs_admin: Vec<String> = vec![];
        for id in ids {
            match location(&id) {
                None => out.errors.push(format!("Unknown location {id}")),
                Some(loc) if loc.admin && !elevation::is_elevated() => needs_admin.push(id),
                Some(loc) => {
                    let mut progress = |done: usize, total: usize| {
                        let _ = app.emit(
                            "clean-progress",
                            CleanProgress { token: token.clone(), label: id.clone(), done, total },
                        );
                    };
                    clean_location(&loc, &mut out, &mut progress);
                }
            }
        }
        if !needs_admin.is_empty() {
            // The helper is a separate process; the UI shows it as waiting.
            let _ = app.emit(
                "clean-progress",
                CleanProgress { token: token.clone(), label: "admin".into(), done: 0, total: 0 },
            );
            out.merge(elevation::run_elevated(&needs_admin));
        }
        out
    })
    .await
    .map_err(|e| e.to_string())
}

/// Entry point when Jharu was started as the elevated helper:
/// `Jharu.exe --jharu-elevated-clean <result.json> <location id>...`.
/// Returns the process exit code, or None for a normal app start.
pub fn helper_main(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(HELPER_FLAG) {
        return None;
    }
    let Some(result_path) = args.get(2) else { return Some(2) };
    let mut out = LocationOutcome::default();
    for id in &args[3..] {
        // Only the fixed administrator locations; anything else is refused.
        match location(id) {
            Some(loc) if loc.admin => clean_location(&loc, &mut out, &mut |_, _| {}),
            _ => out.errors.push(format!("Refused unknown location {id}")),
        }
    }
    let json = serde_json::to_vec(&out).unwrap_or_default();
    // create_new refuses to follow a file or link planted at this path, so the
    // elevated process cannot be tricked into overwriting something else.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(result_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &json));
    Some(if written.is_ok() { 0 } else { 3 })
}

#[cfg(target_os = "windows")]
mod elevation {
    use super::{LocationOutcome, HELPER_FLAG};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
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

    /// What came back from an elevated helper run.
    #[derive(Default)]
    pub struct Helper {
        /// The user declined the Windows permission prompt.
        pub declined: bool,
        pub errors: Vec<String>,
        /// The JSON the helper wrote, when it wrote one.
        pub result: Option<Vec<u8>>,
    }

    /// Relaunches Jharu elevated as `<flag> <result.json> <args>...`, which
    /// shows the Windows permission prompt, and waits for it to finish. The
    /// helper reports back through the result file. Callers pass only fixed
    /// ids as `args`, never text a user or a file could shape.
    pub fn run_helper(flag: &str, args: &[String]) -> Helper {
        let mut out = Helper::default();
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
        let result_path = std::env::temp_dir().join(format!("jharu-elevated-{}-{nonce}.json", std::process::id()));
        let params = format!("{flag} \"{}\" {}", result_path.display(), args.join(" "));

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
                    out.declined = true;
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
                out.result = Some(bytes);
            }
            Err(e) => out.errors.push(format!("No result from the administrator helper: {e}")),
        }
        out
    }

    /// Relaunches Jharu elevated to clean the given locations.
    pub fn run_elevated(ids: &[String]) -> LocationOutcome {
        let mut out = LocationOutcome::default();
        let helper = run_helper(HELPER_FLAG, ids);
        out.admin_declined = helper.declined;
        out.errors.extend(helper.errors);
        if let Some(bytes) = helper.result {
            match serde_json::from_slice::<LocationOutcome>(&bytes) {
                Ok(o) => out.merge(o),
                Err(e) => out.errors.push(format!("Unreadable result from the administrator helper: {e}")),
            }
        }
        out
    }
}

#[cfg(not(target_os = "windows"))]
mod elevation {
    use super::LocationOutcome;

    /// No location on macOS needs administrator rights today.
    pub fn is_elevated() -> bool {
        true
    }

    pub fn run_elevated(_ids: &[String]) -> LocationOutcome {
        LocationOutcome {
            errors: vec!["Administrator cleaning is only implemented on Windows".into()],
            ..Default::default()
        }
    }

    /// What came back from an elevated helper run (never started off Windows).
    #[derive(Default)]
    #[allow(dead_code)]
    pub struct Helper {
        pub declined: bool,
        pub errors: Vec<String>,
        pub result: Option<Vec<u8>>,
    }

    #[allow(dead_code)]
    pub fn run_helper(_flag: &str, _args: &[String]) -> Helper {
        Helper { errors: vec!["The administrator helper is only implemented on Windows".into()], ..Default::default() }
    }
}

// Other modules reuse the same elevation: the system space tab runs its own
// fixed commands through `run_helper` with its own flag.
#[cfg_attr(not(target_os = "windows"), allow(unused_imports))]
pub use elevation::{is_elevated, run_helper};

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("jharu clean {tag} {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn age(path: &Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 86_400);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn cleans_contents_but_keeps_folder_recent_and_in_use() {
        let root = fixture("contents");
        std::fs::write(root.join("old.log"), vec![b'x'; 4096]).unwrap();
        age(&root.join("old.log"), 10);
        std::fs::write(root.join("fresh.tmp"), b"being written").unwrap();
        std::fs::create_dir(root.join("old dir")).unwrap();
        std::fs::write(root.join("old dir").join("inner.bin"), vec![b'y'; 1000]).unwrap();
        age(&root.join("old dir").join("inner.bin"), 10);
        std::fs::write(root.join("held.dat"), b"open elsewhere").unwrap();
        age(&root.join("held.dat"), 10);
        // An old-looking folder with something fresh inside, like a browser
        // profile a running test harness is using.
        std::fs::create_dir(root.join("busy dir")).unwrap();
        std::fs::write(root.join("busy dir").join("live.txt"), b"in use today").unwrap();

        // Hold one file open the way a running program would. (Directories'
        // own times move when their contents change, so age the folder last.)
        #[cfg(target_os = "windows")]
        let _held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).share_mode(0).open(root.join("held.dat")).unwrap()
        };
        let dir_time = SystemTime::now() - Duration::from_secs(10 * 86_400);
        for name in ["old dir", "busy dir"] {
            #[cfg(target_os = "windows")]
            let dir = {
                use std::os::windows::fs::OpenOptionsExt;
                const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
                const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
                std::fs::OpenOptions::new()
                    .access_mode(FILE_WRITE_ATTRIBUTES)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                    .open(root.join(name))
                    .unwrap()
            };
            #[cfg(not(target_os = "windows"))]
            let dir = std::fs::File::open(root.join(name)).unwrap();
            dir.set_modified(dir_time).unwrap();
        }

        let mut out = LocationOutcome::default();
        let mut steps = vec![];
        clean_contents(&root, MIN_AGE, &mut out, &mut |d, t| steps.push((d, t)));
        assert_eq!(steps.last(), Some(&(5, 5)), "progress ends at the total");
        println!("{out:?}");

        assert!(root.is_dir(), "the folder itself must stay");
        assert!(!root.join("old.log").exists());
        assert!(!root.join("old dir").exists());
        assert!(root.join("fresh.tmp").exists(), "recent entries stay");
        assert!(root.join("busy dir").join("live.txt").exists(), "a folder with fresh contents stays");
        assert_eq!(out.recent, 2);
        assert!(out.errors.is_empty());
        #[cfg(target_os = "windows")]
        {
            assert!(root.join("held.dat").exists(), "files in use stay");
            assert_eq!(out.left, 1);
            assert_eq!(out.left_names, vec!["held.dat".to_string()]);
        }
        assert!(out.freed_bytes >= 4096);

        // Put the moved entries back so the user's bin is left as found.
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        {
            let key = root.to_string_lossy().to_lowercase();
            let ours: Vec<_> = trash::os_limited::list()
                .unwrap()
                .into_iter()
                .filter(|t| t.original_parent.to_string_lossy().to_lowercase() == key)
                .collect();
            assert_eq!(ours.len() as u64, out.moved);
            trash::os_limited::restore_all(ours).unwrap();
            // The trash crate restores Windows files without their extension
            // (old.log comes back as "old"); only the folder name is exact.
            assert!(root.join("old dir").join("inner.bin").exists());
        }
        #[cfg(target_os = "windows")]
        drop(_held);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn helper_refuses_unknown_locations_and_writes_once() {
        let root = fixture("helper");
        let result = root.join("result.json");
        let args: Vec<String> = ["Jharu.exe", HELPER_FLAG, &result.to_string_lossy(), "C:\\Windows\\System32", "user-temp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(helper_main(&args), Some(0));
        let out: LocationOutcome = serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
        // A raw path and a non-administrator location are both refused.
        assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
        assert_eq!(out.moved, 0);
        // A second run must not overwrite an existing file.
        assert_eq!(helper_main(&args), Some(3));
        assert_eq!(helper_main(&["Jharu.exe".to_string()]), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
