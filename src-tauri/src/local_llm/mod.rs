//! Local in-app LLM post-processing: a Handy-managed `llama-server` child
//! process cleans transcripts with a small CPU model (S1-mini first).

pub mod backend;
pub mod llama_server;
pub mod prompt;
pub mod registry;

/// Every way local post-processing can fail. Each one falls back to the raw transcript.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocalLlmError {
    #[error("llama-server binary not found at {0}")]
    BinaryMissing(String),
    #[error("model file missing: {0}")]
    ModelMissing(String),
    #[error("model file corrupt: {0}")]
    ModelCorrupt(String),
    #[error("llama-server did not become healthy before the start deadline")]
    StartTimeout,
    #[error("llama-server failed to start: {0}")]
    StartFailed(String),
    #[error("request timed out")]
    RequestTimeout,
    #[error("llama-server returned HTTP {0}")]
    Http(u16),
    #[error("request failed: {0}")]
    Transport(String),
    #[error("output rejected: {0}")]
    BadOutput(String),
    #[error("local model unavailable: {0}")]
    Failed(String),
}
