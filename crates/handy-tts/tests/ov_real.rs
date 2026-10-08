use handy_tts_core::{
    device::Device,
    protocol::{Request, Status, read_header, read_pcm_after_header, write_request},
};
use std::{
    collections::BTreeMap,
    io::BufReader,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

struct Worker(std::process::Child);

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct ProbeDirectory(PathBuf);

impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn traced_cancel_probe(device: Device) {
    let scratch = PathBuf::from(std::env::var_os("TMPDIR").unwrap());
    let directory =
        ProbeDirectory(scratch.join(format!("task-13-cancel-probe-{}", std::process::id())));
    std::fs::create_dir(&directory.0).unwrap();
    let source = directory.0.join("trace.c");
    let proxy = directory.0.join("trace.so");
    let trace = directory.0.join("cancel.calls");
    let library = std::env::var_os("HANDY_OPENVINO_LIBRARY").unwrap();
    std::fs::write(
        &source,
        r#"
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

int ov_infer_request_cancel(void *request) {
    void *library = dlopen(getenv("HANDY_TTS_REAL_LIBRARY"), RTLD_NOW | RTLD_LOCAL);
    if (!library) return -1;
    int (*cancel)(void *) = (int (*)(void *))dlsym(library, "ov_infer_request_cancel");
    if (!cancel) { dlclose(library); return -1; }
    FILE *trace = fopen(getenv("HANDY_TTS_CANCEL_TRACE"), "a");
    if (!trace) { dlclose(library); return -1; }
    fputs("cancel\n", trace);
    fclose(trace);
    int result = cancel(request);
    dlclose(library);
    return result;
}
"#,
    )
    .unwrap();
    let build = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,--no-as-needed"])
        .arg(&library)
        .arg(&source)
        .arg("-ldl")
        .arg("-o")
        .arg(&proxy)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "compile cancellation trace: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let probe = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "--exact",
            "real_worker_cancellation_returns_cancelled_and_next_chunk_can_synthesize",
        ])
        .env("HANDY_OPENVINO_LIBRARY", &proxy)
        .env("HANDY_TTS_REAL_LIBRARY", &library)
        .env("HANDY_TTS_CANCEL_TRACE", &trace)
        .env("HANDY_TTS_CANCEL_PROBE", "1")
        .output()
        .unwrap();
    print!("{}", String::from_utf8_lossy(&probe.stdout));
    eprint!("{}", String::from_utf8_lossy(&probe.stderr));
    assert!(probe.status.success(), "real cancellation probe failed");
    let calls = std::fs::read_to_string(&trace).unwrap_or_default();
    assert_eq!(
        calls.lines().count(),
        usize::from(device != Device::Npu),
        "device cancellation must reach the real C API exactly when supported"
    );
    println!(
        "REAL_CANCEL_API device={device:?} calls={}",
        calls.lines().count()
    );
}

fn prove_infer_request_cancel(root: &std::path::Path, cache: &std::path::Path, device: Device) {
    use handy_tts::cancellation::{AsyncRequest, Wait};
    use openvino::{Core, DeviceType, ElementType, PartialShape, RwPropertyKey, Shape, Tensor};

    struct ObservedRequest<'a> {
        request: openvino::InferRequest,
        flag: &'a AtomicBool,
        backend_cancelled: bool,
        backend_drained: bool,
        backend_error: Option<openvino::InferenceError>,
    }
    impl AsyncRequest for ObservedRequest<'_> {
        fn start(&mut self) -> anyhow::Result<()> {
            AsyncRequest::start(&mut self.request)?;
            self.flag.store(true, Ordering::Release);
            Ok(())
        }
        fn wait(&mut self) -> anyhow::Result<Wait> {
            let result = AsyncRequest::wait(&mut self.request);
            if matches!(result, Ok(Wait::Ready)) {
                self.backend_drained = true;
            }
            if let Err(error) = &result {
                self.backend_error = error.downcast_ref::<openvino::InferenceError>().cloned();
                self.backend_cancelled = error
                    .downcast_ref::<openvino::InferenceError>()
                    .is_some_and(|error| {
                        error.kind == openvino::InferenceErrorKind::InferCancelled
                    });
            }
            result
        }
        fn cancel(&mut self) -> anyhow::Result<()> {
            AsyncRequest::cancel(&mut self.request)
        }
    }

    handy_tts::devices::list().unwrap();
    let mut core = Core::new().unwrap();
    let kind = DeviceType::from(if device == Device::Npu { "NPU" } else { "GPU" });
    core.set_property(&kind, &RwPropertyKey::CacheDir, cache.to_str().unwrap())
        .unwrap();
    if device == Device::Npu {
        core.set_property(
            &kind,
            &RwPropertyKey::Other("NPU_COMPILER_TYPE".into()),
            "DRIVER",
        )
        .unwrap();
    }
    let mut model = core
        .read_model_from_file(
            root.join("onnx/vector_estimator.onnx").to_str().unwrap(),
            "",
        )
        .unwrap();
    let config =
        handy_tts::backend_config::config("supertonic-3", root, Device::Npu, cache).unwrap();
    let shapes = &config.shapes["vector_estimator"];
    for (name, dims) in shapes {
        model
            .reshape_input_by_name(
                name,
                &PartialShape::new_static(dims.len() as i64, dims).unwrap(),
            )
            .unwrap();
    }
    let mut compiled = core.compile_model(&model, kind).unwrap();
    let mut request = compiled.create_infer_request().unwrap();
    let mut tensors = Vec::new();
    for (name, dims) in shapes {
        let mut tensor = Tensor::new(ElementType::F32, &Shape::new(dims).unwrap()).unwrap();
        tensor
            .get_data_mut::<f32>()
            .unwrap()
            .fill(if name == "total_step" { 8.0 } else { 0.1 });
        request.set_tensor(name, &tensor).unwrap();
        tensors.push(tensor);
    }
    let flag = AtomicBool::new(false);
    let mut observed = ObservedRequest {
        request,
        flag: &flag,
        backend_cancelled: false,
        backend_drained: false,
        backend_error: None,
    };
    let cancelled = handy_tts::cancellation::drive(&mut observed, &flag, device);
    assert_eq!(
        cancelled
            .as_ref()
            .unwrap_err()
            .downcast_ref::<openvino::InferenceError>()
            .map(|error| &error.kind),
        Some(&openvino::InferenceErrorKind::InferCancelled),
        "cancelled request must discard output and return InferCancelled; drive={cancelled:?}"
    );
    if device == Device::Npu {
        assert!(
            observed.backend_drained,
            "NPU request must finish before discarding output"
        );
        assert!(!observed.backend_cancelled);
    } else {
        assert!(
            observed.backend_drained || observed.backend_cancelled,
            "GPU request must be settled after cancellation: {:?}",
            observed.backend_error
        );
    }
    println!(
        "REAL_INFER_CANCEL device={device:?} result=InferCancelled drained={} backend_cancelled={}",
        observed.backend_drained, observed.backend_cancelled
    );
}

fn paths() -> (PathBuf, PathBuf, Device) {
    assert_eq!(std::env::var("HANDY_TTS_IT").unwrap(), "1");
    let root = PathBuf::from(std::env::var("HANDY_TTS_MODEL_ROOT").unwrap());
    let cache = PathBuf::from(std::env::var("HANDY_TTS_TEST_CACHE").unwrap());
    let device = match std::env::var("HANDY_TTS_DEVICE").unwrap().as_str() {
        "gpu" => Device::Gpu,
        "npu" => Device::Npu,
        _ => panic!("explicit accelerated device required"),
    };
    (root, cache, device)
}
#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU or NPU"]
fn real_encoder_zero_padding_and_output_crop_preserve_the_original_token_length() {
    let (root, cache, device) = paths();
    let options = ort::session::RunOptions::new().unwrap();
    let config = handy_tts::backend_config::config("supertonic-3", &root, device, &cache).unwrap();
    let mut graph = handy_tts::graph::with_config(config, || {
        handy_tts::graph::session(&root.join("onnx/text_encoder.onnx"), 2)
    })
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("voice_styles/F1.json")).unwrap()).unwrap();
    let data: Vec<Vec<Vec<f32>>> =
        serde_json::from_value(json["style_ttl"]["data"].clone()).unwrap();
    let data: Vec<f32> = data.into_iter().flatten().flatten().collect();
    let ids = ort::value::Tensor::from_array(([1, 4], vec![1i64, 2, 3, 4])).unwrap();
    let mask = ort::value::Tensor::from_array(([1, 1, 4], vec![1.0f32; 4])).unwrap();
    let style = ort::value::Tensor::from_array(([1, 50, 256], data)).unwrap();
    let output = graph
        .run_with_options(
            ort::inputs!["text_ids"=>ids,"text_mask"=>mask,"style_ttl"=>style],
            &options,
        )
        .unwrap();
    let (shape, values) = output.at(0).unwrap().try_extract_tensor::<f32>().unwrap();
    assert_eq!(
        shape.as_ref(),
        [1, 256, 4],
        "encoder output crop must preserve original token length"
    );
    assert!(values.iter().all(|v| v.is_finite()));
    assert!(values.iter().any(|v| v.abs() > 0.01));
    println!(
        "REAL_PADDING_CROP device={device:?} padded_tokens={} output_shape={shape:?} finite=true",
        if device == Device::Npu { 512 } else { 4 }
    );
}
#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU or NPU"]
fn real_worker_cancellation_returns_cancelled_and_next_chunk_can_synthesize() {
    let (root, cache, device) = paths();
    if std::env::var_os("HANDY_TTS_CANCEL_PROBE").is_some() {
        prove_infer_request_cancel(&root, &cache, device);
        return;
    }
    traced_cancel_probe(device);
    let mut child = Worker(
        Command::new(env!("CARGO_BIN_EXE_handy-tts"))
            .args(["--model", "supertonic-3"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    write_request(
        &mut input,
        &Request::Load {
            model_dir: root,
            voice: "F1".into(),
            threads: 2,
            lanes: 1,
            device,
            cache_dir: cache,
        },
    )
    .unwrap();
    let ready = read_header(&mut output).unwrap();
    assert_eq!(ready.status, Status::Ready);
    assert_eq!(ready.device, Some(device));
    assert_eq!(
        ready.placement.unwrap(),
        BTreeMap::from([
            ("duration_predictor".into(), device),
            ("text_encoder".into(), device),
            ("vector_estimator".into(), device),
            ("vocoder".into(), device)
        ])
    );
    write_request(&mut input,&Request::Synthesize{id:1,text:"The cancellation test reads a deliberately lengthy sentence while the control thread receives its independent cancel request, then checks the next sentence uses a clean inference request and fresh buffers.".into(),speed:Some(0.5)}).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    write_request(&mut input, &Request::Cancel { id: 1 }).unwrap();
    let cancelled = read_header(&mut output).unwrap();
    assert_eq!(cancelled.id, 1);
    assert_eq!(cancelled.status, Status::Cancelled);
    assert!(cancelled.samples.is_none_or(|samples| samples == 0));
    assert!(cancelled.sample_rate.is_none());
    write_request(
        &mut input,
        &Request::Synthesize {
            id: 2,
            text: "Reading continues clearly.".into(),
            speed: Some(2.0),
        },
    )
    .unwrap();
    let header = read_header(&mut output).unwrap();
    assert_eq!(header.status, Status::Ok);
    let pcm = read_pcm_after_header(&mut output, header, 2).unwrap();
    assert!(!pcm.samples.is_empty());
    assert!(pcm.samples.iter().all(|v| v.is_finite()));
    write_request(&mut input, &Request::Shutdown).unwrap();
    assert!(child.0.wait().unwrap().success());
    println!(
        "REAL_CANCEL device={device:?} cancelled_id=1 recovered_id=2 samples={}",
        pcm.samples.len()
    );
}

#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and NPU"]
fn real_oversized_npu_chunk_returns_complete_audio_after_word_boundary_splitting() {
    let (root, cache, device) = paths();
    if device != Device::Npu {
        return;
    }
    let mut child = Worker(
        Command::new(env!("CARGO_BIN_EXE_handy-tts"))
            .args(["--model", "supertonic-3"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    write_request(
        &mut input,
        &Request::Load {
            model_dir: root,
            voice: "F1".into(),
            threads: 2,
            lanes: 1,
            device,
            cache_dir: cache,
        },
    )
    .unwrap();
    let ready = read_header(&mut output).unwrap();
    assert_eq!(ready.status, Status::Ready);
    assert_eq!(ready.device, Some(Device::Npu));
    let part = ["Reading every word clearly preserves the whole original sentence."; 5].join(" ");
    let full = format!("{part} {part}");
    assert!(part.chars().count() + "<en></en>".len() < 512);
    assert!(full.chars().count() > 512);
    let mut lengths = Vec::new();
    for (id, text) in [(1, part), (2, full)] {
        write_request(
            &mut input,
            &Request::Synthesize {
                id,
                text,
                speed: Some(2.0),
            },
        )
        .unwrap();
        let header = read_header(&mut output).unwrap();
        assert_eq!(
            header.status,
            Status::Ok,
            "over-cap NPU input must synthesize: {header:?}"
        );
        let pcm = read_pcm_after_header(&mut output, header, id).unwrap();
        assert!(!pcm.samples.is_empty());
        assert!(pcm.samples.iter().all(|value| value.is_finite()));
        lengths.push(pcm.samples.len());
    }
    assert_eq!(
        lengths[1],
        2 * lengths[0],
        "both word-boundary parts must be present"
    );
    write_request(&mut input, &Request::Shutdown).unwrap();
    assert!(child.0.wait().unwrap().success());
    println!(
        "REAL_NPU_OVER_CAP single_samples={} complete_samples={}",
        lengths[0], lengths[1]
    );
}

#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU"]
fn gpu_two_lanes_produce_same_lengths_as_one() {
    let (root, cache, device) = paths();
    if device != Device::Gpu {
        return;
    }
    let text = "Reading clearly, one phrase at a time.";
    let flag = AtomicBool::new(false);
    let mut single =
        handy_tts::runtime::load_adapter("supertonic-3", &root, "F1", 2, device, &cache).unwrap();
    let expected = single.synthesize(text, Some(1.0), &flag).unwrap().len();
    drop(single);
    let mut lanes =
        handy_tts::runtime::load_adapters("supertonic-3", &root, "F1", 2, device, &cache, 2)
            .unwrap();
    let outputs = std::thread::scope(|scope| {
        let handles: Vec<_> = lanes
            .iter_mut()
            .map(|engine| {
                let flag = &flag;
                scope.spawn(move || engine.synthesize(text, Some(1.0), flag).unwrap())
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    for pcm in &outputs {
        assert_eq!(pcm.len(), expected);
        assert!(pcm.iter().all(|sample| sample.is_finite()));
    }
    println!(
        "REAL_GPU_LANES single_samples={expected} lane_samples={:?}",
        outputs.iter().map(Vec::len).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU"]
fn gpu_cancel_one_lane_other_completes() {
    let (root, cache, device) = paths();
    if device != Device::Gpu {
        return;
    }
    traced_cancel_probe(device);
    let mut child = Worker(
        Command::new(env!("CARGO_BIN_EXE_handy-tts"))
            .args(["--model", "supertonic-3"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    write_request(
        &mut input,
        &Request::Load {
            model_dir: root,
            voice: "F1".into(),
            threads: 2,
            lanes: 2,
            device,
            cache_dir: cache,
        },
    )
    .unwrap();
    assert_eq!(read_header(&mut output).unwrap().status, Status::Ready);
    for (id, text) in [
        (1, "Reading clearly, one phrase at a time."),
        (
            2,
            "The cancellation test reads a deliberately lengthy sentence while an independent synthesis continues producing its complete audio, with fresh requests and separate buffers for both lanes.",
        ),
    ] {
        write_request(
            &mut input,
            &Request::Synthesize {
                id,
                text: text.into(),
                speed: Some(1.0),
            },
        )
        .unwrap();
    }
    std::thread::sleep(Duration::from_millis(2));
    write_request(&mut input, &Request::Cancel { id: 2 }).unwrap();
    let mut results = BTreeMap::new();
    let mut expected_samples = None;
    for _ in 0..2 {
        let header = read_header(&mut output).unwrap();
        let id = header.id;
        let status = header.status.clone();
        if status == Status::Ok {
            let pcm = read_pcm_after_header(&mut output, header, id).unwrap();
            assert!(!pcm.samples.is_empty());
            assert!(pcm.samples.iter().all(|sample| sample.is_finite()));
            expected_samples = Some(pcm.samples.len());
        } else {
            assert!(header.samples.is_none_or(|samples| samples == 0));
        }
        assert!(results.insert(id, status).is_none());
    }
    assert_eq!(
        results,
        BTreeMap::from([(1, Status::Ok), (2, Status::Cancelled)])
    );
    for id in [3, 4] {
        write_request(
            &mut input,
            &Request::Synthesize {
                id,
                text: "Reading clearly, one phrase at a time.".into(),
                speed: Some(1.0),
            },
        )
        .unwrap();
    }
    let mut retries = BTreeMap::new();
    for _ in 0..2 {
        let header = read_header(&mut output).unwrap();
        assert_eq!(header.status, Status::Ok);
        let id = header.id;
        let pcm = read_pcm_after_header(&mut output, header, id).unwrap();
        assert_eq!(
            Some(pcm.samples.len()),
            expected_samples,
            "cancellation must preserve the other lane's full audio and both retry lengths"
        );
        assert!(pcm.samples.iter().all(|sample| sample.is_finite()));
        assert!(retries.insert(id, pcm.samples.len()).is_none());
    }
    assert_eq!(retries.keys().copied().collect::<Vec<_>>(), [3, 4]);
    write_request(&mut input, &Request::Shutdown).unwrap();
    assert!(child.0.wait().unwrap().success());
    println!("REAL_GPU_CANCEL lane_one=Ok lane_two=Cancelled retry_lanes=Ok");
}

#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU"]
fn gpu_classes_first_request_within_1_5x_repeat() {
    let (root, cache, device) = paths();
    assert_eq!(device, Device::Gpu);
    let flag = AtomicBool::new(false);
    let start = std::time::Instant::now();
    let engine =
        handy_tts::runtime::load_adapter("supertonic-3", &root, "F1", 2, device, &cache).unwrap();
    println!(
        "GPU_CLASSES initial_load_and_warm_seconds={:.6}",
        start.elapsed().as_secs_f64()
    );
    drop(engine);
    let start = std::time::Instant::now();
    let mut engine =
        handy_tts::runtime::load_adapter("supertonic-3", &root, "F1", 2, device, &cache).unwrap();
    let cached_load = start.elapsed().as_secs_f64();
    println!("GPU_CLASSES cached_load_and_warm_seconds={cached_load:.6}");
    assert!(
        cached_load <= 5.0,
        "cached load plus warm-up exceeds 5 seconds: {cached_load}"
    );
    let fixture_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../handy-tts-core/tests/fixtures/text");
    let cases: BTreeMap<String, Vec<String>> =
        serde_json::from_str(&std::fs::read_to_string(fixture_root.join("goldens.json")).unwrap())
            .unwrap();
    let mut first = vec![];
    let mut repeat = vec![];
    for name in cases.keys() {
        let source = std::fs::read_to_string(fixture_root.join(name)).unwrap();
        let mut cursor = handy_tts_core::text::TextCursor::new(&source, false).unwrap();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            let mut lengths = vec![];
            for timings in [&mut first, &mut repeat] {
                let start = std::time::Instant::now();
                let pcm = handy_tts::runtime::synthesize_bounded(
                    engine.as_mut(),
                    &chunk.text,
                    Some(1.0),
                    &flag,
                )
                .unwrap();
                timings.push(start.elapsed().as_secs_f64());
                assert!(!pcm.is_empty());
                assert!(pcm.iter().all(|sample| sample.is_finite()));
                lengths.push(pcm.len());
            }
            assert_eq!(
                lengths[0], lengths[1],
                "static padding must preserve repeat dimensions: {}",
                chunk.text
            );
        }
        println!(
            "GPU_CLASSES document={name} completed_chunks={}",
            first.len()
        );
    }
    fn p95(values: &mut [f64]) -> f64 {
        values.sort_by(f64::total_cmp);
        values[(values.len() * 95).div_ceil(100) - 1]
    }
    let first_p95 = p95(&mut first);
    let repeat_p95 = p95(&mut repeat);
    println!(
        "GPU_CLASSES chunks={} cached_load_and_warm_seconds={cached_load:.6} first_p95_seconds={first_p95:.6} repeat_p95_seconds={repeat_p95:.6} ratio={:.6}",
        first.len(),
        first_p95 / repeat_p95
    );

    assert!(
        first_p95 <= 1.5 * repeat_p95,
        "GPU first requests exceed 1.5x warmed repeats"
    );
}

#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU"]
fn gpu_static_padding_keeps_duration_predictor_deterministic() {
    let (root, cache, device) = paths();
    let cfg = handy_tts::backend_config::config("supertonic-3", &root, device, &cache).unwrap();
    drop(
        handy_tts::graph::with_config(cfg.clone(), || {
            handy_tts::graph::session(&root.join("onnx/duration_predictor.onnx"), 2)
        })
        .unwrap(),
    );
    let mut graph = handy_tts::graph::with_config(cfg, || {
        handy_tts::graph::session(&root.join("onnx/duration_predictor.onnx"), 2)
    })
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("voice_styles/F1.json")).unwrap()).unwrap();
    let style: Vec<Vec<Vec<f32>>> =
        serde_json::from_value(json["style_dp"]["data"].clone()).unwrap();
    let style = ort::value::Tensor::from_array((
        [1, 8, 16],
        style.into_iter().flatten().flatten().collect::<Vec<_>>(),
    ))
    .unwrap();
    let indexer: Vec<i64> =
        serde_json::from_slice(&std::fs::read(root.join("onnx/unicode_indexer.json")).unwrap())
            .unwrap();
    let text = "<en>the application should begin speaking promptly in a calm and consistent voice, preserve the meaning of every paragraph, pronounce unfamiliar names carefully,</en>";
    let ids = text
        .chars()
        .map(|c| indexer[c as usize])
        .collect::<Vec<_>>();
    let n = ids.len();
    let ids = ort::value::Tensor::from_array(([1, n], ids)).unwrap();
    let mask = ort::value::Tensor::from_array(([1, 1, n], vec![1.0f32; n])).unwrap();
    let options = ort::session::RunOptions::new().unwrap();
    let mut durations = vec![];
    for _ in 0..6 {
        let out = graph
            .run_with_options(
                ort::inputs!["text_ids"=>&ids,"text_mask"=>&mask,"style_dp"=>&style],
                &options,
            )
            .unwrap();
        durations.push(out.at(0).unwrap().try_extract_tensor::<f32>().unwrap().1[0]);
    }
    assert!(
        durations
            .iter()
            .all(|duration| duration.is_finite() && *duration == durations[0]),
        "static GPU padding must give stable duration predictions: {durations:?}"
    );
    println!(
        "GPU_STATIC_DURATION repeats={} duration={:.6}",
        durations.len(),
        durations[0]
    );
}

#[test]
#[ignore = "requires opt-in mounted Supertonic graphs and GPU"]
fn gpu_cached_load_and_warm_within_five_seconds() {
    let (root, cache, device) = paths();
    assert_eq!(device, Device::Gpu);
    let engine =
        handy_tts::runtime::load_adapter("supertonic-3", &root, "F1", 2, device, &cache).unwrap();
    drop(engine);
    let start = std::time::Instant::now();
    let engine =
        handy_tts::runtime::load_adapter("supertonic-3", &root, "F1", 2, device, &cache).unwrap();
    let seconds = start.elapsed().as_secs_f64();
    println!("GPU_CACHED_LOAD_AND_WARM seconds={seconds:.6}");
    assert!(
        seconds <= 5.0,
        "cached load plus warm-up exceeds 5 seconds: {seconds}"
    );
    drop(engine);
}
