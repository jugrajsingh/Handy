use crate::protocol::Pcm;
use std::collections::VecDeque;
/// Maximum waiting PCM duration, excluding current playback.
pub const QUEUED_AUDIO_CEILING: f64 = 30.0;

/// Playback and buffering states exposed to session consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(specta::Type)]
pub enum PlaybackState {
    Waiting,
    Starting,
    Buffering,
    Playing,
    Paused,
    Finished,
    Error,
}
/// Identifies a sentence request within a selection epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    pub epoch: u64,
    pub index: usize,
}
/// PCM for a sentence or sentence part and its source range.
#[derive(Debug)]
pub struct AudioChunk {
    pub ticket: Ticket,
    pub pcm: Pcm,
    pub source_start: u32,
    pub source_end: u32,
    pub pause_ms: u32,
}
impl AudioChunk {
    /// Returns PCM duration without the trailing structural pause.
    pub fn seconds(&self) -> f64 {
        self.pcm.samples.len() as f64 / f64::from(self.pcm.sample_rate)
    }
}
/// Bounds waiting audio and rejects replies from obsolete epochs.
#[derive(Debug)]
pub struct Queue {
    pub state: PlaybackState,
    pub epoch: u64,
    pub next_index: usize,
    pub in_flight: Option<Ticket>,
    pub playing: Option<AudioChunk>,
    pub waiting: VecDeque<AudioChunk>,
    pub failed_index: Option<usize>,
    pub source_finished: bool,
    pub speed: f32,
    pub measured_rtf: f64,
    pub observations: usize,
    pub buffering_pauses: usize,
    resume_state: PlaybackState,
}
impl Default for Queue {
    fn default() -> Self {
        Self {
            state: PlaybackState::Waiting,
            epoch: 0,
            next_index: 0,
            in_flight: None,
            playing: None,
            waiting: VecDeque::new(),
            failed_index: None,
            source_finished: false,
            speed: 1.0,
            measured_rtf: 0.0,
            observations: 0,
            buffering_pauses: 0,
            resume_state: PlaybackState::Buffering,
        }
    }
}
impl Queue {
    /// Creates an idle queue at the selected playback speed.
    pub fn with_speed(speed: f32) -> Self {
        Self {
            speed,
            ..Self::default()
        }
    }
    /// Returns the total duration of waiting PCM.
    pub fn queued_seconds(&self) -> f64 {
        self.waiting.iter().map(AudioChunk::seconds).sum()
    }
    /// Computes adaptive head start from measured synthesis throughput.
    pub fn target_seconds(&self) -> f64 {
        (1.0 + (self.measured_rtf - 0.25).max(0.0) * 8.0 * f64::from(self.speed)).clamp(1.0, 5.0)
    }
    pub(crate) fn next_unspoken(&self) -> usize {
        self.playing
            .as_ref()
            .map(|chunk| chunk.ticket.index)
            .or_else(|| self.waiting.front().map(|chunk| chunk.ticket.index))
            .or(self.failed_index)
            .or_else(|| self.in_flight.map(|ticket| ticket.index))
            .unwrap_or(self.next_index)
    }
    /// Invalidates old audio and returns the sentence to replay.
    pub fn switch_selection(&mut self, downloaded: bool) -> usize {
        let index = self.next_unspoken();
        self.epoch = self.epoch.wrapping_add(1);
        self.in_flight = None;
        self.playing = None;
        self.waiting.clear();
        self.next_index = index;
        self.source_finished = false;
        self.failed_index = if downloaded { None } else { Some(index) };
        self.measured_rtf = 0.0;
        self.observations = 0;
        self.state = if downloaded {
            PlaybackState::Buffering
        } else {
            PlaybackState::Error
        };
        index
    }
    /// Starts, resumes or retries playback from the retained cursor.
    pub fn play(&mut self) {
        if matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) {
            return;
        }
        if self.state == PlaybackState::Paused {
            self.state = self.resume_state;
            self.advance();
            return;
        }
        if self.state == PlaybackState::Finished {
            self.buffering_pauses = 0;
        }
        if matches!(self.state, PlaybackState::Error | PlaybackState::Starting) {
            self.next_index = self.failed_index.unwrap_or(self.next_index);
        } else if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) {
            self.next_index = 0;
        }
        self.failed_index = None;
        self.source_finished = false;
        self.state = PlaybackState::Buffering;
        self.advance();
    }
    /// Reserves one request when playback and capacity allow it.
    pub fn request_next(&mut self) -> Option<Ticket> {
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) || self.failed_index.is_some()
            || self.source_finished
            || self.in_flight.is_some()
            || self.queued_seconds() >= QUEUED_AUDIO_CEILING - 0.00001
        {
            return None;
        }
        let ticket = Ticket {
            epoch: self.epoch,
            index: self.next_index,
        };
        self.next_index += 1;
        self.in_flight = Some(ticket);
        Some(ticket)
    }
    /// Completes a request without a synthesis timing observation.
    pub fn complete(
        &mut self,
        ticket: Ticket,
        chunk: Result<AudioChunk, ()>,
    ) -> Option<AudioChunk> {
        self.complete_timed(ticket, chunk, 0.0)
    }
    /// Completes a request and records synthesis throughput.
    pub fn complete_timed(
        &mut self,
        ticket: Ticket,
        chunk: Result<AudioChunk, ()>,
        synthesis_seconds: f64,
    ) -> Option<AudioChunk> {
        self.complete_part(ticket, chunk, synthesis_seconds, true)
    }
    /// Admits a part or returns it for retry when the queue is full.
    pub fn complete_part(
        &mut self,
        ticket: Ticket,
        chunk: Result<AudioChunk, ()>,
        synthesis_seconds: f64,
        last: bool,
    ) -> Option<AudioChunk> {
        if ticket.epoch != self.epoch || self.in_flight != Some(ticket) {
            return None;
        }
        match chunk {
            Ok(chunk) => {
                let seconds = chunk.seconds();
                if seconds <= 0.0 || !seconds.is_finite() || seconds > QUEUED_AUDIO_CEILING {
                    self.in_flight = None;
                    self.failed_index = Some(ticket.index);
                    self.advance();
                    return None;
                }
                if self.queued_seconds() + seconds > QUEUED_AUDIO_CEILING + 0.00001 {
                    return Some(chunk);
                }
                if synthesis_seconds.is_finite() && synthesis_seconds >= 0.0 {
                    let rtf = synthesis_seconds / seconds;
                    self.measured_rtf = if self.observations == 0 {
                        rtf
                    } else {
                        self.measured_rtf * 0.75 + rtf * 0.25
                    };
                    self.observations += 1;
                }
                self.waiting.push_back(chunk);
            }
            Err(()) => self.failed_index = Some(ticket.index),
        }
        if last || self.failed_index.is_some() {
            self.in_flight = None;
        }
        self.advance();
        None
    }
    /// Starts the next chunk when enough audio is ready or the source ends.
    pub fn advance(&mut self) {
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) || self.playing.is_some()
        {
            return;
        }
        if self.waiting.is_empty() {
            if self.failed_index.is_some() {
                self.state = PlaybackState::Error;
            } else if self.source_finished && self.in_flight.is_none() {
                self.state = PlaybackState::Finished;
            } else if self.state == PlaybackState::Playing {
                self.state = PlaybackState::Buffering;
                self.buffering_pauses += 1;
            }
            return;
        }
        if self.state == PlaybackState::Buffering
            && self.queued_seconds() + 0.00001 < self.target_seconds()
            && !self.source_finished
            && self.failed_index.is_none()
        {
            return;
        }
        self.state = PlaybackState::Playing;
        self.playing = self.waiting.pop_front();
    }
    /// Releases the current chunk and advances playback.
    pub fn playback_finished(&mut self) {
        self.playing = None;
        self.advance();
    }
    /// Pauses output and generation requests while preserving queued audio.
    pub fn pause(&mut self) {
        if matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) {
            self.resume_state = self.state;
            self.state = PlaybackState::Paused;
        }
    }
    /// Drops audio and invalidates pending requests.
    pub fn stop(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.state = PlaybackState::Waiting;
        self.in_flight = None;
        self.playing = None;
        self.waiting.clear();
        self.failed_index = None;
        self.next_index = 0;
        self.source_finished = false;
        self.measured_rtf = 0.0;
        self.observations = 0;
        self.buffering_pauses = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(ticket: Ticket) -> AudioChunk {
        AudioChunk {
            pause_ms: 0,
            ticket,
            pcm: Pcm {
                id: ticket.index as u64,
                sample_rate: 24000,
                samples: vec![0.0; 24000],
            },
            source_start: ticket.index as u32 * 10,
            source_end: ticket.index as u32 * 10 + 10,
        }
    }
    fn finish_request(q: &mut Queue) {
        let t = q.request_next().unwrap();
        q.complete(t, Ok(chunk(t)));
    }
    #[test]
    fn fix2_selection_keeps_earliest_waiting_sentence_after_paused_failure() {
        let mut q = Queue::with_speed(2.0);
        q.play();
        for _ in 0..2 {
            let ticket = q.request_next().unwrap();
            q.complete_timed(ticket, Ok(chunk(ticket)), 1.2);
        }
        let failed = q.request_next().unwrap();
        q.pause();
        q.complete(failed, Err(()));
        assert_eq!(q.waiting.len(), 2);
        assert_eq!(q.failed_index, Some(2));
        assert_eq!(q.switch_selection(true), 0);
    }
    #[test]
    fn fix2_starting_retains_cursor_and_finished_restart_resets_pause_count() {
        let mut q = Queue {
            state: PlaybackState::Starting,
            next_index: 7,
            failed_index: Some(7),
            ..Queue::default()
        };
        q.play();
        assert_eq!(q.request_next().unwrap().index, 7);
        q.stop();
        q.state = PlaybackState::Finished;
        q.buffering_pauses = 9;
        q.play();
        assert_eq!(q.buffering_pauses, 0);
        assert_eq!(q.request_next().unwrap().index, 0);
    }
    #[test]
    fn either_queue_reservation_or_admission_guard_bounds_audio() {
        let mut q = Queue::default();
        q.play();
        finish_request(&mut q);
        for _ in 0..30 {
            finish_request(&mut q);
        }
        if let Some(ticket) = q.request_next() {
            let deferred = q.complete_timed(ticket, Ok(chunk(ticket)), 0.1);
            assert!(deferred.is_some());
        }
        assert!(q.queued_seconds() <= 30.0);
    }
    #[test]
    fn adaptive_targets_and_buffering_transitions_use_controlled_worker_rtf() {
        for (rtf, target) in [(0.1, 1.0), (0.49, 4.84), (1.2, 5.0)] {
            let mut q = Queue::with_speed(2.0);
            q.play();
            let t = q.request_next().unwrap();
            q.complete_timed(t, Ok(chunk(t)), rtf);
            assert!((q.target_seconds() - target).abs() < 0.001);
            if target > 1.0 {
                assert_eq!(q.state, PlaybackState::Buffering);
            }
            while q.state == PlaybackState::Buffering {
                let t = q.request_next().unwrap();
                q.complete_timed(t, Ok(chunk(t)), rtf);
            }
            assert_eq!(q.state, PlaybackState::Playing);
            while q.playing.is_some() {
                q.playback_finished();
            }
            assert_eq!(q.state, PlaybackState::Buffering);
            assert_eq!(q.buffering_pauses, 1);
            let t = q.request_next().unwrap();
            q.complete_timed(t, Ok(chunk(t)), rtf);
            if target > 1.0 {
                assert_eq!(q.state, PlaybackState::Buffering);
            }
            let state = q.state;
            q.pause();
            assert!(q.request_next().is_none());
            q.play();
            assert_eq!(q.state, state);
            q.stop();
            assert_eq!(q.queued_seconds(), 0.0);
            assert!(q.playing.is_none());
            assert!(q.in_flight.is_none());
        }
    }
    #[test]
    fn ceiling_defers_an_inflight_chunk_until_room_and_short_eof_still_plays() {
        let mut q = Queue::default();
        q.play();
        finish_request(&mut q);
        for _ in 0..29 {
            finish_request(&mut q);
        }
        let t = q.request_next().unwrap();
        let mut audio = chunk(t);
        audio.pcm.samples.resize(48000, 0.0);
        let deferred = q.complete_timed(t, Ok(audio), 0.1).unwrap();
        assert_eq!(q.queued_seconds(), 29.0);
        assert_eq!(q.in_flight, Some(t));
        q.playback_finished();
        assert!(q.complete_timed(t, Ok(deferred), 0.1).is_none());
        assert_eq!(q.queued_seconds(), 30.0);
        q.stop();
        q.play();
        let t = q.request_next().unwrap();
        let mut short = chunk(t);
        short.pcm.samples.truncate(2400);
        q.source_finished = true;
        q.complete_timed(t, Ok(short), 0.1);
        assert_eq!(q.state, PlaybackState::Playing);
    }
    #[test]
    fn switching_replays_current_sentence_drops_old_voice_and_rejects_late_pcm() {
        let mut q = Queue::default();
        q.play();
        finish_request(&mut q);
        finish_request(&mut q);
        let old = q.request_next().unwrap();
        let previous_epoch = q.epoch;
        assert_eq!(q.switch_selection(true), 0);
        assert_eq!(q.epoch, previous_epoch + 1);
        assert!(q.playing.is_none());
        assert!(q.waiting.is_empty());
        let current = q.request_next().unwrap();
        assert_eq!(current.index, 0);
        q.complete(old, Ok(chunk(old)));
        assert_eq!(q.in_flight, Some(current));
        assert!(q.playing.is_none());
        q.complete(current, Ok(chunk(current)));
        assert_eq!(q.playing.as_ref().unwrap().ticket, current);
    }
    #[test]
    fn not_downloaded_switch_preserves_sentence_until_play_retry() {
        let mut q = Queue::default();
        q.play();
        finish_request(&mut q);
        finish_request(&mut q);
        q.playback_finished();
        assert_eq!(q.switch_selection(false), 1);
        assert_eq!(q.state, PlaybackState::Error);
        assert_eq!(q.failed_index, Some(1));
        assert!(q.waiting.is_empty());
        assert_eq!(q.request_next(), None);
        q.play();
        assert_eq!(q.request_next().unwrap().index, 1);
    }
    #[test]
    fn fake_worker_never_exceeds_thirty_seconds_and_pause_issues_nothing() {
        let mut q = Queue::default();
        q.play();
        for _ in 0..1200 {
            while let Some(t) = q.request_next() {
                q.complete(t, Ok(chunk(t)));
                assert!(q.queued_seconds() <= 30.0);
            }
            assert!(q.playing.is_some());
            assert_eq!(q.queued_seconds(), 30.0);
            q.pause();
            assert_eq!(q.request_next(), None);
            q.play();
            q.playback_finished();
        }
        let in_flight = q.request_next().unwrap();
        q.pause();
        q.complete(in_flight, Ok(chunk(in_flight)));
        assert_eq!(q.request_next(), None);
        assert!(q.queued_seconds() <= 30.0);
    }
    #[test]
    fn stop_and_edit_drop_audio_and_reject_late_worker_results() {
        let mut q = Queue::default();
        q.play();
        finish_request(&mut q);
        let late = q.request_next().unwrap();
        q.stop();
        q.complete(late, Ok(chunk(late)));
        assert!(q.waiting.is_empty());
        assert!(q.playing.is_none());
        assert!(q.in_flight.is_none());
        q.play();
        assert_eq!(q.request_next().unwrap().index, 0);
    }
    #[test]
    fn twenty_minute_document_crash_resumes_failed_sentence_without_skipping_preceding_audio() {
        let mut q = Queue::default();
        q.play();
        for _ in 0..600 {
            finish_request(&mut q);
            q.playback_finished();
        }
        finish_request(&mut q);
        finish_request(&mut q);
        let failure = q.request_next().unwrap();
        assert_eq!(failure.index, 602);
        q.complete(failure, Err(()));
        assert_eq!(q.request_next(), None);
        assert_eq!(q.playing.as_ref().unwrap().ticket.index, 600);
        q.playback_finished();
        assert_eq!(q.playing.as_ref().unwrap().ticket.index, 601);
        q.playback_finished();
        assert_eq!(q.state, PlaybackState::Error);
        q.play();
        assert_eq!(q.request_next().unwrap().index, 602);
    }
}
