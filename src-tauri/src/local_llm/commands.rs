//! Tauri commands for the local LLM settings UI.

use super::registry::{self, PromptStyle};
use super::{download, LocalLlmManager, LocalLlmStatus};
use crate::managers::model::DownloadProgress;
use crate::settings::{self, AppSettings, LocalLlmContext, LocalLlmStructure, LocalLlmStyling};
use serde::Serialize;
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// One registry entry as the UI sees it.
#[derive(Debug, Clone, Serialize, Type)]
pub struct LocalLlmModelInfo {
    pub id: String,
    pub display_name: String,
    pub attribution: String,
    pub card_url: String,
    pub size_bytes: u64,
    pub prompt_style: PromptStyle,
    pub downloaded: bool,
}

fn models_root(manager: &LocalLlmManager) -> Result<std::path::PathBuf, String> {
    manager
        .models_root()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "app data directory unknown".to_string())
}

fn entry(model_id: &str) -> Result<&'static registry::ModelEntry, String> {
    registry::find(model_id).ok_or_else(|| format!("unknown local model '{model_id}'"))
}

#[tauri::command]
#[specta::specta]
pub fn get_local_llm_models(manager: State<'_, Arc<LocalLlmManager>>) -> Vec<LocalLlmModelInfo> {
    registry::MODELS
        .iter()
        .map(|m| LocalLlmModelInfo {
            id: m.id.to_string(),
            display_name: m.display_name.to_string(),
            attribution: m.attribution.to_string(),
            card_url: m.card_url.to_string(),
            size_bytes: m.size_bytes,
            prompt_style: m.prompt_style,
            downloaded: manager
                .models_root()
                .map(|root| download::is_downloaded(root, m))
                .unwrap_or(false),
        })
        .collect()
}

#[tauri::command]
#[specta::specta]
pub async fn download_local_llm_model(
    app: AppHandle,
    manager: State<'_, Arc<LocalLlmManager>>,
    model_id: String,
) -> Result<(), String> {
    let entry = entry(&model_id)?;
    let root = models_root(&manager)?;
    let progress_app = app.clone();
    let emit = move |p: &DownloadProgress| {
        let _ = progress_app.emit(download::PROGRESS_EVENT, p);
    };
    download_local_llm_model_with_refresh(download::download(&root, entry, &emit), || {
        crate::tray::update_tray_menu(&app);
    })
    .await
}

async fn download_local_llm_model_with_refresh(
    download: impl std::future::Future<Output = Result<std::path::PathBuf, String>>,
    refresh_tray: impl FnOnce(),
) -> Result<(), String> {
    download.await?;
    refresh_tray();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_local_llm_model(
    app: AppHandle,
    manager: State<'_, Arc<LocalLlmManager>>,
    model_id: String,
) -> Result<(), String> {
    let entry = entry(&model_id)?;
    let manager = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || {
        let root = models_root(&manager)?;
        manager.unload();
        delete_local_llm_model_with_refresh(
            || download::delete(&root, entry),
            || crate::tray::update_tray_menu(&app),
        )
    })
    .await
    .map_err(|e| format!("local model deletion worker failed: {e}"))?
}

fn delete_local_llm_model_with_refresh(
    delete: impl FnOnce() -> Result<(), String>,
    refresh_tray: impl FnOnce(),
) -> Result<(), String> {
    delete()?;
    refresh_tray();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_local_llm_status(manager: State<'_, Arc<LocalLlmManager>>) -> LocalLlmStatus {
    manager.status()
}

#[tauri::command]
#[specta::specta]
pub async fn set_local_llm_model(
    app: AppHandle,
    manager: State<'_, Arc<LocalLlmManager>>,
    model_id: Option<String>,
) -> Result<(), String> {
    if let Some(id) = &model_id {
        entry(id)?;
    }
    set_local_llm_model_with_settings(
        Arc::clone(manager.inner()),
        model_id,
        || settings::get_settings(&app),
        |s| settings::write_settings_checked(&app, s),
        || {
            crate::tray::update_tray_menu(&app);
            if let Err(error) = app.emit(
                "settings-changed",
                serde_json::json!({ "setting": "local_llm_model_id" }),
            ) {
                log::error!("Failed to publish local model selection: {error}");
            }
        },
    )
    .await
}

/// Persists selection and refreshes the tray before the blocking model reset.
pub(crate) async fn set_local_llm_model_with_settings(
    manager: Arc<LocalLlmManager>,
    model_id: Option<String>,
    read_settings: impl FnOnce() -> AppSettings,
    write_settings: impl FnOnce(AppSettings) -> Result<(), String>,
    refresh_tray: impl FnOnce(),
) -> Result<(), String> {
    let mut s = read_settings();
    let changed = s.local_llm_model_id != model_id;
    s.local_llm_model_id = model_id;
    write_settings(s)?;
    refresh_tray();
    tauri::async_runtime::spawn_blocking(move || {
        if changed {
            manager.unload();
        }
        manager.reset_failure();
    })
    .await
    .map_err(|e| format!("local model selection worker failed: {e}"))?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_local_llm_styling_setting(
    app: AppHandle,
    styling: LocalLlmStyling,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.local_llm_styling = styling;
    settings::write_settings(&app, s);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_local_llm_structure_setting(
    app: AppHandle,
    structure: LocalLlmStructure,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.local_llm_structure = structure;
    settings::write_settings(&app, s);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_local_llm_context_setting(
    app: AppHandle,
    context: LocalLlmContext,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.local_llm_context = context;
    settings::write_settings(&app, s);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_llm::manager::tests::harness;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn write_downloaded_fixture(
        root: &std::path::Path,
        entry: &registry::ModelEntry,
    ) -> std::path::PathBuf {
        let path = registry::model_path(root, entry);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(entry.size_bytes)
            .unwrap();
        path
    }

    #[tokio::test]
    async fn download_completion_refreshes_tray_once_and_failures_preserve_membership() {
        let dir = tempfile::tempdir().unwrap();
        let entry = registry::find("quill-0.8b-q4km").unwrap();
        let refreshes = std::cell::Cell::new(0);
        assert!(!download::is_downloaded(dir.path(), entry));
        for error in ["download failed", "download was cancelled"] {
            let result =
                download_local_llm_model_with_refresh(async { Err(error.to_string()) }, || {
                    refreshes.set(refreshes.get() + 1)
                })
                .await;
            assert_eq!(result, Err(error.to_string()));
            assert_eq!(refreshes.get(), 0);
            assert!(!download::is_downloaded(dir.path(), entry));
        }
        download_local_llm_model_with_refresh(
            async { Ok(write_downloaded_fixture(dir.path(), entry)) },
            || {
                assert!(download::is_downloaded(dir.path(), entry));
                refreshes.set(refreshes.get() + 1);
            },
        )
        .await
        .unwrap();
        assert_eq!(refreshes.get(), 1);
        assert!(download::is_downloaded(dir.path(), entry));
    }

    #[test]
    fn deletion_refreshes_tray_once_and_failure_preserves_membership() {
        let dir = tempfile::tempdir().unwrap();
        let entry = registry::find("quill-0.8b-q4km").unwrap();
        write_downloaded_fixture(dir.path(), entry);
        let refreshes = std::cell::Cell::new(0);
        let result = delete_local_llm_model_with_refresh(
            || Err("delete failed".into()),
            || refreshes.set(refreshes.get() + 1),
        );
        assert_eq!(result, Err("delete failed".into()));
        assert_eq!(refreshes.get(), 0);
        assert!(download::is_downloaded(dir.path(), entry));
        delete_local_llm_model_with_refresh(
            || download::delete(dir.path(), entry),
            || {
                assert!(!download::is_downloaded(dir.path(), entry));
                refreshes.set(refreshes.get() + 1);
            },
        )
        .unwrap();
        assert_eq!(refreshes.get(), 1);
        assert!(!download::is_downloaded(dir.path(), entry));
    }

    #[tokio::test]
    async fn tray_selection_waits_off_thread_for_an_inflight_request() {
        let h = harness();
        let alternative = registry::find("quill-0.8b-q4km").unwrap();
        let alternative_path = registry::model_path(h.manager.models_root().unwrap(), alternative);
        std::fs::create_dir_all(alternative_path.parent().unwrap()).unwrap();
        std::fs::File::create(alternative_path)
            .unwrap()
            .set_len(alternative.size_bytes)
            .unwrap();
        let (started, release) = {
            let mut fake = h.fake.lock().unwrap();
            fake.request_block = true;
            fake.request_succeeds_on_release = true;
            (fake.request_started.clone(), fake.request_release.clone())
        };
        let old = h.settings.clone();
        let manager = h.manager.clone();
        let request = std::thread::spawn(move || manager.process("hello there friend", &old));
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.load(Ordering::SeqCst));
        let store = Arc::new(Mutex::new(h.settings.clone()));
        let read = store.clone();
        let write = store.clone();
        let manager = h.manager.clone();
        let (refreshed, refresh_done) = oneshot::channel();
        let selection = tokio::spawn(set_local_llm_model_with_settings(
            manager,
            Some("quill-0.8b-q4km".into()),
            move || read.lock().unwrap().clone(),
            move |settings| {
                *write.lock().unwrap() = settings;
                Ok(())
            },
            move || {
                refreshed.send(()).unwrap();
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), refresh_done)
            .await
            .unwrap()
            .unwrap();
        assert!(!selection.is_finished());
        assert_eq!(
            store.lock().unwrap().local_llm_model_id.as_deref(),
            Some("quill-0.8b-q4km")
        );
        assert!(!request.is_finished());
        assert_eq!(h.fake.lock().unwrap().loads, 1);
        release.store(true, Ordering::SeqCst);
        assert_eq!(request.join().unwrap().unwrap(), "hello there friend");
        tokio::time::timeout(Duration::from_secs(1), selection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(h.fake.lock().unwrap().loads, 1);
        assert_eq!(
            h.manager.status().state,
            super::super::manager::LocalLlmStateKind::Unloaded
        );
    }

    #[tokio::test]
    async fn model_selection_preserves_settings_changed_during_unload() {
        let h = harness();
        let (started, release) = {
            let mut fake = h.fake.lock().unwrap();
            fake.request_block = true;
            (fake.request_started.clone(), fake.request_release.clone())
        };
        let manager = h.manager.clone();
        let settings = h.settings.clone();
        let request = std::thread::spawn(move || manager.process("hello there friend", &settings));
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.load(Ordering::SeqCst), "request must be in flight");

        let mut initial = h.settings.clone();
        initial.local_llm_model_id = None;
        let store = Arc::new(Mutex::new(initial));
        let read_store = store.clone();
        let write_store = store.clone();
        let manager = h.manager.clone();
        let model_id = h.settings.local_llm_model_id.clone();
        let (read, read_done) = oneshot::channel();
        let selection = tokio::spawn(set_local_llm_model_with_settings(
            manager,
            model_id.clone(),
            move || {
                let snapshot = read_store.lock().unwrap().clone();
                read.send(()).unwrap();
                snapshot
            },
            move |s| {
                *write_store.lock().unwrap() = s;
                Ok(())
            },
            || {},
        ));
        tokio::time::timeout(Duration::from_secs(1), read_done)
            .await
            .unwrap()
            .unwrap();
        assert!(!selection.is_finished(), "unload must wait for the request");
        store.lock().unwrap().local_llm_styling = LocalLlmStyling::Formal;
        release.store(true, Ordering::SeqCst);
        assert!(request.join().unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(1), selection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let saved = store.lock().unwrap();
        assert_eq!(saved.local_llm_model_id, model_id);
        assert_eq!(saved.local_llm_styling, LocalLlmStyling::Formal);
    }
}
