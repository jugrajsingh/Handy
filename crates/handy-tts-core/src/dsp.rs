use crate::protocol::Pcm;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DspError {
    #[error("invalid audio controls")]
    Controls,
    #[error("invalid PCM")]
    Pcm,
    #[error("stretch failed")]
    Stretch,
}
pub fn transform(pcm: Pcm, speed: f32, pitch: f32, native_speed: bool) -> Result<Pcm, DspError> {
    if !(0.5..=2.0).contains(&speed)
        || !(-6.0..=6.0).contains(&pitch)
        || (speed * 10.0 - (speed * 10.0).round()).abs() > 0.0001
    {
        return Err(DspError::Controls);
    }
    if pcm.samples.is_empty()
        || pcm.samples.iter().any(|v| !v.is_finite())
        || !(8000..=48000).contains(&pcm.sample_rate)
    {
        return Err(DspError::Pcm);
    }
    let stretch_speed = if native_speed { 1.0 } else { speed };
    if stretch_speed == 1.0 && pitch == 0.0 {
        return Ok(pcm);
    }
    let mut stage = signalsmith_stretch::Stretch::preset_default(1, pcm.sample_rate);
    stage.set_transpose_factor_semitones(pitch, None);
    let mut output =
        vec![0.0; (pcm.samples.len() as f64 / f64::from(stretch_speed)).round() as usize];
    if !stage.exact(&pcm.samples, &mut output) || output.iter().any(|v| !v.is_finite()) {
        return Err(DspError::Stretch);
    }
    Ok(Pcm {
        id: pcm.id,
        sample_rate: pcm.sample_rate,
        samples: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pcm() -> Pcm {
        Pcm {
            id: 1,
            sample_rate: 24000,
            samples: (0..24000)
                .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 24000.0).sin() * 0.2)
                .collect(),
        }
    }
    #[test]
    fn duration_and_finite_output_for_control_matrix() {
        for speed in [0.5, 1.0, 2.0] {
            for pitch in [-6.0, 0.0, 6.0] {
                let output = transform(pcm(), speed, pitch, false).unwrap();
                assert_eq!(output.samples.len(), (24000.0 / speed) as usize);
                assert!(output.samples.iter().all(|v| v.is_finite()));
                assert!(output.samples.iter().any(|v| v.abs() > 0.001));
            }
        }
    }
    #[test]
    fn native_speed_is_applied_once_and_invalid_controls_are_rejected() {
        assert_eq!(
            transform(pcm(), 2.0, 0.0, true).unwrap().samples.len(),
            24000
        );
        for (speed, pitch) in [
            (f32::NAN, 0.0),
            (1.05, 0.0),
            (0.4, 0.0),
            (2.1, 0.0),
            (1.0, -7.0),
            (1.0, 7.0),
        ] {
            assert!(transform(pcm(), speed, pitch, false).is_err());
        }
    }
    #[test]
    fn speed_preserves_pitch_and_semitone_stage_moves_frequency_within_five_percent() {
        for speed in [0.5, 1.0, 2.0] {
            for pitch in [-6.0, 0.0, 6.0] {
                let out = transform(pcm(), speed, pitch, false).unwrap();
                let data = &out.samples[512..out.samples.len() - 512];
                let crossings = data
                    .windows(2)
                    .filter(|p| p[0] <= 0.0 && p[1] > 0.0)
                    .count();
                let hz = crossings as f64 / (data.len() as f64 / f64::from(out.sample_rate));
                let expected = 440.0 * 2.0f64.powf(f64::from(pitch) / 12.0);
                assert!(
                    (hz - expected).abs() / expected < 0.05,
                    "speed={speed} pitch={pitch} frequency={hz}"
                );
            }
        }
    }
}
