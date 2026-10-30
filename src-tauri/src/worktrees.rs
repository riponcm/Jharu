use crate::rules::Safety;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::Emitter;

/// How far below the home directory to look for repositories. Worktrees
/// themselves can sit deeper (`<repo>/.claude/worktrees/<name>`); those are
/// found through `git worktree list` once their repository is known.
const HOME_DEPTH: usize = 6;
/// Other drive roots (Windows) are searched a little less deeply.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const DRIVE_DEPTH: usize = 5;
/// How many file names or commit subjects to quote in an explanation.
const LIST_LIMIT: usize = 6;

#[cfg(target_os = "windows")]
const BIN: &str = "Recycle Bin";
#[cfg(not(target_os = "windows"))]
const BIN: &str = "Trash";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every commit is on the default branch and nothing is uncommitted.
    Safe,
    /// Commits differ by ID but `git cherry` finds the same changes upstream.
    ProbablyMerged,
    /// The folder is already gone; only git's record of it is left.
    Record,
    /// Holds work that exists nowhere else, or could not be checked.
    Keep,
}

impl Verdict {
    fn removable(self) -> bool {
        self != Verdict::Keep
    }
}

#[derive(Serialize, Clone)]
pub struct WorktreeItem {
    pub path: String,
    pub name: String,
    /// The main repository's `.git` directory; removal requests carry it back.
    pub common_dir: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub size_bytes: u64,
    pub modified_days: Option<u64>,
    pub last_commit_days: Option<u64>,
    pub last_commit_subject: Option<String>,
    /// Commits reachable from this worktree but not from the default branch.
    pub ahead: Option<u32>,
    pub uncommitted: Vec<String>,
    pub uncommitted_count: usize,
    /// Files that show as modified only because of CRLF/LF differences.
    pub line_ending_files: usize,
    pub verdict: Verdict,
    pub safety: Safety,
    /// The tool that created this worktree for one of its sessions.
    pub owner: Option<String>,
    pub preselect: bool,
    pub summary: String,
    pub explanation: String,
    pub reasons: Vec<String>,
}

#[derive(Serialize)]
pub struct WorktreeRepo {
    pub repo: String,
    pub common_dir: String,
    pub base: Option<String>,
    pub total_bytes: u64,
    pub items: Vec<WorktreeItem>,
}

#[derive(Serialize)]
pub struct WorktreeReport {
    pub git_available: bool,
    pub total_bytes: u64,
    pub safe_bytes: u64,
    pub repos: Vec<WorktreeRepo>,
}

// ---------------------------------------------------------------- git

fn git_binary() -> Option<&'static PathBuf> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = vec![];
        #[cfg(target_os = "macos")]
        {
            // A Finder-launched app gets a minimal PATH, and /usr/bin/git is a
            // shim that pops an "install developer tools" dialog when the tools
            // are missing. Look for real binaries instead.
            for p in [
                "/opt/homebrew/bin/git",
                "/usr/local/bin/git",
                "/opt/local/bin/git",
                "/Library/Developer/CommandLineTools/usr/bin/git",
                "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
            ] {
                candidates.push(PathBuf::from(p));
            }
        }
        #[cfg(not(target_os = "macos"))]
        candidates.push(PathBuf::from("git"));
        #[cfg(target_os = "windows")]
        {
            candidates.push(PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"));
            if let Some(local) = dirs::data_local_dir() {
                candidates.push(local.join(r"Programs\Git\cmd\git.exe"));
            }
        }
        candidates.into_iter().find(|c| {
            git_command(c)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
    })
    .as_ref()
}

fn git_command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    // Scanning must not write to the repository: without this, `git status`
    // refreshes the index, which lives inside the main repo's .git directory.
    cmd.env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let bin = git_binary().ok_or("git was not found")?;
    let out = git_command(bin)
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Whether a repository holds work that exists nowhere else. Deep clean and
/// App sandboxes ask this of every repository inside a folder, so that a
/// clean clone of a public project is not rated like a folder of unpushed
/// work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RepoCheck {
    /// git answered every question below.
    pub readable: bool,
    /// Changed or untracked files; ignored files and CRLF/LF-only
    /// differences do not count.
    pub uncommitted: usize,
    /// Commits on local branches that no remote-tracking branch has.
    pub unpushed: usize,
    pub stashed: bool,
    pub has_remote: bool,
}

impl RepoCheck {
    /// Nothing in it exists only on this machine.
    pub fn backed_up(&self) -> bool {
        self.readable && self.uncommitted == 0 && self.unpushed == 0 && !self.stashed && self.has_remote
    }
}

/// Files with real changes against HEAD, line-ending differences left out.
/// `--numstat`, not `--name-only`: Apple's git (2.39) still names a file
/// whose only change is CRLF line endings under `--ignore-cr-at-eol`, while
/// `--numstat` leaves it out. Binary changes show as "-\t-" and are kept.
fn real_changes(dir: &Path) -> Result<Vec<String>, String> {
    let out = git(dir, &["diff", "HEAD", "--ignore-cr-at-eol", "--numstat"])?;
    Ok(out.lines().filter_map(|l| l.splitn(3, '\t').nth(2)).map(String::from).collect())
}

pub(crate) fn check_repo(dir: &Path) -> RepoCheck {
    let mut c = RepoCheck::default();
    let Ok(status) = git(dir, &["status", "--porcelain=v1", "--untracked-files=normal"]) else { return c };
    let untracked = status.lines().filter(|l| l.starts_with("??")).count();
    let changed = status.lines().filter(|l| l.len() > 3 && !l.starts_with("??")).count();
    let real_changed = if changed == 0 {
        0
    } else {
        real_changes(dir).map(|v| v.len()).unwrap_or(changed)
    };
    c.uncommitted = untracked + real_changed;
    let Ok(remotes) = git(dir, &["remote"]) else { return c };
    c.has_remote = !remotes.trim().is_empty();
    // A repository without a single commit has nothing to push.
    c.unpushed = match git(dir, &["rev-list", "--count", "--branches", "--not", "--remotes"]) {
        Ok(n) => n.trim().parse().unwrap_or(0),
        Err(_) if git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() => 0,
        Err(_) => return c,
    };
    c.stashed = git(dir, &["rev-parse", "--verify", "--quiet", "refs/stash"]).is_ok();
    c.readable = true;
    c
}

// ---------------------------------------------------------------- paths

/// git prints forward slashes on every platform.
fn os_path(s: &str) -> PathBuf {
    #[cfg(target_os = "windows")]
    return PathBuf::from(s.replace('/', "\\"));
    #[cfg(not(target_os = "windows"))]
    return PathBuf::from(s);
}

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

fn days_since(t: SystemTime) -> Option<u64> {
    SystemTime::now().duration_since(t).ok().map(|d| d.as_secs() / 86_400)
}

/// Worktrees that a coding tool creates for its sessions live under a hidden
/// `.<tool>/worktrees` folder.
fn owner_of(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    parts.windows(2).find_map(|w| {
        if w[1] != "worktrees" || !w[0].starts_with('.') {
            return None;
        }
        Some(match w[0].as_str() {
            ".claude" => "Claude Code".to_string(),
            ".codex" => "Codex".to_string(),
            ".cursor" => "Cursor".to_string(),
            other => other.trim_start_matches('.').to_string(),
        })
    })
}

// ---------------------------------------------------------------- discovery

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".Trash" | "Library" | "AppData" | ".cache" | ".npm" | ".cargo"
            | ".rustup" | ".gradle" | ".m2" | ".nuget" | ".vscode" | ".local" | "Pictures"
            | "Music" | "Movies" | "Videos" | "Photos Library.photoslibrary" | "$Recycle.Bin"
            | "System Volume Information"
    )
}

/// What the walk learned: each repository that has worktrees, by its `.git`
/// directory, and folders whose `.git` file points at a repository that no
/// longer exists.
#[derive(Default)]
struct Discovery {
    repos: BTreeMap<String, PathBuf>,
    orphans: Vec<(PathBuf, PathBuf)>,
}

/// Reads `gitdir: <path>` from a worktree's `.git` file.
fn read_gitdir(dot_git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let raw = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let p = os_path(raw);
    Some(if p.is_relative() { dot_git.parent()?.join(p) } else { p })
}

fn note_git_entry(found: &mut Discovery, dot_git: &Path, is_dir: bool) {
    if is_dir {
        // A repository's own .git directory. Only repos with linked worktrees
        // (or stale records of them) are interesting.
        if dot_git.join("worktrees").is_dir() {
            found.repos.insert(path_key(dot_git), dot_git.to_path_buf());
        }
        return;
    }
    let Some(gitdir) = read_gitdir(dot_git) else { return };
    // Submodules also use a .git file, pointing into .git/modules — skip them.
    let is_worktree = gitdir
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "worktrees");
    if !is_worktree {
        return;
    }
    let Some(common) = gitdir.parent().and_then(|p| p.parent()) else { return };
    if common.is_dir() {
        found.repos.insert(path_key(common), common.to_path_buf());
    } else if let Some(folder) = dot_git.parent() {
        found.orphans.push((folder.to_path_buf(), gitdir));
    }
}

fn discover(roots: &[(PathBuf, usize, Vec<String>)], found: &mut Discovery) {
    for (root, depth, top_skip) in roots {
        if !root.is_dir() {
            continue;
        }
        let top_skip = top_skip.clone();
        let walker = crate::walk::walk(root)
            .max_depth(*depth)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(move |depth, _, _, children| {
                let is_git = |c: &jwalk::Result<jwalk::DirEntry<((), ())>>| {
                    c.as_ref().map(|e| e.file_name == ".git").unwrap_or(false)
                };
                if children.iter().any(is_git) {
                    // A repository or worktree root: record its .git and stop.
                    // Worktrees nested inside it are reported by git itself.
                    children.retain(is_git);
                    for c in children.iter_mut().flatten() {
                        c.read_children_path = None;
                    }
                    return;
                }
                children.retain(|c| {
                    let Ok(e) = c else { return true };
                    if !e.file_type().is_dir() {
                        return false;
                    }
                    let name = e.file_name.to_string_lossy();
                    !(skip_dir(&name) || (depth == Some(0) && top_skip.iter().any(|s| s == &*name)))
                });
            });
        for entry in walker.into_iter().flatten() {
            if entry.file_name == ".git" {
                note_git_entry(found, &entry.path(), entry.file_type().is_dir());
            }
        }
    }
}

fn default_roots() -> Vec<(PathBuf, usize, Vec<String>)> {
    let mut roots = vec![];
    let home = dirs::home_dir();
    if let Some(h) = &home {
        roots.push((h.clone(), HOME_DEPTH, vec![]));
    }
    #[cfg(target_os = "windows")]
    {
        // Developers on Windows often keep code outside their profile, on
        // C:\dev, D:\repos and the like, so every fixed drive is searched.
        let skip: Vec<String> = ["Windows", "Program Files", "Program Files (x86)", "ProgramData", "Users", "Recovery", "$WinREAgent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        for d in disks.list() {
            if d.is_removable() {
                continue;
            }
            let mount = d.mount_point().to_path_buf();
            if home.as_ref().is_some_and(|h| path_key(h) == path_key(&mount)) {
                continue;
            }
            roots.push((mount, DRIVE_DEPTH, skip.clone()));
        }
    }
    roots
}

// ---------------------------------------------------------------- git state

#[derive(Debug, Default, Clone)]
struct Listed {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    bare: bool,
    locked: Option<String>,
    prunable: bool,
}

fn list_worktrees(common: &Path) -> Result<Vec<Listed>, String> {
    let out = git(common, &["worktree", "list", "--porcelain"])?;
    let mut all = vec![];
    let mut cur: Option<Listed> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(done) = cur.take() {
                all.push(done);
            }
            cur = Some(Listed { path: os_path(p), ..Default::default() });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        if let Some(h) = line.strip_prefix("HEAD ") {
            w.head = Some(h.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
        } else if line == "bare" {
            w.bare = true;
        } else if line == "locked" || line.starts_with("locked ") {
            w.locked = Some(line.trim_start_matches("locked").trim().to_string());
        } else if line == "prunable" || line.starts_with("prunable ") {
            w.prunable = true;
        }
    }
    all.extend(cur);
    Ok(all)
}

fn ref_exists(common: &Path, r: &str) -> bool {
    git(common, &["rev-parse", "--verify", "--quiet", r]).is_ok()
}

/// The default branch, by display name, plus every ref that counts as "on"
/// it: the remote's copy and the local one, whichever exist.
fn default_branch(common: &Path) -> Option<(String, Vec<String>)> {
    let name = git(common, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .ok()
        .map(|s| s.trim().trim_start_matches("refs/remotes/origin/").to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|b| ref_exists(common, &format!("refs/heads/{b}")))
                .map(String::from)
        })?;
    let refs: Vec<String> = [format!("refs/remotes/origin/{name}"), format!("refs/heads/{name}")]
        .into_iter()
        .filter(|r| ref_exists(common, r))
        .collect();
    if refs.is_empty() {
        None
    } else {
        Some((name, refs))
    }
}

fn measure(path: &Path) -> (u64, Option<SystemTime>) {
    let mut size = 0u64;
    let mut newest: Option<SystemTime> = None;
    for entry in crate::walk::walk(path).skip_hidden(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            size += meta.len();
            if let Ok(m) = meta.modified() {
                if newest.map_or(true, |n| m > n) {
                    newest = Some(m);
                }
            }
        }
    }
    (size, newest)
}

fn quoted_list(items: &[String], total: usize) -> String {
    let mut s = items.iter().take(LIST_LIMIT).cloned().collect::<Vec<_>>().join(", ");
    if total > LIST_LIMIT {
        s.push_str(&format!(" and {} more", total - LIST_LIMIT));
    }
    s
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn recreate_hint(path: &Path, branch: &Option<String>, head: &Option<String>) -> String {
    let p = path.to_string_lossy();
    match (branch, head) {
        (Some(b), _) => format!(
            "The branch `{b}` stays in git, so `git worktree add \"{p}\" {b}` brings the folder back."
        ),
        (None, Some(h)) => format!(
            "It has no branch (detached HEAD) and its commit is already on the default branch; `git worktree add --detach \"{p}\" {}` brings it back.",
            &h[..h.len().min(10)]
        ),
        _ => String::new(),
    }
}

struct RepoCtx<'a> {
    common: &'a Path,
    base: Option<&'a (String, Vec<String>)>,
    /// Every worktree path git knows for this repo, to spot nested ones.
    all_paths: &'a [PathBuf],
}

/// Inspects one linked worktree and decides whether it is safe to remove.
fn inspect(ctx: &RepoCtx, w: &Listed) -> WorktreeItem {
    let path = &w.path;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let owner = owner_of(path);
    let mut item = WorktreeItem {
        path: path.to_string_lossy().into_owned(),
        name,
        common_dir: ctx.common.to_string_lossy().into_owned(),
        branch: w.branch.clone(),
        head: w.head.as_ref().map(|h| h[..h.len().min(10)].to_string()),
        size_bytes: 0,
        modified_days: None,
        last_commit_days: None,
        last_commit_subject: None,
        ahead: None,
        uncommitted: vec![],
        uncommitted_count: 0,
        line_ending_files: 0,
        verdict: Verdict::Keep,
        safety: Safety::Risky,
        owner: owner.clone(),
        preselect: false,
        summary: String::new(),
        explanation: String::new(),
        reasons: vec![],
    };

    if w.prunable || !path.exists() {
        if let Some(reason) = &w.locked {
            item.summary = "Stale record, locked".into();
            item.explanation = "The folder is gone, but the worktree was locked with `git worktree lock`, so git keeps its record on purpose.".into();
            if !reason.is_empty() {
                item.reasons.push(format!("Lock reason: {reason}"));
            }
            return item;
        }
        item.verdict = Verdict::Record;
        item.safety = Safety::Safe;
        item.preselect = true;
        item.summary = "Stale record".into();
        item.explanation = "The folder is already gone, but git still lists it as a worktree. Clearing the record runs `git worktree prune`; it frees no space and touches no files.".into();
        return item;
    }

    let (size, newest) = measure(path);
    item.size_bytes = size;
    item.modified_days = newest.and_then(days_since);

    if let Some(sha) = &w.head {
        if let Ok(out) = git(ctx.common, &["log", "-1", "--format=%ct%x1f%s", sha]) {
            if let Some((ts, subject)) = out.trim_end().split_once('\x1f') {
                item.last_commit_subject = Some(subject.to_string());
                item.last_commit_days = ts
                    .parse::<u64>()
                    .ok()
                    .and_then(|t| days_since(UNIX_EPOCH + Duration::from_secs(t)));
            }
        }
    }

    let mut reasons: Vec<String> = vec![];
    let base_name = ctx.base.map(|b| b.0.clone()).unwrap_or_else(|| "main".into());

    // Commits that are not on the default branch.
    let mut probably_merged = false;
    match (ctx.base, &w.head) {
        (Some((_, refs)), Some(sha)) => {
            let best = refs
                .iter()
                .filter_map(|r| {
                    git(ctx.common, &["rev-list", "--count", &format!("{r}..{sha}")])
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok())
                        .map(|n| (n, r))
                })
                .min_by_key(|(n, _)| *n);
            match best {
                Some((0, _)) => item.ahead = Some(0),
                Some((n, r)) => {
                    item.ahead = Some(n);
                    let unique = git(ctx.common, &["cherry", r, sha])
                        .map(|s| s.lines().filter(|l| l.starts_with('+')).count())
                        .unwrap_or(usize::MAX);
                    if unique == 0 {
                        probably_merged = true;
                    } else {
                        let subjects: Vec<String> = git(
                            ctx.common,
                            &["log", "--format=%s", &format!("-{}", LIST_LIMIT), &format!("{r}..{sha}")],
                        )
                        .map(|s| s.lines().map(|l| format!("\"{l}\"")).collect())
                        .unwrap_or_default();
                        reasons.push(format!(
                            "{} not on {base_name}: {}",
                            plural(n as usize, "commit", "commits"),
                            quoted_list(&subjects, n as usize)
                        ));
                    }
                }
                None => reasons.push(format!("Jharu could not compare this worktree with {base_name}.")),
            }
        }
        (None, _) => reasons.push(
            "Jharu could not find the repository's main branch (no origin/HEAD, main or master) to compare against.".into(),
        ),
        (_, None) => reasons.push("The worktree has no commit checked out.".into()),
    }

    // Uncommitted work. Ignored files (build output, node_modules) do not
    // count, and neither do files whose only change is CRLF versus LF.
    match git(path, &["status", "--porcelain=v1", "--untracked-files=normal", "--ignored=matching"]) {
        Ok(out) => {
            let mut untracked = vec![];
            let mut changed = 0usize;
            for line in out.lines().filter(|l| l.len() > 3) {
                let file = line[3..].to_string();
                match &line[..2] {
                    "!!" => {}
                    "??" => untracked.push(file),
                    _ => changed += 1,
                }
            }
            let real_changes: Vec<String> = if changed == 0 {
                vec![]
            } else {
                match real_changes(path) {
                    Ok(v) => v,
                    Err(_) => out
                        .lines()
                        .filter(|l| l.len() > 3 && !l.starts_with("??") && !l.starts_with("!!"))
                        .map(|l| l[3..].to_string())
                        .collect(),
                }
            };
            item.line_ending_files = changed.saturating_sub(real_changes.len());
            let mut files = real_changes;
            files.extend(untracked);
            item.uncommitted_count = files.len();
            item.uncommitted = files.iter().take(LIST_LIMIT).cloned().collect();
            if !files.is_empty() {
                reasons.push(format!(
                    "{} uncommitted: {}",
                    plural(files.len(), "file", "files"),
                    quoted_list(&files, files.len())
                ));
            }
        }
        Err(e) => reasons.push(format!("git could not read this folder's status ({e}).")),
    }

    if let Some(reason) = &w.locked {
        reasons.push(if reason.is_empty() {
            "Locked with `git worktree lock`.".into()
        } else {
            format!("Locked with `git worktree lock`: {reason}")
        });
    }
    let nested = ctx.all_paths.iter().filter(|p| is_inside(p, path)).count();
    if nested > 0 {
        reasons.push(format!(
            "Contains {} inside it, which would be removed too.",
            plural(nested, "other worktree", "other worktrees")
        ));
    }
    if is_inside(ctx.common, path) || path_key(ctx.common) == path_key(path) {
        reasons.push("Contains the repository's own .git directory.".into());
    }

    let recreate = recreate_hint(path, &w.branch, &w.head);
    let noise = match item.line_ending_files {
        0 => String::new(),
        1 => " One file shows as modified only because of line endings (CRLF versus LF); its content is unchanged.".into(),
        n => format!(" {n} files show as modified only because of line endings (CRLF versus LF); their content is unchanged."),
    };

    if !reasons.is_empty() {
        item.summary = "Keep".into();
        item.explanation = format!(
            "This worktree holds work that is not on {base_name}, or Jharu could not confirm that it is. Jharu will not remove it."
        );
        item.reasons = reasons;
    } else if probably_merged {
        item.verdict = Verdict::ProbablyMerged;
        item.safety = Safety::Caution;
        item.summary = "Probably merged".into();
        item.explanation = format!(
            "Its {} have different IDs from {base_name}, but {base_name} already contains the same changes — usually the result of a rebase or cherry-pick merge. Check before removing.{noise} {recreate}",
            plural(item.ahead.unwrap_or(0) as usize, "commit", "commits")
        );
    } else {
        item.verdict = Verdict::Safe;
        item.safety = Safety::Safe;
        item.summary = if item.line_ending_files > 0 {
            "Merged, clean except line endings".into()
        } else {
            "Merged and clean".into()
        };
        item.explanation = format!(
            "Everything here is already on {base_name} and nothing is uncommitted, so this folder is a spare copy of the repository. Removing it moves the folder to the {BIN}.{noise} {recreate}"
        );
    }

    if let Some(tool) = &owner {
        item.explanation.push_str(&format!(
            " Created by {tool} for one of its sessions, which may still be using it — finish or archive that session first. Not selected automatically."
        ));
    }
    item.preselect = item.verdict == Verdict::Safe && owner.is_none();
    item
}

fn scan_repo(common: &Path) -> Option<WorktreeRepo> {
    let listed = list_worktrees(common).ok()?;
    let mut entries = listed.into_iter();
    // The first entry is always the main checkout (or the bare repository).
    // It is never offered for removal.
    let main = entries.next()?;
    let linked: Vec<Listed> = entries.filter(|w| !w.bare).collect();
    if linked.is_empty() {
        return None;
    }
    let base = default_branch(common);
    let all_paths: Vec<PathBuf> = linked.iter().map(|w| w.path.clone()).collect();
    let ctx = RepoCtx { common, base: base.as_ref(), all_paths: &all_paths };
    let mut items: Vec<WorktreeItem> = linked.iter().map(|w| inspect(&ctx, w)).collect();
    items.sort_by(|a, b| (a.verdict == Verdict::Record).cmp(&(b.verdict == Verdict::Record)).then(b.size_bytes.cmp(&a.size_bytes)));
    Some(WorktreeRepo {
        repo: if main.bare { common.to_string_lossy().into_owned() } else { main.path.to_string_lossy().into_owned() },
        common_dir: common.to_string_lossy().into_owned(),
        base: base.map(|b| b.0),
        total_bytes: items.iter().map(|i| i.size_bytes).sum(),
        items,
    })
}

fn orphan_item(folder: &Path, gitdir: &Path) -> WorktreeItem {
    let (size, newest) = measure(folder);
    WorktreeItem {
        path: folder.to_string_lossy().into_owned(),
        name: folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        common_dir: String::new(),
        branch: None,
        head: None,
        size_bytes: size,
        modified_days: newest.and_then(days_since),
        last_commit_days: None,
        last_commit_subject: None,
        ahead: None,
        uncommitted: vec![],
        uncommitted_count: 0,
        line_ending_files: 0,
        verdict: Verdict::Keep,
        safety: Safety::Risky,
        owner: owner_of(folder),
        preselect: false,
        summary: "Main repository missing".into(),
        explanation: "This folder was a worktree, but the repository it belonged to has been moved or deleted, so Jharu cannot check whether its work was merged. Look inside before deleting it yourself.".into(),
        reasons: vec![format!("Its .git file points to {}, which no longer exists.", gitdir.display())],
    }
}

fn scan_roots(roots: &[(PathBuf, usize, Vec<String>)]) -> WorktreeReport {
    let mut found = Discovery::default();
    discover(roots, &mut found);
    let git_available = git_binary().is_some();

    let mut repos: Vec<WorktreeRepo> = if git_available {
        found.repos.values().filter_map(|c| scan_repo(c)).collect()
    } else {
        vec![]
    };
    if !found.orphans.is_empty() {
        let items: Vec<WorktreeItem> = found.orphans.iter().map(|(f, g)| orphan_item(f, g)).collect();
        repos.push(WorktreeRepo {
            repo: "Worktrees whose repository is gone".into(),
            common_dir: String::new(),
            base: None,
            total_bytes: items.iter().map(|i| i.size_bytes).sum(),
            items,
        });
    }
    repos.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));

    let all = repos.iter().flat_map(|r| &r.items);
    let total_bytes = all.clone().map(|i| i.size_bytes).sum();
    let safe_bytes = all.filter(|i| i.verdict == Verdict::Safe).map(|i| i.size_bytes).sum();
    WorktreeReport { git_available, total_bytes, safe_bytes, repos }
}

#[tauri::command]
pub async fn worktree_scan() -> Result<WorktreeReport, String> {
    tauri::async_runtime::spawn_blocking(|| scan_roots(&default_roots()))
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- removal

#[derive(Deserialize, Clone)]
pub struct RemoveRequest {
    pub path: String,
    pub common_dir: String,
    /// The verdict the user saw; state that got worse since then is skipped.
    pub expected: Verdict,
}

#[derive(Serialize, Default)]
pub struct RemoveOutcome {
    pub freed_bytes: u64,
    pub removed: Vec<String>,
    /// Folders that vanished on their own (a tool archived its session).
    pub already_gone: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Serialize, Clone)]
struct RemoveProgress {
    token: String,
    done: usize,
    total: usize,
    current: String,
}

/// Checks a worktree folder is what it claims to be before it goes anywhere:
/// its `.git` must be a file pointing into this repository's worktree list.
fn guard(path: &Path, common: &Path, main: &Path) -> Result<(), String> {
    if path_key(path) == path_key(main) || is_inside(main, path) {
        return Err("it is the repository's main checkout".into());
    }
    if is_inside(common, path) || path_key(common) == path_key(path) {
        return Err("it contains the repository's .git directory".into());
    }
    let dot_git = path.join(".git");
    if !dot_git.is_file() {
        return Err("it has no .git file, so it is not a linked worktree".into());
    }
    let points_here = read_gitdir(&dot_git)
        .and_then(|g| g.parent().map(Path::to_path_buf))
        .is_some_and(|p| path_key(&p) == path_key(&common.join("worktrees")));
    if !points_here {
        return Err("its .git file points to a different repository".into());
    }
    Ok(())
}

fn remove_impl(requests: &[RemoveRequest], mut progress: impl FnMut(usize, usize, &str)) -> RemoveOutcome {
    let mut out = RemoveOutcome::default();
    let mut by_repo: BTreeMap<String, (PathBuf, Vec<&RemoveRequest>)> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let requests: Vec<&RemoveRequest> = requests.iter().filter(|r| seen.insert(path_key(Path::new(&r.path)))).collect();
    for r in requests.iter().copied() {
        let common = PathBuf::from(&r.common_dir);
        by_repo.entry(path_key(&common)).or_insert_with(|| (common, vec![])).1.push(r);
    }
    let total = requests.len();
    let mut done = 0usize;

    for (common, reqs) in by_repo.values() {
        let listed = match list_worktrees(common) {
            Ok(l) if !l.is_empty() => l,
            Ok(_) | Err(_) => {
                for r in reqs {
                    out.errors.push(format!("{}: git could not read its repository", r.path));
                }
                done += reqs.len();
                continue;
            }
        };
        let main = listed[0].path.clone();
        let base = default_branch(common);
        let all_paths: Vec<PathBuf> = listed[1..].iter().map(|w| w.path.clone()).collect();
        let ctx = RepoCtx { common, base: base.as_ref(), all_paths: &all_paths };
        let mut needs_prune = false;
        let mut expect_gone: Vec<PathBuf> = vec![];

        for r in reqs {
            let path = PathBuf::from(&r.path);
            let label = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| r.path.clone());
            progress(done, total, &label);
            done += 1;

            let Some(entry) = listed[1..].iter().find(|w| path_key(&w.path) == path_key(&path)) else {
                if path.exists() {
                    out.skipped.push(format!("{label}: git no longer lists this folder as a worktree"));
                } else {
                    out.already_gone.push(r.path.clone());
                }
                continue;
            };
            if !path.exists() {
                // Missing between scan and clean, or a stale record the user
                // chose to clear. Either way only git's record is left.
                if entry.locked.is_some() {
                    out.skipped.push(format!("{label}: the record is locked with `git worktree lock`"));
                } else {
                    if r.expected != Verdict::Record {
                        out.already_gone.push(r.path.clone());
                    }
                    needs_prune = true;
                    expect_gone.push(path);
                }
                continue;
            }
            if r.expected == Verdict::Record {
                out.skipped.push(format!("{label}: the folder exists again, so it is no longer a stale record"));
                continue;
            }
            if let Err(why) = guard(&path, common, &main) {
                out.skipped.push(format!("{label}: not removed because {why}"));
                continue;
            }
            // State can change after the scan; decide again right now.
            let now = inspect(&ctx, entry);
            if !now.verdict.removable() || now.verdict > r.expected {
                let why = if now.reasons.is_empty() { now.summary.clone() } else { now.reasons.join(" ") };
                out.skipped.push(format!("{label}: changed since the scan — {why}"));
                continue;
            }
            match crate::trashlog::to_bin(&[&path]) {
                Ok(()) => {
                    out.freed_bytes += now.size_bytes;
                    crate::trashlog::record(&path, now.size_bytes, "Git worktrees");
                    out.removed.push(r.path.clone());
                    needs_prune = true;
                    expect_gone.push(path);
                }
                // Windows moves a folder whole or not at all; a file held open
                // by an editor or terminal shows up only as "aborted".
                Err(e) if path.exists() && e.to_string().contains("aborted") => out.skipped.push(format!(
                    "{label}: a file inside is open in another program (an editor, terminal or dev server), so nothing was moved. Close it and try again."
                )),
                Err(e) => out.errors.push(format!("{label}: {e}")),
            }
        }

        if needs_prune {
            if let Err(e) = git(common, &["worktree", "prune"]) {
                out.errors.push(format!("git worktree prune failed in {}: {e}", main.display()));
            } else if let Ok(after) = list_worktrees(common) {
                for p in expect_gone {
                    if after.iter().any(|w| path_key(&w.path) == path_key(&p)) {
                        out.errors.push(format!("git still lists {} after pruning", p.display()));
                    }
                }
            }
        }
    }
    progress(total, total, "");
    out
}

/// Moves worktree folders to the Trash / Recycle Bin, then runs
/// `git worktree prune` so git forgets them. Branches are never deleted.
#[tauri::command]
pub async fn remove_worktrees(
    app: tauri::AppHandle,
    items: Vec<RemoveRequest>,
    token: String,
) -> Result<RemoveOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = crate::autocare::activity();
        remove_impl(&items, |done, total, current| {
            let _ = app.emit(
                "worktree-progress",
                RemoveProgress { token: token.clone(), done, total, current: current.to_string() },
            );
        })
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a throwaway repository with one worktree of every kind Jharu
    /// must tell apart. The folder name has spaces on purpose.
    struct Fixture {
        root: PathBuf,
        main: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!("jharu wt {tag} {}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            // git records real paths; macOS's temp dir is a symlink into
            // /private, so resolve it or no path would compare equal. (Not on
            // Windows, where canonicalize adds a \\?\ prefix git never prints.)
            #[cfg(not(target_os = "windows"))]
            let root = std::fs::canonicalize(&root).unwrap();
            let main = root.join("main repo");
            let f = Fixture { root, main };
            f.run(&f.root, &["init", "-q", "-b", "main", "main repo"]);
            f.run(&f.main, &["config", "core.autocrlf", "false"]);
            f.write(&f.main, "readme.txt", "line one\nline two\n");
            f.commit(&f.main, "initial");
            f
        }

        fn run(&self, dir: &Path, args: &[&str]) -> String {
            let bin = git_binary().expect("git must be installed to run these tests");
            let out = git_command(bin)
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=Jharu Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        fn write(&self, dir: &Path, file: &str, text: &str) {
            std::fs::write(dir.join(file), text).unwrap();
        }

        fn commit(&self, dir: &Path, msg: &str) {
            self.run(dir, &["add", "-A"]);
            self.run(dir, &["commit", "-q", "-m", msg]);
        }

        fn add(&self, folder: &str, args: &[&str]) -> PathBuf {
            let path = self.root.join(folder);
            let p = path.to_string_lossy().into_owned();
            let mut full = vec!["worktree", "add", "-q", p.as_str()];
            full.extend_from_slice(args);
            self.run(&self.main, &full);
            path
        }

        fn build(&self) {
            // Merged: work committed on a branch, then merged into main.
            let merged = self.add("wt merged", &["-b", "feature-merged"]);
            self.write(&merged, "merged.txt", "done\n");
            self.commit(&merged, "Finish merged feature");
            self.run(&self.main, &["merge", "-q", "--no-ff", "-m", "Merge feature-merged", "feature-merged"]);

            // Unmerged: a commit that exists only on its branch.
            let unmerged = self.add("wt unmerged", &["-b", "feature-unmerged"]);
            self.write(&unmerged, "wip.txt", "half done\n");
            self.commit(&unmerged, "Half-finished idea");

            // Cherry-picked: same change landed on main under a new ID.
            let picked = self.add("wt picked", &["-b", "feature-picked"]);
            self.write(&picked, "picked.txt", "picked\n");
            self.commit(&picked, "Picked change");
            let sha = self.run(&picked, &["rev-parse", "HEAD"]);
            // Move main on first, or the pick reproduces the identical commit.
            self.write(&self.main, "changelog.txt", "unrelated work on main\n");
            self.commit(&self.main, "Unrelated work on main");
            self.run(&self.main, &["cherry-pick", sha.trim()]);

            // Dirty: on main, but with an edit and a new file not committed.
            let dirty = self.add("wt dirty", &["-b", "feature-dirty"]);
            self.write(&dirty, "readme.txt", "line one\nchanged for real\n");
            self.write(&dirty, "notes.txt", "scratch\n");

            // Detached: no branch, commit already on main.
            self.add("wt detached", &["--detach", "main"]);

            // Line endings only: every line rewritten with CRLF, same text.
            let crlf = self.add("wt crlf", &["-b", "feature-crlf"]);
            self.write(&crlf, "readme.txt", "line one\r\nline two\r\n");

            // A tool's session worktree: merged and clean, but not preselected.
            let session = self.main.join(".claude").join("worktrees").join("session one");
            let s = session.to_string_lossy().into_owned();
            self.run(&self.main, &["worktree", "add", "-q", &s, "-b", "claude-session"]);

            // Prunable: the folder was deleted by hand; git still lists it.
            let gone = self.add("wt gone", &["-b", "feature-gone"]);
            std::fs::remove_dir_all(&gone).unwrap();
        }

        fn common(&self) -> PathBuf {
            self.main.join(".git")
        }

        fn scan(&self) -> WorktreeReport {
            scan_roots(&[(self.root.clone(), 6, vec![])])
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn by_name<'a>(r: &'a WorktreeReport, name: &str) -> &'a WorktreeItem {
        r.repos
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("worktree {name} not found"))
    }

    #[test]
    fn classifies_every_kind_of_worktree() {
        let f = Fixture::new("classify");
        f.build();
        let report = f.scan();
        assert!(report.git_available);
        assert_eq!(report.repos.len(), 1, "one repository expected");
        let repo = &report.repos[0];
        assert_eq!(path_key(Path::new(&repo.repo)), path_key(&f.main));
        assert_eq!(repo.base.as_deref(), Some("main"));
        assert!(
            repo.items.iter().all(|i| path_key(Path::new(&i.path)) != path_key(&f.main)),
            "the main checkout must never be offered"
        );
        for i in &repo.items {
            println!("{:<12} {:?} pre={} {} | {}", i.name, i.verdict, i.preselect, i.summary, i.reasons.join(" "));
        }

        let merged = by_name(&report, "wt merged");
        assert_eq!(merged.verdict, Verdict::Safe);
        assert_eq!(merged.ahead, Some(0));
        assert!(merged.explanation.contains("git worktree add"));
        assert!(merged.size_bytes > 0);

        let unmerged = by_name(&report, "wt unmerged");
        assert_eq!(unmerged.verdict, Verdict::Keep);
        assert_eq!(unmerged.ahead, Some(1));
        assert!(unmerged.reasons.iter().any(|r| r.contains("1 commit not on main") && r.contains("Half-finished idea")));

        let picked = by_name(&report, "wt picked");
        assert_eq!(picked.verdict, Verdict::ProbablyMerged);
        assert_eq!(picked.safety, Safety::Caution);

        let dirty = by_name(&report, "wt dirty");
        assert_eq!(dirty.verdict, Verdict::Keep);
        assert_eq!(dirty.uncommitted_count, 2);
        assert!(dirty.reasons.iter().any(|r| r.contains("2 files uncommitted") && r.contains("notes.txt")));

        let detached = by_name(&report, "wt detached");
        assert_eq!(detached.verdict, Verdict::Safe);
        assert!(detached.branch.is_none());

        let crlf = by_name(&report, "wt crlf");
        assert_eq!(crlf.verdict, Verdict::Safe, "line-ending noise must not block: {:?}", crlf.reasons);
        assert_eq!(crlf.line_ending_files, 1);
        assert!(crlf.summary.contains("line endings"));

        let session = by_name(&report, "session one");
        assert_eq!(session.verdict, Verdict::Safe);
        assert_eq!(session.owner.as_deref(), Some("Claude Code"));

        let gone = by_name(&report, "wt gone");
        assert_eq!(gone.verdict, Verdict::Record);

        let mut pre: Vec<&str> = repo.items.iter().filter(|i| i.preselect).map(|i| i.name.as_str()).collect();
        pre.sort();
        assert_eq!(pre, vec!["wt crlf", "wt detached", "wt gone", "wt merged"]);
    }

    #[test]
    fn removes_to_trash_prunes_and_rechecks() {
        let f = Fixture::new("remove");
        f.build();
        let report = f.scan();
        let common = f.common();

        let request = |name: &str| {
            let i = by_name(&report, name);
            RemoveRequest { path: i.path.clone(), common_dir: i.common_dir.clone(), expected: i.verdict }
        };
        let mut requests: Vec<RemoveRequest> = report.repos[0].items.iter().filter(|i| i.preselect).map(|i| request(&i.name)).collect();
        // The user also ticked the probably-merged one.
        requests.push(request("wt picked"));
        // A request for a "keep" worktree, as if state had been tampered with.
        requests.push(RemoveRequest { expected: Verdict::Safe, ..request("wt dirty") });

        // After the scan: one worktree gains a new file, one is archived by
        // its tool (folder vanishes).
        f.write(&f.root.join("wt merged"), "late.txt", "written after the scan\n");
        std::fs::remove_dir_all(f.root.join("wt detached")).unwrap();

        let mut events = vec![];
        let out = remove_impl(&requests, |d, t, c| events.push((d, t, c.to_string())));
        println!("removed {:?}\ngone {:?}\nskipped {:?}\nerrors {:?}", out.removed, out.already_gone, out.skipped, out.errors);

        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(events.last().map(|e| (e.0, e.1)), Some((requests.len(), requests.len())));
        let removed: Vec<String> = out.removed.iter().map(|p| Path::new(p).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(removed.contains(&"wt crlf".to_string()));
        assert!(removed.contains(&"wt picked".to_string()));
        assert!(out.freed_bytes > 0);
        assert!(out.already_gone.iter().any(|p| p.ends_with("wt detached")));
        assert!(out.skipped.iter().any(|s| s.starts_with("wt merged: changed since the scan") && s.contains("late.txt")));
        assert!(out.skipped.iter().any(|s| s.starts_with("wt dirty: changed since the scan")));

        // Folders are gone, git forgot them, and every branch survived.
        assert!(!f.root.join("wt crlf").exists());
        assert!(!f.root.join("wt picked").exists());
        assert!(f.root.join("wt merged").join("late.txt").exists());
        assert!(f.root.join("wt dirty").join("notes.txt").exists());
        assert!(f.main.join("readme.txt").exists(), "main checkout untouched");
        let listed: Vec<String> = list_worktrees(&common).unwrap().iter().map(|w| path_key(&w.path)).collect();
        for name in ["wt crlf", "wt picked", "wt detached", "wt gone"] {
            assert!(!listed.contains(&path_key(&f.root.join(name))), "{name} still listed");
        }
        for b in ["feature-crlf", "feature-picked", "feature-gone", "feature-merged"] {
            assert!(ref_exists(&common, &format!("refs/heads/{b}")), "branch {b} was deleted");
        }

        // The folders went to the Recycle Bin / Trash, not into oblivion.
        // Put them back so the test leaves the user's bin as it found it.
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        {
            let root = path_key(&f.root);
            let ours: Vec<trash::TrashItem> = trash::os_limited::list()
                .unwrap()
                .into_iter()
                .filter(|t| path_key(&t.original_parent) == root)
                .collect();
            let mut names: Vec<String> = ours.iter().map(|t| t.name.to_string_lossy().into_owned()).collect();
            names.sort();
            assert_eq!(names, vec!["wt crlf", "wt picked"]);
            trash::os_limited::restore_all(ours).unwrap();
            assert!(f.root.join("wt crlf").exists());
        }
    }

    /// Read-only scan of this machine: `cargo test --lib smoke_worktrees -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_worktrees() {
        let start = std::time::Instant::now();
        let report = scan_roots(&default_roots());
        println!("scanned in {:?}: {} repos, {:.2} GB total, {:.2} GB safe", start.elapsed(), report.repos.len(), report.total_bytes as f64 / 1e9, report.safe_bytes as f64 / 1e9);
        for r in &report.repos {
            println!("{} (base {:?})", r.repo, r.base);
            for i in &r.items {
                println!("  {:>8.1} MB  {:<16?} pre={:<5} {}  {}", i.size_bytes as f64 / 1e6, i.verdict, i.preselect, i.name, i.reasons.join(" "));
            }
        }
    }

    #[test]
    fn owner_detection() {
        assert_eq!(owner_of(Path::new("/r/app/.claude/worktrees/x")).as_deref(), Some("Claude Code"));
        assert_eq!(owner_of(Path::new("/home/u/.codex/worktrees/a1/app")).as_deref(), Some("Codex"));
        assert_eq!(owner_of(Path::new("/r/app-wt/feature")), None);
    }

    /// Deep clean asks this of every repository it finds: only work that
    /// exists nowhere else should make a folder "review first".
    #[test]
    fn repo_check_tells_backed_up_from_local_work() {
        let f = Fixture::new("repocheck");
        let c = check_repo(&f.main);
        assert!(c.readable && !c.has_remote && !c.backed_up(), "no remote: the only copy {c:?}");
        assert_eq!(c.unpushed, 1);

        f.run(&f.root, &["init", "-q", "--bare", "origin.git"]);
        let origin = f.root.join("origin.git").to_string_lossy().into_owned();
        f.run(&f.main, &["remote", "add", "origin", origin.as_str()]);
        f.run(&f.main, &["push", "-q", "origin", "main"]);
        let c = check_repo(&f.main);
        assert!(c.backed_up(), "pushed and clean {c:?}");

        f.write(&f.main, "notes.txt", "draft
");
        assert_eq!(check_repo(&f.main).uncommitted, 1);
        f.commit(&f.main, "Add notes");
        let c = check_repo(&f.main);
        assert_eq!((c.uncommitted, c.unpushed), (0, 1));
        assert!(!c.backed_up());

        f.run(&f.main, &["push", "-q", "origin", "main"]);
        f.write(&f.main, "readme.txt", "changed
");
        f.run(&f.main, &["stash", "-q"]);
        let c = check_repo(&f.main);
        assert!(c.stashed && !c.backed_up(), "a stash exists only here {c:?}");

        assert!(!check_repo(&f.root.join("not a repo")).readable);
        let _ = std::fs::remove_dir_all(&f.root);
    }
}
