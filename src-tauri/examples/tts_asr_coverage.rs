use anyhow::{bail, Context, Result};
use clap::Parser;
use handy_tts_core::coverage::{
    align, calibration_passes, longest_missing_loss_run, normalize_words, passes,
    relative_anchors_in_order, Coverage,
};
use handy_tts_core::text::TextCursor;
use rubato::{FftFixedIn, Resampler};
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
    /// Write a concatenated WAV: destination followed by two or more input WAVs.
    #[arg(long, num_args = 3..)]
    concat: Vec<PathBuf>,
}

#[derive(Serialize)]
struct Report {
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

fn resample(sample_rate: u32, pcm: &[f32]) -> Result<Vec<f32>> {
    const RATE: usize = 16_000;
    const BLOCK: usize = 1024;
    if sample_rate == RATE as u32 {
        return Ok(pcm.to_vec());
    }
    let mut resampler = FftFixedIn::<f32>::new(sample_rate as usize, RATE, BLOCK, 1, 1)
        .context("create 16 kHz resampler")?;
    let delay = resampler.output_delay();
    let target = (pcm.len() as f64 * RATE as f64 / f64::from(sample_rate)).round() as usize;
    let mut output = Vec::new();
    for chunk in pcm.chunks(BLOCK) {
        let block = resampler
            .process_partial(Some(&[chunk]), None)
            .context("resample WAV block")?;
        output.extend_from_slice(&block[0]);
    }
    let flush_rounds = (delay + target).div_ceil(resampler.output_frames_max()) + 2;
    for _ in 0..flush_rounds {
        if output.len() >= delay + target {
            break;
        }
        let block = resampler
            .process_partial::<&[f32]>(None, None)
            .context("flush resampler tail")?;
        output.extend_from_slice(&block[0]);
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

fn main() -> Result<ExitCode> {
    let args = Args::parse();
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
        let (rate, pcm) = read_wav(path)?;
        let pcm = resample(rate, &pcm)?;
        let transcript = transcribe_cpp::transcribe(&args.model, &pcm, &RunOptions::default())
            .with_context(|| format!("transcribe {}", path.display()))?
            .text;
        let heard = normalize_words(&transcript);
        let coverage = align(&expected, &heard);
        let baseline = reports
            .first()
            .map(|report| &report.coverage)
            .unwrap_or(&coverage);
        let anchors_ok = relative_anchors_in_order(&expected, &heard, baseline, &anchor_refs);
        let longest_missing_loss_run = longest_missing_loss_run(&coverage, baseline);
        let losses = coverage
            .missing
            .iter()
            .filter(|(index, _)| !baseline.missing.iter().any(|(missing, _)| missing == index))
            .cloned()
            .collect();
        let calibration_ok = !args.calibration || calibration_passes(&coverage);
        let passed = passes(&coverage, baseline) && anchors_ok && calibration_ok;
        println!(
            "{}: recall={:.6} longest_missing_run={} longest_missing_loss_run={} anchors_ok={} calibration_ok={} passed={}",
            path.display(),
            coverage.recall,
            coverage.longest_missing_run,
            longest_missing_loss_run,
            anchors_ok,
            calibration_ok,
            passed
        );
        reports.push(Report {
            wav: path.clone(),
            transcript,
            coverage,
            baseline_wav: args.wav[0].clone(),
            longest_missing_loss_run,
            losses,
            anchors_ok,
            calibration_ok,
            passed,
        });
    }
    let file = std::fs::File::create(&args.output)
        .with_context(|| format!("create coverage report {}", args.output.display()))?;
    serde_json::to_writer_pretty(file, &reports).context("write coverage report")?;
    Ok(if reports.iter().all(|report| report.passed) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
