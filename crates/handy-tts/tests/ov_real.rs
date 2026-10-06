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
    handy_tts::initialize_run_options().unwrap();
    handy_tts::reset_inference().unwrap();
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
            handy_tts::run_options().unwrap(),
        )
        .unwrap();
    let (shape, values) = output.at(0).unwrap().try_extract_tensor::<f32>().unwrap();
    assert_eq!(shape.as_ref(), [1, 256, 4]);
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
