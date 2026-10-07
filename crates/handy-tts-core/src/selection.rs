pub use crate::selection_options::{Selection, SelectionError, clamp_selection, maximum_speed};
/// Cursor and resume behavior of a selection transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionChange {
    pub epoch: u64,
    pub sentence_index: usize,
    pub resume: bool,
    pub missing_download: bool,
}
/// Validates and persists a selection before invalidating current audio.
pub fn select_with(
    queue: &mut crate::scheduler::Queue,
    selection: Selection,
    downloaded: bool,
    persist: impl FnOnce(&Selection) -> Result<(), SelectionError>,
) -> Result<SelectionChange, SelectionError> {
    let selection = clamp_selection(selection)?;
    let model = crate::catalog::entry(&selection.model_id).map_err(|_| SelectionError::Model)?;
    if !model.voices.contains(&selection.voice) {
        return Err(SelectionError::Voice);
    }
    persist(&selection)?;
    use crate::scheduler::PlaybackState;
    let old = queue.state;
    let resume = matches!(
        old,
        PlaybackState::Playing | PlaybackState::Buffering | PlaybackState::Starting
    );
    let sentence_index = queue.switch_selection(downloaded);
    if downloaded && old == PlaybackState::Paused {
        queue.pause();
    }
    if downloaded && old == PlaybackState::Error {
        queue.state = PlaybackState::Error;
        queue.failed_index = Some(sentence_index);
    } else if downloaded && !resume && old != PlaybackState::Paused {
        queue.stop();
    }
    Ok(SelectionChange {
        epoch: queue.epoch,
        sentence_index,
        resume: resume && downloaded,
        missing_download: !downloaded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selection() -> Selection {
        Selection {
            model_id: "pocket-en".into(),
            voice: "alba".into(),
            speed: 2.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
        }
    }
    #[test]
    fn settings_and_tray_transactions_replay_current_sentence_and_preserve_missing_position() {
        use crate::protocol::Pcm;
        use crate::scheduler::{AudioChunk, PlaybackState, Queue, Ticket};
        for entry_point in ["settings", "tray"] {
            let mut q = Queue::default();
            q.state = PlaybackState::Playing;
            q.epoch = 4;
            q.playing = Some(AudioChunk {
                pause_ms: 0,
                native: vec![0.0; 24000],
                plan: crate::speed::SpeedPlan {
                    requested: 1.0,
                    native: 1.0,
                    stretch: 1.0,
                },
                ticket: Ticket { epoch: 4, index: 7 },
                pcm: Pcm {
                    id: 7,
                    sample_rate: 24000,
                    samples: vec![0.0; 24000],
                },
                source_start: 70,
                source_end: 80,
            });
            let mut trace = vec![];
            let changed = select_with(&mut q, selection(), true, |value| {
                trace.push((entry_point, value.model_id.clone(), value.voice.clone()));
                Ok(())
            })
            .unwrap();
            assert_eq!(trace, [(entry_point, "pocket-en".into(), "alba".into())]);
            assert_eq!(changed.sentence_index, 7);
            assert!(changed.resume);
            assert_eq!(changed.epoch, 5);
            assert!(q.playing.is_none());
            assert_eq!(q.request_next().unwrap().index, 7);
            let missing = select_with(&mut q, selection(), false, |_| Ok(())).unwrap();
            assert_eq!(missing.sentence_index, 7);
            assert!(!missing.resume);
            assert!(missing.missing_download);
            assert_eq!(q.state, PlaybackState::Error);
        }
    }
    #[test]
    fn failed_persistence_or_invalid_voice_cannot_stop_current_playback() {
        let mut q = crate::scheduler::Queue::default();
        q.play();
        let epoch = q.epoch;
        assert_eq!(
            select_with(&mut q, selection(), true, |_| Err(SelectionError::Persist)),
            Err(SelectionError::Persist)
        );
        assert_eq!(q.epoch, epoch);
        let mut invalid = selection();
        invalid.voice = "missing".into();
        assert_eq!(
            select_with(&mut q, invalid, true, |_| panic!("must not persist")),
            Err(SelectionError::Voice)
        );
        assert_eq!(q.epoch, epoch);
        q.pause();
        select_with(&mut q, selection(), true, |_| Ok(())).unwrap();
        assert_eq!(q.state, crate::scheduler::PlaybackState::Paused);
        assert!(q.request_next().is_none());
    }
    #[test]
    fn all_models_keep_two_x_and_invalid_controls_are_refused() {
        let mut selection = Selection {
            model_id: "supertonic-3".into(),
            voice: "M1".into(),
            speed: 2.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
        };
        assert_eq!(clamp_selection(selection.clone()).unwrap().speed, 2.0);
        selection.model_id = "pocket-en".into();
        let pocket = clamp_selection(selection).unwrap();
        assert_eq!(pocket.speed, 2.0);
        assert_eq!(maximum_speed("kokoro-82m-int8").unwrap(), 2.0);
        assert!(maximum_speed("missing").is_err());
        for speed in [f32::NAN, 0.4, 2.1, 1.05] {
            assert!(
                clamp_selection(Selection {
                    speed,
                    ..pocket.clone()
                })
                .is_err()
            );
        }
    }
}
