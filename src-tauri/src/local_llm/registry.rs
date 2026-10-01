//! Built-in list of local text models pinned by revision, size and SHA-256.

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
    PlainSystemPrompt,
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
},
ModelEntry {
    id: "quill-0.8b-q4km", display_name: "Quill 0.8B",
    attribution: "Quill by Quobi", card_url: "https://huggingface.co/Quobi/Quill",
    backend: BackendKind::LlamaServer, file_name: "quill-0.8b-Q4_K_M.gguf",
    url: "https://huggingface.co/Quobi/Quill/resolve/4cc2cc3c8e7ea9ee69126becd55be23a3a949899/quill-0.8b-Q4_K_M.gguf",
    sha256: "aa54d6f6108d66e4b60a57bdc04ecca6e84e073504918a64b41ac4a0f816f16d", size_bytes: 529296832,
    prompt_style: PromptStyle::PlainSystemPrompt, default_ctx: 4096,
},
ModelEntry {
    id: "quill-2b-q4km", display_name: "Quill 2B",
    attribution: "Quill by Quobi", card_url: "https://huggingface.co/Quobi/Quill",
    backend: BackendKind::LlamaServer, file_name: "quill-2b-Q4_K_M.gguf",
    url: "https://huggingface.co/Quobi/Quill/resolve/4cc2cc3c8e7ea9ee69126becd55be23a3a949899/quill-2b-Q4_K_M.gguf",
    sha256: "b877a22b773d2aac40b3c642c24f1cbbb0b3f1d42cbd3c6eb936533719317196", size_bytes: 1274396096,
    prompt_style: PromptStyle::PlainSystemPrompt, default_ctx: 4096,
},
ModelEntry {
    id: "qwen3-4b-instruct-2507-q4km", display_name: "Qwen3-4B-Instruct-2507",
    attribution: "Qwen by Alibaba, GGUF by Unsloth", card_url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF",
    backend: BackendKind::LlamaServer, file_name: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/a06e946bb6b655725eafa393f4a9745d460374c9/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
    sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597", size_bytes: 2497281120,
    prompt_style: PromptStyle::PlainSystemPrompt, default_ctx: 4096,
},];

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

    #[test]
    fn ui_v21_entries_match_verified_metadata() {
        let expected = [
            (
                "quill-0.8b-q4km",
                "quill-0.8b-Q4_K_M.gguf",
                529_296_832_u64,
                "aa54d6f6108d66e4b60a57bdc04ecca6e84e073504918a64b41ac4a0f816f16d",
            ),
            (
                "quill-2b-q4km",
                "quill-2b-Q4_K_M.gguf",
                1_274_396_096,
                "b877a22b773d2aac40b3c642c24f1cbbb0b3f1d42cbd3c6eb936533719317196",
            ),
            (
                "qwen3-4b-instruct-2507-q4km",
                "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
                2_497_281_120,
                "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
            ),
        ];
        assert_eq!(MODELS.len(), 4);
        for (id, file, size, hash) in expected {
            let entry = find(id).unwrap();
            assert_eq!(entry.file_name, file);
            assert_eq!(entry.size_bytes, size);
            assert_eq!(entry.sha256, hash);
            let (repo, revision) = if id == "qwen3-4b-instruct-2507-q4km" {
                (
                    "unsloth/Qwen3-4B-Instruct-2507-GGUF",
                    "a06e946bb6b655725eafa393f4a9745d460374c9",
                )
            } else {
                ("Quobi/Quill", "4cc2cc3c8e7ea9ee69126becd55be23a3a949899")
            };
            assert_eq!(
                entry.url,
                format!("https://huggingface.co/{repo}/resolve/{revision}/{file}")
            );
            assert_eq!(entry.prompt_style, PromptStyle::PlainSystemPrompt);
            assert_eq!(entry.default_ctx, 4096);
        }
    }
}
