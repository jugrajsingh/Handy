use crate::graph::Session;
use anyhow::{Context, Result, bail, ensure};
use ort::value::{DynValue, Tensor};
use rand::{SeedableRng, rngs::StdRng};
use rand_distr::{Distribution, StandardNormal};
use sentencepiece_rs::SentencePieceProcessor;
use serde::Deserialize;
use serde_json::Value as Json;
use std::{fs, path::Path};

#[derive(Deserialize)]
struct StateSpec {
    index: usize,
    input_name: String,
    module: String,
    key: String,
    shape: Vec<usize>,
    dtype: String,
    fill: String,
}
fn floats(shape: impl Into<Vec<usize>>, data: Vec<f32>) -> Result<DynValue> {
    let shape = shape.into();
    let array = ndarray::ArrayD::from_shape_vec(ndarray::IxDyn(&shape), data)
        .context("shape Pocket float tensor")?;
    Ok(Tensor::from_array(array)
        .context("create Pocket float tensor")?
        .into_dyn())
}
fn initial(spec: &StateSpec) -> Result<DynValue> {
    let count: usize = spec.shape.iter().product();
    match spec.dtype.as_str() {
        "float32" => floats(
            spec.shape.clone(),
            vec![
                if spec.fill == "nan" {
                    f32::NAN
                } else if spec.fill == "ones" {
                    1.0
                } else {
                    0.0
                };
                count
            ],
        ),
        "int64" => Ok(Tensor::from_array((
            spec.shape.clone(),
            vec![i64::from(spec.fill == "ones"); count],
        ))?
        .into_dyn()),
        "bool" => Ok(
            Tensor::from_array((spec.shape.clone(), vec![spec.fill == "ones"; count]))?.into_dyn(),
        ),
        dtype => bail!("unsupported Pocket state dtype {dtype}"),
    }
}
fn forward(
    session: &mut Session,
    specs: &[StateSpec],
    state: &mut Vec<DynValue>,
    mut inputs: Vec<(String, DynValue)>,
    offset: usize,
) -> Result<Vec<DynValue>> {
    inputs.extend(
        specs
            .iter()
            .zip(state.iter())
            .map(|(spec, value)| Ok((spec.input_name.clone(), alias(value)?)))
            .collect::<Result<Vec<_>>>()?,
    );
    let outputs = session
        .run_with_options(inputs, crate::run_options()?)
        .context("run Pocket stateful graph")?;
    let values: Vec<DynValue> = outputs
        .iter()
        .map(|(_, value)| alias(value))
        .collect::<Result<_>>()?;
    *state = specs
        .iter()
        .map(|spec| alias(&values[offset + spec.index]))
        .collect::<Result<_>>()?;
    Ok(values)
}
fn alias(value: &DynValue) -> Result<DynValue> {
    value
        .view()
        .try_upgrade()
        .map_err(|_| anyhow::anyhow!("Pocket state must own its backing tensor"))
}
pub struct Pocket {
    main: Session,
    flow: Session,
    decoder: Session,
    conditioner: Session,
    flow_specs: Vec<StateSpec>,
    mimi_specs: Vec<StateSpec>,
    voice: Vec<DynValue>,
    tokenizer: SentencePieceProcessor,
    rng: StdRng,
}
impl Pocket {
    pub fn load(root: &Path, threads: usize, voice: &str) -> Result<Self> {
        let dir = root.join("onnx/english_2026-04");
        let metadata: Json = serde_json::from_slice(
            &fs::read(dir.join("bundle.json")).context("read Pocket bundle")?,
        )
        .context("parse Pocket bundle")?;
        ensure!(
            metadata["latent_dim"] == 32
                && metadata["conditioning_dim"] == 1024
                && metadata["sample_rate"] == 24000,
            "unexpected Pocket bundle dimensions"
        );
        let mut engine = Self {
            main: crate::session(&dir.join("flow_lm_main_int8.onnx"), threads)?,
            flow: crate::session(&dir.join("flow_lm_flow_int8.onnx"), threads)?,
            decoder: crate::session(&dir.join("mimi_decoder_int8.onnx"), threads)?,
            conditioner: crate::session(&dir.join("text_conditioner.onnx"), threads)?,
            flow_specs: serde_json::from_value(metadata["flow_lm_state_manifest"].clone())
                .context("parse Pocket flow state manifest")?,
            mimi_specs: serde_json::from_value(metadata["mimi_state_manifest"].clone())
                .context("parse Pocket decoder state manifest")?,
            voice: vec![],
            tokenizer: SentencePieceProcessor::open(dir.join("tokenizer.model"))
                .context("load Pocket SentencePiece tokenizer")?,
            rng: StdRng::seed_from_u64(20261001),
        };
        engine.voice(root, voice)?;
        Ok(engine)
    }
    pub fn voice(&mut self, root: &Path, name: &str) -> Result<()> {
        let bytes = fs::read(root.join(format!(
            "languages/english_2026-04/embeddings/{name}.safetensors"
        )))
        .context("read Pocket preset state")?;
        ensure!(bytes.len() >= 8, "short safetensors file");
        let length = u64::from_le_bytes(
            bytes[..8]
                .try_into()
                .context("read safetensors header length")?,
        ) as usize;
        let header: Json = serde_json::from_slice(
            bytes
                .get(8..8 + length)
                .context("safetensors header out of bounds")?,
        )
        .context("parse safetensors header")?;
        let data = bytes
            .get(8 + length..)
            .context("missing safetensors data")?;
        let mut states = Vec::new();
        for spec in &self.flow_specs {
            let key = format!("{}/{}", spec.module, spec.key);
            let source = if spec.key == "step" {
                header.get(format!("{}/offset", spec.module))
            } else {
                header.get(&key)
            };
            let Some(source) = source else {
                states.push(initial(spec)?);
                continue;
            };
            let offsets: Vec<usize> = serde_json::from_value(source["data_offsets"].clone())
                .context("parse voice offsets")?;
            let raw = data
                .get(offsets[0]..offsets[1])
                .context("voice data offset out of bounds")?;
            if spec.dtype == "int64" {
                ensure!(
                    source["dtype"] == "I64" && raw.len() == 8,
                    "voice step must be int64 scalar"
                );
                let step = i64::from_le_bytes(raw.try_into().context("read voice offset scalar")?);
                states.push(Tensor::from_array(([1], vec![step]))?.into_dyn());
            } else {
                ensure!(
                    spec.dtype == "float32" && source["dtype"] == "F32",
                    "voice cache must be float32"
                );
                let shape: Vec<usize> = serde_json::from_value(source["shape"].clone())
                    .context("parse voice cache shape")?;
                ensure!(
                    shape.len() == spec.shape.len()
                        && raw.len() == shape.iter().product::<usize>() * 4,
                    "incompatible voice cache shape"
                );
                let mut target = vec![
                    if spec.fill == "nan" { f32::NAN } else { 0.0 };
                    spec.shape.iter().product()
                ];
                for (linear, chunk) in raw.as_chunks::<4>().0.iter().enumerate() {
                    let mut remainder = linear;
                    let mut dest = 0;
                    let mut stride = 1;
                    let mut fits = true;
                    for dimension in (0..shape.len()).rev() {
                        let coordinate = remainder % shape[dimension];
                        remainder /= shape[dimension];
                        fits &= coordinate < spec.shape[dimension];
                        dest += coordinate * stride;
                        stride *= spec.shape[dimension];
                    }
                    if fits {
                        target[dest] = f32::from_le_bytes(*chunk);
                    }
                }
                states.push(floats(spec.shape.clone(), target)?);
            }
        }
        self.voice = states;
        Ok(())
    }
    fn prepare(text: &str) -> String {
        let mut chars = text.trim().chars();
        let mut prepared = chars
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default();
        prepared.extend(chars);
        if prepared.chars().last().is_some_and(char::is_alphanumeric) {
            prepared.push('.');
        }
        prepared
    }
    fn split(&self, text: &str, punctuation: &str) -> Result<Vec<(usize, String)>> {
        let ids = self
            .tokenizer
            .encode_to_ids(text)
            .context("tokenize Pocket native phrase")?;
        let boundaries = self
            .tokenizer
            .encode_to_ids(punctuation)
            .context("tokenize Pocket punctuation")?;
        let mut starts = vec![0];
        let mut previous = false;
        for (i, id) in ids.iter().enumerate() {
            if boundaries.iter().skip(1).any(|b| b == id) {
                previous = true;
            } else {
                if previous {
                    starts.push(i);
                }
                previous = false;
            }
        }
        starts.push(ids.len());
        starts
            .windows(2)
            .map(|w| {
                Ok((
                    w[1] - w[0],
                    self.tokenizer
                        .decode_ids(&ids[w[0]..w[1]])
                        .context("decode Pocket native phrase")?,
                ))
            })
            .collect()
    }
    fn native_chunks(&self, text: &str) -> Result<Vec<String>> {
        let mut segments = vec![];
        for (count, part) in self.split(&Self::prepare(text), ".!...?")? {
            if count > 50 {
                segments.extend(self.split(part.trim(), ",;:")?);
            } else {
                segments.push((count, part));
            }
        }
        let mut chunks = vec![];
        let mut current = String::new();
        let mut count = 0;
        for (n, part) in segments {
            if !current.is_empty() && count + n > 50 {
                chunks.push(current.trim().to_string());
                current.clear();
                count = 0;
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&part);
            count += n;
        }
        if !current.trim().is_empty() {
            chunks.push(current.trim().to_string());
        }
        Ok(chunks)
    }
    fn phrase(&mut self, text: &str) -> Result<Vec<f32>> {
        let prepared = Self::prepare(text);
        let ids = self
            .tokenizer
            .encode_to_ids(&prepared)
            .context("encode Pocket text")?;
        let limit = ((ids.len() as f64 / 3.0 + 2.0) * 12.5).ceil() as usize;
        let ids: Vec<i64> = ids.into_iter().map(|x| x as i64).collect();
        let text_output = self
            .conditioner
            .run_with_options(
                ort::inputs!["token_ids"=>Tensor::from_array(([1,ids.len()],ids))?],
                crate::run_options()?,
            )
            .context("run Pocket text conditioner")?;
        let (shape, embedding) = text_output
            .at(0)?
            .try_extract_tensor::<f32>()
            .context("read Pocket text embeddings")?;
        let mut dims: Vec<usize> = shape.iter().map(|x| *x as usize).collect();
        if dims.len() == 2 {
            dims.insert(0, 1);
        }
        let embedding = floats(dims, embedding.to_vec())?;
        drop(text_output);
        let mut state = self.voice.iter().map(alias).collect::<Result<Vec<_>>>()?;
        forward(
            &mut self.main,
            &self.flow_specs,
            &mut state,
            vec![
                ("sequence".into(), floats(vec![1, 0, 32], vec![])?),
                ("text_embeddings".into(), embedding),
            ],
            2,
        )?;
        let mut current = floats(vec![1, 1, 32], vec![f32::NAN; 32])?;
        let empty = floats(vec![1, 0, 1024], vec![])?;
        let mut eos = None;
        let tail = if text.split_whitespace().count() <= 4 {
            5
        } else {
            3
        };
        let mut latents = vec![];
        for step in 0..limit {
            let outputs = forward(
                &mut self.main,
                &self.flow_specs,
                &mut state,
                vec![
                    ("sequence".into(), current),
                    ("text_embeddings".into(), alias(&empty)?),
                ],
                2,
            )?;
            let (_, logit) = outputs[1]
                .try_extract_tensor::<f32>()
                .context("read Pocket EOS logit")?;
            if logit[0] > -4.0 && eos.is_none() {
                eos = Some(step);
            }
            if eos.is_some_and(|start| step >= start + tail) {
                break;
            }
            let noise: Vec<f32> = (0..32)
                .map(|_| {
                    let z: f32 = StandardNormal.sample(&mut self.rng);
                    z * 0.7f32.sqrt()
                })
                .collect();
            let output=self.flow.run_with_options(ort::inputs!["c"=>&outputs[0],"s"=>Tensor::from_array(([1,1],vec![0.0f32]))?,"t"=>Tensor::from_array(([1,1],vec![1.0f32]))?,"x"=>Tensor::from_array(([1,32],noise.clone()))?], crate::run_options()?).context("run Pocket flow step")?;
            let (_, velocity) = output
                .at(0)?
                .try_extract_tensor::<f32>()
                .context("read Pocket flow velocity")?;
            let latent: Vec<f32> = noise.iter().zip(velocity).map(|(x, v)| x + v).collect();
            latents.extend_from_slice(&latent);
            current = floats(vec![1, 1, 32], latent)?;
        }
        if eos.is_none() {
            eprintln!("Pocket reached reference frame limit without EOS");
        }
        ensure!(!latents.is_empty(), "Pocket produced no latent frames");
        let mut decoder_state = self
            .mimi_specs
            .iter()
            .map(initial)
            .collect::<Result<Vec<_>>>()?;
        let mut audio = vec![];
        for chunk in latents.chunks(12 * 32) {
            let output = forward(
                &mut self.decoder,
                &self.mimi_specs,
                &mut decoder_state,
                vec![(
                    "latent".into(),
                    floats(vec![1, chunk.len() / 32, 32], chunk.to_vec())?,
                )],
                1,
            )?;
            let (_, pcm) = output
                .first()
                .context("missing decoder waveform")?
                .try_extract_tensor::<f32>()
                .context("read Pocket decoded PCM")?;
            audio.extend_from_slice(&pcm[..pcm.len().min(chunk.len() / 32 * 1920)]);
        }
        Ok(audio)
    }
    pub fn synth(&mut self, text: &str, speed: f32) -> Result<Vec<f32>> {
        ensure!(speed == 1.0, "Pocket native 2x speed is NOT SUPPORTED");
        let chunks = self.native_chunks(text)?;
        let mut pcm = vec![];
        for chunk in chunks {
            pcm.extend(self.phrase(&chunk)?);
        }
        Ok(pcm)
    }
}
