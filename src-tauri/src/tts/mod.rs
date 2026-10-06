pub mod backend;
pub mod manager;
pub mod playback;
pub use manager::TtsManager;
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TtsError {
    #[error("tts.errors.clipboard")]
    Clipboard,
    #[error("tts.errors.cancelled")]
    Cancelled,
    #[error(transparent)]
    Device(#[from] handy_tts_core::device::DeviceError),
    #[error("tts.errors.unknownModel")]
    Model,
    #[error("tts.errors.notDownloaded")]
    NotDownloaded,
    #[error("tts.errors.invalidVoice")]
    Voice,
    #[error("tts.errors.start")]
    Start,
    #[error("tts.errors.worker")]
    Worker,
    #[error("tts.errors.protocol")]
    Protocol,
    #[error("tts.errors.memory")]
    Memory,
    #[error("tts.errors.failed")]
    Failed,
    #[error("tts.errors.audio")]
    Audio,
    #[error("tts.errors.generic")]
    Generic,
}
pub fn memory_available() -> Result<u64, TtsError> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").map_err(|_| TtsError::Memory)?;
        let line = text
            .lines()
            .find(|line| line.starts_with("MemAvailable:"))
            .ok_or(TtsError::Memory)?;
        line.split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|value| value.checked_mul(1024))
            .ok_or(TtsError::Memory)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(u64::MAX)
    }
}
pub fn unload_after(
    timeout: crate::settings::ModelUnloadTimeout,
) -> handy_tts_core::lifecycle::UnloadAfter {
    use crate::settings::ModelUnloadTimeout::*;
    use handy_tts_core::lifecycle::UnloadAfter;
    use std::time::Duration;
    match timeout {
        Never => UnloadAfter::Never,
        Immediately => UnloadAfter::Immediately,
        Min2 => UnloadAfter::Idle(Duration::from_secs(120)),
        Min5 => UnloadAfter::Idle(Duration::from_secs(300)),
        Min10 => UnloadAfter::Idle(Duration::from_secs(600)),
        Min15 => UnloadAfter::Idle(Duration::from_secs(900)),
        Hour1 => UnloadAfter::Idle(Duration::from_secs(3600)),
        Sec15 => UnloadAfter::Idle(Duration::from_secs(15)),
    }
}
pub mod session;
