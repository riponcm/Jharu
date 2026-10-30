//! Whether it is safe to remove a folder Deep clean shows, and why.
//!
//! Deep clean lists every folder on the disk, far more than any list of
//! known caches covers. So the answer comes from three layers, strongest
//! first, and every answer says which layer it came from:
//!
//! 1. Known: the folder is a location Jharu knows, either one of Dev clean's
//!    cache rules (rated exactly as Dev clean rates it) or an entry in the
//!    table below (.rustup is a toolchain, .ssh holds keys, ...).
//! 2. Detected: what the measuring walk found inside. A CACHEDIR.TAG file is
//!    a tool declaring its folder a rebuildable cache; git repositories are
//!    source code; documents and media are the user's files; and so on.
//! 3. Live: a program running from inside the folder means it is in use.
//!
//! Nothing matched means "Unknown — look inside first", never "safe".

use crate::rules::{all_rules, Category, Restore, Rule, Safety};
use crate::safety::{assess, Facts, Level, Reason};
use serde::Serialize;
use std::path::Path;

/// What one folder holds, gathered while Deep clean measures it.
#[derive(Default, Debug, Clone, Copy)]
pub struct Signals {
    pub total: u64,
    /// Bytes under a folder carrying a CACHEDIR.TAG.
    pub cache: u64,
    /// Documents, photos, audio and video.
    pub personal: u64,
    pub models: u64,
    pub programs: u64,
    pub databases: u64,
    pub repos: u64,
    /// Of the repositories git was asked about (see RepoTally), how many
    /// hold nothing that exists only here, and how many have each kind of
    /// local-only work. Repositories not asked about count as unknown.
    pub repos_backed_up: u64,
    pub repos_uncommitted: u64,
    pub repos_unpushed: u64,
    pub repos_no_remote: u64,
    pub envs: u64,
}

impl Signals {
    /// Adds what git said about one repository found inside.
    pub fn add_repo(&mut self, c: &crate::worktrees::RepoCheck) {
        if !c.readable {
            return;
        }
        if c.backed_up() {
            self.repos_backed_up += 1;
        }
        if c.uncommitted > 0 || c.stashed {
            self.repos_uncommitted += 1;
        }
        if c.unpushed > 0 && c.has_remote {
            self.repos_unpushed += 1;
        }
        if !c.has_remote {
            self.repos_no_remote += 1;
        }
    }
}

/// Which kind of file this is, by extension.
pub enum FileKind {
    Personal,
    Model,
    Program,
    Database,
    Other,
}

pub fn file_kind(path: &Path) -> FileKind {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf" | "pages"
        | "numbers" | "key" | "jpg" | "jpeg" | "png" | "heic" | "gif" | "tif" | "tiff" | "raw" | "cr2" | "nef"
        | "arw" | "dng" | "psd" | "ai" | "indd" | "mp4" | "mov" | "mkv" | "avi" | "wmv" | "m4v" | "mp3" | "wav"
        | "flac" | "m4a" | "aac" | "ogg" | "aif" | "aiff" | "tex" | "bib" | "eml" | "msg" => FileKind::Personal,
        "safetensors" | "gguf" | "ggml" | "ckpt" | "pt" | "pth" | "onnx" | "h5" | "tflite" | "keras" | "mlmodel" => {
            FileKind::Model
        }
        "exe" | "dll" | "sys" | "msi" | "so" | "dylib" => FileKind::Program,
        "sqlite" | "sqlite3" | "db" | "ldb" | "mdb" | "accdb" => FileKind::Database,
        _ => FileKind::Other,
    }
}

impl Signals {
    pub fn add_file(&mut self, path: &Path, bytes: u64, in_cache: bool) {
        self.total += bytes;
        if in_cache {
            self.cache += bytes;
        }
        match file_kind(path) {
            FileKind::Personal => self.personal += bytes,
            FileKind::Model => self.models += bytes,
            FileKind::Program => self.programs += bytes,
            FileKind::Database => self.databases += bytes,
            FileKind::Other => {}
        }
    }
}

/// How a known location behaves.
enum Kind {
    Item(Restore),
    /// Holds several kinds of data; the user should open it and decide per part.
    Mixed(&'static str),
    /// Cleaned the right way from another tab.
    Elsewhere(&'static str),
    /// Must not be removed here; the text says what to do instead.
    Advice(&'static str),
}

struct Place {
    /// Relative to the home folder, with "/" separators.
    rel: &'static str,
    what: &'static str,
    kind: Kind,
    tools: &'static [&'static str],
}

fn places() -> Vec<Place> {
    use Kind::*;
    use Restore::*;
    let p = |rel, what, kind, tools| Place { rel, what, kind, tools };
    vec![
        p(".ssh", "Your SSH keys", Item(Never("keys that give you access to servers and Git hosts")), &[]),
        p(".gnupg", "Your GPG keys", Item(Never("encryption and signing keys")), &[]),
        p(".aws", "AWS credentials", Item(Never("cloud credentials and settings")), &[]),
        p(".azure", "Azure credentials", Item(Never("cloud credentials and settings")), &[]),
        p(".kube", "Kubernetes credentials", Item(Never("cluster credentials")), &[]),
        p(".docker", "Docker settings", Item(Never("registry logins and settings")), &[]),
        p(".rustup", "Rust toolchains", Item(Manual("reinstall Rust with rustup")), &["rustc", "cargo"]),
        p(".cargo", "Rust tools and downloads", Mixed("installed Rust tools (bin) and downloaded crates (registry)"), &["cargo"]),
        p(".claude", "Claude Code data", Item(Never("your Claude Code chat history, settings and project memory")), &["claude"]),
        p(".codex", "Codex data", Item(Never("your Codex chat history and settings")), &["codex"]),
        p(".gemini", "Gemini CLI data", Item(Never("its settings and history")), &["gemini"]),
        p(".grok", "Grok CLI data", Item(Never("its settings and history")), &[]),
        p(".projectmem", "projectmem data", Item(Never("projectmem's memory of your projects")), &[]),
        p(".vscode", "VS Code extensions", Item(Manual("reinstall your extensions in VS Code")), &["code"]),
        p(".vscode-insiders", "VS Code Insiders extensions", Item(Manual("reinstall your extensions")), &["code - insiders"]),
        p(".cursor", "Cursor extensions", Item(Manual("reinstall your extensions in Cursor")), &["cursor"]),
        p(".windsurf", "Windsurf extensions", Item(Manual("reinstall your extensions in Windsurf")), &["windsurf"]),
        p(".codeium", "Codeium and Windsurf data", Mixed("the Codeium language server (downloaded again when needed) beside Windsurf's chat history and memories"), &["windsurf"]),
        p(".continue", "Continue data", Item(Never("your Continue chat sessions and settings")), &["code"]),
        p(".venvs", "Python environments", Item(Manual("recreate each environment and reinstall its packages")), &[]),
        p(".virtualenvs", "Python environments", Item(Manual("recreate each environment and reinstall its packages")), &[]),
        p(".u2net", "rembg background-removal model", Item(Download), &[]),
        p(".pkuseg", "pkuseg text-segmentation model", Item(Download), &[]),
        p(".cache", "Caches from many tools", Mixed("caches from several tools, some in use"), &[]),
        p(".julia", "Julia packages and environments", Mixed("your Julia environments, packages and compiled code"), &["julia"]),
        p(".julia/compiled", "Julia precompiled code", Item(Automatic), &["julia"]),
        p(".lmstudio", "LM Studio data", Mixed("LM Studio's models, engines and settings"), &["lm studio"]),
        p(".lmstudio/extensions", "LM Studio engines", Item(Manual("download the engine again in LM Studio")), &["lm studio"]),
        p(".ollama", "Ollama data", Mixed("Ollama's models and its private key"), &["ollama"]),
        p(".conda", "Conda data", Mixed("conda environments and its package cache"), &["conda"]),
        p(".m2", "Maven data", Mixed("Maven's downloads and your settings.xml"), &[]),
        p(".gradle", "Gradle data", Mixed("Gradle's caches and your gradle.properties, which can hold credentials"), &[]),
        p(".nuget", "NuGet data", Mixed("NuGet's package cache and settings"), &["dotnet"]),
        p(".android", "Android SDK data", Mixed("emulator images with their data, and your debug signing key"), &[]),
        p(".npm", "npm cache and logs", Item(Automatic), &[]),
        p(".config", "App settings", Mixed("settings of many command-line tools"), &[]),
        p("AppData", "App data", Mixed("settings and data of all your apps"), &[]),
        p("AppData/Local", "App data (this PC)", Mixed("data of many apps"), &[]),
        p("AppData/Roaming", "App data (roaming)", Mixed("settings of many apps"), &[]),
        p("AppData/Local/Temp", "Temporary files", Elsewhere("Junk files, which keeps the folder and skips files in use"), &[]),
        p("AppData/Local/Packages", "Windows Store app data", Mixed("data of apps from the Microsoft Store, including Claude's and Codex's"), &[]),
        p("AppData/Local/Programs", "Installed programs", Elsewhere("Uninstaller, which also removes their leftovers"), &[]),
        p("AppData/Local/Microsoft", "Windows and Office data", Item(Never("Windows and Office settings and data")), &[]),
        p("AppData/Local/Google", "Chrome data", Elsewhere("Browsers, which separates cache from your profile"), &["chrome"]),
        p("AppData/Local/BraveSoftware", "Brave data", Elsewhere("Browsers, which separates cache from your profile"), &["brave"]),
        p("AppData/Roaming/Mozilla", "Firefox data", Elsewhere("Browsers, which separates cache from your profile"), &["firefox"]),
        p("Documents", "Your documents", Item(Never("your documents")), &[]),
        p("Desktop", "Your desktop", Item(Never("files on your desktop")), &[]),
        p("Pictures", "Your pictures", Item(Never("your pictures")), &[]),
        p("Videos", "Your videos", Item(Never("your videos")), &[]),
        p("Movies", "Your movies", Item(Never("your videos")), &[]),
        p("Music", "Your music", Item(Never("your music")), &[]),
        p("Downloads", "Your downloads", Mixed("files you downloaded; Junk files lists the installers and archives among them"), &[]),
        p("OneDrive", "OneDrive", Advice("Synced with OneDrive: removing files here removes them from the cloud and your other devices too. To free space without losing anything, right-click it in Explorer and choose \"Free up space\"."), &["onedrive"]),
        p("Library", "App data", Mixed("settings and data of all your apps"), &[]),
        p("Library/Caches", "App caches", Mixed("caches of many apps"), &[]),
        p("Library/Application Support", "App data", Mixed("data of many apps"), &[]),
        // macOS
        p(".Trash", "Trash", Elsewhere("the Trash tab, which shows what is in it and empties it"), &[]),
        p("Applications", "Your installed apps", Elsewhere("Uninstaller, which also finds each app's leftovers"), &[]),
        p("Library/Containers", "Sandboxed app data", Mixed("the data of each sandboxed app; App sandboxes lists those whose app is gone"), &[]),
        p("Library/Group Containers", "Shared app data", Mixed("data that apps from one developer share, such as Office and messaging apps"), &[]),
        p("Library/Mobile Documents", "iCloud Drive", Advice("Synced with iCloud Drive: removing files here removes them from iCloud and your other devices too. To free space without losing anything, choose \"Remove Download\" in Finder or turn on Optimize Mac Storage."), &[]),
        p("Library/CloudStorage", "Cloud drives", Advice("OneDrive, Google Drive, Dropbox and other synced folders: removing files here removes them from the cloud too. Use \"Remove Download\" or the service's \"Free up space\" in Finder instead."), &[]),
        p("Library/Developer", "Xcode and simulator data", Mixed("simulators with their apps' data, build data and device support files; Dev clean lists the parts that are safe to clear"), &["xcode", "simulator"]),
    ]
}

/// Locations outside the home folder, by absolute path.
fn system_places() -> Vec<(std::path::PathBuf, &'static str, Kind)> {
    use Kind::*;
    let mut v = vec![];
    #[cfg(target_os = "windows")]
    {
        let drive = std::path::PathBuf::from(format!("{}\\", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())));
        let uninstall = "Uninstaller, which runs each program's own uninstaller and finds its leftovers";
        v.push((drive.join("Program Files"), "Installed programs", Elsewhere(uninstall)));
        v.push((drive.join("Program Files (x86)"), "Installed programs (32-bit)", Elsewhere(uninstall)));
        v.push((
            drive.join("ProgramData"),
            "Data shared by all apps",
            Advice("Settings, licences and data that installed apps share between users. Removing it breaks those apps; clean their caches from Junk files instead."),
        ));
        v.push((
            drive.join("$SysReset"),
            "Windows reset logs",
            Advice("Logs Windows kept from a reset or upgrade. Windows' own Disk Cleanup removes them safely."),
        ));
    }
    #[cfg(target_os = "macos")]
    {
        v.push((std::path::PathBuf::from("/Applications"), "Installed apps", Elsewhere("Uninstaller, which also finds each app's leftovers")));
    }
    v
}

fn key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Verdict {
    pub safety: Safety,
    /// "known", "detected" or "unknown": how sure the rating is.
    pub basis: &'static str,
    /// What the folder is, when known.
    pub what: Option<String>,
    pub reasons: Vec<Reason>,
    /// False where another tab cleans this properly and removing the whole
    /// folder here would do harm (the Temp folder, installed programs).
    pub clean_here: bool,
}

fn gb(b: u64) -> String {
    // Explorer's units on Windows, Finder's elsewhere, like the UI.
    let unit: f64 = if cfg!(target_os = "windows") { 1024f64.powi(3) } else { 1e9 };
    let v = b as f64 / unit;
    if v >= 1.0 {
        format!("{v:.1} GB")
    } else {
        format!("{:.0} MB", v * 1024.0)
    }
}

/// Rates removing `path` (a file or folder Deep clean measured).
pub fn verdict(path: &Path, home: &Path, s: &Signals, facts: &Facts) -> Verdict {
    let mut v = known(path, home, facts).unwrap_or_else(|| detected(s));

    // Live: a program running from inside overrides "safe". Skipped when the
    // known-location check already said its tool is running, so one program
    // is not reported twice.
    let running = facts.running_from(path);
    let already = v.reasons.iter().any(|r| r.level == Level::Warn && r.text.contains("is running"));
    if !running.is_empty() && !already {
        if v.safety < Safety::Caution {
            v.safety = Safety::Caution;
        }
        let names = if running.len() > 3 {
            format!("{} and {} more programs are", running[..3].join(", "), running.len() - 3)
        } else if running.len() == 1 {
            format!("{} is", running[0])
        } else {
            format!("{} are", running.join(", "))
        };
        let them = if running.len() == 1 { "it" } else { "them" };
        v.reasons.push(Reason { level: Level::Warn, text: format!("{names} running from this folder right now. Close {them} first.") });
    }

    // Detected facts that overturn a known location's milder rating: a git
    // repository or personal files inside a cache. Skipped where the rating
    // is already "review first", where an app's own icons and media inside
    // Program Files would only read as a false alarm.
    if v.basis == "known" && v.safety < Safety::Risky {
        if s.repos > 0 {
            let (safety, reasons) = repo_reasons(s);
            v.reasons.extend(reasons);
            v.safety = v.safety.max(safety);
        }
        if s.personal >= 500 << 20 {
            v.reasons.push(Reason {
                level: Level::Bad,
                text: format!("Contains {} of documents, photos or media.", gb(s.personal)),
            });
            v.safety = Safety::Risky;
        }
    }
    v
}

fn repos_word(n: u64) -> String {
    if n == 1 { "a git repository".into() } else { format!("{n} git repositories") }
}

/// What git said about the repositories inside. Work that exists only here
/// (uncommitted, unpushed, stashed, or no remote at all) means review first;
/// repositories whose every commit is on a remote are not a reason on their
/// own, but the files git ignores in them (environments, model weights,
/// data) still exist only here.
fn repo_reasons(s: &Signals) -> (Safety, Vec<Reason>) {
    let at_risk = s.repos.saturating_sub(s.repos_backed_up);
    if at_risk == 0 {
        return (
            Safety::Caution,
            vec![
                Reason {
                    level: Level::Ok,
                    text: format!(
                        "Contains {}; everything committed in {} is also on a remote, so the code can be cloned again.",
                        repos_word(s.repos),
                        if s.repos == 1 { "it" } else { "them" }
                    ),
                },
                Reason {
                    level: Level::Warn,
                    text: "Files git ignores, such as environments, model weights and data, exist only here.".into(),
                },
            ],
        );
    }
    let mut parts = vec![];
    let count = |n: u64, what: &str| if s.repos == 1 { what.to_string() } else { format!("{n} {what}") };
    if s.repos_uncommitted > 0 {
        parts.push(count(s.repos_uncommitted, "with uncommitted or stashed changes"));
    }
    if s.repos_unpushed > 0 {
        parts.push(count(s.repos_unpushed, "with commits not pushed"));
    }
    if s.repos_no_remote > 0 {
        parts.push(count(s.repos_no_remote, "with no remote at all"));
    }
    let checked = s.repos_backed_up + s.repos_uncommitted.max(s.repos_unpushed + s.repos_no_remote);
    let unknown = s.repos.saturating_sub(checked.min(s.repos));
    let text = if parts.is_empty() {
        format!("Contains {}, and Jharu could not check whether {} work that exists nowhere else.", repos_word(s.repos), if s.repos == 1 { "it holds" } else { "they hold" })
    } else if s.repos == 1 {
        format!("Contains a git repository {}: that work exists only here.", parts.join(" and "))
    } else {
        let more = if unknown > 0 { format!("; {unknown} could not be checked") } else { String::new() };
        format!("Contains {} git repositories with work that exists only here: {}{more}.", s.repos, parts.join(", "))
    };
    (Safety::Risky, vec![Reason { level: Level::Bad, text }])
}

fn known(path: &Path, home: &Path, facts: &Facts) -> Option<Verdict> {
    let k = key(path);

    // Dev clean's rules first, so both tabs give the same answer.
    for rule in all_rules() {
        if rule.paths().iter().any(|p| key(p) == k) {
            let (safety, reasons) = assess(&rule, path, facts);
            return Some(Verdict { safety, basis: "known", what: Some(rule.name.to_string()), reasons, clean_here: true });
        }
    }

    if let Some((_, what, kind)) = system_places().into_iter().find(|(p, _, _)| key(p) == k) {
        let clean_here = !matches!(kind, Kind::Elsewhere(_) | Kind::Advice(_));
        let reason = match kind {
            Kind::Elsewhere(tab) => format!("Clean this from {tab}. Removing the whole folder here would do harm."),
            Kind::Advice(text) | Kind::Mixed(text) => text.to_string(),
            Kind::Item(_) => String::new(),
        };
        return Some(Verdict {
            safety: Safety::Risky,
            basis: "known",
            what: Some(what.to_string()),
            reasons: vec![Reason { level: Level::Bad, text: reason }],
            clean_here,
        });
    }

    let rel = path.strip_prefix(home).ok()?;
    let rel = key(rel);
    let place = places().into_iter().find(|p| key(Path::new(p.rel)) == rel)?;
    let clean_here = !matches!(place.kind, Kind::Elsewhere(_) | Kind::Advice(_));
    let (safety, reasons) = match place.kind {
        Kind::Item(restore) => {
            let rule = Rule {
                id: "place",
                name: place.what,
                description: "",
                category: Category::AppData,
                restore,
                tools: place.tools,
                lock: None,
            };
            assess(&rule, path, facts)
        }
        Kind::Mixed(what) => (
            Safety::Risky,
            vec![Reason {
                level: Level::Warn,
                text: format!("Holds {what}. Open it and decide part by part; removing it whole would take the parts you need too."),
            }],
        ),
        Kind::Advice(text) => (Safety::Risky, vec![Reason { level: Level::Bad, text: text.into() }]),
        Kind::Elsewhere(tab) => (
            Safety::Risky,
            vec![Reason { level: Level::Warn, text: format!("Clean this from {tab}. Removing the whole folder here would do harm.") }],
        ),
    };
    Some(Verdict { safety, basis: "known", what: Some(place.what.to_string()), reasons, clean_here })
}

/// Rating from contents alone, for folders Jharu has no entry for.
fn detected(s: &Signals) -> Verdict {
    let share = |part: u64| if s.total == 0 { 0.0 } else { part as f64 / s.total as f64 };
    let mut reasons = vec![];
    let mut safety = Safety::Risky;
    let mut basis = "detected";

    let (repo_safety, repo_lines) = if s.repos > 0 { repo_reasons(s) } else { (Safety::Safe, vec![]) };
    if repo_safety == Safety::Risky {
        reasons.extend(repo_lines.clone());
    } else if share(s.personal) >= 0.2 || s.personal >= 1 << 30 {
        reasons.push(Reason {
            level: Level::Bad,
            text: format!("Holds your own files: {} of documents, photos, audio or video.", gb(s.personal)),
        });
    } else if s.envs > 0 {
        safety = Safety::Caution;
        reasons.push(Reason {
            level: Level::Warn,
            text: format!(
                "Contains {}. Removing means recreating {} and reinstalling {} packages.",
                if s.envs == 1 { "a Python environment".to_string() } else { format!("{} Python environments", s.envs) },
                if s.envs == 1 { "it" } else { "them" },
                if s.envs == 1 { "its" } else { "their" }
            ),
        });
    } else if s.total > 0 && share(s.cache) >= 0.9 {
        safety = Safety::Safe;
        reasons.push(Reason {
            level: Level::Ok,
            text: "Marked as a cache (CACHEDIR.TAG) by the tool that created it, which rebuilds it when needed.".into(),
        });
    } else if share(s.models) >= 0.7 {
        safety = Safety::Caution;
        reasons.push(Reason {
            level: Level::Warn,
            text: format!(
                "Mostly model weights ({}). Usually downloadable again; check where they came from before removing.",
                gb(s.models)
            ),
        });
    } else if share(s.programs) >= 0.5 {
        reasons.push(Reason {
            level: Level::Bad,
            text: if cfg!(windows) {
                "Mostly programs (.exe, .dll): an installed app or tool. Remove it with its uninstaller instead.".into()
            } else {
                "Mostly programs and libraries: an installed app or tool. Remove it with its uninstaller instead.".into()
            },
        });
    } else if s.databases > 0 {
        reasons.push(Reason {
            level: Level::Bad,
            text: "Holds app databases: data an app keeps, possibly your only copy.".into(),
        });
    } else {
        basis = "unknown";
        reasons.push(Reason {
            level: Level::Warn,
            text: "Jharu does not recognise this folder. Look inside before removing it.".into(),
        });
    }
    // Repositories whose work is all on a remote: say so first, and never
    // rate the folder better than "with care", since what git ignores in
    // them is not on the remote.
    if repo_safety == Safety::Caution {
        if basis == "unknown" {
            basis = "detected";
            reasons.clear();
            safety = Safety::Caution;
        }
        safety = safety.max(Safety::Caution);
        reasons.splice(0..0, repo_lines);
    }
    Verdict { safety, basis, what: None, reasons, clean_here: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn idle() -> Facts {
        Facts::default()
    }

    fn home() -> PathBuf {
        PathBuf::from(if cfg!(windows) { "C:\\Users\\test" } else { "/home/test" })
    }

    #[test]
    fn known_places_rate_as_listed() {
        let h = home();
        let v = |rel: &str| verdict(&h.join(rel), &h, &Signals::default(), &idle());
        assert_eq!(v(".ssh").safety, Safety::Risky);
        assert_eq!(v(".rustup").safety, Safety::Caution);
        assert_eq!(v(".u2net").safety, Safety::Caution);
        assert_eq!(v(".npm").safety, Safety::Safe);
        let cache = v(".cache");
        assert_eq!((cache.safety, cache.basis), (Safety::Risky, "known"));
        assert!(cache.reasons[0].text.contains("part by part"));
        let temp = v("AppData/Local/Temp");
        assert!(temp.reasons[0].text.contains("Junk files"));
        assert!(!temp.clean_here, "Temp is cleaned from Junk files, never removed whole");
        let onedrive = v("OneDrive");
        assert!(!onedrive.clean_here && onedrive.reasons[0].text.contains("Free up space"));
        // macOS: the Trash and synced folders are handled elsewhere, even
        // with a git repository inside.
        let with_repo = Signals { total: 1 << 30, repos: 1, ..Signals::default() };
        let trash = verdict(&h.join(".Trash"), &h, &with_repo, &idle());
        assert!(!trash.clean_here && trash.reasons.iter().all(|r| !r.text.contains("git")), "{trash:?}");
        let icloud = verdict(&h.join("Library/Mobile Documents"), &h, &with_repo, &idle());
        assert!(!icloud.clean_here && icloud.safety == Safety::Risky);
        assert!(!v("Library/CloudStorage").clean_here);
        assert_eq!(v("Library/Group Containers").safety, Safety::Risky);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn installed_programs_are_never_cleaned_here() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        // Program Files is full of icons and media; that must not read as
        // "your own files", and it must not get a Clean button.
        let photos = Signals { total: 100, personal: 50, ..Default::default() };
        let v = verdict(&Path::new(&format!("{drive}\\Program Files")), &home(), &photos, &idle());
        assert_eq!((v.safety, v.basis, v.clean_here), (Safety::Risky, "known", false));
        assert!(v.reasons[0].text.contains("Uninstaller"));
    }

    #[test]
    fn contents_decide_unknown_folders() {
        let h = home();
        let p = h.join("some tool");
        let rate = |s: Signals| verdict(&p, &h, &s, &idle());

        let cache = rate(Signals { total: 100, cache: 95, ..Default::default() });
        assert_eq!((cache.safety, cache.basis), (Safety::Safe, "detected"));

        let repo = rate(Signals { total: 100, cache: 100, repos: 2, ..Default::default() });
        assert_eq!(repo.safety, Safety::Risky, "a repository outweighs a cache tag");

        // What git said decides it. A clean clone of a public project with a
        // Python environment beside it is "with care".
        let clean = rate(Signals { total: 100, envs: 1, repos: 1, repos_backed_up: 1, ..Default::default() });
        assert_eq!((clean.safety, clean.basis), (Safety::Caution, "detected"));
        assert!(clean.reasons[0].level == Level::Ok && clean.reasons[0].text.contains("cloned again"), "{:?}", clean.reasons);
        assert!(clean.reasons.iter().any(|r| r.text.contains("Python environment")));
        let only_clean = rate(Signals { total: 100, repos: 2, repos_backed_up: 2, ..Default::default() });
        assert_eq!((only_clean.safety, only_clean.basis), (Safety::Caution, "detected"), "not 'unknown': the repositories explain it");
        let tagged = rate(Signals { total: 100, cache: 100, repos: 1, repos_backed_up: 1, ..Default::default() });
        assert_eq!(tagged.safety, Safety::Caution, "ignored files in a repository keep it from 'safe'");

        let local = rate(Signals { total: 100, repos: 1, repos_uncommitted: 1, ..Default::default() });
        assert_eq!(local.safety, Safety::Risky);
        assert!(local.reasons[0].text.contains("uncommitted"), "{:?}", local.reasons);
        let mixed = rate(Signals { total: 100, repos: 3, repos_backed_up: 1, repos_unpushed: 1, ..Default::default() });
        assert_eq!(mixed.safety, Safety::Risky);
        assert!(mixed.reasons[0].text.contains("1 with commits not pushed") && mixed.reasons[0].text.contains("1 could not be checked"), "{:?}", mixed.reasons);

        let photos = rate(Signals { total: 100, personal: 40, ..Default::default() });
        assert_eq!(photos.safety, Safety::Risky);

        let models = rate(Signals { total: 100, models: 80, ..Default::default() });
        assert_eq!(models.safety, Safety::Caution);

        let program = rate(Signals { total: 100, programs: 60, ..Default::default() });
        assert_eq!(program.safety, Safety::Risky);

        let unknown = rate(Signals { total: 100, ..Default::default() });
        assert_eq!((unknown.safety, unknown.basis), (Safety::Risky, "unknown"));
    }

    #[test]
    fn running_program_inside_means_in_use() {
        let h = home();
        let dir = h.join("cache-like");
        let facts = Facts { exes: vec![dir.join("bin").join("node.exe")], ..Default::default() };
        let v = verdict(&dir, &h, &Signals { total: 10, cache: 10, ..Default::default() }, &facts);
        assert_eq!(v.safety, Safety::Caution);
        assert!(v.reasons.iter().any(|r| r.text.contains("node.exe is running")));

        let many = Facts { exes: (0..6).map(|i| dir.join(format!("p{i}.exe"))).collect(), ..Default::default() };
        let v = verdict(&dir, &h, &Signals::default(), &many);
        assert!(v.reasons.iter().any(|r| r.text.contains("and 3 more programs are running")));
    }

    #[test]
    fn dev_clean_rules_rate_the_same_in_deep_clean() {
        let h = dirs::home_dir().unwrap();
        let rule = all_rules().into_iter().find(|r| r.id == "playwright").unwrap();
        let path = rule.paths().into_iter().next().unwrap();
        let v = verdict(&path, &h, &Signals::default(), &idle());
        assert_eq!(v.safety, assess(&rule, &path, &idle()).0);
        assert_eq!(v.what.as_deref(), Some("Playwright browsers"));
    }
}
