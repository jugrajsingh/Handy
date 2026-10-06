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
};
#[derive(Debug, Clone)]
pub struct BackendConfig {
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
    OpenVino(crate::ov_graph::Graph),
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
pub fn session(path: &Path, threads: usize) -> Result<Session> {
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
        let progress = |completed| {
            if context.device != Device::Cpu {
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
            let graph = Session::OpenVino(crate::ov_graph::Graph::load(
                path,
                if device == Device::Gpu { "GPU" } else { "NPU" },
                &context.cache_dir,
                fixed,
                threads,
                context.model_id == "pocket-en",
            )?);
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
fn from_ov(tensor: &openvino::Tensor, crop: Option<(usize, i64)>) -> Result<DynValue> {
    let mut shape = tensor.get_shape()?.get_dimensions().to_vec();
    macro_rules! convert {
        ($ty:ty) => {{
            let mut data = tensor.get_data::<$ty>()?.to_vec();
            if let Some((axis, len)) = crop {
                data = crate::padding::crop_axis(&mut shape, axis, len, &data)?;
            }
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
                let mut values = BTreeMap::new();
                let mut original = BTreeMap::new();
                for (name, value) in inputs {
                    if let ort::value::ValueType::Tensor { shape, .. } = value.dtype() {
                        original.insert(name.to_string(), shape.to_vec());
                    }
                    values.insert(
                        name.to_string(),
                        to_ov(&value, graph.info.shapes.get(name.as_ref()))?,
                    );
                }
                let tensors = graph.infer(&values)?;
                let mut result = Vec::new();
                for (index, tensor) in tensors.iter().enumerate() {
                    let shape = tensor.get_shape()?.get_dimensions().to_vec();
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
                    result.push((graph.info.outputs[index].clone(), from_ov(tensor, crop)?));
                }
                Ok(Outputs { values: result })
            }
        }
    }
}
