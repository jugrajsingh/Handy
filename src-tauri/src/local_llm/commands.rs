//! Tauri commands for the local LLM settings UI.

use super::registry::{self, PromptStyle};
use super::{download, LocalLlmManager, LocalLlmStatus};
use crate::managers::model::DownloadProgress;
use crate::settings::{self, LocalLlmContext, LocalLlmStructure, LocalLlmStyling};
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
    let mut s = settings::get_settings(&app);
    let changed = s.local_llm_model_id != model_id;
    let manager = Arc::clone(manager.inner());
    tauri::async_runtime::spawn_blocking(move || {
        if changed {
            manager.unload();
        }
        manager.reset_failure();
    })
    .await
    .map_err(|e| format!("local model selection worker failed: {e}"))?;
    s.local_llm_model_id = model_id;
    settings::write_settings(&app, s);
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
