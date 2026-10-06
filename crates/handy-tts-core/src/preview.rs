use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub trait ReadingControl {
    fn pause_for_preview(&self) -> Result<(), String>;
}
struct Lease {
    cancelled: Arc<AtomicBool>,
    stop: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}
#[derive(Default)]
pub struct PreviewCoordinator {
    current: Mutex<Option<Lease>>,
}
impl PreviewCoordinator {
    pub fn begin(
        &self,
        reading: &impl ReadingControl,
        stop: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Result<Arc<AtomicBool>, String> {
        reading.pause_for_preview()?;
        self.cancel()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(Lease {
            cancelled: cancelled.clone(),
            stop,
        });
        Ok(cancelled)
    }
    pub fn cancel(&self) -> Result<(), String> {
        let lease = self
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(lease) = lease {
            lease.cancelled.store(true, Ordering::Release);
            (lease.stop)()?;
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
    pub fn reading_command(
        &self,
        command: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.cancel()?;
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
}
