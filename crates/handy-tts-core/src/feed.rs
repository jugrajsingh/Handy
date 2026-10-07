use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// One chunk of speech followed by a pause, measured at the played sample rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub epoch: u64,
    pub index: usize,
    pub speech: Vec<f32>,
    pub pause: usize,
}

impl Segment {
    /// Counts both speech and its trailing pause.
    pub fn samples(&self) -> usize {
        self.speech.len().saturating_add(self.pause)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedEvent {
    Started(usize),
    Finished { index: usize, delivered: usize },
    Underrun,
}

struct Current {
    segment: Segment,
    offset: usize,
}

/// Sample-counting source independent of an audio device or mixer.
pub struct Feed {
    sample_rate: u32,
    epoch: Option<u64>,
    queue: VecDeque<Segment>,
    current: Option<Current>,
    events: Vec<FeedEvent>,
    content_samples: u64,
    underrun_samples: u64,
    dry: bool,
}

impl Feed {
    /// Creates an empty feed; the first admitted segment establishes its epoch.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            epoch: None,
            queue: VecDeque::new(),
            current: None,
            events: Vec::new(),
            content_samples: 0,
            underrun_samples: 0,
            dry: false,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Admits nonempty segments of the current epoch in insertion order.
    pub fn push(&mut self, segment: Segment) {
        if self.epoch.is_some_and(|epoch| epoch != segment.epoch) || segment.samples() == 0 {
            return;
        }
        self.epoch = Some(segment.epoch);
        self.queue.push_back(segment);
    }

    /// Fills every output sample; missing content is silence outside the content clock.
    pub fn pull(&mut self, out: &mut [f32]) {
        let mut written = 0;
        while written < out.len() {
            if self.current.is_none() {
                match self.queue.pop_front() {
                    Some(segment) => {
                        self.events.push(FeedEvent::Started(segment.index));
                        self.current = Some(Current { segment, offset: 0 });
                    }
                    None => {
                        out[written..].fill(0.0);
                        self.underrun_samples += (out.len() - written) as u64;
                        if !self.dry {
                            self.events.push(FeedEvent::Underrun);
                            self.dry = true;
                        }
                        break;
                    }
                }
            }

            if let Some(current) = &mut self.current {
                let speech = &current.segment.speech;
                let total = current.segment.samples();
                let count = (total - current.offset).min(out.len() - written);
                let speech_count = speech.len().saturating_sub(current.offset).min(count);
                if speech_count > 0 {
                    out[written..written + speech_count]
                        .copy_from_slice(&speech[current.offset..current.offset + speech_count]);
                }
                out[written + speech_count..written + count].fill(0.0);
                current.offset += count;
                written += count;
                self.content_samples += count as u64;
                self.dry = false;

                if current.offset == total {
                    self.events.push(FeedEvent::Finished {
                        index: current.segment.index,
                        delivered: current.offset,
                    });
                    self.current = None;
                }
            }
        }
    }

    /// Takes each pending sample event exactly once.
    pub fn drain_events(&mut self) -> Vec<FeedEvent> {
        std::mem::take(&mut self.events)
    }

    /// Returns queued segments in order, retaining the current segment and its offset.
    pub fn retract_unstarted(&mut self) -> Vec<Segment> {
        self.queue.drain(..).collect()
    }

    /// Clears audio, events and clocks, and admits only the given epoch afterwards.
    pub fn clear(&mut self, epoch: u64) {
        self.epoch = Some(epoch);
        self.queue.clear();
        self.current = None;
        self.events.clear();
        self.content_samples = 0;
        self.underrun_samples = 0;
        self.dry = false;
    }

    pub fn content_samples(&self) -> u64 {
        self.content_samples
    }

    pub fn underrun_samples(&self) -> u64 {
        self.underrun_samples
    }

    /// Counts samples in unstarted segments, excluding the current segment.
    pub fn appended_ahead_samples(&self) -> usize {
        self.queue.iter().map(Segment::samples).sum()
    }

    pub fn unstarted_len(&self) -> usize {
        self.queue.len()
    }
}

pub type SharedFeed = Arc<Mutex<Feed>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(index: usize, speech: usize, pause: usize) -> Segment {
        Segment {
            epoch: 4,
            index,
            speech: vec![index as f32 + 1.0; speech],
            pause,
        }
    }

    #[test]
    fn three_short_chunks_at_2x_start_and_finish_once_in_order() {
        let mut feed = Feed::new(44_100);
        let speech_lengths = [125_358, 81_681, 180_109];
        let pauses = [0, 5_513, 5_513];
        let mut expected = Vec::new();
        let mut expected_events = Vec::new();
        for (index, (&speech, &pause)) in speech_lengths.iter().zip(&pauses).enumerate() {
            let chunk = segment(index, speech, pause);
            expected.extend_from_slice(&chunk.speech);
            expected.resize(expected.len() + pause, 0.0);
            feed.push(chunk);
            expected_events.push(FeedEvent::Started(index));
            expected_events.push(FeedEvent::Finished {
                index,
                delivered: speech + pause,
            });
        }
        assert_eq!(feed.sample_rate(), 44_100);
        assert_eq!(feed.unstarted_len(), 3);
        assert_eq!(feed.appended_ahead_samples(), expected.len());
        let mut delivered = Vec::new();
        let mut events = Vec::new();
        while delivered.len() < expected.len() {
            let count = 1_024.min(expected.len() - delivered.len());
            let mut block = vec![f32::NAN; count];
            feed.pull(&mut block);
            delivered.extend(block);
            events.extend(feed.drain_events());
        }
        assert_eq!(events, expected_events);
        assert_eq!(delivered, expected);
        assert_eq!(feed.content_samples(), expected.len() as u64);
        assert_eq!(feed.underrun_samples(), 0);
        assert_eq!(feed.unstarted_len(), 0);
        assert_eq!(feed.appended_ahead_samples(), 0);
    }

    #[test]
    fn underrun_emits_silence_once_and_does_not_advance_content() {
        let mut feed = Feed::new(24_000);
        let mut block = [f32::NAN; 1_024];
        feed.pull(&mut block);
        assert_eq!(block, [0.0; 1_024]);
        assert_eq!(feed.drain_events(), [FeedEvent::Underrun]);
        feed.pull(&mut block);
        assert!(feed.drain_events().is_empty());
        assert_eq!(feed.content_samples(), 0);
        assert_eq!(feed.underrun_samples(), 2_048);

        feed.push(segment(0, 4, 2));
        feed.pull(&mut block);
        assert_eq!(&block[..6], &[1.0, 1.0, 1.0, 1.0, 0.0, 0.0]);
        assert!(block[6..].iter().all(|sample| *sample == 0.0));
        assert_eq!(feed.content_samples(), 6);
        assert_eq!(feed.underrun_samples(), 3_066);
        assert_eq!(
            feed.drain_events(),
            [
                FeedEvent::Started(0),
                FeedEvent::Finished {
                    index: 0,
                    delivered: 6,
                },
                FeedEvent::Underrun,
            ]
        );
        feed.pull(&mut block);
        assert!(feed.drain_events().is_empty());
        assert_eq!(feed.content_samples(), 6);
        assert_eq!(feed.underrun_samples(), 4_090);
    }

    #[test]
    fn retract_keeps_the_segment_being_pulled() {
        let mut feed = Feed::new(24_000);
        feed.push(segment(0, 1_500, 20));
        feed.push(segment(1, 200, 10));
        feed.push(segment(2, 300, 5));
        feed.pull(&mut [0.0; 1_024]);
        assert_eq!(feed.drain_events(), [FeedEvent::Started(0)]);
        assert_eq!(feed.unstarted_len(), 2);
        assert_eq!(feed.appended_ahead_samples(), 515);
        let retracted = feed.retract_unstarted();
        assert_eq!(
            retracted
                .iter()
                .map(|segment| segment.index)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(retracted, [segment(1, 200, 10), segment(2, 300, 5)]);
        assert_eq!(feed.unstarted_len(), 0);
        assert_eq!(feed.appended_ahead_samples(), 0);
        let mut tail = [f32::NAN; 496];
        feed.pull(&mut tail);
        assert!(tail[..476].iter().all(|sample| *sample == 1.0));
        assert_eq!(&tail[476..], &[0.0; 20]);
        assert_eq!(
            feed.drain_events(),
            [FeedEvent::Finished {
                index: 0,
                delivered: 1_520,
            }]
        );
        assert_eq!(feed.content_samples(), 1_520);
        assert_eq!(feed.underrun_samples(), 0);
    }

    #[test]
    fn clear_discards_old_epoch_events() {
        let mut feed = Feed::new(24_000);
        feed.push(segment(0, 2_000, 100));
        feed.push(segment(1, 200, 10));
        feed.pull(&mut [0.0; 1_024]);
        feed.clear(5);
        assert!(feed.drain_events().is_empty());
        assert_eq!(feed.content_samples(), 0);
        assert_eq!(feed.underrun_samples(), 0);
        assert_eq!(feed.appended_ahead_samples(), 0);
        feed.push(segment(2, 100, 10));
        assert_eq!(feed.unstarted_len(), 0);
        let mut next = segment(3, 1_024, 0);
        next.epoch = 5;
        feed.push(next);
        let mut block = [f32::NAN; 1_024];
        feed.pull(&mut block);
        assert_eq!(block, [4.0; 1_024]);
        assert_eq!(
            feed.drain_events(),
            [
                FeedEvent::Started(3),
                FeedEvent::Finished {
                    index: 3,
                    delivered: 1_024,
                },
            ]
        );
    }

    #[test]
    fn finished_is_reported_only_after_last_sample_pulled() {
        let mut feed = Feed::new(24_000);
        feed.push(segment(7, 1_290, 10));
        feed.pull(&mut [0.0; 1_024]);
        assert_eq!(feed.drain_events(), [FeedEvent::Started(7)]);
        feed.pull(&mut [0.0; 275]);
        assert!(feed.drain_events().is_empty());
        assert_eq!(feed.content_samples(), 1_299);
        let mut last = [f32::NAN];
        feed.pull(&mut last);
        assert_eq!(last, [0.0]);
        assert_eq!(
            feed.drain_events(),
            [FeedEvent::Finished {
                index: 7,
                delivered: 1_300,
            }]
        );
        assert_eq!(feed.content_samples(), 1_300);
        assert!(feed.drain_events().is_empty());
    }

    #[test]
    fn empty_pull_does_not_start_or_finish_a_segment() {
        let mut feed = Feed::new(24_000);
        feed.push(segment(0, 1, 0));
        feed.pull(&mut []);
        assert!(feed.drain_events().is_empty());
        assert_eq!(feed.unstarted_len(), 1);
        assert_eq!(feed.content_samples(), 0);
        assert_eq!(feed.underrun_samples(), 0);
        assert_eq!(feed.retract_unstarted(), [segment(0, 1, 0)]);
    }

    #[test]
    fn pause_only_segment_delivers_every_sample() {
        let mut feed = Feed::new(24_000);
        feed.push(segment(0, 0, 1_024));
        let mut block = [f32::NAN; 1_024];
        feed.pull(&mut block);
        assert_eq!(block, [0.0; 1_024]);
        assert_eq!(feed.content_samples(), 1_024);
        assert_eq!(feed.underrun_samples(), 0);
        assert_eq!(
            feed.drain_events(),
            [
                FeedEvent::Started(0),
                FeedEvent::Finished {
                    index: 0,
                    delivered: 1_024,
                },
            ]
        );
    }
}
