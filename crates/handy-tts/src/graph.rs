use anyhow::{Context, Result, bail};
use handy_tts_core::device::Device;
use ort::{
    session::{RunOptions, Session as OrtSession, SessionInputs},
    value::{DynValue, Tensor},
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub classes: Vec<crate::padding::ShapeClass>,
    pub model_id: String,
    pub device: Device,
    pub cache_dir: PathBuf,
    pub placement: BTreeMap<String, Device>,
    pub shapes: BTreeMap<String, BTreeMap<String, Vec<i64>>>,
}
thread_local! {static CONFIG:RefCell<Option<BackendConfig>>=const {RefCell::new(None)};}
thread_local! {static COMPLETED:Cell<u32>=const {Cell::new(0)};}
pub fn with_config<T>(config: BackendConfig, load: impl FnOnce() -> Result<T>) -> Result<T> {
    let old = CONFIG.with(|slot| slot.replace(Some(config)));
    let count = COMPLETED.with(|slot| slot.replace(0));
    let result = load();
    CONFIG.with(|slot| slot.replace(old));
    COMPLETED.with(|slot| slot.set(count));
    result
}
pub enum Session {
    Ort(OrtSession),
    OpenVino(crate::ov_graph::GraphLane),
}
pub struct Outputs {
    values: Vec<(String, DynValue)>,
}
impl Outputs {
    pub fn get(&self, name: &str) -> Result<&DynValue> {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
            .context("missing named graph output")
    }
    pub fn at(&self, index: usize) -> Result<&DynValue> {
        self.values
            .get(index)
            .map(|(_, v)| v)
            .context("missing indexed graph output")
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &DynValue)> {
        self.values.iter().map(|(n, v)| (n.as_str(), v))
    }
}
/// Cache used by the accelerated loader, with an injectable compiler and request factory.
pub(crate) struct CompilationCache<C: crate::ov_graph::RequestFactory = openvino::CompiledModel> {
    graphs: BTreeMap<PathBuf, Arc<crate::ov_graph::CompiledGraph<C>>>,
    prepared: BTreeMap<PathBuf, crate::ov_graph::GraphLane<C>>,
}
impl<C: crate::ov_graph::RequestFactory> Default for CompilationCache<C> {
    fn default() -> Self {
        Self {
            graphs: BTreeMap::new(),
            prepared: BTreeMap::new(),
        }
    }
}
impl<C: crate::ov_graph::RequestFactory> CompilationCache<C> {
    pub(crate) fn lane(
        &mut self,
        path: &Path,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        compile: impl FnOnce() -> Result<crate::ov_graph::CompiledGraph<C>>,
    ) -> Result<crate::ov_graph::GraphLane<C>> {
        if let Some(lane) = self.prepared.remove(path) {
            return Ok(lane);
        }
        let cached = self.graphs.get(path).cloned();
        let compiled = match cached {
            Some(compiled) => compiled,
            None => {
                let compiled = Arc::new(compile()?);
                self.graphs.insert(path.to_path_buf(), compiled.clone());
                compiled
            }
        };
        compiled.lane(cancelled)
    }
}
impl CompilationCache {
    fn prepare_classes(
        &mut self,
        path: &Path,
        context: &BackendConfig,
        threads: usize,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        if self.prepared.contains_key(path) {
            return Ok(());
        }
        let paths = context
            .placement
            .keys()
            .map(|name| {
                let path = path.with_file_name(format!("{name}.onnx"));
                let graph = self.graphs.get(&path).cloned();
                (path, graph)
            })
            .collect::<Vec<_>>();
        let prepared = std::thread::scope(|scope| {
            let handles = paths
                .into_iter()
                .map(|(path, cached)| {
                    let cancelled = cancelled.clone();
                    scope.spawn(move || {
                        let compiled = match cached {
                            Some(graph) => graph,
                            None => Arc::new(crate::ov_graph::CompiledGraph::load_classes(
                                &path,
                                "GPU",
                                &context.cache_dir,
                                BTreeMap::new(),
                                threads,
                                false,
                                &context.classes,
                            )?),
                        };
                        let lane = compiled.lane(cancelled)?;
                        Ok((path, compiled, lane))
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("GPU graph preparation thread panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        for (path, graph, lane) in prepared {
            self.graphs.insert(path.clone(), graph);
            self.prepared.insert(path, lane);
        }
        Ok(())
    }
}

thread_local! {
    static COMPILED: RefCell<Option<CompilationCache>> = const { RefCell::new(None) };
}

pub(crate) fn with_compiled_graphs<T>(load: impl FnOnce() -> Result<T>) -> Result<T> {
    let old = COMPILED.with(|slot| slot.replace(Some(CompilationCache::default())));
    let result = load();
    COMPILED.with(|slot| slot.replace(old));
    result
}

pub fn session(path: &Path, threads: usize) -> Result<Session> {
    session_for_lane(path, threads, &crate::cancellation::Cancellation::new()?)
}

pub(crate) fn session_for_lane(
    path: &Path,
    threads: usize,
    cancellation: &crate::cancellation::Cancellation,
) -> Result<Session> {
    let context = CONFIG.with(|slot| slot.borrow().clone());
    if let Some(context) = context {
        let name = path
            .file_stem()
            .context("graph filename missing")?
            .to_string_lossy();
        let device = context
            .placement
            .get(name.as_ref())
            .copied()
            .unwrap_or(context.device);
        let reporting = COMPLETED.with(Cell::get) < context.placement.len() as u32;
        let progress = |completed| {
            if context.device != Device::Cpu && reporting {
                let value = handy_tts_core::device::Preparation::Preparing {
                    device: context.device,
                    completed,
                    total: context.placement.len() as u32,
                    graph: name.to_string(),
                };
                if let Ok(json) = serde_json::to_string(&value) {
                    eprintln!("HANDY_TTS_PROGRESS {json}");
                }
            }
        };
        progress(COMPLETED.with(Cell::get));
        if device != Device::Cpu {
            let fixed = context
                .shapes
                .get(name.as_ref())
                .cloned()
                .unwrap_or_default();
            let graph = COMPILED.with(|slot| {
                let mut slot = slot.borrow_mut();
                let compile = || {
                    crate::ov_graph::CompiledGraph::load_classes(
                        path,
                        if device == Device::Gpu { "GPU" } else { "NPU" },
                        &context.cache_dir,
                        fixed,
                        threads,
                        context.model_id == "pocket-en",
                        &context.classes,
                    )
                };
                match slot.as_mut() {
                    Some(cache) => {
                        if !context.classes.is_empty() {
                            cache.prepare_classes(
                                path,
                                &context,
                                threads,
                                cancellation.flag.clone(),
                            )?;
                        }
                        cache.lane(path, cancellation.flag.clone(), compile)
                    }
                    None => Arc::new(compile()?).lane(cancellation.flag.clone()),
                }
            })?;
            let graph = Session::OpenVino(graph);
            let completed = COMPLETED.with(|slot| {
                let n = slot.get() + 1;
                slot.set(n);
                n
            });
            progress(completed);
            return Ok(graph);
        }
        let session = Session::Ort(crate::ort_session(path, threads)?);
        let completed = COMPLETED.with(|slot| {
            let n = slot.get() + 1;
            slot.set(n);
            n
        });
        progress(completed);
        return Ok(session);
    }
    Ok(Session::Ort(crate::ort_session(path, threads)?))
}
fn to_ov(value: &DynValue, target: Option<&Vec<i64>>) -> Result<openvino::Tensor> {
    macro_rules! convert {
        ($ty:ty,$kind:expr) => {
            if let Ok((shape, data)) = value.try_extract_tensor::<$ty>() {
                let actual: Vec<i64> = shape.iter().copied().collect();
                let fixed = target.cloned().unwrap_or_else(|| actual.clone());
                let padded = crate::padding::pad(&actual, &fixed, data)?;
                let mut tensor = openvino::Tensor::new($kind, &openvino::Shape::new(&fixed)?)
                    .context("allocate graph input")?;
                tensor.get_data_mut::<$ty>()?.copy_from_slice(&padded);
                return Ok(tensor);
            }
        };
    }
    convert!(f32, openvino::ElementType::F32);
    convert!(i64, openvino::ElementType::I64);
    convert!(bool, openvino::ElementType::Boolean);
    bail!("unsupported graph input dtype")
}
fn crop_output<T: Clone>(
    shape: &mut [i64],
    index: usize,
    original: &BTreeMap<String, Vec<i64>>,
    data: Vec<T>,
) -> Result<Vec<T>> {
    let crop = if index == 0 && shape.len() == 3 {
        if let Some(actual) = original.get("noisy_latent") {
            Some((2, actual[2]))
        } else if let Some(actual) = original.get("text_ids") {
            Some((2, actual[1]))
        } else {
            original.get("token_ids").map(|actual| (1, actual[1]))
        }
    } else {
        None
    };
    if let Some((axis, length)) = crop {
        return crate::padding::crop_axis(shape, axis, length, &data);
    }
    Ok(data)
}
fn from_ov(
    tensor: &openvino::Tensor,
    index: usize,
    original: &BTreeMap<String, Vec<i64>>,
) -> Result<DynValue> {
    let mut shape = tensor.get_shape()?.get_dimensions().to_vec();
    macro_rules! convert {
        ($ty:ty) => {{
            let data = crop_output(
                &mut shape,
                index,
                original,
                tensor.get_data::<$ty>()?.to_vec(),
            )?;
            Ok(Tensor::from_array((shape, data))?.into_dyn())
        }};
    }
    match tensor.get_element_type()? {
        openvino::ElementType::F32 => convert!(f32),
        openvino::ElementType::I64 => convert!(i64),
        openvino::ElementType::Boolean => convert!(bool),
        _ => bail!("unsupported graph output dtype"),
    }
}

impl Session {
    pub fn run_with_options<'i, 'v: 'i>(
        &mut self,
        inputs: impl Into<SessionInputs<'i, 'v>>,
        options: &RunOptions,
    ) -> Result<Outputs> {
        let inputs = inputs.into();
        match self {
            Self::Ort(session) => {
                let output = session
                    .run_with_options(inputs, options)
                    .context("run CPU ONNX graph")?;
                let values = output
                    .iter()
                    .map(|(name, value)| {
                        value
                            .try_upgrade()
                            .map(|v| (name.to_string(), v))
                            .map_err(|_| {
                                anyhow::anyhow!("CPU graph output must own tensor backing")
                            })
                    })
                    .collect::<Result<_>>()?;
                Ok(Outputs { values })
            }
            Self::OpenVino(graph) => {
                let SessionInputs::ValueMap(inputs) = inputs else {
                    bail!("OpenVINO graphs require named inputs");
                };
                let original = inputs
                    .iter()
                    .filter_map(|(name, value)| {
                        if let ort::value::ValueType::Tensor { shape, .. } = value.dtype() {
                            Some((name.to_string(), shape.to_vec()))
                        } else {
                            None
                        }
                    })
                    .collect::<BTreeMap<_, _>>();
                let text = original
                    .get("text_ids")
                    .and_then(|s| s.get(1).copied())
                    .or_else(|| original.get("text_mask").and_then(|s| s.get(2).copied()))
                    .unwrap_or(0);
                let frames = original
                    .get("noisy_latent")
                    .or_else(|| original.get("latent"))
                    .and_then(|s| s.get(2).copied())
                    .unwrap_or(0);
                let graph = graph.select(text, frames)?;
                let mut values = BTreeMap::new();
                for (name, value) in inputs {
                    values.insert(
                        name.to_string(),
                        to_ov(&value, graph.compiled.info.shapes.get(name.as_ref()))?,
                    );
                }
                let tensors = graph.infer(&values)?;
                let mut result = Vec::new();
                for (index, tensor) in tensors.iter().enumerate() {
                    result.push((
                        graph.compiled.info.outputs[index].clone(),
                        from_ov(tensor, index, &original)?,
                    ));
                }
                Ok(Outputs { values: result })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    mod gpu_classes {
        use super::*;
        #[test]
        fn crop_restores_original_dimensions() {
            for (input, original, output, axis) in [
                ("text_ids", vec![1, 3], vec![1, 2, 8], 2),
                ("noisy_latent", vec![1, 2, 3], vec![1, 2, 8], 2),
            ] {
                let originals = BTreeMap::from([(input.into(), original)]);
                let mut shape = output;
                let data = (0..16).collect::<Vec<_>>();
                let result = crop_output(&mut shape, 0, &originals, data).unwrap();
                assert_eq!(shape, [1, 2, 3]);
                assert_eq!(result, [0, 1, 2, 8, 9, 10]);
                assert_eq!(shape[axis], 3);
            }
        }
    }
}
