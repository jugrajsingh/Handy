use crate::graph::Session;
use anyhow::{Context, Result, ensure};
use misaki_rs::{G2P, Language};
use ndarray::Array;
use ort::value::Value;
use serde_json::Value as Json;
use std::{collections::HashMap, fs, path::Path};

pub struct Kokoro {
    pub(crate) cancellation: crate::cancellation::Cancellation,
    session: Session,
    g2p: G2P,
    vocab: HashMap<char, i64>,
    voice: Vec<f32>,
}
impl Kokoro {
    pub fn load(
        root: &Path,
        threads: usize,
        voice: &str,
        cancellation: crate::cancellation::Cancellation,
    ) -> Result<Self> {
        let tokenizer: Json = serde_json::from_slice(
            &fs::read(root.join("tokenizer.json")).context("read Kokoro tokenizer")?,
        )
        .context("parse Kokoro tokenizer")?;
        let mut vocab = HashMap::new();
        for (symbol, id) in tokenizer["model"]["vocab"]
            .as_object()
            .context("Kokoro vocab must be object")?
        {
            if let Some(c) = symbol.chars().next() {
                vocab.insert(c, id.as_i64().context("vocab ID must be integer")?);
            }
        }
        let mut engine = Self {
            session: crate::session_for_lane(
                &root.join("onnx/model_quantized.onnx"),
                threads,
                &cancellation,
            )?,
            g2p: G2P::new(Language::EnglishUS),
            vocab,
            voice: vec![],
            cancellation,
        };
        engine.voice(root, voice)?;
        Ok(engine)
    }
    pub fn voice(&mut self, root: &Path, name: &str) -> Result<()> {
        let bytes = crate::runtime::read_voice_file(&root.join(format!("voices/{name}.bin")))
            .context("read Kokoro voice")?;
        ensure!(
            bytes.len() % 1024 == 0,
            "voice pack must contain rows of 256 float32 values"
        );
        self.voice = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]]))
            .collect();
        self.g2p = G2P::new(if name.starts_with('b') {
            Language::EnglishGB
        } else {
            Language::EnglishUS
        });
        Ok(())
    }
    pub fn synth(&mut self, text: &str, speed: f32) -> Result<Vec<f32>> {
        let (phones, _) = self
            .g2p
            .g2p(text)
            .context("permissive Misaki English phonemization")?;
        let mut ids = Vec::new();
        let mut unsupported = String::new();
        for ch in phones.trim().chars() {
            if let Some(&id) = self.vocab.get(&ch) {
                ids.push(id);
            } else {
                unsupported.push(ch);
            }
        }
        if !unsupported.is_empty() {
            eprintln!(
                "Kokoro skipped {} unsupported phonemes",
                unsupported.chars().count()
            );
        }
        ensure!(
            !ids.is_empty(),
            "Kokoro phonemizer produced no supported phonemes"
        );
        let space = self
            .vocab
            .get(&' ')
            .copied()
            .context("missing phoneme space ID")?;
        let mut pcm = Vec::new();
        let mut start = 0;
        while start < ids.len() {
            let limit = (start + 510).min(ids.len());
            let end = if limit < ids.len() {
                ids[start..limit]
                    .iter()
                    .rposition(|&id| id == space)
                    .filter(|&offset| offset > 0)
                    .map_or(limit, |offset| start + offset + 1)
            } else {
                limit
            };
            pcm.extend(self.synth_tokens(&ids[start..end], speed)?);
            start = end;
        }
        if ids.len() > 510 {
            eprintln!(
                "Kokoro synthesized {} phonemes using bounded native calls",
                ids.len()
            );
        }
        Ok(pcm)
    }
    fn synth_tokens(&mut self, phones: &[i64], speed: f32) -> Result<Vec<f32>> {
        let count = phones.len();
        ensure!(
            count > 0 && count <= 510,
            "phoneme count {count} outside Kokoro limit"
        );
        let mut ids = Vec::with_capacity(count + 2);
        ids.push(0);
        ids.extend_from_slice(phones);
        ids.push(0);
        let style_index = (count - 1) * 256;
        let style = self
            .voice
            .get(style_index..style_index + 256)
            .context("missing voice style row")?
            .to_vec();
        let n = ids.len();
        let ids = Value::from_array(Array::from_shape_vec((1, n), ids)?)?;
        let style = Value::from_array(Array::from_shape_vec((1, 256), style)?)?;
        let speed = Value::from_array(Array::from_vec(vec![speed]))?;
        let output = self
            .session
            .run_with_options(
                ort::inputs! {"input_ids"=>&ids,"style"=>&style,"speed"=>&speed},
                &self.cancellation.options,
            )
            .context("Kokoro inference")?;
        Ok(output
            .get("waveform")?
            .try_extract_tensor::<f32>()?
            .1
            .to_vec())
    }
}
