//! Downloads registry models with Handy's resumable, size- and SHA-256-checked
//! HTTP downloader (`managers/model/download.rs`).

use super::registry::{self, ModelEntry};
use crate::managers::model::{DownloadProgress, ModelManager};
use hf_hub::api::tokio::CancellationToken;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Event carrying a `DownloadProgress` while a local model downloads.
pub const PROGRESS_EVENT: &str = "local-llm-download-progress";

static IN_FLIGHT: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Marks one model id as downloading; released on drop.
struct InFlight(&'static str);

impl InFlight {
    fn claim(id: &'static str) -> Result<Self, String> {
        let mut ids = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
        if ids.contains(&id) {
            return Err(format!("{id} is already downloading"));
        }
        ids.push(id);
        Ok(Self(id))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut ids = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
        ids.retain(|i| *i != self.0);
    }
}

/// `<final>.partial`, next to the final file.
pub fn partial_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

/// Present with the registry size (the full hash is checked only after download).
pub fn is_downloaded(models_root: &Path, entry: &ModelEntry) -> bool {
    std::fs::metadata(registry::model_path(models_root, entry))
        .map(|m| m.len() == entry.size_bytes)
        .unwrap_or(false)
}

/// Resumes a partial, deletes size/hash mismatches, and renames verified bytes into place.
pub async fn download(
    models_root: &Path,
    entry: &'static ModelEntry,
    on_progress: &(dyn Fn(&DownloadProgress) + Send + Sync),
) -> Result<PathBuf, String> {
    let _claim = InFlight::claim(entry.id)?;
    let final_path = registry::model_path(models_root, entry);
    if is_downloaded(models_root, entry) {
        return Ok(final_path);
    }
    let dir = final_path
        .parent()
        .ok_or_else(|| format!("bad model path {}", final_path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let partial = partial_path(&final_path);
    let completed = ModelManager::download_verified_artifact(
        entry.id,
        entry.url,
        &partial,
        entry.size_bytes,
        entry.sha256,
        &CancellationToken::new(),
        on_progress,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !completed {
        return Err(format!("download of {} was cancelled", entry.id));
    }
    std::fs::rename(&partial, &final_path)
        .map_err(|e| format!("rename {}: {e}", partial.display()))?;
    Ok(final_path)
}

/// Removes `<models_root>/<id>/` (the caller unloads the model first).
pub fn delete(models_root: &Path, entry: &ModelEntry) -> Result<(), String> {
    let dir = models_root.join(entry.id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("delete {}: {e}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_sits_next_to_the_final_file() {
        assert_eq!(
            partial_path(Path::new("/d/s1/m.gguf")),
            PathBuf::from("/d/s1/m.gguf.partial")
        );
    }

    #[test]
    fn downloaded_means_the_registry_size() {
        let dir = tempfile::tempdir().unwrap();
        let entry = registry::find("s1-mini-q4km").unwrap();
        assert!(!is_downloaded(dir.path(), entry));
        let path = registry::model_path(dir.path(), entry);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(entry.size_bytes - 1)
            .unwrap();
        assert!(!is_downloaded(dir.path(), entry));
        std::fs::File::create(&path)
            .unwrap()
            .set_len(entry.size_bytes)
            .unwrap();
        assert!(is_downloaded(dir.path(), entry));
        delete(dir.path(), entry).unwrap();
        assert!(!path.exists());
        delete(dir.path(), entry).unwrap();
    }

    #[test]
    fn one_download_per_model_at_a_time() {
        let first = InFlight::claim("s1-mini-q4km").unwrap();
        assert!(InFlight::claim("s1-mini-q4km").is_err());
        drop(first);
        assert!(InFlight::claim("s1-mini-q4km").is_ok());
    }
}
