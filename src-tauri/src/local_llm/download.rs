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

/// Present with the registry size; UI queries do not hash the file.
pub fn is_downloaded(models_root: &Path, entry: &ModelEntry) -> bool {
    std::fs::metadata(registry::model_path(models_root, entry))
        .map(|m| m.len() == entry.size_bytes)
        .unwrap_or(false)
}

/// Hashes a model file against the compiled registry, without exposing its path.
pub(super) fn verify_model(path: &Path, entry: &ModelEntry) -> Result<bool, String> {
    handy_tts_core::download::verify_artifact(path, entry.size_bytes, entry.sha256, &|| false)
        .map_err(|_| format!("cannot verify {}; delete and re-download", entry.id))
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
        let path = final_path.clone();
        let valid = tokio::task::spawn_blocking(move || verify_model(&path, entry))
            .await
            .map_err(|_| format!("cannot verify {}", entry.id))??;
        if !valid {
            return Err(format!(
                "model file corrupt: {}; delete and re-download",
                entry.id
            ));
        }
        return Ok(final_path);
    }
    let dir = final_path
        .parent()
        .ok_or_else(|| format!("bad model path for {}", entry.id))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", entry.id))?;
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
    finish_download(&partial, &final_path, entry)?;
    Ok(final_path)
}

fn finish_download(partial: &Path, final_path: &Path, entry: &ModelEntry) -> Result<(), String> {
    std::fs::rename(partial, final_path).map_err(|e| format!("rename {}: {e}", entry.id))
}

/// Removes `<models_root>/<id>/` (the caller unloads the model first).
pub fn delete(models_root: &Path, entry: &ModelEntry) -> Result<(), String> {
    let dir = models_root.join(entry.id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("delete {}: {e}", entry.id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn followup_download_errors_identify_model_not_directory() {
        let dir = tempfile::tempdir().unwrap();
        static ENTRY: ModelEntry = ModelEntry {
            id: "followup-download",
            ..registry::MODELS[0]
        };
        let entry = &ENTRY;
        let blocked_root = dir.path().join("blocked");
        std::fs::write(&blocked_root, b"file").unwrap();
        let create = download(&blocked_root, entry, &|_| {}).await.unwrap_err();
        let final_path = registry::model_path(dir.path(), entry);
        let rename = finish_download(&partial_path(&final_path), &final_path, entry).unwrap_err();
        std::fs::write(dir.path().join(entry.id), b"file").unwrap();
        let delete = delete(dir.path(), entry).unwrap_err();
        for error in [create, rename, delete] {
            assert!(
                error.contains(entry.id),
                "download errors must identify the registry entry"
            );
            assert!(
                !error.contains(&dir.path().display().to_string()),
                "download errors must not expose private paths"
            );
        }
    }

    #[tokio::test]
    async fn wbsec_preplaced_same_size_gguf_cannot_take_download_fast_path() {
        static ENTRY: ModelEntry = ModelEntry {
            id: "wbsec-gguf",
            size_bytes: 6,
            sha256: "efa1f375d76194fa51a3556a97e641e61685f914d446979da50a551a4333ffd7",
            ..registry::MODELS[0]
        };
        let root = tempfile::tempdir().unwrap();
        let path = registry::model_path(root.path(), &ENTRY);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"WRONG!").unwrap();
        assert!(is_downloaded(root.path(), &ENTRY));
        assert!(!verify_model(&path, &ENTRY).unwrap());
        let error = download(root.path(), &ENTRY, &|_| {}).await.unwrap_err();
        assert!(error.contains("re-download"));
        assert!(!error.contains(&root.path().display().to_string()));
        std::fs::write(&path, b"public").unwrap();
        assert!(verify_model(&path, &ENTRY).unwrap());
        assert_eq!(download(root.path(), &ENTRY, &|_| {}).await.unwrap(), path);
    }

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
