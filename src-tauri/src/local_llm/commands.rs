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
    let emit = move |p: &DownloadProgress| {
        let _ = app.emit(download::PROGRESS_EVENT, p);
    };
    download::download(&root, entry, &emit).await.map(|_| ())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_local_llm_model(
    manager: State<'_, Arc<LocalLlmManager>>,
    model_id: String,
) -> Result<(), String> {
    let entry = entry(&model_id)?;
    let manager = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || {
        let root = models_root(&manager)?;
        manager.unload();
        download::delete(&root, entry)
    })
    .await
    .map_err(|e| format!("local model deletion worker failed: {e}"))?
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
        || crate::tray::update_tray_menu(&app),
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
