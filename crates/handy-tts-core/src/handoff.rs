use std::sync::Mutex;

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
    deferred: Vec<DeferredJob>,
}
#[derive(Default)]
/// Reserves clipboard capture ahead of native paste.
pub struct PasteFence {
    state: Mutex<FenceState>,
}
impl PasteFence {
    /// Reserves capture unless another capture already owns the fence.
    pub fn reserve_capture(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.capture {
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
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.capture {
            state.deferred.push(DeferredJob::Paste(job));
        } else {
            job();
        }
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
                let mut state = fence.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.capture {
                    let replay = fence.clone();
                    state.deferred.push(DeferredJob::Queue(Box::new(move || {
                        replay.queue_native(replay_queue, job, replay_failed);
                    })));
                } else {
                    job();
                }
            });
            if let Err(error) = queue(guarded) {
                failed(error);
            }
        }));
    }
    /// Serializes actual paste with capture reservation; callbacks must not re-enter.
    pub fn before_paste(&self, job: Job) {
        self.dispatch(job);
    }
}

#[derive(Clone)]
/// Completes a paste only after its native settlement has finished.
pub struct PasteCompletion(std::sync::Arc<Mutex<Option<Job>>>);
impl PasteCompletion {
    pub fn new(done: impl FnOnce() + Send + 'static) -> Self {
        Self(std::sync::Arc::new(Mutex::new(Some(Box::new(done)))))
    }
    pub fn settle_with(&self, native: impl FnOnce()) {
        let done = self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(done) = done {
            native();
            done();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::referee::{Effect, Event, Referee};
    use std::sync::Arc;
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
