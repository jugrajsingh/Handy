use handy_tts_core::{
    feed::{Feed, Segment, SharedFeed},
    playback::{
        segment::{Output, OutputEvent},
        PlaybackError,
    },
};
use rodio::cpal::traits::HostTrait;
use rodio::{
    mixer::Mixer, queue::SourcesQueueOutput, source::UniformSourceIterator, ChannelCount,
    DeviceSinkBuilder as OutputStreamBuilder, MixerDeviceSink as OutputStream, Player as Sink,
    SampleRate, Source,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

struct FormatSpan {
    index: usize,
    rate: u32,
    remaining: usize,
}
struct FormatState {
    rate: u32,
    spans: VecDeque<FormatSpan>,
}
type SharedFormat = Arc<Mutex<FormatState>>;

struct FeedSource {
    feed: SharedFeed,
    format: SharedFormat,
    block: Vec<f32>,
    pos: usize,
    paused: Arc<AtomicBool>,
}
impl Iterator for FeedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let mut feed = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        if self.paused.load(Ordering::Acquire) {
            return Some(0.0);
        }
        let mut format = self.format.lock().unwrap_or_else(|p| p.into_inner());
        feed.pull(&mut self.block[self.pos..self.pos + 1]);
        if let Some(span) = format.spans.front_mut() {
            let rate = span.rate;
            span.remaining -= 1;
            let finished = span.remaining == 0;
            format.rate = rate;
            if finished {
                format.spans.pop_front();
            }
        }
        let sample = self.block[self.pos];
        self.pos = (self.pos + 1) % self.block.len();
        Some(sample)
    }
}
impl Source for FeedSource {
    fn current_span_len(&self) -> Option<usize> {
        let format = self.format.lock().unwrap_or_else(|p| p.into_inner());
        Some(format.spans.front().map(|span| span.remaining).unwrap_or(0))
    }
    fn channels(&self) -> ChannelCount {
        ChannelCount::MIN
    }
    fn sample_rate(&self) -> SampleRate {
        let format = self.format.lock().unwrap_or_else(|p| p.into_inner());
        SampleRate::new(
            format
                .spans
                .front()
                .map(|span| span.rate)
                .unwrap_or(format.rate),
        )
        .unwrap_or(SampleRate::MIN)
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[derive(Clone, Copy)]
struct MixerFormat {
    rate: u32,
    channels: u16,
}

#[derive(Clone)]
struct NativeSource {
    queue: Arc<Mutex<SourcesQueueOutput>>,
    format: SharedFormat,
}
impl Iterator for NativeSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self
            .format
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .spans
            .is_empty()
        {
            return None;
        }
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).next()
    }
}
impl Source for NativeSource {
    fn current_span_len(&self) -> Option<usize> {
        let format = self.format.lock().unwrap_or_else(|p| p.into_inner());
        Some(format.spans.front().map(|span| span.remaining).unwrap_or(0))
    }
    fn channels(&self) -> ChannelCount {
        ChannelCount::MIN
    }
    fn sample_rate(&self) -> SampleRate {
        let format = self.format.lock().unwrap_or_else(|p| p.into_inner());
        SampleRate::new(
            format
                .spans
                .front()
                .map(|span| span.rate)
                .unwrap_or(format.rate),
        )
        .unwrap_or(SampleRate::MIN)
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

struct Conversion {
    input: NativeSource,
    converted: Option<UniformSourceIterator<NativeSource>>,
    idle_phase: u64,
    idle_channel: u16,
}
type SharedConversion = Arc<Mutex<Conversion>>;

struct MixerFeedSource {
    conversion: SharedConversion,
    feed: SharedFeed,
    paused: Arc<AtomicBool>,
    output: MixerFormat,
}
impl Iterator for MixerFeedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let mut conversion = self.conversion.lock().unwrap_or_else(|p| p.into_inner());
        if self.paused.load(Ordering::Acquire) {
            return Some(0.0);
        }
        if conversion.converted.is_none() {
            conversion.converted = Some(UniformSourceIterator::new(
                conversion.input.clone(),
                ChannelCount::new(self.output.channels).unwrap_or(ChannelCount::MIN),
                SampleRate::new(self.output.rate).unwrap_or(SampleRate::MIN),
            ));
        }
        if let Some(sample) = conversion.converted.as_mut().and_then(Iterator::next) {
            conversion.idle_phase = 0;
            conversion.idle_channel = 0;
            return Some(sample);
        }
        conversion.converted = None;
        let mut feed = self.feed.lock().unwrap_or_else(|p| p.into_inner());
        if !self.paused.load(Ordering::Acquire) && conversion.idle_channel == 0 {
            let rate = conversion
                .input
                .format
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .rate;
            conversion.idle_phase += u64::from(rate);
            while conversion.idle_phase >= u64::from(self.output.rate) {
                feed.pull(&mut [0.0]);
                conversion.idle_phase -= u64::from(self.output.rate);
            }
        }
        conversion.idle_channel = (conversion.idle_channel + 1) % self.output.channels;
        Some(0.0)
    }
}
impl Source for MixerFeedSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        ChannelCount::new(self.output.channels).unwrap_or(ChannelCount::MIN)
    }
    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(self.output.rate).unwrap_or(SampleRate::MIN)
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

pub struct RodioOutput {
    selected: Option<String>,
    stream: Option<OutputStream>,
    sink: Option<Arc<Sink>>,
    feed: Option<SharedFeed>,
    rate: Option<u32>,
    mixer: Option<Mixer>,
    mixer_format: Option<MixerFormat>,
    format: Option<SharedFormat>,
    conversion: Option<SharedConversion>,
    scheduled: BTreeMap<usize, usize>,
    lost: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    #[cfg(test)]
    finished_logs: Vec<String>,
}
impl Default for RodioOutput {
    fn default() -> Self {
        Self {
            selected: None,
            stream: None,
            sink: None,
            feed: None,
            rate: None,
            mixer: None,
            mixer_format: None,
            format: None,
            conversion: None,
            scheduled: BTreeMap::new(),
            lost: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            finished_logs: Vec::new(),
        }
    }
}
pub struct PreparedStream {
    selected: Option<String>,
    stream: Option<OutputStream>,
    mixer: Mixer,
    mixer_format: MixerFormat,
    rate: Option<u32>,
    sink: Option<Arc<Sink>>,
    feed: Option<SharedFeed>,
    format: Option<SharedFormat>,
    conversion: Option<SharedConversion>,
    lost: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}
impl PreparedStream {
    fn new(
        mixer: Mixer,
        stream: Option<OutputStream>,
        selected: Option<String>,
        lost: Arc<AtomicBool>,
        rate: Option<u32>,
        mixer_format: MixerFormat,
    ) -> Self {
        let paused = Arc::new(AtomicBool::new(false));
        let mut prepared = Self {
            selected,
            stream,
            mixer,
            mixer_format,
            rate,
            sink: None,
            feed: None,
            format: None,
            conversion: None,
            lost,
            paused,
        };
        if let Some(rate) = rate {
            let feed = Arc::new(Mutex::new(Feed::new(rate)));
            let (sink, format, conversion) = connect_feed(
                &prepared.mixer,
                rate,
                mixer_format,
                prepared.paused.clone(),
                feed.clone(),
            );
            prepared.sink = Some(sink);
            prepared.feed = Some(feed);
            prepared.format = Some(format);
            prepared.conversion = Some(conversion);
        }
        prepared
    }
}

fn connect_feed(
    mixer: &Mixer,
    rate: u32,
    output: MixerFormat,
    paused: Arc<AtomicBool>,
    feed: SharedFeed,
) -> (Arc<Sink>, SharedFormat, SharedConversion) {
    let format = Arc::new(Mutex::new(FormatState {
        rate,
        spans: VecDeque::new(),
    }));
    let (sink, mut source) = Sink::new();
    paused.store(true, Ordering::Release);
    sink.append(FeedSource {
        feed: feed.clone(),
        format: format.clone(),
        block: vec![0.0; 1_024],
        pos: 0,
        paused: paused.clone(),
    });
    // Activate the persistent feed before wrapping the queue's native format.
    let _ = source.next();
    let conversion = Arc::new(Mutex::new(Conversion {
        input: NativeSource {
            queue: Arc::new(Mutex::new(source)),
            format: format.clone(),
        },
        converted: None,
        idle_phase: 0,
        idle_channel: 0,
    }));
    mixer.add(MixerFeedSource {
        conversion: conversion.clone(),
        feed,
        paused: paused.clone(),
        output,
    });
    paused.store(false, Ordering::Release);
    (Arc::new(sink), format, conversion)
}

impl Output for RodioOutput {
    type Prepared = PreparedStream;
    fn names(&self) -> Result<Vec<String>, PlaybackError> {
        crate::audio_toolkit::get_cpal_host()
            .output_devices()
            .map_err(|_| PlaybackError::Unavailable)?
            .map(|device| {
                crate::audio_toolkit::audio::output_device_name(&device)
                    .ok_or(PlaybackError::Unavailable)
            })
            .collect()
    }
    fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
        let prepared = Output::prepare(self, name)?;
        Output::commit(self, prepared);
        Ok(())
    }
    fn prepare(&mut self, name: Option<&str>) -> Result<Self::Prepared, PlaybackError> {
        Self::prepare_stream(name, self.rate)
    }
    fn commit(&mut self, prepared: Self::Prepared) {
        Output::stop(self);
        self.selected = prepared.selected;
        self.sink = prepared.sink;
        self.feed = prepared.feed;
        self.format = prepared.format;
        self.conversion = prepared.conversion;
        self.rate = prepared.rate;
        self.mixer = Some(prepared.mixer);
        self.mixer_format = Some(prepared.mixer_format);
        self.stream = prepared.stream;
        self.lost = prepared.lost;
        self.paused = prepared.paused;
    }
    fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError> {
        RodioOutput::set_sample_rate(self, rate)
    }
    fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
        let rate = self.rate.ok_or(PlaybackError::Unavailable)?;
        if self.sink.is_none() {
            if let Some(mixer) = self.mixer.clone() {
                let output = self.mixer_format.ok_or(PlaybackError::Unavailable)?;
                self.attach_mixer_format(&mixer, output)?;
            } else {
                self.open_stream()?;
            }
        }
        let conversion = self.conversion.as_ref().ok_or(PlaybackError::Unavailable)?;
        let _conversion = conversion.lock().unwrap_or_else(|p| p.into_inner());
        let format = self.format.as_ref().ok_or(PlaybackError::Unavailable)?;
        let feed = self.feed.as_ref().ok_or(PlaybackError::Unavailable)?;
        let mut feed = feed.lock().unwrap_or_else(|p| p.into_inner());
        let before = feed.unstarted_len();
        let index = segment.index;
        let samples = segment.samples();
        feed.push(segment);
        if feed.unstarted_len() > before {
            format
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .spans
                .push_back(FormatSpan {
                    index,
                    rate,
                    remaining: samples,
                });
            self.scheduled.insert(index, samples);
        }
        Ok(())
    }
    fn events(&mut self) -> Vec<OutputEvent> {
        let mut events: Vec<OutputEvent> = self
            .feed
            .as_ref()
            .map(|feed| {
                feed.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .drain_events()
                    .into_iter()
                    .map(OutputEvent::from)
                    .collect()
            })
            .unwrap_or_default();
        for event in &events {
            if let OutputEvent::Finished { index, delivered } = event {
                if let Some(scheduled) = self.scheduled.remove(index) {
                    // Feed underruns occur between segments, never inside one.
                    let line = format!("tts chunk finished index={index} scheduled={scheduled} delivered={delivered} underrun=0");
                    log::info!("{line}");
                    #[cfg(test)]
                    self.finished_logs.push(line);
                }
            }
        }
        if self.lost.swap(false, Ordering::AcqRel) {
            events.push(OutputEvent::DeviceLost);
        }
        events
    }
    fn retract_unstarted(&mut self) -> Vec<Segment> {
        let _conversion = self
            .conversion
            .as_ref()
            .map(|conversion| conversion.lock().unwrap_or_else(|p| p.into_inner()));
        let segments = if let Some(feed) = &self.feed {
            let mut feed = feed.lock().unwrap_or_else(|p| p.into_inner());
            let segments = feed.retract_unstarted();
            if let Some(format) = &self.format {
                format
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .spans
                    .retain(|span| !segments.iter().any(|segment| segment.index == span.index));
            }
            segments
        } else {
            Vec::new()
        };
        for segment in &segments {
            self.scheduled.remove(&segment.index);
        }
        segments
    }
    fn content_samples(&self) -> u64 {
        self.feed
            .as_ref()
            .map(|feed| {
                feed.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .content_samples()
            })
            .unwrap_or_default()
    }
    fn pause(&mut self) {
        if let Some(feed) = &self.feed {
            let _feed = feed.lock().unwrap_or_else(|p| p.into_inner());
            self.paused.store(true, Ordering::Release);
        }
        if let Some(sink) = &self.sink {
            sink.pause();
        }
    }
    fn resume(&mut self) {
        self.paused.store(false, Ordering::Release);
        if let Some(sink) = &self.sink {
            sink.play();
        }
    }
    fn stop(&mut self) {
        if let (Some(conversion), Some(feed), Some(rate)) =
            (&self.conversion, &self.feed, self.rate)
        {
            let mut conversion = conversion.lock().unwrap_or_else(|p| p.into_inner());
            let mut feed = feed.lock().unwrap_or_else(|p| p.into_inner());
            *feed = Feed::new(rate);
            if let Some(format) = &self.format {
                let mut format = format.lock().unwrap_or_else(|p| p.into_inner());
                format.spans.clear();
                format.rate = rate;
            }
            conversion.converted = None;
            conversion.idle_phase = 0;
            conversion.idle_channel = 0;
        }
        self.scheduled.clear();
        Output::resume(self);
    }
}

impl RodioOutput {
    /// Configures the native sample rate for appended model PCM.
    pub fn new(rate: u32) -> Result<Self, PlaybackError> {
        let mut output = Self::default();
        output.set_sample_rate(rate)?;
        Ok(output)
    }
    /// Changes the rate of future appends; admitted segments keep their native rate.
    pub fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError> {
        if rate == 0 {
            return Err(PlaybackError::Unavailable);
        }
        let _conversion = self
            .conversion
            .as_ref()
            .map(|conversion| conversion.lock().unwrap_or_else(|p| p.into_inner()));
        self.rate = Some(rate);
        if let Some(format) = &self.format {
            let mut format = format.lock().unwrap_or_else(|p| p.into_inner());
            if format.spans.is_empty() {
                format.rate = rate;
            }
        }
        Ok(())
    }
    #[cfg(test)]
    fn attach_mixer(&mut self, mixer: &Mixer) -> Result<(), PlaybackError> {
        let rate = self.rate.ok_or(PlaybackError::Unavailable)?;
        self.attach_mixer_format(mixer, MixerFormat { rate, channels: 1 })
    }
    fn attach_mixer_format(
        &mut self,
        mixer: &Mixer,
        output: MixerFormat,
    ) -> Result<(), PlaybackError> {
        let rate = self.rate.ok_or(PlaybackError::Unavailable)?;
        let feed = Arc::new(Mutex::new(Feed::new(rate)));
        let (sink, format, conversion) =
            connect_feed(mixer, rate, output, self.paused.clone(), feed.clone());
        self.sink = Some(sink);
        self.feed = Some(feed);
        self.format = Some(format);
        self.conversion = Some(conversion);
        self.mixer = Some(mixer.clone());
        self.mixer_format = Some(output);
        Ok(())
    }
    #[cfg(test)]
    fn with_mixer(mixer: &Mixer, rate: u32) -> Self {
        let mut output = Self::new(rate).unwrap();
        output.attach_mixer(mixer).unwrap();
        output
    }
    #[cfg(test)]
    fn prepare_mixer(&self, mixer: Mixer, stream: Option<OutputStream>) -> PreparedStream {
        PreparedStream::new(
            mixer,
            stream,
            self.selected.clone(),
            Arc::new(AtomicBool::new(false)),
            self.rate,
            MixerFormat {
                rate: self.rate.unwrap(),
                channels: 1,
            },
        )
    }
    fn open_stream(&mut self) -> Result<(), PlaybackError> {
        let prepared = Self::prepare_stream(self.selected.as_deref(), self.rate)?;
        Output::commit(self, prepared);
        Ok(())
    }
    fn prepare_stream(
        name: Option<&str>,
        rate: Option<u32>,
    ) -> Result<PreparedStream, PlaybackError> {
        let builder = if let Some(name) = name {
            let host = crate::audio_toolkit::get_cpal_host();
            let mut devices = host
                .output_devices()
                .map_err(|_| PlaybackError::Unavailable)?;
            let device = devices
                .find(|device| {
                    crate::audio_toolkit::audio::output_device_name(device).as_deref() == Some(name)
                })
                .ok_or(PlaybackError::Unavailable)?;
            OutputStreamBuilder::from_device(device).map_err(|_| PlaybackError::Unavailable)?
        } else {
            OutputStreamBuilder::from_default_device().map_err(|_| PlaybackError::Unavailable)?
        };
        let lost = Arc::new(AtomicBool::new(false));
        let signal = lost.clone();
        let stream = builder
            .with_error_callback(move |_| signal.store(true, Ordering::Release))
            .open_stream()
            .map_err(|_| PlaybackError::Unavailable)?;
        let mixer_format = MixerFormat {
            rate: stream.config().sample_rate().get(),
            channels: stream.config().channel_count().get(),
        };
        Ok(PreparedStream::new(
            stream.mixer().clone(),
            Some(stream),
            name.map(str::to_owned),
            lost,
            rate,
            mixer_format,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handy_tts_core::{
        feed::Segment,
        playback::segment::{Output as SegmentOutput, OutputEvent},
    };

    fn burst(index: usize, speech: usize, pause: usize) -> Segment {
        Segment {
            epoch: 4,
            index,
            speech: (0..speech)
                .map(|n| ((n as f32 + 0.5) * 0.13).sin() * 0.25)
                .collect(),
            pause,
        }
    }

    fn pull(output: &mut impl Iterator<Item = f32>, count: usize) -> Vec<f32> {
        (0..count).map(|_| output.next().unwrap()).collect()
    }

    #[test]
    fn stop_then_queued_rate_change_preserves_duration() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::new(24_000).unwrap();
        output.attach_mixer(&mixer).unwrap();
        let identity = output.sink.as_ref().unwrap().clone();
        SegmentOutput::append(&mut output, burst(0, 4_800, 0)).unwrap();
        pull(&mut mixed, 1_000);
        SegmentOutput::stop(&mut output);
        assert!(Arc::ptr_eq(output.sink.as_ref().unwrap(), &identity));
        let first = burst(1, 240, 0);
        SegmentOutput::append(&mut output, first.clone()).unwrap();
        output.set_sample_rate(44_100).unwrap();
        SegmentOutput::append(&mut output, burst(2, 4_410, 0)).unwrap();
        let heard = pull(&mut mixed, 2_648);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Started(1),
                OutputEvent::Finished {
                    index: 1,
                    delivered: 240
                },
                OutputEvent::Started(2),
                OutputEvent::Finished {
                    index: 2,
                    delivered: 4_410
                },
                OutputEvent::Underrun,
            ]
        );
        assert_eq!(&heard[..240], first.speech);
        let frames = heard.iter().rposition(|sample| *sample != 0.0).unwrap() + 1;
        assert!(frames.abs_diff(2_640) <= 3);
    }

    #[test]
    fn partial_stop_preserves_all_rate_pairs_and_append_timings() {
        for target in [24_000, 44_100, 48_000] {
            let channels = if target == 48_000 { 2 } else { 1 };
            let device = MixerFormat {
                rate: target,
                channels,
            };
            for initial in [24_000, 44_100] {
                for first in [24_000, 44_100] {
                    for second in [24_000, 44_100] {
                        for timing in 0..3 {
                            let (mixer, mut mixed) = rodio::mixer::mixer(
                                ChannelCount::new(channels).unwrap(),
                                SampleRate::new(target).unwrap(),
                            );
                            let mut output = RodioOutput::new(initial).unwrap();
                            output.attach_mixer_format(&mixer, device).unwrap();
                            let identity = output.sink.as_ref().unwrap().clone();
                            SegmentOutput::append(&mut output, burst(0, initial as usize / 5, 0))
                                .unwrap();
                            pull(&mut mixed, 1_000 * usize::from(channels));
                            SegmentOutput::stop(&mut output);
                            assert!(Arc::ptr_eq(output.sink.as_ref().unwrap(), &identity));
                            if timing == 2 {
                                assert!(pull(&mut mixed, 300 * usize::from(channels))
                                    .iter()
                                    .all(|sample| *sample == 0.0));
                                SegmentOutput::events(&mut output);
                            }
                            let mut heard = Vec::new();
                            for (index, rate, divisor) in [(1, first, 100), (2, second, 10)] {
                                output.set_sample_rate(rate).unwrap();
                                let speech = (0..rate / divisor)
                                    .map(|n| {
                                        (std::f32::consts::TAU * 200.0 * (n as f32 + 0.25)
                                            / rate as f32)
                                            .sin()
                                            * 0.25
                                    })
                                    .collect();
                                SegmentOutput::append(
                                    &mut output,
                                    Segment {
                                        epoch: 4,
                                        index,
                                        speech,
                                        pause: 0,
                                    },
                                )
                                .unwrap();
                                if index == 1 && timing == 1 {
                                    heard.extend(pull(
                                        &mut mixed,
                                        target as usize / 200 * usize::from(channels),
                                    ));
                                }
                            }
                            let frames = target as usize * 11 / 100;
                            heard.extend(pull(
                                &mut mixed,
                                (frames + 8) * usize::from(channels) - heard.len(),
                            ));
                            assert_eq!(
                                SegmentOutput::events(&mut output),
                                [
                                    OutputEvent::Started(1),
                                    OutputEvent::Finished {
                                        index: 1,
                                        delivered: first as usize / 100
                                    },
                                    OutputEvent::Started(2),
                                    OutputEvent::Finished {
                                        index: 2,
                                        delivered: second as usize / 10
                                    },
                                    OutputEvent::Underrun,
                                ],
                                "target={target} initial={initial} rates={first}/{second} timing={timing}"
                            );
                            let mono: Vec<_> = heard
                                .chunks_exact(usize::from(channels))
                                .map(|frame| {
                                    assert!(frame.iter().all(|sample| *sample == frame[0]));
                                    frame[0]
                                })
                                .collect();
                            let played =
                                mono.iter().rposition(|sample| *sample != 0.0).unwrap() + 1;
                            assert!(played.abs_diff(frames) <= 3);
                            assert!(mono[..played].iter().all(|sample| *sample != 0.0));
                            let crossings = mono[..played]
                                .windows(2)
                                .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
                                .count();
                            assert!(crossings.abs_diff(22) <= 2);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn retraction_and_reopen_preserve_later_native_rate_boundaries() {
        for rate in [24_000, 44_100] {
            let next_rate = if rate == 24_000 { 44_100 } else { 24_000 };
            let (mixer, mut mixed) = rodio::mixer::mixer(
                ChannelCount::new(1).unwrap(),
                SampleRate::new(rate).unwrap(),
            );
            let mut output = RodioOutput::new(rate).unwrap();
            output.attach_mixer(&mixer).unwrap();
            SegmentOutput::append(&mut output, burst(0, rate as usize / 5, 0)).unwrap();
            pull(&mut mixed, rate as usize / 10);
            SegmentOutput::events(&mut output);
            output.set_sample_rate(next_rate).unwrap();
            SegmentOutput::append(&mut output, burst(1, next_rate as usize / 100, 0)).unwrap();
            let retracted = SegmentOutput::retract_unstarted(&mut output);
            assert_eq!(retracted.len(), 1);
            assert_eq!(retracted[0].index, 1);
            SegmentOutput::append(&mut output, burst(2, next_rate as usize / 10, 0)).unwrap();
            let heard = pull(&mut mixed, rate as usize / 5 + 8);
            let frames = heard.iter().rposition(|sample| *sample != 0.0).unwrap() + 1;
            assert!(frames.abs_diff(rate as usize / 5) <= 3);
            assert_eq!(
                SegmentOutput::events(&mut output),
                [
                    OutputEvent::Finished {
                        index: 0,
                        delivered: rate as usize / 5
                    },
                    OutputEvent::Started(2),
                    OutputEvent::Finished {
                        index: 2,
                        delivered: next_rate as usize / 10
                    },
                    OutputEvent::Underrun,
                ]
            );
            SegmentOutput::append(&mut output, burst(3, next_rate as usize / 10, 0)).unwrap();
            assert_eq!(SegmentOutput::retract_unstarted(&mut output)[0].index, 3);
            output.set_sample_rate(rate).unwrap();
            SegmentOutput::append(&mut output, burst(4, rate as usize / 100, 0)).unwrap();
            output.set_sample_rate(next_rate).unwrap();
            SegmentOutput::append(&mut output, burst(5, next_rate as usize / 10, 0)).unwrap();
            let heard = pull(&mut mixed, rate as usize * 11 / 100 + 8);
            let frames = heard.iter().rposition(|sample| *sample != 0.0).unwrap() + 1;
            assert!(frames.abs_diff(rate as usize * 11 / 100) <= 3);
            assert_eq!(
                SegmentOutput::events(&mut output),
                [
                    OutputEvent::Started(4),
                    OutputEvent::Finished {
                        index: 4,
                        delivered: rate as usize / 100
                    },
                    OutputEvent::Started(5),
                    OutputEvent::Finished {
                        index: 5,
                        delivered: next_rate as usize / 10
                    },
                    OutputEvent::Underrun,
                ]
            );
            SegmentOutput::append(&mut output, burst(6, next_rate as usize / 5, 0)).unwrap();
            pull(&mut mixed, rate as usize / 100);
            let (replacement, mut replaced) = rodio::mixer::mixer(
                ChannelCount::new(2).unwrap(),
                SampleRate::new(48_000).unwrap(),
            );
            let prepared = PreparedStream::new(
                replacement,
                None,
                None,
                Arc::new(AtomicBool::new(false)),
                output.rate,
                MixerFormat {
                    rate: 48_000,
                    channels: 2,
                },
            );
            SegmentOutput::commit(&mut output, prepared);
            SegmentOutput::append(&mut output, burst(7, next_rate as usize / 10, 0)).unwrap();
            output.set_sample_rate(rate).unwrap();
            SegmentOutput::append(&mut output, burst(8, rate as usize / 100, 0)).unwrap();
            let heard = pull(&mut replaced, (5_280 + 8) * 2);
            let frames = heard.iter().rposition(|sample| *sample != 0.0).unwrap() / 2 + 1;
            assert!(frames.abs_diff(5_280) <= 3);
            assert_eq!(
                SegmentOutput::events(&mut output),
                [
                    OutputEvent::Started(7),
                    OutputEvent::Finished {
                        index: 7,
                        delivered: next_rate as usize / 10
                    },
                    OutputEvent::Started(8),
                    OutputEvent::Finished {
                        index: 8,
                        delivered: rate as usize / 100
                    },
                    OutputEvent::Underrun,
                ]
            );
        }
    }

    #[test]
    fn production_model_rates_preserve_duration_after_stop_and_replacement() {
        for rate in [24_000, 44_100] {
            let (mixer, mut mixed) = rodio::mixer::mixer(
                ChannelCount::new(1).unwrap(),
                SampleRate::new(rate).unwrap(),
            );
            let mut output = RodioOutput::new(rate).unwrap();
            output.attach_mixer(&mixer).unwrap();
            for index in 0..2 {
                let samples = rate as usize / 4;
                SegmentOutput::append(&mut output, burst(index, samples, 0)).unwrap();
                let heard = pull(&mut mixed, samples);
                assert_eq!(
                    heard.iter().filter(|sample| **sample != 0.0).count(),
                    samples
                );
                assert_eq!(
                    SegmentOutput::events(&mut output),
                    [
                        OutputEvent::Started(index),
                        OutputEvent::Finished {
                            index,
                            delivered: samples
                        },
                    ]
                );
                assert_eq!(heard.len() as f64 / f64::from(rate), 0.25);
                SegmentOutput::stop(&mut output);
            }
            let (replacement, mut replaced) = rodio::mixer::mixer(
                ChannelCount::new(1).unwrap(),
                SampleRate::new(rate).unwrap(),
            );
            let prepared = output.prepare_mixer(replacement, None);
            SegmentOutput::commit(&mut output, prepared);
            let samples = rate as usize / 4;
            SegmentOutput::append(&mut output, burst(2, samples, 0)).unwrap();
            let heard = pull(&mut replaced, samples);
            assert_eq!(
                heard.iter().filter(|sample| **sample != 0.0).count(),
                samples
            );
            assert_eq!(heard.len() as f64 / f64::from(rate), 0.25);
            assert_eq!(
                SegmentOutput::events(&mut output),
                [
                    OutputEvent::Started(2),
                    OutputEvent::Finished {
                        index: 2,
                        delivered: samples
                    },
                ]
            );
        }
        assert!(RodioOutput::new(0).is_err());
        assert!(SegmentOutput::append(&mut RodioOutput::default(), burst(0, 10, 0)).is_err());
    }

    #[test]
    fn queued_rate_changes_keep_native_frames_duration_pitch_and_order() {
        for rates in [[44_100, 24_000, 44_100], [24_000, 44_100, 24_000]] {
            let target = rates[0];
            let (mixer, mut mixed) = rodio::mixer::mixer(
                ChannelCount::new(1).unwrap(),
                SampleRate::new(target).unwrap(),
            );
            let mut output = RodioOutput::new(target).unwrap();
            output.attach_mixer(&mixer).unwrap();
            let identity = output.sink.as_ref().unwrap().clone();
            let mut heard = Vec::new();
            for (index, rate) in rates.into_iter().enumerate() {
                output.set_sample_rate(rate).unwrap();
                let speech = (0..rate / 4)
                    .map(|n| {
                        (std::f32::consts::TAU * 200.0 * (n as f32 + 0.25) / rate as f32).sin()
                            * 0.25
                    })
                    .collect();
                SegmentOutput::append(
                    &mut output,
                    Segment {
                        epoch: 4,
                        index,
                        speech,
                        pause: 0,
                    },
                )
                .unwrap();
                if index == 0 {
                    heard.extend(pull(&mut mixed, target as usize / 8));
                }
            }
            assert!(Arc::ptr_eq(output.sink.as_ref().unwrap(), &identity));
            let expected_frames = target as usize * 3 / 4;
            heard.extend(pull(&mut mixed, expected_frames + 8 - heard.len()));
            let events = SegmentOutput::events(&mut output);
            let mut expected = Vec::new();
            for (index, rate) in rates.into_iter().enumerate() {
                expected.push(OutputEvent::Started(index));
                expected.push(OutputEvent::Finished {
                    index,
                    delivered: rate as usize / 4,
                });
            }
            expected.push(OutputEvent::Underrun);
            assert_eq!(events, expected);
            let delivered_frames = heard.iter().rposition(|sample| *sample != 0.0).unwrap() + 1;
            assert!(
                delivered_frames.abs_diff(expected_frames) <= 3,
                "native rates must preserve three quarter-second durations"
            );
            let crossings = heard[..delivered_frames]
                .windows(2)
                .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
                .count();
            assert!(
                crossings.abs_diff(150) <= 3,
                "resampling must preserve the 200 Hz pitch"
            );
            assert!(
                heard[..delivered_frames]
                    .iter()
                    .all(|sample| *sample != 0.0),
                "rate changes cannot insert silent gaps"
            );
        }
    }

    #[test]
    fn stop_after_model_rate_change_uses_the_new_rate_for_next_play() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(44_100).unwrap(),
        );
        let mut output = RodioOutput::new(44_100).unwrap();
        output.attach_mixer(&mixer).unwrap();
        SegmentOutput::append(&mut output, burst(0, 11_025, 0)).unwrap();
        pull(&mut mixed, 5_512);
        output.set_sample_rate(24_000).unwrap();
        SegmentOutput::append(&mut output, burst(1, 6_000, 0)).unwrap();
        SegmentOutput::stop(&mut output);
        SegmentOutput::append(&mut output, burst(2, 6_000, 0)).unwrap();
        let heard = pull(&mut mixed, 11_033);
        let first = heard.iter().position(|sample| *sample != 0.0).unwrap();
        let last = heard.iter().rposition(|sample| *sample != 0.0).unwrap();
        assert!((last + 1 - first).abs_diff(11_025) <= 2);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Started(2),
                OutputEvent::Finished {
                    index: 2,
                    delivered: 6_000
                },
                OutputEvent::Underrun,
            ]
        );
        assert!(output.set_sample_rate(0).is_err());
    }

    #[test]
    fn rodio_feed_plays_owner_fixture_chunks_once_in_order_at_2x() {
        let adapter = include_str!("playback.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(
            !adapter.contains(concat!("sink.", "clear(")),
            "normal append cannot clear the persistent sink"
        );
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(44_100).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 44_100);
        let lengths = [125_358, 81_681, 180_109];
        let pauses = [0, 5_513, 5_513];
        let mut expected = Vec::new();
        let mut expected_events = Vec::new();
        for (index, (&speech, &pause)) in lengths.iter().zip(&pauses).enumerate() {
            let segment = burst(index, speech, pause);
            expected.extend_from_slice(&segment.speech);
            expected.resize(expected.len() + pause, 0.0);
            SegmentOutput::append(&mut output, segment).unwrap();
            expected_events.push(OutputEvent::Started(index));
            expected_events.push(OutputEvent::Finished {
                index,
                delivered: speech + pause,
            });
        }
        assert!(SegmentOutput::events(&mut output).is_empty());
        let total = expected.len() + 22_050;
        let mut heard = Vec::new();
        let mut events = Vec::new();
        while heard.len() < total {
            let count = 1_024.min(total - heard.len());
            heard.extend(pull(&mut mixed, count));
            let next = SegmentOutput::events(&mut output);
            for event in &next {
                if let OutputEvent::Finished { index, .. } = event {
                    assert!(
                        heard.len()
                            >= lengths[..=*index].iter().sum::<usize>()
                                + pauses[..=*index].iter().sum::<usize>()
                    );
                }
            }
            events.extend(next);
        }
        expected_events.push(OutputEvent::Underrun);
        assert_eq!(events, expected_events);
        assert!(
            heard[..expected.len()] == expected,
            "delivered mixer samples must match every scheduled speech and pause sample"
        );
        assert!(heard[expected.len()..].iter().all(|sample| *sample == 0.0));
        assert_eq!(
            heard.iter().filter(|sample| **sample != 0.0).count(),
            lengths.iter().sum::<usize>()
        );
        assert_eq!(
            SegmentOutput::content_samples(&output),
            expected.len() as u64
        );
        assert_eq!(
            output
                .feed
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .underrun_samples(),
            22_050
        );
        assert_eq!(output.finished_logs.len(), 3);
        for (index, line) in output.finished_logs.iter().enumerate() {
            let samples = lengths[index] + pauses[index];
            assert_eq!(line, &format!("tts chunk finished index={index} scheduled={samples} delivered={samples} underrun=0"));
        }
        assert!(SegmentOutput::events(&mut output).is_empty());
        assert!(!output.sink.as_ref().unwrap().empty());
    }

    #[test]
    fn appending_during_playback_has_no_gap() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        let first = burst(0, 4_800, 240);
        let second = burst(1, 7_200, 120);
        SegmentOutput::append(&mut output, first.clone()).unwrap();
        let mut heard = pull(&mut mixed, 2_400);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [OutputEvent::Started(0)]
        );
        SegmentOutput::append(&mut output, second.clone()).unwrap();
        heard.extend(pull(
            &mut mixed,
            first.samples() + second.samples() - heard.len(),
        ));
        let mut expected = first.speech;
        expected.resize(expected.len() + first.pause, 0.0);
        expected.extend(second.speech);
        expected.resize(expected.len() + second.pause, 0.0);
        assert!(
            heard == expected,
            "append must preserve every sample without an extra gap"
        );
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Finished {
                    index: 0,
                    delivered: 5_040
                },
                OutputEvent::Started(1),
                OutputEvent::Finished {
                    index: 1,
                    delivered: 7_320
                },
            ]
        );
    }

    #[test]
    fn stop_clears_feed_without_replacing_sink() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        let identity = output.sink.as_ref().unwrap().clone();
        SegmentOutput::append(&mut output, burst(0, 4_800, 240)).unwrap();
        SegmentOutput::append(&mut output, burst(1, 7_200, 120)).unwrap();
        pull(&mut mixed, 1_337);
        SegmentOutput::stop(&mut output);
        assert!(Arc::ptr_eq(output.sink.as_ref().unwrap(), &identity));
        assert!(SegmentOutput::events(&mut output).is_empty());
        assert_eq!(SegmentOutput::content_samples(&output), 0);
        let next = burst(2, 1_200, 0);
        SegmentOutput::append(&mut output, next.clone()).unwrap();
        assert_eq!(pull(&mut mixed, next.samples()), next.speech);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Started(2),
                OutputEvent::Finished {
                    index: 2,
                    delivered: 1_200
                },
            ]
        );
    }

    #[test]
    fn stop_from_paused_resets_sink_for_audible_next_play() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        SegmentOutput::append(&mut output, burst(0, 4_800, 0)).unwrap();
        SegmentOutput::pause(&mut output);
        pull(&mut mixed, 240);
        SegmentOutput::stop(&mut output);
        let sink = output.sink.as_ref().unwrap();
        assert!(!sink.is_paused());
        let next = burst(1, 240, 0);
        SegmentOutput::append(&mut output, next.clone()).unwrap();
        let heard = pull(&mut mixed, next.samples() + 120);
        let speech: Vec<_> = heard.into_iter().filter(|sample| *sample != 0.0).collect();
        assert!(
            speech == next.speech,
            "Stop-from-Paused must deliver the entire next burst"
        );
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Started(1),
                OutputEvent::Finished {
                    index: 1,
                    delivered: 240
                },
                OutputEvent::Underrun,
            ]
        );
    }

    #[test]
    fn pause_and_retract_preserve_the_current_sample_and_completion_clock() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        let first = burst(0, 4_800, 240);
        let second = burst(1, 240, 120);
        SegmentOutput::append(&mut output, first.clone()).unwrap();
        SegmentOutput::append(&mut output, second.clone()).unwrap();
        let mut heard = pull(&mut mixed, 1_337);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [OutputEvent::Started(0)]
        );
        SegmentOutput::pause(&mut output);
        assert!(pull(&mut mixed, 240).iter().all(|sample| *sample == 0.0));
        assert_eq!(SegmentOutput::content_samples(&output), 1_337);
        assert!(SegmentOutput::events(&mut output).is_empty());
        assert_eq!(SegmentOutput::retract_unstarted(&mut output), [second]);
        SegmentOutput::resume(&mut output);
        let tail = pull(&mut mixed, first.samples() - heard.len() + 120);
        heard.extend(tail.into_iter().filter(|sample| *sample != 0.0));
        assert!(
            heard == first.speech,
            "pause and retract cannot skip current speech"
        );
        assert_eq!(
            SegmentOutput::content_samples(&output),
            first.samples() as u64
        );
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Finished {
                    index: 0,
                    delivered: first.samples()
                },
                OutputEvent::Underrun,
            ]
        );
        assert_eq!(output.finished_logs.len(), 1);
    }

    #[test]
    fn segment_finishes_only_on_the_last_pulled_pause_sample() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        SegmentOutput::append(
            &mut output,
            Segment {
                epoch: 0,
                index: 7,
                speech: vec![0.25; 240],
                pause: 240,
            },
        )
        .unwrap();
        assert!(SegmentOutput::events(&mut output).is_empty());
        pull(&mut mixed, 479);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [OutputEvent::Started(7)]
        );
        assert_eq!(SegmentOutput::content_samples(&output), 479);
        assert!(output.finished_logs.is_empty());
        pull(&mut mixed, 1);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [OutputEvent::Finished {
                index: 7,
                delivered: 480
            }]
        );
        assert_eq!(
            output.finished_logs,
            ["tts chunk finished index=7 scheduled=480 delivered=480 underrun=0"]
        );
        assert!(SegmentOutput::events(&mut output).is_empty());
    }

    #[test]
    fn device_loss_follows_already_pulled_events_and_is_reported_once() {
        let (mixer, mut mixed) = rodio::mixer::mixer(
            ChannelCount::new(1).unwrap(),
            SampleRate::new(24_000).unwrap(),
        );
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        SegmentOutput::append(&mut output, burst(0, 240, 0)).unwrap();
        pull(&mut mixed, 240);
        output.lost.store(true, Ordering::Release);
        assert_eq!(
            SegmentOutput::events(&mut output),
            [
                OutputEvent::Started(0),
                OutputEvent::Finished {
                    index: 0,
                    delivered: 240
                },
                OutputEvent::DeviceLost,
            ]
        );
        assert!(SegmentOutput::events(&mut output).is_empty());
    }
}
