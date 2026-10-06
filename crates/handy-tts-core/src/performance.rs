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
    queue.play();
    let mut now = 0.0;
    let mut generation: Option<(Ticket, f64)> = None;
    let mut playback: Option<(Ticket, f64)> = None;
    let mut first_start = None;
    let mut buffering_started = None;
    let mut buffering_seconds = 0.0;
    let mut queue_peak = 0.0f64;
    let mut pending = None;
    loop {
        if let Some((ticket, audio, elapsed)) = pending.take()
            && let Some(audio) = queue.complete_timed(ticket, Ok(audio), elapsed)
        {
            pending = Some((ticket, audio, elapsed));
        }
        if generation.is_none() && pending.is_none() {
            if queue.next_index == timings.len() {
                queue.source_finished = true;
                queue.advance();
            } else if let Some(ticket) = queue.request_next() {
                generation = Some((ticket, now + timings[ticket.index].synthesis_seconds));
            }
        }
        if let Some(audio) = &queue.playing
            && playback.is_none()
        {
            playback = Some((audio.ticket, now + audio.seconds()));
            first_start.get_or_insert(now);
            if let Some(start) = buffering_started.take() {
                buffering_seconds += now - start;
            }
        }
        queue_peak = queue_peak.max(queue.queued_seconds());
        if queue.state == PlaybackState::Finished {
            break;
        }
        let next = match (generation, playback) {
            (Some((_, a)), Some((_, b))) => a.min(b),
            (Some((_, a)), None) | (None, Some((_, a))) => a,
            (None, None) => return Err(PerformanceError::Stalled),
        };
        now = next;
        if playback.is_some_and(|(_, end)| end <= now + 1e-9) {
            playback = None;
            queue.playback_finished();
            if queue.state == PlaybackState::Buffering {
                buffering_started = Some(now);
            }
        }
        if let Some((ticket, end)) = generation
            && end <= now + 1e-9
        {
            generation = None;
            let Timing {
                synthesis_seconds: elapsed,
                audio_seconds: _,
                samples,
                sample_rate,
            } = timings[ticket.index];
            let audio = AudioChunk {
                pause_ms: 0,
                ticket,
                pcm: Pcm {
                    id: ticket.index as u64,
                    sample_rate,
                    samples: vec![0.0; samples],
                },
                source_start: ticket.index as u32,
                source_end: ticket.index as u32 + 1,
            };
            if let Some(audio) = queue.complete_timed(ticket, Ok(audio), elapsed) {
                pending = Some((ticket, audio, elapsed));
            }
        }
    }
    Ok(Replay {
        chunks: timings.len(),
        speed,
        head_start_wait_seconds: first_start.unwrap_or(0.0),
        buffering_pauses: queue.buffering_pauses,
        buffering_seconds,
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
