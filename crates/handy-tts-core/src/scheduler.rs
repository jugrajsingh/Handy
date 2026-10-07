use crate::{protocol::Pcm, speed::SpeedPlan};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
/// Maximum waiting PCM duration, excluding current playback.
pub const QUEUED_AUDIO_CEILING: f64 = 30.0;
/// Maximum retained PCM duration, including native audio for re-stretching.
pub const STORED_SECONDS_CEILING: f64 = 60.0;

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
    pub native: Vec<f32>,
    pub plan: SpeedPlan,
    pub source_start: u32,
    pub source_end: u32,
    pub pause_ms: u32,
}
impl AudioChunk {
    /// Returns played speech and trailing punctuation pause duration.
    pub fn seconds(&self) -> f64 {
        (self.pcm.samples.len()
            + crate::edges::pause_samples(self.pause_ms, self.plan.requested, self.pcm.sample_rate))
            as f64
            / f64::from(self.pcm.sample_rate)
    }
    fn stored_seconds(&self) -> f64 {
        (self.pcm.samples.len() + self.native.len()) as f64 / f64::from(self.pcm.sample_rate)
    }
}
/// Bounds waiting audio and rejects replies from obsolete epochs.
#[derive(Debug)]
pub struct Queue {
    pub state: PlaybackState,
    pub epoch: u64,
    pub next_index: usize,
    pub in_flight: BTreeMap<usize, Ticket>,
    pub lanes: usize,
    pub ready: BTreeMap<usize, AudioChunk>,
    pub playing: Option<AudioChunk>,
    pub waiting: VecDeque<AudioChunk>,
    pub failed_index: Option<usize>,
    pub source_finished: bool,
    pub speed: f32,
    pub rtf_played: f64,
    pub observations: usize,
    pub buffering_pauses: usize,
    pub buffering_seconds: f64,
    resume_state: PlaybackState,
    next_admit_index: usize,
    last_parts: BTreeSet<usize>,
    estimates: BTreeMap<usize, f64>,
    busy_lanes: BTreeMap<usize, usize>,
    estimated_chunk_seconds: f64,
    now: f64,
    buffering_started: Option<f64>,
}
impl Default for Queue {
    fn default() -> Self {
        Self {
            state: PlaybackState::Waiting,
            epoch: 0,
            next_index: 0,
            in_flight: BTreeMap::new(),
            lanes: 1,
            ready: BTreeMap::new(),
            playing: None,
            waiting: VecDeque::new(),
            failed_index: None,
            source_finished: false,
            speed: 1.0,
            rtf_played: 0.0,
            observations: 0,
            buffering_pauses: 0,
            buffering_seconds: 0.0,
            resume_state: PlaybackState::Buffering,
            next_admit_index: 0,
            last_parts: BTreeSet::new(),
            estimates: BTreeMap::new(),
            busy_lanes: BTreeMap::new(),
            estimated_chunk_seconds: 1.0,
            now: 0.0,
            buffering_started: None,
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
    /// Sets the request limit without cancelling already running lanes.
    pub fn set_lanes(&mut self, lanes: usize) {
        self.lanes = lanes.clamp(1, 3);
    }
    /// Sets the sentence cursor after pending audio has been invalidated.
    pub fn set_cursor(&mut self, index: usize) {
        self.next_index = index;
        self.next_admit_index = index;
    }
    /// Seeds played-time throughput before the first measured completion.
    pub fn estimate_from_rating(&mut self, rtf_1x: f64, native: f32, stretch_cost: f64) {
        if self.observations == 0
            && rtf_1x.is_finite()
            && rtf_1x >= 0.0
            && native.is_finite()
            && native > 0.0
            && stretch_cost.is_finite()
            && stretch_cost > 0.0
        {
            self.rtf_played = rtf_1x / f64::from(native) * stretch_cost;
        }
    }
    /// Returns waiting and out-of-order audio in played seconds.
    pub fn queued_seconds(&self) -> f64 {
        self.waiting
            .iter()
            .chain(self.ready.values())
            .map(AudioChunk::seconds)
            .sum()
    }
    /// Returns all retained native and stretched PCM in sample seconds.
    pub fn stored_seconds(&self) -> f64 {
        self.waiting
            .iter()
            .chain(self.ready.values())
            .chain(self.playing.iter())
            .map(AudioChunk::stored_seconds)
            .sum()
    }
    /// Computes adaptive head start from played-time throughput.
    pub fn target_seconds(&self) -> f64 {
        self.target_for_rtf(self.rtf_played)
    }
    fn target_for_rtf(&self, rtf: f64) -> f64 {
        (0.5 + (rtf - 0.25).max(0.0) * 8.0).clamp(0.5, 5.0)
    }
    /// Computes the read-ahead target in played seconds.
    pub fn read_ahead_seconds(&self) -> f64 {
        (6.0 + (self.rtf_played - 0.25).max(0.0) * 20.0).clamp(6.0, 30.0)
    }
    pub(crate) fn next_unspoken(&self) -> usize {
        self.playing
            .as_ref()
            .map(|chunk| chunk.ticket.index)
            .or_else(|| self.waiting.front().map(|chunk| chunk.ticket.index))
            .or(self.failed_index)
            .into_iter()
            .chain(self.in_flight.keys().copied())
            .chain(self.ready.keys().copied())
            .min()
            .unwrap_or(self.next_index)
    }
    fn clear_pending(&mut self) {
        self.in_flight.clear();
        self.ready.clear();
        self.last_parts.clear();
        self.estimates.clear();
        self.busy_lanes.clear();
    }
    /// Invalidates old audio and returns the sentence to replay.
    pub fn switch_selection(&mut self, downloaded: bool) -> usize {
        let index = self.next_unspoken();
        self.epoch = self.epoch.wrapping_add(1);
        self.clear_pending();
        self.playing = None;
        self.waiting.clear();
        self.next_index = index;
        self.next_admit_index = index;
        self.source_finished = false;
        self.failed_index = if downloaded { None } else { Some(index) };
        self.rtf_played = 0.0;
        self.observations = 0;
        self.estimated_chunk_seconds = 1.0;
        self.state = if downloaded {
            PlaybackState::Buffering
        } else {
            PlaybackState::Error
        };
        self.finish_buffering();
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
            if self.state == PlaybackState::Buffering && self.buffering_pauses > 0 {
                self.buffering_started = Some(self.now);
            }
            self.advance();
            return;
        }
        if self.state == PlaybackState::Finished {
            self.buffering_pauses = 0;
            self.buffering_seconds = 0.0;
        }
        if matches!(self.state, PlaybackState::Error | PlaybackState::Starting) {
            self.next_index = self.failed_index.unwrap_or(self.next_index);
            self.epoch = self.epoch.wrapping_add(1);
        } else {
            self.next_index = 0;
        }
        self.clear_pending();
        self.next_admit_index = self.next_index;
        self.failed_index = None;
        self.source_finished = false;
        self.state = PlaybackState::Buffering;
        self.advance();
    }
    /// Reserves a request when a lane and read-ahead capacity are available.
    pub fn request_next(&mut self) -> Option<Ticket> {
        let estimated = self.estimates.values().sum::<f64>();
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) || self.failed_index.is_some()
            || self.source_finished
            || self.in_flight.len() >= self.lanes
            || self.queued_seconds() >= QUEUED_AUDIO_CEILING - 0.00001
            || self.queued_seconds() + estimated >= self.read_ahead_seconds() - 0.00001
            || self.stored_seconds() >= STORED_SECONDS_CEILING - 0.00001
        {
            return None;
        }
        let ticket = Ticket {
            epoch: self.epoch,
            index: self.next_index,
        };
        self.next_index += 1;
        self.in_flight.insert(ticket.index, ticket);
        self.estimates
            .insert(ticket.index, self.estimated_chunk_seconds);
        let busy = self.in_flight.len();
        // Each timing uses the maximum number of overlapping requests.
        for count in self.busy_lanes.values_mut() {
            *count = (*count).max(busy);
        }
        self.busy_lanes.insert(ticket.index, busy);
        Some(ticket)
    }
    /// Completes a request without a synthesis timing observation.
    pub fn complete(
        &mut self,
        ticket: Ticket,
        chunk: Result<AudioChunk, ()>,
    ) -> Option<AudioChunk> {
        self.complete_part(ticket, chunk, f64::NAN, true)
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
    fn fail_ticket(&mut self, ticket: Ticket) {
        let index = self
            .failed_index
            .map_or(ticket.index, |old| old.min(ticket.index));
        self.failed_index = Some(index);
        self.in_flight.retain(|i, _| *i < index);
        self.estimates.retain(|i, _| *i < index);
        self.busy_lanes.retain(|i, _| *i < index);
        self.ready.retain(|i, _| *i < index);
        self.last_parts.retain(|i| *i < index);
        self.waiting.retain(|a| a.ticket.index < index);
        self.advance();
    }
    /// Stores out-of-order parts and admits only the contiguous prefix.
    pub fn complete_part(
        &mut self,
        ticket: Ticket,
        chunk: Result<AudioChunk, ()>,
        synthesis_seconds: f64,
        last: bool,
    ) -> Option<AudioChunk> {
        if ticket.epoch != self.epoch || self.in_flight.get(&ticket.index) != Some(&ticket) {
            return None;
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(()) => {
                self.fail_ticket(ticket);
                return None;
            }
        };
        let seconds = chunk.seconds();
        if chunk.ticket != ticket
            || seconds <= 0.0
            || !seconds.is_finite()
            || seconds > QUEUED_AUDIO_CEILING
            || chunk.stored_seconds() > STORED_SECONDS_CEILING
        {
            self.fail_ticket(ticket);
            return None;
        }
        if self.ready.contains_key(&ticket.index) {
            return Some(chunk);
        }
        let busy = self
            .busy_lanes
            .get(&ticket.index)
            .copied()
            .unwrap_or(1)
            .max(1);
        let observed_rtf = (synthesis_seconds.is_finite() && synthesis_seconds >= 0.0)
            .then(|| synthesis_seconds / seconds / busy as f64);
        let next_rtf = observed_rtf.map_or(self.rtf_played, |rtf| {
            if self.observations == 0 {
                rtf
            } else {
                self.rtf_played * 0.75 + rtf * 0.25
            }
        });
        let mut contiguous_seconds: f64 = self.waiting.iter().map(AudioChunk::seconds).sum();
        if last && ticket.index == self.next_admit_index {
            let mut next = ticket.index + 1;
            while let Some(audio) = self.ready.get(&next) {
                contiguous_seconds += audio.seconds();
                if !self.last_parts.contains(&next) {
                    break;
                }
                next += 1;
            }
        }
        let starts = ticket.index == self.next_admit_index
            && self.playing.is_none()
            && matches!(
                self.state,
                PlaybackState::Playing | PlaybackState::Buffering
            )
            && (contiguous_seconds + seconds + 0.00001 >= self.target_for_rtf(next_rtf)
                || (self.source_finished && self.in_flight.len() == 1 && self.ready.is_empty())
                || self.failed_index.is_some());
        let first = self.waiting.front().unwrap_or(&chunk);
        let released_seconds = if starts { first.seconds() } else { 0.0 };
        let released_native = if starts {
            first.native.len() as f64 / f64::from(first.pcm.sample_rate)
        } else {
            0.0
        };
        if self.queued_seconds() + seconds - released_seconds > QUEUED_AUDIO_CEILING + 0.00001 {
            return Some(chunk);
        }
        if self.stored_seconds() + chunk.stored_seconds() - released_native
            > STORED_SECONDS_CEILING + 0.00001
        {
            return Some(chunk);
        }
        // Later replies reserve one maximum-sized played chunk for the missing prefix.
        if ticket.index > self.next_admit_index
            && self.stored_seconds() + chunk.stored_seconds()
                > STORED_SECONDS_CEILING - QUEUED_AUDIO_CEILING
        {
            return Some(chunk);
        }
        if observed_rtf.is_some() {
            self.rtf_played = next_rtf;
            self.estimated_chunk_seconds = if self.observations == 0 {
                seconds
            } else {
                self.estimated_chunk_seconds * 0.75 + seconds * 0.25
            };
            self.observations += 1;
        }
        self.ready.insert(ticket.index, chunk);
        if last {
            self.in_flight.remove(&ticket.index);
            self.estimates.remove(&ticket.index);
            self.busy_lanes.remove(&ticket.index);
            self.last_parts.insert(ticket.index);
        }
        while let Some(audio) = self.ready.remove(&self.next_admit_index) {
            self.waiting.push_back(audio);
            if self.last_parts.remove(&self.next_admit_index) {
                self.next_admit_index += 1;
            } else {
                break;
            }
        }
        self.advance();
        None
    }
    fn finish_buffering(&mut self) {
        if let Some(start) = self.buffering_started.take() {
            self.buffering_seconds += self.now - start;
        }
    }
    /// Updates the caller-supplied clock and advances playback.
    pub fn advance_at(&mut self, now: f64) {
        if now.is_finite() && now >= self.now {
            if let Some(start) = &mut self.buffering_started {
                self.buffering_seconds += now - *start;
                *start = now;
            }
            self.now = now;
        }
        self.advance();
    }
    /// Starts the next chunk when contiguous audio meets head start or the source ends.
    pub fn advance(&mut self) {
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) || self.playing.is_some()
        {
            return;
        }
        if self.waiting.is_empty() {
            if self.failed_index.is_some() && self.in_flight.is_empty() && self.ready.is_empty() {
                self.state = PlaybackState::Error;
                self.finish_buffering();
            } else if self.source_finished && self.in_flight.is_empty() && self.ready.is_empty() {
                self.state = PlaybackState::Finished;
                self.finish_buffering();
            } else if self.state == PlaybackState::Playing {
                self.state = PlaybackState::Buffering;
                self.buffering_pauses += 1;
                self.buffering_started = Some(self.now);
            }
            return;
        }
        let contiguous_seconds: f64 = self.waiting.iter().map(AudioChunk::seconds).sum();
        if self.state == PlaybackState::Buffering
            && contiguous_seconds + 0.00001 < self.target_seconds()
            && !(self.source_finished && self.in_flight.is_empty() && self.ready.is_empty())
            && self.failed_index.is_none()
        {
            return;
        }
        self.state = PlaybackState::Playing;
        self.finish_buffering();
        self.playing = self.waiting.pop_front();
        if let Some(audio) = &mut self.playing {
            audio.native = Vec::new();
        }
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
            self.finish_buffering();
        }
    }
    /// Drops audio and invalidates pending requests.
    pub fn stop(&mut self) {
        self.finish_buffering();
        self.epoch = self.epoch.wrapping_add(1);
        self.state = PlaybackState::Waiting;
        self.clear_pending();
        self.playing = None;
        self.waiting.clear();
        self.failed_index = None;
        self.next_index = 0;
        self.next_admit_index = 0;
        self.source_finished = false;
        self.rtf_played = 0.0;
        self.observations = 0;
        self.estimated_chunk_seconds = 1.0;
        self.buffering_pauses = 0;
        self.buffering_seconds = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(ticket: Ticket) -> AudioChunk {
        AudioChunk {
            pause_ms: 0,
            native: vec![0.0; 24000],
            plan: SpeedPlan {
                requested: 1.0,
                native: 1.0,
                stretch: 1.0,
            },
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
    fn new_timing_cannot_admit_past_the_ceiling_using_the_previous_head_start() {
        let mut q = Queue::default();
        q.set_lanes(3);
        q.play();
        let tickets: Vec<_> = (0..3).map(|_| q.request_next().unwrap()).collect();
        let mut later = chunk(tickets[2]);
        later.pcm.samples.resize(30 * 24000, 0.0);
        later.native.clear();
        q.complete(tickets[2], Ok(later));
        assert_eq!(q.queued_seconds(), 30.0);
        assert!(
            q.complete_timed(tickets[0], Ok(chunk(tickets[0])), 10.0)
                .is_some()
        );
        assert!(q.queued_seconds() <= QUEUED_AUDIO_CEILING);
    }
    #[test]
    fn rating_seed_and_overlapping_lane_observations_use_played_time() {
        let mut q = Queue::default();
        q.estimate_from_rating(0.6, 2.0, 1.5);
        assert!((q.rtf_played - 0.45).abs() < 1e-9);
        q.set_lanes(3);
        q.play();
        let tickets: Vec<_> = (0..3).map(|_| q.request_next().unwrap()).collect();
        q.complete_timed(tickets[2], Ok(chunk(tickets[2])), 1.2);
        assert!((q.rtf_played - 0.4).abs() < 1e-9);
        q.complete_timed(tickets[0], Ok(chunk(tickets[0])), 0.6);
        assert!((q.rtf_played - 0.35).abs() < 1e-9);
        q.estimate_from_rating(9.0, 1.0, 1.0);
        assert!((q.rtf_played - 0.35).abs() < 1e-9);
    }
    #[test]
    fn seconds_includes_scaled_pause_and_native_is_released_on_start() {
        let mut q = Queue::with_speed(2.0);
        q.play();
        let t = q.request_next().unwrap();
        q.pause();
        let mut audio = chunk(t);
        audio.plan.requested = 2.0;
        audio.plan.stretch = 2.0;
        audio.pause_ms = 280;
        assert!((audio.seconds() - 1.14).abs() < 1e-9);
        q.complete(t, Ok(audio));
        assert_eq!(q.waiting.front().unwrap().native.len(), 24000);
        q.play();
        assert!(q.playing.as_ref().unwrap().native.is_empty());
    }
    #[test]
    fn buffering_clock_excludes_initial_wait_and_user_pause() {
        let mut q = Queue::default();
        q.play();
        q.advance_at(2.0);
        finish_request(&mut q);
        q.advance_at(3.0);
        q.playback_finished();
        q.advance_at(4.0);
        assert_eq!(q.buffering_seconds, 1.0);
        q.pause();
        q.advance_at(9.0);
        q.play();
        q.advance_at(10.0);
        finish_request(&mut q);
        assert_eq!(q.buffering_seconds, 2.0);
        assert_eq!(q.buffering_pauses, 1);
    }
    #[test]
    fn multipart_reply_keeps_later_sentence_behind_all_earlier_parts() {
        let mut q = Queue::default();
        q.set_lanes(2);
        q.play();
        let first = q.request_next().unwrap();
        let second = q.request_next().unwrap();
        q.pause();
        q.complete(second, Ok(chunk(second)));
        q.complete_part(first, Ok(chunk(first)), 0.1, false);
        assert_eq!(q.waiting.len(), 1);
        assert!(q.ready.contains_key(&1));
        q.complete_part(first, Ok(chunk(first)), 0.1, true);
        assert_eq!(
            q.waiting.iter().map(|a| a.ticket.index).collect::<Vec<_>>(),
            [0, 0, 1]
        );
    }
    #[test]
    fn large_out_of_order_reply_cannot_block_the_earliest_sentence() {
        let mut q = Queue::default();
        q.set_lanes(2);
        q.play();
        let first = q.request_next().unwrap();
        let second = q.request_next().unwrap();
        let mut later = chunk(second);
        later.pcm.samples.resize(30 * 24000, 0.0);
        later.native.resize(30 * 24000, 0.0);
        let deferred = q.complete(second, Ok(later)).unwrap();
        let mut earlier = chunk(first);
        earlier.pcm.samples.resize(30 * 24000, 0.0);
        earlier.native.resize(30 * 24000, 0.0);
        assert!(q.complete(first, Ok(earlier)).is_none());
        assert_eq!(q.playing.as_ref().unwrap().ticket, first);
        assert!(q.complete(second, Ok(deferred)).is_some());
        q.playback_finished();
        let mut later = chunk(second);
        later.pcm.samples.resize(30 * 24000, 0.0);
        later.native.resize(30 * 24000, 0.0);
        assert!(q.complete(second, Ok(later)).is_none());
        assert_eq!(q.playing.as_ref().unwrap().ticket, second);
        assert!(q.stored_seconds() <= STORED_SECONDS_CEILING);
    }
    #[test]
    fn out_of_order_completions_admit_in_index_order() {
        let mut q = Queue::default();
        q.set_lanes(3);
        q.play();
        q.pause();
        q.play();
        let tickets: Vec<_> = (0..3).map(|_| q.request_next().unwrap()).collect();
        q.pause();
        q.complete(tickets[2], Ok(chunk(tickets[2])));
        assert!(q.waiting.is_empty());
        assert_eq!(q.ready.len(), 1);
        q.complete(tickets[0], Ok(chunk(tickets[0])));
        assert_eq!(
            q.waiting.iter().map(|a| a.ticket.index).collect::<Vec<_>>(),
            vec![0]
        );
        q.complete(tickets[1], Ok(chunk(tickets[1])));
        assert_eq!(
            q.waiting.iter().map(|a| a.ticket.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(q.ready.is_empty());
    }
    #[test]
    fn in_flight_never_exceeds_lanes() {
        for lanes in 1..=3 {
            let mut q = Queue::default();
            q.set_lanes(lanes);
            q.play();
            for _ in 0..lanes {
                assert!(q.request_next().is_some());
            }
            assert!(q.request_next().is_none());
            assert_eq!(q.in_flight.len(), lanes);
        }
    }
    #[test]
    fn read_ahead_stops_at_target_and_ceiling() {
        for rtf in [0.1, 1.2] {
            let mut q = Queue {
                rtf_played: rtf,
                ..Queue::default()
            };
            q.play();
            for _ in 0..100 {
                let Some(t) = q.request_next() else { break };
                q.complete_timed(t, Ok(chunk(t)), rtf);
            }
            let target = if rtf == 0.1 { 6.0 } else { 25.0 };
            assert!((q.queued_seconds() - target).abs() < 1.00001);
            assert!(q.request_next().is_none());
            assert!(q.queued_seconds() <= QUEUED_AUDIO_CEILING);
        }
        let mut q = Queue::with_speed(0.5);
        q.play();
        let t = q.request_next().unwrap();
        let mut audio = chunk(t);
        audio.pcm.samples.resize(20 * 24000, 0.0);
        audio.native.resize(10 * 24000, 0.0);
        audio.plan = SpeedPlan {
            requested: 0.5,
            native: 1.0,
            stretch: 0.5,
        };
        q.complete(t, Ok(audio));
        let t = q.request_next().unwrap();
        q.pause();
        let mut audio = chunk(t);
        audio.pcm.samples.resize(20 * 24000, 0.0);
        audio.native.resize(10 * 24000, 0.0);
        audio.plan = SpeedPlan {
            requested: 0.5,
            native: 1.0,
            stretch: 0.5,
        };
        q.complete(t, Ok(audio));
        assert_eq!(q.stored_seconds(), 50.0);
        let t = Ticket {
            epoch: q.epoch,
            index: 2,
        };
        q.in_flight.insert(2, t);
        let mut audio = chunk(t);
        audio.pcm.samples.resize(10 * 24000, 0.0);
        audio.native.resize(5 * 24000, 0.0);
        audio.plan = SpeedPlan {
            requested: 0.5,
            native: 1.0,
            stretch: 0.5,
        };
        assert!(q.complete(t, Ok(audio)).is_some());
        assert!(q.stored_seconds() <= STORED_SECONDS_CEILING);
    }
    #[test]
    fn head_start_has_no_speed_factor() {
        for (rtf, target) in [(0.25, 0.5), (0.49, 2.42), (0.9, 5.0)] {
            for speed in [1.0, 2.0] {
                let q = Queue {
                    rtf_played: rtf,
                    speed,
                    ..Queue::default()
                };
                assert!((q.target_seconds() - target).abs() < 1e-9);
            }
        }
    }
    #[test]
    fn failed_lane_stops_after_earlier_chunks_play() {
        let mut q = Queue::default();
        q.set_lanes(3);
        q.play();
        let tickets: Vec<_> = (0..3).map(|_| q.request_next().unwrap()).collect();
        q.complete(tickets[2], Ok(chunk(tickets[2])));
        q.complete(tickets[1], Err(()));
        assert_ne!(q.state, PlaybackState::Error);
        assert!(q.request_next().is_none());
        q.complete(tickets[0], Ok(chunk(tickets[0])));
        assert_eq!(q.playing.as_ref().unwrap().ticket.index, 0);
        q.playback_finished();
        assert_eq!(q.state, PlaybackState::Error);
        assert!(q.ready.is_empty());
        q.play();
        assert_eq!(q.request_next().unwrap().index, 1);
    }
    #[test]
    fn stale_epoch_completion_in_ready_is_dropped() {
        let mut q = Queue::default();
        q.set_lanes(3);
        q.play();
        let old: Vec<_> = (0..3).map(|_| q.request_next().unwrap()).collect();
        q.complete(old[2], Ok(chunk(old[2])));
        assert_eq!(q.ready.len(), 1);
        q.switch_selection(true);
        assert!(q.ready.is_empty());
        q.complete(old[1], Ok(chunk(old[1])));
        assert!(q.ready.is_empty());
        let new = q.request_next().unwrap();
        q.complete(new, Ok(chunk(new)));
        assert_eq!(q.playing.as_ref().unwrap().ticket, new);
        q.stop();
        assert!(q.ready.is_empty());
        assert!(q.in_flight.is_empty());
    }
    #[test]
    fn set_lanes_lowers_before_next_request() {
        let mut q = Queue::default();
        q.set_lanes(3);
        q.play();
        let tickets: Vec<_> = (0..3).map(|_| q.request_next().unwrap()).collect();
        q.set_lanes(1);
        for t in &tickets[..2] {
            q.complete(*t, Ok(chunk(*t)));
            assert!(q.request_next().is_none());
        }
        q.complete(tickets[2], Ok(chunk(tickets[2])));
        assert!(q.request_next().is_some());
        assert!(q.request_next().is_none());
        q.set_lanes(0);
        assert_eq!(q.lanes, 1);
        q.set_lanes(9);
        assert_eq!(q.lanes, 3);
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
        q.estimates.insert(99, 1.0);
        q.play();
        assert!(q.estimates.is_empty());
        assert_eq!(q.buffering_pauses, 0);
        assert_eq!(q.request_next().unwrap().index, 0);
    }
    #[test]
    fn either_queue_reservation_or_admission_guard_bounds_audio() {
        let mut q = Queue {
            rtf_played: 2.0,
            ..Queue::default()
        };
        q.play();
        q.source_finished = true;
        q.waiting.push_back(chunk(Ticket {
            epoch: q.epoch,
            index: 0,
        }));
        q.advance();
        q.source_finished = false;
        q.next_index = 1;
        q.next_admit_index = 1;
        for _ in 0..30 {
            let Some(t) = q.request_next() else { break };
            let mut audio = chunk(t);
            audio.native.clear();
            q.complete(t, Ok(audio));
        }
        if let Some(ticket) = q.request_next() {
            let deferred = q.complete_timed(ticket, Ok(chunk(ticket)), 0.1);
            assert!(deferred.is_some());
        }
        assert!(q.queued_seconds() <= 30.0);
    }
    #[test]
    fn adaptive_targets_and_buffering_transitions_use_controlled_worker_rtf() {
        for (rtf, target) in [(0.1, 0.5), (0.49, 2.42), (1.2, 5.0)] {
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
            assert!(q.in_flight.is_empty());
        }
    }
    #[test]
    fn ceiling_defers_an_inflight_chunk_until_room_and_short_eof_still_plays() {
        let mut q = Queue {
            rtf_played: 2.0,
            ..Queue::default()
        };
        q.play();
        finish_request(&mut q);
        for _ in 0..29 {
            let t = q.request_next().unwrap();
            let mut audio = chunk(t);
            audio.native.clear();
            q.complete(t, Ok(audio));
        }
        let t = q.request_next().unwrap();
        let mut audio = chunk(t);
        audio.pcm.samples.resize(48000, 0.0);
        let deferred = q.complete_timed(t, Ok(audio), 0.1).unwrap();
        assert_eq!(q.queued_seconds(), 29.0);
        assert_eq!(q.in_flight.get(&t.index), Some(&t));
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
        assert!(q.ready.is_empty());
        assert_eq!(q.in_flight.get(&current.index), Some(&current));
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
        let mut q = Queue {
            rtf_played: 2.0,
            ..Queue::default()
        };
        q.play();
        for _ in 0..1200 {
            while let Some(t) = q.request_next() {
                let mut audio = chunk(t);
                audio.native.clear();
                q.complete(t, Ok(audio));
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
        assert!(q.in_flight.is_empty());
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
