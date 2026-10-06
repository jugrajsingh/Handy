use anyhow::{Context, Result, ensure};
use handy_tts_core::{catalog, device::Device, dsp, protocol::Pcm, text::TextCursor};
use std::{path::Path, sync::atomic::AtomicBool, time::Instant};

fn quantile(values: &mut [f64], fraction: f64) -> Result<f64> {
    ensure!(!values.is_empty(), "measurement corpus contains no chunks");
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * fraction).ceil() as usize;
    Ok(values[index])
}

fn first_audio_latency(
    source: &str,
    mut synthesize: impl FnMut(&str) -> Result<Pcm>,
    clock: impl Fn() -> f64,
) -> Result<f64> {
    use handy_tts_core::scheduler::{AudioChunk, Queue};
    let began = clock();
    let mut cursor = TextCursor::new(source, false)?;
    let mut queue = Queue::with_speed(1.0);
    queue.play();
    loop {
        let ticket = queue
            .request_next()
            .context("first audio scheduler stalled")?;
        let Some(chunk) = cursor.next_chunk()? else {
            queue.in_flight = None;
            queue.source_finished = true;
            queue.advance();
            ensure!(
                queue.playing.is_some(),
                "first audio input contains no speech"
            );
            return Ok(clock() - began);
        };
        let start = clock();
        let pcm = synthesize(&chunk.text)?;
        ensure!(
            !pcm.samples.is_empty() && pcm.samples.iter().all(|v| v.is_finite()),
            "measurement returned invalid audio"
        );
        let deferred = queue.complete_timed(
            ticket,
            Ok(AudioChunk {
                ticket,
                pcm,
                source_start: chunk.source_start,
                source_end: chunk.source_end,
                pause_ms: chunk.pause_ms,
            }),
            clock() - start,
        );
        ensure!(deferred.is_none(), "first audio exceeded queue capacity");
        ensure!(
            queue.failed_index.is_none(),
            "first audio scheduler rejected PCM"
        );
        if queue.playing.is_some() {
            return Ok(clock() - began);
        }
    }
}

fn first_audio_documents(directory: &Path) -> Result<Vec<(std::path::PathBuf, String)>> {
    let mut paths = std::fs::read_dir(directory)
        .context("read explicit first audio inputs")?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "txt"));
    paths.sort();
    ensure!(
        paths.len() == 7,
        "first audio requires seven bake-off documents"
    );
    paths
        .into_iter()
        .map(|path| {
            let source = std::fs::read_to_string(&path).context("read first audio document")?;
            Ok((path, source))
        })
        .collect()
}

/// Measure warm synthesis with DSP and separate cold and cached loads.
pub fn run(
    id: &str,
    root: &Path,
    device: Device,
    cache: &Path,
    corpus: &Path,
    first_audio_inputs: &Path,
) -> Result<()> {
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
    let documents = first_audio_documents(first_audio_inputs)?;
    crate::runtime::synthesize_bounded(
        engine.as_mut(),
        &chunks[0],
        model.native_speed.then_some(1.0),
        &AtomicBool::new(false),
    )?;
    let mut first_audio = Vec::new();
    for (path, source) in &documents {
        let origin = Instant::now();
        let seconds = first_audio_latency(
            source,
            |text| {
                let samples = crate::runtime::synthesize_bounded(
                    engine.as_mut(),
                    text,
                    model.native_speed.then_some(1.0),
                    &AtomicBool::new(false),
                )?;
                dsp::transform(
                    Pcm {
                        id: 1,
                        sample_rate: engine.sample_rate(),
                        samples,
                    },
                    1.0,
                    0.0,
                    model.native_speed,
                )
                .map_err(Into::into)
            },
            || origin.elapsed().as_secs_f64(),
        )?;
        eprintln!(
            "FIRST_AUDIO model={id} device={device:?} input={} seconds={seconds:.6}",
            path.display()
        );
        first_audio.push(seconds);
    }
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
            "load_s_cold":cold,"load_s_cached":cached,"chunks":chunks.len(),"first_audio_inputs":first_audio.len()
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

#[cfg(test)]
mod tests {
    mod first_audio_tests {
        use super::super::{Pcm, TextCursor, first_audio_latency};
        use std::cell::Cell;

        fn measure(source: &str, first_samples: usize, later_delay: f64) -> (f64, Vec<String>) {
            let clock = Cell::new(0.0);
            let mut calls = Vec::new();
            let seconds = first_audio_latency(
                source,
                |text| {
                    let first = calls.is_empty();
                    calls.push(text.to_owned());
                    clock.set(clock.get() + if first { 0.1 } else { later_delay });
                    Ok(Pcm {
                        id: calls.len() as u64,
                        sample_rate: 8000,
                        samples: vec![0.0; if first { first_samples } else { 80000 }],
                    })
                },
                || clock.get(),
            )
            .unwrap();
            (seconds, calls)
        }

        const DOCUMENT: &str = "# A heading with enough characters to form the first chunk

A second sentence which is long enough to form a separate chunk.

The final sentence takes a very long time to synthesize.";

        #[test]
        fn chunks_after_playback_readiness_do_not_change_first_audio() {
            for delay in [1.0, 100.0] {
                let (seconds, calls) = measure(DOCUMENT, 80000, delay);
                assert_eq!(seconds, 0.1);
                assert_eq!(calls.len(), 1);
            }
        }

        #[test]
        fn short_first_chunk_waits_for_the_adaptive_scheduler_target() {
            let (seconds, calls) = measure(DOCUMENT, 800, 0.2);
            assert!((seconds - 0.3).abs() < 1e-9);
            assert_eq!(calls.len(), 2);
        }

        #[test]
        fn first_audio_uses_production_markdown_preparation_and_chunk_boundaries() {
            let (_, calls) = measure(DOCUMENT, 80000, 100.0);
            assert_eq!(
                calls,
                ["A heading with enough characters to form the first chunk."]
            );
            let source = "An opening sentence contains **emphasis** and [a label](https://example.com). More text follows the opening sentence.";
            let expected = TextCursor::new(source, false)
                .unwrap()
                .next_chunk()
                .unwrap()
                .unwrap()
                .text;
            let (_, calls) = measure(source, 80000, 100.0);
            assert_eq!(calls, [expected]);
            assert!(!calls[0].contains("**"));
            assert!(!calls[0].contains("https://"));
        }
    }
}
