use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A model must be read at least this long after it finished downloading
/// before we count it as "actually used" rather than just written to disk.
const USED_THRESHOLD_SECS: u64 = 3_600;

#[derive(Serialize)]
pub struct ModelEntry {
    pub provider: String,
    pub name: String,
    pub size_bytes: u64,
    /// Bytes that only this model owns — what you actually reclaim.
    pub exclusive_bytes: u64,
    pub paths: Vec<String>,
    pub downloaded_days: Option<u64>,
    pub last_used_days: Option<u64>,
    /// True when the files were read well after they were downloaded.
    pub used_since_download: bool,
    pub note: Option<String>,
}

#[derive(Serialize)]
pub struct ModelReport {
    pub total_bytes: u64,
    pub unused_bytes: u64,
    /// False when the filesystem never updates access times, which makes
    /// "never used" unknowable. The UI hides usage claims when false.
    pub usage_tracking_reliable: bool,
    pub models: Vec<ModelEntry>,
}

struct Stamps {
    size: u64,
    newest_mtime: Option<SystemTime>,
    newest_atime: Option<SystemTime>,
}

fn stamps(path: &Path) -> Stamps {
    let mut size = 0u64;
    let mut newest_mtime: Option<SystemTime> = None;
    let mut newest_atime: Option<SystemTime> = None;

    let mut consider = |meta: &std::fs::Metadata| {
        size += meta.len();
        if let Ok(m) = meta.modified() {
            if newest_mtime.map_or(true, |n| m > n) {
                newest_mtime = Some(m);
            }
        }
        if let Ok(a) = meta.accessed() {
            if newest_atime.map_or(true, |n| a > n) {
                newest_atime = Some(a);
            }
        }
    };

    if path.is_file() {
        if let Ok(meta) = path.metadata() {
            consider(&meta);
        }
        return Stamps { size, newest_mtime, newest_atime };
    }

    for entry in crate::walk::walk(path).skip_hidden(false).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                consider(&meta);
            }
        }
    }
    Stamps { size, newest_mtime, newest_atime }
}

fn days_since(t: Option<SystemTime>) -> Option<u64> {
    let t = t?;
    SystemTime::now().duration_since(t).ok().map(|d| d.as_secs() / 86_400)
}

fn secs_epoch(t: Option<SystemTime>) -> u64 {
    t.and_then(|x| x.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn build_entry(provider: &str, name: String, path: &Path, note: Option<String>) -> Option<ModelEntry> {
    let s = stamps(path);
    if s.size == 0 {
        return None;
    }
    let used = secs_epoch(s.newest_atime) > secs_epoch(s.newest_mtime) + USED_THRESHOLD_SECS;
    Some(ModelEntry {
        provider: provider.to_string(),
        name,
        size_bytes: s.size,
        exclusive_bytes: s.size,
        paths: vec![path.to_string_lossy().into_owned()],
        downloaded_days: days_since(s.newest_mtime),
        last_used_days: days_since(s.newest_atime),
        used_since_download: used,
        note,
    })
}

/// Hugging Face lays each repo out as `models--org--name` with blobs,
/// snapshots, and refs beneath it, so every repo is independently removable.
fn scan_huggingface(home: &Path, out: &mut Vec<ModelEntry>) {
    let hub = home.join(".cache/huggingface/hub");
    let Ok(rd) = std::fs::read_dir(&hub) else { return };
    for entry in rd.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let (provider, prefix) = if dir_name.starts_with("models--") {
            ("Hugging Face", "models--")
        } else if dir_name.starts_with("datasets--") {
            ("Hugging Face dataset", "datasets--")
        } else {
            continue;
        };
        let name = dir_name.trim_start_matches(prefix).replace("--", "/");
        if let Some(e) = build_entry(provider, name, &entry.path(), None) {
            out.push(e);
        }
    }
}

#[derive(serde::Deserialize)]
struct OllamaManifest {
    layers: Option<Vec<OllamaLayer>>,
    config: Option<OllamaLayer>,
}

#[derive(serde::Deserialize)]
struct OllamaLayer {
    digest: Option<String>,
}

fn digest_to_blob(root: &Path, digest: &str) -> PathBuf {
    root.join("blobs").join(digest.replace(':', "-"))
}

/// Ollama stores content-addressed blobs shared across models, so removing a
/// model may only safely remove the blobs no other model still references.
fn scan_ollama(home: &Path, out: &mut Vec<ModelEntry>) {
    let root = home.join(".ollama/models");
    let manifests = root.join("manifests");
    if !manifests.exists() {
        return;
    }

    // model name -> digests it references
    let mut model_digests: Vec<(String, PathBuf, Vec<String>)> = vec![];
    // digest -> how many models reference it
    let mut refcount: HashMap<String, usize> = HashMap::new();

    for entry in crate::walk::walk(&manifests).skip_hidden(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(manifest) = serde_json::from_str::<OllamaManifest>(&text) else { continue };

        let mut digests: Vec<String> = manifest
            .layers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|l| l.digest)
            .collect();
        if let Some(d) = manifest.config.and_then(|c| c.digest) {
            digests.push(d);
        }
        if digests.is_empty() {
            continue;
        }

        // .../manifests/<registry>/<namespace>/<model>/<tag> -> "model:tag"
        let rel = path.strip_prefix(&manifests).unwrap_or(&path);
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let name = if parts.len() >= 2 {
            format!("{}:{}", parts[parts.len() - 2], parts[parts.len() - 1])
        } else {
            parts.join("/")
        };

        for d in &digests {
            *refcount.entry(d.clone()).or_insert(0) += 1;
        }
        model_digests.push((name, path, digests));
    }

    for (name, manifest_path, digests) in model_digests {
        let mut total = 0u64;
        let mut exclusive = 0u64;
        let mut paths = vec![manifest_path.to_string_lossy().into_owned()];
        let mut shared = 0usize;
        let mut newest_mtime: Option<SystemTime> = None;
        let mut newest_atime: Option<SystemTime> = None;

        for d in &digests {
            let blob = digest_to_blob(&root, d);
            let Ok(meta) = blob.metadata() else { continue };
            total += meta.len();
            if let Ok(m) = meta.modified() {
                if newest_mtime.map_or(true, |n| m > n) {
                    newest_mtime = Some(m);
                }
            }
            if let Ok(a) = meta.accessed() {
                if newest_atime.map_or(true, |n| a > n) {
                    newest_atime = Some(a);
                }
            }
            if refcount.get(d).copied().unwrap_or(0) <= 1 {
                exclusive += meta.len();
                paths.push(blob.to_string_lossy().into_owned());
            } else {
                shared += 1;
            }
        }

        if total == 0 {
            continue;
        }
        let note = if shared > 0 {
            Some(format!(
                "{shared} layer(s) are shared with other Ollama models and are kept."
            ))
        } else {
            None
        };
        let used = secs_epoch(newest_atime) > secs_epoch(newest_mtime) + USED_THRESHOLD_SECS;
        out.push(ModelEntry {
            provider: "Ollama".into(),
            name,
            size_bytes: total,
            exclusive_bytes: exclusive,
            paths,
            downloaded_days: days_since(newest_mtime),
            last_used_days: days_since(newest_atime),
            used_since_download: used,
            note,
        });
    }
}

/// Whisper and torch hub store one self-contained checkpoint file per model.
fn scan_checkpoint_dir(dir: &Path, provider: &str, exts: &[&str], out: &mut Vec<ModelEntry>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(e) = build_entry(provider, name, &path, None) {
            out.push(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_sync() -> ModelReport {
        let home = dirs::home_dir().unwrap();
        let mut models: Vec<ModelEntry> = vec![];
        scan_huggingface(&home, &mut models);
        scan_ollama(&home, &mut models);
        scan_checkpoint_dir(&home.join(".cache/whisper"), "Whisper", &["pt"], &mut models);
        scan_checkpoint_dir(
            &home.join(".cache/torch/hub/checkpoints"),
            "PyTorch",
            &["pth", "pt", "bin"],
            &mut models,
        );
        let usage_tracking_reliable = models.iter().any(|m| m.used_since_download);
        let total_bytes = models.iter().map(|m| m.size_bytes).sum();
        let unused_bytes = if usage_tracking_reliable {
            models.iter().filter(|m| !m.used_since_download).map(|m| m.exclusive_bytes).sum()
        } else {
            0
        };
        models.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        ModelReport { total_bytes, unused_bytes, usage_tracking_reliable, models }
    }

    /// Read-only scan of this machine: `cargo test --lib smoke_model_scan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn smoke_model_scan() {
        let report = scan_sync();
        println!(
            "total {:.2} GB across {} models | usage tracking reliable: {}",
            report.total_bytes as f64 / 1e9,
            report.models.len(),
            report.usage_tracking_reliable
        );
        for m in report.models.iter().take(12) {
            println!(
                "{:>8.2} GB  {:16}  {:44}  downloaded {:?}d ago, last read {:?}d ago, used={}",
                m.size_bytes as f64 / 1e9,
                m.provider,
                m.name,
                m.downloaded_days,
                m.last_used_days,
                m.used_since_download
            );
        }
    }
}

#[tauri::command]
pub async fn model_scan() -> Result<ModelReport, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = dirs::home_dir().ok_or("No home directory")?;
        let mut models: Vec<ModelEntry> = vec![];

        scan_huggingface(&home, &mut models);
        scan_ollama(&home, &mut models);
        scan_checkpoint_dir(&home.join(".cache/whisper"), "Whisper", &["pt"], &mut models);
        scan_checkpoint_dir(
            &home.join(".cache/torch/hub/checkpoints"),
            "PyTorch",
            &["pth", "pt", "bin"],
            &mut models,
        );
        scan_checkpoint_dir(&home.join(".keras/models"), "Keras", &["h5", "keras"], &mut models);

        // If no model anywhere shows a read after its download, the filesystem
        // is not tracking access times and "never used" would be a false claim.
        let usage_tracking_reliable = models.iter().any(|m| m.used_since_download);

        let total_bytes = models.iter().map(|m| m.size_bytes).sum();
        let unused_bytes = if usage_tracking_reliable {
            models.iter().filter(|m| !m.used_since_download).map(|m| m.exclusive_bytes).sum()
        } else {
            0
        };

        models.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
        Ok(ModelReport { total_bytes, unused_bytes, usage_tracking_reliable, models })
    })
    .await
    .map_err(|e| e.to_string())?
}
