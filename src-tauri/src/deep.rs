use crate::places::{verdict, Signals, Verdict};
use crate::safety::Facts;
use crate::usage::{Seen, Usage};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::Emitter;

#[derive(Serialize, Clone)]
pub struct DeepEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub file_count: u64,
    /// system | applications | user_data | app_data | dev | other
    pub kind: String,
    /// Protected locations can be explored but never cleaned from the app.
    pub protected: bool,
    pub hint: String,
    /// Cloud files stored only online; not included in `size_bytes`.
    pub online_only_bytes: u64,
    /// Hard links to files already counted in an earlier entry of this scan;
    /// not included in `size_bytes`.
    pub shared_bytes: u64,
    /// Folders and files inside that could not be read; when above 0,
    /// `size_bytes` is a floor rather than the truth.
    pub unreadable: u64,
    /// A summary row (space in use that could not be measured), not a path.
    pub is_virtual: bool,
    /// Whether removing it is safe, and why. None for protected locations
    /// and summary rows, which cannot be removed from Jharu at all.
    pub verdict: Option<Verdict>,
}

#[derive(Serialize, Clone)]
struct DeepEvent {
    token: String,
    entry: DeepEntry,
}

pub fn default_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    return PathBuf::from("C:\\");
    #[cfg(not(target_os = "windows"))]
    return PathBuf::from("/");
}

/// Paths that must never be descended into when measuring: separate volumes,
/// virtual filesystems, and the APFS firmlink that would double-count data.
pub(crate) fn excluded(path: &Path) -> bool {
    let s = path.to_string_lossy();
    #[cfg(target_os = "macos")]
    {
        return s == "/System/Volumes"
            || s.starts_with("/System/Volumes/")
            || s == "/Volumes"
            || s.starts_with("/Volumes/")
            || s == "/dev"
            || s == "/Network";
    }
    #[cfg(not(target_os = "macos"))]
    {
        return s.contains("$Recycle.Bin")
            || s.ends_with("\\System Volume Information")
            || s == "/proc"
            || s == "/sys"
            || s == "/dev";
    }
}

pub(crate) fn classify(path: &Path) -> (String, bool, String) {
    let home = dirs::home_dir().unwrap_or_default();
    let s = path.to_string_lossy().to_string();
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

    #[cfg(target_os = "macos")]
    {
        for sys in ["/System", "/usr", "/bin", "/sbin", "/cores", "/private", "/etc", "/var", "/opt/homebrew/Cellar"] {
            if s == *sys || (s.starts_with(sys) && !s.starts_with(&format!("{}/", home.to_string_lossy()))) {
                if s == "/private" || s.starts_with("/private") {
                    return ("system".into(), true, "System temp files, logs, and swap. Managed by macOS.".into());
                }
                return ("system".into(), true, "Operating system files. Cannot be cleaned from here.".into());
            }
        }
        if s == "/Library" || (s.starts_with("/Library/") && !s.starts_with(&home.to_string_lossy().to_string())) {
            return ("system".into(), true, "System-wide support files shared by all users.".into());
        }
        if s == "/Applications" || s.starts_with("/Applications/") {
            return ("applications".into(), false, "Installed apps — use the Uninstaller tab to remove one cleanly.".into());
        }
    }
    #[cfg(target_os = "windows")]
    {
        let low = s.to_lowercase();
        if low.starts_with("c:\\windows") || name.eq_ignore_ascii_case("pagefile.sys") || name.eq_ignore_ascii_case("hiberfil.sys") {
            return ("system".into(), true, "Operating system files. Cannot be cleaned from here.".into());
        }
        if low.starts_with("c:\\program files") {
            return ("applications".into(), false, "Installed programs — uninstall via the Uninstaller tab.".into());
        }
        if low.starts_with("c:\\programdata") {
            return ("app_data".into(), false, "Shared application data. Review before deleting.".into());
        }
    }

    if path.starts_with(&home) {
        let user_dirs = ["Documents", "Desktop", "Downloads", "Pictures", "Movies", "Music", "Videos", "Public"];
        if let Ok(rel) = path.strip_prefix(&home) {
            let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
            if user_dirs.contains(&first.as_str()) {
                return ("user_data".into(), false, "Your personal files. Review carefully before deleting anything.".into());
            }
            if first == "Library" || first == "AppData" {
                return ("app_data".into(), false, "Settings, caches, and data stored by your apps.".into());
            }
            if first.starts_with('.') {
                return ("dev".into(), false, "Configuration or developer data (hidden folder).".into());
            }
        }
    }
    if name.starts_with('.') {
        return ("dev".into(), false, "Hidden configuration or developer data.".into());
    }
    ("other".into(), false, "Unrecognized — explore to see what's inside.".into())
}

/// Repositories asked about per folder; any beyond count as not checked,
/// which rates the folder "review first". Bounds the time a folder full of
/// clones adds to a scan.
const REPOS_CHECKED: usize = 12;

/// Size of `path` and what it holds (see places::Signals).
pub(crate) fn measure(path: &Path, seen: &mut Seen) -> (Usage, Signals) {
    let mut usage = Usage::default();
    let mut signals = Signals::default();
    if path.is_file() {
        if let Ok(meta) = path.symlink_metadata() {
            usage.add_file(path, &meta, seen);
            signals.add_file(path, usage.bytes, false);
        }
        return (usage, signals);
    }
    let repos = Arc::new(AtomicU64::new(0));
    let envs = Arc::new(AtomicU64::new(0));
    let (repo_count, env_count) = (repos.clone(), envs.clone());
    // Where the repositories are, so git can be asked about them after the
    // walk (never during it: git is far slower than reading a folder).
    let repo_dirs = Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
    let found_repos = repo_dirs.clone();
    // The walk's branch state is "inside a folder tagged as a cache": set
    // where a CACHEDIR.TAG appears, inherited by everything below it, and
    // copied onto each entry so its bytes can be counted as cache.
    let walker = crate::walk::walk_with::<(bool, bool)>(path).skip_hidden(false).process_read_dir(
        move |_, dir, in_cache, children| {
            children.retain(|c| c.as_ref().map(|e| !excluded(&e.path())).unwrap_or(true));
            let has = |name: &str| children.iter().flatten().any(|e| e.file_name == name);
            if has("CACHEDIR.TAG") {
                *in_cache = true;
            }
            if has(".git") {
                repo_count.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut found) = found_repos.lock() {
                    if found.len() < REPOS_CHECKED {
                        found.push(dir.to_path_buf());
                    }
                }
            }
            if has("pyvenv.cfg") || has("conda-meta") {
                env_count.fetch_add(1, Ordering::Relaxed);
            }
            for c in children.iter_mut().flatten() {
                c.client_state = *in_cache;
            }
        },
    );
    // A folder Jharu may not read would otherwise measure 0 B, the same as
    // an empty one, and vanish from the map. jwalk reports it as an entry
    // carrying read_children_error, not as an Err.
    for result in walker {
        let Ok(entry) = result else {
            usage.unreadable += 1;
            continue;
        };
        if entry.read_children_error.is_some() {
            usage.unreadable += 1;
        }
        if entry.file_type().is_file() {
            match entry.metadata() {
                Ok(meta) => {
                    let before = usage.bytes;
                    let file = entry.path();
                    usage.add_file(&file, &meta, seen);
                    signals.add_file(&file, usage.bytes - before, entry.client_state);
                }
                Err(_) => usage.unreadable += 1,
            }
        }
    }
    signals.repos = repos.load(Ordering::Relaxed);
    signals.envs = envs.load(Ordering::Relaxed);
    let dirs = std::mem::take(&mut *repo_dirs.lock().unwrap_or_else(|e| e.into_inner()));
    for dir in dirs {
        signals.add_repo(&crate::worktrees::check_repo(&dir));
    }
    (usage, signals)
}

/// For a drive's root: the space in use that the folders above do not
/// explain, as a row of its own, so the list adds up to what Explorer shows.
#[cfg(target_os = "windows")]
fn unaccounted(root: &Path, measured: u64) -> Option<DeepEntry> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let key = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let disk = disks.list().iter().find(|d| key(d.mount_point()) == key(root))?;
    let used = disk.total_space().saturating_sub(disk.available_space());
    let missing = used.saturating_sub(measured);
    // Below 1 GB the gap is file system overhead, not worth a row.
    if missing < 1_000_000_000 {
        return None;
    }
    Some(DeepEntry {
        name: "In use, not measurable from here".into(),
        path: format!("{}::unaccounted", root.display()),
        is_dir: false,
        size_bytes: missing,
        file_count: 0,
        kind: "system".into(),
        protected: true,
        hint: "Space Windows reports as used that no folder above accounts for: restore points and shadow copies (System Volume Information), the Recycle Bin, protected Windows files, and file system overhead. Reading them needs administrator rights; Windows' own Disk Cleanup and System Protection settings manage restore points.".into(),
        online_only_bytes: 0,
        shared_bytes: 0,
        unreadable: 0,
        is_virtual: true,
        verdict: None,
    })
}

#[cfg(not(target_os = "windows"))]
fn unaccounted(_root: &Path, _measured: u64) -> Option<DeepEntry> {
    // On macOS the root mixes the sealed system volume, the data volume and
    // purgeable space, so "used minus measured" would mislead.
    None
}

#[tauri::command]
pub async fn deep_scan(
    app: tauri::AppHandle,
    path: Option<String>,
    token: String,
) -> Result<Vec<DeepEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = crate::autocare::activity();
        let root = path.map(PathBuf::from).unwrap_or_else(default_root);
        let mut children: Vec<PathBuf> = std::fs::read_dir(&root)
            .map_err(|e| format!("Cannot read {}: {e}", root.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                !excluded(p)
                    && !p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false)
            })
            .collect();
        children.sort();

        // Measured one at a time: each walk is already parallel. One `seen`
        // for the whole scan, so a file reachable from two folders (hard
        // links) is counted in the first only. Entries still stream to the
        // UI as each one finishes.
        let mut seen = Seen::default();
        let facts = Facts::gather();
        let home = dirs::home_dir().unwrap_or_default();
        let mut total = 0u64;
        let mut entries: Vec<DeepEntry> = Vec::with_capacity(children.len() + 1);
        for child in children {
            let (usage, signals) = measure(&child, &mut seen);
            total += usage.bytes;
            let (kind, protected, hint) = classify(&child);
            let verdict = (!protected).then(|| verdict(&child, &home, &signals, &facts));
            let entry = DeepEntry {
                name: child
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| child.to_string_lossy().into_owned()),
                path: child.to_string_lossy().into_owned(),
                is_dir: child.is_dir(),
                size_bytes: usage.bytes,
                file_count: usage.files,
                kind,
                protected,
                hint,
                online_only_bytes: usage.online_only_bytes,
                shared_bytes: usage.shared_bytes,
                unreadable: usage.unreadable,
                is_virtual: false,
                verdict,
            };
            let _ = app.emit(
                "deep-entry",
                DeepEvent { token: token.clone(), entry: entry.clone() },
            );
            entries.push(entry);
        }

        if let Some(row) = unaccounted(&root, total) {
            let _ = app.emit("deep-entry", DeepEvent { token: token.clone(), entry: row.clone() });
            entries.push(row);
        }

        entries.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        crate::spacehistory::record_deep(&app, &root, &entries);
        Ok(entries)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    /// Every folder in the home directory with its rating, as Deep clean
    /// shows them. Read-only:
    /// `cargo test --release --lib smoke_home_verdicts -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_home_verdicts() {
        let home = dirs::home_dir().unwrap();
        // JHARU_SMOKE_ROOT rates the folders inside another folder instead.
        let root = std::env::var_os("JHARU_SMOKE_ROOT").map(std::path::PathBuf::from).unwrap_or_else(|| home.clone());
        let facts = crate::safety::Facts::gather();
        let mut seen = crate::usage::Seen::default();
        let mut kids: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).collect();
        kids.sort();
        let mut rows = vec![];
        for p in kids.iter().filter(|p| !p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(true)) {
            let (u, s) = super::measure(p, &mut seen);
            if u.bytes < 50 << 20 {
                continue;
            }
            let v = crate::places::verdict(p, &home, &s, &facts);
            let mut line = format!(
                "{:>7.1} GB  {:<8} {:<9} {:<26} {}",
                u.bytes as f64 / (1u64 << 30) as f64,
                format!("{:?}", v.safety),
                v.basis,
                p.file_name().unwrap().to_string_lossy(),
                v.what.clone().unwrap_or_default()
            );
            if u.unreadable > 0 {
                line.push_str(&format!("  [{} unreadable]", u.unreadable));
            }
            for r in &v.reasons {
                line.push_str(&format!("
{:>40}{:?}: {}", "", r.level, r.text));
            }
            rows.push((u.bytes, line));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, r) in rows {
            println!("{r}");
        }
    }

    /// The measuring walk finds what places::verdict decides on.
    #[test]
    fn walk_collects_signals() {
        let root = std::env::temp_dir().join(format!("jharu signals {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, bytes: usize| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![1u8; bytes]).unwrap();
        };
        // A tagged cache: the tag marks every file below, however deep.
        write("tool cache/CACHEDIR.TAG", 43);
        write("tool cache/a/b/c/blob", 5000);
        write("tool cache/x.bin", 3000);
        // Untagged siblings.
        write("project/.git/HEAD", 20);
        write("project/report.pdf", 7000);
        write("env/pyvenv.cfg", 30);
        write("models/net.safetensors", 9000);

        let mut seen = crate::usage::Seen::default();
        let (usage, s) = super::measure(&root, &mut seen);
        assert_eq!(s.total, usage.bytes);
        assert_eq!(s.cache, 43 + 5000 + 3000, "only the tagged subtree counts as cache");
        assert_eq!(s.repos, 1);
        assert_eq!(s.envs, 1);
        assert_eq!(s.personal, 7000);
        assert_eq!(s.models, 9000);

        // Measured on its own, the tagged folder is entirely cache.
        let (_, cache) = super::measure(&root.join("tool cache"), &mut crate::usage::Seen::default());
        assert_eq!(cache.cache, cache.total);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Deep clean's measurement of every top-level folder while Dev clean's
    /// scan runs at the same time, the situation that used to return 0 B for
    /// C:\Users and C:\Windows. Read-only:
    /// `cargo test --release --lib smoke_deep_concurrent -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_deep_concurrent() {
        let other = std::thread::spawn(crate::scanner::scan);
        let root = super::default_root();
        let mut children: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                !super::excluded(p) && !p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false)
            })
            .collect();
        children.sort();
        let mut seen = crate::usage::Seen::default();
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        let mut total = 0;
        let mut rows: Vec<(u64, String)> = children
            .iter()
            .map(|p| {
                let (u, _) = super::measure(p, &mut seen);
                total += u.bytes;
                let line = format!(
                    "{:>8.1} GiB on disk {:>9} files  (+{:.1} GiB shared, {:.1} GiB online-only)  {}",
                    gib(u.bytes),
                    u.files,
                    gib(u.shared_bytes),
                    gib(u.online_only_bytes),
                    p.display()
                );
                (u.bytes, line)
            })
            .collect();
        if let Some(row) = super::unaccounted(&root, total) {
            rows.push((row.size_bytes, format!("{:>8.1} GiB  {}", gib(row.size_bytes), row.name)));
        }
        let findings = other.join().unwrap().findings.len();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, r) in rows.iter().take(10) {
            println!("{r}");
        }
        println!("measured {:.1} GiB; Dev clean scan alongside found {findings} locations", gib(total));
    }
}
