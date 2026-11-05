//! Lifecycle of the local model: warm-up, serial requests, idle unload,
//! crash restart and the failed-start policy (three failed starts in a row enter `Failed`;
//! a third failure that is a start timeout earns one retry per cooldown, any other third
//! failure lasts till `reset_failure` or `unload`, the actions behind selecting a model and
//! deleting one). Verification runs outside the backend mutex.

use super::backend::{BackendOpts, GenerateRequest, KillSwitch, TextModelBackend};
use super::registry::{self, ModelEntry, PromptStyle};
use super::{prompt, LocalLlmError};
use crate::settings::{AppSettings, ModelUnloadTimeout};
use log::{info, warn};
use serde::Serialize;
use specta::Type;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// Consecutive failed starts before the manager stops trying. When the last one is a start
/// timeout, the first call after `ManagerConfig::retry_cooldown` makes one more start; any
/// other last failure stays failed till `reset_failure` or `unload`.
pub const MAX_FAILED_STARTS: u32 = 3;

/// Timings, overridable in tests.
#[derive(Debug, Clone, Copy)]
pub struct ManagerConfig {
    pub start_timeout: Duration,
    pub idle_tick: Duration,
    /// How long `Failed` lasts after a start timeout before one more start is allowed.
    pub retry_cooldown: Duration,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(10),
            idle_tick: Duration::from_secs(10),
            retry_cooldown: Duration::from_secs(5 * 60),
        }
    }
}

/// Internal lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalLlmState {
    Unloaded,
    Starting,
    Ready,
    Stopping,
    Failed { reason: String },
}

/// State name as sent to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LocalLlmStateKind {
    Unloaded,
    Starting,
    Ready,
    Stopping,
    Failed,
}

/// Payload of the `local-llm-state-changed` event and `get_local_llm_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct LocalLlmStatus {
    pub state: LocalLlmStateKind,
    pub model_id: Option<String>,
    pub error: Option<String>,
}

/// What the idle timer needs from the app on each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleInputs {
    pub timeout: ModelUnloadTimeout,
    pub recording: bool,
}

/// App-side callbacks, so the manager is testable without Tauri.
pub struct ManagerHooks {
    pub idle_inputs: Box<dyn Fn() -> IdleInputs + Send + Sync>,
    pub on_status: Box<dyn Fn(&LocalLlmStatus) + Send + Sync>,
}

struct Inner {
    backend: Box<dyn TextModelBackend>,
    state: LocalLlmState,
    loaded: Option<&'static ModelEntry>,
    last_used: Instant,
    failed_starts: u32,
    last_start_failure: Option<(Instant, LocalLlmError)>,
    /// When a `Failed` state caused by a start timeout may start once more.
    retry_at: Option<Instant>,
    /// The cooldown has elapsed and its one start has not run yet.
    retry_probe: bool,
}

type VerificationResult = Result<bool, LocalLlmError>;
type ModelVerifier =
    Arc<dyn Fn(&Path, &ModelEntry, &dyn Fn() -> bool) -> VerificationResult + Send + Sync>;
type RecordingCancellation = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone, PartialEq, Eq)]
struct FileIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    modified: std::time::SystemTime,
    changed: (i64, i64),
    size: u64,
}
impl FileIdentity {
    fn read(path: &Path, entry: &ModelEntry) -> Result<Self, LocalLlmError> {
        let metadata = std::fs::metadata(path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LocalLlmError::ModelMissing(entry.id.into()),
            _ => LocalLlmError::ModelRead(entry.id.into()),
        })?;
        #[cfg(unix)]
        let (device, inode, changed) = {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                (metadata.ctime(), metadata.ctime_nsec()),
            )
        };
        #[cfg(not(unix))]
        let (device, inode, changed) = (0, 0, (0, 0));
        Ok(Self {
            path: path.to_path_buf(),
            device,
            inode,
            modified: metadata
                .modified()
                .map_err(|_| LocalLlmError::ModelRead(entry.id.into()))?,
            changed,
            size: metadata.len(),
        })
    }
}
struct HashJob {
    cancelled: AtomicBool,
    #[cfg(test)]
    waiting: AtomicBool,
    result: Mutex<Option<VerificationResult>>,
    done: std::sync::Condvar,
}
struct ActiveHash {
    identity: FileIdentity,
    model: &'static str,
    job: Arc<HashJob>,
    thread: std::thread::JoinHandle<()>,
}
#[derive(Default)]
struct VerificationState {
    active: Option<ActiveHash>,
    cache: std::collections::BTreeMap<&'static str, (FileIdentity, bool)>,
}
#[derive(Default)]
struct Verification {
    state: Mutex<VerificationState>,
    epoch: std::sync::atomic::AtomicU64,
}
struct VerifiedFile {
    identity: FileIdentity,
    valid: bool,
}
struct VerificationWait<'a> {
    deadline: Instant,
    epoch: u64,
    cancelled: &'a dyn Fn() -> bool,
    shutdown: Arc<AtomicBool>,
    recording: Option<RecordingCancellation>,
}
impl Verification {
    fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(active) = &lock(&self.state).active {
            active.job.cancelled.store(true, Ordering::SeqCst);
            active.job.done.notify_all();
        }
    }
    fn verify(
        &self,
        path: &Path,
        entry: &'static ModelEntry,
        verifier: ModelVerifier,
        wait: VerificationWait<'_>,
    ) -> Result<VerifiedFile, LocalLlmError> {
        let VerificationWait {
            deadline,
            epoch,
            cancelled,
            shutdown,
            recording,
        } = wait;
        let identity = FileIdentity::read(path, entry)?;
        let mut waiting_for: Option<Arc<HashJob>> = None;
        loop {
            let mut state = lock(&self.state);
            if cancelled() || self.epoch.load(Ordering::SeqCst) != epoch {
                if let Some(active) = &state.active {
                    if active.identity.path == identity.path {
                        active.job.cancelled.store(true, Ordering::SeqCst);
                        active.job.done.notify_all();
                    }
                }
                return Err(LocalLlmError::VerificationCancelled);
            }
            if Instant::now() >= deadline {
                return Err(LocalLlmError::VerificationTimeout);
            }
            if let Some(job) = &waiting_for {
                if let Some(result) = lock(&job.result).clone() {
                    return result.map(|valid| VerifiedFile { identity, valid });
                }
            }
            let completed = state
                .active
                .as_ref()
                .and_then(|active| lock(&active.job.result).clone());
            if let Some(result) = completed {
                if let Some(active) = state.active.take() {
                    let _ = active.thread.join();
                    if let Ok(valid) = result {
                        state.cache.insert(active.model, (active.identity, valid));
                    }
                }
            }
            if let Some((cached, valid)) = state.cache.get(entry.id) {
                if cached.path == identity.path
                    && cached.device == identity.device
                    && cached.inode == identity.inode
                    && cached.modified == identity.modified
                    && cached.changed == identity.changed
                    && cached.size == identity.size
                {
                    return Ok(VerifiedFile {
                        identity,
                        valid: *valid,
                    });
                }
            }
            if state.active.is_none() {
                let job = Arc::new(HashJob {
                    cancelled: AtomicBool::new(false),
                    #[cfg(test)]
                    waiting: AtomicBool::new(false),
                    result: Mutex::new(None),
                    done: std::sync::Condvar::new(),
                });
                let work = job.clone();
                let key = identity.clone();
                let verify = verifier.clone();
                let stopping = shutdown.clone();
                let recording = recording.clone();
                let thread = std::thread::Builder::new()
                    .name("local-llm-verify".into())
                    .spawn(move || {
                        let cancelled = || {
                            work.cancelled.load(Ordering::SeqCst)
                                || stopping.load(Ordering::SeqCst)
                                || recording.as_ref().is_some_and(|check| check())
                        };
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let valid = verify(&key.path, entry, &cancelled)?;
                            if cancelled() {
                                return Err(LocalLlmError::VerificationCancelled);
                            }
                            if FileIdentity::read(&key.path, entry)? != key {
                                return Err(LocalLlmError::ModelRead(entry.id.into()));
                            }
                            Ok(valid)
                        }))
                        .unwrap_or_else(|_| Err(LocalLlmError::ModelRead(entry.id.into())));
                        *lock(&work.result) = Some(result);
                        work.done.notify_all();
                    })
                    .map_err(|_| LocalLlmError::Failed("cannot start model verification".into()))?;
                state.active = Some(ActiveHash {
                    identity: identity.clone(),
                    model: entry.id,
                    job,
                    thread,
                });
            }
            let Some(active) = &state.active else {
                continue;
            };
            let same_file = active.identity == identity;
            let job = active.job.clone();
            if same_file {
                waiting_for = Some(job.clone());
            }
            drop(state);
            let result = lock(&job.result);
            if let Some(result) = result.as_ref() {
                if same_file {
                    return result.clone().map(|valid| VerifiedFile { identity, valid });
                }
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            #[cfg(test)]
            job.waiting.store(true, Ordering::SeqCst);
            let _wait = job
                .done
                .wait_timeout(result, remaining.min(Duration::from_millis(10)))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}
impl Drop for Verification {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct LocalLlmManager {
    inner: Mutex<Inner>,
    status: Mutex<LocalLlmStatus>,
    cancel: Arc<AtomicBool>,
    kill_switch: Arc<dyn KillSwitch>,
    models_root: Option<PathBuf>,
    pub(super) verify: ModelVerifier,
    verification: Verification,
    hooks: ManagerHooks,
    config: ManagerConfig,
}

impl LocalLlmManager {
    /// Obtains the backend kill switch on a std thread.
    /// `models_root` is `<app_data>/models/llm`; `None` when the app data dir is unknown.
    pub fn new(
        backend: Box<dyn TextModelBackend>,
        models_root: Option<PathBuf>,
        hooks: ManagerHooks,
        config: ManagerConfig,
    ) -> Self {
        let (backend, kill_switch) = on_std_thread(move || {
            let switch = backend.kill_switch();
            (backend, switch)
        });
        Self {
            inner: Mutex::new(Inner {
                backend,
                state: LocalLlmState::Unloaded,
                loaded: None,
                last_used: Instant::now(),
                failed_starts: 0,
                last_start_failure: None,
                retry_at: None,
                retry_probe: false,
            }),
            status: Mutex::new(LocalLlmStatus {
                state: LocalLlmStateKind::Unloaded,
                model_id: None,
                error: None,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            kill_switch,
            models_root,
            verify: Arc::new(super::download::verify_model),
            verification: Verification::default(),
            hooks,
            config,
        }
    }

    #[cfg(test)]
    pub(super) fn verification_timeout(&mut self, timeout: Duration) {
        self.config.start_timeout = timeout;
    }

    /// Returns the model storage root without calling the backend.
    pub fn models_root(&self) -> Option<&Path> {
        self.models_root.as_deref()
    }

    /// Returns the cached status without waiting for the backend.
    pub fn status(&self) -> LocalLlmStatus {
        lock(&self.status).clone()
    }

    /// Starts the model on a std background thread; never blocks the caller.
    pub fn warm_up(self: &Arc<Self>, settings: &AppSettings) {
        self.warm_up_inner(settings, None);
    }

    /// Keeps recording cancellation connected after a warm-up wait times out.
    pub(super) fn warm_up_with_cancel(
        self: &Arc<Self>,
        settings: &AppSettings,
        cancelled: RecordingCancellation,
    ) {
        self.warm_up_inner(settings, Some(cancelled));
    }

    fn warm_up_inner(
        self: &Arc<Self>,
        settings: &AppSettings,
        cancelled: Option<RecordingCancellation>,
    ) {
        let this = Arc::clone(self);
        let settings = settings.clone();
        let spawned = std::thread::Builder::new()
            .name("local-llm-warmup".into())
            .spawn(move || {
                let result = if let Some(cancelled) = cancelled {
                    this.ready(
                        &settings,
                        Instant::now(),
                        &AtomicBool::new(false),
                        Some(cancelled),
                    )
                    .map(|_| ())
                } else {
                    this.ensure_started(&settings)
                };
                if let Err(e) = result {
                    warn!("local-llm: warm-up failed: {e}");
                }
            });
        if let Err(e) = spawned {
            warn!("local-llm: cannot start the warm-up thread: {e}");
        }
    }

    /// Starts on a std thread and blocks until ready or failed; use spawn_blocking in async code.
    pub fn ensure_started(&self, settings: &AppSettings) -> Result<(), LocalLlmError> {
        let call_started = Instant::now();
        on_std_thread(|| {
            self.ready(settings, call_started, &AtomicBool::new(false), None)
                .map(|_| ())
        })
    }

    #[cfg(test)]
    /// Serial cleanup on a std thread; blocks the caller, so use spawn_blocking in async code.
    pub fn process(
        &self,
        transcript: &str,
        settings: &AppSettings,
    ) -> Result<String, LocalLlmError> {
        self.process_with_cancel(transcript, settings, &AtomicBool::new(false))
    }

    pub(super) fn process_with_cancel(
        &self,
        transcript: &str,
        settings: &AppSettings,
        caller_cancelled: &AtomicBool,
    ) -> Result<String, LocalLlmError> {
        let call_started = Instant::now();
        on_std_thread(|| {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(LocalLlmError::Failed("shutting down".into()));
            }
            let (mut inner, entry) = self.ready(settings, call_started, caller_cancelled, None)?;
            let result = run_chunks(&mut inner, entry, transcript, settings);
            inner.last_used = Instant::now();
            match &result {
                Err(LocalLlmError::RequestTimeout) | Err(LocalLlmError::Transport(_)) => {
                    warn!("local-llm: stopping llama-server after a failed request");
                    self.stop_locked(&mut inner);
                }
                _ => {
                    if settings.model_unload_timeout == ModelUnloadTimeout::Immediately {
                        self.stop_locked(&mut inner);
                    }
                }
            }
            result
        })
    }

    /// Runs one idle step on a std thread; blocks the caller, so use spawn_blocking in async code.
    pub fn idle_tick(&self, now: Instant) {
        on_std_thread(|| {
            let inputs = (self.hooks.idle_inputs)();
            let mut inner = lock(&self.inner);
            if inner.state != LocalLlmState::Ready {
                return;
            }
            if !inner.backend.is_alive() {
                warn!("local-llm: llama-server exited unexpectedly");
                inner.loaded = None;
                self.set_state(
                    &mut inner,
                    LocalLlmState::Unloaded,
                    Some("llama-server exited unexpectedly".into()),
                );
                return;
            }
            if inputs.recording {
                inner.last_used = now;
                return;
            }
            let limit = match inputs.timeout {
                ModelUnloadTimeout::Never | ModelUnloadTimeout::Immediately => return,
                other => match other.to_seconds() {
                    Some(secs) => Duration::from_secs(secs),
                    None => return,
                },
            };
            if now.saturating_duration_since(inner.last_used) >= limit {
                info!("local-llm: unloading after {}s idle", limit.as_secs());
                self.stop_locked(&mut inner);
            }
        });
    }

    /// Unloads on a std thread; blocks the caller, so use spawn_blocking in async code.
    pub fn unload(&self) {
        self.cancel_verification();
        on_std_thread(|| {
            let mut inner = lock(&self.inner);
            self.stop_locked(&mut inner);
        });
    }

    /// Kills on a std thread, then waits at most 3 s for the request lock; use spawn_blocking in async code.
    pub fn shutdown(&self) {
        on_std_thread(|| {
            self.cancel.store(true, Ordering::SeqCst);
            self.cancel_verification();
            self.kill_switch.trigger();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut inner = match self.inner.try_lock() {
                    Ok(inner) => inner,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            warn!("local-llm: shutdown request lock remained busy after 3 s");
                            return;
                        }
                        std::thread::sleep(remaining.min(Duration::from_millis(10)));
                        continue;
                    }
                };
                self.stop_locked(&mut inner);
                return;
            }
        });
    }

    /// Resets failure on a std thread; blocks the caller, so use spawn_blocking in async code.
    pub fn reset_failure(&self) {
        on_std_thread(|| {
            let mut inner = lock(&self.inner);
            inner.failed_starts = 0;
            inner.last_start_failure = None;
            inner.retry_at = None;
            inner.retry_probe = false;
            if matches!(inner.state, LocalLlmState::Failed { .. }) {
                self.set_state(&mut inner, LocalLlmState::Unloaded, None);
            }
        });
    }

    /// Starts a std background thread that runs `idle_tick` every `config.idle_tick` until the manager is dropped or shut down.
    pub fn start_idle_thread(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let tick = self.config.idle_tick;
        let spawned = std::thread::Builder::new()
            .name("local-llm-idle".into())
            .spawn(move || loop {
                std::thread::sleep(tick);
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                if manager.cancel.load(Ordering::SeqCst) {
                    break;
                }
                manager.idle_tick(Instant::now());
            });
        if let Err(e) = spawned {
            warn!("local-llm: cannot start the idle timer: {e}");
        }
    }

    fn model_file(&self, entry: &ModelEntry) -> Result<PathBuf, LocalLlmError> {
        let root = self
            .models_root
            .as_deref()
            .ok_or_else(|| LocalLlmError::ModelMissing(entry.id.into()))?;
        let path = registry::model_path(root, entry);
        let meta = std::fs::metadata(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LocalLlmError::ModelMissing(entry.id.into()),
            _ => LocalLlmError::ModelRead(entry.id.into()),
        })?;
        if meta.len() != entry.size_bytes {
            return Err(LocalLlmError::ModelCorrupt(entry.id.into()));
        }
        Ok(path)
    }

    fn ready(
        &self,
        settings: &AppSettings,
        call_started: Instant,
        caller_cancelled: &AtomicBool,
        recording: Option<RecordingCancellation>,
    ) -> Result<(MutexGuard<'_, Inner>, &'static ModelEntry), LocalLlmError> {
        let epoch = self.verification.epoch.load(Ordering::SeqCst);
        let cancelled = || {
            self.cancel.load(Ordering::SeqCst)
                || caller_cancelled.load(Ordering::SeqCst)
                || recording.as_ref().is_some_and(|check| check())
        };
        if cancelled() {
            return Err(LocalLlmError::VerificationCancelled);
        }
        let mut inner = lock(&self.inner);
        let (entry, path, ready) = self.candidate_locked(&mut inner, settings, call_started)?;
        if ready {
            return Ok((inner, entry));
        }
        drop(inner);
        let deadline = call_started + self.config.start_timeout;
        let result = self.verification.verify(
            &path,
            entry,
            self.verify.clone(),
            VerificationWait {
                deadline,
                epoch,
                cancelled: &cancelled,
                shutdown: self.cancel.clone(),
                recording: recording.clone(),
            },
        );
        let verified = match result {
            Ok(verified) => verified,
            Err(error) => {
                self.report_verification_error(entry, &error);
                return Err(error);
            }
        };
        if !verified.valid {
            let error = LocalLlmError::ModelCorrupt(entry.id.into());
            self.report_verification_error(entry, &error);
            return Err(error);
        }
        let mut inner = lock(&self.inner);
        if cancelled() || self.verification.epoch.load(Ordering::SeqCst) != epoch {
            return Err(LocalLlmError::VerificationCancelled);
        }
        let (entry, path, ready) = self.candidate_locked(&mut inner, settings, call_started)?;
        if ready {
            return Ok((inner, entry));
        }
        if FileIdentity::read(&path, entry)? != verified.identity {
            return Err(LocalLlmError::ModelRead(entry.id.into()));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LocalLlmError::VerificationTimeout);
        }
        self.start_locked(&mut inner, entry, path, settings, remaining)?;
        Ok((inner, entry))
    }

    fn report_verification_error(&self, entry: &ModelEntry, error: &LocalLlmError) {
        let mut status = lock(&self.status);
        if status.state != LocalLlmStateKind::Unloaded {
            return;
        }
        status.model_id = Some(entry.id.into());
        status.error = Some(error.to_string());
        let snapshot = status.clone();
        drop(status);
        (self.hooks.on_status)(&snapshot);
    }

    /// Cancels the active verification without discarding completed byte checks.
    pub(super) fn cancel_verification(&self) {
        self.verification.cancel();
    }

    /// Clears a recovered model's error without waiting for a backend request.
    pub(super) fn clear_download_error(&self, model: &str) {
        let mut status = lock(&self.status);
        if status.model_id.as_deref() == Some(model) && status.state == LocalLlmStateKind::Unloaded
        {
            status.error = None;
            let snapshot = status.clone();
            drop(status);
            (self.hooks.on_status)(&snapshot);
        }
    }

    fn candidate_locked(
        &self,
        inner: &mut Inner,
        settings: &AppSettings,
        call_started: Instant,
    ) -> Result<(&'static ModelEntry, PathBuf, bool), LocalLlmError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(LocalLlmError::Failed("shutting down".into()));
        }
        if let LocalLlmState::Failed { reason } = &inner.state {
            // A start timeout earns one probe start per cooldown. The probe is the strike that
            // reaches the limit again: a timeout re-arms the timer, any other error is sticky.
            if !inner.retry_at.is_some_and(|at| Instant::now() >= at) {
                return Err(LocalLlmError::Failed(reason.clone()));
            }
            inner.retry_at = None;
            inner.retry_probe = true;
            inner.failed_starts = MAX_FAILED_STARTS - 1;
            self.set_state(inner, LocalLlmState::Unloaded, None);
        }
        // A start that failed while this call waited for the lock answers it too.
        if let Some((at, err)) = &inner.last_start_failure {
            if *at >= call_started {
                return Err(err.clone());
            }
        }
        let model_id = settings
            .local_llm_model_id
            .as_deref()
            .ok_or_else(|| LocalLlmError::ModelMissing("no local model selected".into()))?;
        let entry = registry::find(model_id)
            .ok_or_else(|| LocalLlmError::ModelMissing(format!("unknown model id {model_id}")))?;
        let path = match self.model_file(entry) {
            Ok(p) => p,
            Err(e) => {
                self.stop_locked(inner);
                self.set_state_for(
                    inner,
                    LocalLlmState::Unloaded,
                    Some(entry.id),
                    Some(e.to_string()),
                );
                return Err(e);
            }
        };
        if inner.state == LocalLlmState::Ready {
            if inner.loaded == Some(entry) && inner.backend.is_alive() {
                return Ok((entry, path, true));
            }
            warn!("local-llm: llama-server gone or model changed; restarting");
            self.stop_locked(inner);
        }
        Ok((entry, path, false))
    }

    fn start_locked(
        &self,
        inner: &mut Inner,
        entry: &'static ModelEntry,
        model_path: PathBuf,
        settings: &AppSettings,
        start_timeout: Duration,
    ) -> Result<(), LocalLlmError> {
        self.set_state_for(inner, LocalLlmState::Starting, Some(entry.id), None);
        let opts = BackendOpts {
            model_path,
            threads: settings.local_llm_threads.max(1),
            ctx: entry.default_ctx,
            start_timeout,
            cancel: Arc::clone(&self.cancel),
            single_spawn: std::mem::take(&mut inner.retry_probe),
        };
        let result = inner.backend.ensure_loaded(entry, &opts);
        let result = if self.cancel.load(Ordering::SeqCst) {
            Err(LocalLlmError::Failed("shutting down".into()))
        } else {
            result
        };
        match result {
            Ok(()) => {
                inner.loaded = Some(entry);
                inner.failed_starts = 0;
                inner.last_start_failure = None;
                inner.retry_at = None;
                inner.last_used = Instant::now();
                self.set_state_for(inner, LocalLlmState::Ready, Some(entry.id), None);
                Ok(())
            }
            Err(e) => {
                inner.backend.unload();
                inner.loaded = None;
                inner.failed_starts += 1;
                inner.last_start_failure = Some((Instant::now(), e.clone()));
                let next = if inner.failed_starts >= MAX_FAILED_STARTS {
                    inner.retry_at = (e == LocalLlmError::StartTimeout)
                        .then(|| Instant::now() + self.config.retry_cooldown);
                    LocalLlmState::Failed {
                        reason: e.to_string(),
                    }
                } else {
                    LocalLlmState::Unloaded
                };
                self.set_state_for(inner, next, Some(entry.id), Some(e.to_string()));
                Err(e)
            }
        }
    }

    fn stop_locked(&self, inner: &mut Inner) {
        if inner.loaded.is_none() && inner.state == LocalLlmState::Unloaded {
            return;
        }
        let model_id = inner.loaded.map(|m| m.id);
        self.set_state_for(inner, LocalLlmState::Stopping, model_id, None);
        inner.backend.unload();
        inner.loaded = None;
        self.set_state_for(inner, LocalLlmState::Unloaded, None, None);
    }

    fn set_state(&self, inner: &mut Inner, state: LocalLlmState, error: Option<String>) {
        let model_id = inner.loaded.map(|m| m.id);
        self.set_state_for(inner, state, model_id, error);
    }

    fn set_state_for(
        &self,
        inner: &mut Inner,
        state: LocalLlmState,
        model_id: Option<&str>,
        error: Option<String>,
    ) {
        let kind = match &state {
            LocalLlmState::Unloaded => LocalLlmStateKind::Unloaded,
            LocalLlmState::Starting => LocalLlmStateKind::Starting,
            LocalLlmState::Ready => LocalLlmStateKind::Ready,
            LocalLlmState::Stopping => LocalLlmStateKind::Stopping,
            LocalLlmState::Failed { .. } => LocalLlmStateKind::Failed,
        };
        inner.state = state;
        let status = LocalLlmStatus {
            state: kind,
            model_id: model_id.map(str::to_string),
            error,
        };
        *lock(&self.status) = status.clone();
        (self.hooks.on_status)(&status);
    }
}

/// Chunks, generates per chunk in order, guards each output, joins with one space.
fn run_chunks(
    inner: &mut Inner,
    entry: &ModelEntry,
    transcript: &str,
    settings: &AppSettings,
) -> Result<String, LocalLlmError> {
    let control = match entry.prompt_style {
        PromptStyle::S1ControlLine => prompt::control_line_for(settings),
        PromptStyle::PlainSystemPrompt => String::new(),
    };
    let system = prompt::system_prompt(entry.prompt_style, settings)?;
    let budget = prompt::chunk_budget(&system, &control);
    let mut outputs = Vec::new();
    for chunk in prompt::chunk_transcript(transcript, budget) {
        let req = GenerateRequest {
            messages: prompt::build_messages(entry.prompt_style, &system, &control, &chunk),
            max_tokens: prompt::max_tokens(&chunk, entry.default_ctx),
            timeout: prompt::request_timeout(&chunk),
        };
        let cleaned = prompt::clean_output(&inner.backend.generate(&req)?);
        prompt::check_output(&chunk, &cleaned)?;
        if !cleaned.is_empty() {
            outputs.push(cleaned);
        }
    }
    Ok(outputs.join(" "))
}

/// Locks a mutex, recovering the data if a panicking thread poisoned it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs blocking backend work outside any async runtime worker.
fn on_std_thread<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| match scope.spawn(work).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::settings::{get_default_settings, LOCAL_LLM_PROVIDER_ID};
    use std::collections::VecDeque;

    #[derive(Default)]
    pub(crate) struct FakeState {
        pub request_block: bool,
        pub request_succeeds_on_release: bool,
        pub request_ignores_kill: bool,
        pub request_killed: Arc<AtomicBool>,
        pub request_started: Arc<AtomicBool>,
        pub request_release: Arc<AtomicBool>,
        pub loads: u32,
        pub unloads: u32,
        pub requests: Vec<GenerateRequest>,
        pub start_results: VecDeque<Result<(), LocalLlmError>>,
        pub replies: VecDeque<Result<String, LocalLlmError>>,
        pub start_delay: Duration,
        pub block_until_cancel: bool,
        pub alive: bool,
        pub single_spawns: Vec<bool>,
    }

    /// Echoes the transcript (the user message after the control line) unless a reply is queued.
    pub(crate) struct FakeBackend(pub Arc<Mutex<FakeState>>);

    struct FakeKillSwitch(Arc<AtomicBool>);

    impl KillSwitch for FakeKillSwitch {
        fn trigger(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl TextModelBackend for FakeBackend {
        fn kill_switch(&self) -> Arc<dyn KillSwitch> {
            Arc::new(FakeKillSwitch(lock(&self.0).request_killed.clone()))
        }

        fn ensure_loaded(
            &mut self,
            _model: &ModelEntry,
            opts: &BackendOpts,
        ) -> Result<(), LocalLlmError> {
            let (delay, block) = {
                let mut s = lock(&self.0);
                s.loads += 1;
                s.single_spawns.push(opts.single_spawn);
                (s.start_delay, s.block_until_cancel)
            };
            if block {
                for _ in 0..500 {
                    if opts.cancel.load(Ordering::SeqCst) {
                        return Err(LocalLlmError::StartFailed(
                            "start cancelled: shutting down".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(LocalLlmError::StartTimeout);
            }
            std::thread::sleep(delay);
            let mut s = lock(&self.0);
            let r = s.start_results.pop_front().unwrap_or(Ok(()));
            s.alive = r.is_ok();
            r
        }

        fn generate(&mut self, req: &GenerateRequest) -> Result<String, LocalLlmError> {
            let (block, ignores_kill, killed, started, release) = {
                let s = lock(&self.0);
                (
                    s.request_block,
                    s.request_ignores_kill,
                    s.request_killed.clone(),
                    s.request_started.clone(),
                    s.request_release.clone(),
                )
            };
            if block {
                started.store(true, Ordering::SeqCst);
                for _ in 0..500 {
                    if release.load(Ordering::SeqCst) {
                        if lock(&self.0).request_succeeds_on_release {
                            break;
                        }
                        return Err(LocalLlmError::Transport("request interrupted".into()));
                    }
                    if !ignores_kill && killed.load(Ordering::SeqCst) {
                        return Err(LocalLlmError::Transport("request interrupted".into()));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !release.load(Ordering::SeqCst) {
                    return Err(LocalLlmError::RequestTimeout);
                }
            }
            let mut s = lock(&self.0);
            s.requests.push(req.clone());
            if let Some(reply) = s.replies.pop_front() {
                return reply;
            }
            let user = &req.messages[1].content;
            Ok(if user.starts_with("[Styling:") {
                user.split_once('\n')
                    .map(|(_, text)| text)
                    .unwrap_or(user)
                    .to_string()
            } else {
                user.to_string()
            })
        }

        fn unload(&mut self) {
            let mut s = lock(&self.0);
            s.unloads += 1;
            s.alive = false;
        }

        fn is_alive(&mut self) -> bool {
            lock(&self.0).alive
        }
    }

    pub(crate) struct Harness {
        pub manager: Arc<LocalLlmManager>,
        pub fake: Arc<Mutex<FakeState>>,
        pub timeout: Arc<Mutex<ModelUnloadTimeout>>,
        pub recording: Arc<AtomicBool>,
        pub events: Arc<Mutex<Vec<LocalLlmStatus>>>,
        pub settings: AppSettings,
        pub model_file: PathBuf,
        _dir: tempfile::TempDir,
    }

    /// Manager over a FakeBackend with a sparse, correctly sized model file.
    pub(crate) fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let entry = registry::find("s1-mini-q4km").unwrap();
        let model_file = registry::model_path(dir.path(), entry);
        std::fs::create_dir_all(model_file.parent().unwrap()).unwrap();
        std::fs::File::create(&model_file)
            .unwrap()
            .set_len(entry.size_bytes)
            .unwrap();
        let fake = Arc::new(Mutex::new(FakeState::default()));
        let timeout = Arc::new(Mutex::new(ModelUnloadTimeout::Min2));
        let recording = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::new()));
        let (t, r, ev) = (timeout.clone(), recording.clone(), events.clone());
        let hooks = ManagerHooks {
            idle_inputs: Box::new(move || IdleInputs {
                timeout: *lock(&t),
                recording: r.load(Ordering::SeqCst),
            }),
            on_status: Box::new(move |s| lock(&ev).push(s.clone())),
        };
        let mut manager = LocalLlmManager::new(
            Box::new(FakeBackend(fake.clone())),
            Some(dir.path().to_path_buf()),
            hooks,
            ManagerConfig {
                start_timeout: Duration::from_secs(2),
                idle_tick: Duration::from_secs(3600),
                retry_cooldown: Duration::from_secs(3600),
            },
        );
        manager.verify = Arc::new(|_, _, _| Ok(true));
        let manager = Arc::new(manager);
        let mut settings = get_default_settings();
        settings.post_process_provider_id = LOCAL_LLM_PROVIDER_ID.to_string();
        settings.local_llm_model_id = Some(entry.id.to_string());
        settings.model_unload_timeout = ModelUnloadTimeout::Min2;
        Harness {
            manager,
            fake,
            timeout,
            recording,
            events,
            settings,
            model_file,
            _dir: dir,
        }
    }

    fn fix1_hash_probe(
        h: &mut Harness,
        good: bool,
    ) -> (
        Arc<AtomicBool>,
        Arc<std::sync::atomic::AtomicUsize>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered, ready) = std::sync::mpsc::channel();
        let flag = release.clone();
        let count = calls.clone();
        let manager = Arc::get_mut(&mut h.manager).unwrap();
        manager.config.start_timeout = Duration::from_millis(40);
        manager.verify = Arc::new(move |_, _, cancelled| {
            count.fetch_add(1, Ordering::SeqCst);
            entered.send(()).unwrap();
            for _ in 0..5000 {
                if cancelled() {
                    return Err(LocalLlmError::VerificationCancelled);
                }
                if flag.load(Ordering::SeqCst) {
                    return Ok(good);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(LocalLlmError::ModelRead("fixture deadline".into()))
        });
        (release, calls, ready)
    }
    struct Fix1Release(Arc<AtomicBool>);
    impl Drop for Fix1Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    fn fix1_finish_hash(manager: &LocalLlmManager) {
        let job = lock(&manager.verification.state)
            .active
            .as_ref()
            .unwrap()
            .job
            .clone();
        let result = lock(&job.result);
        let (result, _) = job
            .done
            .wait_timeout_while(result, Duration::from_secs(2), |value| value.is_none())
            .unwrap();
        assert!(
            result.is_some(),
            "background hash must settle after release or cancellation"
        );
    }

    #[test]
    fn fix1_recording_cancel_reaches_warmup_hash_after_raw_fallback() {
        let mut h = harness();
        let (release, _, entered) = fix1_hash_probe(&mut h, true);
        let _release = Fix1Release(release);
        let caller = Arc::new(AtomicBool::new(false));
        let flag = caller.clone();
        h.manager
            .warm_up_with_cancel(&h.settings, Arc::new(move || flag.load(Ordering::SeqCst)));
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..2000 {
            if h.manager.status().error.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            h.manager.status().error.as_deref(),
            Some(LocalLlmError::VerificationTimeout.to_string().as_str())
        );
        caller.store(true, Ordering::SeqCst);
        let job = lock(&h.manager.verification.state)
            .active
            .as_ref()
            .unwrap()
            .job
            .clone();
        let result = lock(&job.result);
        let (result, _) = job
            .done
            .wait_timeout_while(result, Duration::from_secs(1), |value| value.is_none())
            .unwrap();
        assert_eq!(
            *result,
            Some(Err(LocalLlmError::VerificationCancelled)),
            "recording cancellation must reach a warm-up hash after raw fallback"
        );
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn fix1_slow_hash_is_bounded_outside_inner_and_reused_without_a_queue() {
        let mut h = harness();
        let (release, calls, entered) = fix1_hash_probe(&mut h, true);
        let _release = Fix1Release(release.clone());
        let manager = h.manager.clone();
        let settings = h.settings.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || {
            tx.send(manager.process("preserve raw words", &settings))
                .unwrap()
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let inner = h.manager.inner.try_lock();
        assert!(inner.is_ok(), "hashing must release the lifecycle mutex");
        let inner = inner.unwrap();
        let result = rx.recv_timeout(Duration::from_secs(1));
        drop(inner);
        first.join().unwrap();
        assert!(
            matches!(result, Ok(Err(LocalLlmError::VerificationTimeout))),
            "slow verification must return raw fallback at the cold-start bound"
        );
        for _ in 0..3 {
            assert_eq!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::VerificationTimeout)
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only one background hash may run while callers time out"
        );
        assert_eq!(lock(&h.manager.inner).failed_starts, 0);
        assert_eq!(h.manager.status().state, LocalLlmStateKind::Unloaded);
        assert!(!h.manager.status().error.unwrap().contains("delete"));
        release.store(true, Ordering::SeqCst);
        fix1_finish_hash(&h.manager);
        h.manager.ensure_started(&h.settings).unwrap();
        h.manager.unload();
        h.manager.ensure_started(&h.settings).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a later cold load must reuse completed verification"
        );
    }

    #[test]
    fn fix1_cancel_and_shutdown_reach_the_active_hash_without_waiting_for_inner() {
        for shutdown in [false, true] {
            let mut h = harness();
            let (entered, ready) = std::sync::mpsc::channel();
            let (observed, cancelled_hash) = std::sync::mpsc::channel();
            let manager = Arc::get_mut(&mut h.manager).unwrap();
            manager.config.start_timeout = Duration::from_secs(3);
            manager.verify = Arc::new(move |_, _, cancelled| {
                entered.send(()).unwrap();
                for _ in 0..2000 {
                    if cancelled() {
                        observed.send(()).unwrap();
                        return Err(LocalLlmError::VerificationCancelled);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(true)
            });
            let caller_cancelled = Arc::new(AtomicBool::new(false));
            let flag = caller_cancelled.clone();
            let child = h.manager.clone();
            let settings = h.settings.clone();
            let request = std::thread::spawn(move || {
                child.process_with_cancel("raw words", &settings, &flag)
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            if shutdown {
                let started = Instant::now();
                h.manager.shutdown();
                assert!(
                    started.elapsed() < Duration::from_millis(500),
                    "shutdown must not spend its request-lock deadline on a hash"
                );
            } else {
                caller_cancelled.store(true, Ordering::SeqCst);
            }
            assert!(
                cancelled_hash.recv_timeout(Duration::from_secs(1)).is_ok(),
                "caller cancellation must reach the active hash callback"
            );
            assert!(request.join().unwrap().is_err());
            assert_eq!(lock(&h.fake).loads, 0);
            fix1_finish_hash(&h.manager);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fix1_cache_rehashes_changed_identity_and_remembers_corruption() {
        use std::sync::atomic::AtomicUsize;
        for field in ["path", "device", "inode", "modified", "changed", "size"] {
            let mut h = harness();
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(move |_, _, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            });
            for _ in 0..2 {
                assert!(matches!(
                    h.manager.ensure_started(&h.settings),
                    Err(LocalLlmError::ModelCorrupt(_))
                ));
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "an unchanged corrupt GGUF must not be hashed twice"
            );
            {
                let mut state = lock(&h.manager.verification.state);
                let (cached, _) = state.cache.get_mut("s1-mini-q4km").unwrap();
                match field {
                    "path" => cached.path = cached.path.with_extension("alias"),
                    "device" => cached.device ^= 1,
                    "inode" => cached.inode ^= 1,
                    "modified" => cached.modified += Duration::from_secs(1),
                    "changed" => cached.changed.0 += 1,
                    "size" => cached.size += 1,
                    _ => unreachable!(),
                }
            }
            assert!(matches!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::ModelCorrupt(_))
            ));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "changed {field} alone must invalidate verification"
            );
        }
    }

    #[test]
    fn fix1_file_changed_during_hash_is_not_loaded_and_is_rechecked() {
        let mut h = harness();
        let (release, calls, entered) = fix1_hash_probe(&mut h, true);
        Arc::get_mut(&mut h.manager).unwrap().config.start_timeout = Duration::from_secs(3);
        let _release = Fix1Release(release.clone());
        let manager = h.manager.clone();
        let settings = h.settings.clone();
        let request = std::thread::spawn(move || manager.ensure_started(&settings));
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let job = lock(&h.manager.verification.state)
            .active
            .as_ref()
            .unwrap()
            .job
            .clone();
        for _ in 0..2000 {
            if job.waiting.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(job.waiting.load(Ordering::SeqCst));
        let state = lock(&h.manager.verification.state);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&h.model_file)
            .unwrap();
        file.set_modified(file.metadata().unwrap().modified().unwrap() + Duration::from_secs(1))
            .unwrap();
        release.store(true, Ordering::SeqCst);
        let result = lock(&job.result);
        let (result, _) = job
            .done
            .wait_timeout_while(result, Duration::from_secs(2), |value| value.is_none())
            .unwrap();
        assert!(result.is_some());
        drop(result);
        drop(state);
        assert!(
            matches!(request.join().unwrap(), Err(LocalLlmError::ModelRead(_))),
            "completed verification errors must not restart hashing or load changed files"
        );
        assert_eq!(lock(&h.fake).loads, 0);
        h.manager.ensure_started(&h.settings).unwrap();
        assert!(
            calls.load(Ordering::SeqCst) == 2,
            "completed verification errors must not restart hashing or load changed files"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fix2_restored_mtime_tampering_rehashes_before_loading() {
        use std::io::{Read, Write};
        use std::os::unix::fs::MetadataExt;
        let mut h = harness();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(move |path, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            let mut byte = [0];
            std::fs::File::open(path)
                .unwrap()
                .read_exact(&mut byte)
                .unwrap();
            Ok(byte == [0])
        });
        h.manager.ensure_started(&h.settings).unwrap();
        h.manager.unload();
        let before = std::fs::metadata(&h.model_file).unwrap();
        let identity =
            FileIdentity::read(&h.model_file, registry::find("s1-mini-q4km").unwrap()).unwrap();
        assert_eq!(
            identity.device,
            before.dev(),
            "file identity must capture the filesystem device"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&h.model_file)
            .unwrap();
        file.write_all(&[255]).unwrap();
        file.set_modified(before.modified().unwrap()).unwrap();
        let after = file.metadata().unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_ne!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec())
        );
        assert!(
            matches!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::ModelCorrupt(_))
            ),
            "restored mtime must not let changed bytes reach the loader"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "changed ctime must force rehash"
        );
        assert_eq!(lock(&h.fake).loads, 1, "tampered bytes must not load again");
    }

    #[test]
    fn fix2_worker_identity_check_rejects_changed_file_without_caching() {
        let mut h = harness();
        let (release, _, entered) = fix1_hash_probe(&mut h, true);
        Arc::get_mut(&mut h.manager).unwrap().config.start_timeout = Duration::from_secs(5);
        let _release = Fix1Release(release.clone());
        let (manager, settings) = (h.manager.clone(), h.settings.clone());
        let request = std::thread::spawn(move || manager.ensure_started(&settings));
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let job = lock(&h.manager.verification.state)
            .active
            .as_ref()
            .unwrap()
            .job
            .clone();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&h.model_file)
            .unwrap();
        file.set_modified(file.metadata().unwrap().modified().unwrap() + Duration::from_secs(1))
            .unwrap();
        release.store(true, Ordering::SeqCst);
        fix1_finish_hash(&h.manager);
        let result = lock(&job.result).clone();
        let outcome = request.join().unwrap();
        assert!(
            matches!(result, Some(Err(LocalLlmError::ModelRead(_)))),
            "worker identity check must return a read error before any loader check"
        );
        assert!(matches!(outcome, Err(LocalLlmError::ModelRead(_))));
        assert!(
            lock(&h.manager.verification.state).cache.is_empty(),
            "changed identity must not enter verification cache"
        );
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn fix2_loader_identity_check_rejects_change_after_successful_hash() {
        let mut h = harness();
        let (release, _, entered) = fix1_hash_probe(&mut h, true);
        Arc::get_mut(&mut h.manager).unwrap().config.start_timeout = Duration::from_secs(5);
        let _release = Fix1Release(release.clone());
        let (manager, settings) = (h.manager.clone(), h.settings.clone());
        let request = std::thread::spawn(move || manager.ensure_started(&settings));
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let held = lock(&h.manager.inner);
        let job = lock(&h.manager.verification.state)
            .active
            .as_ref()
            .unwrap()
            .job
            .clone();
        release.store(true, Ordering::SeqCst);
        fix1_finish_hash(&h.manager);
        assert_eq!(
            *lock(&job.result),
            Some(Ok(true)),
            "worker must finish against unchanged bytes"
        );
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&h.model_file)
            .unwrap();
        file.set_modified(file.metadata().unwrap().modified().unwrap() + Duration::from_secs(1))
            .unwrap();
        drop(held);
        assert!(
            matches!(request.join().unwrap(), Err(LocalLlmError::ModelRead(_))),
            "loader identity check must reject changes after successful verification"
        );
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn fix3_start_timeouts_reach_sticky_failed_without_another_spawn() {
        let h = harness();
        for _ in 0..=MAX_FAILED_STARTS {
            lock(&h.fake)
                .start_results
                .push_back(Err(LocalLlmError::StartTimeout));
        }
        for _ in 0..MAX_FAILED_STARTS {
            assert_eq!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::StartTimeout)
            );
        }
        assert_eq!(
            lock(&h.manager.inner).failed_starts,
            MAX_FAILED_STARTS,
            "every backend start timeout must count as a failed start"
        );
        assert_eq!(state(&h), LocalLlmStateKind::Failed);
        assert_eq!(lock(&h.fake).loads, MAX_FAILED_STARTS);
        assert!(
            matches!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::Failed(_))
            ),
            "exhausted start timeouts must stay Failed"
        );
        assert_eq!(
            lock(&h.fake).loads,
            MAX_FAILED_STARTS,
            "Failed must not spawn another backend"
        );
    }

    /// Drives `ensure_started` through one failed start per error and returns after the last.
    fn fail_starts(h: &Harness, errors: &[LocalLlmError]) {
        for error in errors {
            lock(&h.fake).start_results.push_back(Err(error.clone()));
        }
        for error in errors {
            assert_eq!(
                h.manager.ensure_started(&h.settings),
                Err(error.clone()),
                "each scripted start must report its own error"
            );
        }
    }

    fn three_timeouts() -> Vec<LocalLlmError> {
        vec![LocalLlmError::StartTimeout; MAX_FAILED_STARTS as usize]
    }

    fn retry_at(h: &Harness) -> Option<Instant> {
        lock(&h.manager.inner).retry_at
    }

    fn expire_cooldown(h: &Harness) {
        lock(&h.manager.inner).retry_at = Some(Instant::now());
    }

    fn count_verifications(h: &mut Harness) -> Arc<std::sync::atomic::AtomicUsize> {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(move |_, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        });
        calls
    }

    /// Moves the model's mtime so a cached verification no longer answers for it.
    fn invalidate_verification_identity(h: &Harness) {
        let file = std::fs::File::options()
            .write(true)
            .open(&h.model_file)
            .unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn retry_production_cooldown_is_five_minutes() {
        assert_eq!(
            ManagerConfig::default().retry_cooldown,
            Duration::from_secs(300),
            "the production retry cooldown must be 5 minutes"
        );
    }

    #[test]
    fn retry_three_timeouts_wait_for_the_cooldown_without_a_spawn() {
        let mut h = harness();
        let hashes = count_verifications(&mut h);
        let before = Instant::now();
        fail_starts(&h, &three_timeouts());
        let after = Instant::now();
        let cooldown = h.manager.config.retry_cooldown;
        let at = retry_at(&h).expect("a timeout streak must schedule a retry");
        assert!(
            at >= before + cooldown && at <= after + cooldown,
            "the retry time must be one cooldown after the third timeout"
        );
        assert_eq!(state(&h), LocalLlmStateKind::Failed);
        assert_eq!(hashes.load(Ordering::SeqCst), 1, "the streak hashes once");
        invalidate_verification_identity(&h);
        for _ in 0..2 {
            assert!(
                matches!(
                    h.manager.ensure_started(&h.settings),
                    Err(LocalLlmError::Failed(_))
                ),
                "Failed must answer before the cooldown"
            );
        }
        assert_eq!(
            hashes.load(Ordering::SeqCst),
            1,
            "no verification before the cooldown"
        );
        assert_eq!(
            lock(&h.fake).loads,
            MAX_FAILED_STARTS,
            "no spawn before the cooldown"
        );
        expire_cooldown(&h);
        fail_starts(&h, &[LocalLlmError::StartTimeout]);
        assert_eq!(
            hashes.load(Ordering::SeqCst),
            2,
            "the cooldown must allow one verification"
        );
        assert_eq!(
            lock(&h.fake).loads,
            MAX_FAILED_STARTS + 1,
            "the cooldown must allow one load"
        );
        invalidate_verification_identity(&h);
        assert!(matches!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::Failed(_))
        ));
        assert_eq!(
            hashes.load(Ordering::SeqCst),
            2,
            "a re-armed cooldown must not verify again"
        );
        assert_eq!(
            lock(&h.fake).loads,
            MAX_FAILED_STARTS + 1,
            "a re-armed cooldown must not load again"
        );
    }

    #[test]
    fn retry_one_spawn_after_the_cooldown_and_success_clears() {
        let h = harness();
        fail_starts(&h, &three_timeouts());
        expire_cooldown(&h);
        assert_eq!(
            h.manager.ensure_started(&h.settings),
            Ok(()),
            "the first call after the cooldown must start again"
        );
        assert_eq!(
            lock(&h.fake).loads,
            MAX_FAILED_STARTS + 1,
            "exactly one start per cooldown"
        );
        assert_eq!(state(&h), LocalLlmStateKind::Ready);
        assert_eq!(
            lock(&h.manager.inner).failed_starts,
            0,
            "a good start resets the strikes"
        );
    }

    /// `unload` moves `Failed` to `Unloaded` and leaves the timer: the one real stale timer.
    #[test]
    fn retry_good_start_clears_a_timer_left_by_unload() {
        let h = harness();
        fail_starts(&h, &three_timeouts());
        h.manager.unload();
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        assert!(retry_at(&h).is_some(), "unload must leave the timer stale");
        assert_eq!(h.manager.ensure_started(&h.settings), Ok(()));
        assert!(retry_at(&h).is_none(), "a good start clears the timer");
    }

    #[test]
    fn retry_marks_only_the_probe_as_a_single_spawn() {
        let h = harness();
        fail_starts(&h, &three_timeouts());
        assert_eq!(
            lock(&h.fake).single_spawns,
            [false; 3],
            "ordinary starts may spawn twice"
        );
        expire_cooldown(&h);
        assert_eq!(h.manager.ensure_started(&h.settings), Ok(()));
        assert_eq!(
            lock(&h.fake).single_spawns.last(),
            Some(&true),
            "the probe must be limited to one spawn"
        );
        h.manager.unload();
        assert_eq!(h.manager.ensure_started(&h.settings), Ok(()));
        assert_eq!(
            lock(&h.fake).single_spawns.last(),
            Some(&false),
            "the start after a probe may spawn twice"
        );
    }

    #[test]
    fn retry_probe_limit_outlives_a_failed_check_until_reset() {
        let mut h = harness();
        let failing = Arc::new(AtomicBool::new(false));
        let fail_now = failing.clone();
        Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(move |_, _, _| {
            if fail_now.load(Ordering::SeqCst) {
                Err(LocalLlmError::ModelRead("fixture".into()))
            } else {
                Ok(true)
            }
        });
        let probe_is_checked_then_started = |h: &Harness, expected: bool, message: &str| {
            fail_starts(h, &three_timeouts());
            expire_cooldown(h);
            failing.store(true, Ordering::SeqCst);
            invalidate_verification_identity(h);
            assert!(matches!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::ModelRead(_))
            ));
            failing.store(false, Ordering::SeqCst);
            if !expected {
                h.manager.reset_failure();
            }
            assert_eq!(h.manager.ensure_started(&h.settings), Ok(()));
            assert_eq!(
                lock(&h.fake).single_spawns.last(),
                Some(&expected),
                "{message}"
            );
            h.manager.unload();
        };
        probe_is_checked_then_started(&h, true, "a failed check must not spend the probe limit");
        probe_is_checked_then_started(&h, false, "reset must clear the probe limit");
    }

    /// Manager over the real backend; its fake binary appends a line per spawn to the returned file.
    #[cfg(unix)]
    fn real_backend_harness(script: &str, start_timeout: Duration) -> (Harness, PathBuf) {
        let mut h = harness();
        let root = h._dir.path().to_path_buf();
        let spawns = root.join("spawns");
        let script = format!(
            "echo x >> '{log}'\nrun=$(wc -l < '{log}')\n{script}",
            log = spawns.display()
        );
        let binary = crate::local_llm::llama_server::tests::fake_binary(&root, &script);
        let mut manager = LocalLlmManager::new(
            Box::new(crate::local_llm::llama_server::LlamaServerBackend::with_binary(binary)),
            Some(root),
            ManagerHooks {
                idle_inputs: Box::new(|| IdleInputs {
                    timeout: ModelUnloadTimeout::Never,
                    recording: false,
                }),
                on_status: Box::new(|_| {}),
            },
            ManagerConfig {
                start_timeout,
                idle_tick: Duration::from_secs(3600),
                retry_cooldown: Duration::from_secs(3600),
            },
        );
        manager.verify = Arc::new(|_, _, _| Ok(true));
        h.manager = Arc::new(manager);
        (h, spawns)
    }

    #[cfg(unix)]
    fn spawn_count(spawns: &Path) -> usize {
        std::fs::read_to_string(spawns).unwrap().lines().count()
    }

    #[cfg(unix)]
    #[test]
    fn retry_crash_then_hang_streak_is_sticky() {
        let (h, spawns) = real_backend_harness(
            "if [ $((run % 2)) -eq 1 ]; then exit 1; fi\nexec sleep 30",
            Duration::from_secs(2),
        );
        for _ in 0..MAX_FAILED_STARTS {
            let result = h.manager.ensure_started(&h.settings);
            assert!(
                matches!(result, Err(LocalLlmError::StartFailed(_))),
                "a crash then a hang must report the crash"
            );
        }
        assert_eq!(state(&h), LocalLlmStateKind::Failed);
        assert!(
            retry_at(&h).is_none(),
            "a crash streak must not schedule a retry"
        );
        assert_eq!(spawn_count(&spawns), 6, "each start exits then hangs");
        assert!(matches!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::Failed(_))
        ));
        assert_eq!(spawn_count(&spawns), 6, "a sticky failure must not spawn");
    }

    #[cfg(unix)]
    #[test]
    fn retry_probe_that_crashes_spawns_one_child() {
        let (h, spawns) = real_backend_harness(
            "if [ \"$run\" -gt 3 ]; then exit 1; fi\nexec sleep 30",
            Duration::from_secs(2),
        );
        for _ in 0..MAX_FAILED_STARTS {
            assert_eq!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::StartTimeout)
            );
        }
        assert_eq!(spawn_count(&spawns), 3);
        expire_cooldown(&h);
        let probe = h.manager.ensure_started(&h.settings);
        assert!(
            matches!(probe, Err(LocalLlmError::StartFailed(_))),
            "a probe crash must report the crash"
        );
        assert_eq!(
            spawn_count(&spawns),
            4,
            "a retry must spawn at most one child"
        );
        assert_eq!(state(&h), LocalLlmStateKind::Failed);
        assert!(retry_at(&h).is_none(), "a probe crash must not re-arm");
    }

    #[cfg(unix)]
    #[test]
    fn retry_probe_that_hangs_spawns_one_child() {
        let (h, spawns) = real_backend_harness("exec sleep 30", Duration::from_secs(2));
        for _ in 0..MAX_FAILED_STARTS {
            assert_eq!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::StartTimeout),
                "a hang must report a start timeout"
            );
        }
        assert_eq!(
            spawn_count(&spawns),
            3,
            "a timed-out start must not spawn again"
        );
        expire_cooldown(&h);
        assert_eq!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::StartTimeout)
        );
        assert_eq!(
            spawn_count(&spawns),
            4,
            "a retry must spawn at most one child"
        );
        assert!(retry_at(&h).is_some(), "a probe timeout must re-arm");
    }

    #[test]
    fn retry_probe_timeout_rearms_without_a_second_spawn() {
        let h = harness();
        fail_starts(&h, &three_timeouts());
        expire_cooldown(&h);
        fail_starts(&h, &[LocalLlmError::StartTimeout]);
        assert_eq!(lock(&h.fake).loads, MAX_FAILED_STARTS + 1);
        assert_eq!(
            state(&h),
            LocalLlmStateKind::Failed,
            "a probe timeout must fail the manager again"
        );
        let at = retry_at(&h).expect("a probe timeout must schedule a fresh retry");
        assert!(at > Instant::now(), "the fresh retry time lies ahead");
        assert!(matches!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::Failed(_))
        ));
        assert_eq!(
            lock(&h.fake).loads,
            MAX_FAILED_STARTS + 1,
            "no second spawn in the same cooldown"
        );
    }

    #[test]
    fn retry_three_crashes_never_get_a_retry_time() {
        let h = harness();
        fail_starts(&h, &vec![LocalLlmError::StartFailed("boom".into()); 3]);
        assert_eq!(state(&h), LocalLlmStateKind::Failed);
        assert!(
            retry_at(&h).is_none(),
            "a crash streak must not schedule a retry"
        );
        assert!(matches!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::Failed(_))
        ));
        assert_eq!(
            lock(&h.fake).loads,
            3,
            "a crash streak must not spawn again"
        );
    }

    #[test]
    fn retry_probe_crash_after_timeouts_is_sticky() {
        let h = harness();
        fail_starts(&h, &three_timeouts());
        expire_cooldown(&h);
        fail_starts(&h, &[LocalLlmError::StartFailed("boom".into())]);
        assert_eq!(
            state(&h),
            LocalLlmStateKind::Failed,
            "a probe crash must fail the manager again after one start"
        );
        assert!(
            retry_at(&h).is_none(),
            "a probe crash must not schedule a retry"
        );
        assert!(matches!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::Failed(_))
        ));
        assert_eq!(lock(&h.fake).loads, MAX_FAILED_STARTS + 1);
    }

    #[test]
    fn retry_the_last_strike_decides_in_a_mixed_streak() {
        let crash = LocalLlmError::StartFailed("boom".into());
        let timeout = LocalLlmError::StartTimeout;
        let h = harness();
        fail_starts(&h, &[timeout.clone(), timeout.clone(), crash.clone()]);
        assert!(retry_at(&h).is_none(), "a final crash is sticky");
        let h = harness();
        fail_starts(&h, &[crash.clone(), crash, timeout]);
        assert!(retry_at(&h).is_some(), "a final timeout earns a retry");
    }

    #[test]
    fn retry_reset_failure_clears_the_timer() {
        let h = harness();
        fail_starts(&h, &three_timeouts());
        assert!(retry_at(&h).is_some());
        h.manager.reset_failure();
        assert!(retry_at(&h).is_none(), "reset must clear the retry timer");
        assert_eq!(lock(&h.manager.inner).failed_starts, 0);
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
    }

    #[cfg(unix)]
    #[test]
    fn fix2_swap_during_hash_cannot_cache_another_files_result() {
        use std::io::{Read, Write};
        let mut h = harness();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&h.model_file)
            .unwrap()
            .write_all(&[255])
            .unwrap();
        let replacement = h.model_file.with_extension("replacement");
        let saved = h.model_file.with_extension("saved");
        std::fs::File::create(&replacement)
            .unwrap()
            .set_len(std::fs::metadata(&h.model_file).unwrap().len())
            .unwrap();
        let swap = replacement.clone();
        let original = saved.clone();
        Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(move |path, _, _| {
            let before = std::fs::metadata(path).unwrap().modified().unwrap();
            std::fs::rename(path, &original).unwrap();
            std::fs::rename(&swap, path).unwrap();
            let mut reader = std::fs::File::open(path).unwrap();
            let mut byte = [255];
            reader.read_exact(&mut byte).unwrap();
            std::fs::rename(path, &swap).unwrap();
            std::fs::rename(&original, path).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(before)
                .unwrap();
            assert!(reader.metadata().unwrap().is_file());
            Ok(byte == [0])
        });
        assert!(
            matches!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::ModelRead(_))
            ),
            "rename swap must not associate another file's hash with the original identity"
        );
        assert!(lock(&h.manager.verification.state).cache.is_empty());
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn fix1_production_constructor_verifies_current_gguf_bytes() {
        let h = harness();
        let manager = LocalLlmManager::new(
            Box::new(FakeBackend(h.fake.clone())),
            Some(
                h.model_file
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf(),
            ),
            ManagerHooks {
                idle_inputs: Box::new(|| IdleInputs {
                    timeout: ModelUnloadTimeout::Never,
                    recording: false,
                }),
                on_status: Box::new(|_| {}),
            },
            ManagerConfig {
                start_timeout: Duration::from_secs(10),
                idle_tick: Duration::from_secs(10),
                retry_cooldown: Duration::from_secs(300),
            },
        );
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.gguf");
        std::fs::write(&path, b"WRONG!").unwrap();
        let entry = ModelEntry {
            size_bytes: 6,
            sha256: "efa1f375d76194fa51a3556a97e641e61685f914d446979da50a551a4333ffd7",
            ..registry::MODELS[0]
        };
        assert_eq!(
            (manager.verify)(&path, &entry, &|| false),
            Ok(false),
            "production constructor must verify current GGUF bytes"
        );
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn fix1_metadata_read_error_replaces_stale_corruption_guidance() {
        let mut h = harness();
        Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(|_, _, _| Ok(false));
        assert!(matches!(
            h.manager.ensure_started(&h.settings),
            Err(LocalLlmError::ModelCorrupt(_))
        ));
        std::fs::remove_file(&h.model_file).unwrap();
        let parent = h.model_file.parent().unwrap();
        std::fs::remove_dir(parent).unwrap();
        std::fs::write(parent, b"not a directory").unwrap();
        assert!(
            matches!(
                h.manager.ensure_started(&h.settings),
                Err(LocalLlmError::ModelRead(_))
            ),
            "metadata read errors must not be classified as corruption"
        );
        assert!(
            !h.manager.status().error.unwrap().contains("delete"),
            "read errors must replace stale corruption guidance"
        );
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn fix1_successful_redownload_clears_only_its_stale_status() {
        let mut h = harness();
        Arc::get_mut(&mut h.manager).unwrap().verify = Arc::new(|_, _, _| Ok(false));
        assert!(h.manager.ensure_started(&h.settings).is_err());
        h.manager.clear_download_error("another-model");
        assert!(h.manager.status().error.is_some());
        h.manager.clear_download_error("s1-mini-q4km");
        assert!(
            h.manager.status().error.is_none(),
            "successful re-download must clear stale corruption guidance"
        );
    }

    #[test]
    fn followup_model_errors_carry_entry_id() {
        let h = harness();
        let entry = registry::find("s1-mini-q4km").unwrap();
        std::fs::remove_file(&h.model_file).unwrap();
        assert_eq!(
            h.manager.model_file(entry),
            Err(LocalLlmError::ModelMissing(entry.id.into())),
            "missing model must identify the registry entry"
        );
        std::fs::File::create(&h.model_file).unwrap();
        assert_eq!(
            h.manager.model_file(entry),
            Err(LocalLlmError::ModelCorrupt(entry.id.into())),
            "corrupt model must identify the registry entry"
        );
    }

    fn state(h: &Harness) -> LocalLlmStateKind {
        h.manager.status().state
    }

    #[test]
    fn warm_up_reaches_ready_without_blocking() {
        let h = harness();
        lock(&h.fake).start_delay = Duration::from_millis(200);
        let t0 = Instant::now();
        h.manager.warm_up(&h.settings);
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "warm_up must not block"
        );
        for _ in 0..100 {
            if state(&h) == LocalLlmStateKind::Ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state(&h), LocalLlmStateKind::Ready);
        let kinds: Vec<_> = lock(&h.events).iter().map(|s| s.state).collect();
        assert_eq!(
            kinds,
            vec![LocalLlmStateKind::Starting, LocalLlmStateKind::Ready]
        );
    }

    #[test]
    fn process_returns_model_output_and_reuses_the_server() {
        let h = harness();
        assert_eq!(
            h.manager
                .process("hello there friend", &h.settings)
                .unwrap(),
            "hello there friend"
        );
        assert_eq!(
            h.manager.process("second one here", &h.settings).unwrap(),
            "second one here"
        );
        let s = lock(&h.fake);
        assert_eq!(s.loads, 1);
        assert_eq!(
            s.requests[0].messages[1].content,
            "[Styling: semi-formal] [Structure: prose] [Context: general]\nhello there friend"
        );
    }

    #[test]
    fn idle_unload_fires_at_the_timeout() {
        let h = harness();
        h.manager
            .process("hello there friend", &h.settings)
            .unwrap();
        let t0 = Instant::now();
        h.manager.idle_tick(t0 + Duration::from_secs(100));
        assert_eq!(state(&h), LocalLlmStateKind::Ready);
        h.manager.idle_tick(t0 + Duration::from_secs(121));
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        assert_eq!(lock(&h.fake).unloads, 1);
    }

    #[test]
    fn immediately_stops_after_each_process() {
        let mut h = harness();
        h.settings.model_unload_timeout = ModelUnloadTimeout::Immediately;
        h.manager
            .process("hello there friend", &h.settings)
            .unwrap();
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        h.manager.process("hello there again", &h.settings).unwrap();
        assert_eq!(lock(&h.fake).loads, 2);
    }

    #[test]
    fn never_keeps_the_server_loaded() {
        let h = harness();
        *lock(&h.timeout) = ModelUnloadTimeout::Never;
        h.manager
            .process("hello there friend", &h.settings)
            .unwrap();
        h.manager
            .idle_tick(Instant::now() + Duration::from_secs(36_000));
        assert_eq!(state(&h), LocalLlmStateKind::Ready);
    }

    #[test]
    fn timeout_changed_while_loaded_takes_effect_on_the_next_tick() {
        let h = harness();
        *lock(&h.timeout) = ModelUnloadTimeout::Never;
        h.manager
            .process("hello there friend", &h.settings)
            .unwrap();
        let later = Instant::now() + Duration::from_secs(3600);
        h.manager.idle_tick(later);
        assert_eq!(state(&h), LocalLlmStateKind::Ready);
        *lock(&h.timeout) = ModelUnloadTimeout::Min2;
        h.manager.idle_tick(later);
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
    }

    #[test]
    fn a_recording_longer_than_the_timeout_keeps_the_warm_server() {
        let h = harness();
        h.manager.ensure_started(&h.settings).unwrap();
        h.recording.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        h.manager.idle_tick(t0 + Duration::from_secs(300));
        assert_eq!(
            state(&h),
            LocalLlmStateKind::Ready,
            "never unload mid-recording"
        );
        h.recording.store(false, Ordering::SeqCst);
        h.manager.idle_tick(t0 + Duration::from_secs(360));
        assert_eq!(
            state(&h),
            LocalLlmStateKind::Ready,
            "idle time counts from the recording"
        );
        h.manager.idle_tick(t0 + Duration::from_secs(421));
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
    }

    #[test]
    fn unexpected_exit_returns_to_unloaded_then_restarts() {
        let h = harness();
        h.manager
            .process("hello there friend", &h.settings)
            .unwrap();
        lock(&h.fake).alive = false;
        h.manager.idle_tick(Instant::now());
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        assert_eq!(
            h.manager.process("hello there again", &h.settings).unwrap(),
            "hello there again"
        );
        assert_eq!(lock(&h.fake).loads, 2);
        lock(&h.fake).alive = false;
        h.manager.process("and a third time", &h.settings).unwrap();
        assert_eq!(
            lock(&h.fake).loads,
            3,
            "a dead server found by process is restarted too"
        );
    }

    #[test]
    fn three_failed_starts_enter_failed_and_reset_clears_it() {
        let h = harness();
        for _ in 0..3 {
            lock(&h.fake)
                .start_results
                .push_back(Err(LocalLlmError::StartFailed("boom".into())));
        }
        for _ in 0..3 {
            assert!(h
                .manager
                .process("hello there friend", &h.settings)
                .is_err());
        }
        assert_eq!(state(&h), LocalLlmStateKind::Failed);
        assert!(matches!(
            h.manager.process("hello there friend", &h.settings),
            Err(LocalLlmError::Failed(_))
        ));
        assert_eq!(lock(&h.fake).loads, 3, "Failed must not try a fourth start");
        h.manager.reset_failure();
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        assert!(h.manager.process("hello there friend", &h.settings).is_ok());
        assert_eq!(lock(&h.fake).loads, 4);
    }

    #[test]
    fn a_call_waiting_on_a_failing_start_gets_its_error_without_a_second_start() {
        let h = harness();
        let failure = LocalLlmError::StartTimeout;
        {
            let mut s = lock(&h.fake);
            s.start_delay = Duration::from_millis(300);
            s.start_results.push_back(Err(failure.clone()));
        }
        h.manager.warm_up(&h.settings);
        std::thread::sleep(Duration::from_millis(50));
        let r = h.manager.process("hello there friend", &h.settings);
        assert_eq!(r, Err(failure));
        assert_eq!(lock(&h.fake).loads, 1);
    }

    #[test]
    fn shutdown_while_starting_returns_promptly_and_blocks_new_work() {
        let h = harness();
        lock(&h.fake).block_until_cancel = true;
        h.manager.warm_up(&h.settings);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(state(&h), LocalLlmStateKind::Starting);
        let t0 = Instant::now();
        h.manager.shutdown();
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "exit must not wait out the start deadline"
        );
        assert_ne!(state(&h), LocalLlmStateKind::Ready);
        assert!(matches!(
            h.manager.process("hello there friend", &h.settings),
            Err(LocalLlmError::Failed(_))
        ));
    }

    #[test]
    fn model_file_removed_while_ready_stops_the_server_and_errors() {
        let h = harness();
        h.manager
            .process("hello there friend", &h.settings)
            .unwrap();
        std::fs::remove_file(&h.model_file).unwrap();
        assert!(matches!(
            h.manager.process("hello there friend", &h.settings),
            Err(LocalLlmError::ModelMissing(_))
        ));
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        assert_eq!(lock(&h.fake).unloads, 1);
    }

    #[test]
    fn wrong_size_model_is_corrupt_and_never_started() {
        let h = harness();
        std::fs::File::create(&h.model_file)
            .unwrap()
            .set_len(10)
            .unwrap();
        assert!(matches!(
            h.manager.process("hello there friend", &h.settings),
            Err(LocalLlmError::ModelCorrupt(_))
        ));
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn no_selected_model_is_model_missing() {
        let mut h = harness();
        h.settings.local_llm_model_id = None;
        assert!(matches!(
            h.manager.process("hello there friend", &h.settings),
            Err(LocalLlmError::ModelMissing(_))
        ));
        assert_eq!(lock(&h.fake).loads, 0);
    }

    #[test]
    fn a_timed_out_request_stops_the_server() {
        let h = harness();
        lock(&h.fake)
            .replies
            .push_back(Err(LocalLlmError::RequestTimeout));
        assert_eq!(
            h.manager.process("hello there friend", &h.settings),
            Err(LocalLlmError::RequestTimeout)
        );
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
    }

    #[test]
    fn a_rejected_output_keeps_the_server_ready() {
        let h = harness();
        lock(&h.fake).replies.push_back(Ok("one".into()));
        assert!(matches!(
            h.manager
                .process("one two three four five six", &h.settings),
            Err(LocalLlmError::BadOutput(_))
        ));
        assert_eq!(state(&h), LocalLlmStateKind::Ready);
    }

    #[test]
    fn long_transcripts_are_processed_chunk_by_chunk_in_order() {
        let h = harness();
        let sentence = "this is sentence number {} and it carries a few more words.";
        let transcript: Vec<String> = (0..200)
            .map(|i| sentence.replace("{}", &i.to_string()))
            .collect();
        let transcript = transcript.join(" ");
        let out = h.manager.process(&transcript, &h.settings).unwrap();
        assert_eq!(out, transcript);
        let s = lock(&h.fake);
        assert!(s.requests.len() > 1);
        for r in &s.requests {
            let chunk = r.messages[1].content.split_once('\n').unwrap().1;
            assert!(prompt::est_tokens(chunk) <= prompt::CHUNK_TOKEN_LIMIT);
            assert_eq!(r.max_tokens, prompt::max_tokens(chunk, 4096));
        }
    }

    #[test]
    fn shutdown_during_a_request_returns_within_three_seconds() {
        let h = harness();
        h.manager.ensure_started(&h.settings).unwrap();
        let (started, release) = {
            let mut fake = lock(&h.fake);
            fake.request_block = true;
            (fake.request_started.clone(), fake.request_release.clone())
        };
        let manager = h.manager.clone();
        let settings = h.settings.clone();
        let request = std::thread::spawn(move || manager.process("hello there friend", &settings));
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started.load(Ordering::SeqCst), "request must be in flight");
        let t0 = Instant::now();
        h.manager.shutdown();
        let elapsed = t0.elapsed();
        release.store(true, Ordering::SeqCst);
        assert!(request.join().unwrap().is_err());
        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown took {elapsed:?}"
        );
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
        assert!(!lock(&h.fake).alive);
        assert!(matches!(
            h.manager.process("hello again", &h.settings),
            Err(LocalLlmError::Failed(_))
        ));
    }

    #[test]
    fn shutdown_returns_after_three_seconds_when_a_request_ignores_kill() {
        let h = harness();
        h.manager.ensure_started(&h.settings).unwrap();
        let (started, release, killed) = {
            let mut fake = lock(&h.fake);
            fake.request_block = true;
            fake.request_ignores_kill = true;
            (
                fake.request_started.clone(),
                fake.request_release.clone(),
                fake.request_killed.clone(),
            )
        };
        let manager = h.manager.clone();
        let settings = h.settings.clone();
        let request = std::thread::spawn(move || manager.process("hello there friend", &settings));
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started.load(Ordering::SeqCst), "request must be in flight");
        let (returned, result) = std::sync::mpsc::channel();
        let manager = h.manager.clone();
        let shutdown = std::thread::spawn(move || {
            let t0 = Instant::now();
            manager.shutdown();
            returned.send(t0.elapsed()).unwrap();
        });
        let elapsed = result.recv_timeout(Duration::from_secs(4));
        let request_still_running = !request.is_finished();
        release.store(true, Ordering::SeqCst);
        assert!(request.join().unwrap().is_err());
        shutdown.join().unwrap();
        let elapsed =
            elapsed.expect("shutdown must return within 4 s even when the request ignores kill");
        eprintln!("shutdown cap observed: {elapsed:?}");
        assert!(
            (Duration::from_secs(3)..Duration::from_millis(3500)).contains(&elapsed),
            "shutdown must return within [3.0 s, 3.5 s), observed {elapsed:?}"
        );
        assert!(
            killed.load(Ordering::SeqCst),
            "kill switch must have been triggered"
        );
        assert!(
            request_still_running,
            "request must remain blocked after shutdown returns"
        );
        assert_eq!(state(&h), LocalLlmStateKind::Unloaded);
    }

    pub(crate) fn plain_harness() -> Harness {
        plain_harness_for("quill-0.8b-q4km")
    }

    fn plain_harness_for(model_id: &str) -> Harness {
        let mut h = harness();
        let entry = registry::find(model_id).unwrap();
        let root = h.model_file.parent().unwrap().parent().unwrap();
        let file = registry::model_path(root, entry);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::File::create(&file)
            .unwrap()
            .set_len(entry.size_bytes)
            .unwrap();
        h.settings.local_llm_model_id = Some(entry.id.into());
        h.settings.post_process_selected_prompt_id = Some("plain".into());
        h.settings.post_process_prompts = vec![crate::settings::LLMPrompt {
            id: "plain".into(),
            name: "Plain".into(),
            prompt: "Return only the transcript. ${output}".into(),
        }];
        h
    }

    #[test]
    fn plain_manager_builds_requests_and_preserves_shared_guards() {
        let h = plain_harness();
        let input = "hello there friend";
        assert_eq!(h.manager.process(input, &h.settings).unwrap(), input);
        let fake = h.fake.lock().unwrap();
        let request = &fake.requests[0];
        assert_eq!(request.messages[0].content, "Return only the transcript. ");
        assert_eq!(request.messages[1].content, input);
        assert_eq!(request.max_tokens, prompt::max_tokens(input, 4096));
        assert_eq!(request.timeout, prompt::request_timeout(input));
        drop(fake);
        let multiline = "hello there\nfriend ${output}";
        assert_eq!(
            h.manager.process(multiline, &h.settings).unwrap(),
            multiline
        );
        let styled = "[Styling: casual]\nhello ${output}";
        h.fake.lock().unwrap().replies.push_back(Ok(styled.into()));
        assert_eq!(h.manager.process(styled, &h.settings).unwrap(), styled);
        let fake = h.fake.lock().unwrap();
        assert_eq!(fake.requests[1].messages[1].content, multiline);
        assert_eq!(fake.requests[2].messages[1].content, styled);
        let short_requests = fake.requests.len();
        drop(fake);
        let long = "hello there friend. ".repeat(300);
        h.manager.process(&long, &h.settings).unwrap();
        let system = prompt::system_prompt(PromptStyle::PlainSystemPrompt, &h.settings).unwrap();
        let fake = h.fake.lock().unwrap();
        assert!(fake.requests.len() > short_requests + 1);
        for request in &fake.requests {
            assert!(
                prompt::est_tokens(&request.messages[1].content)
                    <= prompt::chunk_budget(&system, "")
            );
        }
        drop(fake);
        for rejected in [String::new(), "one".to_string()] {
            h.fake.lock().unwrap().replies.push_back(Ok(rejected));
            assert!(h
                .manager
                .process("one two three four five", &h.settings)
                .is_err());
        }
    }

    #[derive(serde::Deserialize)]
    struct ParityFixture {
        ctx: u32,
        system: String,
        server_args_without_host: Vec<String>,
        body_flags: serde_json::Value,
        cases: Vec<ParityCase>,
    }

    #[derive(serde::Deserialize)]
    struct ParityCase {
        id: String,
        input: String,
        requests: Vec<ParityRequest>,
        old_chunks: usize,
        old_max_tokens: Vec<u32>,
        s1_gold_tokens: u32,
        muesli_gold_tokens: u32,
    }

    #[derive(serde::Deserialize)]
    struct ParityRequest {
        user: String,
        max_tokens: u32,
        timeout_ms: u64,
    }

    fn request_estimate(request: &GenerateRequest) -> u32 {
        prompt::est_tokens(&request.messages[1].content)
    }

    /// `testdata/muesli_parity.json` is what the evaluation's `handy` variant for
    /// `muesli-2b-q4km` (eval/scripts/cleanup_port.py) sends for every eval sample: system
    /// prompt, chunks, `max_tokens`, timeout and request flags. The app must send the same.
    #[test]
    fn muesli_requests_match_the_evaluated_handy_variant() {
        let fixture: ParityFixture =
            serde_json::from_str(include_str!("testdata/muesli_parity.json")).unwrap();
        let entry = registry::find("muesli-2b-q4km").unwrap();
        assert_eq!(
            entry.prompt_style,
            PromptStyle::PlainSystemPrompt,
            "muesli must ship with the prompt style it was evaluated with"
        );
        assert_eq!(entry.default_ctx, fixture.ctx);

        let mut h = plain_harness_for("muesli-2b-q4km");
        let defaults = crate::settings::get_default_settings();
        h.settings.post_process_selected_prompt_id =
            Some(defaults.post_process_prompts[0].id.clone());
        h.settings.post_process_prompts = defaults.post_process_prompts;
        assert_eq!(
            prompt::system_prompt(entry.prompt_style, &h.settings).unwrap(),
            fixture.system
        );

        let args: Vec<String> = crate::local_llm::llama_server::server_args(
            std::path::Path::new("/m/model.gguf"),
            std::path::Path::new("/private/llm.sock"),
            h.settings.local_llm_threads,
            entry.default_ctx,
        )
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect();
        let host = args.iter().position(|arg| arg == "--host").unwrap();
        let mut without_host = args.clone();
        without_host.drain(host..host + 2);
        assert_eq!(without_host, fixture.server_args_without_host);

        let mut sent = 0;
        for case in &fixture.cases {
            let before = h.fake.lock().unwrap().requests.len();
            assert_eq!(
                h.manager.process(&case.input, &h.settings).unwrap(),
                case.requests
                    .iter()
                    .map(|r| r.user.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                "{}",
                case.id
            );
            let fake = h.fake.lock().unwrap();
            let requests = &fake.requests[before..];
            assert_eq!(requests.len(), case.requests.len(), "{}", case.id);
            for (request, expected) in requests.iter().zip(&case.requests) {
                let mut body = fixture.body_flags.clone();
                body["messages"] = serde_json::json!([
                    {"role": "system", "content": fixture.system},
                    {"role": "user", "content": expected.user},
                ]);
                body["max_tokens"] = expected.max_tokens.into();
                assert_eq!(
                    crate::local_llm::llama_server::chat_body(request),
                    body,
                    "{}",
                    case.id
                );
                assert_eq!(
                    request.timeout,
                    Duration::from_millis(expected.timeout_ms),
                    "{}",
                    case.id
                );
                sent += 1;
            }
            let replies: u32 = requests.iter().map(|r| r.max_tokens).sum();
            assert!(
                replies >= case.s1_gold_tokens.max(case.muesli_gold_tokens),
                "the reply budget must cover the real gold tokens of {}",
                case.id
            );
            for request in requests {
                // Measured worst case: real Latin tokens are at most 1.57x the estimate; the
                // real default system prompt is 232 tokens; the chat template adds 16.
                let prompt = 232 + 16 + request_estimate(request) * 157 / 100;
                assert!(
                    prompt + request.max_tokens <= fixture.ctx,
                    "prompt plus reply must fit the context for {}",
                    case.id
                );
            }
            let latin_only = case.input.chars().all(|c| {
                matches!(c, '\u{0}'..='\u{24F}' | '\u{2000}'..='\u{206F}' | '\u{20A0}'..='\u{21FF}')
            });
            if latin_only {
                assert_eq!(
                    case.requests.len(),
                    case.old_chunks,
                    "Latin-script chunking must match the evaluated request path: {}",
                    case.id
                );
                let tokens: Vec<u32> = case.requests.iter().map(|r| r.max_tokens).collect();
                assert_eq!(
                    tokens, case.old_max_tokens,
                    "Latin-script max_tokens must match the evaluated request path: {}",
                    case.id
                );
            }
        }
        assert_eq!(
            sent,
            fixture
                .cases
                .iter()
                .map(|c| c.requests.len())
                .sum::<usize>()
        );
        assert!(fixture.cases.iter().any(|c| c.requests.len() > 1));
        assert_eq!(
            fixture
                .cases
                .iter()
                .filter(|c| c.requests.len() != c.old_chunks)
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["s119", "v2-023", "v2-028"],
            "only these Indic samples re-chunk against the evaluated request path"
        );
    }

    #[test]
    fn plain_manager_sends_no_request_for_missing_prompt() {
        let mut h = plain_harness();
        h.settings.post_process_selected_prompt_id = None;
        assert!(h
            .manager
            .process("hello there friend", &h.settings)
            .is_err());
        assert!(h.fake.lock().unwrap().requests.is_empty());
    }
}
