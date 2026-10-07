pub mod backend_config;
pub mod cancellation;
pub mod graph;
use graph::session;
pub mod devices;
mod kokoro;
pub mod ov_graph;
pub mod padding;
mod pocket;
pub mod runtime;
mod supertonic;
use anyhow::{Context, Result, bail};
use ort::{
    execution_providers::CPUExecutionProvider,
    session::{Session, builder::GraphOptimizationLevel},
};
use std::path::Path;
fn ort_session(path: &Path, threads: usize) -> Result<Session> {
    Session::builder()
        .context("create ORT session builder")?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_intra_threads(threads)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_inter_threads(1)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_execution_providers([CPUExecutionProvider::default().build()])
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .commit_from_file(path)
        .with_context(|| format!("load ONNX graph {}", path.display()))
}

/// Dispatches a validated native speed before any model inference.
pub(crate) fn synth_native(
    model: &str,
    speed: Option<f32>,
    synthesize: impl FnOnce(f32) -> Result<Vec<f32>>,
) -> Result<Vec<f32>> {
    let native = speed.unwrap_or(1.0);
    anyhow::ensure!((0.5..=2.0).contains(&native), "invalid speed");
    anyhow::ensure!(
        model != "pocket-en" || native == 1.0,
        "Pocket requires native speed 1.0"
    );
    synthesize(native)
}

pub enum Engine {
    Pocket(Box<pocket::Pocket>),
    Supertonic(Box<supertonic::Supertonic>),
    Kokoro(Box<kokoro::Kokoro>),
}
impl Engine {
    pub fn load(id: &str, root: &Path, threads: usize, voice: &str) -> Result<Self> {
        match id {
            "pocket-en" => Ok(Self::Pocket(Box::new(pocket::Pocket::load(
                root, threads, voice,
            )?))),
            "supertonic-3" => Ok(Self::Supertonic(Box::new(supertonic::Supertonic::load(
                root, threads, voice,
            )?))),
            "kokoro-82m-int8" => Ok(Self::Kokoro(Box::new(kokoro::Kokoro::load(
                root, threads, voice,
            )?))),
            _ => bail!("unknown model id"),
        }
    }
    pub fn synth(&mut self, text: &str, speed: Option<f32>) -> Result<Vec<f32>> {
        match self {
            Self::Pocket(e) => synth_native("pocket-en", speed, |native| e.synth(text, native)),
            Self::Supertonic(e) => {
                synth_native("supertonic-3", speed, |native| e.synth(text, native))
            }
            Self::Kokoro(e) => {
                synth_native("kokoro-82m-int8", speed, |native| e.synth(text, native))
            }
        }
    }
    pub fn voice(&mut self, root: &Path, voice: &str) -> Result<()> {
        match self {
            Self::Pocket(e) => e.voice(root, voice),
            Self::Supertonic(e) => e.voice(root, voice),
            Self::Kokoro(e) => e.voice(root, voice),
        }
    }
    pub fn sample_rate(&self) -> u32 {
        match self {
            Self::Supertonic(e) => e.rate(),
            _ => 24000,
        }
    }
}

static RUN_OPTIONS: std::sync::OnceLock<ort::session::RunOptions> = std::sync::OnceLock::new();
pub fn initialize_run_options() -> Result<()> {
    if RUN_OPTIONS.get().is_none() {
        let options = ort::session::RunOptions::new().context("create cancellable ORT options")?;
        let _ = RUN_OPTIONS.set(options);
    }
    Ok(())
}
pub fn run_options() -> Result<&'static ort::session::RunOptions> {
    RUN_OPTIONS.get().context("ORT run options not initialized")
}
pub fn cancel_inference() {
    cancellation::CANCELLED.store(true, std::sync::atomic::Ordering::Release);
    if let Some(options) = RUN_OPTIONS.get() {
        let _ = options.terminate();
    }
}
pub fn reset_inference() -> Result<()> {
    cancellation::CANCELLED.store(false, std::sync::atomic::Ordering::Release);
    run_options()?
        .unterminate()
        .context("reset inference cancellation")
}

pub mod measure;
