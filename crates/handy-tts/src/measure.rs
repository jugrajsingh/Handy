use anyhow::{Context, Result, ensure};
use handy_tts_core::{catalog, device::Device, dsp, protocol::Pcm, text::TextCursor};
use std::{path::Path, sync::atomic::AtomicBool, time::Instant};

fn quantile(values: &mut [f64], fraction: f64) -> Result<f64> {
    ensure!(!values.is_empty(), "measurement corpus contains no chunks");
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * fraction).ceil() as usize;
    Ok(values[index])
}

/// Measure warm synthesis with DSP and separate cold and cached loads.
pub fn run(id: &str, root: &Path, device: Device, cache: &Path, corpus: &Path) -> Result<()> {
    let model = catalog::entry(id)?;
    let source = std::fs::read_to_string(corpus).context("read explicit measurement corpus")?;
    let mut cursor = TextCursor::new(&source, false)?;
    let mut chunks = Vec::new();
    while chunks.len() < 7 {
        let Some(chunk) = cursor.next_chunk()? else {
            break;
        };
        chunks.push(chunk.text);
    }
    ensure!(
        chunks.len() >= 3,
        "measurement corpus needs at least three chunks"
    );
    let began = Instant::now();
    let engine =
        crate::runtime::load_adapter(id, root, model.default_voice, model.threads, device, cache)?;
    let cold = began.elapsed().as_secs_f64();
    let placement = engine.placement();
    drop(engine);
    let began = Instant::now();
    let mut engine =
        crate::runtime::load_adapter(id, root, model.default_voice, model.threads, device, cache)?;
    let cached = began.elapsed().as_secs_f64();
    let mut first_audio = Vec::new();
    let mut one = Vec::new();
    let mut two = Vec::new();
    let native = model.native_speed;
    for speed in [1.0, 2.0] {
        for (index, text) in chunks.iter().enumerate() {
            let began = Instant::now();
            let samples =
                engine.synthesize(text, native.then_some(speed), &AtomicBool::new(false))?;
            let pcm = dsp::transform(
                Pcm {
                    id: index as u64 + 1,
                    sample_rate: engine.sample_rate(),
                    samples,
                },
                speed,
                0.0,
                native,
            )?;
            let seconds = began.elapsed().as_secs_f64();
            let audio_seconds = pcm.samples.len() as f64 / f64::from(pcm.sample_rate);
            ensure!(
                audio_seconds > 0.0 && pcm.samples.iter().all(|v| v.is_finite()),
                "measurement returned invalid audio"
            );
            if speed == 1.0 {
                one.push(seconds / audio_seconds);
                first_audio.push(seconds);
            } else {
                two.push(seconds / audio_seconds);
            }
            eprintln!(
                "MEASURE model={id} device={device:?} speed={speed} chunk={index} seconds={seconds:.6} audio_seconds={audio_seconds:.6} rtf={:.6}",
                seconds / audio_seconds
            );
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "supported":true,"engine":if id=="kokoro-82m-int8"{"onnx-misaki-rs"}else{"ported-engine"},
            "placement":placement,"first_audio_p95_s":quantile(&mut first_audio,0.95)?,
            "rtf_1x":quantile(&mut one,0.5)?,"rtf_2x":quantile(&mut two,0.5)?,
            "load_s_cold":cold,"load_s_cached":cached,"chunks":chunks.len()
        })
    );
    Ok(())
}

/// Measure every prepared Pocket chunk and enforce the continuity gate.
pub fn continuity(
    id: &str,
    root: &Path,
    device: Device,
    cache: &Path,
    corpus: &Path,
    output: &Path,
) -> Result<()> {
    use handy_tts_core::performance::{Timing, gate, replay};
    use std::io::Write;
    ensure!(
        id == "pocket-en",
        "continuity probe is the Pocket acceptance gate"
    );
    let model = catalog::entry(id)?;
    let source = std::fs::read_to_string(corpus).context("read explicit continuity corpus")?;
    let words = source.split_whitespace().count();
    let mut cursor = TextCursor::new(&source, false)?;
    let mut engine =
        crate::runtime::load_adapter(id, root, model.default_voice, model.threads, device, cache)?;
    let mut records = std::io::BufWriter::new(
        std::fs::File::create(output).context("create explicit continuity output")?,
    );
    let mut timings = Vec::new();
    let mut rtfs = Vec::new();
    while let Some(chunk) = cursor.next_chunk()? {
        let began = Instant::now();
        let samples = engine.synthesize(&chunk.text, None, &AtomicBool::new(false))?;
        let synthesis_seconds = began.elapsed().as_secs_f64();
        let began = Instant::now();
        let pcm = dsp::transform(
            Pcm {
                id: chunk.index as u64 + 1,
                sample_rate: engine.sample_rate(),
                samples,
            },
            2.0,
            0.0,
            false,
        )?;
        let dsp_seconds = began.elapsed().as_secs_f64();
        let audio_seconds = pcm.samples.len() as f64 / f64::from(pcm.sample_rate);
        rtfs.push((synthesis_seconds + dsp_seconds) / audio_seconds);
        timings.push(Timing {
            synthesis_seconds: synthesis_seconds + dsp_seconds,
            audio_seconds,
            samples: pcm.samples.len(),
            sample_rate: pcm.sample_rate,
        });
        writeln!(
            records,
            "{}",
            serde_json::json!({"kind":"dsp", "samples":pcm.samples.len(), "audio_s":audio_seconds, "synth_s":synthesis_seconds, "dsp_s":dsp_seconds})
        )?;
    }
    records.flush()?;
    let median = quantile(&mut rtfs, 0.5)?;
    let result = replay(&timings, 2.0).map_err(|error| anyhow::anyhow!(error))?;
    let pass = gate(id, device, device, median, words, result.buffering_pauses);
    println!(
        "DSP_GATE speed=2 median_rtf={median:.6} buffering_pauses={} head_start_wait_seconds={:.6} words={words} pass={pass}",
        result.buffering_pauses, result.head_start_wait_seconds
    );
    ensure!(pass, "Pocket continuity gate failed");
    Ok(())
}
#[cfg(test)]
mod measurement_tests {
    use super::*;
    #[test]
    fn seven_observation_median_and_p95_use_observed_values_without_interpolation() {
        assert_eq!(
            quantile(&mut [7., 1., 4., 2., 6., 3., 5.], 0.5).unwrap(),
            4.
        );
        assert_eq!(
            quantile(&mut [7., 1., 4., 2., 6., 3., 5.], 0.95).unwrap(),
            7.
        );
        assert!(quantile(&mut [], 0.5).is_err());
    }
}
