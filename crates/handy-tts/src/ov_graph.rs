use anyhow::{Context, Result, ensure};
use handy_tts_core::device::Device;
use openvino::{Core, DeviceType, InferRequest, PartialShape, RwPropertyKey, Tensor};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};
type SharedCore = Arc<Mutex<Option<Core>>>;
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
    /// Fills static inputs on a lane's request before its warm inference.
    fn zero_inputs(request: &mut Self::Request, info: &GraphInfo) -> Result<()>;
}
impl RequestFactory for openvino::CompiledModel {
    type Request = InferRequest;
    fn create_request(&mut self) -> Result<InferRequest> {
        self.create_infer_request()
            .context("create graph inference request")
    }
    fn zero_inputs(request: &mut InferRequest, info: &GraphInfo) -> Result<()> {
        for name in &info.inputs {
            let mut tensor = request.get_tensor(name).context("get static warm input")?;
            tensor.get_raw_data_mut()?.fill(0);
            request
                .set_tensor(name, &tensor)
                .context("set static warm input")?;
        }
        Ok(())
    }
}

/// Compiled weights and graph metadata shared by synthesis lanes.
pub struct CompiledGraph<C = openvino::CompiledModel> {
    pub info: GraphInfo,
    classes: Vec<crate::padding::ShapeClass>,
    variants: Vec<Arc<CompiledGraph<C>>>,
    compiled: Mutex<C>,
    device: Device,
    _core: SharedCore,
}

/// One lane's request and cancellation flag.
pub struct GraphLane<C: RequestFactory = openvino::CompiledModel> {
    request: C::Request,
    variants: Vec<GraphLane<C>>,
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
        let variants = self
            .variants
            .iter()
            .map(|graph| graph.lane(cancelled.clone()))
            .collect::<Result<_>>()?;
        let mut lane = GraphLane {
            compiled: self.clone(),
            request,
            cancelled,
            variants,
        };
        if !self.classes.is_empty() {
            lane.warm()?;
        }
        Ok(lane)
    }
}
impl<C: RequestFactory> GraphLane<C> {
    /// Primes every static class on this lane's own inference requests.
    pub fn warm(&mut self) -> Result<()> {
        let start = Instant::now();
        C::zero_inputs(&mut self.request, &self.compiled.info)?;
        self.run()?;
        for variant in &mut self.variants {
            C::zero_inputs(&mut variant.request, &variant.compiled.info)?;
            variant.run()?;
        }
        eprintln!(
            "HANDY_TTS_WARM graph={} classes={} seconds={:.6} cached={}",
            self.compiled.info.graph,
            self.compiled.classes.len(),
            start.elapsed().as_secs_f64(),
            self.compiled.info.cached
        );
        Ok(())
    }
    /// Returns the request for the smallest class covering the original dimensions.
    pub fn select(&mut self, text: i64, frames: i64) -> Result<&mut Self> {
        if self.compiled.classes.is_empty() {
            return Ok(self);
        }
        let index = crate::padding::class_index(&self.compiled.classes, text, frames)?;
        if index == 0 {
            Ok(self)
        } else {
            self.variants
                .get_mut(index - 1)
                .context("compiled class request missing")
        }
    }
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
    /// Compiles the measured GPU capacities once for all synthesis lanes.
    pub fn load_classes(
        path: &Path,
        device: &str,
        cache: &Path,
        fixed: BTreeMap<String, Vec<i64>>,
        threads: usize,
        pocket: bool,
        classes: &[crate::padding::ShapeClass],
    ) -> Result<Self> {
        if classes.is_empty() {
            return Self::load(path, device, cache, fixed, threads, pocket);
        }
        ensure!(device == "GPU" && classes.len() <= 6, "invalid GPU classes");
        ensure!(
            classes
                .windows(2)
                .all(|pair| pair[0].text < pair[1].text && pair[0].frames < pair[1].frames),
            "GPU classes must ascend"
        );
        let prepare_start = Instant::now();
        let name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .context("graph name missing")?;
        let core = Self::class_core(device, cache, threads, name)?;
        let mut model = core
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenVINO core poisoned"))?
            .as_mut()
            .context("OpenVINO core missing")?
            .read_model_from_file(path.to_str().context("graph path must be UTF-8")?, "")
            .context("read pinned ONNX graph")?;
        let mut models = classes.iter().map(|&class| {
            let shapes = crate::backend_config::supertonic_shapes(class)
                .remove(name)
                .context("unknown class graph")?;
            Self::reshape(&mut model, &shapes)?;
            Self::compile(path, device, &core, &model, shapes)
        });
        let mut first = models.next().context("GPU class missing")??;
        first.variants = models
            .map(|model| model.map(Arc::new))
            .collect::<Result<_>>()?;
        first.classes = classes.to_vec();
        let compile_seconds = first.info.compile_seconds
            + first
                .variants
                .iter()
                .map(|model| model.info.compile_seconds)
                .sum::<f64>();
        eprintln!(
            "HANDY_TTS_CLASSES graph={name} prepare_seconds={:.6} compile_seconds_sum={compile_seconds:.6}",
            prepare_start.elapsed().as_secs_f64()
        );
        Ok(first)
    }

    pub fn load(
        path: &Path,
        device: &str,
        cache: &Path,
        fixed: BTreeMap<String, Vec<i64>>,
        threads: usize,
        pocket: bool,
    ) -> Result<Self> {
        let core = Self::core(device, cache, threads, pocket)?;
        let mut model = core
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenVINO core poisoned"))?
            .as_mut()
            .context("OpenVINO core missing")?
            .read_model_from_file(path.to_str().context("graph path must be UTF-8")?, "")
            .context("read pinned ONNX graph")?;
        Self::reshape(&mut model, &fixed)?;
        Self::compile(path, device, &core, &model, fixed)
    }
    fn class_core(device: &str, cache: &Path, threads: usize, graph: &str) -> Result<SharedCore> {
        Self::core(
            device,
            &cache.join(STATIC_CLASS_CACHE),
            threads,
            class_full_precision(graph),
        )
    }
    fn core(
        device: &str,
        cache: &Path,
        threads: usize,
        full_precision: bool,
    ) -> Result<SharedCore> {
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
                if full_precision { "f32" } else { "f16" },
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
        Ok(Arc::new(Mutex::new(Some(core))))
    }
    fn reshape(model: &mut openvino::Model, fixed: &BTreeMap<String, Vec<i64>>) -> Result<()> {
        let partials = fixed
            .iter()
            .map(|(name, shape)| {
                Ok((
                    name.as_str(),
                    PartialShape::new_static(shape.len() as i64, shape)
                        .context("create fixed graph shape")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if !partials.is_empty() {
            let shapes = partials
                .iter()
                .map(|(name, shape)| (*name, shape))
                .collect::<Vec<_>>();
            model
                .reshape(&shapes)
                .context("reshape static graph inputs")?;
        }
        Ok(())
    }
    fn compile(
        path: &Path,
        device: &str,
        core: &Arc<Mutex<Option<Core>>>,
        model: &openvino::Model,
        fixed: BTreeMap<String, Vec<i64>>,
    ) -> Result<Self> {
        let start = Instant::now();
        let compiled = core
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenVINO core poisoned"))?
            .as_mut()
            .context("OpenVINO core missing")?
            .compile_model(model, DeviceType::from(device))
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
            classes: vec![],
            variants: vec![],
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
            _core: core.clone(),
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

/// Versioned cache directory for static GPU class graphs.
const STATIC_CLASS_CACHE: &str = "gpu-static-mixed-v2";

/// Supertonic class graphs compiled with f32 inference precision.
fn class_full_precision(graph: &str) -> bool {
    matches!(graph, "duration_predictor" | "text_encoder" | "vocoder")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::{AsyncRequest, Wait};

    #[test]
    fn class_graphs_keep_the_vocoder_at_full_precision() {
        assert!(class_full_precision("duration_predictor"));
        assert!(class_full_precision("text_encoder"));
        assert!(class_full_precision("vocoder"));
        assert!(!class_full_precision("vector_estimator"));
        assert_ne!(STATIC_CLASS_CACHE, "gpu-static-mixed-v1");
    }
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
        fn zero_inputs(_: &mut FakeRequest, _: &GraphInfo) -> Result<()> {
            Ok(())
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
            classes: vec![],
            variants: vec![],
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
            _core: Arc::new(Mutex::new(None)),
        }
    }
    mod gpu_classes {
        use super::*;
        struct WarmFactory;
        struct WarmRequest {
            warmed: bool,
            first_use_seconds: usize,
        }
        impl RequestFactory for WarmFactory {
            type Request = WarmRequest;
            fn create_request(&mut self) -> Result<WarmRequest> {
                Ok(WarmRequest {
                    warmed: false,
                    first_use_seconds: 0,
                })
            }
            fn zero_inputs(_: &mut WarmRequest, _: &GraphInfo) -> Result<()> {
                Ok(())
            }
        }
        impl AsyncRequest for WarmRequest {
            fn start(&mut self) -> Result<()> {
                if !self.warmed {
                    self.first_use_seconds += 1;
                    self.warmed = true;
                }
                Ok(())
            }
            fn wait(&mut self) -> Result<Wait> {
                Ok(Wait::Ready)
            }
            fn cancel(&mut self) -> Result<()> {
                Ok(())
            }
        }
        fn fake() -> CompiledGraph<WarmFactory> {
            CompiledGraph {
                info: GraphInfo {
                    device: "GPU".into(),
                    graph: "fake".into(),
                    compile_seconds: 0.0,
                    cached: false,
                    inputs: vec![],
                    outputs: vec![],
                    shapes: BTreeMap::new(),
                },
                compiled: Mutex::new(WarmFactory),
                device: Device::Gpu,
                _core: Arc::new(Mutex::new(None)),
                classes: vec![],
                variants: vec![],
            }
        }
        #[test]
        fn requests_after_ready_use_only_warmed_classes() {
            let cfg = crate::backend_config::config(
                "supertonic-3",
                Path::new("."),
                Device::Gpu,
                Path::new("cache"),
            )
            .unwrap();
            let mut model = fake();
            model.classes = cfg.classes.clone();
            model.variants = (1..cfg.classes.len()).map(|_| Arc::new(fake())).collect();
            let shared = Arc::new(model);
            for _ in 0..3 {
                let mut lane = shared.lane(Arc::new(AtomicBool::new(false))).unwrap();
                let baseline = lane.first_use_seconds();
                assert_eq!(baseline, 6);
                let observed = [
                    (79, 82),
                    (60, 54),
                    (110, 118),
                    (47, 43),
                    (66, 61),
                    (166, 165),
                    (174, 157),
                    (76, 133),
                    (273, 409),
                    (20, 22),
                    (76, 83),
                    (126, 125),
                    (15, 20),
                    (13, 20),
                    (17, 20),
                    (57, 53),
                    (35, 33),
                    (34, 32),
                    (69, 65),
                    (33, 32),
                    (22, 22),
                    (23, 22),
                    (35, 37),
                    (77, 87),
                    (32, 31),
                    (21, 21),
                    (28, 27),
                    (22, 22),
                    (23, 22),
                    (59, 58),
                    (28, 27),
                    (26, 24),
                    (34, 32),
                    (14, 20),
                    (63, 68),
                    (17, 22),
                    (177, 159),
                    (21, 26),
                    (68, 81),
                    (276, 264),
                    (47, 41),
                    (21, 25),
                    (304, 293),
                    (244, 231),
                    (136, 129),
                    (226, 240),
                    (42, 52),
                    (294, 310),
                    (182, 215),
                    (56, 76),
                    (54, 70),
                    (248, 330),
                    (103, 101),
                    (187, 173),
                    (128, 117),
                    (18, 23),
                    (52, 54),
                    (28, 27),
                    (110, 103),
                    (162, 144),
                    (115, 109),
                    (31, 28),
                    (72, 67),
                    (20, 29),
                    (186, 172),
                    (182, 181),
                    (175, 153),
                    (29, 26),
                    (62, 65),
                    (207, 209),
                    (80, 74),
                    (302, 300),
                    (191, 186),
                    (239, 272),
                    (128, 138),
                    (235, 282),
                    (301, 404),
                    (22, 33),
                    (267, 322),
                    (207, 203),
                    (301, 301),
                    (290, 299),
                    (300, 302),
                    (133, 141),
                    (288, 300),
                    (249, 259),
                    (149, 145),
                    (248, 243),
                    (259, 300),
                    (238, 221),
                    (196, 210),
                    (145, 174),
                    (223, 274),
                    (164, 206),
                    (224, 250),
                    (232, 282),
                    (219, 222),
                    (264, 291),
                    (172, 161),
                    (291, 384),
                    (259, 328),
                    (224, 268),
                    (297, 266),
                    (28, 26),
                    (295, 302),
                    (187, 188),
                    (288, 289),
                    (204, 202),
                    (171, 180),
                    (296, 298),
                    (177, 159),
                    (68, 81),
                    (276, 264),
                    (47, 41),
                    (234, 230),
                    (234, 232),
                    (147, 134),
                    (191, 186),
                    (239, 272),
                    (128, 138),
                    (235, 282),
                    (301, 404),
                    (22, 33),
                    (267, 322),
                    (207, 203),
                    (301, 301),
                    (290, 299),
                    (300, 302),
                    (133, 141),
                    (288, 300),
                    (249, 259),
                    (149, 145),
                    (248, 243),
                    (259, 300),
                    (238, 221),
                    (196, 210),
                    (145, 174),
                    (223, 274),
                    (164, 206),
                    (224, 250),
                    (232, 282),
                    (219, 222),
                    (264, 291),
                    (172, 161),
                    (80, 74),
                    (302, 300),
                    (283, 279),
                    (267, 304),
                    (152, 168),
                ];
                // Task s2 recorded these native-1x shapes across the seven documents and owner fixture.
                for step in 0..100 {
                    let (text, frames) = observed[step * observed.len() / 100];
                    let selected = lane.select(text, frames).unwrap();
                    selected.run().unwrap();
                }
                assert_eq!(
                    lane.first_use_seconds(),
                    baseline,
                    "requests after Ready must have zero first-use charges"
                );
            }
        }
        impl GraphLane<WarmFactory> {
            fn first_use_seconds(&self) -> usize {
                self.request.first_use_seconds
                    + self
                        .variants
                        .iter()
                        .map(|v| v.request.first_use_seconds)
                        .sum::<usize>()
            }
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
