use anyhow::{Context, Result, ensure};
use handy_tts_core::device::Device;
use openvino::{Core, DeviceType, InferRequest, PartialShape, RwPropertyKey, Tensor};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};
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
/// Creates independent requests from one compiled graph.
pub trait RequestFactory {
    type Request: crate::cancellation::AsyncRequest;
    /// Creates a request with buffers independent of existing requests.
    fn create_request(&mut self) -> Result<Self::Request>;
}
impl RequestFactory for openvino::CompiledModel {
    type Request = InferRequest;
    fn create_request(&mut self) -> Result<InferRequest> {
        self.create_infer_request()
            .context("create graph inference request")
    }
}

/// Compiled weights and graph metadata shared by synthesis lanes.
pub struct CompiledGraph<C = openvino::CompiledModel> {
    pub info: GraphInfo,
    compiled: Mutex<C>,
    device: Device,
    _core: Mutex<Option<Core>>,
}

/// One lane's request and cancellation flag.
pub struct GraphLane<C: RequestFactory = openvino::CompiledModel> {
    request: C::Request,
    pub compiled: Arc<CompiledGraph<C>>,
    cancelled: Arc<AtomicBool>,
}
impl<C: RequestFactory> CompiledGraph<C> {
    /// Creates a lane using these compiled weights and the supplied flag.
    pub fn lane(self: &Arc<Self>, cancelled: Arc<AtomicBool>) -> Result<GraphLane<C>> {
        let request = self
            .compiled
            .lock()
            .map_err(|_| anyhow::anyhow!("compiled graph poisoned"))?
            .create_request()?;
        Ok(GraphLane {
            compiled: self.clone(),
            request,
            cancelled,
        })
    }
}
impl<C: RequestFactory> GraphLane<C> {
    fn run(&mut self) -> Result<()> {
        if let Err(error) =
            crate::cancellation::drive(&mut self.request, &self.cancelled, self.compiled.device)
        {
            self.request = self
                .compiled
                .compiled
                .lock()
                .map_err(|_| anyhow::anyhow!("compiled graph poisoned"))?
                .create_request()
                .context("replace cancelled OpenVINO request")?;
            return Err(error.context("run cancellable OpenVINO graph"));
        }
        Ok(())
    }
}
impl CompiledGraph {
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
        let compiled = core
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
            compiled: Mutex::new(compiled),
            device: match device {
                "GPU" => Device::Gpu,
                "NPU" => Device::Npu,
                _ => Device::Cpu,
            },
            _core: Mutex::new(Some(core)),
        })
    }
}
impl GraphLane {
    pub fn infer(&mut self, inputs: &BTreeMap<String, Tensor>) -> Result<Vec<Tensor>> {
        ensure!(
            inputs.len() == self.compiled.info.inputs.len(),
            "wrong input count"
        );
        for name in &self.compiled.info.inputs {
            let tensor = inputs.get(name).context("missing named graph input")?;
            self.request
                .set_tensor(name, tensor)
                .context("set graph input tensor")?;
        }
        self.run()?;
        (0..self.compiled.info.outputs.len())
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
    Ok(CompiledGraph::load(path, device, cache, fixed, 2, false)?.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::{AsyncRequest, Wait};
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Factory(Arc<AtomicUsize>);
    struct FakeRequest {
        id: usize,
        cancel_on_wait: Option<Arc<AtomicBool>>,
        waits: usize,
        cancels: usize,
    }
    impl RequestFactory for Factory {
        type Request = FakeRequest;
        fn create_request(&mut self) -> Result<FakeRequest> {
            Ok(FakeRequest {
                id: self.0.fetch_add(1, Ordering::SeqCst),
                cancel_on_wait: None,
                waits: 0,
                cancels: 0,
            })
        }
    }
    impl AsyncRequest for FakeRequest {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn wait(&mut self) -> Result<Wait> {
            self.waits += 1;
            if self.waits == 1 {
                if let Some(flag) = &self.cancel_on_wait {
                    flag.store(true, Ordering::Release);
                }
                Ok(Wait::Pending)
            } else {
                Ok(Wait::Ready)
            }
        }
        fn cancel(&mut self) -> Result<()> {
            self.cancels += 1;
            Ok(())
        }
    }
    fn compiled(device: Device) -> Arc<CompiledGraph<Factory>> {
        Arc::new(compiled_model(device))
    }
    fn compiled_model(device: Device) -> CompiledGraph<Factory> {
        CompiledGraph {
            info: GraphInfo {
                device: format!("{device:?}"),
                graph: "fake".into(),
                compile_seconds: 0.0,
                cached: false,
                inputs: vec![],
                outputs: vec![],
                shapes: BTreeMap::new(),
            },
            compiled: std::sync::Mutex::new(Factory(Arc::new(AtomicUsize::new(0)))),
            device,
            _core: Mutex::new(None),
        }
    }
    #[test]
    fn lanes_share_weights_through_production_compilation_cache() {
        for device in [Device::Gpu, Device::Npu] {
            let mut cache = crate::graph::CompilationCache::<Factory>::default();
            let mut compilations = BTreeMap::<PathBuf, usize>::new();
            let mut lanes = Vec::new();
            let paths = [PathBuf::from("encoder.onnx"), PathBuf::from("decoder.onnx")];
            for _ in 0..3 {
                let mut graphs = Vec::new();
                for path in &paths {
                    let graph = cache
                        .lane(path, Arc::new(AtomicBool::new(false)), || {
                            *compilations.entry(path.clone()).or_default() += 1;
                            Ok(compiled_model(device))
                        })
                        .unwrap();
                    graphs.push(graph);
                }
                lanes.push(graphs);
            }
            for path in &paths {
                assert_eq!(
                    compilations[path], 1,
                    "production cache must compile each accelerated graph only once"
                );
            }
            for graph in 0..paths.len() {
                for lane in 1..lanes.len() {
                    assert!(Arc::ptr_eq(
                        &lanes[0][graph].compiled,
                        &lanes[lane][graph].compiled
                    ));
                    assert_ne!(lanes[0][graph].request.id, lanes[lane][graph].request.id);
                }
                assert_eq!(
                    lanes
                        .iter()
                        .map(|lane| lane[graph].request.id)
                        .collect::<Vec<_>>(),
                    [0, 1, 2]
                );
            }
            assert!(!Arc::ptr_eq(&lanes[0][0].compiled, &lanes[0][1].compiled));
        }
    }

    #[test]
    fn lanes_share_compiled_graph_and_own_requests() {
        let shared = compiled(Device::Gpu);
        let one = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
        let two = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
        assert!(Arc::ptr_eq(&one.compiled, &two.compiled));
        assert_ne!(one.request.id, two.request.id);
        assert!(!Arc::ptr_eq(&one.cancelled, &two.cancelled));
    }
    #[test]
    fn cancel_lane_two_leaves_lane_one_running() {
        let shared = compiled(Device::Gpu);
        let mut one = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
        let mut two = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
        two.request.cancel_on_wait = Some(two.cancelled.clone());
        let old_id = two.request.id;
        assert!(two.run().is_err());
        assert_ne!(two.request.id, old_id, "cancelled request must be replaced");
        assert!(
            one.run().is_ok(),
            "cancelling lane two must leave lane one running"
        );
        assert_eq!(one.request.cancels, 0);
        assert!(!one.cancelled.load(Ordering::Acquire));
        two.cancelled.store(false, Ordering::Release);
        assert!(two.run().is_ok(), "cancelled lane must be reusable");
    }
    #[test]
    fn npu_lane_drains_and_discards_only_its_request() {
        let shared = compiled(Device::Npu);
        let mut one = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
        let mut two = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
        two.request.cancel_on_wait = Some(two.cancelled.clone());
        let result = crate::cancellation::drive(&mut two.request, &two.cancelled, shared.device);
        assert_eq!((two.request.waits, two.request.cancels), (2, 0));
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<openvino::InferenceError>()
                .unwrap()
                .kind,
            openvino::InferenceErrorKind::InferCancelled
        );
        assert!(one.run().is_ok());
        assert_eq!(one.request.cancels, 0);
    }
}
