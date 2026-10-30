//! Works out how safe it is to clear a Dev clean finding, and why.
//!
//! A fixed label per cache was wrong in both directions: Playwright's
//! browsers were "safe to clear" although nothing downloads them again by
//! itself, and Cargo's registry was "re-downloadable" although Cargo fetches
//! what it needs on the next build. The rating now answers four questions,
//! each from a fact about the cache or a live check of this machine, and
//! takes the worst answer:
//!
//! 1. Does it hold data that exists nowhere else?   Restore::Never -> Risky
//! 2. Does it come back by itself?   Manual step or large download -> Caution
//! 3. Does anything outside point into it?   e.g. uv in symlink mode -> Risky
//! 4. Is a program using it right now?   tool running or lock held -> Caution
//!
//! Every answer becomes a reason line the user sees, good ones included, so
//! the rating can be checked rather than taken on trust.

use crate::rules::{Restore, Rule, Safety};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
const BIN: &str = "Recycle Bin";
#[cfg(not(target_os = "windows"))]
const BIN: &str = "Trash";

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warn,
    Bad,
}

#[derive(Serialize, Clone, Debug)]
pub struct Reason {
    pub level: Level,
    pub text: String,
}

/// Live facts about this machine, gathered once per scan.
#[derive(Default)]
pub struct Facts {
    /// Running process names, lowercase, without ".exe".
    pub running: HashSet<String>,
    /// Where each running program's executable lives, to tell when a
    /// program is running from inside a folder.
    pub exes: Vec<PathBuf>,
    /// uv is configured to symlink packages into environments instead of
    /// linking or copying them, so environments would break with the cache.
    pub uv_symlink_mode: bool,
}

impl Facts {
    pub fn gather() -> Facts {
        let (running, exes) = running_processes();
        Facts { running, exes, uv_symlink_mode: uv_symlink_mode() }
    }

    /// Names of programs running from inside `dir`.
    pub fn running_from(&self, dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = self
            .exes
            .iter()
            .filter(|exe| is_under(exe, dir))
            .filter_map(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// `path` is `dir` or inside it, ignoring case where the file system does:
/// sysinfo may report `C:\Users\X\...` for a folder scanned as `c:\users\x`.
fn is_under(path: &Path, dir: &Path) -> bool {
    let key = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_string();
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            s.to_lowercase()
        } else {
            s
        }
    };
    let (p, d) = (key(path), key(dir));
    p == d || (p.starts_with(&d) && p.as_bytes().get(d.len()) == Some(&b'/'))
}

fn running_processes() -> (HashSet<String>, Vec<PathBuf>) {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    );
    let mut names = HashSet::new();
    let mut exes = vec![];
    for p in sys.processes().values() {
        let name = p.name().to_string_lossy().to_lowercase();
        names.insert(name.strip_suffix(".exe").map(String::from).unwrap_or(name));
        if let Some(exe) = p.exe() {
            // macOS: Electron apps run as "Electron" or "<App> Helper", so
            // the app's own name is that of the bundle around the program.
            if let Some(app) = app_bundle_name(exe) {
                names.insert(app);
            }
            exes.push(exe.to_path_buf());
        }
    }
    (names, exes)
}

/// The outermost `.app` bundle a program lives in, lowercased and without
/// the extension: "visual studio code" for
/// `/Applications/Visual Studio Code.app/Contents/MacOS/Electron`.
pub(crate) fn app_bundle_name(exe: &Path) -> Option<String> {
    exe.components().find_map(|c| {
        let s = c.as_os_str().to_string_lossy();
        let stem = s.strip_suffix(".app")?;
        (!stem.is_empty()).then(|| stem.to_lowercase())
    })
}

/// `UV_LINK_MODE=symlink`, or `link-mode = "symlink"` in uv's user config.
/// A project's own pyproject.toml can set it too; that is not checked.
fn uv_symlink_mode() -> bool {
    if std::env::var("UV_LINK_MODE").is_ok_and(|v| v.trim().eq_ignore_ascii_case("symlink")) {
        return true;
    }
    let mut configs: Vec<PathBuf> = vec![];
    if let Some(d) = dirs::config_dir() {
        configs.push(d.join("uv").join("uv.toml"));
    }
    if let Some(h) = dirs::home_dir() {
        configs.push(h.join(".config").join("uv").join("uv.toml"));
    }
    configs.iter().any(|c| {
        std::fs::read_to_string(c).is_ok_and(|text| {
            text.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#') && l.starts_with("link-mode") && l.contains("symlink")
            })
        })
    })
}

/// True when another program holds this lock file. Taking the lock for an
/// instant and dropping it is how every tool that uses it checks the same.
pub fn lock_held(lock: &Path) -> bool {
    let Ok(file) = std::fs::File::open(lock) else { return false };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// The rating for clearing `path`, found by `rule`, with the reasons.
pub fn assess(rule: &Rule, path: &Path, facts: &Facts) -> (Safety, Vec<Reason>) {
    let mut safety = Safety::Safe;
    let mut reasons: Vec<Reason> = vec![];
    let mut note = |level: Level, at_least: Safety, text: String| {
        if at_least > safety {
            safety = at_least;
        }
        reasons.push(Reason { level, text });
    };

    // 1 and 2: what clearing loses, and whether it comes back by itself.
    match rule.restore {
        Restore::Automatic => note(
            Level::Ok,
            Safety::Safe,
            "Comes back by itself: the tool downloads or rebuilds what it needs the next time it runs. Nothing stops working.".into(),
        ),
        Restore::Download => note(
            Level::Warn,
            Safety::Caution,
            "Downloaded again automatically the next time it is used, but these are large files, so expect a long download then.".into(),
        ),
        Restore::Manual(how) => note(
            Level::Warn,
            Safety::Caution,
            format!("Does not come back by itself: anything that uses it fails until you {how}."),
        ),
        Restore::Never(what) => note(
            Level::Bad,
            Safety::Risky,
            format!("Holds {what}. Jharu cannot re-create this: once the {BIN} is emptied, it is gone."),
        ),
    }

    // 3: whether something outside depends on what is inside.
    if rule.id == "uv" {
        if facts.uv_symlink_mode {
            note(
                Level::Bad,
                Safety::Risky,
                "uv is set to link-mode = symlink, so your environments point into this cache and would stop working.".into(),
            );
        } else {
            note(
                Level::Ok,
                Safety::Safe,
                "Your environments hold their own links to these files (uv's default), so they keep working.".into(),
            );
        }
    }

    // 4: whether a program is using it at this moment.
    let running: Vec<&str> = rule.tools.iter().copied().filter(|t| facts.running.contains(*t)).collect();
    if !running.is_empty() {
        note(
            Level::Warn,
            Safety::Caution,
            format!(
                "{} is running right now and may be using it. Close it first; otherwise Jharu leaves everything in place.",
                running.join(", ")
            ),
        );
    } else if rule.lock.is_some_and(|l| lock_held(&path.join(l))) {
        note(
            Level::Warn,
            Safety::Caution,
            "Its lock file is held, so a program is working in it right now. Try again when it finishes.".into(),
        );
    } else if let Some(tool) = rule.tools.first() {
        note(Level::Ok, Safety::Safe, format!("Not in use right now: {tool} is not running."));
    }

    (safety, reasons)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{all_rules, Category};

    fn rule(restore: Restore, tools: &'static [&'static str], lock: Option<&'static str>) -> Rule {
        Rule { id: "test", name: "Test", description: "", category: Category::DevCache, restore, tools, lock }
    }

    fn running(names: &[&str]) -> Facts {
        Facts { running: names.iter().map(|s| s.to_string()).collect(), exes: vec![], uv_symlink_mode: false }
    }

    #[test]
    fn a_program_is_also_known_by_its_app_bundle() {
        let name = |p: &str| app_bundle_name(Path::new(p));
        assert_eq!(name("/Applications/Visual Studio Code.app/Contents/MacOS/Electron").as_deref(), Some("visual studio code"));
        // Helpers sit in bundles inside the app; the outermost one names it.
        assert_eq!(
            name("/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Renderer).app/Contents/MacOS/Cursor Helper (Renderer)").as_deref(),
            Some("cursor")
        );
        assert_eq!(name("/usr/local/bin/aider"), None);
        assert_eq!(name("/Users/me/my.app.data/tool"), None);
    }

    #[test]
    fn automatic_and_idle_is_safe() {
        let (s, reasons) = assess(&rule(Restore::Automatic, &["cargo"], None), Path::new("."), &running(&[]));
        assert_eq!(s, Safety::Safe);
        assert!(reasons.iter().all(|r| r.level == Level::Ok));
        assert!(reasons.iter().any(|r| r.text.contains("cargo is not running")));
    }

    #[test]
    fn tool_running_lowers_to_caution() {
        let (s, reasons) = assess(&rule(Restore::Automatic, &["cargo"], None), Path::new("."), &running(&["cargo"]));
        assert_eq!(s, Safety::Caution);
        assert!(reasons.iter().any(|r| r.level == Level::Warn && r.text.contains("cargo is running")));
    }

    #[test]
    fn manual_restore_is_caution_and_names_the_command() {
        let (s, reasons) = assess(&rule(Restore::Manual("npx playwright install"), &[], None), Path::new("."), &running(&[]));
        assert_eq!(s, Safety::Caution);
        assert!(reasons[0].text.contains("npx playwright install"));
    }

    #[test]
    fn unique_data_is_risky_even_when_idle() {
        let (s, _) = assess(&rule(Restore::Never("volumes"), &["dockerd"], None), Path::new("."), &running(&[]));
        assert_eq!(s, Safety::Risky);
    }

    #[test]
    fn uv_in_symlink_mode_is_risky() {
        let uv = all_rules().into_iter().find(|r| r.id == "uv").unwrap();
        let (s, _) = assess(&uv, Path::new("."), &running(&[]));
        assert_eq!(s, Safety::Safe);
        let facts = Facts { uv_symlink_mode: true, ..running(&[]) };
        let (s, reasons) = assess(&uv, Path::new("."), &facts);
        assert_eq!(s, Safety::Risky);
        assert!(reasons.iter().any(|r| r.level == Level::Bad && r.text.contains("symlink")));
    }

    #[test]
    fn held_lock_file_means_in_use() {
        let dir = std::env::temp_dir().join(format!("jharu safety {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join(".lock");
        std::fs::write(&lock, b"").unwrap();
        let r = rule(Restore::Automatic, &[], Some(".lock"));

        let holder = std::fs::File::open(&lock).unwrap();
        holder.lock().unwrap();
        assert_eq!(assess(&r, &dir, &running(&[])).0, Safety::Caution, "a held lock means in use");
        holder.unlock().unwrap();
        assert_eq!(assess(&r, &dir, &running(&[])).0, Safety::Safe, "a free lock file is fine");

        drop(holder);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ratings the rules produce on an idle machine, so a change to a
    /// rule's facts shows up here.
    #[test]
    fn idle_ratings_per_rule() {
        let idle = running(&[]);
        let rating = |id: &str| {
            let r = all_rules().into_iter().find(|r| r.id == id).unwrap();
            assess(&r, Path::new("."), &idle).0
        };
        for id in ["pip", "uv", "npm", "yarn", "pnpm", "cargo-registry", "go-mod", "gradle", "maven", "nuget", "conda-pkgs", "jetbrains"] {
            assert_eq!(rating(id), Safety::Safe, "{id}");
        }
        for id in ["playwright", "puppeteer", "ollama", "lm-studio", "hf-hub", "torch"] {
            assert_eq!(rating(id), Safety::Caution, "{id}");
        }
        for id in ["docker", "conda-envs", "xcode-archives"] {
            assert_eq!(rating(id), Safety::Risky, "{id}");
        }
    }
}
