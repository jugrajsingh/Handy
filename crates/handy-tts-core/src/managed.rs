use crate::lifecycle::{Lifecycle, State, UnloadAfter};
use std::time::Duration;

pub trait ProcessBackend {
    type Error;
    fn start(&mut self) -> Result<(), Self::Error>;
    fn stop(&mut self);
    fn alive(&mut self) -> bool;
}
pub struct Managed<B: ProcessBackend> {
    pub backend: B,
    pub lifecycle: Lifecycle,
}
#[derive(Debug, PartialEq, Eq)]
pub enum StartError<E> {
    Guard(&'static str),
    Backend(E),
}
impl<B: ProcessBackend> Managed<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            lifecycle: Lifecycle::default(),
        }
    }
    pub fn ensure_started(
        &mut self,
        available: u64,
        now: Duration,
    ) -> Result<(), StartError<B::Error>> {
        if self.lifecycle.state == State::Ready && self.backend.alive() {
            return Ok(());
        }
        self.lifecycle
            .begin_start(available)
            .map_err(StartError::Guard)?;
        match self.backend.start() {
            Ok(()) => {
                self.lifecycle.ready(now);
                Ok(())
            }
            Err(error) => {
                self.backend.stop();
                self.lifecycle.start_failed();
                Err(StartError::Backend(error))
            }
        }
    }
    pub fn unload(&mut self) {
        self.lifecycle.stopping();
        self.backend.stop();
        self.lifecycle.stopped();
    }
    pub fn idle_tick(&mut self, now: Duration, timeout: UnloadAfter) {
        if self.lifecycle.needs_unload(now, timeout) {
            self.unload();
        }
    }
}
impl<B: ProcessBackend> Drop for Managed<B> {
    fn drop(&mut self) {
        self.backend.stop();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::MIN_AVAILABLE_BYTES;
    use std::process::{Child, Command};
    struct Sleeper {
        child: Option<Child>,
        starts: usize,
        fail: bool,
    }
    impl ProcessBackend for Sleeper {
        type Error = std::io::Error;
        fn start(&mut self) -> Result<(), Self::Error> {
            self.starts += 1;
            if self.fail {
                return Err(std::io::Error::other("fake start failure"));
            }
            self.child = Some(Command::new("sleep").arg("60").spawn()?);
            Ok(())
        }
        fn stop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        fn alive(&mut self) -> bool {
            self.child
                .as_mut()
                .is_some_and(|c| c.try_wait().is_ok_and(|status| status.is_none()))
        }
    }
    #[test]
    fn unload_and_idle_unload_kill_and_reap_a_real_child() {
        let mut m = Managed::new(Sleeper {
            child: None,
            starts: 0,
            fail: false,
        });
        for manual in [true, false] {
            m.ensure_started(MIN_AVAILABLE_BYTES, Duration::ZERO)
                .unwrap();
            let pid = m.backend.child.as_ref().unwrap().id();
            assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
            if manual {
                m.unload();
            } else {
                m.idle_tick(
                    Duration::from_secs(60),
                    UnloadAfter::Idle(Duration::from_secs(60)),
                );
            }
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
            assert_eq!(m.lifecycle.state, State::Unloaded);
            assert!(m.backend.child.is_none());
        }
    }
    #[test]
    fn failed_starts_stop_the_backend_and_refuse_fourth_attempt() {
        let mut m = Managed::new(Sleeper {
            child: None,
            starts: 0,
            fail: true,
        });
        for _ in 0..3 {
            assert!(
                m.ensure_started(MIN_AVAILABLE_BYTES, Duration::ZERO)
                    .is_err()
            );
        }
        assert_eq!(m.lifecycle.state, State::Failed);
        assert!(matches!(
            m.ensure_started(MIN_AVAILABLE_BYTES, Duration::ZERO),
            Err(StartError::Guard("failed_starts"))
        ));
        assert_eq!(m.backend.starts, 3);
        m.lifecycle.selected();
        m.backend.fail = false;
        m.ensure_started(MIN_AVAILABLE_BYTES, Duration::ZERO)
            .unwrap();
        m.unload();
    }
}
