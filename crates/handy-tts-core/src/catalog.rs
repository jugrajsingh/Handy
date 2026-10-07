use serde::Deserialize;
use std::path::{Component, Path};
use std::sync::OnceLock;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
/// Describes one immutable model artifact and its integrity requirements.
pub struct ModelFile {
    pub model_id: String,
    pub repo: String,
    pub revision: String,
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub gated: bool,
}
#[derive(Debug, Clone)]
/// Combines model metadata, voice names and pinned download files.
pub struct ModelEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub license: &'static str,
    pub attribution: &'static str,
    pub card: &'static str,
    pub default_voice: &'static str,
    pub threads: usize,
    pub native_speed: crate::speed::NativeWindow,
    pub files: Vec<ModelFile>,
    pub voices: Vec<String>,
    pub bytes: u64,
}
#[derive(Debug, Error, Clone, PartialEq)]
pub enum CatalogError {
    #[error("tts.errors.registry")]
    Invalid,
    #[error("tts.errors.unknownModel")]
    Unknown,
}
static CATALOG: OnceLock<Result<Vec<ModelEntry>, CatalogError>> = OnceLock::new();
/// Accepts relative paths consisting only of normal filesystem components.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}
/// Validates a pinned file manifest and assembles the supported model entries.
pub fn parse_catalog(json: &str) -> Result<Vec<ModelEntry>, CatalogError> {
    #[derive(Deserialize)]
    struct Manifest {
        native_speed: std::collections::BTreeMap<String, crate::speed::NativeWindow>,
        files: Vec<ModelFile>,
    }
    let manifest: Manifest = serde_json::from_str(json).map_err(|_| CatalogError::Invalid)?;
    if manifest.native_speed.len() != 3 {
        return Err(CatalogError::Invalid);
    }
    let files = manifest.files;
    let mut entries = Vec::new();
    for (id, name, license, attribution, card, default_voice, threads) in [
        (
            "pocket-en",
            "Pocket TTS (English)",
            "CC-BY-4.0",
            "tts.pocketAttribution",
            "https://huggingface.co/KevinAHM/pocket-tts-onnx",
            "alba",
            4,
        ),
        (
            "supertonic-3",
            "Supertonic 3",
            "OpenRAIL-M",
            "tts.supertonicRestrictions",
            "https://huggingface.co/Supertone/supertonic-3",
            "F1",
            2,
        ),
        (
            "kokoro-82m-int8",
            "Kokoro-82M (INT8)",
            "Apache-2.0",
            "tts.kokoroAttribution",
            "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX",
            "af_heart",
            4,
        ),
    ] {
        let native_speed = *manifest.native_speed.get(id).ok_or(CatalogError::Invalid)?;
        if !native_speed.valid()
            || (id == "pocket-en"
                && native_speed != (crate::speed::NativeWindow { min: 1.0, max: 1.0 }))
        {
            return Err(CatalogError::Invalid);
        }
        let model_files: Vec<_> = files
            .iter()
            .filter(|file| file.model_id == id)
            .cloned()
            .collect();
        if model_files.is_empty() {
            return Err(CatalogError::Invalid);
        }
        let mut voices = Vec::new();
        let mut paths = std::collections::HashSet::new();
        let mut bytes = 0u64;
        for file in &model_files {
            if !valid_path(&file.path)
                || !valid_path(&file.repo)
                || !paths.insert(&file.path)
                || file.revision.len() != 40
                || !file.revision.bytes().all(|c| c.is_ascii_hexdigit())
                || file.sha256.len() != 64
                || !file.sha256.bytes().all(|c| c.is_ascii_hexdigit())
                || file.bytes == 0
            {
                return Err(CatalogError::Invalid);
            }
            bytes = bytes.checked_add(file.bytes).ok_or(CatalogError::Invalid)?;
            let path = Path::new(&file.path);
            if file.path.contains("embeddings/")
                || file.path.contains("voice_styles/")
                || (file.path.starts_with("voices/") && file.path.ends_with(".bin"))
            {
                voices.push(
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .ok_or(CatalogError::Invalid)?
                        .to_string(),
                );
            }
        }
        voices.sort();
        if !voices.iter().any(|v| v == default_voice) {
            return Err(CatalogError::Invalid);
        }
        entries.push(ModelEntry {
            id,
            name,
            license,
            attribution,
            card,
            default_voice,
            native_speed,
            threads,
            files: model_files,
            voices,
            bytes,
        });
    }
    if files
        .iter()
        .any(|file| !entries.iter().any(|entry| entry.id == file.model_id))
    {
        return Err(CatalogError::Invalid);
    }
    Ok(entries)
}
/// Returns the validated embedded catalog, initialized once per process.
pub fn entries() -> Result<&'static [ModelEntry], CatalogError> {
    CATALOG
        .get_or_init(|| parse_catalog(include_str!("models.json")))
        .as_ref()
        .map(|entries| entries.as_slice())
        .map_err(Clone::clone)
}
/// Looks up a supported model by its stable identifier.
pub fn entry(id: &str) -> Result<&'static ModelEntry, CatalogError> {
    entries()?
        .iter()
        .find(|entry| entry.id == id)
        .ok_or(CatalogError::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pocket_window_other_than_unity_is_rejected() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("models.json")).unwrap();
        json["native_speed"]["pocket-en"] = serde_json::json!({"min": 1.0, "max": 1.1});
        assert_eq!(
            parse_catalog(&json.to_string()).unwrap_err(),
            CatalogError::Invalid
        );
    }
    #[test]
    fn pinned_catalog_has_exact_totals_voices_and_safe_paths() {
        for (id, count, size, voices) in [
            ("pocket-en", 36, 295738958, 27),
            ("supertonic-3", 19, 401301114, 10),
            ("kokoro-82m-int8", 33, 106998117, 28),
        ] {
            let entry = entry(id).unwrap();
            assert_eq!(
                (entry.files.len(), entry.bytes, entry.voices.len()),
                (count, size, voices)
            );
        }
        for path in ["../escape", "/absolute", "a/../b", "a\\b", ""] {
            assert!(!valid_path(path));
        }
        let bad = include_str!("models.json").replacen("README.md", "../README.md", 1);
        assert!(parse_catalog(&bad).is_err());
        assert!(entry("missing").is_err());
    }
}
