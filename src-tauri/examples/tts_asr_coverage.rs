use anyhow::{bail, Context, Result};
use clap::Parser;
use handy_tts_core::coverage::{
    align, calibration_passes, longest_missing_loss_run, normalize_words, passes,
    relative_anchors_in_order, Coverage,
};
use handy_tts_core::text::TextCursor;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use transcribe_cpp::RunOptions;

#[derive(Parser)]
#[command(about = "Measure offline ASR word coverage against prepared read-aloud text")]
struct Args {
    #[arg(long)]
    model: PathBuf,
    #[arg(long)]
    expected: PathBuf,
    #[arg(long, required = true, num_args = 1..)]
    wav: Vec<PathBuf>,
    #[arg(long)]
    anchors: Option<String>,
    #[arg(long)]
    output: PathBuf,
    /// Require the plain-prose calibration recall threshold of 0.95.
    #[arg(long)]
    calibration: bool,
    /// Bind each non-calibration WAV to its continuity measurement.
    #[arg(long, num_args = 1..)]
    continuity: Vec<PathBuf>,
    /// Write a concatenated WAV: destination followed by two or more input WAVs.
    #[arg(long, num_args = 3..)]
    concat: Vec<PathBuf>,
}

#[derive(Serialize)]
struct Report {
    binding: Option<serde_json::Value>,
    calibration: bool,
    expected_words: Vec<String>,
    wav: PathBuf,
    transcript: String,
    #[serde(flatten)]
    coverage: Coverage,
    baseline_wav: PathBuf,
    longest_missing_loss_run: usize,
    losses: Vec<(usize, String)>,
    anchors_ok: bool,
    calibration_ok: bool,
    passed: bool,
}

fn read_wav(path: &Path) -> Result<(u32, Vec<f32>)> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("open WAV {}", path.display()))?;
    let spec = reader.spec();
    if !matches!(spec.channels, 1 | 2) || spec.sample_rate == 0 {
        bail!(
            "WAV must be mono or stereo with a positive sample rate: {}",
            path.display()
        );
    }
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            if spec.bits_per_sample == 0 || spec.bits_per_sample > 32 {
                bail!("unsupported PCM bit depth: {}", spec.bits_per_sample);
            }
            let scale = 2.0_f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    if samples.is_empty()
        || samples.len() % usize::from(spec.channels) != 0
        || samples.iter().any(|sample| !sample.is_finite())
    {
        bail!(
            "WAV has empty, incomplete or non-finite samples: {}",
            path.display()
        );
    }
    let mono = samples
        .chunks_exact(usize::from(spec.channels))
        .map(|frame| frame.iter().sum::<f32>() / f32::from(spec.channels))
        .collect();
    Ok((spec.sample_rate, mono))
}

fn resample_block(resampler: &mut Fft<f32>, samples: &[f32]) -> Result<Vec<f32>> {
    let input = InterleavedSlice::new(samples, 1, samples.len()).context("read resampler input")?;
    let mut samples_out = vec![0.0; resampler.output_frames_max()];
    let frames = samples_out.len();
    let mut output = InterleavedSlice::new_mut(&mut samples_out, 1, frames)
        .context("allocate resampler output")?;
    let mut indexing = Indexing::new();
    indexing.partial_len = Some(samples.len());
    let (_, produced) = resampler
        .process_into_buffer(&input, &mut output, Some(&indexing))
        .context("resample WAV block")?;
    samples_out.truncate(produced);
    Ok(samples_out)
}

fn resample(sample_rate: u32, pcm: &[f32]) -> Result<Vec<f32>> {
    const RATE: usize = 16_000;
    const BLOCK: usize = 1024;
    if sample_rate == RATE as u32 {
        return Ok(pcm.to_vec());
    }
    let mut resampler = Fft::<f32>::new(sample_rate as usize, RATE, BLOCK, 1, FixedSync::Input)
        .context("create 16 kHz resampler")?;
    let delay = resampler.output_delay();
    let target = (pcm.len() as f64 * RATE as f64 / f64::from(sample_rate)).round() as usize;
    let mut output = Vec::new();
    for chunk in pcm.chunks(BLOCK) {
        output.extend(resample_block(&mut resampler, chunk)?);
    }
    let flush_rounds = (delay + target).div_ceil(resampler.output_frames_max()) + 2;
    for _ in 0..flush_rounds {
        if output.len() >= delay + target {
            break;
        }
        output.extend(resample_block(&mut resampler, &[]).context("flush resampler tail")?);
    }
    if output.len() < delay + target {
        bail!("resampler did not flush the complete WAV");
    }
    Ok(output[delay..delay + target].to_vec())
}

fn concatenate(paths: &[PathBuf]) -> Result<()> {
    let Some((destination, inputs)) = paths.split_first() else {
        return Ok(());
    };
    if inputs.iter().any(|path| path == destination) {
        bail!("concatenated WAV must not overwrite an input");
    }
    let mut writer = hound::WavWriter::create(
        destination,
        hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .with_context(|| format!("create concatenated WAV {}", destination.display()))?;
    for path in inputs {
        let (rate, pcm) = read_wav(path)?;
        for sample in resample(rate, &pcm)? {
            writer
                .write_sample(sample)
                .context("write concatenated sample")?;
        }
    }
    writer.finalize().context("finalize concatenated WAV")
}

fn prepared_words(source: &str) -> Result<Vec<String>> {
    let mut cursor = TextCursor::new(source, false).context("prepare expected text")?;
    let mut words = Vec::new();
    while let Some(chunk) = cursor.next_chunk().context("prepare expected chunk")? {
        words.extend(normalize_words(&chunk.text));
    }
    if words.is_empty() {
        bail!("expected text has no spoken words");
    }
    Ok(words)
}

fn coverage_report(
    expected: &[String],
    path: &Path,
    transcript: String,
    first_report: Option<&Report>,
    anchors: &[&[&str]],
    calibration: bool,
) -> Report {
    let heard = normalize_words(&transcript);
    let coverage = align(expected, &heard);
    let baseline = first_report
        .map(|report| &report.coverage)
        .unwrap_or(&coverage);
    let anchors_ok = relative_anchors_in_order(expected, &heard, baseline, anchors);
    let longest_missing_loss_run = longest_missing_loss_run(&coverage, baseline);
    let losses = coverage
        .missing
        .iter()
        .filter(|(index, _)| !baseline.missing.iter().any(|(missing, _)| missing == index))
        .cloned()
        .collect();
    let calibration_ok = !calibration || calibration_passes(&coverage);
    let passed = passes(&coverage, baseline) && anchors_ok && calibration_ok;
    Report {
        binding: None,
        calibration,
        expected_words: expected.to_vec(),
        wav: path.to_path_buf(),
        transcript,
        coverage,
        baseline_wav: first_report
            .map(|report| report.wav.clone())
            .unwrap_or_else(|| path.to_path_buf()),
        longest_missing_loss_run,
        losses,
        anchors_ok,
        calibration_ok,
        passed,
    }
}

fn append_report<'a>(
    reports: &'a mut Vec<Report>,
    expected: &[String],
    path: &Path,
    transcript: String,
    anchors: &[&[&str]],
    calibration: bool,
) -> &'a Report {
    let index = reports.len();
    let report = coverage_report(
        expected,
        path,
        transcript,
        reports.first(),
        anchors,
        calibration,
    );
    reports.push(report);
    &reports[index]
}

fn exit_status(reports: &[Report]) -> u8 {
    if reports.iter().all(|report| report.passed) {
        0
    } else {
        1
    }
}

fn coverage_binding(
    paths: &[PathBuf],
    wav: &Path,
    expected: &[String],
) -> Result<Option<serde_json::Value>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut matched = Vec::new();
    for path in paths {
        let record: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let binding = &record["coverage_input"];
        if binding["wav"].as_str() == wav.to_str() {
            for key in ["model", "device", "speed", "corpus"] {
                anyhow::ensure!(
                    binding[key] == record[key],
                    "coverage run metadata mismatch: {key}"
                );
            }
            anyhow::ensure!(
                binding["source_wav"] == record["wav"],
                "coverage source WAV mismatch"
            );
            let words: Vec<String> = serde_json::from_value(binding["expected_words"].clone())?;
            anyhow::ensure!(words == expected, "coverage expected text mismatch");
            matched.push(binding.clone());
        }
    }
    anyhow::ensure!(
        matched.len() == 1,
        "each coverage WAV needs exactly one continuity record"
    );
    Ok(matched.pop())
}

fn main() -> Result<ExitCode> {
    let args = Args::parse();
    anyhow::ensure!(
        !args.calibration || args.continuity.is_empty(),
        "calibration cannot bind a continuity run"
    );
    concatenate(&args.concat)?;
    let source = std::fs::read_to_string(&args.expected)
        .with_context(|| format!("read expected text {}", args.expected.display()))?;
    let expected = prepared_words(&source)?;
    let anchors: Vec<Vec<String>> = args
        .anchors
        .as_deref()
        .map(|value| value.split('|').map(normalize_words).collect())
        .unwrap_or_default();
    if anchors.iter().any(Vec::is_empty) {
        bail!("anchor phrases must contain words");
    }
    let anchor_words: Vec<Vec<&str>> = anchors
        .iter()
        .map(|words| words.iter().map(String::as_str).collect())
        .collect();
    let anchor_refs: Vec<&[&str]> = anchor_words.iter().map(Vec::as_slice).collect();
    transcribe_cpp::init_logging();
    transcribe_cpp::init_backends_default().context("initialize ASR backends")?;
    let mut reports: Vec<Report> = Vec::new();
    for path in &args.wav {
        let binding = coverage_binding(&args.continuity, path, &expected)?;
        let (rate, pcm) = read_wav(path)?;
        let pcm = resample(rate, &pcm)?;
        let transcript = transcribe_cpp::transcribe(&args.model, &pcm, &RunOptions::default())
            .with_context(|| format!("transcribe {}", path.display()))?
            .text;
        append_report(
            &mut reports,
            &expected,
            path,
            transcript,
            &anchor_refs,
            args.calibration,
        );
        let report = reports.last_mut().context("coverage report missing")?;
        report.binding = binding;
        println!(
            "{}: recall={:.6} longest_missing_run={} longest_missing_loss_run={} anchors_ok={} calibration_ok={} passed={}",
            report.wav.display(),
            report.coverage.recall,
            report.coverage.longest_missing_run,
            report.longest_missing_loss_run,
            report.anchors_ok,
            report.calibration_ok,
            report.passed
        );
    }
    let file = std::fs::File::create(&args.output)
        .with_context(|| format!("create coverage report {}", args.output.display()))?;
    serde_json::to_writer_pretty(file, &reports).context("write coverage report")?;
    Ok(ExitCode::from(exit_status(&reports)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_binding_rejects_wrong_text_identity_and_duplicate_records() {
        let scratch = std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap();
        let directory = tempfile::tempdir_in(scratch).unwrap();
        let path = directory.path().join("run.json");
        let wav = directory.path().join("delivered.wav");
        let words = vec!["alpha".to_string(), "beta".to_string()];
        let binding = serde_json::json!({"model":"pocket-en","device":"cpu","speed":2.0,
            "corpus":"/fixture/owner.txt","wav":wav,"source_wav":wav,"scope":"owner",
            "expected_words":words});
        let mut record = binding.clone();
        record["coverage_input"] = binding.clone();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(
            coverage_binding(std::slice::from_ref(&path), &wav, &words).unwrap(),
            Some(binding)
        );
        assert!(coverage_binding(&[path.clone(), path.clone()], &wav, &words).is_err());
        assert!(coverage_binding(std::slice::from_ref(&path), &wav, &["wrong".into()]).is_err());
        record["model"] = "kokoro-82m-int8".into();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(coverage_binding(std::slice::from_ref(&path), &wav, &words).is_err());
        assert!(coverage_binding(&[path], &directory.path().join("other.wav"), &words).is_err());
    }

    fn wav_fixture<T: hound::Sample + Copy>(
        spec: hound::WavSpec,
        samples: &[T],
    ) -> (tempfile::TempDir, PathBuf) {
        let scratch = std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap();
        let directory = tempfile::tempdir_in(scratch).unwrap();
        let path = directory.path().join("input.wav");
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        (directory, path)
    }

    fn float_spec(channels: u16, sample_rate: u32) -> hound::WavSpec {
        hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        }
    }

    #[test]
    fn stereo_wav_downmixes_both_channels() {
        let (_directory, path) = wav_fixture(
            float_spec(2, 44_100),
            &[0.75_f32, -0.25, 0.25, 0.75, -0.5, 0.5],
        );
        let (rate, mono) = read_wav(&path).unwrap();
        assert_eq!(rate, 44_100);
        assert_eq!(mono, [0.25, 0.5, 0.0]);
    }

    #[test]
    fn wav_decodes_integer_and_float_pcm() {
        let (_integer_directory, integer_path) = wav_fixture(
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            &[i16::MIN, -16_384, 0, 16_384, i16::MAX],
        );
        let (rate, samples) = read_wav(&integer_path).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples, [-1.0, -0.5, 0.0, 0.5, 32_767.0 / 32_768.0]);
        let (_float_directory, float_path) =
            wav_fixture(float_spec(1, 24_000), &[-0.75_f32, 0.125, 0.5]);
        let (rate, samples) = read_wav(&float_path).unwrap();
        assert_eq!(rate, 24_000);
        assert_eq!(samples, [-0.75, 0.125, 0.5]);
    }

    #[test]
    fn wav_rejects_invalid_input() {
        for (spec, samples) in [
            (float_spec(3, 16_000), vec![0.1_f32; 6]),
            (float_spec(1, 16_000), vec![]),
            (float_spec(1, 16_000), vec![f32::NAN]),
            (float_spec(1, 16_000), vec![f32::INFINITY]),
        ] {
            let (_directory, path) = wav_fixture(spec, &samples);
            assert!(read_wav(&path).is_err(), "accepted {spec:?}");
        }
        let (_directory, path) = wav_fixture(float_spec(1, 16_000), &[0.1_f32]);
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[12..16], b"fmt ");
        bytes[24..32].fill(0);
        std::fs::write(&path, bytes).unwrap();
        assert!(read_wav(&path).is_err());
    }

    #[test]
    fn resampling_at_sixteen_khz_is_identity() {
        let samples = [-0.75, 0.125, 0.0, 0.5, -0.25];
        assert_eq!(resample(16_000, &samples).unwrap(), samples);
    }

    #[test]
    fn resampling_preserves_final_tone_and_duration() {
        for rate in [44_100, 48_000] {
            for length in [64, 300, 1024, 4396] {
                let mut samples = vec![0.0_f32; length];
                let burst = length.min(200);
                for (index, sample) in samples[length - burst..].iter_mut().enumerate() {
                    *sample = 0.5
                        * (2.0 * std::f32::consts::PI * 1000.0 * index as f32 / rate as f32).sin();
                }
                let output = resample(rate, &samples).unwrap();
                let expected = (length as f64 * 16_000.0 / f64::from(rate)).round() as usize;
                assert!(output.len().abs_diff(expected) <= 1);
                assert!(output.iter().all(|sample| sample.is_finite()));
                let tail_length =
                    ((burst as f64 * 16_000.0 / f64::from(rate)) / 2.0).round() as usize;
                let tail = &output[output.len() - tail_length..];
                let rms = (tail.iter().map(|sample| sample * sample).sum::<f32>()
                    / tail.len() as f32)
                    .sqrt();
                assert!(
                    rms > 0.15,
                    "tail tone lost: rate={rate}, length={length}, rms={rms}"
                );
            }
        }
    }

    #[test]
    fn concatenation_preserves_order_and_refuses_to_overwrite_inputs() {
        let (first_directory, first) = wav_fixture(float_spec(1, 16_000), &[0.25_f32; 16]);
        let (_second_directory, second) = wav_fixture(float_spec(1, 16_000), &[-0.5_f32; 12]);
        let output = first_directory.path().join("joined.wav");
        concatenate(&[output.clone(), first.clone(), second.clone()]).unwrap();
        let (rate, samples) = read_wav(&output).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples, [vec![0.25; 16], vec![-0.5; 12]].concat());
        let original = std::fs::read(&first).unwrap();
        assert!(concatenate(&[first.clone(), first.clone(), second]).is_err());
        assert_eq!(std::fs::read(first).unwrap(), original);
    }

    #[test]
    fn expected_words_use_text_cursor_preparation() {
        let source = "# A bright morning\n\nRead [the story](https://example.org/article).\n\n```rust\nsecret = 1\n```\n\nWe walked home.";
        assert_eq!(
            prepared_words(source).unwrap(),
            [
                "a", "bright", "morning", "read", "the", "story", "code", "block", "skipped", "we",
                "walked", "home",
            ]
        );
        assert!(prepared_words("").is_err());
    }

    #[test]
    fn reports_compare_later_wavs_to_first_baseline() {
        let expected = normalize_words("first second third fourth");
        let mut reports = Vec::new();
        append_report(
            &mut reports,
            &expected,
            Path::new("first.wav"),
            "first second third fourth".into(),
            &[],
            false,
        );
        append_report(
            &mut reports,
            &expected,
            Path::new("later.wav"),
            "first fourth".into(),
            &[],
            false,
        );
        append_report(
            &mut reports,
            &expected,
            Path::new("third.wav"),
            "first third fourth".into(),
            &[],
            false,
        );
        let first = &reports[0];
        let later = &reports[1];
        assert!(first.passed);
        assert_eq!(later.baseline_wav, Path::new("first.wav"));
        assert_eq!(later.coverage.recall, 0.5);
        assert_eq!(later.longest_missing_loss_run, 2);
        assert_eq!(later.losses, [(1, "second".into()), (2, "third".into())]);
        assert!(!later.passed);
        assert_eq!(reports[2].baseline_wav, Path::new("first.wav"));
        assert!(!reports[2].passed);
    }

    #[test]
    fn reports_propagate_anchor_and_calibration_failures() {
        let expected = normalize_words("alpha beta gamma");
        let anchor_failure = coverage_report(
            &expected,
            Path::new("anchor.wav"),
            "alpha beta gamma".into(),
            None,
            &[&["missing", "phrase"]],
            false,
        );
        assert_eq!(anchor_failure.coverage.recall, 1.0);
        assert!(!anchor_failure.anchors_ok);
        assert!(!anchor_failure.passed);
        let expected = normalize_words(&"word ".repeat(100));
        let uncalibrated = coverage_report(
            &expected,
            Path::new("plain.wav"),
            "word ".repeat(94),
            None,
            &[],
            false,
        );
        assert!(uncalibrated.passed);
        let calibrated = coverage_report(
            &expected,
            Path::new("calibration.wav"),
            "word ".repeat(94),
            None,
            &[],
            true,
        );
        assert!(calibrated.anchors_ok);
        assert!(!calibrated.calibration_ok);
        assert!(!calibrated.passed);
    }

    #[test]
    fn any_failed_report_returns_failure() {
        let expected = normalize_words("first second third fourth");
        let good = coverage_report(
            &expected,
            Path::new("good.wav"),
            "first second third fourth".into(),
            None,
            &[],
            false,
        );
        let bad = coverage_report(
            &expected,
            Path::new("bad.wav"),
            "first fourth".into(),
            Some(&good),
            &[],
            false,
        );
        assert_eq!(exit_status(std::slice::from_ref(&good)), 0);
        let mut reports = [good, bad];
        assert_eq!(exit_status(&reports), 1);
        reports.reverse();
        assert_eq!(exit_status(&reports), 1);
    }
}
