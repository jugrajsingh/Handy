use cpal::traits::{DeviceTrait, HostTrait};
use handy_tts_core::{
    feed::{Feed, Segment, SharedFeed},
    playback::{
        segment::{Output, OutputEvent},
        Output as LegacyOutput, PlaybackError, Poll,
    },
    protocol::Pcm,
};
use rodio::{mixer::Mixer, OutputStream, OutputStreamBuilder, Sink, Source};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

struct FeedSource {
    feed: SharedFeed,
    rate: u32,
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
        feed.pull(&mut self.block[self.pos..self.pos + 1]);
        let sample = self.block[self.pos];
        self.pos = (self.pos + 1) % self.block.len();
        Some(sample)
    }
}
impl Source for FeedSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.block.len() - self.pos)
    }
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self) -> u32 {
        self.rate
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
    rate: u32,
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
            rate: 24_000,
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
    stream: OutputStream,
    sink: Arc<Sink>,
    feed: SharedFeed,
    lost: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

fn connect_feed(mixer: &Mixer, rate: u32, paused: Arc<AtomicBool>) -> (Arc<Sink>, SharedFeed) {
    let feed = Arc::new(Mutex::new(Feed::new(rate)));
    let (sink, mut source) = Sink::new();
    paused.store(true, Ordering::Release);
    sink.append(FeedSource {
        feed: feed.clone(),
        rate,
        block: vec![0.0; 1_024],
        pos: 0,
        paused: paused.clone(),
    });
    // Prime the queue's format before the mixer constructs its rate converter.
    let _ = source.next();
    mixer.add(source);
    paused.store(false, Ordering::Release);
    (Arc::new(sink), feed)
}

impl Output for RodioOutput {
    type Prepared = PreparedStream;
    fn names(&self) -> Result<Vec<String>, PlaybackError> {
        crate::audio_toolkit::get_cpal_host()
            .output_devices()
            .map_err(|_| PlaybackError::Unavailable)?
            .map(|d| d.name().map_err(|_| PlaybackError::Unavailable))
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
        self.sink = Some(prepared.sink);
        self.feed = Some(prepared.feed);
        self.stream = Some(prepared.stream);
        self.lost = prepared.lost;
        self.paused = prepared.paused;
    }
    fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
        if self.sink.is_none() {
            self.open_stream()?;
        }
        let feed = self.feed.as_ref().ok_or(PlaybackError::Unavailable)?;
        let mut feed = feed.lock().unwrap_or_else(|p| p.into_inner());
        let before = feed.unstarted_len();
        let index = segment.index;
        let samples = segment.samples();
        feed.push(segment);
        if feed.unstarted_len() > before {
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
        let segments = self
            .feed
            .as_ref()
            .map(|feed| {
                feed.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retract_unstarted()
            })
            .unwrap_or_default();
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
        if let Some(feed) = &self.feed {
            *feed.lock().unwrap_or_else(|p| p.into_inner()) = Feed::new(self.rate);
        }
        self.scheduled.clear();
        Output::resume(self);
    }
}

impl RodioOutput {
    #[cfg(test)]
    fn with_mixer(mixer: &Mixer, rate: u32) -> Self {
        let mut output = Self {
            rate,
            ..Self::default()
        };
        let (sink, feed) = connect_feed(mixer, rate, output.paused.clone());
        output.sink = Some(sink);
        output.feed = Some(feed);
        output
    }
    fn open_stream(&mut self) -> Result<(), PlaybackError> {
        let prepared = Self::prepare_stream(self.selected.as_deref(), self.rate)?;
        Output::commit(self, prepared);
        Ok(())
    }
    fn prepare_stream(name: Option<&str>, rate: u32) -> Result<PreparedStream, PlaybackError> {
        let builder = if let Some(name) = name {
            let host = crate::audio_toolkit::get_cpal_host();
            let mut devices = host
                .output_devices()
                .map_err(|_| PlaybackError::Unavailable)?;
            let device = devices
                .find(|d| d.name().is_ok_and(|n| n == name))
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
        let paused = Arc::new(AtomicBool::new(false));
        let (sink, feed) = connect_feed(stream.mixer(), rate, paused.clone());
        Ok(PreparedStream {
            selected: name.map(str::to_owned),
            stream,
            sink,
            feed,
            lost,
            paused,
        })
    }
}

// The legacy session and preview migrate to segment playback in s9.
impl LegacyOutput for RodioOutput {
    type Prepared = PreparedStream;
    fn names(&self) -> Result<Vec<String>, PlaybackError> {
        Output::names(self)
    }
    fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
        if self.sink.is_none() {
            self.selected = name.map(str::to_owned);
            Ok(())
        } else {
            Output::open(self, name)
        }
    }
    fn prepare(&mut self, name: Option<&str>) -> Result<Self::Prepared, PlaybackError> {
        Output::prepare(self, name)
    }
    fn commit(&mut self, prepared: Self::Prepared) {
        Output::commit(self, prepared);
    }
    fn play(&mut self, pcm: Pcm, pause_ms: u32) -> Result<(), PlaybackError> {
        if self.rate != pcm.sample_rate {
            let prepared = Self::prepare_stream(self.selected.as_deref(), pcm.sample_rate)?;
            Output::commit(self, prepared);
            self.rate = pcm.sample_rate;
        }
        Output::append(
            self,
            Segment {
                epoch: 0,
                index: pcm.id as usize,
                pause: (u64::from(pcm.sample_rate) * u64::from(pause_ms) / 1000) as usize,
                speech: pcm.samples,
            },
        )
    }
    fn poll(&mut self) -> Poll {
        let events = Output::events(self);
        if events.contains(&OutputEvent::DeviceLost) {
            Poll::DeviceLost
        } else if events
            .iter()
            .any(|event| matches!(event, OutputEvent::Finished { .. }))
        {
            Poll::Finished
        } else {
            Poll::Playing
        }
    }
    fn pause(&mut self) {
        Output::pause(self);
    }
    fn resume(&mut self) {
        Output::resume(self);
    }
    fn stop(&mut self) {
        Output::stop(self);
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
    fn rodio_feed_plays_owner_fixture_chunks_once_in_order_at_2x() {
        let adapter = include_str!("playback.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(
            !adapter.contains(concat!("sink.", "clear(")),
            "normal append cannot clear the persistent sink"
        );
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 44_100);
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
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 24_000);
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
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 24_000);
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
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 24_000);
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
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 24_000);
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
    fn legacy_poll_finishes_only_on_the_last_pulled_pause_sample() {
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 24_000);
        let mut output = RodioOutput::with_mixer(&mixer, 24_000);
        LegacyOutput::play(
            &mut output,
            Pcm {
                id: 7,
                sample_rate: 24_000,
                samples: vec![0.25; 240],
            },
            10,
        )
        .unwrap();
        assert_eq!(LegacyOutput::poll(&mut output), Poll::Playing);
        pull(&mut mixed, 479);
        assert_eq!(LegacyOutput::poll(&mut output), Poll::Playing);
        assert_eq!(SegmentOutput::content_samples(&output), 479);
        assert!(output.finished_logs.is_empty());
        pull(&mut mixed, 1);
        assert_eq!(LegacyOutput::poll(&mut output), Poll::Finished);
        assert_eq!(
            output.finished_logs,
            ["tts chunk finished index=7 scheduled=480 delivered=480 underrun=0"]
        );
        assert_eq!(LegacyOutput::poll(&mut output), Poll::Playing);
    }

    #[test]
    fn device_loss_follows_already_pulled_events_and_is_reported_once() {
        let (mixer, mut mixed) = rodio::mixer::mixer(1, 24_000);
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
