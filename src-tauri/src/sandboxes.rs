//! Hidden app sandboxes: the private folders that packaged apps keep.
//!
//! On Windows an app installed from the Microsoft Store (or any MSIX package)
//! does not write to your real AppData. Windows gives it a private, virtual
//! copy under `%LOCALAPPDATA%\Packages\<PackageFamilyName>\`: `LocalCache`
//! holds what the app believes is its `AppData\Local` and `AppData\Roaming`,
//! `TempState` and `AC\Temp` its scratch files, `LocalState` and `Settings`
//! its own data. Explorer's Storage settings and most cleaners never look
//! there. On one test laptop a single package (Claude) held 86 GB inside
//! `LocalCache`: uv and pip caches, Rust `target` folders, screen recordings.
//! macOS has the same idea in `~/Library/Containers/<bundle id>/Data` and
//! `~/Library/Group Containers` (untested here, written to the same rules).
//!
//! For each sandbox this lists the app (friendly name, installed or left
//! behind, running or not), its real size, and a breakdown by area, and
//! rates every area the way the other tabs do:
//!
//! - Temp areas (`TempState`, `AC\Temp`, `AC\INetCache`, macOS `Caches`):
//!   Safe, but only while the app is not running, and only with evidence
//!   that it is not: its executable is not a process and nothing runs from
//!   its install folder. If Jharu cannot tell which program belongs to the
//!   package, it says so and does not call the folder safe.
//! - The app's own data (`LocalState`, `RoamingState`, `Settings`): Risky.
//! - `LocalCache` is opened up. Jharu maps Dev clean's rules onto it (uv's
//!   cache at `%LOCALAPPDATA%\uv\cache` is `LocalCache\Local\uv\cache`
//!   inside the sandbox) and rates the rest with Deep clean's verdicts, so a
//!   cache found here is rated exactly like the real one.
//! - Packages whose app is no longer installed are Caution, with the reason.
//!
//! Removal moves only the rated folders (or, for areas the app expects to
//! exist, what is inside them) to the Recycle Bin / Trash, checks each one
//! again right before, skips what is in use and says why, reports progress,
//! and stops when asked.

// The Windows parsers are compiled and tested everywhere, but only called
// on Windows.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::deep;
use crate::places;
use crate::rules::{all_rules, Safety};
use crate::safety::{assess, Facts, Level, Reason};
use crate::usage::{self, Seen};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri::Emitter;

#[cfg(target_os = "windows")]
const BIN: &str = "Recycle Bin";
#[cfg(not(target_os = "windows"))]
const BIN: &str = "Trash";

/// How many names of entries left in place to quote back.
const NAME_LIMIT: usize = 5;

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(target_os = "windows", allow(dead_code))]
enum Layout {
    /// `%LOCALAPPDATA%\Packages\<family>`: Windows MSIX / Store apps.
    Msix,
    /// `~/Library/Containers/<bundle id>`: a sandboxed Mac app.
    Container,
    /// `~/Library/Group Containers/<group id>`: shared by a developer's apps.
    GroupContainer,
}

impl Layout {
    fn kind(self) -> &'static str {
        match self {
            Layout::Msix => "msix",
            Layout::Container => "container",
            Layout::GroupContainer => "group_container",
        }
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Installed,
    /// The folder is still there but the app is not installed.
    Orphaned,
    /// Jharu could not list the installed apps, so it does not say.
    Unknown,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Move the folder itself.
    Folder,
    /// Move what is inside and keep the folder, which the app expects.
    Contents,
}

struct Package {
    /// Package family name or bundle id: the folder's name.
    id: String,
    name: String,
    root: PathBuf,
    layout: Layout,
    state: State,
    install: Option<PathBuf>,
    /// Every install folder of the app (see `MsixApp::roots`).
    installs: Vec<PathBuf>,
    version: Option<String>,
    /// Process names (lowercase, no extension) of the app.
    exes: Vec<String>,
    /// A Windows component rather than an app you installed.
    system: bool,
}

/// What Jharu knows about the installed apps.
#[cfg_attr(target_os = "windows", allow(dead_code))]
enum Inventory {
    Msix(HashMap<String, MsixApp>),
    Mac(Vec<MacApp>),
    Unavailable,
}

#[derive(Clone, Debug)]
struct MsixApp {
    full_name: String,
    /// The install folder of the package that describes the app.
    root: PathBuf,
    /// Every registered version and resource package of the family: any of
    /// them can be the one running.
    roots: Vec<PathBuf>,
    raw_display: String,
    version: Option<String>,
}

#[derive(Clone, Debug)]
struct MacApp {
    bundle_id: String,
    name: String,
    path: PathBuf,
    executable: Option<String>,
    /// The developer team that signed it, which names its group containers.
    team: Option<String>,
}

struct Ctx {
    /// Folders whose children are sandboxes.
    roots: Vec<(PathBuf, Layout)>,
    home: PathBuf,
    /// The real `%LOCALAPPDATA%` and `%APPDATA%`, which `LocalCache` mirrors.
    local_appdata: PathBuf,
    roaming_appdata: PathBuf,
    system_root: PathBuf,
    inventory: Inventory,
}

impl Ctx {
    fn system() -> Option<Ctx> {
        let home = dirs::home_dir()?;
        #[cfg(target_os = "windows")]
        {
            let local = dirs::data_local_dir()?;
            return Some(Ctx {
                roots: vec![(local.join("Packages"), Layout::Msix)],
                home,
                roaming_appdata: dirs::data_dir()?,
                local_appdata: local,
                system_root: PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())),
                inventory: msix_inventory(),
            });
        }
        #[cfg(target_os = "macos")]
        {
            let lib = home.join("Library");
            let app_dirs = [PathBuf::from("/Applications"), home.join("Applications"), PathBuf::from("/System/Applications")];
            return Some(Ctx {
                roots: vec![(lib.join("Containers"), Layout::Container), (lib.join("Group Containers"), Layout::GroupContainer)],
                local_appdata: lib.clone(),
                roaming_appdata: lib,
                home,
                system_root: PathBuf::from("/System"),
                inventory: Inventory::Mac(mac_inventory_from(&app_dirs)),
            });
        }
        #[allow(unreachable_code)]
        {
            let _ = home;
            None
        }
    }
}

#[derive(Serialize, Clone)]
pub struct Entry {
    pub path: String,
    /// Relative to the sandbox folder, with `/`.
    pub rel: String,
    pub label: String,
    /// What this is, in a few words.
    pub what: String,
    pub size_bytes: u64,
    /// What removing it would free; 0 where Jharu does not offer removal.
    pub reclaimable_bytes: u64,
    pub file_count: u64,
    pub safety: Safety,
    /// "layout" (the sandbox's own structure), "known" (a Dev clean rule or a
    /// place Jharu knows), "detected" (from contents) or "unknown".
    pub basis: String,
    pub reasons: Vec<Reason>,
    pub removable: bool,
    pub mode: Mode,
    pub preselect: bool,
    /// Where the app itself sees this folder (inside `LocalCache`).
    pub appears_as: Option<String>,
    /// Parts of an area Jharu opened up (`LocalCache`).
    pub children: Vec<Entry>,
}

#[derive(Serialize)]
pub struct SandboxPackage {
    pub id: String,
    pub name: String,
    pub root: String,
    pub kind: &'static str,
    pub state: State,
    /// Programs of the app that are running right now.
    pub running: Vec<String>,
    pub install_path: Option<String>,
    pub version: Option<String>,
    pub system: bool,
    pub size_bytes: u64,
    /// What the safe, removable parts would free.
    pub safe_bytes: u64,
    /// Set when the package as a whole needs care (left behind by an
    /// uninstalled app, or its app is running).
    pub safety: Option<Safety>,
    pub reasons: Vec<Reason>,
    pub entries: Vec<Entry>,
}

#[derive(Serialize)]
pub struct SandboxReport {
    /// False on systems without app sandboxes (Linux).
    pub supported: bool,
    pub platform: &'static str,
    pub locations: Vec<String>,
    /// False when installed apps could not be listed, so none is called left
    /// behind.
    pub inventory_known: bool,
    pub total_bytes: u64,
    pub safe_bytes: u64,
    pub orphan_bytes: u64,
    /// The scan was stopped; the list holds only what was measured.
    pub cancelled: bool,
    pub packages: Vec<SandboxPackage>,
}

// ---------------------------------------------------------------- paths

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

/// A symbolic link, junction or other reparse point. Never followed: a
/// container's `Documents` is often a link to your real Documents folder.
fn is_link(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Real directories directly inside `dir`, by name.
fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().symlink_metadata().is_ok_and(|m| m.is_dir() && !is_link(&m)))
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    v
}

fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn exe_key(s: &str) -> String {
    let base = s.rsplit(['\\', '/']).next().unwrap_or(s).to_lowercase();
    base.strip_suffix(".exe").map(String::from).unwrap_or(base)
}

/// "Microsoft.WindowsNotepad_8wekyb3d8bbwe" -> "Windows Notepad".
fn pretty_name(id: &str) -> String {
    let id = id.split('_').next().unwrap_or(id);
    let seg = id.rsplit('.').find(|s| !s.is_empty() && !s.chars().all(|c| c.is_ascii_digit())).unwrap_or(id);
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in seg.chars() {
        if let Some(p) = prev {
            if p.is_lowercase() && c.is_uppercase() {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    let mut chars = out.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => id.to_string(),
    }
}

// ---------------------------------------------------------------- Windows: installed packages

/// "Claude_4.4212.0.0_x64__pzs8sxrjxfjjc" -> "Claude_pzs8sxrjxfjjc".
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn family_of(full_name: &str) -> Option<String> {
    let (name, rest) = full_name.split_once('_')?;
    let publisher = rest.rsplit('_').next()?;
    (!name.is_empty() && !publisher.is_empty()).then(|| format!("{name}_{publisher}"))
}

/// A language or scale package that goes with an app: its full name has a
/// resource id between the architecture and the publisher.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_resource_package(full_name: &str) -> bool {
    let parts: Vec<&str> = full_name.split('_').collect();
    parts.len() >= 5 && !parts[3].is_empty()
}

/// "4.10.0.0" as numbers, so 4.10 is newer than 4.9.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn version_of(full_name: &str) -> Vec<u64> {
    full_name.split('_').nth(1).unwrap_or("").split('.').map(|n| n.parse().unwrap_or(0)).collect()
}

/// Whether package `a` describes its app better than `b`: not a resource
/// package, then the newer version.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn describes_better(a: &str, b: &str) -> bool {
    match (is_resource_package(a), is_resource_package(b)) {
        (false, true) => true,
        (true, false) => false,
        _ => version_of(a) > version_of(b),
    }
}

/// The packages registered for this user. Windows records each under
/// `HKCU\Software\Classes\Local Settings\...\AppModel\Repository\Packages`,
/// with its install folder and display name.
#[cfg(target_os = "windows")]
fn msix_inventory() -> Inventory {
    use winreg::enums::*;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
    let Ok(repo) = hkcu.open_subkey_with_flags(path, KEY_READ) else { return Inventory::Unavailable };
    let mut names: Vec<String> = vec![];
    for name in repo.enum_keys() {
        match name {
            Ok(n) => names.push(n),
            // A list with holes could call an installed app left behind.
            Err(_) => return Inventory::Unavailable,
        }
    }
    let mut map: HashMap<String, MsixApp> = HashMap::new();
    for full_name in names {
        let Some(family) = family_of(&full_name) else { continue };
        // A key that cannot be opened still proves the package is installed.
        let key = repo.open_subkey_with_flags(&full_name, KEY_READ).ok();
        let read = |value: &str| key.as_ref().and_then(|k| k.get_value::<String, _>(value).ok()).unwrap_or_default();
        let root = PathBuf::from(read("PackageRootFolder"));
        let app = MsixApp {
            version: full_name.split('_').nth(1).map(String::from),
            roots: if root.as_os_str().is_empty() { vec![] } else { vec![root.clone()] },
            root,
            raw_display: read("DisplayName"),
            full_name,
        };
        match map.entry(family.to_lowercase()) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(app);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let slot = o.get_mut();
                let mut roots = std::mem::take(&mut slot.roots);
                roots.extend(app.roots.iter().cloned());
                // Two versions during an update: the newer one describes the
                // app. Resource packages carry no programs and never do.
                if describes_better(&app.full_name, &slot.full_name) {
                    *slot = app;
                }
                slot.roots = roots;
            }
        }
    }
    // An empty list means the registry could not tell us, not that nothing
    // is installed.
    if map.is_empty() {
        Inventory::Unavailable
    } else {
        Inventory::Msix(map)
    }
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
fn msix_inventory() -> Inventory {
    Inventory::Unavailable
}

/// Resolves a registry display name such as
/// `@{PackageFullName?ms-resource://Package/Resources/AppName}`.
#[cfg(target_os = "windows")]
fn load_indirect(source: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::SHLoadIndirectString;

    let wide: Vec<u16> = std::ffi::OsStr::new(source).encode_wide().chain(Some(0)).collect();
    let mut buf = [0u16; 260];
    let hr = unsafe { SHLoadIndirectString(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, std::ptr::null_mut()) };
    if hr < 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(not(target_os = "windows"))]
fn load_indirect(_source: &str) -> Option<String> {
    None
}

#[derive(Default, Debug, PartialEq)]
struct Manifest {
    display_name: Option<String>,
    /// Process names of the package's applications.
    executables: Vec<String>,
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&needle) {
        let at = from + i;
        // `Executable=`, not `xExecutable=`.
        if tag[..at].chars().next_back().is_some_and(char::is_whitespace) {
            let rest = &tag[at + needle.len()..];
            return rest.find('"').map(|end| &rest[..end]);
        }
        from = at + needle.len();
    }
    None
}

/// The parts of an AppxManifest.xml Jharu needs: the app's name (when the
/// manifest spells it out rather than pointing at a resource) and the
/// programs it runs.
fn parse_manifest(text: &str) -> Manifest {
    let mut m = Manifest::default();
    if let Some(start) = text.find("<DisplayName>") {
        let rest = &text[start + "<DisplayName>".len()..];
        if let Some(end) = rest.find("</DisplayName>") {
            let name = rest[..end].trim();
            if !name.is_empty() && !name.starts_with("ms-resource:") {
                m.display_name = Some(unescape(name));
            }
        }
    }
    let mut rest = text;
    while let Some(i) = rest.find("<Application ") {
        let tail = &rest[i + 1..];
        let end = tail.find('>').unwrap_or(tail.len());
        if let Some(exe) = attr(&tail[..end], "Executable") {
            let key = exe_key(exe);
            if !key.is_empty() && !m.executables.contains(&key) {
                m.executables.push(key);
            }
        }
        rest = &tail[end..];
    }
    m
}

// ---------------------------------------------------------------- macOS: installed apps

/// The apps in `dirs` (and one level of folders such as Utilities), read
/// from their Info.plist files.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn mac_inventory_from(dirs: &[PathBuf]) -> Vec<MacApp> {
    fn visit(dir: &Path, depth: usize, out: &mut Vec<MacApp>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == "app") {
                if let Ok(plist::Value::Dictionary(d)) = plist::Value::from_file(path.join("Contents/Info.plist")) {
                    let get = |k: &str| d.get(k).and_then(|v| v.as_string()).map(String::from);
                    if let Some(bundle_id) = get("CFBundleIdentifier") {
                        let name = get("CFBundleDisplayName")
                            .or_else(|| get("CFBundleName"))
                            .map(|n| clean_name(&n))
                            .filter(|n| !n.is_empty())
                            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
                            .unwrap_or_else(|| bundle_id.clone());
                        let executable = get("CFBundleExecutable");
                        let team = crate::codesign::team_of_app(&path, executable.as_deref());
                        out.push(MacApp { bundle_id, name, path: path.clone(), executable, team });
                    }
                }
            } else if depth > 0 && path.is_dir() {
                visit(&path, depth - 1, out);
            }
        }
    }
    let mut out = vec![];
    for d in dirs {
        visit(d, 1, &mut out);
    }
    out
}

/// Some apps put invisible direction marks around their name (WhatsApp's
/// starts with U+200E); they would only show as stray spacing.
fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect::<String>().trim().to_string()
}

/// "group.com.acme.app", "ABCDE12345.com.acme.app" or
/// "ABCDE12345.group.com.acme.app" -> (team, "com.acme.app").
fn group_parts(group: &str) -> (Option<&str>, &str) {
    let (team, rest) = match group.split_once('.') {
        Some((team, rest)) if team.len() == 10 && team.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) => (Some(team), rest),
        _ => (None, group),
    };
    (team, rest.strip_prefix("group.").unwrap_or(rest))
}

fn group_bundle_id(group: &str) -> &str {
    group_parts(group).1
}

/// The installed app a container belongs to. A container's id is its app's
/// bundle id, or that of an extension inside the app ("<app id>.FileProvider").
/// A group container is shared by a developer's apps: it is named after one
/// of them, or after the team that signed them all ("UBF8T346G9.Office").
fn owner_of<'a>(apps: &'a [MacApp], id: &str, layout: Layout) -> Option<&'a MacApp> {
    let (team, bundle) = if layout == Layout::GroupContainer { group_parts(id) } else { (None, id) };
    let b = bundle.to_lowercase();
    let named = |a: &&MacApp| {
        let app = a.bundle_id.to_lowercase();
        app == b || b.starts_with(&format!("{app}.")) || (layout == Layout::GroupContainer && app.starts_with(&format!("{b}.")))
    };
    apps.iter().find(named).or_else(|| {
        let team = team?;
        let same: Vec<&MacApp> = apps.iter().filter(|a| a.team.as_deref() == Some(team)).collect();
        // Several apps of the team: the one whose id shares the most with
        // the container's name.
        same.into_iter().max_by_key(|a| common_prefix(&a.bundle_id.to_lowercase(), &b))
    })
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

// ---------------------------------------------------------------- discovery

fn discover(ctx: &Ctx) -> Vec<Package> {
    let mut out = vec![];
    for (root, layout) in &ctx.roots {
        for (name, path) in subdirs(root) {
            out.push(package_for(ctx, name, path, *layout));
        }
    }
    out
}

fn package_for(ctx: &Ctx, id: String, root: PathBuf, layout: Layout) -> Package {
    let mut pkg = Package {
        name: pretty_name(&id),
        id,
        root,
        layout,
        state: State::Unknown,
        install: None,
        installs: vec![],
        version: None,
        exes: vec![],
        system: false,
    };
    match (&ctx.inventory, layout) {
        (Inventory::Msix(map), Layout::Msix) => match map.get(&pkg.id.to_lowercase()) {
            Some(app) => {
                pkg.state = State::Installed;
                pkg.version = app.version.clone();
                if !app.root.as_os_str().is_empty() {
                    pkg.install = Some(app.root.clone());
                }
                pkg.system = pkg.install.as_ref().is_some_and(|p| p.starts_with(&ctx.system_root));
                pkg.installs = app.roots.clone();
                if let Some(root) = &pkg.install {
                    if !pkg.installs.contains(root) {
                        pkg.installs.push(root.clone());
                    }
                }
                // The programs of every version: during an update the old
                // one may still be the one running.
                let mut manifest = Manifest::default();
                for root in &pkg.installs {
                    let Some(m) = std::fs::read_to_string(root.join("AppxManifest.xml")).ok().map(|t| parse_manifest(&t)) else { continue };
                    for exe in m.executables {
                        if !manifest.executables.contains(&exe) {
                            manifest.executables.push(exe);
                        }
                    }
                    if m.display_name.is_some() && (manifest.display_name.is_none() || pkg.install.as_ref() == Some(root)) {
                        manifest.display_name = m.display_name;
                    }
                }
                pkg.exes = manifest.executables;
                let raw = app.raw_display.trim();
                let registry = if raw.starts_with("@{") {
                    load_indirect(raw)
                } else if raw.is_empty() || raw.starts_with("ms-resource:") {
                    None
                } else {
                    Some(raw.to_string())
                };
                if let Some(name) = registry.or(manifest.display_name) {
                    pkg.name = name;
                }
            }
            None => pkg.state = State::Orphaned,
        },
        (Inventory::Mac(apps), Layout::Container | Layout::GroupContainer) => {
            let bundle = if layout == Layout::GroupContainer { group_bundle_id(&pkg.id).to_string() } else { pkg.id.clone() };
            match owner_of(apps, &pkg.id, layout) {
                Some(app) => {
                    pkg.state = State::Installed;
                    pkg.name = app.name.clone();
                    pkg.install = Some(app.path.clone());
                    pkg.installs = vec![app.path.clone()];
                    pkg.exes = app.executable.iter().map(|e| exe_key(e)).collect();
                }
                // Apple's own apps live on the sealed system volume and in
                // places Jharu does not list; never call them left behind.
                None if bundle.starts_with("com.apple.") => {
                    pkg.state = State::Installed;
                    pkg.system = true;
                }
                None => pkg.state = State::Orphaned,
            }
        }
        _ => {}
    }
    pkg
}

// ---------------------------------------------------------------- running

#[derive(Debug)]
enum RunState {
    Running(Vec<String>),
    /// Its programs are known and none is running.
    Idle,
    /// Jharu does not know which programs belong to the app, so it cannot
    /// tell whether it is running.
    Unverifiable,
    /// The app is not installed.
    NotInstalled,
}

fn run_state(pkg: &Package, facts: &Facts) -> RunState {
    if pkg.state == State::Orphaned {
        return RunState::NotInstalled;
    }
    let mut names: Vec<String> = pkg.exes.iter().filter(|e| facts.running.contains(*e)).cloned().collect();
    for dir in &pkg.installs {
        names.extend(facts.running_from(dir).iter().map(|n| exe_key(n)));
    }
    names.sort();
    names.dedup();
    if !names.is_empty() {
        RunState::Running(names)
    } else if pkg.exes.is_empty() && pkg.installs.is_empty() {
        RunState::Unverifiable
    } else {
        RunState::Idle
    }
}

fn running_names(run: &RunState) -> Vec<String> {
    match run {
        RunState::Running(n) => n.clone(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------- what each folder is

#[derive(Clone, Debug)]
enum Role {
    /// Rebuilt by the app; the text says what it holds.
    Temp(&'static str),
    /// Sign-ins of the app's embedded browser.
    Cookies,
    /// The app's own data; the text says what.
    AppData(&'static str),
    /// Managed by Windows for the package.
    System,
    /// Holds caches next to the app's own data; opened up part by part.
    Mixed,
    Unknown,
}

fn role_for(layout: Layout, key: &str) -> Role {
    use Role::*;
    match layout {
        Layout::Msix => match key {
            "tempstate" => Temp("Scratch files the app writes while it runs. Windows itself clears this folder when the disk runs low, so apps treat it as disposable"),
            "ac/temp" => Temp("The app's private Temp folder, the one its code sees as %TEMP%"),
            "ac/inetcache" => Temp("The web cache of the app's embedded browser: pages and images fetched again when needed"),
            "ac/inethistory" => Temp("Browsing history of the app's embedded browser"),
            "ac/inetcookies" => Cookies,
            "localstate" => AppData("the app's own data and settings (LocalState)"),
            "roamingstate" => AppData("the app's roaming data and settings (RoamingState)"),
            "settings" => AppData("the app's settings store (settings.dat)"),
            "systemappdata" => System,
            "localcache" => Mixed,
            _ => Unknown,
        },
        Layout::Container | Layout::GroupContainer => match key {
            "library/caches" => Temp("Caches the app rebuilds when it needs them"),
            "tmp" => Temp("Temporary files"),
            "library/logs" => Temp("Log files the app writes"),
            "library/saved application state" => Temp("Windows and documents the app reopens at launch; without them it simply starts fresh"),
            "library/cookies" | "library/httpstorages" => Cookies,
            "library/application support" => AppData("the app's own data (Application Support)"),
            "library/preferences" => AppData("the app's settings (Preferences)"),
            "documents" | "desktop" | "downloads" | "movies" | "music" | "pictures" => AppData("files the app saved for you"),
            _ => Unknown,
        },
    }
}

// ---------------------------------------------------------------- planning

/// A folder Jharu rates: found by listing directories only, so deciding what
/// to rate is cheap and removal can repeat it.
#[derive(Clone)]
struct Planned {
    path: PathBuf,
    label: String,
    kind: Kind,
    mode: Mode,
    appears_as: Option<PathBuf>,
    children: Vec<Planned>,
    /// Loose files next to the children, counted in the size only.
    loose: u64,
}

#[derive(Clone)]
enum Kind {
    Role(Role),
    /// A Dev clean rule, by id.
    Rule(String),
    /// Anything else inside `LocalCache`, rated like the real folder at this
    /// path.
    Generic(PathBuf),
}

fn area(pkg: &Package, path: PathBuf, key: &str, label: String) -> Planned {
    Planned { path, label, kind: Kind::Role(role_for(pkg.layout, key)), mode: Mode::Contents, appears_as: None, children: vec![], loose: 0 }
}

fn plan(pkg: &Package, ctx: &Ctx) -> Vec<Planned> {
    let mut out = vec![];
    match pkg.layout {
        Layout::Msix => {
            for (name, path) in subdirs(&pkg.root) {
                let low = name.to_lowercase();
                if low == "ac" {
                    for (n2, p2) in subdirs(&path) {
                        out.push(area(pkg, p2, &format!("ac/{}", n2.to_lowercase()), format!("AC/{n2}")));
                    }
                } else if low == "localcache" {
                    let (children, loose) = drill(ctx, &path);
                    out.push(Planned {
                        path,
                        label: name,
                        kind: Kind::Role(Role::Mixed),
                        mode: Mode::Folder,
                        appears_as: None,
                        children,
                        loose,
                    });
                } else {
                    out.push(area(pkg, path, &low, name));
                }
            }
        }
        Layout::Container => mac_areas(pkg, &pkg.root.join("Data"), "Data/", &mut out),
        Layout::GroupContainer => mac_areas(pkg, &pkg.root, "", &mut out),
    }
    out
}

fn mac_areas(pkg: &Package, base: &Path, prefix: &str, out: &mut Vec<Planned>) {
    for (name, path) in subdirs(base) {
        if name.eq_ignore_ascii_case("Library") {
            for (n2, p2) in subdirs(&path) {
                out.push(area(pkg, p2, &format!("library/{}", n2.to_lowercase()), format!("{prefix}Library/{n2}")));
            }
        } else {
            out.push(area(pkg, path, &name.to_lowercase(), format!("{prefix}{name}")));
        }
    }
}

/// Where a real AppData location lives inside `LocalCache`, for each Dev
/// clean rule that has one. Rules under the home folder but outside AppData
/// (`~/.cache`, `~/.cargo`) are not virtualised and not found here.
fn rule_locations(ctx: &Ctx, cache: &Path) -> Vec<(PathBuf, String)> {
    let mut found = vec![];
    for rule in all_rules() {
        for real in rule.paths() {
            let virt = if let Ok(rest) = real.strip_prefix(&ctx.local_appdata) {
                cache.join("Local").join(rest)
            } else if let Ok(rest) = real.strip_prefix(&ctx.roaming_appdata) {
                cache.join("Roaming").join(rest)
            } else {
                continue;
            };
            if virt.exists() {
                found.push((virt, rule.id.to_string()));
            }
        }
    }
    found
}

/// Opens `LocalCache`: Dev clean's locations first, then every other folder.
fn drill(ctx: &Ctx, cache: &Path) -> (Vec<Planned>, u64) {
    let rules = rule_locations(ctx, cache);
    let mut out = vec![];
    let mut loose = 0u64;
    if let Ok(rd) = std::fs::read_dir(cache) {
        for e in rd.flatten() {
            if e.path().symlink_metadata().is_ok_and(|m| m.is_file()) {
                loose += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    for (name, path) in subdirs(cache) {
        let real = match name.to_lowercase().as_str() {
            "local" => ctx.local_appdata.clone(),
            "roaming" => ctx.roaming_appdata.clone(),
            "locallow" => ctx.home.join("AppData").join("LocalLow"),
            _ => ctx.home.join("AppData").join(&name),
        };
        let is_local = name.eq_ignore_ascii_case("local");
        walk_level(&path, &real, &name, is_local, 0, &rules, &mut out, &mut loose);
    }
    (out, loose)
}

#[allow(clippy::too_many_arguments)]
fn walk_level(
    dir: &Path,
    real: &Path,
    label: &str,
    is_local: bool,
    depth: usize,
    rules: &[(PathBuf, String)],
    out: &mut Vec<Planned>,
    loose: &mut u64,
) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else { continue };
            if is_link(&meta) {
                continue;
            }
            if meta.is_file() {
                *loose += meta.len();
            }
        }
    }
    for (name, path) in subdirs(dir) {
        let key = path_key(&path);
        let shown = format!("{label}/{name}");
        let real_child = real.join(&name);
        if let Some((_, id)) = rules.iter().find(|(p, _)| path_key(p) == key) {
            out.push(Planned {
                path,
                label: shown,
                kind: Kind::Rule(id.clone()),
                mode: Mode::Folder,
                appears_as: Some(real_child),
                children: vec![],
                loose: 0,
            });
        } else if rules.iter().any(|(p, _)| is_inside(p, &path)) {
            walk_level(&path, &real_child, &shown, is_local, depth + 1, rules, out, loose);
        } else if is_local && depth == 0 && name.eq_ignore_ascii_case("Temp") {
            out.push(Planned {
                path,
                label: shown,
                kind: Kind::Role(Role::Temp("The app's private copy of %LOCALAPPDATA%\\Temp")),
                mode: Mode::Contents,
                appears_as: Some(real_child),
                children: vec![],
                loose: 0,
            });
        } else {
            out.push(Planned {
                path,
                label: shown,
                kind: Kind::Generic(real_child.clone()),
                mode: Mode::Folder,
                appears_as: Some(real_child),
                children: vec![],
                loose: 0,
            });
        }
    }
}

fn flatten<'a>(plans: &'a [Planned], out: &mut Vec<&'a Planned>) {
    for p in plans {
        out.push(p);
        flatten(&p.children, out);
    }
}

// ---------------------------------------------------------------- rating

struct Measured {
    size: u64,
    reclaimable: u64,
    files: u64,
    signals: places::Signals,
}

fn measure_entry(path: &Path) -> Measured {
    let mut seen = Seen::default();
    let (usage, signals) = deep::measure(path, &mut seen);
    Measured {
        size: usage.bytes,
        reclaimable: usage.bytes.saturating_sub(seen.linked_outside()),
        files: usage.files,
        signals,
    }
}

fn warn(text: impl Into<String>) -> Reason {
    Reason { level: Level::Warn, text: text.into() }
}
fn good(text: impl Into<String>) -> Reason {
    Reason { level: Level::Ok, text: text.into() }
}
fn bad(text: impl Into<String>) -> Reason {
    Reason { level: Level::Bad, text: text.into() }
}

struct Rated {
    safety: Safety,
    basis: &'static str,
    what: String,
    reasons: Vec<Reason>,
    removable: bool,
}

fn rate_role(role: &Role, pkg: &Package) -> Rated {
    let orphan = pkg.state == State::Orphaned;
    let mut r = Rated { safety: Safety::Risky, basis: "layout", what: String::new(), reasons: vec![], removable: false };
    match role {
        Role::Temp(what) => {
            r.safety = Safety::Safe;
            r.what = "Temporary files".into();
            r.reasons.push(good(format!("{what}.")));
            r.removable = true;
        }
        Role::Cookies => {
            r.safety = Safety::Caution;
            r.what = "Sign-ins and web storage".into();
            r.reasons.push(warn(
                "Sign-in cookies of the app's embedded browser. Removing them signs you out of the services you use inside the app; nothing else is lost.",
            ));
            r.removable = true;
        }
        Role::AppData(what) => {
            r.what = "The app's own data".into();
            if orphan {
                r.safety = Safety::Caution;
                r.reasons.push(warn(format!(
                    "Holds {what}. The app is no longer installed, so nothing reads it any more, but Jharu cannot tell whether it holds files the app saved for you. Look inside before removing it."
                )));
                r.removable = true;
            } else {
                r.reasons.push(bad(format!(
                    "Holds {what}. Jharu cannot re-create this: once the {BIN} is emptied, it is gone."
                )));
            }
        }
        Role::System => {
            r.what = "Managed by Windows".into();
            r.reasons.push(bad(
                "Windows keeps the package's own bookkeeping here (state it carries across app updates). Jharu leaves it alone.",
            ));
        }
        Role::Mixed => {
            r.what = "The app's private copy of AppData".into();
            if orphan {
                r.safety = Safety::Caution;
            }
            r.reasons.push(warn(
                "Holds the app's private copy of AppData: its caches next to its own settings and data. The app sees these files at their usual AppData locations. Open it and decide part by part; removing it whole would take the parts it needs too.",
            ));
        }
        Role::Unknown => {
            // Filled in by the caller from what the folder holds.
            r.what = "Folder Jharu does not recognise".into();
        }
    }
    r
}

/// Folds in whether the app is using the folder right now.
fn apply_run(r: &mut Rated, run: &RunState, pkg: &Package) {
    if r.safety == Safety::Risky || !r.removable {
        return;
    }
    match run {
        RunState::Running(names) => {
            r.safety = r.safety.max(Safety::Caution);
            r.reasons.push(warn(format!(
                "{} is running right now and may be using these files. Close it first; otherwise Jharu leaves everything in place.",
                names.join(", ")
            )));
        }
        RunState::Unverifiable => {
            r.safety = r.safety.max(Safety::Caution);
            r.reasons.push(warn(format!(
                "Jharu could not tell which program belongs to {}, so it cannot confirm the app is closed. Close it before removing.",
                pkg.name
            )));
        }
        RunState::Idle => r.reasons.push(good(format!("Not in use right now: {} is not running.", pkg.name))),
        RunState::NotInstalled => {
            r.reasons.push(good("The app is no longer installed, so none of its programs can be using these files."))
        }
    }
}

fn rate_planned(
    pl: &Planned,
    pkg: &Package,
    ctx: &Ctx,
    facts: &Facts,
    run: &RunState,
    cancelled: &dyn Fn() -> bool,
    note: &dyn Fn(&str),
) -> Option<Entry> {
    if cancelled() {
        return None;
    }
    note(&format!("{} · {}", pkg.name, pl.label));
    let mut children = vec![];
    let (size, reclaimable_bytes, files, mut rated) = match &pl.kind {
        Kind::Role(Role::Mixed) => {
            let mut size = pl.loose;
            let mut files = 0;
            for c in &pl.children {
                let e = rate_planned(c, pkg, ctx, facts, run, cancelled, note)?;
                size += e.size_bytes;
                files += e.file_count;
                children.push(e);
            }
            children.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
            (size, 0, files, rate_role(&Role::Mixed, pkg))
        }
        Kind::Role(role) => {
            let m = measure_entry(&pl.path);
            let mut r = rate_role(role, pkg);
            if matches!(role, Role::Unknown) {
                // The folder's own layout says nothing; what it holds does.
                let v = places::verdict(&pl.path, &ctx.home, &m.signals, facts);
                r.safety = v.safety;
                r.basis = v.basis;
                r.reasons = v.reasons;
                r.removable = v.clean_here && v.safety != Safety::Risky;
                if pkg.state == State::Orphaned && v.safety == Safety::Risky && v.basis == "unknown" {
                    r.safety = Safety::Caution;
                    r.reasons.push(warn("The app is no longer installed, so nothing reads this folder any more. Look inside before removing it."));
                    r.removable = true;
                }
            }
            (m.size, m.reclaimable, m.files, r)
        }
        Kind::Rule(id) => {
            let m = measure_entry(&pl.path);
            let rule = all_rules().into_iter().find(|r| r.id == id.as_str())?;
            let (safety, reasons) = assess(&rule, &pl.path, facts);
            let r = Rated { safety, basis: "known", what: rule.name.to_string(), reasons, removable: safety != Safety::Risky };
            (m.size, m.reclaimable, m.files, r)
        }
        Kind::Generic(real) => {
            let m = measure_entry(&pl.path);
            let v = places::verdict(real, &ctx.home, &m.signals, facts);
            let r = Rated {
                safety: v.safety,
                basis: v.basis,
                what: v.what.unwrap_or_else(|| "Folder inside the app's private AppData".into()),
                reasons: v.reasons,
                removable: v.clean_here && v.safety != Safety::Risky,
            };
            (m.size, m.reclaimable, m.files, r)
        }
    };
    apply_run(&mut rated, run, pkg);
    // A package left behind: say so on everything that is not plainly safe.
    if pkg.state == State::Orphaned && rated.safety != Safety::Safe && rated.removable && matches!(pl.kind, Kind::Rule(_) | Kind::Generic(_)) {
        rated.reasons.push(warn("The app is no longer installed, so nothing of its own is using this."));
    }
    let preselect = rated.removable && rated.safety == Safety::Safe && size > 0;
    Some(Entry {
        path: pl.path.to_string_lossy().into_owned(),
        rel: rel_of(&pkg.root, &pl.path),
        label: pl.label.clone(),
        what: rated.what,
        size_bytes: size,
        reclaimable_bytes: if rated.removable { reclaimable_bytes.min(size) } else { 0 },
        file_count: files,
        safety: rated.safety,
        basis: rated.basis.to_string(),
        reasons: rated.reasons,
        removable: rated.removable,
        mode: pl.mode,
        preselect,
        appears_as: pl.appears_as.as_ref().map(|p| p.to_string_lossy().into_owned()),
        children,
    })
}

fn all_entries<'a>(entries: &'a [Entry], out: &mut Vec<&'a Entry>) {
    for e in entries {
        out.push(e);
        all_entries(&e.children, out);
    }
}

fn scan_package(
    pkg: &Package,
    ctx: &Ctx,
    facts: &Facts,
    cancelled: &dyn Fn() -> bool,
    note: &dyn Fn(&str),
) -> Option<SandboxPackage> {
    let run = run_state(pkg, facts);
    let mut entries = vec![];
    for pl in plan(pkg, ctx) {
        entries.push(rate_planned(&pl, pkg, ctx, facts, &run, cancelled, note)?);
    }
    entries.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    // Files directly in the sandbox folder, outside every area.
    let mut size: u64 = entries.iter().map(|e| e.size_bytes).sum();
    let top = match pkg.layout {
        Layout::Container => pkg.root.join("Data"),
        _ => pkg.root.clone(),
    };
    if let Ok(rd) = std::fs::read_dir(&top) {
        for e in rd.flatten() {
            if let Ok(m) = e.path().symlink_metadata() {
                if m.is_file() {
                    size += m.len();
                }
            }
        }
    }

    let mut flat = vec![];
    all_entries(&entries, &mut flat);
    let safe_bytes = flat.iter().filter(|e| e.preselect).map(|e| e.reclaimable_bytes).sum();

    let mut reasons = vec![];
    let mut safety = None;
    match pkg.state {
        State::Orphaned => {
            safety = Some(Safety::Caution);
            reasons.push(warn(match pkg.layout {
                Layout::Msix => "Windows has no record of this app for your account, so it is not installed. Uninstalled apps can leave their data behind (for example when removed with the option to keep it). Nothing of its own is using these folders, but look before removing what it saved.".to_string(),
                _ => format!(
                    "No app with this bundle id was found in /Applications, ~/Applications or /System/Applications. It may be installed somewhere else; if it is gone, nothing of its own is using these folders."
                ),
            }));
        }
        State::Unknown => reasons.push(warn(
            "Jharu could not read the list of installed apps, so it does not say whether this one is still installed.",
        )),
        State::Installed => {}
    }
    if let RunState::Running(names) = &run {
        safety = Some(Safety::Caution);
        reasons.push(warn(format!("{} is running right now. Close it first; until then Jharu will not remove anything here.", names.join(", "))));
    }
    if pkg.system {
        reasons.push(warn("This is a component of the operating system, not an app you installed."));
    }

    Some(SandboxPackage {
        id: pkg.id.clone(),
        name: pkg.name.clone(),
        root: pkg.root.to_string_lossy().into_owned(),
        kind: pkg.layout.kind(),
        state: pkg.state,
        running: running_names(&run),
        install_path: pkg.install.as_ref().map(|p| p.to_string_lossy().into_owned()),
        version: pkg.version.clone(),
        system: pkg.system,
        size_bytes: size,
        safe_bytes,
        safety,
        reasons,
        entries,
    })
}

fn scan_impl(
    ctx: &Ctx,
    facts: &Facts,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(usize, usize, &str),
) -> SandboxReport {
    let packages_found = discover(ctx);
    let total = packages_found.len();
    let mut packages = vec![];
    let mut was_cancelled = false;
    for (i, pkg) in packages_found.iter().enumerate() {
        if cancelled() {
            was_cancelled = true;
            break;
        }
        progress(i, total, &pkg.name);
        let note = |what: &str| progress(i, total, what);
        match scan_package(pkg, ctx, facts, cancelled, &note) {
            Some(p) => packages.push(p),
            None => {
                was_cancelled = true;
                break;
            }
        }
    }
    progress(total, total, "");
    packages.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    SandboxReport {
        supported: !ctx.roots.is_empty(),
        platform: if cfg!(target_os = "windows") { "windows" } else { "macos" },
        locations: ctx.roots.iter().map(|(p, _)| p.to_string_lossy().into_owned()).collect(),
        inventory_known: !matches!(ctx.inventory, Inventory::Unavailable),
        total_bytes: packages.iter().map(|p| p.size_bytes).sum(),
        safe_bytes: packages.iter().map(|p| p.safe_bytes).sum(),
        orphan_bytes: packages.iter().filter(|p| p.state == State::Orphaned).map(|p| p.size_bytes).sum(),
        cancelled: was_cancelled,
        packages,
    }
}

// ---------------------------------------------------------------- commands: cancel and scan

/// Tokens of tasks the user asked to stop.
fn cancels() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

fn is_cancelled(token: &str) -> bool {
    cancels().lock().map(|s| s.contains(token)).unwrap_or(false)
}

/// Stops a scan or a removal after the item it is working on.
#[tauri::command]
pub fn cancel_sandbox_task(token: String) {
    if let Ok(mut s) = cancels().lock() {
        s.insert(token);
    }
}

fn forget(token: &str) {
    if let Ok(mut s) = cancels().lock() {
        s.remove(token);
    }
}

#[derive(Serialize, Clone)]
struct Progress {
    token: String,
    done: usize,
    total: usize,
    current: String,
}

#[tauri::command]
pub async fn sandbox_scan(app: tauri::AppHandle, token: String) -> Result<SandboxReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let report = match Ctx::system() {
            None => SandboxReport {
                supported: false,
                platform: "other",
                locations: vec![],
                inventory_known: false,
                total_bytes: 0,
                safe_bytes: 0,
                orphan_bytes: 0,
                cancelled: false,
                packages: vec![],
            },
            Some(ctx) => {
                let facts = Facts::gather();
                scan_impl(&ctx, &facts, &|| is_cancelled(&token), &|done, total, current| {
                    let _ = app.emit(
                        "sandbox-scan-progress",
                        Progress { token: token.clone(), done, total, current: current.to_string() },
                    );
                })
            }
        };
        forget(&token);
        report
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- removal

#[derive(Deserialize, Clone)]
pub struct RemoveRequest {
    /// The sandbox folder.
    pub package_root: String,
    pub path: String,
    /// The rating the user saw: "safe", "caution" or "risky". Anything that
    /// has become worse since is skipped.
    pub expected: String,
}

#[derive(Serialize, Default, Debug)]
pub struct RemoveOutcome {
    pub freed_bytes: u64,
    pub removed: Vec<String>,
    /// Folders emptied only in part: something inside was open elsewhere.
    pub partial: Vec<String>,
    pub already_gone: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    /// The user stopped it; the rest was not touched.
    pub cancelled: bool,
    pub not_attempted: usize,
}

fn parse_safety(s: &str) -> Safety {
    match s {
        "safe" => Safety::Safe,
        "caution" => Safety::Caution,
        _ => Safety::Risky,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Worst answers first, for the "changed since the scan" message.
fn why(reasons: &[Reason]) -> String {
    let mut worst: Vec<&Reason> = reasons.iter().filter(|r| r.level != Level::Ok).collect();
    worst.sort_by_key(|r| std::cmp::Reverse(r.level == Level::Bad));
    worst.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// Moves what is inside `dir` to the bin one entry at a time, keeping `dir`.
/// Returns false if the user stopped it.
fn clear_contents(
    dir: &Path,
    trash: &dyn Fn(&Path) -> Result<(), String>,
    cancelled: &dyn Fn() -> bool,
    out: &mut RemoveOutcome,
) -> bool {
    let entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(e) => {
            out.errors.push(format!("{}: {e}", dir.display()));
            return true;
        }
    };
    let (mut moved, mut left) = (0usize, 0usize);
    let mut left_names: Vec<String> = vec![];
    let mut finished = true;
    for p in entries {
        if cancelled() {
            finished = false;
            break;
        }
        // A link or junction may lead anywhere; it stays where it is.
        if p.symlink_metadata().is_ok_and(|m| is_link(&m)) {
            left += 1;
            if left_names.len() < NAME_LIMIT {
                left_names.push(format!("{} (a link)", file_name(&p)));
            }
            continue;
        }
        // Counted before it goes, so hard links kept alive elsewhere are not
        // claimed as freed.
        let size = usage::reclaimable(&p);
        let _ = trash(&p);
        if p.symlink_metadata().is_err() {
            moved += 1;
            out.freed_bytes += size;
        } else {
            left += 1;
            if left_names.len() < NAME_LIMIT {
                left_names.push(file_name(&p));
            }
        }
    }
    let label = file_name(dir);
    if left > 0 {
        out.partial.push(format!(
            "{label}: moved {moved}, left {left} in place because another program has them open, or they are links Jharu never follows ({})",
            left_names.join(", ")
        ));
    } else if moved > 0 {
        out.removed.push(dir.to_string_lossy().into_owned());
    }
    finished
}

fn remove_impl(
    ctx: &Ctx,
    facts: &dyn Fn() -> Facts,
    requests: &[RemoveRequest],
    trash: &dyn Fn(&Path) -> Result<(), String>,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(usize, usize, &str),
) -> RemoveOutcome {
    let mut out = RemoveOutcome::default();
    let mut seen: HashSet<String> = HashSet::new();
    let requests: Vec<&RemoveRequest> = requests.iter().filter(|r| seen.insert(path_key(Path::new(&r.path)))).collect();
    let total = requests.len();
    let packages = discover(ctx);
    let mut plans: HashMap<String, Vec<Planned>> = HashMap::new();

    for (done, r) in requests.iter().enumerate() {
        let path = PathBuf::from(&r.path);
        let label = file_name(&path);
        if cancelled() {
            out.cancelled = true;
            out.not_attempted = total - done;
            break;
        }
        progress(done, total, &label);

        // The sandbox must be one Jharu itself lists, and the folder one of
        // the folders it rates inside it; nothing the request says is trusted
        // beyond picking among those.
        let root_key = path_key(Path::new(&r.package_root));
        let Some(pkg) = packages.iter().find(|p| path_key(&p.root) == root_key) else {
            out.skipped.push(format!("{label}: not inside an app sandbox that Jharu lists"));
            continue;
        };
        if !is_inside(&path, &pkg.root) {
            out.skipped.push(format!("{label}: not inside the sandbox {}", pkg.name));
            continue;
        }
        let Ok(meta) = path.symlink_metadata() else {
            out.already_gone.push(r.path.clone());
            continue;
        };
        if is_link(&meta) {
            out.skipped.push(format!("{label}: it is a link to somewhere else, which Jharu never follows"));
            continue;
        }
        let plan_list = plans.entry(root_key.clone()).or_insert_with(|| plan(pkg, ctx));
        let mut flat = vec![];
        flatten(plan_list, &mut flat);
        let Some(planned) = flat.into_iter().find(|p| path_key(&p.path) == path_key(&path)) else {
            out.skipped.push(format!("{label}: not one of the folders Jharu rates in {}", pkg.name));
            continue;
        };

        // State can change after the scan; decide again right now.
        let facts_now = facts();
        let run = run_state(pkg, &facts_now);
        if let RunState::Running(names) = &run {
            out.skipped.push(format!("{label}: {} is running. Close {} and try again.", names.join(", "), pkg.name));
            continue;
        }
        let Some(now) = rate_planned(planned, pkg, ctx, &facts_now, &run, cancelled, &|_| {}) else {
            out.cancelled = true;
            out.not_attempted = total - done;
            break;
        };
        if !now.removable || now.safety > parse_safety(&r.expected) {
            out.skipped.push(format!("{label}: changed since the scan: {}", why(&now.reasons)));
            continue;
        }

        match planned.mode {
            Mode::Contents => {
                if !clear_contents(&path, trash, cancelled, &mut out) {
                    out.cancelled = true;
                    out.not_attempted = total - done - 1;
                    break;
                }
            }
            Mode::Folder => {
                let size = usage::reclaimable(&path);
                match trash(&path) {
                    Ok(()) => {
                        out.freed_bytes += size;
                        out.removed.push(r.path.clone());
                    }
                    // Windows moves a folder whole or not at all; a file held
                    // open by a program shows up only as "aborted".
                    Err(e) if path.exists() && e.contains("aborted") => out.skipped.push(format!(
                        "{label}: a file inside is open in another program, so nothing was moved. Close it and try again."
                    )),
                    Err(e) => out.errors.push(format!("{label}: {e}")),
                }
            }
        }
    }
    progress(total, total, "");
    out
}

fn trash_to_bin(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

/// Moves the rated folders of app sandboxes to the Recycle Bin / Trash.
#[tauri::command]
pub async fn remove_sandbox_items(
    app: tauri::AppHandle,
    items: Vec<RemoveRequest>,
    token: String,
) -> Result<RemoveOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let out = match Ctx::system() {
            None => RemoveOutcome { errors: vec!["App sandboxes do not exist on this system.".into()], ..Default::default() },
            Some(ctx) => remove_impl(&ctx, &Facts::gather, &items, &trash_to_bin, &|| is_cancelled(&token), &|done, total, current| {
                let _ = app.emit(
                    "sandbox-progress",
                    Progress { token: token.clone(), done, total, current: current.to_string() },
                );
            }),
        };
        forget(&token);
        out
    })
    .await
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            // A space in the name, like this repository's own folder.
            let root = std::env::temp_dir().join(format!("jharu sb {tag} {}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![7u8; bytes]).unwrap();
            p
        }

        fn packages(&self) -> PathBuf {
            self.root.join("Local").join("Packages")
        }

        fn manifest(&self, install: &str, name: &str, exe: &str) -> PathBuf {
            let dir = self.root.join("Program Files").join("WindowsApps").join(install);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("AppxManifest.xml"),
                format!(
                    "<?xml version=\"1.0\"?>\n<Package xmlns=\"http://schemas.microsoft.com/appx/manifest/foundation/windows10\">\n  <Identity Name=\"Acme\" Version=\"1.0.0.0\" />\n  <Properties>\n    <DisplayName>{name}</DisplayName>\n  </Properties>\n  <Applications>\n    <Application Id=\"App\" Executable=\"app\\{exe}\" EntryPoint=\"Windows.FullTrustApplication\">\n    </Application>\n  </Applications>\n</Package>\n"
                ),
            )
            .unwrap();
            dir
        }

        /// The Windows layout: an installed app "Acme Notes", a left-behind
        /// "Gone.Tool", both with every kind of folder.
        fn windows(&self) -> Ctx {
            let acme = "AcmeNotes_8wekyb3d8bbwe";
            for pkg in [acme, "Gone.Tool_8wekyb3d8bbwe"] {
                self.file(&format!("Local/Packages/{pkg}/TempState/scratch.tmp"), 1000);
                self.file(&format!("Local/Packages/{pkg}/AC/Temp/a.tmp"), 2000);
                self.file(&format!("Local/Packages/{pkg}/AC/INetCache/page.html"), 3000);
                self.file(&format!("Local/Packages/{pkg}/AC/INetCookies/c.cookie"), 400);
                self.file(&format!("Local/Packages/{pkg}/LocalState/notes.db"), 5000);
                self.file(&format!("Local/Packages/{pkg}/RoamingState/prefs.json"), 600);
                self.file(&format!("Local/Packages/{pkg}/Settings/settings.dat"), 700);
                self.file(&format!("Local/Packages/{pkg}/SystemAppData/state.bin"), 800);
                self.file(&format!("Local/Packages/{pkg}/Odd/thing.bin"), 900);
                // A cargo-style target folder: its tool marks it a cache.
                self.file(&format!("Local/Packages/{pkg}/LocalCache/Local/build-target/CACHEDIR.TAG"), 40);
                self.file(&format!("Local/Packages/{pkg}/LocalCache/Local/build-target/debug/app.bin"), 20_000);
                self.file(&format!("Local/Packages/{pkg}/LocalCache/Local/Temp/t.tmp"), 1500);
                self.file(&format!("Local/Packages/{pkg}/LocalCache/Local/Mystery/data.bin"), 4000);
                self.file(&format!("Local/Packages/{pkg}/LocalCache/Roaming/Notes/state.json"), 300);
                self.file(&format!("Local/Packages/{pkg}/LocalCache/loose.txt"), 11);
            }
            // The real AppData locations, which LocalCache mirrors: Dev
            // clean's uv rule lives at <LocalAppData>\uv\cache.
            let local = dirs::data_local_dir().unwrap_or_else(|| self.root.join("Local"));
            let uv = format!("Local/Packages/{acme}/LocalCache/Local/uv/cache/wheel.whl");
            if cfg!(target_os = "windows") {
                self.file(&uv, 50_000);
                self.file(&format!("Local/Packages/{acme}/LocalCache/Local/uv/python/python.exe"), 6000);
            }
            let install = self.manifest("AcmeNotes_1.0.0.0_x64__8wekyb3d8bbwe", "Acme Notes Pro", "AcmeNotes.exe");
            let mut map = HashMap::new();
            map.insert(
                acme.to_lowercase(),
                MsixApp { full_name: "AcmeNotes_1.0.0.0_x64__8wekyb3d8bbwe".into(), roots: vec![install.clone()], root: install, raw_display: "ms-resource:AppName".into(), version: Some("1.0.0.0".into()) },
            );
            Ctx {
                roots: vec![(self.packages(), Layout::Msix)],
                home: dirs::home_dir().unwrap_or_else(|| self.root.clone()),
                local_appdata: local,
                roaming_appdata: dirs::data_dir().unwrap_or_else(|| self.root.join("Roaming")),
                system_root: self.root.join("Windows"),
                inventory: Inventory::Msix(map),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn idle() -> Facts {
        Facts::default()
    }

    fn running(name: &str) -> Facts {
        Facts { running: [name.to_string()].into_iter().collect(), ..Default::default() }
    }

    fn scan(ctx: &Ctx, facts: &Facts) -> SandboxReport {
        scan_impl(ctx, facts, &|| false, &|_, _, _| {})
    }

    fn pkg<'a>(r: &'a SandboxReport, id: &str) -> &'a SandboxPackage {
        r.packages.iter().find(|p| p.id == id).unwrap_or_else(|| panic!("package {id} missing"))
    }

    fn entry<'a>(p: &'a SandboxPackage, rel: &str) -> &'a Entry {
        let mut all = vec![];
        all_entries(&p.entries, &mut all);
        all.into_iter().find(|e| e.rel.eq_ignore_ascii_case(rel)).unwrap_or_else(|| panic!("entry {rel} missing"))
    }

    #[test]
    fn manifest_names_and_programs() {
        let m = parse_manifest(
            "<Package><Properties><DisplayName>Acme &amp; Co Notes</DisplayName></Properties><Applications>\
             <Application Id=\"A\" Executable=\"app\\Notes.EXE\"/><Application Id=\"B\" Executable=\"helper.exe\" />\
             <Application Id=\"C\" xExecutable=\"no.exe\" /></Applications></Package>",
        );
        assert_eq!(m.display_name.as_deref(), Some("Acme & Co Notes"));
        assert_eq!(m.executables, vec!["notes", "helper"]);
        // A name that points at a resource is not a name.
        assert_eq!(parse_manifest("<DisplayName>ms-resource:AppName</DisplayName>").display_name, None);
    }

    #[test]
    fn names_families_and_group_ids() {
        assert_eq!(family_of("Claude_4.4212.0.0_x64__pzs8sxrjxfjjc").as_deref(), Some("Claude_pzs8sxrjxfjjc"));
        assert_eq!(family_of("c5e2524a-ea46_10.0.26100.1_neutral_neutral_cw5n1h2txyewy").as_deref(), Some("c5e2524a-ea46_cw5n1h2txyewy"));
        assert_eq!(pretty_name("Microsoft.WindowsNotepad_8wekyb3d8bbwe"), "Windows Notepad");
        assert_eq!(pretty_name("com.tinyspeck.slackmacgap"), "Slackmacgap");
        assert_eq!(group_bundle_id("group.com.acme.app"), "com.acme.app");
        assert_eq!(group_bundle_id("ABCDE12345.com.acme.app"), "com.acme.app");
        assert_eq!(group_bundle_id("com.acme.app"), "com.acme.app");
        assert_eq!(group_parts("6QRF79J2W6.group.com.acme.unzip"), (Some("6QRF79J2W6"), "com.acme.unzip"));
        assert_eq!(group_parts("UBF8T346G9.Office"), (Some("UBF8T346G9"), "Office"));
        assert_eq!(clean_name("\u{200e}WhatsApp"), "WhatsApp");
    }

    #[test]
    fn newest_non_resource_package_describes_the_app() {
        let main = "Foo_4.9.0.0_x64__pub";
        let newer = "Foo_4.10.0.0_x64__pub";
        let resource = "Foo_4.10.0.0_neutral_split.scale-100_pub";
        assert!(is_resource_package(resource) && !is_resource_package(main) && !is_resource_package("Foo_1.0.0.0_neutral__pub"));
        assert!(describes_better(newer, main), "4.10 is newer than 4.9");
        assert!(!describes_better(resource, main), "a resource package never describes the app");
        assert!(describes_better(main, resource));
    }

    #[test]
    fn windows_sandbox_ratings() {
        let f = Fixture::new("rate");
        let ctx = f.windows();
        let r = scan(&ctx, &idle());
        assert!(r.supported && r.inventory_known && !r.cancelled);
        assert_eq!(r.packages.len(), 2);

        let acme = pkg(&r, "AcmeNotes_8wekyb3d8bbwe");
        assert_eq!(acme.name, "Acme Notes Pro", "the name comes from the manifest, not the folder name");
        assert_eq!(acme.state, State::Installed);
        assert_eq!(acme.version.as_deref(), Some("1.0.0.0"));
        assert!(acme.running.is_empty() && acme.safety.is_none());

        // Safe, but only on evidence the app is closed.
        for rel in ["TempState", "AC/Temp", "AC/INetCache"] {
            let e = entry(acme, rel);
            assert_eq!((e.safety, e.removable, e.preselect, e.mode), (Safety::Safe, true, true, Mode::Contents), "{rel}");
            assert!(e.reasons.iter().any(|r| r.level == Level::Ok && r.text.contains("Acme Notes Pro is not running")), "{rel}");
        }
        assert_eq!(entry(acme, "TempState").size_bytes, 1000);
        let cookies = entry(acme, "AC/INetCookies");
        assert_eq!((cookies.safety, cookies.removable, cookies.preselect), (Safety::Caution, true, false));

        // The app's own data is Risky and not offered.
        for rel in ["LocalState", "RoamingState", "Settings", "SystemAppData"] {
            let e = entry(acme, rel);
            assert_eq!((e.safety, e.removable), (Safety::Risky, false), "{rel}");
            assert_eq!(e.reclaimable_bytes, 0);
            assert!(e.reasons.iter().any(|r| r.level == Level::Bad), "{rel}");
        }
        let odd = entry(acme, "Odd");
        assert_eq!((odd.safety, odd.basis.as_str(), odd.removable), (Safety::Risky, "unknown", false));

        // LocalCache is opened up part by part.
        let cache = entry(acme, "LocalCache");
        assert_eq!((cache.safety, cache.removable), (Safety::Risky, false));
        assert!(cache.children.len() >= 3);
        let target = entry(acme, "LocalCache/Local/build-target");
        assert_eq!((target.safety, target.basis.as_str(), target.removable, target.mode), (Safety::Safe, "detected", true, Mode::Folder));
        assert!(target.reasons.iter().any(|r| r.text.contains("CACHEDIR.TAG")));
        assert!(target.appears_as.is_some());
        let mystery = entry(acme, "LocalCache/Local/Mystery");
        assert_eq!((mystery.safety, mystery.removable), (Safety::Risky, false));
        let temp = entry(acme, "LocalCache/Local/Temp");
        assert_eq!((temp.safety, temp.mode), (Safety::Safe, Mode::Contents));
        // The area adds up: children plus the loose file.
        let kids: u64 = cache.children.iter().map(|c| c.size_bytes).sum();
        assert_eq!(cache.size_bytes, kids + 11);
        let uv_extra = if cfg!(target_os = "windows") { 50_000 + 6000 } else { 0 };
        assert_eq!(cache.size_bytes, 40 + 20_000 + 1500 + 4000 + 300 + uv_extra + 11);

        // Dev clean's rule finds uv inside the sandbox and rates it the same.
        #[cfg(target_os = "windows")]
        {
            let uv = entry(acme, "LocalCache/Local/uv/cache");
            assert_eq!((uv.safety, uv.basis.as_str(), uv.what.as_str()), (Safety::Safe, "known", "uv cache"));
            assert!(uv.reasons.iter().any(|r| r.text.contains("uv is not running")));
            assert_eq!(uv.size_bytes, 50_000);
            // The Python installs next to the cache are not a cache.
            let py = entry(acme, "LocalCache/Local/uv/python");
            assert!(!py.removable && py.safety == Safety::Risky);
        }

        // The sizes add up to the files written.
        let written = 1000 + 2000 + 3000 + 400 + 5000 + 600 + 700 + 800 + 900 + 40 + 20_000 + 1500 + 4000 + 300 + 11 + uv_extra;
        assert_eq!(acme.size_bytes, written);
        let safe = 1000 + 2000 + 3000 + 40 + 20_000 + 1500 + if cfg!(target_os = "windows") { 50_000 } else { 0 };
        assert_eq!(acme.safe_bytes, safe);
    }

    #[test]
    fn left_behind_packages_are_caution() {
        let f = Fixture::new("orphan");
        let r = scan(&f.windows(), &idle());
        let gone = pkg(&r, "Gone.Tool_8wekyb3d8bbwe");
        assert_eq!(gone.state, State::Orphaned);
        assert_eq!(gone.safety, Some(Safety::Caution));
        assert!(gone.reasons[0].text.contains("no record"));
        assert_eq!(gone.name, "Tool");
        assert_eq!(r.orphan_bytes, gone.size_bytes);
        for rel in ["LocalState", "RoamingState", "Settings"] {
            let e = entry(gone, rel);
            assert_eq!((e.safety, e.removable, e.preselect), (Safety::Caution, true, false), "{rel}");
            assert!(e.reasons.iter().any(|r| r.level == Level::Warn && r.text.contains("no longer installed")));
        }
        // Temp files of an app that is gone are safe: nothing can use them.
        let temp = entry(gone, "TempState");
        assert_eq!(temp.safety, Safety::Safe);
        assert!(temp.reasons.iter().any(|r| r.text.contains("no longer installed")));
        // Windows' own bookkeeping stays untouchable.
        assert!(!entry(gone, "SystemAppData").removable);
        assert_eq!(entry(gone, "Odd").safety, Safety::Caution);
    }

    #[test]
    fn running_app_lowers_everything_to_caution() {
        let f = Fixture::new("running");
        let ctx = f.windows();
        let r = scan(&ctx, &running("acmenotes"));
        let acme = pkg(&r, "AcmeNotes_8wekyb3d8bbwe");
        assert_eq!(acme.running, vec!["acmenotes"]);
        assert_eq!(acme.safety, Some(Safety::Caution));
        assert_eq!(acme.safe_bytes, 0, "nothing is preselected while the app runs");
        let temp = entry(acme, "TempState");
        assert_eq!((temp.safety, temp.preselect), (Safety::Caution, false));
        assert!(temp.reasons.iter().any(|r| r.level == Level::Warn && r.text.contains("acmenotes is running") && r.text.contains("Close it first")));

        // Also when only a program inside its install folder runs.
        let install = ctx.inventory_install("AcmeNotes_8wekyb3d8bbwe");
        let facts = Facts { exes: vec![install.join("helper").join("worker.exe")], ..Default::default() };
        let r = scan(&ctx, &facts);
        assert_eq!(pkg(&r, "AcmeNotes_8wekyb3d8bbwe").running, vec!["worker"]);
    }

    impl Ctx {
        fn inventory_install(&self, id: &str) -> PathBuf {
            match &self.inventory {
                Inventory::Msix(m) => m[&id.to_lowercase()].root.clone(),
                _ => panic!("not a Windows inventory"),
            }
        }
    }

    #[test]
    fn unknown_programs_do_not_count_as_closed() {
        let f = Fixture::new("unverifiable");
        let mut ctx = f.windows();
        // The package is installed, but neither a manifest nor an install
        // folder tells which programs belong to it.
        if let Inventory::Msix(m) = &mut ctx.inventory {
            let app = m.get_mut("acmenotes_8wekyb3d8bbwe").unwrap();
            app.root = PathBuf::new();
            app.roots.clear();
        }
        let r = scan(&ctx, &idle());
        let temp = entry(pkg(&r, "AcmeNotes_8wekyb3d8bbwe"), "TempState");
        assert_eq!((temp.safety, temp.preselect), (Safety::Caution, false));
        assert!(temp.reasons.iter().any(|r| r.text.contains("cannot confirm the app is closed")));
    }

    #[test]
    fn unlisted_apps_are_not_called_left_behind() {
        let f = Fixture::new("noinv");
        let mut ctx = f.windows();
        ctx.inventory = Inventory::Unavailable;
        let r = scan(&ctx, &idle());
        assert!(!r.inventory_known);
        assert!(r.packages.iter().all(|p| p.state == State::Unknown));
        assert_eq!(r.orphan_bytes, 0);
        assert!(pkg(&r, "Gone.Tool_8wekyb3d8bbwe").reasons[0].text.contains("could not read the list"));
    }

    fn request(p: &SandboxPackage, rel: &str) -> RemoveRequest {
        let e = entry(p, rel);
        RemoveRequest {
            package_root: p.root.clone(),
            path: e.path.clone(),
            expected: match e.safety {
                Safety::Safe => "safe",
                Safety::Caution => "caution",
                Safety::Risky => "risky",
            }
            .into(),
        }
    }

    /// Stands in for the Recycle Bin: tests never touch the real one.
    fn fake_bin(p: &Path) -> Result<(), String> {
        if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        }
        .map_err(|e| e.to_string())
    }

    #[test]
    fn removal_moves_only_rated_folders_and_rechecks() {
        let f = Fixture::new("remove");
        let ctx = f.windows();
        let r = scan(&ctx, &idle());
        let acme = pkg(&r, "AcmeNotes_8wekyb3d8bbwe");
        let acme_dir = f.packages().join("AcmeNotes_8wekyb3d8bbwe");

        let mut requests = vec![
            request(acme, "TempState"),
            request(acme, "AC/INetCache"),
            request(acme, "LocalCache/Local/build-target"),
            request(acme, "LocalCache/Local/Temp"),
            request(acme, "AC/INetCookies"),
            // Tampered: Risky areas, a folder outside the sandbox, a folder
            // inside it that Jharu does not rate, the sandbox itself.
            RemoveRequest { expected: "safe".into(), ..request(acme, "LocalState") },
            RemoveRequest { package_root: acme.root.clone(), path: f.root.join("Local").to_string_lossy().into_owned(), expected: "safe".into() },
            RemoveRequest { package_root: acme.root.clone(), path: acme_dir.join("LocalState").join("notes.db").to_string_lossy().into_owned(), expected: "safe".into() },
            RemoveRequest { package_root: acme.root.clone(), path: acme.root.clone(), expected: "safe".into() },
            RemoveRequest { package_root: f.root.to_string_lossy().into_owned(), path: acme_dir.join("Settings").to_string_lossy().into_owned(), expected: "safe".into() },
        ];
        // The cookies were rated Caution; the user ticked them knowingly.
        requests[4].expected = "caution".into();

        // After the scan, the build-target folder gains a git repository.
        std::fs::create_dir_all(acme_dir.join("LocalCache/Local/build-target/src/.git")).unwrap();
        std::fs::write(acme_dir.join("LocalCache/Local/build-target/src/.git/HEAD"), "ref").unwrap();

        let events = std::cell::RefCell::new(vec![]);
        let out = remove_impl(&ctx, &idle, &requests, &fake_bin, &|| false, &|d, t, c| events.borrow_mut().push((d, t, c.to_string())));
        println!("{out:#?}");

        // Contents go, the folders the app expects stay.
        assert!(acme_dir.join("TempState").is_dir() && !acme_dir.join("TempState/scratch.tmp").exists());
        assert!(acme_dir.join("AC/INetCache").is_dir() && !acme_dir.join("AC/INetCache/page.html").exists());
        assert!(acme_dir.join("LocalCache/Local/Temp").is_dir() && !acme_dir.join("LocalCache/Local/Temp/t.tmp").exists());
        assert!(acme_dir.join("AC/INetCookies").is_dir() && !acme_dir.join("AC/INetCookies/c.cookie").exists());
        assert_eq!(out.freed_bytes, 1000 + 3000 + 1500 + 400);
        assert_eq!(out.removed.len(), 4);

        // The cache that gained a repository since the scan stays.
        assert!(acme_dir.join("LocalCache/Local/build-target").is_dir());
        assert!(out.skipped.iter().any(|s| s.starts_with("build-target: changed since the scan") && s.contains("git repository")), "{:?}", out.skipped);

        // Everything else is untouched and every tampered request refused.
        for keep in ["LocalState/notes.db", "RoamingState/prefs.json", "Settings/settings.dat", "SystemAppData/state.bin", "AC/Temp/a.tmp", "Odd/thing.bin"] {
            assert!(acme_dir.join(keep).exists(), "{keep} must stay");
        }
        assert!(out.skipped.iter().any(|s| s.starts_with("LocalState: changed since the scan")));
        assert!(out.skipped.iter().any(|s| s.contains("not inside the sandbox")));
        assert!(out.skipped.iter().any(|s| s.contains("not one of the folders Jharu rates")));
        assert!(out.skipped.iter().any(|s| s.contains("not inside an app sandbox that Jharu lists")));
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(f.root.join("Local").is_dir());
        assert_eq!(events.borrow().last().map(|e| (e.0, e.1)), Some((requests.len(), requests.len())));
    }

    #[test]
    fn removal_skips_a_running_app_and_stops_when_asked() {
        let f = Fixture::new("stop");
        let ctx = f.windows();
        let r = scan(&ctx, &idle());
        let acme = pkg(&r, "AcmeNotes_8wekyb3d8bbwe");
        let acme_dir = f.packages().join("AcmeNotes_8wekyb3d8bbwe");
        let requests = vec![request(acme, "TempState"), request(acme, "AC/INetCache"), request(acme, "LocalCache/Local/build-target")];

        // The app was started after the scan.
        let out = remove_impl(&ctx, &|| running("acmenotes"), &requests, &fake_bin, &|| false, &|_, _, _| {});
        assert_eq!(out.skipped.len(), 3, "{:?}", out.skipped);
        assert!(out.skipped[0].contains("acmenotes is running") && out.skipped[0].contains("Close Acme Notes"));
        assert!(acme_dir.join("TempState/scratch.tmp").exists() && out.freed_bytes == 0);

        // The user presses Cancel while the first item is being moved.
        let pressed = Cell::new(false);
        let bin = |p: &Path| {
            let moved = fake_bin(p);
            pressed.set(true);
            moved
        };
        let out = remove_impl(&ctx, &idle, &requests, &bin, &|| pressed.get(), &|_, _, _| {});
        assert!(out.cancelled);
        assert_eq!(out.not_attempted, 2, "{out:?}");
        assert!(!acme_dir.join("TempState/scratch.tmp").exists(), "the first item was done");
        assert!(acme_dir.join("AC/INetCache/page.html").exists(), "the rest was not touched");
        assert!(acme_dir.join("LocalCache/Local/build-target").exists());
    }

    #[test]
    fn a_scan_can_be_stopped() {
        let f = Fixture::new("scancancel");
        let ctx = f.windows();
        let r = scan_impl(&ctx, &idle(), &|| true, &|_, _, _| {});
        assert!(r.cancelled && r.packages.is_empty());
        // Stopping part-way drops the package it was measuring.
        let n = Cell::new(0);
        let r = scan_impl(&ctx, &idle(), &|| { n.set(n.get() + 1); n.get() > 3 }, &|_, _, _| {});
        assert!(r.cancelled);
        assert!(r.packages.len() < 2);
    }

    #[test]
    fn links_are_never_followed() {
        let f = Fixture::new("links");
        let ctx = f.windows();
        let keep = f.file("Precious/doc.txt", 10);
        let acme = f.packages().join("AcmeNotes_8wekyb3d8bbwe");
        // A link inside the sandbox pointing at real files (created only
        // where the system allows it without elevation).
        let link = acme.join("AC").join("Linked");
        if !make_link(&link, &f.root.join("Precious")) {
            return println!("could not create a link here; skipping");
        }
        let r = scan(&ctx, &idle());
        let p = pkg(&r, "AcmeNotes_8wekyb3d8bbwe");
        let mut all = vec![];
        all_entries(&p.entries, &mut all);
        assert!(all.iter().all(|e| !e.rel.ends_with("Linked")), "a link must not be listed");
        let req = RemoveRequest { package_root: p.root.clone(), path: link.to_string_lossy().into_owned(), expected: "safe".into() };
        let out = remove_impl(&ctx, &idle, &[req], &fake_bin, &|| false, &|_, _, _| {});
        assert!(out.skipped.iter().any(|s| s.contains("link")), "{:?}", out.skipped);
        assert!(keep.exists());

        // A link inside a folder whose contents are moved stays where it is.
        let inner = acme.join("TempState").join("Linked");
        if make_link(&inner, &f.root.join("Precious")) {
            let r = scan(&ctx, &idle());
            let p = pkg(&r, "AcmeNotes_8wekyb3d8bbwe");
            let out = remove_impl(&ctx, &idle, &[request(p, "TempState")], &fake_bin, &|| false, &|_, _, _| {});
            assert!(keep.exists(), "the link's target must be untouched: {out:?}");
            assert!(!acme.join("TempState/scratch.tmp").exists());
            assert!(inner.symlink_metadata().is_ok(), "the link itself is left");
            assert!(out.partial.iter().any(|s| s.contains("Linked (a link)")), "{:?}", out.partial);
            drop_link(&inner);
        }
        drop_link(&link);
    }

    /// A directory junction on Windows (no elevation needed), a symlink elsewhere.
    #[cfg(target_os = "windows")]
    fn make_link(link: &Path, target: &Path) -> bool {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .creation_flags(0x0800_0000)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    fn drop_link(link: &Path) {
        // Removes the junction only, never what it points at.
        let _ = std::fs::remove_dir(link);
    }
    #[cfg(not(target_os = "windows"))]
    fn make_link(link: &Path, target: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(not(target_os = "windows"))]
    fn drop_link(link: &Path) {
        let _ = std::fs::remove_file(link);
    }

    #[test]
    fn mac_containers_follow_the_same_rules() {
        let f = Fixture::new("mac");
        // Installed apps, read from real Info.plist files.
        let app = f.root.join("Applications").join("Acme Notes.app").join("Contents");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>com.acme.notes</string>\
             <key>CFBundleDisplayName</key><string>Acme Notes</string>\
             <key>CFBundleExecutable</key><string>AcmeNotes</string></dict></plist>",
        )
        .unwrap();
        let apps = mac_inventory_from(&[f.root.join("Applications")]);
        assert_eq!(apps.len(), 1);
        assert_eq!((apps[0].bundle_id.as_str(), apps[0].name.as_str()), ("com.acme.notes", "Acme Notes"));

        for id in ["com.acme.notes", "com.gone.app"] {
            f.file(&format!("Containers/{id}/Data/Library/Caches/c.bin"), 2000);
            f.file(&format!("Containers/{id}/Data/tmp/t.bin"), 300);
            f.file(&format!("Containers/{id}/Data/Library/Application Support/db.sqlite"), 5000);
            f.file(&format!("Containers/{id}/Data/Library/Preferences/p.plist"), 100);
            f.file(&format!("Containers/{id}/Data/Documents/d.txt"), 50);
            f.file(&format!("Containers/{id}/Container.plist"), 7);
        }
        f.file("Group Containers/ABCDE12345.com.acme.notes/Library/Caches/g.bin", 900);
        f.file("Containers/com.apple.Notes/Data/Library/Caches/n.bin", 100);
        let ctx = Ctx {
            roots: vec![(f.root.join("Containers"), Layout::Container), (f.root.join("Group Containers"), Layout::GroupContainer)],
            home: f.root.clone(),
            local_appdata: f.root.clone(),
            roaming_appdata: f.root.clone(),
            system_root: f.root.join("System"),
            inventory: Inventory::Mac(apps),
        };
        let r = scan(&ctx, &idle());
        assert_eq!(r.packages.len(), 4);

        let acme = pkg(&r, "com.acme.notes");
        assert_eq!((acme.name.as_str(), acme.state), ("Acme Notes", State::Installed));
        let caches = entry(acme, "Data/Library/Caches");
        assert_eq!((caches.safety, caches.removable, caches.mode, caches.size_bytes), (Safety::Safe, true, Mode::Contents, 2000));
        assert_eq!(entry(acme, "Data/tmp").safety, Safety::Safe);
        for rel in ["Data/Library/Application Support", "Data/Library/Preferences", "Data/Documents"] {
            assert_eq!((entry(acme, rel).safety, entry(acme, rel).removable), (Safety::Risky, false), "{rel}");
        }
        assert_eq!(acme.size_bytes, 2000 + 300 + 5000 + 100 + 50, "Container.plist sits outside Data");
    }

    #[test]
    fn containers_of_extensions_and_team_groups_belong_to_their_app() {
        let app = |id: &str, team: Option<&str>| MacApp {
            bundle_id: id.into(),
            name: id.into(),
            path: PathBuf::from(format!("/Applications/{id}.app")),
            executable: None,
            team: team.map(String::from),
        };
        let apps = vec![
            app("com.microsoft.OneDrive", Some("UBF8T346G9")),
            app("com.microsoft.Word", Some("UBF8T346G9")),
            app("net.whatsapp.WhatsApp", Some("57T9237FN3")),
        ];
        let owner = |id: &str, layout| owner_of(&apps, id, layout).map(|a| a.bundle_id.as_str());
        assert_eq!(owner("com.microsoft.OneDrive.FileProvider", Layout::Container), Some("com.microsoft.OneDrive"));
        assert_eq!(owner("group.net.whatsapp.WhatsApp.shared", Layout::GroupContainer), Some("net.whatsapp.WhatsApp"));
        assert_eq!(owner("UBF8T346G9.Office", Layout::GroupContainer).map(|a| a.starts_with("com.microsoft.")), Some(true));
        assert_eq!(owner("UBF8T346G9.com.microsoft.wordwidget", Layout::GroupContainer), Some("com.microsoft.Word"));
        // A team with no installed app, and a plain container of a removed app.
        assert_eq!(owner("JQ525L2MZD.com.adobe.GrowthSDK", Layout::GroupContainer), None);
        assert_eq!(owner("com.gone.app", Layout::Container), None);
        // A container never matches by a shorter id that merely starts the same.
        assert_eq!(owner("com.microsoft.OneDriveX", Layout::Container), None);
    }

    #[test]
    fn mac_leftovers_and_group_containers() {
        let f = Fixture::new("mac2");
        f.file("Containers/com.gone.app/Data/Library/Caches/c.bin", 2000);
        f.file("Containers/com.gone.app/Data/Library/Application Support/db", 5000);
        f.file("Containers/com.apple.Notes/Data/Library/Caches/n.bin", 100);
        f.file("Group Containers/ABCDE12345.com.acme.notes/Library/Caches/g.bin", 900);
        f.file("Group Containers/ABCDE12345.com.acme.notes/Library/Application Support/x", 20);
        let apps = vec![MacApp { bundle_id: "com.acme.notes.helper".into(), name: "Acme".into(), path: f.root.join("Acme.app"), executable: Some("acme".into()), team: None }];
        let ctx = Ctx {
            roots: vec![(f.root.join("Containers"), Layout::Container), (f.root.join("Group Containers"), Layout::GroupContainer)],
            home: f.root.clone(),
            local_appdata: f.root.clone(),
            roaming_appdata: f.root.clone(),
            system_root: f.root.join("System"),
            inventory: Inventory::Mac(apps),
        };
        let r = scan(&ctx, &running("acme"));
        let gone = pkg(&r, "com.gone.app");
        assert_eq!((gone.state, gone.safety), (State::Orphaned, Some(Safety::Caution)));
        assert!(gone.reasons[0].text.contains("/Applications"));
        assert_eq!(entry(gone, "Data/Library/Application Support").safety, Safety::Caution);
        assert_eq!(entry(gone, "Data/Library/Caches").safety, Safety::Safe);
        // Apple's apps are never "left behind".
        assert_eq!(pkg(&r, "com.apple.Notes").state, State::Installed);
        // The group container belongs to the installed app by its bundle id,
        // and that app is running.
        let group = pkg(&r, "ABCDE12345.com.acme.notes");
        assert_eq!(group.state, State::Installed);
        assert_eq!(group.running, vec!["acme"]);
        assert_eq!(entry(group, "Library/Caches").safety, Safety::Caution);
    }

    /// Read-only look at this machine's sandboxes:
    /// `cargo test --lib smoke_sandboxes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_sandboxes() {
        let start = std::time::Instant::now();
        let Some(ctx) = Ctx::system() else { return println!("no app sandboxes on this system") };
        let facts = Facts::gather();
        let listed = discover(&ctx);
        println!("{} sandboxes under {:?}", listed.len(), ctx.roots.iter().map(|r| &r.0).collect::<Vec<_>>());
        let only = std::env::var("JHARU_SMOKE_ONLY").ok();
        let report = scan_impl(&ctx, &facts, &|| false, &|d, t, c| {
            if only.is_none() && !c.is_empty() && d % 40 == 0 {
                eprintln!("  {d}/{t} {c}");
            }
        });
        println!(
            "scanned in {:?}: {:.2} GB in {} packages, {:.2} GB safe to clear, {:.2} GB left behind",
            start.elapsed(),
            report.total_bytes as f64 / 1e9,
            report.packages.len(),
            report.safe_bytes as f64 / 1e9,
            report.orphan_bytes as f64 / 1e9
        );
        for p in report.packages.iter().filter(|p| p.size_bytes >= 20 << 20) {
            println!(
                "{:>8.2} GB  {:<10?} {:<28} {}{}",
                p.size_bytes as f64 / 1e9,
                p.state,
                p.name,
                p.id,
                if p.running.is_empty() { String::new() } else { format!("  RUNNING {:?}", p.running) }
            );
            let mut all = vec![];
            all_entries(&p.entries, &mut all);
            for e in all.iter().filter(|e| e.size_bytes >= 20 << 20) {
                println!("    {:>8.2} GB  {:<8?} {:<8} {}{}", e.size_bytes as f64 / 1e9, e.safety, e.basis, e.rel, if e.removable { "  [removable]" } else { "" });
                for r in &e.reasons {
                    println!("                      {:?}: {}", r.level, r.text);
                }
            }
        }
    }
}
