use crate::managers::{
    history::{HistoryClearSummary, HistoryEntry, HistoryManager, PaginatedHistory},
    transcription_jobs::TranscriptionJobManager,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Persist the layout used to compare raw and cleaned history transcripts.
#[tauri::command]
#[specta::specta]
pub async fn change_history_compare_view_setting(
    app: AppHandle,
    view: crate::settings::HistoryCompareView,
) -> Result<(), String> {
    change_history_compare_view_with(
        view,
        || crate::settings::get_settings(&app),
        |settings| crate::settings::write_settings_checked(&app, settings),
    )
}

/// Update the comparison view through the supplied settings load and save operations.
pub(crate) fn change_history_compare_view_with(
    view: crate::settings::HistoryCompareView,
    load: impl FnOnce() -> crate::settings::AppSettings,
    save: impl FnOnce(crate::settings::AppSettings) -> Result<(), String>,
) -> Result<(), String> {
    let mut settings = load();
    settings.history_compare_view = view;
    save(settings)
}

#[tauri::command]
#[specta::specta]
pub async fn get_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    cursor: Option<i64>,
    limit: Option<usize>,
) -> Result<PaginatedHistory, String> {
    history_manager
        .get_history_entries(cursor, limit)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_history_entry_saved(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    let manager = Arc::clone(history_manager.inner());
    tauri::async_runtime::spawn_blocking(move || manager.toggle_saved_status(id))
        .await
        .map_err(|error| format!("History saved-status worker failed: {error}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_history_clear_summary(
    history_manager: State<'_, Arc<HistoryManager>>,
    keep_saved: bool,
) -> Result<HistoryClearSummary, String> {
    let manager = Arc::clone(history_manager.inner());
    tauri::async_runtime::spawn_blocking(move || manager.history_clear_summary(keep_saved))
        .await
        .map_err(|error| format!("History count worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn clear_history(
    history_manager: State<'_, Arc<HistoryManager>>,
    jobs: State<'_, Arc<TranscriptionJobManager>>,
    keep_saved: bool,
) -> Result<HistoryClearSummary, String> {
    let manager = Arc::clone(history_manager.inner());
    let jobs = Arc::clone(jobs.inner());
    tauri::async_runtime::spawn_blocking(move || {
        manager.clear_history(keep_saved, |id| jobs.cancel(id))
    })
    .await
    .map_err(|error| format!("History clear worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_audio_file_path(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_name: String,
) -> Result<String, String> {
    let path = history_manager.get_audio_file_path(&file_name);
    path.to_str()
        .ok_or_else(|| "Invalid file path".to_string())
        .map(|s| s.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    jobs: State<'_, Arc<TranscriptionJobManager>>,
    id: i64,
) -> Result<(), String> {
    jobs.cancel(id);
    history_manager
        .delete_entry(id)
        .await
        .map_err(|e| e.to_string())
}

/// Queues the entry to be transcribed again with the current model. Its
/// progress and outcome arrive as `TranscriptionJob` events.
#[tauri::command]
#[specta::specta]
pub fn retry_history_entry_transcription(jobs: State<'_, Arc<TranscriptionJobManager>>, id: i64) {
    jobs.retry(id);
}

/// Stops the entry's background transcription: a queued one is dropped, a
/// running one stops after the chunk it is on. The entry keeps the text it
/// had before.
#[tauri::command]
#[specta::specta]
pub fn cancel_history_entry_transcription(jobs: State<'_, Arc<TranscriptionJobManager>>, id: i64) {
    jobs.cancel(id);
}

/// Adds an audio or video file to History and queues its transcription.
/// Fails without adding anything if the file has no audio Handy can read.
#[tauri::command]
#[specta::specta]
pub async fn transcribe_file(
    jobs: State<'_, Arc<TranscriptionJobManager>>,
    path: String,
) -> Result<HistoryEntry, String> {
    let jobs = Arc::clone(&jobs);
    tauri::async_runtime::spawn_blocking(move || jobs.transcribe_file(PathBuf::from(path)))
        .await
        .map_err(|e| format!("Failed to read the file: {}", e))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn update_history_limit(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    limit: usize,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.history_limit = limit;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn update_recording_retention_period(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    period: String,
) -> Result<(), String> {
    use crate::settings::RecordingRetentionPeriod;

    let retention_period = match period.as_str() {
        "never" => RecordingRetentionPeriod::Never,
        "preserve_limit" => RecordingRetentionPeriod::PreserveLimit,
        "days3" => RecordingRetentionPeriod::Days3,
        "weeks2" => RecordingRetentionPeriod::Weeks2,
        "months3" => RecordingRetentionPeriod::Months3,
        _ => return Err(format!("Invalid retention period: {}", period)),
    };

    let mut settings = crate::settings::get_settings(&app);
    settings.recording_retention_period = retention_period;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn compare_view_loads_before_saving_once() {
        let calls = RefCell::new(Vec::new());
        change_history_compare_view_with(
            crate::settings::HistoryCompareView::Stacked,
            || {
                calls.borrow_mut().push("load");
                crate::settings::get_default_settings()
            },
            |settings| {
                calls.borrow_mut().push("save");
                assert_eq!(
                    settings.history_compare_view,
                    crate::settings::HistoryCompareView::Stacked
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["load", "save"]);
    }
}
