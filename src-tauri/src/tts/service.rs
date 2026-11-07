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
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
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
    refused_shortcut: AtomicBool,
    reading: AtomicBool,
    tray_refresh: mpsc::SyncSender<()>,
    referee: Mutex<Referee>,
    binding: Mutex<String>,
    pub fence: Arc<PasteFence>,
    pipeline: Mutex<handy_tts_core::handoff::PipelineFence>,
    pub downloader: Downloader,
    tray: Mutex<handy_tts_core::tray::TtsMenu>,
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
        technical: settings.tts_technical_reading,
        full_paths: settings.tts_full_paths,
        full_links: settings.tts_full_links,
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
pub(crate) fn dictation_start_with(
    cancel_preview: impl FnOnce() -> Result<(), String>,
    stop_reading: impl FnOnce() -> Result<(), super::TtsError>,
    begin: impl FnOnce(),
) -> Result<(), super::TtsError> {
    begin();
    if let Err(error) = cancel_preview() {
        log::warn!("Read-aloud preview stop failed during dictation: {error}");
    }
    if let Err(error) = stop_reading() {
        log::warn!("Read-aloud stop acknowledgment failed during dictation: {error}");
    }
    Ok(())
}
fn read_shortcut_with(
    referee: &mut Referee,
    ready: Result<(), String>,
    refused: &AtomicBool,
    log: impl FnOnce(&str),
) -> Vec<Effect> {
    if let Err(error) = ready {
        if !refused.swap(true, Ordering::AcqRel) {
            log(&error);
        }
        return Vec::new();
    }
    refused.store(false, Ordering::Release);
    referee.apply(Event::ReadShortcut)
}
fn unavailable_with(error: &str, log: impl FnOnce(&str), notify: impl FnOnce()) {
    log(error);
    notify();
}
struct CaptureReservation<'a>(&'a PasteFence);
impl Drop for CaptureReservation<'_> {
    fn drop(&mut self) {
        self.0.release_capture();
    }
}

fn capture_with(fence: &PasteFence, revision: u64, capture: impl FnOnce(), refused: impl FnOnce()) {
    if !fence.reserve_capture_since(revision) {
        refused();
        return;
    }
    let _reservation = CaptureReservation(fence);
    capture();
}

fn capture_off_thread(job: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(job)
}
fn apply_ordered(
    referee: &Mutex<Referee>,
    event: Event,
    dispatch: impl FnOnce(&Referee, Vec<Effect>),
) {
    let mut referee = referee.lock().unwrap_or_else(|p| p.into_inner());
    let effects = referee.apply(event);
    dispatch(&referee, effects);
}
fn driver_or_cpu<T, E: std::fmt::Display>(result: Result<Option<T>, E>) -> Option<T> {
    result.unwrap_or_else(|error| {
        log::warn!("NPU driver discovery failed; using no driver: {error}");
        None
    })
}
fn available_or_cpu(output: Option<&str>) -> Vec<Device> {
    let devices = output
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|value| value.get("available").and_then(|v| v.as_array()).cloned());
    let mut result = vec![Device::Cpu];
    if let Some(devices) = devices {
        for value in devices {
            let device = match value.as_str() {
                Some("GPU") => Device::Gpu,
                Some("NPU") => Device::Npu,
                _ => continue,
            };
            if !result.contains(&device) {
                result.push(device);
            }
        }
    }
    result
}
fn run_tray_refreshes(events: mpsc::Receiver<()>, mut refresh: impl FnMut() -> bool) {
    while events.recv().is_ok() {
        if !refresh() {
            break;
        }
    }
}
/// The vocabulary for each document: the built-in one plus the user's file, read again every
/// time. A file that cannot be used raises one warning per distinct error.
fn user_vocabulary(app: tauri::AppHandle) -> super::session::VocabularyLoader {
    let reported = super::vocabulary::Reported::default();
    Arc::new(move || {
        let dir = match crate::portable::app_data_dir(&app) {
            Ok(dir) => dir,
            Err(error) => {
                log::warn!(
                    "Read-aloud vocabulary: no app data dir, using the built-in one: {error}"
                );
                return Arc::new(handy_tts_core::vocabulary::Vocabulary::builtin());
            }
        };
        let path = dir.join(super::vocabulary::FILE_NAME);
        super::vocabulary::load_vocabulary(&dir, &reported, &mut |error| {
            crate::notices::raise(
                &app,
                crate::notices::Notice::new(
                    crate::notices::NoticeCode::VocabularyInvalid,
                    crate::notices::NoticeSource::ReadAloud,
                )
                .with_detail(format!("{}: {error}", path.display())),
            )
        })
    })
}
impl TtsService {
    pub fn initialize(app: &tauri::AppHandle) -> Arc<Self> {
        let (tray_refresh, refreshes) = mpsc::sync_channel(1);
        let service = Arc::new(Self {
            app: app.clone(),
            refused_shortcut: AtomicBool::new(false),
            reading: AtomicBool::new(false),
            tray_refresh,
            ready: Mutex::new(Err("tts.errors.starting".into())),
            referee: Mutex::new(Referee::default()),
            binding: Mutex::new("transcribe".into()),
            fence: Arc::new(PasteFence::default()),
            pipeline: Mutex::new(handy_tts_core::handoff::PipelineFence::default()),
            downloader: Downloader::default(),
            tray: Mutex::new(handy_tts_core::tray::tts_menu(
                &[],
                None,
                handy_tts_core::lifecycle::State::Unloaded,
            )),
            preview: Arc::new(handy_tts_core::preview::PreviewCoordinator::default()),
            cancellations: Mutex::new(std::collections::BTreeMap::new()),
        });
        let started = service.clone();
        std::thread::spawn(move || {
            let result = started.boot();
            *started.ready.lock().unwrap_or_else(|p| p.into_inner()) = result;
            started.refresh_tray();
            let weak = Arc::downgrade(&started);
            drop(started);
            run_tray_refreshes(refreshes, || {
                let Some(service) = weak.upgrade() else {
                    return false;
                };
                service.update_tray_snapshot();
                true
            });
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
        let driver_result =
            handy_tts_core::worker_env::discover_driver(&home.join(".local/share/intel-npu"));
        let driver_failed = driver_result.is_err();
        let driver = driver_or_cpu(driver_result);
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
        let output = super::clipboard::capture_command_with_timeout(
            command,
            std::time::Duration::from_secs(30),
        )
        .ok();
        let available = available_or_cpu(if driver_failed {
            None
        } else {
            output.as_deref()
        });
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
            {
                let app = self.app.clone();
                Arc::new(move || crate::settings::get_settings(&app).selected_output_device)
            },
            user_vocabulary(self.app.clone()),
            Arc::new(move |state| {
                let _ = app.emit("read-aloud-state", state);
            }),
        );
        manager.spawn_idle(self.app.clone(), session.busy.clone());
        let app = self.app.clone();
        let position = settings.overlay_position;
        let (created, wait) = mpsc::channel();
        let created = self
            .app
            .run_on_main_thread(move || {
                let _ = created.send(window::create(&app, position));
            })
            .map_err(|_| "tts.errors.window".to_string())
            .and_then(|()| {
                wait.recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or_else(|_| Err("tts.errors.window".into()))
            });
        if let Err(error) = created {
            session.shutdown();
            manager.unload();
            return Err(error);
        }
        let refresh = self.tray_refresh.clone();
        manager.on_status_change(Arc::new(move || {
            let _ = refresh.try_send(());
        }));
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
        if matches!(command, SessionCommand::Stop | SessionCommand::Close) {
            return self.preview.stopping_command(|| {
                self.ready()?
                    .session
                    .send(command)
                    .map_err(|e| e.to_string())
            });
        }
        self.preview.reading_command(|| {
            let referee = self.referee.lock().unwrap_or_else(|p| p.into_inner());
            if referee.activity == Activity::Dictating
                && matches!(
                    command,
                    SessionCommand::Play | SessionCommand::Replace { autoplay: true, .. }
                )
            {
                return Err("tts.errors.busy".into());
            }
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
                use handy_tts_core::{
                    feed::Segment,
                    playback::segment::{OutputEvent, Playback},
                };
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
                    let model = handy_tts_core::catalog::entry(&chosen.model_id)
                        .map_err(|e| e.to_string())?;
                    let plan = handy_tts_core::speed::plan(chosen.speed, model.native_speed)
                        .map_err(|e| e.to_string())?;
                    let pcm = handy_tts_core::dsp::stretch(raw, plan.stretch, chosen.pitch)
                        .map_err(|e| e.to_string())?;
                    let handy = crate::settings::get_settings(&app).selected_output_device;
                    let mut output = Playback::new(
                        super::playback::RodioOutput::default(),
                        chosen.output_device.as_deref(),
                        handy.as_deref(),
                    )
                    .map_err(|e| e.to_string())?;
                    output
                        .output
                        .set_sample_rate(pcm.sample_rate)
                        .map_err(|e| e.to_string())?;
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return Ok(());
                    }
                    output
                        .append(Segment {
                            epoch: 0,
                            index: 0,
                            speech: pcm.samples,
                            pause: 0,
                        })
                        .map_err(|e| e.to_string())?;
                    while !cancel.load(std::sync::atomic::Ordering::Acquire) {
                        if output
                            .events()
                            .map_err(|e| e.to_string())?
                            .iter()
                            .any(|event| matches!(event, OutputEvent::Finished { index: 0, .. }))
                        {
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
    /// Queues a tray refresh after download or selection changes.
    pub fn refresh_tray(&self) {
        let _ = self.tray_refresh.try_send(());
    }
    fn update_tray_snapshot(&self) {
        let Ok(ready) = self.ready() else {
            return;
        };
        let settings = crate::settings::get_settings(&self.app);
        let downloaded = handy_tts_core::catalog::entries()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| handy_tts_core::download::is_downloaded(&ready.root, entry))
                    .map(|entry| (entry.id.to_owned(), entry.name.to_owned()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let desired = handy_tts_core::tray::tts_menu(
            &downloaded,
            Some(&settings.tts_model_id),
            ready.manager.status(),
        );
        let changed = {
            let mut cached = self.tray.lock().unwrap_or_else(|p| p.into_inner());
            if *cached == desired {
                false
            } else {
                *cached = desired;
                true
            }
        };
        if changed {
            crate::tray::update_tray_menu(&self.app);
        }
    }
    /// Returns the tray snapshot updated by download, selection and status events.
    pub fn tray_snapshot(&self) -> handy_tts_core::tray::TtsMenu {
        self.tray.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn ready(&self) -> Result<Ready, String> {
        self.ready.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn is_reading(&self) -> bool {
        self.reading.load(Ordering::Acquire)
    }
    pub fn close(&self) {
        self.apply(Event::Close);
        let _ = self.cancel_preview();
    }
    pub fn apply(&self, event: Event) {
        apply_ordered(&self.referee, event, |referee, effects| {
            self.preview
                .set_dictating(referee.activity == Activity::Dictating);
            self.reading.store(
                referee.popup_open || referee.activity == Activity::Reading,
                Ordering::Release,
            );
            self.dispatch_effects(effects);
        });
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
                            client_revision: None,
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
                Effect::CaptureRefused => crate::notices::raise(
                    &self.app,
                    crate::notices::Notice::new(
                        crate::notices::NoticeCode::ReadAloudBusyPasting,
                        crate::notices::NoticeSource::ReadAloud,
                    ),
                ),
                Effect::StartDictation | Effect::Capture { .. } => {}
            }
        }
    }
    pub fn read_shortcut(self: &Arc<Self>) {
        let revision = self.fence.revision();
        let ticket = {
            let mut referee = self.referee.lock().unwrap_or_else(|p| p.into_inner());
            if self.ready().is_ok() {
                crate::notices::clear(&self.app, crate::notices::NoticeSource::ReadAloud);
            }
            let effects = read_shortcut_with(
                &mut referee,
                self.ready().map(|_| ()),
                &self.refused_shortcut,
                |error| {
                    unavailable_with(
                        error,
                        |error| log::warn!("Read-aloud shortcut refused before readiness: {error}"),
                        || {
                            crate::notices::raise(
                                &self.app,
                                crate::notices::Notice::new(
                                    crate::notices::NoticeCode::ReadAloudUnavailable,
                                    crate::notices::NoticeSource::ReadAloud,
                                ),
                            )
                        },
                    )
                },
            );
            self.reading.store(
                referee.popup_open || referee.activity == Activity::Reading,
                Ordering::Release,
            );
            if let Some(Effect::Capture { ticket }) = effects.first() {
                Some(*ticket)
            } else {
                self.dispatch_effects(effects);
                None
            }
        };
        let service = self.clone();
        capture_off_thread(move || {
            if let Some(ticket) = ticket {
                service.capture_shortcut(ticket, revision);
            } else {
                let _ = service.cancel_preview();
            }
        });
    }
    fn capture_shortcut(self: &Arc<Self>, ticket: u64, revision: u64) {
        capture_with(
            &self.fence,
            revision,
            || {
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
                    self.app
                        .clipboard()
                        .read_text()
                        .map_err(|_| super::TtsError::Clipboard)
                };
                if let Err(error) = self.cancel_preview() {
                    let _ = self.app.emit("tts-preview-error", error);
                    self.apply(Event::Close);
                } else {
                    self.apply(Event::Clipboard {
                        ticket,
                        text: text.unwrap_or_default(),
                    });
                }
            },
            || self.apply(Event::CaptureRefused { ticket }),
        );
    }
    pub fn dictation_start(&self, binding: &str) -> Result<(), super::TtsError> {
        dictation_start_with(
            || self.cancel_preview(),
            || {
                self.ready()
                    .map_or(Ok(()), |ready| ready.session.stop_and_wait())
            },
            || {
                self.pipeline
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .begin();
                *self.binding.lock().unwrap_or_else(|p| p.into_inner()) = binding.into();
                self.apply(Event::DictationStart);
            },
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_reads_technical_by_default_with_full_paths_and_short_links() {
        let chosen = selection(&crate::settings::get_default_settings());
        assert!(chosen.technical);
        assert!(chosen.full_paths);
        assert!(!chosen.full_links);
    }
    #[test]
    fn selection_follows_the_technical_reading_settings() {
        let mut settings = crate::settings::get_default_settings();
        settings.tts_technical_reading = false;
        settings.tts_full_paths = false;
        settings.tts_full_links = true;
        let chosen = selection(&settings);
        assert!(!chosen.technical);
        assert!(!chosen.full_paths);
        assert!(chosen.full_links);
    }
    #[test]
    fn wbfix3_capture_reservation_releases_on_unwind() {
        let fence = PasteFence::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            capture_with(
                &fence,
                fence.revision(),
                || panic!("fixture unwind"),
                || panic!("unexpected refusal"),
            );
        }));
        assert!(result.is_err());
        assert!(fence.reserve_capture(), "capture must release on unwind");
        fence.release_capture();
    }

    #[test]
    fn wbfix3_busy_capture_settles_without_changing_dictation() {
        let fence = PasteFence::default();
        let mut referee = Referee::default();
        referee.apply(Event::DictationStart);
        referee.apply(Event::DictationProcessing);
        referee.apply(Event::ReadShortcut);
        let ticket = referee.ticket;
        assert!(
            referee
                .apply(Event::CaptureRefused {
                    ticket: ticket.wrapping_sub(1)
                })
                .is_empty(),
            "stale capture refusal must be ignored"
        );
        assert!(referee.capture_pending);
        assert!(fence.reserve_capture());
        let mut effects = Vec::new();
        capture_with(
            &fence,
            fence.revision(),
            || panic!("busy capture must not read"),
            || {
                effects = referee.apply(Event::CaptureRefused { ticket });
            },
        );
        assert_eq!(
            effects,
            [Effect::CaptureRefused],
            "busy capture must emit a visible refusal"
        );
        assert_eq!(referee.activity, Activity::Dictating);
        assert!(!referee.popup_open && !referee.capture_pending && referee.captured.is_none());
        assert!(
            referee.apply(Event::DictationFinished).is_empty(),
            "refused capture must never open a popup"
        );
        fence.release_capture();
        referee.apply(Event::ReadShortcut);
        assert_eq!(
            referee.apply(Event::Clipboard {
                ticket: referee.ticket,
                text: String::new()
            }),
            [Effect::OpenPopup {
                text: String::new()
            }]
        );
    }

    #[test]
    fn wbfix_shortcut_returns_before_capture_work_finishes() {
        let (release, wait) = mpsc::channel();
        let caller = std::thread::current().id();
        let began = std::time::Instant::now();
        let worker = capture_off_thread(move || {
            assert_ne!(std::thread::current().id(), caller);
            wait.recv().unwrap();
        });
        assert!(began.elapsed() < std::time::Duration::from_millis(50));
        release.send(()).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn wbfix_referee_effects_cannot_overtake_an_earlier_transition() {
        let referee = Arc::new(Mutex::new(Referee::default()));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let ticket = match referee.lock().unwrap().apply(Event::ReadShortcut)[0] {
            Effect::Capture { ticket } => ticket,
            _ => panic!(),
        };
        let (entered, wait) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let state = referee.clone();
        let observed = trace.clone();
        let capture = std::thread::spawn(move || {
            apply_ordered(
                &state,
                Event::Clipboard {
                    ticket,
                    text: "original".into(),
                },
                |_, effects| {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    observed.lock().unwrap().extend(effects);
                },
            )
        });
        wait.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let state = referee.clone();
        let observed = trace.clone();
        let (applied, applied_wait) = mpsc::channel();
        let dictation = std::thread::spawn(move || {
            apply_ordered(&state, Event::DictationStart, |_, effects| {
                observed.lock().unwrap().extend(effects);
                applied.send(()).unwrap();
            })
        });
        let overtook = applied_wait
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        release.send(()).unwrap();
        capture.join().unwrap();
        dictation.join().unwrap();
        assert!(!overtook, "effects must dispatch in referee order");
        assert_eq!(
            *trace.lock().unwrap(),
            [
                Effect::OpenPopup {
                    text: "original".into()
                },
                Effect::StopReading,
                Effect::ClosePopup,
                Effect::StartDictation
            ]
        );
        assert!(referee
            .lock()
            .unwrap()
            .apply(Event::Clipboard {
                ticket,
                text: "stale".into()
            })
            .is_empty());
    }
    #[test]
    fn wbfix_unavailable_shortcut_reports_a_visible_notice() {
        let logged = std::cell::Cell::new(false);
        let notified = std::cell::Cell::new(false);
        unavailable_with(
            "tts.errors.starting",
            |_| logged.set(true),
            || notified.set(true),
        );
        assert!(
            logged.get() && notified.get(),
            "unavailable shortcut must report visibly"
        );
    }
    #[test]
    fn wbfix_bad_driver_and_device_probes_keep_cpu_available() {
        assert!(driver_or_cpu::<(), _>(Err("unreadable driver")).is_none());
        for output in [
            None,
            Some("not json"),
            Some("{}"),
            Some(r#"{"available":[]}"#),
        ] {
            assert_eq!(available_or_cpu(output), [Device::Cpu]);
        }
        assert_eq!(
            available_or_cpu(Some(r#"{"available":["CPU","GPU"]}"#)),
            [Device::Cpu, Device::Gpu]
        );
    }
    #[test]
    fn wbfix_dictation_is_active_before_preview_cancellation() {
        let active = std::cell::Cell::new(false);
        dictation_start_with(
            || {
                assert!(
                    active.get(),
                    "dictation must block new previews before cancellation"
                );
                Ok(())
            },
            || Ok(()),
            || active.set(true),
        )
        .unwrap();
    }
    #[test]
    fn tray_refresh_waits_for_events_without_periodic_model_scans() {
        let (refresh, events) = mpsc::channel();
        let (scanned, scans) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            run_tray_refreshes(events, || {
                scanned.send(()).unwrap();
                true
            })
        });
        assert!(
            scans
                .recv_timeout(std::time::Duration::from_millis(650))
                .is_err(),
            "idle tray must not scan models"
        );
        for _ in 0..2 {
            refresh.send(()).unwrap();
            scans
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(
                scans
                    .recv_timeout(std::time::Duration::from_millis(650))
                    .is_err(),
                "tray scans must be driven by events"
            );
        }
        drop(refresh);
        worker.join().unwrap();
    }
    #[test]
    fn early_shortcut_leaves_referee_unchanged_and_logs_once_until_ready() {
        for dictating in [false, true] {
            let mut referee = Referee::default();
            if dictating {
                referee.apply(Event::DictationStart);
            }
            let before = format!("{referee:?}");
            let refused = AtomicBool::new(false);
            let mut logs = 0;
            for _ in 0..2 {
                assert!(read_shortcut_with(
                    &mut referee,
                    Err("tts.errors.starting".into()),
                    &refused,
                    |_| logs += 1
                )
                .is_empty());
                assert_eq!(format!("{referee:?}"), before);
            }
            assert_eq!(logs, 1);
            assert!(matches!(
                read_shortcut_with(&mut referee, Ok(()), &refused, |_| logs += 1).as_slice(),
                [Effect::Capture { .. }]
            ));
            assert!(!refused.load(Ordering::Acquire));
        }
    }
}
