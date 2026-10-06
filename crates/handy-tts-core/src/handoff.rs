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

type Job = Box<dyn FnOnce() + Send>;
#[derive(Default)]
struct FenceState {
    capture: bool,
    deferred: Vec<Job>,
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
    /// Releases capture and runs deferred work outside the fence lock.
    pub fn release_capture(&self) {
        let jobs = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.capture = false;
            std::mem::take(&mut state.deferred)
        };
        for job in jobs {
            job();
        }
    }
    fn dispatch(&self, job: Job) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.capture {
            state.deferred.push(job);
        } else {
            job();
        }
    }
    /// Defers queueing during capture; queue callbacks may enter the paste fence.
    pub fn before_queue(&self, job: Job) {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.capture {
                state.deferred.push(job);
                return;
            }
        }
        job();
    }
    /// Serializes actual paste with capture reservation; callbacks must not re-enter.
    pub fn before_paste(&self, job: Job) {
        self.dispatch(job);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::referee::{Effect, Event, Referee};
    use std::sync::Arc;
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
}
