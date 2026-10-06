//! Shared shortcut event handling logic
//!
//! This module contains the common logic for handling shortcut events,
//! used by both the Tauri and handy-keys implementations.

use log::warn;
use std::sync::{atomic::AtomicBool, Arc};
use tauri::{AppHandle, Manager};

use crate::actions::ACTION_MAP;
use crate::managers::audio::AudioRecordingManager;
use crate::settings::get_settings;
use crate::transcription_coordinator::is_transcribe_binding;
use crate::TranscriptionCoordinator;

static READ_ALOUD_UNWIRED_LOGGED: AtomicBool = AtomicBool::new(false);

fn action_for_binding<'a, T>(
    binding_id: &str,
    actions: &'a std::collections::HashMap<String, T>,
    logged: &AtomicBool,
    log_unwired: impl FnOnce(),
) -> Result<Option<&'a T>, ()> {
    if let Some(action) = actions.get(binding_id) {
        return Ok(Some(action));
    }
    if binding_id == "read_aloud" {
        if !logged.swap(true, std::sync::atomic::Ordering::AcqRel) {
            log_unwired();
        }
        return Ok(None);
    }
    Err(())
}

/// Handle a shortcut event from either implementation.
///
/// This function contains the shared logic for:
/// - Looking up the action in ACTION_MAP
/// - Handling the cancel binding (only fires when recording)
/// - Routing transcribe bindings to the coordinator, which applies the
///   configured activation mode (toggle / push-to-talk / hold-or-toggle)
///
/// # Arguments
/// * `app` - The Tauri app handle
/// * `binding_id` - The ID of the binding (e.g., "transcribe", "cancel")
/// * `hotkey_string` - The string representation of the hotkey
/// * `is_pressed` - Whether this is a key press (true) or release (false)
pub fn handle_shortcut_event(
    app: &AppHandle,
    binding_id: &str,
    hotkey_string: &str,
    is_pressed: bool,
) {
    if binding_id == "read_aloud" {
        if is_pressed {
            if let Some(service) = app.try_state::<Arc<crate::tts::service::TtsService>>() {
                service.read_shortcut();
            }
        }
        return;
    }
    if binding_id == "cancel" && is_pressed {
        if let Some(service) = app.try_state::<Arc<crate::tts::service::TtsService>>() {
            if service.is_reading() {
                let service = service.inner().clone();
                std::thread::spawn(move || service.close());
                return;
            }
        }
    }
    // Transcribe bindings are handled by the coordinator.
    if is_transcribe_binding(binding_id) {
        let settings = get_settings(app);
        if let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() {
            coordinator.send_input(
                binding_id,
                hotkey_string,
                is_pressed,
                settings.shortcut_activation,
                std::time::Duration::from_millis(settings.hold_threshold_ms),
            );
        } else {
            warn!("TranscriptionCoordinator is not initialized");
        }
        return;
    }

    let action =
        match action_for_binding(binding_id, &ACTION_MAP, &READ_ALOUD_UNWIRED_LOGGED, || {
            log::info!("Read aloud shortcut action is not initialized");
        }) {
            Ok(Some(action)) => action,
            Ok(None) => return,
            Err(()) => {
                warn!(
            "No action defined in ACTION_MAP for shortcut ID '{}'. Shortcut: '{}', Pressed: {}",
            binding_id, hotkey_string, is_pressed
        );
                return;
            }
        };

    // Cancel binding: only fires when recording and key is pressed
    if binding_id == "cancel" {
        let audio_manager = app.state::<Arc<AudioRecordingManager>>();
        if audio_manager.is_recording() && is_pressed {
            action.start(app, binding_id, hotkey_string);
        }
        return;
    }

    // Remaining bindings (e.g. "test") use simple start/stop on press/release.
    if is_pressed {
        action.start(app, binding_id, hotkey_string);
    } else {
        action.stop(app, binding_id, hotkey_string);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unwired_read_aloud_is_a_safe_noop_logged_once() {
        let logged = AtomicBool::new(false);
        let calls = std::cell::Cell::new(0);
        let actions = std::collections::HashMap::<String, ()>::new();
        for _pressed in [true, false, true, false] {
            assert!(action_for_binding("read_aloud", &actions, &logged, || calls
                .set(calls.get() + 1))
            .unwrap()
            .is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(action_for_binding("unknown", &actions, &logged, || panic!(
            "unknown action is not read aloud"
        ))
        .is_err());
        let wired = std::collections::HashMap::from([("read_aloud".into(), ())]);
        assert_eq!(
            action_for_binding("read_aloud", &wired, &logged, || panic!(
                "wired action is not a noop"
            )),
            Ok(Some(&()))
        );
    }
}
