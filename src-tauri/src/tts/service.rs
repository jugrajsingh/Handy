use super::{
    manager::ManagerConfig,
    session::{ReadAloudSession, SessionCommand},
    window, TtsManager,
};
use handy_tts_core::{
    device::{Device, Ratings},
    download::{Downloader, Progress},
    handoff::PasteFence,
    referee::{Activity, Effect, Event, Referee},
    selection::Selection,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Emitter, Manager};
#[derive(Clone)]
pub struct Ready {
    pub manager: Arc<TtsManager>,
    pub session: Arc<ReadAloudSession>,
    pub root: PathBuf,
    pub ratings: Ratings,
    pub available: Vec<Device>,
    pub preparer: Arc<handy_tts_core::preparation::Preparer>,
}
pub struct TtsService {
    app: tauri::AppHandle,
    ready: Mutex<Result<Ready, String>>,
    referee: Mutex<Referee>,
    binding: Mutex<String>,
    pub fence: Arc<PasteFence>,
    pipeline: Mutex<handy_tts_core::handoff::PipelineFence>,
    pub downloader: Downloader,
    preview: Arc<handy_tts_core::preview::PreviewCoordinator>,
    pub cancellations:
        Mutex<std::collections::BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>>,
}
pub(crate) fn selection(settings: &crate::settings::AppSettings) -> Selection {
    Selection {
        model_id: settings.tts_model_id.clone(),
        voice: settings
            .tts_voice
            .get(&settings.tts_model_id)
            .cloned()
            .unwrap_or_else(|| "alba".into()),
        speed: settings.tts_speed,
        pitch: settings.tts_pitch_semitones,
        output_device: settings.tts_output_device.clone(),
        read_code: settings.tts_read_code_blocks,
    }
}
pub(crate) fn reading_cancel_with(effect: &Effect, set_owner: impl FnOnce(bool)) {
    match effect {
        Effect::OpenPopup { .. } => set_owner(true),
        Effect::ClosePopup => set_owner(false),
        _ => {}
    }
}
pub(crate) fn preview_with(
    reading: &impl handy_tts_core::preview::ReadingControl,
    coordinator: &handy_tts_core::preview::PreviewCoordinator,
    stop: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    launch: impl FnOnce(Arc<std::sync::atomic::AtomicBool>),
) -> Result<(), String> {
    let cancel = coordinator.begin(reading, stop)?;
    launch(cancel);
    Ok(())
}
struct PreviewCompletion(Arc<(Mutex<bool>, std::sync::Condvar)>);
impl Drop for PreviewCompletion {
    fn drop(&mut self) {
        let (finished, wake) = self.0.as_ref();
        *finished.lock().unwrap_or_else(|p| p.into_inner()) = true;
        wake.notify_all();
    }
}
impl TtsService {
    pub fn initialize(app: &tauri::AppHandle) -> Arc<Self> {
        let service = Arc::new(Self {
            app: app.clone(),
            ready: Mutex::new(Err("tts.errors.starting".into())),
            referee: Mutex::new(Referee::default()),
            binding: Mutex::new("transcribe".into()),
            fence: Arc::new(PasteFence::default()),
            pipeline: Mutex::new(handy_tts_core::handoff::PipelineFence::default()),
            downloader: Downloader::default(),
            preview: Arc::new(handy_tts_core::preview::PreviewCoordinator::default()),
            cancellations: Mutex::new(std::collections::BTreeMap::new()),
        });
        let started = service.clone();
        std::thread::spawn(move || {
            let result = started.boot();
            *started.ready.lock().unwrap_or_else(|p| p.into_inner()) = result;
        });
        service
    }
    fn boot(&self) -> Result<Ready, String> {
        let settings = crate::settings::get_settings(&self.app);
        let root = self
            .app
            .path()
            .app_data_dir()
            .map_err(|_| "tts.errors.path".to_string())?
            .join("models/tts");
        let home = self
            .app
            .path()
            .home_dir()
            .map_err(|_| "tts.errors.path".to_string())?;
        let cache = self
            .app
            .path()
            .app_cache_dir()
            .map_err(|_| "tts.errors.path".to_string())?
            .join("tts-ov-cache");
        let resource = self
            .app
            .path()
            .resolve(
                "resources/tts/ratings.json",
                tauri::path::BaseDirectory::Resource,
            )
            .map_err(|_| "tts.errors.path".to_string())?;
        let ratings: Ratings = serde_json::from_slice(
            &std::fs::read(resource).map_err(|_| "tts.errors.invalidRatings".to_string())?,
        )
        .map_err(|_| "tts.errors.invalidRatings".to_string())?;
        let binary = std::env::var_os("HANDY_TTS_WORKER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/lib/Handy/tts/handy-tts"));
        let mut command = std::process::Command::new(&binary);
        command
            .arg("--list-devices")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let driver =
            handy_tts_core::worker_env::discover_driver(&home.join(".local/share/intel-npu"))
                .map_err(|e| e.to_string())?;
        let parent = binary
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .as_os_str();
        command.env("LD_LIBRARY_PATH", parent).envs(
            handy_tts_core::worker_env::worker_environment(
                Device::Npu,
                driver.as_ref(),
                Some(parent),
            ),
        );
        if binary.with_file_name("libopenvino_c.so.2641").is_file() {
            command.env(
                "HANDY_OPENVINO_LIBRARY",
                binary.with_file_name("libopenvino_c.so.2641"),
            );
        }
        #[cfg(target_os = "linux")]
        crate::process_supervisor::set_parent_death_signal(&mut command);
        let available = match super::clipboard::capture_command_with_timeout(
            command,
            std::time::Duration::from_secs(30),
        ) {
            Ok(output) => {
                let value: serde_json::Value =
                    serde_json::from_str(&output).map_err(|_| "tts.errors.protocol".to_string())?;
                value["available"]
                    .as_array()
                    .ok_or("tts.errors.protocol")?
                    .iter()
                    .filter_map(|name| match name.as_str() {
                        Some("CPU") => Some(Device::Cpu),
                        Some("GPU") => Some(Device::Gpu),
                        Some("NPU") => Some(Device::Npu),
                        _ => None,
                    })
                    .collect()
            }
            _ => vec![Device::Cpu],
        };
        let config = ManagerConfig {
            root: root.clone(),
            binary,
            cache,
            home,
            ratings: ratings.clone(),
            available: available.clone(),
            run_on: settings.tts_run_on.clone(),
        };
        let manager = TtsManager::new(config).map_err(|e| e.to_string())?;
        let app = self.app.clone();
        let session = ReadAloudSession::spawn(
            manager.clone(),
            selection(&settings),
            settings.selected_output_device.clone(),
            Arc::new(move |state| {
                let _ = app.emit("read-aloud-state", state);
            }),
        );
        manager.spawn_idle(self.app.clone(), session.busy.clone());
        let app = self.app.clone();
        let position = settings.overlay_position;
        self.app
            .run_on_main_thread(move || {
                if let Err(error) = window::create(&app, position) {
                    log::error!("{error}");
                }
            })
            .map_err(|_| "tts.errors.window".to_string())?;
        self.app.manage(manager.clone());
        self.app.manage(session.clone());
        Ok(Ready {
            manager,
            session,
            root,
            ratings,
            available,
            preparer: Arc::new(handy_tts_core::preparation::Preparer::default()),
        })
    }
    pub fn cancel_preview(&self) -> Result<(), String> {
        self.preview.cancel()
    }
    pub fn reading_command(&self, command: SessionCommand) -> Result<(), String> {
        self.preview.reading_command(|| {
            self.ready()?
                .session
                .send(command)
                .map_err(|e| e.to_string())
        })
    }
    pub fn preview(&self, chosen: Selection, text: String) -> Result<(), String> {
        let ready = self.ready()?;
        let preview = ready.manager.fork().map_err(|e| e.to_string())?;
        let completed = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let stop = {
            let preview = preview.clone();
            let completed = completed.clone();
            Arc::new(move || {
                preview.cancel(1);
                preview.interrupt();
                let (finished, wake) = completed.as_ref();
                let finished = finished.lock().unwrap_or_else(|p| p.into_inner());
                let (finished, _) = wake
                    .wait_timeout_while(finished, std::time::Duration::from_secs(5), |finished| {
                        !*finished
                    })
                    .map_err(|_| "tts.errors.generic".to_owned())?;
                if *finished {
                    Ok(())
                } else {
                    Err("tts.errors.generic".into())
                }
            }) as Arc<dyn Fn() -> Result<(), String> + Send + Sync>
        };
        let coordinator = self.preview.clone();
        let app = self.app.clone();
        preview_with(ready.session.as_ref(), &self.preview, stop, |cancel| {
            std::thread::spawn(move || {
                let _completed = PreviewCompletion(completed);
                use handy_tts_core::playback::{Playback, Poll};
                let result = (|| -> Result<(), String> {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(());
                    }
                    let raw = preview
                        .synthesize(&chosen, 1, &text)
                        .map_err(|e| e.to_string())?;
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(());
                    }
                    let pcm = handy_tts_core::dsp::transform(
                        raw,
                        chosen.speed,
                        chosen.pitch,
                        chosen.model_id != "pocket-en",
                    )
                    .map_err(|e| e.to_string())?;
                    let handy = crate::settings::get_settings(&app).selected_output_device;
                    let mut output = Playback::new(
                        super::playback::RodioOutput::default(),
                        chosen.output_device.as_deref(),
                        handy.as_deref(),
                    )
                    .map_err(|e| e.to_string())?;
                    output.play(0, pcm, 0).map_err(|e| e.to_string())?;
                    while !cancel.load(std::sync::atomic::Ordering::Acquire) {
                        if output.poll().map_err(|e| e.to_string())? == Poll::Finished {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    output.stop();
                    Ok(())
                })();
                preview.unload();
                coordinator.finish(&cancel);
                if let Err(error) = result {
                    if !cancel.load(std::sync::atomic::Ordering::Acquire) {
                        let _ = app.emit("tts-preview-error", error);
                    }
                }
            });
        })
    }
    pub fn ready(&self) -> Result<Ready, String> {
        self.ready.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn is_reading(&self) -> bool {
        let referee = self.referee.lock().unwrap_or_else(|p| p.into_inner());
        referee.popup_open || referee.activity == Activity::Reading
    }
    pub fn close(&self) {
        let _ = self.cancel_preview();
        self.apply(Event::Close);
    }
    pub fn apply(&self, event: Event) {
        let effects = self
            .referee
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .apply(event);
        self.dispatch_effects(effects);
    }
    fn dispatch_effects(&self, effects: Vec<Effect>) {
        for effect in effects {
            reading_cancel_with(&effect, |active| {
                crate::shortcut::cancel::set(
                    &self.app,
                    crate::shortcut::cancel::Owner::Reading,
                    active,
                )
            });
            match effect {
                Effect::StopReading => {
                    if let Ok(ready) = self.ready() {
                        let _ = ready.session.send(SessionCommand::Close);
                    }
                }
                Effect::ClosePopup => {
                    let app = self.app.clone();
                    let _ = self.app.run_on_main_thread(move || window::hide(&app));
                }
                Effect::OpenPopup { text } => {
                    if let Ok(ready) = self.ready() {
                        let _ = ready.session.send(SessionCommand::Replace {
                            text,
                            autoplay: true,
                        });
                    }
                    let app = self.app.clone();
                    let _ = self.app.run_on_main_thread(move || window::show(&app));
                }
                Effect::FinishRecording => {
                    if let Some(coordinator) =
                        self.app.try_state::<crate::TranscriptionCoordinator>()
                    {
                        let binding = self
                            .binding
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        coordinator.stop_recording(&binding, "read_aloud");
                    }
                }
                Effect::StartDictation | Effect::Capture { .. } => {}
            }
        }
    }
    pub fn read_shortcut(self: &Arc<Self>) {
        let effects = self
            .referee
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .apply(Event::ReadShortcut);
        if let Some(Effect::Capture { ticket }) = effects.first() {
            let ticket = *ticket;
            if !self.fence.reserve_capture() {
                self.apply(Event::Clipboard {
                    ticket,
                    text: String::new(),
                });
                return;
            }
            let service = self.clone();
            std::thread::spawn(move || {
                #[cfg(target_os = "linux")]
                let text = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                    super::clipboard::capture_wayland()
                } else {
                    let mut command = std::process::Command::new("xclip");
                    command.args(["-selection", "clipboard", "-o", "-t", "UTF8_STRING"]);
                    super::clipboard::capture_command_with_timeout(
                        command,
                        std::time::Duration::from_secs(30),
                    )
                };
                #[cfg(not(target_os = "linux"))]
                let text = {
                    use tauri_plugin_clipboard_manager::ClipboardExt;
                    service
                        .app
                        .clipboard()
                        .read_text()
                        .map_err(|_| super::TtsError::Clipboard)
                };
                if let Err(error) = service.cancel_preview() {
                    let _ = service.app.emit("tts-preview-error", error);
                    service.apply(Event::Close);
                } else {
                    service.apply(Event::Clipboard {
                        ticket,
                        text: text.unwrap_or_default(),
                    });
                }
                service.fence.release_capture();
            });
        } else {
            let service = self.clone();
            std::thread::spawn(move || {
                if let Err(error) = service.cancel_preview() {
                    let _ = service.app.emit("tts-preview-error", error);
                    return;
                }
                service.dispatch_effects(effects);
            });
        }
    }
    pub fn dictation_start(&self, binding: &str) -> Result<(), super::TtsError> {
        self.cancel_preview().map_err(|_| super::TtsError::Worker)?;
        if let Ok(ready) = self.ready() {
            ready.session.stop_and_wait()?;
        }
        self.pipeline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .begin();
        *self.binding.lock().unwrap_or_else(|p| p.into_inner()) = binding.into();
        self.apply(Event::DictationStart);
        Ok(())
    }
    pub fn generation(&self) -> u64 {
        self.pipeline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
    }
    pub fn paste_queued(&self, generation: u64) {
        self.pipeline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queued(generation);
    }
    pub fn pipeline_finished(&self, generation: u64) -> bool {
        let finished = self
            .pipeline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .finish(generation);
        if finished {
            self.dictation_finished();
        }
        finished
    }
    pub fn paste_completed(&self, generation: u64) {
        self.pipeline
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pasted(generation);
        if self.pipeline_finished(generation) {
            if let Some(c) = self.app.try_state::<crate::TranscriptionCoordinator>() {
                c.notify_processing_finished();
            }
        }
    }
    pub fn dictation_aborted(&self) {
        self.apply(Event::Cancel);
    }
    pub fn dictation_processing(&self) {
        self.apply(Event::DictationProcessing);
    }
    pub fn dictation_finished(&self) {
        self.apply(Event::DictationFinished);
    }
    pub fn download_progress(&self, progress: Progress) {
        let _ = self.app.emit("tts-download-progress", progress);
    }
    pub fn shutdown(&self) {
        let _ = self.cancel_preview();
        if let Ok(ready) = self.ready() {
            ready.preparer.shutdown();
            let _ = ready.session.shutdown();
            ready.manager.unload();
        }
    }
}

impl handy_tts_core::preview::ReadingControl for ReadAloudSession {
    fn pause_for_preview(&self) -> Result<(), String> {
        self.pause_and_wait().map_err(|e| e.to_string())
    }
}
