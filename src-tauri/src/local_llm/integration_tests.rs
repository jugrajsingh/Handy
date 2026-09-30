//! Real llama-server + S1-mini end to end. Ignored by default; run with
//! `HANDY_LOCAL_LLM_IT=1 HANDY_LOCAL_LLM_IT_MODEL=<gguf> HANDY_LLAMA_SERVER=<binary>`
//! and `cargo test --lib local_llm::integration_tests -- --ignored`.

#![cfg(target_os = "linux")]

use super::llama_server::LlamaServerBackend;
use super::manager::{IdleInputs, LocalLlmManager, ManagerConfig, ManagerHooks};
use super::registry;
use crate::settings::{get_default_settings, ModelUnloadTimeout, LOCAL_LLM_PROVIDER_ID};
use std::path::PathBuf;
use std::time::Duration;

#[derive(serde::Deserialize)]
struct Golden {
    input: String,
    output: String,
}

/// Pids of live `llama-server` processes whose parent is this test process.
fn llama_children() -> Vec<u32> {
    let me = std::process::id().to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            let Some((head, tail)) = stat.rsplit_once(") ") else {
                return false;
            };
            let fields: Vec<&str> = tail.split_whitespace().collect();
            head.ends_with("(llama-server") && fields.get(1) == Some(&me.as_str())
        })
        .collect()
}

fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

#[test]
#[ignore = "needs llama-server and the S1-mini GGUF; set HANDY_LOCAL_LLM_IT=1 and pass --ignored"]
fn real_server_matches_goldens_and_is_gone_after_unload() {
    assert_eq!(
        std::env::var("HANDY_LOCAL_LLM_IT").as_deref(),
        Ok("1"),
        "set HANDY_LOCAL_LLM_IT=1"
    );
    let model = PathBuf::from(
        std::env::var("HANDY_LOCAL_LLM_IT_MODEL")
            .expect("HANDY_LOCAL_LLM_IT_MODEL must point at s1-mini-q4_k_m.gguf"),
    );
    let entry = registry::find("s1-mini-q4km").expect("registry entry");
    let root = tempfile::tempdir().unwrap();
    let dest = registry::model_path(root.path(), entry);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&model, &dest).unwrap();

    let manager = LocalLlmManager::new(
        Box::new(LlamaServerBackend::new()),
        Some(root.path().to_path_buf()),
        ManagerHooks {
            idle_inputs: Box::new(|| IdleInputs {
                timeout: ModelUnloadTimeout::Never,
                recording: false,
            }),
            on_status: Box::new(|_| {}),
        },
        ManagerConfig {
            start_timeout: Duration::from_secs(30),
            idle_tick: Duration::from_secs(3600),
        },
    );
    let mut settings = get_default_settings();
    settings.post_process_provider_id = LOCAL_LLM_PROVIDER_ID.to_string();
    settings.local_llm_model_id = Some(entry.id.to_string());
    settings.model_unload_timeout = ModelUnloadTimeout::Never;

    let goldens: Vec<Golden> =
        serde_json::from_str(include_str!("testdata/golden.json")).expect("golden.json");
    assert_eq!(goldens.len(), 3);
    for g in &goldens {
        let out = manager
            .process(&g.input, &settings)
            .expect("local post-processing");
        assert_eq!(out, g.output, "input: {}", g.input);
    }

    let children = llama_children();
    assert_eq!(
        children.len(),
        1,
        "exactly one llama-server child while Ready"
    );
    eprintln!(
        "local-llm IT: llama-server pid {} RSS {:?} KiB",
        children[0],
        rss_kib(children[0])
    );
    manager.unload();
    assert!(
        llama_children().is_empty(),
        "llama-server must be gone after unload()"
    );
}
