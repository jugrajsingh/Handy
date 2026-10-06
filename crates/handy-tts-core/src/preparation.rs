use crate::device::{Device, Preparation, RunOn};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub trait Compilation: Send + Sync + 'static {
    fn run(&self, cancelled: &AtomicBool) -> Result<(), String>;
    fn snapshot(&self) -> Preparation;
    fn stop(&self);
}
struct Entry {
    choice: RunOn,
    job: Arc<dyn Compilation>,
    cancelled: AtomicBool,
    finished: Mutex<Option<Preparation>>,
}
#[derive(Default)]
pub struct Preparer {
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    serial: Arc<Mutex<()>>,
}
impl Preparer {
    pub fn request(&self, model: &str, choice: RunOn, job: Arc<dyn Compilation>) -> bool {
        let entry = Arc::new(Entry {
            choice,
            job,
            cancelled: AtomicBool::new(false),
            finished: Mutex::new(None),
        });
        {
            let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            if entries.get(model).is_some_and(|old| {
                old.choice == choice
                    && !old.cancelled.load(Ordering::Acquire)
                    && !matches!(
                        *old.finished.lock().unwrap_or_else(|p| p.into_inner()),
                        Some(Preparation::Failed { .. })
                    )
            }) {
                return false;
            }
            if let Some(old) = entries.insert(model.into(), entry.clone()) {
                old.cancelled.store(true, Ordering::Release);
                old.job.stop();
            }
        }
        let serial = self.serial.clone();
        std::thread::spawn(move || {
            let _slot = serial.lock().unwrap_or_else(|p| p.into_inner());
            if entry.cancelled.load(Ordering::Acquire) {
                return;
            }
            let result = entry.job.run(&entry.cancelled);
            let state = entry.job.snapshot();
            let state = match (result, state) {
                (Ok(()), ready @ Preparation::Ready { .. }) => ready,
                (_, failed @ Preparation::Failed { .. }) => failed,
                (Err(error), _) => Preparation::Failed {
                    device: Device::Cpu,
                    error,
                },
                _ => Preparation::Failed {
                    device: Device::Cpu,
                    error: "tts.errors.prepareFailed".into(),
                },
            };
            *entry.finished.lock().unwrap_or_else(|p| p.into_inner()) = Some(state);
            entry.job.stop();
        });
        true
    }
    pub fn snapshot(&self, model: &str) -> Preparation {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(model)
            .cloned();
        entry.map_or(Preparation::Idle, |entry| {
            entry
                .finished
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_else(|| entry.job.snapshot())
        })
    }
    pub fn cancel(&self, model: &str) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(model)
        {
            entry.cancelled.store(true, Ordering::Release);
            entry.job.stop();
        }
    }
    pub fn shutdown(&self) {
        let entries = std::mem::take(&mut *self.entries.lock().unwrap_or_else(|p| p.into_inner()));
        for entry in entries.into_values() {
            entry.cancelled.store(true, Ordering::Release);
            entry.job.stop();
        }
    }
}
pub fn visible_preparation(
    active: bool,
    foreground: &Preparation,
    background: &Preparation,
) -> Preparation {
    if active
        && matches!(
            foreground,
            Preparation::Preparing { .. } | Preparation::Failed { .. }
        )
    {
        foreground.clone()
    } else if !matches!(background, Preparation::Idle) {
        background.clone()
    } else if active {
        foreground.clone()
    } else {
        Preparation::Idle
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Fake {
        calls: AtomicUsize,
        stops: AtomicUsize,
        entered: AtomicBool,
        released: AtomicBool,
        failed: AtomicBool,
    }
    impl Compilation for Fake {
        fn run(&self, cancelled: &AtomicBool) -> Result<(), String> {
            if cancelled.load(Ordering::Acquire) {
                return Err("tts.errors.cancelled".into());
            }
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.entered.store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !self.released.load(Ordering::Acquire) {
                if std::time::Instant::now() >= deadline {
                    return Err("fixture timed out".into());
                }
                std::thread::yield_now();
            }
            if self.failed.load(Ordering::Acquire) {
                Err("tts.errors.prepareFailed".into())
            } else {
                Ok(())
            }
        }
        fn snapshot(&self) -> Preparation {
            if self.failed.load(Ordering::Acquire) {
                Preparation::Failed {
                    device: Device::Npu,
                    error: "tts.errors.prepareFailed".into(),
                }
            } else if self.released.load(Ordering::Acquire) {
                Preparation::Ready {
                    device: Device::Npu,
                }
            } else {
                Preparation::Preparing {
                    device: Device::Npu,
                    completed: 1,
                    total: 4,
                    graph: "text_encoder".into(),
                }
            }
        }
        fn stop(&self) {
            self.stops.fetch_add(1, Ordering::AcqRel);
            self.released.store(true, Ordering::Release);
        }
    }
    fn fake() -> Arc<Fake> {
        Arc::new(Fake {
            calls: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        })
    }
    fn until(condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !condition() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    #[test]
    fn background_progress_is_visible_while_the_active_worker_is_ready() {
        let foreground = Preparation::Ready {
            device: Device::Cpu,
        };
        let background = Preparation::Preparing {
            device: Device::Npu,
            completed: 1,
            total: 4,
            graph: "text_encoder".into(),
        };
        assert_eq!(
            visible_preparation(true, &foreground, &background),
            background
        );
        assert_eq!(
            visible_preparation(true, &foreground, &Preparation::Idle),
            foreground
        );
        assert_eq!(
            visible_preparation(false, &foreground, &Preparation::Idle),
            Preparation::Idle
        );
    }
    #[test]
    fn failed_same_device_preparation_can_be_requested_again() {
        let preparer = Preparer::default();
        let first = fake();
        first.failed.store(true, Ordering::Release);
        first.released.store(true, Ordering::Release);
        assert!(preparer.request("supertonic-3", RunOn::Npu, first.clone()));
        until(|| first.stops.load(Ordering::Acquire) == 1);
        assert!(matches!(
            preparer.snapshot("supertonic-3"),
            Preparation::Failed { .. }
        ));
        let retry = fake();
        assert!(preparer.request("supertonic-3", RunOn::Npu, retry.clone()));
        until(|| retry.entered.load(Ordering::Acquire));
        preparer.shutdown();
        until(|| retry.stops.load(Ordering::Acquire) >= 1);
    }
    #[test]
    fn inactive_compile_is_serial_cached_cancelled_and_reaped_without_touching_reading() {
        let preparer = Preparer::default();
        let reading = fake();
        let first = fake();
        assert!(preparer.request("supertonic-3", RunOn::Npu, first.clone()));
        until(|| first.entered.load(Ordering::Acquire));
        assert!(matches!(
            preparer.snapshot("supertonic-3"),
            Preparation::Preparing {
                completed: 1,
                total: 4,
                ..
            }
        ));
        assert!(!preparer.request("supertonic-3", RunOn::Npu, fake()));
        let queued = fake();
        assert!(preparer.request("pocket-en", RunOn::Gpu, queued.clone()));
        assert_eq!(queued.calls.load(Ordering::Acquire), 0);
        preparer.cancel("pocket-en");
        first.released.store(true, Ordering::Release);
        until(|| first.stops.load(Ordering::Acquire) == 1);
        assert!(matches!(
            preparer.snapshot("supertonic-3"),
            Preparation::Ready { .. }
        ));
        assert_eq!(queued.calls.load(Ordering::Acquire), 0);
        assert_eq!(reading.stops.load(Ordering::Acquire), 0);
        preparer.shutdown();
    }
}
