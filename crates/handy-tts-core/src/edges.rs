/// The -45 dBFS speech edge threshold.
pub const THRESHOLD: f32 = 0.005_623;

/// Trimmed speech and the number of removed samples at each edge.
pub struct Trimmed {
    pub samples: Vec<f32>,
    pub lead_removed: usize,
    pub tail_removed: usize,
}

/// Trims quiet edges with bounded guards and fades.
pub fn trim(mut samples: Vec<f32>, rate: u32) -> Trimmed {
    let Some(first) = samples.iter().position(|s| s.abs() >= THRESHOLD) else {
        return Trimmed {
            samples,
            lead_removed: 0,
            tail_removed: 0,
        };
    };
    let last = samples
        .iter()
        .rposition(|s| s.abs() >= THRESHOLD)
        .unwrap_or(first);
    let guard = (f64::from(rate) * 0.010).round() as usize;
    let lead_removed = first.saturating_sub(guard).min(rate as usize);
    let tail_removed = (samples.len() - last - 1)
        .saturating_sub(guard)
        .min((u64::from(rate) * 2) as usize);
    samples.truncate(samples.len() - tail_removed);
    samples.drain(..lead_removed);
    let fade_in = ((f64::from(rate) * 0.005).round() as usize).min(samples.len());
    let fade_out = ((f64::from(rate) * 0.010).round() as usize).min(samples.len());
    for (i, sample) in samples.iter_mut().take(fade_in).enumerate() {
        *sample *= i as f32 / fade_in.saturating_sub(1).max(1) as f32;
    }
    for (i, sample) in samples.iter_mut().rev().take(fade_out).enumerate() {
        *sample *= i as f32 / fade_out.saturating_sub(1).max(1) as f32;
    }
    Trimmed {
        samples,
        lead_removed,
        tail_removed,
    }
}

/// Returns the pause length at the requested playback speed.
pub fn pause_samples(base_ms: u32, requested_speed: f32, rate: u32) -> usize {
    if base_ms == 0 {
        return 0;
    }
    let played_ms = (f64::from(base_ms) / f64::from(requested_speed)).max(40.0);
    (played_ms * f64::from(rate) / 1000.0).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_every_sample_between_first_and_last_loud_sample() {
        let mut samples = vec![0.0; 1000];
        samples[100..200].fill(0.5);
        samples[800..900].fill(0.5);
        let out = trim(samples.clone(), 1000);
        assert_eq!(out.lead_removed, 90);
        assert_eq!(out.tail_removed, 90);
        assert_eq!(&out.samples[10..810], &samples[100..900]);
        assert_eq!(out.samples.len(), 820);
    }

    #[test]
    fn trim_respects_guard_and_maxima() {
        let mut samples = vec![0.0; 6000];
        samples[2000..3000].fill(THRESHOLD);
        let out = trim(samples, 1000);
        assert_eq!(out.lead_removed, 1000);
        assert_eq!(out.tail_removed, 2000);
        assert_eq!(out.samples.len(), 3000);
        let mut samples = vec![0.0; 1000];
        samples[100..900].fill(THRESHOLD);
        let out = trim(samples, 1000);
        assert_eq!((out.lead_removed, out.tail_removed), (90, 90));
    }

    #[test]
    fn all_quiet_chunk_is_kept_whole() {
        for samples in [vec![], vec![0.0; 1000], vec![THRESHOLD / 2.0; 1000]] {
            let out = trim(samples.clone(), 1000);
            assert_eq!(out.samples, samples);
            assert_eq!((out.lead_removed, out.tail_removed), (0, 0));
        }
    }

    #[test]
    fn fades_are_linear_and_bounded() {
        let out = trim(vec![1.0; 30], 1000);
        assert_eq!(&out.samples[..5], &[0.0, 0.25, 0.5, 0.75, 1.0]);
        assert_eq!(&out.samples[5..20], &[1.0; 15]);
        for (i, sample) in out.samples[20..].iter().enumerate() {
            assert!((sample - (9 - i) as f32 / 9.0).abs() < 0.000001);
        }
        for len in 1..15 {
            let out = trim(vec![1.0; len], 1000);
            assert_eq!(out.samples.len(), len);
            assert!(
                out.samples
                    .iter()
                    .all(|s| s.is_finite() && (0.0..=1.0).contains(s))
            );
        }
    }

    #[test]
    fn pause_scales_with_requested_speed() {
        let requested_speed = 2.0;
        let native_speed = 1.0;
        assert_ne!(requested_speed, native_speed);
        assert_eq!(pause_samples(280, requested_speed, 24000), 3360);
        assert_eq!(pause_samples(120, requested_speed, 24000), 1440);
        assert_eq!(pause_samples(20, requested_speed, 24000), 960);
        assert_eq!(pause_samples(0, requested_speed, 24000), 0);
        assert_eq!(pause_samples(450, 1.5, 22050), 6615);
        assert_eq!(pause_samples(100, 0.7, 22050), 3150);
    }
}
