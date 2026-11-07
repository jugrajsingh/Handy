use super::TtsError;
use handy_tts_core::catalog::ModelEntry;
use handy_tts_core::protocol::{
    read_header, read_pcm_after_header, write_request, Header, Pcm, Request, Status,
};
use std::io::BufReader;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub struct LoadSpec<'a> {
    pub binary: &'a Path,
    pub root: &'a Path,
    pub entry: &'a ModelEntry,
    pub voice: &'a str,
    pub device: handy_tts_core::device::Device,
    pub cache: &'a Path,
    pub home: &'a Path,
    pub lanes: usize,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct WorkerReady {
    pub device: handy_tts_core::device::Device,
    pub placement: std::collections::BTreeMap<String, handy_tts_core::device::Device>,
}
pub trait Control: Clone + Send + Sync + 'static {
    fn cancel(&self, id: u64);
    fn kill(&self);
    fn preparation(&self) -> Option<handy_tts_core::device::Preparation> {
        None
    }
}
/// Maps a non-Ok chunk reply to its error; a busy worker is retryable.
fn reply_error(header: &handy_tts_core::protocol::Header) -> Result<Pcm, TtsError> {
    let message = header.message.as_deref().unwrap_or("no message");
    log::warn!(
        "TTS worker replied {:?} to request {}: {message}",
        header.status,
        header.id
    );
    if header.status == Status::Error && message == "busy" {
        Err(TtsError::Busy)
    } else {
        Err(TtsError::Worker)
    }
}
/// Returns the worker's own failure line, without its prefix and line ending.
fn worker_failure_line(line: &str) -> Option<&str> {
    line.strip_prefix("handy-tts: ").map(str::trim_end)
}
fn read_worker_diagnostics(
    mut reader: impl std::io::BufRead,
    preparation: &Arc<Mutex<Option<handy_tts_core::device::Preparation>>>,
) {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&bytes);
        if let Some(failure) = worker_failure_line(&line) {
            log::warn!("TTS worker reported: {failure}");
        }
        if let Some(json) = line.strip_prefix("HANDY_TTS_PROGRESS ") {
            if let Ok(value) = serde_json::from_str(json) {
                *preparation.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
            }
        }
    }
}

pub trait Transport: Send + 'static {
    type Control: Control;
    fn control(&self) -> Self::Control;
    fn start(&mut self, spec: &LoadSpec<'_>) -> Result<WorkerReady, TtsError>;
    fn submit(
        &mut self,
        id: u64,
        text: &str,
        speed: Option<f32>,
    ) -> Result<mpsc::Receiver<Result<Pcm, TtsError>>, TtsError>;
    fn set_lanes(&mut self, lanes: usize) -> Result<(), TtsError>;
    fn synthesis_timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn alive(&mut self) -> bool;
    fn stop(&mut self);
}

#[derive(Clone, Default)]
pub struct WorkerControl {
    child: Arc<Mutex<Option<Child>>>,
    input: Arc<Mutex<Option<ChildStdin>>>,
    preparation: Arc<Mutex<Option<handy_tts_core::device::Preparation>>>,
}
impl WorkerControl {
    pub fn cancel(&self, id: u64) {
        if let Some(input) = self
            .input
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            let _ = write_request(input, &Request::Cancel { id });
        }
    }
    pub fn kill(&self) {
        if let Some(child) = self
            .child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            let _ = child.kill();
        }
    }
}
struct Reply {
    header: Header,
    pcm: Option<Pcm>,
}
type PendingReplies =
    Arc<Mutex<std::collections::BTreeMap<u64, mpsc::Sender<Result<Pcm, TtsError>>>>>;

#[derive(Default)]
pub struct WorkerBackend {
    control: WorkerControl,
    replies: Option<mpsc::Receiver<Result<Reply, TtsError>>>,
    pending: PendingReplies,
    #[cfg(test)]
    synthesis_timeout: Option<Duration>,
}
impl WorkerBackend {
    #[cfg(test)]
    pub(super) fn with_synthesis_timeout(timeout: Duration) -> Self {
        Self {
            control: WorkerControl::default(),
            replies: None,
            pending: Arc::default(),
            synthesis_timeout: Some(timeout),
        }
    }
    pub fn control(&self) -> WorkerControl {
        self.control.clone()
    }
    pub fn start(&mut self, spec: &LoadSpec<'_>) -> Result<WorkerReady, TtsError> {
        let LoadSpec {
            binary,
            root,
            entry,
            voice,
            device,
            cache,
            home,
            lanes,
        } = *spec;
        self.stop();
        self.pending = Arc::default();
        self.control
            .preparation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let mut command = Command::new(binary);
        command
            .args(["--model", entry.id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let driver = if device == handy_tts_core::device::Device::Npu {
            handy_tts_core::worker_env::discover_driver(&home.join(".local/share/intel-npu"))
                .map_err(|_| TtsError::Start)?
        } else {
            None
        };
        let mut libraries = binary
            .parent()
            .unwrap_or(Path::new("."))
            .as_os_str()
            .to_owned();
        if let Some(parent) = std::env::var_os("LD_LIBRARY_PATH") {
            libraries.push(":");
            libraries.push(parent);
        }
        command.env("LD_LIBRARY_PATH", &libraries);
        command.envs(handy_tts_core::worker_env::worker_environment(
            device,
            driver.as_ref(),
            Some(&libraries),
        ));
        if binary
            .parent()
            .is_some_and(|p| p.join("libopenvino_c.so.2641").is_file())
        {
            command.env(
                "HANDY_OPENVINO_LIBRARY",
                binary.with_file_name("libopenvino_c.so.2641"),
            );
        }
        #[cfg(target_os = "linux")]
        crate::process_supervisor::set_parent_death_signal(&mut command);
        let mut child = crate::process_supervisor::spawn_from_supervisor(command)
            .map_err(|_| TtsError::Start)?;
        let input = child.stdin.take().ok_or(TtsError::Protocol)?;
        let output = child.stdout.take().ok_or(TtsError::Protocol)?;
        *self.control.input.lock().unwrap_or_else(|p| p.into_inner()) = Some(input);
        let stderr = child.stderr.take();
        *self.control.child.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
        if let Some(stderr) = stderr {
            let preparation = self.control.preparation.clone();
            std::thread::Builder::new()
                .name("tts-stderr".into())
                .spawn(move || {
                    read_worker_diagnostics(BufReader::new(stderr), &preparation);
                })
                .map_err(|_| TtsError::Start)?;
        }
        let (tx, rx) = mpsc::channel();
        let pending = self.pending.clone();
        std::thread::Builder::new()
            .name("tts-replies".into())
            .spawn(move || {
                let mut reader = BufReader::new(output);
                loop {
                    let result: Result<Reply, TtsError> = (|| {
                        let header = read_header(&mut reader).map_err(|_| TtsError::Protocol)?;
                        let pcm = if header.status == Status::Ok {
                            Some(
                                read_pcm_after_header(&mut reader, header.clone(), header.id)
                                    .map_err(|_| TtsError::Protocol)?,
                            )
                        } else {
                            None
                        };
                        Ok(Reply { header, pcm })
                    })();
                    let failed = result.is_err();
                    match result {
                        Ok(reply) if reply.header.id == 0 => {
                            if tx.send(Ok(reply)).is_err() {
                                break;
                            }
                        }
                        Ok(reply) => {
                            let sender = pending
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .remove(&reply.header.id);
                            if let Some(sender) = sender {
                                let result = if reply.header.status == Status::Ok {
                                    reply.pcm.ok_or(TtsError::Protocol)
                                } else {
                                    reply_error(&reply.header)
                                };
                                let _ = sender.send(result);
                            }
                        }
                        Err(error) => {
                            let requests = std::mem::take(
                                &mut *pending.lock().unwrap_or_else(|p| p.into_inner()),
                            );
                            for sender in requests.into_values() {
                                let _ = sender.send(Err(error.clone()));
                            }
                            let _ = tx.send(Err(error));
                        }
                    }
                    if failed {
                        break;
                    }
                }
            })
            .map_err(|_| TtsError::Start)?;
        self.replies = Some(rx);
        self.send(&Request::Load {
            model_dir: root.to_path_buf(),
            voice: voice.into(),
            threads: entry.threads,
            lanes: lanes.clamp(1, 3),
            device,
            cache_dir: cache.to_path_buf(),
        })?;
        let reply = self.receive(Duration::from_secs(120))?;
        if reply.header.id != 0 || reply.header.status != Status::Ready {
            self.stop();
            return Err(
                if reply.header.id == 0
                    && reply.header.status == Status::Error
                    && reply.header.message.as_deref() == Some("not_enough_memory")
                {
                    TtsError::Memory
                } else if reply.header.id == 0
                    && reply.header.status == Status::Error
                    && reply.header.message.as_deref() == Some("npu_compile_failed")
                    && device == handy_tts_core::device::Device::Npu
                {
                    TtsError::Device(handy_tts_core::device::DeviceError::CompileFailed)
                } else {
                    TtsError::Start
                },
            );
        }
        Ok(WorkerReady {
            device: reply.header.device.ok_or(TtsError::Protocol)?,
            placement: reply.header.placement.ok_or(TtsError::Protocol)?,
        })
    }
    fn send(&self, request: &Request) -> Result<(), TtsError> {
        let mut guard = self.control.input.lock().unwrap_or_else(|p| p.into_inner());
        write_request(guard.as_mut().ok_or(TtsError::Protocol)?, request)
            .map_err(|_| TtsError::Protocol)
    }
    fn receive(&self, timeout: Duration) -> Result<Reply, TtsError> {
        self.replies
            .as_ref()
            .ok_or(TtsError::Protocol)?
            .recv_timeout(timeout)
            .map_err(|_| TtsError::Worker)?
    }
    /// Submits one request and returns its dedicated PCM reply channel.
    pub fn submit(
        &mut self,
        id: u64,
        text: &str,
        speed: Option<f32>,
    ) -> Result<mpsc::Receiver<Result<Pcm, TtsError>>, TtsError> {
        if id == 0 {
            return Err(TtsError::Protocol);
        }
        let (send, receive) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.contains_key(&id) || pending.len() >= 3 {
                return Err(TtsError::Protocol);
            }
            pending.insert(id, send);
        }
        if let Err(error) = self.send(&Request::Synthesize {
            id,
            text: text.into(),
            speed,
        }) {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(error);
        }
        Ok(receive)
    }
    /// Sends a lane reduction to the loaded worker.
    pub fn set_lanes(&mut self, lanes: usize) -> Result<(), TtsError> {
        self.send(&Request::SetLanes {
            lanes: lanes.clamp(1, 3),
        })
    }
    #[cfg(test)]
    fn synthesize_with_timeout(
        &mut self,
        id: u64,
        text: &str,
        speed: Option<f32>,
        timeout: Duration,
    ) -> Result<Pcm, TtsError> {
        let receive = self.submit(id, text, speed)?;
        match receive.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                self.stop();
                Err(TtsError::Worker)
            }
        }
    }
    pub fn alive(&mut self) -> bool {
        let mut guard = self.control.child.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(Ok(Some(_))) => {
                guard.take();
                false
            }
            Some(Err(_)) | None => false,
        }
    }
    pub fn stop(&mut self) {
        self.control.kill();
        let child = self
            .control
            .child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(mut child) = child {
            crate::process_supervisor::stop_child(&mut child);
        }
        self.control
            .input
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.replies = None;
        let requests = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        for sender in requests.into_values() {
            let _ = sender.send(Err(TtsError::Cancelled));
        }
    }
}
impl Drop for WorkerBackend {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Control for WorkerControl {
    fn cancel(&self, id: u64) {
        WorkerControl::cancel(self, id);
    }
    fn kill(&self) {
        WorkerControl::kill(self);
    }
    fn preparation(&self) -> Option<handy_tts_core::device::Preparation> {
        self.preparation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}
impl Transport for WorkerBackend {
    type Control = WorkerControl;
    fn control(&self) -> Self::Control {
        WorkerBackend::control(self)
    }
    fn start(&mut self, spec: &LoadSpec<'_>) -> Result<WorkerReady, TtsError> {
        WorkerBackend::start(self, spec)
    }
    fn submit(
        &mut self,
        id: u64,
        text: &str,
        speed: Option<f32>,
    ) -> Result<mpsc::Receiver<Result<Pcm, TtsError>>, TtsError> {
        WorkerBackend::submit(self, id, text, speed)
    }
    fn set_lanes(&mut self, lanes: usize) -> Result<(), TtsError> {
        WorkerBackend::set_lanes(self, lanes)
    }
    fn synthesis_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(timeout) = self.synthesis_timeout {
            return timeout;
        }
        Duration::from_secs(60)
    }
    fn alive(&mut self) -> bool {
        WorkerBackend::alive(self)
    }
    fn stop(&mut self) {
        WorkerBackend::stop(self);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn busy_reply_is_retryable_and_other_failures_stop_the_chunk() {
        let header = |status, message: &str| Header {
            device: None,
            placement: None,
            id: 7,
            status,
            sample_rate: None,
            samples: None,
            message: Some(message.into()),
        };
        assert_eq!(
            reply_error(&header(Status::Error, "busy")),
            Err(TtsError::Busy)
        );
        assert_eq!(
            reply_error(&header(Status::Error, "chunk_failed")),
            Err(TtsError::Worker)
        );
        assert_eq!(
            reply_error(&header(Status::Cancelled, "cancelled")),
            Err(TtsError::Worker)
        );
    }
    #[test]
    fn worker_failure_lines_are_kept_and_progress_lines_are_not() {
        assert_eq!(
            worker_failure_line("handy-tts: chunk failed: kind=OpenVinoGeneralError\n"),
            Some("chunk failed: kind=OpenVinoGeneralError")
        );
        assert_eq!(
            worker_failure_line("HANDY_TTS_PROGRESS {\"state\":\"ready\"}\n"),
            None
        );
        assert_eq!(worker_failure_line("SYNTH_START lane=1 id=4\n"), None);
    }
    #[test]
    fn followup_worker_diagnostics_survive_invalid_utf8() {
        use handy_tts_core::device::{Device, Preparation};
        let progress = Arc::default();
        read_worker_diagnostics(
            std::io::Cursor::new(
                b"bad byte \xff\nHANDY_TTS_PROGRESS {\"state\":\"ready\",\"device\":\"gpu\"}",
            ),
            &progress,
        );
        assert_eq!(
            *progress.lock().unwrap(),
            Some(Preparation::Ready {
                device: Device::Gpu
            }),
            "invalid diagnostics must not stop progress parsing"
        );
        read_worker_diagnostics(std::io::Cursor::new(b"HANDY_TTS_PROGRESS {\"state\":\"preparing\",\"device\":\"gpu\",\"completed\":1,\"total\":2,\"graph\":\"graph-\xff\"}\n"), &progress);
        assert_eq!(
            *progress.lock().unwrap(),
            Some(Preparation::Preparing {
                device: Device::Gpu,
                completed: 1,
                total: 2,
                graph: "graph-�".into()
            }),
            "progress strings must use lossy decoding"
        );
    }

    #[test]
    fn wbfix3_worker_diagnostics_do_not_log_private_output() {
        if std::env::var_os("HANDY_TEST_DIAGNOSTIC_LOG").is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tts::backend::tests::wbfix3_worker_diagnostics_do_not_log_private_output",
                    "--nocapture",
                ])
                .env("HANDY_TEST_DIAGNOSTIC_LOG", "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "worker diagnostics must not reach the log sink"
            );
            return;
        }
        struct Logs(Mutex<Vec<String>>);
        impl log::Log for Logs {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, r: &log::Record<'_>) {
                self.0.lock().unwrap().push(r.args().to_string());
            }
            fn flush(&self) {}
        }
        static LOGS: Logs = Logs(Mutex::new(Vec::new()));
        log::set_logger(&LOGS).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
        let progress = Arc::default();
        read_worker_diagnostics(
            std::io::Cursor::new(b"fixture-key /private/fixture/model\n"),
            &progress,
        );
        assert!(
            LOGS.0.lock().unwrap().is_empty(),
            "worker diagnostics must not reach the log sink"
        );
        read_worker_diagnostics(
            std::io::Cursor::new(b"HANDY_TTS_PROGRESS {\"state\":\"ready\",\"device\":\"gpu\"}\n"),
            &progress,
        );
        assert_eq!(
            *progress.lock().unwrap(),
            Some(handy_tts_core::device::Preparation::Ready {
                device: handy_tts_core::device::Device::Gpu
            })
        );
    }

    #[test]
    fn reader_routes_out_of_order_replies_by_id() {
        use handy_tts_core::device::Device;
        use std::os::unix::fs::PermissionsExt;
        let fixture =
            tempfile::tempdir_in(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap()).unwrap();
        let binary = fixture.path().join("out-of-order.py");
        std::fs::write(&binary, r#"#!/usr/bin/python3
import json, struct, sys
pending = []
for line in sys.stdin:
    request = json.loads(line)
    if request['type'] == 'load':
        assert request['lanes'] == 3
        print(json.dumps({'id': 0, 'status': 'ready', 'device': 'cpu', 'placement': {'main': 'cpu'}, 'samples': 0}), flush=True)
    elif request['type'] == 'synthesize':
        pending.append(request['id'])
        if len(pending) == 3:
            for id in [pending[2], pending[0], pending[1]]:
                print(json.dumps({'id': id, 'status': 'ok', 'sample_rate': 24000, 'samples': id}), flush=True)
                sys.stdout.buffer.write(struct.pack('<' + 'f' * id, *([float(id)] * id)))
                sys.stdout.buffer.flush()
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut backend = WorkerBackend::default();
        backend
            .start(&LoadSpec {
                binary: &binary,
                root: fixture.path(),
                entry: handy_tts_core::catalog::entry("pocket-en").unwrap(),
                voice: "alba",
                device: Device::Cpu,
                cache: fixture.path(),
                home: fixture.path(),
                lanes: 3,
            })
            .unwrap();
        let receivers: Vec<_> = (1..=3)
            .map(|id| backend.submit(id, "sentence", Some(1.0)).unwrap())
            .collect();
        for (index, receive) in receivers.into_iter().enumerate() {
            let id = index as u64 + 1;
            let pcm = receive
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(pcm.id, id);
            assert_eq!(pcm.samples, vec![id as f32; id as usize]);
        }
        assert!(backend.alive());
    }
    #[test]
    fn cancelled_reply_is_drained_before_retry_and_worker_stays_alive() {
        use handy_tts_core::device::Device;
        use std::io::{Read, Write};
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let fixture =
            tempfile::tempdir_in(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap()).unwrap();
        let socket = fixture.path().join("gate.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let binary = fixture.path().join("late-worker.py");
        std::fs::write(&binary, r#"#!/usr/bin/python3
import json, pathlib, socket, struct, sys
root = None
for line in sys.stdin:
    request = json.loads(line)
    if request['type'] == 'load':
        root = pathlib.Path(request['model_dir'])
        print(json.dumps({'id': 0, 'status': 'ready', 'device': 'cpu', 'placement': {'main': 'cpu'}, 'samples': 0}), flush=True)
    elif request['type'] == 'cancel':
        (root / 'cancelled').write_text(str(request['id']))
    elif request['type'] == 'synthesize':
        if request['id'] == 1:
            gate = socket.socket(socket.AF_UNIX)
            gate.connect(str(root / 'gate.sock'))
            gate.sendall(b'S')
            gate.recv(1)
            gate.close()
        print(json.dumps({'id': request['id'], 'status': 'ok', 'sample_rate': 24000, 'samples': 1}), flush=True)
        sys.stdout.buffer.write(struct.pack('<f', float(request['id'])))
        sys.stdout.buffer.flush()
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut backend = WorkerBackend::default();
        backend
            .start(&LoadSpec {
                binary: &binary,
                root: fixture.path(),
                entry: handy_tts_core::catalog::entry("pocket-en").unwrap(),
                voice: "alba",
                device: Device::Cpu,
                cache: fixture.path(),
                home: fixture.path(),
                lanes: 1,
            })
            .unwrap();
        let child = backend.control.child.lock().unwrap().as_ref().unwrap().id();
        backend
            .send(&Request::Synthesize {
                id: 1,
                text: "A".into(),
                speed: None,
            })
            .unwrap();
        backend.control.cancel(1);
        let (mut gate, _) = listener.accept().unwrap();
        gate.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut signal = [0];
        gate.read_exact(&mut signal).unwrap();
        assert_eq!(signal, [b'S']);
        gate.write_all(b"R").unwrap();
        for id in [2, 3] {
            let pcm = backend
                .synthesize_with_timeout(id, "retry", None, Duration::from_secs(2))
                .unwrap();
            assert_eq!(pcm.id, id);
            assert_eq!(pcm.samples, [id as f32]);
            assert!(backend.alive());
            assert_eq!(
                backend.control.child.lock().unwrap().as_ref().unwrap().id(),
                child
            );
        }
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("cancelled")).unwrap(),
            "1"
        );
        backend.stop();
    }
    #[test]
    fn control_and_reaper_share_the_owned_child_instead_of_a_reusable_pid() {
        use handy_tts_core::device::Device;
        use std::os::unix::fs::PermissionsExt;
        let scratch = std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let binary = fixture.path().join("ready-worker.py");
        std::fs::write(&binary, r#"#!/usr/bin/python3
import json, sys
for line in sys.stdin:
    if json.loads(line)['type'] == 'load':
        print(json.dumps({'id': 0, 'status': 'ready', 'device': 'cpu', 'placement': {'main': 'cpu'}, 'samples': 0}), flush=True)
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut backend = WorkerBackend::default();
        backend
            .start(&LoadSpec {
                binary: &binary,
                root: fixture.path(),
                entry: handy_tts_core::catalog::entry("pocket-en").unwrap(),
                voice: "alba",
                device: Device::Cpu,
                cache: fixture.path(),
                home: fixture.path(),
                lanes: 1,
            })
            .unwrap();
        let control = backend.control();
        assert!(Arc::ptr_eq(&control.child, &backend.control.child));
        let mut child = control.child.lock().unwrap().take().unwrap();
        let id = child.id();
        let copy = control.clone();
        let killer = std::thread::spawn(move || {
            for _ in 0..50 {
                copy.kill();
            }
        });
        killer.join().unwrap();
        backend
            .send(&Request::Load {
                model_dir: fixture.path().into(),
                voice: "alba".into(),
                threads: 4,
                lanes: 1,
                device: Device::Cpu,
                cache_dir: fixture.path().into(),
            })
            .unwrap();
        assert_eq!(
            backend
                .receive(Duration::from_secs(2))
                .unwrap()
                .header
                .status,
            Status::Ready
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "a detached owned child cannot be killed through a cached PID"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(control.child.lock().unwrap().is_none());
        control.kill();
        assert!(!Path::new(&format!("/proc/{id}")).exists());
        backend.stop();
    }
    #[test]
    fn worker_memory_header_returns_memory_error_and_reaps_child() {
        use handy_tts_core::device::Device;
        use std::os::unix::fs::PermissionsExt;
        let fixture =
            tempfile::tempdir_in(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap()).unwrap();
        let binary = fixture.path().join("memory-worker.py");
        std::fs::write(
            &binary,
            r#"#!/usr/bin/python3
import json, sys
for line in sys.stdin:
    print(json.dumps({'id': 0, 'status': 'error', 'message': 'not_enough_memory'}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut backend = WorkerBackend::default();
        let result = backend.start(&LoadSpec {
            binary: &binary,
            root: fixture.path(),
            entry: handy_tts_core::catalog::entry("pocket-en").unwrap(),
            voice: "alba",
            device: Device::Cpu,
            cache: fixture.path(),
            home: fixture.path(),
            lanes: 1,
        });
        assert!(matches!(result, Err(TtsError::Memory)));
        assert!(!backend.alive());
        assert!(backend.control.child.lock().unwrap().is_none());
    }
}
