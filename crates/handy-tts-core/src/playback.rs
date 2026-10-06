use crate::protocol::Pcm;
use thiserror::Error;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    Playing,
    Finished,
    DeviceLost,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PlaybackError {
    #[error("tts.errors.outputUnavailable")]
    Unavailable,
}
pub trait Output {
    fn names(&self) -> Result<Vec<String>, PlaybackError>;
    /// Opens a replacement stream, preserving the active stream on failure.
    fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError>;
    fn play(&mut self, pcm: Pcm, pause_ms: u32) -> Result<(), PlaybackError>;
    fn poll(&mut self) -> Poll;
    fn pause(&mut self);
    fn resume(&mut self);
    fn stop(&mut self);
}
pub fn resolve_output(
    tts: Option<&str>,
    handy: Option<&str>,
    available: &[String],
) -> (Option<String>, bool) {
    let selected = tts.or(handy).filter(|value| *value != "Default");
    match selected {
        Some(name) if available.iter().any(|candidate| candidate == name) => {
            (Some(name.to_owned()), false)
        }
        Some(_) => (None, true),
        None => (None, false),
    }
}
pub struct Playback<B: Output> {
    pub output: B,
    pub fallback_reported: bool,
    current: Option<(usize, Pcm, u32)>,
    pending_fallback: bool,
    paused: bool,
    replay_pending: bool,
}
impl<B: Output> Playback<B> {
    pub fn new(
        mut output: B,
        tts: Option<&str>,
        handy: Option<&str>,
    ) -> Result<Self, PlaybackError> {
        let (selected, fallback) = match output.names() {
            Ok(names) => resolve_output(tts, handy, &names),
            Err(_) => (None, true),
        };
        let fell_back = if output.open(selected.as_deref()).is_ok() {
            fallback
        } else {
            output.open(None)?;
            true
        };
        Ok(Self {
            output,
            fallback_reported: false,
            current: None,
            pending_fallback: fell_back,
            paused: false,
            replay_pending: false,
        })
    }
    pub fn reopen(&mut self, tts: Option<&str>, handy: Option<&str>) -> Result<(), PlaybackError> {
        let (selected, fallback) = match self.output.names() {
            Ok(names) => resolve_output(tts, handy, &names),
            Err(_) => (None, true),
        };
        let fell_back = if self.output.open(selected.as_deref()).is_ok() {
            fallback
        } else {
            self.output.open(None)?;
            true
        };
        self.output.stop();
        self.replay_pending = self.current.is_some();
        self.pending_fallback |= fell_back;
        if self.paused {
            self.output.pause();
        }
        Ok(())
    }
    pub fn play(&mut self, index: usize, pcm: Pcm, pause_ms: u32) -> Result<(), PlaybackError> {
        if self.output.play(pcm.clone(), pause_ms).is_err() {
            self.output.open(None)?;
            self.output.play(pcm.clone(), pause_ms)?;
            self.pending_fallback = true;
        }
        self.current = Some((index, pcm, pause_ms));
        self.replay_pending = false;
        if self.paused {
            self.output.pause();
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Poll, PlaybackError> {
        if self.replay_pending {
            if self.paused {
                return Ok(Poll::Playing);
            }
            if let Some((_, pcm, pause)) = &self.current {
                self.output.play(pcm.clone(), *pause)?;
            }
            self.replay_pending = false;
        }
        match self.output.poll() {
            Poll::Finished => {
                self.current = None;
                Ok(Poll::Finished)
            }
            Poll::Playing => Ok(Poll::Playing),
            Poll::DeviceLost => {
                self.replay_pending = self.current.is_some();
                self.output.stop();
                self.output.open(None)?;
                self.pending_fallback = true;
                if self.paused {
                    self.output.pause();
                } else if let Some((_, pcm, pause)) = &self.current {
                    self.output.play(pcm.clone(), *pause)?;
                }
                if !self.paused {
                    self.replay_pending = false;
                }
                Ok(Poll::Playing)
            }
        }
    }
    pub fn take_fallback(&mut self) -> bool {
        if self.pending_fallback && !self.fallback_reported {
            self.pending_fallback = false;
            self.fallback_reported = true;
            true
        } else {
            false
        }
    }
    pub fn current_index(&self) -> Option<usize> {
        self.current.as_ref().map(|(index, _, _)| *index)
    }
    pub fn pause(&mut self) {
        self.paused = true;
        self.output.pause();
    }
    pub fn resume(&mut self) {
        self.paused = false;
        self.output.resume();
    }
    pub fn stop(&mut self) {
        self.output.stop();
        self.current = None;
        self.replay_pending = false;
        self.paused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        names: Vec<String>,
        opened: Vec<Option<String>>,
        played: Vec<u64>,
        lost: bool,
        default_missing: bool,
        stops: usize,
        paused: bool,
        names_failed: bool,
    }
    impl Output for Fake {
        fn names(&self) -> Result<Vec<String>, PlaybackError> {
            if self.names_failed {
                return Err(PlaybackError::Unavailable);
            }
            Ok(self.names.clone())
        }
        fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
            if name.is_none() && self.default_missing {
                return Err(PlaybackError::Unavailable);
            }
            self.paused = false;
            self.opened.push(name.map(String::from));
            Ok(())
        }
        fn play(&mut self, pcm: Pcm, _: u32) -> Result<(), PlaybackError> {
            self.played.push(pcm.id);
            Ok(())
        }
        fn poll(&mut self) -> Poll {
            if self.lost {
                self.lost = false;
                Poll::DeviceLost
            } else {
                Poll::Playing
            }
        }
        fn pause(&mut self) {
            self.paused = true;
        }
        fn resume(&mut self) {
            self.paused = false;
        }
        fn stop(&mut self) {
            self.stops += 1;
        }
    }
    #[test]
    fn bluetooth_disappearance_reopens_default_once_and_retries_current_chunk() {
        let fake = Fake {
            names: vec!["Bluetooth".into()],
            ..Fake::default()
        };
        let mut playback = Playback::new(fake, Some("Bluetooth"), None).unwrap();
        playback
            .play(
                7,
                Pcm {
                    id: 7,
                    sample_rate: 24000,
                    samples: vec![0.0; 24000],
                },
                100,
            )
            .unwrap();
        playback.output.lost = true;
        playback.poll().unwrap();
        assert_eq!(playback.output.opened, [Some("Bluetooth".into()), None]);
        assert_eq!(playback.output.played, [7, 7]);
        assert_eq!(playback.current_index(), Some(7));
        assert!(playback.take_fallback());
        assert!(!playback.take_fallback());
        playback.output.lost = true;
        playback.poll().unwrap();
        assert!(!playback.take_fallback());
        playback.stop();
        assert_eq!(playback.current_index(), None);
    }
    #[test]
    fn missing_default_preserves_cursor_and_same_as_handy_resolves_correctly() {
        assert_eq!(
            resolve_output(None, Some("Bluetooth"), &["Bluetooth".into()]),
            (Some("Bluetooth".into()), false)
        );
        assert_eq!(
            resolve_output(Some("Default"), Some("Bluetooth"), &["Bluetooth".into()]),
            (None, false)
        );
        assert_eq!(resolve_output(Some("Gone"), None, &[]), (None, true));
        let fake = Fake {
            names: vec!["Bluetooth".into()],
            ..Fake::default()
        };
        let mut playback = Playback::new(fake, Some("Bluetooth"), None).unwrap();
        playback
            .play(
                9,
                Pcm {
                    id: 9,
                    sample_rate: 24000,
                    samples: vec![0.0; 24],
                },
                0,
            )
            .unwrap();
        playback.output.default_missing = true;
        playback.output.lost = true;
        assert_eq!(playback.poll(), Err(PlaybackError::Unavailable));
        assert_eq!(playback.current_index(), Some(9));
    }
    fn chunk() -> Pcm {
        Pcm {
            id: 7,
            sample_rate: 24000,
            samples: vec![0.0; 24],
        }
    }

    #[test]
    fn device_loss_while_paused_defers_replay_until_resume() {
        let mut p = Playback::new(Fake::default(), None, None).unwrap();
        p.play(7, chunk(), 100).unwrap();
        p.pause();
        p.output.lost = true;
        assert_eq!(p.poll(), Ok(Poll::Playing));
        assert_eq!(p.output.played, [7]);
        assert!(p.output.paused);
        assert_eq!(p.current_index(), Some(7));
        p.poll().unwrap();
        assert_eq!(p.output.played, [7]);
        p.resume();
        p.poll().unwrap();
        assert_eq!(p.output.played, [7, 7]);
        assert!(!p.output.paused);
    }

    #[test]
    fn enumeration_failure_uses_default_and_reports_once() {
        let fake = Fake {
            names_failed: true,
            ..Fake::default()
        };
        let mut p = Playback::new(fake, Some("Bluetooth"), None).unwrap();
        assert_eq!(p.output.opened, [None]);
        assert!(p.take_fallback());
        assert!(!p.take_fallback());
        p.reopen(Some("Bluetooth"), None).unwrap();
        assert_eq!(p.output.opened, [None, None]);
        assert!(!p.take_fallback());
        let mut p = Playback::new(Fake::default(), None, None).unwrap();
        p.output.names_failed = true;
        p.reopen(Some("Bluetooth"), None).unwrap();
        assert!(p.take_fallback());
        assert!(!p.take_fallback());
    }

    #[test]
    fn reopen_preserves_current_chunk_and_unreported_fallback() {
        let mut p = Playback::new(Fake::default(), Some("Gone"), None).unwrap();
        p.play(7, chunk(), 100).unwrap();
        p.reopen(Some("Default"), None).unwrap();
        assert_eq!(p.current_index(), Some(7));
        assert!(p.take_fallback());
        assert!(!p.take_fallback());
        p.poll().unwrap();
        assert_eq!(p.output.played, [7, 7]);
        p.output.default_missing = true;
        assert_eq!(p.reopen(None, None), Err(PlaybackError::Unavailable));
        assert_eq!(p.current_index(), Some(7));
    }
}
