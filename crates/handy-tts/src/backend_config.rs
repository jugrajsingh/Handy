use crate::graph::BackendConfig;
use crate::padding::ShapeClass;
use anyhow::{Context, Result};
use handy_tts_core::device::Device;
use std::{collections::BTreeMap, path::Path};

pub fn config(model_id: &str, root: &Path, device: Device, cache: &Path) -> Result<BackendConfig> {
    let mut placement = BTreeMap::new();
    let mut shapes = BTreeMap::new();
    if model_id == "pocket-en" {
        for graph in [
            "text_conditioner",
            "flow_lm_main_int8",
            "flow_lm_flow_int8",
            "mimi_decoder_int8",
        ] {
            let target = if graph == "flow_lm_main_int8"
                || device == Device::Gpu
                    && matches!(graph, "mimi_decoder_int8" | "flow_lm_flow_int8")
            {
                Device::Cpu
            } else {
                device
            };
            placement.insert(graph.into(), target);
        }
        if device == Device::Npu {
            shapes.insert(
                "text_conditioner".into(),
                BTreeMap::from([("token_ids".into(), vec![1, 64])]),
            );
            shapes.insert(
                "flow_lm_flow_int8".into(),
                BTreeMap::from([
                    ("c".into(), vec![1, 1024]),
                    ("s".into(), vec![1, 1]),
                    ("t".into(), vec![1, 1]),
                    ("x".into(), vec![1, 32]),
                ]),
            );
            let bundle: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join("onnx/english_2026-04/bundle.json"))
                    .context("read pinned Pocket static shapes")?,
            )
            .context("decode Pocket static shapes")?;
            let mut decoder = BTreeMap::from([("latent".into(), vec![1, 12, 32])]);
            for state in bundle["mimi_state_manifest"]
                .as_array()
                .context("Pocket decoder state manifest missing")?
            {
                let name = state["input_name"]
                    .as_str()
                    .context("Pocket state input name missing")?;
                let dimensions = state["shape"]
                    .as_array()
                    .context("Pocket state shape missing")?
                    .iter()
                    .map(|d| d.as_i64().context("Pocket state dimension invalid"))
                    .collect::<Result<Vec<_>>>()?;
                decoder.insert(name.into(), dimensions);
            }
            shapes.insert("mimi_decoder_int8".into(), decoder);
        }
    } else if model_id == "supertonic-3" {
        for graph in [
            "duration_predictor",
            "text_encoder",
            "vector_estimator",
            "vocoder",
        ] {
            placement.insert(graph.into(), device);
        }
        if device == Device::Npu {
            shapes = supertonic_shapes(ShapeClass {
                text: 512,
                frames: 512,
            });
        }
    } else if model_id == "kokoro-82m-int8" {
        placement.insert("model_quantized".into(), device);
        if device == Device::Npu {
            shapes.insert(
                "model_quantized".into(),
                BTreeMap::from([
                    ("input_ids".into(), vec![1, 512]),
                    ("style".into(), vec![1, 256]),
                    ("speed".into(), vec![1]),
                ]),
            );
        }
    } else {
        anyhow::bail!("unknown model id");
    }
    Ok(BackendConfig {
        model_id: model_id.into(),
        device,
        cache_dir: cache.into(),
        placement,
        shapes,
        classes: if model_id == "supertonic-3" && device == Device::Gpu {
            [
                (32, 48),
                (80, 144),
                (176, 208),
                (224, 288),
                (272, 336),
                (304, 416),
            ]
            .into_iter()
            .map(|(text, frames)| ShapeClass { text, frames })
            .collect()
        } else {
            vec![]
        },
    })
}
/// Input shapes for all four Supertonic graphs at a static capacity.
pub fn supertonic_shapes(class: ShapeClass) -> BTreeMap<String, BTreeMap<String, Vec<i64>>> {
    let mut shapes = BTreeMap::new();
    let text = [
        ("text_ids".into(), vec![1, class.text]),
        ("text_mask".into(), vec![1, 1, class.text]),
    ];
    let mut dp = BTreeMap::from(text.clone());
    dp.insert("style_dp".into(), vec![1, 8, 16]);
    let mut encoder = BTreeMap::from(text);
    encoder.insert("style_ttl".into(), vec![1, 50, 256]);
    let estimator = BTreeMap::from([
        ("noisy_latent".into(), vec![1, 144, class.frames]),
        ("text_emb".into(), vec![1, 256, class.text]),
        ("style_ttl".into(), vec![1, 50, 256]),
        ("latent_mask".into(), vec![1, 1, class.frames]),
        ("text_mask".into(), vec![1, 1, class.text]),
        ("current_step".into(), vec![1]),
        ("total_step".into(), vec![1]),
    ]);
    shapes.insert("duration_predictor".into(), dp);
    shapes.insert("text_encoder".into(), encoder);
    shapes.insert("vector_estimator".into(), estimator);
    shapes.insert(
        "vocoder".into(),
        BTreeMap::from([("latent".into(), vec![1, 144, class.frames])]),
    );
    shapes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measured_placements_and_supertonic_static_shapes_are_explicit() {
        let supertonic = config(
            "supertonic-3",
            Path::new("."),
            Device::Npu,
            Path::new("cache"),
        )
        .unwrap();
        assert_eq!(
            supertonic.shapes["vector_estimator"]["noisy_latent"],
            [1, 144, 512]
        );
        assert_eq!(supertonic.placement.len(), 4);
        assert!(
            supertonic
                .placement
                .values()
                .all(|device| *device == Device::Npu)
        );
        let pocket = config("pocket-en", Path::new("."), Device::Gpu, Path::new("cache")).unwrap();
        assert_eq!(pocket.placement["flow_lm_main_int8"], Device::Cpu);
        assert_eq!(pocket.placement["mimi_decoder_int8"], Device::Cpu);
        assert_eq!(
            pocket.placement["flow_lm_flow_int8"],
            Device::Cpu,
            "the int8 flow sampler clips speech on the Intel GPU"
        );
        assert_eq!(pocket.placement["text_conditioner"], Device::Gpu);
        let kokoro = config(
            "kokoro-82m-int8",
            Path::new("."),
            Device::Gpu,
            Path::new("cache"),
        )
        .unwrap();
        assert_eq!(kokoro.placement["model_quantized"], Device::Gpu);
    }

    mod gpu_classes {
        use super::*;
        #[test]
        fn smallest_fitting_class_is_chosen() {
            let cfg = config(
                "supertonic-3",
                Path::new("."),
                Device::Gpu,
                Path::new("cache"),
            )
            .unwrap();
            assert_eq!(cfg.classes.len(), 6);
            let first = supertonic_shapes(cfg.classes[0]);
            assert_eq!(first["vector_estimator"]["text_emb"], [1, 256, 32]);
            assert_eq!(first["vector_estimator"]["noisy_latent"], [1, 144, 48]);
            assert_eq!(first["vector_estimator"]["text_mask"], [1, 1, 32]);
            assert_eq!(first["vector_estimator"]["latent_mask"], [1, 1, 48]);
            for (index, class) in cfg.classes.iter().enumerate() {
                assert_eq!(
                    crate::padding::class_index(&cfg.classes, class.text, class.frames).unwrap(),
                    index
                );
            }
            assert_eq!(
                crate::padding::class_index(&cfg.classes, 70, 150).unwrap(),
                2
            );
            assert_eq!(
                crate::padding::class_index(&cfg.classes, 180, 100).unwrap(),
                3
            );
            assert!(
                crate::padding::class_index(&cfg.classes, 305, 1)
                    .unwrap_err()
                    .is::<crate::padding::StaticCapacity>()
            );
            assert!(
                crate::padding::class_index(&cfg.classes, 1, 417)
                    .unwrap_err()
                    .is::<crate::padding::StaticCapacity>()
            );
            for device in [Device::Cpu, Device::Npu] {
                assert!(
                    config("supertonic-3", Path::new("."), device, Path::new("cache"))
                        .unwrap()
                        .classes
                        .is_empty()
                );
            }
        }
    }
}
