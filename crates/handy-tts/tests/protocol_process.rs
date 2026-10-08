use handy_tts_core::lifecycle::{MIN_AVAILABLE_BYTES, State, UnloadAfter};
use handy_tts_core::managed::{Managed, ProcessBackend};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct WorkerProcess {
    child: Option<Child>,
}

impl ProcessBackend for WorkerProcess {
    type Error = std::io::Error;
    fn start(&mut self) -> Result<(), Self::Error> {
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_handy-tts"))
                .args(["--model", "pocket-en"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().is_ok_and(|status| status.is_none()))
    }
}

#[test]
fn actual_worker_is_killed_and_reaped_on_explicit_and_idle_unload() {
    for manual in [true, false] {
        let mut manager = Managed::new(WorkerProcess { child: None });
        manager
            .ensure_started(MIN_AVAILABLE_BYTES, Duration::ZERO)
            .unwrap();
        let pid = manager.backend.child.as_ref().unwrap().id();
        assert!(manager.backend.alive());
        assert_eq!(manager.lifecycle.state, State::Ready);
        if manual {
            manager.unload();
        } else {
            manager.idle_tick(
                Duration::from_secs(6),
                UnloadAfter::Idle(Duration::from_secs(5)),
            );
        }
        assert_eq!(manager.lifecycle.state, State::Unloaded);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(manager.backend.child.is_none());
    }
}

#[test]
fn oversized_npu_chunk_synthesizes_every_word_with_static_token_and_frame_caps() {
    use handy_tts::runtime::{self, SynthEngine};
    use handy_tts_core::{
        device::Device,
        protocol::{self, Header, Pcm, Request, Status},
    };
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    struct Capped {
        spoken: Arc<Mutex<Vec<String>>>,
        cap: usize,
    }
    impl SynthEngine for Capped {
        fn synthesize(
            &mut self,
            text: &str,
            _: Option<f32>,
            _: &AtomicBool,
        ) -> anyhow::Result<Vec<f32>> {
            let words: Vec<_> = text.split_whitespace().collect();
            handy_tts::padding::pad(
                &[1, words.len() as i64],
                &[1, self.cap as i64],
                &vec![0u8; words.len()],
            )?;
            self.spoken
                .lock()
                .unwrap()
                .extend(words.iter().map(|word| (*word).to_owned()));
            Ok(vec![0.25; words.len() * 800])
        }
        fn device(&self) -> Device {
            Device::Npu
        }
        fn sample_rate(&self) -> u32 {
            8000
        }
    }
    for cap in [64, 8] {
        let text = (0..100)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let mut input = Vec::new();
        protocol::write_request(
            &mut input,
            &Request::Load {
                model_dir: "fixture".into(),
                voice: "alba".into(),
                threads: 4,
                lanes: 1,
                device: Device::Npu,
                cache_dir: "cache".into(),
            },
        )
        .unwrap();
        protocol::write_request(
            &mut input,
            &Request::Synthesize {
                id: 1,
                text: text.clone(),
                speed: None,
            },
        )
        .unwrap();
        // Keep the reader open until synthesis has completed.
        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        use std::io::Write;
        writer.write_all(&input).unwrap();
        let (replies, mut output) = std::os::unix::net::UnixStream::pair().unwrap();
        replies
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let copied = spoken.clone();
        let worker = std::thread::spawn(move || {
            runtime::run(
                std::io::BufReader::new(reader),
                &mut output,
                |_, _, _, _, _| {
                    Ok(Box::new(Capped {
                        spoken: copied.clone(),
                        cap,
                    }))
                },
                || Ok(u64::MAX),
            )
            .unwrap();
        });
        let mut replies = std::io::BufReader::new(replies);
        let _: Header = protocol::read_header(&mut replies).unwrap();
        let header = protocol::read_header(&mut replies).unwrap();
        assert_eq!(header.status, Status::Ok);
        let pcm: Pcm = protocol::read_pcm_after_header(&mut replies, header, 1).unwrap();
        protocol::write_request(&mut writer, &Request::Shutdown).unwrap();
        worker.join().unwrap();
        assert_eq!(pcm.samples.len(), 100 * 800);
        assert_eq!(
            *spoken.lock().unwrap(),
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn process_interleaves_replies_by_id() {
    use handy_tts::runtime::{self, SynthEngine};
    use handy_tts_core::protocol::{self, Status};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicBool, Ordering};

    const CHILD: &str = "HANDY_TTS_TEST_LANE_PROCESS";
    if std::env::var_os(CHILD).is_some() {
        struct Fake;
        impl SynthEngine for Fake {
            fn synthesize(
                &mut self,
                text: &str,
                _: Option<f32>,
                cancelled: &AtomicBool,
            ) -> anyhow::Result<Vec<f32>> {
                std::thread::sleep(Duration::from_millis(if text == "slow" { 200 } else { 10 }));
                anyhow::ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
                Ok(vec![if text == "slow" { 0.25 } else { -0.5 }; 2048])
            }
            fn sample_rate(&self) -> u32 {
                24000
            }
        }
        runtime::run(
            BufReader::new(std::io::stdin()),
            std::io::stdout(),
            |_, _, _, _, _| Ok(Box::new(Fake)),
            || Ok(u64::MAX),
        )
        .unwrap();
        return;
    }
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process_interleaves_replies_by_id",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut process = WorkerProcess { child: Some(child) };
    let child = process.child.as_mut().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    writeln!(input, "{}", serde_json::json!({"type":"load","model_dir":"fixture","voice":"alba","threads":4,"lanes":2})).unwrap();
    let ready = loop {
        let mut line = String::new();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.starts_with('{') {
            break serde_json::from_str::<protocol::Header>(&line).unwrap();
        }
    };
    assert_eq!(ready.status, Status::Ready);
    for (id, text) in [(1, "slow"), (2, "fast")] {
        protocol::write_request(
            &mut input,
            &protocol::Request::Synthesize {
                id,
                text: text.into(),
                speed: None,
            },
        )
        .unwrap();
    }
    assert_eq!(
        protocol::read_pcm(&mut output, 2).unwrap().samples,
        vec![-0.5; 2048]
    );
    assert_eq!(
        protocol::read_pcm(&mut output, 1).unwrap().samples,
        vec![0.25; 2048]
    );
    protocol::write_request(&mut input, &protocol::Request::Shutdown).unwrap();
    assert!(process.child.as_mut().unwrap().wait().unwrap().success());
    process.child = None;
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}
