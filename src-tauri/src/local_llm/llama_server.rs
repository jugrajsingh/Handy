//! `LlamaServerBackend`: runs llama.cpp's `llama-server` as a child process on
//! a private Unix socket with a per-spawn API key.

use super::backend::{BackendOpts, GenerateRequest, KillSwitch, TextModelBackend};
use super::registry::ModelEntry;
use super::LocalLlmError;
#[cfg(target_os = "linux")]
use crate::process_supervisor::set_parent_death_signal;
pub use crate::process_supervisor::{spawn_from_supervisor, stop_child};
use log::{info, warn};
use std::ffi::OsString;
use std::io::{self, Read};
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// Dev-only override for the binary path.
pub const BINARY_ENV: &str = "HANDY_LLAMA_SERVER";
/// Where the RPM installs the static CPU build.
pub const INSTALLED_BINARY: &str = "/usr/lib/Handy/llm/llama-server";

const LOCAL_AUTHORITY: &str = "localhost";
const HEALTH_POLL: Duration = Duration::from_millis(100);
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
/// Retry an early child exit once with a new socket.
const SPAWN_ATTEMPTS: u32 = 2;

fn binary_file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

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
        Err(LocalLlmError::BinaryMissing(binary_file_name(&candidate)))
    }
}

/// Command-line arguments for one spawn.
pub fn server_args(model_path: &Path, socket: &Path, threads: u8, ctx: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-m".into(),
        model_path.as_os_str().to_owned(),
        "--host".into(),
        socket.as_os_str().to_owned(),
    ];
    let rest = [
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

fn build_command(binary: &Path, opts: &BackendOpts, socket: &Path, api_key: &str) -> Command {
    let mut cmd = Command::new(binary);
    cmd.args(server_args(
        &opts.model_path,
        socket,
        opts.threads,
        opts.ctx,
    ))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    cmd.env("LLAMA_API_KEY", api_key);
    #[cfg(target_os = "linux")]
    set_parent_death_signal(&mut cmd);
    cmd
}

struct PrivateSocket {
    path: PathBuf,
}

#[cfg(unix)]
fn validate_private_dir(path: &Path, uid: u32) -> Result<(), LocalLlmError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| LocalLlmError::StartFailed("private socket directory unavailable".into()))?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(LocalLlmError::StartFailed(
            "socket directory must be owned by this user with mode 0700".into(),
        ));
    }
    Ok(())
}

impl PrivateSocket {
    #[cfg(unix)]
    fn create(root: &Path) -> Result<Self, LocalLlmError> {
        use std::os::unix::{ffi::OsStrExt, fs::DirBuilderExt};
        // SAFETY: geteuid has no preconditions.
        validate_private_dir(root, unsafe { libc::geteuid() })?;
        let directory = root.join(format!("hllm-{}", &random_api_key()?[..12]));
        let path = directory.join("s.sock");
        // SAFETY: an all-zero sockaddr_un is a valid value for inspecting sun_path.
        let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if path.as_os_str().as_bytes().len() >= address.sun_path.len() {
            return Err(LocalLlmError::StartFailed(
                "private socket path exceeds the Unix socket length limit".into(),
            ));
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| {
                LocalLlmError::StartFailed("cannot create private socket directory".into())
            })?;
        let socket = Self { path };
        // SAFETY: geteuid has no preconditions.
        validate_private_dir(&directory, unsafe { libc::geteuid() })?;
        Ok(socket)
    }

    #[cfg(not(unix))]
    fn create(_: &Path) -> Result<Self, LocalLlmError> {
        Err(LocalLlmError::StartFailed(
            "local LLM requires Unix sockets on this platform".into(),
        ))
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(directory) = self.path.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

impl Drop for PrivateSocket {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// 32 hex chars from the OS RNG.
pub fn random_api_key() -> Result<String, LocalLlmError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| LocalLlmError::StartFailed(format!("cannot read /dev/urandom: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Why a readiness wait ended without a healthy server.
#[derive(Debug, PartialEq, Eq)]
pub enum HealthError {
    Exited(String),
    Timeout,
    Cancelled,
}

/// Polls `GET /health` until 200, the child exits, the deadline passes or `cancel` is set.
#[cfg(test)]
pub fn wait_healthy(
    client: &reqwest::blocking::Client,
    child: &mut Child,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), HealthError> {
    wait_healthy_inner(client, LOCAL_AUTHORITY, child, deadline, cancel, None)
}

fn wait_healthy_inner(
    client: &reqwest::blocking::Client,
    authority: &str,
    child: &mut Child,
    deadline: Instant,
    cancel: &AtomicBool,
    kill: Option<&ChildKillSwitch>,
) -> Result<(), HealthError> {
    let url = format!("http://{authority}/health");
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
                let alive = match kill {
                    Some(kill) => kill.try_wait(child),
                    None => child.try_wait(),
                };
                return match alive {
                    Ok(None) if !cancel.load(Ordering::SeqCst) => Ok(()),
                    Ok(None) => Err(HealthError::Cancelled),
                    Ok(Some(status)) => Err(HealthError::Exited(status.to_string())),
                    Err(error) => Err(HealthError::Exited(error.to_string())),
                };
            }
        }
        std::thread::sleep(HEALTH_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// `POST /v1/chat/completions`, returning `choices[0].message.content`.
pub fn post_chat(
    client: &reqwest::blocking::Client,
    authority: &str,
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
        .post(format!("http://{authority}/v1/chat/completions"))
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
    if value
        .pointer("/choices/0/finish_reason")
        .and_then(|reason| reason.as_str())
        == Some("length")
    {
        return Err(LocalLlmError::BadOutput(
            "response reached the token limit".into(),
        ));
    }
    value
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            LocalLlmError::BadOutput("response has no choices[0].message.content".into())
        })
}

/// A local client restricted to one socket, without proxies or redirects.
pub fn socket_client(socket: &Path) -> Result<reqwest::blocking::Client, LocalLlmError> {
    #[cfg(unix)]
    {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .unix_socket(socket.to_path_buf())
            .build()
            .map_err(|_| LocalLlmError::StartFailed("cannot create private socket client".into()))
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
        Err(LocalLlmError::StartFailed(
            "local LLM requires Unix sockets on this platform".into(),
        ))
    }
}

/// Serializes signalling with PID publication and reaping.
struct ChildKillSwitch {
    pid: Arc<AtomicI32>,
    gate: Mutex<()>,
    cancel: Mutex<Arc<AtomicBool>>,
    triggered: AtomicBool,
    socket: Mutex<Weak<PrivateSocket>>,
}

impl ChildKillSwitch {
    fn new() -> Self {
        Self {
            pid: Arc::new(AtomicI32::new(0)),
            gate: Mutex::new(()),
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            triggered: AtomicBool::new(false),
            socket: Mutex::new(Weak::new()),
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

    /// Also publishes the live child PID while polling for exit.
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
        if let Some(socket) = lock(&self.socket).upgrade() {
            socket.cleanup();
        }
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
    http_authority: String,
    _socket: Arc<PrivateSocket>,
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
    http_authority: String,
    installed: PathBuf,
    data_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
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
            http_authority: LOCAL_AUTHORITY.into(),
            installed: PathBuf::from(INSTALLED_BINARY),
            data_dir: None,
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            running: None,
            kill: Arc::new(ChildKillSwitch::new()),
        }
    }

    /// Always uses `binary`, whatever `HANDY_LLAMA_SERVER` says.
    #[cfg(test)]
    pub fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary_env: None,
            http_authority: LOCAL_AUTHORITY.into(),
            data_dir: binary.parent().map(Path::to_path_buf),
            runtime_dir: None,
            installed: binary,
            running: None,
            kill: Arc::new(ChildKillSwitch::new()),
        }
    }

    /// Uses the app data directory when XDG_RUNTIME_DIR is absent.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        let mut backend = Self::new();
        backend.data_dir = data_dir;
        backend
    }

    fn socket_root(&self) -> Result<PathBuf, LocalLlmError> {
        if let Some(root) = &self.runtime_dir {
            return Ok(root.clone());
        }
        let data = self.data_dir.as_ref().ok_or_else(|| {
            LocalLlmError::StartFailed("private app data directory unavailable".into())
        })?;
        let root = data.join("llm-runtime");
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            match std::fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => {
                    return Err(LocalLlmError::StartFailed(
                        "cannot create private runtime directory".into(),
                    ))
                }
            }
        }
        Ok(root)
    }

    fn spawn_child(&self, cmd: Command) -> io::Result<Child> {
        let child = spawn_from_supervisor(cmd)?;
        self.kill.publish(&child);
        Ok(child)
    }

    fn start_once(&self, binary: &Path, opts: &BackendOpts, deadline: Instant) -> StartOutcome {
        let socket = match self
            .socket_root()
            .and_then(|root| PrivateSocket::create(&root))
        {
            Ok(socket) => Arc::new(socket),
            Err(error) => return StartOutcome::Error(error),
        };
        *lock(&self.kill.socket) = Arc::downgrade(&socket);
        let api_key = match random_api_key() {
            Ok(k) => k,
            Err(e) => return StartOutcome::Error(e),
        };
        let client = match socket_client(&socket.path) {
            Ok(c) => c,
            Err(e) => return StartOutcome::Error(e),
        };
        let cmd = build_command(binary, opts, &socket.path, &api_key);
        let mut child = match self.spawn_child(cmd) {
            Ok(c) => c,
            Err(e) => {
                return StartOutcome::Error(LocalLlmError::StartFailed(format!(
                    "spawn {}: {e}",
                    binary_file_name(binary)
                )))
            }
        };
        match wait_healthy_inner(
            &client,
            &self.http_authority,
            &mut child,
            deadline,
            &opts.cancel,
            Some(&self.kill),
        ) {
            Ok(()) => StartOutcome::Ready(Running {
                child,
                http_authority: self.http_authority.clone(),
                _socket: socket,
                api_key,
                model_path: opts.model_path.clone(),
                client,
            }),
            Err(HealthError::Exited(status)) => {
                self.kill.stop(&mut child);
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
                        "local-llm: llama-server ready for {} on private socket",
                        model.id
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
            .as_mut()
            .ok_or_else(|| LocalLlmError::Failed("llama-server is not running".into()))?;
        if !matches!(self.kill.try_wait(&mut running.child), Ok(None)) {
            return Err(LocalLlmError::Transport(
                "llama-server exited before request".into(),
            ));
        }
        post_chat(
            &running.client,
            &running.http_authority,
            &running.api_key,
            req,
        )
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::local_llm::prompt::ChatMessage;
    use crate::local_llm::registry;
    use std::io::Write;
    use std::sync::Arc;

    /// Serves scripted responses and returns the raw request headers and bodies.
    fn serve(
        responses: Vec<(u16, String)>,
    ) -> (tempfile::TempDir, std::thread::JoinHandle<Vec<String>>) {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.path().join("s.sock")).unwrap();
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
                let location = if status == 302 {
                    "Location: http://localhost/redirected\r\n"
                } else {
                    ""
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });
        (dir, handle)
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
            http_authority: LOCAL_AUTHORITY.into(),
            api_key: String::new(),
            model_path: dir.path().join("m.gguf"),
            client: socket_client(&dir.path().join("s.sock")).unwrap(),
            _socket: Arc::new(PrivateSocket::create(&backend.socket_root().unwrap()).unwrap()),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn ensure_loaded_publishes_the_child_pid_and_the_kill_switch_stops_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let bin = fake_binary(
            dir.path(),
            &format!("echo $$ > '{}'\nexec sleep 30", pidfile.display()),
        );
        let mut backend = LlamaServerBackend::with_binary(bin);
        let kill = Arc::clone(&backend.kill);
        let options = opts(dir.path().join("m.gguf"), Duration::from_secs(10));
        let (tx, rx) = mpsc::channel();
        let start = std::thread::spawn(move || {
            let result = backend.ensure_loaded(registry::find("s1-mini-q4km").unwrap(), &options);
            tx.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut pid = None;
        while Instant::now() < deadline {
            pid = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok());
            if pid.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let published_pid = kill.pid.load(Ordering::SeqCst);
        let stopped = Instant::now();
        kill.trigger();
        let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        start.join().unwrap();
        let pid = pid.expect("fake server must write its PID within 2 s");
        assert_eq!(published_pid, pid, "startup must publish the child PID");
        assert!(matches!(result, Err(LocalLlmError::StartFailed(_))));
        assert!(stopped.elapsed() < Duration::from_secs(2));
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn wbsec_socket_start_generate_kill_and_tcp_isolation() {
        struct ReapedChild(Child);
        impl Drop for ReapedChild {
            fn drop(&mut self) {
                stop_child(&mut self.0);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut attacker = ReapedChild(Command::new("python3").args(["-u", "-c", r#"
import json, select, socket, sys, time
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen(2)
print('127.0.0.1:' + str(s.getsockname()[1]), flush=True)
traffic = False
private_request = False
deadline = time.monotonic() + 15
for _ in range(8):
    remaining = deadline - time.monotonic()
    if remaining <= 0: raise SystemExit(3)
    ready, _, _ = select.select([s, sys.stdin], [], [], remaining)
    if not ready: raise SystemExit(3)
    if sys.stdin in ready:
        if sys.stdin.readline() != 'done\n': raise SystemExit(3)
        print('intercepted' if private_request else 'traffic' if traffic else 'isolated', flush=True)
        raise SystemExit(1 if traffic else 0)
    connection, _ = s.accept()
    connection.settimeout(2)
    traffic = True
    request = connection.makefile('rb')
    first = request.readline().decode()
    headers = {}
    for _ in range(100):
        line = request.readline()
        if line == b'\r\n': break
        key, value = line.decode().split(':', 1)
        headers[key.lower()] = value.strip()
    else: raise SystemExit(3)
    size = int(headers.get('content-length', '0'))
    assert size < 65536
    body = request.read(size)
    if first.startswith('POST /v1/chat/completions'):
        private_request = (headers.get('authorization', '').startswith('Bearer ') and json.loads(body)['messages'][0]['content'] == 'synthetic transcript')
        assert private_request
    reply = b'{"choices":[{"message":{"content":"synthetic result"}}]}'
    connection.sendall(b'HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: ' + str(len(reply)).encode() + b'\r\n\r\n' + reply)
    request.close()
    connection.close()
raise SystemExit(3)
"#]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
        let mut observer = BufReader::new(attacker.0.stdout.take().unwrap());
        let mut authority = String::new();
        observer.read_line(&mut authority).unwrap();
        let authority = authority.trim().to_owned();
        assert!(authority.starts_with("127.0.0.1:"));
        let binary = fake_binary(
            root.path(),
            r#"exec python3 - "$@" <<'PY'
import json, os, socket, sys, time
path = sys.argv[sys.argv.index('--host') + 1]
s = socket.socket(socket.AF_UNIX)
s.bind(path)
s.listen(2)
for expected in ('GET /health', 'POST /v1/chat/completions'):
    connection, _ = s.accept()
    request = connection.makefile('rb')
    assert request.readline().decode().startswith(expected)
    headers = {}
    for _ in range(100):
        line = request.readline()
        if line == b'\r\n': break
        key, value = line.decode().split(':', 1)
        headers[key.lower()] = value.strip()
    else: raise SystemExit(3)
    body = request.read(int(headers.get('content-length', '0')))
    if expected.startswith('POST'):
        assert headers['authorization'] == 'Bearer ' + os.environ['LLAMA_API_KEY']
        assert json.loads(body)['messages'][0]['content'] == 'synthetic transcript'
    reply = b'{"choices":[{"message":{"content":"synthetic result"}}]}'
    connection.sendall(b'HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: ' + str(len(reply)).encode() + b'\r\n\r\n' + reply)
    request.close()
    connection.close()
time.sleep(30)
PY"#,
        );
        let mut backend = LlamaServerBackend::with_binary(binary);
        backend.http_authority = authority;
        let result = backend.ensure_loaded(
            registry::find("s1-mini-q4km").unwrap(),
            &opts(root.path().join("m.gguf"), Duration::from_secs(5)),
        );
        let generated = result.and_then(|()| {
            backend.generate(&GenerateRequest {
                messages: vec![ChatMessage {
                    role: "user",
                    content: "synthetic transcript".into(),
                }],
                max_tokens: 32,
                timeout: Duration::from_secs(2),
            })
        });
        let directory = backend
            .running
            .as_ref()
            .map(|r| r._socket.path.parent().unwrap().to_path_buf());
        backend.kill_switch().trigger();
        let cleaned_before_unload = directory.as_ref().is_none_or(|path| !path.exists());
        backend.unload();
        attacker
            .0
            .stdin
            .take()
            .unwrap()
            .write_all(b"done\n")
            .unwrap();
        let mut observation = String::new();
        observer.read_line(&mut observation).unwrap();
        let status = attacker.0.wait().unwrap();
        assert_eq!(
            observation.trim(),
            "isolated",
            "TCP isolation must keep the synthetic transcript and key off the HTTP authority"
        );
        assert!(status.success());
        assert!(
            cleaned_before_unload,
            "kill switch must clean the socket even before unload"
        );
        assert_eq!(
            generated,
            Ok("synthetic result".into()),
            "generation must use the private socket"
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("llm-runtime"))
                .unwrap()
                .count(),
            0,
            "socket directories must be removed after cleanup"
        );
    }

    #[test]
    fn fix1_private_socket_rejects_symlinks_and_uses_xdg_runtime() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "HANDY_TEST_WBSEC_RUNTIME";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let status = Command::new(std::env::current_exe().unwrap()).args(["--exact", "local_llm::llama_server::tests::fix1_private_socket_rejects_symlinks_and_uses_xdg_runtime", "--nocapture"])
                .env(CHILD, "1").env("XDG_RUNTIME_DIR", directory.path()).status().unwrap();
            assert!(
                status.success(),
                "private runtime selection and symlink refusal must hold in the child"
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        let mut backend = LlamaServerBackend::new();
        backend.data_dir = Some(root.join("app-data"));
        std::fs::create_dir(root.join("app-data")).unwrap();
        assert_eq!(
            backend.socket_root().unwrap(),
            root,
            "XDG_RUNTIME_DIR must select the private runtime root"
        );
        let socket = PrivateSocket::create(&backend.socket_root().unwrap()).unwrap();
        assert_eq!(socket.path.parent().unwrap().parent().unwrap(), root);
        drop(socket);
        let link = root.join("linked-runtime");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(
            PrivateSocket::create(&link).is_err(),
            "a symlink must not be accepted as a private socket directory"
        );
    }

    #[test]
    fn wbsec_health_response_cannot_make_an_exited_child_ready() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut child = sleeper();
        let pid = child.id() as i32;
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = [0; 1024];
            assert!(stream.read(&mut head).unwrap() > 0);
            // SAFETY: this is the live, unreaped child owned by this test.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            std::thread::sleep(Duration::from_millis(50));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let result = wait_healthy(
            &socket_client(&socket).unwrap(),
            &mut child,
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        );
        stop_child(&mut child);
        server.join().unwrap();
        assert!(
            matches!(result, Err(HealthError::Exited(_))),
            "a health response must not hide child exit"
        );
    }

    #[test]
    fn wbsec_generate_rechecks_child_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path(), "exit 0");
        let mut backend = LlamaServerBackend::with_binary(binary);
        let mut child = sleeper();
        stop_child(&mut child);
        let socket = Arc::new(PrivateSocket::create(&backend.socket_root().unwrap()).unwrap());
        backend.running = Some(Running {
            child,
            http_authority: LOCAL_AUTHORITY.into(),
            client: socket_client(&socket.path).unwrap(),
            _socket: socket,
            api_key: "fixture".into(),
            model_path: dir.path().join("m.gguf"),
        });
        assert_eq!(
            backend.generate(&GenerateRequest {
                messages: vec![],
                max_tokens: 1,
                timeout: Duration::from_secs(1)
            }),
            Err(LocalLlmError::Transport(
                "llama-server exited before request".into()
            )),
            "generation must refuse an exited child before sending"
        );
    }

    #[test]
    fn wbsec_redirects_are_refused_on_the_private_client() {
        let (dir, server) = serve(vec![(302, "{}".into())]);
        let result = post_chat(
            &socket_client(&dir.path().join("s.sock")).unwrap(),
            LOCAL_AUTHORITY,
            "fixture",
            &GenerateRequest {
                messages: vec![],
                max_tokens: 1,
                timeout: Duration::from_secs(1),
            },
        );
        assert_eq!(result, Err(LocalLlmError::Http(302)));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn wbsec_private_socket_permissions_ownership_and_cleanup() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PrivateSocket::create(root.path()).is_err());
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = PrivateSocket::create(root.path()).unwrap();
        let directory = socket.path.parent().unwrap().to_path_buf();
        let metadata = std::fs::metadata(&directory).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert!(validate_private_dir(&directory, metadata.uid() + 1).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_private_dir(&directory, metadata.uid()).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket.path).unwrap();
        drop(socket);
        assert!(!directory.exists());
        drop(listener);
        let long = root.path().join("x".repeat(108));
        std::fs::create_dir(&long).unwrap();
        std::fs::set_permissions(&long, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(PrivateSocket::create(&long).is_err());
        assert_eq!(std::fs::read_dir(&long).unwrap().count(), 0);
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
            Err(LocalLlmError::BinaryMissing("installed".into()))
        );
        assert!(matches!(
            resolve_binary(None, &installed),
            Err(LocalLlmError::BinaryMissing(_))
        ));
    }

    #[test]
    fn server_args_match_the_card_contract() {
        let args = server_args(
            Path::new("/m/s1.gguf"),
            Path::new("/private/llm.sock"),
            4,
            4096,
        );
        let args: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(
            args.join(" "),
            "-m /m/s1.gguf --host /private/llm.sock -t 4 -c 4096 -np 1 --temp 0 --jinja --chat-template-kwargs {\"enable_thinking\":false} --no-webui"
        );
    }

    #[test]
    fn api_key_travels_in_the_environment_not_argv() {
        let options = opts(PathBuf::from("/m/s1.gguf"), Duration::from_secs(10));
        let key = "test-only-key";
        let cmd = build_command(
            Path::new("/llm/llama-server"),
            &options,
            Path::new("/private/llm.sock"),
            key,
        );
        assert!(!cmd.get_args().any(|arg| arg == "--api-key" || arg == key));
        assert!(cmd.get_envs().any(|(name, value)| {
            name == "LLAMA_API_KEY" && value == Some(std::ffi::OsStr::new(key))
        }));
    }

    #[test]
    fn server_args_clamp_zero_threads_to_one() {
        let args = server_args(
            Path::new("/m/s1.gguf"),
            Path::new("/private/llm.sock"),
            0,
            4096,
        );
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
        let (dir, server) = serve(vec![(503, "{}".into()), (200, "{}".into())]);
        let mut child = sleeper();
        let cancel = AtomicBool::new(false);
        let client = socket_client(&dir.path().join("s.sock")).unwrap();
        let r = wait_healthy(
            &client,
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
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("true").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let cancel = AtomicBool::new(false);
        let client = socket_client(&dir.path().join("s.sock")).unwrap();
        let r = wait_healthy(
            &client,
            &mut child,
            Instant::now() + Duration::from_secs(5),
            &cancel,
        );
        assert!(matches!(r, Err(HealthError::Exited(_))));
    }

    #[test]
    fn health_wait_times_out_and_honours_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let client = socket_client(&dir.path().join("s.sock")).unwrap();
        let mut child = sleeper();
        let cancel = AtomicBool::new(false);
        let t0 = Instant::now();
        let r = wait_healthy(
            &client,
            &mut child,
            t0 + Duration::from_millis(300),
            &cancel,
        );
        assert_eq!(r, Err(HealthError::Timeout));
        assert!(t0.elapsed() < Duration::from_secs(2));
        cancel.store(true, Ordering::SeqCst);
        let r = wait_healthy(
            &client,
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
        let (dir, server) = serve(vec![(200, reply.into())]);
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
        let out = post_chat(
            &socket_client(&dir.path().join("s.sock")).unwrap(),
            LOCAL_AUTHORITY,
            "k3y",
            &req,
        )
        .unwrap();
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
    fn wbfix3_truncated_response_is_bad_output() {
        let reply =
            r#"{"choices":[{"finish_reason":"length","message":{"content":"Incomplete"}}]}"#;
        let (dir, server) = serve(vec![(200, reply.into())]);
        let result = post_chat(
            &socket_client(&dir.path().join("s.sock")).unwrap(),
            LOCAL_AUTHORITY,
            "fixture-key",
            &GenerateRequest {
                messages: vec![],
                max_tokens: 8,
                timeout: Duration::from_secs(5),
            },
        );
        server.join().unwrap();
        assert!(
            matches!(result, Err(LocalLlmError::BadOutput(_))),
            "truncated cleanup must be rejected"
        );
    }

    #[test]
    fn wbfix3_server_output_cannot_reach_log_readers() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path(), "printf 'fixture-key /private/fixture/model\\n'; printf 'fixture-key /private/fixture/model\\n' >&2");
        let options = opts(dir.path().join("model"), Duration::from_secs(2));
        let mut child = spawn_from_supervisor(build_command(
            &binary,
            &options,
            Path::new("/private/llm.sock"),
            "fixture-key",
        ))
        .unwrap();
        let pipes = (child.stdout.take(), child.stderr.take());
        assert!(child.wait().unwrap().success());
        assert!(
            pipes.0.is_none() && pipes.1.is_none(),
            "server output must have no log reader pipes"
        );
    }

    #[test]
    fn post_chat_maps_http_errors_and_missing_content() {
        let (dir, server) = serve(vec![(401, "{}".into()), (200, "{}".into())]);
        let req = GenerateRequest {
            messages: vec![],
            max_tokens: 8,
            timeout: Duration::from_secs(5),
        };
        let client = socket_client(&dir.path().join("s.sock")).unwrap();
        assert_eq!(
            post_chat(&client, LOCAL_AUTHORITY, "k", &req),
            Err(LocalLlmError::Http(401))
        );
        assert!(matches!(
            post_chat(&client, LOCAL_AUTHORITY, "k", &req),
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
    fn early_exit_is_retried_once_with_a_new_socket() {
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
        assert_eq!(
            std::fs::read_dir(dir.path().join("llm-runtime"))
                .unwrap()
                .count(),
            0,
            "failed starts must remove socket directories"
        );
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
