//! What AI coding assistants and agents leave behind on disk, per tool.
//!
//! These tools keep three very different kinds of data side by side, and a
//! cleaner that cannot tell them apart either misses the gigabytes or eats
//! your history. Every location Jharu lists here has a class, and the class
//! decides the rating:
//!
//! - Cache, Logs: rebuilt or rewritten by the tool. Safe while the tool is
//!   not running, Caution while it is.
//! - Download: large things the tool fetches again (Claude Code binaries, VM
//!   images). Caution: nothing is lost, but expect a wait.
//! - History, State, Extensions: conversation transcripts, memory, settings,
//!   installed plugins. Risky. They are listed with size and age but never
//!   preselected, and nothing Jharu can do brings them back.
//! - Scratch: screenshots, recordings and images an agent made for you. Jharu
//!   cannot tell whether you still want them, so Caution.
//! - Workspace: an editor's per-project storage. For a project whose folder
//!   no longer exists it is Caution ("project folder is gone"); for projects
//!   that still exist it is only listed, never offered for removal.
//!
//! Old transcripts can be selected by age ("older than N days"); the rating
//! stays Risky. Git worktrees these tools create are not duplicated here:
//! the Git worktrees tab asks git whether each one is merged.
//!
//! Nothing is guessed: only locations Jharu knows are listed, so "unknown,
//! look inside" never appears as "safe". Removal goes to the Recycle Bin or
//! Trash, re-checks every item right before acting on it, skips what a
//! running tool may be using, reports progress, and can be cancelled.

use crate::rules::Safety;
use crate::safety::{Facts, Level, Reason};
use crate::usage::{reclaimable, Seen, Usage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tauri::Emitter;

#[cfg(target_os = "windows")]
const BIN: &str = "Recycle Bin";
#[cfg(not(target_os = "windows"))]
const BIN: &str = "Trash";

/// "Older than N days" choices offered for history.
const AGE_CUTOFFS: [u64; 5] = [7, 30, 90, 180, 365];
/// A file changed more recently than this is never moved by an age selection:
/// a session may still be writing to it.
const MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Entries handed to the Recycle Bin per call. One call per entry is slow
/// when a folder holds thousands; one call for all hides which ones failed.
const BATCH: usize = 50;
/// How many names of entries left in place to quote back.
const NAME_LIMIT: usize = 5;
/// Locations measured at the same time.
const SCAN_THREADS: usize = 4;
/// How far below the home folder to look for Aider's files.
const AIDER_DEPTH: usize = 5;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const AIDER_DRIVE_DEPTH: usize = 4;

// ---------------------------------------------------------------- tools

struct ToolDef {
    id: &'static str,
    name: &'static str,
    /// Process names (lowercase, no extension) that mean the tool is running.
    procs: &'static [&'static str],
    /// The Git worktrees tab handles this tool's spare checkouts.
    worktrees: bool,
}

const TOOLS: &[ToolDef] = &[
    ToolDef { id: "claude_code", name: "Claude Code", procs: &["claude"], worktrees: true },
    ToolDef { id: "claude_desktop", name: "Claude desktop app", procs: &["claude"], worktrees: false },
    ToolDef { id: "codex", name: "OpenAI Codex", procs: &["codex"], worktrees: true },
    ToolDef { id: "cursor", name: "Cursor", procs: &["cursor"], worktrees: true },
    // macOS names a program by its app bundle too (safety::app_bundle_name).
    ToolDef { id: "windsurf", name: "Windsurf (Codeium)", procs: &["windsurf"], worktrees: false },
    ToolDef {
        id: "copilot",
        name: "GitHub Copilot (VS Code)",
        procs: &["code", "code - insiders", "visual studio code", "visual studio code - insiders"],
        worktrees: false,
    },
    ToolDef { id: "continue", name: "Continue", procs: &["code", "visual studio code"], worktrees: false },
    ToolDef { id: "aider", name: "Aider", procs: &["aider"], worktrees: false },
];

fn tool_def(id: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.id == id)
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Cache,
    Logs,
    Download,
    History,
    State,
    Extensions,
    Scratch,
    Workspace,
    /// Handled by another tab; shown only to point there.
    Link,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Project {
    Exists,
    Gone,
    /// On a drive or machine Jharu cannot see right now, or unreadable.
    Unknown,
}

struct ProjectInfo {
    state: Project,
    path: Option<PathBuf>,
}

/// One location Jharu knows, before it is measured.
struct Spec {
    tool: &'static str,
    label: String,
    what: String,
    class: Class,
    /// Existing folders (or files). The first one identifies the item.
    paths: Vec<PathBuf>,
    removable: bool,
    /// Supports "older than N days" selection.
    aged: bool,
    /// Move the folder itself, not just what is inside it.
    trash_self: bool,
    /// A folder name never touched by an age selection (Claude's `memory`).
    skip_dir: Option<&'static str>,
    project: Option<ProjectInfo>,
    /// A reason to hold back that the class alone does not give.
    warn: Option<String>,
    link: Option<&'static str>,
}

impl Spec {
    fn new(tool: &'static str, label: &str, class: Class, paths: Vec<PathBuf>, what: &str) -> Spec {
        Spec {
            tool,
            label: label.to_string(),
            what: what.to_string(),
            class,
            paths,
            removable: class != Class::Link,
            aged: false,
            trash_self: false,
            skip_dir: None,
            project: None,
            warn: None,
            link: None,
        }
    }
    fn aged(mut self) -> Spec {
        self.aged = true;
        self
    }
    fn skipping(mut self, dir: &'static str) -> Spec {
        self.skip_dir = Some(dir);
        self
    }
    fn own(mut self) -> Spec {
        self.trash_self = true;
        self
    }
    fn listed_only(mut self) -> Spec {
        self.removable = false;
        self
    }
    fn warning(mut self, text: &str) -> Spec {
        self.warn = Some(text.to_string());
        self
    }
    fn id(&self) -> String {
        self.paths[0].to_string_lossy().into_owned()
    }
}

/// A symlink or junction. Moving what such a folder lists would move files
/// that live somewhere else.
fn is_link(p: &Path) -> bool {
    p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

/// Adds the spec if at least one of its folders exists. A location that is a
/// link to elsewhere is not offered for removal at all: what it holds is not
/// this tool's to clear.
fn add(v: &mut Vec<Spec>, mut spec: Spec) {
    let removable = spec.removable;
    spec.paths.retain(|p| p.symlink_metadata().is_ok() && !(removable && is_link(p)));
    if !spec.paths.is_empty() {
        v.push(spec);
    }
}

// ---------------------------------------------------------------- report

#[derive(Serialize, Clone)]
pub struct AgeBucket {
    pub days: u64,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Serialize, Clone)]
pub struct AiItem {
    /// The first folder's path; removal requests carry it back.
    pub id: String,
    pub tool: String,
    pub label: String,
    pub class: Class,
    pub path: String,
    /// Every folder this item covers (the first is `path`).
    pub paths: Vec<String>,
    pub size_bytes: u64,
    pub file_count: u64,
    pub modified_days: Option<u64>,
    pub safety: Safety,
    pub reasons: Vec<Reason>,
    pub explanation: String,
    pub removable: bool,
    pub preselect: bool,
    /// The tool is running now, so whole folders are held back.
    pub tool_running: bool,
    /// What an "older than N days" selection would move; empty unless the
    /// item supports it.
    pub ages: Vec<AgeBucket>,
    /// A tab that handles this location properly.
    pub link: Option<String>,
}

#[derive(Serialize)]
pub struct AiTool {
    pub id: String,
    pub name: String,
    pub running: Vec<String>,
    pub total_bytes: u64,
    pub safe_bytes: u64,
    pub items: Vec<AiItem>,
    /// This tool's spare git checkouts are cleaned in the Git worktrees tab.
    pub worktrees_link: bool,
}

#[derive(Serialize)]
pub struct AiReport {
    pub total_bytes: u64,
    /// Caches and logs of tools that are not running.
    pub safe_bytes: u64,
    /// History, settings and extensions: listed, never preselected.
    pub history_bytes: u64,
    pub tools: Vec<AiTool>,
}

// ---------------------------------------------------------------- folders

/// The per-user folders locations are found under; tests point them at a
/// throwaway tree.
#[allow(dead_code)]
struct Dirs {
    home: PathBuf,
    /// Roaming AppData on Windows, Application Support on macOS.
    config: PathBuf,
    /// Local AppData on Windows.
    local: PathBuf,
    /// Local AppData on Windows, Library/Caches on macOS.
    cache: PathBuf,
}

impl Dirs {
    fn real() -> Option<Dirs> {
        let home = dirs::home_dir()?;
        Some(Dirs {
            config: dirs::config_dir().unwrap_or_else(|| home.join(".config")),
            local: dirs::data_local_dir().unwrap_or_else(|| home.join(".local/share")),
            cache: dirs::cache_dir().unwrap_or_else(|| home.join(".cache")),
            home,
        })
    }
}

fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        s.to_lowercase()
    } else {
        s
    }
}

fn child_dirs(dir: &Path, wanted: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| wanted(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn under(base: &Path, names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(|n| base.join(n)).collect()
}

/// The Chromium-based shell that Cursor, VS Code, Windsurf and the Claude
/// desktop app share keeps the same rebuildable folders in the same places.
fn electron_caches(tool: &'static str, app: &str, base: &Path, v: &mut Vec<Spec>) {
    add(v, Spec::new(
        tool,
        &format!("{app} web cache"),
        Class::Cache,
        under(base, &["Cache"]),
        &format!("Pages and files {app}'s built-in browser downloaded. Fetched again when needed."),
    ));
    add(v, Spec::new(
        tool,
        &format!("{app} compiled code and graphics caches"),
        Class::Cache,
        under(base, &[
            "Code Cache", "GPUCache", "DawnGraphiteCache", "DawnWebGPUCache", "DawnCache", "GraphiteDawnCache",
            "GrShaderCache", "ShaderCache", "GPUPersistentCache",
        ]),
        &format!("JavaScript and graphics shaders {app} compiled once to start faster. Rebuilt on the next launch."),
    ));
    add(v, Spec::new(
        tool,
        &format!("{app} editor code cache"),
        Class::Cache,
        under(base, &["CachedData"]),
        &format!("Compiled copies of {app}'s own code, kept for faster starts. Rebuilt on the next launch."),
    ));
    add(v, Spec::new(
        tool,
        &format!("{app} downloaded extension installers"),
        Class::Cache,
        under(base, &["CachedExtensionVSIXs"]),
        "Installer files of extensions, kept for updating and reinstalling. Downloaded again if they are needed.",
    ));
    add(v, Spec::new(
        tool,
        &format!("{app} logs and crash reports"),
        Class::Logs,
        under(base, &["logs", "Crashpad"]),
        &format!("Diagnostic logs and crash dumps {app} wrote. It writes new ones; nothing reads the old ones."),
    ));
}

fn claude_code(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "claude_code";
    let c = d.home.join(".claude");
    add(v, Spec::new(t, "Session transcripts", Class::History, under(&c, &["projects"]),
        "Every conversation you had with Claude Code: one .jsonl file per session, in a folder per project. Each project's memory notes are kept apart and never touched by an age selection.")
        .aged().skipping("memory"));
    add(v, Spec::new(t, "File history", Class::History, under(&c, &["file-history"]),
        "Copies of files Claude Code edited, which let you rewind a session. They only matter for sessions you may still rewind.").aged());
    add(v, Spec::new(t, "Todo lists", Class::History, under(&c, &["todos"]),
        "The task lists Claude Code kept for each session.").aged());
    add(v, Spec::new(t, "Shell snapshots", Class::Cache, under(&c, &["shell-snapshots"]),
        "A snapshot of your shell environment, taken at the start of each session. Claude Code takes a fresh one next time."));
    add(v, Spec::new(t, "Feature-flag cache", Class::Cache, under(&c, &["statsig"]),
        "Cached feature flags and usage state. Fetched again at the next start."));
    add(v, Spec::new(t, "Claude Code cache", Class::Cache, under(&c, &["cache"]),
        "Claude Code's own cache. Rebuilt when needed."));
    add(v, Spec::new(t, "IDE connection files", Class::Cache, under(&c, &["ide"]),
        "Small files that tell Claude Code which editor is open and on which port. The editor extension writes them again when it reconnects.")
        .warning("An editor that is open right now may be connected through these files. Close the editor first, or it loses the connection until it reconnects."));
    add(v, Spec::new(t, "Command-line cache", Class::Cache, claude_cli_cache(d),
        "Cache the Claude Code command-line tool keeps. Rebuilt when needed."));
    add(v, Spec::new(t, "Plugins", Class::Extensions, under(&c, &["plugins"]),
        "Plugins you installed into Claude Code, with their settings. Reinstalling them is up to you."));
    add(v, Spec::new(t, "Skills", Class::Extensions, under(&c, &["skills"]),
        "Skills you wrote or installed. A skill you wrote exists nowhere else."));
}

/// The command-line tool keeps its cache in an `env-paths` folder. On Windows
/// that folder also has room for data and config, so only `Cache` is named.
fn claude_cli_cache(d: &Dirs) -> Vec<PathBuf> {
    let base = d.cache.join("claude-cli-nodejs");
    if cfg!(target_os = "windows") {
        vec![base.join("Cache")]
    } else {
        vec![base]
    }
}

fn claude_desktop(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "claude_desktop";
    let mut bases: Vec<PathBuf> = vec![];
    #[cfg(target_os = "windows")]
    {
        // Current builds keep their data in Claude-Data; older MSIX builds in
        // the package's LocalCache. %APPDATA%\Claude is only a view of the
        // package folder, so it is used only when neither exists.
        bases.extend(child_dirs(&d.local.join("Claude-Data"), |n| n.to_lowercase().starts_with("claude_")));
        for pkg in child_dirs(&d.local.join("Packages"), |n| n.starts_with("Claude_")) {
            bases.push(pkg.join("LocalCache").join("Roaming").join("Claude"));
        }
        if !bases.iter().any(|b| b.exists()) {
            bases.push(d.config.join("Claude"));
        }
        add(v, Spec::new(t, "Claude logs", Class::Logs, vec![d.local.join("Claude").join("Logs")],
            "Diagnostic logs the Claude desktop app wrote. It writes new ones."));
    }
    #[cfg(not(target_os = "windows"))]
    {
        bases.push(d.config.join("Claude"));
        add(v, Spec::new(t, "Claude logs", Class::Logs, vec![d.home.join("Library").join("Logs").join("Claude")],
            "Diagnostic logs the Claude desktop app wrote. It writes new ones."));
    }
    for base in bases.iter().filter(|b| b.is_dir()) {
        electron_caches(t, "Claude", base, v);
        // The app's own Help > Troubleshooting > Clear Cache empties the
        // cache folders above; its data folder as a whole must stay.
        add(v, Spec::new(t, "Claude Code downloaded by the app", Class::Download, under(base, &["claude-code"]),
            "Claude Code versions the desktop app downloaded for its Code tab. It downloads the one it needs again."));
        add(v, Spec::new(t, "Cowork virtual machine images", Class::Download, under(base, &["vm_bundles", "vm-bundles", "vm_bundle", "vm"]),
            "The virtual machine images Claude's agent mode runs in. Large; downloaded again the next time you use it."));
        add(v, Spec::new(t, "Agent scratch workspaces", Class::Scratch, under(base, &["scratch-workspaces"]),
            "Working folders the app gave its agents for scratch files."));
        add(v, Spec::new(t, "Code and agent sessions", Class::History, under(base, &["claude-code-sessions", "local-agent-mode-sessions"]),
            "Sessions from the app's Code tab and agent mode, with the files they produced.").aged());
    }
}

fn codex(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "codex";
    let c = d.home.join(".codex");
    add(v, Spec::new(t, "Session transcripts", Class::History, under(&c, &["sessions"]),
        "Every conversation you had with Codex, one rollout .jsonl file per session, in folders by date.").aged());
    add(v, Spec::new(t, "Archived sessions", Class::History, under(&c, &["archived_sessions"]),
        "Sessions you archived in Codex.").aged());
    add(v, Spec::new(t, "Thread database", Class::State, under(&c, &["sqlite"]),
        "Codex's index of your threads. Losing it loses the thread list."));
    add(v, Spec::new(t, "Logs", Class::Logs, under(&c, &["log"]), "Diagnostic logs Codex wrote. It writes new ones."));
    add(v, Spec::new(t, "Cache", Class::Cache, under(&c, &["cache", "tmp", ".tmp"]),
        "Codex's cache and temporary folders (downloaded plugin listings, helper files). Rebuilt when needed."));
    add(v, Spec::new(t, "Generated images", Class::Scratch, under(&c, &["generated_images"]),
        "Images Codex generated for you, one folder per session."));
    add(v, Spec::new(t, "Plugins", Class::Extensions, under(&c, &["plugins"]),
        "Plugins installed into Codex, with their settings. Reinstalling them is up to you."));
    add(v, Spec::new(t, "Spare git checkouts", Class::Link, under(&c, &["worktrees"]),
        "Worktrees Codex made for its sessions. The Git worktrees tab asks git whether each is merged.").listed_only().linked("worktrees"));
    #[cfg(target_os = "windows")]
    for pkg in child_dirs(&d.local.join("Packages"), |n| n.starts_with("OpenAI.Codex_")) {
        let cache = pkg.join("LocalCache");
        electron_caches(t, "Codex app", &cache.join("Roaming").join("Codex"), v);
        add(v, Spec::new(t, "Codex app logs", Class::Logs, vec![cache.join("Local").join("Codex").join("Logs")],
            "Diagnostic logs the Codex desktop app wrote. It writes new ones."));
    }
}

impl Spec {
    fn linked(mut self, tab: &'static str) -> Spec {
        self.link = Some(tab);
        self
    }
}

fn cursor(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "cursor";
    let base = d.config.join("Cursor");
    electron_caches(t, "Cursor", &base, v);
    workspace_specs(t, "Cursor", &base.join("User").join("workspaceStorage"), v);
    add(v, Spec::new(t, "Global state: chats, settings, sign-in", Class::State, vec![base.join("User").join("globalStorage")],
        "Cursor's saved chats and composer history, its settings and your sign-in, and the state of its extensions."));
    add(v, Spec::new(t, "Extensions", Class::Extensions, vec![d.home.join(".cursor").join("extensions")],
        "Extensions installed in Cursor. Reinstalling them is up to you."));
    add(v, Spec::new(t, "Spare git checkouts", Class::Link, vec![d.home.join(".cursor").join("worktrees")],
        "Worktrees Cursor made for its agents. The Git worktrees tab asks git whether each is merged.").listed_only().linked("worktrees"));
}

fn windsurf(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "windsurf";
    let base = d.config.join("Windsurf");
    electron_caches(t, "Windsurf", &base, v);
    workspace_specs(t, "Windsurf", &base.join("User").join("workspaceStorage"), v);
    add(v, Spec::new(t, "Global state: chats, settings, sign-in", Class::State, vec![base.join("User").join("globalStorage")],
        "Windsurf's saved state: settings, sign-in and the state of its extensions."));
    let codeium = d.home.join(".codeium");
    add(v, Spec::new(t, "Language server", Class::Download, under(&codeium, &["bin"]),
        "The Codeium language server programs. Downloaded again when Windsurf or a Codeium extension starts."));
    add(v, Spec::new(t, "Cascade conversations", Class::History, vec![codeium.join("windsurf").join("cascade")],
        "Your conversations with Windsurf's Cascade agent.").aged());
    add(v, Spec::new(t, "Cascade memories", Class::State, vec![codeium.join("windsurf").join("memories")],
        "What Cascade remembered about you and your projects."));
    add(v, Spec::new(t, "Extensions", Class::Extensions, vec![d.home.join(".windsurf").join("extensions")],
        "Extensions installed in Windsurf. Reinstalling them is up to you."));
}

fn copilot(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "copilot";
    for (app, folder, ext_dir) in [("VS Code", "Code", ".vscode"), ("VS Code Insiders", "Code - Insiders", ".vscode-insiders")] {
        let base = d.config.join(folder);
        electron_caches(t, app, &base, v);
        workspace_specs(t, app, &base.join("User").join("workspaceStorage"), v);
        let global = base.join("User").join("globalStorage");
        for dir in child_dirs(&global, |n| n.to_lowercase().starts_with("github.copilot")) {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            add(v, Spec::new(t, &format!("{app}: {name} storage"), Class::State, vec![dir],
                "What the Copilot extension saved: chat history, settings and cached data. Jharu cannot tell which part is only a cache.").own());
        }
        add(v, Spec::new(t, &format!("{app} chats outside a project"), Class::History, vec![global.join("emptyWindowChatSessions")],
            "Copilot chats you had in a window with no folder open.").aged());
        add(v, Spec::new(t, &format!("{app} agent sessions"), Class::History, under(&base, &["agentSessionData"]),
            "Agent sessions saved by the editor's built-in AI features.").aged());
        let exts = d.home.join(ext_dir).join("extensions");
        for dir in child_dirs(&exts, |n| n.to_lowercase().starts_with("github.copilot")) {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            add(v, Spec::new(t, &format!("Extension {name}"), Class::Extensions, vec![dir],
                "An installed Copilot extension version. Older versions pile up after updates, but Jharu cannot tell which one the editor is loading.").own());
        }
    }
}

fn continue_dev(d: &Dirs, v: &mut Vec<Spec>) {
    let t = "continue";
    let c = d.home.join(".continue");
    add(v, Spec::new(t, "Chat sessions", Class::History, under(&c, &["sessions"]),
        "Your saved conversations with Continue, one .json file per session.").aged());
    add(v, Spec::new(t, "Code index", Class::Cache, under(&c, &["index"]),
        "The search index Continue built over your code. Rebuilt the next time it indexes a project."));
    add(v, Spec::new(t, "Logs and usage data", Class::Logs, under(&c, &["logs", "dev_data"]),
        "Logs and usage records Continue wrote on this machine. It writes new ones."));
}

fn build_specs(d: &Dirs) -> Vec<Spec> {
    let mut v = vec![];
    claude_code(d, &mut v);
    claude_desktop(d, &mut v);
    codex(d, &mut v);
    cursor(d, &mut v);
    windsurf(d, &mut v);
    copilot(d, &mut v);
    continue_dev(d, &mut v);
    v
}

// ---------------------------------------------------------------- workspaces

/// Turns the `file:///d%3A/projects/app` an editor stores into a path.
/// Anything that is not a local file (a remote machine, a container) is None.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // A host part means a network share; Jharu cannot vouch for those.
    if !rest.starts_with('/') {
        return None;
    }
    let raw = rest.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                bytes.push(b);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let decoded = String::from_utf8(bytes).ok()?;
    #[cfg(target_os = "windows")]
    {
        // "/d:/projects/app" becomes a drive path with backslash separators.
        let b = decoded.as_bytes();
        if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            return Some(PathBuf::from(decoded[1..].replace('/', "\\")));
        }
        return None;
    }
    #[cfg(not(target_os = "windows"))]
    return Some(PathBuf::from(decoded));
}

/// Whether `p` still exists. A path on a drive or volume that is not here is
/// Unknown, not Gone: an unplugged disk must never read as "deleted".
fn project_state(p: &Path) -> Project {
    match std::fs::metadata(p) {
        Ok(_) => return Project::Exists,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Project::Unknown,
    }
    use std::path::Component;
    let mut comps = p.components();
    match comps.next() {
        #[cfg(target_os = "windows")]
        Some(Component::Prefix(prefix)) => {
            use std::path::Prefix;
            match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    let root = format!("{}:\\", letter as char);
                    if Path::new(&root).exists() {
                        Project::Gone
                    } else {
                        Project::Unknown
                    }
                }
                _ => Project::Unknown,
            }
        }
        #[cfg(not(target_os = "windows"))]
        Some(Component::RootDir) => {
            // Removable and network volumes mount below these.
            let first = comps.next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
            if matches!(first.as_str(), "Volumes" | "mnt" | "media" | "run") {
                let mut mount = PathBuf::from("/");
                mount.push(&first);
                if let Some(c) = comps.next() {
                    mount.push(c.as_os_str());
                }
                if !mount.exists() {
                    return Project::Unknown;
                }
            }
            Project::Gone
        }
        _ => Project::Unknown,
    }
}

/// Reads which project a `workspaceStorage/<hash>` folder belongs to.
fn read_workspace(dir: &Path) -> ProjectInfo {
    let unknown = ProjectInfo { state: Project::Unknown, path: None };
    let Ok(text) = std::fs::read_to_string(dir.join("workspace.json")) else { return unknown };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return unknown };
    let uri = json.get("folder").or_else(|| json.get("workspace")).and_then(|v| v.as_str());
    match uri.and_then(file_uri_to_path) {
        Some(path) => ProjectInfo { state: project_state(&path), path: Some(path) },
        None => unknown,
    }
}

fn workspace_specs(tool: &'static str, app: &str, dir: &Path, v: &mut Vec<Spec>) {
    for ws in child_dirs(dir, |_| true) {
        let info = read_workspace(&ws);
        let name = info
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| ws.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let mut spec = Spec::new(
            tool,
            &format!("{app} storage for {name}"),
            Class::Workspace,
            vec![ws],
            &format!("What {app} saved for one project: its chat sessions, editing history and editor state."),
        )
        .own();
        spec.project = Some(info);
        add(v, spec);
    }
}

// ---------------------------------------------------------------- Aider

fn skip_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "Library" | "AppData" | "target" | "venv" | "site-packages" | "Pictures" | "Music"
                | "Movies" | "Videos" | "Photos Library.photoslibrary" | "$Recycle.Bin" | "System Volume Information"
        )
}

fn aider_roots() -> Vec<(PathBuf, usize, Vec<String>)> {
    let mut roots = vec![];
    let home = dirs::home_dir();
    if let Some(h) = &home {
        roots.push((h.clone(), AIDER_DEPTH, vec![]));
    }
    #[cfg(target_os = "windows")]
    {
        let skip: Vec<String> = ["Windows", "Program Files", "Program Files (x86)", "ProgramData", "Users", "Recovery"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        for d in disks.list() {
            let mount = d.mount_point().to_path_buf();
            if d.is_removable() || home.as_ref().is_some_and(|h| path_key(h) == path_key(&mount)) {
                continue;
            }
            roots.push((mount, AIDER_DRIVE_DEPTH, skip.clone()));
        }
    }
    roots
}

/// Aider writes `.aider.*` files into the folder it was started in. Finds
/// them, as (parent folder, file name, is a folder).
fn find_aider(roots: &[(PathBuf, usize, Vec<String>)]) -> Vec<(PathBuf, String, bool)> {
    let found: Arc<Mutex<Vec<(PathBuf, String, bool)>>> = Arc::new(Mutex::new(vec![]));
    for (root, depth, top_skip) in roots {
        if !root.is_dir() {
            continue;
        }
        let top_skip = top_skip.clone();
        let sink = found.clone();
        let walker = crate::walk::walk(root)
            .max_depth(*depth)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(move |depth, dir, _, children| {
                for c in children.iter().flatten() {
                    let name = c.file_name.to_string_lossy();
                    if name.starts_with(".aider") {
                        if let Ok(mut f) = sink.lock() {
                            f.push((dir.to_path_buf(), name.into_owned(), c.file_type().is_dir()));
                        }
                    }
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
        for _ in walker.into_iter().flatten() {}
    }
    let mut v = found.lock().map(|f| f.clone()).unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

fn aider_specs(roots: &[(PathBuf, usize, Vec<String>)]) -> Vec<Spec> {
    let mut v = vec![];
    for (dir, name, _) in find_aider(roots) {
        let lower = name.to_lowercase();
        let project = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.to_string_lossy().into_owned());
        let class = if lower.starts_with(".aider.chat.history") || lower.starts_with(".aider.input.history") || lower.starts_with(".aider.llm.history") {
            Class::History
        } else if lower.starts_with(".aider.tags.cache") {
            Class::Cache
        } else {
            // .aider.conf.yml, model settings: configuration, not leftovers.
            continue;
        };
        let what = match class {
            Class::History => "Aider's record of your conversations in this project, kept next to your code.",
            _ => "Aider's index of this project's code. Rebuilt the next time Aider runs there.",
        };
        add(&mut v, Spec::new("aider", &format!("{project}/{name}"), class, vec![dir.join(&name)], what).listed_only());
    }
    v
}

// ---------------------------------------------------------------- rating

fn running_names(def: &ToolDef, facts: &Facts) -> Vec<String> {
    def.procs.iter().filter(|p| facts.running.contains(**p)).map(|p| p.to_string()).collect()
}

fn raise(s: &mut Safety, to: Safety) {
    if to > *s {
        *s = to;
    }
}

fn ok(text: String) -> Reason {
    Reason { level: Level::Ok, text }
}
fn warn(text: String) -> Reason {
    Reason { level: Level::Warn, text }
}
fn bad(text: String) -> Reason {
    Reason { level: Level::Bad, text }
}

/// How safe it is to remove this location right now, and why.
fn rate(def: &ToolDef, spec: &Spec, facts: &Facts) -> (Safety, Vec<Reason>) {
    let name = def.name;
    let mut safety = Safety::Safe;
    let mut reasons: Vec<Reason> = vec![];

    // What removing it loses, and whether it comes back.
    match spec.class {
        Class::Cache => reasons.push(ok(format!(
            "A cache: {name} rebuilds it by itself the next time it runs. Nothing is lost and nothing stops working."
        ))),
        Class::Logs => reasons.push(ok(format!(
            "Logs and crash reports: {name} writes new ones, and nothing depends on the old ones."
        ))),
        Class::Download => {
            raise(&mut safety, Safety::Caution);
            reasons.push(warn(format!(
                "{name} downloads it again when it needs it, but these are large files, so expect a wait the next time."
            )));
        }
        Class::History => {
            raise(&mut safety, Safety::Risky);
            reasons.push(bad(format!(
                "Your conversation history with {name}; cannot be re-created. Once the {BIN} is emptied, it is gone."
            )));
        }
        Class::State => {
            raise(&mut safety, Safety::Risky);
            reasons.push(bad(format!(
                "{name}'s saved state: chat history, memory and settings. Jharu cannot re-create it; once the {BIN} is emptied, it is gone."
            )));
        }
        Class::Extensions => {
            raise(&mut safety, Safety::Risky);
            reasons.push(bad(format!(
                "Extensions, plugins or skills installed for {name}, with their settings. Reinstalling is a manual job, and anything you wrote yourself is not recoverable."
            )));
        }
        Class::Scratch => {
            raise(&mut safety, Safety::Caution);
            reasons.push(warn(
                "Screenshots, recordings, images or working files an agent made for you. Nothing re-creates them and Jharu cannot tell whether you still need them; look through them first.".into(),
            ));
        }
        Class::Workspace => match spec.project.as_ref() {
            Some(ProjectInfo { state: Project::Gone, path }) => {
                raise(&mut safety, Safety::Caution);
                let shown = path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                reasons.push(warn(format!(
                    "The project folder is gone: {shown} no longer exists, so nothing can open these chats and this editor state from that project again."
                )));
                reasons.push(warn(format!(
                    "It still holds that project's saved chats, and Jharu cannot re-create them once the {BIN} is emptied."
                )));
            }
            Some(ProjectInfo { state: Project::Exists, path }) => {
                raise(&mut safety, Safety::Risky);
                let shown = path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                reasons.push(bad(format!(
                    "The project folder still exists ({shown}); this is its chat history and editor state, and cannot be re-created."
                )));
            }
            _ => {
                raise(&mut safety, Safety::Risky);
                reasons.push(warn(
                    "Jharu cannot see where this project lives (a drive or remote machine that is not connected, or a path it cannot read), so it cannot tell whether the project still exists.".into(),
                ));
            }
        },
        Class::Link => {
            raise(&mut safety, Safety::Caution);
            reasons.push(warn(
                "Spare git checkouts. The Git worktrees tab asks git whether each one is merged and what is uncommitted; use that tab, not this one.".into(),
            ));
        }
    }

    if let Some(text) = &spec.warn {
        raise(&mut safety, Safety::Caution);
        reasons.push(warn(text.clone()));
    }

    // Whether a program is using it at this moment.
    let running = running_names(def, facts);
    let inside: Vec<String> = spec.paths.iter().flat_map(|p| facts.running_from(p)).collect();
    if !running.is_empty() {
        raise(&mut safety, Safety::Caution);
        reasons.push(warn(if spec.aged {
            format!(
                "{} is running right now. Jharu only moves sessions older than the age you choose, and never anything changed in the last day, so a live session is left alone.",
                running.join(", ")
            )
        } else {
            format!(
                "{} is running right now and may be using it. Close it first; otherwise Jharu leaves everything in place.",
                running.join(", ")
            )
        }));
    } else if !inside.is_empty() {
        raise(&mut safety, Safety::Caution);
        reasons.push(warn(format!("{} is running from inside this folder right now. Close it first.", inside.join(", "))));
    } else if spec.class != Class::Link {
        reasons.push(ok(format!("Not in use right now: {name} is not running.")));
    }

    (safety, reasons)
}

// ---------------------------------------------------------------- measuring

#[derive(Default, Clone)]
struct Measured {
    bytes: u64,
    files: u64,
    newest: Option<SystemTime>,
    ages: Vec<(u64, u64)>,
}

fn days_since(t: SystemTime) -> Option<u64> {
    SystemTime::now().duration_since(t).ok().map(|d| d.as_secs() / 86_400)
}

/// Whether an age selection may touch this file: not under a skipped folder
/// (a project's `memory`).
fn selectable(skip_dir: Option<&str>, root: &Path, file: &Path) -> bool {
    let Ok(rel) = file.strip_prefix(root) else { return false };
    match skip_dir {
        Some(skip) => !rel.components().any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(skip)),
        None => true,
    }
}

fn measure(spec: &Spec) -> Measured {
    let mut m = Measured { ages: vec![(0, 0); AGE_CUTOFFS.len()], ..Default::default() };
    let mut usage = Usage::default();
    let mut seen = Seen::default();
    let mut note = |m: &mut Measured, path: &Path, root: &Path, meta: &std::fs::Metadata| {
        usage.add_file(path, meta, &mut seen);
        let modified = meta.modified().ok();
        if let Some(t) = modified {
            if m.newest.map_or(true, |n| t > n) {
                m.newest = Some(t);
            }
        }
        if spec.aged && selectable(spec.skip_dir, root, path) {
            if let Some(age) = modified.and_then(days_since) {
                for (i, cutoff) in AGE_CUTOFFS.iter().enumerate() {
                    if age >= *cutoff {
                        m.ages[i].0 += meta.len();
                        m.ages[i].1 += 1;
                    }
                }
            }
        }
    };
    for root in &spec.paths {
        if root.is_file() {
            if let Ok(meta) = root.symlink_metadata() {
                note(&mut m, root, root.parent().unwrap_or(root), &meta);
            }
            continue;
        }
        for entry in crate::walk::walk(root).skip_hidden(false).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                note(&mut m, &entry.path(), root, &meta);
            }
        }
    }
    m.bytes = usage.bytes;
    m.files = usage.files;
    m
}

/// Measures every spec, a few at a time. Each walk already spreads over
/// Jharu's walk pool; this keeps it busy while one folder is mostly one
/// huge file.
fn measure_all(specs: &[Spec]) -> Vec<Measured> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Measured>> = Mutex::new(vec![Measured::default(); specs.len()]);
    std::thread::scope(|s| {
        for _ in 0..SCAN_THREADS.min(specs.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= specs.len() {
                    break;
                }
                let m = measure(&specs[i]);
                if let Ok(mut r) = results.lock() {
                    r[i] = m;
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

// ---------------------------------------------------------------- scan

fn make_item(def: &ToolDef, spec: &Spec, m: &Measured, facts: &Facts) -> AiItem {
    let (safety, reasons) = rate(def, spec, facts);
    let running = !running_names(def, facts).is_empty() || spec.paths.iter().any(|p| !facts.running_from(p).is_empty());
    let aged_ok = spec.aged && spec.removable;
    AiItem {
        id: spec.id(),
        tool: spec.tool.to_string(),
        label: spec.label.clone(),
        class: spec.class,
        path: spec.id(),
        paths: spec.paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        size_bytes: m.bytes,
        file_count: m.files,
        modified_days: m.newest.and_then(days_since),
        safety,
        reasons,
        explanation: spec.what.clone(),
        removable: spec.removable,
        preselect: spec.removable && safety == Safety::Safe,
        tool_running: running,
        ages: if aged_ok {
            AGE_CUTOFFS
                .iter()
                .zip(&m.ages)
                .map(|(d, (bytes, files))| AgeBucket { days: *d, bytes: *bytes, files: *files })
                .collect()
        } else {
            vec![]
        },
        link: spec.link.map(String::from),
    }
}

fn scan_with(d: &Dirs, facts: &Facts, aider: &[(PathBuf, usize, Vec<String>)]) -> AiReport {
    let mut specs = build_specs(d);
    specs.extend(aider_specs(aider));
    let measured = measure_all(&specs);

    // Per-project storage for projects that still exist (or cannot be
    // checked) is folded into one listed-only row per editor, so a few
    // hundred projects do not bury the rest.
    struct Fold {
        tool: &'static str,
        dir: PathBuf,
        count: usize,
        unknown: usize,
        m: Measured,
    }
    let mut folds: BTreeMap<String, Fold> = BTreeMap::new();
    let mut by_tool: HashMap<&'static str, Vec<AiItem>> = HashMap::new();
    for (spec, m) in specs.iter().zip(&measured) {
        let Some(def) = tool_def(spec.tool) else { continue };
        let gone = matches!(spec.project, Some(ProjectInfo { state: Project::Gone, .. }));
        if spec.class == Class::Workspace && !gone {
            let Some(dir) = spec.paths[0].parent() else { continue };
            let fold = folds.entry(path_key(dir)).or_insert_with(|| Fold {
                tool: spec.tool,
                dir: dir.to_path_buf(),
                count: 0,
                unknown: 0,
                m: Measured::default(),
            });
            fold.count += 1;
            if matches!(spec.project, Some(ProjectInfo { state: Project::Unknown, .. })) {
                fold.unknown += 1;
            }
            fold.m.bytes += m.bytes;
            fold.m.files += m.files;
            if m.newest.map_or(false, |t| fold.m.newest.map_or(true, |n| t > n)) {
                fold.m.newest = m.newest;
            }
            continue;
        }
        by_tool.entry(spec.tool).or_default().push(make_item(def, spec, m, facts));
    }
    for fold in folds.into_values() {
        let Some(def) = tool_def(fold.tool) else { continue };
        let mut spec = Spec::new(
            fold.tool,
            &format!("Storage for {} project{} that still exist", fold.count, if fold.count == 1 { "" } else { "s" }),
            Class::Workspace,
            vec![fold.dir],
            "Chat history and editor state for projects that are still on disk. Listed so the total adds up; to remove one, delete it from the editor, because Jharu cannot tell which of them you still use.",
        )
        .listed_only();
        spec.project = Some(ProjectInfo { state: Project::Exists, path: None });
        let mut item = make_item(def, &spec, &fold.m, facts);
        item.reasons = vec![bad(
            "Chat history and editor state for projects whose folders still exist; cannot be re-created.".into(),
        )];
        if fold.unknown > 0 {
            item.reasons.push(warn(format!(
                "{} of them belong to drives or remote machines Jharu cannot see right now, so it cannot tell whether those projects still exist.",
                fold.unknown
            )));
        }
        item.reasons.extend(rate(def, &spec, facts).1.into_iter().filter(|r| r.text.contains("running") || r.text.contains("in use")));
        by_tool.entry(fold.tool).or_default().push(item);
    }

    let mut tools = vec![];
    for def in TOOLS {
        let Some(mut items) = by_tool.remove(def.id) else { continue };
        items.sort_by(|a, b| (!a.removable).cmp(&!b.removable).then(b.size_bytes.cmp(&a.size_bytes)));
        let counted = || items.iter().filter(|i| i.class != Class::Link);
        let total_bytes = counted().map(|i| i.size_bytes).sum();
        let safe_bytes = counted().filter(|i| i.removable && i.safety == Safety::Safe).map(|i| i.size_bytes).sum();
        tools.push(AiTool {
            id: def.id.to_string(),
            name: def.name.to_string(),
            running: running_names(def, facts),
            total_bytes,
            safe_bytes,
            worktrees_link: def.worktrees,
            items,
        });
    }
    tools.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));

    let all = || tools.iter().flat_map(|t| &t.items).filter(|i| i.class != Class::Link);
    AiReport {
        total_bytes: all().map(|i| i.size_bytes).sum(),
        safe_bytes: all().filter(|i| i.removable && i.safety == Safety::Safe).map(|i| i.size_bytes).sum(),
        history_bytes: all().filter(|i| i.safety == Safety::Risky).map(|i| i.size_bytes).sum(),
        tools,
    }
}

#[tauri::command]
pub async fn ai_tools_scan() -> Result<AiReport, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(d) = Dirs::real() else {
            return AiReport { total_bytes: 0, safe_bytes: 0, history_bytes: 0, tools: vec![] };
        };
        scan_with(&d, &Facts::gather(), &aider_roots())
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- removal

/// The rating the user saw. Local, because the rating type is serialize-only.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Expected {
    Safe,
    Caution,
    Risky,
}

impl Expected {
    fn safety(self) -> Safety {
        match self {
            Expected::Safe => Safety::Safe,
            Expected::Caution => Safety::Caution,
            Expected::Risky => Safety::Risky,
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct AiRemoveRequest {
    /// The item's `id`.
    pub path: String,
    /// For history: move only files older than this many days.
    pub older_than_days: Option<u64>,
    pub expected: Expected,
}

#[derive(Serialize, Default, Debug)]
pub struct AiRemoveOutcome {
    pub freed_bytes: u64,
    pub moved: u64,
    pub removed: Vec<String>,
    pub already_gone: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

#[derive(Serialize, Clone)]
struct AiProgress {
    token: String,
    done: usize,
    total: usize,
    current: String,
    /// Progress inside the current item.
    step_done: usize,
    step_total: usize,
}

fn cancelled_tokens() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Asks a running removal to stop after the batch it is on.
#[tauri::command]
pub fn cancel_ai_removal(token: String) {
    if let Ok(mut s) = cancelled_tokens().lock() {
        s.insert(token);
    }
}

#[derive(Default)]
struct Trashed {
    freed: u64,
    moved: u64,
    left: Vec<String>,
    cancelled: bool,
}

/// Moves entries to the Recycle Bin / Trash in batches, then trusts only
/// what is no longer there. Windows moves what it can and reports the rest
/// as "aborted", so a program holding a file open leaves just that entry.
fn trash_entries(paths: &[PathBuf], cancelled: &dyn Fn() -> bool, step: &mut dyn FnMut(usize, usize)) -> Trashed {
    let mut r = Trashed::default();
    step(0, paths.len());
    for batch in paths.chunks(BATCH) {
        if cancelled() {
            r.cancelled = true;
            break;
        }
        // What deleting really releases, asked before the files are gone.
        let sizes: Vec<u64> = batch.iter().map(|p| reclaimable(p)).collect();
        if crate::trashlog::to_bin(batch).is_err() && batch.len() > 1 && batch.iter().all(|p| p.symlink_metadata().is_ok()) {
            // Failed before moving anything; give every entry its own chance.
            for p in batch {
                let _ = crate::trashlog::to_bin(&[p]);
            }
        }
        for (p, size) in batch.iter().zip(sizes) {
            if p.symlink_metadata().is_err() {
                r.moved += 1;
                r.freed += size;
            } else {
                r.left.push(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            }
        }
        step((r.moved as usize + r.left.len()).min(paths.len()), paths.len());
    }
    r
}

/// What a whole-location removal moves: the folders themselves for items
/// that are folders per project, otherwise what is inside each folder, so
/// the folder the tool expects stays in place. Links are never moved; the
/// second value counts how many were left out.
fn whole_targets(spec: &Spec) -> (Vec<PathBuf>, usize) {
    let mut v = vec![];
    let mut links = 0;
    for root in &spec.paths {
        if is_link(root) {
            links += 1;
        } else if spec.trash_self || !root.is_dir() {
            v.push(root.clone());
        } else if let Ok(rd) = std::fs::read_dir(root) {
            // Finder's .DS_Store is not the tool's data, and trashing through
            // Finder writes a new one here anyway (see cleaning.rs).
            for e in rd.flatten().filter(|e| e.file_name() != ".DS_Store") {
                let p = e.path();
                if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                    links += 1;
                } else {
                    v.push(p);
                }
            }
        }
    }
    (v, links)
}

/// Files an age selection would move right now.
fn aged_targets(spec: &Spec, days: u64) -> Vec<PathBuf> {
    let min = Duration::from_secs(days.max(1).saturating_mul(86_400)).max(MIN_AGE);
    let now = SystemTime::now();
    let mut v = vec![];
    for root in &spec.paths {
        for entry in crate::walk::walk(root).skip_hidden(false).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !selectable(spec.skip_dir, root, &path) {
                continue;
            }
            let old_enough = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age >= min);
            if old_enough {
                v.push(path);
            }
        }
    }
    v
}

fn plural_links(n: usize) -> String {
    if n == 1 { "1 link".to_string() } else { format!("{n} links") }
}

fn remove_with(
    requests: &[AiRemoveRequest],
    specs: &[Spec],
    facts: &Facts,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, usize, &str, usize, usize),
) -> AiRemoveOutcome {
    let mut out = AiRemoveOutcome::default();
    let mut seen: HashSet<String> = HashSet::new();
    let requests: Vec<&AiRemoveRequest> = requests.iter().filter(|r| seen.insert(path_key(Path::new(&r.path)))).collect();
    let total = requests.len();

    for (done, req) in requests.iter().enumerate() {
        if cancelled() {
            out.cancelled = true;
            break;
        }
        let key = path_key(Path::new(&req.path));
        let Some(spec) = specs.iter().find(|s| path_key(&s.paths[0]) == key) else {
            // Only locations Jharu itself lists can be removed; anything
            // else a request names is refused.
            if Path::new(&req.path).exists() {
                out.skipped.push(format!("{}: Jharu does not list this location, so it is not removed", req.path));
            } else {
                out.already_gone.push(req.path.clone());
            }
            continue;
        };
        let label = spec.label.clone();
        progress(done, total, &label, 0, 0);
        let Some(def) = tool_def(spec.tool) else { continue };
        if !spec.removable {
            out.skipped.push(format!("{label}: Jharu only lists this one; it does not remove it"));
            continue;
        }

        // State can change after the scan; rate it again right now.
        let (now, reasons) = rate(def, spec, facts);
        if now > req.expected.safety() {
            let why: Vec<&str> = reasons.iter().filter(|r| r.level != Level::Ok).map(|r| r.text.as_str()).collect();
            out.skipped.push(format!("{label}: changed since the scan: {}", why.join(" ")));
            continue;
        }
        // A project's storage goes only while its folder is confirmed gone.
        if spec.class == Class::Workspace && !matches!(spec.project, Some(ProjectInfo { state: Project::Gone, .. })) {
            out.skipped.push(format!("{label}: changed since the scan: its project folder exists or cannot be checked, so this storage is kept"));
            continue;
        }
        let running = running_names(def, facts);
        let mut links = 0;
        let targets = if spec.aged {
            let Some(days) = req.older_than_days.filter(|d| AGE_CUTOFFS.contains(d)) else {
                out.skipped.push(format!("{label}: choose how old the sessions must be first"));
                continue;
            };
            aged_targets(spec, days)
        } else {
            let inside: Vec<String> = spec.paths.iter().flat_map(|p| facts.running_from(p)).collect();
            if !running.is_empty() || !inside.is_empty() {
                let who = if running.is_empty() { inside } else { running };
                out.skipped.push(format!("{label}: {} is running and may be using it. Close it and try again.", who.join(", ")));
                continue;
            }
            let (t, l) = whole_targets(spec);
            links = l;
            t
        };
        if links > 0 {
            out.skipped.push(format!(
                "{label}: {} to another folder left in place; Jharu does not follow links out of a tool's folder",
                plural_links(links)
            ));
        }
        if targets.is_empty() {
            if links == 0 {
                out.already_gone.push(spec.id());
            }
            continue;
        }

        let r = trash_entries(&targets, cancelled, &mut |d, t| progress(done, total, &label, d, t));
        out.freed_bytes += r.freed;
        out.moved += r.moved;
        if r.moved > 0 {
            out.removed.push(spec.id());
        }
        if !r.left.is_empty() {
            let names: Vec<&str> = r.left.iter().take(NAME_LIMIT).map(String::as_str).collect();
            out.skipped.push(format!(
                "{label}: {} left in place because a program has {} open ({}{}). Close what may be using them and try again.",
                if r.left.len() == 1 { "1 entry was".to_string() } else { format!("{} entries were", r.left.len()) },
                if r.left.len() == 1 { "it" } else { "them" },
                names.join(", "),
                if r.left.len() > NAME_LIMIT { ", ..." } else { "" }
            ));
        }
        if r.cancelled {
            out.cancelled = true;
            break;
        }
    }
    progress(total, total, "", 0, 0);
    out
}

/// Moves the chosen locations to the Recycle Bin / Trash. Each is rated
/// again first; anything that got worse, or that a running tool may be
/// using, stays where it is and the outcome says why.
#[tauri::command]
pub async fn remove_ai_leftovers(
    app: tauri::AppHandle,
    items: Vec<AiRemoveRequest>,
    token: String,
) -> Result<AiRemoveOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(d) = Dirs::real() else { return AiRemoveOutcome::default() };
        let specs = build_specs(&d);
        let facts = Facts::gather();
        let t = token.clone();
        let is_cancelled = move || cancelled_tokens().lock().map(|s| s.contains(&t)).unwrap_or(false);
        let out = remove_with(&items, &specs, &facts, &is_cancelled, &mut |done, total, current, step_done, step_total| {
            let _ = app.emit(
                "ai-tools-progress",
                AiProgress { token: token.clone(), done, total, current: current.to_string(), step_done, step_total },
            );
        });
        if let Ok(mut s) = cancelled_tokens().lock() {
            s.remove(&token);
        }
        out
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn age(path: &Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 86_400);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    fn file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn old(path: &Path, bytes: usize, days: u64) {
        file(path, bytes);
        age(path, days);
    }

    fn idle() -> Facts {
        Facts::default()
    }

    fn running(names: &[&str]) -> Facts {
        Facts { running: names.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    /// A throwaway home with a folder for every tool. The path has spaces.
    struct Fixture {
        root: PathBuf,
        d: Dirs,
        projects: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!("jharu ai {tag} {}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let d = Dirs {
                home: root.join("home dir"),
                config: root.join("config dir"),
                local: root.join("local dir"),
                cache: root.join("cache dir"),
            };
            std::fs::create_dir_all(&d.home).unwrap();
            let projects = root.join("projects");
            std::fs::create_dir_all(projects.join("live app")).unwrap();
            let f = Fixture { root, d, projects };
            f.build();
            f
        }

        fn claude_desktop_base(&self) -> PathBuf {
            #[cfg(target_os = "windows")]
            return self.d.local.join("Claude-Data").join("claude_test");
            #[cfg(not(target_os = "windows"))]
            return self.d.config.join("Claude");
        }

        fn build(&self) {
            let d = &self.d;
            // Claude Code
            let c = d.home.join(".claude");
            old(&c.join("projects/proj-a/old session.jsonl"), 4000, 200);
            old(&c.join("projects/proj-a/mid session.jsonl"), 3000, 45);
            file(&c.join("projects/proj-a/new session.jsonl"), 2000);
            old(&c.join("projects/proj-a/memory/MEMORY.md"), 500, 300);
            old(&c.join("projects/proj-a/abc-uuid/subagents/agent.jsonl"), 1500, 120);
            old(&c.join("file-history/hist.bin"), 800, 400);
            file(&c.join("shell-snapshots/snapshot.sh"), 700);
            file(&c.join("statsig/cache.json"), 300);
            file(&c.join("ide/1234.lock"), 50);
            file(&c.join("plugins/p/plugin.json"), 400);
            file(&c.join("settings.json"), 100);
            // Claude desktop app
            let cd = self.claude_desktop_base();
            file(&cd.join("Cache/Cache_Data/f_000001"), 5000);
            file(&cd.join("GPUCache/data_0"), 1000);
            file(&cd.join("Code Cache/js/index"), 1000);
            file(&cd.join("logs/main.log"), 900);
            file(&cd.join("claude-code/2.1.0/claude.exe"), 6000);
            file(&cd.join("Local Storage/leveldb/000003.log"), 100);
            file(&cd.join("config.json"), 100);
            // Codex
            let x = d.home.join(".codex");
            old(&x.join("sessions/2025/01/01/rollout-a.jsonl"), 5000, 150);
            file(&x.join("sessions/2026/05/05/rollout-b.jsonl"), 1000);
            file(&x.join("log/codex-login.log"), 100);
            file(&x.join("generated_images/s1/img.png"), 2500);
            file(&x.join("worktrees/w1/src/main.rs"), 900);
            file(&x.join("config.toml"), 100);
            // Cursor
            let cu = d.config.join("Cursor");
            file(&cu.join("Cache/Cache_Data/data_1"), 4000);
            file(&cu.join("GPUCache/data_0"), 1000);
            file(&cu.join("CachedData/abc/main.js"), 2000);
            file(&cu.join("logs/20260101/main.log"), 800);
            file(&cu.join("User/globalStorage/state.vscdb"), 3000);
            let ws = |hash: &str, uri: Option<String>| {
                let dir = cu.join("User/workspaceStorage").join(hash);
                file(&dir.join("state.vscdb"), 1200);
                file(&dir.join("chatSessions/s1.json"), 300);
                if let Some(u) = uri {
                    std::fs::write(dir.join("workspace.json"), format!("{{\"folder\": \"{u}\"}}")).unwrap();
                }
            };
            ws("gone1", Some(path_uri(&self.projects.join("deleted app"))));
            ws("gone2", Some(path_uri(&self.projects.join("another deleted app"))));
            ws("live1", Some(path_uri(&self.projects.join("live app"))));
            ws("remote1", Some("vscode-remote://ssh-remote%2Bbox/home/me/app".to_string()));
            ws("nojson", None);
            file(&d.home.join(".cursor/extensions/ext-1.0/package.json"), 600);
            // VS Code with Copilot
            let code = d.config.join("Code");
            file(&code.join("CachedExtensionVSIXs/ms-python.python-1.vsix"), 7000);
            file(&code.join("CachedData/x/main.js"), 1500);
            file(&code.join("User/globalStorage/github.copilot-chat/embeddings.db"), 2200);
            file(&code.join("User/globalStorage/ms-python.python/state.json"), 200);
            file(&d.home.join(".vscode/extensions/github.copilot-chat-0.1.0/package.json"), 900);
            file(&d.home.join(".vscode/extensions/ms-python.python-1/package.json"), 900);
            // Continue
            let co = d.home.join(".continue");
            old(&co.join("sessions/s1.json"), 1000, 100);
            file(&co.join("index/lance/data"), 2000);
            file(&co.join("config.yaml"), 100);
            // Aider, in a project under home
            let p = d.home.join("code/my project");
            file(&p.join(".aider.chat.history.md"), 1500);
            file(&p.join(".aider.tags.cache.v4/cache.db"), 700);
            file(&p.join(".aider.conf.yml"), 50);
        }

        fn aider_roots(&self) -> Vec<(PathBuf, usize, Vec<String>)> {
            vec![(self.d.home.clone(), 5, vec![])]
        }

        fn scan(&self, facts: &Facts) -> AiReport {
            scan_with(&self.d, facts, &self.aider_roots())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The `file:///d%3A/projects/app` form editors write.
    fn path_uri(p: &Path) -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        let mut out = String::from("file://");
        if !s.starts_with('/') {
            out.push('/');
        }
        for ch in s.chars() {
            match ch {
                ' ' => out.push_str("%20"),
                ':' => out.push_str("%3A"),
                c => out.push(c),
            }
        }
        out
    }

    fn find<'a>(r: &'a AiReport, tool: &str, label_part: &str) -> &'a AiItem {
        r.tools
            .iter()
            .find(|t| t.id == tool)
            .unwrap_or_else(|| panic!("tool {tool} missing"))
            .items
            .iter()
            .find(|i| i.label.contains(label_part))
            .unwrap_or_else(|| panic!("{tool}: no item with {label_part:?}"))
    }

    #[test]
    fn rates_every_class_honestly_when_nothing_runs() {
        let f = Fixture::new("rate");
        let r = f.scan(&idle());
        for t in &r.tools {
            for i in &t.items {
                println!("{:<18} {:<8?} {:<10?} pre={:<5} rm={:<5} {:>6} B  {}", t.id, i.safety, i.class, i.preselect, i.removable, i.size_bytes, i.label);
            }
        }

        // Caches and logs: Safe, preselected, with an "idle" reason.
        for (tool, part) in [
            ("claude_code", "Shell snapshots"),
            ("claude_code", "Feature-flag"),
            ("claude_desktop", "web cache"),
            ("claude_desktop", "logs"),
            ("codex", "Logs"),
            ("cursor", "web cache"),
            ("cursor", "editor code cache"),
            ("copilot", "downloaded extension installers"),
            ("continue", "Code index"),
        ] {
            let i = find(&r, tool, part);
            assert_eq!(i.safety, Safety::Safe, "{tool} {part}");
            assert!(i.preselect, "{tool} {part}");
            assert!(i.reasons.iter().any(|x| x.level == Level::Ok && x.text.contains("is not running")), "{tool} {part}");
        }

        // History, state, extensions: Risky, never preselected, with the reason.
        for (tool, part) in [
            ("claude_code", "Session transcripts"),
            ("claude_code", "File history"),
            ("claude_code", "Plugins"),
            ("codex", "Session transcripts"),
            ("cursor", "Global state"),
            ("cursor", "Extensions"),
            ("copilot", "github.copilot-chat storage"),
            ("copilot", "Extension github.copilot-chat-0.1.0"),
            ("continue", "Chat sessions"),
        ] {
            let i = find(&r, tool, part);
            assert_eq!(i.safety, Safety::Risky, "{tool} {part}");
            assert!(!i.preselect, "{tool} {part}");
            assert!(i.reasons.iter().any(|x| x.level == Level::Bad), "{tool} {part}");
        }
        assert!(find(&r, "claude_code", "Session transcripts").reasons[0].text.contains("cannot be re-created"));

        // Downloads and scratch output: Caution.
        assert_eq!(find(&r, "claude_desktop", "downloaded by the app").safety, Safety::Caution);
        assert_eq!(find(&r, "codex", "Generated images").safety, Safety::Caution);
        // The IDE files carry their own warning.
        let ide = find(&r, "claude_code", "IDE connection");
        assert_eq!(ide.safety, Safety::Caution);
        assert!(!ide.preselect);

        // Nothing the tools did not create, and no unknown folder, is listed.
        for t in &r.tools {
            for i in &t.items {
                assert!(!i.paths.iter().any(|p| p.ends_with("settings.json") || p.ends_with("config.toml") || p.contains("Local Storage")), "{}", i.label);
            }
        }
        // VS Code's own non-Copilot extension is not listed.
        assert!(!r.tools.iter().flat_map(|t| &t.items).any(|i| i.label.contains("ms-python")));
        assert!(r.history_bytes > 0 && r.safe_bytes > 0 && r.total_bytes > r.safe_bytes + r.history_bytes);
    }

    #[test]
    fn running_tool_lowers_caches_to_caution_and_names_it() {
        let f = Fixture::new("running");
        let r = f.scan(&running(&["cursor", "code"]));
        let cache = find(&r, "cursor", "web cache");
        assert_eq!(cache.safety, Safety::Caution);
        assert!(!cache.preselect);
        assert!(cache.tool_running);
        assert!(cache.reasons.iter().any(|x| x.level == Level::Warn && x.text.contains("cursor is running")));
        assert_eq!(find(&r, "copilot", "downloaded extension installers").safety, Safety::Caution);
        assert_eq!(r.tools.iter().find(|t| t.id == "cursor").unwrap().running, vec!["cursor".to_string()]);
        // Another tool that is not running is unaffected.
        assert_eq!(find(&r, "codex", "Logs").safety, Safety::Safe);
        // History stays Risky whether or not the tool runs.
        assert_eq!(find(&r, "cursor", "Global state").safety, Safety::Risky);
    }

    #[test]
    fn workspaces_of_deleted_projects_are_caution_and_the_rest_is_folded() {
        let f = Fixture::new("workspaces");
        let r = f.scan(&idle());
        let cursor = r.tools.iter().find(|t| t.id == "cursor").unwrap();
        let gone: Vec<&AiItem> = cursor.items.iter().filter(|i| i.class == Class::Workspace && i.removable).collect();
        assert_eq!(gone.len(), 2, "{:?}", gone.iter().map(|i| &i.label).collect::<Vec<_>>());
        for g in &gone {
            assert_eq!(g.safety, Safety::Caution);
            assert!(!g.preselect);
            assert!(g.reasons.iter().any(|x| x.text.contains("project folder is gone")), "{:?}", g.reasons);
            assert!(g.label.contains("deleted app"));
        }
        // Projects that exist, a remote one and one with no workspace.json
        // are one listed-only row.
        let fold: Vec<&AiItem> = cursor.items.iter().filter(|i| i.class == Class::Workspace && !i.removable).collect();
        assert_eq!(fold.len(), 1);
        assert!(fold[0].label.contains("3 projects that still exist"), "{}", fold[0].label);
        assert_eq!(fold[0].safety, Safety::Risky);
        assert!(fold[0].reasons.iter().any(|x| x.text.contains("cannot see")));
        let live_bytes: u64 = 3 * (1200 + 300) + 0;
        assert!(fold[0].size_bytes >= live_bytes - 3 * 60, "{}", fold[0].size_bytes);
    }

    #[test]
    fn file_uris_and_project_state() {
        #[cfg(target_os = "windows")]
        {
            assert_eq!(file_uri_to_path("file:///d%3A/projects/media%20grabber"), Some(PathBuf::from(r"d:\projects\media grabber")));
            assert_eq!(file_uri_to_path("file:///e%3A/Research%20Paper%20hub/HVDM"), Some(PathBuf::from(r"e:\Research Paper hub\HVDM")));
        }
        #[cfg(not(target_os = "windows"))]
        assert_eq!(file_uri_to_path("file:///Users/me/my%20app"), Some(PathBuf::from("/Users/me/my app")));
        assert_eq!(file_uri_to_path("vscode-remote://ssh-remote%2Bbox/home/me"), None);
        assert_eq!(file_uri_to_path("file://server/share/app"), None);
        assert_eq!(file_uri_to_path("not a uri"), None);

        let root = std::env::temp_dir().join(format!("jharu ai state {}", std::process::id()));
        std::fs::create_dir_all(root.join("here")).unwrap();
        assert_eq!(project_state(&root.join("here")), Project::Exists);
        assert_eq!(project_state(&root.join("deleted")), Project::Gone);
        let _ = std::fs::remove_dir_all(&root);

        // A drive that is not connected is Unknown, never Gone.
        #[cfg(target_os = "windows")]
        if let Some(letter) = ('D'..='Z').rev().find(|l| !Path::new(&format!("{l}:\\")).exists()) {
            assert_eq!(project_state(Path::new(&format!("{letter}:\\projects\\app"))), Project::Unknown);
        }
        #[cfg(target_os = "macos")]
        assert_eq!(project_state(Path::new("/Volumes/jharu-no-such-disk/app")), Project::Unknown);
    }

    #[test]
    fn age_buckets_leave_memory_alone() {
        let f = Fixture::new("ages");
        let r = f.scan(&idle());
        let t = find(&r, "claude_code", "Session transcripts");
        assert!(t.removable && t.safety == Safety::Risky);
        let bucket = |days: u64| t.ages.iter().find(|b| b.days == days).unwrap();
        // old (200 d, 4000 B), mid (45 d, 3000 B), subagent (120 d, 1500 B); memory (300 d) never counts.
        assert_eq!((bucket(7).bytes, bucket(7).files), (8500, 3));
        assert_eq!((bucket(30).bytes, bucket(30).files), (8500, 3));
        assert_eq!((bucket(90).bytes, bucket(90).files), (5500, 2));
        assert_eq!((bucket(180).bytes, bucket(180).files), (4000, 1));
        assert_eq!((bucket(365).bytes, bucket(365).files), (0, 0));
        // Items without age selection offer none.
        assert!(find(&r, "claude_code", "Shell snapshots").ages.is_empty());
        // Codex sessions are found by date folder.
        assert_eq!(find(&r, "codex", "Session transcripts").ages.iter().find(|b| b.days == 90).unwrap().files, 1);
    }

    #[test]
    fn aider_files_are_listed_but_never_removable() {
        let f = Fixture::new("aider");
        let r = f.scan(&idle());
        let aider = r.tools.iter().find(|t| t.id == "aider").expect("aider listed");
        assert_eq!(aider.items.len(), 2, "the config file is not a leftover: {:?}", aider.items.iter().map(|i| &i.label).collect::<Vec<_>>());
        let hist = find(&r, "aider", "chat.history");
        assert_eq!((hist.safety, hist.removable, hist.preselect), (Safety::Risky, false, false));
        assert!(hist.label.starts_with("my project/"));
        let tags = find(&r, "aider", "tags.cache");
        assert_eq!((tags.safety, tags.removable, tags.preselect), (Safety::Safe, false, false));

        // A request for one is refused.
        let specs = {
            let mut s = build_specs(&f.d);
            s.extend(aider_specs(&f.aider_roots()));
            s
        };
        let req = AiRemoveRequest { path: tags.id.clone(), older_than_days: None, expected: Expected::Safe };
        let out = remove_with(&[req], &specs, &idle(), &|| false, &mut |_, _, _, _, _| {});
        assert!(out.skipped.iter().any(|s| s.contains("only lists this one")), "{:?}", out.skipped);
        assert!(f.d.home.join("code/my project/.aider.tags.cache.v4").exists());
    }

    #[test]
    fn worktrees_are_pointed_to_not_duplicated() {
        let f = Fixture::new("links");
        let r = f.scan(&idle());
        let codex = r.tools.iter().find(|t| t.id == "codex").unwrap();
        assert!(codex.worktrees_link);
        let link = find(&r, "codex", "Spare git checkouts");
        assert_eq!((link.removable, link.link.as_deref()), (false, Some("worktrees")));
        // Its size is not part of the totals: the other tab owns it.
        let counted: u64 = codex.items.iter().filter(|i| i.class != Class::Link).map(|i| i.size_bytes).sum();
        assert_eq!(codex.total_bytes, counted);
    }

    /// Items of the real Recycle Bin that came from `root`, to put back so
    /// the test leaves the user's bin as it found it.
    #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
    fn restore_from_bin(root: &Path) -> usize {
        let key = path_key(root);
        let ours: Vec<trash::TrashItem> = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|t| {
                let parent = path_key(&t.original_parent);
                parent == key || parent.starts_with(&format!("{key}/"))
            })
            .collect();
        let n = ours.len();
        trash::os_limited::restore_all(ours).unwrap();
        n
    }

    /// Puts back whatever a test moved to the real bin, even when an
    /// assertion fails first. Declare it after the fixture so it drops first.
    /// On macOS tests never reach the real Trash (trashlog::to_bin).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    struct BinGuard(PathBuf);

    impl Drop for BinGuard {
        fn drop(&mut self) {
            #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
            {
                if let Ok(items) = trash::os_limited::list() {
                    let key = path_key(&self.0);
                    let ours: Vec<trash::TrashItem> = items
                        .into_iter()
                        .filter(|t| {
                            let parent = path_key(&t.original_parent);
                            parent == key || parent.starts_with(&format!("{key}/"))
                        })
                        .collect();
                    let _ = trash::os_limited::restore_all(ours);
                }
            }
        }
    }

    #[test]
    fn removal_rechecks_skips_and_moves_to_the_bin() {
        let f = Fixture::new("remove");
        let _bin = BinGuard(f.root.clone());
        let report = f.scan(&idle());
        let specs = build_specs(&f.d);
        let req = |tool: &str, part: &str, days: Option<u64>| {
            let i = find(&report, tool, part);
            AiRemoveRequest {
                path: i.id.clone(),
                older_than_days: days,
                expected: match i.safety {
                    Safety::Safe => Expected::Safe,
                    Safety::Caution => Expected::Caution,
                    Safety::Risky => Expected::Risky,
                },
            }
        };
        let cursor_cache = req("cursor", "web cache", None);
        let requests = vec![
            cursor_cache.clone(),
            req("claude_code", "Shell snapshots", None),
            req("claude_code", "Session transcripts", Some(90)),
            req("claude_code", "File history", None), // aged item without a cutoff
            req("cursor", "storage for deleted app", None),
            req("codex", "Logs", None),
            // Not something Jharu lists, as if a request were tampered with.
            AiRemoveRequest { path: f.d.home.join(".claude/settings.json").to_string_lossy().into_owned(), older_than_days: None, expected: Expected::Safe },
        ];

        // After the scan: Codex starts running, so its logs must stay.
        let facts = running(&["codex"]);
        let mut steps = vec![];
        let out = remove_with(&requests, &specs, &facts, &|| false, &mut |d, t, c, sd, st| steps.push((d, t, c.to_string(), sd, st)));
        println!("removed {:?}\nskipped {:?}\nerrors {:?}\ngone {:?}\nfreed {}", out.removed, out.skipped, out.errors, out.already_gone, out.freed_bytes);

        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(!out.cancelled);
        assert_eq!(steps.last().map(|s| (s.0, s.1)), Some((requests.len(), requests.len())));

        // Caches: contents moved, folder kept.
        let cu = f.d.config.join("Cursor");
        assert!(cu.join("Cache").is_dir() && std::fs::read_dir(cu.join("Cache")).unwrap().next().is_none());
        assert!(f.d.home.join(".claude/shell-snapshots").is_dir());
        assert!(!f.d.home.join(".claude/shell-snapshots/snapshot.sh").exists());
        assert!(cu.join("GPUCache/data_0").exists(), "only the requested items go");

        // Transcripts older than 90 days only: 200 d and 120 d; 45 d, new and memory stay.
        let c = f.d.home.join(".claude/projects/proj-a");
        assert!(!c.join("old session.jsonl").exists());
        assert!(!c.join("abc-uuid/subagents/agent.jsonl").exists());
        assert!(c.join("mid session.jsonl").exists());
        assert!(c.join("new session.jsonl").exists());
        assert!(c.join("memory/MEMORY.md").exists(), "project memory is never touched");

        // A deleted project's storage goes whole; the others stay.
        assert!(!cu.join("User/workspaceStorage/gone1").exists());
        assert!(cu.join("User/workspaceStorage/gone2").exists());
        assert!(cu.join("User/workspaceStorage/live1/state.vscdb").exists());

        assert!(f.d.home.join(".claude/file-history/hist.bin").exists());
        assert!(out.skipped.iter().any(|s| s.starts_with("File history: choose how old")), "{:?}", out.skipped);
        assert!(f.d.home.join(".codex/log/codex-login.log").exists());
        assert!(out.skipped.iter().any(|s| s.starts_with("Logs: changed since the scan") && s.contains("codex is running")), "{:?}", out.skipped);
        assert!(out.skipped.iter().any(|s| s.contains("settings.json") && s.contains("does not list")), "{:?}", out.skipped);
        assert!(f.d.home.join(".claude/settings.json").exists());
        assert_eq!(out.removed.len(), 4);
        assert!(out.freed_bytes >= 4000 + 700 + 5500 + 1500, "freed {}", out.freed_bytes);

        // The same cache request again: nothing left to move.
        let again = remove_with(&[cursor_cache], &specs, &idle(), &|| false, &mut |_, _, _, _, _| {});
        assert!(again.removed.is_empty() && again.already_gone.len() == 1, "{again:?}");

        // Whole-folder requests stay put while the tool runs, even when the
        // request itself was fine at scan time.
        let during = remove_with(&[req("cursor", "compiled code and graphics", None)], &specs, &running(&["cursor"]), &|| false, &mut |_, _, _, _, _| {});
        assert!(during.removed.is_empty() && during.skipped.len() == 1, "{during:?}");
        assert!(cu.join("GPUCache/data_0").exists());

        // The files went to the Recycle Bin / Trash, not into oblivion.
        // Put them back so the test leaves the user's bin as it found it.
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        {
            let n = restore_from_bin(&f.root);
            assert!(n >= 4, "expected moved entries in the bin, found {n}");
            assert!(f.d.home.join(".claude/projects/proj-a/old session.jsonl").exists());
        }
    }

    #[test]
    fn a_running_tool_does_not_block_old_sessions_but_changed_state_does() {
        let f = Fixture::new("aged running");
        let _bin = BinGuard(f.root.clone());
        let report = f.scan(&idle());
        let specs = build_specs(&f.d);
        let t = find(&report, "claude_code", "Session transcripts");
        let req = AiRemoveRequest { path: t.id.clone(), older_than_days: Some(180), expected: Expected::Risky };
        let out = remove_with(&[req], &specs, &running(&["claude"]), &|| false, &mut |_, _, _, _, _| {});
        assert_eq!(out.removed.len(), 1, "{out:?}");
        assert!(!f.d.home.join(".claude/projects/proj-a/old session.jsonl").exists());
        assert!(f.d.home.join(".claude/projects/proj-a/new session.jsonl").exists(), "today's session is never touched");

        // A workspace whose project folder comes back is no longer Caution.
        let gone = find(&report, "cursor", "storage for another deleted app");
        std::fs::create_dir_all(f.projects.join("another deleted app")).unwrap();
        let specs = build_specs(&f.d);
        let req = AiRemoveRequest { path: gone.id.clone(), older_than_days: None, expected: Expected::Caution };
        let out = remove_with(&[req], &specs, &idle(), &|| false, &mut |_, _, _, _, _| {});
        assert!(out.removed.is_empty(), "{out:?}");
        assert!(out.skipped.iter().any(|s| s.contains("changed since the scan")), "{:?}", out.skipped);
        assert!(f.d.config.join("Cursor/User/workspaceStorage/gone2").exists());

        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        restore_from_bin(&f.root);
    }

    #[test]
    fn cancelling_stops_after_the_current_batch() {
        let f = Fixture::new("cancel");
        let _bin = BinGuard(f.root.clone());
        // 120 entries make three batches; the third is never started.
        for i in 0..(BATCH * 2 + 20) {
            file(&f.d.home.join(format!(".continue/logs/entry {i}.log")), 10);
        }
        let report = f.scan(&idle());
        let specs = build_specs(&f.d);
        let logs = find(&report, "continue", "Logs and usage data");
        let req = AiRemoveRequest { path: logs.id.clone(), older_than_days: None, expected: Expected::Safe };
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            calls.set(calls.get() + 1);
            // Called before the item, before batch 1, 2 and 3.
            calls.get() >= 4
        };
        let out = remove_with(&[req], &specs, &idle(), &cancelled, &mut |_, _, _, _, _| {});
        assert!(out.cancelled);
        assert_eq!(out.moved as usize, BATCH * 2);
        let left = std::fs::read_dir(f.d.home.join(".continue/logs")).unwrap().count();
        assert_eq!(left, 20);

        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        restore_from_bin(&f.root);
    }

    /// A folder link, made with whatever the platform allows without
    /// elevation (a junction on Windows). None when it cannot be made.
    fn make_dir_link(link: &Path, target: &Path) -> Option<()> {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let ok = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .creation_flags(0x0800_0000)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            return ok.then_some(());
        }
        #[cfg(not(target_os = "windows"))]
        std::os::unix::fs::symlink(target, link).ok()
    }

    #[test]
    fn links_out_of_a_tool_folder_are_never_followed() {
        let f = Fixture::new("links out");
        let _bin = BinGuard(f.root.clone());
        let outside = f.root.join("somewhere else");
        file(&outside.join("precious.txt"), 100);

        // A link inside a cache folder is left in place, not moved or followed.
        let cache = f.d.config.join("Cursor/Cache");
        if make_dir_link(&cache.join("link to elsewhere"), &outside).is_none() {
            // Windows junctions need no privileges, so a failure there is a bug.
            assert!(!cfg!(target_os = "windows"), "could not create a junction");
            return;
        }
        // A tool folder that is itself a link is not listed.
        let statsig = f.d.home.join(".claude/statsig");
        std::fs::remove_dir_all(&statsig).unwrap();
        make_dir_link(&statsig, &outside).unwrap();

        let report = f.scan(&idle());
        assert!(!report.tools.iter().flat_map(|t| &t.items).any(|i| i.label.contains("Feature-flag")));
        let specs = build_specs(&f.d);
        let i = find(&report, "cursor", "web cache");
        let req = AiRemoveRequest { path: i.id.clone(), older_than_days: None, expected: Expected::Safe };
        let out = remove_with(&[req], &specs, &idle(), &|| false, &mut |_, _, _, _, _| {});
        assert!(out.skipped.iter().any(|s| s.contains("link")), "{:?}", out.skipped);
        assert!(outside.join("precious.txt").exists());
        assert!(cache.join("link to elsewhere").exists(), "the link itself stays too");
        assert!(!cache.join("Cache_Data").exists(), "real entries still go");
    }

    #[test]
    fn requests_cannot_widen_what_a_scan_offered() {
        let f = Fixture::new("tamper");
        let _bin = BinGuard(f.root.clone());
        let report = f.scan(&idle());
        let specs = build_specs(&f.d);
        let live = f.d.config.join("Cursor/User/workspaceStorage/live1").to_string_lossy().into_owned();
        // Storage of a project that still exists, claimed as Risky so the
        // re-rating alone would let it through.
        let out = remove_with(
            &[AiRemoveRequest { path: live, older_than_days: None, expected: Expected::Risky }],
            &specs,
            &idle(),
            &|| false,
            &mut |_, _, _, _, _| {},
        );
        assert!(out.removed.is_empty(), "{out:?}");
        assert!(f.d.config.join("Cursor/User/workspaceStorage/live1/state.vscdb").exists());
        // An age that is not one of the offered choices is refused.
        let t = find(&report, "claude_code", "Session transcripts");
        let out = remove_with(
            &[AiRemoveRequest { path: t.id.clone(), older_than_days: Some(u64::MAX), expected: Expected::Risky }],
            &specs,
            &idle(),
            &|| false,
            &mut |_, _, _, _, _| {},
        );
        assert!(out.removed.is_empty() && out.skipped.iter().any(|s| s.contains("choose how old")), "{out:?}");
        assert!(f.d.home.join(".claude/projects/proj-a/old session.jsonl").exists());
    }

    #[test]
    fn claude_desktop_prefers_the_real_data_folder_over_its_view() {
        let f = Fixture::new("claude base");
        let r = f.scan(&idle());
        let paths: Vec<&String> = r.tools.iter().filter(|t| t.id == "claude_desktop").flat_map(|t| &t.items).flat_map(|i| &i.paths).collect();
        assert!(!paths.is_empty());
        // Local Storage and config.json hold sign-in and settings: never listed.
        assert!(paths.iter().all(|p| !p.contains("Local Storage") && !p.ends_with("config.json")));
        #[cfg(target_os = "windows")]
        assert!(paths.iter().all(|p| p.contains("Claude-Data")), "{paths:?}");
    }

    #[test]
    fn deep_clean_knows_the_ai_tool_home_folders() {
        use crate::places::{verdict, Signals};
        let home = PathBuf::from(if cfg!(windows) { "C:\\Users\\test" } else { "/home/test" });
        let v = |rel: &str| verdict(&home.join(rel), &home, &Signals::default(), &idle());
        for rel in [".claude", ".codex", ".continue"] {
            let r = v(rel);
            assert_eq!((r.safety, r.basis), (Safety::Risky, "known"), "{rel}");
        }
        let codeium = v(".codeium");
        assert_eq!((codeium.safety, codeium.basis), (Safety::Risky, "known"));
        assert!(codeium.reasons[0].text.contains("part by part"));
        assert_eq!(v(".windsurf").safety, Safety::Caution);
    }

    /// Read-only listing of this machine: `cargo test --lib smoke_ai_tools -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_ai_tools() {
        let start = std::time::Instant::now();
        let d = Dirs::real().unwrap();
        let r = scan_with(&d, &Facts::gather(), &aider_roots());
        println!(
            "scanned in {:?}: {:.2} GB total, {:.2} GB safe to clear, {:.2} GB history and settings",
            start.elapsed(),
            r.total_bytes as f64 / 1e9,
            r.safe_bytes as f64 / 1e9,
            r.history_bytes as f64 / 1e9
        );
        for t in &r.tools {
            println!("{} ({:.2} GB, running: {:?})", t.name, t.total_bytes as f64 / 1e9, t.running);
            for i in &t.items {
                let aged = i.ages.iter().find(|b| b.days == 90).map(|b| format!(" [>90d {:.1} MB]", b.bytes as f64 / 1e6)).unwrap_or_default();
                println!(
                    "  {:>9.1} MB  {:<8?} {:<10?} pre={:<5} {}{}  {}",
                    i.size_bytes as f64 / 1e6,
                    i.safety,
                    i.class,
                    i.preselect,
                    i.label,
                    aged,
                    i.path
                );
            }
        }
    }
}
