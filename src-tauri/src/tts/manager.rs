use super::{
    backend::{Control, LoadSpec, Transport, WorkerBackend, WorkerReady},
    TtsError,
};
use handy_tts_core::{
    catalog,
    device::{self, Device, DeviceError, Preparation, Ratings, RunOn},
    download,
    lifecycle::{Lifecycle, State, UnloadAfter},
    protocol::Pcm,
    selection::Selection,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex, TryLockError,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct ManagerConfig {
    pub root: PathBuf,
    pub binary: PathBuf,
    pub cache: PathBuf,
    pub home: PathBuf,
    pub ratings: Ratings,
    pub available: Vec<Device>,
    pub run_on: BTreeMap<String, RunOn>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerKey {
    model: String,
    voice: String,
    device: Device,
}
#[derive(Clone, serde::Serialize, specta::Type)]
pub struct ManagerSnapshot {
    pub state: State,
    pub preparation: Preparation,
    pub ready: Option<WorkerReady>,
    pub fallback_from: Option<Device>,
}
pub(super) struct Checks {
    pub(super) memory: fn() -> Result<u64, TtsError>,
    pub(super) downloaded: fn(&Path, &catalog::ModelEntry) -> bool,
}
struct Inner<B: Transport> {
    backend: B,
    lifecycle: Lifecycle,
    key: Option<WorkerKey>,
    generation: u64,
    loaded_lanes: usize,
    config: ManagerConfig,
    last_failure: Option<(Instant, TtsError)>,
}
struct IdleWake<'a>(&'a mpsc::SyncSender<()>);
impl Drop for IdleWake<'_> {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}
pub struct TtsManager<B: Transport = WorkerBackend> {
    inner: Mutex<Inner<B>>,
    control: B::Control,
    checks: Checks,
    started: Instant,
    cached: Mutex<ManagerSnapshot>,
    cancelled_ids: Mutex<std::collections::BTreeSet<u64>>,
    lanes: std::sync::atomic::AtomicUsize,
    low_memory: std::sync::atomic::AtomicBool,
    requests: std::sync::atomic::AtomicUsize,
    interruption: AtomicU64,
    activity: AtomicU64,
    invalid: Arc<Mutex<BTreeMap<String, Vec<Device>>>>,
    idle_wake: mpsc::SyncSender<()>,
    idle_events: Mutex<Option<mpsc::Receiver<()>>>,
    status_changed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
impl TtsManager {
    pub fn fork(&self) -> Result<Arc<Self>, TtsError> {
        self.fork_with_backend(WorkerBackend::default())
    }
    pub fn new(config: ManagerConfig) -> Result<Arc<Self>, TtsError> {
        Self::with_backend(
            config,
            WorkerBackend::default(),
            Checks {
                memory: super::memory_available,
                downloaded: download::is_downloaded,
            },
        )
    }
}
impl<B: Transport> TtsManager<B> {
    pub(super) fn with_backend(
        config: ManagerConfig,
        backend: B,
        checks: Checks,
    ) -> Result<Arc<Self>, TtsError> {
        Self::with_policy(
            config,
            backend,
            checks,
            Arc::new(Mutex::new(BTreeMap::new())),
        )
    }
    fn with_policy(
        config: ManagerConfig,
        backend: B,
        checks: Checks,
        invalid: Arc<Mutex<BTreeMap<String, Vec<Device>>>>,
    ) -> Result<Arc<Self>, TtsError> {
        config
            .ratings
            .validate(&["pocket-en", "supertonic-3", "kokoro-82m-int8"])?;
        let control = backend.control();
        let (idle_wake, idle_events) = mpsc::sync_channel(1);
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                backend,
                lifecycle: Lifecycle::default(),
                key: None,
                generation: 0,
                loaded_lanes: 1,
                config,
                last_failure: None,
            }),
            control,
            idle_wake,
            idle_events: Mutex::new(Some(idle_events)),
            status_changed: Mutex::new(None),
            checks,
            started: Instant::now(),
            cancelled_ids: Mutex::default(),
            lanes: std::sync::atomic::AtomicUsize::new(1),
            low_memory: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
            interruption: AtomicU64::new(0),
            activity: AtomicU64::new(0),
            invalid,
            cached: Mutex::new(ManagerSnapshot {
                state: State::Unloaded,
                preparation: Preparation::Idle,
                ready: None,
                fallback_from: None,
            }),
        }))
    }
    pub fn npu_driver_too_old(&self, model: &str) -> bool {
        self.invalid
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(model)
            .is_some_and(|devices| devices.contains(&Device::Npu))
    }
    pub fn configuration(&self) -> ManagerConfig {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .config
            .clone()
    }
    fn fork_with_backend<C: Transport>(&self, backend: C) -> Result<Arc<TtsManager<C>>, TtsError> {
        TtsManager::with_policy(
            self.configuration(),
            backend,
            Checks {
                memory: self.checks.memory,
                downloaded: self.checks.downloaded,
            },
            self.invalid.clone(),
        )
    }
    pub fn ensure_cancellable(
        &self,
        selection: &Selection,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(), TtsError> {
        self.ensure_with(selection, || cancelled.load(Ordering::Acquire), true)
    }
    pub fn snapshot(&self) -> ManagerSnapshot {
        let mut snapshot = self
            .cached
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if snapshot.state == State::Starting {
            if let Some(progress) = self.control.preparation() {
                snapshot.preparation = progress;
            }
        }
        snapshot
    }
    pub fn status(&self) -> State {
        self.snapshot().state
    }
    fn publish(&self, state: State, preparation: Preparation) {
        let mut cached = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        cached.state = state;
        cached.preparation = preparation;
        if state != State::Ready {
            cached.ready = None;
            cached.fallback_from = None;
        }
        drop(cached);
        self.wake_idle();
        if let Some(notify) = self
            .status_changed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            notify();
        }
    }
    /// Installs the notification used to refresh the tray after status changes.
    pub fn on_status_change(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        *self
            .status_changed
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(notify);
    }
    /// Wakes the idle monitor to reschedule its unload deadline.
    pub fn wake_idle(&self) {
        let _ = self.idle_wake.try_send(());
    }
    pub fn downloaded(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        catalog::entry(id).is_ok_and(|entry| (self.checks.downloaded)(&inner.config.root, entry))
    }
    pub fn has_model(&self, model: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .key
            .as_ref()
            .is_some_and(|key| key.model == model)
    }
    pub fn cancel(&self, id: u64) {
        self.activity.fetch_add(1, Ordering::AcqRel);
        self.cancelled_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id);
        self.control.cancel(id);
    }
    fn cancelled(&self, id: u64) -> bool {
        self.cancelled_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&id)
    }
    pub(super) fn available_lanes(&self) -> usize {
        self.lanes.load(Ordering::Acquire)
    }
    pub(super) fn lower_lanes(&self) {
        self.low_memory.store(true, Ordering::Release);
        if self.lanes.swap(1, Ordering::AcqRel) > 1 {
            let mut inner = match self.inner.try_lock() {
                Ok(inner) => inner,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            if inner.backend.set_lanes(1).is_err() {
                self.control.kill();
            } else {
                inner.loaded_lanes = 1;
            }
        }
    }
    pub fn interrupt(&self) {
        self.interruption.fetch_add(1, Ordering::AcqRel);
        self.activity.fetch_add(1, Ordering::AcqRel);
        self.control.kill();
    }
    fn stop_backend(&self, inner: &mut Inner<B>) {
        inner.backend.stop();
        self.lanes.store(1, Ordering::Release);
        self.low_memory.store(false, Ordering::Release);
    }
    pub fn unload(&self) {
        self.interrupt();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let failed = inner.lifecycle.state == State::Failed;
        inner.lifecycle.stopping();
        self.publish(State::Stopping, Preparation::Idle);
        self.stop_backend(&mut inner);
        inner.key = None;
        inner.lifecycle.stopped();
        if failed {
            inner.lifecycle.state = State::Failed;
        }
        self.publish(inner.lifecycle.state, Preparation::Idle);
    }
    pub fn selected(&self) {
        self.unload();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.lifecycle.selected();
        inner.last_failure = None;
        self.publish(State::Unloaded, Preparation::Idle);
    }
    /// Clears Failed after selection while retaining a healthy loaded worker.
    pub fn retry_failed_selection(&self) {
        if self.status() == State::Failed {
            self.selected();
        }
    }
    pub fn set_run_on(&self, model: &str, choice: RunOn) -> Result<(), TtsError> {
        catalog::entry(model).map_err(|_| TtsError::Model)?;
        let active = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let active = inner.key.as_ref().is_some_and(|key| key.model == model);
            inner.config.run_on.insert(model.into(), choice);
            active
        };
        if active {
            self.selected();
        }
        Ok(())
    }
    pub fn ensure(&self, selection: &Selection) -> Result<(), TtsError> {
        self.ensure_with(selection, || false, false)
    }
    fn ensure_with(
        &self,
        selection: &Selection,
        cancelled: impl Fn() -> bool,
        discard_cancelled_ready: bool,
    ) -> Result<(), TtsError> {
        let _idle_wake = IdleWake(&self.idle_wake);
        let generation = self.interruption.load(Ordering::Acquire);
        self.activity.fetch_add(1, Ordering::AcqRel);
        let interrupted = || cancelled() || self.interruption.load(Ordering::Acquire) != generation;
        let call = Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if interrupted() {
            return Err(TtsError::Cancelled);
        }
        if let Some((failed, error)) = &inner.last_failure {
            if *failed >= call {
                return Err(error.clone());
            }
        }
        let entry = catalog::entry(&selection.model_id).map_err(|_| TtsError::Model)?;
        if !(self.checks.downloaded)(&inner.config.root, entry) {
            log::warn!(
                "TTS worker start refused: {}: {}",
                entry.id,
                TtsError::NotDownloaded
            );
            return Err(TtsError::NotDownloaded);
        }
        if !entry.voices.contains(&selection.voice) {
            return Err(TtsError::Voice);
        }
        if let Some(key) = &inner.key {
            if key.model == selection.model_id
                && key.voice == selection.voice
                && inner.backend.alive()
            {
                if self.low_memory.load(Ordering::Acquire) && inner.loaded_lanes > 1 {
                    inner.backend.set_lanes(1)?;
                    inner.loaded_lanes = 1;
                }
                inner.lifecycle.touch(self.started.elapsed());
                self.wake_idle();
                if interrupted() {
                    if discard_cancelled_ready {
                        self.stop_backend(&mut inner);
                        inner.key = None;
                        inner.lifecycle.stopped();
                        self.publish(State::Unloaded, Preparation::Idle);
                    }
                    return Err(TtsError::Cancelled);
                }
                return Ok(());
            }
        }
        if inner.key.is_some() {
            self.stop_backend(&mut inner);
            inner.key = None;
            inner.lifecycle.stopped();
            self.publish(State::Unloaded, Preparation::Idle);
        }
        let config = inner.config.clone();
        let choice = config.run_on.get(entry.id).copied().unwrap_or_default();
        let rating = config
            .ratings
            .models
            .get(entry.id)
            .ok_or(DeviceError::InvalidRatings)?;
        let invalid = self
            .invalid
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(entry.id)
            .cloned()
            .unwrap_or_default();
        let ordered = device::candidates(choice, &config.available, rating, &invalid)?;
        let first = ordered[0];
        let attempts: Vec<Device> = if choice == RunOn::Auto && ordered.len() > 1 {
            ordered.into_iter().take(2).collect()
        } else {
            vec![first, first]
        };
        let mut last = TtsError::Start;
        for device in attempts {
            if interrupted() {
                return Err(TtsError::Cancelled);
            }
            let available = (self.checks.memory)()?;
            if interrupted() {
                return Err(TtsError::Cancelled);
            }
            inner.lifecycle.begin_start(available).map_err(|_| {
                if available < handy_tts_core::memory::MIN_AVAILABLE_BYTES {
                    TtsError::Memory
                } else {
                    TtsError::Failed
                }
            })?;
            let graphs = rating.devices.get(&device).map_or(0, |r| r.placement.len()) as u32;
            self.publish(
                State::Starting,
                Preparation::Preparing {
                    device,
                    completed: 0,
                    total: graphs,
                    graph: String::new(),
                },
            );
            let root = config.root.join(entry.id);
            let cache = config.cache.join(entry.id).join(format!("{device:?}"));
            log::info!("TTS worker starting: {} on {device:?}", entry.id);
            let bytes = |value: Option<f64>| {
                value
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .map(|v| (v * 1024.0 * 1024.0) as u64)
            };
            let sizing = &rating
                .devices
                .get(&device)
                .ok_or(DeviceError::InvalidRatings)?
                .lanes;
            let lanes = if self.low_memory.load(Ordering::Acquire) {
                1
            } else if let (Some(base), Some(lane)) =
                (bytes(sizing.base_rss_mib), bytes(sizing.lane_rss_mib))
            {
                handy_tts_core::memory::lanes_for(available, base, lane, sizing.best_lanes)
            } else {
                1
            };
            inner.generation = inner.generation.wrapping_add(1);
            let result = inner.backend.start(&LoadSpec {
                binary: &config.binary,
                root: &root,
                entry,
                voice: &selection.voice,
                device,
                cache: &cache,
                home: &config.home,
                lanes,
            });
            if interrupted() && (discard_cancelled_ready || result.is_err()) {
                self.stop_backend(&mut inner);
                inner.key = None;
                inner.lifecycle.stopped();
                self.publish(State::Unloaded, Preparation::Idle);
                return Err(TtsError::Cancelled);
            }
            match result {
                Err(TtsError::Memory) => {
                    log::warn!(
                        "TTS worker start failed: {}: {}",
                        entry.id,
                        TtsError::Memory
                    );
                    self.stop_backend(&mut inner);
                    inner.key = None;
                    inner.lifecycle.stopped();
                    inner.last_failure = Some((Instant::now(), TtsError::Memory));
                    self.publish(
                        State::Unloaded,
                        Preparation::Failed {
                            device,
                            error: TtsError::Memory.to_string(),
                        },
                    );
                    return Err(TtsError::Memory);
                }
                Ok(ready) if ready.device == device && !ready.placement.is_empty() => {
                    let active_lanes = if self.low_memory.load(Ordering::Acquire) {
                        1
                    } else {
                        lanes
                    };
                    if active_lanes < lanes {
                        inner.backend.set_lanes(active_lanes)?;
                    }
                    inner.loaded_lanes = active_lanes;
                    self.lanes.store(active_lanes, Ordering::Release);
                    log::info!("TTS worker ready: {} on {device:?}", entry.id);
                    inner.lifecycle.ready(self.started.elapsed());
                    inner.key = Some(WorkerKey {
                        model: selection.model_id.clone(),
                        voice: selection.voice.clone(),
                        device,
                    });
                    inner.last_failure = None;
                    self.publish(State::Ready, Preparation::Ready { device });
                    let mut cached = self.cached.lock().unwrap_or_else(|p| p.into_inner());
                    cached.ready = Some(ready);
                    cached.fallback_from = (device != first).then_some(first);
                    drop(cached);
                    if interrupted() {
                        if discard_cancelled_ready {
                            self.stop_backend(&mut inner);
                            inner.key = None;
                            inner.lifecycle.stopped();
                            self.publish(State::Unloaded, Preparation::Idle);
                        }
                        return Err(TtsError::Cancelled);
                    }
                    return Ok(());
                }
                outcome => {
                    last = outcome.err().unwrap_or(TtsError::Protocol);
                    log::warn!(
                        "TTS worker start failed: {} on {device:?}: {last}",
                        entry.id
                    );
                    self.stop_backend(&mut inner);
                    inner.key = None;
                    if interrupted() {
                        inner.lifecycle.stopped();
                        self.publish(State::Unloaded, Preparation::Idle);
                        return Err(TtsError::Cancelled);
                    }
                    inner.lifecycle.start_failed();
                    if device == Device::Npu && last == TtsError::Device(DeviceError::CompileFailed)
                    {
                        self.invalid
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .entry(entry.id.into())
                            .or_default()
                            .push(device);
                        last = TtsError::Device(DeviceError::NpuDriverTooOld);
                    }
                    inner.last_failure = Some((Instant::now(), last.clone()));
                    self.publish(
                        inner.lifecycle.state,
                        Preparation::Failed {
                            device,
                            error: last.to_string(),
                        },
                    );
                    if inner.lifecycle.state == State::Failed {
                        break;
                    }
                }
            }
        }
        Err(last)
    }
    pub fn prepare(
        self: &Arc<Self>,
        selection: Selection,
    ) -> std::thread::JoinHandle<Result<(), TtsError>> {
        let manager = self.clone();
        std::thread::spawn(move || manager.ensure(&selection))
    }
    pub fn synthesize(&self, selection: &Selection, id: u64, text: &str) -> Result<Pcm, TtsError> {
        let _idle_wake = IdleWake(&self.idle_wake);
        self.requests.fetch_add(1, Ordering::AcqRel);
        struct RequestDone<'a, B: Transport>(&'a TtsManager<B>, u64);
        impl<B: Transport> Drop for RequestDone<'_, B> {
            fn drop(&mut self) {
                self.0.requests.fetch_sub(1, Ordering::AcqRel);
                self.0
                    .cancelled_ids
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&self.1);
            }
        }
        let _request = RequestDone(self, id);
        for attempt in 0..2 {
            if self.cancelled(id) {
                return Err(TtsError::Cancelled);
            }
            self.ensure_with(selection, || self.cancelled(id), false)?;
            if self.cancelled(id) {
                let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                if !inner.backend.alive() {
                    self.stop_backend(&mut inner);
                    inner.key = None;
                    inner.lifecycle.stopped();
                    self.publish(State::Unloaded, Preparation::Idle);
                }
                return Err(TtsError::Cancelled);
            }
            let (receive, timeout, generation) = {
                let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                let cancelled = self.cancelled_ids.lock().unwrap_or_else(|p| p.into_inner());
                if cancelled.contains(&id)
                    || !inner.key.as_ref().is_some_and(|key| {
                        key.model == selection.model_id && key.voice == selection.voice
                    })
                {
                    return Err(TtsError::Cancelled);
                }
                inner.lifecycle.touch(self.started.elapsed());
                let native = handy_tts_core::speed::plan(
                    selection.speed,
                    catalog::entry(&selection.model_id)
                        .map_err(|_| TtsError::Worker)?
                        .native_speed,
                )
                .map_err(|_| TtsError::Worker)?
                .native;
                (
                    inner.backend.submit(id, text, Some(native)),
                    inner.backend.synthesis_timeout(),
                    inner.generation,
                )
            };
            let result = match receive {
                Err(error) => Err(error),
                Ok(receive) => match receive.recv_timeout(timeout) {
                    Ok(result) => result,
                    Err(_) => {
                        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                        if inner.generation == generation {
                            self.stop_backend(&mut inner);
                            inner.key = None;
                            inner.lifecycle.stopped();
                            self.publish(State::Unloaded, Preparation::Idle);
                        }
                        Err(TtsError::Worker)
                    }
                },
            };
            self.wake_idle();
            if self.cancelled(id) {
                return Err(TtsError::Cancelled);
            }
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let same_worker = inner.generation == generation;
            let alive = if same_worker {
                inner.backend.alive()
            } else {
                false
            };
            if result.is_ok() || alive {
                return result;
            }
            if same_worker {
                self.stop_backend(&mut inner);
                inner.key = None;
                inner.lifecycle.stopped();
                self.publish(State::Unloaded, Preparation::Idle);
            }
            drop(inner);
            if attempt == 1 {
                return result;
            }
        }
        Err(TtsError::Worker)
    }
    pub fn idle(&self, timeout: UnloadAfter, busy: bool) {
        self.idle_with(timeout, || busy, || {});
    }
    fn idle_with(&self, timeout: UnloadAfter, busy: impl Fn() -> bool, before_stop: impl FnOnce()) {
        let (generation, last_used) = {
            let mut inner = match self.inner.try_lock() {
                Ok(inner) => inner,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            let was_busy = inner.lifecycle.busy;
            inner.lifecycle.busy = busy() || self.requests.load(Ordering::Acquire) > 0;
            if was_busy || inner.lifecycle.busy {
                inner.lifecycle.touch(self.started.elapsed());
            }
            if !inner
                .lifecycle
                .needs_unload(self.started.elapsed(), timeout)
            {
                return;
            }
            (
                self.activity.load(Ordering::Acquire),
                inner.lifecycle.last_used,
            )
        };
        before_stop();
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        inner.lifecycle.busy = busy() || self.requests.load(Ordering::Acquire) > 0;
        if self.activity.load(Ordering::Acquire) != generation
            || inner.lifecycle.last_used != last_used
            || !inner
                .lifecycle
                .needs_unload(self.started.elapsed(), timeout)
        {
            return;
        }
        inner.lifecycle.stopping();
        self.publish(State::Stopping, Preparation::Idle);
        self.stop_backend(&mut inner);
        inner.key = None;
        inner.lifecycle.stopped();
        self.publish(State::Unloaded, Preparation::Idle);
    }
    fn idle_delay(&self, timeout: UnloadAfter, busy: bool) -> Option<Duration> {
        let inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        if inner.lifecycle.state != State::Ready
            || busy
            || self.requests.load(Ordering::Acquire) > 0
        {
            return None;
        }
        match timeout {
            UnloadAfter::Never => None,
            UnloadAfter::Immediately => Some(Duration::ZERO),
            UnloadAfter::Idle(limit) => Some(
                limit.saturating_sub(
                    self.started
                        .elapsed()
                        .saturating_sub(inner.lifecycle.last_used),
                ),
            ),
        }
    }
    pub fn spawn_idle(
        self: &Arc<Self>,
        app: tauri::AppHandle,
        busy: Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.spawn_idle_with(
            move || super::unload_after(crate::settings::get_settings(&app).model_unload_timeout),
            busy,
        );
    }
    fn spawn_idle_with(
        self: &Arc<Self>,
        timeout: impl Fn() -> UnloadAfter + Send + 'static,
        busy: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let Some(events) = self
            .idle_events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        else {
            return;
        };
        let manager = Arc::downgrade(self);
        std::thread::spawn(move || loop {
            let Some(manager) = manager.upgrade() else {
                break;
            };
            let timeout = timeout();
            manager.idle_with(timeout, || busy.load(Ordering::Acquire), || {});
            let delay = manager.idle_delay(timeout, busy.load(Ordering::Acquire));
            drop(manager);
            let result = match delay {
                Some(delay) => events.recv_timeout(delay),
                None => events
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            if matches!(result, Err(mpsc::RecvTimeoutError::Disconnected)) {
                break;
            }
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[derive(Default)]
    struct Trace {
        starts: Vec<Device>,
        speeds: Vec<Option<f32>>,
        load_lanes: Vec<usize>,
        stopped: usize,
        fail_all: bool,
        fail_npu: bool,
        memory_at_load: bool,
        submit_failures: usize,
        limits: Vec<usize>,
        start_gate: Option<Arc<std::sync::Barrier>>,
        start_hook: Option<Arc<dyn Fn() + Send + Sync>>,
        alive_gate: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    }
    #[derive(Clone, Default)]
    struct FakeControl {
        alive: Arc<AtomicBool>,
        kills: Arc<AtomicUsize>,
    }
    impl Control for FakeControl {
        fn cancel(&self, _: u64) {}
        fn kill(&self) {
            self.alive.store(false, Ordering::Release);
            self.kills.fetch_add(1, Ordering::Relaxed);
        }
    }
    struct Fake {
        control: FakeControl,
        trace: Arc<Mutex<Trace>>,
    }
    impl Transport for Fake {
        type Control = FakeControl;
        fn control(&self) -> Self::Control {
            self.control.clone()
        }
        fn start(&mut self, spec: &LoadSpec<'_>) -> Result<WorkerReady, TtsError> {
            let mut trace = self.trace.lock().unwrap();
            trace.starts.push(spec.device);
            trace.load_lanes.push(spec.lanes);
            if let Some(gate) = &trace.start_gate {
                gate.wait();
                gate.wait();
            }
            if let Some(hook) = &trace.start_hook {
                hook();
            }
            if trace.memory_at_load {
                return Err(TtsError::Memory);
            }
            if trace.fail_all {
                return Err(TtsError::Start);
            }
            if trace.fail_npu && spec.device == Device::Npu {
                return Err(TtsError::Device(DeviceError::CompileFailed));
            }
            self.control.alive.store(true, Ordering::Release);
            Ok(WorkerReady {
                device: spec.device,
                placement: BTreeMap::from([("main".into(), spec.device)]),
            })
        }
        fn submit(
            &mut self,
            id: u64,
            _: &str,
            speed: Option<f32>,
        ) -> Result<mpsc::Receiver<Result<Pcm, TtsError>>, TtsError> {
            let mut trace = self.trace.lock().unwrap();
            trace.speeds.push(speed);
            if trace.submit_failures > 0 {
                trace.submit_failures -= 1;
                self.control.alive.store(false, Ordering::Release);
                return Err(TtsError::Protocol);
            }
            drop(trace);
            let (send, receive) = mpsc::channel();
            send.send(Ok(Pcm {
                id,
                sample_rate: 24000,
                samples: vec![0.0; 24],
            }))
            .unwrap();
            Ok(receive)
        }
        fn set_lanes(&mut self, lanes: usize) -> Result<(), TtsError> {
            self.trace.lock().unwrap().limits.push(lanes);
            Ok(())
        }
        fn alive(&mut self) -> bool {
            let gate = self.trace.lock().unwrap().alive_gate.take();
            if let Some((entered, release)) = gate {
                entered.wait();
                release.wait();
            }
            self.control.alive.load(Ordering::Acquire)
        }
        fn stop(&mut self) {
            self.control.kill();
            self.trace.lock().unwrap().stopped += 1;
        }
    }
    fn config(choice: RunOn) -> ManagerConfig {
        use device::{DeviceRating, ModelRating};
        let devices: BTreeMap<Device, DeviceRating> =
            [(Device::Cpu, 5.0), (Device::Gpu, 8.0), (Device::Npu, 9.0)]
                .into_iter()
                .map(|(d, score)| {
                    (
                        d,
                        DeviceRating {
                            lanes: device::LaneSizing::default(),
                            supported: true,
                            placement: BTreeMap::from([("main".into(), d)]),
                            first_audio_p95_s: Some(1.0),
                            rtf_1x: Some(0.1),
                            rtf_2x: Some(0.2),
                            load_s_cold: Some(20.0),
                            load_s_cached: Some(2.0),
                            speed_score: Some(score),
                            recommended: d == Device::Npu,
                        },
                    )
                })
                .collect();
        ManagerConfig {
            root: "fixture".into(),
            binary: "worker".into(),
            cache: "cache".into(),
            home: "home".into(),
            available: vec![Device::Cpu, Device::Gpu, Device::Npu],
            run_on: BTreeMap::from([("supertonic-3".into(), choice)]),
            ratings: Ratings {
                schema_version: 1,
                metadata: BTreeMap::new(),
                models: [
                    ("pocket-en", 9),
                    ("supertonic-3", 8),
                    ("kokoro-82m-int8", 6),
                ]
                .into_iter()
                .map(|(id, q)| {
                    (
                        id.into(),
                        ModelRating {
                            quality: q,
                            devices: devices.clone(),
                        },
                    )
                })
                .collect(),
            },
        }
    }
    fn selection() -> Selection {
        Selection {
            model_id: "supertonic-3".into(),
            voice: "F1".into(),
            speed: 1.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
        }
    }
    fn manager(
        choice: RunOn,
        trace: Arc<Mutex<Trace>>,
        memory: fn() -> Result<u64, TtsError>,
    ) -> Arc<TtsManager<Fake>> {
        TtsManager::with_backend(
            config(choice),
            Fake {
                control: FakeControl::default(),
                trace,
            },
            Checks {
                memory,
                downloaded: |_, _| true,
            },
        )
        .unwrap()
    }
    fn enough() -> Result<u64, TtsError> {
        Ok(handy_tts_core::memory::MIN_AVAILABLE_BYTES)
    }
    #[test]
    fn submission_failure_after_worker_exit_retries_once() {
        for failures in [1, 2] {
            let trace = Arc::new(Mutex::new(Trace {
                submit_failures: failures,
                ..Default::default()
            }));
            let model = manager(RunOn::Cpu, trace.clone(), enough);
            model.ensure(&selection()).unwrap();
            let result = model.synthesize(&selection(), 41, "Retry this sentence");
            if failures == 1 {
                assert_eq!(result.unwrap().id, 41);
                assert_eq!(model.status(), State::Ready);
                assert!(model.control.alive.load(Ordering::Acquire));
            } else {
                assert_eq!(result, Err(TtsError::Protocol));
                assert_eq!(model.status(), State::Unloaded);
                assert!(!model.control.alive.load(Ordering::Acquire));
                assert!(model.inner.lock().unwrap().key.is_none());
            }
            let trace = trace.lock().unwrap();
            assert_eq!(trace.starts.len(), 2);
            assert_eq!(trace.speeds.len(), 2);
            assert!(trace.stopped >= failures);
        }
    }
    fn three_lane_manager(trace: Arc<Mutex<Trace>>) -> Arc<TtsManager<Fake>> {
        let mut config = config(RunOn::Cpu);
        for rating in config.ratings.models.values_mut() {
            rating.devices.get_mut(&Device::Cpu).unwrap().lanes = device::LaneSizing {
                best_lanes: 3,
                rtf_by_lanes: None,
                base_rss_mib: Some(100.0),
                lane_rss_mib: Some(100.0),
            };
        }
        TtsManager::with_backend(
            config,
            Fake {
                control: FakeControl::default(),
                trace,
            },
            Checks {
                memory: || Ok(8 * 1024 * 1024 * 1024),
                downloaded: |_, _| true,
            },
        )
        .unwrap()
    }
    #[test]
    fn idle_unload_resets_memory_adaptation_for_next_load() {
        for idle in [true, false] {
            let trace = Arc::new(Mutex::new(Trace::default()));
            let model = three_lane_manager(trace.clone());
            model.ensure(&selection()).unwrap();
            model.lower_lanes();
            assert_eq!(model.available_lanes(), 1);
            if idle {
                model.idle(UnloadAfter::Immediately, false);
            } else {
                model.unload();
            }
            assert_eq!(model.status(), State::Unloaded);
            model.ensure(&selection()).unwrap();
            assert_eq!(trace.lock().unwrap().load_lanes, [3, 3]);
            assert_eq!(trace.lock().unwrap().limits, [1]);
            assert_eq!(model.available_lanes(), 3);
        }
    }
    #[test]
    fn memory_pressure_during_load_is_applied_to_the_new_worker() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let model = three_lane_manager(trace.clone());
        model.ensure(&selection()).unwrap();
        model.lower_lanes();
        model.unload();
        let weak = Arc::downgrade(&model);
        trace.lock().unwrap().start_hook =
            Some(Arc::new(move || weak.upgrade().unwrap().lower_lanes()));
        model.ensure(&selection()).unwrap();
        assert_eq!(trace.lock().unwrap().load_lanes, [3, 3]);
        assert_eq!(trace.lock().unwrap().limits, [1, 1]);
        assert_eq!(model.available_lanes(), 1);
    }
    #[test]
    fn crash_restart_resets_memory_adaptation_for_next_load() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let model = three_lane_manager(trace.clone());
        model.ensure(&selection()).unwrap();
        model.lower_lanes();
        model.control.kill();
        model.ensure(&selection()).unwrap();
        model.lower_lanes();
        trace.lock().unwrap().submit_failures = 1;
        assert!(model.synthesize(&selection(), 42, "Restart").is_ok());
        assert_eq!(trace.lock().unwrap().load_lanes, [3, 3, 3]);
        assert_eq!(model.available_lanes(), 3);
    }
    #[test]
    fn missing_startup_default_reports_not_downloaded_without_starting_worker() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let model = TtsManager::with_backend(
            config(RunOn::Cpu),
            Fake {
                control: FakeControl::default(),
                trace: trace.clone(),
            },
            Checks {
                memory: enough,
                downloaded: |_, _| false,
            },
        )
        .unwrap();
        let settings = crate::settings::get_default_settings();
        assert_eq!(
            model.ensure(&super::super::service::selection(&settings)),
            Err(TtsError::NotDownloaded)
        );
        assert!(trace.lock().unwrap().starts.is_empty());
    }
    #[test]
    fn worker_start_and_failure_are_logged_without_credentials() {
        struct Logs(Mutex<Vec<String>>);
        impl log::Log for Logs {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("{} {}", record.level(), record.args()));
            }
            fn flush(&self) {}
        }
        static LOGS: Logs = Logs(Mutex::new(Vec::new()));
        log::set_logger(&LOGS).unwrap();
        log::set_max_level(log::LevelFilter::Info);
        let trace = Arc::new(Mutex::new(Trace::default()));
        trace.lock().unwrap().fail_all = true;
        let model = manager(RunOn::Cpu, trace, enough);
        assert!(model.ensure(&selection()).is_err());
        let reading = super::super::session::tests::selection_actor();
        super::super::commands::select_with_store(
            selection(),
            true,
            &reading,
            crate::settings::get_default_settings,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            super::super::commands::select_with_store(
                selection(),
                false,
                &reading,
                crate::settings::get_default_settings,
                |_| Ok(())
            ),
            Err("tts.errors.notDownloaded".into())
        );
        assert!(reading.shutdown());
        let logs = LOGS.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|line| line.contains("INFO TTS selection saved: supertonic-3")),
            "selection success is logged"
        );
        assert!(
            logs.iter().any(|line| line
                .contains("WARN TTS selection failed: supertonic-3: tts.errors.notDownloaded")),
            "selection failure is logged"
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("INFO TTS worker starting: supertonic-3")),
            "worker start is logged"
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("WARN TTS worker start failed: supertonic-3")),
            "worker failure is logged"
        );
    }
    #[test]
    fn interrupted_npu_start_is_cancelled_without_failure_retry_or_driver_rejection() {
        use std::io::Read;
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        for mode in 0..3 {
            let fixture =
                tempfile::tempdir_in(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap()).unwrap();
            let listener = UnixListener::bind(fixture.path().join("gate.sock")).unwrap();
            let binary = fixture.path().join("starting-worker.py");
            std::fs::write(&binary, r#"#!/usr/bin/python3
import json, pathlib, socket, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['type'] == 'load':
        root = pathlib.Path(request['model_dir']).parent
        starts = root / 'starts'
        first = not starts.exists()
        with starts.open('a') as out:
            out.write(request['device'] + '\n')
        if first:
            gate = socket.socket(socket.AF_UNIX)
            gate.connect(str(root / 'gate.sock'))
            gate.sendall(b'S')
            gate.recv(1)
        device = request['device']
        print(json.dumps({'id': 0, 'status': 'ready', 'device': device, 'placement': {'main': device}, 'samples': 0}), flush=True)
"#).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut config = config(RunOn::Auto);
            config.root = fixture.path().into();
            config.binary = binary;
            config.cache = fixture.path().join("cache");
            config.home = fixture.path().join("home");
            let parent = TtsManager::with_backend(
                config,
                WorkerBackend::default(),
                Checks {
                    memory: enough,
                    downloaded: |_, _| true,
                },
            )
            .unwrap();
            let child = parent.fork().unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let task = {
                let child = child.clone();
                let cancelled = cancelled.clone();
                std::thread::spawn(move || match mode {
                    0 => child.ensure_cancellable(&selection(), &cancelled),
                    1 => child
                        .synthesize(&selection(), 91, "interrupted")
                        .map(|_| ()),
                    _ => child.ensure(&selection()),
                })
            };
            let (mut gate, _) = listener.accept().unwrap();
            gate.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut signal = [0];
            gate.read_exact(&mut signal).unwrap();
            assert_eq!(signal, [b'S']);
            if mode == 0 {
                cancelled.store(true, Ordering::Release);
            }
            if mode == 1 {
                child.cancel(91);
            }
            child.interrupt();
            assert_eq!(task.join().unwrap(), Err(TtsError::Cancelled));
            assert_eq!(
                std::fs::read_to_string(fixture.path().join("starts")).unwrap(),
                "npu\n"
            );
            assert_eq!(child.inner.lock().unwrap().lifecycle.failed_starts, 0);
            assert!(!child.npu_driver_too_old("supertonic-3"));
            assert!(!parent.npu_driver_too_old("supertonic-3"));
            assert_eq!(child.status(), State::Unloaded);
        }
    }
    #[test]
    fn synthesis_deadline_reaps_worker_and_play_reloads_even_with_never_unload() {
        use std::os::unix::fs::PermissionsExt;
        let fixture =
            tempfile::tempdir_in(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap()).unwrap();
        let binary = fixture.path().join("wedged-worker.py");
        std::fs::write(&binary, r#"#!/usr/bin/python3
import json, os, pathlib, struct, sys, time
root = None
for line in sys.stdin:
    request = json.loads(line)
    if request['type'] == 'load':
        root = pathlib.Path(request['model_dir']).parent
        with (root / 'starts').open('a') as trace:
            trace.write(str(os.getpid()) + '\n')
        print(json.dumps({'id': 0, 'status': 'ready', 'device': 'cpu', 'placement': {'main': 'cpu'}, 'samples': 0}), flush=True)
    elif request['type'] == 'synthesize':
        if not (root / 'allow-retry').exists():
            while True:
                time.sleep(60)
        print(json.dumps({'id': request['id'], 'status': 'ok', 'sample_rate': 24000, 'samples': 1}), flush=True)
        sys.stdout.buffer.write(struct.pack('<f', 0.25))
        sys.stdout.buffer.flush()
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = config(RunOn::Cpu);
        config.root = fixture.path().into();
        config.binary = binary;
        config.cache = fixture.path().join("cache");
        config.home = fixture.path().join("home");
        let manager = TtsManager::with_backend(
            config,
            WorkerBackend::with_synthesis_timeout(Duration::from_millis(100)),
            Checks {
                memory: enough,
                downloaded: |_, _| true,
            },
        )
        .unwrap();
        assert!(matches!(
            manager.synthesize(&selection(), 1, "Wedged"),
            Err(TtsError::Worker)
        ));
        let starts = std::fs::read_to_string(fixture.path().join("starts")).unwrap();
        let pids = starts.lines().collect::<Vec<_>>();
        assert_eq!(
            pids.len(),
            2,
            "deadline triggers exactly one automatic retry"
        );
        for pid in &pids {
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "deadline must kill and reap every wedged worker"
            );
        }
        assert_eq!(manager.status(), State::Unloaded);
        manager.idle(UnloadAfter::Never, false);
        std::fs::write(fixture.path().join("allow-retry"), []).unwrap();
        let pcm = manager.synthesize(&selection(), 2, "Next Play").unwrap();
        assert_eq!(pcm.samples, [0.25]);
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("starts"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        manager.unload();
    }
    #[test]
    fn same_model_selection_clears_failed_and_allows_play_retry() {
        use handy_tts_core::{
            feed::Segment,
            playback::{
                segment::{Output, OutputEvent},
                PlaybackError,
            },
            scheduler::Ticket,
            session::{Session, Worker},
        };
        struct Speaker;
        impl Output for Speaker {
            type Prepared = ();
            fn names(&self) -> Result<Vec<String>, PlaybackError> {
                Ok(vec![])
            }
            fn open(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn prepare(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn commit(&mut self, _: ()) {}
            fn set_sample_rate(&mut self, _: u32) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn append(&mut self, _: Segment) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn events(&mut self) -> Vec<OutputEvent> {
                vec![]
            }
            fn retract_unstarted(&mut self) -> Vec<Segment> {
                vec![]
            }
            fn content_samples(&self) -> u64 {
                0
            }
            fn pause(&mut self) {}
            fn resume(&mut self) {}
            fn stop(&mut self) {}
        }
        struct Engine(Arc<TtsManager<Fake>>);
        impl Worker for Engine {
            fn request(&mut self, _: Ticket, _: &str, _: &Selection) -> Result<(), String> {
                Ok(())
            }
            fn poll(&mut self) -> Option<handy_tts_core::session::Completion> {
                None
            }
            fn cancel(&mut self, _: Ticket) {}
            fn selection_changed(&mut self) {
                self.0.selected();
            }
            fn retry_failed_selection(&mut self) {
                self.0.retry_failed_selection();
            }
        }
        let trace = Arc::new(Mutex::new(Trace {
            fail_all: true,
            ..Trace::default()
        }));
        let manager = manager(RunOn::Cpu, trace.clone(), enough);
        assert!(manager.ensure(&selection()).is_err());
        assert!(manager.ensure(&selection()).is_err());
        assert_eq!(manager.status(), State::Failed);
        let mut session = Session::new(
            "Read again",
            selection(),
            Engine(manager.clone()),
            Speaker,
            None,
        )
        .unwrap();
        session.select(selection(), true, |_| Ok(())).unwrap();
        assert_eq!(
            manager.status(),
            State::Unloaded,
            "explicit same-model selection clears Failed"
        );
        trace.lock().unwrap().fail_all = false;
        manager.ensure(&selection()).unwrap();
        assert_eq!(manager.status(), State::Ready);
        session.select(selection(), true, |_| Ok(())).unwrap();
        assert_eq!(
            manager.status(),
            State::Ready,
            "same selection keeps a healthy worker loaded"
        );
    }
    fn idle_after_unlock(synthesis: bool) {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let manager = manager(RunOn::Cpu, trace.clone(), enough);
        let (sampled, samples) = mpsc::channel();
        let weak = Arc::downgrade(&manager);
        manager.spawn_idle_with(
            move || {
                sampled.send(weak.upgrade().unwrap().status()).unwrap();
                UnloadAfter::Idle(Duration::from_millis(100))
            },
            Arc::new(AtomicBool::new(false)),
        );
        samples.recv_timeout(Duration::from_secs(2)).unwrap();
        let (entered, ready) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let wait_release = Mutex::new(wait_release);
        let weak = Arc::downgrade(&manager);
        manager.on_status_change(Arc::new(move || {
            if weak.upgrade().unwrap().status() == State::Ready {
                entered.send(()).unwrap();
                wait_release.lock().unwrap().recv().unwrap();
            }
        }));
        let loading = manager.clone();
        let load = std::thread::spawn(move || {
            if synthesis {
                loading
                    .synthesize(&selection(), 77, "Cancelled")
                    .map(|_| ())
            } else {
                loading.ensure(&selection())
            }
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let alive_entered = Arc::new(std::sync::Barrier::new(2));
        let alive_release = Arc::new(std::sync::Barrier::new(2));
        if synthesis {
            trace.lock().unwrap().alive_gate = Some((alive_entered.clone(), alive_release.clone()));
            release.send(()).unwrap();
            alive_entered.wait();
        }
        loop {
            if samples.recv_timeout(Duration::from_secs(2)).unwrap() == State::Ready {
                break;
            }
        }
        assert!(matches!(
            manager.inner.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        while samples.recv_timeout(Duration::from_millis(650)).is_ok() {}
        if synthesis {
            alive_release.wait();
        } else {
            release.send(()).unwrap();
        }
        assert_eq!(load.join().unwrap(), Ok(()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while manager.status() != State::Unloaded && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            manager.status(),
            State::Unloaded,
            "unlocking the worker must reschedule its idle deadline"
        );
        while samples.recv_timeout(Duration::from_millis(650)).is_ok() {}
    }
    #[test]
    fn idle_monitor_reschedules_readiness_after_unlock() {
        idle_after_unlock(false);
    }
    #[test]
    fn idle_monitor_reschedules_synthesis_completion_after_unlock() {
        idle_after_unlock(true);
    }
    #[test]
    fn idle_monitor_sleeps_until_activity_and_uses_one_unload_deadline() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let manager = manager(RunOn::Cpu, trace.clone(), enough);
        let (sampled, samples) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        manager.spawn_idle_with(
            move || {
                sampled.send(()).unwrap();
                UnloadAfter::Idle(Duration::from_millis(100))
            },
            busy.clone(),
        );
        samples.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            samples.recv_timeout(Duration::from_millis(1200)).is_err(),
            "unloaded manager must not poll settings"
        );
        busy.store(true, Ordering::Release);
        manager.ensure(&selection()).unwrap();
        manager.wake_idle();
        samples.recv_timeout(Duration::from_secs(2)).unwrap();
        busy.store(false, Ordering::Release);
        manager.wake_idle();
        let deadline = Instant::now() + Duration::from_secs(3);
        while manager.status() != State::Unloaded && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(manager.status(), State::Unloaded);
        assert_eq!(trace.lock().unwrap().stopped, 1);
        while samples.try_recv().is_ok() {}
        assert!(
            samples.recv_timeout(Duration::from_millis(1200)).is_err(),
            "unloaded monitor resumes blocking after its deadline"
        );
    }
    #[test]
    fn npu_transport_failure_does_not_reject_the_driver() {
        let trace = Arc::new(Mutex::new(Trace {
            fail_all: true,
            ..Trace::default()
        }));
        let manager = manager(RunOn::Npu, trace.clone(), enough);
        assert_eq!(manager.ensure(&selection()), Err(TtsError::Start));
        assert_eq!(manager.inner.lock().unwrap().lifecycle.failed_starts, 2);
        assert!(!manager.npu_driver_too_old("supertonic-3"));
    }
    #[test]
    fn idle_rechecks_activity_and_generation_after_its_decision() {
        for stays_busy in [false, true] {
            let trace = Arc::new(Mutex::new(Trace::default()));
            let manager = manager(RunOn::Cpu, trace.clone(), enough);
            manager.ensure(&selection()).unwrap();
            let busy = Arc::new(AtomicBool::new(false));
            let gate = Arc::new(std::sync::Barrier::new(2));
            let idle = {
                let manager = manager.clone();
                let busy = busy.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    manager.idle_with(
                        UnloadAfter::Immediately,
                        || busy.load(Ordering::Acquire),
                        || {
                            gate.wait();
                            gate.wait();
                        },
                    )
                })
            };
            gate.wait();
            busy.store(true, Ordering::Release);
            if !stays_busy {
                manager.ensure(&selection()).unwrap();
            }
            busy.store(stays_busy, Ordering::Release);
            gate.wait();
            idle.join().unwrap();
            assert_eq!(manager.status(), State::Ready);
            assert!(manager.control.alive.load(Ordering::Acquire));
            assert_eq!(trace.lock().unwrap().stopped, 0);
            busy.store(false, Ordering::Release);
            manager.idle_with(
                UnloadAfter::Immediately,
                || busy.load(Ordering::Acquire),
                || {},
            );
            assert_eq!(manager.status(), State::Unloaded);
            assert_eq!(trace.lock().unwrap().stopped, 1);
        }
    }
    #[test]
    fn idle_tick_does_not_wait_behind_startup_with_stale_activity() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let trace = Arc::new(Mutex::new(Trace {
            start_gate: Some(gate.clone()),
            ..Trace::default()
        }));
        let manager = manager(RunOn::Cpu, trace.clone(), enough);
        let start = manager.prepare(selection());
        gate.wait();
        let busy = AtomicBool::new(false);
        let samples = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let (done, wait) = std::sync::mpsc::channel();
            let manager = &manager;
            let busy = &busy;
            let samples = &samples;
            scope.spawn(move || {
                manager.idle_with(
                    UnloadAfter::Immediately,
                    || {
                        samples.fetch_add(1, Ordering::Relaxed);
                        busy.load(Ordering::Acquire)
                    },
                    || {},
                );
                done.send(()).unwrap();
            });
            let skipped = wait.recv_timeout(Duration::from_secs(1)).is_ok();
            busy.store(true, Ordering::Release);
            gate.wait();
            start.join().unwrap().unwrap();
            assert!(
                skipped,
                "a contended idle tick must return without waiting for startup"
            );
        });
        assert_eq!(samples.load(Ordering::Relaxed), 0);
        assert_eq!(manager.status(), State::Ready);
        assert!(manager.control.alive.load(Ordering::Acquire));
        manager.idle(UnloadAfter::Immediately, false);
        assert_eq!(manager.status(), State::Unloaded);
    }
    #[test]
    fn cancelled_prepare_cannot_leave_a_worker_spawned_after_its_stop() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let trace = Arc::new(Mutex::new(Trace {
            start_gate: Some(gate.clone()),
            ..Trace::default()
        }));
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = {
            let m = m.clone();
            let cancelled = cancelled.clone();
            std::thread::spawn(move || m.ensure_cancellable(&selection(), &cancelled))
        };
        gate.wait();
        cancelled.store(true, Ordering::Release);
        m.interrupt();
        gate.wait();
        assert_eq!(task.join().unwrap(), Err(TtsError::Cancelled));
        assert!(!m.control.alive.load(Ordering::Acquire));
        assert_eq!(m.status(), State::Unloaded);
        assert_eq!(trace.lock().unwrap().stopped, 1);
    }
    #[test]
    fn cancelled_synthesis_during_start_preserves_the_live_worker() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let gate = Arc::new(std::sync::Barrier::new(2));
        trace.lock().unwrap().start_gate = Some(gate.clone());
        let manager = manager(RunOn::Cpu, trace.clone(), enough);
        let started = manager.clone();
        let thread =
            std::thread::spawn(move || started.synthesize(&selection(), 19, "Cancelled preview"));
        gate.wait();
        manager.cancel(19);
        manager.interrupt();
        gate.wait();
        assert_eq!(thread.join().unwrap().unwrap_err(), TtsError::Cancelled);
        assert!(manager.control.alive.load(Ordering::Acquire));
        assert_eq!(manager.status(), State::Ready);
        assert_eq!(trace.lock().unwrap().stopped, 0);
        assert!(trace.lock().unwrap().speeds.is_empty());
    }
    #[test]
    fn child_uses_live_device_choices_and_shares_npu_refusal_with_parent() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        let boot = m.configuration();
        m.set_run_on("supertonic-3", RunOn::Auto).unwrap();
        let child_trace = Arc::new(Mutex::new(Trace {
            fail_npu: true,
            ..Trace::default()
        }));
        let child = m
            .fork_with_backend(Fake {
                control: FakeControl::default(),
                trace: child_trace.clone(),
            })
            .unwrap();
        assert_eq!(boot.run_on["supertonic-3"], RunOn::Cpu);
        assert_eq!(child.configuration().run_on["supertonic-3"], RunOn::Auto);
        child.ensure(&selection()).unwrap();
        assert_eq!(
            child_trace.lock().unwrap().starts,
            [Device::Npu, Device::Gpu]
        );
        assert!(child.npu_driver_too_old("supertonic-3"));
        assert!(m.npu_driver_too_old("supertonic-3"));
        m.ensure(&selection()).unwrap();
        assert_eq!(trace.lock().unwrap().starts, [Device::Gpu]);
        m.set_run_on("supertonic-3", RunOn::Npu).unwrap();
        assert_eq!(
            m.ensure(&selection()),
            Err(TtsError::Device(DeviceError::NpuDriverTooOld))
        );
    }
    #[test]
    fn speed_and_pitch_changes_reuse_loaded_worker_but_native_speed_is_sent_per_request() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        let mut s = selection();
        m.synthesize(&s, 1, "One.").unwrap();
        s.speed = 2.0;
        s.pitch = 6.0;
        m.synthesize(&s, 2, "Two.").unwrap();
        assert_eq!(trace.lock().unwrap().starts, [Device::Cpu]);
        assert_eq!(trace.lock().unwrap().speeds, [Some(1.0), Some(1.0)]);
    }
    #[test]
    fn load_lanes_use_selected_device_measurements_and_legacy_defaults() {
        for (device, choice, mem, expected) in [
            (Device::Cpu, RunOn::Cpu, 8 * 1024 * 1024 * 1024, 3),
            (Device::Gpu, RunOn::Gpu, 8 * 1024 * 1024 * 1024, 2),
            (Device::Npu, RunOn::Npu, 8 * 1024 * 1024 * 1024, 1),
            (Device::Cpu, RunOn::Cpu, 1400 * 1024 * 1024, 1),
        ] {
            let trace = Arc::new(Mutex::new(Trace::default()));
            let memory: fn() -> Result<u64, TtsError> =
                if mem < handy_tts_core::memory::LANE_RESERVE_BYTES {
                    || Ok(1400 * 1024 * 1024)
                } else {
                    || Ok(8 * 1024 * 1024 * 1024)
                };
            let m = manager(choice, trace.clone(), memory);
            {
                let mut inner = m.inner.lock().unwrap();
                let model = inner.config.ratings.models.get_mut("supertonic-3").unwrap();
                for (d, rating) in &mut model.devices {
                    if *d == Device::Npu {
                        continue;
                    }
                    rating.lanes = device::LaneSizing {
                        best_lanes: if *d == Device::Cpu { 3 } else { 2 },
                        rtf_by_lanes: None,
                        base_rss_mib: Some(100.0),
                        lane_rss_mib: Some(100.0),
                    };
                }
            }
            m.ensure(&selection()).unwrap();
            assert_eq!(trace.lock().unwrap().starts, [device]);
            assert_eq!(trace.lock().unwrap().load_lanes, [expected]);
            assert_eq!(m.available_lanes(), expected);
        }
        for missing_base in [true, false] {
            let trace = Arc::new(Mutex::new(Trace::default()));
            let m = manager(RunOn::Cpu, trace.clone(), || Ok(8 * 1024 * 1024 * 1024));
            m.inner
                .lock()
                .unwrap()
                .config
                .ratings
                .models
                .get_mut("supertonic-3")
                .unwrap()
                .devices
                .get_mut(&Device::Cpu)
                .unwrap()
                .lanes = device::LaneSizing {
                best_lanes: 3,
                rtf_by_lanes: None,
                base_rss_mib: if missing_base { None } else { Some(100.0) },
                lane_rss_mib: if missing_base { Some(100.0) } else { None },
            };
            m.ensure(&selection()).unwrap();
            assert_eq!(
                trace.lock().unwrap().load_lanes,
                [1],
                "partial measurements use one lane"
            );
        }
        let legacy: device::DeviceRating =
            serde_json::from_str(r#"{"supported":false,"placement":{},"recommended":false}"#)
                .unwrap();
        assert_eq!(legacy.lanes.best_lanes, 1);
        assert_eq!(legacy.lanes.base_rss_mib, None);
        assert_eq!(legacy.lanes.lane_rss_mib, None);
    }
    #[test]
    fn pocket_requests_explicit_unity_native_speed_at_two_times() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        let mut s = selection();
        s.model_id = "pocket-en".into();
        s.voice = "alba".into();
        s.speed = 2.0;
        m.synthesize(&s, 1, "Every word.").unwrap();
        assert_eq!(trace.lock().unwrap().speeds, [Some(1.0)]);
    }
    #[test]
    fn changing_an_inactive_model_device_preserves_the_loaded_worker() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        m.ensure(&selection()).unwrap();
        m.set_run_on("pocket-en", RunOn::Gpu).unwrap();
        assert_eq!(m.status(), State::Ready);
        assert_eq!(trace.lock().unwrap().stopped, 0);
        m.ensure(&selection()).unwrap();
        assert_eq!(trace.lock().unwrap().starts, [Device::Cpu]);
    }
    #[test]
    fn auto_falls_back_once_marks_old_npu_and_explicit_keeps_its_choice() {
        let trace = Arc::new(Mutex::new(Trace {
            fail_npu: true,
            ..Trace::default()
        }));
        let m = manager(RunOn::Auto, trace.clone(), enough);
        m.ensure(&selection()).unwrap();
        assert_eq!(trace.lock().unwrap().starts, [Device::Npu, Device::Gpu]);
        assert_eq!(m.snapshot().fallback_from, Some(Device::Npu));
        assert_eq!(m.snapshot().ready.unwrap().device, Device::Gpu);
        m.set_run_on("supertonic-3", RunOn::Npu).unwrap();
        assert_eq!(
            m.ensure(&selection()),
            Err(TtsError::Device(DeviceError::NpuDriverTooOld))
        );
        assert_eq!(
            m.inner.lock().unwrap().config.run_on["supertonic-3"],
            RunOn::Npu
        );
        assert_eq!(trace.lock().unwrap().starts.len(), 2);
    }
    #[test]
    fn three_physical_failures_latch_until_reselection_and_idle_uses_shared_lifecycle() {
        let trace = Arc::new(Mutex::new(Trace {
            fail_all: true,
            ..Trace::default()
        }));
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        assert!(m.ensure(&selection()).is_err());
        assert!(m.ensure(&selection()).is_err());
        assert_eq!(trace.lock().unwrap().starts.len(), 3);
        assert_eq!(m.status(), State::Failed);
        assert_eq!(m.ensure(&selection()), Err(TtsError::Failed));
        trace.lock().unwrap().fail_all = false;
        m.selected();
        m.ensure(&selection()).unwrap();
        assert_eq!(m.status(), State::Ready);
        m.idle(UnloadAfter::Immediately, true);
        assert_eq!(m.status(), State::Ready);
        m.idle(UnloadAfter::Immediately, false);
        assert_eq!(m.status(), State::Unloaded);
        assert!(!m.control.alive.load(Ordering::Acquire));
    }
    #[test]
    fn memory_refusal_does_not_start_or_increment_failure_streak_and_background_prepare_joins() {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let m = manager(RunOn::Cpu, trace.clone(), || {
            Ok(handy_tts_core::memory::MIN_AVAILABLE_BYTES - 1)
        });
        assert_eq!(m.ensure(&selection()), Err(TtsError::Memory));
        assert!(trace.lock().unwrap().starts.is_empty());
        assert_eq!(m.inner.lock().unwrap().lifecycle.failed_starts, 0);
        let m = manager(RunOn::Cpu, trace.clone(), enough);
        m.prepare(selection()).join().unwrap().unwrap();
        assert_eq!(m.status(), State::Ready);
    }
    #[test]
    fn real_crashing_worker_drains_preceding_sentences_and_retries_failed_sentence() {
        use handy_tts_core::{
            feed::{Feed, Segment},
            playback::{
                segment::{Output, OutputEvent},
                PlaybackError,
            },
            scheduler::PlaybackState,
            session::Session,
            text::TextCursor,
        };
        use std::os::unix::fs::PermissionsExt;
        #[derive(Default)]
        struct Speaker {
            played: Vec<usize>,
            finished: bool,
            feed: Option<Feed>,
            remaining: std::collections::VecDeque<usize>,
            speech_indices: std::collections::BTreeMap<usize, usize>,
        }
        impl Output for Speaker {
            type Prepared = ();
            fn names(&self) -> Result<Vec<String>, PlaybackError> {
                Ok(vec![])
            }
            fn open(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn prepare(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
                Ok(())
            }
            fn commit(&mut self, _: ()) {}
            fn set_sample_rate(&mut self, rate: u32) -> Result<(), PlaybackError> {
                if self.feed.is_none() {
                    self.feed = Some(Feed::new(rate));
                }
                Ok(())
            }
            fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
                self.speech_indices.insert(
                    segment.index,
                    segment.speech[segment.speech.len() / 2] as usize,
                );
                self.remaining.push_back(segment.samples());
                self.feed.as_mut().unwrap().push(segment);
                Ok(())
            }
            fn events(&mut self) -> Vec<OutputEvent> {
                let Some(feed) = &mut self.feed else {
                    return vec![];
                };
                if self.finished {
                    if let Some(count) = self.remaining.pop_front() {
                        feed.pull(&mut vec![0.0; count]);
                    }
                    self.finished = false;
                }
                let events: Vec<_> = feed
                    .drain_events()
                    .into_iter()
                    .map(OutputEvent::from)
                    .collect();
                for event in &events {
                    if let OutputEvent::Started(index) = event {
                        self.played.push(self.speech_indices.remove(index).unwrap());
                    }
                }
                events
            }
            fn retract_unstarted(&mut self) -> Vec<Segment> {
                self.feed
                    .as_mut()
                    .map(|f| f.retract_unstarted())
                    .unwrap_or_default()
            }
            fn content_samples(&self) -> u64 {
                self.feed.as_ref().map_or(0, |f| f.content_samples())
            }
            fn pause(&mut self) {}
            fn resume(&mut self) {}
            fn stop(&mut self) {
                self.feed = None;
                self.remaining.clear();
                self.speech_indices.clear();
            }
        }
        let scratch = std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let binary = fixture.path().join("crashing-worker.py");
        std::fs::write(&binary, r#"#!/usr/bin/python3
import json, pathlib, re, struct, sys
root = None
for line in sys.stdin:
    request = json.loads(line)
    if request['type'] == 'load':
        root = pathlib.Path(request['model_dir'])
        print(json.dumps({'id': 0, 'status': 'ready', 'device': 'cpu', 'placement': {'main': 'cpu'}, 'samples': 0}), flush=True)
    elif request['type'] == 'synthesize':
        index = int(re.search(r'Sentence(\d+)', request['text']).group(1))
        if index == 750 and not (root.parent / 'allow-retry').exists():
            sys.exit(1)
        print(json.dumps({'id': request['id'], 'status': 'ok', 'sample_rate': 8000, 'samples': 8000}), flush=True)
        sys.stdout.buffer.write(struct.pack('<f', float(index)) * 8000)
        sys.stdout.buffer.flush()
    elif request['type'] == 'shutdown':
        break
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = config(RunOn::Cpu);
        config.root = fixture.path().to_path_buf();
        config.binary = binary;
        config.cache = fixture.path().join("cache");
        config.home = fixture.path().join("home");
        let manager = TtsManager::with_backend(
            config,
            WorkerBackend::default(),
            Checks {
                memory: enough,
                downloaded: |_, _| true,
            },
        )
        .unwrap();
        let source = (0..1500)
            .map(|i| {
                format!("Sentence{i:04} has enough characters to form its own speech chunk.\n\n")
            })
            .collect::<String>();
        let mut cursor = TextCursor::new(&source, false).unwrap();
        let mut chunks = Vec::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1500);
        let worker = super::super::session::ManagerWorker::new(manager.clone());
        let mut session =
            Session::new(&source, selection(), worker, Speaker::default(), None).unwrap();
        session.set_dsp_clock(|| 0.0);
        session.play().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut now = 0.0;
        let mut draining = false;
        while session.queue.state != PlaybackState::Error && Instant::now() < deadline {
            let hold_preceding = session.queue.failed_index.is_none()
                && session
                    .queue
                    .playing
                    .as_ref()
                    .is_some_and(|audio| audio.ticket.index >= 748);
            session.playback.output.finished = now % 4.0 == 0.0 && !hold_preceding;
            session.tick(now).unwrap();
            draining |= session.queue.failed_index.is_some() && session.queue.playing.is_some();
            assert!(session.queue.queued_seconds() <= 30.0);
            now += 0.25;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(draining);
        assert_eq!(session.queue.state, PlaybackState::Error);
        assert_eq!(session.queue.failed_index, Some(750));
        assert_eq!(session.snapshot().source_start, chunks[750].source_start);
        assert_eq!(session.snapshot().source_end, chunks[750].source_end);
        assert_eq!(session.playback.output.played, (0..750).collect::<Vec<_>>());
        std::fs::write(fixture.path().join("allow-retry"), []).unwrap();
        session.play().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while session.queue.state != PlaybackState::Finished && Instant::now() < deadline {
            session.playback.output.finished = now % 4.0 == 0.0;
            session.tick(now).unwrap();
            assert!(session.queue.queued_seconds() <= 30.0);
            now += 0.25;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(session.queue.state, PlaybackState::Finished);
        assert_eq!(
            session.playback.output.played,
            (0..1500).collect::<Vec<_>>()
        );
        drop(session);
        manager.unload();
        assert_eq!(manager.status(), State::Unloaded);
    }
    #[test]
    fn worker_memory_refusal_is_not_retried_or_counted_as_npu_rejection() {
        let trace = Arc::new(Mutex::new(Trace {
            memory_at_load: true,
            ..Trace::default()
        }));
        let manager = manager(RunOn::Npu, trace.clone(), enough);
        assert_eq!(manager.ensure(&selection()), Err(TtsError::Memory));
        assert_eq!(trace.lock().unwrap().starts, [Device::Npu]);
        assert_eq!(manager.inner.lock().unwrap().lifecycle.failed_starts, 0);
        assert!(!manager.npu_driver_too_old("supertonic-3"));
        assert_eq!(manager.status(), State::Unloaded);
    }
}
