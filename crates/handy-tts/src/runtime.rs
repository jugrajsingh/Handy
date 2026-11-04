use anyhow::{Context, Result, bail, ensure};
use handy_tts_core::protocol::{Header, Request, Status, write_reply};
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

#[derive(Debug)]
struct VoiceFile {
    name: String,
}
impl std::fmt::Display for VoiceFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "voice file {:?}", self.name)
    }
}

pub(crate) fn read_voice_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| VoiceFile {
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    })
}

fn log_failure(operation: &str, error: &anyhow::Error) {
    let kind = if let Some(error) = error.downcast_ref::<std::io::Error>() {
        format!("{:?}", error.kind())
    } else if let Some(error) = error.downcast_ref::<openvino::InferenceError>() {
        format!("OpenVino{:?}", error.kind)
    } else if let Some(error) = error.downcast_ref::<serde_json::Error>() {
        format!("Json{:?}", error.classify())
    } else if error.is::<ort::Error>() {
        "OnnxRuntime".into()
    } else {
        "Other".into()
    };
    if let Some(file) = error.downcast_ref::<VoiceFile>() {
        eprintln!(
            "handy-tts: {operation} failed: file={:?} kind={kind}",
            file.name
        );
    } else {
        eprintln!("handy-tts: {operation} failed: kind={kind}");
    }
}

pub trait SynthEngine: Send {
    /// Exposes lane cancellation to the independent control reader.
    fn cancellation(&self) -> Option<crate::cancellation::Cancellation> {
        None
    }
    fn synthesize(
        &mut self,
        text: &str,
        speed: Option<f32>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<f32>>;
    /// Checks text length against the selected graph's smallest text-input cap.
    fn fits_text(&self, _text: &str) -> bool {
        true
    }
    fn sample_rate(&self) -> u32;
    fn device(&self) -> handy_tts_core::device::Device {
        handy_tts_core::device::Device::Cpu
    }
    fn placement(&self) -> std::collections::BTreeMap<String, handy_tts_core::device::Device> {
        std::collections::BTreeMap::new()
    }
}
pub fn available_bytes(meminfo: &str) -> Result<u64> {
    let line = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .context("MemAvailable missing")?;
    let kib: u64 = line
        .split_whitespace()
        .nth(1)
        .context("MemAvailable value missing")?
        .parse()
        .context("parse MemAvailable")?;
    kib.checked_mul(1024).context("MemAvailable overflow")
}
pub fn memory_available() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        available_bytes(&std::fs::read_to_string("/proc/meminfo").context("read MemAvailable")?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(u64::MAX)
    }
}

/// Retries over-cap static inputs as ordered word-boundary parts with bounded PCM.
pub fn synthesize_bounded(
    engine: &mut dyn SynthEngine,
    text: &str,
    speed: Option<f32>,
    cancelled: &AtomicBool,
) -> Result<Vec<f32>> {
    let mut pending = vec![text];
    let mut samples = Vec::new();
    while let Some(part) = pending.pop() {
        ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
        let result = if engine.fits_text(part) {
            engine.synthesize(part, speed, cancelled)
        } else {
            Err(crate::padding::StaticCapacity.into())
        };
        match result {
            Ok(pcm) => {
                ensure!(
                    pcm.len() <= handy_tts_core::protocol::MAX_SAMPLES - samples.len(),
                    "PCM limit exceeded"
                );
                samples.extend(pcm);
            }
            Err(error)
                if error
                    .downcast_ref::<crate::padding::StaticCapacity>()
                    .is_some() =>
            {
                let split = part
                    .char_indices()
                    .filter(|(i, ch)| *i > 0 && ch.is_whitespace() && !part[*i..].trim().is_empty())
                    .min_by_key(|(i, _)| i.abs_diff(part.len() / 2))
                    .map(|(i, _)| i)
                    .or_else(|| {
                        part.char_indices()
                            .nth(part.chars().count() / 2)
                            .map(|(i, _)| i)
                            .filter(|i| *i > 0)
                    })
                    .context("static input cannot split further")?;
                pending.push(part[split..].trim_start());
                pending.push(part[..split].trim_end());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(samples)
}

const CONTROL_CAPACITY: usize = 8;

struct CancelTarget {
    flag: Arc<AtomicBool>,
    inference: Option<crate::cancellation::Cancellation>,
}
impl CancelTarget {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Some(inference) = &self.inference
            && let Err(error) = inference.cancel()
        {
            log_failure("cancel", &error);
        }
    }
}
type CancelFlags = Arc<std::sync::Mutex<std::collections::HashMap<u64, CancelTarget>>>;

struct Job {
    id: u64,
    text: String,
    speed: Option<f32>,
    cancel: Arc<AtomicBool>,
}
enum Event {
    Control(Request, Option<Arc<AtomicBool>>),
    Duplicate(u64),
    Finished(Result<()>),
    ReaderFailed(anyhow::Error),
}
struct Lane {
    sender: Option<mpsc::SyncSender<Job>>,
    busy: Arc<AtomicBool>,
    cancellation: Option<crate::cancellation::Cancellation>,
}

fn cancel_all(flags: &CancelFlags) {
    if let Ok(flags) = flags.lock() {
        for cancel in flags.values() {
            cancel.cancel();
        }
    }
}

fn reply(writer: &std::sync::Mutex<impl Write>, header: &Header, samples: &[f32]) -> Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("reply writer poisoned"))?;
    write_reply(&mut *writer, header, samples).context("write worker reply")
}

fn error_reply(writer: &std::sync::Mutex<impl Write>, id: u64, message: &str) -> Result<()> {
    reply(
        writer,
        &Header {
            device: None,
            placement: None,
            id,
            status: Status::Error,
            sample_rate: None,
            samples: None,
            message: Some(message.into()),
        },
        &[],
    )
}

fn chunk_reply(
    writer: &std::sync::Mutex<impl Write>,
    id: u64,
    cancel: &AtomicBool,
    result: Result<(u32, Vec<f32>)>,
    completed: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("reply writer poisoned"))?;
    completed()?;
    let mut header = Header {
        device: None,
        placement: None,
        id,
        status: Status::Error,
        sample_rate: None,
        samples: None,
        message: None,
    };
    let mut pcm = Vec::new();
    if id != 0 && cancel.load(Ordering::SeqCst) {
        header.status = Status::Cancelled;
        header.message = Some("cancelled".into());
    } else {
        match result {
            Ok((rate, samples)) => {
                header.status = Status::Ok;
                header.sample_rate = Some(rate);
                header.samples = Some(samples.len());
                pcm = samples;
            }
            Err(error) => {
                log_failure("chunk", &error);
                header.message = Some("chunk_failed".into());
            }
        }
    }
    write_reply(&mut *writer, &header, &pcm).context("write chunk reply")
}

fn lane_loop(
    mut engine: Box<dyn SynthEngine>,
    jobs: mpsc::Receiver<Job>,
    writer: &std::sync::Mutex<impl Write>,
    flags: &CancelFlags,
    busy: &AtomicBool,
    tx: &mpsc::SyncSender<Event>,
) {
    let trace = std::env::var_os("HANDY_TTS_TRACE_TIMINGS").is_some();
    for job in jobs {
        let began = std::time::Instant::now();
        if trace {
            eprintln!(
                "SYNTH_START lane={:?} id={} time={:?}",
                std::thread::current().name(),
                job.id,
                std::time::SystemTime::now()
            );
        }
        let result = (|| {
            let samples = synthesize_bounded(engine.as_mut(), &job.text, job.speed, &job.cancel)?;
            ensure!(
                !samples.is_empty()
                    && samples.len() <= handy_tts_core::protocol::MAX_SAMPLES
                    && samples.iter().all(|v| v.is_finite()),
                "invalid PCM"
            );
            ensure!(
                (8000..=48000).contains(&engine.sample_rate()),
                "invalid sample rate"
            );
            Ok((engine.sample_rate(), samples))
        })();
        if trace {
            eprintln!(
                "SYNTH_END lane={:?} id={} time={:?} seconds={:.6}",
                std::thread::current().name(),
                job.id,
                std::time::SystemTime::now(),
                began.elapsed().as_secs_f64()
            );
        }
        let result = chunk_reply(writer, job.id, &job.cancel, result, || {
            let mut flags = flags
                .lock()
                .map_err(|_| anyhow::anyhow!("cancel flags poisoned"))?;
            flags.remove(&job.id);
            busy.store(false, Ordering::SeqCst);
            Ok(())
        });
        let _ = tx.send(Event::Finished(result));
    }
}

/// Dispatches a single-engine factory with a divided CPU thread budget.
pub fn run(
    reader: impl BufRead + Send + 'static,
    writer: impl Write + Send,
    mut load: impl FnMut(
        &Path,
        &str,
        usize,
        handy_tts_core::device::Device,
        &Path,
    ) -> Result<Box<dyn SynthEngine>>,
    memory: impl Fn() -> Result<u64>,
) -> Result<()> {
    run_lanes(
        reader,
        writer,
        |root, voice, threads, device, cache, lanes| {
            (0..lanes)
                .map(|_| load(root, voice, (threads / lanes).max(1), device, cache))
                .collect()
        },
        memory,
    )
}

/// Loads all engine lanes together before dispatching concurrent syntheses.
pub fn run_lanes(
    reader: impl BufRead + Send + 'static,
    writer: impl Write + Send,
    mut load: impl FnMut(
        &Path,
        &str,
        usize,
        handy_tts_core::device::Device,
        &Path,
        usize,
    ) -> Result<Vec<Box<dyn SynthEngine>>>,
    memory: impl Fn() -> Result<u64>,
) -> Result<()> {
    let writer = std::sync::Mutex::new(writer);
    let flags: CancelFlags = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let (tx, rx) = mpsc::sync_channel::<Event>(CONTROL_CAPACITY);
    let reader_flags = flags.clone();
    let reader_tx = tx.clone();
    let controls = std::thread::Builder::new()
        .name("tts-controls".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let mut reader = reader;
                loop {
                    let mut bytes = Vec::new();
                    let count = reader
                        .by_ref()
                        .take((handy_tts_core::protocol::MAX_CONTROL_BYTES + 1) as u64)
                        .read_until(b'\n', &mut bytes)
                        .context("read control")?;
                    if count == 0 {
                        cancel_all(&reader_flags);
                        let _ = reader_tx.send(Event::Control(Request::Shutdown, None));
                        break;
                    }
                    ensure!(
                        count <= handy_tts_core::protocol::MAX_CONTROL_BYTES
                            && bytes.last() == Some(&b'\n'),
                        "control line too long"
                    );
                    let request: Request =
                        serde_json::from_slice(&bytes).context("decode control")?;
                    let cancel = match &request {
                        Request::Synthesize { id, .. } => {
                            let mut flags = reader_flags
                                .lock()
                                .map_err(|_| anyhow::anyhow!("cancel flags poisoned"))?;
                            if flags.contains_key(id) {
                                drop(flags);
                                if reader_tx.send(Event::Duplicate(*id)).is_err() {
                                    break;
                                }
                                continue;
                            }
                            let cancel = Arc::new(AtomicBool::new(false));
                            flags.insert(
                                *id,
                                CancelTarget {
                                    flag: cancel.clone(),
                                    inference: None,
                                },
                            );
                            Some(cancel)
                        }
                        Request::Cancel { id } => {
                            let flags = reader_flags
                                .lock()
                                .map_err(|_| anyhow::anyhow!("cancel flags poisoned"))?;
                            if let Some(cancel) = flags.get(id) {
                                cancel.cancel();
                            }
                            continue;
                        }
                        Request::Shutdown => {
                            cancel_all(&reader_flags);
                            None
                        }
                        _ => None,
                    };
                    let shutdown = matches!(request, Request::Shutdown);
                    if reader_tx.send(Event::Control(request, cancel)).is_err() || shutdown {
                        break;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                cancel_all(&reader_flags);
                let _ = reader_tx.send(Event::ReaderFailed(error));
            }
        })
        .context("start control reader")?;
    let result = std::thread::scope(|scope| {
        let mut lanes: Vec<Lane> = Vec::new();
        let mut active_lanes = 0;
        let result = (|| -> Result<()> {
            while let Ok(event) = rx.recv() {
                match event {
                    Event::ReaderFailed(error) => return Err(error),
                    Event::Duplicate(id) => error_reply(&writer, id, "busy")?,
                    Event::Finished(result) => result?,
                    Event::Control(
                        Request::Load {
                            model_dir,
                            voice,
                            threads,
                            device,
                            cache_dir,
                            lanes: count,
                        },
                        _,
                    ) => {
                        if lanes.iter().any(|lane| lane.busy.load(Ordering::SeqCst)) {
                            error_reply(&writer, 0, "busy")?;
                            continue;
                        }
                        // Dropping each sender releases its previous engine.
                        lanes.clear();
                        active_lanes = 0;
                        if memory()? < handy_tts_core::memory::MIN_AVAILABLE_BYTES {
                            error_reply(&writer, 0, "not_enough_memory")?;
                            continue;
                        }
                        let loaded = (|| -> Result<Vec<Box<dyn SynthEngine>>> {
                            ensure!((1..=8).contains(&threads), "invalid threads");
                            ensure!((1..=3).contains(&count), "invalid lanes");
                            let engines =
                                load(&model_dir, &voice, threads, device, &cache_dir, count)?;
                            ensure!(engines.len() == count, "loader returned wrong lane count");
                            Ok(engines)
                        })();
                        match loaded {
                            Ok(engines) => {
                                let actual_device = engines[0].device();
                                let actual_placement = engines[0].placement();
                                for (index, engine) in engines.into_iter().enumerate() {
                                    let cancellation = engine.cancellation();
                                    let (sender, jobs) = mpsc::sync_channel::<Job>(1);
                                    let writer = &writer;
                                    let tx = tx.clone();
                                    let flags = flags.clone();
                                    let busy = Arc::new(AtomicBool::new(false));
                                    let worker_busy = busy.clone();
                                    std::thread::Builder::new()
                                        .name(format!("tts-lane-{index}"))
                                        .spawn_scoped(scope, move || {
                                            lane_loop(
                                                engine,
                                                jobs,
                                                writer,
                                                &flags,
                                                &worker_busy,
                                                &tx,
                                            );
                                        })
                                        .context("start synthesis lane")?;
                                    lanes.push(Lane {
                                        sender: Some(sender),
                                        busy,
                                        cancellation,
                                    });
                                }
                                active_lanes = count;
                                reply(
                                    &writer,
                                    &Header {
                                        device: Some(actual_device),
                                        placement: Some(actual_placement),
                                        id: 0,
                                        status: Status::Ready,
                                        sample_rate: None,
                                        samples: Some(0),
                                        message: None,
                                    },
                                    &[],
                                )?;
                            }
                            Err(error) => {
                                log_failure("load", &error);
                                let message = if device == handy_tts_core::device::Device::Npu
                                    && error.is::<crate::ov_graph::NpuCompileRejected>()
                                {
                                    "npu_compile_failed"
                                } else {
                                    "load_failed"
                                };
                                error_reply(&writer, 0, message)?;
                            }
                        }
                    }
                    Event::Control(Request::SetLanes { lanes: count }, _) => {
                        if count == 0 || count > active_lanes {
                            error_reply(&writer, 0, "invalid_lanes")?;
                            continue;
                        }
                        for lane in &mut lanes[count..] {
                            lane.sender.take();
                        }
                        active_lanes = count;
                        reply(
                            &writer,
                            &Header {
                                device: None,
                                placement: None,
                                id: 0,
                                status: Status::Ready,
                                sample_rate: None,
                                samples: Some(0),
                                message: None,
                            },
                            &[],
                        )?;
                    }
                    Event::Control(Request::Synthesize { id, text, speed }, cancel) => {
                        let cancel = cancel.context("synthesis cancel flag missing")?;
                        let valid = (|| -> Result<()> {
                            ensure!(
                                id != 0 && !text.trim().is_empty(),
                                "invalid synthesis request"
                            );
                            if let Some(speed) = speed {
                                ensure!((0.5..=2.0).contains(&speed), "invalid speed");
                            }
                            ensure!(active_lanes > 0, "model not loaded");
                            Ok(())
                        })();
                        if let Err(error) = valid {
                            chunk_reply(&writer, id, &cancel, Err(error), || Ok(()))?;
                            flags
                                .lock()
                                .map_err(|_| anyhow::anyhow!("cancel flags poisoned"))?
                                .remove(&id);
                        } else {
                            let in_flight = lanes
                                .iter()
                                .filter(|lane| lane.busy.load(Ordering::SeqCst))
                                .count();
                            let available = if in_flight < active_lanes {
                                lanes[..active_lanes]
                                    .iter_mut()
                                    .find(|lane| !lane.busy.load(Ordering::SeqCst))
                            } else {
                                None
                            };
                            if let Some(lane) = available {
                                {
                                    let mut flags = flags
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!("cancel flags poisoned"))?;
                                    let target = flags
                                        .get_mut(&id)
                                        .context("synthesis cancel target missing")?;
                                    if let Some(inference) = &lane.cancellation {
                                        inference.reset()?;
                                        target.inference = Some(inference.clone());
                                        if target.flag.load(Ordering::SeqCst) {
                                            target.cancel();
                                        }
                                    }
                                }
                                lane.busy.store(true, Ordering::SeqCst);
                                lane.sender
                                    .as_ref()
                                    .context("lane was released")?
                                    .send(Job {
                                        id,
                                        text,
                                        speed,
                                        cancel,
                                    })
                                    .context("dispatch synthesis")?;
                            } else {
                                error_reply(&writer, id, "busy")?;
                                flags
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("cancel flags poisoned"))?
                                    .remove(&id);
                            }
                        }
                    }
                    Event::Control(Request::Shutdown, _) => break,
                    Event::Control(Request::Cancel { .. }, _) => bail!("cancel reached dispatcher"),
                }
            }
            Ok(())
        })();
        cancel_all(&flags);
        drop(lanes);
        drop(rx);
        result
    });
    if controls.is_finished() {
        controls
            .join()
            .map_err(|_| anyhow::anyhow!("control reader panicked"))?;
    }
    result
}

pub struct EngineAdapter {
    text_capacity: Option<usize>,
    engine: crate::Engine,
    device: handy_tts_core::device::Device,
    placement: std::collections::BTreeMap<String, handy_tts_core::device::Device>,
}
impl SynthEngine for EngineAdapter {
    fn fits_text(&self, text: &str) -> bool {
        match (&self.engine, self.text_capacity) {
            (crate::Engine::Supertonic(_), Some(capacity)) => {
                crate::supertonic::prepare_text(text).chars().count() <= capacity
            }
            _ => true,
        }
    }
    fn cancellation(&self) -> Option<crate::cancellation::Cancellation> {
        Some(self.engine.cancellation().clone())
    }
    fn synthesize(
        &mut self,
        text: &str,
        speed: Option<f32>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<f32>> {
        ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
        let output = self.engine.synth(text, speed)?;
        ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
        Ok(output)
    }
    fn sample_rate(&self) -> u32 {
        self.engine.sample_rate()
    }
    fn device(&self) -> handy_tts_core::device::Device {
        self.device
    }
    fn placement(&self) -> std::collections::BTreeMap<String, handy_tts_core::device::Device> {
        self.placement.clone()
    }
}
pub fn load_adapter(
    model: &str,
    root: &Path,
    voice: &str,
    threads: usize,
    device: handy_tts_core::device::Device,
    cache: &Path,
) -> Result<Box<dyn SynthEngine>> {
    let mut engines = load_adapters(model, root, voice, threads, device, cache, 1)?;
    engines
        .pop()
        .context("single lane loader returned no engine")
}

/// Loads independent engines with shared OpenVINO compilation and divided CPU threads.
pub fn load_adapters(
    model: &str,
    root: &Path,
    voice: &str,
    threads: usize,
    device: handy_tts_core::device::Device,
    cache: &Path,
    lanes: usize,
) -> Result<Vec<Box<dyn SynthEngine>>> {
    ensure!((1..=3).contains(&lanes), "invalid lanes");
    ensure!((1..=8).contains(&threads), "invalid threads");
    let config = crate::backend_config::config(model, root, device, cache)?;
    let text_capacity = config
        .classes
        .iter()
        .map(|class| class.text as usize)
        .max()
        .or_else(|| {
            config
                .shapes
                .values()
                .filter_map(|inputs| {
                    inputs
                        .get("text_ids")
                        .and_then(|shape| shape.last())
                        .copied()
                })
                .filter_map(|value| usize::try_from(value).ok())
                .min()
        });
    let placement = config.placement.clone();
    let engines = crate::graph::with_config(config, || {
        crate::Engine::load_lanes(model, root, (threads / lanes).max(1), voice, lanes)
    })?;
    Ok(engines
        .into_iter()
        .map(|engine| {
            Box::new(EngineAdapter {
                text_capacity,
                engine,
                device,
                placement: placement.clone(),
            }) as Box<dyn SynthEngine>
        })
        .collect())
}
pub fn parse_model(args: impl Iterator<Item = String>) -> Result<String> {
    let args: Vec<_> = args.collect();
    ensure!(
        args.len() == 2 && args[0] == "--model",
        "usage: handy-tts --model <model-id>"
    );
    ensure!(
        ["pocket-en", "supertonic-3", "kokoro-82m-int8"].contains(&args[1].as_str()),
        "unknown model id"
    );
    Ok(args[1].clone())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod lanes {
        use super::*;
        use handy_tts_core::protocol::{read_header, read_pcm_after_header};
        use std::io::BufReader;
        use std::os::unix::net::UnixStream;
        use std::sync::Mutex;
        use std::time::Duration;

        struct Scripted {
            cancellation: crate::cancellation::Cancellation,
            release: Arc<AtomicBool>,
            started: Arc<AtomicBool>,
        }
        impl SynthEngine for Scripted {
            fn cancellation(&self) -> Option<crate::cancellation::Cancellation> {
                Some(self.cancellation.clone())
            }
            fn synthesize(
                &mut self,
                text: &str,
                _: Option<f32>,
                cancelled: &AtomicBool,
            ) -> Result<Vec<f32>> {
                if text == "slow" {
                    self.started.store(true, Ordering::SeqCst);
                    for _ in 0..2000 {
                        if self.release.load(Ordering::SeqCst)
                            || self.cancellation.flag.load(Ordering::SeqCst)
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
                ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
                Ok(vec![if text == "slow" { 0.25 } else { -0.5 }; 4096])
            }
            fn sample_rate(&self) -> u32 {
                24000
            }
        }
        struct YieldWriter {
            stream: UnixStream,
            release: Arc<AtomicBool>,
            yield_header: bool,
            headers: usize,
        }
        impl Write for YieldWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let count = self.stream.write(bytes)?;
                if bytes == b"\n" {
                    self.headers += 1;
                }
                if self.yield_header && self.headers == 2 && bytes == b"\n" {
                    self.yield_header = false;
                    self.release.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.stream.flush()
            }
        }
        struct Harness {
            input: UnixStream,
            output: BufReader<UnixStream>,
            thread: Option<std::thread::JoinHandle<Result<()>>>,
            release: Arc<AtomicBool>,
            started: Arc<AtomicBool>,
            loads: Arc<Mutex<usize>>,
            threads: Arc<Mutex<Vec<usize>>>,
        }
        impl Harness {
            fn new(lanes: usize, yield_header: bool) -> Self {
                let (input, reader) = UnixStream::pair().unwrap();
                let (output, stream) = UnixStream::pair().unwrap();
                output
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let release = Arc::new(AtomicBool::new(false));
                let started = Arc::new(AtomicBool::new(false));
                let loads = Arc::new(Mutex::new(0));
                let threads = Arc::new(Mutex::new(Vec::new()));
                let state = (
                    release.clone(),
                    started.clone(),
                    loads.clone(),
                    threads.clone(),
                );
                let thread = std::thread::spawn(move || {
                    run(
                        BufReader::new(reader),
                        YieldWriter {
                            stream,
                            release: state.0.clone(),
                            yield_header,
                            headers: 0,
                        },
                        move |_, _, threads, _, _| {
                            *state.2.lock().unwrap() += 1;
                            state.3.lock().unwrap().push(threads);
                            Ok(Box::new(Scripted {
                                cancellation: crate::cancellation::Cancellation::new().unwrap(),
                                release: state.0.clone(),
                                started: state.1.clone(),
                            }))
                        },
                        || Ok(u64::MAX),
                    )
                });
                let mut harness = Self {
                    input,
                    output: BufReader::new(output),
                    thread: Some(thread),
                    release,
                    started,
                    loads,
                    threads,
                };
                harness.raw(serde_json::json!({"type":"load","model_dir":"fixture","voice":"alba","threads":4,"lanes":lanes}));
                assert_eq!(harness.header().status, Status::Ready);
                assert_eq!(*harness.loads.lock().unwrap(), lanes);
                harness
            }
            fn raw(&mut self, request: serde_json::Value) {
                writeln!(self.input, "{request}").unwrap();
            }
            fn synth(&mut self, id: u64, text: &str) {
                self.raw(serde_json::json!({"type":"synthesize","id":id,"text":text}));
            }
            fn header(&mut self) -> Header {
                read_header(&mut self.output).unwrap()
            }
            fn pcm(&mut self, id: u64) -> Vec<f32> {
                let header = self.header();
                read_pcm_after_header(&mut self.output, header, id)
                    .unwrap()
                    .samples
            }
            fn wait_started(&self) {
                for _ in 0..2000 {
                    if self.started.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                panic!("slow lane did not start");
            }
            fn shutdown(&mut self) {
                self.raw(serde_json::json!({"type":"shutdown"}));
                self.thread.take().unwrap().join().unwrap().unwrap();
            }
        }
        impl Drop for Harness {
            fn drop(&mut self) {
                self.release.store(true, Ordering::SeqCst);
                if self.thread.is_some() {
                    let _ = writeln!(self.input, "{}", serde_json::json!({"type":"shutdown"}));
                    let _ = self.output.get_ref().shutdown(std::net::Shutdown::Both);
                    let _ = self.thread.take().unwrap().join();
                }
            }
        }
        #[test]
        fn lane_factories_split_the_cpu_thread_budget() {
            for lanes in 1..=3 {
                let mut harness = Harness::new(lanes, false);
                assert_eq!(
                    *harness.threads.lock().unwrap(),
                    vec![(4 / lanes).max(1); lanes]
                );
                harness.shutdown();
            }
        }

        mod coordinated {
            use super::*;
            use std::collections::BTreeSet;
            use std::sync::Condvar;

            #[derive(Default)]
            pub struct Trace {
                pub calls: Vec<(usize, u64)>,
                pub released: BTreeSet<u64>,
                pub dropped: BTreeSet<usize>,
            }
            #[derive(Default)]
            pub struct State {
                pub trace: Mutex<Trace>,
                changed: Condvar,
            }
            impl State {
                pub fn wait(&self, predicate: impl Fn(&Trace) -> bool, message: &str) {
                    let trace = self.trace.lock().unwrap();
                    let (trace, timeout) = self
                        .changed
                        .wait_timeout_while(trace, Duration::from_secs(3), |trace| {
                            !predicate(trace)
                        })
                        .unwrap();
                    let satisfied = !timeout.timed_out() || predicate(&trace);
                    drop(trace);
                    assert!(satisfied, "{message}");
                }
                pub fn release(&self, id: u64) {
                    self.trace.lock().unwrap().released.insert(id);
                    self.changed.notify_all();
                }
            }
            struct Engine {
                lane: usize,
                state: Arc<State>,
            }
            impl Drop for Engine {
                fn drop(&mut self) {
                    self.state.trace.lock().unwrap().dropped.insert(self.lane);
                    self.state.changed.notify_all();
                }
            }
            impl SynthEngine for Engine {
                fn synthesize(
                    &mut self,
                    text: &str,
                    _: Option<f32>,
                    cancelled: &AtomicBool,
                ) -> Result<Vec<f32>> {
                    let id: u64 = text.parse()?;
                    let mut trace = self.state.trace.lock().unwrap();
                    trace.calls.push((self.lane, id));
                    self.state.changed.notify_all();
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !trace.released.contains(&id) && !cancelled.load(Ordering::SeqCst) {
                        ensure!(
                            std::time::Instant::now() < deadline,
                            "fixture engine deadline"
                        );
                        trace = self
                            .state
                            .changed
                            .wait_timeout(trace, Duration::from_millis(10))
                            .unwrap()
                            .0;
                    }
                    ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
                    Ok(vec![id as f32; 32])
                }
                fn sample_rate(&self) -> u32 {
                    24000
                }
            }
            type Accepted = Arc<(Mutex<usize>, Condvar)>;
            struct ObservedReader {
                reader: BufReader<UnixStream>,
                consumed: usize,
                accepted: Accepted,
            }
            impl Read for ObservedReader {
                fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                    self.reader.read(bytes)
                }
            }
            impl BufRead for ObservedReader {
                fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                    *self.accepted.0.lock().unwrap() = self.consumed;
                    self.accepted.1.notify_all();
                    self.reader.fill_buf()
                }
                fn consume(&mut self, amount: usize) {
                    self.consumed += self.reader.buffer()[..amount]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count();
                    self.reader.consume(amount);
                }
            }
            struct CoordinatedWriter {
                stream: UnixStream,
                flushes: usize,
                hold: Option<(usize, usize)>,
                accepted: Accepted,
            }
            impl Write for CoordinatedWriter {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.stream.write(bytes)
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    self.stream.flush()?;
                    self.flushes += 1;
                    if let Some((flush, lines)) = self.hold
                        && self.flushes == flush
                    {
                        let accepted = self.accepted.0.lock().unwrap();
                        let (accepted, timeout) = self
                            .accepted
                            .1
                            .wait_timeout_while(accepted, Duration::from_secs(3), |count| {
                                *count < lines
                            })
                            .unwrap();
                        if timeout.timed_out() && *accepted < lines {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "reader did not accept coordinated control",
                            ));
                        }
                    }
                    Ok(())
                }
            }
            pub struct Fixture {
                pub state: Arc<State>,
                input: Option<UnixStream>,
                pub output: BufReader<UnixStream>,
                thread: Option<std::thread::JoinHandle<()>>,
                exited: mpsc::Receiver<Result<()>>,
                accepted: Accepted,
            }
            impl Fixture {
                pub fn new(lanes: usize, hold: Option<(usize, usize)>) -> Self {
                    let (input, reader) = UnixStream::pair().unwrap();
                    let (output, stream) = UnixStream::pair().unwrap();
                    output
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let state = Arc::new(State::default());
                    let worker_state = state.clone();
                    let accepted: Accepted = Arc::new((Mutex::new(0), Condvar::new()));
                    let reader_accepted = accepted.clone();
                    let writer_accepted = accepted.clone();
                    let (done, exited) = mpsc::channel();
                    let thread = std::thread::spawn(move || {
                        let mut next_lane = 0;
                        let result = run(
                            ObservedReader {
                                reader: BufReader::new(reader),
                                consumed: 0,
                                accepted: reader_accepted,
                            },
                            CoordinatedWriter {
                                stream,
                                flushes: 0,
                                hold,
                                accepted: writer_accepted,
                            },
                            |_, _, _, _, _| {
                                let lane = next_lane;
                                next_lane += 1;
                                Ok(Box::new(Engine {
                                    lane,
                                    state: worker_state.clone(),
                                }))
                            },
                            || Ok(u64::MAX),
                        );
                        let _ = done.send(result);
                    });
                    let mut fixture = Self {
                        state,
                        input: Some(input),
                        output: BufReader::new(output),
                        thread: Some(thread),
                        exited,
                        accepted,
                    };
                    fixture.raw(serde_json::json!({"type":"load","model_dir":"fixture","voice":"alba","threads":4,"lanes":lanes}));
                    assert_eq!(fixture.header().status, Status::Ready);
                    fixture
                }
                pub fn raw(&mut self, request: serde_json::Value) {
                    writeln!(self.input.as_mut().unwrap(), "{request}").unwrap();
                }
                pub fn synth(&mut self, id: u64) {
                    self.raw(
                        serde_json::json!({"type":"synthesize","id":id,"text":id.to_string()}),
                    );
                }
                pub fn header(&mut self) -> Header {
                    read_header(&mut self.output).unwrap()
                }
                pub fn pcm(&mut self, id: u64) {
                    let header = self.header();
                    assert_eq!(
                        header.status,
                        Status::Ok,
                        "completed request must leave admission capacity available"
                    );
                    assert_eq!(
                        read_pcm_after_header(&mut self.output, header, id)
                            .unwrap()
                            .samples,
                        vec![id as f32; 32]
                    );
                }
                pub fn started(&self, id: u64) {
                    self.state.wait(
                        |trace| trace.calls.iter().any(|(_, called)| *called == id),
                        "engine request did not start",
                    );
                }
                pub fn close_input(&mut self) {
                    self.input
                        .take()
                        .unwrap()
                        .shutdown(std::net::Shutdown::Write)
                        .unwrap();
                }
                pub fn finish(&mut self) {
                    let result = self.exited.recv_timeout(Duration::from_secs(3));
                    assert!(
                        result.is_ok(),
                        "runtime did not exit within the EOF/shutdown deadline"
                    );
                    result.unwrap().unwrap();
                    self.thread.take().unwrap().join().unwrap();
                }
                pub fn shutdown(&mut self) {
                    self.raw(serde_json::json!({"type":"shutdown"}));
                    self.finish();
                }
            }
            impl Drop for Fixture {
                fn drop(&mut self) {
                    if let Some(mut input) = self.input.take() {
                        let _ = writeln!(input, "{}", serde_json::json!({"type":"shutdown"}));
                        let _ = input.shutdown(std::net::Shutdown::Write);
                    }
                    let _ = self.output.get_ref().shutdown(std::net::Shutdown::Both);
                    self.state.trace.lock().unwrap().released.extend(1..=10);
                    self.state.changed.notify_all();
                    *self.accepted.0.lock().unwrap() = usize::MAX;
                    self.accepted.1.notify_all();
                    if let Some(thread) = self.thread.take()
                        && (self.exited.recv_timeout(Duration::from_secs(3)).is_ok()
                            || thread.is_finished())
                    {
                        thread.join().unwrap();
                    }
                }
            }
        }
        #[test]
        fn closed_dispatcher_still_replies_to_every_accepted_job() {
            struct HeldReply {
                bytes: Arc<Mutex<Vec<u8>>>,
                published: mpsc::Sender<()>,
                resume: mpsc::Receiver<()>,
                first: bool,
            }
            impl Write for HeldReply {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.bytes.lock().unwrap().extend_from_slice(bytes);
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    if self.first {
                        self.first = false;
                        self.published.send(()).unwrap();
                        self.resume.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                    Ok(())
                }
            }
            let (jobs, receiver) = mpsc::sync_channel(1);
            let (events, dispatcher) = mpsc::sync_channel(8);
            let (published, publication) = mpsc::channel();
            let (resume, resume_reply) = mpsc::channel();
            let (done, exited) = mpsc::channel();
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let writer = Mutex::new(HeldReply {
                bytes: bytes.clone(),
                published,
                resume: resume_reply,
                first: true,
            });
            let cancelled = Arc::new(AtomicBool::new(true));
            let flags: CancelFlags = Arc::new(Mutex::new(
                [
                    (
                        1,
                        CancelTarget {
                            flag: cancelled.clone(),
                            inference: None,
                        },
                    ),
                    (
                        2,
                        CancelTarget {
                            flag: cancelled.clone(),
                            inference: None,
                        },
                    ),
                ]
                .into(),
            ));
            let busy = Arc::new(AtomicBool::new(true));
            jobs.send(Job {
                id: 1,
                text: "slow".into(),
                speed: None,
                cancel: cancelled.clone(),
            })
            .unwrap();
            let worker_busy = busy.clone();
            let worker = std::thread::spawn(move || {
                lane_loop(
                    Box::new(Scripted {
                        cancellation: crate::cancellation::Cancellation::new().unwrap(),
                        release: Arc::new(AtomicBool::new(false)),
                        started: Arc::new(AtomicBool::new(false)),
                    }),
                    receiver,
                    &writer,
                    &flags,
                    &worker_busy,
                    &events,
                );
                done.send(()).unwrap();
            });
            publication.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(!busy.load(Ordering::SeqCst));
            busy.store(true, Ordering::SeqCst);
            jobs.send(Job {
                id: 2,
                text: "slow".into(),
                speed: None,
                cancel: cancelled,
            })
            .unwrap();
            drop(jobs);
            drop(dispatcher);
            resume.send(()).unwrap();
            exited.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.join().unwrap();
            let mut replies = std::io::Cursor::new(bytes.lock().unwrap().clone());
            for id in 1..=2 {
                let header = read_header(&mut replies).unwrap();
                assert_eq!(
                    (header.id, header.status, header.samples),
                    (id, Status::Cancelled, None)
                );
            }
            assert_eq!(replies.position() as usize, replies.get_ref().len());
        }
        #[test]
        fn next_request_after_complete_reply_is_not_busy() {
            let mut h = coordinated::Fixture::new(1, Some((2, 3 + CONTROL_CAPACITY)));
            h.state.release(1);
            h.state.release(2);
            h.synth(1);
            h.pcm(1);
            h.synth(2);
            for _ in 0..CONTROL_CAPACITY {
                h.raw(serde_json::json!({"type":"set_lanes","lanes":1}));
            }
            let mut completed = false;
            let mut acknowledged = 0;
            for _ in 0..=CONTROL_CAPACITY {
                let header = h.header();
                assert_ne!(
                    header.message.as_deref(),
                    Some("busy"),
                    "completed request must leave admission capacity available"
                );
                if header.id == 2 {
                    assert!(!completed);
                    assert_eq!(
                        read_pcm_after_header(&mut h.output, header, 2)
                            .unwrap()
                            .samples,
                        vec![2.0; 32]
                    );
                    completed = true;
                } else {
                    assert_eq!((header.id, header.status), (0, Status::Ready));
                    acknowledged += 1;
                }
            }
            assert!(completed);
            assert_eq!(acknowledged, CONTROL_CAPACITY);
            assert_eq!(h.state.trace.lock().unwrap().calls, [(0, 1), (0, 2)]);
            h.shutdown();
        }
        #[test]
        fn next_load_after_complete_reply_is_not_busy() {
            let mut h = coordinated::Fixture::new(1, Some((2, 3 + CONTROL_CAPACITY)));
            h.state.release(1);
            h.synth(1);
            h.pcm(1);
            h.raw(serde_json::json!({"type":"load","model_dir":"fixture","voice":"alba","threads":4,"lanes":1}));
            for _ in 0..CONTROL_CAPACITY {
                h.raw(serde_json::json!({"type":"set_lanes","lanes":1}));
            }
            for _ in 0..=CONTROL_CAPACITY {
                let header = h.header();
                assert_eq!(
                    (header.id, header.status),
                    (0, Status::Ready),
                    "completed request must allow reloading"
                );
            }
            h.state.wait(
                |trace| trace.dropped.contains(&0),
                "reloaded engine was retained",
            );
            h.state.release(2);
            h.synth(2);
            h.pcm(2);
            assert_eq!(h.state.trace.lock().unwrap().calls, [(0, 1), (1, 2)]);
            h.shutdown();
        }
        #[test]
        fn lowering_busy_lanes_blocks_new_work_until_retired_jobs_finish() {
            let mut h = coordinated::Fixture::new(2, None);
            h.synth(1);
            h.started(1);
            h.synth(2);
            h.started(2);
            h.state.release(1);
            h.pcm(1);
            h.raw(serde_json::json!({"type":"set_lanes","lanes":1}));
            assert_eq!(h.header().status, Status::Ready);
            h.state.release(3);
            h.synth(3);
            let header = h.header();
            assert_eq!(
                (header.id, header.status, header.message.as_deref()),
                (3, Status::Error, Some("busy"))
            );
            assert_eq!(h.state.trace.lock().unwrap().calls, [(0, 1), (1, 2)]);
            h.state.release(2);
            h.pcm(2);
            h.state.release(4);
            h.synth(4);
            h.pcm(4);
            assert_eq!(
                h.state.trace.lock().unwrap().calls,
                [(0, 1), (1, 2), (0, 4)]
            );
            h.shutdown();
        }
        #[test]
        fn lowering_lanes_releases_idle_and_drained_engines_before_shutdown() {
            for busy in [false, true] {
                let mut h = coordinated::Fixture::new(2, None);
                if busy {
                    h.synth(1);
                    h.started(1);
                    h.synth(2);
                    h.started(2);
                }
                h.raw(serde_json::json!({"type":"set_lanes","lanes":1}));
                assert_eq!(h.header().status, Status::Ready);
                if busy {
                    assert!(h.state.trace.lock().unwrap().dropped.is_empty());
                    h.state.release(2);
                    h.pcm(2);
                }
                h.state.wait(
                    |trace| trace.dropped.contains(&1),
                    "retired engine was retained before shutdown",
                );
                assert!(!h.state.trace.lock().unwrap().dropped.contains(&0));
                if busy {
                    h.state.release(1);
                    h.pcm(1);
                }
                h.shutdown();
                assert_eq!(h.state.trace.lock().unwrap().dropped, [0, 1].into());
            }
        }
        #[test]
        fn eof_with_busy_lanes_cancels_and_exits() {
            let mut h = coordinated::Fixture::new(3, None);
            for id in 1..=3 {
                h.synth(id);
                h.started(id);
            }
            h.close_input();
            let mut ids = std::collections::BTreeSet::new();
            for _ in 0..3 {
                let header = h.header();
                assert_eq!(header.status, Status::Cancelled);
                assert_eq!(header.samples, None);
                assert!(ids.insert(header.id));
            }
            assert_eq!(ids, [1, 2, 3].into());
            h.finish();
            assert_eq!(h.state.trace.lock().unwrap().dropped, [0, 1, 2].into());
        }
        #[test]
        fn cancellation_before_lane_start_never_calls_engine() {
            let mut h = coordinated::Fixture::new(1, Some((1, 4)));
            h.synth(1);
            h.raw(serde_json::json!({"type":"cancel","id":1}));
            h.raw(serde_json::json!({"type":"set_lanes","lanes":1}));
            let mut cancelled = false;
            let mut ready = false;
            for _ in 0..2 {
                let header = h.header();
                match header.status {
                    Status::Cancelled => {
                        assert_eq!(header.id, 1);
                        cancelled = true;
                    }
                    Status::Ready => {
                        assert_eq!(header.id, 0);
                        ready = true;
                    }
                    other => panic!("unexpected pre-start reply {other:?}"),
                }
            }
            assert!(cancelled && ready);
            assert!(h.state.trace.lock().unwrap().calls.is_empty());
            h.shutdown();
        }
        #[test]
        fn two_lanes_reply_out_of_order_with_intact_framing() {
            let mut h = Harness::new(2, true);
            assert_eq!(*h.loads.lock().unwrap(), 2);
            h.synth(1, "slow");
            h.wait_started();
            h.synth(2, "fast");
            assert_eq!(h.pcm(2), vec![-0.5; 4096]);
            h.release.store(true, Ordering::SeqCst);
            assert_eq!(h.pcm(1), vec![0.25; 4096]);
            h.shutdown();
        }
        #[test]
        fn request_beyond_lanes_is_busy() {
            let mut h = Harness::new(1, false);
            h.synth(1, "slow");
            h.wait_started();
            h.synth(2, "fast");
            let header = h.header();
            assert_eq!(
                (header.id, header.status, header.message.as_deref()),
                (2, Status::Error, Some("busy"))
            );
            h.release.store(true, Ordering::SeqCst);
            assert_eq!(h.pcm(1).len(), 4096);
        }
        #[test]
        fn cancel_one_id_keeps_other_lane_result() {
            let mut h = Harness::new(2, false);
            h.synth(1, "slow");
            h.wait_started();
            h.synth(2, "fast");
            h.raw(serde_json::json!({"type":"cancel","id":1}));
            let mut cancelled = false;
            let mut intact = false;
            for _ in 0..2 {
                let header = h.header();
                if header.id == 1 {
                    assert_eq!(header.status, Status::Cancelled);
                    assert_eq!(header.samples, None);
                    cancelled = true;
                } else {
                    assert_eq!(
                        read_pcm_after_header(&mut h.output, header, 2)
                            .unwrap()
                            .samples,
                        vec![-0.5; 4096]
                    );
                    intact = true;
                }
            }
            assert!(cancelled && intact);
        }
        #[test]
        fn set_lanes_lowers_and_rejects_raise() {
            let mut h = Harness::new(2, false);
            h.raw(serde_json::json!({"type":"set_lanes","lanes":1}));
            assert_eq!(h.header().status, Status::Ready);
            h.raw(serde_json::json!({"type":"set_lanes","lanes":2}));
            assert_eq!(h.header().message.as_deref(), Some("invalid_lanes"));
            h.synth(1, "slow");
            h.wait_started();
            h.synth(2, "fast");
            assert_eq!(h.header().message.as_deref(), Some("busy"));
            h.release.store(true, Ordering::SeqCst);
            assert_eq!(h.pcm(1).len(), 4096);
        }
        #[test]
        fn shutdown_with_lanes_in_flight_exits() {
            let mut h = Harness::new(3, false);
            h.synth(1, "slow");
            h.wait_started();
            h.synth(2, "slow");
            h.synth(3, "slow");
            let start = std::time::Instant::now();
            h.shutdown();
            assert!(start.elapsed() < Duration::from_secs(1));
            let mut ids = std::collections::BTreeSet::new();
            for _ in 0..3 {
                let header = h.header();
                assert!(ids.insert(header.id));
                assert_eq!(header.status, Status::Cancelled);
                assert_eq!(header.samples, None);
            }
            assert_eq!(ids, [1, 2, 3].into());
        }
    }
    #[test]
    fn pocket_engine_refuses_native_speed_other_than_one() {
        let mut calls = 0;
        for speed in [Some(0.5), Some(2.0), Some(f32::NAN)] {
            assert!(
                crate::synth_native("pocket-en", speed, |_| {
                    calls += 1;
                    Ok(vec![0.25])
                })
                .is_err()
            );
        }
        assert_eq!(calls, 0);
        for speed in [None, Some(1.0)] {
            assert_eq!(
                crate::synth_native("pocket-en", speed, |native| {
                    assert_eq!(native, 1.0);
                    Ok(vec![0.25])
                })
                .unwrap(),
                [0.25]
            );
        }
        assert_eq!(
            crate::synth_native("supertonic-3", Some(1.3), |native| Ok(vec![native])).unwrap(),
            [1.3]
        );
        assert_eq!(
            crate::synth_native("kokoro-82m-int8", Some(1.1), |native| Ok(vec![native])).unwrap(),
            [1.1]
        );
    }
    #[test]
    fn only_npu_compile_rejection_has_the_driver_failure_header() {
        use handy_tts_core::device::Device;
        for (device, compile_rejected, expected) in [
            (Device::Npu, true, "npu_compile_failed"),
            (Device::Npu, false, "load_failed"),
            (Device::Cpu, true, "load_failed"),
        ] {
            let mut controls = Vec::new();
            handy_tts_core::protocol::write_request(
                &mut controls,
                &Request::Load {
                    model_dir: "/fixture".into(),
                    voice: "fixture".into(),
                    threads: 4,
                    lanes: 1,
                    device,
                    cache_dir: "/fixture/cache".into(),
                },
            )
            .unwrap();
            handy_tts_core::protocol::write_request(&mut controls, &Request::Shutdown).unwrap();
            let mut replies = Vec::new();
            run(
                std::io::Cursor::new(controls),
                &mut replies,
                |_, _, _, _, _| {
                    let error = anyhow::anyhow!("fixture load failure");
                    Err(if compile_rejected {
                        error.context(crate::ov_graph::NpuCompileRejected)
                    } else {
                        error
                    })
                },
                || Ok(u64::MAX),
            )
            .unwrap();
            let header =
                handy_tts_core::protocol::read_header(&mut std::io::Cursor::new(replies)).unwrap();
            assert_eq!(header.status, Status::Error);
            assert_eq!(header.message.as_deref(), Some(expected));
        }
    }
    #[test]
    fn follow_up_missing_voice_logs_safe_stderr() {
        const CHILD: &str = "HANDY_TTS_TEST_MISSING_VOICE";
        if std::env::var_os(CHILD).is_some() {
            let mut controls = Vec::new();
            handy_tts_core::protocol::write_request(
                &mut controls,
                &Request::Load {
                    model_dir: "/fixture/private-parent".into(),
                    voice: "missing-voice".into(),
                    threads: 4,
                    lanes: 1,
                    device: handy_tts_core::device::Device::Cpu,
                    cache_dir: "/fixture/cache".into(),
                },
            )
            .unwrap();
            handy_tts_core::protocol::write_request(
                &mut controls,
                &Request::Synthesize {
                    id: 0,
                    text: "fixture-private-text".into(),
                    speed: None,
                },
            )
            .unwrap();
            handy_tts_core::protocol::write_request(&mut controls, &Request::Shutdown).unwrap();
            let mut replies = Vec::new();
            run(
                std::io::Cursor::new(controls),
                &mut replies,
                |_, _, _, _, _| {
                    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/missing-voice.bin");
                    read_voice_file(&path).context("fixture-private-error-text")?;
                    bail!("fixture file unexpectedly exists")
                },
                || Ok(u64::MAX),
            )
            .unwrap();
            let mut replies = std::io::Cursor::new(replies);
            let header = handy_tts_core::protocol::read_header(&mut replies).unwrap();
            assert_eq!(header.status, Status::Error);
            assert_eq!(header.message.as_deref(), Some("load_failed"));
            let header = handy_tts_core::protocol::read_header(&mut replies).unwrap();
            assert_eq!(header.status, Status::Error);
            assert_eq!(header.message.as_deref(), Some("chunk_failed"));
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::tests::follow_up_missing_voice_logs_safe_stderr",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "child test failed");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr,
            "handy-tts: load failed: file=\"missing-voice.bin\" kind=NotFound\nhandy-tts: chunk failed: kind=Other\n"
        );
    }

    #[test]
    fn follow_up_zero_id_is_invalid_even_with_cancel_sentinel() {
        for pending in [None, Some(0), Some(7)] {
            let mut controls = Vec::new();
            if let Some(id) = pending {
                handy_tts_core::protocol::write_request(&mut controls, &Request::Cancel { id })
                    .unwrap();
            }
            handy_tts_core::protocol::write_request(
                &mut controls,
                &Request::Synthesize {
                    id: 0,
                    text: "fixture-private-text".into(),
                    speed: None,
                },
            )
            .unwrap();
            handy_tts_core::protocol::write_request(&mut controls, &Request::Shutdown).unwrap();
            let mut replies = Vec::new();
            run(
                std::io::Cursor::new(controls),
                &mut replies,
                |_, _, _, _, _| panic!("invalid synthesis must not load"),
                || Ok(u64::MAX),
            )
            .unwrap();
            let header =
                handy_tts_core::protocol::read_header(&mut std::io::Cursor::new(replies)).unwrap();
            assert_eq!(header.id, 0);
            assert_eq!(header.status, Status::Error);
            assert_eq!(header.message.as_deref(), Some("chunk_failed"));
        }
    }

    #[test]
    fn parses_available_memory_without_defaulting_missing_data_to_safe() {
        assert_eq!(
            available_bytes("MemTotal: 20 kB\nMemAvailable: 1048576 kB\n").unwrap(),
            1073741824
        );
        assert!(available_bytes("MemFree: 1048576 kB\n").is_err());
        assert!(available_bytes("MemAvailable: bad kB\n").is_err());
    }
    #[test]
    fn rejects_bare_worker_invocation_and_unknown_models() {
        assert!(parse_model(Vec::new().into_iter()).is_err());
        assert!(parse_model(["--model".into(), "remote-api".into()].into_iter()).is_err());
        assert_eq!(
            parse_model(["--model".into(), "pocket-en".into()].into_iter()).unwrap(),
            "pocket-en"
        );
    }
    #[test]
    fn fake_engine_observes_cancellation_and_error_without_real_models() {
        struct Fake;
        impl SynthEngine for Fake {
            fn synthesize(
                &mut self,
                _: &str,
                _: Option<f32>,
                cancel: &AtomicBool,
            ) -> Result<Vec<f32>> {
                ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
                Ok(vec![0.25, -0.5])
            }
            fn sample_rate(&self) -> u32 {
                24000
            }
        }
        let mut fake = Fake;
        let cancel = AtomicBool::new(false);
        assert_eq!(fake.synthesize("hi", None, &cancel).unwrap(), [0.25, -0.5]);
        cancel.store(true, Ordering::SeqCst);
        assert!(fake.synthesize("hi", None, &cancel).is_err());

        for (available, expected_status, expected_loads) in [
            (
                handy_tts_core::memory::MIN_AVAILABLE_BYTES - 1,
                Status::Error,
                0,
            ),
            (
                handy_tts_core::memory::MIN_AVAILABLE_BYTES,
                Status::Ready,
                1,
            ),
        ] {
            let mut controls = Vec::new();
            handy_tts_core::protocol::write_request(
                &mut controls,
                &Request::Load {
                    model_dir: "/fixture/model".into(),
                    voice: "alba".into(),
                    threads: 4,
                    lanes: 1,
                    device: handy_tts_core::device::Device::Cpu,
                    cache_dir: "/fixture/cache".into(),
                },
            )
            .unwrap();
            handy_tts_core::protocol::write_request(&mut controls, &Request::Shutdown).unwrap();
            let mut replies = Vec::new();
            let mut loads = 0;
            run(
                std::io::Cursor::new(controls),
                &mut replies,
                |_, _, _, _, _| {
                    loads += 1;
                    Ok(Box::new(Fake))
                },
                || Ok(available),
            )
            .unwrap();
            let reply =
                handy_tts_core::protocol::read_header(&mut std::io::Cursor::new(replies)).unwrap();
            assert_eq!(reply.status, expected_status);
            assert_eq!(loads, expected_loads);
            if expected_status == Status::Error {
                assert_eq!(reply.message.as_deref(), Some("not_enough_memory"));
            }
        }
    }

    mod gpu_classes {
        use super::*;
        struct Capped {
            words: Vec<String>,
        }
        impl SynthEngine for Capped {
            fn sample_rate(&self) -> u32 {
                1
            }
            fn device(&self) -> handy_tts_core::device::Device {
                handy_tts_core::device::Device::Gpu
            }
            fn synthesize(
                &mut self,
                text: &str,
                _: Option<f32>,
                _: &AtomicBool,
            ) -> Result<Vec<f32>> {
                if text.len() > 20 {
                    return Err(crate::padding::StaticCapacity.into());
                }
                let words = text
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let samples = words
                    .iter()
                    .map(|word| word.parse::<f32>().unwrap())
                    .collect();
                self.words.extend(words);
                Ok(samples)
            }
        }
        #[test]
        fn wbfix_supertonic_cap_splits_unbroken_text_before_inference() {
            struct TextCap {
                delivered: String,
            }
            impl SynthEngine for TextCap {
                fn sample_rate(&self) -> u32 {
                    24000
                }
                fn fits_text(&self, text: &str) -> bool {
                    crate::supertonic::prepare_text(text).chars().count() <= 304
                }
                fn synthesize(
                    &mut self,
                    text: &str,
                    _: Option<f32>,
                    _: &AtomicBool,
                ) -> Result<Vec<f32>> {
                    assert!(
                        self.fits_text(text),
                        "text length must fit before inference"
                    );
                    self.delivered.push_str(text);
                    Ok(text
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .map(|c| c as u32 as f32)
                        .collect())
                }
            }
            for count in [400, 20_000] {
                let text = format!("{} Following text.", "@".repeat(count));
                let mut engine = TextCap {
                    delivered: String::new(),
                };
                let samples =
                    synthesize_bounded(&mut engine, &text, Some(1.0), &AtomicBool::new(false))
                        .unwrap();
                assert_eq!(
                    samples,
                    text.chars()
                        .filter(|c| !c.is_whitespace())
                        .map(|c| c as u32 as f32)
                        .collect::<Vec<_>>()
                );
                assert!(engine.delivered.ends_with("Following text."));
            }
        }
        #[test]
        fn over_cap_request_splits_and_keeps_all_words() {
            let text = (1..=100)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let mut engine = Capped { words: vec![] };
            let samples =
                synthesize_bounded(&mut engine, &text, Some(1.0), &AtomicBool::new(false)).unwrap();
            assert_eq!(engine.words.join(" "), text);
            assert_eq!(samples, (1..=100).map(|n| n as f32).collect::<Vec<_>>());
        }
    }
}
