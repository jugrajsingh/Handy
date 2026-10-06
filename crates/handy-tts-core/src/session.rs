use crate::{
    dsp,
    playback::{Output, Playback, Poll},
    protocol::Pcm,
    scheduler::{AudioChunk, PlaybackState, QUEUED_AUDIO_CEILING, Queue, Ticket},
    selection::{Selection, SelectionChange, SelectionError, select_with},
    text::{TextChunk, TextCursor},
};
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};

type ReportedPosition = (u32, u32, f64, f64);
type CursorFactory = for<'s> fn(&'s str, bool) -> Result<TextCursor<'s>, String>;

fn text_cursor(source: &str, read_code: bool) -> Result<TextCursor<'_>, String> {
    TextCursor::new(source, read_code).map_err(|_| "tts.errors.text".to_string())
}

fn dsp_clock() -> Box<dyn Fn() -> f64 + Send> {
    #[cfg(test)]
    {
        Box::new(|| 0.0)
    }
    #[cfg(not(test))]
    {
        let origin = std::time::Instant::now();
        Box::new(move || origin.elapsed().as_secs_f64())
    }
}

/// Splits NPU input at word boundaries using the engine's token or frame capacity check.
pub fn synthesize_for_device(
    device: crate::device::Device,
    text: &str,
    mut fits: impl FnMut(&str) -> Result<bool, String>,
    mut synthesize: impl FnMut(&str) -> Result<Pcm, String>,
) -> Result<Pcm, String> {
    if device != crate::device::Device::Npu {
        return synthesize(text);
    }
    let mut pending = vec![text.trim()];
    let mut result: Option<Pcm> = None;
    while let Some(part) = pending.pop() {
        if part.is_empty() {
            return Err("tts.errors.text".into());
        }
        if !fits(part)? {
            let split = part
                .char_indices()
                .filter(|(_, ch)| ch.is_whitespace())
                .min_by_key(|(index, _)| index.abs_diff(part.len() / 2))
                .map(|(index, _)| index)
                .ok_or_else(|| "tts.errors.text".to_string())?;
            pending.push(part[split..].trim_start());
            pending.push(part[..split].trim_end());
            continue;
        }
        let pcm = synthesize(part)?;
        if pcm.samples.is_empty()
            || !(8000..=48000).contains(&pcm.sample_rate)
            || pcm.samples.iter().any(|sample| !sample.is_finite())
            || pcm.samples.len() > crate::protocol::MAX_SAMPLES
        {
            return Err("tts.errors.audio".into());
        }
        match &mut result {
            Some(combined) => {
                if pcm.sample_rate != combined.sample_rate
                    || pcm.id != combined.id
                    || pcm.samples.len() > crate::protocol::MAX_SAMPLES - combined.samples.len()
                {
                    return Err("tts.errors.audio".into());
                }
                combined.samples.extend(pcm.samples);
            }
            None => result = Some(pcm),
        }
    }
    result.ok_or_else(|| "tts.errors.audio".into())
}

/// Worker result and synthesis time for one request.
pub struct Completion {
    pub ticket: Ticket,
    pub result: Result<Pcm, String>,
    pub synthesis_seconds: f64,
}
/// Nonblocking transport implemented by a supervised worker adapter.
pub trait Worker {
    fn request(&mut self, ticket: Ticket, text: &str, selection: &Selection) -> Result<(), String>;
    fn poll(&mut self) -> Option<Completion>;
    fn cancel(&mut self, ticket: Ticket);
    fn selection_changed(&mut self);
}
/// Playback progress and errors suitable for frontend events.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct SessionSnapshot {
    pub epoch: u64,
    pub state: PlaybackState,
    pub source_start: u32,
    pub source_end: u32,
    pub elapsed_seconds: f64,
    pub remaining_seconds: f64,
    pub progress: f64,
    pub error: Option<String>,
    pub output_fallback: bool,
    pub queued_audio_seconds: f64,
    pub buffering_pauses: usize,
    pub head_start_seconds: Option<f64>,
}
struct Parts {
    ticket: Ticket,
    samples: VecDeque<Pcm>,
    elapsed_per_second: f64,
}
/// Owns incremental text, bounded audio scheduling and output playback.
pub struct Session<'a, W: Worker, O: Output> {
    source: &'a str,
    source_utf16: usize,
    handy_output: Option<String>,
    cursor: TextCursor<'a>,
    cursor_factory: CursorFactory,
    pending_text: Option<TextChunk>,
    dsp_clock: Box<dyn Fn() -> f64 + Send>,
    reported_position: Cell<Option<ReportedPosition>>,
    pub worker: W,
    pub playback: Playback<O>,
    pub queue: Queue,
    selection: Selection,
    metadata: BTreeMap<usize, TextChunk>,
    parts: Option<Parts>,
    output_ticket: Option<Ticket>,
    current_started: f64,
    current_elapsed: f64,
    played_seconds: f64,
    now: f64,
    initial_wait: Option<f64>,
    first_play: Option<f64>,
    error: Option<String>,
    output_fallback: bool,
}
impl<'a, W: Worker, O: Output> Session<'a, W, O> {
    /// Creates an idle session without opening an output device.
    pub fn new(
        source: &'a str,
        selection: Selection,
        worker: W,
        output: O,
        handy_output: Option<&str>,
    ) -> Result<Self, String> {
        let cursor = TextCursor::new(source, selection.read_code)
            .map_err(|_| "tts.errors.text".to_string())?;
        let playback = Playback::idle(output);
        Ok(Self {
            source,
            source_utf16: source.encode_utf16().count(),
            handy_output: handy_output.map(str::to_owned),
            cursor,
            cursor_factory: text_cursor,
            pending_text: None,
            dsp_clock: dsp_clock(),
            reported_position: Cell::new(None),
            worker,
            playback,
            queue: Queue::with_speed(selection.speed),
            selection,
            metadata: BTreeMap::new(),
            parts: None,
            output_ticket: None,
            current_started: 0.0,
            current_elapsed: 0.0,
            played_seconds: 0.0,
            now: 0.0,
            initial_wait: None,
            first_play: None,
            error: None,
            output_fallback: false,
        })
    }
    /// Supplies the monotonic DSP clock used when measuring completion latency.
    pub fn set_dsp_clock(&mut self, clock: impl Fn() -> f64 + Send + 'static) {
        self.dsp_clock = Box::new(clock);
    }
    fn prepare_cursor(
        &self,
        read_code: bool,
        index: usize,
        position: Option<u32>,
    ) -> Result<(TextCursor<'a>, Option<TextChunk>), String> {
        let mut cursor = (self.cursor_factory)(self.source, read_code)?;
        while let Some(text) = cursor
            .next_chunk()
            .map_err(|_| "tts.errors.text".to_string())?
        {
            let found = position.map_or(text.index >= index, |pos| text.source_end > pos);
            if found {
                return Ok((cursor, Some(text)));
            }
        }
        Ok((cursor, None))
    }
    fn seek(&mut self, index: usize) -> Result<(), String> {
        let (cursor, pending) = self.prepare_cursor(self.selection.read_code, index, None)?;
        self.cursor = cursor;
        self.pending_text = pending;
        self.metadata.clear();
        if let Some(text) = &self.pending_text {
            self.metadata.insert(text.index, text.clone());
        }
        self.parts = None;
        self.output_ticket = None;
        Ok(())
    }
    /// Starts, resumes or retries playback from the retained cursor.
    pub fn play(&mut self) -> Result<(), String> {
        if self.source.trim().is_empty() {
            return Ok(());
        }
        self.playback
            .ensure_open(
                self.selection.output_device.as_deref(),
                self.handy_output.as_deref(),
            )
            .map_err(|_| "tts.errors.audio".to_string())?;
        let old = self.queue.state;
        self.queue.play();
        if old == PlaybackState::Paused {
            self.playback.resume();
            self.current_started = self.now;
        } else if !matches!(old, PlaybackState::Playing | PlaybackState::Buffering) {
            self.seek(self.queue.next_index)?;
            if old != PlaybackState::Error {
                self.reported_position.set(None);
                self.played_seconds = 0.0;
                self.current_elapsed = 0.0;
            }
            self.initial_wait = Some(self.now);
            self.first_play = None;
            self.error = None;
        }
        Ok(())
    }
    /// Pauses output and generation requests while preserving queued audio.
    pub fn pause(&mut self) {
        if !matches!(
            self.queue.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) {
            return;
        }
        if self.queue.state == PlaybackState::Playing {
            self.current_elapsed += (self.now - self.current_started).max(0.0);
        }
        self.queue.pause();
        self.playback.pause();
    }
    /// Drops audio and invalidates pending requests.
    pub fn stop(&mut self) {
        if let Some(ticket) = self.queue.in_flight {
            self.worker.cancel(ticket);
        }
        self.playback.stop();
        self.queue.stop();
        self.metadata.clear();
        self.parts = None;
        self.output_ticket = None;
        self.error = None;
        self.pending_text = None;
        self.reported_position.set(None);
        self.played_seconds = 0.0;
        self.current_elapsed = 0.0;
        self.first_play = None;
    }
    /// Stops at the active sentence and rejects stale audio until retry.
    pub fn fail(&mut self, error: String) {
        if let Some(ticket) = self.queue.in_flight {
            self.worker.cancel(ticket);
        }
        self.playback.stop();
        let index = self.queue.switch_selection(true);
        self.queue.failed_index = Some(index);
        self.queue.state = PlaybackState::Error;
        self.parts = None;
        self.output_ticket = None;
        self.current_elapsed = 0.0;
        self.error = Some(error);
    }
    /// Applies a selection and replays the current sentence when active.
    pub fn select(
        &mut self,
        selection: Selection,
        downloaded: bool,
        persist: impl FnOnce(&Selection) -> Result<(), SelectionError>,
    ) -> Result<SelectionChange, SelectionError> {
        let index = self.queue.next_unspoken();
        let position = (self.selection.read_code != selection.read_code)
            .then(|| self.metadata.get(&index).map(|text| text.source_start))
            .flatten();
        let (cursor, pending) = self
            .prepare_cursor(selection.read_code, index, position)
            .map_err(|_| SelectionError::Controls)?;
        let new_index = pending.as_ref().map_or(index, |text| text.index);
        let old_ticket = self.queue.in_flight;
        let reload = self.selection.model_id != selection.model_id
            || self.selection.voice != selection.voice;
        let output_changed = self.selection.output_device != selection.output_device;
        let mut changed = select_with(&mut self.queue, selection.clone(), downloaded, |value| {
            let prepared = if output_changed {
                Some(
                    self.playback
                        .prepare(
                            selection.output_device.as_deref(),
                            self.handy_output.as_deref(),
                        )
                        .map_err(|_| SelectionError::Controls)?,
                )
            } else {
                None
            };
            persist(value)?;
            if let Some(prepared) = prepared {
                self.playback.commit(prepared);
            }
            Ok(())
        })?;
        self.playback.stop();
        if let Some(ticket) = old_ticket {
            self.worker.cancel(ticket);
        }
        if reload {
            self.worker.selection_changed();
        }
        self.selection = selection;
        self.queue.speed = self.selection.speed;
        self.queue.next_index = new_index;
        if self.queue.failed_index.is_some() {
            self.queue.failed_index = Some(new_index);
        }
        changed.sentence_index = new_index;
        self.cursor = cursor;
        self.pending_text = pending;
        self.metadata.clear();
        if let Some(text) = &self.pending_text {
            self.metadata.insert(text.index, text.clone());
        }
        self.parts = None;
        self.output_ticket = None;
        self.current_elapsed = 0.0;
        self.error = changed
            .missing_download
            .then(|| "tts.errors.notDownloaded".into());
        Ok(changed)
    }
    fn received(&mut self, completion: Completion) {
        if completion.ticket.epoch != self.queue.epoch
            || self.queue.in_flight != Some(completion.ticket)
        {
            return;
        }
        let pcm = match completion.result {
            Ok(pcm) => pcm,
            Err(error) => {
                self.error = Some(error);
                self.queue.complete(completion.ticket, Err(()));
                return;
            }
        };
        let began = (self.dsp_clock)();
        let pcm = match dsp::transform(
            pcm,
            self.selection.speed,
            self.selection.pitch,
            self.selection.model_id != "pocket-en",
        ) {
            Ok(pcm) => pcm,
            Err(_) => {
                self.error = Some("tts.errors.audio".into());
                self.queue.complete(completion.ticket, Err(()));
                return;
            }
        };
        let elapsed = completion.synthesis_seconds + ((self.dsp_clock)() - began).max(0.0);
        let seconds = pcm.samples.len() as f64 / f64::from(pcm.sample_rate);
        let limit = (QUEUED_AUDIO_CEILING * f64::from(pcm.sample_rate)) as usize;
        let mut data = pcm.samples.into_iter();
        let mut samples = VecDeque::new();
        loop {
            let next: Vec<f32> = data.by_ref().take(limit).collect();
            if next.is_empty() {
                break;
            }
            samples.push_back(Pcm {
                id: pcm.id,
                sample_rate: pcm.sample_rate,
                samples: next,
            });
        }
        self.parts = Some(Parts {
            ticket: completion.ticket,
            samples,
            elapsed_per_second: elapsed / seconds,
        });
    }
    fn admit_parts(&mut self) {
        let Some(mut parts) = self.parts.take() else {
            return;
        };
        while let Some(pcm) = parts.samples.pop_front() {
            let seconds = pcm.samples.len() as f64 / f64::from(pcm.sample_rate);
            let Some(text) = self.metadata.get(&parts.ticket.index) else {
                self.error = Some("tts.errors.text".into());
                self.queue.complete(parts.ticket, Err(()));
                return;
            };
            let last = parts.samples.is_empty();
            let audio = AudioChunk {
                pause_ms: if last { text.pause_ms } else { 0 },
                ticket: parts.ticket,
                pcm,
                source_start: text.source_start,
                source_end: text.source_end,
            };
            if let Some(audio) = self.queue.complete_part(
                parts.ticket,
                Ok(audio),
                seconds * parts.elapsed_per_second,
                last,
            ) {
                parts.samples.push_front(audio.pcm);
                self.parts = Some(parts);
                return;
            }
        }
    }
    /// Polls worker and output using a monotonic caller-supplied clock.
    pub fn tick(&mut self, now: f64) -> Result<(), String> {
        if !now.is_finite() || now < self.now {
            return Err("tts.errors.clock".into());
        }
        self.now = now;
        if self.queue.state == PlaybackState::Playing && self.output_ticket.is_some() {
            match self
                .playback
                .poll()
                .map_err(|_| "tts.errors.audio".to_string())?
            {
                Poll::Finished => {
                    if let Some(audio) = &self.queue.playing {
                        self.played_seconds += audio.seconds();
                    }
                    self.queue.playback_finished();
                    self.output_ticket = None;
                    self.current_elapsed = 0.0;
                }
                Poll::Playing | Poll::DeviceLost => {}
            }
        }
        if let Some(completion) = self.worker.poll() {
            self.received(completion);
        }
        self.admit_parts();
        if self.queue.in_flight.is_none()
            && self.parts.is_none()
            && matches!(
                self.queue.state,
                PlaybackState::Playing | PlaybackState::Buffering
            )
            && let Some(ticket) = self.queue.request_next()
        {
            let next = match self.pending_text.take() {
                Some(text) => Some(text),
                None => self
                    .cursor
                    .next_chunk()
                    .map_err(|_| "tts.errors.text".to_string())?,
            };
            match next {
                Some(mut text) => {
                    text.index = ticket.index;
                    let result = self.worker.request(ticket, &text.text, &self.selection);
                    self.metadata.insert(ticket.index, text);
                    if let Err(error) = result {
                        self.error = Some(error);
                        self.queue.complete(ticket, Err(()));
                    }
                }
                None => {
                    self.queue.in_flight = None;
                    self.queue.source_finished = true;
                    self.queue.advance();
                }
            }
        }
        self.queue.advance();
        if self.output_ticket.is_none()
            && self.queue.state == PlaybackState::Playing
            && let Some(audio) = &self.queue.playing
        {
            let pause = audio.pause_ms;
            self.playback
                .play(audio.ticket.index, audio.pcm.clone(), pause)
                .map_err(|_| "tts.errors.audio".to_string())?;
            self.output_ticket = Some(audio.ticket);
            self.current_started = now;
            self.first_play.get_or_insert(now);
            self.reported_position.set(None);
            self.snapshot();
        }
        let min = self
            .queue
            .playing
            .as_ref()
            .map(|c| c.ticket.index)
            .or_else(|| self.queue.waiting.front().map(|c| c.ticket.index))
            .or_else(|| self.queue.in_flight.map(|t| t.index))
            .or(self.queue.failed_index)
            .unwrap_or(self.queue.next_index);
        self.metadata.retain(|index, _| *index >= min);
        self.output_fallback |= self.playback.take_fallback();
        Ok(())
    }
    /// Returns the current source range, progress and playback state.
    pub fn snapshot(&self) -> SessionSnapshot {
        let range = self
            .queue
            .playing
            .as_ref()
            .map(|c| (c.source_start, c.source_end))
            .or_else(|| {
                self.queue.failed_index.and_then(|i| {
                    self.metadata
                        .get(&i)
                        .map(|t| (t.source_start, t.source_end))
                })
            })
            .unwrap_or((0, 0));
        let chars = self.source_utf16 as f64;
        let progress = if self.queue.state == PlaybackState::Finished {
            1.0
        } else if chars > 0.0 {
            f64::from(range.0) / chars
        } else {
            0.0
        };
        let current = if self.queue.state == PlaybackState::Playing {
            (self.now - self.current_started).max(0.0)
        } else {
            0.0
        };
        let elapsed = self.played_seconds + self.current_elapsed + current;
        let rate = if range.0 > 0 && elapsed > 0.0 {
            f64::from(range.0) / elapsed
        } else {
            15.0 * f64::from(self.selection.speed)
        };
        let mut position = (
            range.0,
            range.1,
            progress,
            if self.queue.state == PlaybackState::Finished {
                0.0
            } else {
                ((chars - f64::from(range.0)) / rate).max(0.0)
            },
        );
        if matches!(
            self.queue.state,
            PlaybackState::Buffering | PlaybackState::Paused | PlaybackState::Starting
        ) {
            position = self.reported_position.get().unwrap_or(position);
        } else {
            self.reported_position.set(Some(position));
        }
        SessionSnapshot {
            epoch: self.queue.epoch,
            state: self.queue.state,
            source_start: position.0,
            source_end: position.1,
            elapsed_seconds: elapsed,
            remaining_seconds: position.3,
            progress: position.2,
            error: if self.queue.state == PlaybackState::Error {
                self.error.clone()
            } else {
                None
            },
            output_fallback: self.output_fallback,
            queued_audio_seconds: self.queue.queued_seconds(),
            buffering_pauses: self.queue.buffering_pauses,
            head_start_seconds: self.first_play.zip(self.initial_wait).map(|(a, b)| a - b),
        }
    }
}
impl<W: Worker, O: Output> Drop for Session<'_, W, O> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::PlaybackError;
    use std::rc::Rc;
    #[derive(Default)]
    struct FakeWorker {
        now: f64,
        rtf: f64,
        pending: Option<(f64, Completion)>,
        requested: Vec<Ticket>,
        cancelled: Vec<Ticket>,
        fail_once: Option<usize>,
        seconds: f64,
        selections: usize,
    }
    impl Worker for FakeWorker {
        fn request(&mut self, ticket: Ticket, _: &str, _: &Selection) -> Result<(), String> {
            self.requested.push(ticket);
            let result = if self.fail_once == Some(ticket.index) {
                self.fail_once = None;
                Err("tts.errors.worker".into())
            } else {
                Ok(Pcm {
                    id: ticket.index as u64 + 1,
                    sample_rate: 8000,
                    samples: vec![0.0; (self.seconds * 8000.0) as usize],
                })
            };
            self.pending = Some((
                self.now + self.rtf * self.seconds,
                Completion {
                    ticket,
                    result,
                    synthesis_seconds: self.rtf * self.seconds,
                },
            ));
            Ok(())
        }
        fn poll(&mut self) -> Option<Completion> {
            if self
                .pending
                .as_ref()
                .is_some_and(|(due, _)| *due <= self.now)
            {
                self.pending.take().map(|(_, c)| c)
            } else {
                None
            }
        }
        fn cancel(&mut self, t: Ticket) {
            self.cancelled.push(t);
        }
        fn selection_changed(&mut self) {
            self.selections += 1;
        }
    }
    struct PreparedStream {
        name: Option<String>,
        live: Rc<Cell<usize>>,
    }
    impl Drop for PreparedStream {
        fn drop(&mut self) {
            self.live.set(self.live.get() - 1);
        }
    }
    #[derive(Default)]
    struct FakeOutput {
        now: f64,
        deadline: Option<f64>,
        remaining: f64,
        paused: bool,
        played: Vec<u64>,
        sample_counts: Vec<usize>,
        stops: usize,
        pauses: Vec<u32>,
        opens: usize,
        names: Vec<String>,
        last_open: Option<String>,
        fail_open: bool,
        fail_after_open: Option<usize>,
        prepared: Rc<Cell<usize>>,
        commits: Rc<Cell<usize>>,
    }
    impl Output for FakeOutput {
        type Prepared = PreparedStream;
        fn names(&self) -> Result<Vec<String>, PlaybackError> {
            Ok(self.names.clone())
        }
        fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
            let prepared = self.prepare(name)?;
            self.commit(prepared);
            Ok(())
        }
        fn prepare(&mut self, name: Option<&str>) -> Result<Self::Prepared, PlaybackError> {
            if self.fail_open
                || self
                    .fail_after_open
                    .is_some_and(|limit| self.opens >= limit)
            {
                return Err(PlaybackError::Unavailable);
            }
            self.opens += 1;
            self.prepared.set(self.prepared.get() + 1);
            Ok(PreparedStream {
                name: name.map(str::to_owned),
                live: self.prepared.clone(),
            })
        }
        fn commit(&mut self, prepared: Self::Prepared) {
            self.last_open = prepared.name.clone();
            self.deadline = None;
            self.commits.set(self.commits.get() + 1);
        }
        fn play(&mut self, pcm: Pcm, pause: u32) -> Result<(), PlaybackError> {
            self.pauses.push(pause);
            self.sample_counts.push(pcm.samples.len());
            self.deadline = Some(
                self.now
                    + pcm.samples.len() as f64 / f64::from(pcm.sample_rate)
                    + f64::from(pause) / 1000.0,
            );
            self.played.push(pcm.id);
            self.paused = false;
            Ok(())
        }
        fn poll(&mut self) -> Poll {
            if !self.paused && self.deadline.is_some_and(|d| d <= self.now) {
                self.deadline = None;
                Poll::Finished
            } else {
                Poll::Playing
            }
        }
        fn pause(&mut self) {
            self.remaining = self.deadline.map_or(0.0, |d| (d - self.now).max(0.0));
            self.paused = true;
        }
        fn resume(&mut self) {
            if self.paused {
                self.deadline = Some(self.now + self.remaining);
                self.paused = false;
            }
        }
        fn stop(&mut self) {
            self.deadline = None;
            self.stops += 1;
        }
    }
    fn selection() -> Selection {
        Selection {
            model_id: "supertonic-3".into(),
            voice: "F1".into(),
            speed: 2.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
        }
    }
    fn session(source: &str, rtf: f64) -> Session<'_, FakeWorker, FakeOutput> {
        Session::new(
            source,
            selection(),
            FakeWorker {
                rtf,
                seconds: 1.0,
                ..FakeWorker::default()
            },
            FakeOutput::default(),
            None,
        )
        .unwrap()
    }
    #[test]
    fn fix1_idle_output_failure_does_not_block_stop_and_recovers_on_play() {
        let mut s = Session::new(
            "The reading document stays available when audio returns.",
            selection(),
            FakeWorker::default(),
            FakeOutput {
                fail_open: true,
                ..FakeOutput::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(s.playback.output.opens, 0);
        s.stop();
        assert_eq!(s.play().unwrap_err(), "tts.errors.audio");
        s.stop();
        s.playback.output.fail_open = false;
        s.play().unwrap();
        assert_eq!(s.playback.output.opens, 1);
        assert_eq!(s.queue.state, PlaybackState::Buffering);
    }
    fn step(s: &mut Session<'_, FakeWorker, FakeOutput>, now: f64) {
        s.worker.now = now;
        s.playback.output.now = now;
        s.tick(now).unwrap();
        assert!(s.queue.queued_seconds() <= 30.00001);
    }
    #[test]
    fn followup_n1_code_toggle_after_retry_before_tick_retains_destination() {
        let source = format!(
            "# Start\n\n```\n{}\n```\n\nThe destination sentence must remain the active source after switching.\n\nAnother long sentence follows the destination sentence here.",
            "alpha beta gamma delta ".repeat(60)
        );
        let target = source.find("The destination").unwrap() as u32;
        let mut cursor = TextCursor::new(&source, true).unwrap();
        let mut destination = None;
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            if chunk.source_start == target {
                destination = Some(chunk.index);
                break;
            }
        }
        let destination = destination.unwrap();
        assert_eq!(destination, 6);
        let mut cursor = TextCursor::new(&source, false).unwrap();
        let mut expected = Vec::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            if chunk.source_start >= target {
                expected.push(chunk.index as u64 + 1);
            }
        }
        for pause_before_select in [false, true] {
            let mut s = session(&source, 0.01);
            s.selection.read_code = true;
            s.worker.fail_once = Some(destination);
            s.play().unwrap();
            let mut now = 0.0;
            for _ in 0..500 {
                step(&mut s, now);
                now += 0.1;
                if s.queue.state == PlaybackState::Error {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Error);
            assert_eq!(s.snapshot().source_start, target);
            let played_before = s.playback.output.played.len();
            s.play().unwrap();
            if pause_before_select {
                s.pause();
            }
            s.select(selection(), true, |_| Ok(())).unwrap();
            s.pause();
            let requested = s.worker.requested.len();
            step(&mut s, now);
            now += 0.1;
            assert_eq!(s.worker.requested.len(), requested);
            s.play().unwrap();
            let mut first_source = None;
            for _ in 0..200 {
                step(&mut s, now);
                now += 0.1;
                if let Some(audio) = &s.queue.playing {
                    first_source.get_or_insert(audio.source_start);
                }
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(first_source, Some(target));
            assert_eq!(s.queue.state, PlaybackState::Finished);
            assert_eq!(&s.playback.output.played[played_before..], expected);
        }
    }
    #[test]
    fn followup_n2_failed_persist_does_not_reopen_or_repeat_active_sentence() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(20);
        for restore_would_fail in [false, true] {
            let mut s = session(&source, 0.1);
            s.playback.output.names.push("Headphones".into());
            s.play().unwrap();
            for i in 0..12 {
                step(&mut s, f64::from(i) * 0.1);
            }
            let before = (
                s.playback.output.opens,
                s.playback.output.stops,
                s.queue.epoch,
            );
            let old_selection = s.selection.clone();
            let old_deadline = s.playback.output.deadline;
            let old_ticket = s.queue.playing.as_ref().unwrap().ticket;
            let old_pending = s.queue.in_flight;
            let old_waiting: Vec<_> = s.queue.waiting.iter().map(|c| c.ticket).collect();
            let prepared = s.playback.output.prepared.clone();
            let commits = s.playback.output.commits.clone();
            let old_commits = commits.get();
            if restore_would_fail {
                s.playback.output.fail_after_open = Some(before.0 + 1);
            }
            let mut chosen = selection();
            chosen.output_device = Some("Headphones".into());
            assert_eq!(
                s.select(chosen, true, |_| {
                    assert_eq!(prepared.get(), 1);
                    assert_eq!(commits.get(), old_commits);
                    Err(SelectionError::Persist)
                }),
                Err(SelectionError::Persist)
            );
            assert_eq!(prepared.get(), 0);
            assert_eq!(commits.get(), old_commits);
            assert_eq!(s.playback.output.last_open, None);
            assert_eq!(s.playback.output.deadline, old_deadline);
            assert_eq!(s.selection, old_selection);
            assert_eq!(s.queue.playing.as_ref().unwrap().ticket, old_ticket);
            assert_eq!(s.queue.in_flight, old_pending);
            assert_eq!(
                s.queue.waiting.iter().map(|c| c.ticket).collect::<Vec<_>>(),
                old_waiting
            );
            assert!(s.worker.cancelled.is_empty());
            assert_eq!(s.worker.selections, 0);
            step(&mut s, 1.2);
            assert_eq!(s.playback.output.stops, 0);
            assert!(s.playback.output.opens <= before.0 + 1);
            assert_eq!(s.queue.epoch, before.2);
            assert_eq!(s.selection.output_device, None);
            assert_eq!(
                s.playback
                    .output
                    .played
                    .iter()
                    .filter(|id| **id == 1)
                    .count(),
                1
            );
        }
    }
    #[test]
    fn followup_output_change_commits_only_after_persistence() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(20);
        let mut s = session(&source, 0.1);
        s.playback.output.names.push("Headphones".into());
        s.play().unwrap();
        for i in 0..12 {
            step(&mut s, f64::from(i) * 0.1);
        }
        let prepared = s.playback.output.prepared.clone();
        let commits = s.playback.output.commits.clone();
        let before = (s.queue.epoch, s.playback.output.opens, commits.get());
        let mut chosen = selection();
        chosen.output_device = Some("Headphones".into());
        let changed = s
            .select(chosen.clone(), true, |value| {
                assert_eq!(value, &chosen);
                assert_eq!(prepared.get(), 1);
                assert_eq!(commits.get(), before.2);
                Ok(())
            })
            .unwrap();
        assert_eq!(prepared.get(), 0);
        assert_eq!(commits.get(), before.2 + 1);
        assert_eq!(s.playback.output.opens, before.1 + 1);
        assert_eq!(s.playback.output.last_open.as_deref(), Some("Headphones"));
        assert_eq!(s.playback.output.stops, 1);
        assert_eq!(changed.epoch, before.0 + 1);
        assert_eq!(changed.sentence_index, 0);
        assert_eq!(s.selection, chosen);
        for i in 12..17 {
            step(&mut s, f64::from(i) * 0.1);
        }
        assert_eq!(s.playback.output.played, [1, 1]);
    }
    #[test]
    fn fix2_reopen_or_seek_error_preserves_the_entire_old_session() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(20);
        for seek_error in [false, true] {
            let mut s = session(&source, 0.1);
            s.play().unwrap();
            for i in 0..5 {
                step(&mut s, f64::from(i) * 0.1);
            }
            let old_selection = s.selection.clone();
            let epoch = s.queue.epoch;
            let ticket = s.queue.playing.as_ref().unwrap().ticket;
            let waiting: Vec<_> = s.queue.waiting.iter().map(|c| c.ticket).collect();
            let in_flight = s.queue.in_flight;
            let old_output = (
                s.playback.output.deadline,
                s.playback.output.paused,
                s.playback.output.stops,
            );
            if seek_error {
                s.cursor_factory = |_, _| Err("tts.errors.text".into());
            } else {
                s.playback.output.fail_open = true;
            }
            let mut chosen = selection();
            chosen.voice = "M1".into();
            chosen.output_device = Some("Default".into());
            assert_eq!(
                s.select(chosen, true, |_| panic!(
                    "failed preparation must not persist"
                )),
                Err(SelectionError::Controls)
            );
            assert_eq!(s.selection, old_selection);
            assert_eq!(s.queue.epoch, epoch);
            assert_eq!(s.queue.playing.as_ref().unwrap().ticket, ticket);
            assert_eq!(
                s.queue.waiting.iter().map(|c| c.ticket).collect::<Vec<_>>(),
                waiting
            );
            assert_eq!(s.queue.in_flight, in_flight);
            assert_eq!(
                (
                    s.playback.output.deadline,
                    s.playback.output.paused,
                    s.playback.output.stops
                ),
                old_output
            );
            assert!(s.worker.cancelled.is_empty());
            assert_eq!(s.worker.selections, 0);
        }
    }
    #[test]
    fn fix2_dsp_time_uses_the_injected_clock_for_pocket_audio() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut s = session("A sufficiently long sentence for an audio request.", 0.1);
        s.selection.model_id = "pocket-en".into();
        s.selection.voice = "alba".into();
        s.selection.speed = 2.0;
        let calls = Arc::new(AtomicUsize::new(0));
        let clock = calls.clone();
        s.dsp_clock = Box::new(move || clock.fetch_add(1, Ordering::SeqCst) as f64 * 0.25);
        s.play().unwrap();
        step(&mut s, 0.0);
        step(&mut s, 0.2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!((s.queue.measured_rtf - 0.7).abs() < 1e-12);
    }
    #[test]
    fn fix2_paused_selection_plays_all_waiting_sentences_before_failure() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(12);
        let mut s = session(&source, 1.2);
        s.worker.fail_once = Some(2);
        s.play().unwrap();
        let mut now = 0.0;
        for _ in 0..200 {
            step(&mut s, now);
            now += 0.1;
            if s.queue.in_flight.is_some_and(|t| t.index == 2) {
                break;
            }
        }
        assert_eq!(s.queue.state, PlaybackState::Buffering);
        assert_eq!(s.queue.waiting.len(), 2);
        s.pause();
        for _ in 0..20 {
            step(&mut s, now);
            now += 0.1;
        }
        assert_eq!(s.queue.failed_index, Some(2));
        let chosen = Selection {
            voice: "M1".into(),
            ..selection()
        };
        assert_eq!(
            s.select(chosen, true, |_| Ok(())).unwrap().sentence_index,
            0
        );
        s.play().unwrap();
        for _ in 0..400 {
            step(&mut s, now);
            now += 0.1;
        }
        assert_eq!(s.playback.output.played, (1..=12).collect::<Vec<_>>());
    }
    #[test]
    fn fix2_buffering_and_paused_snapshots_retain_reported_position() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(40);
        let mut s = session(&source, 1.6);
        s.play().unwrap();
        let mut last = s.snapshot();
        let mut now = 0.0;
        for _ in 0..1000 {
            step(&mut s, now);
            now += 0.1;
            let snap = s.snapshot();
            if snap.state == PlaybackState::Buffering && snap.buffering_pauses > 0 {
                assert!(last.source_start > 0);
                assert_eq!(
                    (
                        snap.source_start,
                        snap.source_end,
                        snap.progress,
                        snap.remaining_seconds
                    ),
                    (
                        last.source_start,
                        last.source_end,
                        last.progress,
                        last.remaining_seconds
                    )
                );
                s.pause();
                step(&mut s, now + 1.0);
                let paused = s.snapshot();
                assert_eq!(
                    (
                        paused.source_start,
                        paused.source_end,
                        paused.progress,
                        paused.remaining_seconds
                    ),
                    (
                        last.source_start,
                        last.source_end,
                        last.progress,
                        last.remaining_seconds
                    )
                );
                s.select(
                    Selection {
                        voice: "M1".into(),
                        ..selection()
                    },
                    true,
                    |_| Ok(()),
                )
                .unwrap();
                let selected = s.snapshot();
                assert_eq!(
                    (
                        selected.source_start,
                        selected.source_end,
                        selected.progress,
                        selected.remaining_seconds
                    ),
                    (
                        last.source_start,
                        last.source_end,
                        last.progress,
                        last.remaining_seconds
                    )
                );
                s.stop();
                assert_eq!(s.snapshot().source_start, 0);
                return;
            }
            last = snap;
        }
        panic!("no underrun observed");
    }
    #[test]
    fn fix2_error_is_exposed_only_after_earlier_audio_drains() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(20);
        let mut s = session(&source, 0.1);
        s.worker.fail_once = Some(5);
        s.play().unwrap();
        let mut draining = false;
        for i in 0..300 {
            step(&mut s, f64::from(i) * 0.1);
            if s.queue.failed_index.is_some() && s.queue.state != PlaybackState::Error {
                draining = true;
                assert!(s.snapshot().error.is_none());
            }
            if s.queue.state == PlaybackState::Error {
                assert!(draining);
                assert_eq!(s.snapshot().error.as_deref(), Some("tts.errors.worker"));
                return;
            }
        }
        panic!("failure did not drain");
    }
    #[test]
    fn fix2_missing_part_metadata_ends_the_pending_request() {
        let mut s = session("A sufficiently long sentence for an audio request.", 0.1);
        s.play().unwrap();
        step(&mut s, 0.0);
        s.metadata.clear();
        step(&mut s, 0.2);
        assert!(s.queue.in_flight.is_none());
        assert_eq!(s.queue.state, PlaybackState::Error);
        assert!(s.snapshot().error.is_some());
    }
    #[test]
    fn fix2_read_code_toggle_preserves_original_position_in_both_directions() {
        let source = format!(
            "# Start\n\n```\n{}\n```\n\nThe destination sentence must remain the active source after switching.\n\nAnother long sentence follows the destination sentence here.",
            "alpha beta gamma delta ".repeat(60)
        );
        let target = source.find("The destination").unwrap() as u32;
        for initial in [false, true] {
            let mut s = session(&source, 0.01);
            s.selection.read_code = initial;
            s.play().unwrap();
            let mut now = 0.0;
            for _ in 0..500 {
                step(&mut s, now);
                now += 0.1;
                if s.snapshot().source_start == target {
                    break;
                }
            }
            assert_eq!(s.snapshot().source_start, target);
            let mut chosen = s.selection.clone();
            chosen.read_code = !initial;
            s.select(chosen, true, |_| Ok(())).unwrap();
            for _ in 0..20 {
                step(&mut s, now);
                now += 0.1;
                if s.queue.playing.is_some() {
                    break;
                }
            }
            assert_eq!(s.snapshot().source_start, target);
        }
    }
    #[test]
    fn repeated_play_and_idle_pause_do_not_stall_output() {
        let source = "A sentence long enough to begin playback without delay.\n\n".repeat(10);
        let mut s = session(&source, 0.1);
        s.pause();
        s.play().unwrap();
        for i in 0..5 {
            step(&mut s, f64::from(i) * 0.1);
        }
        assert!(!s.playback.output.paused);
        assert_eq!(s.queue.state, PlaybackState::Playing);
        s.play().unwrap();
        assert_eq!(s.queue.state, PlaybackState::Playing);
        for i in 5..200 {
            step(&mut s, f64::from(i) * 0.1);
        }
        assert_eq!(s.queue.state, PlaybackState::Finished);
        assert_eq!(s.snapshot().remaining_seconds, 0.0);
        s.play().unwrap();
        assert_eq!(s.snapshot().elapsed_seconds, 0.0);
    }

    #[test]
    fn external_failure_cancels_pending_audio_and_play_retries_current_sentence() {
        let source = "A sentence long enough to begin playback without delay.\n\n".repeat(10);
        let mut s = session(&source, 0.1);
        s.play().unwrap();
        for i in 0..20 {
            step(&mut s, f64::from(i) * 0.1);
        }
        let current = s.queue.playing.as_ref().unwrap().ticket;
        let old = s.queue.in_flight;
        s.fail("tts.errors.audio".into());
        assert_eq!(s.queue.failed_index, Some(current.index));
        assert!(s.queue.playing.is_none());
        assert!(s.parts.is_none());
        assert!(s.output_ticket.is_none());
        if let Some(ticket) = old {
            assert!(s.worker.cancelled.contains(&ticket));
        }
        s.play().unwrap();
        step(&mut s, 2.0);
        assert_eq!(s.worker.requested.last().unwrap().index, current.index);
        assert_ne!(s.worker.requested.last().unwrap().epoch, current.epoch);
        for i in 21..40 {
            step(&mut s, f64::from(i) * 0.1);
        }
        assert!(
            s.playback
                .output
                .played
                .iter()
                .filter(|id| **id == current.index as u64 + 1)
                .count()
                >= 2
        );
    }
    #[test]
    fn npu_over_cap_chunk_preserves_all_words_and_audio_through_fake_worker() {
        use crate::device::Device;
        struct NpuWorker {
            pending: Option<Completion>,
            calls: Vec<String>,
            requests: Vec<String>,
            model: &'static str,
        }
        impl Worker for NpuWorker {
            fn request(&mut self, ticket: Ticket, text: &str, _: &Selection) -> Result<(), String> {
                self.requests.push(text.to_owned());
                let model = self.model;
                let fits = |part: &str| match model {
                    "pocket-en" => part.chars().count() <= 64,
                    _ => part.split_whitespace().count() * 80 <= 512,
                };
                let result = synthesize_for_device(
                    Device::Npu,
                    text,
                    |part| Ok(fits(part)),
                    |part| {
                        if !fits(part) {
                            return Err("static shape exceeded".into());
                        }
                        self.calls.push(part.to_owned());
                        Ok(Pcm {
                            id: ticket.index as u64,
                            sample_rate: 8000,
                            samples: part
                                .split_whitespace()
                                .map(|word| word.len() as f32)
                                .collect(),
                        })
                    },
                );
                self.pending = Some(Completion {
                    ticket,
                    result,
                    synthesis_seconds: 0.0,
                });
                Ok(())
            }
            fn poll(&mut self) -> Option<Completion> {
                self.pending.take()
            }
            fn cancel(&mut self, _: Ticket) {
                self.pending = None;
            }
            fn selection_changed(&mut self) {}
        }
        let source = format!("# A starting heading.\n\n{}", "tiny ".repeat(55));
        for (model, voice) in [("pocket-en", "alba"), ("supertonic-3", "F1")] {
            let worker = NpuWorker {
                pending: None,
                calls: vec![],
                requests: vec![],
                model,
            };
            let chosen = Selection {
                model_id: model.into(),
                voice: voice.into(),
                speed: 1.0,
                ..selection()
            };
            let mut s = Session::new(&source, chosen, worker, FakeOutput::default(), None).unwrap();
            s.play().unwrap();
            for i in 0..100 {
                let now = f64::from(i) * 0.1;
                s.playback.output.now = now;
                s.tick(now).unwrap();
                assert!(s.snapshot().error.is_none());
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
            assert!(s.worker.requests.iter().any(|text| text.len() > 200));
            assert!(s.worker.calls.len() > s.worker.requests.len());
            let requested_words: Vec<_> = s
                .worker
                .requests
                .iter()
                .flat_map(|text| text.split_whitespace())
                .collect();
            let synthesized_words: Vec<_> = s
                .worker
                .calls
                .iter()
                .flat_map(|text| text.split_whitespace())
                .collect();
            assert_eq!(synthesized_words, requested_words);
            assert_eq!(
                s.playback.output.sample_counts.iter().sum::<usize>(),
                requested_words.len()
            );
        }
        let mut called = Vec::new();
        let pcm = synthesize_for_device(
            Device::Cpu,
            "an unchanged CPU request",
            |_| panic!("CPU has no static cap"),
            |part| {
                called.push(part.to_owned());
                Ok(Pcm {
                    id: 0,
                    sample_rate: 8000,
                    samples: vec![0.5],
                })
            },
        )
        .unwrap();
        assert_eq!(called, ["an unchanged CPU request"]);
        assert_eq!(pcm.samples, [0.5]);
    }
    #[test]
    fn selection_in_error_keeps_failed_sentence_until_play_retries_new_voice_or_model() {
        let source =
            "Every sentence has enough characters to form its own speech chunk. ".repeat(20);
        for model_change in [false, true] {
            let mut s = session(&source, 0.1);
            s.worker.fail_once = Some(3);
            s.play().unwrap();
            let mut now = 0.0;
            for _ in 0..200 {
                step(&mut s, now);
                now += 0.1;
                if s.queue.state == PlaybackState::Error {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Error);
            assert_eq!(s.queue.failed_index, Some(3));
            let range = (s.snapshot().source_start, s.snapshot().source_end);
            let mut chosen = selection();
            if model_change {
                chosen.model_id = "pocket-en".into();
                chosen.voice = "alba".into();
            } else {
                chosen.voice = "M1".into();
            }
            let changed = s.select(chosen, true, |_| Ok(())).unwrap();
            assert_eq!(changed.sentence_index, 3);
            assert!(!changed.resume);
            assert_eq!(s.queue.state, PlaybackState::Error);
            assert_eq!(s.queue.failed_index, Some(3));
            assert_eq!((s.snapshot().source_start, s.snapshot().source_end), range);
            let requests = s.worker.requested.len();
            step(&mut s, now);
            assert_eq!(s.worker.requested.len(), requests);
            s.play().unwrap();
            step(&mut s, now + 0.1);
            assert_eq!(s.worker.requested.last().unwrap().index, 3);
            assert_eq!(s.worker.selections, 1);
        }
    }
    #[test]
    fn returning_to_same_as_handy_retains_the_inherited_device_without_worker_reload() {
        let output = FakeOutput {
            names: vec!["Handy headphones".into(), "Alternate".into()],
            ..FakeOutput::default()
        };
        let mut chosen = selection();
        chosen.output_device = Some("Alternate".into());
        let mut s = Session::new(
            "A full sentence to read.",
            chosen,
            FakeWorker::default(),
            output,
            Some("Handy headphones"),
        )
        .unwrap();
        assert_eq!(s.playback.output.last_open, None);
        s.play().unwrap();
        assert_eq!(s.playback.output.last_open.as_deref(), Some("Alternate"));
        s.select(selection(), true, |_| Ok(())).unwrap();
        assert_eq!(
            s.playback.output.last_open.as_deref(),
            Some("Handy headphones")
        );
        assert_eq!(s.worker.selections, 0);
    }
    #[test]
    fn split_audio_pauses_once_and_output_change_reopens_without_worker_reload() {
        let mut s = session("# A heading which has a structural pause after it.", 0.01);
        s.worker.seconds = 65.0;
        s.play().unwrap();
        for step in 0..2000 {
            let now = f64::from(step) * 0.05;
            s.worker.now = now;
            s.playback.output.now = now;
            s.tick(now).unwrap();
            if s.queue.state == PlaybackState::Finished {
                break;
            }
        }
        let pauses = &s.playback.output.pauses;
        assert!(pauses.len() >= 3);
        assert_eq!(pauses.iter().filter(|pause| **pause > 0).count(), 1);
        let mut chosen = selection();
        chosen.output_device = Some("gone headset".into());
        let opens = s.playback.output.opens;
        s.select(chosen, true, |_| Ok(())).unwrap();
        assert_eq!(s.playback.output.opens, opens + 1);
        assert_eq!(s.worker.selections, 0);
    }
    #[test]
    fn controlled_fast_borderline_and_slow_workers_drive_buffering_and_adaptive_head_start() {
        let source =
            "Reading one measured sentence at a time makes this test deterministic. ".repeat(2000);
        for rtf in [0.1, 0.49, 1.2] {
            let mut s = session(&source, rtf);
            s.play().unwrap();
            for i in 0..1200 {
                step(&mut s, i as f64 * 0.1);
            }
            let snap = s.snapshot();
            assert!(snap.head_start_seconds.unwrap() > 0.0);
            if rtf < 1.0 {
                assert_eq!(snap.buffering_pauses, 0);
            } else {
                assert!(snap.buffering_pauses > 0);
            }
            s.pause();
            let count = s.worker.requested.len();
            for i in 1200..1300 {
                step(&mut s, i as f64 * 0.1);
            }
            assert_eq!(s.worker.requested.len(), count);
            assert_eq!(s.queue.state, PlaybackState::Paused);
            s.stop();
            assert!(s.parts.is_none());
            assert!(s.queue.waiting.is_empty());
            assert!(s.queue.playing.is_none());
        }
    }
    #[test]
    fn twenty_minute_document_crash_resumes_failed_sentence_without_skipping_preceding_audio() {
        let source =
            "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(1500);
        let mut cursor = TextCursor::new(&source, false).unwrap();
        let mut chunks = Vec::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        let crash = chunks.len() / 2;
        assert!(crash > 600);
        let mut s = session(&source, 0.1);
        s.worker.fail_once = Some(crash);
        s.play().unwrap();
        let mut now = 0.0;
        let mut draining = false;
        for _ in 0..20000 {
            step(&mut s, now);
            now += 0.1;
            if s.queue.failed_index.is_some()
                && let Some(playing) = &s.queue.playing
            {
                draining = true;
                assert!(!s.queue.waiting.is_empty() || playing.ticket.index == crash - 1);
                assert_eq!(s.worker.requested.last().unwrap().index, crash);
            }
            if s.queue.state == PlaybackState::Error {
                break;
            }
        }
        assert!(draining);
        assert_eq!(s.queue.state, PlaybackState::Error);
        assert_eq!(s.queue.failed_index, Some(crash));
        assert_eq!(s.snapshot().source_start, chunks[crash].source_start);
        assert_eq!(s.snapshot().source_end, chunks[crash].source_end);
        assert!(s.snapshot().source_end > s.snapshot().source_start);
        assert_eq!(
            s.playback.output.played,
            (1..=crash as u64).collect::<Vec<_>>()
        );
        s.play().unwrap();
        for _ in 0..20000 {
            step(&mut s, now);
            now += 0.1;
            if s.queue.state == PlaybackState::Finished {
                break;
            }
        }
        assert_eq!(s.queue.state, PlaybackState::Finished);
        assert_eq!(
            s.worker
                .requested
                .iter()
                .filter(|t| t.index == crash)
                .count(),
            2
        );
        assert_eq!(
            s.playback.output.played,
            (1..=chunks.len() as u64).collect::<Vec<_>>()
        );
        let mut expected_requests: Vec<_> = (0..chunks.len()).collect();
        expected_requests.insert(crash, crash);
        assert_eq!(
            s.worker
                .requested
                .iter()
                .map(|t| t.index)
                .collect::<Vec<_>>(),
            expected_requests
        );
    }
    #[test]
    fn selection_and_missing_model_preserve_cursor_and_old_epoch_completion_is_dropped() {
        let source = "The first sentence has enough words and length for playback. The second sentence is also worth reading aloud.";
        let mut s = session(source, 0.1);
        s.play().unwrap();
        for i in 0..5 {
            step(&mut s, i as f64 * 0.1);
        }
        let current = s.queue.playing.as_ref().unwrap().ticket.index;
        let old = s.queue.in_flight;
        let mut chosen = selection();
        chosen.voice = "M1".into();
        let changed = s.select(chosen.clone(), true, |_| Ok(())).unwrap();
        assert_eq!(changed.sentence_index, current);
        if let Some(ticket) = old {
            s.received(Completion {
                ticket,
                result: Ok(Pcm {
                    id: 99,
                    sample_rate: 8000,
                    samples: vec![0.0; 8000],
                }),
                synthesis_seconds: 0.1,
            });
        }
        assert!(s.queue.waiting.is_empty());
        assert!(s.parts.is_none());
        step(&mut s, 0.6);
        assert_eq!(s.worker.requested.last().unwrap().index, current);
        let changed = s.select(chosen, false, |_| Ok(())).unwrap();
        assert_eq!(changed.sentence_index, current);
        assert_eq!(s.queue.state, PlaybackState::Error);
        assert_eq!(
            s.snapshot().error.as_deref(),
            Some("tts.errors.notDownloaded")
        );
    }
    #[test]
    fn oversized_result_is_split_into_bounded_pcm_parts_and_stop_discards_late_reply() {
        let source =
            "This single sentence produces an unusually long audio result in the fake worker.";
        let mut s = session(source, 0.001);
        s.worker.seconds = 65.0;
        s.play().unwrap();
        step(&mut s, 0.0);
        step(&mut s, 0.1);
        assert!(s.queue.playing.as_ref().unwrap().seconds() <= 30.0);
        assert!(s.parts.is_some());
        assert!(s.queue.in_flight.is_some());
        let old = s.queue.in_flight.unwrap();
        s.stop();
        s.received(Completion {
            ticket: old,
            result: Ok(Pcm {
                id: 1,
                sample_rate: 8000,
                samples: vec![0.0; 8000],
            }),
            synthesis_seconds: 0.1,
        });
        assert!(s.parts.is_none());
        assert!(s.queue.waiting.is_empty());
        assert_eq!(s.queue.state, PlaybackState::Waiting);
    }
    #[test]
    fn starting_without_audio_keeps_last_reported_position() {
        let source = "A sentence with sufficient characters for the opening chunk.\n\n".repeat(20);
        let mut s = session(&source, 0.1);
        s.play().unwrap();
        for i in 0..30 {
            step(&mut s, i as f64 * 0.1);
        }
        let prior = s.snapshot();
        assert!(prior.progress > 0.0);
        s.queue.playing = None;
        s.queue.failed_index = None;
        s.queue.state = PlaybackState::Starting;
        let now = s.snapshot();
        assert_eq!(now.source_start, prior.source_start);
        assert_eq!(now.source_end, prior.source_end);
        assert_eq!(now.progress, prior.progress);
        assert_eq!(now.remaining_seconds, prior.remaining_seconds);
    }
}
