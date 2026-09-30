//! `LlamaServerBackend`: runs llama.cpp's `llama-server` as a child process on
//! a loopback port with a per-spawn API key.

use super::backend::{BackendOpts, GenerateRequest, KillSwitch, TextModelBackend};
use super::registry::ModelEntry;
use super::LocalLlmError;
use log::{debug, info, warn};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Dev-only override for the binary path.
pub const BINARY_ENV: &str = "HANDY_LLAMA_SERVER";
/// Where the RPM installs the static CPU build.
pub const INSTALLED_BINARY: &str = "/usr/lib/Handy/llm/llama-server";

const HEALTH_POLL: Duration = Duration::from_millis(100);
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const STOP_GRACE: Duration = Duration::from_secs(2);
/// Retry an early child exit once with a new port.
const SPAWN_ATTEMPTS: u32 = 2;

/// `HANDY_LLAMA_SERVER` if set and non-empty, else the installed binary.
pub fn resolve_binary(
    env_value: Option<OsString>,
    installed: &Path,
) -> Result<PathBuf, LocalLlmError> {
    let candidate = match env_value.filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => installed.to_path_buf(),
    };
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(LocalLlmError::BinaryMissing(
            candidate.display().to_string(),
        ))
    }
}

/// Command-line arguments for one spawn.
pub fn server_args(
    model_path: &Path,
    port: u16,
    api_key: &str,
    threads: u8,
    ctx: u32,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-m".into(), model_path.as_os_str().to_owned()];
    let rest = [
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--api-key".to_string(),
        api_key.to_string(),
        "-t".to_string(),
        threads.max(1).to_string(),
        "-c".to_string(),
        ctx.to_string(),
        "-np".to_string(),
        "1".to_string(),
        "--temp".to_string(),
        "0".to_string(),
        "--jinja".to_string(),
        "--chat-template-kwargs".to_string(),
        r#"{"enable_thinking":false}"#.to_string(),
        "--no-webui".to_string(),
    ];
    args.extend(rest.into_iter().map(OsString::from));
    args
}

/// A currently free loopback port; `ensure_loaded` retries an early exit once.
pub fn pick_port() -> io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// 32 hex chars from the OS RNG.
pub fn random_api_key() -> Result<String, LocalLlmError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| LocalLlmError::StartFailed(format!("cannot read /dev/urandom: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct SpawnJob {
    cmd: Command,
    reply: mpsc::Sender<io::Result<Child>>,
}

static SPAWNER: OnceLock<Mutex<mpsc::Sender<SpawnJob>>> = OnceLock::new();

/// Spawns from a persistent thread; Linux parent-death signals track the spawning thread.
pub fn spawn_from_supervisor(cmd: Command) -> io::Result<Child> {
    let sender = SPAWNER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<SpawnJob>();
        let started = std::thread::Builder::new()
            .name("local-llm-spawner".into())
            .spawn(move || {
                for mut job in rx {
                    let _ = job.reply.send(job.cmd.spawn());
                }
            });
        if let Err(e) = started {
            warn!("local-llm: cannot start the spawner thread: {e}");
        }
        Mutex::new(tx)
    });
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .lock()
        .map_err(|_| io::Error::other("spawner lock poisoned"))?
        .send(SpawnJob {
            cmd,
            reply: reply_tx,
        })
        .map_err(|_| io::Error::other("spawner thread is gone"))?;
    reply_rx
        .recv()
        .map_err(|_| io::Error::other("spawner thread is gone"))?
}

/// SIGKILL the child if Handy dies without running its exit hook.
#[cfg(target_os = "linux")]
fn set_parent_death_signal(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: before exec, this calls only prctl/getppid and constructs non-allocating OS errors.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// Sends the child's output to Handy's log at debug level on reader threads.
fn forward_output(child: &mut Child) {
    fn pump<R: Read + Send + 'static>(stream: R, name: &str) {
        let spawned = std::thread::Builder::new()
            .name(format!("local-llm-{name}"))
            .spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    debug!("llama-server: {line}");
                }
            });
        if let Err(e) = spawned {
            warn!("local-llm: cannot start the output reader: {e}");
        }
    }
    if let Some(out) = child.stdout.take() {
        pump(out, "stdout");
    }
    if let Some(err) = child.stderr.take() {
        pump(err, "stderr");
    }
}

/// SIGTERM, up to 2 s grace, then SIGKILL; always reaps.
pub fn stop_child(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: this unreaped child owns its PID, preventing PID reuse before kill(2).
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + STOP_GRACE;
        for _ in 0..(STOP_GRACE.as_millis() / 50) {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Why a readiness wait ended without a healthy server.
#[derive(Debug, PartialEq, Eq)]
pub enum HealthError {
    Exited(String),
    Timeout,
    Cancelled,
}

/// Polls `GET /health` until 200, the child exits, the deadline passes or `cancel` is set.
pub fn wait_healthy(
    client: &reqwest::blocking::Client,
    port: u16,
    child: &mut Child,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), HealthError> {
    wait_healthy_inner(client, port, child, deadline, cancel, None)
}

fn wait_healthy_inner(
    client: &reqwest::blocking::Client,
    port: u16,
    child: &mut Child,
    deadline: Instant,
    cancel: &AtomicBool,
    kill: Option<&ChildKillSwitch>,
) -> Result<(), HealthError> {
    let url = format!("http://127.0.0.1:{port}/health");
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(HealthError::Cancelled);
        }
        let exited = match kill {
            Some(kill) => kill.try_wait(child),
            None => child.try_wait(),
        };
        match exited {
            Ok(Some(status)) => return Err(HealthError::Exited(status.to_string())),
            Ok(None) => {}
            Err(e) => return Err(HealthError::Exited(e.to_string())),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(HealthError::Timeout);
        }
        let per_try = (deadline - now).min(HEALTH_REQUEST_TIMEOUT);
        if let Ok(resp) = client.get(&url).timeout(per_try).send() {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        std::thread::sleep(HEALTH_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// `POST /v1/chat/completions`, returning `choices[0].message.content`.
pub fn post_chat(
    client: &reqwest::blocking::Client,
    port: u16,
    api_key: &str,
    req: &GenerateRequest,
) -> Result<String, LocalLlmError> {
    let body = serde_json::json!({
        "messages": req.messages,
        "temperature": 0,
        "stream": false,
        "max_tokens": req.max_tokens,
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(api_key)
        .timeout(req.timeout)
        .json(&body)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                LocalLlmError::RequestTimeout
            } else {
                LocalLlmError::Transport(e.to_string())
            }
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(LocalLlmError::Http(status.as_u16()));
    }
    let value: serde_json::Value = resp.json().map_err(|e| {
        if e.is_timeout() {
            LocalLlmError::RequestTimeout
        } else {
            LocalLlmError::BadOutput(format!("invalid JSON: {e}"))
        }
    })?;
    value
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            LocalLlmError::BadOutput("response has no choices[0].message.content".into())
        })
}

/// A client that never routes loopback traffic (or the API key) through a user proxy.
pub fn loopback_client() -> Result<reqwest::blocking::Client, LocalLlmError> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| LocalLlmError::StartFailed(format!("http client: {e}")))
}

/// Serializes signalling with PID publication and reaping.
struct ChildKillSwitch {
    pid: Arc<AtomicI32>,
    gate: Mutex<()>,
    cancel: Mutex<Arc<AtomicBool>>,
    triggered: AtomicBool,
}

impl ChildKillSwitch {
    fn new() -> Self {
        Self {
            pid: Arc::new(AtomicI32::new(0)),
            gate: Mutex::new(()),
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            triggered: AtomicBool::new(false),
        }
    }

    fn use_cancel(&self, cancel: &Arc<AtomicBool>) {
        *lock(&self.cancel) = Arc::clone(cancel);
        if self.triggered.load(Ordering::SeqCst) {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    fn publish(&self, child: &Child) {
        let _guard = lock(&self.gate);
        self.pid.store(child.id() as i32, Ordering::SeqCst);
        if self.triggered.load(Ordering::SeqCst) {
            self.signal_locked();
        }
    }

    fn signal_locked(&self) {
        #[cfg(target_os = "linux")]
        {
            let pid = self.pid.load(Ordering::SeqCst);
            if pid != 0 {
                // SAFETY: the gate prevents the owner from reaping this PID during signalling.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    fn try_wait(&self, child: &mut Child) -> io::Result<Option<std::process::ExitStatus>> {
        let _guard = lock(&self.gate);
        self.pid.store(0, Ordering::SeqCst);
        let result = child.try_wait();
        if matches!(result, Ok(None)) {
            self.pid.store(child.id() as i32, Ordering::SeqCst);
        }
        result
    }

    fn stop(&self, child: &mut Child) {
        {
            let _guard = lock(&self.gate);
            self.pid.store(0, Ordering::SeqCst);
        }
        stop_child(child);
    }
}

impl KillSwitch for ChildKillSwitch {
    fn trigger(&self) {
        self.triggered.store(true, Ordering::SeqCst);
        lock(&self.cancel).store(true, Ordering::SeqCst);
        let _guard = lock(&self.gate);
        self.signal_locked();
    }
}

/// Recovers mutex data if a thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Running {
    child: Child,
    port: u16,
    api_key: String,
    model_path: PathBuf,
    client: reqwest::blocking::Client,
}

enum StartOutcome {
    Ready(Running),
    ExitedEarly(String),
    Error(LocalLlmError),
}

/// The llama-server runtime.
pub struct LlamaServerBackend {
    binary_env: Option<OsString>,
    installed: PathBuf,
    running: Option<Running>,
    kill: Arc<ChildKillSwitch>,
}

impl Default for LlamaServerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LlamaServerBackend {
    /// Uses `HANDY_LLAMA_SERVER` (read once, here) or the installed binary.
    pub fn new() -> Self {
        Self {
            binary_env: std::env::var_os(BINARY_ENV),
            installed: PathBuf::from(INSTALLED_BINARY),
            running: None,
            kill: Arc::new(ChildKillSwitch::new()),
        }
    }

    /// Always uses `binary`, whatever `HANDY_LLAMA_SERVER` says.
    #[cfg(test)]
    pub fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary_env: None,
            installed: binary,
            running: None,
            kill: Arc::new(ChildKillSwitch::new()),
        }
    }

    fn spawn_child(&self, cmd: Command) -> io::Result<Child> {
        let child = spawn_from_supervisor(cmd)?;
        self.kill.publish(&child);
        Ok(child)
    }

    fn start_once(&self, binary: &Path, opts: &BackendOpts, deadline: Instant) -> StartOutcome {
        let port = match pick_port() {
            Ok(p) => p,
            Err(e) => {
                return StartOutcome::Error(LocalLlmError::StartFailed(format!(
                    "no free port: {e}"
                )))
            }
        };
        let api_key = match random_api_key() {
            Ok(k) => k,
            Err(e) => return StartOutcome::Error(e),
        };
        let client = match loopback_client() {
            Ok(c) => c,
            Err(e) => return StartOutcome::Error(e),
        };
        let mut cmd = Command::new(binary);
        cmd.args(server_args(
            &opts.model_path,
            port,
            &api_key,
            opts.threads,
            opts.ctx,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        #[cfg(target_os = "linux")]
        set_parent_death_signal(&mut cmd);
        let mut child = match self.spawn_child(cmd) {
            Ok(c) => c,
            Err(e) => {
                return StartOutcome::Error(LocalLlmError::StartFailed(format!(
                    "spawn {}: {e}",
                    binary.display()
                )))
            }
        };
        forward_output(&mut child);
        match wait_healthy_inner(
            &client,
            port,
            &mut child,
            deadline,
            &opts.cancel,
            Some(&self.kill),
        ) {
            Ok(()) => StartOutcome::Ready(Running {
                child,
                port,
                api_key,
                model_path: opts.model_path.clone(),
                client,
            }),
            Err(HealthError::Exited(status)) => {
                let _ = child.wait();
                StartOutcome::ExitedEarly(format!("llama-server exited during start ({status})"))
            }
            Err(HealthError::Timeout) => {
                self.kill.stop(&mut child);
                StartOutcome::Error(LocalLlmError::StartTimeout)
            }
            Err(HealthError::Cancelled) => {
                self.kill.stop(&mut child);
                StartOutcome::Error(LocalLlmError::StartFailed(
                    "start cancelled: shutting down".into(),
                ))
            }
        }
    }
}

impl TextModelBackend for LlamaServerBackend {
    fn kill_switch(&self) -> Arc<dyn KillSwitch> {
        self.kill.clone()
    }

    fn ensure_loaded(
        &mut self,
        model: &ModelEntry,
        opts: &BackendOpts,
    ) -> Result<(), LocalLlmError> {
        self.kill.use_cancel(&opts.cancel);
        if opts.cancel.load(Ordering::SeqCst) {
            return Err(LocalLlmError::StartFailed(
                "start cancelled: shutting down".into(),
            ));
        }
        if let Some(running) = self.running.as_mut() {
            if running.model_path == opts.model_path
                && matches!(self.kill.try_wait(&mut running.child), Ok(None))
            {
                return Ok(());
            }
        }
        self.unload();
        let binary = resolve_binary(self.binary_env.clone(), &self.installed)?;
        let deadline = Instant::now() + opts.start_timeout;
        let mut last = LocalLlmError::StartTimeout;
        for attempt in 1..=SPAWN_ATTEMPTS {
            match self.start_once(&binary, opts, deadline) {
                StartOutcome::Ready(running) => {
                    info!(
                        "local-llm: llama-server ready for {} on port {}",
                        model.id, running.port
                    );
                    self.running = Some(running);
                    return Ok(());
                }
                StartOutcome::ExitedEarly(msg) => {
                    warn!("local-llm: {msg} (attempt {attempt}/{SPAWN_ATTEMPTS})");
                    last = LocalLlmError::StartFailed(msg);
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                StartOutcome::Error(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn generate(&mut self, req: &GenerateRequest) -> Result<String, LocalLlmError> {
        let running = self
            .running
            .as_ref()
            .ok_or_else(|| LocalLlmError::Failed("llama-server is not running".into()))?;
        post_chat(&running.client, running.port, &running.api_key, req)
    }

    fn unload(&mut self) {
        if let Some(mut running) = self.running.take() {
            self.kill.stop(&mut running.child);
            info!("local-llm: llama-server stopped");
        }
    }

    fn is_alive(&mut self) -> bool {
        let alive = match self.running.as_mut() {
            Some(running) => matches!(self.kill.try_wait(&mut running.child), Ok(None)),
            None => false,
        };
        if !alive {
            if let Some(mut running) = self.running.take() {
                let _ = running.child.wait();
            }
        }
        alive
    }
}

impl Drop for LlamaServerBackend {
    fn drop(&mut self) {
        self.unload();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_llm::prompt::ChatMessage;
    use crate::local_llm::registry;
    use std::io::Write;
    use std::sync::Arc;

    /// Serves scripted responses and returns the raw request headers and bodies.
    fn serve(responses: Vec<(u16, String)>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in responses {
                let (mut sock, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut head = String::new();
                let mut content_length = 0usize;
                for _ in 0..100 {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut req_body = vec![0u8; content_length];
                reader.read_exact(&mut req_body).unwrap();
                seen.push(format!("{head}\r\n{}", String::from_utf8_lossy(&req_body)));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });
        (port, handle)
    }

    fn sleeper() -> Child {
        Command::new("sleep").arg("30").spawn().unwrap()
    }

    fn opts(model_path: PathBuf, start_timeout: Duration) -> BackendOpts {
        BackendOpts {
            model_path,
            threads: 4,
            ctx: 4096,
            start_timeout,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fake_binary(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-llama-server");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kill_switch_kills_a_running_child_and_owner_reaps_it() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path(), "exec sleep 30");
        let mut backend = LlamaServerBackend::with_binary(bin.clone());
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = backend.spawn_child(cmd).unwrap();
        let pid = child.id() as i32;
        let published_pid = backend.kill.pid.load(Ordering::SeqCst);
        backend.running = Some(Running {
            child,
            port: 0,
            api_key: String::new(),
            model_path: dir.path().join("m.gguf"),
            client: loopback_client().unwrap(),
        });
        let switch = backend.kill_switch();
        switch.trigger();
        for _ in 0..100 {
            if !backend.is_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let killed = !backend.is_alive();
        backend.unload();
        assert_eq!(
            published_pid, pid,
            "PID must be published before the kill switch triggers"
        );
        assert!(killed, "SIGKILL must stop the child before normal unload");
        assert_eq!(backend.kill.pid.load(Ordering::SeqCst), 0);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child must be reaped"
        );
    }

    #[test]
    fn env_override_wins_and_missing_binary_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed");
        let custom = dir.path().join("custom");
        std::fs::write(&custom, b"").unwrap();
        assert_eq!(
            resolve_binary(Some(custom.clone().into_os_string()), &installed).unwrap(),
            custom
        );
        assert_eq!(
            resolve_binary(Some(OsString::new()), &installed),
            Err(LocalLlmError::BinaryMissing(
                installed.display().to_string()
            ))
        );
        assert!(matches!(
            resolve_binary(None, &installed),
            Err(LocalLlmError::BinaryMissing(_))
        ));
    }

    #[test]
    fn server_args_match_the_card_contract() {
        let args = server_args(Path::new("/m/s1.gguf"), 4242, "k3y", 4, 4096);
        let args: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(
            args.join(" "),
            "-m /m/s1.gguf --host 127.0.0.1 --port 4242 --api-key k3y -t 4 -c 4096 -np 1 --temp 0 --jinja --chat-template-kwargs {\"enable_thinking\":false} --no-webui"
        );
    }

    #[test]
    fn server_args_clamp_zero_threads_to_one() {
        let args = server_args(Path::new("/m/s1.gguf"), 4242, "k3y", 0, 4096);
        let threads = args.iter().position(|arg| arg == "-t").unwrap();
        assert_eq!(args[threads + 1], OsString::from("1"));
    }

    #[test]
    fn api_keys_are_random_hex() {
        let a = random_api_key().unwrap();
        let b = random_api_key().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn health_wait_succeeds_after_a_503() {
        let (port, server) = serve(vec![(503, "{}".into()), (200, "{}".into())]);
        let mut child = sleeper();
        let cancel = AtomicBool::new(false);
        let client = loopback_client().unwrap();
        let r = wait_healthy(
            &client,
            port,
            &mut child,
            Instant::now() + Duration::from_secs(5),
            &cancel,
        );
        stop_child(&mut child);
        assert_eq!(r, Ok(()));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn health_wait_reports_an_exited_child() {
        let mut child = Command::new("true").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let cancel = AtomicBool::new(false);
        let client = loopback_client().unwrap();
        let r = wait_healthy(
            &client,
            pick_port().unwrap(),
            &mut child,
            Instant::now() + Duration::from_secs(5),
            &cancel,
        );
        assert!(matches!(r, Err(HealthError::Exited(_))));
    }

    #[test]
    fn health_wait_times_out_and_honours_cancel() {
        let client = loopback_client().unwrap();
        let mut child = sleeper();
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        let r = wait_healthy(
            &client,
            pick_port().unwrap(),
            &mut child,
            t0 + Duration::from_millis(300),
            &cancel,
        );
        assert_eq!(r, Err(HealthError::Timeout));
        assert!(t0.elapsed() < Duration::from_secs(2));
        cancel.store(true, Ordering::SeqCst);
        let r = wait_healthy(
            &client,
            pick_port().unwrap(),
            &mut child,
            Instant::now() + Duration::from_secs(30),
            &cancel,
        );
        assert_eq!(r, Err(HealthError::Cancelled));
        stop_child(&mut child);
    }

    #[test]
    fn post_chat_sends_the_contract_and_reads_content() {
        let reply = r#"{"choices":[{"message":{"role":"assistant","content":"Hello there."}}]}"#;
        let (port, server) = serve(vec![(200, reply.into())]);
        let req = GenerateRequest {
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: "sys".into(),
                },
                ChatMessage {
                    role: "user",
                    content: "ctl\nhello there".into(),
                },
            ],
            max_tokens: 71,
            timeout: Duration::from_secs(5),
        };
        let out = post_chat(&loopback_client().unwrap(), port, "k3y", &req).unwrap();
        assert_eq!(out, "Hello there.");
        let seen = server.join().unwrap();
        let raw = &seen[0];
        assert!(raw.starts_with("POST /v1/chat/completions "));
        assert!(raw
            .to_ascii_lowercase()
            .contains("authorization: bearer k3y"));
        let body: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 71);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "ctl\nhello there");
    }

    #[test]
    fn post_chat_maps_http_errors_and_missing_content() {
        let (port, server) = serve(vec![(401, "{}".into()), (200, "{}".into())]);
        let req = GenerateRequest {
            messages: vec![],
            max_tokens: 8,
            timeout: Duration::from_secs(5),
        };
        let client = loopback_client().unwrap();
        assert_eq!(
            post_chat(&client, port, "k", &req),
            Err(LocalLlmError::Http(401))
        );
        assert!(matches!(
            post_chat(&client, port, "k", &req),
            Err(LocalLlmError::BadOutput(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn stop_child_terminates_and_reaps() {
        let mut child = sleeper();
        let t0 = Instant::now();
        stop_child(&mut child);
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn child_outlives_the_thread_that_requested_the_spawn() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        #[cfg(target_os = "linux")]
        set_parent_death_signal(&mut cmd);
        let mut child = std::thread::spawn(move || spawn_from_supervisor(cmd).unwrap())
            .join()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let still_running = matches!(child.try_wait(), Ok(None));
        stop_child(&mut child);
        assert!(
            still_running,
            "llama-server must survive the exit of the thread that asked for it"
        );
    }

    #[test]
    fn early_exit_is_retried_once_with_a_new_port() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("spawns");
        let bin = fake_binary(
            dir.path(),
            &format!("echo x >> '{}'\nexit 1", counter.display()),
        );
        let mut backend = LlamaServerBackend::with_binary(bin);
        let entry = registry::find("s1-mini-q4km").unwrap();
        let r = backend.ensure_loaded(
            entry,
            &opts(dir.path().join("m.gguf"), Duration::from_secs(5)),
        );
        assert!(matches!(r, Err(LocalLlmError::StartFailed(_))), "{r:?}");
        let spawns = std::fs::read_to_string(&counter).unwrap();
        assert_eq!(spawns.lines().count(), 2);
        assert!(!backend.is_alive());
    }

    #[test]
    fn start_timeout_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let bin = fake_binary(
            dir.path(),
            &format!("echo $$ > '{}'\nexec sleep 30", pidfile.display()),
        );
        let mut backend = LlamaServerBackend::with_binary(bin);
        let entry = registry::find("s1-mini-q4km").unwrap();
        let t0 = Instant::now();
        let r = backend.ensure_loaded(
            entry,
            &opts(dir.path().join("m.gguf"), Duration::from_millis(500)),
        );
        assert_eq!(r, Err(LocalLlmError::StartTimeout));
        assert!(t0.elapsed() < Duration::from_secs(4));
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        assert!(
            !Path::new(&format!("/proc/{}", pid.trim())).exists(),
            "timed-out child must be gone"
        );
    }

    #[test]
    fn cancel_during_start_stops_the_child_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_binary(dir.path(), "exec sleep 30");
        let mut backend = LlamaServerBackend::with_binary(bin);
        let entry = registry::find("s1-mini-q4km").unwrap();
        let o = opts(dir.path().join("m.gguf"), Duration::from_secs(30));
        let cancel = o.cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::SeqCst);
        });
        let t0 = Instant::now();
        let r = backend.ensure_loaded(entry, &o);
        canceller.join().unwrap();
        assert!(matches!(r, Err(LocalLlmError::StartFailed(_))), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(3));
    }
}
