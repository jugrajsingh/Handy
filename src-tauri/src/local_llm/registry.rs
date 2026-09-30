//! Built-in list of local text models. v1 ships one entry (S1-mini Q4_K_M).

use std::path::{Path, PathBuf};

/// Which runtime loads a model. v1 has one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    LlamaServer,
}

/// How the chat messages for a model are built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum PromptStyle {
    /// S1-mini: fixed card system prompt, control line + transcript as the user turn.
    S1ControlLine,
}

/// One downloadable model file, pinned by revision, size and SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub attribution: &'static str,
    pub card_url: &'static str,
    pub backend: BackendKind,
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size_bytes: u64,
    pub prompt_style: PromptStyle,
    pub default_ctx: u32,
}

/// The registry. Hugging Face URLs are pinned to a commit, never `main`.
pub const MODELS: &[ModelEntry] = &[ModelEntry {
    id: "s1-mini-q4km",
    display_name: "S1-mini by Superwhisper",
    attribution: "S1-mini by Superwhisper",
    card_url: "https://huggingface.co/superwhisper/s1-mini-GGUF",
    backend: BackendKind::LlamaServer,
    file_name: "s1-mini-q4_k_m.gguf",
    url: "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/34add00a48a2e5d24e5a4ee5405a99620a3a240c/s1-mini-q4_k_m.gguf",
    sha256: "3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634",
    size_bytes: 484_219_808,
    prompt_style: PromptStyle::S1ControlLine,
    default_ctx: 4096,
}];

/// Looks up a registry entry by id.
pub fn find(id: &str) -> Option<&'static ModelEntry> {
    MODELS.iter().find(|m| m.id == id)
}

/// `<models_root>/<id>/<file_name>`, where `models_root` is `<app_data>/models/llm`.
pub fn model_path(models_root: &Path, entry: &ModelEntry) -> PathBuf {
    models_root.join(entry.id).join(entry.file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_ids_are_unique() {
        let ids: HashSet<&str> = MODELS.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), MODELS.len());
    }

    #[test]
    fn registry_entries_are_pinned_and_sized() {
        for m in MODELS {
            assert_eq!(m.sha256.len(), 64, "{}: sha256 must be 64 hex chars", m.id);
            assert!(
                m.sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{}: sha256 must be lowercase hex",
                m.id
            );
            let revision = m
                .url
                .split("/resolve/")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default();
            assert_eq!(revision.len(), 40, "{}: URL must pin a 40-hex commit", m.id);
            assert!(revision.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(
                m.url.ends_with(m.file_name),
                "{}: URL must end with file_name",
                m.id
            );
            assert!(m.size_bytes > 0, "{}: size must be non-zero", m.id);
            assert!(m.default_ctx >= 2048, "{}: context too small", m.id);
            assert!(!m.attribution.is_empty() && m.card_url.starts_with("https://"));
        }
    }

    #[test]
    fn s1_mini_entry_matches_the_verified_file() {
        let m = find("s1-mini-q4km").expect("s1-mini entry");
        assert_eq!(m.size_bytes, 484_219_808);
        assert_eq!(m.prompt_style, PromptStyle::S1ControlLine);
        assert_eq!(m.backend, BackendKind::LlamaServer);
        assert_eq!(
            model_path(Path::new("/data/models/llm"), m),
            PathBuf::from("/data/models/llm/s1-mini-q4km/s1-mini-q4_k_m.gguf")
        );
    }

    #[test]
    fn unknown_id_is_not_found() {
        assert!(find("nope").is_none());
    }
}
