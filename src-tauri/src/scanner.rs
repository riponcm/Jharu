use crate::rules::{all_rules, Category, Safety};
use crate::safety::{assess, Facts, Reason};
use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

#[derive(Serialize)]
pub struct Finding {
    pub rule_id: String,
    pub name: String,
    pub description: String,
    pub category: Category,
    /// Worked out per finding by safety::assess, from the rule's facts and
    /// live checks of this machine.
    pub safety: Safety,
    /// Why: one line per question the rating answers, good answers included.
    pub reasons: Vec<Reason>,
    pub path: String,
    pub size_bytes: u64,
    /// What clearing this location would actually free. Lower than
    /// `size_bytes` when files are hard-linked from elsewhere, as uv's cache
    /// is from every virtualenv it installed into.
    pub reclaimable_bytes: u64,
    /// False when `reclaimable_bytes` is the quick estimate (only large files
    /// checked for links) and the UI should ask `reclaimable_size` for the
    /// exact figure.
    pub reclaimable_exact: bool,
    pub file_count: u64,
    /// Days since the newest file inside was modified (rough "last used").
    pub stale_days: Option<u64>,
    /// Direct children with their sizes, largest first (for drill-down).
    pub children: Vec<ChildEntry>,
    /// Present when this location could not be fully read, so the size shown
    /// is incomplete rather than accurate.
    pub read_error: Option<String>,
}

#[derive(Serialize)]
pub struct ChildEntry {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Serialize)]
pub struct ScanResult {
    pub disk_total: u64,
    pub disk_free: u64,
    pub findings: Vec<Finding>,
}

struct DirStats {
    size: u64,
    /// What deleting the directory would free (see Finding).
    reclaimable: u64,
    reclaimable_exact: bool,
    files: u64,
    newest_mtime: Option<SystemTime>,
    /// Set when the directory exists but could not be fully read. Without this
    /// an unreadable cache would silently vanish from the results instead of
    /// telling the user that data is there but out of reach.
    read_error: Option<String>,
}

fn measure_dir(path: &Path) -> DirStats {
    // Counted like Deep clean: each physical file once, online-only cloud
    // files left out, so a cache's size matches what removing it frees.
    let mut usage = crate::usage::Usage::default();
    let mut seen = crate::usage::Seen::default();
    let mut newest: Option<SystemTime> = None;
    let mut read_error: Option<String> = None;

    for result in crate::walk::walk(path).skip_hidden(false) {
        let entry = match result {
            Ok(e) => e,
            Err(e) => {
                read_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        // A directory that could not be opened arrives as an Ok entry carrying
        // read_children_error; the Err arm above only catches walk failures.
        if let Some(e) = &entry.read_children_error {
            read_error.get_or_insert_with(|| e.to_string());
        }
        if entry.file_type().is_file() {
            match entry.metadata() {
                Ok(meta) => {
                    usage.add_file(&entry.path(), &meta, &mut seen);
                    if let Ok(modified) = meta.modified() {
                        if newest.map_or(true, |n| modified > n) {
                            newest = Some(modified);
                        }
                    }
                }
                Err(e) => {
                    read_error.get_or_insert_with(|| e.to_string());
                }
            }
        }
    }
    // The quick pass checks only large files for links. Where none lead
    // outside, its figure is exact. Where some do (uv's cache, linked into
    // every virtualenv), small linked files were missed, so the figure is an
    // upper bound; the exact count opens every file and takes tens of
    // seconds, so the UI asks for it separately instead of holding the scan.
    let linked = seen.linked_outside();
    DirStats {
        size: usage.bytes,
        reclaimable: usage.bytes.saturating_sub(linked),
        reclaimable_exact: linked == 0,
        files: usage.files,
        newest_mtime: newest,
        read_error,
    }
}

fn shallow_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    crate::walk::walk(path)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn top_children(path: &Path, limit: usize) -> Vec<ChildEntry> {
    let mut children: Vec<ChildEntry> = std::fs::read_dir(path)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let p = e.path();
                    ChildEntry {
                        name: e.file_name().to_string_lossy().into_owned(),
                        path: p.to_string_lossy().into_owned(),
                        size_bytes: shallow_size(&p),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    children.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    children.truncate(limit);
    children
}

pub fn scan() -> ScanResult {
    // Rules are walked one at a time on purpose. jwalk already parallelises
    // each walk across rayon's global pool, so spawning a thread per rule
    // nests two layers of parallelism and starves that pool — walks then
    // return empty and their findings silently disappear.
    let findings: Vec<Finding> = {
        let mut collected: Vec<Finding> = vec![];
        let facts = Facts::gather();
        for rule in all_rules() {
            let mut per_rule: Vec<Finding> = vec![];
            for path in rule.paths() {
                if !path.exists() {
                    continue;
                }
                let stats = measure_dir(&path);
                // Only skip when the directory is genuinely empty. If it
                // could not be read, report it so the user learns why.
                if stats.size == 0 && stats.read_error.is_none() {
                    continue;
                }
                let stale_days = stats.newest_mtime.and_then(|m| {
                    SystemTime::now().duration_since(m).ok().map(|d| d.as_secs() / 86_400)
                });
                let (safety, reasons) = assess(&rule, &path, &facts);
                per_rule.push(Finding {
                    rule_id: rule.id.to_string(),
                    name: rule.name.to_string(),
                    description: rule.description.to_string(),
                    category: rule.category,
                    safety,
                    reasons,
                    path: path.to_string_lossy().into_owned(),
                    size_bytes: stats.size,
                    reclaimable_bytes: stats.reclaimable.min(stats.size),
                    reclaimable_exact: stats.reclaimable_exact,
                    file_count: stats.files,
                    stale_days,
                    children: top_children(&path, 8),
                    read_error: stats.read_error,
                });
            }
            collected.extend(per_rule);
        }
        collected
    };

    let mut findings = findings;
    findings.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    let (disk_total, disk_free) = disk_totals();
    ScanResult { disk_total, disk_free, findings }
}

#[cfg(test)]
mod tests {
    /// Read-only scan of this machine: `cargo test --lib smoke_scan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_scan() {
        let result = super::scan();
        println!(
            "disk: {:.1} GB total, {:.1} GB free",
            result.disk_total as f64 / 1e9,
            result.disk_free as f64 / 1e9
        );
        for f in &result.findings {
            println!(
                "{:>9.2} GB (frees {:>6.2} GB)  {:8}  {}  ({} files){}",
                f.size_bytes as f64 / 1e9,
                f.reclaimable_bytes as f64 / 1e9,
                format!("{:?}", f.safety),
                f.name,
                f.file_count,
                f.read_error
                    .as_deref()
                    .map(|e| format!("  [READ ERROR: {e}]"))
                    .unwrap_or_default()
            );
            for r in &f.reasons {
                println!("               {:?}: {}", r.level, r.text);
            }
        }
        assert!(result.disk_total > 0);
    }
}

fn disk_totals() -> (u64, u64) {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let home = dirs::home_dir().unwrap_or_default();
    disks
        .list()
        .iter()
        .filter(|d| home.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| (d.total_space(), d.available_space()))
        .unwrap_or((0, 0))
}
