use anyhow::{Context, Result, bail, ensure};
use handy_tts_core::protocol::{Header, Request, Status, write_reply};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

pub trait SynthEngine: Send {
    fn synthesize(
        &mut self,
        text: &str,
        speed: Option<f32>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<f32>>;
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

pub fn run(
    reader: impl BufRead + Send + 'static,
    mut writer: impl Write,
    mut load: impl FnMut(
        &Path,
        &str,
        usize,
        handy_tts_core::device::Device,
        &Path,
    ) -> Result<Box<dyn SynthEngine>>,
    memory: impl Fn() -> Result<u64>,
) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Request>(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let active_id = Arc::new(AtomicU64::new(0));
    let cancelled_id = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let cancel_reader = Arc::clone(&cancel);
    let active_reader = Arc::clone(&active_id);
    let cancelled_reader = Arc::clone(&cancelled_id);
    let shutdown_reader = Arc::clone(&shutdown);
    let controls = std::thread::Builder::new()
        .name("tts-controls".into())
        .spawn(move || -> Result<()> {
            let mut reader = reader;
            loop {
                let mut bytes = Vec::new();
                let count = reader
                    .by_ref()
                    .take(16385)
                    .read_until(b'\n', &mut bytes)
                    .context("read control")?;
                if count == 0 {
                    shutdown_reader.store(true, Ordering::SeqCst);
                    cancel_reader.store(true, Ordering::SeqCst);
                    crate::cancel_inference();
                    break;
                }
                ensure!(
                    count <= 16384 && bytes.last() == Some(&b'\n'),
                    "control line too long"
                );
                let request: Request = serde_json::from_slice(&bytes).context("decode control")?;
                match request {
                    Request::Cancel { id } => {
                        cancelled_reader.store(id, Ordering::SeqCst);
                        if active_reader.load(Ordering::SeqCst) == id {
                            cancel_reader.store(true, Ordering::SeqCst);
                            crate::cancel_inference();
                        }
                    }
                    Request::Shutdown => {
                        shutdown_reader.store(true, Ordering::SeqCst);
                        cancel_reader.store(true, Ordering::SeqCst);
                        crate::cancel_inference();
                        let _ = tx.send(Request::Shutdown);
                        break;
                    }
                    request => {
                        if tx.send(request).is_err() {
                            break;
                        }
                    }
                }
            }
            Ok(())
        })
        .context("start control reader")?;
    let mut engine: Option<Box<dyn SynthEngine>> = None;
    for request in rx {
        match request {
            Request::Load {
                model_dir,
                voice,
                threads,
                device,
                cache_dir,
            } => {
                if memory()? < handy_tts_core::memory::MIN_AVAILABLE_BYTES {
                    write_reply(
                        &mut writer,
                        &Header {
                            device: None,
                            placement: None,
                            id: 0,
                            status: Status::Error,
                            sample_rate: None,
                            samples: None,
                            message: Some("not_enough_memory".into()),
                        },
                        &[],
                    )?;
                    continue;
                }
                let loaded = (|| {
                    ensure!((1..=8).contains(&threads), "invalid threads");
                    load(&model_dir, &voice, threads, device, &cache_dir)
                })();
                match loaded {
                    Ok(value) => {
                        let actual_device = value.device();
                        let actual_placement = value.placement();
                        engine = Some(value);
                        write_reply(
                            &mut writer,
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
                    Err(_) => {
                        write_reply(
                            &mut writer,
                            &Header {
                                device: None,
                                placement: None,
                                id: 0,
                                status: Status::Error,
                                sample_rate: None,
                                samples: None,
                                message: Some("load_failed".into()),
                            },
                            &[],
                        )?;
                    }
                }
            }
            Request::Synthesize { id, text, speed } => {
                active_id.store(id, Ordering::SeqCst);
                cancel.store(
                    cancelled_id.load(Ordering::SeqCst) == id || shutdown.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
                let result = (|| {
                    ensure!(
                        id != 0 && !text.trim().is_empty(),
                        "invalid synthesis request"
                    );
                    if let Some(speed) = speed {
                        ensure!((0.5..=2.0).contains(&speed), "invalid speed");
                    }
                    let engine = engine.as_mut().context("model not loaded")?;
                    let samples = engine.synthesize(&text, speed, &cancel)?;
                    ensure!(
                        !samples.is_empty()
                            && samples.len() <= handy_tts_core::protocol::MAX_SAMPLES
                            && samples.iter().all(|v| v.is_finite()),
                        "invalid PCM"
                    );
                    Ok::<_, anyhow::Error>((engine.sample_rate(), samples))
                })();
                if cancel.load(Ordering::SeqCst) {
                    write_reply(
                        &mut writer,
                        &Header {
                            device: None,
                            placement: None,
                            id,
                            status: Status::Cancelled,
                            sample_rate: None,
                            samples: None,
                            message: Some("cancelled".into()),
                        },
                        &[],
                    )?;
                } else {
                    match result {
                        Ok((rate, samples)) => write_reply(
                            &mut writer,
                            &Header {
                                device: None,
                                placement: None,
                                id,
                                status: Status::Ok,
                                sample_rate: Some(rate),
                                samples: Some(samples.len()),
                                message: None,
                            },
                            &samples,
                        )?,
                        Err(_) => write_reply(
                            &mut writer,
                            &Header {
                                device: None,
                                placement: None,
                                id,
                                status: Status::Error,
                                sample_rate: None,
                                samples: None,
                                message: Some("chunk_failed".into()),
                            },
                            &[],
                        )?,
                    }
                }
                active_id.store(0, Ordering::SeqCst);
                let _ = cancelled_id.compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst);
            }
            Request::Shutdown => break,
            Request::Cancel { .. } => bail!("cancel reached synthesis loop"),
        }
    }
    drop(engine);
    if controls.is_finished() {
        controls
            .join()
            .map_err(|_| anyhow::anyhow!("control reader panicked"))??;
    }
    Ok(())
}

pub struct EngineAdapter {
    engine: crate::Engine,
    device: handy_tts_core::device::Device,
    placement: std::collections::BTreeMap<String, handy_tts_core::device::Device>,
}
impl SynthEngine for EngineAdapter {
    fn synthesize(
        &mut self,
        text: &str,
        speed: Option<f32>,
        cancelled: &AtomicBool,
    ) -> Result<Vec<f32>> {
        ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
        crate::reset_inference()?;
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
    let config = crate::backend_config::config(model, root, device, cache)?;
    let placement = config.placement.clone();
    let engine =
        crate::graph::with_config(config, || crate::Engine::load(model, root, threads, voice))?;
    Ok(Box::new(EngineAdapter {
        engine,
        device,
        placement,
    }))
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
pub fn model_path(path: PathBuf) -> Result<PathBuf> {
    ensure!(path.is_dir(), "model directory missing");
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
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
}
