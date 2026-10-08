pub mod backend_config;
pub mod cancellation;
pub mod graph;
use graph::session_for_lane;
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
        Self::load_lane(id, root, threads, voice, cancellation::Cancellation::new()?)
    }
    fn load_lane(
        id: &str,
        root: &Path,
        threads: usize,
        voice: &str,
        cancellation: cancellation::Cancellation,
    ) -> Result<Self> {
        match id {
            "pocket-en" => Ok(Self::Pocket(Box::new(pocket::Pocket::load(
                root,
                threads,
                voice,
                cancellation,
            )?))),
            "supertonic-3" => Ok(Self::Supertonic(Box::new(supertonic::Supertonic::load(
                root,
                threads,
                voice,
                cancellation,
            )?))),
            "kokoro-82m-int8" => Ok(Self::Kokoro(Box::new(kokoro::Kokoro::load(
                root,
                threads,
                voice,
                cancellation,
            )?))),
            _ => bail!("unknown model id"),
        }
    }
    /// Loads lane-local engine state while sharing accelerated compiled weights.
    pub fn load_lanes(
        id: &str,
        root: &Path,
        threads_per_lane: usize,
        voice: &str,
        lanes: usize,
    ) -> Result<Vec<Self>> {
        anyhow::ensure!((1..=3).contains(&lanes), "invalid lanes");
        anyhow::ensure!(
            (1..=8).contains(&threads_per_lane) && threads_per_lane * lanes <= 8,
            "invalid lane thread budget"
        );
        graph::with_compiled_graphs(|| {
            (0..lanes)
                .map(|_| Self::load(id, root, threads_per_lane, voice))
                .collect()
        })
    }
    /// Returns the cancellation handle for this engine's lane.
    pub fn cancellation(&self) -> &cancellation::Cancellation {
        match self {
            Self::Pocket(engine) => &engine.cancellation,
            Self::Supertonic(engine) => &engine.cancellation,
            Self::Kokoro(engine) => &engine.cancellation,
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

pub mod measure;
