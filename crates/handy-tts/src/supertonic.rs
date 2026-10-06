// Phrase-level port of DavidValin/supertonic3-tts helper.rs at
// 146bc2bb960b4f4a2525cce0520de2a5670ffd50. MIT, see SUPERTONIC-CODE-LICENSE.
use crate::graph::Session;
use anyhow::{Context, Result, ensure};
use ndarray::{Array, Array3};
use ort::value::Value;
use rand_distr::{Distribution, StandardNormal};
use serde::Deserialize;
use std::{fs, path::Path};
use unicode_normalization::UnicodeNormalization;

#[derive(Deserialize)]
struct Component {
    data: Vec<Vec<Vec<f32>>>,
    dims: Vec<usize>,
}
#[derive(Deserialize)]
struct Voice {
    style_ttl: Component,
    style_dp: Component,
}
#[derive(Deserialize)]
struct Ae {
    sample_rate: u32,
    base_chunk_size: usize,
}
#[derive(Deserialize)]
struct Ttl {
    chunk_compress_factor: usize,
    latent_dim: usize,
}
#[derive(Deserialize)]
struct Config {
    ae: Ae,
    ttl: Ttl,
}

pub struct Supertonic {
    dp: Session,
    encoder: Session,
    estimator: Session,
    vocoder: Session,
    cfg: Config,
    indexer: Vec<i64>,
    ttl: Array3<f32>,
    style_dp: Array3<f32>,
}
fn style(component: Component) -> Result<Array3<f32>> {
    ensure!(
        component.dims.len() == 3,
        "voice style must have three dimensions"
    );
    Ok(Array3::from_shape_vec(
        (component.dims[0], component.dims[1], component.dims[2]),
        component.data.into_iter().flatten().flatten().collect(),
    )?)
}
impl Supertonic {
    pub fn load(root: &Path, threads: usize, voice: &str) -> Result<Self> {
        let dir = root.join("onnx");
        let cfg: Config = serde_json::from_slice(
            &fs::read(dir.join("tts.json")).context("read Supertonic config")?,
        )
        .context("parse Supertonic config")?;
        let indexer = serde_json::from_slice(
            &fs::read(dir.join("unicode_indexer.json")).context("read unicode indexer")?,
        )
        .context("parse unicode indexer")?;
        let mut engine = Self {
            dp: crate::session(&dir.join("duration_predictor.onnx"), threads)?,
            encoder: crate::session(&dir.join("text_encoder.onnx"), threads)?,
            estimator: crate::session(&dir.join("vector_estimator.onnx"), threads)?,
            vocoder: crate::session(&dir.join("vocoder.onnx"), threads)?,
            cfg,
            indexer,
            ttl: Array3::zeros((0, 0, 0)),
            style_dp: Array3::zeros((0, 0, 0)),
        };
        engine.voice(root, voice)?;
        Ok(engine)
    }
    pub fn voice(&mut self, root: &Path, name: &str) -> Result<()> {
        let data: Voice = serde_json::from_slice(
            &fs::read(root.join(format!("voice_styles/{name}.json")))
                .context("read voice style")?,
        )
        .context("parse voice style")?;
        self.ttl = style(data.style_ttl)?;
        self.style_dp = style(data.style_dp)?;
        Ok(())
    }
    pub fn rate(&self) -> u32 {
        self.cfg.ae.sample_rate
    }
    pub fn synth(&mut self, text: &str, speed: f32) -> Result<Vec<f32>> {
        // Model-specific character normalization from the pinned inference path.
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
        let tagged = format!("<en>{text}</en>");
        let ids: Vec<i64> = tagged
            .chars()
            .map(|c| self.indexer.get(c as usize).copied().unwrap_or(-1))
            .collect();
        let n = ids.len();
        let ids = Value::from_array(Array::from_shape_vec((1, n), ids)?)?;
        let text_mask = Value::from_array(Array3::<f32>::ones((1, 1, n)))?;
        let dp_style = Value::from_array(self.style_dp.clone())?;
        let ttl_style = Value::from_array(self.ttl.clone())?;
        let output = self.dp.run_with_options(
            ort::inputs! {"text_ids"=>&ids,"style_dp"=>&dp_style,"text_mask"=>&text_mask},
            crate::run_options()?,
        )?;
        let duration = output.get("duration")?.try_extract_tensor::<f32>()?.1[0] / speed;
        ensure!(
            duration.is_finite() && duration > 0.0 && duration < 120.0,
            "invalid predicted duration {duration}"
        );
        drop(output);
        let output = self.encoder.run_with_options(
            ort::inputs! {"text_ids"=>&ids,"style_ttl"=>&ttl_style,"text_mask"=>&text_mask},
            crate::run_options()?,
        )?;
        let (shape, data) = output.get("text_emb")?.try_extract_tensor::<f32>()?;
        let embedding = Value::from_array(Array3::from_shape_vec(
            (shape[0] as usize, shape[1] as usize, shape[2] as usize),
            data.to_vec(),
        )?)?;
        drop(output);
        let samples = (duration * self.rate() as f32) as usize;
        let frames =
            samples.div_ceil(self.cfg.ae.base_chunk_size * self.cfg.ttl.chunk_compress_factor);
        let dims = self.cfg.ttl.latent_dim * self.cfg.ttl.chunk_compress_factor;
        let mut rng = rand::thread_rng();
        let noise: Vec<f32> = (0..dims * frames)
            .map(|_| StandardNormal.sample(&mut rng))
            .collect();
        let mut latent = Value::from_array(Array3::from_shape_vec((1, dims, frames), noise)?)?;
        let mask = Value::from_array(Array3::<f32>::ones((1, 1, frames)))?;
        let steps = 8;
        let total = Value::from_array(Array::from_vec(vec![steps as f32]))?;
        for step in 0..steps {
            let current = Value::from_array(Array::from_vec(vec![step as f32]))?;
            let output=self.estimator.run_with_options(ort::inputs!{
                "noisy_latent"=>&latent,"text_emb"=>&embedding,"style_ttl"=>&ttl_style,
                "latent_mask"=>&mask,"text_mask"=>&text_mask,"current_step"=>&current,"total_step"=>&total
            }, crate::run_options()?)?;
            let (shape, data) = output.get("denoised_latent")?.try_extract_tensor::<f32>()?;
            latent = Value::from_array(Array3::from_shape_vec(
                (shape[0] as usize, shape[1] as usize, shape[2] as usize),
                data.to_vec(),
            )?)?;
        }
        let output = self
            .vocoder
            .run_with_options(ort::inputs! {"latent"=>&latent}, crate::run_options()?)?;
        let mut pcm = output
            .get("wav_tts")?
            .try_extract_tensor::<f32>()?
            .1
            .to_vec();
        pcm.truncate(samples);
        Ok(pcm)
    }
}
