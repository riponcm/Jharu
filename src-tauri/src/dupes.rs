//! Duplicate finder: the same content stored twice.
//!
//! Two kinds of duplicates are looked for in one pass:
//!
//! 1. Your own files in the folders you choose (default: Documents, Downloads,
//!    Desktop, Pictures, Videos and Music).
//! 2. The same large model or package kept by several tools: Hugging Face,
//!    Ollama, LM Studio, PyTorch hub and friends, and the wheel and package
//!    caches of pip, uv and conda. A GGUF pulled into Ollama and downloaded
//!    again in LM Studio is the typical find.
//!
//! How files are compared, cheapest test first, so most files are never read:
//!
//! 1. Size. A file with a unique size has no twin (files under the minimum
//!    size, 1 MB by default, are not looked at at all).
//! 2. File identity. Paths that lead to the same physical file (hard links)
//!    are not duplicates: they already share space. They are collapsed to one
//!    copy before anything is read, and listed separately as "already shared".
//! 3. The first and last 64 KB. Different files almost always differ there.
//! 4. A hash of the whole file, only for what is still equal. This is the
//!    verdict: Jharu never treats files as equal by size, name or date alone.
//!
//! The hash is BLAKE3 (the `blake3` crate). Duplicate detection reads every
//! byte of every surviving candidate, often tens of gigabytes of model files
//! on one disk, so speed decides how long the user waits. BLAKE3 hashes at
//! several GB/s per core with SIMD, which keeps the scan bound by the disk
//! rather than the CPU, where SHA-256 would be several times slower. It is a
//! cryptographic hash, so equal hashes mean equal content for all practical
//! purposes. Nothing is removed on a hash alone either: the removal step
//! hashes the kept copy and the copy to remove again, right before acting.
//!
//! Which copy to keep is decided by [`keep_score`] (a tool that is running
//! now, a copy that cannot be removed anyway, a copy outside Downloads, then
//! the oldest); the user can pick another. Every copy carries a safety rating
//! with reasons, as in Dev clean: tool caches are rated by `safety::assess`,
//! the user's own files are at best Caution, and anything inside a Git
//! repository, an app install or a cloud-sync folder is Risky.

use crate::rules::{all_rules, Rule, Safety};
use crate::safety::{assess, Facts, Level, Reason};
use jwalk::rayon::prelude::*;
use jwalk::rayon::{ThreadPool, ThreadPoolBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tauri::Emitter;

/// Files smaller than this are ignored unless the user lowers it.
const DEFAULT_MIN_BYTES: u64 = 1 << 20;
/// Never look below this: a threshold of a few bytes would match every empty
/// config file on the disk.
const FLOOR_BYTES: u64 = 4 << 10;
/// How much of each end of a file the quick comparison reads.
const EDGE: u64 = 64 << 10;
/// Read size for full hashes; also how often a cancel request is noticed.
const CHUNK: usize = 1 << 20;
/// How many unreadable files to name in the report.
const UNREADABLE_LISTED: usize = 20;

// ---------------------------------------------------------------- report

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The user's own files.
    Files,
    /// A model stored by an ML tool.
    Model,
    /// A package or wheel kept by a package manager.
    Package,
}

/// One physical copy of the content. Several paths only when they are hard
/// links of each other.
#[derive(Serialize, Clone, Debug)]
pub struct DupeCopy {
    pub path: String,
    /// Other paths that are the same file (hard links). Removing this copy
    /// removes all of them.
    pub link_paths: Vec<String>,
    pub size_bytes: u64,
    pub modified_days: Option<u64>,
    /// Where it lives, in words: "Downloads", "Ollama models", ...
    pub place: String,
    pub safety: Safety,
    pub reasons: Vec<Reason>,
    /// False for copies Jharu will never remove (inside a Git repository or
    /// an app).
    pub removable: bool,
    /// What removing this copy really frees: 0 when the same file is also
    /// hard-linked somewhere that was not scanned.
    pub reclaim_bytes: u64,
    /// Ticked by default: only copies rated Safe.
    pub preselect: bool,
    /// The copy Jharu suggests keeping.
    pub keep: bool,
    /// Why it is the suggested one.
    pub keep_note: Option<String>,
    #[serde(skip)]
    modified_secs: u64,
    #[serde(skip)]
    score: i64,
    /// The tool that owns this copy is running now.
    #[serde(skip)]
    in_use: bool,
    /// The copy lives in a tool's own folder.
    #[serde(skip)]
    in_tool: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct DupeGroup {
    /// Hash of the content; removal requests carry it back for re-checking.
    pub hash: String,
    pub name: String,
    pub kind: Kind,
    /// Size of each copy.
    pub size_bytes: u64,
    pub copies: Vec<DupeCopy>,
    /// What removing every copy except the suggested one would free.
    pub wasted_bytes: u64,
}

/// Paths that are one file: already sharing space, not duplicates.
#[derive(Serialize, Clone, Debug)]
pub struct HardLinkSet {
    pub name: String,
    pub size_bytes: u64,
    pub paths: Vec<String>,
    /// Space the extra links would cost if they were separate copies.
    pub shared_bytes: u64,
    /// APFS clones rather than hard links: separate files that share their
    /// data until one is changed.
    pub clones: bool,
}

#[derive(Serialize, Default, Debug)]
pub struct DupeReport {
    pub groups: Vec<DupeGroup>,
    pub hard_links: Vec<HardLinkSet>,
    pub hard_link_bytes: u64,
    /// Sum of every group's `wasted_bytes`.
    pub wasted_bytes: u64,
    /// The part of it in copies rated Safe.
    pub safe_bytes: u64,
    /// Files at or above the minimum size that were looked at.
    pub files_seen: u64,
    /// Cloud placeholders skipped without being read.
    pub online_only_skipped: u64,
    pub unreadable_count: u64,
    pub unreadable: Vec<String>,
    pub min_bytes: u64,
    /// The folders that were searched.
    pub roots: Vec<String>,
    /// Tool caches that were searched in addition (display names).
    pub caches: Vec<String>,
    pub cancelled: bool,
    pub seconds: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct RootInfo {
    pub path: String,
    pub label: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ScanRequest {
    pub roots: Vec<String>,
    pub min_bytes: Option<u64>,
    pub include_caches: bool,
}

// ---------------------------------------------------------------- jobs

/// Cancel requests by token, set by `dupes_cancel` and polled by the work.
fn cancel_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

fn token_cancelled(token: &str) -> bool {
    cancel_set().lock().map(|s| s.contains(token)).unwrap_or(false)
}

type CancelFn<'a> = &'a (dyn Fn() -> bool + Sync);
type EmitFn<'a> = &'a (dyn Fn(&str, u64, u64, &str) + Sync);

/// Cancellation and (throttled) progress for one long operation.
pub struct Job<'a> {
    cancel: CancelFn<'a>,
    emit: EmitFn<'a>,
    last: Mutex<Instant>,
}

impl<'a> Job<'a> {
    pub fn new(cancel: CancelFn<'a>, emit: EmitFn<'a>) -> Job<'a> {
        let now = Instant::now();
        let long_ago = now.checked_sub(std::time::Duration::from_secs(1)).unwrap_or(now);
        Job { cancel, emit, last: Mutex::new(long_ago) }
    }

    fn cancelled(&self) -> bool {
        (self.cancel)()
    }

    /// Progress is sent at most five times a second unless `force`d.
    fn report(&self, phase: &str, done: u64, total: u64, current: &str, force: bool) {
        let Ok(mut last) = self.last.lock() else { return };
        if !force && last.elapsed() < std::time::Duration::from_millis(200) {
            return;
        }
        *last = Instant::now();
        drop(last);
        (self.emit)(phase, done, total, current);
    }
}

fn hash_pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        // Reading several large files at once helps SSDs and costs little on
        // spinning disks at this size; more threads would only add seeks.
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 4);
        ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("jharu-hash-{i}"))
            .build()
            .expect("could not start the hashing thread pool")
    })
}

// ---------------------------------------------------------------- paths

/// Comparable form of a path: separators unified, no trailing slash, and
/// case-folded where the file system ignores case.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        s.to_lowercase()
    } else {
        s
    }
}

fn is_inside(child: &Path, parent: &Path) -> bool {
    let (c, p) = (path_key(child), path_key(parent));
    c.len() > p.len() && c.starts_with(&p) && c.as_bytes()[p.len()] == b'/'
}

fn days_since(secs: u64) -> Option<u64> {
    if secs == 0 {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now.saturating_sub(secs) / 86_400)
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Folders never searched below the chosen roots: other people's repositories
/// and dependency trees are full of identical files by design, and the rest
/// are system-managed.
fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "$Recycle.Bin" | "System Volume Information" | ".Trash"
            | ".Trashes" | "Photos Library.photoslibrary"
    )
}

// ---------------------------------------------------------------- locations

/// The tool caches searched for the same model or package stored twice.
const CACHE_RULES: &[(&str, Kind)] = &[
    ("hf-hub", Kind::Model),
    ("ollama", Kind::Model),
    ("lm-studio", Kind::Model),
    ("torch", Kind::Model),
    ("keras", Kind::Model),
    ("whisper", Kind::Model),
    ("pip", Kind::Package),
    ("uv", Kind::Package),
    ("conda-pkgs", Kind::Package),
];

/// Where each tool keeps its files on this machine, built from the same
/// rules Dev clean uses.
pub struct Locations {
    rules: Vec<(Rule, Kind)>,
    /// (index into `rules`, folder)
    roots: Vec<(usize, PathBuf)>,
}

impl Locations {
    pub fn none() -> Locations {
        Locations { rules: vec![], roots: vec![] }
    }

    /// Searches the folders the rules name, plus the locations users move
    /// them to with environment variables.
    pub fn discover() -> Locations {
        let mut found = Locations::none();
        for rule in all_rules() {
            let Some(&(_, kind)) = CACHE_RULES.iter().find(|(id, _)| *id == rule.id) else { continue };
            let mut folders = rule.paths();
            for var in match rule.id {
                "hf-hub" => &["HF_HOME", "HF_HUB_CACHE"][..],
                "ollama" => &["OLLAMA_MODELS"][..],
                "torch" => &["TORCH_HOME"][..],
                _ => &[][..],
            } {
                if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                    folders.push(PathBuf::from(v));
                }
            }
            let mut seen = HashSet::new();
            let folders: Vec<PathBuf> = folders
                .into_iter()
                .filter(|f| f.is_dir() && seen.insert(path_key(f)))
                .collect();
            if folders.is_empty() {
                continue;
            }
            found.rules.push((rule, kind));
            let idx = found.rules.len() - 1;
            found.roots.extend(folders.into_iter().map(|f| (idx, f)));
        }
        found
    }

    /// A fixture or test setup: one rule id at one folder.
    #[cfg(test)]
    fn with(entries: &[(&str, &Path)]) -> Locations {
        let mut found = Locations::none();
        for (id, folder) in entries {
            let rule = all_rules().into_iter().find(|r| r.id == *id).unwrap();
            let kind = CACHE_RULES.iter().find(|(i, _)| i == id).unwrap().1;
            found.rules.push((rule, kind));
            found.roots.push((found.rules.len() - 1, folder.to_path_buf()));
        }
        found
    }

    /// The tool whose folder holds `path`, preferring the deepest match.
    fn find(&self, path: &Path) -> Option<(&Rule, &Path, Kind)> {
        self.roots
            .iter()
            .filter(|(_, root)| is_inside(path, root))
            .max_by_key(|(_, root)| path_key(root).len())
            .map(|(i, root)| (&self.rules[*i].0, root.as_path(), self.rules[*i].1))
    }

    fn folders(&self) -> Vec<(PathBuf, String)> {
        self.roots.iter().map(|(i, p)| (p.clone(), self.rules[*i].0.name.to_string())).collect()
    }
}

/// The default folders: the user content folders under the home directory.
pub fn default_user_roots() -> Vec<RootInfo> {
    let mut out: Vec<RootInfo> = vec![];
    let mut seen = HashSet::new();
    for (label, dir) in [
        ("Documents", dirs::document_dir()),
        ("Downloads", dirs::download_dir()),
        ("Desktop", dirs::desktop_dir()),
        ("Pictures", dirs::picture_dir()),
        ("Videos", dirs::video_dir()),
        ("Music", dirs::audio_dir()),
    ] {
        if let Some(d) = dir.filter(|d| d.is_dir()) {
            if seen.insert(path_key(&d)) {
                out.push(RootInfo { path: d.to_string_lossy().into_owned(), label: label.into() });
            }
        }
    }
    out
}

/// A short name for a folder the user chose: the well-known folder it is,
/// else its own name.
fn root_label(path: &Path) -> String {
    for r in default_user_roots() {
        if path_key(Path::new(&r.path)) == path_key(path) {
            return r.label;
        }
    }
    file_name(path)
}

// ---------------------------------------------------------------- hashing

struct Cand {
    path: PathBuf,
    size: u64,
}

/// One physical file, with every path found for it.
#[derive(Debug)]
struct Physical {
    size: u64,
    paths: Vec<PathBuf>,
    /// How many paths lead to this file in total, scanned or not.
    links: u64,
    mtime: u64,
    /// The paths are APFS clones sharing one copy of the data, not hard
    /// links. How many more clones exist elsewhere is not known.
    clone: bool,
}

type Digest = [u8; 32];

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn changed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, "the file changed while it was being read")
}

/// Hash of the size plus the first and last 64 KB. Smaller files are read
/// whole, and the second value says the hash already covers everything.
fn hash_edges(path: &Path, size: u64) -> std::io::Result<(Digest, bool)> {
    if size <= 2 * EDGE {
        let h = hash_full(path, size, &|| false, &|_| {})?.ok_or_else(changed)?;
        return Ok((h, true));
    }
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let mut buf = vec![0u8; EDGE as usize];
    if read_full(&mut f, &mut buf)? != buf.len() {
        return Err(changed());
    }
    hasher.update(&buf);
    f.seek(SeekFrom::Start(size - EDGE))?;
    if read_full(&mut f, &mut buf)? != buf.len() {
        return Err(changed());
    }
    hasher.update(&buf);
    Ok((*hasher.finalize().as_bytes(), false))
}

/// BLAKE3 of the whole file. `None` when cancelled. A file that is shorter
/// or longer than `size` was changed by someone else and is an error.
fn hash_full(path: &Path, size: u64, cancel: &dyn Fn() -> bool, bytes: &dyn Fn(u64)) -> std::io::Result<Option<Digest>> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK.min(size as usize).max(4096)];
    let mut total = 0u64;
    loop {
        if cancel() {
            return Ok(None);
        }
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        bytes(n as u64);
        if n < buf.len() {
            break;
        }
    }
    if total != size {
        return Err(changed());
    }
    Ok(Some(*hasher.finalize().as_bytes()))
}

fn hex(d: &Digest) -> String {
    blake3::Hash::from_bytes(*d).to_hex().to_string()
}

fn parse_hex(s: &str) -> Option<Digest> {
    blake3::Hash::from_hex(s).ok().map(|h| *h.as_bytes())
}

// ---------------------------------------------------------------- listing

/// Folders to search, without any that sit inside another, so no file is
/// reached twice (it would look like a hard link of itself).
fn disjoint(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort_by_key(|r| path_key(r).len());
    let mut kept: Vec<PathBuf> = vec![];
    for r in roots {
        if !kept.iter().any(|k| path_key(k) == path_key(&r) || is_inside(&r, k)) {
            kept.push(r);
        }
    }
    kept
}

/// Every regular file of at least `min` bytes, grouped by size.
fn list_files(roots: &[PathBuf], min: u64, job: &Job, stats: &mut Stats) -> (HashMap<u64, Vec<Cand>>, u64) {
    let mut by_size: HashMap<u64, Vec<Cand>> = HashMap::new();
    let mut seen_files = 0u64;
    let mut entries = 0u64;
    for root in roots {
        let walker = crate::walk::walk(root)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(|_, _, _, children| {
                // A Python environment is not searched: the same package files
                // in several environments are the environments' business.
                let is_env = children.iter().flatten().any(|e| {
                    (e.file_type().is_file() && e.file_name == "pyvenv.cfg")
                        || (e.file_type().is_dir() && e.file_name == "conda-meta")
                });
                if is_env {
                    children.clear();
                    return;
                }
                children.retain(|c| match c {
                    Ok(e) => !(e.file_type().is_dir() && skip_dir(&e.file_name.to_string_lossy())),
                    Err(_) => true,
                });
            });
        for entry in walker {
            // A folder that cannot be opened is reported, not skipped quietly:
            // a scan that found nothing must say whether it could look.
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let at = e.path().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone());
                    stats.add_unreadable(&at, e);
                    continue;
                }
            };
            entries += 1;
            if entries % 512 == 0 {
                if job.cancelled() {
                    return (by_size, seen_files);
                }
                job.report("listing", seen_files, 0, &entry.parent_path().to_string_lossy(), false);
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    stats.add_unreadable(&entry.path(), e);
                    continue;
                }
            };
            if meta.len() < min {
                continue;
            }
            seen_files += 1;
            by_size.entry(meta.len()).or_default().push(Cand { path: entry.path(), size: meta.len() });
        }
    }
    (by_size, seen_files)
}

#[derive(Default)]
struct Stats {
    online_only: u64,
    unreadable: Vec<String>,
    unreadable_count: u64,
}

impl Stats {
    fn add_unreadable(&mut self, path: &Path, why: impl std::fmt::Display) {
        self.unreadable_count += 1;
        if self.unreadable.len() < UNREADABLE_LISTED {
            self.unreadable.push(format!("{}: {why}", path.display()));
        }
    }
}

/// Reads each candidate's identity (no file contents) and merges paths that
/// are the same file.
fn collapse(cands: Vec<Cand>, job: &Job, stats: &mut Stats) -> Vec<Physical> {
    enum Info {
        Gone(String),
        Online,
        File { id: Option<(u64, u64)>, clone: Option<(u64, u64)>, links: u64, mtime: u64 },
    }
    let done = AtomicU64::new(0);
    let total = cands.len() as u64;
    let infos: Vec<(Cand, Info)> = hash_pool().install(|| {
        cands
            .into_par_iter()
            .map(|c| {
                let info = if job.cancelled() {
                    Info::Gone("cancelled".into())
                } else {
                    match std::fs::symlink_metadata(&c.path) {
                        Err(e) => Info::Gone(e.to_string()),
                        Ok(meta) if !meta.is_file() || meta.len() != c.size => Info::Gone("it changed during the scan".into()),
                        Ok(meta) => {
                            let fi = crate::usage::file_info(&c.path, &meta);
                            if fi.online_only {
                                Info::Online
                            } else {
                                Info::File { id: fi.id, clone: fi.clone, links: fi.links, mtime: mtime_secs(&meta) }
                            }
                        }
                    }
                };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                job.report("checking", n, total, "", false);
                (c, info)
            })
            .collect()
    });

    let mut out: Vec<Physical> = vec![];
    let mut by_id: HashMap<(u64, u64, u64), usize> = HashMap::new();
    let mut by_clone: HashMap<(u64, u64, u64), usize> = HashMap::new();
    for (c, info) in infos {
        match info {
            Info::Gone(why) => {
                if why != "cancelled" {
                    stats.add_unreadable(&c.path, why);
                }
            }
            Info::Online => stats.online_only += 1,
            Info::File { id, clone, links, mtime } => match (id, clone) {
                (Some((vol, idx)), _) => {
                    if let Some(&i) = by_id.get(&(c.size, vol, idx)) {
                        out[i].paths.push(c.path);
                    } else {
                        by_id.insert((c.size, vol, idx), out.len());
                        out.push(Physical { size: c.size, paths: vec![c.path], links, mtime, clone: false });
                    }
                }
                // Untouched APFS clones of one another hold one copy of the
                // data between them: like hard links, not duplicates.
                (None, Some((vol, cid))) => {
                    if let Some(&i) = by_clone.get(&(c.size, vol, cid)) {
                        out[i].paths.push(c.path);
                    } else {
                        by_clone.insert((c.size, vol, cid), out.len());
                        out.push(Physical { size: c.size, paths: vec![c.path], links, mtime, clone: true });
                    }
                }
                (None, None) => out.push(Physical { size: c.size, paths: vec![c.path], links, mtime, clone: false }),
            },
        }
    }
    for p in &mut out {
        p.paths.sort_by(|a, b| a.as_os_str().len().cmp(&b.as_os_str().len()).then_with(|| a.cmp(b)));
    }
    out
}

/// Groups physical files that are the same, using the cheap-to-expensive
/// ladder above. Each returned bucket holds at least two files with
/// identical content.
fn find_equal(candidates: Vec<Physical>, job: &Job, stats: &mut Stats) -> Vec<(Digest, Vec<Physical>)> {
    // Same size is the first filter, applied again now that hard links are merged.
    let mut by_size: BTreeMap<u64, Vec<Physical>> = BTreeMap::new();
    for p in candidates {
        by_size.entry(p.size).or_default().push(p);
    }
    let pool: Vec<Physical> = by_size.into_values().filter(|v| v.len() >= 2).flatten().collect();
    if pool.is_empty() {
        return vec![];
    }

    // First and last 64 KB.
    let done = AtomicU64::new(0);
    let total = pool.len() as u64;
    let edged: Vec<(Physical, std::io::Result<(Digest, bool)>)> = hash_pool().install(|| {
        pool.into_par_iter()
            .map(|p| {
                let r = if job.cancelled() { Err(changed()) } else { hash_edges(&p.paths[0], p.size) };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                job.report("comparing", n, total, &file_name(&p.paths[0]), false);
                (p, r)
            })
            .collect()
    });
    let mut buckets: HashMap<(u64, Digest), Vec<(Physical, bool)>> = HashMap::new();
    for (p, r) in edged {
        match r {
            Ok((d, whole)) => buckets.entry((p.size, d)).or_default().push((p, whole)),
            Err(e) => {
                if !job.cancelled() {
                    stats.add_unreadable(&p.paths[0], e)
                }
            }
        }
    }
    if job.cancelled() {
        return vec![];
    }

    // Full hash, only for what the edges could not tell apart.
    let mut finished: Vec<(Digest, Vec<Physical>)> = vec![];
    let mut to_hash: Vec<Physical> = vec![];
    for ((_, d), members) in buckets {
        if members.len() < 2 {
            continue;
        }
        if members.iter().all(|(_, whole)| *whole) {
            finished.push((d, members.into_iter().map(|(p, _)| p).collect()));
        } else {
            to_hash.extend(members.into_iter().map(|(p, _)| p));
        }
    }
    let total_bytes: u64 = to_hash.iter().map(|p| p.size).sum();
    let hashed = AtomicU64::new(0);
    let full: Vec<(Physical, std::io::Result<Option<Digest>>)> = hash_pool().install(|| {
        to_hash
            .into_par_iter()
            .map(|p| {
                let name = file_name(&p.paths[0]);
                let r = hash_full(&p.paths[0], p.size, &|| job.cancelled(), &|n| {
                    let d = hashed.fetch_add(n, Ordering::Relaxed) + n;
                    job.report("hashing", d, total_bytes, &name, false);
                });
                (p, r)
            })
            .collect()
    });
    let mut by_hash: HashMap<(u64, Digest), Vec<Physical>> = HashMap::new();
    for (p, r) in full {
        match r {
            Ok(Some(d)) => by_hash.entry((p.size, d)).or_default().push(p),
            Ok(None) => {}
            Err(e) => stats.add_unreadable(&p.paths[0], e),
        }
    }
    if job.cancelled() {
        return vec![];
    }
    finished.extend(by_hash.into_iter().filter(|(_, v)| v.len() >= 2).map(|((_, d), v)| (d, v)));
    finished
}

// ---------------------------------------------------------------- safety

/// Facts about this machine and the tool folders, shared by every rating.
pub struct Ctx<'a> {
    pub locations: &'a Locations,
    pub facts: &'a Facts,
}

struct Rating {
    safety: Safety,
    reasons: Vec<Reason>,
    /// False for copies Jharu never removes.
    removable: bool,
    /// The tool that owns this copy and is running now.
    in_use: Option<String>,
    owner: Option<String>,
}

/// The nearest folder above `path` that is a Git repository's working tree.
fn git_repo_of(path: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir().map(|h| path_key(&h));
    path.ancestors().skip(1).find_map(|a| {
        if home.as_deref() == Some(path_key(a).as_str()) {
            return None;
        }
        a.join(".git").exists().then(|| a.to_path_buf())
    })
}

/// The Python environment (virtualenv or conda env) holding `path`. With
/// `below`, only folders inside it count: conda keeps its package cache
/// (`pkgs`) next to the base environment's `conda-meta`, and that cache is not
/// part of the environment.
fn python_env_of(path: &Path, below: Option<&Path>) -> Option<PathBuf> {
    path.ancestors().skip(1).take_while(|a| below.map_or(true, |b| is_inside(a, b))).find_map(|a| {
        (a.join("pyvenv.cfg").is_file() || a.join("conda-meta").is_dir()).then(|| a.to_path_buf())
    })
}

/// "report (1).pdf", "report - Copy.pdf": names a browser or file manager
/// gives to the second copy of something.
fn copy_like_name(name: &str) -> bool {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name).to_lowercase();
    let numbered = stem.ends_with(')')
        && stem.rfind(" (").is_some_and(|i| stem[i + 2..stem.len() - 1].chars().all(|c| c.is_ascii_digit()) && stem.len() - 1 > i + 2);
    numbered || stem.ends_with(" - copy") || stem.ends_with(" copy") || stem.starts_with("copy of ")
}

/// An app install or an app-managed library: files there belong to the app.
fn app_folder_of(path: &Path) -> Option<String> {
    for comp in path.components() {
        let n = comp.as_os_str().to_string_lossy().to_lowercase();
        if n.ends_with(".app") || n.ends_with(".photoslibrary") || n.ends_with(".lrlibrary") {
            return Some(comp.as_os_str().to_string_lossy().into_owned());
        }
    }
    let mut roots: Vec<PathBuf> = vec![];
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData", "windir"] {
            if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                roots.push(PathBuf::from(v));
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    for r in ["/Applications", "/System", "/Library", "/usr", "/opt", "/bin", "/sbin"] {
        roots.push(PathBuf::from(r));
    }
    roots.iter().find(|r| is_inside(path, r)).map(|r| r.to_string_lossy().into_owned())
}

/// The sync service whose folder holds `path`.
fn cloud_service(path: &Path) -> Option<&'static str> {
    for comp in path.components() {
        let n = comp.as_os_str().to_string_lossy().to_lowercase();
        let service = if n.starts_with("onedrive") {
            "OneDrive"
        } else if n == "dropbox" || n.starts_with("dropbox (") {
            "Dropbox"
        } else if matches!(n.as_str(), "icloud drive" | "iclouddrive" | "mobile documents" | "com~apple~clouddocs") {
            "iCloud Drive"
        } else if matches!(n.as_str(), "google drive" | "googledrive" | "my drive") {
            "Google Drive"
        } else if matches!(n.as_str(), "box" | "box sync") {
            "Box"
        } else if matches!(n.as_str(), "pcloud drive" | "pcloud") {
            "pCloud"
        } else {
            continue;
        };
        return Some(service);
    }
    for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            if is_inside(path, Path::new(&v)) {
                return Some("OneDrive");
            }
        }
    }
    None
}

/// What the tool that owns a cache folder loses when one of its files goes.
fn tool_effect(rule: &Rule, path: &Path) -> (Level, String) {
    let hf_blob = path.components().any(|c| c.as_os_str().to_string_lossy() == "blobs")
        && path.components().any(|c| c.as_os_str().to_string_lossy().starts_with("models--"));
    match rule.id {
        "hf-hub" if hf_blob => (
            Level::Warn,
            "Hugging Face's snapshot folders link to this file, so removing it leaves that cached model incomplete until it is downloaded again. `huggingface-cli delete-cache` removes whole models more cleanly.".into(),
        ),
        "hf-hub" => (Level::Warn, "Hugging Face re-downloads it the next time code asks for this model.".into()),
        "ollama" => (
            Level::Warn,
            "Ollama reads this file as part of a model. If it is removed, that model fails to load until you run `ollama pull` for it again.".into(),
        ),
        "lm-studio" => (
            Level::Warn,
            "LM Studio lists this file as a downloaded model; it disappears from the library until downloaded again.".into(),
        ),
        "torch" | "keras" | "whisper" => (
            Level::Warn,
            format!("{} downloads it again the next time code asks for this model.", rule.name),
        ),
        _ => (Level::Ok, format!("{} fetches it again when an install needs it.", rule.name)),
    }
}

fn rate(p: &Physical, others: usize, ctx: &Ctx) -> Rating {
    let path = &p.paths[0];
    let mut safety = Safety::Safe;
    let mut reasons: Vec<Reason> = vec![];
    let mut removable = true;
    let mut in_use = None;
    let mut owner = None;
    let mut note = |level: Level, at_least: Safety, text: String| {
        if at_least > safety {
            safety = at_least;
        }
        reasons.push(Reason { level, text });
    };

    let copies = if others == 1 { "another place".to_string() } else { format!("{others} other places") };
    note(
        Level::Ok,
        Safety::Safe,
        format!("The same content is stored in {copies}, compared byte for byte with a hash. Removing this copy loses no data."),
    );

    let tool = ctx.locations.find(path);
    if let Some((rule, root, _)) = tool {
        owner = Some(rule.name.to_string());
        // The rule's own rating and its live checks (tool running, lock held).
        let (s, rs) = assess(rule, root, ctx.facts);
        for r in rs {
            note(r.level, s, r.text);
        }
        let (level, text) = tool_effect(rule, path);
        note(level, Safety::Safe, text);
        in_use = rule.tools.iter().find(|t| ctx.facts.running.contains(**t)).map(|t| t.to_string());
    } else {
        note(
            Level::Warn,
            Safety::Caution,
            "These are your own files. Check which copy your apps, shortcuts and links use before removing the other: Jharu cannot see what points at a file.".into(),
        );
    }

    if let Some(repo) = git_repo_of(path) {
        removable = false;
        note(
            Level::Bad,
            Safety::Risky,
            format!("Inside the Git repository {}: a tracked file would show up as deleted. Jharu leaves repositories alone.", repo.display()),
        );
    }
    if let Some(env) = python_env_of(path, tool.map(|(_, root, _)| root)) {
        removable = false;
        note(
            Level::Bad,
            Safety::Risky,
            format!("Part of the Python environment {}: removing a file would break it. Manage environments in the Python tab.", env.display()),
        );
    }
    if let Some(app) = app_folder_of(path) {
        removable = false;
        note(
            Level::Bad,
            Safety::Risky,
            format!("Inside an app or system folder ({app}): the app may stop working. Jharu leaves it alone."),
        );
    }
    if let Some(service) = cloud_service(path) {
        note(
            Level::Bad,
            Safety::Risky,
            format!("Inside a {service} folder: removing it here removes it from your other devices and the cloud copy too. Only {service}'s own bin brings it back."),
        );
    }
    if p.clone && p.paths.len() == 1 {
        note(
            Level::Warn,
            Safety::Safe,
            "This file is an APFS clone: it shares its data with another file that was not searched, so removing it frees almost no space.".into(),
        );
    }
    if p.clone && p.paths.len() > 1 {
        note(
            Level::Ok,
            Safety::Safe,
            format!("{} more paths here are APFS clones sharing this same data; they are removed together.", p.paths.len() - 1),
        );
    }
    if p.links > p.paths.len() as u64 {
        note(
            Level::Warn,
            Safety::Safe,
            format!(
                "The same file is also hard-linked at {} place(s) that were not searched, so removing this path frees no space until those go too.",
                p.links - p.paths.len() as u64
            ),
        );
    }
    if p.paths.len() > 1 && !p.clone {
        note(
            Level::Ok,
            Safety::Safe,
            format!("{} more paths here are hard links of this same file; they are removed together.", p.paths.len() - 1),
        );
    }
    if let Some(tool) = &in_use {
        note(
            Level::Warn,
            Safety::Caution,
            format!("{tool} is running right now and may be using this file. Jharu skips it until you close it."),
        );
    }
    Rating { safety, reasons, removable, in_use, owner }
}

/// Higher is kept. Order of importance: a copy that cannot be removed, a tool
/// that is running now, a copy removal would not free anyway, then where it
/// lives. The oldest copy breaks ties (see [`order_copies`]).
fn keep_score(c: &DupeCopy) -> (i64, String) {
    let mut score = 0i64;
    let mut why: Vec<&str> = vec![];
    if !c.removable {
        score += 1000;
        why.push("Jharu cannot remove it (repository or app)");
    }
    if c.in_use {
        score += 500;
        why.push("its tool is running now");
    }
    if c.reclaim_bytes == 0 {
        score += 300;
        why.push("it is hard-linked elsewhere, so removing it frees nothing");
    }
    let lower = c.path.to_lowercase().replace('\\', "/");
    if !c.in_tool {
        if lower.split('/').any(|s| s == "downloads") {
            score -= 50;
        } else if lower.split('/').any(|s| s == "desktop") {
            score -= 20;
        } else {
            score += 50;
            why.push("not in Downloads or Desktop");
        }
    }
    if copy_like_name(&file_name(Path::new(&c.path))) {
        score -= 10;
    }
    (score, why.join(", "))
}

// ---------------------------------------------------------------- groups

fn display_name(copies: &[Physical]) -> String {
    let blobby = |n: &str| n.starts_with("sha256-") || (n.len() >= 32 && n.chars().all(|c| c.is_ascii_hexdigit()));
    let names: Vec<String> = copies.iter().map(|p| file_name(&p.paths[0])).collect();
    names.iter().find(|n| !blobby(n)).or(names.first()).cloned().unwrap_or_default()
}

fn build_group(digest: &Digest, mut copies: Vec<Physical>, ctx: &Ctx) -> DupeGroup {
    // A stable order before anything else, so the suggestion is repeatable.
    copies.sort_by(|a, b| a.paths[0].cmp(&b.paths[0]));
    let size = copies[0].size;
    let others = copies.len() - 1;
    let name = display_name(&copies);
    let mut kind = Kind::Files;

    let mut built: Vec<DupeCopy> = vec![];
    for p in &copies {
        let rating = rate(p, others, ctx);
        let loc = ctx.locations.find(&p.paths[0]);
        if let Some((_, _, k)) = loc {
            kind = match (kind, k) {
                (Kind::Model, _) | (_, Kind::Model) => Kind::Model,
                _ => Kind::Package,
            };
        } else if kind == Kind::Files && matches!(crate::places::file_kind(&p.paths[0]), crate::places::FileKind::Model) {
            kind = Kind::Model;
        }
        let place = match (&rating.owner, p.paths[0].parent()) {
            (Some(o), _) => o.clone(),
            (None, Some(dir)) => user_place(dir),
            _ => String::new(),
        };
        let reclaim = if p.links > p.paths.len() as u64 || (p.clone && p.paths.len() == 1) { 0 } else { p.size };
        let mut c = DupeCopy {
            path: p.paths[0].to_string_lossy().into_owned(),
            link_paths: p.paths[1..].iter().map(|x| x.to_string_lossy().into_owned()).collect(),
            size_bytes: p.size,
            modified_days: days_since(p.mtime),
            place,
            safety: rating.safety,
            reasons: rating.reasons,
            removable: rating.removable,
            reclaim_bytes: reclaim,
            preselect: false,
            keep: false,
            keep_note: None,
            modified_secs: p.mtime,
            score: 0,
            in_use: rating.in_use.is_some(),
            in_tool: rating.owner.is_some(),
        };
        c.score = keep_score(&c).0;
        built.push(c);
    }

    // Keeper: highest score, then the oldest, then the first path.
    let mut order: Vec<usize> = (0..built.len()).collect();
    order.sort_by(|&a, &b| {
        built[b].score.cmp(&built[a].score)
            .then(built[a].modified_secs.cmp(&built[b].modified_secs))
            .then(built[a].path.cmp(&built[b].path))
    });
    let keeper = order[0];
    let why = {
        let (_, w) = keep_score(&built[keeper]);
        if w.is_empty() { "the oldest copy".to_string() } else { format!("{w}; the oldest wins a tie") }
    };
    built[keeper].keep = true;
    built[keeper].keep_note = Some(format!("Suggested to keep: {why}."));
    for (i, c) in built.iter_mut().enumerate() {
        c.preselect = i != keeper && c.removable && c.safety == Safety::Safe && c.reclaim_bytes > 0;
    }
    built.sort_by(|a, b| b.keep.cmp(&a.keep).then(a.path.cmp(&b.path)));
    let wasted = built.iter().filter(|c| !c.keep && c.removable).map(|c| c.reclaim_bytes).sum();

    DupeGroup { hash: hex(digest), name, kind, size_bytes: size, copies: built, wasted_bytes: wasted }
}

/// A user file's place in words: a well-known folder it sits in, else the
/// name of its own folder.
fn user_place(dir: &Path) -> String {
    for r in default_user_roots() {
        if path_key(dir) == path_key(Path::new(&r.path)) || is_inside(dir, Path::new(&r.path)) {
            return r.label;
        }
    }
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------- scan

pub fn scan_impl(req: &ScanRequest, locations: &Locations, facts: &Facts, job: &Job) -> DupeReport {
    let started = Instant::now();
    let min = req.min_bytes.unwrap_or(DEFAULT_MIN_BYTES).max(FLOOR_BYTES);
    let mut report = DupeReport { min_bytes: min, ..Default::default() };

    let mut stats = Stats::default();
    let mut user: Vec<PathBuf> = vec![];
    for r in &req.roots {
        let p = PathBuf::from(r);
        if p.is_dir() {
            user.push(p);
        } else {
            stats.add_unreadable(&p, "it is not a folder Jharu can open");
        }
    }
    let user = disjoint(user);
    let mut roots = user.clone();
    if req.include_caches {
        roots.extend(locations.folders().into_iter().map(|(p, _)| p));
    }
    let roots = disjoint(roots);
    report.roots = user.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    if req.include_caches {
        let mut labels: Vec<String> = locations.folders().into_iter().map(|(_, l)| l).collect();
        labels.sort();
        labels.dedup();
        report.caches = labels;
    }

    let (by_size, files_seen) = list_files(&roots, min, job, &mut stats);
    report.files_seen = files_seen;
    if job.cancelled() {
        report.cancelled = true;
        report.unreadable_count = stats.unreadable_count;
        report.unreadable = stats.unreadable;
        report.seconds = started.elapsed().as_secs_f64();
        return report;
    }

    // Only sizes seen at least twice can hold a duplicate (or a hard link).
    let cands: Vec<Cand> = by_size.into_values().filter(|v| v.len() >= 2).flatten().collect();
    let physicals = collapse(cands, job, &mut stats);

    for p in physicals.iter().filter(|p| p.paths.len() >= 2) {
        report.hard_links.push(HardLinkSet {
            name: file_name(&p.paths[0]),
            size_bytes: p.size,
            paths: p.paths.iter().map(|x| x.to_string_lossy().into_owned()).collect(),
            shared_bytes: p.size * (p.paths.len() as u64 - 1),
            clones: p.clone,
        });
    }
    report.hard_links.sort_by(|a, b| b.shared_bytes.cmp(&a.shared_bytes).then(a.name.cmp(&b.name)));
    report.hard_link_bytes = report.hard_links.iter().map(|h| h.shared_bytes).sum();

    let equal = find_equal(physicals, job, &mut stats);
    let ctx = Ctx { locations, facts };
    report.groups = equal.into_iter().map(|(d, v)| build_group(&d, v, &ctx)).collect();
    report.groups.sort_by(|a, b| b.wasted_bytes.cmp(&a.wasted_bytes).then(a.name.cmp(&b.name)));
    report.wasted_bytes = report.groups.iter().map(|g| g.wasted_bytes).sum();
    report.safe_bytes = report
        .groups
        .iter()
        .flat_map(|g| g.copies.iter().filter(|c| !c.keep && c.removable && c.safety == Safety::Safe))
        .map(|c| c.reclaim_bytes)
        .sum();

    report.online_only_skipped = stats.online_only;
    report.unreadable_count = stats.unreadable_count;
    report.unreadable = stats.unreadable;
    report.cancelled = job.cancelled();
    report.seconds = started.elapsed().as_secs_f64();
    report
}

// ---------------------------------------------------------------- removal

#[derive(Deserialize, Clone, Debug)]
pub struct RemoveCopy {
    /// The copy's path and any hard-link paths, all removed together.
    pub paths: Vec<String>,
    /// The rating the user saw: "safe", "caution" or "risky".
    pub expected: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RemoveGroup {
    pub hash: String,
    pub size_bytes: u64,
    /// The copy to keep (its path and hard-link paths).
    pub keep: Vec<String>,
    pub remove: Vec<RemoveCopy>,
}

#[derive(Serialize, Default, Debug)]
pub struct RemoveOutcome {
    pub freed_bytes: u64,
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

fn parse_safety(s: &str) -> Safety {
    match s {
        "caution" => Safety::Caution,
        "risky" => Safety::Risky,
        // Anything unrecognised is held to the strictest standard: the copy
        // is removed only if it still rates Safe.
        _ => Safety::Safe,
    }
}

fn safety_word(s: Safety) -> &'static str {
    match s {
        Safety::Safe => "Safe",
        Safety::Caution => "Caution",
        Safety::Risky => "Risky",
    }
}

/// What makes two paths one file: a hard link's file id, or an untouched
/// APFS clone's clone id.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    id: Option<(u64, u64)>,
    clone: Option<(u64, u64)>,
}

impl Identity {
    fn same_data(&self, other: &Identity) -> bool {
        (self.id.is_some() && self.id == other.id) || (self.clone.is_some() && self.clone == other.clone)
    }
}

/// A file right now: a regular file (not a link, not a cloud placeholder) of
/// the expected size whose content hashes to `want`. Returns its identity.
fn verify(path: &Path, size: u64, want: &Digest, job: &Job) -> Result<(Identity, u64, u64), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot be read ({e})"))?;
    if !meta.is_file() {
        return Err("it is not a regular file any more".into());
    }
    let fi = crate::usage::file_info(path, &meta);
    if fi.online_only {
        return Err("it is stored only online, so Jharu will not read it".into());
    }
    if meta.len() != size {
        return Err("its size changed since the scan".into());
    }
    match hash_full(path, size, &|| job.cancelled(), &|_| {}) {
        Ok(Some(d)) if &d == want => Ok((Identity { id: fi.id, clone: fi.clone }, fi.links, mtime_secs(&meta))),
        Ok(Some(_)) => Err("its content changed since the scan".into()),
        Ok(None) => Err("cancelled".into()),
        Err(e) => Err(format!("cannot be read ({e})")),
    }
}

/// Sends one file to the Recycle Bin / Trash. Tests pass their own, so no test
/// ever touches the real bin.
pub type BinFn<'a> = &'a dyn Fn(&Path) -> Result<(), String>;

fn real_bin(p: &Path) -> Result<(), String> {
    trash::delete(p).map_err(|e| e.to_string())
}

/// True when two paths lead to the same place, however they are spelled.
fn same_place(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
        || matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if path_key(&x) == path_key(&y))
}

pub fn remove_impl(groups: &[RemoveGroup], locations: &Locations, facts: &Facts, job: &Job, bin: BinFn) -> RemoveOutcome {
    let mut out = RemoveOutcome::default();
    let ctx = Ctx { locations, facts };
    let total: usize = groups.iter().map(|g| g.remove.len()).sum();
    let mut done = 0usize;

    'groups: for g in groups {
        let Some(want) = parse_hex(&g.hash) else {
            out.errors.push("A request carried an invalid content hash and was ignored.".into());
            done += g.remove.len();
            continue;
        };
        let label = g.keep.first().map(|k| file_name(Path::new(k))).unwrap_or_default();

        // The copy that stays must still be there and still be the same
        // content: otherwise removing the others could lose the last copy.
        let mut keeper: Option<(Identity, PathBuf)> = None;
        let mut keep_err = "no copy to keep was given".to_string();
        for k in &g.keep {
            let kp = PathBuf::from(k);
            match verify(&kp, g.size_bytes, &want, job) {
                Ok((id, _, _)) => {
                    keeper = Some((id, kp));
                    break;
                }
                Err(e) if e == "cancelled" => {
                    out.cancelled = true;
                    break 'groups;
                }
                Err(e) => keep_err = format!("{k}: {e}"),
            }
        }
        let Some((keeper_id, _)) = keeper else {
            out.skipped.push(format!("{label}: nothing was removed because the copy to keep is not safe to rely on ({keep_err})"));
            done += g.remove.len();
            continue;
        };

        for copy in &g.remove {
            if job.cancelled() {
                out.cancelled = true;
                break 'groups;
            }
            let first = copy.paths.first().map(|p| file_name(Path::new(p))).unwrap_or_default();
            job.report("removing", done as u64, total as u64, &first, true);
            done += 1;
            let name = if first.is_empty() { label.clone() } else { first.clone() };

            let present: Vec<PathBuf> = copy.paths.iter().map(PathBuf::from).filter(|p| p.symlink_metadata().is_ok()).collect();
            if present.is_empty() {
                out.skipped.push(format!("{name}: already gone"));
                continue;
            }
            if present.iter().any(|p| g.keep.iter().any(|k| same_place(p, Path::new(k)))) {
                out.skipped.push(format!("{name}: it is the copy being kept"));
                continue;
            }

            // Check every path of the copy right now, against the hash.
            let mut verified: Vec<(PathBuf, Identity)> = vec![];
            let mut links = 1u64;
            let mut mtime = 0u64;
            let mut problem: Option<String> = None;
            for p in &present {
                match verify(p, g.size_bytes, &want, job) {
                    Ok((id, l, m)) => {
                        links = links.max(l);
                        mtime = m;
                        verified.push((p.clone(), id));
                    }
                    Err(e) if e == "cancelled" => {
                        out.cancelled = true;
                        break 'groups;
                    }
                    Err(e) => {
                        problem = Some(format!("{}: {e}", p.display()));
                        break;
                    }
                }
            }
            if let Some(why) = problem {
                out.skipped.push(format!("{name}: changed since the scan, so it was left alone ({why})"));
                continue;
            }
            if let Some((_, id)) = verified.iter().find(|(_, id)| id.same_data(&keeper_id)) {
                let what = if id.id.is_some() { "a hard link" } else { "an APFS clone" };
                out.skipped.push(format!("{name}: it is {what} of the copy being kept, so removing it would free nothing"));
                continue;
            }

            // Rate it again now: location, running tools and locks can change.
            let clone = verified.iter().any(|(_, id)| id.clone.is_some());
            let phys = Physical { size: g.size_bytes, paths: verified.iter().map(|(p, _)| p.clone()).collect(), links, mtime, clone };
            let now = rate(&phys, g.keep.len().max(1), &ctx);
            if !now.removable {
                let why = now.reasons.iter().find(|r| r.level == Level::Bad).map(|r| r.text.clone()).unwrap_or_default();
                out.skipped.push(format!("{name}: not removed. {why}"));
                continue;
            }
            if let Some(tool) = now.in_use {
                out.skipped.push(format!("{name}: {tool} is running and may be using it. Close it and try again."));
                continue;
            }
            let expected = parse_safety(&copy.expected);
            if now.safety > expected {
                out.skipped.push(format!(
                    "{name}: now rated {} instead of {}, so it was left for you to look at again",
                    safety_word(now.safety),
                    safety_word(expected)
                ));
                continue;
            }

            for (p, _) in &verified {
                // Hard-link siblings are freed only by the last one to go.
                let size = crate::usage::reclaimable(p);
                match bin(p) {
                    Ok(()) => {
                        out.freed_bytes += size;
                        out.removed.push(p.to_string_lossy().into_owned());
                    }
                    // Windows reports a file held open by another program
                    // only as "Some operations were aborted".
                    Err(e) if p.exists() && e.contains("aborted") => out.skipped.push(format!(
                        "{name}: it is open in another program, so nothing was moved. Close that program and try again."
                    )),
                    Err(e) => out.errors.push(format!("{}: {e}", p.display())),
                }
            }
        }
    }
    job.report("removing", total as u64, total as u64, "", true);
    out
}

// ---------------------------------------------------------------- commands

#[derive(Serialize, Clone)]
struct ProgressEvent {
    token: String,
    phase: String,
    done: u64,
    total: u64,
    current: String,
}

fn emitter(app: tauri::AppHandle, token: String) -> impl Fn(&str, u64, u64, &str) + Sync + Send {
    move |phase: &str, done: u64, total: u64, current: &str| {
        let _ = app.emit(
            "dupes-progress",
            ProgressEvent { token: token.clone(), phase: phase.into(), done, total, current: current.into() },
        );
    }
}

/// The user content folders searched by default.
#[tauri::command]
pub fn dupes_default_roots() -> Vec<RootInfo> {
    default_user_roots()
}

/// Checks a folder the user typed before it is added to the search.
#[tauri::command]
pub fn dupes_check_folder(path: String) -> Result<RootInfo, String> {
    let p = PathBuf::from(path.trim().trim_matches('"'));
    if !p.is_dir() {
        return Err(format!("{} is not a folder Jharu can open.", p.display()));
    }
    Ok(RootInfo { path: p.to_string_lossy().into_owned(), label: root_label(&p) })
}

/// Asks a running scan or removal to stop at the next safe point.
#[tauri::command]
pub fn dupes_cancel(token: String) {
    if let Ok(mut s) = cancel_set().lock() {
        s.insert(token);
    }
}

#[tauri::command]
pub async fn dupes_scan(app: tauri::AppHandle, request: ScanRequest, token: String) -> Result<DupeReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let emit = emitter(app, token.clone());
        let cancel = || token_cancelled(&token);
        let job = Job::new(&cancel, &emit);
        let locations = Locations::discover();
        let facts = Facts::gather();
        let report = scan_impl(&request, &locations, &facts, &job);
        if let Ok(mut s) = cancel_set().lock() {
            s.remove(&token);
        }
        report
    })
    .await
    .map_err(|e| e.to_string())
}

/// Moves the chosen duplicate copies to the Recycle Bin / Trash after
/// checking each one, and the copy kept, against the hash again.
#[tauri::command]
pub async fn dupes_remove(app: tauri::AppHandle, groups: Vec<RemoveGroup>, token: String) -> Result<RemoveOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let emit = emitter(app, token.clone());
        let cancel = || token_cancelled(&token);
        let job = Job::new(&cancel, &emit);
        let locations = Locations::discover();
        let facts = Facts::gather();
        let outcome = remove_impl(&groups, &locations, &facts, &job, &real_bin);
        if let Ok(mut s) = cancel_set().lock() {
            s.remove(&token);
        }
        outcome
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway tree under the temp folder. The name has spaces on purpose.
    struct Fixture {
        root: PathBuf,
        /// Stands in for the Recycle Bin: tests never touch the real one.
        bin: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!("jharu dupes {tag} {}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let bin = root.with_file_name(format!("jharu dupes {tag} {} bin", std::process::id()));
            let _ = std::fs::remove_dir_all(&bin);
            std::fs::create_dir_all(&bin).unwrap();
            Fixture { root, bin }
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let d = self.root.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        /// Moves a file into the fake bin, like the real bin would.
        fn to_bin(&self, p: &Path) -> Result<(), String> {
            let n = std::fs::read_dir(&self.bin).map(|d| d.count()).unwrap_or(0);
            std::fs::rename(p, self.bin.join(format!("{n} {}", file_name(p)))).map_err(|e| e.to_string())
        }

        fn binned(&self) -> usize {
            std::fs::read_dir(&self.bin).map(|d| d.count()).unwrap_or(0)
        }

        fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.bin);
        }
    }

    fn blob(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn scan(roots: &[&Path], min: u64, locs: &Locations, facts: &Facts) -> DupeReport {
        let cancel = || false;
        let emit = |_: &str, _: u64, _: u64, _: &str| {};
        let job = Job::new(&cancel, &emit);
        let req = ScanRequest {
            roots: roots.iter().map(|r| r.to_string_lossy().into_owned()).collect(),
            min_bytes: Some(min),
            include_caches: true,
        };
        scan_impl(&req, locs, facts, &job)
    }

    fn removal(group: &DupeGroup, remove: &[&DupeCopy]) -> RemoveGroup {
        let keeper = group.copies.iter().find(|c| c.keep).unwrap();
        let mut keep = vec![keeper.path.clone()];
        keep.extend(keeper.link_paths.clone());
        RemoveGroup {
            hash: group.hash.clone(),
            size_bytes: group.size_bytes,
            keep,
            remove: remove
                .iter()
                .map(|c| {
                    let mut paths = vec![c.path.clone()];
                    paths.extend(c.link_paths.clone());
                    RemoveCopy { paths, expected: match c.safety {
                        Safety::Safe => "safe",
                        Safety::Caution => "caution",
                        Safety::Risky => "risky",
                    }.into() }
                })
                .collect(),
        }
    }

    fn run_remove(f: &Fixture, groups: &[RemoveGroup], locs: &Locations, facts: &Facts) -> RemoveOutcome {
        let cancel = || false;
        let emit = |_: &str, _: u64, _: u64, _: &str| {};
        remove_impl(groups, locs, facts, &Job::new(&cancel, &emit), &|p| f.to_bin(p))
    }

    #[test]
    fn finds_identical_files_and_ignores_lookalikes() {
        let f = Fixture::new("find");
        let a = blob(400_000, 1);
        // Same size, same first and last 64 KB, different in the middle: only
        // a full hash can tell this one apart.
        let mut sneaky = a.clone();
        sneaky[200_000] ^= 0xFF;
        f.file("Documents/report final.bin", &a);
        f.file("Documents/old stuff/report copy.bin", &a);
        f.file("Documents/sneaky.bin", &sneaky);
        // Different size, equal prefix.
        f.file("Documents/shorter.bin", &a[..300_000]);
        // Below the minimum size, identical.
        f.file("Documents/tiny 1.txt", b"same");
        f.file("Documents/tiny 2.txt", b"same");

        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert_eq!(r.groups.len(), 1, "{:?}", r.groups);
        let g = &r.groups[0];
        assert_eq!(g.copies.len(), 2);
        assert_eq!(g.size_bytes, 400_000);
        assert_eq!(g.wasted_bytes, 400_000);
        assert_eq!(g.kind, Kind::Files);
        assert!(g.copies.iter().all(|c| !c.path.contains("sneaky") && !c.path.contains("shorter")));
        assert_eq!(g.copies.iter().filter(|c| c.keep).count(), 1);
        assert!(r.hard_links.is_empty());
        assert_eq!(r.files_seen, 4, "files under the minimum are not even listed");
    }

    #[test]
    fn small_files_that_fit_in_the_edges_are_compared_whole() {
        let f = Fixture::new("small");
        let mut a = blob(100_000, 5);
        let b = a.clone();
        a[50_000] ^= 1; // inside the region a two-edge read would skip if it were larger
        f.file("a x.bin", &a);
        f.file("b x.bin", &b);
        let r = scan(&[&f.root], 4096, &Locations::none(), &Facts::default());
        assert!(r.groups.is_empty());
    }

    #[test]
    fn hard_links_are_shared_space_not_duplicates() {
        let f = Fixture::new("links");
        let data = blob(300_000, 2);
        let orig = f.file("Documents/model v1.bin", &data);
        std::fs::hard_link(&orig, f.root.join("Documents").join("model alias.bin")).unwrap();
        // Only hard links of one file: nothing to remove.
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert!(r.groups.is_empty(), "{:?}", r.groups);
        assert_eq!(r.hard_links.len(), 1);
        assert_eq!(r.hard_links[0].paths.len(), 2);
        assert_eq!(r.hard_link_bytes, 300_000);

        // A real copy next to them: one group of two physical files, the
        // linked pair counted as one copy.
        f.file("Backup folder/model copy.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.copies.len(), 2, "hard links are one copy, not two");
        assert_eq!(g.wasted_bytes, 300_000);
        let linked = g.copies.iter().find(|c| !c.link_paths.is_empty()).expect("the linked pair");
        assert_eq!(linked.link_paths.len(), 1);
        assert!(linked.reasons.iter().any(|r| r.text.contains("hard links of this same file")));
    }

    /// `cp -c` and uv's cache make APFS clones: one copy of the data.
    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clones_are_shared_space_not_duplicates() {
        let f = Fixture::new("clones");
        let data = blob(300_000, 21);
        let orig = f.file("Documents/model v1.bin", &data);
        let clone = f.root.join("Documents").join("model clone.bin");
        let ok = std::process::Command::new("/bin/cp").arg("-c").arg(&orig).arg(&clone).status().map(|s| s.success()).unwrap_or(false);
        if !ok || crate::usage::clone_info(&orig).is_none() {
            eprintln!("fixture is not on APFS; skipped");
            return;
        }
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert!(r.groups.is_empty(), "{:?}", r.groups);
        assert_eq!(r.hard_links.len(), 1);
        assert!(r.hard_links[0].clones);

        // A real copy: one group of two physical files.
        let copy = f.file("Backup folder/model copy.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.copies.len(), 2, "the clones are one copy, not two");
        let pair = g.copies.iter().find(|c| !c.link_paths.is_empty()).expect("the clone pair");
        assert!(pair.reasons.iter().any(|r| r.text.contains("APFS clones")));

        // Asked to remove the keeper's clone: it would free nothing.
        let mut req = removal(g, &[]);
        req.keep = vec![orig.to_string_lossy().into_owned()];
        req.remove = vec![RemoveCopy { paths: vec![clone.to_string_lossy().into_owned()], expected: "safe".into() }];
        let out = run_remove(&f, &[req], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty(), "a clone of the keeper is not removed: {out:?}");
        assert!(out.skipped.iter().any(|s| s.contains("APFS clone")), "{out:?}");
        assert!(clone.exists() && copy.exists());
    }

    #[test]
    fn overlapping_folders_do_not_make_a_file_its_own_duplicate() {
        let f = Fixture::new("overlap");
        f.file("Documents/deep/only one.bin", &blob(200_000, 3));
        let docs = f.root.join("Documents");
        let deep = docs.join("deep");
        let r = scan(&[&docs, &deep, &docs], 100_000, &Locations::none(), &Facts::default());
        assert!(r.groups.is_empty());
        assert!(r.hard_links.is_empty());
        assert_eq!(r.roots.len(), 1);
    }

    #[test]
    fn git_repositories_and_node_modules_are_not_searched() {
        let f = Fixture::new("skip");
        let data = blob(200_000, 4);
        f.file("Documents/a.bin", &data);
        f.file("Documents/proj/.git/objects/x.bin", &data);
        f.file("Documents/proj/node_modules/pkg/x.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert!(r.groups.is_empty(), "{:?}", r.groups);
    }

    #[test]
    fn ratings_for_user_files_repos_apps_and_cloud_folders() {
        let f = Fixture::new("rate");
        let data = blob(200_000, 6);
        f.file("Documents/plain.bin", &data);
        f.file("Documents/Downloads/plain copy.bin", &data);
        // A git working tree: marker is a .git entry above the file.
        f.dir("Documents/code repo/.git");
        f.file("Documents/code repo/assets/plain in repo.bin", &data);
        f.file("OneDrive - Contoso/Documents/plain synced.bin", &data);
        f.file("Tools/Thing.app/Contents/plain in app.bin", &data);

        let r = scan(&[&f.root.join("Documents"), &f.root.join("OneDrive - Contoso"), &f.root.join("Tools")], 100_000, &Locations::none(), &Facts::default());
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.copies.len(), 5);
        let by = |needle: &str| g.copies.iter().find(|c| c.path.contains(needle)).unwrap();

        let plain = by("plain.bin");
        assert_eq!(plain.safety, Safety::Caution);
        assert!(plain.removable);
        assert!(plain.reasons.iter().any(|r| r.level == Level::Warn && r.text.contains("your own files")));

        let repo = by("plain in repo");
        assert_eq!(repo.safety, Safety::Risky);
        assert!(!repo.removable, "a copy inside a Git repository is never offered");
        assert!(repo.reasons.iter().any(|r| r.level == Level::Bad && r.text.contains("Git repository")));

        let app = by("plain in app");
        assert_eq!(app.safety, Safety::Risky);
        assert!(!app.removable);

        let cloud = by("plain synced");
        assert_eq!(cloud.safety, Safety::Risky);
        assert!(cloud.removable, "cloud copies can be removed, but only on purpose");
        assert!(!cloud.preselect);
        assert!(cloud.reasons.iter().any(|r| r.text.contains("OneDrive")));

        // Nothing user-owned is ever preselected.
        assert!(g.copies.iter().all(|c| !c.preselect));
        // The keeper is a copy that cannot be removed anyway (repo or app),
        // and the rest of the removable ones are not Downloads-first.
        let keeper = g.copies.iter().find(|c| c.keep).unwrap();
        assert!(!keeper.removable);
        assert!(keeper.keep_note.as_ref().unwrap().contains("cannot remove"));
    }

    #[test]
    fn downloads_copy_is_removed_before_the_documents_copy() {
        let f = Fixture::new("keeper");
        let data = blob(200_000, 7);
        f.file("Documents/thesis.bin", &data);
        f.file("Downloads/thesis (1).bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        let keeper = g.copies.iter().find(|c| c.keep).unwrap();
        assert!(keeper.path.contains("Documents"), "kept {}", keeper.path);
        assert!(g.copies[0].keep, "the suggested copy is listed first");
    }

    #[test]
    fn oldest_copy_is_kept_when_nothing_else_decides() {
        let f = Fixture::new("oldest");
        let data = blob(200_000, 8);
        let newer = f.file("Projects/a/data new.bin", &data);
        let older = f.file("Projects/b/data old.bin", &data);
        let t = SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
        std::fs::File::options().write(true).open(&older).unwrap().set_modified(t).unwrap();
        let _ = newer;
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let keeper = r.groups[0].copies.iter().find(|c| c.keep).unwrap();
        assert!(keeper.path.contains("data old"), "kept {}", keeper.path);
    }

    #[test]
    fn same_model_in_two_tools_is_a_model_group_with_tool_ratings() {
        let f = Fixture::new("models");
        let model = blob(600_000, 9);
        let ollama = f.dir("home/.ollama/models");
        let lm = f.dir("home/.lmstudio/models");
        f.file("home/.ollama/models/blobs/sha256-0123456789abcdef", &model);
        f.file("home/.lmstudio/models/org/repo/llama-3-8b.Q4_K_M.gguf", &model);
        let locs = Locations::with(&[("ollama", &ollama), ("lm-studio", &lm)]);

        // Neither tool running.
        let r = scan(&[], 100_000, &locs, &Facts::default());
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.kind, Kind::Model);
        assert_eq!(g.name, "llama-3-8b.Q4_K_M.gguf", "the readable name, not Ollama's hash");
        assert_eq!(r.caches, vec!["LM Studio models".to_string(), "Ollama models".to_string()]);
        let o = g.copies.iter().find(|c| c.place == "Ollama models").unwrap();
        assert_eq!(o.safety, Safety::Caution, "Ollama does not re-create it by itself");
        assert!(o.reasons.iter().any(|r| r.text.contains("ollama pull")));
        assert!(!o.preselect);
        // Ties between two idle tools go to the oldest, then the path.
        assert_eq!(g.copies.iter().filter(|c| c.keep).count(), 1);

        // Ollama running: its copy is the one the user actually runs.
        let facts = Facts { running: ["ollama".to_string()].into_iter().collect(), ..Default::default() };
        let r = scan(&[], 100_000, &locs, &facts);
        let g = &r.groups[0];
        let keeper = g.copies.iter().find(|c| c.keep).unwrap();
        assert_eq!(keeper.place, "Ollama models");
        assert!(keeper.keep_note.as_ref().unwrap().contains("tool is running"));
        let lmc = g.copies.iter().find(|c| c.place == "LM Studio models").unwrap();
        assert!(lmc.removable);
    }

    #[test]
    fn package_caches_that_rebuild_themselves_are_safe_to_dedupe() {
        let f = Fixture::new("pkgs");
        let wheel = blob(300_000, 10);
        let pip = f.dir("cache/pip");
        let uv = f.dir("cache/uv");
        f.file("cache/pip/wheels/torch-2.4-cp312-win_amd64.whl", &wheel);
        f.file("cache/uv/wheels-v3/pypi/torch/torch-2.4-cp312-win_amd64.whl", &wheel);
        let locs = Locations::with(&[("pip", &pip), ("uv", &uv)]);
        let r = scan(&[], 100_000, &locs, &Facts::default());
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.kind, Kind::Package);
        let removable: Vec<&DupeCopy> = g.copies.iter().filter(|c| !c.keep).collect();
        assert_eq!(removable.len(), 1);
        assert_eq!(removable[0].safety, Safety::Safe);
        assert!(removable[0].preselect);
        assert_eq!(r.safe_bytes, 300_000);
        assert!(removable[0].reasons.iter().all(|r| r.level != Level::Bad));
    }

    #[test]
    fn hf_blob_warns_about_snapshot_links() {
        let f = Fixture::new("hf");
        let data = blob(300_000, 11);
        let hf = f.dir("home/.cache/huggingface");
        let lm = f.dir("home/.lmstudio/models");
        f.file("home/.cache/huggingface/hub/models--org--repo/blobs/abc123", &data);
        f.file("home/.lmstudio/models/org/repo/file.safetensors", &data);
        let locs = Locations::with(&[("hf-hub", &hf), ("lm-studio", &lm)]);
        let r = scan(&[], 100_000, &locs, &Facts::default());
        let hfc = r.groups[0].copies.iter().find(|c| c.place == "Hugging Face cache").unwrap();
        assert!(hfc.reasons.iter().any(|r| r.text.contains("snapshot folders link")));
    }

    #[test]
    fn cancel_ends_the_scan_without_results() {
        let f = Fixture::new("cancel");
        let data = blob(200_000, 12);
        f.file("a.bin", &data);
        f.file("b.bin", &data);
        let cancel = || true;
        let emit = |_: &str, _: u64, _: u64, _: &str| {};
        let req = ScanRequest { roots: vec![f.root.to_string_lossy().into_owned()], min_bytes: Some(100_000), include_caches: false };
        let r = scan_impl(&req, &Locations::none(), &Facts::default(), &Job::new(&cancel, &emit));
        assert!(r.cancelled);
        assert!(r.groups.is_empty());
    }

    #[test]
    fn removal_keeps_one_copy_and_moves_the_other_to_the_bin() {
        let f = Fixture::new("remove");
        let data = blob(250_000, 13);
        let keep = f.file("Documents/keep me.bin", &data);
        let gone = f.file("Downloads/keep me (1).bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        let victim = g.copies.iter().find(|c| !c.keep).unwrap();
        let out = run_remove(&f, &[removal(g, &[victim])], &Locations::none(), &Facts::default());
        assert_eq!(out.removed.len(), 1, "{out:?}");
        assert_eq!(out.freed_bytes, 250_000);
        assert!(out.skipped.is_empty() && out.errors.is_empty(), "{out:?}");
        assert!(keep.exists());
        assert!(!gone.exists());
        // It went to the bin (the fixture's own), not into oblivion.
        assert_eq!(f.binned(), 1);
    }

    #[test]
    fn removal_rechecks_hash_of_both_the_copy_and_the_keeper() {
        let f = Fixture::new("recheck");
        let data = blob(250_000, 14);
        let keep = f.file("Documents/original.bin", &data);
        let copy = f.file("Backup/original.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        let keeper_path = g.copies.iter().find(|c| c.keep).unwrap().path.clone();
        let victim_path = g.copies.iter().find(|c| !c.keep).unwrap().path.clone();
        let victim = g.copies.iter().find(|c| !c.keep).unwrap();
        let req = removal(g, &[victim]);

        // The copy changes after the scan (same length, new content).
        let mut edited = data.clone();
        edited[100_000] ^= 0xFF;
        std::fs::write(&victim_path, &edited).unwrap();
        let out = run_remove(&f, &[req.clone()], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty());
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].contains("changed since the scan"), "{:?}", out.skipped);
        assert!(Path::new(&victim_path).exists());

        // The keeper changes: nothing at all is removed.
        std::fs::write(&victim_path, &data).unwrap();
        std::fs::write(&keeper_path, &edited).unwrap();
        let out = run_remove(&f, &[req.clone()], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty());
        assert!(out.skipped[0].contains("copy to keep"), "{:?}", out.skipped);
        assert!(Path::new(&victim_path).exists());

        // The keeper is gone: same.
        std::fs::remove_file(&keeper_path).unwrap();
        let out = run_remove(&f, &[req], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty());
        assert!(Path::new(&victim_path).exists());
        let _ = (keep, copy);
    }

    #[test]
    fn a_hard_link_of_the_keeper_is_not_removed() {
        let f = Fixture::new("linkkeep");
        let data = blob(250_000, 15);
        let a = f.file("Documents/one.bin", &data);
        let link = f.root.join("Documents").join("one link.bin");
        std::fs::hard_link(&a, &link).unwrap();
        f.file("Other/one copy.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        // Ask to remove the keeper's own link path as if it were a copy.
        let keeper = g.copies.iter().find(|c| c.keep).unwrap();
        let mut req = removal(g, &[]);
        let linked = g.copies.iter().find(|c| !c.link_paths.is_empty());
        if let Some(l) = linked {
            // Keeper is the linked pair or the plain copy; build the hostile
            // request by hand: remove a path that is a link of what is kept.
            let other = if keeper.path == l.path { l.link_paths[0].clone() } else { keeper.path.clone() };
            req.remove.push(RemoveCopy { paths: vec![other], expected: "caution".into() });
        }
        let out = run_remove(&f, &[req], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty(), "{out:?}");
        assert!(a.exists() && link.exists());
    }

    #[test]
    fn removal_skips_a_copy_that_is_now_inside_a_repository_or_tool_in_use() {
        let f = Fixture::new("skiprm");
        let data = blob(250_000, 16);
        let ollama = f.dir("home/.ollama/models");
        let lm = f.dir("home/.lmstudio/models");
        let o = f.file("home/.ollama/models/blobs/sha256-feedfacefeedface", &data);
        let l = f.file("home/.lmstudio/models/m/model.gguf", &data);
        let locs = Locations::with(&[("ollama", &ollama), ("lm-studio", &lm)]);
        let r = scan(&[], 100_000, &locs, &Facts::default());
        let g = &r.groups[0];
        // Suppose the user keeps LM Studio's copy and removes Ollama's.
        let mut req = removal(g, &[]);
        req.keep = vec![l.to_string_lossy().into_owned()];
        req.remove = vec![RemoveCopy { paths: vec![o.to_string_lossy().into_owned()], expected: "caution".into() }];

        // Ollama starts running after the scan.
        let running = Facts { running: ["ollama".to_string()].into_iter().collect(), ..Default::default() };
        let out = run_remove(&f, &[req.clone()], &locs, &running);
        assert!(out.removed.is_empty());
        assert!(out.skipped[0].contains("ollama is running"), "{:?}", out.skipped);
        assert!(o.exists());

        // A copy that became Risky since the scan is not removed on the old rating.
        let repo_copy = f.file("Code/.git/HEAD", b"ref: refs/heads/main\n");
        let _ = repo_copy;
        let inrepo = f.file("Code/data.bin", &data);
        let out = run_remove(
            &f,
            &[RemoveGroup {
                hash: g.hash.clone(),
                size_bytes: g.size_bytes,
                keep: vec![l.to_string_lossy().into_owned()],
                remove: vec![RemoveCopy { paths: vec![inrepo.to_string_lossy().into_owned()], expected: "caution".into() }],
            }],
            &locs,
            &Facts::default(),
        );
        assert!(out.removed.is_empty());
        assert!(out.skipped[0].contains("Git repository"), "{:?}", out.skipped);
        assert!(inrepo.exists());
    }

    #[test]
    fn removal_can_be_cancelled_before_it_starts() {
        let f = Fixture::new("cancelrm");
        let data = blob(250_000, 17);
        f.file("Documents/a.bin", &data);
        let b = f.file("Backup/a.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        let victim = g.copies.iter().find(|c| !c.keep).unwrap();
        let req = removal(g, &[victim]);
        let cancel = || true;
        let emit = |_: &str, _: u64, _: u64, _: &str| {};
        let out = remove_impl(&[req], &Locations::none(), &Facts::default(), &Job::new(&cancel, &emit), &|p| f.to_bin(p));
        assert!(out.cancelled);
        assert!(out.removed.is_empty());
        assert!(f.root.join("Documents").join("a.bin").exists() && b.exists());
    }

    #[test]
    fn python_environments_are_not_searched_and_never_removed_from() {
        let f = Fixture::new("env");
        let data = blob(200_000, 18);
        f.file("Documents/lib.dll", &data);
        f.file("Documents/proj/.venv/pyvenv.cfg", b"home = x\n");
        let inside = f.file("Documents/proj/.venv/Lib/site-packages/pkg/lib.dll", &data);
        f.file("Documents/envs/conda one/conda-meta/history", b"");
        f.file("Documents/envs/conda one/Library/bin/lib.dll", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        assert!(r.groups.is_empty(), "{:?}", r.groups);

        // If one is asked for anyway, it is refused.
        let outside = f.root.join("Documents").join("lib.dll");
        let hash = hex(&hash_full(&outside, 200_000, &|| false, &|_| {}).unwrap().unwrap());
        let req = RemoveGroup {
            hash,
            size_bytes: 200_000,
            keep: vec![outside.to_string_lossy().into_owned()],
            remove: vec![RemoveCopy { paths: vec![inside.to_string_lossy().into_owned()], expected: "caution".into() }],
        };
        let out = run_remove(&f, &[req], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty());
        assert!(out.skipped[0].contains("Python environment"), "{:?}", out.skipped);
        assert!(inside.exists());
    }

    #[test]
    fn an_unknown_expected_rating_is_held_to_safe() {
        let f = Fixture::new("expect");
        let data = blob(250_000, 20);
        let keep = f.file("Documents/doc.bin", &data);
        let other = f.file("Backup/doc.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        let mut req = removal(g, &[]);
        req.keep = vec![keep.to_string_lossy().into_owned()];
        // Your own file rates Caution now, so a request that does not say what
        // the user saw cannot remove it.
        req.remove = vec![RemoveCopy { paths: vec![other.to_string_lossy().into_owned()], expected: "".into() }];
        let out = run_remove(&f, &[req], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty(), "{out:?}");
        assert!(out.skipped[0].contains("now rated Caution"), "{:?}", out.skipped);
        assert!(other.exists() && f.binned() == 0);
    }

    #[test]
    fn the_copy_to_keep_can_never_be_removed_under_another_spelling() {
        let f = Fixture::new("samepath");
        let data = blob(250_000, 21);
        let keep = f.file("Documents/doc.bin", &data);
        f.file("Backup/doc.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let g = &r.groups[0];
        let mut req = removal(g, &[]);
        req.keep = vec![keep.to_string_lossy().into_owned()];
        let dotted = f.root.join("Documents").join("..").join("Documents").join("doc.bin");
        req.remove = vec![RemoveCopy { paths: vec![dotted.to_string_lossy().into_owned()], expected: "caution".into() }];
        let out = run_remove(&f, &[req], &Locations::none(), &Facts::default());
        assert!(out.removed.is_empty(), "{out:?}");
        assert!(keep.exists() && f.binned() == 0);
    }

    #[test]
    fn what_could_not_be_searched_is_reported_not_hidden() {
        let f = Fixture::new("missing");
        f.file("Documents/a.bin", &blob(200_000, 22));
        let gone = f.root.join("not here");
        let r = scan(&[&f.root, &gone], 100_000, &Locations::none(), &Facts::default());
        assert_eq!(r.unreadable_count, 1);
        assert!(r.unreadable[0].contains("not here"), "{:?}", r.unreadable);
    }

    #[test]
    fn conda_package_cache_next_to_the_base_environment_is_still_a_cache() {
        let f = Fixture::new("conda");
        let data = blob(300_000, 23);
        let conda = f.dir("home/miniconda3/pkgs");
        let uv = f.dir("cache/uv");
        f.file("home/miniconda3/conda-meta/history", b"");
        f.file("home/miniconda3/pkgs/numpy-1.26/lib/libnum.dll", &data);
        f.file("cache/uv/wheels/libnum.dll", &data);
        let locs = Locations::with(&[("conda-pkgs", &conda), ("uv", &uv)]);
        let r = scan(&[], 100_000, &locs, &Facts::default());
        assert_eq!(r.groups.len(), 1, "{:?}", r.groups);
        let c = r.groups[0].copies.iter().find(|c| c.place.to_lowercase().contains("conda")).unwrap();
        assert!(c.removable || c.keep, "the package cache is not part of the environment");
        assert!(c.reasons.iter().all(|r| !r.text.contains("Python environment")));
        // An environment itself is still protected.
        assert!(python_env_of(&f.root.join("home/miniconda3/lib/b.dll"), None).is_some());
        assert!(python_env_of(&f.root.join("home/miniconda3/pkgs/a/b.dll"), Some(&conda)).is_none());
    }

    #[test]
    fn second_copies_by_name_lose_ties() {
        assert!(copy_like_name("report (1).pdf"));
        assert!(copy_like_name("report - Copy.pdf"));
        assert!(copy_like_name("Copy of report.pdf"));
        assert!(!copy_like_name("report (final).pdf"));
        assert!(!copy_like_name("report.pdf"));
        let f = Fixture::new("copyname");
        let data = blob(200_000, 19);
        f.file("Documents/paper (1).bin", &data);
        f.file("Documents/paper.bin", &data);
        let r = scan(&[&f.root], 100_000, &Locations::none(), &Facts::default());
        let keeper = r.groups[0].copies.iter().find(|c| c.keep).unwrap();
        assert!(keeper.path.ends_with("paper.bin"), "kept {}", keeper.path);
    }

    #[test]
    fn cloud_and_app_detection() {
        assert_eq!(cloud_service(Path::new("C:/Users/x/OneDrive - Corp/Documents/a.bin")), Some("OneDrive"));
        assert_eq!(cloud_service(Path::new("/Users/x/Library/Mobile Documents/com~apple~CloudDocs/a")), Some("iCloud Drive"));
        assert_eq!(cloud_service(Path::new("/home/x/Dropbox/a")), Some("Dropbox"));
        assert_eq!(cloud_service(Path::new("/home/x/Documents/a")), None);
        assert!(app_folder_of(Path::new("/Applications/Foo.app/Contents/a")).is_some());
        assert!(app_folder_of(Path::new("/home/x/Pictures/Photos Library.photoslibrary/a")).is_some());
        assert!(app_folder_of(Path::new("/home/x/Documents/a")).is_none());
    }

    #[test]
    fn digests_round_trip_and_blob_names_are_not_used_as_titles() {
        let d = *blake3::hash(b"x").as_bytes();
        assert_eq!(parse_hex(&hex(&d)), Some(d));
        assert_eq!(parse_hex("nonsense"), None);
        let p = |n: &str| Physical { size: 1, paths: vec![PathBuf::from(n)], links: 1, mtime: 0, clone: false };
        assert_eq!(display_name(&[p("/x/sha256-abc"), p("/y/model.gguf")]), "model.gguf");
        assert_eq!(display_name(&[p("/x/sha256-abc")]), "sha256-abc");
    }

    /// Read-only scan of this machine's default folders and tool caches:
    /// `cargo test --lib smoke_dupes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_dupes() {
        let roots: Vec<String> = default_user_roots().into_iter().map(|r| r.path).collect();
        let req = ScanRequest { roots, min_bytes: None, include_caches: true };
        let cancel = || false;
        let emit = |phase: &str, done: u64, total: u64, cur: &str| {
            if phase != "listing" {
                eprintln!("  {phase} {done}/{total} {cur}");
            }
        };
        let r = scan_impl(&req, &Locations::discover(), &Facts::gather(), &Job::new(&cancel, &emit));
        println!(
            "scanned {} files in {:.1}s: {} groups, {:.2} GB wasted ({:.2} GB safe), {:.2} GB already shared by hard links, {} cloud placeholders skipped, {} unreadable",
            r.files_seen, r.seconds, r.groups.len(), r.wasted_bytes as f64 / 1e9, r.safe_bytes as f64 / 1e9,
            r.hard_link_bytes as f64 / 1e9, r.online_only_skipped, r.unreadable_count
        );
        println!("roots: {:?}\ncaches: {:?}", r.roots, r.caches);
        for g in r.groups.iter().take(25) {
            println!("{:>9.1} MB wasted  {:?}  {}  x{}", g.wasted_bytes as f64 / 1e6, g.kind, g.name, g.copies.len());
            for c in &g.copies {
                println!("      {} {:?} {}  [{}]", if c.keep { "KEEP" } else { "    " }, c.safety, c.path, c.place);
            }
        }
        for u in &r.unreadable {
            println!("unreadable: {u}");
        }
    }
}
