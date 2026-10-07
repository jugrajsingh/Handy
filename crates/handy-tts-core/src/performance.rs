use crate::{
    protocol::Pcm,
    scheduler::{AudioChunk, PlaybackState, Queue, Ticket},
};
/// Measured synthesis and DSP time with the shortened PCM duration.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub synthesis_seconds: f64,
    pub audio_seconds: f64,
    pub samples: usize,
    pub sample_rate: u32,
}
/// Continuity and queue measurements from scheduler replay.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Replay {
    pub chunks: usize,
    pub speed: f32,
    pub head_start_wait_seconds: f64,
    pub buffering_pauses: usize,
    pub buffering_seconds: f64,
    pub queued_audio_high_water_seconds: f64,
    pub wall_seconds: f64,
}
/// Invalid measurements or a replay that cannot make progress.
#[derive(Debug, thiserror::Error)]
pub enum PerformanceError {
    #[error("no valid measured chunks")]
    Invalid,
    #[error("scheduler replay stalled")]
    Stalled,
}
/// Replays measured chunk timings through the production queue.
pub fn replay(timings: &[Timing], speed: f32) -> Result<Replay, PerformanceError> {
    replay_with_lanes(timings, speed, 1)
}
/// Replays concurrent synthesis completions through the production queue.
pub fn replay_with_lanes(
    timings: &[Timing],
    speed: f32,
    lanes: usize,
) -> Result<Replay, PerformanceError> {
    if !(1..=3).contains(&lanes) {
        return Err(PerformanceError::Invalid);
    }
    if timings.is_empty()
        || !(0.5..=2.0).contains(&speed)
        || timings.iter().any(|t| {
            !t.synthesis_seconds.is_finite()
                || t.synthesis_seconds < 0.0
                || !t.audio_seconds.is_finite()
                || t.audio_seconds <= 0.0
                || t.samples == 0
                || !(8000..=48000).contains(&t.sample_rate)
        })
    {
        return Err(PerformanceError::Invalid);
    }
    let mut queue = Queue::with_speed(speed);
    queue.set_lanes(lanes);
    queue.play();
    let mut now = 0.0;
    let mut generation = std::collections::BTreeMap::<usize, (Ticket, f64)>::new();
    let mut playback: Option<(Ticket, f64)> = None;
    let mut first_start = None;
    let mut queue_peak = 0.0f64;
    let mut pending = std::collections::BTreeMap::<usize, (Ticket, AudioChunk, f64)>::new();
    loop {
        queue.advance_at(now);
        for index in pending.keys().copied().collect::<Vec<_>>() {
            if let Some((ticket, audio, elapsed)) = pending.remove(&index) {
                let frames = audio.pcm.samples.len();
                if let Some(audio) = queue.complete_timed(ticket, Ok(audio), elapsed) {
                    let elapsed = elapsed * audio.pcm.samples.len() as f64 / frames as f64;
                    pending.insert(index, (ticket, audio, elapsed));
                }
            }
        }
        while queue.next_index < timings.len() {
            let Some(ticket) = queue.request_next() else {
                break;
            };
            generation.insert(
                ticket.index,
                (ticket, now + timings[ticket.index].synthesis_seconds),
            );
        }
        if queue.next_index == timings.len() {
            queue.source_finished = true;
            queue.advance();
        }
        if let Some(audio) = &queue.playing
            && playback.is_none()
        {
            playback = Some((audio.ticket, now + audio.seconds()));
            first_start.get_or_insert(now);
        }
        queue_peak = queue_peak.max(queue.queued_seconds());
        if queue.state == PlaybackState::Finished {
            break;
        }
        if queue.state == PlaybackState::Error {
            return Err(PerformanceError::Invalid);
        }
        let next = generation
            .values()
            .map(|(_, end)| *end)
            .chain(playback.iter().map(|(_, end)| *end))
            .min_by(f64::total_cmp)
            .ok_or(PerformanceError::Stalled)?;
        now = next;
        queue.advance_at(now);
        let completed: Vec<_> = generation
            .iter()
            .filter(|(_, (_, end))| *end <= now + 1e-9)
            .map(|(index, _)| *index)
            .collect();
        for index in completed {
            let Some((ticket, _)) = generation.remove(&index) else {
                continue;
            };
            let Timing {
                synthesis_seconds: elapsed,
                samples,
                sample_rate,
                ..
            } = timings[index];
            let audio = AudioChunk {
                pause_ms: 0,
                ticket,
                pcm: Pcm {
                    id: index as u64,
                    sample_rate,
                    samples: vec![0.0; samples],
                },
                native: vec![0.0; (samples as f64 * f64::from(speed)).round() as usize],
                plan: crate::speed::SpeedPlan {
                    requested: speed,
                    native: 1.0,
                    stretch: speed,
                },
                source_start: index as u32,
                source_end: index as u32 + 1,
            };
            let frames = audio.pcm.samples.len();
            if let Some(audio) = queue.complete_timed(ticket, Ok(audio), elapsed) {
                let elapsed = elapsed * audio.pcm.samples.len() as f64 / frames as f64;
                pending.insert(index, (ticket, audio, elapsed));
            }
        }
        if playback.is_some_and(|(_, end)| end <= now + 1e-9) {
            playback = None;
            queue.playback_finished();
        }
    }
    Ok(Replay {
        chunks: timings.len(),
        speed,
        head_start_wait_seconds: first_start.unwrap_or(0.0),
        buffering_pauses: queue.buffering_pauses,
        buffering_seconds: queue.buffering_seconds,
        queued_audio_high_water_seconds: queue_peak,
        wall_seconds: now,
    })
}
/// Applies the controller continuity and throughput gates.
pub fn gate(
    model: &str,
    device: crate::device::Device,
    recommended: crate::device::Device,
    median_rtf: f64,
    words: usize,
    pauses: usize,
) -> bool {
    if !median_rtf.is_finite() || median_rtf < 0.0 {
        return false;
    }
    match model {
        "pocket-en" => words >= 2000 && pauses == 0,
        "supertonic-3" | "kokoro-82m-int8" => device == recommended && median_rtf <= 0.33,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Device;
    #[test]
    fn controller_gates_pocket_continuity_and_other_engines_on_the_recommended_device() {
        assert!(gate("pocket-en", Device::Cpu, Device::Cpu, 1.4, 2000, 0));
        assert!(!gate("pocket-en", Device::Cpu, Device::Cpu, 0.2, 1999, 0));
        assert!(!gate("pocket-en", Device::Cpu, Device::Cpu, 0.2, 2000, 1));
        for model in ["kokoro-82m-int8", "supertonic-3"] {
            assert!(gate(model, Device::Npu, Device::Npu, 0.33, 0, 0));
            assert!(!gate(model, Device::Cpu, Device::Npu, 0.1, 0, 0));
            assert!(!gate(model, Device::Npu, Device::Npu, 0.331, 0, 0));
        }
    }
    #[test]
    fn parallel_replay_waits_for_the_prefix_and_delivers_every_chunk() {
        let timings: Vec<_> = [3.0, 0.1, 0.2]
            .into_iter()
            .map(|synthesis_seconds| Timing {
                synthesis_seconds,
                audio_seconds: 1.0,
                samples: 8000,
                sample_rate: 8000,
            })
            .collect();
        let result = replay_with_lanes(&timings, 2.0, 3).unwrap();
        assert_eq!(result.chunks, 3);
        assert_eq!(result.head_start_wait_seconds, 3.0);
        assert_eq!(result.wall_seconds, 6.0);
        assert_eq!(result.buffering_pauses, 0);
        assert_eq!(result.buffering_seconds, 0.0);
    }
    #[test]
    fn replay_delivers_a_capacity_split_without_losing_played_duration() {
        let timings: Vec<_> = [1, 1, 30]
            .into_iter()
            .map(|seconds| Timing {
                synthesis_seconds: f64::from(seconds),
                audio_seconds: f64::from(seconds),
                samples: seconds as usize * 8000,
                sample_rate: 8000,
            })
            .collect();
        let result = replay(&timings, 1.0).unwrap();
        assert_eq!(result.chunks, 3);
        assert_eq!(result.head_start_wait_seconds, 32.0);
        assert_eq!(result.wall_seconds, 64.0);
        assert!(result.queued_audio_high_water_seconds <= 30.0);
    }
    #[test]
    fn measured_timings_replay_adaptive_scheduler_and_slow_producer_refuses_continuity() {
        for (rtf, pauses) in [(0.49, false), (1.4, true)] {
            let timings = vec![
                Timing {
                    synthesis_seconds: rtf,
                    audio_seconds: 1.0,
                    samples: 8000,
                    sample_rate: 8000
                };
                100
            ];
            let out = replay(&timings, 2.0).unwrap();
            assert_eq!(out.buffering_pauses > 0, pauses);
            assert!(out.queued_audio_high_water_seconds <= 30.00001);
        }
    }
}
