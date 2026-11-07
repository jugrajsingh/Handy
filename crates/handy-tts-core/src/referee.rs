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
    ReadClipboard,
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
    pub popup_text: String,
    pub force_replace: bool,
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
            popup_text: String::new(),
            force_replace: false,
        }
    }
}
fn normalised(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_owned()
}
fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}
/// Whether two texts read the same once line endings and surrounding whitespace are ignored.
fn same_text(a: &str, b: &str) -> bool {
    normalised(a) == normalised(b)
}
impl Referee {
    fn invalidate(&mut self) {
        self.ticket = self.ticket.wrapping_add(1);
        self.capture_pending = false;
        self.captured = None;
        self.force_replace = false;
    }
    fn close_popup(&mut self, effects: &mut Vec<Effect>) {
        self.invalidate();
        self.activity = Activity::Idle;
        self.popup_open = false;
        effects.extend([Effect::StopReading, Effect::ClosePopup]);
    }
    fn open_if_ready(&mut self, effects: &mut Vec<Effect>) {
        if self.dictation_finished
            && !self.capture_pending
            && let Some(text) = self.captured.take()
        {
            self.show(text, effects);
        }
    }
    fn show(&mut self, text: String, effects: &mut Vec<Effect>) {
        self.activity = Activity::Reading;
        self.popup_open = true;
        self.popup_text.clone_from(&text);
        effects.push(Effect::OpenPopup { text });
    }
    fn start_capture(&mut self, effects: &mut Vec<Effect>, force_replace: bool) {
        self.ticket = self.ticket.wrapping_add(1);
        self.capture_pending = true;
        self.force_replace = force_replace;
        self.dictation_finished = self.activity != Activity::Dictating;
        effects.push(Effect::Capture {
            ticket: self.ticket,
        });
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
            Event::ReadShortcut if self.capture_pending || self.captured.is_some() => {
                self.invalidate();
            }
            Event::ReadShortcut if self.popup_open => {
                self.start_capture(&mut effects, false);
            }
            Event::ReadClipboard if self.popup_open && !self.capture_pending => {
                self.start_capture(&mut effects, true);
            }
            Event::ReadClipboard => {}
            Event::ReadShortcut => self.start_capture(&mut effects, false),
            Event::CaptureRefused { ticket } if ticket == self.ticket && self.capture_pending => {
                self.invalidate();
                effects.push(Effect::CaptureRefused);
            }
            Event::Clipboard { ticket, text }
                if ticket == self.ticket && self.capture_pending && self.popup_open =>
            {
                let force = self.force_replace;
                self.invalidate();
                if !force && (is_blank(&text) || same_text(&text, &self.popup_text)) {
                    self.close_popup(&mut effects);
                } else if !is_blank(&text) {
                    self.show(text, &mut effects);
                }
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
        let Effect::Capture { ticket } = r.apply(Event::ReadShortcut)[0] else {
            panic!()
        };
        assert_eq!(
            r.apply(Event::Clipboard {
                ticket,
                text: "original".into()
            }),
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

    mod open_popup {
        use super::*;

        fn open_with(text: &str) -> Referee {
            let mut r = Referee::default();
            r.apply(Event::ReadShortcut);
            let ticket = r.ticket;
            r.apply(Event::Clipboard {
                ticket,
                text: text.into(),
            });
            assert!(r.popup_open);
            r
        }

        fn capture(r: &mut Referee, event: Event) -> u64 {
            let effects = r.apply(event);
            let [Effect::Capture { ticket }] = effects.as_slice() else {
                panic!("expected one capture, got {effects:?}")
            };
            *ticket
        }

        fn clipboard(r: &mut Referee, ticket: u64, text: &str) -> Vec<Effect> {
            r.apply(Event::Clipboard {
                ticket,
                text: text.into(),
            })
        }

        fn closed(r: &Referee) -> bool {
            !r.popup_open && r.activity == Activity::Idle && !r.capture_pending
        }

        #[test]
        fn shortcut_captures_before_deciding_and_keeps_the_popup_open() {
            let mut r = open_with("old text");
            capture(&mut r, Event::ReadShortcut);
            assert!(r.popup_open && r.capture_pending);
            assert_eq!(r.activity, Activity::Reading);
        }

        #[test]
        fn shortcut_replaces_the_popup_text_when_the_clipboard_differs() {
            let mut r = open_with("old text");
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_eq!(
                clipboard(&mut r, ticket, "new text"),
                [Effect::OpenPopup {
                    text: "new text".into()
                }]
            );
            assert!(r.popup_open && !r.capture_pending);
            assert_eq!(r.activity, Activity::Reading);
            assert_eq!(r.popup_text, "new text");
        }

        #[test]
        fn shortcut_compares_against_the_text_in_the_popup_including_edits() {
            let mut r = open_with("copied");
            r.popup_text = "copied and then edited".into();
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_eq!(
                clipboard(&mut r, ticket, "copied"),
                [Effect::OpenPopup {
                    text: "copied".into()
                }]
            );
            let ticket = capture(&mut r, Event::ReadShortcut);
            r.popup_text = "copied and then edited".into();
            assert_eq!(
                clipboard(&mut r, ticket, "copied and then edited"),
                [Effect::StopReading, Effect::ClosePopup]
            );
        }

        #[test]
        fn shortcut_closes_when_the_clipboard_equals_the_popup_text() {
            let mut r = open_with("same words");
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_eq!(
                clipboard(&mut r, ticket, "same words"),
                [Effect::StopReading, Effect::ClosePopup]
            );
            assert!(closed(&r));
        }

        #[test]
        fn equality_ignores_surrounding_whitespace_and_line_ending_style() {
            let mut r = open_with("one\ntwo");
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_eq!(
                clipboard(&mut r, ticket, "  one\r\ntwo\r\n"),
                [Effect::StopReading, Effect::ClosePopup]
            );
            assert!(closed(&r));
        }

        #[test]
        fn shortcut_closes_when_the_clipboard_is_empty_or_blank() {
            for blank in ["", "   ", "\n\r\n\t"] {
                let mut r = open_with("old text");
                let ticket = capture(&mut r, Event::ReadShortcut);
                assert_eq!(
                    clipboard(&mut r, ticket, blank),
                    [Effect::StopReading, Effect::ClosePopup],
                    "{blank:?}"
                );
                assert!(closed(&r));
            }
        }

        #[test]
        fn refused_capture_leaves_the_popup_as_it_is() {
            let mut r = open_with("old text");
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_eq!(
                r.apply(Event::CaptureRefused { ticket }),
                [Effect::CaptureRefused]
            );
            assert!(r.popup_open && !r.capture_pending);
            assert_eq!(r.activity, Activity::Reading);
            assert_eq!(r.popup_text, "old text");
            assert!(clipboard(&mut r, ticket, "late").is_empty());
        }

        #[test]
        fn capture_arriving_after_the_popup_closed_opens_nothing() {
            for closer in [Event::Close, Event::Cancel, Event::DictationStart] {
                let mut r = open_with("old text");
                let ticket = capture(&mut r, Event::ReadShortcut);
                r.apply(closer.clone());
                assert!(
                    clipboard(&mut r, ticket, "late text").is_empty(),
                    "{closer:?}"
                );
                assert_ne!(r.activity, Activity::Reading, "{closer:?}");
                assert!(!r.popup_open, "{closer:?}");
            }
        }

        #[test]
        fn second_press_during_the_capture_cancels_it_and_never_leaves_two_reads() {
            let mut r = open_with("old text");
            let first = capture(&mut r, Event::ReadShortcut);
            assert!(r.apply(Event::ReadShortcut).is_empty());
            assert!(r.popup_open && !r.capture_pending);
            assert!(clipboard(&mut r, first, "stale").is_empty());
            assert_eq!(r.popup_text, "old text");
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_ne!(ticket, first);
            assert_eq!(
                clipboard(&mut r, ticket, "fresh"),
                [Effect::OpenPopup {
                    text: "fresh".into()
                }]
            );
            assert!(clipboard(&mut r, ticket, "fresh again").is_empty());
        }

        #[test]
        fn stale_capture_arriving_during_a_newer_capture_is_ignored() {
            let mut r = open_with("old text");
            let first = capture(&mut r, Event::ReadShortcut);
            assert!(r.apply(Event::ReadShortcut).is_empty());
            let newer = capture(&mut r, Event::ReadShortcut);
            assert_ne!(first, newer);
            assert!(clipboard(&mut r, first, "stale text").is_empty());
            assert!(r.capture_pending && r.popup_open);
            assert_eq!(r.popup_text, "old text");
            assert_eq!(
                clipboard(&mut r, newer, "fresh text"),
                [Effect::OpenPopup {
                    text: "fresh text".into()
                }]
            );
        }

        #[test]
        fn read_clipboard_replaces_the_text_even_when_it_is_equal() {
            let mut r = open_with("same words");
            let ticket = capture(&mut r, Event::ReadClipboard);
            assert_eq!(
                clipboard(&mut r, ticket, "same words"),
                [Effect::OpenPopup {
                    text: "same words".into()
                }]
            );
            assert!(r.popup_open && !r.force_replace);
        }

        #[test]
        fn read_clipboard_with_an_empty_or_blank_clipboard_keeps_the_popup_text() {
            for blank in ["", "  \n\t"] {
                let mut r = open_with("old text");
                let ticket = capture(&mut r, Event::ReadClipboard);
                assert!(clipboard(&mut r, ticket, blank).is_empty(), "{blank:?}");
                assert!(r.popup_open && !r.capture_pending && !r.force_replace);
                assert_eq!(r.activity, Activity::Reading);
                assert_eq!(r.popup_text, "old text");
                assert!(clipboard(&mut r, ticket, "late").is_empty());
            }
        }

        #[test]
        fn read_clipboard_refused_leaves_the_popup_and_the_next_shortcut_still_toggles() {
            let mut r = open_with("old text");
            let ticket = capture(&mut r, Event::ReadClipboard);
            assert_eq!(
                r.apply(Event::CaptureRefused { ticket }),
                [Effect::CaptureRefused]
            );
            assert!(r.popup_open && !r.force_replace);
            let ticket = capture(&mut r, Event::ReadShortcut);
            assert_eq!(
                clipboard(&mut r, ticket, "old text"),
                [Effect::StopReading, Effect::ClosePopup]
            );
        }

        #[test]
        fn read_clipboard_is_ignored_without_a_popup_or_during_a_capture() {
            let mut closed = Referee::default();
            assert!(closed.apply(Event::ReadClipboard).is_empty());
            assert!(!closed.capture_pending);
            let mut r = open_with("old text");
            let first = capture(&mut r, Event::ReadShortcut);
            assert!(r.apply(Event::ReadClipboard).is_empty());
            assert!(r.capture_pending);
            assert_eq!(r.ticket, first);
        }

        #[test]
        fn read_clipboard_capture_arriving_after_close_opens_nothing() {
            let mut r = open_with("old text");
            let ticket = capture(&mut r, Event::ReadClipboard);
            r.apply(Event::Close);
            assert!(clipboard(&mut r, ticket, "late text").is_empty());
            assert!(!r.popup_open && !r.force_replace);
        }
    }
}
