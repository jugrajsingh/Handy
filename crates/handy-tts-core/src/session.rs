use crate::{
    dsp,
    feed::Segment,
    playback::segment::{Output, OutputEvent, Playback},
    protocol::Pcm,
    scheduler::{AudioChunk, PlaybackState, QUEUED_AUDIO_CEILING, Queue, Ticket},
    selection::{Selection, SelectionChange, SelectionError, select_with},
    text::{TextChunk, TextCursor},
};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
    /// Returns false when admission must wait for worker capacity.
    fn try_request(
        &mut self,
        ticket: Ticket,
        text: &str,
        selection: &Selection,
    ) -> Result<bool, String> {
        self.request(ticket, text, selection).map(|()| true)
    }
    /// Cancels requests no longer retained by the scheduler.
    fn retain_requests(&mut self, _tickets: &[Ticket]) {}
    fn poll(&mut self) -> Option<Completion>;
    /// Supplies an optional request limit before each new request, including memory adapt-down.
    fn available_lanes(&mut self) -> Option<usize> {
        None
    }

    fn cancel(&mut self, ticket: Ticket);
    fn selection_changed(&mut self);
    /// Clears a failed worker latch after an explicit selection.
    fn retry_failed_selection(&mut self) {}
    /// Installs an optional actor notification for transports that complete off-thread.
    fn set_completion_waker(&mut self, _wake: std::sync::Arc<dyn Fn() + Send + Sync>) {}
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
fn split_samples(
    pcm: Pcm,
    native: Vec<f32>,
    plan: crate::speed::SpeedPlan,
    pause_ms: u32,
) -> VecDeque<(Pcm, Vec<f32>)> {
    let pause_seconds = crate::edges::pause_samples(pause_ms, plan.requested, pcm.sample_rate)
        as f64
        / f64::from(pcm.sample_rate);
    let part_seconds = (QUEUED_AUDIO_CEILING - pause_seconds)
        .min(crate::scheduler::STORED_SECONDS_CEILING / (1.0 + f64::from(plan.stretch)));
    let limit = (part_seconds * f64::from(pcm.sample_rate)).floor() as usize;
    let output_len = pcm.samples.len();
    let mut data = pcm.samples.into_iter();
    let mut native_data = native.into_iter();
    let native_len = native_data.len();
    let mut consumed = 0;
    let mut native_consumed = 0;
    let mut samples = VecDeque::new();
    loop {
        let next: Vec<f32> = data.by_ref().take(limit.max(1)).collect();
        if next.is_empty() {
            break;
        }
        consumed += next.len();
        let native_end = (consumed as f64 * native_len as f64 / output_len as f64).round() as usize;
        let native_part = native_data
            .by_ref()
            .take(native_end - native_consumed)
            .collect();
        native_consumed = native_end;
        samples.push_back((
            Pcm {
                id: pcm.id,
                sample_rate: pcm.sample_rate,
                samples: next,
            },
            native_part,
        ));
    }
    samples
}
struct Parts {
    ticket: Ticket,
    samples: VecDeque<(Pcm, Vec<f32>)>,
    plan: crate::speed::SpeedPlan,
    elapsed_per_second: f64,
}
struct Appended {
    sentence: usize,
    segment: usize,
    samples: usize,
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
    native_window: fn(&str) -> Result<crate::speed::NativeWindow, String>,
    metadata: BTreeMap<usize, TextChunk>,
    parts: BTreeMap<usize, Parts>,
    appended: VecDeque<Appended>,
    retracted_segments: VecDeque<(usize, usize)>,
    seen_sentences: BTreeSet<usize>,
    next_part_index: usize,
    audible_range: Option<(u32, u32)>,
    started_sentence: Option<usize>,
    request_plans: BTreeMap<usize, crate::speed::SpeedPlan>,
    restart_from: Option<usize>,
    sample_rate: u32,
    content_origin: u64,
    elapsed_offset: f64,
    completed_chars: u32,
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
            native_window: |id| {
                crate::catalog::entry(id)
                    .map(|entry| entry.native_speed)
                    .map_err(|error| error.to_string())
            },
            metadata: BTreeMap::new(),
            parts: BTreeMap::new(),
            appended: VecDeque::new(),
            retracted_segments: VecDeque::new(),
            seen_sentences: BTreeSet::new(),
            next_part_index: 0,
            audible_range: None,
            started_sentence: None,
            request_plans: BTreeMap::new(),
            restart_from: None,
            sample_rate: 0,
            content_origin: 0,
            elapsed_offset: 0.0,
            completed_chars: 0,
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
        self.parts.clear();
        self.appended.clear();
        self.retracted_segments.clear();
        self.seen_sentences.clear();
        self.next_part_index = 0;
        self.request_plans.clear();
        self.restart_from = None;
        self.started_sentence = None;
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
        } else if !matches!(old, PlaybackState::Playing | PlaybackState::Buffering) {
            self.elapsed_offset = if old == PlaybackState::Error {
                self.snapshot().elapsed_seconds
            } else {
                0.0
            };
            if matches!(old, PlaybackState::Error | PlaybackState::Finished) {
                self.playback.stop();
            }
            self.content_origin = self.playback.content_samples();
            self.seek(self.queue.next_index)?;
            if old != PlaybackState::Error {
                self.reported_position.set(None);
                self.played_seconds = 0.0;
                self.content_origin = self.playback.content_samples();
                self.completed_chars = 0;
                self.audible_range = None;
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
        self.queue.pause();
        self.playback.pause();
    }
    /// Drops audio and invalidates pending requests.
    pub fn stop(&mut self) {
        for ticket in self.queue.in_flight.values().copied() {
            self.worker.cancel(ticket);
        }
        self.playback.stop();
        self.queue.stop();
        self.metadata.clear();
        self.parts.clear();
        self.appended.clear();
        self.retracted_segments.clear();
        self.seen_sentences.clear();
        self.next_part_index = 0;
        self.request_plans.clear();
        self.restart_from = None;
        self.error = None;
        self.pending_text = None;
        self.reported_position.set(None);
        self.played_seconds = 0.0;
        self.content_origin = self.playback.content_samples();
        self.first_play = None;
        self.audible_range = None;
        self.started_sentence = None;
        self.elapsed_offset = 0.0;
        self.completed_chars = 0;
    }
    /// Stops at the active sentence and rejects stale audio until retry.
    pub fn fail(&mut self, error: String) {
        for ticket in self.queue.in_flight.values().copied() {
            self.worker.cancel(ticket);
        }
        self.elapsed_offset = self.snapshot().elapsed_seconds;
        self.playback.stop();
        let index = self.queue.switch_selection(true);
        self.queue.failed_index = Some(index);
        self.queue.state = PlaybackState::Error;
        self.parts.clear();
        self.appended.clear();
        self.retracted_segments.clear();
        self.seen_sentences.clear();
        self.next_part_index = 0;
        self.request_plans.clear();
        self.restart_from = None;
        self.content_origin = self.playback.content_samples();
        self.error = Some(error);
    }
    /// Applies a selection and replays the current sentence when active.
    pub fn select(
        &mut self,
        selection: Selection,
        downloaded: bool,
        persist: impl FnOnce(&Selection) -> Result<(), SelectionError>,
    ) -> Result<SelectionChange, SelectionError> {
        if downloaded && crate::selection::speed_pitch_only(&self.selection, &selection) {
            let changed = select_with(
                &mut self.queue,
                &self.selection,
                selection.clone(),
                downloaded,
                persist,
            )?;
            self.set_speed_pitch(selection.speed, selection.pitch)
                .map_err(|_| SelectionError::Controls)?;
            return Ok(changed);
        }
        let index = self.queue.next_unspoken();
        let position = (self.selection.read_code != selection.read_code)
            .then(|| self.metadata.get(&index).map(|text| text.source_start))
            .flatten();
        let (cursor, pending) = self
            .prepare_cursor(selection.read_code, index, position)
            .map_err(|_| SelectionError::Controls)?;
        let new_index = pending.as_ref().map_or(index, |text| text.index);
        let old_tickets = self.queue.in_flight.clone();
        let reload = self.selection.model_id != selection.model_id
            || self.selection.voice != selection.voice;
        let output_changed = self.selection.output_device != selection.output_device;
        let elapsed = self.snapshot().elapsed_seconds;
        let mut changed = select_with(
            &mut self.queue,
            &self.selection,
            selection.clone(),
            downloaded,
            |value| {
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
                self.playback.stop();
                if let Some(prepared) = prepared {
                    self.playback.commit(prepared);
                }
                Ok(())
            },
        )?;
        self.elapsed_offset = elapsed;
        for ticket in old_tickets.values().copied() {
            self.worker.cancel(ticket);
        }
        if reload {
            self.worker.selection_changed();
        } else {
            self.worker.retry_failed_selection();
        }
        self.selection = selection;
        self.queue.speed = self.selection.speed;
        self.queue.set_cursor(new_index);
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
        self.parts.clear();
        self.appended.clear();
        self.retracted_segments.clear();
        self.seen_sentences.clear();
        self.next_part_index = 0;
        self.request_plans.clear();
        self.restart_from = None;
        self.content_origin = self.playback.content_samples();
        self.started_sentence = None;
        self.error = changed
            .missing_download
            .then(|| "tts.errors.notDownloaded".into());
        Ok(changed)
    }
    fn speed_plan(&self) -> Result<crate::speed::SpeedPlan, String> {
        let window = (self.native_window)(&self.selection.model_id)?;
        crate::speed::plan(self.selection.speed, window).map_err(|_| "tts.errors.audio".into())
    }
    fn received(&mut self, completion: Completion) {
        if completion.ticket.epoch != self.queue.epoch
            || self.queue.in_flight.get(&completion.ticket.index) != Some(&completion.ticket)
        {
            return;
        }
        if self
            .restart_from
            .is_some_and(|index| completion.ticket.index >= index)
        {
            self.queue.in_flight.remove(&completion.ticket.index);
            self.request_plans.remove(&completion.ticket.index);
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
        let plan = match self
            .request_plans
            .get(&completion.ticket.index)
            .copied()
            .and_then(|old| {
                crate::speed::restretch(old.native, self.selection.speed).map(|stretch| {
                    crate::speed::SpeedPlan {
                        requested: self.selection.speed,
                        native: old.native,
                        stretch,
                    }
                })
            })
            .ok_or_else(|| "tts.errors.audio".to_string())
        {
            Ok(plan) => plan,
            Err(control_error) => {
                self.error = Some(control_error);
                self.queue.complete(completion.ticket, Err(()));
                return;
            }
        };
        let began = (self.dsp_clock)();
        let native = crate::edges::trim(pcm.samples, pcm.sample_rate).samples;
        let pcm = match dsp::stretch(
            Pcm {
                samples: native.clone(),
                ..pcm
            },
            plan.stretch,
            self.selection.pitch,
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
        let pause_ms = self
            .metadata
            .get(&completion.ticket.index)
            .map_or(0, |text| text.pause_ms);
        let samples = split_samples(pcm, native, plan, pause_ms);
        self.parts.insert(
            completion.ticket.index,
            Parts {
                ticket: completion.ticket,
                samples,
                plan,
                elapsed_per_second: elapsed / seconds,
            },
        );
    }
    fn admit_parts(&mut self) {
        let indices: Vec<_> = self.parts.keys().copied().collect();
        for index in indices {
            let Some(mut parts) = self.parts.remove(&index) else {
                continue;
            };
            if self.queue.in_flight.get(&index) != Some(&parts.ticket) {
                continue;
            }
            while let Some((pcm, native)) = parts.samples.pop_front() {
                let seconds = pcm.samples.len() as f64 / f64::from(pcm.sample_rate);
                let Some(text) = self.metadata.get(&index) else {
                    self.error = Some("tts.errors.text".into());
                    self.queue.complete(parts.ticket, Err(()));
                    break;
                };
                let last = parts.samples.is_empty();
                let audio = AudioChunk {
                    native,
                    plan: parts.plan,
                    pause_ms: if last { text.pause_ms } else { 0 },
                    ticket: parts.ticket,
                    pcm,
                    source_start: text.source_start,
                    source_end: text.source_end,
                };
                let remainder = self.queue.complete_part(
                    parts.ticket,
                    Ok(audio),
                    seconds * parts.elapsed_per_second,
                    last,
                );
                if let Some(audio) = remainder {
                    parts.samples.push_front((audio.pcm, audio.native));
                    self.parts.insert(index, parts);
                    break;
                }
            }
        }
    }
    fn advance(&mut self) {
        self.queue.advance_at(self.now);
    }
    fn output_events(&mut self) -> Result<(), String> {
        for event in self
            .playback
            .events()
            .map_err(|_| "tts.errors.audio".to_string())?
        {
            match event {
                OutputEvent::Started(segment) => {
                    let index = self
                        .appended
                        .iter()
                        .find(|audio| audio.segment == segment)
                        .map(|audio| audio.sentence)
                        .ok_or_else(|| "tts.errors.audio".to_string())?;
                    self.started_sentence = Some(index);
                    if let Some(text) = self.metadata.get(&index) {
                        self.audible_range = Some((text.source_start, text.source_end));
                        self.reported_position.set(None);
                    }
                    self.queue.playback_started();
                    self.first_play.get_or_insert(self.now);
                }
                OutputEvent::Finished { index, delivered } => {
                    let Some(scheduled) = self.appended.pop_front() else {
                        return Err("tts.errors.audio".into());
                    };
                    if index != scheduled.segment || delivered != scheduled.samples {
                        return Err("tts.errors.audio".into());
                    }
                    if let Some(audio) = &self.queue.playing {
                        self.played_seconds += audio.seconds();
                        self.completed_chars = audio.source_end;
                    }
                    self.queue.playback_finished();
                }
                OutputEvent::Underrun => {
                    if self.queue.state == PlaybackState::Playing && self.queue.playing.is_none() {
                        self.advance();
                    }
                }
                OutputEvent::DeviceLost => {}
            }
        }
        Ok(())
    }
    fn append_ahead(&mut self) -> Result<(), String> {
        if self.queue.state != PlaybackState::Playing {
            return Ok(());
        }
        let audible = self.playback.current_index().is_some();
        let ahead = self.appended.len().saturating_sub(usize::from(audible));
        for (position, audio) in self
            .queue
            .playing
            .iter()
            .chain(self.queue.waiting.iter())
            .skip(self.appended.len())
            .enumerate()
        {
            let (ticket, pcm, pause_ms, speed) = (
                audio.ticket,
                &audio.pcm,
                audio.pause_ms,
                audio.plan.requested,
            );
            if ahead + position >= 2 {
                break;
            }
            self.sample_rate = pcm.sample_rate;
            self.playback
                .output
                .set_sample_rate(pcm.sample_rate)
                .map_err(|_| "tts.errors.audio".to_string())?;
            let index = if self
                .retracted_segments
                .front()
                .is_some_and(|(sentence, _)| *sentence == ticket.index)
            {
                self.retracted_segments
                    .pop_front()
                    .map(|(_, segment)| segment)
                    .ok_or_else(|| "tts.errors.audio".to_string())?
            } else if self.seen_sentences.insert(ticket.index) {
                ticket.index
            } else {
                let index = self
                    .source_utf16
                    .checked_add(self.next_part_index)
                    .ok_or_else(|| "tts.errors.audio".to_string())?;
                self.next_part_index = self
                    .next_part_index
                    .checked_add(1)
                    .ok_or_else(|| "tts.errors.audio".to_string())?;
                index
            };
            let segment = Segment {
                epoch: ticket.epoch,
                index,
                pause: crate::edges::pause_samples(pause_ms, speed, pcm.sample_rate),
                speech: pcm.samples.clone(),
            };
            let samples = segment.samples();
            self.playback
                .append(segment)
                .map_err(|_| "tts.errors.audio".to_string())?;
            self.appended.push_back(Appended {
                sentence: ticket.index,
                segment: index,
                samples,
            });
        }
        Ok(())
    }
    /// Applies playback controls from the next sentence without changing the epoch.
    pub fn set_speed_pitch(&mut self, speed: f32, pitch: f32) -> Result<(), String> {
        let mut chosen = self.selection.clone();
        chosen.speed = speed;
        chosen.pitch = pitch;
        crate::selection::clamp_selection(chosen.clone())
            .map_err(|_| "tts.errors.audio".to_string())?;
        self.playback.pause();
        let result = self.restretch_unstarted(chosen);
        if self.queue.state != PlaybackState::Paused {
            self.playback.resume();
        }
        result?;
        self.append_ahead()
    }
    fn restretch_unstarted(&mut self, chosen: Selection) -> Result<(), String> {
        let speed = chosen.speed;
        let pitch = chosen.pitch;
        self.output_events()?;
        let retracted = self.playback.retract_unstarted();
        self.output_events()?;
        for segment in &retracted {
            if let Some(position) = self
                .appended
                .iter()
                .rposition(|audio| audio.segment == segment.index)
                && let Some(audio) = self.appended.remove(position)
            {
                self.retracted_segments
                    .push_back((audio.sentence, audio.segment));
            }
        }
        self.selection = chosen;
        self.queue.speed = speed;
        let audible_index = self.started_sentence.filter(|index| {
            self.queue
                .playing
                .as_ref()
                .is_some_and(|audio| audio.ticket.index == *index)
        });
        let current_started = audible_index.is_some();
        let mut restart = None;
        for audio in self
            .queue
            .playing
            .iter_mut()
            .skip(usize::from(current_started))
            .chain(self.queue.waiting.iter_mut())
            .chain(self.queue.ready.values_mut())
        {
            if Some(audio.ticket.index) == audible_index {
                continue;
            }
            if let Some(stretch) = crate::speed::restretch(audio.plan.native, speed) {
                audio.pcm = dsp::stretch(
                    Pcm {
                        samples: audio.native.clone(),
                        ..audio.pcm.clone()
                    },
                    stretch,
                    pitch,
                )
                .map_err(|_| "tts.errors.audio".to_string())?;
                audio.plan.requested = speed;
                audio.plan.stretch = stretch;
            } else {
                restart =
                    Some(restart.map_or(audio.ticket.index, |i: usize| i.min(audio.ticket.index)));
            }
        }
        for (index, plan) in &self.request_plans {
            if Some(*index) != audible_index
                && self.queue.in_flight.contains_key(index)
                && crate::speed::restretch(plan.native, speed).is_none()
            {
                restart = Some(restart.map_or(*index, |i| i.min(*index)));
            }
        }
        for (index, parts) in &mut self.parts {
            if Some(*index) == audible_index {
                continue;
            }
            if let Some(stretch) = crate::speed::restretch(parts.plan.native, speed) {
                let old_samples: usize =
                    parts.samples.iter().map(|(pcm, _)| pcm.samples.len()).sum();
                if let Some((first, _)) = parts.samples.front() {
                    let rate = first.sample_rate;
                    let id = first.id;
                    let native: Vec<_> = parts
                        .samples
                        .iter_mut()
                        .flat_map(|(_, native)| std::mem::take(native))
                        .collect();
                    let pcm = dsp::stretch(
                        Pcm {
                            id,
                            sample_rate: rate,
                            samples: native.clone(),
                        },
                        stretch,
                        pitch,
                    )
                    .map_err(|_| "tts.errors.audio".to_string())?;
                    let new_samples = pcm.samples.len();
                    if new_samples > 0 {
                        parts.elapsed_per_second *= old_samples as f64 / new_samples as f64;
                    }
                    parts.plan.requested = speed;
                    parts.plan.stretch = stretch;
                    let pause_ms = self.metadata.get(index).map_or(0, |text| text.pause_ms);
                    parts.samples = split_samples(pcm, native, parts.plan, pause_ms);
                }
            } else {
                restart = Some(restart.map_or(*index, |i| i.min(*index)));
            }
        }
        let mut played = 0.0;
        let mut stored = 0.0;
        for audio in self
            .queue
            .playing
            .iter()
            .chain(self.queue.waiting.iter())
            .chain(self.queue.ready.values())
        {
            let current = self
                .queue
                .playing
                .as_ref()
                .is_some_and(|first| std::ptr::eq(first, audio));
            if !current {
                played += audio.seconds();
            }
            stored += audio.stored_seconds();
            if played > QUEUED_AUDIO_CEILING + 0.00001
                || stored > crate::scheduler::STORED_SECONDS_CEILING + 0.00001
            {
                restart = Some(restart.map_or(audio.ticket.index, |i| i.min(audio.ticket.index)));
                break;
            }
        }
        if let Some(index) = restart {
            self.restart_from = Some(index);
            self.retracted_segments
                .retain(|(sentence, _)| *sentence < index);
            self.seen_sentences.retain(|sentence| *sentence < index);
            self.queue
                .waiting
                .retain(|audio| audio.ticket.index < index);
            self.queue.ready.retain(|i, _| *i < index);
            let discarded: Vec<_> = self
                .parts
                .keys()
                .filter(|i| **i >= index)
                .copied()
                .collect();
            for i in discarded {
                self.queue.in_flight.remove(&i);
            }
            self.parts.retain(|i, _| *i < index);
            if !current_started
                && self
                    .queue
                    .playing
                    .as_ref()
                    .is_some_and(|a| a.ticket.index >= index)
            {
                self.queue.playing = None;
            }
        }
        Ok(())
    }
    /// Polls worker and output using a monotonic caller-supplied clock.
    pub fn tick(&mut self, now: f64) -> Result<(), String> {
        if !now.is_finite() || now < self.now {
            return Err("tts.errors.clock".into());
        }
        self.now = now;
        self.output_events()?;
        self.advance();
        while let Some(completion) = self.worker.poll() {
            self.received(completion);
        }
        self.admit_parts();
        let retained: Vec<_> = self.queue.in_flight.values().copied().collect();
        self.worker.retain_requests(&retained);
        if let Some(index) = self.restart_from
            && self.queue.failed_index.is_none()
            && self.queue.in_flight.is_empty()
            && self.parts.is_empty()
            && self.queue.ready.is_empty()
        {
            let (cursor, pending) = self.prepare_cursor(self.selection.read_code, index, None)?;
            self.cursor = cursor;
            self.pending_text = pending;
            self.metadata.retain(|i, _| *i < index);
            self.request_plans.retain(|i, _| *i < index);
            self.queue.rerequest_from(index);
            self.restart_from = None;
        }
        while self.restart_from.is_none() {
            if let Some(lanes) = self.worker.available_lanes() {
                self.queue.set_lanes(lanes);
                if lanes == 0 {
                    break;
                }
            }
            let Some(ticket) = self.queue.request_next() else {
                break;
            };
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
                    let result = self.speed_plan().and_then(|plan| {
                        self.request_plans.insert(ticket.index, plan);
                        let mut native_selection = self.selection.clone();
                        native_selection.speed = plan.native;
                        self.worker
                            .try_request(ticket, &text.text, &native_selection)
                    });
                    match result {
                        Ok(false) => {
                            self.queue.defer_request(ticket);
                            self.request_plans.remove(&ticket.index);
                            self.pending_text = Some(text);
                            break;
                        }
                        Ok(true) => {
                            self.metadata.insert(ticket.index, text);
                        }
                        Err(error) => {
                            self.metadata.insert(ticket.index, text);
                            self.error = Some(error);
                            self.queue.complete(ticket, Err(()));
                            let retained: Vec<_> = self.queue.in_flight.values().copied().collect();
                            self.worker.retain_requests(&retained);
                        }
                    }
                }
                None => {
                    self.queue.in_flight.remove(&ticket.index);
                    self.queue.source_finished = true;
                    break;
                }
            }
        }
        self.advance();
        self.append_ahead()?;
        let min = self
            .queue
            .playing
            .as_ref()
            .map(|c| c.ticket.index)
            .or_else(|| self.queue.waiting.front().map(|c| c.ticket.index))
            .or_else(|| self.queue.in_flight.keys().next().copied())
            .or(self.queue.failed_index)
            .unwrap_or(self.queue.next_index);
        self.metadata.retain(|index, _| *index >= min);
        self.seen_sentences.retain(|index| *index >= min);
        self.request_plans.retain(|index, _| *index >= min);
        self.output_fallback |= self.playback.take_fallback();
        Ok(())
    }
    /// Returns the current source range, progress and playback state.
    pub fn snapshot(&self) -> SessionSnapshot {
        let range = if self.queue.state == PlaybackState::Error {
            self.queue
                .failed_index
                .and_then(|i| self.metadata.get(&i))
                .map(|t| (t.source_start, t.source_end))
                .or(self.audible_range)
                .unwrap_or((0, 0))
        } else {
            self.audible_range.unwrap_or((0, 0))
        };
        let chars = self.source_utf16 as f64;
        let progress = if self.queue.state == PlaybackState::Finished {
            1.0
        } else if chars > 0.0 {
            f64::from(range.0) / chars
        } else {
            0.0
        };
        let content = self
            .playback
            .content_samples()
            .saturating_sub(self.content_origin);
        let elapsed = self.elapsed_offset
            + if self.sample_rate > 0 {
                content as f64 / f64::from(self.sample_rate)
            } else {
                0.0
            };
        let rate = if self.completed_chars > 0 && self.played_seconds > 0.0 {
            f64::from(self.completed_chars) / self.played_seconds
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
        native_speeds: Vec<Option<f32>>,
        cancelled: Vec<Ticket>,
        fail_once: Option<usize>,
        busy_once: bool,
        seconds: f64,
        selections: usize,
    }
    impl Worker for FakeWorker {
        fn request(
            &mut self,
            ticket: Ticket,
            _: &str,
            selection: &Selection,
        ) -> Result<(), String> {
            self.requested.push(ticket);
            self.native_speeds.push(Some(selection.speed));
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
        fn try_request(
            &mut self,
            ticket: Ticket,
            text: &str,
            selection: &Selection,
        ) -> Result<bool, String> {
            if std::mem::take(&mut self.busy_once) {
                return Ok(false);
            }
            self.request(ticket, text, selection).map(|()| true)
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
        feed: Option<crate::feed::Feed>,
        available: usize,
        clock: f64,
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
        fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError> {
            if self.feed.is_none() {
                self.feed = Some(crate::feed::Feed::new(rate));
            }
            Ok(())
        }
        fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
            self.sample_counts.push(segment.speech.len());
            self.pauses.push(
                (segment.pause * 1000 / self.feed.as_ref().unwrap().sample_rate() as usize) as u32,
            );
            self.available += segment.samples();
            self.deadline = Some(self.now + segment.samples() as f64 / 8000.0);
            self.feed.as_mut().unwrap().push(segment);
            Ok(())
        }
        fn events(&mut self) -> Vec<OutputEvent> {
            let delta = (self.now - self.clock).max(0.0);
            self.clock = self.now;
            let Some(feed) = &mut self.feed else {
                return vec![];
            };
            if !self.paused {
                let count =
                    ((delta * f64::from(feed.sample_rate())).round() as usize).min(self.available);
                feed.pull(&mut vec![0.0; count]);
                self.available -= count;
            }
            let events: Vec<_> = feed
                .drain_events()
                .into_iter()
                .map(OutputEvent::from)
                .collect();
            for event in &events {
                if let OutputEvent::Started(index) = event {
                    self.played.push(*index as u64 + 1);
                }
            }
            events
        }
        fn retract_unstarted(&mut self) -> Vec<Segment> {
            let retracted = self
                .feed
                .as_mut()
                .map(|f| f.retract_unstarted())
                .unwrap_or_default();
            self.available -= retracted.iter().map(Segment::samples).sum::<usize>();
            retracted
        }
        fn content_samples(&self) -> u64 {
            self.feed.as_ref().map_or(0, |f| f.content_samples())
        }
        fn pause(&mut self) {
            self.paused = true;
        }
        fn resume(&mut self) {
            self.paused = false;
            self.clock = self.now;
        }
        fn stop(&mut self) {
            self.deadline = None;
            self.feed = None;
            self.available = 0;
            self.clock = self.now;
            self.paused = false;
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
    fn worker_backpressure_retains_ticket_and_text_until_admitted() {
        let source = "Every sentence contains enough characters for its own speech chunk.";
        let mut session = session(source, 0.1);
        session.worker.busy_once = true;
        session.play().unwrap();
        session.tick(0.0).unwrap();
        assert_eq!(session.queue.failed_index, None);
        assert_eq!(session.queue.next_index, 0);
        assert!(session.queue.in_flight.is_empty());
        assert!(session.worker.requested.is_empty());
        assert_eq!(session.pending_text.as_ref().unwrap().text, source);
        assert_eq!(session.snapshot().error, None);
        session.tick(0.0).unwrap();
        assert_eq!(
            session.worker.requested,
            [Ticket {
                epoch: session.queue.epoch,
                index: 0
            }]
        );
        assert_eq!(session.metadata[&0].text, source);
        assert_eq!(session.queue.failed_index, None);
        assert!(session.queue.in_flight.contains_key(&0));
    }
    mod streaming {
        use super::*;
        use crate::{
            feed::{Feed, Segment},
            playback::segment::{Output, OutputEvent},
        };

        struct Audio {
            feed: Feed,
            paused: bool,
            trace: Vec<OutputEvent>,
            appended: Vec<(usize, usize)>,
            expected_speech: Option<Vec<f32>>,
        }
        impl Default for Audio {
            fn default() -> Self {
                Self {
                    feed: Feed::new(8000),
                    paused: false,
                    trace: vec![],
                    appended: vec![],
                    expected_speech: None,
                }
            }
        }
        impl Output for Audio {
            type Prepared = ();
            fn names(&self) -> Result<Vec<String>, PlaybackError> {
                Ok(vec![])
            }
            fn open(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn prepare(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn commit(&mut self, _: ()) {}
            fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError> {
                assert_eq!(rate, 8000);
                Ok(())
            }
            fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
                if let Some(expected) = &self.expected_speech {
                    assert_eq!(&segment.speech, expected);
                }
                self.appended.push((segment.index, segment.speech.len()));
                self.feed.push(segment);
                Ok(())
            }
            fn events(&mut self) -> Vec<OutputEvent> {
                let events: Vec<_> = self
                    .feed
                    .drain_events()
                    .into_iter()
                    .map(OutputEvent::from)
                    .collect();
                self.trace.extend(&events);
                events
            }
            fn retract_unstarted(&mut self) -> Vec<Segment> {
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
                self.feed = Feed::new(8000);
                self.paused = false;
            }
        }
        #[derive(Default)]
        struct Lanes {
            pending: VecDeque<Completion>,
            requests: Vec<(Ticket, f32)>,
            texts: Vec<String>,
            cancelled: Vec<Ticket>,
            failed: Option<usize>,
            slow: bool,
            native_samples: Option<usize>,
        }
        impl Worker for Lanes {
            fn request(
                &mut self,
                ticket: Ticket,
                text: &str,
                selection: &Selection,
            ) -> Result<(), String> {
                self.requests.push((ticket, selection.speed));
                self.texts.push(text.to_owned());
                self.pending.push_front(Completion {
                    ticket,
                    result: if self.failed == Some(ticket.index) {
                        Err("tts.errors.worker".into())
                    } else {
                        Ok(Pcm {
                            id: ticket.index as u64,
                            sample_rate: 8000,
                            samples: vec![0.25; self.native_samples.unwrap_or(8000)],
                        })
                    },
                    synthesis_seconds: if self.slow { 3.0 } else { 0.01 },
                });
                Ok(())
            }
            fn poll(&mut self) -> Option<Completion> {
                self.pending.pop_front()
            }
            fn cancel(&mut self, ticket: Ticket) {
                self.cancelled.push(ticket);
            }
            fn selection_changed(&mut self) {}
        }
        fn create(source: &str, speed: f32) -> Session<'_, Lanes, Audio> {
            let mut s = Session::new(
                source,
                Selection {
                    speed,
                    ..selection()
                },
                Lanes::default(),
                Audio::default(),
                None,
            )
            .unwrap();
            s.queue.set_lanes(3);
            s.play().unwrap();
            s
        }
        fn pump(s: &mut Session<'_, Lanes, Audio>, now: f64, pull: usize) {
            s.tick(now).unwrap();
            if s.queue.state == PlaybackState::Playing && !s.playback.output.paused {
                s.playback.output.feed.pull(&mut vec![0.0; pull]);
            }
        }
        #[test]
        fn resume_from_buffering_keeps_native_audio_until_started() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(8);
            for (speed, pitch) in [(2.0, 0.0), (1.0, 2.0), (2.0, 2.0)] {
                let mut s = create(&source, 1.0);
                s.tick(0.0).unwrap();
                s.pause();
                s.tick(0.01).unwrap();
                assert!(s.queue.playing.is_none());
                assert!(!s.queue.waiting.is_empty());
                let epoch = s.queue.epoch;
                s.play().unwrap();
                s.tick(0.02).unwrap();
                assert!(s.playback.current_index().is_none());
                assert_eq!(s.playback.content_samples(), 0);
                let native = crate::edges::trim(vec![0.25; 8000], 8000).samples;
                let expected = dsp::stretch(
                    Pcm {
                        id: 0,
                        sample_rate: 8000,
                        samples: native,
                    },
                    speed,
                    pitch,
                )
                .unwrap();
                s.playback.output.expected_speech = Some(expected.samples.clone());
                s.set_speed_pitch(speed, pitch).unwrap();
                assert_eq!(s.queue.epoch, epoch);
                assert!(!s.playback.output.paused);
                assert_eq!(s.queue.state, PlaybackState::Playing);
                for i in 2..2000 {
                    pump(&mut s, f64::from(i) * 0.02, 160);
                    if s.queue.state == PlaybackState::Finished {
                        break;
                    }
                }
                assert_eq!(s.queue.state, PlaybackState::Finished);
                assert_eq!(s.snapshot().error, None);
                let started: Vec<_> = s
                    .playback
                    .output
                    .trace
                    .iter()
                    .filter_map(|event| match event {
                        OutputEvent::Started(index) => Some(*index),
                        _ => None,
                    })
                    .collect();
                let finished: Vec<_> = s
                    .playback
                    .output
                    .trace
                    .iter()
                    .filter_map(|event| match event {
                        OutputEvent::Finished { index, delivered } => {
                            assert_eq!(
                                *delivered,
                                expected.samples.len()
                                    + crate::edges::pause_samples(450, speed, 8000)
                            );
                            Some(*index)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(started, (0..8).collect::<Vec<_>>());
                assert_eq!(finished, started);
                assert_eq!(
                    s.worker
                        .texts
                        .iter()
                        .flat_map(|text| text.split_whitespace())
                        .collect::<Vec<_>>(),
                    source.split_whitespace().collect::<Vec<_>>()
                );
            }
        }
        #[test]
        fn buffered_resume_charges_retained_native_to_storage_budget() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(12);
            let mut s = create(&source, 2.0);
            let bounded = |s: &Session<'_, Lanes, Audio>| {
                let retained: f64 = s
                    .queue
                    .playing
                    .iter()
                    .chain(s.queue.waiting.iter())
                    .chain(s.queue.ready.values())
                    .map(|audio| {
                        (audio.pcm.samples.len() + audio.native.len()) as f64
                            / f64::from(audio.pcm.sample_rate)
                    })
                    .sum();
                assert!(
                    retained <= crate::scheduler::STORED_SECONDS_CEILING + 0.00001,
                    "retained PCM exceeds storage ceiling: {retained}"
                );
                assert!(s.queue.queued_seconds() <= QUEUED_AUDIO_CEILING + 0.00001);
            };
            s.worker.native_samples = Some(104_000);
            s.tick(0.0).unwrap();
            bounded(&s);
            assert_eq!(s.worker.pending.len(), 3);
            for completion in &mut s.worker.pending {
                completion.synthesis_seconds = 60.0;
            }
            s.pause();
            bounded(&s);
            s.tick(0.01).unwrap();
            bounded(&s);
            assert!(s.queue.playing.is_none());
            assert_eq!(s.queue.waiting.len(), 3);
            assert!((s.queue.stored_seconds() - 58.5).abs() < 0.00001);
            s.worker.native_samples = Some(24_000);
            s.play().unwrap();
            bounded(&s);
            s.tick(0.02).unwrap();
            bounded(&s);
            assert_eq!(s.worker.pending.len(), 3);
            s.tick(0.03).unwrap();
            assert_eq!(s.playback.content_samples(), 0);
            bounded(&s);
            assert_eq!(s.queue.playing.as_ref().unwrap().native.len(), 104_000);
            s.playback.output.feed.pull(&mut [0.0]);
            bounded(&s);
            s.tick(0.04).unwrap();
            bounded(&s);
            assert!(s.queue.playing.as_ref().unwrap().native.is_empty());
            for i in 4..6000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                bounded(&s);
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
            let started: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|event| match event {
                    OutputEvent::Started(index) => Some(*index),
                    _ => None,
                })
                .collect();
            let finished: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|event| match event {
                    OutputEvent::Finished { index, delivered } => {
                        let speech = if *index < 3 { 52_000 } else { 12_000 };
                        assert_eq!(*delivered, speech + 1800);
                        Some(*index)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(started, (0..12).collect::<Vec<_>>());
            assert_eq!(finished, started);
            assert_eq!(s.queue.stored_seconds(), 0.0);
            assert_eq!(
                s.worker
                    .texts
                    .iter()
                    .flat_map(|text| text.split_whitespace())
                    .collect::<Vec<_>>(),
                source.split_whitespace().collect::<Vec<_>>()
            );
        }
        #[test]
        fn failed_control_render_resumes_output_before_returning_error() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(8);
            let mut s = create(&source, 1.0);
            s.tick(0.0).unwrap();
            s.tick(0.01).unwrap();
            let native = &mut s.queue.playing.as_mut().unwrap().native;
            assert!(!native.is_empty());
            native[0] = f32::NAN;
            assert_eq!(s.set_speed_pitch(2.0, 0.0), Err("tts.errors.audio".into()));
            assert_eq!(s.queue.state, PlaybackState::Playing);
            assert!(!s.playback.output.paused);
        }
        #[test]
        fn twenty_minute_document_plays_gapless_at_2x_with_fast_worker() {
            let source = "Every sentence has enough characters to form its own speech chunk.\n\n"
                .repeat(1700);
            let mut s = create(&source, 2.0);
            for i in 0..200_000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                assert!(
                    s.playback.output.feed.unstarted_len() <= 2,
                    "append_ahead_is_bounded"
                );
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
            let trace = &s.playback.output.trace;
            let started: Vec<_> = trace
                .iter()
                .filter_map(|e| {
                    if let OutputEvent::Started(i) = e {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            let finished: Vec<_> = trace
                .iter()
                .filter_map(|e| {
                    if let OutputEvent::Finished { index, .. } = e {
                        Some(*index)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(started, (0..1700).collect::<Vec<_>>());
            assert!(s.snapshot().elapsed_seconds >= 1200.0);
            assert_eq!(finished, started);
            assert_eq!(s.queue.buffering_pauses, 0);
            assert_eq!(s.playback.output.feed.underrun_samples(), 0);
            for event in trace {
                if let OutputEvent::Finished { delivered, .. } = event {
                    assert_eq!(*delivered, 5800);
                }
            }
        }
        #[test]
        fn highlight_follows_started_event_not_append() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(8);
            let mut s = create(&source, 2.0);
            s.tick(0.0).unwrap();
            s.tick(0.01).unwrap();
            assert!(!s.playback.output.appended.is_empty());
            assert_eq!(s.snapshot().source_end, 0);
            s.playback.output.feed.pull(&mut [0.0]);
            s.tick(0.02).unwrap();
            let first = s.snapshot();
            assert!(first.source_end > first.source_start);
            assert!(s.playback.output.feed.unstarted_len() > 0);
            assert_eq!(first.elapsed_seconds, 1.0 / 8000.0);
            assert_eq!(s.snapshot().source_start, first.source_start);
            s.tick(0.03).unwrap();
            assert_eq!(s.snapshot().elapsed_seconds, 1.0 / 8000.0);
            s.playback.output.feed.pull(&mut vec![0.0; 5799]);
            s.tick(0.04).unwrap();
            assert_eq!(s.snapshot().source_start, first.source_start);
            s.playback.output.feed.pull(&mut [0.0]);
            s.tick(0.05).unwrap();
            assert_eq!(s.snapshot().source_start, s.metadata[&1].source_start);
            assert!(s.snapshot().source_start > first.source_start);
        }
        #[test]
        fn speed_change_applies_at_next_boundary_without_restart() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(20);
            let mut s = create(&source, 1.0);
            let epoch = s.queue.epoch;
            let mut changed = false;
            let mut previous = 0;
            for i in 0..5000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                let range = s.snapshot().source_start;
                assert!(range >= previous);
                previous = range;
                if !changed && s.playback.current_index() == Some(3) {
                    let chosen = Selection {
                        speed: 2.0,
                        ..s.selection.clone()
                    };
                    let mut persisted = 0;
                    let changed_selection = s
                        .select(chosen, true, |_| {
                            persisted += 1;
                            Ok(())
                        })
                        .unwrap();
                    assert_eq!(persisted, 1);
                    assert_eq!(changed_selection.epoch, epoch);
                    changed = true;
                    assert_eq!(s.queue.epoch, epoch);
                }
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert!(changed);
            assert_eq!(s.queue.state, PlaybackState::Finished);
            let started: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|e| {
                    if let OutputEvent::Started(i) = e {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(started, (0..20).collect::<Vec<_>>());
            for (index, count) in &s.playback.output.appended {
                if *index <= 3 {
                    assert_eq!(*count, 8000);
                }
            }
            for index in 4..20 {
                assert_eq!(
                    s.playback
                        .output
                        .appended
                        .iter()
                        .rev()
                        .find(|(i, _)| *i == index)
                        .unwrap()
                        .1,
                    4000
                );
            }
        }
        #[test]
        fn speed_change_with_out_of_range_restretch_rerequests_from_that_chunk() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(12);
            let mut s = create(&source, 2.0);
            s.native_window = |_| Ok(crate::speed::NativeWindow { min: 0.5, max: 2.0 });
            s.tick(0.0).unwrap();
            s.tick(0.01).unwrap();
            s.playback.output.feed.pull(&mut [0.0]);
            s.tick(0.02).unwrap();
            let epoch = s.queue.epoch;
            s.set_speed_pitch(0.5, 0.0).unwrap();
            assert_eq!(s.queue.epoch, epoch);
            for i in 2..5000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
            assert!(
                s.worker
                    .requests
                    .iter()
                    .filter(|(t, _)| t.index == 1)
                    .count()
                    >= 2
            );
            let started: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|e| {
                    if let OutputEvent::Started(i) = e {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(started, (0..12).collect::<Vec<_>>());
        }
        #[test]
        fn failed_middle_lane_stops_at_its_sentence_after_earlier_audio() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(12);
            let mut s = create(&source, 2.0);
            s.worker.failed = Some(4);
            for i in 0..5000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                if s.queue.state == PlaybackState::Error {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Error);
            assert_eq!(s.queue.failed_index, Some(4));
            let finished: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|e| {
                    if let OutputEvent::Finished { index, .. } = e {
                        Some(*index)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(finished, [0, 1, 2, 3]);
            s.worker.failed = None;
            s.play().unwrap();
            for i in 5000..10_000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
        }
        #[test]
        fn slowing_prepared_audio_keeps_queue_bounded_and_every_sentence() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(40);
            let mut s = create(&source, 2.0);
            s.worker.slow = true;
            for i in 0..25 {
                pump(&mut s, f64::from(i) * 0.02, 160);
            }
            assert!(s.queue.queued_seconds() > 10.0);
            let epoch = s.queue.epoch;
            s.set_speed_pitch(0.5, 0.0).unwrap();
            assert_eq!(s.queue.epoch, epoch);
            assert!(s.queue.queued_seconds() <= QUEUED_AUDIO_CEILING + 0.00001);
            assert!(s.queue.stored_seconds() <= crate::scheduler::STORED_SECONDS_CEILING + 0.00001);
            for i in 25..20_000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                assert!(s.queue.queued_seconds() <= QUEUED_AUDIO_CEILING + 0.00001);
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
            let started: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|event| {
                    if let OutputEvent::Started(i) = event {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(started, (0..40).collect::<Vec<_>>());
        }
        #[test]
        fn split_sentence_segments_have_distinct_ids_and_keep_the_same_highlight() {
            let source = "Every sentence has enough characters to form its own speech chunk.";
            let mut s = create(source, 0.5);
            s.worker.native_samples = Some(160_000);
            for i in 0..4000 {
                pump(&mut s, f64::from(i) * 0.02, 160);
                if s.started_sentence.is_some() {
                    assert_eq!(s.snapshot().source_start, 0);
                }
                if s.queue.state == PlaybackState::Finished {
                    break;
                }
            }
            assert_eq!(s.queue.state, PlaybackState::Finished);
            let started: Vec<_> = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|event| {
                    if let OutputEvent::Started(i) = event {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(started.len(), 2);
            assert_ne!(started[0], started[1]);
            assert_eq!(s.playback.output.feed.underrun_samples(), 0);
            let delivered: usize = s
                .playback
                .output
                .trace
                .iter()
                .filter_map(|event| {
                    if let OutputEvent::Finished { delivered, .. } = event {
                        Some(*delivered)
                    } else {
                        None
                    }
                })
                .sum();
            assert_eq!(delivered, 320_000 + 7200);
        }
        #[test]
        fn stop_with_three_lanes_returns_without_waiting_and_drops_late_replies() {
            let source =
                "Every sentence has enough characters to form its own speech chunk.\n\n".repeat(12);
            let mut s = create(&source, 2.0);
            s.tick(0.0).unwrap();
            assert_eq!(s.queue.in_flight.len(), 3);
            s.stop();
            assert_eq!(s.worker.cancelled.len(), 3);
            s.tick(0.01).unwrap();
            assert!(s.playback.output.appended.is_empty());
            assert_eq!(s.queue.state, PlaybackState::Waiting);
        }
    }
    #[test]
    fn session_requests_native_speed_from_plan() {
        for (id, window, expected) in [
            (
                "pocket-en",
                crate::speed::NativeWindow { min: 1.0, max: 1.0 },
                1.0,
            ),
            (
                "supertonic-3",
                crate::speed::NativeWindow { min: 1.0, max: 1.3 },
                1.3,
            ),
        ] {
            let mut s = session(
                "Every word in this sentence must reach the listener completely.",
                0.0,
            );
            s.selection.model_id = id.into();
            s.native_window = |id| {
                Ok(crate::speed::NativeWindow {
                    min: 1.0,
                    max: if id == "pocket-en" { 1.0 } else { 1.3 },
                })
            };
            assert_eq!((s.native_window)(id).unwrap(), window);
            s.play().unwrap();
            step(&mut s, 0.0);
            assert_eq!(s.worker.native_speeds, [Some(expected)]);
            step(&mut s, 0.1);
            assert_eq!(
                s.playback.output.sample_counts,
                [(8000.0 / (2.0 / expected)).round() as usize]
            );
        }
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
            let old_pending = s.queue.in_flight.clone();
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
        for i in 0..5 {
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
        assert_eq!(s.playback.output.stops, 2);
        assert_eq!(changed.epoch, before.0 + 1);
        assert_eq!(changed.sentence_index, 0);
        assert_eq!(s.selection, chosen);
        for i in 5..10 {
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
            let in_flight = s.queue.in_flight.clone();
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
        let played_seconds = s.queue.playing.as_ref().unwrap().seconds();
        assert!((s.queue.rtf_played - 0.35 / played_seconds).abs() < 1e-12);
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
            if s.queue.in_flight.contains_key(&2) {
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
        assert!(s.queue.in_flight.is_empty());
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
        let old = s.queue.in_flight.clone();
        s.fail("tts.errors.audio".into());
        assert_eq!(s.queue.failed_index, Some(current.index));
        assert!(s.queue.playing.is_none());
        assert!(s.parts.is_empty());
        assert!(s.appended.is_empty());
        for ticket in old.values() {
            assert!(s.worker.cancelled.contains(ticket));
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
        s.worker.seconds = 130.0;
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
            assert!(s.parts.is_empty());
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
        let old = s.queue.in_flight.clone();
        let mut chosen = selection();
        chosen.voice = "M1".into();
        let changed = s.select(chosen.clone(), true, |_| Ok(())).unwrap();
        assert_eq!(changed.sentence_index, current);
        for ticket in old.values().copied() {
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
        assert!(s.parts.is_empty());
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
        s.worker.seconds = 130.0;
        s.play().unwrap();
        step(&mut s, 0.0);
        step(&mut s, 0.2);
        assert!(s.queue.playing.as_ref().unwrap().seconds() <= 30.0);
        assert!(!s.parts.is_empty());
        assert!(!s.queue.in_flight.is_empty());
        let old = *s.queue.in_flight.values().next().unwrap();
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
        assert!(s.parts.is_empty());
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
