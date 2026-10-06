use crate::lifecycle::State;
#[derive(Debug, Clone, PartialEq, Eq)]
/// One downloaded model choice in the native tray.
pub struct ModelChoice {
    pub id: String,
    pub label: String,
    pub checked: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
/// Cached tray choices and worker lifecycle presentation.
pub struct TtsMenu {
    pub choices: Vec<ModelChoice>,
    pub unload_enabled: bool,
    pub status_key: String,
}
/// Builds tray choices from downloaded models and lifecycle state.
pub fn tts_menu(downloaded: &[(String, String)], selected: Option<&str>, state: State) -> TtsMenu {
    let choices = downloaded
        .iter()
        .map(|(id, name)| ModelChoice {
            id: format!("tts_model:{id}"),
            label: name.clone(),
            checked: selected == Some(id.as_str()),
        })
        .collect();
    let status_key = match state {
        State::Unloaded => "tts.status.unloaded",
        State::Starting => "tts.status.starting",
        State::Ready => "tts.status.ready",
        State::Stopping => "tts.status.stopping",
        State::Failed => "tts.status.failed",
    };
    TtsMenu {
        choices,
        unload_enabled: state == State::Ready,
        status_key: status_key.into(),
    }
}
/// Dispatches a selection only when its model is downloaded.
pub fn select_tts_with<E>(
    id: &str,
    downloaded: &[(String, String)],
    select: impl FnOnce(String) -> Result<(), E>,
) -> Result<bool, E> {
    if !downloaded.iter().any(|(model, _)| model == id) {
        return Ok(false);
    }
    select(id.to_owned())?;
    Ok(true)
}
/// Runs selection or unload work away from the native callback thread.
pub fn select_tts_on_worker<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::spawn(work)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tray_entry_replays_current_sentence_and_missing_entry_cannot_mutate_cursor() {
        use crate::{
            protocol::Pcm,
            scheduler::{AudioChunk, PlaybackState, Queue, Ticket},
            selection::{Selection, select_with},
        };
        let mut queue = Queue::default();
        queue.state = PlaybackState::Playing;
        queue.next_index = 4;
        queue.playing = Some(AudioChunk {
            pause_ms: 0,
            ticket: Ticket { epoch: 0, index: 2 },
            pcm: Pcm {
                id: 1,
                sample_rate: 8000,
                samples: vec![0.0; 8000],
            },
            source_start: 30,
            source_end: 60,
        });
        let choice = Selection {
            model_id: "pocket-en".into(),
            voice: "alba".into(),
            speed: 2.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
        };
        let downloaded = vec![("pocket-en".into(), "Pocket TTS (English)".into())];
        assert!(
            !select_tts_with::<crate::selection::SelectionError>(
                "missing",
                &downloaded,
                |_| panic!("must refuse")
            )
            .unwrap()
        );
        assert_eq!(queue.playing.as_ref().unwrap().ticket.index, 2);
        assert!(
            select_tts_with("pocket-en", &downloaded, |_| select_with(
                &mut queue,
                choice,
                true,
                |_| Ok(())
            )
            .map(|change| {
                assert_eq!(change.sentence_index, 2);
            }))
            .unwrap()
        );
        assert_eq!(queue.epoch, 1);
        assert_eq!(queue.next_index, 2);
        assert!(queue.playing.is_none());
        assert!(queue.waiting.is_empty());
    }
    #[test]
    fn every_lifecycle_state_has_pure_choices_and_only_ready_can_unload() {
        let downloaded = vec![("pocket-en".into(), "Pocket TTS (English)".into())];
        for state in [
            State::Unloaded,
            State::Starting,
            State::Ready,
            State::Stopping,
            State::Failed,
        ] {
            let menu = tts_menu(&downloaded, Some("pocket-en"), state);
            assert_eq!(
                menu.choices,
                [ModelChoice {
                    id: "tts_model:pocket-en".into(),
                    label: "Pocket TTS (English)".into(),
                    checked: true
                }]
            );
            assert_eq!(menu.unload_enabled, state == State::Ready);
        }
        assert!(!tts_menu(&downloaded, Some("missing"), State::Unloaded).choices[0].checked);
        assert!(tts_menu(&[], None, State::Unloaded).choices.is_empty());
    }
    #[test]
    fn tray_selection_refuses_missing_download_and_dispatches_manager_work_off_thread() {
        let downloaded = vec![("pocket-en".into(), "Pocket TTS (English)".into())];
        assert_eq!(
            select_tts_with::<()>("missing", &downloaded, |_| panic!("must not select")),
            Ok(false)
        );
        let caller = std::thread::current().id();
        let (selected, worker) = select_tts_on_worker(move || {
            let mut selected = String::new();
            assert_eq!(
                select_tts_with::<()>("pocket-en", &downloaded, |id| {
                    selected = id;
                    Ok(())
                }),
                Ok(true)
            );
            (selected, std::thread::current().id())
        })
        .join()
        .unwrap();
        assert_eq!(selected, "pocket-en");
        assert_ne!(worker, caller);
    }
}
