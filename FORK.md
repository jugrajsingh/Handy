# About this fork

## What this is

This is a fork of [Handy](https://github.com/cjpais/Handy), built on upstream v0.10.0. It is not affiliated with or endorsed by the upstream author. It is released under the same licence as upstream, the MIT License (see [LICENSE](LICENSE)). The models it can download have their own licences, which the app shows on each model's card.

Handy is a free, offline speech-to-text application. This fork keeps all of it and adds local post-processing, read aloud and a set of fixes. The full list is in [CHANGELOG.md](CHANGELOG.md).

## What it adds

**Local post-processing.** A "Local (in-app)" post-processing provider cleans up a transcript with a small language model that runs on the CPU through a bundled `llama-server`. Nothing has to be installed or started separately, and transcripts never leave the machine. Five downloadable models are offered, each pinned to a fixed revision, size and SHA-256. History keeps which model produced each cleaned-up entry and compares it with the raw text.

**Read aloud.** A shortcut (Super+Alt+R by default) reads the text you last copied, in a popup that highlights the sentence being read. Three downloadable voice models are offered: Pocket TTS, Supertonic 3 and Kokoro-82M. Each runs on the CPU, or on an Intel GPU or NPU through OpenVINO, and the app picks the best device that works on the machine. Reading is streamed and has speed and pitch controls. Markdown marks are not spoken and code blocks are skipped unless you ask for them. Technical reading speaks paths, file names, flags, versions and code names the way an engineer would say them, using a vocabulary file you can extend (`tts-vocabulary.json` in the app data directory, which also accepts entries for one voice model only).

**Speech-to-text changes.** The transcription engines and models are upstream's, and the fork does not change how speech is recognised. Its changes are around dictation: shortcut changes are applied atomically and always keep a usable dictation shortcut, a Raw Shortcut sits next to the Post-processing Shortcut, the helper programs that paste dictated text run under a deadline so a hung helper cannot hold up dictation, selecting a model from the tray no longer blocks dictation, and Clear History cancels transcriptions still running for the entries it removes. Dictation and reading never overlap.

**Notable fixes.** Supertonic on the Intel GPU no longer adds static, Pocket TTS on the Intel GPU no longer sounds fast and unintelligible, a new read started while the last one is being cancelled no longer reports that the worker stopped, and characters a voice cannot pronounce no longer stop reading on the NPU. The History list keeps its place when entries are added or removed, and the tray menu stays in step with the models and settings.

## Why choose it, and why not

Choose this fork if you want dictation with a local cleanup model, or want to hear copied text read back, on Linux with an Intel machine, and you are comfortable building from source.

Choose upstream if any of these apply:

- You use Windows or macOS. The fork's additions are only built and tested on Linux, and the fork's read aloud worker and local model bundling are Linux-only.
- You want official releases, installers and an updater. The fork has no published packages yet.
- You want the smallest install. The fork bundles a `llama-server` and a text-to-speech worker with its OpenVINO libraries.
- You want a project reviewed and used by many people. The fork has one maintainer and was tested on one machine, with an Intel GPU and NPU in mind.

Its new interface strings are translated into the other 25 languages by machine and have not been reviewed by native speakers.

## How it tracks upstream

Upstream changes are merged into the fork regularly, and upstream's own release notes describe them. The fork's additions are kept as a separate series of commits on top of upstream, and that series is re-applied to each new upstream version. The "Relationship to upstream" section of [CHANGELOG.md](CHANGELOG.md) lists every setting, event, command, dependency and upstream file the fork touches.

## Getting it

There are no packages yet. Clone this repository and build it from source by following [BUILD.md](BUILD.md), plus the extra build requirements in the "Relationship to upstream" section of [CHANGELOG.md](CHANGELOG.md): a static, CPU-only `llama-server` staged as `llm-bin/`, and the text-to-speech worker with its OpenVINO libraries staged as `tts-bin/`. RPM packaging exists for the maintainer's own machine only, and is not a supported way to install it.

## Platform support

- Built and tested on Fedora 44 (KDE Plasma, Wayland) on an Intel Core Ultra 7 258V, and packaged as an RPM only.
- Local post-processing needs Unix sockets and uses the Linux parent-death signal to stop `llama-server` if the app dies. The `llama-server` bundling is Linux-only.
- Read aloud's worker process, its OpenVINO libraries, the parent-death signal and the 1 GiB free-memory check are Linux-only. On other platforms the memory check is skipped. Clipboard capture uses `wl-paste` or `xclip` on Linux.
- The device ratings shown on the Text-to-speech Models page were measured on that one machine. Other hardware may rank devices differently.
- Windows and macOS are not supported by these features yet.
