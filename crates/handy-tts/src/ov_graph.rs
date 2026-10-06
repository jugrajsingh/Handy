use anyhow::{Context, Result, ensure};
use openvino::{Core, DeviceType, InferRequest, PartialShape, RwPropertyKey, Tensor};
use std::{collections::BTreeMap, path::Path, time::Instant};
#[derive(Debug)]
pub(crate) struct NpuCompileRejected;
impl std::fmt::Display for NpuCompileRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NPU graph compile rejected")
    }
}
#[derive(Debug, serde::Serialize)]
pub struct GraphInfo {
    pub device: String,
    pub graph: String,
    pub compile_seconds: f64,
    pub cached: bool,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub shapes: BTreeMap<String, Vec<i64>>,
}
pub struct Graph {
    pub info: GraphInfo,
    request: InferRequest,
    compiled: openvino::CompiledModel,
    _core: Core,
}
impl Graph {
    pub fn load(
        path: &Path,
        device: &str,
        cache: &Path,
        fixed: BTreeMap<String, Vec<i64>>,
        threads: usize,
        pocket: bool,
    ) -> Result<Self> {
        ensure!(
            ["CPU", "GPU", "NPU"].contains(&device),
            "invalid OpenVINO device"
        );
        let _ = crate::devices::list()?;
        std::fs::create_dir_all(cache).context("create OpenVINO cache directory")?;
        let mut core = Core::new().context("create graph OpenVINO core")?;
        let kind = DeviceType::from(device);
        core.set_property(
            &kind,
            &RwPropertyKey::CacheDir,
            cache.to_str().context("cache path must be UTF-8")?,
        )
        .context("set OpenVINO cache")?;
        core.set_property(&kind, &RwPropertyKey::HintPerformanceMode, "LATENCY")
            .context("set latency performance hint")?;
        if device == "NPU" {
            core.set_property(
                &kind,
                &RwPropertyKey::Other("NPU_COMPILER_TYPE".into()),
                "DRIVER",
            )
            .context("use NPU driver compiler")?;
        }
        if device == "GPU" {
            core.set_property(
                &kind,
                &RwPropertyKey::HintInferencePrecision,
                if pocket { "f32" } else { "f16" },
            )
            .context("set GPU inference precision")?;
        }
        if device == "CPU" {
            core.set_property(
                &kind,
                &RwPropertyKey::InferenceNumThreads,
                &threads.to_string(),
            )
            .context("set CPU threads")?;
            core.set_property(&kind, &RwPropertyKey::NumStreams, "1")
                .context("set single CPU stream")?;
        }
        let mut model = core
            .read_model_from_file(path.to_str().context("graph path must be UTF-8")?, "")
            .context("read pinned ONNX graph")?;
        for (name, shape) in &fixed {
            let partial = PartialShape::new_static(shape.len() as i64, shape)
                .context("create fixed NPU shape")?;
            model
                .reshape_input_by_name(name, &partial)
                .context("reshape NPU input")?;
        }
        let start = Instant::now();
        let mut compiled = core
            .compile_model(&model, kind)
            .map_err(|error| {
                let error = anyhow::Error::new(error);
                if device == "NPU" {
                    error.context(NpuCompileRejected)
                } else {
                    error
                }
            })
            .context("compile ONNX on explicit OpenVINO device")?;
        let compile_seconds = start.elapsed().as_secs_f64();
        let cached = compiled
            .get_property(&openvino::PropertyKey::Other("LOADED_FROM_CACHE".into()))
            .context("get graph cache status")?
            .eq_ignore_ascii_case("yes");
        let inputs = (0..compiled.get_input_size()?)
            .map(|i| compiled.get_input_by_index(i)?.get_name())
            .collect::<std::result::Result<Vec<_>, openvino::InferenceError>>()
            .context("get graph input names")?;
        let outputs = (0..compiled.get_output_size()?)
            .map(|i| compiled.get_output_by_index(i)?.get_name())
            .collect::<std::result::Result<Vec<_>, openvino::InferenceError>>()
            .context("get graph output names")?;
        let request = compiled
            .create_infer_request()
            .context("create graph inference request")?;
        Ok(Self {
            info: GraphInfo {
                device: device.into(),
                graph: path
                    .file_name()
                    .context("graph needs filename")?
                    .to_string_lossy()
                    .into(),
                compile_seconds,
                cached,
                inputs,
                outputs,
                shapes: fixed,
            },
            request,
            compiled,
            _core: core,
        })
    }
    pub fn infer(&mut self, inputs: &BTreeMap<String, Tensor>) -> Result<Vec<Tensor>> {
        ensure!(inputs.len() == self.info.inputs.len(), "wrong input count");
        for name in &self.info.inputs {
            let tensor = inputs.get(name).context("missing named graph input")?;
            self.request
                .set_tensor(name, tensor)
                .context("set graph input tensor")?;
        }
        if let Err(error) =
            crate::cancellation::drive(&mut self.request, &crate::cancellation::CANCELLED)
        {
            self.request = self
                .compiled
                .create_infer_request()
                .context("replace cancelled OpenVINO request")?;
            return Err(error.context("run cancellable OpenVINO graph"));
        }
        (0..self.info.outputs.len())
            .map(|i| {
                let value = self
                    .request
                    .get_output_tensor_by_index(i)
                    .context("get graph output")?;
                let mut copy = Tensor::new(value.get_element_type()?, &value.get_shape()?)
                    .context("allocate owned graph output")?;
                copy.get_raw_data_mut()?
                    .copy_from_slice(value.get_raw_data()?);
                Ok(copy)
            })
            .collect()
    }
}
pub fn probe(path: &Path, device: &str, cache: &Path) -> Result<GraphInfo> {
    let fixed = if device == "NPU" {
        BTreeMap::from([
            ("text_ids".into(), vec![1, 512]),
            ("style_dp".into(), vec![1, 8, 16]),
            ("text_mask".into(), vec![1, 1, 512]),
        ])
    } else {
        BTreeMap::new()
    };
    Ok(Graph::load(path, device, cache, fixed, 2, false)?.info)
}
