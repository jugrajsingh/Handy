use thiserror::Error;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PlaybackError {
    #[error("tts.errors.outputUnavailable")]
    Unavailable,
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
pub struct PreparedOutput<T> {
    stream: T,
    fell_back: bool,
}
pub mod segment {
    use super::{PlaybackError, PreparedOutput, resolve_output};
    use crate::feed::{FeedEvent, Segment};
    use std::collections::VecDeque;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OutputEvent {
        Started(usize),
        Finished { index: usize, delivered: usize },
        Underrun,
        DeviceLost,
    }

    impl From<FeedEvent> for OutputEvent {
        fn from(event: FeedEvent) -> Self {
            match event {
                FeedEvent::Started(index) => Self::Started(index),
                FeedEvent::Finished { index, delivered } => Self::Finished { index, delivered },
                FeedEvent::Underrun => Self::Underrun,
            }
        }
    }

    /// One persistent stream carrying a sample-counting feed.
    pub trait Output {
        /// An uncommitted stream whose drop releases only its own resources.
        type Prepared;
        fn names(&self) -> Result<Vec<String>, PlaybackError>;
        /// Replaces the stream on success, preserving the active stream on failure.
        fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError>;
        /// Opens a candidate without changing active audio, events or clocks.
        fn prepare(&mut self, name: Option<&str>) -> Result<Self::Prepared, PlaybackError>;
        fn commit(&mut self, prepared: Self::Prepared);
        /// Configures the model sample rate before appending its segments.
        fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError>;
        /// Appends without replacing the stream; a failed append admits no samples.
        fn append(&mut self, segment: Segment) -> Result<(), PlaybackError>;
        /// Drains sample events in order, including events pulled before a device loss.
        fn events(&mut self) -> Vec<OutputEvent>;
        fn retract_unstarted(&mut self) -> Vec<Segment>;
        fn content_samples(&self) -> u64;
        /// Stops sample pulls before returning; resume continues at the same sample.
        fn pause(&mut self);
        fn resume(&mut self);
        /// Clears audio, events and clocks, keeping the stream open and unpaused.
        fn stop(&mut self);
    }

    /// Retains appended segments until sample events confirm their completion.
    pub struct Playback<O: Output> {
        pub output: O,
        pub fallback_notified: bool,
        retained: VecDeque<Segment>,
        audible: Option<usize>,
        pending_events: Vec<OutputEvent>,
        replay_at: Option<usize>,
        fallback_pending: bool,
        device_lost: bool,
        is_paused: bool,
        is_open: bool,
        past_content: u64,
    }

    impl<O: Output> Playback<O> {
        pub fn new(
            output: O,
            tts: Option<&str>,
            handy: Option<&str>,
        ) -> Result<Self, PlaybackError> {
            let mut playback = Self::idle(output);
            playback.ensure_open(tts, handy)?;
            Ok(playback)
        }

        /// Keeps output unopened until playback is requested.
        pub fn idle(output: O) -> Self {
            Self {
                output,
                fallback_notified: false,
                retained: VecDeque::new(),
                audible: None,
                pending_events: Vec::new(),
                replay_at: None,
                fallback_pending: false,
                device_lost: false,
                is_paused: false,
                is_open: false,
                past_content: 0,
            }
        }

        pub fn ensure_open(
            &mut self,
            tts: Option<&str>,
            handy: Option<&str>,
        ) -> Result<(), PlaybackError> {
            if !self.is_open {
                let prepared = self.prepare(tts, handy)?;
                self.commit(prepared);
            }
            Ok(())
        }

        /// Prepares a device choice without consuming events or changing playback.
        pub fn prepare(
            &mut self,
            tts: Option<&str>,
            handy: Option<&str>,
        ) -> Result<PreparedOutput<O::Prepared>, PlaybackError> {
            let names = self.output.names().unwrap_or_default();
            let (selected, missing) = resolve_output(tts, handy, &names);
            let (stream, fell_back) = match self.output.prepare(selected.as_deref()) {
                Ok(stream) => (stream, missing),
                Err(_) if selected.is_some() => (self.output.prepare(None)?, true),
                Err(error) => return Err(error),
            };
            Ok(PreparedOutput { stream, fell_back })
        }

        /// Swaps streams, preserving sample events and all remaining segments for replay.
        pub fn commit(&mut self, prepared: PreparedOutput<O::Prepared>) {
            if self.is_open {
                self.collect_events();
                self.begin_replay();
            }
            self.output.commit(prepared.stream);
            self.is_open = true;
            self.device_lost = false;
            self.fallback_pending |= prepared.fell_back;
            if self.is_paused {
                self.output.pause();
            }
        }

        pub fn reopen(
            &mut self,
            tts: Option<&str>,
            handy: Option<&str>,
        ) -> Result<(), PlaybackError> {
            let prepared = self.prepare(tts, handy)?;
            self.commit(prepared);
            Ok(())
        }

        pub fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
            if segment.samples() == 0 {
                return Ok(());
            }
            if !self.is_open && self.replay_at.is_some() {
                self.open_default()?;
            }
            self.append_replay()?;
            if self.output.append(segment.clone()).is_err() {
                self.collect_events();
                self.begin_replay();
                self.retained.push_back(segment);
                self.replay_at = Some(0);
                self.open_default()?;
                self.append_replay()?;
            } else {
                self.retained.push_back(segment);
            }
            Ok(())
        }

        /// Returns completion only from sample events and recovers a lost device.
        pub fn events(&mut self) -> Result<Vec<OutputEvent>, PlaybackError> {
            self.collect_events();
            if self.device_lost {
                self.begin_replay();
                self.is_open = false;
                self.open_default()?;
            } else if !self.is_open && self.replay_at.is_some() {
                self.open_default()?;
            }
            self.flush_replay()?;
            Ok(std::mem::take(&mut self.pending_events))
        }

        fn collect_events(&mut self) {
            for event in self.output.events() {
                match event {
                    OutputEvent::Started(index) => self.audible = Some(index),
                    OutputEvent::Finished { index, .. } => {
                        if let Some(position) = self
                            .retained
                            .iter()
                            .position(|segment| segment.index == index)
                        {
                            self.retained.remove(position);
                            if let Some(next) = &mut self.replay_at
                                && position < *next
                            {
                                *next -= 1;
                            }
                        }
                        if self.audible == Some(index) {
                            self.audible = None;
                        }
                    }
                    OutputEvent::DeviceLost => self.device_lost = true,
                    OutputEvent::Underrun => {}
                }
                self.pending_events.push(event);
            }
        }

        fn begin_replay(&mut self) {
            self.output.pause();
            self.collect_events();
            if self.replay_at.is_none() {
                let current = self
                    .retained
                    .iter()
                    .find(|segment| Some(segment.index) == self.audible)
                    .cloned();
                let waiting = self.output.retract_unstarted();
                let current_index = current.as_ref().map(|segment| segment.index);
                self.retained = current
                    .into_iter()
                    .chain(
                        waiting
                            .into_iter()
                            .filter(|segment| Some(segment.index) != current_index),
                    )
                    .collect();
            }
            self.past_content += self.output.content_samples();
            self.output.stop();
            self.replay_at = (!self.retained.is_empty()).then_some(0);
        }

        fn open_default(&mut self) -> Result<(), PlaybackError> {
            self.is_open = false;
            self.output.open(None)?;
            self.is_open = true;
            self.device_lost = false;
            self.fallback_pending = true;
            if self.is_paused {
                self.output.pause();
            }
            Ok(())
        }

        fn flush_replay(&mut self) -> Result<(), PlaybackError> {
            if self.is_paused {
                return Ok(());
            }
            self.append_replay()
        }

        fn append_replay(&mut self) -> Result<(), PlaybackError> {
            while let Some(index) = self.replay_at {
                if let Some(segment) = self.retained.get(index) {
                    self.output.append(segment.clone())?;
                    self.replay_at = Some(index + 1);
                } else {
                    self.replay_at = None;
                }
            }
            Ok(())
        }

        /// Retracts only unstarted segments, preserving undrained completion events.
        pub fn retract_unstarted(&mut self) -> Vec<Segment> {
            self.collect_events();
            let mut waiting = self.output.retract_unstarted();
            let mut replay_current = false;
            if let Some(next) = self.replay_at {
                for segment in self.retained.iter().skip(next) {
                    if Some(segment.index) == self.audible {
                        replay_current =
                            !waiting.iter().any(|queued| queued.index == segment.index);
                    } else if !waiting.iter().any(|queued| queued.index == segment.index) {
                        waiting.push(segment.clone());
                    }
                }
            }
            self.retained
                .retain(|segment| !waiting.iter().any(|queued| queued.index == segment.index));
            if waiting
                .iter()
                .any(|segment| Some(segment.index) == self.audible)
            {
                self.audible = None;
            }
            self.replay_at = replay_current.then_some(0);
            waiting
        }

        pub fn content_samples(&self) -> u64 {
            self.past_content + self.output.content_samples()
        }

        pub fn current_index(&self) -> Option<usize> {
            self.audible
        }

        pub fn take_fallback(&mut self) -> bool {
            if self.fallback_pending && !self.fallback_notified {
                self.fallback_pending = false;
                self.fallback_notified = true;
                true
            } else {
                false
            }
        }

        pub fn pause(&mut self) {
            self.is_paused = true;
            self.output.pause();
        }

        pub fn resume(&mut self) {
            self.is_paused = false;
            self.output.resume();
        }

        pub fn stop(&mut self) {
            self.output.stop();
            self.retained.clear();
            self.audible = None;
            self.pending_events.clear();
            self.replay_at = None;
            self.device_lost = false;
            self.is_paused = false;
            self.past_content = 0;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::feed::Feed;

        struct Fake {
            feed: Feed,
            names: Vec<String>,
            opened: Vec<Option<String>>,
            appended: Vec<usize>,
            lost: bool,
            default_missing: bool,
            stops: usize,
            paused: bool,
            names_failed: bool,
            fail_append: bool,
            fail_index: Option<usize>,
            advance_before_retract: usize,
        }

        impl Default for Fake {
            fn default() -> Self {
                Self {
                    feed: Feed::new(24_000),
                    names: Vec::new(),
                    opened: Vec::new(),
                    appended: Vec::new(),
                    lost: false,
                    default_missing: false,
                    stops: 0,
                    paused: false,
                    names_failed: false,
                    fail_append: false,
                    fail_index: None,
                    advance_before_retract: 0,
                }
            }
        }

        impl Fake {
            fn pull(&mut self, count: usize) -> Vec<f32> {
                let mut samples = vec![0.0; count];
                if !self.paused {
                    self.feed.pull(&mut samples);
                }
                samples
            }
        }

        impl Output for Fake {
            type Prepared = Option<String>;

            fn names(&self) -> Result<Vec<String>, PlaybackError> {
                if self.names_failed {
                    Err(PlaybackError::Unavailable)
                } else {
                    Ok(self.names.clone())
                }
            }

            fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
                let prepared = self.prepare(name)?;
                self.commit(prepared);
                Ok(())
            }

            fn prepare(&mut self, name: Option<&str>) -> Result<Self::Prepared, PlaybackError> {
                if name.is_none() && self.default_missing {
                    Err(PlaybackError::Unavailable)
                } else {
                    Ok(name.map(str::to_owned))
                }
            }

            fn commit(&mut self, prepared: Self::Prepared) {
                self.feed = Feed::new(24_000);
                self.paused = false;
                self.opened.push(prepared);
            }

            fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError> {
                assert_eq!(rate, self.feed.sample_rate());
                Ok(())
            }
            fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
                if self.fail_index == Some(segment.index) {
                    self.fail_index = None;
                    return Err(PlaybackError::Unavailable);
                }
                if self.fail_append {
                    self.fail_append = false;
                    return Err(PlaybackError::Unavailable);
                }
                self.appended.push(segment.index);
                self.feed.push(segment);
                Ok(())
            }

            fn events(&mut self) -> Vec<OutputEvent> {
                let mut events: Vec<_> = self
                    .feed
                    .drain_events()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                if self.lost {
                    self.lost = false;
                    events.push(OutputEvent::DeviceLost);
                }
                events
            }

            fn retract_unstarted(&mut self) -> Vec<Segment> {
                let count = std::mem::take(&mut self.advance_before_retract);
                if !self.paused {
                    self.feed.pull(&mut vec![0.0; count]);
                }
                self.feed.retract_unstarted()
            }

            fn content_samples(&self) -> u64 {
                self.feed.content_samples()
            }

            fn pause(&mut self) {
                self.paused = true;
            }

            fn resume(&mut self) {
                self.paused = false;
            }

            fn stop(&mut self) {
                self.feed = Feed::new(24_000);
                self.stops += 1;
                self.paused = false;
            }
        }

        fn chunk(index: usize) -> Segment {
            Segment {
                epoch: 4,
                index,
                speech: vec![index as f32 + 1.0; 120],
                pause: 12,
            }
        }

        fn start(playback: &mut Playback<Fake>, index: usize) {
            playback.append(chunk(index)).unwrap();
            playback.output.pull(1);
            assert_eq!(playback.events().unwrap(), [OutputEvent::Started(index)]);
        }

        #[test]
        fn bluetooth_disappearance_reopens_default_once_and_retries_current_chunk() {
            let fake = Fake {
                names: vec!["Bluetooth".into()],
                ..Fake::default()
            };
            let mut playback = Playback::new(fake, Some("Bluetooth"), None).unwrap();
            start(&mut playback, 7);
            playback.output.lost = true;
            assert_eq!(playback.events().unwrap(), [OutputEvent::DeviceLost]);
            assert_eq!(playback.output.opened, [Some("Bluetooth".into()), None]);
            assert_eq!(playback.output.appended, [7, 7]);
            assert_eq!(playback.current_index(), Some(7));
            assert!(playback.take_fallback());
            assert!(!playback.take_fallback());
            playback.output.lost = true;
            playback.events().unwrap();
            assert!(!playback.take_fallback());
            playback.stop();
            assert_eq!(playback.current_index(), None);
        }

        #[test]
        fn device_loss_replays_current_then_unstarted_in_order() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            for index in 0..3 {
                playback.append(chunk(index)).unwrap();
            }
            assert_eq!(playback.output.pull(40), vec![1.0; 40]);
            playback.output.lost = true;
            assert_eq!(
                playback.events().unwrap(),
                [OutputEvent::Started(0), OutputEvent::DeviceLost]
            );
            assert_eq!(playback.output.appended, [0, 1, 2, 0, 1, 2]);
            let samples = playback.output.pull(396);
            let expected: Vec<_> = (0..3)
                .flat_map(|index| {
                    let mut samples = chunk(index).speech;
                    samples.extend([0.0; 12]);
                    samples
                })
                .collect();
            assert_eq!(samples, expected);
            assert_eq!(
                playback.events().unwrap(),
                [
                    OutputEvent::Started(0),
                    OutputEvent::Finished {
                        index: 0,
                        delivered: 132
                    },
                    OutputEvent::Started(1),
                    OutputEvent::Finished {
                        index: 1,
                        delivered: 132
                    },
                    OutputEvent::Started(2),
                    OutputEvent::Finished {
                        index: 2,
                        delivered: 132
                    },
                ]
            );
            assert_eq!(playback.content_samples(), 436);
            assert_eq!(playback.current_index(), None);
        }

        #[test]
        fn stream_replacement_quiesces_pulls_before_capturing_replay() {
            for device_loss in [false, true] {
                let mut playback = Playback::new(Fake::default(), None, None).unwrap();
                start(&mut playback, 0);
                playback.append(chunk(1)).unwrap();
                playback.append(chunk(2)).unwrap();
                playback.output.advance_before_retract = 132;
                if device_loss {
                    playback.output.lost = true;
                } else {
                    let prepared = playback.prepare(None, None).unwrap();
                    playback.commit(prepared);
                }
                playback.events().unwrap();
                assert_eq!(playback.output.appended, [0, 1, 2, 0, 1, 2]);
                playback.output.pull(396);
                let finished: Vec<_> = playback
                    .events()
                    .unwrap()
                    .into_iter()
                    .filter_map(|event| {
                        if let OutputEvent::Finished { index, .. } = event {
                            Some(index)
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(finished, [0, 1, 2]);
            }
        }

        #[test]
        fn repeated_loss_before_replay_starts_does_not_duplicate_the_current_segment() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            start(&mut playback, 0);
            playback.append(chunk(1)).unwrap();
            for _ in 0..2 {
                playback.output.lost = true;
                playback.events().unwrap();
            }
            assert_eq!(playback.output.appended, [0, 1, 0, 1, 0, 1]);
            playback.output.pull(264);
            let finished: Vec<_> = playback
                .events()
                .unwrap()
                .into_iter()
                .filter_map(|event| {
                    if let OutputEvent::Finished { index, .. } = event {
                        Some(index)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(finished, [0, 1]);
        }

        #[test]
        fn failed_replay_append_retries_without_skipping_after_a_prior_segment_finishes() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            for index in 0..3 {
                playback.append(chunk(index)).unwrap();
            }
            playback.output.pull(1);
            playback.events().unwrap();
            playback.output.fail_index = Some(1);
            playback.output.lost = true;
            assert_eq!(playback.events(), Err(PlaybackError::Unavailable));
            assert_eq!(playback.output.appended, [0, 1, 2, 0]);
            playback.output.pull(132);
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [0, 1, 2, 0, 1, 2]);
            playback.output.pull(264);
            assert_eq!(
                playback.events().unwrap(),
                [
                    OutputEvent::Started(1),
                    OutputEvent::Finished {
                        index: 1,
                        delivered: 132
                    },
                    OutputEvent::Started(2),
                    OutputEvent::Finished {
                        index: 2,
                        delivered: 132
                    },
                ]
            );
        }

        #[test]
        fn loss_after_a_finished_segment_replays_only_remaining_segments() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            for index in 0..3 {
                playback.append(chunk(index)).unwrap();
            }
            playback.output.pull(133);
            playback.output.lost = true;
            assert_eq!(
                playback.events().unwrap(),
                [
                    OutputEvent::Started(0),
                    OutputEvent::Finished {
                        index: 0,
                        delivered: 132
                    },
                    OutputEvent::Started(1),
                    OutputEvent::DeviceLost,
                ]
            );
            assert_eq!(playback.output.appended, [0, 1, 2, 1, 2]);
            assert_eq!(playback.current_index(), Some(1));
        }

        #[test]
        fn missing_default_preserves_cursor_and_retries_all_unplayed_segments() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            start(&mut playback, 9);
            playback.append(chunk(10)).unwrap();
            playback.output.default_missing = true;
            playback.output.lost = true;
            assert_eq!(playback.events(), Err(PlaybackError::Unavailable));
            assert_eq!(playback.current_index(), Some(9));
            assert_eq!(playback.output.appended, [9, 10]);
            playback.output.default_missing = false;
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [9, 10, 9, 10]);
            assert!(playback.take_fallback());
        }

        #[test]
        fn device_loss_while_paused_defers_replay_until_resume() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            start(&mut playback, 7);
            playback.append(chunk(8)).unwrap();
            playback.pause();
            playback.output.lost = true;
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [7, 8]);
            assert!(playback.output.paused);
            assert_eq!(playback.current_index(), Some(7));
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [7, 8]);
            playback.resume();
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [7, 8, 7, 8]);
            assert!(!playback.output.paused);
        }

        #[test]
        fn enumeration_failure_uses_default_and_reports_once() {
            let fake = Fake {
                names_failed: true,
                ..Fake::default()
            };
            let mut playback = Playback::new(fake, Some("Bluetooth"), None).unwrap();
            assert_eq!(playback.output.opened, [None]);
            assert!(playback.take_fallback());
            assert!(!playback.take_fallback());
            playback.reopen(Some("Bluetooth"), None).unwrap();
            assert_eq!(playback.output.opened, [None, None]);
            assert!(!playback.take_fallback());
        }

        #[test]
        fn enumeration_failure_on_default_does_not_report_fallback() {
            for (tts, handy) in [
                (None, None),
                (Some("Default"), Some("Bluetooth")),
                (None, Some("Default")),
            ] {
                let fake = Fake {
                    names_failed: true,
                    ..Fake::default()
                };
                let mut playback = Playback::new(fake, tts, handy).unwrap();
                assert!(!playback.take_fallback());
                playback.reopen(tts, handy).unwrap();
                assert!(!playback.take_fallback());
                let prepared = playback.prepare(tts, handy).unwrap();
                playback.commit(prepared);
                assert!(!playback.take_fallback());
            }
        }

        #[test]
        fn reopen_while_paused_keeps_output_paused_and_replays_on_resume() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            start(&mut playback, 7);
            playback.append(chunk(8)).unwrap();
            playback.pause();
            playback.reopen(None, None).unwrap();
            assert!(playback.output.paused);
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [7, 8]);
            playback.resume();
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [7, 8, 7, 8]);
            assert!(!playback.output.paused);
        }

        #[test]
        fn prepare_and_drop_preserve_active_audio_events_and_fallback() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            playback.append(chunk(7)).unwrap();
            playback.append(chunk(8)).unwrap();
            playback.output.pull(1);
            let prepared = playback.prepare(Some("Gone"), None).unwrap();
            drop(prepared);
            assert_eq!(playback.output.opened, [None]);
            assert_eq!(playback.output.stops, 0);
            assert_eq!(playback.output.appended, [7, 8]);
            assert_eq!(playback.content_samples(), 1);
            assert!(!playback.take_fallback());
            assert_eq!(playback.events().unwrap(), [OutputEvent::Started(7)]);
            playback.output.default_missing = true;
            assert!(playback.prepare(None, None).is_err());
            assert_eq!(playback.current_index(), Some(7));
            assert_eq!(playback.output.stops, 0);
        }

        #[test]
        fn committed_change_preserves_undrained_events_and_replays_remaining_audio() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            playback.append(chunk(7)).unwrap();
            playback.append(chunk(8)).unwrap();
            playback.output.pull(133);
            let prepared = playback.prepare(None, None).unwrap();
            playback.commit(prepared);
            assert_eq!(playback.current_index(), Some(8));
            assert_eq!(
                playback.events().unwrap(),
                [
                    OutputEvent::Started(7),
                    OutputEvent::Finished {
                        index: 7,
                        delivered: 132
                    },
                    OutputEvent::Started(8),
                ]
            );
            assert_eq!(playback.output.appended, [7, 8, 8]);
            assert_eq!(playback.content_samples(), 133);
        }

        #[test]
        fn retract_tracks_the_audible_segment_even_before_events_are_drained() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            for index in 0..3 {
                playback.append(chunk(index)).unwrap();
            }
            playback.output.pull(10);
            assert_eq!(playback.retract_unstarted(), [chunk(1), chunk(2)]);
            playback.output.lost = true;
            assert_eq!(
                playback.events().unwrap(),
                [OutputEvent::Started(0), OutputEvent::DeviceLost]
            );
            assert_eq!(playback.output.appended, [0, 1, 2, 0]);
        }

        #[test]
        fn retract_during_paused_recovery_keeps_current_replay_and_removes_waiting_audio() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            start(&mut playback, 0);
            playback.append(chunk(1)).unwrap();
            playback.append(chunk(2)).unwrap();
            playback.pause();
            playback.output.lost = true;
            playback.events().unwrap();
            assert_eq!(playback.retract_unstarted(), [chunk(1), chunk(2)]);
            playback.resume();
            playback.events().unwrap();
            assert_eq!(playback.output.appended, [0, 1, 2, 0]);
            assert_eq!(playback.output.pull(132)[0], 1.0);
            assert_eq!(
                playback.events().unwrap(),
                [
                    OutputEvent::Started(0),
                    OutputEvent::Finished {
                        index: 0,
                        delivered: 132
                    },
                ]
            );
        }

        #[test]
        fn retract_during_partial_replay_removes_both_appended_and_pending_unstarted_audio() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            for index in 0..3 {
                playback.append(chunk(index)).unwrap();
            }
            playback.output.pull(1);
            playback.events().unwrap();
            playback.output.fail_index = Some(1);
            playback.output.lost = true;
            assert_eq!(playback.events(), Err(PlaybackError::Unavailable));
            assert_eq!(playback.retract_unstarted(), [chunk(0), chunk(1), chunk(2)]);
            playback.events().unwrap();
            assert_eq!(playback.output.feed.unstarted_len(), 0);
            assert_eq!(playback.output.appended, [0, 1, 2, 0]);
            assert_eq!(playback.current_index(), None);
        }

        #[test]
        fn pause_and_resume_continue_at_the_same_sample_without_replay() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            playback.append(chunk(0)).unwrap();
            playback.output.pull(119);
            playback.events().unwrap();
            playback.pause();
            assert_eq!(playback.output.pull(13), vec![0.0; 13]);
            assert_eq!(playback.content_samples(), 119);
            assert!(playback.events().unwrap().is_empty());
            playback.resume();
            assert_eq!(playback.output.pull(13)[0], 1.0);
            assert_eq!(playback.content_samples(), 132);
            assert_eq!(playback.output.appended, [0]);
            assert_eq!(
                playback.events().unwrap(),
                [OutputEvent::Finished {
                    index: 0,
                    delivered: 132
                }]
            );
        }

        #[test]
        fn stop_from_paused_discards_old_events_and_next_play_is_audible() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            playback.append(chunk(0)).unwrap();
            playback.output.pull(10);
            playback.pause();
            playback.stop();
            assert_eq!(playback.current_index(), None);
            assert_eq!(playback.content_samples(), 0);
            assert!(playback.events().unwrap().is_empty());
            assert_eq!(playback.output.opened.len(), 1);
            playback.append(chunk(1)).unwrap();
            assert_eq!(playback.output.pull(132)[0], 2.0);
            assert_eq!(
                playback.events().unwrap(),
                [
                    OutputEvent::Started(1),
                    OutputEvent::Finished {
                        index: 1,
                        delivered: 132
                    },
                ]
            );
        }

        #[test]
        fn append_failure_reopens_default_without_losing_current_or_waiting_audio() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            start(&mut playback, 0);
            playback.append(chunk(1)).unwrap();
            playback.output.fail_append = true;
            playback.append(chunk(2)).unwrap();
            assert_eq!(playback.output.appended, [0, 1, 0, 1, 2]);
            assert!(playback.take_fallback());
            playback.output.pull(396);
            let finished: Vec<_> = playback
                .events()
                .unwrap()
                .into_iter()
                .filter_map(|event| {
                    if let OutputEvent::Finished { index, .. } = event {
                        Some(index)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(finished, [0, 1, 2]);
        }

        fn assert_delivered_once_in_order(
            playback: &mut Playback<Fake>,
            indexes: &[usize],
            earlier_samples: u64,
        ) {
            let mut expected_samples = Vec::new();
            let mut expected_events = Vec::new();
            for &index in indexes {
                let segment = chunk(index);
                let scheduled = segment.samples();
                expected_samples.extend(segment.speech);
                expected_samples.resize(expected_samples.len() + segment.pause, 0.0);
                expected_events.push(OutputEvent::Started(index));
                expected_events.push(OutputEvent::Finished {
                    index,
                    delivered: scheduled,
                });
            }
            assert_eq!(
                playback.output.pull(expected_samples.len()),
                expected_samples
            );
            assert_eq!(playback.events().unwrap(), expected_events);
            assert_eq!(
                playback.content_samples(),
                earlier_samples + expected_samples.len() as u64
            );
            assert_eq!(playback.output.feed.underrun_samples(), 0);
            assert!(playback.events().unwrap().is_empty());
            assert_eq!(playback.current_index(), None);
        }

        #[test]
        fn first_append_failure_replays_the_segment_on_default() {
            for paused in [false, true] {
                let mut playback = Playback::new(Fake::default(), None, None).unwrap();
                if paused {
                    playback.pause();
                }
                playback.output.fail_append = true;
                assert_eq!(playback.append(chunk(0)), Ok(()));
                assert_eq!(playback.output.appended, [0]);
                assert_eq!(playback.output.opened, [None, None]);
                assert_eq!(playback.output.paused, paused);
                assert!(playback.take_fallback());
                assert!(!playback.take_fallback());
                assert_eq!(playback.append(chunk(1)), Ok(()));
                assert_eq!(playback.output.appended, [0, 1]);
                if paused {
                    assert_eq!(playback.output.pull(132), vec![0.0; 132]);
                    assert_eq!(playback.content_samples(), 0);
                    assert!(playback.events().unwrap().is_empty());
                    playback.resume();
                }
                assert_delivered_once_in_order(&mut playback, &[0, 1], 0);
            }
        }

        #[test]
        fn append_failure_after_drained_queue_replays_the_segment_on_default() {
            for paused in [false, true] {
                let mut playback = Playback::new(Fake::default(), None, None).unwrap();
                playback.append(chunk(0)).unwrap();
                assert_delivered_once_in_order(&mut playback, &[0], 0);
                assert_eq!(playback.output.pull(32), vec![0.0; 32]);
                assert_eq!(playback.events().unwrap(), [OutputEvent::Underrun]);
                assert_eq!(playback.content_samples(), 132);
                if paused {
                    playback.pause();
                }
                playback.output.fail_append = true;
                assert_eq!(playback.append(chunk(1)), Ok(()));
                assert_eq!(playback.output.appended, [0, 1]);
                assert_eq!(playback.output.opened, [None, None]);
                assert_eq!(playback.output.paused, paused);
                assert_eq!(playback.append(chunk(2)), Ok(()));
                assert_eq!(playback.output.appended, [0, 1, 2]);
                if paused {
                    assert_eq!(playback.output.pull(132), vec![0.0; 132]);
                    assert_eq!(playback.content_samples(), 132);
                    assert!(playback.events().unwrap().is_empty());
                    playback.resume();
                }
                assert_delivered_once_in_order(&mut playback, &[1, 2], 132);
            }
        }

        #[test]
        fn replacement_append_failure_returns_error_until_retained_segment_is_admitted() {
            let mut playback = Playback::new(Fake::default(), None, None).unwrap();
            playback.output.fail_index = Some(0);
            playback.output.fail_append = true;
            assert_eq!(playback.append(chunk(0)), Err(PlaybackError::Unavailable));
            assert!(playback.output.appended.is_empty());
            assert_eq!(playback.output.opened, [None, None]);
            assert!(playback.events().unwrap().is_empty());
            assert_eq!(playback.output.appended, [0]);
            assert_delivered_once_in_order(&mut playback, &[0], 0);
        }
    }
}
