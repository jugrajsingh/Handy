# Changelog

This project is built on [Handy](https://github.com/cjpais/Handy) v0.10.0 and documents its own changes on top of it, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Changes merged in from upstream are documented in upstream's own release notes.

## [Unreleased]

### Added

Read aloud (text to speech)

- A Read aloud shortcut (default Super+Alt+R, changeable under General) reads the text you last copied. It takes the clipboard with `wl-paste` on Wayland and `xclip` on X11. A popup opens and starts reading at once. Pressing the shortcut again while the popup is open stops reading and closes it.
- The popup shows the text with the sentence being read highlighted, and the highlight follows the audio. Click the text, or paste into the popup, to edit it. It shows elapsed and total time with a progress bar, and Play, Pause and Stop buttons. Esc closes the popup and Ctrl+Enter plays or pauses.
- Errors appear in a banner in the popup. When the chosen model is not downloaded, the banner links to the Text-to-speech Models page. If the chosen output device cannot be opened, reading falls back to the default output device and the popup says so. If a dictation is still pasting when you press the shortcut, a notice asks you to try again, and if the voice worker is not ready a notice says reading is unavailable.
- Three downloadable voice models: Pocket TTS (English, the default; about 296 MB, 27 voices), Supertonic 3 (about 401 MB, 10 voices) and Kokoro-82M INT8 (about 107 MB, 28 voices). Each file is pinned to a Hugging Face revision with a fixed size and SHA-256, and downloads are verified and resumable.
- Pocket voices are gated on Hugging Face. Accept the voice terms there, then enter a Hugging Face token in the model card or under Advanced. The token is kept in memory only and is cleared when the app restarts. The `HF_TOKEN` environment variable is also honoured.
- Each model runs on the CPU through ONNX Runtime, or on an Intel GPU or NPU through OpenVINO. The Run on control on each downloaded model's card offers Auto, CPU, GPU and NPU, listing only devices that model supports on this machine. Auto picks the best available device and falls back when one fails to prepare. The cards show Quality and Speed ratings, a Recommended badge, and a progress line while a device prepares. The ratings ship with the app and were measured on one machine (see Platform support).
- Speed from 0.5x to 2.0x and pitch from -6 to +6 semitones, applied by time stretching with a single pass over the audio. A speed or pitch change applies from the next sentence. Pause lengths after punctuation scale with the speed.
- Reading is streamed. Several sentences are synthesized in parallel (up to three, fewer when memory is short) while playback streams ahead with a bounded read-ahead. When a model or device synthesizes slower than playback, reading pauses and the popup shows Buffering until enough audio is ready. Silence at the edges of each sentence is trimmed and replaced with a pause that suits the punctuation. Supertonic on the GPU uses six fixed input sizes that are warmed up when the model loads, and on the NPU every model uses fixed input shapes.
- Text is prepared as Markdown, so formatting marks are not spoken, and fenced code blocks are replaced by the words "code block skipped" unless Read code blocks is switched on. Text with very long unbroken runs is still read to the end.
- Technical reading is on by default and speaks paths, file names, extensions, flags, versions and code names the way an engineer would say them. Slashes, underscores, dots and dashes are named, so `tests/lib.sh` is read as "tests slash lib dot S H" and `--dry-run` as "dash dash dry dash run". A leading `~` is read as "home". Names in camelCase or snake_case are split into words. A known extension is read as its letters or name (".md" as "markdown", ".yaml" as "Y A M L"), a `:line` or `:line:column` suffix is read as "at line 684 column 7", each dot of a version is spoken (`v0.10.0` as "version 0 dot 10 dot 0"), and the `src` folder is read as "source". Ordinary prose, decimals and numbers are left as they are, and so are abbreviations such as "e.g.". Web links are read as "link to <site>".
- Three toggles on the Text-to-speech page control it: Technical reading (on by default), Read full paths (on by default; off reads only the file name of a path) and Read full links (off by default; on reads the site and its path instead of only the site). Read full paths and Read full links apply only while Technical reading is on. Changing any of them while reading applies from the current position without repeating text that was already spoken.
- Pronunciations come from a built-in vocabulary that you can extend. The file `tts-vocabulary.json` in the app data directory is created on first use with no entries and is read again at the start of every read, so an edit applies to the next read without a restart. It can add or override symbols, extensions, terms, acronyms and checked examples. A term can match a whole word, any part of one, a prefix, a suffix or a regular expression. An optional `models` section holds entries for one voice model id that apply only while that model is selected. Spoken replacements may hold only letters, digits and spaces.
- A vocabulary file that cannot be used never fails or delays a read. Reading falls back to the built-in vocabulary, and a "Vocabulary File Has an Error" warning (notice code `VocabularyInvalid`) names the file and the error, with line and column when known. A file that is not valid JSON or has an unsupported version, an entry that is not valid, text that is not UTF-8, a file over 1 MiB and anything that is not a regular file are all handled this way. The warning is shown once for each distinct error and is cleared by the next successful load.
- `handy-tts --check-vocabulary [file]` runs every example in the built-in vocabulary, or in the built-in vocabulary merged with your file. It prints PASS or FAIL for each example and exits with a failure status if any example fails, so a vocabulary edit can be checked before it is used.
- Dictation and reading never overlap. Starting a dictation stops reading and closes the popup. Pressing Read aloud during a dictation ends the recording, lets the dictation finish, then opens the popup.
- The voice engine runs as a separate worker process that is started when needed and unloaded after the existing model unload timeout. It refuses to start with less than 1 GiB of available memory. After three failed starts in a row it stays in a Failed state until a model is selected again.
- A Text-to-speech settings page (model, voice with a Preview voice button, speed, pitch, output device, Read code blocks, Technical reading, Read full paths and Read full links) and a Text-to-speech Models page, with the same card design as Transcription Models.
- The tray menu has a Text-to-speech model submenu with a checked list of downloaded models, the worker's status (Unloaded, Starting, Loaded, Stopping or Failed) and an Unload text-to-speech model item that is enabled only while the worker is loaded.
- The RPM bundles the worker and the OpenVINO libraries at `/usr/lib/Handy/tts`, and recommends `intel-compute-runtime`, `intel-level-zero`, `oneapi-level-zero` and `wl-clipboard`. The `HANDY_TTS_WORKER` environment variable overrides the worker path for development.

In-app local post-processing

- New "Local (in-app)" post-processing provider that runs a small language model on the CPU through a bundled llama.cpp `llama-server`. No external service has to be installed or started, and transcripts never leave the machine.
- This build starts `llama-server` as a managed child process. It listens on a private Unix socket inside a directory that only the current user can open (mode 0700) under `XDG_RUNTIME_DIR`, or under the app data directory when that is not set, and it opens no network port. A random API key is generated for each launch and passed through the environment instead of the command line. The app stops the server on exit, and on Linux the kernel terminates it if the app dies without cleaning up.
- The model loads only when needed: it starts in the background when a post-processing recording begins, so recording is never delayed, and it unloads after the existing model unload timeout (a duration, Immediately, or Never).
- Before a model is loaded, its file is checked against its pinned size and SHA-256. A corrupt file is reported with advice to delete and re-download it, and a check that runs past the start deadline falls back to the raw transcript.
- Local post-processing never blocks a dictation on failure. A start deadline of 10 seconds applies, a server that exits unexpectedly is started again on the next dictation, and after three failed starts in a row the model enters an error state. When the last of them was a start timeout, for example under heavy machine load, the first dictation after 5 minutes tries one more start and recovers on its own; after any other failure the error state lasts till a model is selected or deleted, or the app restarts. Every failure falls back to the raw transcript.
- Five downloadable local models, each pinned to a Hugging Face revision with a fixed size and SHA-256: S1-mini by Superwhisper, Quill 0.8B, Quill 2B, Qwen3-4B-Instruct-2507 and Muesli 2B (all Q4_K_M quantizations). Each model shows its attribution and a link to its model card.
- Models are downloaded and deleted on demand through the verified, resumable downloader. Download progress and errors survive leaving the page, and a stale error is cleared before the next model action.
- S1-mini is driven by Styling (casual, semi-casual, semi-formal, formal), Structure (prose, lists) and Context (general, email) dropdowns.
- Quill, Qwen3 and Muesli models use the selected prompt as their system prompt, and the Prompts group is shown for them. S1-mini uses its own fixed prompt, so the prompt editor is hidden for it with a note. If the selected prompt is missing or empty, post-processing is skipped and the raw transcript is kept.
- Long transcripts are split at sentence boundaries, processed in order and rejoined. The token budget for each chunk and for the reply is estimated per script, so Gurmukhi, Devanagari and other Indic text gets a larger reply budget. A model that still reaches the limit falls back to the raw transcript. Output that loses more than half of the input words is rejected, and think blocks and invisible characters are stripped from model output.
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

Model licences

- Each post-processing model shows its licence with a link to the licence text at the pinned model revision. S1-mini is Apache-2.0 with one added term: it must be named "S1-mini" by "Superwhisper" with that capitalization, and that term is shown under the licence link.
- Each text-to-speech model card shows its licence (CC-BY-4.0 for Pocket TTS, OpenRAIL-M for Supertonic 3, Apache-2.0 for Kokoro) with a link to the model card. Hovering the licence shows the attribution or use restrictions that apply.

Translations

- All new post-processing and read aloud strings are translated into the 25 other languages. The translations are machine-generated and have not yet been reviewed by native speakers.

### Changed

- Sidebar entries are renamed to "Transcription Models" and "Post-processing Models". The sidebar is wider and labels wrap instead of being truncated.
- The new sidebar entries Text-to-speech and Text-to-speech Models have their own icons, and a test keeps every sidebar icon unique.
- The Cargo project is now a workspace with two new crates, `handy-tts` (the voice worker) and `handy-tts-core` (shared text, scheduling, audio and download logic).
- The tray item "Unload Model" is renamed "Unload transcription model", and the tray model submenu is labelled "Transcription model".
- The transcription model dropdown in the footer is wider, shows each model's size, and lets long names wrap.
- Model buttons in the footer use the available width. A tooltip with the full name appears only when the name is still truncated.
- The post-processing shortcut is no longer listed on the Post-processing Models page.
- When post-processing is disabled, a cleared Raw Shortcut is restored to its default, and an assigned Raw Shortcut stays active.
- Retention cleanup of old history entries now announces each removed entry to the History page.
- Changing the post-processing provider, the local model or the compare view now saves with a checked write that restores the previous value on failure, and the tray refreshes after a provider or model change. Toggling an entry's saved status runs off the UI thread.
- Each helper program used to paste dictated text now runs under a deadline that grows with the length of the text, and its leftovers are cleaned up whatever the outcome, so a hung helper cannot hold up dictation, shortcuts or reading.
- One built-in pronunciation entry was removed from the technical reading vocabulary because splitting names in camelCase already reads that word correctly, and the bundled vocabulary examples now use generic paths.

### Fixed

- Shortcut changes are applied atomically: a refused or failed change leaves the stored settings and registered shortcuts as they were, and an active dictation shortcut is always preserved.
- The History list keeps its loaded pages and scroll position when a dictation is added or updated, and when retention removes entries, instead of jumping back to the first page. A list that is already at the top stays at the top.
- Clear History also cancels any transcription still running for the entries it removes.
- The tray menu refreshes after post-processing is toggled, after the provider or local model changes, and after a native check click on a model entry.
- Selecting a model from the tray no longer blocks dictation.
- Selecting a different local model saves the selection before the running model is unloaded.
- Invisible characters in front of a think block no longer prevent the block from being removed from model output.
- A selected local model whose file is not downloaded no longer shows its name or a checkmark in the tray.
- The Prompts group is hidden for the local provider until the model list has loaded, so the prompt editor does not flash up for S1-mini.
- Supertonic on the Intel GPU no longer adds static (hiss above 12 kHz) to the audio. Its GPU vocoder is kept at full precision and its OpenVINO cache folder was renewed, so the first read on the GPU after updating prepares the device again.
- Pocket TTS on the Intel GPU no longer sounds too fast and unintelligible. Its flow sampler now runs on the CPU while the text conditioner stays on the GPU, which costs about 15% more CPU time.
- Starting a new read while the previous one is still being cancelled no longer shows "The text-to-speech worker stopped". A request the worker reports as busy is tried again every 25 ms for up to 10 seconds, and stops as soon as the read is cancelled. Failed worker replies are now logged with their cause.
- Characters that Supertonic has no sound for, such as box drawing, emoji and math symbols, are removed before synthesis, so pasted text with a table border no longer stops reading on the NPU. The spaces around them are collapsed.
- Changing Read code blocks or a technical reading setting while reading continues at the current position instead of repeating text that was already spoken.

### Platform support

- Built and tested on Fedora 44 (KDE Plasma, Wayland) on an Intel Core Ultra 7 258V, and packaged as an RPM only.
- Local post-processing needs Unix sockets and uses the Linux parent-death signal to stop `llama-server` if the app dies. The `llama-server` bundling is Linux-only.
- Read aloud's worker process, its OpenVINO libraries, the parent-death signal and the 1 GiB free-memory check are Linux-only. On other platforms the memory check is skipped. Clipboard capture uses `wl-paste` or `xclip` on Linux.
- The device ratings shown on the Text-to-speech Models page were measured on that one machine. Other hardware may rank devices differently.
- Windows and macOS are not supported by these features yet.

### Relationship to upstream

- Base version: upstream v0.10.0.
- New settings keys, all with serde defaults so existing stores load unchanged: `local_llm_model_id`, `local_llm_styling`, `local_llm_structure`, `local_llm_context`, `local_llm_threads` (default 4), `history_compare_view` (default `diff`), `tts_model_id` (default `pocket-en`), `tts_voice` (a map of model to voice), `tts_speed` (default 1.0), `tts_pitch_semitones` (default 0), `tts_output_device`, `tts_read_code_blocks` (default off), `tts_technical_reading` (default on), `tts_full_paths` (default on), `tts_full_links` (default off) and `tts_run_on` (a map of model to Auto, CPU, GPU or NPU). A new post-processing provider with id `local_llm` and a new shortcut binding `read_aloud` are added.
- New events: `local-llm-state-changed`, `local-llm-download-progress`, `read-aloud-state`, `tts-download-progress`, `tts-preview-error` and `open-tts-models`. The `history-updated` payload gains a `cleared` action, and retention cleanup now emits a `deleted` action for each removed entry. Three notice codes (`ReadAloudUnavailable`, `ReadAloudBusyPasting` and `VocabularyInvalid`) and one notice source (`ReadAloud`) are added.
- `HistoryEntry` gains `post_process_provider` and `post_process_model`, backed by two additive nullable columns in a new history migration.
- New Tauri commands: `clear_binding`, `clear_history`, `get_history_clear_summary`, `change_history_compare_view_setting`; the `local_llm` commands for listing, downloading, deleting, selecting and reporting status of models and for the three S1 settings; and the text-to-speech commands `get_tts_models`, `get_tts_status`, `get_tts_devices`, `get_read_aloud_state`, `read_aloud_replace_text`, `read_aloud_play`, `read_aloud_pause`, `read_aloud_stop`, `read_aloud_close`, `set_tts_selection`, `set_tts_model`, `set_tts_run_on`, `unload_tts_model`, `download_tts_model`, `cancel_tts_download`, `delete_tts_model`, `set_tts_hf_token` and `preview_tts_voice`.
- New dependencies: in `src-tauri`, `thiserror` and the `blocking` feature of `reqwest`, plus the two workspace crates. The workspace adds `ort`, `ndarray`, `rand`, `rand_distr`, `regex`, `unicode-normalization`, `sentencepiece-rs`, `misaki-rs`, `signalsmith-stretch`, `openvino` and `openvino-sys` (runtime-linked), and `handy-tts-core` adds `pulldown-cmark` and `sha2`. Workspace-wide lint settings and a `cargo fmt --all` format script were added, and the lockfile moved to the workspace root.
- New user data file: `tts-vocabulary.json` in the app data directory, created on first use. The built-in vocabulary is compiled in from `src-tauri/resources/tts/vocabulary.json`, so nothing extra has to be staged for it.
- Build requirements: a static, CPU-only `llama-server` must be staged as `llm-bin/`, and the text-to-speech worker with its OpenVINO libraries as `tts-bin/`. The RPM bundle installs them at `/usr/lib/Handy/llm` and `/usr/lib/Handy/tts` (`bundle.linux.rpm.files` in `tauri.conf.json`).
- Upstream files that gain hunks, grouped:
  - Backend core: `actions.rs`, `settings.rs`, `lib.rs`, `tray.rs`, `notices.rs`, `overlay.rs`, `input.rs`, `utils.rs`, `clipboard.rs` (paste hardening), `transcription_coordinator.rs`, `managers/transcription_jobs.rs`, `commands/mod.rs`, `commands/transcription.rs`.
  - History and models: `managers/history.rs`, `commands/history.rs`, `managers/model/download.rs`.
  - Shortcuts: `shortcut/mod.rs`, `shortcut/handler.rs`, `shortcut/handy_keys.rs`, `shortcut/tauri_impl.rs`.
  - Paste transactions: `paste_tx/mod.rs`, `paste_tx/windows.rs`, `paste_tx/macos.rs`.
  - Packaging and config: `Cargo.toml`, `tauri.conf.json`, `capabilities/default.json`, `package.json`, `flake.nix`, `BUILD.md`, `.github/workflows/build.yml`, `.github/workflows/test.yml`.
  - Frontend: `bindings.ts`, `App.tsx`, `Sidebar.tsx`, the footer and model selector components, the shortcut input, the General, History, Post-processing and Advanced settings, the settings store, `lib/notices.ts`, and the English translation file plus the other 25 locale files.
  - New code lives in `src-tauri/src/local_llm/`, `src-tauri/src/tts/`, `src-tauri/src/shortcut/policy.rs`, `crates/handy-tts/`, `crates/handy-tts-core/`, `src-tauri/resources/tts/`, `src/read-aloud/` and `src/components/settings/tts/`.
