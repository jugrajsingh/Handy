#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The mutually exclusive voice activity.
pub enum Activity {
    Idle,
    Dictating,
    Reading,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Whether dictation is recording or completing its output pipeline.
pub enum DictationPhase {
    Recording,
    Processing,
}
#[derive(Debug, Clone, PartialEq, Eq)]
/// Inputs to the voice-activity referee.
pub enum Event {
    DictationStart,
    DictationProcessing,
    ReadShortcut,
    Clipboard { ticket: u64, text: String },
    CaptureRefused { ticket: u64 },
    DictationFinished,
    Cancel,
    Close,
}
#[derive(Debug, Clone, PartialEq, Eq)]
/// Ordered actions for the native adapter to perform.
pub enum Effect {
    Capture { ticket: u64 },
    CaptureRefused,
    StopReading,
    ClosePopup,
    StartDictation,
    FinishRecording,
    OpenPopup { text: String },
}
#[derive(Debug, Clone)]
/// Coordinates clipboard capture, dictation completion and popup toggling.
pub struct Referee {
    pub activity: Activity,
    pub popup_open: bool,
    pub dictation_phase: DictationPhase,
    pub ticket: u64,
    pub capture_pending: bool,
    pub captured: Option<String>,
    pub dictation_finished: bool,
}
impl Default for Referee {
    fn default() -> Self {
        Self {
            activity: Activity::Idle,
            popup_open: false,
            dictation_phase: DictationPhase::Recording,
            ticket: 0,
            capture_pending: false,
            captured: None,
            dictation_finished: false,
        }
    }
}
impl Referee {
    fn invalidate(&mut self) {
        self.ticket = self.ticket.wrapping_add(1);
        self.capture_pending = false;
        self.captured = None;
    }
    fn open_if_ready(&mut self, effects: &mut Vec<Effect>) {
        if self.dictation_finished
            && !self.capture_pending
            && let Some(text) = self.captured.take()
        {
            self.activity = Activity::Reading;
            self.popup_open = true;
            effects.push(Effect::OpenPopup { text });
        }
    }
    /// Applies one input and returns its ordered effects.
    pub fn apply(&mut self, event: Event) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            Event::DictationStart => {
                self.invalidate();
                if self.activity == Activity::Reading || self.popup_open {
                    effects.extend([Effect::StopReading, Effect::ClosePopup]);
                }
                self.popup_open = false;
                self.activity = Activity::Dictating;
                self.dictation_phase = DictationPhase::Recording;
                self.dictation_finished = false;
                effects.push(Effect::StartDictation);
            }
            Event::DictationProcessing => {
                self.activity = Activity::Dictating;
                self.dictation_phase = DictationPhase::Processing;
            }
            Event::ReadShortcut if self.popup_open => {
                self.invalidate();
                self.activity = Activity::Idle;
                self.popup_open = false;
                effects.extend([Effect::StopReading, Effect::ClosePopup]);
            }
            Event::ReadShortcut if self.capture_pending || self.captured.is_some() => {
                self.invalidate();
            }
            Event::ReadShortcut => {
                self.ticket = self.ticket.wrapping_add(1);
                self.capture_pending = true;
                self.dictation_finished = self.activity != Activity::Dictating;
                effects.push(Effect::Capture {
                    ticket: self.ticket,
                });
            }
            Event::CaptureRefused { ticket } if ticket == self.ticket && self.capture_pending => {
                self.invalidate();
                effects.push(Effect::CaptureRefused);
            }
            Event::Clipboard { ticket, text } if ticket == self.ticket && self.capture_pending => {
                self.capture_pending = false;
                self.captured = Some(text);
                if self.activity == Activity::Dictating
                    && self.dictation_phase == DictationPhase::Recording
                {
                    self.dictation_phase = DictationPhase::Processing;
                    effects.push(Effect::FinishRecording);
                }
                self.open_if_ready(&mut effects);
            }
            Event::DictationFinished => {
                self.dictation_finished = true;
                if self.activity == Activity::Dictating {
                    self.activity = Activity::Idle;
                }
                self.open_if_ready(&mut effects);
            }
            Event::Cancel | Event::Close => {
                self.invalidate();
                if self.activity == Activity::Reading || self.popup_open {
                    effects.extend([Effect::StopReading, Effect::ClosePopup]);
                    self.activity = Activity::Idle;
                }
                if event == Event::Cancel {
                    self.activity = Activity::Idle;
                    self.dictation_finished = true;
                }
                self.popup_open = false;
            }
            Event::Clipboard { .. } | Event::CaptureRefused { .. } => {}
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn covers_every_spec_row_and_captures_before_recording_stop() {
        let mut r = Referee::default();
        assert_eq!(
            r.apply(Event::ReadShortcut),
            [Effect::Capture { ticket: 1 }]
        );
        assert_eq!(
            r.apply(Event::Clipboard {
                ticket: 1,
                text: "copied".into()
            }),
            [Effect::OpenPopup {
                text: "copied".into()
            }]
        );
        assert_eq!(
            r.apply(Event::DictationStart),
            [
                Effect::StopReading,
                Effect::ClosePopup,
                Effect::StartDictation
            ]
        );
        assert_eq!(
            r.apply(Event::ReadShortcut),
            [Effect::Capture { ticket: 3 }]
        );
        assert_eq!(
            r.apply(Event::Clipboard {
                ticket: 3,
                text: "original".into()
            }),
            [Effect::FinishRecording]
        );
        assert!(!r.popup_open);
        assert_eq!(
            r.apply(Event::DictationFinished),
            [Effect::OpenPopup {
                text: "original".into()
            }]
        );
        assert_eq!(
            r.apply(Event::ReadShortcut),
            [Effect::StopReading, Effect::ClosePopup]
        );
        r.apply(Event::ReadShortcut);
        let t = r.ticket;
        r.apply(Event::Clipboard {
            ticket: t,
            text: String::new(),
        });
        assert_eq!(
            r.apply(Event::Cancel),
            [Effect::StopReading, Effect::ClosePopup]
        );
    }
    #[test]
    fn read_during_post_processing_waits_for_paste_and_keeps_original_clipboard() {
        let mut r = Referee::default();
        r.apply(Event::DictationStart);
        r.apply(Event::DictationProcessing);
        assert_eq!(
            r.apply(Event::ReadShortcut),
            [Effect::Capture { ticket: 2 }]
        );
        assert!(
            r.apply(Event::Clipboard {
                ticket: 2,
                text: "before paste".into()
            })
            .is_empty()
        );
        assert!(!r.popup_open);
        assert_eq!(
            r.apply(Event::DictationFinished),
            [Effect::OpenPopup {
                text: "before paste".into()
            }]
        );
    }
    #[test]
    fn stale_capture_and_double_toggle_never_reopen_popup() {
        let mut r = Referee::default();
        r.apply(Event::ReadShortcut);
        let t = r.ticket;
        r.apply(Event::ReadShortcut);
        assert!(
            r.apply(Event::Clipboard {
                ticket: t,
                text: "stale".into()
            })
            .is_empty()
        );
        assert!(!r.popup_open);
        r.apply(Event::ReadShortcut);
        let current = r.ticket;
        assert!(
            r.apply(Event::Clipboard {
                ticket: t,
                text: "stale while a new capture is pending".into(),
            })
            .is_empty()
        );
        assert!(r.capture_pending);
        assert_eq!(r.ticket, current);
        assert!(!r.popup_open);
        let t = r.ticket;
        r.apply(Event::Cancel);
        assert!(
            r.apply(Event::Clipboard {
                ticket: t,
                text: "cancelled".into()
            })
            .is_empty()
        );
    }
    #[test]
    fn fix1_cancelled_or_failed_start_opens_next_read_without_toggling_dictation() {
        for captured in [false, true] {
            let mut referee = Referee::default();
            referee.apply(Event::DictationStart);
            let old = referee.apply(Event::ReadShortcut);
            let Effect::Capture { ticket } = old[0] else {
                panic!()
            };
            if captured {
                referee.apply(Event::Clipboard {
                    ticket,
                    text: "abandoned".into(),
                });
            }
            referee.apply(Event::Cancel);
            assert_eq!(referee.activity, Activity::Idle);
            assert!(
                referee
                    .apply(Event::Clipboard {
                        ticket,
                        text: "stale".into()
                    })
                    .is_empty()
            );
            let Effect::Capture { ticket } = referee.apply(Event::ReadShortcut)[0] else {
                panic!()
            };
            assert_eq!(
                referee.apply(Event::Clipboard {
                    ticket,
                    text: "fresh".into()
                }),
                vec![Effect::OpenPopup {
                    text: "fresh".into()
                }]
            );
        }
    }
}
