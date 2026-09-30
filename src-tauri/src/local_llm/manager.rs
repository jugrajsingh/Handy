//! Lifecycle of the local model: warm-up, serial requests, idle unload,
//! crash restart and the failed-start policy. One mutex serializes it all.

use super::backend::{BackendOpts, GenerateRequest, KillSwitch, TextModelBackend};
use super::registry::{self, ModelEntry};
use super::{prompt, LocalLlmError};
use crate::settings::{AppSettings, ModelUnloadTimeout};
use log::{info, warn};
use serde::Serialize;
use specta::Type;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// Consecutive failed starts before the manager stops trying.
pub const MAX_FAILED_STARTS: u32 = 3;

/// Timings, overridable in tests.
#[derive(Debug, Clone, Copy)]
pub struct ManagerConfig {
    pub start_timeout: Duration,
    pub idle_tick: Duration,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(10),
            idle_tick: Duration::from_secs(10),
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
}

pub struct LocalLlmManager {
    inner: Mutex<Inner>,
    status: Mutex<LocalLlmStatus>,
    cancel: Arc<AtomicBool>,
    kill_switch: Arc<dyn KillSwitch>,
    models_root: Option<PathBuf>,
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
            }),
            status: Mutex::new(LocalLlmStatus {
                state: LocalLlmStateKind::Unloaded,
                model_id: None,
                error: None,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            kill_switch,
            models_root,
            hooks,
            config,
        }
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
        let this = Arc::clone(self);
        let settings = settings.clone();
        let spawned = std::thread::Builder::new()
            .name("local-llm-warmup".into())
            .spawn(move || {
                if let Err(e) = this.ensure_started(&settings) {
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
            let mut inner = lock(&self.inner);
            self.ready_locked(&mut inner, settings, call_started)
                .map(|_| ())
        })
    }

    /// Serial cleanup on a std thread; blocks the caller, so use spawn_blocking in async code.
    pub fn process(
        &self,
        transcript: &str,
        settings: &AppSettings,
    ) -> Result<String, LocalLlmError> {
        let call_started = Instant::now();
        on_std_thread(|| {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(LocalLlmError::Failed("shutting down".into()));
            }
            let mut inner = lock(&self.inner);
            let entry = self.ready_locked(&mut inner, settings, call_started)?;
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
        on_std_thread(|| {
            let mut inner = lock(&self.inner);
            self.stop_locked(&mut inner);
        });
    }

    /// Kills on a std thread, then waits at most 3 s for the request lock; use spawn_blocking in async code.
    pub fn shutdown(&self) {
        on_std_thread(|| {
            self.cancel.store(true, Ordering::SeqCst);
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
            .ok_or_else(|| LocalLlmError::ModelMissing("app data directory unknown".into()))?;
        let path = registry::model_path(root, entry);
        let meta = std::fs::metadata(&path)
            .map_err(|_| LocalLlmError::ModelMissing(path.display().to_string()))?;
        if meta.len() != entry.size_bytes {
            return Err(LocalLlmError::ModelCorrupt(format!(
                "{} is {} bytes, expected {}",
                path.display(),
                meta.len(),
                entry.size_bytes
            )));
        }
        Ok(path)
    }

    fn ready_locked(
        &self,
        inner: &mut Inner,
        settings: &AppSettings,
        call_started: Instant,
    ) -> Result<&'static ModelEntry, LocalLlmError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(LocalLlmError::Failed("shutting down".into()));
        }
        if let LocalLlmState::Failed { reason } = &inner.state {
            return Err(LocalLlmError::Failed(reason.clone()));
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
                return Err(e);
            }
        };
        if inner.state == LocalLlmState::Ready {
            if inner.loaded == Some(entry) && inner.backend.is_alive() {
                return Ok(entry);
            }
            warn!("local-llm: llama-server gone or model changed; restarting");
            self.stop_locked(inner);
        }
        self.start_locked(inner, entry, path, settings)?;
        Ok(entry)
    }

    fn start_locked(
        &self,
        inner: &mut Inner,
        entry: &'static ModelEntry,
        model_path: PathBuf,
        settings: &AppSettings,
    ) -> Result<(), LocalLlmError> {
        self.set_state_for(inner, LocalLlmState::Starting, Some(entry.id), None);
        let opts = BackendOpts {
            model_path,
            threads: settings.local_llm_threads.max(1),
            ctx: entry.default_ctx,
            start_timeout: self.config.start_timeout,
            cancel: Arc::clone(&self.cancel),
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
    let control = prompt::control_line_for(settings);
    let budget = prompt::chunk_budget(prompt::system_prompt(entry.prompt_style), &control);
    let mut outputs = Vec::new();
    for chunk in prompt::chunk_transcript(transcript, budget) {
        let req = GenerateRequest {
            messages: prompt::build_messages(entry.prompt_style, &control, &chunk),
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
                    if release.load(Ordering::SeqCst)
                        || (!ignores_kill && killed.load(Ordering::SeqCst))
                    {
                        return Err(LocalLlmError::Transport("request interrupted".into()));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(LocalLlmError::RequestTimeout);
            }
            let mut s = lock(&self.0);
            s.requests.push(req.clone());
            if let Some(reply) = s.replies.pop_front() {
                return reply;
            }
            let user = &req.messages[1].content;
            Ok(user
                .split_once('\n')
                .map(|(_, t)| t)
                .unwrap_or(user)
                .to_string())
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
        let manager = Arc::new(LocalLlmManager::new(
            Box::new(FakeBackend(fake.clone())),
            Some(dir.path().to_path_buf()),
            hooks,
            ManagerConfig {
                start_timeout: Duration::from_secs(2),
                idle_tick: Duration::from_secs(3600),
            },
        ));
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
        {
            let mut s = lock(&h.fake);
            s.start_delay = Duration::from_millis(300);
            s.start_results.push_back(Err(LocalLlmError::StartTimeout));
        }
        h.manager.warm_up(&h.settings);
        std::thread::sleep(Duration::from_millis(50));
        let r = h.manager.process("hello there friend", &h.settings);
        assert_eq!(r, Err(LocalLlmError::StartTimeout));
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
}
