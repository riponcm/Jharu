//! A journal of everything Jharu sends to the Recycle Bin / Trash.
//!
//! The bin itself only knows a file's name, folder and deletion time. It does
//! not know that Jharu was the one that put it there, or from which tab, so
//! the Recycle Bin tab could neither tell Jharu's own items from the user's
//! nor offer a one-click "put it back" for what a cleaning just moved.
//!
//! Every place that trashes something calls [`record`] right after the move
//! succeeded. One JSON object per line is appended to
//! `<local app data>/Jharu/trash-log.jsonl`; a line is written with a single
//! call, so two cleanings running at once cannot interleave half-lines, and a
//! damaged line is skipped on reading instead of losing the rest.
//!
//! The journal is only a hint. Nothing is deleted or restored on its word:
//! the bin tab matches each entry against what is really in the bin, and a
//! journal that is missing or unwritable costs nothing but the labels.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Entries kept. Older ones are dropped when the file grows past
/// [`MAX_ENTRIES`] plus [`SLACK`], so the file is not rewritten on every call.
const MAX_ENTRIES: usize = 20_000;
const SLACK: usize = 2_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the item was before it was moved.
    pub path: String,
    /// Bytes the move freed (for folders with hard links, what really freed).
    pub size: u64,
    /// Unix seconds, just after the move.
    pub time: u64,
    /// The tab or feature that did it, e.g. "Dev clean".
    pub source: String,
}

/// Serializes appends against the occasional rewrite that trims the file.
static LOCK: Mutex<()> = Mutex::new(());

#[cfg(not(test))]
fn journal_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Jharu").join("trash-log.jsonl"))
}

// Tests of other modules trash fixture files through the same code paths;
// they must not write into the user's real journal. A test that wants the
// journal points this thread at a temp file; all others record nothing.
#[cfg(test)]
thread_local! {
    pub static TEST_JOURNAL: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn journal_path() -> Option<PathBuf> {
    TEST_JOURNAL.with(|p| p.borrow().clone())
}

/// Moves paths to the Trash / Recycle Bin, all at once.
///
/// Cleaning code goes through here rather than calling the `trash` crate, so
/// tests on macOS never fill the user's real Trash: there the fixture is
/// deleted instead. Tests on Windows and Linux use the real bin and put
/// their items back, which macOS offers no API for.
#[cfg(not(all(test, target_os = "macos")))]
pub fn to_bin<P: AsRef<Path>>(paths: &[P]) -> Result<(), String> {
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
    trash::delete_all(refs).map_err(|e| e.to_string())
}

#[cfg(all(test, target_os = "macos"))]
pub fn to_bin<P: AsRef<Path>>(paths: &[P]) -> Result<(), String> {
    for p in paths {
        let p = p.as_ref();
        let meta = p.symlink_metadata().map_err(|e| format!("{}: {e}", p.display()))?;
        let r = if meta.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        r.map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Notes that `path` was just moved to the bin. Never fails the caller: the
/// item is already in the bin, and cleaning must not break over a log.
pub fn record(path: &Path, size: u64, source: &str) {
    let Some(file) = journal_path() else { return };
    let entry = Entry { path: path.to_string_lossy().into_owned(), size, time: now_secs(), source: source.to_string() };
    let Ok(mut line) = serde_json::to_string(&entry) else { return };
    line.push('\n');
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn parse(text: &str) -> Vec<Entry> {
    text.lines().filter_map(|l| serde_json::from_str::<Entry>(l.trim()).ok()).collect()
}

/// Every entry, oldest first. Trims the file when it has grown too long.
pub fn read_all() -> Vec<Entry> {
    let Some(file) = journal_path() else { return vec![] };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(text) = std::fs::read_to_string(&file) else { return vec![] };
    let mut entries = parse(&text);
    if entries.len() > MAX_ENTRIES + SLACK {
        entries.drain(..entries.len() - MAX_ENTRIES);
        let mut kept = String::new();
        for e in &entries {
            if let Ok(l) = serde_json::to_string(e) {
                kept.push_str(&l);
                kept.push('\n');
            }
        }
        // Write beside the journal and swap, so a crash cannot leave half of it.
        let tmp = file.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, kept).is_ok() && std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    entries
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Points this thread's journal at a fresh file in a folder with a space
    /// in its name, and removes the folder when dropped.
    pub struct TempJournal(pub PathBuf);

    impl TempJournal {
        pub fn new(tag: &str) -> TempJournal {
            let dir = std::env::temp_dir().join(format!("jharu journal {tag} {}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("trash-log.jsonl");
            TEST_JOURNAL.with(|p| *p.borrow_mut() = Some(file));
            TempJournal(dir)
        }
    }

    impl Drop for TempJournal {
        fn drop(&mut self) {
            TEST_JOURNAL.with(|p| *p.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempJournal;
    use super::*;

    #[test]
    fn records_lines_and_skips_damaged_ones() {
        let j = TempJournal::new("lines");
        // Nothing is recorded, and nothing fails, without a journal.
        assert!(read_all().is_empty());
        record(Path::new("C:\\Users\\me\\My Folder\\cache"), 1234, "Dev clean");
        record(Path::new("/home/me/other \"quoted\" dir"), 5, "Browsers");
        let file = j.0.join("trash-log.jsonl");
        // A torn write, and a line from some future version, must not hide the rest.
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"{\"path\":\"C:\\\\half\n").unwrap();
        f.write_all(b"not json at all\n").unwrap();
        drop(f);
        record(Path::new("D:\\last"), 9, "Git worktrees");

        let all = read_all();
        assert_eq!(all.len(), 3, "{all:?}");
        assert_eq!(all[0].path, "C:\\Users\\me\\My Folder\\cache");
        assert_eq!(all[0].size, 1234);
        assert_eq!(all[0].source, "Dev clean");
        assert_eq!(all[1].path, "/home/me/other \"quoted\" dir");
        assert_eq!(all[2].source, "Git worktrees");
        assert!(all[2].time >= all[0].time && all[0].time > 1_600_000_000);
    }

    #[test]
    fn trims_an_overgrown_journal_keeping_the_newest() {
        let j = TempJournal::new("trim");
        let file = j.0.join("trash-log.jsonl");
        let mut text = String::new();
        for n in 0..(MAX_ENTRIES + SLACK + 10) {
            let e = Entry { path: format!("p{n}"), size: 1, time: n as u64, source: "t".into() };
            text.push_str(&serde_json::to_string(&e).unwrap());
            text.push('\n');
        }
        std::fs::write(&file, text).unwrap();
        let all = read_all();
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all.last().unwrap().path, format!("p{}", MAX_ENTRIES + SLACK + 9));
        // The file itself was rewritten.
        assert_eq!(parse(&std::fs::read_to_string(&file).unwrap()).len(), MAX_ENTRIES);
        assert!(!file.with_extension("jsonl.tmp").exists());
    }
}
