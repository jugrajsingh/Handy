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
    },
    Shutdown,
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
        let worker = ManagerWorker::new(manager.clone());
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
                    super::playback::RodioOutput::default(),
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
                    let _ = session.play();
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
                            let _ = session.play();
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
                        }) => {
                            if session
                                .select(selection.clone(), downloaded, |_| Ok(()))
                                .is_ok()
                            {
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
                        && worker.manager.status() == handy_tts_core::lifecycle::State::Starting
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
