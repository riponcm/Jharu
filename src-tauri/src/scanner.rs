use crate::rules::{all_rules, Category, Safety};
use jwalk::WalkDir;
use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

#[derive(Serialize)]
pub struct Finding {
    pub rule_id: String,
    pub name: String,
    pub description: String,
    pub category: Category,
    pub safety: Safety,
    pub path: String,
    pub size_bytes: u64,
    pub file_count: u64,
    /// Days since the newest file inside was modified (rough "last used").
    pub stale_days: Option<u64>,
    /// Direct children with their sizes, largest first (for drill-down).
    pub children: Vec<ChildEntry>,
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
    files: u64,
    newest_mtime: Option<SystemTime>,
}

fn measure_dir(path: &Path) -> DirStats {
    let mut size = 0u64;
    let mut files = 0u64;
    let mut newest: Option<SystemTime> = None;

    for entry in WalkDir::new(path).skip_hidden(false).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                size += meta.len();
                files += 1;
                if let Ok(modified) = meta.modified() {
                    if newest.map_or(true, |n| modified > n) {
                        newest = Some(modified);
                    }
                }
            }
        }
    }
    DirStats { size, files, newest_mtime: newest }
}

fn shallow_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    WalkDir::new(path)
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
    let findings: Vec<Finding> = std::thread::scope(|s| {
        let handles: Vec<_> = all_rules()
            .into_iter()
            .map(|rule| {
                s.spawn(move || {
                    let mut per_rule: Vec<Finding> = vec![];
                    for path in rule.paths() {
                        if !path.exists() {
                            continue;
                        }
                        let stats = measure_dir(&path);
                        if stats.size == 0 {
                            continue;
                        }
                        let stale_days = stats.newest_mtime.and_then(|m| {
                            SystemTime::now().duration_since(m).ok().map(|d| d.as_secs() / 86_400)
                        });
                        per_rule.push(Finding {
                            rule_id: rule.id.to_string(),
                            name: rule.name.to_string(),
                            description: rule.description.to_string(),
                            category: rule.category,
                            safety: rule.safety,
                            path: path.to_string_lossy().into_owned(),
                            size_bytes: stats.size,
                            file_count: stats.files,
                            stale_days,
                            children: top_children(&path, 8),
                        });
                    }
                    per_rule
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });

    let mut findings = findings;
    findings.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    let (disk_total, disk_free) = disk_totals();
    ScanResult { disk_total, disk_free, findings }
}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke_scan() {
        let result = super::scan();
        println!(
            "disk: {:.1} GB total, {:.1} GB free",
            result.disk_total as f64 / 1e9,
            result.disk_free as f64 / 1e9
        );
        for f in &result.findings {
            println!(
                "{:>9.2} GB  {:8}  {}  ({} files)",
                f.size_bytes as f64 / 1e9,
                format!("{:?}", f.safety),
                f.name,
                f.file_count
            );
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
