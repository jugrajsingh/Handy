use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

/// Runs every vocabulary example and prints one PASS or FAIL line each; the built-in
/// vocabulary alone, or merged with the user file at `path`. True only when none failed.
fn check_vocabulary(path: Option<&Path>, out: &mut impl Write) -> Result<bool> {
    let vocabulary = match path {
        None => handy_tts_core::vocabulary::Vocabulary::builtin(),
        Some(path) => {
            let json = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let file = handy_tts_core::vocabulary::Vocabulary::parse(&json)
                .with_context(|| format!("parse {}", path.display()))?;
            handy_tts_core::vocabulary::Vocabulary::merged(&file)
        }
    };
    let results = handy_tts_core::vocabulary::run_examples(&vocabulary);
    let mut failed = 0_usize;
    for result in &results {
        if result.expected == result.actual {
            writeln!(out, "PASS {}", result.text)?;
        } else {
            failed += 1;
            writeln!(
                out,
                "FAIL {}: expected \"{}\" got \"{}\"",
                result.text, result.expected, result.actual
            )?;
        }
    }
    writeln!(
        out,
        "vocabulary: {} examples, {failed} failed",
        results.len()
    )?;
    Ok(failed == 0)
}

fn main() -> Result<()> {
    if std::env::args().skip(1).eq(["--list-devices"]) {
        println!("{}", serde_json::to_string(&handy_tts::devices::list()?)?);
        return Ok(());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--check-vocabulary") {
        anyhow::ensure!(
            args.len() <= 2,
            "--check-vocabulary takes at most one vocabulary file"
        );
        let passed = check_vocabulary(args.get(1).map(Path::new), &mut std::io::stdout().lock())?;
        std::process::exit(i32::from(!passed));
    }
    if args.first().is_some_and(|a| a == "--measure-lanes") {
        anyhow::ensure!(
            args.len() == 7,
            "lane measurement needs model, root, device, cache, corpus and lanes"
        );
        let device = match args[3].as_str() {
            "cpu" => handy_tts_core::device::Device::Cpu,
            "gpu" => handy_tts_core::device::Device::Gpu,
            "npu" => handy_tts_core::device::Device::Npu,
            _ => anyhow::bail!("unknown device"),
        };
        return handy_tts::measure::lane_probe(
            &args[1],
            std::path::Path::new(&args[2]),
            device,
            std::path::Path::new(&args[4]),
            std::path::Path::new(&args[5]),
            args[6].parse()?,
        );
    }
    if args.first().is_some_and(|a| a == "--measure-continuity") {
        anyhow::ensure!(
            args.len() >= 11,
            "continuity needs model, root, device, cache, corpus, output, --speed and --wav"
        );
        let device = match args[3].as_str() {
            "cpu" => handy_tts_core::device::Device::Cpu,
            "gpu" => handy_tts_core::device::Device::Gpu,
            "npu" => handy_tts_core::device::Device::Npu,
            _ => anyhow::bail!("unknown device"),
        };
        let mut speed = None;
        let mut wav = None;
        let mut lanes = None;
        let mut gpu_inputs = None;
        let mut require_long_document = false;
        let mut startup_only = false;
        anyhow::ensure!(
            (args.len() - 7).is_multiple_of(2),
            "continuity options need values"
        );
        for pair in args[7..].as_chunks::<2>().0 {
            match pair[0].as_str() {
                "--speed" => speed = Some(pair[1].parse()?),
                "--wav" => wav = Some(std::path::Path::new(&pair[1])),
                "--lanes" => lanes = Some(pair[1].parse()?),
                "--startup-only" => startup_only = pair[1].parse()?,
                "--require-long-document" => require_long_document = pair[1].parse()?,
                "--gpu-timings" => gpu_inputs = Some(std::path::Path::new(&pair[1])),
                _ => anyhow::bail!("unknown continuity option"),
            }
        }
        return handy_tts::measure::continuity(
            &args[1],
            std::path::Path::new(&args[2]),
            device,
            std::path::Path::new(&args[4]),
            std::path::Path::new(&args[5]),
            std::path::Path::new(&args[6]),
            handy_tts::measure::ContinuityOptions {
                speed: speed.ok_or_else(|| anyhow::anyhow!("--speed required"))?,
                wav: wav.ok_or_else(|| anyhow::anyhow!("--wav required"))?,
                lanes,
                gpu_inputs,
                require_long_document,
                startup_only,
            },
        );
    }
    if args.first().is_some_and(|a| a == "--measure-matrix") {
        anyhow::ensure!(
            args.len() == 8 && args[6] == "--first-audio-inputs",
            "matrix measurement needs model, root, device, cache, corpus and --first-audio-inputs directory"
        );
        let device = match args[3].as_str() {
            "cpu" => handy_tts_core::device::Device::Cpu,
            "gpu" => handy_tts_core::device::Device::Gpu,
            "npu" => handy_tts_core::device::Device::Npu,
            _ => anyhow::bail!("unknown device"),
        };
        return handy_tts::measure::run(
            &args[1],
            std::path::Path::new(&args[2]),
            device,
            std::path::Path::new(&args[4]),
            std::path::Path::new(&args[5]),
            std::path::Path::new(&args[7]),
        );
    }
    if args.first().is_some_and(|a| a == "--measure-once") {
        anyhow::ensure!(
            args.len() == 5,
            "measurement needs model, root, device and cache"
        );
        let device = match args[3].as_str() {
            "cpu" => handy_tts_core::device::Device::Cpu,
            "gpu" => handy_tts_core::device::Device::Gpu,
            "npu" => handy_tts_core::device::Device::Npu,
            _ => anyhow::bail!("unknown device"),
        };
        let model = handy_tts_core::catalog::entry(&args[1])?;
        let start = std::time::Instant::now();
        let mut engine = handy_tts::runtime::load_adapter(
            &args[1],
            std::path::Path::new(&args[2]),
            model.default_voice,
            model.threads,
            device,
            std::path::Path::new(&args[4]),
        )?;
        let load_seconds = start.elapsed().as_secs_f64();
        let plan = handy_tts_core::speed::plan(2.0, model.native_speed)?;
        let begin = std::time::Instant::now();
        let samples = engine.synthesize(
            "Reading clearly, one phrase at a time.",
            Some(plan.native),
            &std::sync::atomic::AtomicBool::new(false),
        )?;
        let synth_seconds = begin.elapsed().as_secs_f64();
        let dsp_start = std::time::Instant::now();
        let output = handy_tts_core::dsp::stretch(
            handy_tts_core::protocol::Pcm {
                id: 1,
                sample_rate: engine.sample_rate(),
                samples,
            },
            plan.stretch,
            0.0,
        )?;
        let dsp_seconds = dsp_start.elapsed().as_secs_f64();
        let audio_seconds = output.samples.len() as f64 / f64::from(output.sample_rate);
        println!(
            "{}",
            serde_json::json!({"model":model.id,"device":device,"engine":if model.id=="kokoro-82m-int8"{"onnx-misaki-rs"}else{"ported-engine"},"placement":engine.placement(),"load_seconds":load_seconds,"synthesis_seconds":synth_seconds,"dsp_seconds":dsp_seconds,"audio_seconds":audio_seconds,"rtf_2x":(synth_seconds+dsp_seconds)/audio_seconds,"finite":output.samples.iter().all(|v|v.is_finite()),"samples":output.samples.len()})
        );
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "--probe-openvino") {
        anyhow::ensure!(args.len() == 4, "probe needs graph, device and cache");
        let info = handy_tts::ov_graph::probe(
            std::path::Path::new(&args[1]),
            &args[2],
            std::path::Path::new(&args[3]),
        )?;
        println!("{}", serde_json::to_string(&info)?);
        return Ok(());
    }
    let model = handy_tts::runtime::parse_model(std::env::args().skip(1))?;
    handy_tts::runtime::run_lanes(
        std::io::BufReader::new(std::io::stdin()),
        std::io::BufWriter::new(std::io::stdout()),
        |root, voice, threads, device, cache, lanes| {
            handy_tts::runtime::load_adapters(&model, root, voice, threads, device, cache, lanes)
        },
        handy_tts::runtime::memory_available,
    )
}
