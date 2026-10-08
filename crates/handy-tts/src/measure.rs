use anyhow::{Context, Result, ensure};
use handy_tts_core::{catalog, device::Device, dsp, protocol::Pcm, text::TextCursor};
use std::{
    path::Path,
    sync::{
        Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

fn rss_mib(status: &str) -> Result<f64> {
    let mut fields = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .context("worker VmRSS missing")?
        .split_whitespace()
        .skip(1);
    let kib: u64 = fields
        .next()
        .context("worker VmRSS value missing")?
        .parse()
        .context("parse worker VmRSS")?;
    ensure!(fields.next() == Some("kB"), "worker VmRSS unit must be kB");
    Ok(kib as f64 / 1024.0)
}

fn resident_memory() -> Result<f64> {
    rss_mib(
        &std::fs::read_to_string(format!("/proc/{}/status", std::process::id()))
            .context("read worker resident memory")?,
    )
}

fn measurement_chunks(corpus: &Path) -> Result<Vec<handy_tts_core::text::TextChunk>> {
    let source = std::fs::read_to_string(corpus).context("read explicit measurement corpus")?;
    let mut cursor = TextCursor::new(&source, false)?;
    let mut chunks = Vec::new();
    while chunks.len() < 7 {
        let Some(chunk) = cursor.next_chunk()? else {
            break;
        };
        chunks.push(chunk);
    }
    ensure!(
        chunks.len() >= 3,
        "measurement corpus needs at least three chunks"
    );
    Ok(chunks)
}

fn concurrent_batch(
    engines: Vec<Box<dyn crate::runtime::SynthEngine>>,
    chunks: &[handy_tts_core::text::TextChunk],
) -> Result<(f64, f64)> {
    ensure!(
        (1..=3).contains(&engines.len()),
        "invalid measurement lanes"
    );
    let next = AtomicUsize::new(0);
    let barrier = Barrier::new(engines.len() + 1);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for mut engine in engines {
            let next = &next;
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<_> {
                barrier.wait();
                let mut audio_seconds = 0.0;
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(chunk) = chunks.get(index) else {
                        break;
                    };
                    let samples = crate::runtime::synthesize_bounded(
                        engine.as_mut(),
                        &chunk.text,
                        Some(1.0),
                        &AtomicBool::new(false),
                    )?;
                    let rate = engine.sample_rate();
                    ensure!(
                        rate > 0 && !samples.is_empty() && samples.iter().all(|v| v.is_finite()),
                        "invalid lane PCM"
                    );
                    let trimmed = handy_tts_core::edges::trim(samples, rate);
                    let pcm = dsp::stretch(
                        Pcm {
                            id: index as u64,
                            sample_rate: rate,
                            samples: trimmed.samples,
                        },
                        1.0,
                        0.0,
                    )?;
                    let pause = handy_tts_core::edges::pause_samples(chunk.pause_ms, 1.0, rate);
                    audio_seconds += (pcm.samples.len() + pause) as f64 / f64::from(rate);
                }
                Ok((audio_seconds, engine))
            }));
        }
        let began = Instant::now();
        barrier.wait();
        let mut audio_seconds = 0.0;
        let mut retained = Vec::new();
        for handle in handles {
            let (audio, engine) = handle
                .join()
                .map_err(|_| anyhow::anyhow!("measurement lane panicked"))??;
            audio_seconds += audio;
            retained.push(engine);
        }
        let seconds = began.elapsed().as_secs_f64();
        ensure!(audio_seconds > 0.0, "lane sweep contains no audio");
        Ok((seconds, audio_seconds))
    })
}

/// Measures one lane count in an isolated worker process at native 1.0x.
pub fn lane_probe(
    id: &str,
    root: &Path,
    device: Device,
    cache: &Path,
    corpus: &Path,
    lanes: usize,
) -> Result<()> {
    let model = catalog::entry(id)?;
    let chunks = measurement_chunks(corpus)?;
    let base_rss_mib = resident_memory()?;
    let mut engines = crate::runtime::load_adapters(
        id,
        root,
        model.default_voice,
        model.threads,
        device,
        cache,
        lanes,
    )?;
    for engine in &mut engines {
        crate::runtime::synthesize_bounded(
            engine.as_mut(),
            &chunks[0].text,
            Some(1.0),
            &AtomicBool::new(false),
        )?;
    }
    let finished = AtomicBool::new(false);
    let loaded_rss = resident_memory()?;
    let (timing, peak) = std::thread::scope(|scope| -> Result<_> {
        let sampler = scope.spawn(|| -> Result<f64> {
            let mut peak = loaded_rss;
            while !finished.load(Ordering::SeqCst) {
                peak = peak.max(resident_memory()?);
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(peak)
        });
        let timing = concurrent_batch(engines, &chunks);
        finished.store(true, Ordering::SeqCst);
        let peak = sampler
            .join()
            .map_err(|_| anyhow::anyhow!("RSS sampler panicked"))??;
        Ok((timing?, peak))
    })?;
    println!(
        "{}",
        serde_json::json!({"lanes":lanes, "seconds":timing.0, "audio_seconds":timing.1, "rtf":timing.0/timing.1, "base_rss_mib":base_rss_mib, "peak_rss_mib":peak, "chunks":chunks.len()})
    );
    Ok(())
}

fn lane_sweep(
    id: &str,
    root: &Path,
    device: Device,
    cache: &Path,
    corpus: &Path,
) -> Result<serde_json::Value> {
    let mut rtfs = Vec::new();
    let mut base: f64 = 0.0;
    let mut per_lane: f64 = 0.0;
    for lanes in 1..=3 {
        let output = std::process::Command::new(
            std::env::current_exe().context("locate measurement worker")?,
        )
        .arg("--measure-lanes")
        .arg(id)
        .arg(root)
        .arg(
            serde_json::to_value(device)?
                .as_str()
                .context("serialize measurement device")?,
        )
        .arg(cache)
        .arg(corpus)
        .arg(lanes.to_string())
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("run isolated lane measurement")?;
        ensure!(output.status.success(), "lane measurement failed: {lanes}");
        let row: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("decode lane measurement")?;
        let number = |field: &str| -> Result<f64> {
            row[field]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .with_context(|| format!("invalid lane measurement {field}"))
        };
        rtfs.push(number("rtf")?);
        base = base.max(number("base_rss_mib")?);
        per_lane = per_lane
            .max((number("peak_rss_mib")? - number("base_rss_mib")?).max(0.0) / lanes as f64);
        eprintln!("LANE_SWEEP model={id} device={device:?} {row}");
    }
    Ok(serde_json::json!({"rtf_by_lanes":rtfs, "base_rss_mib":base, "lane_rss_mib":per_lane}))
}

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
            queue.in_flight.remove(&ticket.index);
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
        let pcm = Pcm {
            samples: handy_tts_core::edges::trim(pcm.samples, pcm.sample_rate).samples,
            ..pcm
        };
        let deferred = queue.complete_timed(
            ticket,
            Ok(AudioChunk {
                native: pcm.samples.clone(),
                plan: handy_tts_core::speed::SpeedPlan {
                    requested: 1.0,
                    native: 1.0,
                    stretch: 1.0,
                },
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
    let chunks = measurement_chunks(corpus)?
        .into_iter()
        .map(|chunk| chunk.text)
        .collect::<Vec<_>>();
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
        Some(1.0),
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
                    Some(1.0),
                    &AtomicBool::new(false),
                )?;
                dsp::stretch(
                    Pcm {
                        id: 1,
                        sample_rate: engine.sample_rate(),
                        samples,
                    },
                    1.0,
                    0.0,
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

    for speed in [1.0, 2.0] {
        for (index, text) in chunks.iter().enumerate() {
            let began = Instant::now();
            let plan = handy_tts_core::speed::plan(speed, model.native_speed)?;
            let samples = engine.synthesize(text, Some(plan.native), &AtomicBool::new(false))?;
            let pcm = dsp::stretch(
                Pcm {
                    id: index as u64 + 1,
                    sample_rate: engine.sample_rate(),
                    samples,
                },
                plan.stretch,
                0.0,
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
    drop(engine);
    let mut result = serde_json::json!({
        "supported":true,"engine":if id=="kokoro-82m-int8"{"onnx-misaki-rs"}else{"ported-engine"},
        "placement":placement,"first_audio_p95_s":quantile(&mut first_audio,0.95)?,
        "rtf_1x":quantile(&mut one,0.5)?,"rtf_2x":quantile(&mut two,0.5)?,
        "load_s_cold":cold,"load_s_cached":cached,"chunks":chunks.len(),"first_audio_inputs":first_audio.len()
    });
    let lanes = lane_sweep(id, root, device, cache, corpus)?;
    result
        .as_object_mut()
        .context("measurement result must be an object")?
        .extend(
            lanes
                .as_object()
                .context("lane result must be an object")?
                .clone(),
        );
    println!("{result}");
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
        let pcm = dsp::stretch(
            Pcm {
                id: chunk.index as u64 + 1,
                sample_rate: engine.sample_rate(),
                samples,
            },
            2.0,
            0.0,
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
    mod lane_sweep {
        use super::super::*;
        use std::sync::{Arc, Mutex};

        struct Fake {
            start: Arc<Barrier>,
            calls: Arc<Mutex<Vec<String>>>,
            first: bool,
        }
        impl crate::runtime::SynthEngine for Fake {
            fn synthesize(
                &mut self,
                text: &str,
                speed: Option<f32>,
                _: &AtomicBool,
            ) -> Result<Vec<f32>> {
                assert_eq!(speed, Some(1.0));
                self.calls.lock().unwrap().push(text.to_owned());
                if self.first {
                    self.first = false;
                    self.start.wait();
                }
                Ok(vec![0.5; 8000])
            }
            fn sample_rate(&self) -> u32 {
                8000
            }
        }

        #[test]
        fn concurrent_lanes_measure_every_chunk_once_with_played_pauses() {
            for lanes in 1..=3 {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let start = Arc::new(Barrier::new(lanes));
                let engines = (0..lanes)
                    .map(|_| {
                        Box::new(Fake {
                            start: start.clone(),
                            calls: calls.clone(),
                            first: true,
                        }) as Box<dyn crate::runtime::SynthEngine>
                    })
                    .collect();
                let chunks = (0..7)
                    .map(|index| handy_tts_core::text::TextChunk {
                        index,
                        text: index.to_string(),
                        source_start: 0,
                        source_end: 1,
                        pause_ms: 280,
                    })
                    .collect::<Vec<_>>();
                let (seconds, audio) = concurrent_batch(engines, &chunks).unwrap();
                assert!(seconds > 0.0);
                assert!((audio - 7.0 * 1.28).abs() < 1e-9);
                let mut calls = calls.lock().unwrap().clone();
                calls.sort();
                assert_eq!(
                    calls,
                    (0..7).map(|index| index.to_string()).collect::<Vec<_>>()
                );
            }
        }

        #[test]
        fn rss_uses_worker_resident_kibibytes() {
            assert_eq!(
                rss_mib("VmSize: 999999 kB\nVmRSS:\t2048 kB\n").unwrap(),
                2.0
            );
            for status in ["VmSize: 2048 kB", "VmRSS: nope kB", "VmRSS: 2048 MB"] {
                assert!(rss_mib(status).is_err());
            }
        }
    }
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
            let (seconds, calls) = measure(DOCUMENT.trim_start_matches("# "), 80, 0.2);
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
