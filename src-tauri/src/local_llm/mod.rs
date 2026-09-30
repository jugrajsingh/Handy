//! Local in-app LLM post-processing: a Handy-managed `llama-server` child
//! process cleans transcripts with a small CPU model (S1-mini first).

pub mod backend;
pub mod commands;
pub mod download;
pub mod llama_server;
pub mod manager;
pub mod prompt;
pub mod registry;

pub use manager::{LocalLlmManager, LocalLlmStatus};

use crate::managers::audio::AudioRecordingManager;
use crate::settings::{get_settings, AppSettings, LOCAL_LLM_PROVIDER_ID};
use llama_server::LlamaServerBackend;
use log::{info, warn};
use manager::{IdleInputs, ManagerConfig, ManagerHooks};
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};

/// Event carrying a `LocalLlmStatus` on every state change.
pub const STATE_EVENT: &str = "local-llm-state-changed";

/// Every way local post-processing can fail. Each one falls back to the raw transcript.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocalLlmError {
    #[error("llama-server binary not found at {0}")]
    BinaryMissing(String),
    #[error("model file missing: {0}")]
    ModelMissing(String),
    #[error("model file corrupt: {0}")]
    ModelCorrupt(String),
    #[error("llama-server did not become healthy before the start deadline")]
    StartTimeout,
    #[error("llama-server failed to start: {0}")]
    StartFailed(String),
    #[error("request timed out")]
    RequestTimeout,
    #[error("llama-server returned HTTP {0}")]
    Http(u16),
    #[error("request failed: {0}")]
    Transport(String),
    #[error("output rejected: {0}")]
    BadOutput(String),
    #[error("local model unavailable: {0}")]
    Failed(String),
}

/// Builds the app's manager: llama-server backend, `<app_data>/models/llm`,
/// idle inputs from settings and the recorder, status events to the UI.
pub fn create_manager(app: &AppHandle) -> Arc<LocalLlmManager> {
    let models_root = match crate::portable::app_data_dir(app) {
        Ok(dir) => Some(dir.join("models").join("llm")),
        Err(e) => {
            warn!("local-llm: app data directory unavailable: {e}");
            None
        }
    };
    let idle_app = app.clone();
    let status_app = app.clone();
    let hooks = ManagerHooks {
        idle_inputs: Box::new(move || IdleInputs {
            timeout: get_settings(&idle_app).model_unload_timeout,
            recording: idle_app
                .try_state::<Arc<AudioRecordingManager>>()
                .map(|r| r.is_recording())
                .unwrap_or(false),
        }),
        on_status: Box::new(move |status| {
            let _ = status_app.emit(STATE_EVENT, status);
        }),
    };
    let manager = Arc::new(LocalLlmManager::new(
        Box::new(LlamaServerBackend::new()),
        models_root,
        hooks,
        ManagerConfig::default(),
    ));
    manager.start_idle_thread();
    manager
}

/// Runs `manager.process` off the async runtime. Any error becomes `None`,
/// so the caller keeps the raw transcript.
pub async fn post_process(
    manager: Option<Arc<LocalLlmManager>>,
    settings: &AppSettings,
    transcript: &str,
) -> Option<String> {
    let Some(manager) = manager else {
        warn!("local-llm: manager not initialized; using the raw transcript");
        return None;
    };
    let settings = settings.clone();
    let transcript = transcript.to_string();
    let started = Instant::now();
    match tokio::task::spawn_blocking(move || manager.process(&transcript, &settings)).await {
        Ok(Ok(text)) => {
            info!(
                "local-llm: post-processing succeeded in {} ms ({} chars)",
                started.elapsed().as_millis(),
                text.len()
            );
            Some(text)
        }
        Ok(Err(e)) => {
            warn!("local-llm: falling back to the raw transcript: {e}");
            None
        }
        Err(e) => {
            warn!("local-llm: worker task failed ({e}); using the raw transcript");
            None
        }
    }
}

/// On a post-process recording start with the local provider selected,
/// starts the model in the background. Never blocks the caller.
pub fn warm_up_for_recording(app: &AppHandle) {
    let settings = get_settings(app);
    if settings.post_process_provider_id != LOCAL_LLM_PROVIDER_ID {
        return;
    }
    if let Some(manager) = app.try_state::<Arc<LocalLlmManager>>() {
        manager.warm_up(&settings);
    }
}

#[cfg(test)]
mod tests {
    use super::manager::tests::harness;
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_successful_local_run_returns_the_text() {
        let h = harness();
        let out = post_process(Some(h.manager.clone()), &h.settings, "hello there friend").await;
        assert_eq!(out.as_deref(), Some("hello there friend"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_local_error_yields_none() {
        let h = harness();
        h.fake
            .lock()
            .unwrap()
            .start_results
            .push_back(Err(LocalLlmError::StartFailed("boom".into())));
        assert_eq!(
            post_process(Some(h.manager.clone()), &h.settings, "hello there friend").await,
            None
        );
        h.fake
            .lock()
            .unwrap()
            .replies
            .push_back(Err(LocalLlmError::Http(500)));
        assert_eq!(
            post_process(Some(h.manager.clone()), &h.settings, "hello there friend").await,
            None
        );
        h.fake.lock().unwrap().replies.push_back(Ok(String::new()));
        assert_eq!(
            post_process(
                Some(h.manager.clone()),
                &h.settings,
                "one two three four five"
            )
            .await,
            None
        );
        assert_eq!(
            post_process(None, &h.settings, "hello there friend").await,
            None
        );
    }
}
