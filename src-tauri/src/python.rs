use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How deep below the home directory to look for environments. Deep enough to
/// find project-local .venv folders, shallow enough to stay fast.
const MAX_DEPTH: usize = 6;

#[derive(Serialize, Clone)]
pub struct PyPackage {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Serialize)]
pub struct PyEnv {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size_bytes: u64,
    pub stale_days: Option<u64>,
    pub packages: Vec<PyPackage>,
}

#[derive(Serialize)]
pub struct PyDuplicate {
    pub name: String,
    pub copies: usize,
    pub total_bytes: u64,
    /// Everything beyond the single largest copy — what duplication costs you.
    pub wasted_bytes: u64,
}

#[derive(Serialize)]
pub struct PyReport {
    pub total_bytes: u64,
    pub wasted_bytes: u64,
    pub envs: Vec<PyEnv>,
    pub duplicates: Vec<PyDuplicate>,
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".git" | ".Trash" | "Library" | "Applications" | ".cache"
            | "Pictures" | "Music" | "Movies" | "Photos Library.photoslibrary"
    )
}

fn dir_size(path: &Path) -> u64 {
    crate::walk::walk(path)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for entry in crate::walk::walk(path).max_depth(3).skip_hidden(false).into_iter().flatten() {
        if let Ok(meta) = entry.metadata() {
            if let Ok(m) = meta.modified() {
                if newest.map_or(true, |n| m > n) {
                    newest = Some(m);
                }
            }
        }
    }
    newest
}

/// Locate every virtualenv (pyvenv.cfg) and conda env (conda-meta) under home.
fn find_envs(home: &Path) -> Vec<(PathBuf, String)> {
    let mut envs = vec![];
    let walker = crate::walk::walk(home)
        .max_depth(MAX_DEPTH)
        .skip_hidden(false)
        .process_read_dir(|_, _, _, children| {
            children.retain(|c| {
                c.as_ref()
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        !(e.file_type().is_dir() && skip_dir(&name))
                    })
                    .unwrap_or(true)
            });
        });

    for entry in walker.into_iter().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == "pyvenv.cfg" && entry.file_type().is_file() {
            if let Some(parent) = path.parent() {
                envs.push((parent.to_path_buf(), "virtualenv".to_string()));
            }
        } else if name == "conda-meta" && entry.file_type().is_dir() {
            if let Some(parent) = path.parent() {
                envs.push((parent.to_path_buf(), "conda".to_string()));
            }
        }
    }

    envs.sort();
    envs.dedup_by(|a, b| a.0 == b.0);
    envs
}

fn site_packages_dirs(env: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![];
    // Windows layout
    let win = env.join("Lib").join("site-packages");
    if win.is_dir() {
        dirs.push(win);
    }
    // Unix layout: lib/python3.x/site-packages
    if let Ok(rd) = std::fs::read_dir(env.join("lib")) {
        for e in rd.flatten() {
            let sp = e.path().join("site-packages");
            if sp.is_dir() {
                dirs.push(sp);
            }
        }
    }
    dirs
}

/// Normalizes `torch-2.4.0.dist-info` and `torch` to the same package name.
fn package_name(entry_name: &str) -> Option<String> {
    if entry_name.starts_with("__pycache__") || entry_name.starts_with('.') {
        return None;
    }
    let base = entry_name
        .strip_suffix(".dist-info")
        .or_else(|| entry_name.strip_suffix(".egg-info"))
        .unwrap_or(entry_name);
    // strip a trailing -<version> on metadata dirs
    let base = match base.rsplit_once('-') {
        Some((head, tail)) if tail.chars().next().is_some_and(|c| c.is_ascii_digit()) => head,
        _ => base,
    };
    let cleaned = base.trim_end_matches(".py").to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    /// Read-only scan of this machine: `cargo test --lib smoke_find_envs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_find_envs() {
        let home = dirs::home_dir().unwrap();
        let start = std::time::Instant::now();
        let envs = super::find_envs(&home);
        println!("found {} environments in {:?}", envs.len(), start.elapsed());
        for (path, kind) in envs.iter().take(10) {
            println!("  {kind:12} {}", path.display());
        }
    }
}

#[tauri::command]
pub async fn python_scan() -> Result<PyReport, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = dirs::home_dir().ok_or("No home directory")?;
        let found = find_envs(&home);

        // Sequential on purpose — see the note in scanner.rs about nesting
        // threads around jwalk walks.
        let mut envs: Vec<PyEnv> = found
            .into_iter()
            .map(|(path, kind)| {
                {
                    {
                        let mut packages: HashMap<String, PyPackage> = HashMap::new();
                        for sp in site_packages_dirs(&path) {
                            let Ok(rd) = std::fs::read_dir(&sp) else { continue };
                            for entry in rd.flatten() {
                                let raw = entry.file_name().to_string_lossy().into_owned();
                                let Some(name) = package_name(&raw) else { continue };
                                let size = dir_size(&entry.path());
                                if size == 0 {
                                    continue;
                                }
                                packages
                                    .entry(name.clone())
                                    .and_modify(|p| p.size_bytes += size)
                                    .or_insert(PyPackage {
                                        name,
                                        path: entry.path().to_string_lossy().into_owned(),
                                        size_bytes: size,
                                    });
                            }
                        }
                        let mut packages: Vec<PyPackage> = packages.into_values().collect();
                        packages.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
                        packages.truncate(30);

                        let size_bytes = dir_size(&path);
                        let stale_days = newest_mtime(&path).and_then(|m| {
                            SystemTime::now().duration_since(m).ok().map(|d| d.as_secs() / 86_400)
                        });

                        PyEnv {
                            name: path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
                            path: path.to_string_lossy().into_owned(),
                            kind,
                            size_bytes,
                            stale_days,
                            packages,
                        }
                    }
                }
            })
            .collect();

        envs.retain(|e| e.size_bytes > 0);
        envs.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

        // A package installed in several environments costs its size once per copy.
        let mut by_package: HashMap<String, Vec<u64>> = HashMap::new();
        for env in &envs {
            for p in &env.packages {
                by_package.entry(p.name.clone()).or_default().push(p.size_bytes);
            }
        }
        let mut duplicates: Vec<PyDuplicate> = by_package
            .into_iter()
            .filter(|(_, sizes)| sizes.len() > 1)
            .map(|(name, sizes)| {
                let total: u64 = sizes.iter().sum();
                let largest = sizes.iter().copied().max().unwrap_or(0);
                PyDuplicate {
                    name,
                    copies: sizes.len(),
                    total_bytes: total,
                    wasted_bytes: total.saturating_sub(largest),
                }
            })
            .collect();
        duplicates.sort_by(|a, b| b.wasted_bytes.cmp(&a.wasted_bytes));
        duplicates.truncate(25);

        let total_bytes = envs.iter().map(|e| e.size_bytes).sum();
        let wasted_bytes = duplicates.iter().map(|d| d.wasted_bytes).sum();

        Ok(PyReport { total_bytes, wasted_bytes, envs, duplicates })
    })
    .await
    .map_err(|e| e.to_string())?
}
