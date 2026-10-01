# Changelog

All notable changes on this branch compared with upstream main are documented here, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

In-app local post-processing

- New "Local (in-app)" post-processing provider that runs a small language model on the CPU through a bundled llama.cpp `llama-server`. No external service has to be installed or started, and transcripts never leave the machine.
- Handy starts `llama-server` as a managed child process that listens on loopback only, on a free port, with a random API key per launch passed through the environment instead of the command line. Handy stops it on exit, and on Linux the kernel terminates it if Handy dies without cleaning up.
- The model loads only when needed: it starts in the background when a post-processing recording begins, so recording is never delayed, and it unloads after the existing model unload timeout (a duration, Immediately, or Never).
- Local post-processing never blocks a dictation on failure. A start deadline of 10 seconds applies, a server that exits unexpectedly is started again on the next dictation, and after three failed starts in a row the model enters an error state until a model is selected again or Handy restarts. Every failure falls back to the raw transcript.
- Four downloadable local models, each pinned to a Hugging Face revision with a fixed size and SHA-256: S1-mini by Superwhisper, Quill 0.8B, Quill 2B and Qwen3-4B-Instruct-2507 (all Q4_K_M quantizations). Each model shows its attribution and a link to its model card.
- Models are downloaded and deleted on demand through the verified, resumable downloader. Download progress and errors survive leaving the page, and a stale error is cleared before the next model action.
- S1-mini is driven by Styling (casual, semi-casual, semi-formal, formal), Structure (prose, lists) and Context (general, email) dropdowns.
- Quill and Qwen3 models use the selected Handy prompt as their system prompt, and the Prompts group is shown for them. S1-mini uses its own fixed prompt, so the prompt editor is hidden for it with a note. If the selected prompt is missing or empty, post-processing is skipped and the raw transcript is kept.
- Long transcripts are split at sentence boundaries, processed in order and rejoined. Output that loses more than half of the input words is rejected, and think blocks and invisible characters are stripped from model output.
- A live status label (Not loaded, Loading, Ready, Error with the reason) is shown next to the model controls.
- The RPM bundles a static, CPU-only `llama-server` at `/usr/lib/Handy/llm/llama-server`. The `HANDY_LLAMA_SERVER` environment variable overrides the path for development.
- An opt-in integration test (`HANDY_LOCAL_LLM_IT=1`) runs the real `llama-server` against golden outputs.

Post-processing Models page

- Provider selection is its own group at the top of the page. The group below it is titled "Local model" for the local provider and "API (OpenAI compatible)" for remote providers.
- The local model list shows one row per model with its name, size, downloaded state, an Active badge for the selected model, and Use, Download (with progress) and Delete actions. The model settings sit below the list.

Shortcuts

- With post-processing enabled, General shows a Post-processing Shortcut and a Raw Shortcut together, sharing one activation mode. The post-processing shortcut moved there from the Post-processing Models page.
- Either dictation shortcut can be cleared with a Clear button. A cleared shortcut shows as Unassigned, and clearing is refused when it would leave no dictation shortcut.
- A shortcut already assigned to another action is refused with a message naming that action, checked against all stored bindings.

History

- Post-processed entries show a "Post-processed with <model>" badge. Local models appear by display name, and remote providers by the stored model name.
- A Compare view picker (Diff, Side by side, Stacked) is shown in the History header and remembered across restarts. Diff marks removed and added words, and falls back to Side by side above 4000 words per side.
- Each entry remembers the provider and model that produced its post-processed text. Older entries are unaffected.
- Copy post-processed text and Copy raw text are icon buttons in each entry's action row.
- A Clear History button removes all unstarred entries and their recordings after a confirmation that states how many entries and recordings will be removed. Starred entries are kept.

Tray and footer

- The tray menu shows a Post-processing model submenu with the selected model's status, a checked list of downloaded models, and an Unload post-processing model item that is enabled only while the model is ready. The group appears only when post-processing is enabled with the local provider.
- The footer has a second picker for the post-processing model, next to the transcription model picker. Each picker shows the same icon as its sidebar page. Choosing a model does not load it. With no model downloaded, the picker reads "No post-processing model" and opens the Post-processing Models page.

### Changed

- Sidebar entries are renamed to "Transcription Models" and "Post-processing Models". The sidebar is wider and labels wrap instead of being truncated.
- The tray item "Unload Model" is renamed "Unload transcription model", and the tray model submenu is labelled "Transcription model".
- The transcription model dropdown in the footer is wider, shows each model's size, and lets long names wrap.
- Model buttons in the footer use the available width. A tooltip with the full name appears only when the name is still truncated.
- The post-processing shortcut is no longer listed on the Post-processing Models page.
- When post-processing is disabled, a cleared Raw Shortcut is restored to its default, and an assigned Raw Shortcut stays active.
- Retention cleanup of old history entries now announces each removed entry to the History page.
- Changing the post-processing provider, the local model or the compare view now saves with a checked write that restores the previous value on failure, and the tray refreshes after a provider or model change. Toggling an entry's saved status runs off the UI thread.

### Fixed

- Shortcut changes are applied atomically: a refused or failed change leaves the stored settings and registered shortcuts as they were, and an active dictation shortcut is always preserved.
- The History list keeps its loaded pages and scroll position when a dictation is added or updated, and when retention removes entries, instead of jumping back to the first page. A list that is already at the top stays at the top.
- The tray menu refreshes after post-processing is toggled, after the provider or local model changes, and after a native check click on a model entry.
- Selecting a model from the tray no longer blocks dictation.
- Selecting a different local model saves the selection before the running model is unloaded.
- Invisible characters in front of a think block no longer prevent the block from being removed from model output.
- A selected local model whose file is not downloaded no longer shows its name or a checkmark in the tray.
- The Prompts group is hidden for the local provider until the model list has loaded, so the prompt editor does not flash up for S1-mini.

### Notes for upstream PR

- Build requirement: a static, CPU-only `llama-server` must be staged as `llm-bin/` for the RPM bundle, which installs it at `/usr/lib/Handy/llm` (`bundle.linux.rpm.files` in `tauri.conf.json`). Bundling and process supervision are implemented for Linux only.
- New settings keys, all with serde defaults so existing stores load unchanged: `local_llm_model_id`, `local_llm_styling`, `local_llm_structure`, `local_llm_context`, `local_llm_threads` (default 4) and `history_compare_view` (default `diff`). A new post-processing provider with id `local_llm` is added.
- New events: `local-llm-state-changed` and `local-llm-download-progress`. The `history-updated` payload gains a `cleared` action, and retention cleanup now emits a `deleted` action for each removed entry.
- `HistoryEntry` gains `post_process_provider` and `post_process_model`, backed by two additive nullable columns in a new history migration.
- New Tauri commands: `clear_binding`, `clear_history`, `get_history_clear_summary`, `change_history_compare_view_setting`, and the `local_llm` commands for listing, downloading, deleting, selecting and reporting status of models and for the three S1 settings.
- New dependencies: `thiserror`, and the `blocking` feature of `reqwest`.
- Upstream files that gain hunks: `actions.rs`, `settings.rs`, `lib.rs`, `tray.rs`, `managers/history.rs`, `commands/history.rs`, `managers/model/download.rs`, `shortcut/mod.rs`, `shortcut/handy_keys.rs`, `shortcut/tauri_impl.rs`, `Cargo.toml`, `tauri.conf.json`, `bindings.ts`, `App.tsx`, `Sidebar.tsx`, the footer and model selector components, the shortcut inputs, the General, History and Post-processing settings, the settings store, and the English translation file. New code lives in `src-tauri/src/local_llm/` and `src-tauri/src/shortcut/policy.rs`.
