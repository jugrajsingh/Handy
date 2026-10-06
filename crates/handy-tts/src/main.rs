use anyhow::Result;
fn main() -> Result<()> {
    if std::env::args().skip(1).eq(["--list-devices"]) {
        println!("{}", serde_json::to_string(&handy_tts::devices::list()?)?);
        return Ok(());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--measure-continuity") {
        anyhow::ensure!(
            args.len() == 7,
            "continuity needs model, root, device, cache, corpus and output"
        );
        handy_tts::initialize_run_options()?;
        let device = match args[3].as_str() {
            "cpu" => handy_tts_core::device::Device::Cpu,
            "gpu" => handy_tts_core::device::Device::Gpu,
            "npu" => handy_tts_core::device::Device::Npu,
            _ => anyhow::bail!("unknown device"),
        };
        return handy_tts::measure::continuity(
            &args[1],
            std::path::Path::new(&args[2]),
            device,
            std::path::Path::new(&args[4]),
            std::path::Path::new(&args[5]),
            std::path::Path::new(&args[6]),
        );
    }
    if args.first().is_some_and(|a| a == "--measure-matrix") {
        anyhow::ensure!(
            args.len() == 8 && args[6] == "--first-audio-inputs",
            "matrix measurement needs model, root, device, cache, corpus and --first-audio-inputs directory"
        );
        handy_tts::initialize_run_options()?;
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
        handy_tts::initialize_run_options()?;
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
        let native = model.id != "pocket-en";
        let begin = std::time::Instant::now();
        let samples = engine.synthesize(
            "Reading clearly, one phrase at a time.",
            if native { Some(2.0) } else { None },
            &std::sync::atomic::AtomicBool::new(false),
        )?;
        let synth_seconds = begin.elapsed().as_secs_f64();
        let dsp_start = std::time::Instant::now();
        let output = handy_tts_core::dsp::transform(
            handy_tts_core::protocol::Pcm {
                id: 1,
                sample_rate: engine.sample_rate(),
                samples,
            },
            2.0,
            0.0,
            native,
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
    handy_tts::initialize_run_options()?;
    handy_tts::runtime::run(
        std::io::BufReader::new(std::io::stdin()),
        std::io::BufWriter::new(std::io::stdout()),
        |root, voice, threads, device, cache| {
            handy_tts::runtime::load_adapter(&model, root, voice, threads, device, cache)
        },
        handy_tts::runtime::memory_available,
    )
}
