use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub trait ReadingControl {
    fn pause_for_preview(&self) -> Result<(), String>;
}
#[derive(Clone)]
struct Lease {
    cancelled: Arc<AtomicBool>,
    stop: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}
#[derive(Default)]
pub struct PreviewCoordinator {
    current: Mutex<Option<Lease>>,
    transitions: Mutex<()>,
    dictating: AtomicBool,
}
impl PreviewCoordinator {
    pub fn begin(
        &self,
        reading: &impl ReadingControl,
        stop: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Result<Arc<AtomicBool>, String> {
        let _transition = self.transitions.lock().unwrap_or_else(|p| p.into_inner());
        if self.dictating.load(Ordering::Acquire) {
            return Err("tts.errors.busy".into());
        }
        reading.pause_for_preview()?;
        self.cancel_current()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(Lease {
            cancelled: cancelled.clone(),
            stop,
        });
        Ok(cancelled)
    }
    /// Records microphone ownership before any preview cancellation.
    pub fn set_dictating(&self, active: bool) {
        self.dictating.store(active, Ordering::Release);
    }
    pub fn cancel(&self) -> Result<(), String> {
        let _transition = self.transitions.lock().unwrap_or_else(|p| p.into_inner());
        self.cancel_current()
    }
    fn cancel_current(&self) -> Result<(), String> {
        let lease = self
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(lease) = lease {
            lease.cancelled.store(true, Ordering::Release);
            (lease.stop)()?;
            self.finish(&lease.cancelled);
        }
        Ok(())
    }
    pub fn finish(&self, cancelled: &Arc<AtomicBool>) {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        if current
            .as_ref()
            .is_some_and(|lease| Arc::ptr_eq(&lease.cancelled, cancelled))
        {
            current.take();
        }
    }
    /// Sends Stop even when an active preview cannot acknowledge cancellation.
    pub fn stopping_command(
        &self,
        command: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let result = command();
        let cancelled = self.cancel();
        result.and(cancelled)
    }
    pub fn reading_command(
        &self,
        command: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let _transition = self.transitions.lock().unwrap_or_else(|p| p.into_inner());
        self.cancel_current()?;
        command()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Reading {
        document: String,
        paused: AtomicBool,
        fail_pause: bool,
    }
    impl ReadingControl for Reading {
        fn pause_for_preview(&self) -> Result<(), String> {
            if self.fail_pause {
                return Err("tts.errors.audio".into());
            }
            self.paused.store(true, Ordering::Release);
            Ok(())
        }
    }
    #[test]
    fn wbfix_failed_preview_cancellation_cannot_veto_stop() {
        let reading = Reading {
            document: "original".into(),
            paused: AtomicBool::new(false),
            fail_pause: false,
        };
        let coordinator = PreviewCoordinator::default();
        coordinator
            .begin(&reading, Arc::new(|| Err("stop pending".into())))
            .unwrap();
        let sent = AtomicBool::new(false);
        assert_eq!(
            coordinator.stopping_command(|| {
                sent.store(true, Ordering::Release);
                Ok(())
            }),
            Err("stop pending".into())
        );
        assert!(
            sent.load(Ordering::Acquire),
            "failed preview cancellation must still dispatch Stop"
        );
    }
    #[test]
    fn wbfix_preview_is_refused_throughout_dictation_and_reenabled_after_settlement() {
        let reading = Reading {
            document: "original".into(),
            paused: AtomicBool::new(false),
            fail_pause: false,
        };
        let coordinator = PreviewCoordinator::default();
        coordinator.set_dictating(true);
        assert_eq!(
            coordinator
                .begin(&reading, Arc::new(|| Ok(())))
                .unwrap_err(),
            "tts.errors.busy"
        );
        assert!(!reading.paused.load(Ordering::Acquire));
        coordinator.set_dictating(false);
        let flag = coordinator.begin(&reading, Arc::new(|| Ok(()))).unwrap();
        coordinator.set_dictating(true);
        coordinator.cancel().unwrap();
        assert!(flag.load(Ordering::Acquire));
        assert_eq!(
            coordinator
                .begin(&reading, Arc::new(|| Ok(())))
                .unwrap_err(),
            "tts.errors.busy"
        );
    }
    #[test]
    fn preview_preserves_document_and_play_cancels_before_reading_without_stale_completion() {
        let reading = Reading {
            document: "The owner's original document.".into(),
            paused: AtomicBool::new(false),
            fail_pause: false,
        };
        let preview = PreviewCoordinator::default();
        let stops = Arc::new(AtomicUsize::new(0));
        let stop = {
            let stops = stops.clone();
            Arc::new(move || {
                stops.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }) as Arc<dyn Fn() -> Result<(), String> + Send + Sync>
        };
        let first = preview.begin(&reading, stop.clone()).unwrap();
        assert!(reading.paused.load(Ordering::Acquire));
        assert_eq!(reading.document, "The owner's original document.");
        let second = preview.begin(&reading, stop).unwrap();
        assert!(first.load(Ordering::Acquire));
        assert!(!second.load(Ordering::Acquire));
        preview.finish(&first);
        let result = preview.reading_command(|| {
            assert!(second.load(Ordering::Acquire));
            assert_eq!(stops.load(Ordering::Acquire), 2);
            assert_eq!(reading.document, "The owner's original document.");
            Ok(())
        });
        assert_eq!(result, Ok(()));
        preview.cancel().unwrap();
        assert_eq!(stops.load(Ordering::Acquire), 2);
    }
    #[test]
    fn failed_preview_stop_acknowledgement_prevents_playback_overlap() {
        let reading = Reading {
            document: "Keep this text".into(),
            paused: AtomicBool::new(false),
            fail_pause: false,
        };
        let coordinator = PreviewCoordinator::default();
        let cancelled = coordinator
            .begin(&reading, Arc::new(|| Err("tts.errors.generic".into())))
            .unwrap();
        let mut played = false;
        let result = coordinator.reading_command(|| {
            played = true;
            Ok(())
        });
        assert_eq!(result, Err("tts.errors.generic".into()));
        assert!(!played);
        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(reading.document, "Keep this text");
    }
    #[test]
    fn failed_pause_does_not_launch_preview_and_finishing_releases_only_current_lease() {
        let reading = Reading {
            document: "Text remains here.".into(),
            paused: AtomicBool::new(false),
            fail_pause: true,
        };
        let preview = PreviewCoordinator::default();
        assert_eq!(
            preview.begin(&reading, Arc::new(|| Ok(()))).unwrap_err(),
            "tts.errors.audio"
        );
        assert!(preview.current.lock().unwrap().is_none());
        let reading = Reading {
            fail_pause: false,
            ..reading
        };
        let flag = preview.begin(&reading, Arc::new(|| Ok(()))).unwrap();
        preview.finish(&flag);
        assert!(preview.current.lock().unwrap().is_none());
        assert!(!flag.load(Ordering::Acquire));
    }
    #[test]
    fn fix1_failed_stop_retains_lease_for_every_competing_command() {
        let reading = Reading {
            document: "Retain the document".into(),
            paused: AtomicBool::new(false),
            fail_pause: false,
        };
        let coordinator = PreviewCoordinator::default();
        let stops = Arc::new(AtomicUsize::new(0));
        let trace = stops.clone();
        coordinator
            .begin(
                &reading,
                Arc::new(move || {
                    trace.fetch_add(1, Ordering::AcqRel);
                    Err("stop pending".into())
                }),
            )
            .unwrap();
        for _ in 0..2 {
            assert_eq!(
                coordinator.reading_command(|| panic!("reading overlapped preview")),
                Err("stop pending".into())
            );
        }
        assert_eq!(stops.load(Ordering::Acquire), 2);
    }
    #[test]
    fn fix1_play_and_close_wait_for_stop_without_blocking_completion() {
        let reading = Reading {
            document: "Keep the original".into(),
            paused: AtomicBool::new(false),
            fail_pause: false,
        };
        let coordinator = Arc::new(PreviewCoordinator::default());
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let first = entered.clone();
        let second = release.clone();
        let stopping = AtomicBool::new(false);
        let flag = coordinator
            .begin(
                &reading,
                Arc::new(move || {
                    if stopping.swap(true, Ordering::AcqRel) {
                        return Err("concurrent stop before acknowledgement".into());
                    }
                    first.wait();
                    second.wait();
                    Ok(())
                }),
            )
            .unwrap();
        let closing = coordinator.clone();
        let close = std::thread::spawn(move || closing.cancel());
        entered.wait();
        let playing = coordinator.clone();
        let (done, wait) = std::sync::mpsc::channel();
        let play = std::thread::spawn(move || {
            playing.reading_command(|| {
                done.send(()).unwrap();
                Ok(())
            })
        });
        let premature = wait
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        coordinator.finish(&flag);
        release.wait();
        close.join().unwrap().unwrap();
        play.join().unwrap().unwrap();
        assert!(!premature, "Play must wait for acknowledged stop");
        wait.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
    }
    #[test]
    fn fix1_overlapping_begins_keep_one_owned_preview() {
        let coordinator = Arc::new(PreviewCoordinator::default());
        let pauses = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        struct Pausing {
            calls: Arc<AtomicUsize>,
            entered: Arc<std::sync::Barrier>,
            release: Arc<std::sync::Barrier>,
        }
        impl ReadingControl for Pausing {
            fn pause_for_preview(&self) -> Result<(), String> {
                if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                    self.entered.wait();
                    self.release.wait();
                }
                Ok(())
            }
        }
        let first = coordinator.clone();
        let calls = pauses.clone();
        let gate = entered.clone();
        let go = release.clone();
        let a = std::thread::spawn(move || {
            first
                .begin(
                    &Pausing {
                        calls,
                        entered: gate,
                        release: go,
                    },
                    Arc::new(|| Ok(())),
                )
                .unwrap()
        });
        entered.wait();
        let second = coordinator.clone();
        let calls = pauses.clone();
        let gate = entered.clone();
        let go = release.clone();
        let (done, wait) = std::sync::mpsc::channel();
        let b = std::thread::spawn(move || {
            let flag = second
                .begin(
                    &Pausing {
                        calls,
                        entered: gate,
                        release: go,
                    },
                    Arc::new(|| Ok(())),
                )
                .unwrap();
            done.send(()).unwrap();
            flag
        });
        let premature = wait
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        release.wait();
        let a = a.join().unwrap();
        let b = b.join().unwrap();
        assert!(!premature, "begin transitions must be serialized");
        assert!(a.load(Ordering::Acquire));
        assert!(!b.load(Ordering::Acquire));
        coordinator.cancel().unwrap();
        assert!(b.load(Ordering::Acquire));
    }
}
