use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
/// Tracks actual paste completion for the current dictation generation.
pub struct PipelineFence {
    pub generation: u64,
    pub paste_queued: bool,
    pub finished: bool,
}
impl PipelineFence {
    /// Starts a new pipeline and invalidates older completions.
    pub fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.paste_queued = false;
        self.finished = false;
        self.generation
    }
    /// Marks a native paste as queued for the matching generation.
    pub fn queued(&mut self, generation: u64) {
        if self.generation == generation {
            self.paste_queued = true;
        }
    }
    /// Clears the paste barrier only for the matching generation.
    pub fn pasted(&mut self, generation: u64) {
        if self.generation == generation {
            self.paste_queued = false;
        }
    }
    /// Releases processing once, after the matching paste has completed.
    pub fn finish(&mut self, generation: u64) -> bool {
        if self.generation != generation || self.paste_queued || self.finished {
            return false;
        }
        self.finished = true;
        true
    }
}

pub type Job = Box<dyn FnOnce() + Send>;
pub type NativeQueue = std::sync::Arc<dyn Fn(Job) -> Result<(), String> + Send + Sync>;
pub type NativeFailure = std::sync::Arc<dyn Fn(String) + Send + Sync>;
enum DeferredJob {
    Queue(Job),
    Paste(Job),
}
#[derive(Default)]
struct FenceState {
    capture: bool,
    pastes: usize,
    deferred: Vec<DeferredJob>,
}
#[derive(Default)]
/// Reserves clipboard capture ahead of native paste.
pub struct PasteFence {
    state: Mutex<FenceState>,
    revision: std::sync::atomic::AtomicU64,
}
impl PasteFence {
    /// Reads a nonblocking paste stamp at shortcut dispatch.
    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Reserves capture unless another capture or paste already owns the fence.
    pub fn reserve_capture(&self) -> bool {
        self.reserve_capture_since(self.revision())
    }
    /// Rejects capture if native paste started after the shortcut press.
    pub fn reserve_capture_since(&self, revision: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.capture || state.pastes > 0 || self.revision() != revision {
            return false;
        }
        state.capture = true;
        true
    }
    /// Releases capture and replays deferred work through its queue or paste barrier.
    pub fn release_capture(&self) {
        let jobs = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.capture = false;
            std::mem::take(&mut state.deferred)
        };
        for job in jobs {
            match job {
                DeferredJob::Queue(job) => self.before_queue(job),
                DeferredJob::Paste(job) => self.before_paste(job),
            }
        }
    }
    fn dispatch(&self, job: Job) {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.capture {
                state.deferred.push(DeferredJob::Paste(job));
                return;
            }
            state.pastes += 1;
            self.revision
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        let _active = ActivePaste(self);
        job();
    }
    /// Defers queueing during capture; queue callbacks may enter the paste fence.
    pub fn before_queue(&self, job: Job) {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.capture {
                state.deferred.push(DeferredJob::Queue(job));
                return;
            }
        }
        job();
    }
    /// Queues native work behind both clipboard capture barriers.
    pub fn queue_native(
        self: &std::sync::Arc<Self>,
        queue: NativeQueue,
        job: Job,
        failed: NativeFailure,
    ) {
        let fence = self.clone();
        self.before_queue(Box::new(move || {
            let replay_queue = queue.clone();
            let replay_failed = failed.clone();
            let guarded = Box::new(move || {
                {
                    let mut state = fence.state.lock().unwrap_or_else(|p| p.into_inner());
                    if state.capture {
                        let replay = fence.clone();
                        state.deferred.push(DeferredJob::Queue(Box::new(move || {
                            replay.queue_native(replay_queue, job, replay_failed);
                        })));
                        return;
                    }
                    if state.pastes > 0 {
                        drop(state);
                        replay_failed("native paste still active".into());
                        return;
                    }
                    state.pastes += 1;
                    fence
                        .revision
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                let _active = ActivePaste(&fence);
                job();
            });
            if let Err(error) = queue(guarded) {
                failed(error);
            }
        }));
    }
    /// Prevents capture during native paste without holding the state lock.
    pub fn before_paste(&self, job: Job) {
        self.dispatch(job);
    }
}

struct ActivePaste<'a>(&'a PasteFence);
impl Drop for ActivePaste<'_> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pastes -= 1;
    }
}

#[derive(Default)]
struct Settlement {
    done: Option<Job>,
    settling: bool,
}
struct CompletionState {
    settlement: Mutex<Settlement>,
    wake: Condvar,
    holders: std::sync::atomic::AtomicUsize,
}
impl CompletionState {
    fn finish(&self) {
        let done = self
            .settlement
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .done
            .take();
        self.wake.notify_all();
        if let Some(done) = done {
            done();
        }
    }
}
impl Drop for CompletionState {
    fn drop(&mut self) {
        self.finish();
    }
}
struct SettleOnDrop<'a>(&'a CompletionState);
impl Drop for SettleOnDrop<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}
/// Completes a paste after native settlement, unwind, abandonment or timeout.
pub struct PasteCompletion(Arc<CompletionState>);
impl Clone for PasteCompletion {
    fn clone(&self) -> Self {
        self.0
            .holders
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(self.0.clone())
    }
}
impl Drop for PasteCompletion {
    fn drop(&mut self) {
        if self
            .0
            .holders
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
        {
            self.0.finish();
        }
    }
}
impl PasteCompletion {
    pub fn new(done: impl FnOnce() + Send + 'static) -> Self {
        Self(Arc::new(CompletionState {
            settlement: Mutex::new(Settlement {
                done: Some(Box::new(done)),
                settling: false,
            }),
            wake: Condvar::new(),
            holders: std::sync::atomic::AtomicUsize::new(1),
        }))
    }
    /// Releases processing after the paste budget even if a native callback stalls.
    pub fn with_timeout(
        timeout: std::time::Duration,
        done: impl FnOnce() + Send + 'static,
    ) -> Self {
        let completion = Self::new(done);
        let state = completion.0.clone();
        std::thread::spawn(move || {
            let pending = state.settlement.lock().unwrap_or_else(|p| p.into_inner());
            let (pending, _) = state
                .wake
                .wait_timeout_while(pending, timeout, |s| s.done.is_some())
                .unwrap_or_else(|p| p.into_inner());
            let expired = pending.done.is_some();
            drop(pending);
            if expired {
                log::warn!("Native paste timed out; releasing dictation processing");
                state.finish();
            }
        });
        completion
    }
    pub fn settle_with(&self, native: impl FnOnce()) {
        {
            let mut pending = self.0.settlement.lock().unwrap_or_else(|p| p.into_inner());
            if pending.done.is_none() || pending.settling {
                return;
            }
            pending.settling = true;
        }
        let _settle = SettleOnDrop(&self.0);
        native();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::referee::{Effect, Event, Referee};
    use std::sync::Arc;
    #[test]
    fn wbfix_delayed_capture_never_reads_a_paste_completed_after_the_press() {
        let fence = PasteFence::default();
        let pressed = fence.revision();
        fence.before_paste(Box::new(|| {}));
        assert!(
            !fence.reserve_capture_since(pressed),
            "late capture must reject intervening paste"
        );
        assert!(fence.reserve_capture());
        fence.release_capture();
    }
    #[test]
    fn wbfix_hung_native_settlement_releases_processing_within_its_budget() {
        let pipeline = Arc::new(Mutex::new(PipelineFence::default()));
        let generation = pipeline.lock().unwrap().begin();
        pipeline.lock().unwrap().queued(generation);
        let (done, wait) = std::sync::mpsc::channel();
        let state = pipeline.clone();
        let completion =
            PasteCompletion::with_timeout(std::time::Duration::from_millis(50), move || {
                let mut state = state.lock().unwrap();
                state.pasted(generation);
                done.send(state.finish(generation)).unwrap();
            });
        let (release, blocked) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            completion.settle_with(|| {
                blocked.recv().unwrap();
            })
        });
        assert!(
            wait.recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            "hung paste must release processing"
        );
        let mut state = pipeline.lock().unwrap();
        let new = state.begin();
        assert!(state.finish(new), "new dictation must remain usable");
        drop(state);
        release.send(()).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn wbfix_paste_completion_settles_on_drop_and_native_panic() {
        for panic in [false, true] {
            let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed = settled.clone();
            let completion =
                PasteCompletion::with_timeout(std::time::Duration::from_secs(5), move || {
                    observed.store(true, std::sync::atomic::Ordering::Release);
                });
            if panic {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    completion.settle_with(|| panic!("injected native panic"));
                }));
            }
            drop(completion);
            assert!(
                settled.load(std::sync::atomic::Ordering::Acquire),
                "paste must always settle"
            );
        }
    }
    #[test]
    fn wbfix_capture_does_not_hold_or_capture_an_active_paste() {
        let fence = Arc::new(PasteFence::default());
        let (entered, wait) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let paste = fence.clone();
        let job = std::thread::spawn(move || {
            paste.before_paste(Box::new(move || {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
            }))
        });
        wait.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let (reserved, result) = std::sync::mpsc::channel();
        let capture = fence.clone();
        let reader = std::thread::spawn(move || reserved.send(capture.reserve_capture()).unwrap());
        let early = result.recv_timeout(std::time::Duration::from_millis(150));
        release.send(()).unwrap();
        job.join().unwrap();
        reader.join().unwrap();
        assert!(!early.unwrap(), "active paste must refuse capture promptly");
        assert!(fence.reserve_capture());
        fence.release_capture();
    }
    #[test]
    fn deferred_native_paste_waits_for_capture_reserved_during_release() {
        let fence = Arc::new(PasteFence::default());
        let clipboard = Arc::new(Mutex::new("original clipboard"));
        assert!(fence.reserve_capture());
        let next_capture = fence.clone();
        fence.before_queue(Box::new(move || {
            assert!(next_capture.reserve_capture());
        }));
        let pasted = clipboard.clone();
        fence.before_paste(Box::new(move || {
            *pasted.lock().unwrap() = "Handy paste";
        }));
        fence.release_capture();
        assert_eq!(*clipboard.lock().unwrap(), "original clipboard");
        assert!(!fence.reserve_capture());
        fence.release_capture();
        assert_eq!(*clipboard.lock().unwrap(), "Handy paste");
        assert!(fence.reserve_capture());
        fence.release_capture();
    }
    #[test]
    fn immediate_queue_callback_can_enter_native_paste_fence() {
        let fence = Arc::new(PasteFence::default());
        let native_fence = fence.clone();
        let (completed, completion) = std::sync::mpsc::channel();
        let queue = std::thread::spawn(move || {
            fence.before_queue(Box::new(move || {
                native_fence.before_paste(Box::new(move || completed.send(()).unwrap()));
            }));
        });
        completion
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        queue.join().unwrap();
    }
    #[test]
    fn read_during_post_processing_waits_for_paste_and_keeps_original_clipboard() {
        let fence = Arc::new(PasteFence::default());
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut referee = Referee::default();
        referee.apply(Event::DictationStart);
        referee.apply(Event::DictationProcessing);
        let effects = referee.apply(Event::ReadShortcut);
        let Effect::Capture { ticket } = effects[0] else {
            panic!()
        };
        assert!(fence.reserve_capture());
        let copied = trace.clone();
        fence.before_queue(Box::new(move || copied.lock().unwrap().push("Handy paste")));
        assert!(trace.lock().unwrap().is_empty());
        assert!(referee.apply(Event::DictationFinished).is_empty());
        let opened = referee.apply(Event::Clipboard {
            ticket,
            text: "original clipboard".into(),
        });
        assert_eq!(
            opened,
            [Effect::OpenPopup {
                text: "original clipboard".into()
            }]
        );
        trace.lock().unwrap().push("clipboard captured");
        fence.release_capture();
        assert_eq!(
            *trace.lock().unwrap(),
            ["clipboard captured", "Handy paste"]
        );
    }
    #[test]
    fn either_paste_fence_layer_protects_capture_reserved_before_queueing() {
        let fence = Arc::new(PasteFence::default());
        let trace = Arc::new(Mutex::new(Vec::new()));
        assert!(fence.reserve_capture());
        let native_fence = fence.clone();
        let completed = trace.clone();
        fence.before_queue(Box::new(move || {
            native_fence.before_paste(Box::new(move || completed.lock().unwrap().push("pasted")));
        }));
        assert!(trace.lock().unwrap().is_empty());
        trace.lock().unwrap().push("captured");
        fence.release_capture();
        assert_eq!(*trace.lock().unwrap(), ["captured", "pasted"]);
    }
    #[test]
    fn capture_reserved_after_native_queue_still_defers_actual_paste() {
        let fence = Arc::new(PasteFence::default());
        let native = Arc::new(Mutex::new(Vec::<Job>::new()));
        let queued = native.clone();
        let inner = fence.clone();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let result = trace.clone();
        fence.before_queue(Box::new(move || {
            queued.lock().unwrap().push(Box::new(move || {
                inner.before_paste(Box::new(move || result.lock().unwrap().push("pasted")))
            }))
        }));
        assert!(fence.reserve_capture());
        let run = native.lock().unwrap().pop().unwrap();
        run();
        assert!(trace.lock().unwrap().is_empty());
        fence.release_capture();
        assert_eq!(*trace.lock().unwrap(), ["pasted"]);
        assert!(fence.reserve_capture());
        assert!(!fence.reserve_capture());
        fence.release_capture();
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::referee::{Effect, Event, Referee};
    use std::sync::Arc;
    #[test]
    fn actual_paste_completion_releases_processing_once_and_old_pipeline_cannot_finish_a_new_dictation()
     {
        let mut f = PipelineFence::default();
        let old = f.begin();
        f.queued(old);
        assert!(!f.finish(old));
        f.pasted(old);
        assert!(f.finish(old));
        assert!(!f.finish(old));
        let new = f.begin();
        f.pasted(old);
        assert!(!f.finish(old));
        assert!(f.finish(new));
        let current = f.begin();
        f.queued(current);
        f.pasted(old);
        assert!(f.paste_queued);
        assert!(!f.finish(current));
        f.pasted(current);
        assert!(f.finish(current));
    }
    #[test]
    fn fix1_deferred_paste_returns_to_native_executor_after_capture_release() {
        let fence = Arc::new(PasteFence::default());
        let jobs = Arc::new(Mutex::new(std::collections::VecDeque::<Job>::new()));
        let queued = jobs.clone();
        let executor: NativeQueue = Arc::new(move |job| {
            queued.lock().unwrap().push_back(job);
            Ok(())
        });
        let (done, wait) = std::sync::mpsc::channel();
        fence.queue_native(
            executor,
            Box::new(move || done.send(std::thread::current().id()).unwrap()),
            Arc::new(|e| panic!("native queue failed: {e}")),
        );
        assert!(fence.reserve_capture());
        jobs.lock().unwrap().pop_front().unwrap()();
        assert!(wait.try_recv().is_err());
        let captured = fence.clone();
        std::thread::spawn(move || captured.release_capture())
            .join()
            .unwrap();
        assert!(
            wait.try_recv().is_err(),
            "capture worker must only requeue native paste"
        );
        assert!(fence.reserve_capture());
        jobs.lock().unwrap().pop_front().unwrap()();
        assert!(
            wait.try_recv().is_err(),
            "native replay must recheck a newer capture"
        );
        let captured = fence.clone();
        std::thread::spawn(move || captured.release_capture())
            .join()
            .unwrap();
        jobs.lock().unwrap().pop_front().unwrap()();
        assert_eq!(wait.recv().unwrap(), std::thread::current().id());
    }
    #[test]
    fn fix1_reliable_paste_waits_for_native_settlement_before_opening_popup() {
        let referee = Arc::new(Mutex::new(Referee::default()));
        let pipeline = Arc::new(Mutex::new(PipelineFence::default()));
        let generation = pipeline.lock().unwrap().begin();
        pipeline.lock().unwrap().queued(generation);
        referee.lock().unwrap().apply(Event::DictationStart);
        referee.lock().unwrap().apply(Event::DictationProcessing);
        let Effect::Capture { ticket } = referee.lock().unwrap().apply(Event::ReadShortcut)[0]
        else {
            panic!()
        };
        referee.lock().unwrap().apply(Event::Clipboard {
            ticket,
            text: "original".into(),
        });
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = stages.clone();
        let p = pipeline.clone();
        let r = referee.clone();
        let completion = PasteCompletion::new(move || {
            assert_eq!(
                *observed.lock().unwrap(),
                vec!["receipt", "auto-submit", "restore"]
            );
            let mut pipeline = p.lock().unwrap();
            pipeline.pasted(generation);
            assert!(pipeline.finish(generation));
            assert_eq!(
                r.lock().unwrap().apply(Event::DictationFinished),
                vec![Effect::OpenPopup {
                    text: "original".into()
                }]
            );
        });
        assert!(!pipeline.lock().unwrap().finish(generation));
        assert!(!referee.lock().unwrap().popup_open);
        stages.lock().unwrap().push("receipt");
        completion.settle_with(|| {
            stages.lock().unwrap().extend(["auto-submit", "restore"]);
        });
        completion.settle_with(|| panic!("settlement must happen once"));
        assert!(pipeline.lock().unwrap().finished);
        assert!(referee.lock().unwrap().popup_open);
    }
}
