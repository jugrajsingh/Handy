use super::{TtsError, TtsManager};
use handy_tts_core::{
    scheduler::{PlaybackState, Ticket},
    selection::Selection,
    session::{Completion, Session, SessionSnapshot, Worker},
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

struct Work {
    ticket: Ticket,
    text: String,
    selection: Selection,
    id: u64,
}
#[derive(Clone)]
pub(super) struct ManagerWorker {
    manager: Arc<TtsManager>,
    tx: mpsc::SyncSender<Work>,
    rx: Arc<Mutex<mpsc::Receiver<Completion>>>,
    next: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
}
impl ManagerWorker {
    pub(super) fn new(manager: Arc<TtsManager>) -> Self {
        let (tx, jobs) = mpsc::sync_channel::<Work>(1);
        let (results, rx) = mpsc::sync_channel(1);
        let active = Arc::new(AtomicU64::new(0));
        let engine = manager.clone();
        std::thread::spawn(move || {
            for job in jobs {
                let start = Instant::now();
                let result = engine
                    .synthesize(&job.selection, job.id, &job.text)
                    .map_err(|e| e.to_string());
                if results
                    .send(Completion {
                        ticket: job.ticket,
                        result,
                        synthesis_seconds: start.elapsed().as_secs_f64(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            manager,
            tx,
            rx: Arc::new(Mutex::new(rx)),
            next: Arc::new(AtomicU64::new(1)),
            active,
        }
    }
}
impl Worker for ManagerWorker {
    fn request(&mut self, ticket: Ticket, text: &str, selection: &Selection) -> Result<(), String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.active.store(id, Ordering::Release);
        self.tx
            .try_send(Work {
                ticket,
                text: text.into(),
                selection: selection.clone(),
                id,
            })
            .map_err(|_| "tts.errors.busy".into())
    }
    fn poll(&mut self) -> Option<Completion> {
        self.rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_recv()
            .ok()
    }
    fn cancel(&mut self, _: Ticket) {
        let id = self.active.swap(0, Ordering::AcqRel);
        if id != 0 {
            self.manager.cancel(id);
        }
    }
    fn selection_changed(&mut self) {
        self.manager.selected();
    }
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct SessionView {
    pub text: String,
    pub visible: bool,
    pub snapshot: SessionSnapshot,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct SessionEvent {
    pub text: Option<String>,
    pub visible: bool,
    pub snapshot: SessionSnapshot,
}
pub enum SessionCommand {
    Interrupt(mpsc::Sender<()>),
    PauseAcknowledged(mpsc::Sender<()>),
    Replace {
        text: String,
        autoplay: bool,
    },
    Play,
    Pause,
    Stop,
    Close,
    Select {
        selection: Selection,
        downloaded: bool,
        persist: SelectionPersist,
        done: mpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}
type SelectionPersist =
    Box<dyn FnOnce(&Selection) -> Result<(), handy_tts_core::selection::SelectionError> + Send>;
fn apply_selection<W: Worker, O: handy_tts_core::playback::Output>(
    session: &mut Session<'_, W, O>,
    selection: Selection,
    downloaded: bool,
    persist: SelectionPersist,
    done: mpsc::Sender<Result<(), String>>,
) -> bool {
    let result = session.select(selection, downloaded, persist);
    let success = result.is_ok();
    let _ = done.send(result.map(|_| ()).map_err(|error| error.to_string()));
    success
}
pub struct ReadAloudSession {
    tx: mpsc::SyncSender<SessionCommand>,
    view: Arc<Mutex<SessionView>>,
    pub busy: Arc<AtomicBool>,
    manager: Arc<TtsManager>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl ReadAloudSession {
    pub fn spawn(
        manager: Arc<TtsManager>,
        selection: Selection,
        handy_output: Option<String>,
        emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> Arc<Self> {
        let worker = ManagerWorker::new(manager.clone());
        Self::spawn_with(
            manager,
            worker,
            super::playback::RodioOutput::default,
            selection,
            handy_output,
            emit,
        )
    }
    fn spawn_with<W, O>(
        manager: Arc<TtsManager>,
        worker: W,
        output: impl Fn() -> O + Send + 'static,
        selection: Selection,
        handy_output: Option<String>,
        emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> Arc<Self>
    where
        W: Worker + Clone + Send + 'static,
        O: handy_tts_core::playback::Output + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(16);
        let empty = SessionSnapshot {
            epoch: 0,
            state: PlaybackState::Waiting,
            source_start: 0,
            source_end: 0,
            elapsed_seconds: 0.0,
            remaining_seconds: 0.0,
            progress: 0.0,
            error: None,
            output_fallback: false,
            queued_audio_seconds: 0.0,
            buffering_pauses: 0,
            head_start_seconds: None,
        };
        let view = Arc::new(Mutex::new(SessionView {
            text: String::new(),
            visible: false,
            snapshot: empty,
        }));
        let cached = view.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let working = busy.clone();
        let status_manager = manager.clone();
        let join = std::thread::spawn(move || {
            let began = Instant::now();
            let mut source = String::new();
            let mut selected = selection;
            let mut visible = false;
            let mut autoplay = false;
            let mut epoch = 0;
            let mut last_emit = Instant::now() - Duration::from_secs(1);
            'document: loop {
                let mut session = match Session::new(
                    &source,
                    selected.clone(),
                    worker.clone(),
                    output(),
                    handy_output.as_deref(),
                ) {
                    Ok(session) => session,
                    Err(error) => {
                        let mut current = cached.lock().unwrap_or_else(|p| p.into_inner());
                        current.snapshot.error = Some(error);
                        emit(SessionEvent {
                            text: Some(current.text.clone()),
                            visible: current.visible,
                            snapshot: current.snapshot.clone(),
                        });
                        break;
                    }
                };
                session.queue.epoch = epoch;
                if autoplay {
                    if let Err(error) = session.play() {
                        session.fail(error);
                    }
                }
                let mut text_sent = false;
                loop {
                    let message = rx.recv_timeout(Duration::from_millis(20));
                    match message {
                        Ok(SessionCommand::PauseAcknowledged(done)) => {
                            session.pause();
                            let _ = done.send(());
                        }
                        Ok(SessionCommand::Interrupt(done)) => {
                            session.stop();
                            let _ = done.send(());
                        }
                        Ok(SessionCommand::Replace {
                            text,
                            autoplay: start,
                        }) => {
                            session.stop();
                            epoch = session.queue.epoch;
                            drop(session);
                            source = text;
                            autoplay = start;
                            visible = true;
                            continue 'document;
                        }
                        Ok(SessionCommand::Play) => {
                            if let Err(error) = session.play() {
                                session.fail(error);
                            }
                        }
                        Ok(SessionCommand::Pause) => session.pause(),
                        Ok(SessionCommand::Stop) => session.stop(),
                        Ok(SessionCommand::Close) => {
                            session.stop();
                            visible = false;
                        }
                        Ok(SessionCommand::Select {
                            selection,
                            downloaded,
                            persist,
                            done,
                        }) => {
                            if apply_selection(
                                &mut session,
                                selection.clone(),
                                downloaded,
                                persist,
                                done,
                            ) {
                                selected = selection;
                            }
                        }
                        Ok(SessionCommand::Shutdown)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            session.stop();
                            working.store(false, Ordering::Release);
                            break 'document;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if let Err(error) = session.tick(began.elapsed().as_secs_f64()) {
                        session.fail(error);
                    }
                    let mut snapshot = session.snapshot();
                    if snapshot.state == PlaybackState::Buffering
                        && status_manager.status() == handy_tts_core::lifecycle::State::Starting
                    {
                        snapshot.state = PlaybackState::Starting;
                    }
                    working.store(
                        matches!(
                            snapshot.state,
                            PlaybackState::Playing
                                | PlaybackState::Paused
                                | PlaybackState::Buffering
                                | PlaybackState::Starting
                        ),
                        Ordering::Release,
                    );
                    let mut current = cached.lock().unwrap_or_else(|p| p.into_inner());
                    if current.text != source {
                        current.text = source.clone();
                    }
                    current.visible = visible;
                    current.snapshot = snapshot;
                    if last_emit.elapsed() >= Duration::from_millis(200) {
                        emit(SessionEvent {
                            text: if text_sent {
                                None
                            } else {
                                Some(source.clone())
                            },
                            visible,
                            snapshot: current.snapshot.clone(),
                        });
                        text_sent = true;
                        last_emit = Instant::now();
                    }
                }
            }
        });
        Arc::new(Self {
            tx,
            view,
            busy,
            manager,
            join: Mutex::new(Some(join)),
        })
    }
    pub fn send(&self, message: SessionCommand) -> Result<(), TtsError> {
        self.tx.try_send(message).map_err(|_| TtsError::Generic)
    }
    pub fn select_and_wait(
        &self,
        selection: Selection,
        downloaded: bool,
        persist: SelectionPersist,
    ) -> Result<(), String> {
        let (done, wait) = mpsc::channel();
        self.send(SessionCommand::Select {
            selection,
            downloaded,
            persist,
            done,
        })
        .map_err(|e| e.to_string())?;
        wait.recv().map_err(|_| "tts.errors.selection".to_owned())?
    }
    pub fn snapshot(&self) -> SessionView {
        self.view.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn pause_and_wait(&self) -> Result<(), TtsError> {
        let (done, wait) = mpsc::channel();
        self.send(SessionCommand::PauseAcknowledged(done))?;
        wait.recv_timeout(Duration::from_millis(500))
            .map_err(|_| TtsError::Generic)
    }
    pub fn stop_and_wait(&self) -> Result<(), TtsError> {
        let (done, wait) = mpsc::channel();
        self.send(SessionCommand::Interrupt(done))?;
        wait.recv_timeout(Duration::from_millis(500))
            .map_err(|_| TtsError::Generic)
    }
    pub fn shutdown(&self) -> bool {
        self.manager.interrupt();
        let _ = self.send(SessionCommand::Shutdown);
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut join = self.join.lock().unwrap_or_else(|p| p.into_inner());
        while join.as_ref().is_some_and(|thread| !thread.is_finished()) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if join
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(thread) = join.take() {
                return thread.join().is_ok();
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handy_tts_core::{
        playback::{Output, PlaybackError, Poll},
        protocol::Pcm,
    };
    #[derive(Default)]
    struct Audio {
        fail_prepare: bool,
        active: bool,
        stops: usize,
        commits: usize,
    }
    impl Output for Audio {
        type Prepared = ();
        fn names(&self) -> Result<Vec<String>, PlaybackError> {
            Ok(vec!["new".into()])
        }
        fn open(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
            Ok(())
        }
        fn prepare(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
            if self.fail_prepare {
                Err(PlaybackError::Unavailable)
            } else {
                Ok(())
            }
        }
        fn commit(&mut self, _: ()) {
            self.commits += 1;
        }
        fn play(&mut self, _: Pcm, _: u32) -> Result<(), PlaybackError> {
            self.active = true;
            Ok(())
        }
        fn poll(&mut self) -> Poll {
            Poll::Playing
        }
        fn pause(&mut self) {}
        fn resume(&mut self) {}
        fn stop(&mut self) {
            self.active = false;
            self.stops += 1;
        }
    }
    #[derive(Default, Clone)]
    struct Engine;
    impl Worker for Engine {
        fn request(&mut self, _: Ticket, _: &str, _: &Selection) -> Result<(), String> {
            Ok(())
        }
        fn poll(&mut self) -> Option<Completion> {
            None
        }
        fn cancel(&mut self, _: Ticket) {}
        fn selection_changed(&mut self) {}
    }
    fn selection() -> Selection {
        Selection {
            model_id: "supertonic-3".into(),
            voice: "F1".into(),
            speed: 1.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
        }
    }
    fn manager() -> Arc<TtsManager> {
        use handy_tts_core::device::{Device, DeviceRating, ModelRating, Ratings};
        use std::collections::BTreeMap;
        let devices = [Device::Cpu, Device::Gpu, Device::Npu]
            .into_iter()
            .map(|device| {
                let cpu = device == Device::Cpu;
                (
                    device,
                    DeviceRating {
                        supported: cpu,
                        placement: BTreeMap::from([("main".into(), device)]),
                        first_audio_p95_s: cpu.then_some(1.0),
                        rtf_1x: cpu.then_some(0.1),
                        rtf_2x: cpu.then_some(0.2),
                        load_s_cold: cpu.then_some(1.0),
                        load_s_cached: cpu.then_some(1.0),
                        speed_score: cpu.then_some(5.0),
                        recommended: cpu,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        TtsManager::new(super::super::manager::ManagerConfig {
            root: "unused-test-root".into(),
            binary: "unused-test-worker".into(),
            cache: "unused-test-cache".into(),
            home: "unused-test-home".into(),
            ratings: Ratings {
                schema_version: 1,
                metadata: BTreeMap::new(),
                models: ["pocket-en", "supertonic-3", "kokoro-82m-int8"]
                    .into_iter()
                    .map(|id| {
                        (
                            id.into(),
                            ModelRating {
                                quality: 9,
                                devices: devices.clone(),
                            },
                        )
                    })
                    .collect(),
            },
            available: vec![handy_tts_core::device::Device::Cpu],
            run_on: Default::default(),
        })
        .unwrap()
    }
    #[derive(Default)]
    struct SharedAudio {
        unavailable: bool,
        prepares: usize,
        commits: usize,
        stops: usize,
    }
    struct RecoveringAudio(Arc<Mutex<SharedAudio>>);
    impl Output for RecoveringAudio {
        type Prepared = ();
        fn names(&self) -> Result<Vec<String>, PlaybackError> {
            Ok(vec!["new".into()])
        }
        fn open(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
            self.prepare(None)
        }
        fn prepare(&mut self, _: Option<&str>) -> Result<(), PlaybackError> {
            let mut state = self.0.lock().unwrap();
            state.prepares += 1;
            if state.unavailable {
                Err(PlaybackError::Unavailable)
            } else {
                Ok(())
            }
        }
        fn commit(&mut self, _: ()) {
            self.0.lock().unwrap().commits += 1;
        }
        fn play(&mut self, _: Pcm, _: u32) -> Result<(), PlaybackError> {
            Ok(())
        }
        fn poll(&mut self) -> Poll {
            Poll::Playing
        }
        fn pause(&mut self) {}
        fn resume(&mut self) {}
        fn stop(&mut self) {
            self.0.lock().unwrap().stops += 1;
        }
    }
    fn actor(audio: Arc<Mutex<SharedAudio>>) -> Arc<ReadAloudSession> {
        ReadAloudSession::spawn_with(
            manager(),
            Engine,
            move || RecoveringAudio(audio.clone()),
            selection(),
            None,
            Arc::new(|_| {}),
        )
    }
    fn wait_for_state(reading: &ReadAloudSession, state: PlaybackState) -> SessionView {
        for _ in 0..100 {
            let view = reading.snapshot();
            if view.snapshot.state == state {
                return view;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("session did not reach {state:?}");
    }
    #[test]
    fn fix1_idle_session_acknowledges_dictation_stop_and_retries_output_after_failure() {
        let audio = Arc::new(Mutex::new(SharedAudio {
            unavailable: true,
            ..Default::default()
        }));
        let reading = actor(audio.clone());
        reading.stop_and_wait().unwrap();
        assert_eq!(
            audio.lock().unwrap().prepares,
            0,
            "idle dictation needs no output"
        );
        reading
            .send(SessionCommand::Replace {
                text: "Retained document.".into(),
                autoplay: true,
            })
            .unwrap();
        reading.stop_and_wait().unwrap();
        assert!(audio.lock().unwrap().prepares > 0);
        audio.lock().unwrap().unavailable = false;
        reading.send(SessionCommand::Play).unwrap();
        reading.pause_and_wait().unwrap();
        assert_eq!(audio.lock().unwrap().commits, 1);
        assert_eq!(reading.snapshot().text, "Retained document.");
        assert!(reading.shutdown());
    }
    #[test]
    fn fix1_selection_command_returns_prepare_error_and_never_changes_saved_selection() {
        let audio = Arc::new(Mutex::new(SharedAudio::default()));
        let reading = actor(audio.clone());
        reading
            .send(SessionCommand::Replace {
                text: "Retain me.".into(),
                autoplay: true,
            })
            .unwrap();
        reading.pause_and_wait().unwrap();
        let before = {
            let state = audio.lock().unwrap();
            (state.commits, state.stops)
        };
        audio.lock().unwrap().unavailable = true;
        let saved = Arc::new(Mutex::new(selection()));
        let storage = saved.clone();
        let mut chosen = selection();
        chosen.output_device = Some("new".into());
        assert!(reading
            .select_and_wait(
                chosen,
                true,
                Box::new(move |selection| {
                    *storage.lock().unwrap() = selection.clone();
                    Ok(())
                })
            )
            .is_err());
        assert_eq!(*saved.lock().unwrap(), selection());
        let state = audio.lock().unwrap();
        assert_eq!((state.commits, state.stops), before);
        drop(state);
        assert!(reading.shutdown());
    }
    #[test]
    fn fix1_service_preview_pause_keeps_the_actual_reading_document_and_resume_position() {
        let reading = actor(Arc::new(Mutex::new(SharedAudio::default())));
        let document = "The owner's original reading document.";
        reading
            .send(SessionCommand::Replace {
                text: document.into(),
                autoplay: true,
            })
            .unwrap();
        let before = wait_for_state(&reading, PlaybackState::Buffering).snapshot;
        let coordinator = handy_tts_core::preview::PreviewCoordinator::default();
        let preview = actor(Arc::new(Mutex::new(SharedAudio::default())));
        let mut cancelled = None;
        super::super::service::preview_with(
            reading.as_ref(),
            &coordinator,
            Arc::new(|| Ok(())),
            |flag| {
                cancelled = Some(flag);
                preview
                    .send(SessionCommand::Replace {
                        text: "Independent preview text.".into(),
                        autoplay: true,
                    })
                    .unwrap();
            },
        )
        .unwrap();
        let paused = wait_for_state(&reading, PlaybackState::Paused);
        assert_eq!(paused.text, document);
        assert_eq!(paused.snapshot.epoch, before.epoch);
        assert_eq!(paused.snapshot.source_start, before.source_start);
        preview.pause_and_wait().unwrap();
        assert_eq!(reading.snapshot().text, document);
        coordinator
            .reading_command(|| {
                reading
                    .send(SessionCommand::Play)
                    .map_err(|error| error.to_string())
            })
            .unwrap();
        assert!(cancelled.unwrap().load(Ordering::Acquire));
        assert_eq!(
            wait_for_state(&reading, PlaybackState::Buffering).text,
            document
        );
        assert!(preview.shutdown());
        assert!(reading.shutdown());
    }
    #[test]
    fn fix1_selection_prepare_failure_acknowledges_error_without_persisting_or_stopping() {
        let old = selection();
        let saved = Arc::new(Mutex::new(old.clone()));
        let mut session = Session::new(
            "The owner's original reading document.",
            old.clone(),
            Engine,
            Audio::default(),
            None,
        )
        .unwrap();
        session.play().unwrap();
        session
            .playback
            .play(
                7,
                Pcm {
                    id: 7,
                    sample_rate: 10,
                    samples: vec![0.1; 10],
                },
                0,
            )
            .unwrap();
        session.playback.output.fail_prepare = true;
        let before = (
            session.queue.epoch,
            session.playback.output.stops,
            session.playback.output.commits,
        );
        let mut chosen = old.clone();
        chosen.output_device = Some("new".into());
        let storage = saved.clone();
        let (done, wait) = mpsc::channel();
        assert!(!apply_selection(
            &mut session,
            chosen.clone(),
            true,
            Box::new(move |value| {
                *storage.lock().unwrap() = value.clone();
                Ok(())
            }),
            done
        ));
        assert!(
            wait.recv().unwrap().is_err(),
            "failed preparation must reach the command caller"
        );
        assert_eq!(*saved.lock().unwrap(), old);
        assert_eq!(
            before,
            (
                session.queue.epoch,
                session.playback.output.stops,
                session.playback.output.commits
            )
        );
        assert_eq!(session.playback.current_index(), Some(7));
        assert!(session.playback.output.active);
        session.playback.output.fail_prepare = false;
        let storage = saved.clone();
        let (done, wait) = mpsc::channel();
        assert!(apply_selection(
            &mut session,
            chosen.clone(),
            true,
            Box::new(move |value| {
                *storage.lock().unwrap() = value.clone();
                Ok(())
            }),
            done
        ));
        assert_eq!(wait.recv().unwrap(), Ok(()));
        assert_eq!(*saved.lock().unwrap(), chosen);
    }
}
