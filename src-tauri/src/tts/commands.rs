use super::{
    manager::ManagerSnapshot,
    service::{selection, TtsService},
    session::{SessionCommand, SessionView},
};
use handy_tts_core::{
    catalog,
    device::{ModelRating, Preparation, RunOn},
    download::{self, HttpTransport},
    selection::{clamp_selection, Selection},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{AppHandle, State};
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct TtsModelInfo {
    pub id: String,
    pub display_name: String,
    pub license: String,
    pub attribution: String,
    pub card_url: String,
    pub size_bytes: u64,
    pub voices: Vec<String>,
    pub downloaded: bool,
    pub active: bool,
    pub rating: ModelRating,
    pub run_on: RunOn,
    pub preparation: Preparation,
    pub npu_driver_too_old: bool,
}
async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job)
        .await
        .map_err(|_| "tts.errors.generic".to_string())?
}
#[tauri::command]
#[specta::specta]
pub async fn get_tts_models(
    app: AppHandle,
    service: State<'_, Arc<TtsService>>,
) -> Result<Vec<TtsModelInfo>, String> {
    let service = service.inner().clone();
    blocking(move || {
        let ready = service.ready()?;
        let settings = crate::settings::get_settings(&app);
        let snapshot = ready.manager.snapshot();
        catalog::entries()
            .map_err(|e| e.to_string())?
            .iter()
            .map(|entry| {
                let rating = ready
                    .ratings
                    .models
                    .get(entry.id)
                    .cloned()
                    .ok_or("tts.errors.invalidRatings")?;
                Ok(TtsModelInfo {
                    id: entry.id.into(),
                    display_name: entry.name.into(),
                    license: entry.license.into(),
                    attribution: entry.attribution.into(),
                    card_url: entry.card.into(),
                    size_bytes: entry.bytes,
                    voices: entry.voices.iter().map(|voice| voice.to_string()).collect(),
                    downloaded: download::is_downloaded(&ready.root, entry),
                    active: entry.id == settings.tts_model_id,
                    rating,
                    run_on: settings
                        .tts_run_on
                        .get(entry.id)
                        .copied()
                        .unwrap_or_default(),
                    npu_driver_too_old: ready.manager.npu_driver_too_old(entry.id),
                    preparation: handy_tts_core::preparation::visible_preparation(
                        entry.id == settings.tts_model_id,
                        &snapshot.preparation,
                        &ready.preparer.snapshot(entry.id),
                    ),
                })
            })
            .collect()
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn get_tts_status(
    service: State<'_, Arc<TtsService>>,
) -> Result<ManagerSnapshot, String> {
    let service = service.inner().clone();
    blocking(move || Ok(service.ready()?.manager.snapshot())).await
}
#[tauri::command]
#[specta::specta]
pub async fn get_tts_devices(
    service: State<'_, Arc<TtsService>>,
) -> Result<Vec<handy_tts_core::device::Device>, String> {
    let service = service.inner().clone();
    blocking(move || Ok(service.ready()?.available)).await
}
#[tauri::command]
#[specta::specta]
pub async fn get_read_aloud_state(
    service: State<'_, Arc<TtsService>>,
) -> Result<SessionView, String> {
    let service = service.inner().clone();
    blocking(move || Ok(service.ready()?.session.snapshot())).await
}
#[tauri::command]
#[specta::specta]
pub async fn read_aloud_replace_text(
    text: String,
    client_revision: u64,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        service.reading_command(SessionCommand::Replace {
            client_revision: Some(client_revision),
            text,
            autoplay: false,
        })
    })
    .await
}
macro_rules! session_command {
    ($name:ident,$message:expr) => {
        #[tauri::command]
        #[specta::specta]
        pub async fn $name(service: State<'_, Arc<TtsService>>) -> Result<(), String> {
            let service = service.inner().clone();
            blocking(move || service.reading_command($message)).await
        }
    };
}
session_command!(read_aloud_play, SessionCommand::Play);
session_command!(read_aloud_pause, SessionCommand::Pause);
session_command!(read_aloud_stop, SessionCommand::Stop);
#[tauri::command]
#[specta::specta]
pub async fn read_aloud_close(service: State<'_, Arc<TtsService>>) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        service.close();
        Ok(())
    })
    .await
}
pub fn set_selection(
    app: &AppHandle,
    service: &TtsService,
    chosen: Selection,
) -> Result<(), String> {
    let chosen = clamp_selection(chosen).map_err(|e| e.to_string())?;
    let entry = catalog::entry(&chosen.model_id).map_err(|e| e.to_string())?;
    if !entry.voices.contains(&chosen.voice) {
        return Err("tts.errors.invalidVoice".into());
    }
    let ready = service.ready()?;
    let mut settings = crate::settings::get_settings(app);
    settings.tts_model_id = chosen.model_id.clone();
    settings
        .tts_voice
        .insert(chosen.model_id.clone(), chosen.voice.clone());
    settings.tts_speed = chosen.speed;
    settings.tts_pitch_semitones = chosen.pitch;
    settings.tts_output_device = chosen.output_device.clone();
    settings.tts_read_code_blocks = chosen.read_code;
    let app = app.clone();
    ready.session.select_and_wait(
        chosen,
        download::is_downloaded(&ready.root, entry),
        Box::new(move |_| {
            crate::settings::write_settings_checked(&app, settings)
                .map_err(|_| handy_tts_core::selection::SelectionError::Persist)
        }),
    )
}
#[tauri::command]
#[specta::specta]
pub async fn set_tts_selection(
    chosen: Selection,
    app: AppHandle,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || set_selection(&app, &service, chosen)).await
}
#[tauri::command]
#[specta::specta]
pub async fn set_tts_model(
    model_id: String,
    app: AppHandle,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        let entry = catalog::entry(&model_id).map_err(|e| e.to_string())?;
        let mut chosen = selection(&crate::settings::get_settings(&app));
        chosen.model_id = model_id;
        chosen.voice = crate::settings::get_settings(&app)
            .tts_voice
            .get(entry.id)
            .cloned()
            .unwrap_or_else(|| entry.default_voice.into());
        set_selection(&app, &service, chosen)
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn set_tts_run_on(
    model_id: String,
    choice: RunOn,
    app: AppHandle,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        catalog::entry(&model_id).map_err(|e| e.to_string())?;
        let ready = service.ready()?;
        let mut settings = crate::settings::get_settings(&app);
        settings.tts_run_on.insert(model_id.clone(), choice);
        crate::settings::write_settings_checked(&app, settings.clone())?;
        ready
            .manager
            .set_run_on(&model_id, choice)
            .map_err(|e| e.to_string())?;
        if settings.tts_model_id == model_id {
            ready.session.select_and_wait(
                selection(&settings),
                ready.manager.downloaded(&model_id),
                Box::new(|_| Ok(())),
            )?;
        }
        super::preparation::prepare(&ready, &model_id, choice)?;
        Ok(())
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn unload_tts_model(service: State<'_, Arc<TtsService>>) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        let ready = service.ready()?;
        ready
            .session
            .send(SessionCommand::Stop)
            .map_err(|e| e.to_string())?;
        ready.manager.unload();
        Ok(())
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn download_tts_model(
    model_id: String,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        let ready = service.ready()?;
        let entry = catalog::entry(&model_id).map_err(|e| e.to_string())?;
        let cancel = Arc::new(AtomicBool::new(false));
        let _ownership =
            DownloadOwnership::claim(&service.cancellations, &model_id, cancel.clone())?;
        let transport = HttpTransport::new("https://huggingface.co").map_err(|e| e.to_string())?;
        let result = service
            .downloader
            .download(&ready.root, entry, &transport, &cancel, |progress| {
                service.download_progress(progress)
            })
            .map(|_| ())
            .map_err(|e| e.to_string());
        if result.is_ok() {
            super::preparation::prepare(
                &ready,
                &model_id,
                ready
                    .manager
                    .configuration()
                    .run_on
                    .get(&model_id)
                    .copied()
                    .unwrap_or_default(),
            )?;
        }
        result
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn cancel_tts_download(
    model_id: String,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        if let Some(cancel) = service
            .cancellations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&model_id)
        {
            cancel.store(true, Ordering::Release);
        }
        Ok(())
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn delete_tts_model(
    model_id: String,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        let ready = service.ready()?;
        ready.preparer.cancel(&model_id);
        if ready.manager.has_model(&model_id) {
            ready.session.stop_and_wait().map_err(|e| e.to_string())?;
            ready.manager.selected();
        }
        let entry = catalog::entry(&model_id).map_err(|e| e.to_string())?;
        download::delete(&ready.root, entry).map_err(|e| e.to_string())
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn set_tts_hf_token(
    token: String,
    service: State<'_, Arc<TtsService>>,
) -> Result<bool, String> {
    let service = service.inner().clone();
    blocking(move || {
        service.downloader.credentials.set(token);
        Ok(service.downloader.credentials.configured())
    })
    .await
}
#[tauri::command]
#[specta::specta]
pub async fn preview_tts_voice(
    voice: String,
    text: String,
    app: AppHandle,
    service: State<'_, Arc<TtsService>>,
) -> Result<(), String> {
    let service = service.inner().clone();
    blocking(move || {
        let mut chosen = selection(&crate::settings::get_settings(&app));
        chosen.voice = voice;
        let ready = service.ready()?;
        let entry = catalog::entry(&chosen.model_id).map_err(|e| e.to_string())?;
        if !entry.voices.contains(&chosen.voice) {
            return Err("tts.errors.invalidVoice".into());
        }
        if !download::is_downloaded(&ready.root, entry) {
            return Err("tts.errors.notDownloaded".into());
        }
        service.preview(chosen, text)
    })
    .await
}

struct DownloadOwnership<'a> {
    map: &'a std::sync::Mutex<std::collections::BTreeMap<String, Arc<AtomicBool>>>,
    model: &'a str,
}
impl<'a> DownloadOwnership<'a> {
    fn claim(
        map: &'a std::sync::Mutex<std::collections::BTreeMap<String, Arc<AtomicBool>>>,
        model: &'a str,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        if entries.contains_key(model) {
            return Err("tts.errors.downloading".into());
        }
        entries.insert(model.into(), cancel);
        Ok(Self { map, model })
    }
}
impl Drop for DownloadOwnership<'_> {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.model);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn second_download_cannot_steal_cancel_ownership_and_error_drop_releases_it() {
        let map = std::sync::Mutex::new(std::collections::BTreeMap::new());
        let original = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        let first = DownloadOwnership::claim(&map, "pocket-en", original.clone()).unwrap();
        assert!(DownloadOwnership::claim(&map, "pocket-en", second.clone()).is_err());
        map.lock()
            .unwrap()
            .get("pocket-en")
            .unwrap()
            .store(true, Ordering::Release);
        assert!(original.load(Ordering::Acquire));
        assert!(!second.load(Ordering::Acquire));
        drop(first);
        assert!(map.lock().unwrap().is_empty());
        assert!(DownloadOwnership::claim(&map, "pocket-en", second).is_ok());
    }
}
