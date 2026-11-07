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

fn edge_diagnostics(samples: &[f32], rate: u32) -> serde_json::Value {
    let first = samples
        .iter()
        .position(|s| s.abs() >= handy_tts_core::edges::THRESHOLD);
    let last = samples
        .iter()
        .rposition(|s| s.abs() >= handy_tts_core::edges::THRESHOLD);
    serde_json::json!({"samples": samples.len(), "sample_rate": rate,
        "lead_samples": first, "tail_samples": last.map(|i| samples.len() - i - 1),
        "all_quiet": first.is_none()})
}

fn join_diagnostics(
    a: &serde_json::Value,
    b: &serde_json::Value,
    rate: u32,
) -> Option<serde_json::Value> {
    let millis = |samples: u64| samples as f64 * 1000.0 / f64::from(rate);
    let pause = a["pause_samples"].as_u64()?;
    let tail = a["scheduled"]
        .as_u64()?
        .checked_sub(pause)?
        .checked_sub(a["last_loud"].as_u64()? + 1)?;
    let lead = b["first_loud"].as_u64()?;
    let dry = b["started_sample"]
        .as_u64()?
        .checked_sub(a["finished_sample"].as_u64()?)?;
    Some(
        serde_json::json!({"rendered_tail_ms": millis(tail), "rendered_lead_ms": millis(lead),
        "pause_ms": millis(pause), "dry_ms": millis(dry), "gap_ms": millis(tail + lead + pause + dry)}),
    )
}

fn measurement_chunks(corpus: &Path) -> Result<Vec<handy_tts_core::text::TextChunk>> {
    let source = std::fs::read_to_string(corpus).context("read explicit measurement corpus")?;
    let mut cursor = TextCursor::streaming(&source, false)?;
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
    let mut cursor = TextCursor::streaming(source, false)?;
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
            crate::diagnostic_file_name(path)
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

/// Runs the production session against an isolated worker and a real-time capturing feed.
pub fn continuity(
    id: &str,
    root: &Path,
    device: Device,
    cache: &Path,
    corpus: &Path,
    output: &Path,
    options: ContinuityOptions<'_>,
) -> Result<()> {
    let model = catalog::entry(id)?;
    let source = std::fs::read_to_string(corpus).context("read explicit continuity corpus")?;
    let vocabulary = options
        .technical
        .map(|_| std::sync::Arc::new(handy_tts_core::vocabulary::Vocabulary::builtin()));
    let mut cursor = TextCursor::streaming_with(
        &source,
        false,
        vocabulary.clone(),
        options.technical.unwrap_or_default(),
    )?;
    let mut pauses = Vec::new();
    let mut prepared_texts = Vec::new();
    let long_document = source.split_whitespace().count() >= 2000;
    let mut coverage_words = Vec::new();
    let mut coverage_end = 0;
    while let Some(chunk) = cursor.next_chunk()? {
        if !long_document || coverage_words.len() < 500 {
            coverage_words.extend(handy_tts_core::coverage::normalize_words(&chunk.text));
            coverage_end = pauses.len();
        }
        pauses.push(chunk.pause_ms);
        prepared_texts.push(chunk.text);
    }
    if long_document {
        ensure!(
            coverage_words.len() >= 500,
            "long corpus needs 500 prepared words"
        );
        coverage_words.truncate(500);
    }
    let expected_chunks = pauses.len();
    ensure!(expected_chunks > 0, "continuity corpus contains no speech");
    let ratings: handy_tts_core::device::Ratings = serde_json::from_str(include_str!(
        "../../../src-tauri/resources/tts/ratings.json"
    ))?;
    let rating = ratings
        .models
        .get(id)
        .and_then(|model| model.devices.get(&device))
        .context("continuity rating missing")?;
    ensure!(rating.supported, "unsupported continuity device");
    let before = crate::runtime::memory_available().context("read MemAvailable before load")?;
    let lanes = options.lanes.unwrap_or_else(|| {
        handy_tts_core::memory::lanes_for(
            before,
            (rating.lanes.base_rss_mib.unwrap_or(0.0) * 1048576.0) as u64,
            (rating.lanes.lane_rss_mib.unwrap_or(0.0) * 1048576.0) as u64,
            rating.lanes.best_lanes,
        )
    });
    ensure!((1..=3).contains(&lanes), "continuity lanes must be 1 to 3");
    let parent_before = stable_parent_samples()?;
    let began = Instant::now();
    let worker = ProcessWorker::start(id, root, device, cache, lanes)?;
    eprintln!("CONTINUITY_LOAD lanes={lanes} mem_available={before}");
    let load_seconds = began.elapsed().as_secs_f64();
    let selection = handy_tts_core::selection::Selection {
        model_id: id.into(),
        voice: model.default_voice.into(),
        speed: options.speed,
        pitch: 0.0,
        output_device: None,
        read_code: false,
        technical: options.technical.is_some(),
        full_paths: options.technical.unwrap_or_default().full_paths,
        full_links: options.technical.unwrap_or_default().full_links,
    };
    let capture = CaptureOutput::new(0, Some(options.wav))?;
    let mut session = handy_tts_core::session::Session::with_vocabulary(
        &source, selection, worker, capture, None, vocabulary,
    )
    .map_err(anyhow::Error::msg)?;
    session.play().map_err(anyhow::Error::msg)?;
    session
        .queue
        .estimate_from_rating(rating.rtf_1x.unwrap_or(0.0), 1.0, f64::from(options.speed));
    let dsp_timings = std::sync::Arc::new(std::sync::Mutex::new((None::<Instant>, Vec::new())));
    let dsp_observer = dsp_timings.clone();
    let dsp_origin = Instant::now();
    session.set_dsp_clock(move || {
        let now = Instant::now();
        if let Ok(mut timing) = dsp_observer.lock() {
            if let Some(start) = timing.0.take() {
                timing.1.push(now.duration_since(start).as_secs_f64());
            } else {
                timing.0 = Some(now);
            }
        }
        now.duration_since(dsp_origin).as_secs_f64()
    });
    drive_session(&mut session, true, options.startup_only)?;
    let snapshot = session.snapshot();
    let rate = session
        .playback
        .output
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("capture poisoned"))?
        .feed
        .sample_rate();
    session.playback.output.finish_wav()?;
    let mut capture = session
        .playback
        .output
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("capture poisoned"))?;
    let dsp_times = dsp_timings
        .lock()
        .map_err(|_| anyhow::anyhow!("DSP timing poisoned"))?;
    for (index, raw) in session.worker.records.iter().enumerate() {
        if let Some(chunk) = capture
            .chunks
            .iter_mut()
            .find(|c| c["index"] == raw["index"])
        {
            for key in [
                "synthesis_seconds",
                "trimmed_samples",
                "lead_removed",
                "tail_removed",
                "rendered_samples",
                "rendered_edges",
            ] {
                chunk[key] = raw[key].clone();
            }
            chunk["dsp_seconds"] = (*dsp_times.1.get(index).context("DSP timing missing")?).into();
        }
    }
    let chunks = aggregate_chunks(&capture.chunks, &pauses, rate, options.speed)?;
    let mut rtfs = chunks
        .iter()
        .map(|chunk| {
            let elapsed = chunk["synthesis_seconds"]
                .as_f64()
                .context("synthesis time missing")?
                + chunk["dsp_seconds"].as_f64().context("DSP time missing")?;
            let played = chunk["scheduled"]
                .as_u64()
                .context("scheduled samples missing")? as f64
                / f64::from(rate);
            Ok(elapsed / played)
        })
        .collect::<Result<Vec<_>>>()?;
    let median = quantile(&mut rtfs, 0.5)?;
    let worker_peak = session.worker.peak_rss;
    let combined_growth = session.worker.combined_growth;
    let rss = session.worker.rss_samples.clone();
    let worker_before = rss_mib(&std::fs::read_to_string(format!(
        "/proc/{}/status",
        session.worker.child.id()
    ))?)?;
    session.worker.shutdown()?;
    let worker_exited = session.worker.child.try_wait()?.is_some();
    let parent_after = stable_parent_samples()?;
    let cleanup =
        cleanup_memory_record(&parent_before, &parent_after, worker_before, worker_exited);
    let coverage_wav = if long_document && !options.startup_only {
        let samples =
            chunks.get(coverage_end).context("coverage chunk missing")?["finished_sample"]
                .as_u64()
                .context("coverage sample position missing")?;
        write_coverage_prefix(options.wav, samples)?
    } else {
        options.wav.to_path_buf()
    };
    let after = crate::runtime::memory_available().context("read MemAvailable after unload")?;
    let mut record = serde_json::json!({
        "schema_version": 1, "startup_only": options.startup_only, "model": id, "device": device, "speed": options.speed,
        "lanes": lanes, "corpus": corpus, "words": source.split_whitespace().count(),
        "sample_rate": rate, "expected_chunks": expected_chunks, "wav": options.wav,
        "chunks": chunks, "segments": capture.chunks, "source_pauses_ms": pauses, "buffering_pauses": snapshot.buffering_pauses,
        "buffering_seconds": session.queue.buffering_seconds,
        "underrun_samples": capture.feed.underrun_samples(),
        "head_start_seconds": capture.chunks.first().and_then(|r| r["started_seconds"].as_f64()), "load_seconds": load_seconds,
        "worker_peak_rss_mib": worker_peak, "combined_growth_mib": combined_growth,
        "rss_samples": rss, "cleanup": cleanup,
        "mem_available_before_bytes": before, "mem_available_after_bytes": after,
        "coverage_input": {"model": id, "device": device, "speed": options.speed,
            "corpus": corpus, "scope": if long_document { "first_500_words" } else { "owner" },
            "wav": coverage_wav, "source_wav": options.wav, "expected_words": coverage_words},
        "unload_delta_mib": (before as f64 - after as f64) / 1048576.0,
        "median_rtf": median,
        "requests": session.worker.records,
    });
    if let Some(technical) = options.technical {
        record["technical"] = serde_json::json!({
            "full_paths": technical.full_paths, "full_links": technical.full_links,
            "prepared_texts": prepared_texts,
        });
    }
    if let Some(directory) = options.gpu_inputs {
        ensure!(
            id == "supertonic-3" && device == Device::Gpu,
            "GPU timing inputs require Supertonic GPU"
        );
        record["gpu"] = gpu_repeat_timings(id, root, device, cache, directory, lanes)?;
    }
    std::fs::write(output, serde_json::to_vec_pretty(&record)?)
        .context("write continuity evidence")?;
    println!(
        "CONTINUITY model={id} device={device:?} speed={} lanes={lanes} chunks={expected_chunks} head_start={} buffering={} underrun={} median_rtf={median}",
        options.speed,
        snapshot.head_start_seconds.unwrap_or(0.0),
        snapshot.buffering_pauses,
        capture.feed.underrun_samples()
    );
    if options.require_long_document {
        let pass = source.split_whitespace().count() >= 2000
            && snapshot.buffering_pauses == 0
            && capture.feed.underrun_samples() == 0;
        println!("LONG_DOCUMENT_GATE pass={pass}");
        ensure!(pass, "continuity long-document gate failed");
    }
    Ok(())
}

fn stable_parent_samples() -> Result<Vec<f64>> {
    let mut samples = Vec::new();
    for _ in 0..3 {
        samples.push(resident_memory()?);
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(samples)
}

fn cleanup_memory_record(
    before: &[f64],
    after: &[f64],
    worker: f64,
    exited: bool,
) -> serde_json::Value {
    serde_json::json!({"worker_exited": exited, "worker_rss_before_mib": worker,
        "worker_rss_after_mib": if exited { 0.0 } else { worker },
        "parent_before_mib": before, "parent_after_mib": after})
}

fn write_coverage_prefix(source: &Path, samples: u64) -> Result<std::path::PathBuf> {
    use std::io::{Read, Write};
    let stem = source
        .file_stem()
        .context("coverage WAV stem missing")?
        .to_string_lossy();
    let path = source.with_file_name(format!("{stem}-first-500.wav"));
    let mut input = std::fs::File::open(source).context("open delivered coverage WAV")?;
    let mut header = [0; 44];
    input.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"RIFF" && &header[36..40] == b"data",
        "unexpected capture WAV header"
    );
    let bytes = u32::try_from(samples * 4).context("coverage WAV exceeds size limit")?;
    header[4..8].copy_from_slice(&(bytes + 36).to_le_bytes());
    header[40..44].copy_from_slice(&bytes.to_le_bytes());
    let mut output = std::fs::File::create(&path).context("create delivered coverage prefix")?;
    output.write_all(&header)?;
    ensure!(
        std::io::copy(&mut input.take(u64::from(bytes)), &mut output)? == u64::from(bytes),
        "coverage prefix truncated"
    );
    Ok(path)
}

/// Explicit output and concurrency controls for a continuity measurement.
pub struct ContinuityOptions<'a> {
    pub speed: f32,
    pub wav: &'a Path,
    pub lanes: Option<usize>,
    pub gpu_inputs: Option<&'a Path>,
    pub require_long_document: bool,
    pub startup_only: bool,
    /// Technical reading with the built-in vocabulary and these options; `None` reads as before.
    pub technical: Option<handy_tts_core::vocabulary::TechnicalOptions>,
}

/// Splits the valueless technical reading flags out of the continuity options: `--technical`
/// turns it on with the built-in vocabulary, `--short-paths` and `--full-links` change its
/// options and need `--technical`. Returns the options and the remaining arguments.
pub fn technical_flags(
    args: &[String],
) -> Result<(
    Option<handy_tts_core::vocabulary::TechnicalOptions>,
    Vec<String>,
)> {
    let (mut technical, mut short_paths, mut full_links) = (false, false, false);
    let mut rest = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--technical" => technical = true,
            "--short-paths" => short_paths = true,
            "--full-links" => full_links = true,
            _ => rest.push(arg.clone()),
        }
    }
    ensure!(
        technical || !(short_paths || full_links),
        "--short-paths and --full-links need --technical"
    );
    let options = technical.then_some(handy_tts_core::vocabulary::TechnicalOptions {
        full_paths: !short_paths,
        full_links,
    });
    Ok((options, rest))
}

fn gpu_repeat_timings(
    id: &str,
    root: &Path,
    device: Device,
    cache: &Path,
    directory: &Path,
    lanes: usize,
) -> Result<serde_json::Value> {
    let model = catalog::entry(id)?;
    let began = Instant::now();
    let mut engines = crate::runtime::load_adapters(
        id,
        root,
        model.default_voice,
        model.threads,
        device,
        cache,
        lanes,
    )?;
    let engine = &mut engines[0];
    let cached_load_seconds = began.elapsed().as_secs_f64();
    let mut first = Vec::new();
    let mut repeat = Vec::new();
    for (_, source) in first_audio_documents(directory)? {
        let mut cursor = TextCursor::streaming(&source, false)?;
        while let Some(chunk) = cursor.next_chunk()? {
            for times in [&mut first, &mut repeat] {
                let start = Instant::now();
                crate::runtime::synthesize_bounded(
                    engine.as_mut(),
                    &chunk.text,
                    Some(1.0),
                    &AtomicBool::new(false),
                )?;
                times.push(start.elapsed().as_secs_f64());
            }
        }
    }
    let requests = first.len();
    Ok(
        serde_json::json!({"corpus_documents": 7, "requests": requests,
        "first_p95_s": quantile(&mut first.clone(), 0.95)?, "repeat_p95_s": quantile(&mut repeat.clone(), 0.95)?,
        "cached_load_seconds": cached_load_seconds, "first_seconds": first, "repeat_seconds": repeat}),
    )
}

fn aggregate_chunks(
    segments: &[serde_json::Value],
    pauses: &[u32],
    rate: u32,
    requested: f32,
) -> Result<Vec<serde_json::Value>> {
    let mut chunks = Vec::new();
    let mut position = 0;
    while position < segments.len() {
        let first = &segments[position];
        let index = first["index"].as_u64().context("segment index missing")? as usize;
        ensure!(index < pauses.len(), "part without a sentence");
        let begin = position;
        position += 1;
        while position < segments.len()
            && segments[position]["index"]
                .as_u64()
                .context("part index missing")? as usize
                >= pauses.len()
        {
            position += 1;
        }
        let last = &segments[position - 1];
        let mut row = first.clone();
        let pause = handy_tts_core::edges::pause_samples(pauses[index], requested, rate);
        let speech = first["rendered_samples"]
            .as_u64()
            .context("rendered speech length missing")?;
        row["scheduled"] = (speech + pause as u64).into();
        row["pause_samples"] = pause.into();
        row["delivered"] = segments[begin..position]
            .iter()
            .map(|s| s["delivered"].as_u64().context("delivered samples missing"))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .sum::<u64>()
            .into();
        row["finished_sample"] = last["finished_sample"].clone();
        row["gap_after_ms"] = last["gap_after_ms"].clone();
        row["join"] = last["join"].clone();
        let start = first["started_sample"].as_u64();
        let loud_position = |segment: &serde_json::Value, key: &str| {
            segment[key]
                .as_u64()
                .zip(segment["started_sample"].as_u64())
                .zip(start)
                .map(|((loud, offset), start)| offset - start + loud)
        };
        row["first_loud"] = segments[begin..position]
            .iter()
            .find_map(|s| loud_position(s, "first_loud"))
            .into();
        row["last_loud"] = segments[begin..position]
            .iter()
            .rev()
            .find_map(|s| loud_position(s, "last_loud"))
            .into();
        row["all_quiet"] = row["first_loud"].is_null().into();
        row["segment_indices"] = segments[begin..position]
            .iter()
            .map(|s| s["index"].clone())
            .collect::<Vec<_>>()
            .into();
        chunks.push(row);
    }
    for index in 0..chunks.len().saturating_sub(1) {
        let a = &chunks[index];
        let b = &chunks[index + 1];
        let gap = a["last_loud"]
            .as_u64()
            .zip(a["started_sample"].as_u64())
            .zip(b["first_loud"].as_u64().zip(b["started_sample"].as_u64()))
            .map(|((last, end), (first, start))| {
                (start + first).saturating_sub(end + last + 1) as f64 * 1000.0 / f64::from(rate)
            });
        chunks[index]["gap_after_ms"] = gap.into();
    }
    Ok(chunks)
}

struct TimedReply {
    id: u64,
    pcm: Result<Pcm, String>,
    received: Instant,
}

struct ProcessWorker {
    child: std::process::Child,
    input: std::process::ChildStdin,
    replies: std::sync::mpsc::Receiver<Result<TimedReply, String>>,
    reader: Option<std::thread::JoinHandle<()>>,
    pending: std::collections::BTreeMap<u64, (handy_tts_core::scheduler::Ticket, Instant)>,
    next_id: u64,
    lanes: usize,
    records: Vec<serde_json::Value>,
    peak_rss: f64,
    combined_baseline: Option<f64>,
    combined_growth: f64,
    rss_samples: Vec<serde_json::Value>,
    last_sample: Instant,
}
impl ProcessWorker {
    fn start(id: &str, root: &Path, device: Device, cache: &Path, lanes: usize) -> Result<Self> {
        use handy_tts_core::protocol::{Request, Status, read_header, write_request};
        let model = catalog::entry(id)?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--model", id])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .env("HANDY_TTS_TRACE_TIMINGS", "1")
            .spawn()
            .context("spawn continuity worker")?;
        let result = (|| -> Result<_> {
            let mut input = child.stdin.take().context("worker stdin missing")?;
            let mut output =
                std::io::BufReader::new(child.stdout.take().context("worker stdout missing")?);
            write_request(
                &mut input,
                &Request::Load {
                    model_dir: root.into(),
                    voice: model.default_voice.into(),
                    threads: model.threads,
                    lanes,
                    device,
                    cache_dir: cache.into(),
                },
            )?;
            let ready = read_header(&mut output)?;
            ensure!(
                ready.status == Status::Ready && ready.device == Some(device),
                "continuity load failed: {:?}",
                ready.message
            );
            let (sender, replies) = std::sync::mpsc::sync_channel(lanes);
            let reader = std::thread::spawn(move || {
                loop {
                    let result = handy_tts_core::protocol::read_header(&mut output)
                        .map(|header| {
                            let id = header.id;
                            let pcm = handy_tts_core::protocol::read_pcm_after_header(
                                &mut output,
                                header,
                                id,
                            )
                            .map_err(|error| error.to_string());
                            TimedReply {
                                id,
                                pcm,
                                received: Instant::now(),
                            }
                        })
                        .map_err(|error| error.to_string());
                    let fatal = result.is_err();
                    if sender.send(result).is_err() || fatal {
                        break;
                    }
                }
            });
            Ok((input, replies, reader))
        })();
        match result {
            Ok((input, replies, reader)) => Ok(Self {
                child,
                input,
                replies,
                reader: Some(reader),
                pending: Default::default(),
                next_id: 1,
                lanes,
                records: Vec::new(),
                peak_rss: 0.0,
                combined_baseline: None,
                combined_growth: 0.0,
                rss_samples: Vec::new(),
                last_sample: Instant::now(),
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
    fn sample_memory(&mut self, elapsed: f64, started: bool) -> Result<()> {
        if self.last_sample.elapsed() < Duration::from_millis(100) {
            return Ok(());
        }
        self.last_sample = Instant::now();
        let worker = rss_mib(&std::fs::read_to_string(format!(
            "/proc/{}/status",
            self.child.id()
        ))?)?;
        let parent = resident_memory()?;
        self.peak_rss = self.peak_rss.max(worker);
        if started {
            let baseline = *self.combined_baseline.get_or_insert(parent + worker);
            self.combined_growth = self.combined_growth.max(parent + worker - baseline);
        }
        self.rss_samples.push(
            serde_json::json!({"seconds": elapsed, "worker_mib": worker, "parent_mib": parent}),
        );
        Ok(())
    }
    fn shutdown(&mut self) -> Result<()> {
        handy_tts_core::protocol::write_request(
            &mut self.input,
            &handy_tts_core::protocol::Request::Shutdown,
        )?;
        for _ in 0..=self.lanes + 1 {
            match self.replies.recv_timeout(Duration::from_secs(60)) {
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!("continuity shutdown timed out")
                }
            }
        }
        ensure!(self.child.wait()?.success(), "continuity worker failed");
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("continuity reader panicked"))?;
        }
        Ok(())
    }
}
impl Drop for ProcessWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl handy_tts_core::session::Worker for ProcessWorker {
    fn rendered(&mut self, ticket: handy_tts_core::scheduler::Ticket, pcm: &Pcm) {
        if let Some(record) = self.records.iter_mut().find(|r| r["index"] == ticket.index) {
            record["rendered_samples"] = pcm.samples.len().into();
            record["rendered_edges"] = edge_diagnostics(&pcm.samples, pcm.sample_rate);
        }
    }
    fn request(
        &mut self,
        ticket: handy_tts_core::scheduler::Ticket,
        text: &str,
        selection: &handy_tts_core::selection::Selection,
    ) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        eprintln!(
            "CONTINUITY_REQUEST id={id} index={} chars={} pending={} lanes={}",
            ticket.index,
            text.chars().count(),
            self.pending.len(),
            self.lanes
        );
        handy_tts_core::protocol::write_request(
            &mut self.input,
            &handy_tts_core::protocol::Request::Synthesize {
                id,
                text: text.into(),
                speed: Some(selection.speed),
            },
        )
        .map_err(|e| e.to_string())?;
        self.pending.insert(id, (ticket, Instant::now()));
        Ok(())
    }
    fn poll(&mut self) -> Option<handy_tts_core::session::Completion> {
        let (id, result, received) = match self.replies.try_recv().ok()? {
            Ok(reply) => (reply.id, reply.pcm, reply.received),
            Err(error) => (*self.pending.keys().next()?, Err(error), Instant::now()),
        };
        let (ticket, began) = self.pending.remove(&id)?;
        let synthesis_seconds = received.duration_since(began).as_secs_f64();
        if let Ok(pcm) = &result {
            let raw = edge_diagnostics(&pcm.samples, pcm.sample_rate);
            let trim = handy_tts_core::edges::trim(pcm.samples.clone(), pcm.sample_rate);
            let trimmed = edge_diagnostics(&trim.samples, pcm.sample_rate);
            self.records.push(serde_json::json!({"index": ticket.index, "synthesis_seconds": synthesis_seconds,
                "trimmed_samples": trim.samples.len(), "lead_removed": trim.lead_removed, "tail_removed": trim.tail_removed,
                "raw_edges": raw, "trimmed_edges": trimmed}));
        }
        Some(handy_tts_core::session::Completion {
            ticket,
            result,
            synthesis_seconds,
        })
    }
    fn available_lanes(&mut self) -> Option<usize> {
        if crate::runtime::memory_available()
            .is_ok_and(|m| m < handy_tts_core::memory::LANE_RESERVE_BYTES)
            && self.lanes > 1
        {
            eprintln!("CONTINUITY_LANES lower_to=1");
            self.lanes = 1;
            let _ = handy_tts_core::protocol::write_request(
                &mut self.input,
                &handy_tts_core::protocol::Request::SetLanes { lanes: 1 },
            );
        }
        Some(self.lanes)
    }
    fn cancel(&mut self, ticket: handy_tts_core::scheduler::Ticket) {
        if let Some((&id, _)) = self.pending.iter().find(|(_, (t, _))| *t == ticket) {
            let _ = handy_tts_core::protocol::write_request(
                &mut self.input,
                &handy_tts_core::protocol::Request::Cancel { id },
            );
        }
    }
    fn selection_changed(&mut self) {}
}

struct CaptureState {
    feed: handy_tts_core::feed::Feed,
    chunks: Vec<serde_json::Value>,
    events: Vec<handy_tts_core::playback::segment::OutputEvent>,
    paused: bool,
    position: u64,
    first_started: bool,
    origin: Option<Instant>,
    terminal: bool,
    wav: Option<std::io::BufWriter<std::fs::File>>,
}
impl CaptureState {
    fn new(rate: u32, path: Option<&Path>) -> Result<Self> {
        use std::io::Write;
        let mut wav = path
            .map(std::fs::File::create)
            .transpose()?
            .map(std::io::BufWriter::new);
        if let Some(writer) = &mut wav {
            writer.write_all(&[0; 44])?;
        }
        Ok(Self {
            feed: handy_tts_core::feed::Feed::new(rate),
            chunks: Vec::new(),
            events: Vec::new(),
            paused: false,
            position: 0,
            first_started: false,
            origin: None,
            terminal: false,
            wav,
        })
    }
    fn pull(&mut self, count: usize) -> Result<()> {
        use handy_tts_core::feed::FeedEvent;
        use std::io::Write;
        if self.paused || self.chunks.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(count * 4);
        for _ in 0..count {
            if self.terminal && self.chunks.iter().all(|c| c["delivered"] == c["scheduled"]) {
                break;
            }
            let mut sample = [0.0];
            self.feed.pull(&mut sample);
            for event in self.feed.drain_events() {
                match event {
                    FeedEvent::Started(index) => {
                        self.first_started = true;
                        let position = self
                            .chunks
                            .iter()
                            .position(|r| r["index"] == index)
                            .context("unrecorded segment started")?;
                        let row = &mut self.chunks[position];
                        row["started_sample"] = self.position.into();
                        row["started_seconds"] =
                            self.origin.map(|t| t.elapsed().as_secs_f64()).into();
                        if position > 0 {
                            let join = join_diagnostics(
                                &self.chunks[position - 1],
                                &self.chunks[position],
                                self.feed.sample_rate(),
                            );
                            eprintln!(
                                "CONTINUITY_JOIN before={} next={index} components={}",
                                self.chunks[position - 1]["index"],
                                serde_json::to_string(&join)?
                            );
                        }
                    }
                    FeedEvent::Finished { index, delivered } => {
                        let row = self
                            .chunks
                            .iter_mut()
                            .find(|r| r["index"] == index)
                            .context("unrecorded segment finished")?;
                        row["finished_sample"] = (self.position + 1).into();
                        row["delivered"] = delivered.into();
                    }
                    FeedEvent::Underrun => {}
                }
                self.events.push(event.into());
            }
            bytes.extend_from_slice(&sample[0].to_le_bytes());
            self.position += 1;
        }
        if let Some(wav) = &mut self.wav {
            wav.write_all(&bytes)?;
        }
        Ok(())
    }
    fn finish_wav(&mut self) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        for index in 0..self.chunks.len().saturating_sub(1) {
            let a = &self.chunks[index];
            let b = &self.chunks[index + 1];
            if a["started_sample"].is_null() || b["started_sample"].is_null() {
                continue;
            }
            let end = a["started_sample"]
                .as_u64()
                .context("chunk start missing")?;
            let start = b["started_sample"]
                .as_u64()
                .context("chunk start missing")?;
            let gap = a["last_loud"]
                .as_u64()
                .zip(b["first_loud"].as_u64())
                .map(|(last, first)| {
                    (start + first).saturating_sub(end + last + 1) as f64 * 1000.0
                        / f64::from(self.feed.sample_rate())
                });
            self.chunks[index]["gap_after_ms"] = gap.into();
            self.chunks[index]["join"] = join_diagnostics(
                &self.chunks[index],
                &self.chunks[index + 1],
                self.feed.sample_rate(),
            )
            .into();
        }
        if let Some(wav) = &mut self.wav {
            let bytes =
                u32::try_from(self.position * 4).context("capture exceeds WAV size limit")?;
            let rate = self.feed.sample_rate();
            wav.seek(SeekFrom::Start(0))?;
            wav.write_all(b"RIFF")?;
            wav.write_all(&(bytes + 36).to_le_bytes())?;
            wav.write_all(b"WAVEfmt ")?;
            wav.write_all(&16u32.to_le_bytes())?;
            wav.write_all(&3u16.to_le_bytes())?;
            wav.write_all(&1u16.to_le_bytes())?;
            wav.write_all(&rate.to_le_bytes())?;
            wav.write_all(&(rate * 4).to_le_bytes())?;
            wav.write_all(&4u16.to_le_bytes())?;
            wav.write_all(&32u16.to_le_bytes())?;
            wav.write_all(b"data")?;
            wav.write_all(&bytes.to_le_bytes())?;
            wav.flush()?;
        }
        Ok(())
    }
}
struct CaptureOutput {
    state: std::sync::Arc<std::sync::Mutex<CaptureState>>,
}
impl CaptureOutput {
    fn new(rate: u32, path: Option<&Path>) -> Result<Self> {
        Ok(Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(CaptureState::new(rate, path)?)),
        })
    }
    fn finish_wav(&mut self) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("capture poisoned"))?
            .finish_wav()
    }
}
impl handy_tts_core::playback::segment::Output for CaptureOutput {
    type Prepared = ();
    fn names(&self) -> Result<Vec<String>, handy_tts_core::playback::PlaybackError> {
        Ok(Vec::new())
    }
    fn open(&mut self, _: Option<&str>) -> Result<(), handy_tts_core::playback::PlaybackError> {
        Ok(())
    }
    fn prepare(&mut self, _: Option<&str>) -> Result<(), handy_tts_core::playback::PlaybackError> {
        Ok(())
    }
    fn commit(&mut self, _: ()) {}
    fn set_sample_rate(
        &mut self,
        rate: u32,
    ) -> Result<(), handy_tts_core::playback::PlaybackError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| handy_tts_core::playback::PlaybackError::Unavailable)?;
        if state.feed.sample_rate() == 0 {
            state.feed = handy_tts_core::feed::Feed::new(rate);
        }
        if rate != state.feed.sample_rate() {
            return Err(handy_tts_core::playback::PlaybackError::Unavailable);
        }
        Ok(())
    }
    fn append(
        &mut self,
        segment: handy_tts_core::feed::Segment,
    ) -> Result<(), handy_tts_core::playback::PlaybackError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| handy_tts_core::playback::PlaybackError::Unavailable)?;
        let first = segment
            .speech
            .iter()
            .position(|s| s.abs() >= handy_tts_core::edges::THRESHOLD);
        let last = segment
            .speech
            .iter()
            .rposition(|s| s.abs() >= handy_tts_core::edges::THRESHOLD);
        state.chunks.push(serde_json::json!({"index": segment.index, "scheduled": segment.samples(),
            "delivered": 0, "pause_samples": segment.pause, "first_loud": first, "last_loud": last, "all_quiet": first.is_none()}));
        state.feed.push(segment);
        Ok(())
    }
    fn events(&mut self) -> Vec<handy_tts_core::playback::segment::OutputEvent> {
        self.state
            .lock()
            .map(|mut s| std::mem::take(&mut s.events))
            .unwrap_or_default()
    }
    fn retract_unstarted(&mut self) -> Vec<handy_tts_core::feed::Segment> {
        self.state
            .lock()
            .map(|mut s| s.feed.retract_unstarted())
            .unwrap_or_default()
    }
    fn content_samples(&self) -> u64 {
        self.state
            .lock()
            .map(|s| s.feed.content_samples())
            .unwrap_or(0)
    }
    fn pause(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.paused = true;
        }
    }
    fn resume(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.paused = false;
        }
    }
    fn stop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.feed.clear(0);
            s.paused = false;
        }
    }
}

trait CaptureWorker: handy_tts_core::session::Worker {
    fn sample_memory(&mut self, _elapsed: f64, _started: bool) -> Result<()> {
        Ok(())
    }
}
impl CaptureWorker for ProcessWorker {
    fn sample_memory(&mut self, elapsed: f64, started: bool) -> Result<()> {
        ProcessWorker::sample_memory(self, elapsed, started)
    }
}

fn drive_session<W: CaptureWorker>(
    session: &mut handy_tts_core::session::Session<'_, W, CaptureOutput>,
    real_time: bool,
    startup_only: bool,
) -> Result<()> {
    use handy_tts_core::scheduler::PlaybackState;
    let origin = Instant::now();
    let state = session.playback.output.state.clone();
    state
        .lock()
        .map_err(|_| anyhow::anyhow!("capture poisoned"))?
        .origin = Some(origin);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mixer = real_time.then(|| {
            scope.spawn(|| -> Result<()> {
                for step in 0..720_000u64 {
                    if done.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    let mut capture = state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("capture poisoned"))?;
                    let rate = u64::from(capture.feed.sample_rate());
                    let count = ((step + 1) * rate / 200 - step * rate / 200) as usize;
                    capture.pull(count)?;
                    drop(capture);
                    std::thread::sleep(
                        (origin + Duration::from_millis((step + 1) * 5))
                            .saturating_duration_since(Instant::now()),
                    );
                }
                anyhow::bail!("capture sample clock timed out")
            })
        });
        let result = (|| -> Result<()> {
            session.play().map_err(anyhow::Error::msg)?;
            let mut last_progress = 0.0;
            let mut last_queue = None;
            for step in 0..1_800_000u64 {
                let now = if real_time {
                    origin.elapsed().as_secs_f64()
                } else {
                    step as f64 * 0.005
                };
                if now >= 3600.0 {
                    break;
                }
                session.tick(now).map_err(anyhow::Error::msg)?;
                let snapshot = session.snapshot();
                let queue_key = (
                    snapshot.state,
                    session.queue.next_index,
                    session.queue.observations,
                    session.queue.in_flight.len(),
                    session.queue.waiting.len(),
                    session.queue.ready.len(),
                );
                if real_time && last_queue != Some(queue_key) {
                    let chunks = session
                        .queue
                        .playing
                        .iter()
                        .chain(&session.queue.waiting)
                        .chain(session.queue.ready.values());
                    let (native, rendered) = chunks.fold((0usize, 0usize), |(n, r), a| {
                        (n + a.native.len(), r + a.pcm.samples.len())
                    });
                    eprintln!(
                        "CONTINUITY_QUEUE seconds={now:.6} state={:?} next={} lanes={} in_flight={} queued={} stored={} native_samples={native} rendered_samples={rendered} rtf={} head={} read_ahead={}",
                        snapshot.state,
                        session.queue.next_index,
                        session.queue.lanes,
                        session.queue.in_flight.len(),
                        session.queue.queued_seconds(),
                        session.queue.stored_seconds(),
                        session.queue.rtf_played,
                        session.queue.target_seconds(),
                        session.queue.read_ahead_seconds()
                    );
                    last_queue = Some(queue_key);
                }
                ensure!(
                    snapshot.state != PlaybackState::Error,
                    "continuity session failed: {:?}",
                    snapshot.error
                );
                let started = {
                    let mut capture = state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("capture poisoned"))?;
                    capture.terminal = session.queue.source_finished
                        && session.queue.in_flight.is_empty()
                        && session.queue.ready.is_empty()
                        && session.queue.waiting.is_empty();
                    capture.first_started
                };
                if real_time && now - last_progress >= 30.0 {
                    let capture = state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("capture poisoned"))?;
                    let finished = capture
                        .chunks
                        .iter()
                        .filter(|chunk| chunk["delivered"] == chunk["scheduled"])
                        .count();
                    eprintln!(
                        "CONTINUITY_PROGRESS elapsed={now:.3} state={:?} source_start={} source_finished={} in_flight={} ready={} waiting={} segments={} finished={finished} buffering={} underrun={} first_started={:?}",
                        snapshot.state,
                        snapshot.source_start,
                        session.queue.source_finished,
                        session.queue.in_flight.len(),
                        session.queue.ready.len(),
                        session.queue.waiting.len(),
                        capture.chunks.len(),
                        snapshot.buffering_pauses,
                        capture.feed.underrun_samples(),
                        capture
                            .chunks
                            .first()
                            .and_then(|c| c["started_seconds"].as_f64()),
                    );
                    last_progress = now;
                }
                session.worker.sample_memory(now, started)?;
                if snapshot.state == PlaybackState::Finished || (startup_only && started) {
                    return Ok(());
                }
                if real_time {
                    std::thread::sleep(Duration::from_millis(2));
                } else {
                    let mut capture = state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("capture poisoned"))?;
                    let rate = u64::from(capture.feed.sample_rate());
                    capture.pull(((step + 1) * rate / 200 - step * rate / 200) as usize)?;
                }
            }
            anyhow::bail!("continuity session timed out")
        })();
        done.store(true, Ordering::SeqCst);
        if let Some(mixer) = mixer {
            mixer
                .join()
                .map_err(|_| anyhow::anyhow!("capture mixer panicked"))??;
        }
        result
    })
}
#[cfg(test)]
mod measurement_tests {
    use super::*;
    #[test]
    fn edge_diagnostics_separate_quiet_edges_from_speech_and_keep_quiet_chunks() {
        let edges = edge_diagnostics(&[0.0, 0.0, 0.5, 0.0, 0.5, 0.0], 8000);
        assert_eq!(edges["lead_samples"], 2);
        assert_eq!(edges["tail_samples"], 1);
        assert_eq!(edges["samples"], 6);
        assert_eq!(edges["all_quiet"], false);
        assert_eq!(edge_diagnostics(&[0.0; 4], 8000)["all_quiet"], true);
    }
    #[test]
    fn join_diagnostics_account_for_rendered_edges_pause_and_dry_interval() {
        let a = serde_json::json!({"started_sample": 0, "finished_sample": 1000, "scheduled": 1000, "pause_samples": 200, "last_loud": 719});
        let b = serde_json::json!({"started_sample": 1080, "first_loud": 80});
        let join = join_diagnostics(&a, &b, 8000).unwrap();
        assert_eq!(join["rendered_tail_ms"], 10.0);
        assert_eq!(join["rendered_lead_ms"], 10.0);
        assert_eq!(join["pause_ms"], 25.0);
        assert_eq!(join["dry_ms"], 10.0);
        assert_eq!(join["gap_ms"], 55.0);
        assert!(join_diagnostics(&a, &serde_json::json!({}), 8000).is_none());
    }
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

#[cfg(test)]
mod continuity_tests {
    use super::*;
    use handy_tts_core::{
        scheduler::Ticket,
        selection::Selection,
        session::{Completion, Session, Worker},
    };
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeWorker(VecDeque<Ticket>);
    impl CaptureWorker for FakeWorker {}
    impl Worker for FakeWorker {
        fn request(&mut self, ticket: Ticket, _: &str, _: &Selection) -> Result<(), String> {
            self.0.push_front(ticket);
            Ok(())
        }
        fn poll(&mut self) -> Option<Completion> {
            self.0.pop_front().map(|ticket| Completion {
                ticket,
                result: Ok(Pcm {
                    id: ticket.index as u64 + 1,
                    sample_rate: 8000,
                    samples: vec![0.5; 8000],
                }),
                synthesis_seconds: 0.01,
            })
        }
        fn available_lanes(&mut self) -> Option<usize> {
            Some(3)
        }
        fn cancel(&mut self, _: Ticket) {}
        fn selection_changed(&mut self) {}
    }

    #[test]
    fn continuity_record_contains_every_chunk_once() {
        let source = include_str!("../../handy-tts-core/tests/fixtures/speech/owner-2x.txt");
        let selection = Selection {
            model_id: "supertonic-3".into(),
            voice: "F1".into(),
            speed: 2.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
            technical: false,
            full_paths: true,
            full_links: false,
        };
        let mut session = Session::new(
            source,
            selection,
            FakeWorker::default(),
            CaptureOutput::new(8000, None).unwrap(),
            None,
        )
        .unwrap();
        session.set_dsp_clock(|| 0.0);
        drive_session(&mut session, false, false).unwrap();
        let capture = session.playback.output.state.lock().unwrap();
        let rows = &capture.chunks;
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .map(|r| r["index"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        for row in rows {
            assert_eq!(row["scheduled"], row["delivered"]);
            assert_eq!(
                row["finished_sample"].as_u64().unwrap() - row["started_sample"].as_u64().unwrap(),
                row["scheduled"].as_u64().unwrap()
            );
        }
        assert_eq!(capture.feed.underrun_samples(), 0);
        drop(capture);
        assert!(session.snapshot().head_start_seconds.unwrap() > 0.0);
    }
}

#[cfg(test)]
mod continuity_parts_tests {
    use super::*;

    #[test]
    fn split_segments_form_one_complete_sentence_and_missing_tail_is_detected() {
        let segments = vec![
            serde_json::json!({"index":0,"scheduled":1000,"delivered":1000,"started_sample":0,"finished_sample":1000,"first_loud":0,"last_loud":999,"pause_samples":0,"trimmed_samples":1600,"rendered_samples":1400,"synthesis_seconds":0.1,"dsp_seconds":0.0}),
            serde_json::json!({"index":100,"scheduled":440,"delivered":440,"started_sample":1000,"finished_sample":1440,"first_loud":0,"last_loud":399,"pause_samples":40,"gap_after_ms":60.0}),
            serde_json::json!({"index":1,"scheduled":1880,"delivered":1880,"started_sample":1440,"finished_sample":3320,"first_loud":20,"last_loud":1799,"pause_samples":80,"trimmed_samples":2000,"rendered_samples":1800,"synthesis_seconds":0.1,"dsp_seconds":0.0}),
        ];
        let result = aggregate_chunks(&segments, &[40, 80], 1000, 1.0).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["index"], 0);
        assert_eq!(result[0]["scheduled"], 1440);
        assert_eq!(result[0]["delivered"], 1440);
        assert_eq!(result[0]["finished_sample"], 1440);
        assert_eq!(result[0]["gap_after_ms"], 60.0);
        let missing = aggregate_chunks(
            &[segments[0].clone(), segments[2].clone()],
            &[40, 80],
            1000,
            1.0,
        )
        .unwrap();
        assert_ne!(missing[0]["scheduled"], missing[0]["delivered"]);
    }
}

#[cfg(test)]
mod quiet_capture_tests {
    use super::*;
    use handy_tts_core::{feed::Segment, playback::segment::Output};

    #[test]
    fn quiet_segment_keeps_samples_and_reports_an_unmeasurable_gap() {
        let mut output = CaptureOutput::new(8000, None).unwrap();
        for (index, value) in [0.0, 0.5].into_iter().enumerate() {
            output
                .append(Segment {
                    epoch: 0,
                    index,
                    speech: vec![value; 800],
                    pause: 0,
                })
                .unwrap();
        }
        let mut state = output.state.lock().unwrap();
        state.terminal = true;
        state.pull(1600).unwrap();
        state.finish_wav().unwrap();
        assert_eq!(state.chunks[0]["delivered"], 800);
        assert!(state.chunks[0]["gap_after_ms"].is_null());
        assert_eq!(state.feed.content_samples(), 1600);
    }
}

#[cfg(test)]
mod cleanup_measurement_tests {
    use super::*;

    #[test]
    fn coverage_prefix_contains_only_delivered_samples_through_the_selected_chunk() {
        use handy_tts_core::{feed::Segment, playback::segment::Output};
        let scratch = std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap();
        let directory = std::path::PathBuf::from(scratch)
            .join(format!("s15-coverage-prefix-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("delivered.wav");
        let mut output = CaptureOutput::new(8000, Some(&path)).unwrap();
        output
            .append(Segment {
                epoch: 0,
                index: 0,
                speech: vec![0.5; 800],
                pause: 0,
            })
            .unwrap();
        let mut state = output.state.lock().unwrap();
        state.terminal = true;
        state.pull(800).unwrap();
        state.finish_wav().unwrap();
        let prefix = write_coverage_prefix(&path, 400).unwrap();
        let source = std::fs::read(path).unwrap();
        let bytes = std::fs::read(prefix).unwrap();
        assert_eq!(bytes.len(), 44 + 400 * 4);
        assert_eq!(&bytes[44..], &source[44..44 + 400 * 4]);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 1600);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cleanup_uses_local_samples_and_reaped_worker_not_host_memory() {
        let before = [50.0, 50.0, 50.0];
        let after = [55.0, 55.0, 55.0];
        let first = cleanup_memory_record(&before, &after, 100.0, true);
        assert_eq!(first["worker_rss_after_mib"], 0.0);
        assert_eq!(first["worker_exited"], true);
        assert_eq!(first["parent_before_mib"], serde_json::json!(before));
        assert_eq!(first["parent_after_mib"], serde_json::json!(after));
    }
}

#[cfg(test)]
mod technical_flag_tests {
    use super::*;
    use handy_tts_core::vocabulary::{TechnicalOptions, Vocabulary};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn no_flag_leaves_technical_reading_off() {
        let (technical, rest) =
            technical_flags(&args(&["--speed", "1.0", "--wav", "x.wav"])).unwrap();
        assert_eq!(technical, None);
        assert_eq!(rest, args(&["--speed", "1.0", "--wav", "x.wav"]));
    }

    #[test]
    fn technical_uses_full_paths_and_short_links() {
        let (technical, rest) =
            technical_flags(&args(&["--speed", "1.0", "--technical", "--wav", "x.wav"])).unwrap();
        assert_eq!(technical, Some(TechnicalOptions::default()));
        assert_eq!(rest, args(&["--speed", "1.0", "--wav", "x.wav"]));
    }

    #[test]
    fn short_paths_and_full_links_change_the_options() {
        let (technical, _) =
            technical_flags(&args(&["--technical", "--short-paths", "--full-links"])).unwrap();
        assert_eq!(
            technical,
            Some(TechnicalOptions {
                full_paths: false,
                full_links: true,
            })
        );
    }

    #[test]
    fn option_flags_without_technical_are_refused() {
        assert!(technical_flags(&args(&["--short-paths"])).is_err());
        assert!(technical_flags(&args(&["--full-links"])).is_err());
    }

    #[test]
    fn prepared_text_for_the_owner_line_names_slash_dot_and_underscore() {
        let source = "Run tests/lib.sh then check fixture_base and /review before .sh files.";
        let vocabulary = Some(std::sync::Arc::new(Vocabulary::builtin()));
        let mut cursor =
            TextCursor::streaming_with(source, false, vocabulary, TechnicalOptions::default())
                .unwrap();
        let mut text = String::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            text.push_str(&chunk.text);
            text.push(' ');
        }
        println!("prepared chunk texts: {text}");
        for expected in [
            "tests slash lib dot S H",
            "fixture underscore base",
            "slash review",
            "dot S H",
        ] {
            assert!(
                text.contains(expected),
                "{expected:?} missing from {text:?}"
            );
        }
    }
}
