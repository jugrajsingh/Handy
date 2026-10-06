use std::time::Duration;

pub use crate::memory::MIN_AVAILABLE_BYTES;
pub const MAX_FAILED_STARTS: u32 = 3;
#[derive(Debug, Clone, Copy, PartialEq, Eq, specta::Type, serde::Serialize, serde::Deserialize)]
pub enum State {
    Unloaded,
    Starting,
    Ready,
    Stopping,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadAfter {
    Never,
    Immediately,
    Idle(Duration),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    pub state: State,
    pub failed_starts: u32,
    pub last_used: Duration,
    pub busy: bool,
}
impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            state: State::Unloaded,
            failed_starts: 0,
            last_used: Duration::ZERO,
            busy: false,
        }
    }
}
impl Lifecycle {
    pub fn touch(&mut self, now: Duration) {
        self.last_used = now;
    }
    pub fn begin_start(&mut self, available: u64) -> Result<(), &'static str> {
        if self.state == State::Failed {
            return Err("failed_starts");
        }
        if available < MIN_AVAILABLE_BYTES {
            return Err("not_enough_memory");
        }
        self.state = State::Starting;
        Ok(())
    }
    pub fn ready(&mut self, now: Duration) {
        self.state = State::Ready;
        self.failed_starts = 0;
        self.last_used = now;
    }
    pub fn start_failed(&mut self) {
        self.failed_starts += 1;
        self.state = if self.failed_starts >= MAX_FAILED_STARTS {
            State::Failed
        } else {
            State::Unloaded
        };
    }
    pub fn selected(&mut self) {
        self.failed_starts = 0;
        self.state = State::Unloaded;
    }
    pub fn needs_unload(&self, now: Duration, timeout: UnloadAfter) -> bool {
        self.state == State::Ready
            && !self.busy
            && match timeout {
                UnloadAfter::Never => false,
                UnloadAfter::Immediately => true,
                UnloadAfter::Idle(limit) => now.saturating_sub(self.last_used) >= limit,
            }
    }
    pub fn stopping(&mut self) {
        self.state = State::Stopping;
    }
    pub fn stopped(&mut self) {
        self.state = State::Unloaded;
        self.busy = false;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn three_failures_lock_until_selection_and_success_resets_streak() {
        let mut l = Lifecycle::default();
        for count in 1..=3 {
            l.begin_start(MIN_AVAILABLE_BYTES).unwrap();
            l.start_failed();
            assert_eq!(l.failed_starts, count);
        }
        assert_eq!(l.state, State::Failed);
        assert_eq!(l.begin_start(MIN_AVAILABLE_BYTES), Err("failed_starts"));
        l.selected();
        l.begin_start(MIN_AVAILABLE_BYTES).unwrap();
        l.ready(Duration::from_secs(2));
        assert_eq!(l.failed_starts, 0);
        assert_eq!(l.state, State::Ready);
    }
    #[test]
    fn idle_uses_live_timeout_and_protects_active_and_paused_sessions() {
        let mut l = Lifecycle::default();
        l.ready(Duration::from_secs(5));
        assert!(!l.needs_unload(
            Duration::from_secs(64),
            UnloadAfter::Idle(Duration::from_secs(60))
        ));
        assert!(l.needs_unload(
            Duration::from_secs(65),
            UnloadAfter::Idle(Duration::from_secs(60))
        ));
        assert!(!l.needs_unload(Duration::from_secs(600), UnloadAfter::Never));
        assert!(l.needs_unload(Duration::from_secs(5), UnloadAfter::Immediately));
        l.busy = true;
        assert!(!l.needs_unload(Duration::from_secs(600), UnloadAfter::Immediately));
    }
    #[test]
    fn memory_guard_is_inclusive_and_does_not_count_as_a_start_failure() {
        let mut l = Lifecycle::default();
        assert_eq!(
            l.begin_start(MIN_AVAILABLE_BYTES - 1),
            Err("not_enough_memory")
        );
        assert_eq!(l.failed_starts, 0);
        assert_eq!(l.state, State::Unloaded);
        assert!(l.begin_start(MIN_AVAILABLE_BYTES).is_ok());
    }
}
