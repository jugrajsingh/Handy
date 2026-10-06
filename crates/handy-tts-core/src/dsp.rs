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
pub fn transform(
    mut pcm: Pcm,
    speed: f32,
    pitch: f32,
    native_speed: bool,
) -> Result<Pcm, DspError> {
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
    let stretched_len = |len: usize| (len as f64 / f64::from(stretch_speed)).round() as usize;
    let output_len = stretched_len(pcm.samples.len());
    let minimum_output = stage.output_latency() * 2;
    if output_len < minimum_output {
        let minimum_input = (minimum_output as f64 * f64::from(stretch_speed)).ceil() as usize;
        pcm.samples
            .resize(minimum_input.max(pcm.samples.len()), 0.0);
    }
    let mut output = vec![0.0; stretched_len(pcm.samples.len())];
    if !stage.exact(&pcm.samples, &mut output) || output.iter().any(|v| !v.is_finite()) {
        return Err(DspError::Stretch);
    }
    output.truncate(output_len);
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
    fn frequency(samples: &[f32], rate: u32) -> f64 {
        let crossings: Vec<f64> = samples
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0] <= 0.0 && pair[1] > 0.0)
            .map(|(i, pair)| i as f64 - f64::from(pair[0]) / f64::from(pair[1] - pair[0]))
            .collect();
        assert!(crossings.len() > 2);
        f64::from(rate) * (crossings.len() - 1) as f64
            / (crossings[crossings.len() - 1] - crossings[0])
    }

    #[test]
    fn short_chunks_keep_duration_frequency_and_energy() {
        for rate in [24000, 44100] {
            for millis in [50, 200] {
                for speed in [0.5, 1.0, 1.5, 2.0] {
                    for pitch in [-6.0, 6.0] {
                        for native in [false, true] {
                            let len = (rate * millis / 1000) as usize;
                            let input = Pcm {
                                id: 7,
                                sample_rate: rate,
                                samples: (0..len)
                                    .map(|i| {
                                        (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32)
                                            .sin()
                                            * 0.2
                                    })
                                    .collect(),
                            };
                            let out = transform(input, speed, pitch, native).unwrap();
                            let ratio = if native { 1.0 } else { speed };
                            assert_eq!(
                                out.samples.len(),
                                (len as f64 / f64::from(ratio)).round() as usize
                            );
                            assert!(out.samples.iter().all(|v| v.is_finite()));
                            let trim = out.samples.len() / 10;
                            let data = &out.samples[trim..out.samples.len() - trim];
                            let hz = frequency(data, rate);
                            let expected = 440.0 * 2.0f64.powf(f64::from(pitch) / 12.0);
                            let energy = data.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>()
                                / data.len() as f64;
                            println!(
                                "short rate={rate} ms={millis} speed={speed} pitch={pitch} native={native} hz={hz:.2} energy={energy:.5}"
                            );
                            assert!(
                                (hz - expected).abs() / expected < 0.05,
                                "frequency={hz} expected={expected}"
                            );
                            let expected_energy = 0.2_f64.powi(2) / 2.0;
                            assert!(
                                (energy - expected_energy).abs() / expected_energy < 0.3,
                                "energy={energy} expected={expected_energy}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn native_speed_keeps_length_and_applies_nonzero_pitch() {
        for speed in [0.5, 1.0, 1.5, 2.0] {
            for pitch in [-6.0, 6.0] {
                let out = transform(pcm(), speed, pitch, true).unwrap();
                assert_eq!(out.samples.len(), 24000);
                let hz = frequency(&out.samples[512..out.samples.len() - 512], out.sample_rate);
                let expected = 440.0 * 2.0f64.powf(f64::from(pitch) / 12.0);
                assert!(
                    (hz - expected).abs() / expected < 0.05,
                    "frequency={hz} expected={expected}"
                );
            }
        }
    }
}
