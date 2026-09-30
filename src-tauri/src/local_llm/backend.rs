//! The loader seam: one trait per model runtime. v1 has `LlamaServerBackend`.

use super::prompt::ChatMessage;
use super::registry::ModelEntry;
use super::LocalLlmError;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// How to start a model.
#[derive(Debug, Clone)]
pub struct BackendOpts {
    pub model_path: PathBuf,
    pub threads: u8,
    pub ctx: u32,
    /// Hard deadline for the whole start, including readiness.
    pub start_timeout: Duration,
    /// Set on app exit; a start in progress must abort promptly.
    pub cancel: Arc<AtomicBool>,
}

/// One chat completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateRequest {
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub timeout: Duration,
}

/// Thread-safe handle that forcibly stops the backend's running child, callable while another
/// thread holds the backend for a blocking call.
pub trait KillSwitch: Send + Sync {
    fn trigger(&self);
}

/// A text-model runtime driven by `LocalLlmManager` under its mutex.
pub trait TextModelBackend: Send {
    /// Returns a handle independent of the backend's exclusive request lock.
    fn kill_switch(&self) -> Arc<dyn KillSwitch>;
    /// Starts the model if it is not already running with `opts.model_path`.
    fn ensure_loaded(
        &mut self,
        model: &ModelEntry,
        opts: &BackendOpts,
    ) -> Result<(), LocalLlmError>;
    /// Runs one completion and returns the raw assistant text.
    fn generate(&mut self, req: &GenerateRequest) -> Result<String, LocalLlmError>;
    /// Stops the model and frees its memory. Idempotent.
    fn unload(&mut self);
    /// `false` once the model process has exited.
    fn is_alive(&mut self) -> bool;
}
