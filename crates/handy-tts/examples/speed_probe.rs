use anyhow::{Context, Result, bail, ensure};
use handy_tts::runtime::{SynthEngine, load_adapter};
use handy_tts_core::{catalog, device::Device, dsp, protocol::Pcm, text::TextCursor};
use serde_json::json;
use std::{
    collections::BTreeSet, fs, io::Write, path::PathBuf, sync::atomic::AtomicBool, time::Instant,
};
use unicode_normalization::UnicodeNormalization;

struct Args {
    model: String,
    root: PathBuf,
    device: Device,
    cache: PathBuf,
    texts: Vec<PathBuf>,
    native: Vec<f32>,
    stretch: Vec<f32>,
    out: PathBuf,
    max_words: usize,
    paragraph: bool,
    repeat: bool,
}

fn speeds(value: &str) -> Result<Vec<f32>> {
    let speeds = value
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<f32>, _>>()?;
    ensure!(
        !speeds.is_empty() && speeds.iter().all(|v| (0.5..=2.0).contains(v)),
        "invalid speeds"
    );
    Ok(speeds)
}

fn arguments() -> Result<Args> {
    parse_arguments(std::env::args().skip(1))
}

fn parse_arguments(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut args = args.into_iter().peekable();
    let mut values = std::collections::BTreeMap::new();
    let mut texts = Vec::new();
    let mut paragraph = false;
    let mut repeat = true;
    while let Some(key) = args.next() {
        if key == "--first-paragraph" {
            paragraph = true;
            continue;
        }
        if key == "--no-repeat" {
            repeat = false;
            continue;
        }
        ensure!(
            [
                "--model",
                "--root",
                "--device",
                "--cache",
                "--text",
                "--native",
                "--stretch",
                "--out",
                "--max-words"
            ]
            .contains(&key.as_str()),
            "unknown option {key}"
        );
        if key == "--text" {
            let before = texts.len();
            while let Some(value) = args.next_if(|value| !value.starts_with("--")) {
                texts.push(PathBuf::from(value));
            }
            ensure!(texts.len() > before, "missing value for {key}");
        } else {
            let value = args
                .next()
                .with_context(|| format!("missing value for {key}"))?;
            ensure!(values.insert(key, value).is_none(), "duplicate option");
        }
    }
    let get = |key: &str| {
        values
            .get(key)
            .with_context(|| format!("required option {key}"))
    };
    let device = match get("--device")?.as_str() {
        "cpu" => Device::Cpu,
        "gpu" => Device::Gpu,
        "npu" => Device::Npu,
        _ => bail!("device must be cpu, gpu or npu"),
    };
    ensure!(!texts.is_empty(), "at least one --text is required");
    Ok(Args {
        model: get("--model")?.clone(),
        root: get("--root")?.into(),
        device,
        cache: get("--cache")?.into(),
        texts,
        native: speeds(get("--native")?)?,
        stretch: speeds(get("--stretch")?)?,
        out: get("--out")?.into(),
        max_words: values
            .get("--max-words")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(usize::MAX),
        paragraph,
        repeat,
    })
}

fn limit_words(source: &str, maximum: usize) -> &str {
    let mut words = 0;
    let mut inside = false;
    for (index, ch) in source.char_indices() {
        if ch.is_whitespace() {
            inside = false;
        } else if !inside {
            if words == maximum {
                return source[..index].trim_end();
            }
            words += 1;
            inside = true;
        }
    }
    source
}

fn first_paragraph(source: &str) -> &str {
    source
        .split_once("\n\n")
        .map_or(source, |(paragraph, _)| paragraph)
}

fn text_ids(text: &str) -> usize {
    // Mirrors the pinned Supertonic character normalization before language tags.
    let mut text: String = text.nfkd().collect();
    for (from, to) in [
        ("\u{2013}", "-"),
        ("\u{2011}", "-"),
        ("\u{2014}", "-"),
        ("_", " "),
        ("\u{201c}", "\""),
        ("\u{201d}", "\""),
        ("\u{2018}", "'"),
        ("\u{2019}", "'"),
        ("´", "'"),
        ("`", "'"),
        ("[", " "),
        ("]", " "),
        ("|", " "),
        ("/", " "),
        ("#", " "),
        ("→", " "),
        ("←", " "),
        ("@", " at "),
        ("e.g.,", "for example, "),
        ("i.e.,", "that is, "),
    ] {
        text = text.replace(from, to);
    }
    for symbol in ["♥", "☆", "♡", "©", "\\"] {
        text = text.replace(symbol, "");
    }
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !text.ends_with(['.', '!', '?', ':', ';', ',', '\"', '\'']) {
        text.push('.');
    }
    text.chars().count() + "<en></en>".len()
}

fn frame_count(samples: usize, stride: usize) -> usize {
    samples.div_ceil(stride)
}

fn queue_capacity_split<'a>(pending: &mut Vec<&'a str>, part: &'a str) -> Result<()> {
    let split = part
        .char_indices()
        .filter(|(i, ch)| *i > 0 && ch.is_whitespace() && !part[*i..].trim().is_empty())
        .min_by_key(|(i, _)| i.abs_diff(part.len() / 2))
        .map(|(i, _)| i)
        .context("NPU chunk cannot split")?;
    pending.push(part[split..].trim_start());
    pending.push(part[..split].trim_end());
    Ok(())
}

fn hybrid_ratio(requested: f32, native: f32) -> Option<f32> {
    let ratio = requested / native;
    (0.5..=2.0).contains(&ratio).then_some(ratio)
}

fn write_wav(writer: &mut impl Write, rate: u32, samples: &[f32]) -> Result<()> {
    ensure!(
        rate > 0 && samples.iter().all(|v| v.is_finite()),
        "invalid WAV PCM"
    );
    let bytes = u32::try_from(samples.len().checked_mul(2).context("WAV size overflow")?)?;
    let riff = bytes.checked_add(36).context("RIFF size overflow")?;
    let byte_rate = rate.checked_mul(2).context("WAV rate overflow")?;
    writer.write_all(b"RIFF")?;
    writer.write_all(&riff.to_le_bytes())?;
    writer.write_all(b"WAVEfmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    for value in [1u16, 1] {
        writer.write_all(&value.to_le_bytes())?;
    }
    writer.write_all(&rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    for value in [2u16, 16] {
        writer.write_all(&value.to_le_bytes())?;
    }
    writer.write_all(b"data")?;
    writer.write_all(&bytes.to_le_bytes())?;
    for value in samples {
        writer.write_all(&((value.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())?;
    }
    Ok(())
}

struct Probe {
    args: Args,
    engine: Box<dyn SynthEngine>,
    stride: Option<usize>,
    seen: BTreeSet<(usize, usize)>,
    log: fs::File,
}

impl Probe {
    fn record(&mut self, value: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.log, &value).context("write measurement row")?;
        self.log
            .write_all(b"\n")
            .context("finish measurement row")?;
        self.log.flush().context("flush measurement row")
    }

    fn synthesize(
        &mut self,
        text: &str,
        native: f32,
        document: &str,
        index: usize,
    ) -> Result<Vec<Pcm>> {
        let mut pending = vec![text];
        let mut chunks = Vec::new();
        while let Some(part) = pending.pop() {
            let began = Instant::now();
            let result = self
                .engine
                .synthesize(part, Some(native), &AtomicBool::new(false));
            let seconds = began.elapsed().as_secs_f64();
            let samples = match result {
                Ok(samples) => samples,
                Err(error)
                    if self.args.device == Device::Npu
                        && error.is::<handy_tts::padding::StaticCapacity>() =>
                {
                    queue_capacity_split(&mut pending, part)?;
                    self.record(json!({"kind":"static_split", "document":document, "chunk_index":index, "text":part, "native_speed":native, "seconds":seconds}))?;
                    continue;
                }
                Err(error) => return Err(error).context("synthesize probe chunk"),
            };
            ensure!(
                !samples.is_empty() && samples.iter().all(|v| v.is_finite()),
                "invalid engine PCM"
            );
            let ids = self.stride.map(|_| text_ids(part));
            let frames = self.stride.map(|stride| frame_count(samples.len(), stride));
            let first_use = self.seen.insert((
                ids.unwrap_or(part.chars().count()),
                frames.unwrap_or(samples.len()),
            ));
            self.record(json!({"kind":"synthesis", "model":self.args.model, "device":self.args.device,
                "document":document, "chunk_index":index, "part_index":chunks.len(), "text":part, "native_speed":native,
                "samples":samples.len(), "sample_rate":self.engine.sample_rate(), "synthesis_seconds":seconds,
                "first_use":first_use, "repeat":false, "text_id_count":ids, "latent_frame_count":frames}))?;
            if self.args.repeat {
                let began = Instant::now();
                let repeat = self
                    .engine
                    .synthesize(part, Some(native), &AtomicBool::new(false))
                    .context("repeat probe chunk")?;
                self.record(json!({"kind":"synthesis", "model":self.args.model, "device":self.args.device,
                "document":document, "chunk_index":index, "part_index":chunks.len(), "text":part, "native_speed":native,
                "samples":repeat.len(), "sample_rate":self.engine.sample_rate(), "synthesis_seconds":began.elapsed().as_secs_f64(),
                "first_use":false, "repeat":true, "text_id_count":ids,
                "latent_frame_count":self.stride.map(|stride| frame_count(repeat.len(), stride))}))?;
            }
            chunks.push(Pcm {
                id: index as u64,
                sample_rate: self.engine.sample_rate(),
                samples,
            });
        }
        Ok(chunks)
    }

    fn render(&mut self, stem: &str, native: f32, chunks: &[Pcm]) -> Result<()> {
        let audio: Vec<_> = chunks
            .iter()
            .flat_map(|pcm| pcm.samples.iter().copied())
            .collect();
        let path = self.args.out.join(format!("{stem}-native{native:.1}.wav"));
        write_wav(
            &mut fs::File::create(path).context("create native WAV")?,
            self.engine.sample_rate(),
            &audio,
        )?;
        for requested in self.args.stretch.clone() {
            let Some(ratio) = hybrid_ratio(requested, native) else {
                self.record(json!({"kind":"skipped_hybrid", "stem":stem, "native_speed":native, "requested_speed":requested, "stretch_ratio":requested/native}))?;
                continue;
            };
            let name = if native == 1.0 {
                format!("{stem}-native1.0-signalsmith{requested:.1}.wav")
            } else {
                format!("{stem}-native{native:.1}-signalsmith{ratio:.6}.wav")
            };
            let began = Instant::now();
            let stretched = chunks
                .iter()
                .map(|pcm| dsp::stretch(pcm.clone(), ratio, 0.0))
                .collect::<Result<Vec<_>, _>>()?;
            let seconds = began.elapsed().as_secs_f64();
            let samples: Vec<_> = stretched.into_iter().flat_map(|pcm| pcm.samples).collect();
            let played = samples.len() as f64 / f64::from(self.engine.sample_rate());
            write_wav(
                &mut fs::File::create(self.args.out.join(&name)).context("create stretch WAV")?,
                self.engine.sample_rate(),
                &samples,
            )?;
            self.record(json!({"kind":"stretch", "file":name, "native_speed":native, "requested_speed":requested,
                "stretch_ratio":ratio, "seconds":seconds, "played_seconds":played, "seconds_per_played_second":seconds/played}))?;
        }
        Ok(())
    }

    fn document(&mut self, path: &std::path::Path) -> Result<()> {
        let source = fs::read_to_string(path).context("read probe document")?;
        let source = if self.args.paragraph {
            first_paragraph(&source)
        } else {
            &source
        };
        let source = limit_words(source, self.args.max_words);
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("invalid text filename")?;
        let name = if self.args.paragraph {
            format!("{name}-paragraph")
        } else {
            name.to_owned()
        };
        let device = serde_json::to_value(self.args.device)?
            .as_str()
            .context("device name")?
            .to_owned();
        let stem = format!("{}-{device}-{name}", self.args.model);
        fs::write(self.args.out.join(format!("{stem}-expected.txt")), source)
            .context("write exact expected text")?;
        for native in self.args.native.clone() {
            let mut cursor = TextCursor::new(source, false)?;
            let mut audio = Vec::new();
            while let Some(chunk) = cursor.next_chunk()? {
                audio.extend(self.synthesize(&chunk.text, native, &name, chunk.index)?);
            }
            ensure!(!audio.is_empty(), "document contains no speech");
            self.render(&stem, native, &audio)?;
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = arguments()?;
    let model = catalog::entry(&args.model)?;
    ensure!(
        model.id != "pocket-en" || args.native == [1.0],
        "Pocket has no native speed control"
    );
    fs::create_dir_all(&args.out).context("create probe output")?;
    let stride = if args.model == "supertonic-3" {
        let config: serde_json::Value = serde_json::from_slice(
            &fs::read(args.root.join("onnx/tts.json")).context("read Supertonic tts.json")?,
        )?;
        let base = config["ae"]["base_chunk_size"]
            .as_u64()
            .context("base_chunk_size missing")?;
        let compress = config["ttl"]["chunk_compress_factor"]
            .as_u64()
            .context("chunk_compress_factor missing")?;
        let stride = usize::try_from(
            base.checked_mul(compress)
                .context("frame stride overflow")?,
        )?;
        ensure!(stride > 0, "frame stride must be positive");
        Some(stride)
    } else {
        None
    };
    let began = Instant::now();
    let engine = load_adapter(
        &args.model,
        &args.root,
        model.default_voice,
        model.threads,
        args.device,
        &args.cache,
    )
    .context("load probe engine")?;
    let load = began.elapsed().as_secs_f64();
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(
            args.out
                .join(format!("{}-{:?}.jsonl", args.model, args.device).to_lowercase()),
        )
        .context("open probe JSONL")?;
    let mut probe = Probe {
        args,
        engine,
        stride,
        seen: BTreeSet::new(),
        log,
    };
    probe.record(json!({"kind":"load", "model":probe.args.model, "device":probe.args.device, "seconds":load, "placement":probe.engine.placement()}))?;
    for path in probe.args.texts.clone() {
        probe.document(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn parses_step_three_multifile_text_and_repeated_flags() {
        let argv = [
            "--model",
            "supertonic-3",
            "--root",
            "/models/supertonic3",
            "--device",
            "gpu",
            "--cache",
            "/src/.superpowers/scratch/read-aloud-streaming/ov-cache",
            "--text",
            "/src/crates/handy-tts-core/tests/fixtures/speech/owner-2x.txt",
            "/src/crates/handy-tts-core/tests/fixtures/text/01-heading.txt",
            "/src/crates/handy-tts-core/tests/fixtures/text/06-long-doc.txt",
            "--max-words",
            "500",
            "--native",
            "0.5,0.6,0.7,0.8,0.9,1.0,1.1,1.2,1.3,1.4,1.5,1.6,1.7,1.8,1.9,2.0",
            "--stretch",
            "1.5,2.0",
            "--out",
            "/src/.superpowers/scratch/read-aloud-streaming/sweep",
        ];
        let parsed = parse_arguments(argv.map(str::to_owned)).unwrap();
        assert_eq!(parsed.model, "supertonic-3");
        assert_eq!(parsed.root, PathBuf::from(argv[3]));
        assert_eq!(parsed.device, Device::Gpu);
        assert_eq!(parsed.cache, PathBuf::from(argv[7]));
        assert_eq!(
            parsed.texts,
            argv[9..12].iter().map(PathBuf::from).collect::<Vec<_>>()
        );
        assert_eq!(parsed.max_words, 500);
        assert_eq!(
            parsed.native,
            (5..=20).map(|i| i as f32 / 10.0).collect::<Vec<_>>()
        );
        assert_eq!(parsed.stretch, [1.5, 2.0]);
        assert_eq!(parsed.out, PathBuf::from(argv[19]));
        assert!(parsed.repeat && !parsed.paragraph);
        let mut repeated = argv.to_vec();
        repeated.insert(11, "--text");
        repeated.insert(10, "--text");
        assert_eq!(
            parse_arguments(repeated.into_iter().map(str::to_owned))
                .unwrap()
                .texts,
            parsed.texts
        );
        let missing_text = argv[..9]
            .iter()
            .chain(argv[12..].iter())
            .map(|value| (*value).to_owned());
        assert!(
            parse_arguments(missing_text)
                .err()
                .unwrap()
                .to_string()
                .contains("missing value for --text")
        );
    }
    #[test]
    fn composed_probe_records_chunks_repeats_and_exact_hybrid_audio() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Fake(Arc<AtomicUsize>);
        impl SynthEngine for Fake {
            fn synthesize(
                &mut self,
                text: &str,
                speed: Option<f32>,
                _: &AtomicBool,
            ) -> Result<Vec<f32>> {
                self.0.fetch_add(1, Ordering::Relaxed);
                let len = (text.chars().count() as f32 * 1000.0
                    / speed.context("native speed missing")?)
                .round() as usize;
                Ok((0..len).map(|i| (i as f32 * 0.1).sin() * 0.2).collect())
            }
            fn sample_rate(&self) -> u32 {
                24000
            }
        }
        let root = PathBuf::from(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap());
        let scratch = Scratch(root.join(format!("s2-probe-{}", std::process::id())));
        fs::create_dir(&scratch.0).unwrap();
        let input = scratch.0.join("owner-2x.txt");
        let source = "# A clear heading\n\nRead the whole sentence, including its final words.";
        fs::write(&input, source).unwrap();
        let log_path = scratch.0.join("probe.jsonl");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut probe = Probe {
            args: Args {
                model: "supertonic-3".into(),
                root: scratch.0.clone(),
                device: Device::Cpu,
                cache: scratch.0.clone(),
                texts: vec![input.clone()],
                native: vec![1.0, 1.1],
                stretch: vec![2.0],
                out: scratch.0.clone(),
                max_words: usize::MAX,
                paragraph: false,
                repeat: true,
            },
            engine: Box::new(Fake(Arc::clone(&calls))),
            stride: Some(100),
            seen: BTreeSet::new(),
            log: fs::File::create(&log_path).unwrap(),
        };
        probe.document(&input).unwrap();
        let rows: Vec<serde_json::Value> = fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect();
        let mut cursor = TextCursor::new(source, false).unwrap();
        let mut expected = Vec::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            expected.push(chunk.text);
        }
        let actual: Vec<_> = rows
            .iter()
            .filter(|row| {
                row["kind"] == "synthesis" && row["repeat"] == false && row["native_speed"] == 1.0
            })
            .map(|row| row["text"].as_str().unwrap())
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            rows.iter().filter(|row| row["repeat"] == true).count(),
            expected.len() * 2
        );
        assert_eq!(calls.load(Ordering::Relaxed), expected.len() * 4);
        let mut exact_samples = 0;
        for row in rows.iter().filter(|row| {
            row["kind"] == "synthesis" && row["repeat"] == false && row["native_speed"] != 1.0
        }) {
            exact_samples += (row["samples"].as_u64().unwrap() as f64 / f64::from(2.0f32 / 1.1))
                .round() as usize;
        }
        let wav = fs::read(
            scratch
                .0
                .join("supertonic-3-cpu-owner-2x-native1.1-signalsmith1.818182.wav"),
        )
        .unwrap();
        assert_eq!(wav.len(), 44 + exact_samples * 2);
        assert!(rows.iter().any(|row| row["kind"] == "stretch"
            && row["stretch_ratio"].as_f64().map(|ratio| ratio as f32) == Some(2.0f32 / 1.1)));
        probe.args.repeat = false;
        let before = calls.load(Ordering::Relaxed);
        probe
            .synthesize("One complete sentence.", 1.0, "no-repeat", 0)
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), before + 1);
        let rows: Vec<serde_json::Value> = fs::read_to_string(scratch.0.join("probe.jsonl"))
            .unwrap()
            .lines()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect();
        assert_eq!(
            rows.iter()
                .filter(|row| row["document"] == "no-repeat")
                .count(),
            1
        );
    }
    #[test]
    fn capacity_split_preserves_both_halves_for_stack_order() {
        let source = "Alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa.";
        let mut pending = vec!["existing"];
        queue_capacity_split(&mut pending, source).unwrap();
        let left = pending.pop().unwrap();
        let right = pending.pop().unwrap();
        assert_eq!(format!("{left} {right}"), source);
        assert_eq!(pending, ["existing"]);
        assert!(queue_capacity_split(&mut Vec::new(), "unsplittable").is_err());
    }
    #[test]
    fn npu_capacity_splits_preserve_all_text_and_pcm_in_order() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct BoundedNpu(Vec<String>);
        impl SynthEngine for BoundedNpu {
            fn synthesize(
                &mut self,
                text: &str,
                _: Option<f32>,
                _: &AtomicBool,
            ) -> Result<Vec<f32>> {
                if text.split_whitespace().count() > 2 {
                    return Err(anyhow::Error::new(handy_tts::padding::StaticCapacity)
                        .context("fake bounded NPU"));
                }
                Ok(text
                    .split_whitespace()
                    .flat_map(|word| {
                        let position = self.0.iter().position(|expected| expected == word).unwrap();
                        std::iter::repeat_n((position + 1) as f32 / 32.0, 16)
                    })
                    .collect())
            }
            fn sample_rate(&self) -> u32 {
                24000
            }
        }
        struct Failure(Arc<AtomicUsize>);
        impl SynthEngine for Failure {
            fn synthesize(&mut self, _: &str, _: Option<f32>, _: &AtomicBool) -> Result<Vec<f32>> {
                self.0.fetch_add(1, Ordering::Relaxed);
                bail!("ordinary engine failure")
            }
            fn sample_rate(&self) -> u32 {
                24000
            }
        }
        let source = "Ant bat cat dog eel fox gum hat ink jet key log map nut owl pig.";
        let root = PathBuf::from(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap());
        let scratch = Scratch(root.join(format!("s2-npu-probe-{}", std::process::id())));
        fs::create_dir(&scratch.0).unwrap();
        let input = scratch.0.join("npu.txt");
        fs::write(&input, source).unwrap();
        let mut cursor = TextCursor::new(source, false).unwrap();
        let prepared = cursor.next_chunk().unwrap().unwrap().text;
        assert!(cursor.next_chunk().unwrap().is_none());
        let words: Vec<_> = prepared.split_whitespace().map(str::to_owned).collect();
        let log_path = scratch.0.join("probe.jsonl");
        let mut probe = Probe {
            args: Args {
                model: "supertonic-3".into(),
                root: scratch.0.clone(),
                device: Device::Npu,
                cache: scratch.0.clone(),
                texts: vec![input.clone()],
                native: vec![1.0],
                stretch: vec![],
                out: scratch.0.clone(),
                max_words: usize::MAX,
                paragraph: false,
                repeat: false,
            },
            engine: Box::new(BoundedNpu(words.clone())),
            stride: Some(100),
            seen: BTreeSet::new(),
            log: fs::File::create(&log_path).unwrap(),
        };
        probe.document(&input).unwrap();
        let rows: Vec<serde_json::Value> = fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .map(|row| serde_json::from_str(row).unwrap())
            .collect();
        let successful: Vec<_> = rows
            .iter()
            .filter(|row| row["kind"] == "synthesis")
            .collect();
        let actual_words: Vec<_> = successful
            .iter()
            .flat_map(|row| row["text"].as_str().unwrap().split_whitespace())
            .collect();
        assert_eq!(actual_words, words);
        assert!(
            successful
                .iter()
                .all(|row| row["text"].as_str().unwrap().split_whitespace().count() <= 2)
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row["kind"] == "static_split")
                .count(),
            7
        );
        let audio = fs::read(scratch.0.join("supertonic-3-npu-npu-native1.0.wav")).unwrap();
        let actual_pcm: Vec<_> = audio[44..]
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
            .collect();
        let expected_pcm: Vec<_> = (0..words.len())
            .flat_map(|position| {
                std::iter::repeat_n(
                    (((position + 1) as f32 / 32.0) * 32767.0).round() as i16,
                    16,
                )
            })
            .collect();
        assert_eq!(actual_pcm, expected_pcm);

        let before = fs::read(&log_path).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        probe.engine = Box::new(Failure(Arc::clone(&calls)));
        let error = probe.document(&input).unwrap_err();
        assert_eq!(error.root_cause().to_string(), "ordinary engine failure");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(fs::read(&log_path).unwrap(), before);
    }
    #[test]
    fn max_words_preserves_markdown_and_paragraph_selection() {
        assert_eq!(limit_words("# Heading\n\nA fine day.", 3), "# Heading\n\nA");
        assert_eq!(
            first_paragraph("One paragraph.\n\nSecond."),
            "One paragraph."
        );
    }
    #[test]
    fn shapes_match_tagged_normalization_and_frame_ceiling() {
        assert_eq!(
            text_ids("Hello world"),
            "<en>Hello world.</en>".chars().count()
        );
        assert_eq!(
            text_ids("[A] e.g., café@home"),
            "<en>A for example, cafe\u{301} at home.</en>"
                .chars()
                .count()
        );
        assert_eq!(frame_count(1001, 100), 11);
    }
    #[test]
    fn wav_is_mono_little_endian_pcm_with_exact_header_and_clipping() {
        let mut bytes = Vec::new();
        write_wav(&mut bytes, 24000, &[-2.0, 0.0, 2.0]).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[22..24], &1u16.to_le_bytes());
        assert_eq!(&bytes[24..28], &24000u32.to_le_bytes());
        assert_eq!(&bytes[40..44], &6u32.to_le_bytes());
        assert_eq!(&bytes[44..], &[1, 128, 0, 0, 255, 127]);
        assert!(write_wav(&mut Vec::new(), 24000, &[f32::NAN]).is_err());
    }
    #[test]
    fn hybrid_ratio_is_exact_and_outside_interval_is_skipped() {
        assert_eq!(hybrid_ratio(2.0, 1.1), Some(2.0 / 1.1));
        assert_eq!(hybrid_ratio(2.0, 0.5), None);
        assert_eq!(hybrid_ratio(1.5, 0.6), None);
    }
}
