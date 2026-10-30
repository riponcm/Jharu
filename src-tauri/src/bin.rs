//! The Recycle Bin / Trash tab: what is in the bin, emptying it, and putting
//! back what Jharu sent there.
//!
//! Three things shape this module:
//!
//! - Purging is the one permanent action in Jharu, so it is built to be
//!   narrow. The request lists the exact items the user saw; each is looked up
//!   again right before it goes, and one that is gone, changed or no longer
//!   matches the chosen rule (say "older than 30 days") is skipped. Nothing
//!   that was not in the request is touched, even if it now matches.
//! - The bin does not know who put an item there. Jharu's journal
//!   (`trashlog.rs`) does, so each bin item is matched against it: same
//!   original path and a deletion time close to the journal's. Items Jharu
//!   moved from a cache tab are "Safe" to purge; everything else is the
//!   user's own deleted files and gets "Caution".
//! - Restoring through the `trash` crate on Windows brought files back
//!   without their extension (`foo.txt` returned as `foo`). The crate names
//!   the restored file with the item's display name, and Explorer hides
//!   known extensions in display names. [`true_name`] recovers the real name
//!   from the bin's own metadata file before restoring.

use crate::rules::Safety;
use crate::safety::{Level, Reason};
use crate::trashlog::{self, Entry};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Emitter;

#[cfg(target_os = "windows")]
const BIN: &str = "Recycle Bin";
#[cfg(not(target_os = "windows"))]
const BIN: &str = "Trash";

/// Items handed to the shell per purge call. One call for everything hides
/// which one failed and cannot be cancelled; one call per item is slow.
const CHUNK: usize = 25;
/// A journal entry is written just after the move; the bin stamps the move
/// itself. A big folder can take minutes in between, so allow a generous
/// window before the journal's time and a few seconds after it.
const WINDOW_BEFORE: i64 = 1800;
const WINDOW_AFTER: i64 = 15;
/// Journal rows shown in the restore list.
const LOG_LIMIT: usize = 500;

/// What the tabs call themselves in the journal when what they moved was
/// rebuilt by its own tool. "Dev clean" is written only for findings Dev
/// clean itself rated Safe; the others are written as
/// "Dev clean (not rated safe)" and get no free pass here.
fn rebuilds_itself(source: &str) -> bool {
    matches!(source, "Dev clean" | "System folders")
}

// ---------------------------------------------------------------- model

/// One item as the platform reports it, before anything is decided about it.
#[derive(Clone, Debug)]
struct Raw {
    /// Opaque, stable while the item stays in the bin. Windows: the shell's
    /// parsing name. macOS: the full path inside ~/.Trash.
    id: String,
    /// The real file name, with its extension.
    name: String,
    /// The folder it was deleted from, where the platform knows it.
    parent: Option<PathBuf>,
    /// Unix seconds.
    deleted: Option<i64>,
    size: u64,
    is_dir: bool,
}

impl Raw {
    fn original(&self) -> Option<PathBuf> {
        self.parent.as_ref().map(|p| p.join(&self.name))
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct BinItem {
    pub id: String,
    pub name: String,
    pub original_path: Option<String>,
    pub original_folder: Option<String>,
    pub deleted_at: Option<i64>,
    pub age_days: Option<u64>,
    pub size_bytes: u64,
    pub is_dir: bool,
    pub by_jharu: bool,
    /// The tab that moved it here, for Jharu's items.
    pub source: Option<String>,
    pub safety: Safety,
    pub summary: String,
    pub reasons: Vec<Reason>,
}

#[derive(Serialize, Debug)]
pub struct BinReport {
    /// False where Jharu cannot read the bin (Linux builds).
    pub supported: bool,
    pub bin_name: String,
    pub total_items: usize,
    pub total_bytes: u64,
    pub jharu_items: usize,
    pub jharu_bytes: u64,
    pub other_items: usize,
    pub other_bytes: u64,
    pub items: Vec<BinItem>,
    /// Something the user should know about how complete this list is.
    pub note: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct LogEntry {
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub time: u64,
    pub source: String,
    /// The item is still in the bin and can be put back.
    pub in_bin: bool,
    pub bin_id: Option<String>,
    /// Something already exists at the original path, so a restore would
    /// collide with it.
    pub destination_taken: bool,
}

#[derive(Serialize, Debug)]
pub struct LogReport {
    pub supported: bool,
    pub bin_name: String,
    pub total_logged: usize,
    pub in_bin: usize,
    /// Newest first, at most [`LOG_LIMIT`].
    pub entries: Vec<LogEntry>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Everything in the request.
    All,
    /// Only items deleted at least N days ago.
    Older,
    /// Only items Jharu moved here, deleted at least N days ago.
    JharuOlder,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PurgeRequest {
    pub id: String,
    /// The deletion time the user saw; a different one means another item.
    pub deleted: Option<i64>,
}

#[derive(Serialize, Default, Debug)]
pub struct PurgeOutcome {
    pub purged: usize,
    pub freed_bytes: u64,
    pub already_gone: usize,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

#[derive(Serialize, Default, Debug)]
pub struct RestoreOutcome {
    pub restored: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Comparable form of a path: separators unified, no trailing slash, and
/// case-folded where the file system ignores case.
fn key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        s.to_lowercase()
    } else {
        s
    }
}

fn file_name_of(path: &str) -> String {
    Path::new(&path.replace('\\', "/")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn age_days(deleted: Option<i64>, now: i64) -> Option<u64> {
    deleted.map(|d| (now - d).max(0) as u64 / 86_400)
}

// ---------------------------------------------------------------- recycle bin metadata

/// What Windows keeps about a deleted item in its `$I` file beside the `$R`
/// file that holds the data.
#[derive(Debug, PartialEq, Eq)]
struct Info {
    original: PathBuf,
    size: u64,
}

/// Parses a `$I` file. Version 1 (Vista to 8) has a fixed 260-character path;
/// version 2 (Windows 10 and later) stores the length first.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_info(b: &[u8]) -> Option<Info> {
    let u64_at = |at: usize| b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    let version = u64_at(0)?;
    let size = u64_at(8)?;
    let (start, chars) = match version {
        1 => (24usize, 260usize),
        2 => (28, u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize),
        _ => return None,
    };
    let bytes = b.get(start..)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .take(chars)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    let text = String::from_utf16(&units).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(Info { original: PathBuf::from(text), size })
}

/// The real name of a bin item, extension included.
///
/// `listed` is the shell's display name, which hides known extensions
/// ("foo.txt" is listed as "foo"). `id` is the item's parsing name, a path
/// like `C:\$Recycle.Bin\S-1-5-21-…\$R1A2B3C.txt` whose extension is the
/// original's. The `$I` file, when readable, holds the original path itself
/// and settles it; without it the extension of `id` is added when the listed
/// name lacks it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn true_name(listed: &str, id: &str, info: Option<&Info>) -> String {
    if let Some(name) = info.and_then(|i| i.original.file_name()) {
        let name = name.to_string_lossy();
        // Only trust a record that is about this item.
        if name.to_lowercase().starts_with(&listed.to_lowercase()) {
            return name.into_owned();
        }
    }
    let ext = Path::new(&id.replace('\\', "/")).extension().map(|e| e.to_string_lossy().into_owned());
    match ext {
        Some(ext) if !listed.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase())) => format!("{listed}.{ext}"),
        _ => listed.to_string(),
    }
}

// ---------------------------------------------------------------- platform

#[cfg(target_os = "windows")]
mod plat {
    use super::*;
    use std::io::Read;

    pub const SUPPORTED: bool = true;

    /// A bin item with the handle the `trash` crate needs to act on it. The
    /// handle carries the corrected name, so restoring recreates the file
    /// under its real name.
    pub struct Native {
        pub raw: Raw,
        pub item: trash::TrashItem,
    }

    /// The `$I` file for a `$R` path, read when the bin folder allows it.
    pub fn read_info(r_path: &Path) -> Option<Info> {
        let name = r_path.file_name()?.to_string_lossy().into_owned();
        let rest = name.get(2..).filter(|_| name[..2].eq_ignore_ascii_case("$R"))?;
        let i_path = r_path.with_file_name(format!("$I{rest}"));
        let mut bytes = Vec::new();
        std::fs::File::open(i_path).ok()?.take(16 * 1024).read_to_end(&mut bytes).ok()?;
        parse_info(&bytes)
    }

    fn folder_bytes(dir: &Path) -> u64 {
        crate::walk::walk(dir)
            .skip_hidden(false)
            .follow_links(false)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    pub fn list() -> Result<Vec<Native>, String> {
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(items.len());
        for mut t in items {
            let id = t.id.to_string_lossy().into_owned();
            let r_path = PathBuf::from(&id);
            let info = read_info(&r_path);
            let name = true_name(&t.name.to_string_lossy(), &id, info.as_ref());
            t.name = name.clone().into();

            let (size, is_dir) = match r_path.symlink_metadata() {
                // The bin records a folder's size when it is deleted; walking a
                // big deleted node_modules just to list it would take minutes.
                Ok(m) if m.is_dir() => {
                    let recorded = info.as_ref().map(|i| i.size).filter(|&s| s > 0);
                    (recorded.unwrap_or_else(|| folder_bytes(&r_path)), true)
                }
                Ok(m) => (m.len(), false),
                // The bin's own folder is not readable: ask the shell.
                Err(_) => match trash::os_limited::metadata(&t) {
                    Ok(meta) => match meta.size {
                        trash::TrashItemSize::Bytes(b) => (b, false),
                        trash::TrashItemSize::Entries(_) => (info.as_ref().map_or(0, |i| i.size), true),
                    },
                    Err(_) => (info.as_ref().map_or(0, |i| i.size), false),
                },
            };
            out.push(Native {
                raw: Raw {
                    id,
                    name,
                    parent: Some(t.original_parent.clone()),
                    deleted: Some(t.time_deleted),
                    size,
                    is_dir,
                },
                item: t,
            });
        }
        Ok(out)
    }

    pub fn purge(batch: &[&Native]) -> Result<(), String> {
        trash::os_limited::purge_all(batch.iter().map(|n| &n.item)).map_err(|e| e.to_string())
    }

    /// Puts one item back. `dest` is unused here: Windows knows where it came from.
    pub fn restore(n: &Native, _dest: Option<&Path>) -> Result<PathBuf, String> {
        let target = n.item.original_path();
        if let Some(parent) = target.parent() {
            // The folder it was in may be gone too (a removed worktree's own
            // parent); Explorer recreates it, and so does this.
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| format!("could not recreate {}: {e}", parent.display()))?;
            }
        }
        trash::os_limited::restore_all([n.item.clone()]).map_err(|e| e.to_string())?;
        Ok(target)
    }
}

#[cfg(target_os = "macos")]
mod plat {
    //! macOS keeps the bin as an ordinary folder, ~/.Trash. Items there carry
    //! no record of where they came from, so only what the journal knows can
    //! be put back. Not compiled or run by the developers so far.
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub const SUPPORTED: bool = true;

    pub struct Native {
        pub raw: Raw,
    }

    pub fn trash_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".Trash"))
    }

    pub fn list() -> Result<Vec<Native>, String> {
        let dir = trash_dir().ok_or("Jharu could not find your home folder")?;
        let rd = std::fs::read_dir(&dir).map_err(|e| {
            format!("could not read {}: {e}. Jharu needs Full Disk Access to look inside the Trash.", dir.display())
        })?;
        let mut out = vec![];
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".DS_Store" {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = path.symlink_metadata() else { continue };
            let size = if meta.is_dir() {
                crate::walk::walk(&path)
                    .skip_hidden(false)
                    .follow_links(false)
                    .into_iter()
                    .flatten()
                    .filter(|e| e.file_type().is_file())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            } else {
                meta.len()
            };
            out.push(Native {
                raw: Raw {
                    id: path.to_string_lossy().into_owned(),
                    name,
                    parent: None,
                    // Moving a file into the Trash is a rename, which changes
                    // the status-change time and nothing else.
                    deleted: Some(meta.ctime()),
                    size,
                    is_dir: meta.is_dir(),
                },
            });
        }
        Ok(out)
    }

    pub fn purge(batch: &[&Native]) -> Result<(), String> {
        let dir = trash_dir().ok_or("no home folder")?;
        for n in batch {
            let path = PathBuf::from(&n.raw.id);
            // Only ever remove direct children of ~/.Trash.
            if path.parent() != Some(dir.as_path()) {
                return Err(format!("{} is not inside the Trash", path.display()));
            }
            let meta = path.symlink_metadata().map_err(|e| e.to_string())?;
            if meta.is_dir() {
                std::fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", n.raw.name))?;
            } else {
                std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", n.raw.name))?;
            }
        }
        Ok(())
    }

    pub fn restore(n: &Native, dest: Option<&Path>) -> Result<PathBuf, String> {
        let dest = dest.ok_or("Jharu does not know where this came from; use Finder's Put Back")?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // rename replaces an existing file silently; look once more right before.
        if dest.symlink_metadata().is_ok() {
            return Err(format!("something already exists at {}", dest.display()));
        }
        std::fs::rename(&n.raw.id, dest).map_err(|e| format!("could not move it back to {}: {e}", dest.display()))?;
        Ok(dest.to_path_buf())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod plat {
    use super::*;

    pub const SUPPORTED: bool = false;

    pub struct Native {
        pub raw: Raw,
    }

    pub fn list() -> Result<Vec<Native>, String> {
        Ok(vec![])
    }

    pub fn purge(_batch: &[&Native]) -> Result<(), String> {
        Err("Jharu manages the Recycle Bin on Windows and macOS only".into())
    }

    pub fn restore(_n: &Native, _dest: Option<&Path>) -> Result<PathBuf, String> {
        Err("Jharu manages the Recycle Bin on Windows and macOS only".into())
    }
}

use plat::Native;

// ---------------------------------------------------------------- matching and rating

/// For each bin item, the journal entry that describes it, if any.
///
/// An entry describes an item when the original path matches (on macOS,
/// where the bin does not keep it, the file name) and the bin's deletion time
/// is within the window around the entry's. When the same path was trashed
/// more than once, each entry takes the closest item not already taken, newest
/// entry first, so an old row never claims a newer deletion.
fn match_journal(items: &[Raw], log: &[Entry]) -> Vec<Option<usize>> {
    let mut taken: Vec<Option<usize>> = vec![None; items.len()];
    // Index the items once: the journal holds up to 20 000 rows and the bin
    // thousands of items, so comparing every pair would take minutes.
    let mut by_path: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (ii, it) in items.iter().enumerate() {
        match it.original() {
            Some(orig) => by_path.entry(key(&orig)).or_default().push(ii),
            None => by_name.entry(it.name.to_lowercase()).or_default().push(ii),
        }
    }
    let mut order: Vec<usize> = (0..log.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(log[i].time));
    for li in order {
        let e = &log[li];
        let candidates = by_path
            .get(&key(Path::new(&e.path)))
            .into_iter()
            .chain(by_name.get(&file_name_of(&e.path).to_lowercase()))
            .flatten();
        let mut best: Option<(i64, usize)> = None;
        for &ii in candidates {
            if taken[ii].is_some() {
                continue;
            }
            let Some(deleted) = items[ii].deleted else { continue };
            let delta = e.time as i64 - deleted;
            if delta < -WINDOW_AFTER || delta > WINDOW_BEFORE {
                continue;
            }
            if best.map_or(true, |(d, _)| delta.abs() < d) {
                best = Some((delta.abs(), ii));
            }
        }
        if let Some((_, ii)) = best {
            taken[ii] = Some(li);
        }
    }
    taken
}

fn ago(days: Option<u64>) -> String {
    match days {
        None => "at an unknown time".into(),
        Some(0) => "today".into(),
        Some(1) => "1 day ago".into(),
        Some(n) => format!("{n} days ago"),
    }
}

/// How safe it is to empty this item, and why. Each line is a fact about the
/// item, so the rating can be checked rather than taken on trust.
fn rate(entry: Option<&Entry>, age: Option<u64>) -> (Safety, String, Vec<Reason>) {
    let ok = |t: String| Reason { level: Level::Ok, text: t };
    let warn = |t: String| Reason { level: Level::Warn, text: t };
    let mut reasons = vec![];
    let (safety, summary) = match entry {
        Some(e) if rebuilds_itself(&e.source) => {
            reasons.push(ok(format!("Jharu moved this here from {} {}.", e.source, ago(age))));
            reasons.push(ok(if e.source == "Dev clean" {
                "Dev clean rated this a safe cache when it was moved: the tool that made it builds it again when needed, so emptying loses nothing unique.".to_string()
            } else {
                "System folders only clears temporary files, logs and caches that the system or the program that wrote them creates again, so emptying loses nothing unique.".to_string()
            }));
            (Safety::Safe, "Cache Jharu removed".to_string())
        }
        Some(e) => {
            reasons.push(ok(format!("Jharu moved this here from {} {}.", e.source, ago(age))));
            reasons.push(warn(match e.source.as_str() {
                "ML models" => "Models can be downloaded again, but they are large; emptying means that download if you need one.".to_string(),
                "Dev clean (not rated safe)" => "Dev clean did not rate this a plain cache (it needs a manual step to rebuild, or may hold unique data); look inside before emptying.".to_string(),
                "Python" => "Environments and packages must be reinstalled if a project still needs them.".to_string(),
                "Git worktrees" => "A copy of a repository folder. Its branches are still in git, but anything that was not committed is only here.".to_string(),
                "Browsers" => "Browser folders can hold logins and history as well as cache.".to_string(),
                other => format!("{other} can include files that exist nowhere else; look inside before emptying."),
            }));
            (Safety::Caution, "Moved by Jharu, check first".to_string())
        }
        None => {
            reasons.push(warn(
                "Not moved here by Jharu, so Jharu cannot tell what it is. These are your own deleted files; check before emptying.".to_string(),
            ));
            if age == Some(0) {
                reasons.push(warn("Deleted today; you may still want it.".to_string()));
            }
            (Safety::Caution, "Your own deleted file".to_string())
        }
    };
    if let Some(n) = age.filter(|&n| n >= 7) {
        reasons.push(ok(format!("In the {BIN} for {n} days without being put back.")));
    }
    (safety, summary, reasons)
}

fn build_items(raws: &[Raw], log: &[Entry], now: i64) -> Vec<BinItem> {
    let matched = match_journal(raws, log);
    let mut items: Vec<BinItem> = raws
        .iter()
        .zip(&matched)
        .map(|(r, m)| {
            let entry = m.map(|li| &log[li]);
            let age = age_days(r.deleted, now);
            let (safety, summary, reasons) = rate(entry, age);
            // macOS does not keep the original place; the journal does.
            let original = r.original().map(|p| p.to_string_lossy().into_owned()).or_else(|| entry.map(|e| e.path.clone()));
            let folder = original.as_ref().and_then(|p| Path::new(p).parent().map(|d| d.to_string_lossy().into_owned()));
            BinItem {
                id: r.id.clone(),
                name: r.name.clone(),
                original_path: original,
                original_folder: folder,
                deleted_at: r.deleted,
                age_days: age,
                size_bytes: r.size,
                is_dir: r.is_dir,
                by_jharu: entry.is_some(),
                source: entry.map(|e| e.source.clone()),
                safety,
                summary,
                reasons,
            }
        })
        .collect();
    items.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    items
}

fn report_from(items: Vec<BinItem>, note: Option<String>) -> BinReport {
    let sum = |f: &dyn Fn(&BinItem) -> bool| items.iter().filter(|i| f(i)).map(|i| i.size_bytes).sum::<u64>();
    let count = |f: &dyn Fn(&BinItem) -> bool| items.iter().filter(|i| f(i)).count();
    BinReport {
        supported: plat::SUPPORTED,
        bin_name: BIN.to_string(),
        total_items: items.len(),
        total_bytes: sum(&|_| true),
        jharu_items: count(&|i| i.by_jharu),
        jharu_bytes: sum(&|i| i.by_jharu),
        other_items: count(&|i| !i.by_jharu),
        other_bytes: sum(&|i| !i.by_jharu),
        items,
        note,
    }
}

/// Whether an item belongs in a purge of this scope.
fn in_scope(item: &BinItem, scope: Scope, days: u32) -> bool {
    let old_enough = item.age_days.is_some_and(|a| a >= days as u64);
    match scope {
        Scope::All => true,
        Scope::Older => old_enough,
        Scope::JharuOlder => item.by_jharu && old_enough,
    }
}

fn log_report(raws: &[Raw], log: &[Entry]) -> LogReport {
    let matched = match_journal(raws, log);
    let mut bin_of: HashMap<usize, &Raw> = HashMap::new();
    for (ri, m) in matched.iter().enumerate() {
        if let Some(li) = m {
            bin_of.insert(*li, &raws[ri]);
        }
    }
    let mut order: Vec<usize> = (0..log.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(log[i].time));
    let in_bin = bin_of.len();
    let entries = order
        .into_iter()
        .take(LOG_LIMIT)
        .map(|li| {
            let e = &log[li];
            let raw = bin_of.get(&li);
            LogEntry {
                path: e.path.clone(),
                name: file_name_of(&e.path),
                size_bytes: e.size,
                time: e.time,
                source: e.source.clone(),
                in_bin: raw.is_some(),
                bin_id: raw.map(|r| r.id.clone()),
                destination_taken: raw.is_some() && Path::new(&e.path).symlink_metadata().is_ok(),
            }
        })
        .collect();
    LogReport { supported: plat::SUPPORTED, bin_name: BIN.to_string(), total_logged: log.len(), in_bin, entries }
}

// ---------------------------------------------------------------- cancel tokens

static CANCELLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn cancel(token: &str) {
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashSet::new).insert(token.to_string());
}

fn is_cancelled(token: &str) -> bool {
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|s| s.contains(token))
}

fn forget(token: &str) {
    if let Some(s) = CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.remove(token);
    }
}

// ---------------------------------------------------------------- operations

pub(crate) fn list_report() -> Result<BinReport, String> {
    let natives = plat::list()?;
    let raws: Vec<Raw> = natives.iter().map(|n| n.raw.clone()).collect();
    let note = if cfg!(target_os = "macos") {
        Some("The Trash does not record where an item came from or when it was deleted; the time shown is when it was moved in. Only items Jharu moved can be put back by Jharu.".to_string())
    } else {
        None
    };
    Ok(report_from(build_items(&raws, &trashlog::read_all(), now_secs()), note))
}

fn log_impl() -> Result<LogReport, String> {
    let raws: Vec<Raw> = plat::list()?.iter().map(|n| n.raw.clone()).collect();
    Ok(log_report(&raws, &trashlog::read_all()))
}

/// Permanently removes the requested items from the bin, each only if it is
/// still there, unchanged, and in the chosen scope.
fn purge_impl(
    requests: &[PurgeRequest],
    scope: Scope,
    days: u32,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, usize, &str),
) -> Result<PurgeOutcome, String> {
    let mut out = PurgeOutcome::default();
    let natives = plat::list()?;
    let raws: Vec<Raw> = natives.iter().map(|n| n.raw.clone()).collect();
    let items = build_items(&raws, &trashlog::read_all(), now_secs());
    let by_id: HashMap<&str, usize> = natives.iter().enumerate().map(|(i, n)| (n.raw.id.as_str(), i)).collect();
    let item_of: HashMap<&str, &BinItem> = items.iter().map(|i| (i.id.as_str(), i)).collect();

    let mut seen: HashSet<&str> = HashSet::new();
    let mut chosen: Vec<&Native> = vec![];
    for req in requests {
        if !seen.insert(req.id.as_str()) {
            continue;
        }
        let (Some(&ni), Some(item)) = (by_id.get(req.id.as_str()), item_of.get(req.id.as_str())) else {
            out.already_gone += 1;
            continue;
        };
        // The time the user saw must be the time the item has now; a request
        // that carries none cannot show it is the same item.
        if req.deleted != item.deleted_at {
            out.skipped.push(format!("{}: a different item now has this place in the {BIN}", item.name));
        } else if !in_scope(item, scope, days) {
            out.skipped.push(format!("{}: no longer matches what you chose to empty", item.name));
        } else {
            chosen.push(&natives[ni]);
        }
    }

    let total = chosen.len();
    let mut done = 0usize;
    // (id, message): kept until the fresh listing shows which really failed.
    let mut failed: Vec<(&str, String)> = vec![];
    progress(0, total, "");
    for batch in chosen.chunks(CHUNK) {
        if cancelled() {
            out.cancelled = true;
            break;
        }
        progress(done, total, &batch[0].raw.name);
        if plat::purge(batch).is_err() {
            // One bad item can stop a batch before it removes anything; give
            // each its own chance so the error names the one at fault.
            for n in batch {
                if let Err(e) = plat::purge(&[*n]) {
                    failed.push((n.raw.id.as_str(), format!("{}: {e}", n.raw.name)));
                }
            }
        }
        done += batch.len();
        progress(done, total, "");
    }

    // The truth is what is no longer in the bin. A batch that failed part-way
    // may have removed some items before the one that stopped it, and those
    // then fail their own retry with "not found"; only items still there count
    // as failures.
    let after: HashSet<String> = plat::list()?.into_iter().map(|n| n.raw.id).collect();
    for (id, msg) in &failed {
        if after.contains(*id) {
            out.errors.push(msg.clone());
        }
    }
    // Items past the point where a cancel stopped were never attempted.
    let attempted = if out.cancelled { done } else { total };
    for n in chosen.iter().take(attempted) {
        if !after.contains(&n.raw.id) {
            out.purged += 1;
            out.freed_bytes += n.raw.size;
        }
    }
    let still: usize = chosen.iter().take(attempted).filter(|n| after.contains(&n.raw.id)).count();
    if still > out.errors.len() {
        out.errors.push(format!("{} items are still in the {BIN}", still - out.errors.len()));
    }
    Ok(out)
}

/// Puts the given bin items back where they were.
fn restore_impl(ids: &[String], progress: &mut dyn FnMut(usize, usize, &str)) -> Result<RestoreOutcome, String> {
    let mut out = RestoreOutcome::default();
    let natives = plat::list()?;
    let raws: Vec<Raw> = natives.iter().map(|n| n.raw.clone()).collect();
    let log = trashlog::read_all();
    let matched = match_journal(&raws, &log);
    let by_id: HashMap<&str, usize> = natives.iter().enumerate().map(|(i, n)| (n.raw.id.as_str(), i)).collect();

    let mut seen: HashSet<&str> = HashSet::new();
    let total = ids.len();
    for (done, id) in ids.iter().enumerate() {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let Some(&ni) = by_id.get(id.as_str()) else {
            out.skipped.push(format!("An item is no longer in the {BIN} (already restored or emptied)"));
            continue;
        };
        let n = &natives[ni];
        progress(done, total, &n.raw.name);
        // Where it goes: the bin's own record, else the journal's.
        let dest: Option<PathBuf> = n.raw.original().or_else(|| matched[ni].map(|li| PathBuf::from(&log[li].path)));
        let Some(dest) = dest else {
            out.skipped.push(format!("{}: Jharu does not know where this came from", n.raw.name));
            continue;
        };
        if dest.symlink_metadata().is_ok() {
            out.skipped.push(format!("{}: something already exists at {}", n.raw.name, dest.display()));
            continue;
        }
        match plat::restore(n, Some(&dest)) {
            Ok(path) if path.symlink_metadata().is_ok() => out.restored.push(path.to_string_lossy().into_owned()),
            Ok(path) => out.errors.push(format!("{}: restored, but nothing is at {}", n.raw.name, path.display())),
            Err(e) => out.errors.push(format!("{}: {e}", n.raw.name)),
        }
    }
    progress(total, total, "");
    Ok(out)
}

// ---------------------------------------------------------------- commands

#[derive(Serialize, Clone)]
struct BinProgress {
    token: String,
    /// "purge" or "restore"
    phase: &'static str,
    done: usize,
    total: usize,
    current: String,
}

#[tauri::command]
pub async fn bin_list() -> Result<BinReport, String> {
    tauri::async_runtime::spawn_blocking(list_report).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn bin_log() -> Result<LogReport, String> {
    tauri::async_runtime::spawn_blocking(log_impl).await.map_err(|e| e.to_string())?
}

/// Permanent. The UI confirms with the total before calling this.
#[tauri::command]
pub async fn bin_purge(
    app: tauri::AppHandle,
    items: Vec<PurgeRequest>,
    scope: Scope,
    days: Option<u32>,
    token: String,
) -> Result<PurgeOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let t = token.clone();
        let result = purge_impl(
            &items,
            scope,
            days.unwrap_or(0),
            &|| is_cancelled(&t),
            &mut |done, total, current| {
                let _ = app.emit(
                    "bin-progress",
                    BinProgress { token: token.clone(), phase: "purge", done, total, current: current.to_string() },
                );
            },
        );
        forget(&token);
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn bin_restore(app: tauri::AppHandle, ids: Vec<String>, token: String) -> Result<RestoreOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        restore_impl(&ids, &mut |done, total, current| {
            let _ = app.emit(
                "bin-progress",
                BinProgress { token: token.clone(), phase: "restore", done, total, current: current.to_string() },
            );
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops a running purge after the batch it is on.
#[tauri::command]
pub fn bin_cancel(token: String) {
    cancel(&token);
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, parent: Option<&str>, deleted: i64) -> Raw {
        Raw {
            id: format!("id:{name}:{deleted}"),
            name: name.to_string(),
            parent: parent.map(PathBuf::from),
            deleted: Some(deleted),
            size: 100,
            is_dir: false,
        }
    }

    fn entry(path: &str, time: u64, source: &str) -> Entry {
        Entry { path: path.to_string(), size: 100, time, source: source.to_string() }
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn parses_both_recycle_bin_info_formats() {
        let path = "C:\\Users\\me\\My Folder\\report v2.final.docx";
        let mut v2 = vec![];
        v2.extend(2u64.to_le_bytes());
        v2.extend(4096u64.to_le_bytes());
        v2.extend(0u64.to_le_bytes());
        let mut units = utf16(path);
        units.extend([0, 0]);
        v2.extend(((units.len() / 2) as u32).to_le_bytes());
        v2.extend(&units);
        let info = parse_info(&v2).unwrap();
        assert_eq!(info.original, PathBuf::from(path));
        assert_eq!(info.size, 4096);

        let mut v1 = vec![];
        v1.extend(1u64.to_le_bytes());
        v1.extend(7u64.to_le_bytes());
        v1.extend(0u64.to_le_bytes());
        let mut fixed = utf16(path);
        fixed.resize(520, 0);
        v1.extend(&fixed);
        assert_eq!(parse_info(&v1).unwrap(), Info { original: PathBuf::from(path), size: 7 });

        // Truncated, unknown version, empty path.
        assert!(parse_info(&v2[..20]).is_none());
        let mut v3 = v2.clone();
        v3[0] = 3;
        assert!(parse_info(&v3).is_none());
        let mut empty = v2[..28].to_vec();
        empty.extend([0, 0]);
        assert!(parse_info(&empty).is_none());
    }

    /// The bug: restored files came back without their extension.
    #[cfg(windows)]
    #[test]
    fn true_name_restores_the_extension_explorer_hides() {
        let id = "C:\\$Recycle.Bin\\S-1-5-21-1-2-3-1001\\$R1A2B3C.txt";
        // Explorer lists "foo.txt" as "foo".
        assert_eq!(true_name("foo", id, None), "foo.txt");
        // Already complete (extensions shown): left alone, in any case.
        assert_eq!(true_name("foo.txt", id, None), "foo.txt");
        assert_eq!(true_name("FOO.TXT", id, None), "FOO.TXT");
        // Several dots: only the last extension is hidden.
        assert_eq!(true_name("archive.tar", "C:\\$Recycle.Bin\\S\\$RXYZ123.gz", None), "archive.tar.gz");
        // No extension, folders: nothing to add.
        assert_eq!(true_name("README", "C:\\$Recycle.Bin\\S\\$RXYZ123", None), "README");
        assert_eq!(true_name("my.folder", "C:\\$Recycle.Bin\\S\\$RXYZ123", None), "my.folder");
        // The bin's own record wins, including the ambiguous "a.txt.txt".
        let info = Info { original: PathBuf::from("C:\\x\\a.txt.txt"), size: 1 };
        assert_eq!(true_name("a.txt", "C:\\$Recycle.Bin\\S\\$RQ.txt", Some(&info)), "a.txt.txt");
        // A record about some other item is not trusted.
        let other = Info { original: PathBuf::from("C:\\x\\other.png"), size: 1 };
        assert_eq!(true_name("foo", id, Some(&other)), "foo.txt");
    }

    #[test]
    fn matches_journal_entries_to_bin_items_by_path_and_time() {
        let raws = vec![
            raw("cache", Some("C:\\Users\\me\\My Folder"), 1_000_000),
            // The same path, trashed again later: a second, separate item.
            raw("cache", Some("C:\\Users\\me\\My Folder"), 1_500_000),
            // Same name, different folder: not Jharu's.
            raw("cache", Some("C:\\Other"), 1_500_000),
            // Same path as an entry but deleted days after the entry: not it.
            raw("late", Some("C:\\Users\\me"), 9_000_000),
        ];
        let log = vec![
            entry("c:/users/me/my folder/cache", 1_000_004, "Dev clean"),
            entry("C:\\Users\\me\\My Folder\\cache", 1_500_002, "Browsers"),
            entry("C:\\Users\\me\\late", 1_000_000, "Deep clean"),
        ];
        let m = match_journal(&raws, &log);
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert_eq!(m, vec![Some(0), Some(1), None, None]);
        }
        // The bin item with no known original place (macOS) is matched by name.
        let mut bare = raw("cache", None, 1_000_001);
        bare.parent = None;
        let m = match_journal(&[bare], &log[..1]);
        assert_eq!(m, vec![Some(0)]);
    }

    #[test]
    fn rates_by_who_moved_it_and_says_why() {
        let (s, summary, reasons) = rate(Some(&entry("C:\\a", 1, "Dev clean")), Some(3));
        assert_eq!(s, Safety::Safe);
        assert_eq!(summary, "Cache Jharu removed");
        assert!(reasons.iter().all(|r| r.level == Level::Ok));
        assert!(reasons[0].text.contains("Dev clean") && reasons[0].text.contains("3 days ago"));

        let (s, _, reasons) = rate(Some(&entry("C:\\a", 1, "Git worktrees")), Some(0));
        assert_eq!(s, Safety::Caution);
        assert!(reasons.iter().any(|r| r.level == Level::Warn && r.text.contains("uncommitted") || r.text.contains("committed")));

        let (s, _, reasons) = rate(Some(&entry("C:\\a", 1, "Desktop")), None);
        assert_eq!(s, Safety::Caution);
        assert!(reasons.iter().any(|r| r.level == Level::Warn));

        let (s, summary, reasons) = rate(None, Some(40));
        assert_eq!(s, Safety::Caution);
        assert_eq!(summary, "Your own deleted file");
        assert!(reasons[0].text.contains("check before emptying"));
        assert!(reasons.iter().any(|r| r.text.contains("40 days")));
        let (_, _, today) = rate(None, Some(0));
        assert!(today.iter().any(|r| r.text.contains("Deleted today")));
    }

    #[test]
    fn scopes_pick_items_by_age_and_owner() {
        let now = 100 * 86_400;
        let raws = vec![
            raw("old mine", Some("C:\\a"), now - 40 * 86_400),
            raw("old other", Some("C:\\b"), now - 40 * 86_400),
            raw("new mine", Some("C:\\c"), now - 2 * 86_400),
            raw("new other", Some("C:\\d"), now - 86_400 / 2),
        ];
        let log = vec![
            entry("C:\\a\\old mine", (now - 40 * 86_400) as u64, "Dev clean"),
            entry("C:\\c\\new mine", (now - 2 * 86_400) as u64, "Dev clean"),
        ];
        let items = build_items(&raws, &log, now);
        let names = |scope, days| {
            let mut v: Vec<&str> = items.iter().filter(|i| in_scope(i, scope, days)).map(|i| i.name.as_str()).collect();
            v.sort();
            v
        };
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert_eq!(names(Scope::All, 0).len(), 4);
            assert_eq!(names(Scope::Older, 30), vec!["old mine", "old other"]);
            assert_eq!(names(Scope::JharuOlder, 30), vec!["old mine"]);
            assert_eq!(names(Scope::JharuOlder, 1), vec!["new mine", "old mine"]);
            assert_eq!(names(Scope::Older, 0).len(), 4);
        }
    }

    #[test]
    fn report_totals_split_jharu_from_the_rest() {
        let now = 1_000_000;
        let mut raws = vec![raw("a", Some("C:\\x"), now - 10), raw("b", Some("C:\\y"), now - 10)];
        raws[0].size = 300;
        raws[1].size = 50;
        let log = vec![entry("C:\\x\\a", now as u64, "Dev clean")];
        let r = report_from(build_items(&raws, &log, now), None);
        assert_eq!(r.total_items, 2);
        assert_eq!(r.total_bytes, 350);
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert_eq!((r.jharu_items, r.jharu_bytes), (1, 300));
            assert_eq!((r.other_items, r.other_bytes), (1, 50));
            assert_eq!(r.items[0].name, "a", "largest first");
        }
    }

    #[test]
    fn log_report_marks_what_is_still_in_the_bin() {
        let raws = vec![raw("a", Some("C:\\x"), 5_000)];
        let log = vec![entry("C:\\x\\a", 5_001, "Dev clean"), entry("C:\\x\\gone", 4_000, "Dev clean")];
        let r = log_report(&raws, &log);
        assert_eq!(r.total_logged, 2);
        assert_eq!(r.entries[0].name, "a", "newest first");
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert_eq!(r.in_bin, 1);
            assert!(r.entries[0].in_bin && r.entries[0].bin_id.is_some());
            assert!(!r.entries[1].in_bin && r.entries[1].bin_id.is_none());
        }
    }

    #[test]
    fn matching_a_full_journal_against_a_big_bin_is_fast() {
        let raws: Vec<Raw> = (0..5_000).map(|n| raw(&format!("item{n}"), Some("C:/Users/me/My Folder"), 1_000_000 + n)).collect();
        let log: Vec<Entry> = (0..20_000u64)
            .map(|n| entry(&format!("C:/Users/me/My Folder/item{}", n % 7_000), 1_000_001 + n % 5_000, "Dev clean"))
            .collect();
        let start = std::time::Instant::now();
        let m = match_journal(&raws, &log);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert!(m.iter().filter(|x| x.is_some()).count() > 4_000);
        }
    }

    #[test]
    fn cancel_tokens_are_per_task() {
        cancel("t1");
        assert!(is_cancelled("t1") && !is_cancelled("t2"));
        forget("t1");
        assert!(!is_cancelled("t1"));
    }

    /// Read-only look at the real Trash:
    /// `cargo test --lib smoke_trash -- --ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn smoke_trash() {
        let start = std::time::Instant::now();
        let r = list_report().unwrap();
        println!(
            "listed in {:?}: {} items, {:.1} MB; jharu {} / other {}",
            start.elapsed(),
            r.total_items,
            r.total_bytes as f64 / 1e6,
            r.jharu_items,
            r.other_items
        );
        for i in r.items.iter().take(15) {
            println!("{:>9.1} KB {:?} {} dir={} from {:?} age {:?}", i.size_bytes as f64 / 1e3, i.safety, i.name, i.is_dir, i.original_folder, i.age_days);
        }
    }

    // ------------------------------------------------ the real Recycle Bin (Windows)
    //
    // These put a few fixture files in the real bin and take them out again.
    // Each item is found by its exact original path inside a folder made for
    // the test; nothing else in the bin is listed for action, let alone
    // restored or purged.

    #[cfg(target_os = "windows")]
    mod windows_bin {
        use super::*;
        use crate::trashlog::testing::TempJournal;

        /// A fixture folder (with a space in its name) that, whatever happens,
        /// is emptied out of the bin and off the disk when the test ends.
        struct Fix {
            dir: PathBuf,
        }

        impl Fix {
            fn new(tag: &str) -> Fix {
                let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
                let dir = std::env::temp_dir().join(format!("jharu bin {tag} {} {nanos}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                Fix { dir }
            }

            fn file(&self, name: &str, text: &str) -> PathBuf {
                let p = self.dir.join(name);
                std::fs::write(&p, text).unwrap();
                p
            }

            /// Bin items that were deleted from this fixture folder or one
            /// inside it. The folder's name is unique to the test run.
            fn ours(&self) -> Vec<Native> {
                let root = key(&self.dir);
                plat::list()
                    .unwrap()
                    .into_iter()
                    .filter(|n| {
                        n.raw.parent.as_ref().is_some_and(|p| {
                            let k = key(p);
                            k == root || k.starts_with(&format!("{root}/"))
                        })
                    })
                    .collect()
            }
        }

        impl Drop for Fix {
            fn drop(&mut self) {
                let left = self.ours();
                if !left.is_empty() {
                    let refs: Vec<&Native> = left.iter().collect();
                    let _ = plat::purge(&refs);
                }
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        fn never_cancelled() -> bool {
            false
        }

        #[test]
        fn restore_keeps_the_file_extension() {
            let fix = Fix::new("ext");
            let names = ["foo.txt", "archive.tar.gz", "noext", "report v2.final.docx", "UPPER.TXT", "a.txt.txt"];
            for n in names {
                fix.file(n, &format!("content of {n}"));
            }
            let folder = fix.dir.join("my.folder");
            std::fs::create_dir(&folder).unwrap();
            std::fs::write(folder.join("inner.rs"), "fn main() {}").unwrap();
            for n in names {
                trash::delete(fix.dir.join(n)).unwrap();
            }
            trash::delete(&folder).unwrap();

            // What the trash crate alone reports, for the record.
            let crate_names: Vec<String> = trash::os_limited::list()
                .unwrap()
                .into_iter()
                .filter(|t| key(&t.original_parent) == key(&fix.dir))
                .map(|t| t.name.to_string_lossy().into_owned())
                .collect();
            println!("trash crate display names: {crate_names:?}");

            let ours = fix.ours();
            // The bin's own record is readable, so names do not rest on the
            // extension guess alone; it also holds each item's size.
            for n in &ours {
                let info = plat::read_info(Path::new(&n.raw.id)).unwrap_or_else(|| panic!("no $I record for {}", n.raw.name));
                assert_eq!(info.original, fix.dir.join(&n.raw.name), "record names the original path");
                println!("{:<22} listed {:>3} B, record says {:>3} B, dir={}", n.raw.name, n.raw.size, info.size, n.raw.is_dir);
            }
            let folder_item = ours.iter().find(|n| n.raw.name == "my.folder").unwrap();
            assert!(folder_item.raw.is_dir);
            assert_eq!(folder_item.raw.size, 12, "folder size is the files inside it");
            let mut found: Vec<String> = ours.iter().map(|n| n.raw.name.clone()).collect();
            found.sort();
            let mut expected: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            expected.push("my.folder".into());
            expected.sort();
            assert_eq!(found, expected, "listing must show the real names");

            let ids: Vec<String> = ours.iter().map(|n| n.raw.id.clone()).collect();
            let out = restore_impl(&ids, &mut |_, _, _| {}).unwrap();
            assert!(out.errors.is_empty() && out.skipped.is_empty(), "{out:?}");
            assert_eq!(out.restored.len(), 7);
            for n in names {
                let p = fix.dir.join(n);
                assert!(p.is_file(), "{n} did not come back under its own name");
                assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("content of {n}"));
            }
            assert_eq!(std::fs::read_to_string(folder.join("inner.rs")).unwrap(), "fn main() {}");
            assert!(fix.ours().is_empty(), "restored items must leave the bin");
        }

        #[test]
        fn restore_skips_a_taken_place_and_recreates_a_missing_folder() {
            let fix = Fix::new("restore");
            fix.file("taken.txt", "original");
            let nested = fix.dir.join("sub folder");
            std::fs::create_dir(&nested).unwrap();
            std::fs::write(nested.join("deep.txt"), "deep").unwrap();
            trash::delete(fix.dir.join("taken.txt")).unwrap();
            trash::delete(nested.join("deep.txt")).unwrap();
            std::fs::remove_dir(&nested).unwrap();
            // A new file now sits where the first one was.
            fix.file("taken.txt", "newer");

            let ids: Vec<String> = fix.ours().iter().map(|n| n.raw.id.clone()).collect();
            let out = restore_impl(&ids, &mut |_, _, _| {}).unwrap();
            assert_eq!(out.restored.len(), 1, "{out:?}");
            assert!(out.skipped.iter().any(|s| s.starts_with("taken.txt: something already exists")), "{out:?}");
            assert_eq!(std::fs::read_to_string(fix.dir.join("taken.txt")).unwrap(), "newer", "never overwrites");
            assert_eq!(std::fs::read_to_string(nested.join("deep.txt")).unwrap(), "deep");
            // The skipped one is still in the bin, untouched.
            assert_eq!(fix.ours().len(), 1);
            // And a request for something that left the bin is reported, not fatal.
            let out = restore_impl(&["no such id".to_string()], &mut |_, _, _| {}).unwrap();
            assert_eq!(out.skipped.len(), 1);
        }

        #[test]
        fn purge_removes_only_the_requested_items_and_rechecks_scope() {
            let _j = TempJournal::new("purge");
            let fix = Fix::new("purge");
            for n in ["mine.cache", "yours.txt", "kept.txt", "folder gone.txt"] {
                fix.file(n, &"x".repeat(2048));
            }
            let mine = fix.dir.join("mine.cache");
            trash::delete(&mine).unwrap();
            trashlog::record(&mine, 2048, "Dev clean");
            for n in ["yours.txt", "kept.txt", "folder gone.txt"] {
                trash::delete(fix.dir.join(n)).unwrap();
            }

            let natives = fix.ours();
            assert_eq!(natives.len(), 4);
            let req = |name: &str| {
                let n = natives.iter().find(|n| n.raw.name == name).unwrap();
                PurgeRequest { id: n.raw.id.clone(), deleted: n.raw.deleted }
            };
            let report = list_report().unwrap();
            let item = |name: &str| report.items.iter().find(|i| i.name == name && i.original_folder.as_deref().is_some_and(|f| key(Path::new(f)) == key(&fix.dir))).unwrap();
            assert!(item("mine.cache").by_jharu);
            assert_eq!(item("mine.cache").source.as_deref(), Some("Dev clean"));
            assert_eq!(item("mine.cache").safety, Safety::Safe);
            assert!(!item("yours.txt").by_jharu);
            assert_eq!(item("yours.txt").safety, Safety::Caution);
            assert!(item("yours.txt").size_bytes >= 2048);

            // "Only Jharu's, older than 0 days", asked of one Jharu item and
            // one of the user's: the user's is skipped as out of scope.
            let out = purge_impl(
                &[req("mine.cache"), req("yours.txt")],
                Scope::JharuOlder,
                0,
                &never_cancelled,
                &mut |_, _, _| {},
            )
            .unwrap();
            println!("{out:?}");
            assert_eq!(out.purged, 1);
            assert!(out.freed_bytes >= 2048);
            assert!(out.errors.is_empty(), "{out:?}");
            assert!(out.skipped.iter().any(|s| s.starts_with("yours.txt: no longer matches")), "{out:?}");
            let left: Vec<String> = fix.ours().iter().map(|n| n.raw.name.clone()).collect();
            assert!(!left.contains(&"mine.cache".to_string()));
            assert_eq!(left.len(), 3, "{left:?}");
            assert!(!mine.exists(), "purged means gone, not restored");

            // An item whose deletion time differs from what the user saw is a
            // different item and is left alone; one already gone is counted.
            let mut stale = req("kept.txt");
            stale.deleted = stale.deleted.map(|d| d - 3600);
            let out = purge_impl(
                &[stale, PurgeRequest { id: "gone".into(), deleted: None }],
                Scope::All,
                0,
                &never_cancelled,
                &mut |_, _, _| {},
            )
            .unwrap();
            assert_eq!((out.purged, out.already_gone), (0, 1), "{out:?}");
            assert!(out.skipped.iter().any(|s| s.starts_with("kept.txt: a different item")));
            assert_eq!(fix.ours().len(), 3);
            // A request that carries no deletion time cannot show it is the same item.
            let mut unsure = req("kept.txt");
            unsure.deleted = None;
            let out = purge_impl(&[unsure], Scope::All, 0, &never_cancelled, &mut |_, _, _| {}).unwrap();
            assert_eq!(out.purged, 0, "{out:?}");
            assert_eq!(out.skipped.len(), 1, "{out:?}");
            assert_eq!(fix.ours().len(), 3);

            // Cancelled before the first batch: nothing is removed.
            let out = purge_impl(&[req("kept.txt")], Scope::All, 0, &|| true, &mut |_, _, _| {}).unwrap();
            assert!(out.cancelled);
            assert_eq!(out.purged, 0);
            assert_eq!(fix.ours().len(), 3);

            // Everything left in the request, scope All.
            let out = purge_impl(
                &[req("kept.txt"), req("yours.txt"), req("folder gone.txt")],
                Scope::All,
                0,
                &never_cancelled,
                &mut |_, _, _| {},
            )
            .unwrap();
            assert_eq!(out.purged, 3, "{out:?}");
            assert!(fix.ours().is_empty());
        }

        #[test]
        fn restore_log_follows_an_item_into_and_out_of_the_bin() {
            let _j = TempJournal::new("log");
            let fix = Fix::new("log");
            let p = fix.file("from jharu.log", "log text");
            trash::delete(&p).unwrap();
            trashlog::record(&p, 8, "System folders");

            let log = log_impl().unwrap();
            let e = log.entries.iter().find(|e| key(Path::new(&e.path)) == key(&p)).expect("journal row");
            assert!(e.in_bin && !e.destination_taken, "{e:?}");
            assert_eq!(e.name, "from jharu.log");
            assert_eq!(e.source, "System folders");
            let id = e.bin_id.clone().unwrap();

            let out = restore_impl(&[id], &mut |_, _, _| {}).unwrap();
            assert_eq!(out.restored.len(), 1, "{out:?}");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), "log text");

            let log = log_impl().unwrap();
            let e = log.entries.iter().find(|e| key(Path::new(&e.path)) == key(&p)).unwrap();
            assert!(!e.in_bin, "restored, so no longer in the bin");
        }

        #[test]
        fn cleaning_a_folder_is_journaled_and_shows_up_as_jharus() {
            let _j = TempJournal::new("clean");
            let fix = Fix::new("clean");
            fix.file("old.log", "log");
            fix.file("scratch file.tmp", "tmp");
            let mut out = crate::cleaning::LocationOutcome::default();
            crate::cleaning::clean_contents(&fix.dir, std::time::Duration::from_secs(0), &mut out, &mut |_, _| {});
            assert_eq!(out.moved, 2, "{out:?}");

            let logged: Vec<Entry> = trashlog::read_all();
            assert_eq!(logged.len(), 2, "{logged:?}");
            assert!(logged.iter().all(|e| e.source == "System folders"));
            let report = list_report().unwrap();
            let mine: Vec<&BinItem> = report
                .items
                .iter()
                .filter(|i| i.original_folder.as_deref().is_some_and(|f| key(Path::new(f)) == key(&fix.dir)))
                .collect();
            assert_eq!(mine.len(), 2);
            assert!(mine.iter().all(|i| i.by_jharu && i.safety == Safety::Safe), "{mine:?}");
            let mut names: Vec<&str> = mine.iter().map(|i| i.name.as_str()).collect();
            names.sort();
            assert_eq!(names, vec!["old.log", "scratch file.tmp"]);
        }

        /// Read-only: `cargo test --lib smoke_bin -- --ignored --nocapture`
        #[test]
        #[ignore]
        fn smoke_bin() {
            let start = std::time::Instant::now();
            let r = list_report().unwrap();
            println!(
                "listed in {:?}: {} items, {:.1} MB; jharu {} / other {}",
                start.elapsed(),
                r.total_items,
                r.total_bytes as f64 / 1e6,
                r.jharu_items,
                r.other_items
            );
            for i in r.items.iter().take(15) {
                println!("{:>9.1} KB {:?} {} dir={} from {:?} age {:?}", i.size_bytes as f64 / 1e3, i.safety, i.name, i.is_dir, i.original_folder, i.age_days);
            }
        }
    }
}
