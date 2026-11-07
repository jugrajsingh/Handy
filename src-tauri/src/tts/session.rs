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
type CompletionWake = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

type MemoryProbe = Arc<dyn Fn() -> Result<u64, TtsError> + Send + Sync>;
type ActiveRequests = Arc<Mutex<std::collections::BTreeMap<(u64, usize), u64>>>;
type CancelledTickets = Arc<Mutex<std::collections::BTreeSet<(u64, usize)>>>;

pub(super) struct ManagerWorker<B: super::backend::Transport = super::backend::WorkerBackend> {
    manager: Arc<TtsManager<B>>,
    tx: mpsc::SyncSender<Work>,
    rx: Arc<Mutex<mpsc::Receiver<Completion>>>,
    next: Arc<AtomicU64>,
    active: ActiveRequests,
    cancelled: CancelledTickets,
    wake: CompletionWake,
    memory: MemoryProbe,
}
impl<B: super::backend::Transport> Clone for ManagerWorker<B> {
    fn clone(&self) -> Self {
        Self {
            manager: self.manager.clone(),
            tx: self.tx.clone(),
            rx: self.rx.clone(),
            next: self.next.clone(),
            active: self.active.clone(),
            cancelled: self.cancelled.clone(),
            wake: self.wake.clone(),
            memory: self.memory.clone(),
        }
    }
}
impl<B: super::backend::Transport> ManagerWorker<B> {
    pub(super) fn new(manager: Arc<TtsManager<B>>) -> Self {
        Self::with_memory_probe(manager, Arc::new(super::memory_available))
    }
    fn with_memory_probe(manager: Arc<TtsManager<B>>, memory: MemoryProbe) -> Self {
        let lanes = manager
            .configuration()
            .ratings
            .models
            .values()
            .flat_map(|model| model.devices.values())
            .map(|rating| rating.lanes.best_lanes.clamp(1, 3))
            .max()
            .unwrap_or(1);
        let (tx, jobs) = mpsc::sync_channel::<Work>(lanes);
        let jobs = Arc::new(Mutex::new(jobs));
        let (results, rx) = mpsc::sync_channel(lanes);
        let active: ActiveRequests = Arc::default();
        let cancelled: CancelledTickets = Arc::default();
        let wake: CompletionWake = Arc::new(Mutex::new(None));
        for _ in 0..3 {
            let jobs = jobs.clone();
            let results = results.clone();
            let notify = wake.clone();
            let engine = manager.clone();
            let active = active.clone();
            let cancelled = cancelled.clone();
            std::thread::spawn(move || loop {
                let job = jobs.lock().unwrap_or_else(|p| p.into_inner()).recv();
                let Ok(job) = job else {
                    break;
                };
                let start = Instant::now();
                let result = engine
                    .synthesize(&job.selection, job.id, &job.text)
                    .map_err(|e| e.to_string());
                if let Err(error) = &result {
                    log::error!("TTS synthesis failed: {}: {error}", job.selection.model_id);
                }
                let key = (job.ticket.epoch, job.ticket.index);
                active
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
                cancelled
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
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
                let wake = notify.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if let Some(wake) = wake {
                    wake();
                }
            });
        }
        Self {
            manager,
            tx,
            rx: Arc::new(Mutex::new(rx)),
            next: Arc::new(AtomicU64::new(1)),
            active,
            cancelled,
            wake,
            memory,
        }
    }
}
impl<B: super::backend::Transport> Worker for ManagerWorker<B> {
    fn request(&mut self, ticket: Ticket, text: &str, selection: &Selection) -> Result<(), String> {
        if self.try_request(ticket, text, selection)? {
            Ok(())
        } else {
            Err("tts.errors.busy".into())
        }
    }
    fn try_request(
        &mut self,
        ticket: Ticket,
        text: &str,
        selection: &Selection,
    ) -> Result<bool, String> {
        let key = (ticket.epoch, ticket.index);
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if active.contains_key(&key) {
            return Err("tts.errors.busy".into());
        }
        if active.len() >= self.manager.available_lanes() {
            return Ok(false);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        active.insert(key, id);
        match self.tx.try_send(Work {
            ticket,
            text: text.into(),
            selection: selection.clone(),
            id,
        }) {
            Ok(()) => Ok(true),
            Err(mpsc::TrySendError::Full(_)) => {
                active.remove(&key);
                Ok(false)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                active.remove(&key);
                Err("tts.errors.worker".into())
            }
        }
    }
    fn retain_requests(&mut self, tickets: &[Ticket]) {
        let obsolete: Vec<_> = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .map(|&(epoch, index)| Ticket { epoch, index })
            .filter(|ticket| !tickets.contains(ticket))
            .collect();
        for ticket in obsolete {
            self.cancel(ticket);
        }
    }
    fn poll(&mut self) -> Option<Completion> {
        self.rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_recv()
            .ok()
    }
    fn cancel(&mut self, ticket: Ticket) {
        let key = (ticket.epoch, ticket.index);
        let active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(&id) = active.get(&key) {
            if self
                .cancelled
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(key)
            {
                self.manager.cancel(id);
            }
        }
    }
    fn available_lanes(&mut self) -> Option<usize> {
        if !(self.memory)().is_ok_and(|bytes| bytes >= handy_tts_core::memory::LANE_RESERVE_BYTES) {
            self.manager.lower_lanes();
        }
        let cancelled = self
            .cancelled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len();
        Some(self.manager.available_lanes().saturating_sub(cancelled))
    }
    fn selection_changed(&mut self) {
        self.manager.selected();
    }
    fn set_completion_waker(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.wake.lock().unwrap_or_else(|p| p.into_inner()) = Some(wake);
    }
    fn retry_failed_selection(&mut self) {
        self.manager.retry_failed_selection();
    }
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct SessionView {
    pub client_revision: Option<u64>,
    pub document_id: u64,
    pub text: String,
    pub visible: bool,
    pub snapshot: SessionSnapshot,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct SessionEvent {
    pub client_revision: Option<u64>,
    pub document_id: u64,
    pub text: Option<String>,
    pub visible: bool,
    pub snapshot: SessionSnapshot,
}
impl SessionView {
    fn event(&self, text: Option<String>) -> SessionEvent {
        SessionEvent {
            client_revision: self.client_revision,
            document_id: self.document_id,
            text,
            visible: self.visible,
            snapshot: self.snapshot.clone(),
        }
    }
}
pub enum SessionCommand {
    WorkerReady,
    Interrupt(mpsc::Sender<()>),
    PauseAcknowledged(mpsc::Sender<()>),
    Replace {
        client_revision: Option<u64>,
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
fn apply_selection<W: Worker, O: handy_tts_core::playback::segment::Output>(
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
fn reading_active(state: PlaybackState) -> bool {
    matches!(
        state,
        PlaybackState::Playing
            | PlaybackState::Paused
            | PlaybackState::Buffering
            | PlaybackState::Starting
    )
}
#[derive(Default)]
struct InboxState {
    commands: std::collections::VecDeque<SessionCommand>,
    close: bool,
    stop: bool,
    shutdown: bool,
}
#[derive(Default)]
struct SessionInbox {
    state: Mutex<InboxState>,
    wake: std::sync::Condvar,
}
impl SessionInbox {
    fn send(&self, command: SessionCommand) -> Result<(), TtsError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(
            command,
            SessionCommand::Stop
                | SessionCommand::Close
                | SessionCommand::Shutdown
                | SessionCommand::Interrupt(_)
        ) {
            let close = matches!(command, SessionCommand::Close | SessionCommand::Shutdown);
            state.commands.retain_mut(|queued| match queued {
                SessionCommand::Replace { autoplay, .. } => {
                    *autoplay = false;
                    !close
                }
                SessionCommand::Play | SessionCommand::Pause | SessionCommand::WorkerReady => false,
                _ => true,
            });
        }
        match command {
            SessionCommand::Close => state.close = true,
            SessionCommand::Stop => state.stop = true,
            SessionCommand::Shutdown => state.shutdown = true,
            command => {
                if matches!(command, SessionCommand::Interrupt(_)) {
                    state.stop = true;
                }
                if matches!(command, SessionCommand::WorkerReady)
                    && state
                        .commands
                        .iter()
                        .any(|c| matches!(c, SessionCommand::WorkerReady))
                {
                    return Ok(());
                }
                if state.commands.len() >= 16 {
                    self.wake.notify_one();
                    return Err(TtsError::Generic);
                }
                state.commands.push_back(command);
            }
        }
        self.wake.notify_one();
        Ok(())
    }
    fn recv(&self, timeout: Option<Duration>) -> Result<SessionCommand, mpsc::RecvTimeoutError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let empty =
            |s: &mut InboxState| !s.close && !s.stop && !s.shutdown && s.commands.is_empty();
        state = if let Some(timeout) = timeout {
            self.wake
                .wait_timeout_while(state, timeout, empty)
                .unwrap_or_else(|p| p.into_inner())
                .0
        } else {
            self.wake
                .wait_while(state, empty)
                .unwrap_or_else(|p| p.into_inner())
        };
        if std::mem::take(&mut state.shutdown) {
            return Ok(SessionCommand::Shutdown);
        }
        if std::mem::take(&mut state.close) {
            return Ok(SessionCommand::Close);
        }
        if std::mem::take(&mut state.stop) {
            return Ok(SessionCommand::Stop);
        }
        state
            .commands
            .pop_front()
            .ok_or(mpsc::RecvTimeoutError::Timeout)
    }
}
pub struct ReadAloudSession {
    tx: Arc<SessionInbox>,
    view: Arc<Mutex<SessionView>>,
    pub busy: Arc<AtomicBool>,
    manager: Arc<TtsManager>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl ReadAloudSession {
    pub fn spawn(
        manager: Arc<TtsManager>,
        selection: Selection,
        handy_output: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> Arc<Self> {
        let worker = ManagerWorker::new(manager.clone());
        Self::spawn_with_output_settings(
            manager,
            worker,
            super::playback::RodioOutput::default,
            selection,
            handy_output,
            emit,
        )
    }
    #[cfg(test)]
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
        O: handy_tts_core::playback::segment::Output + Send + 'static,
    {
        Self::spawn_with_output_settings(
            manager,
            worker,
            output,
            selection,
            Arc::new(move || handy_output.clone()),
            emit,
        )
    }
    fn spawn_with_output_settings<W, O>(
        manager: Arc<TtsManager>,
        mut worker: W,
        output: impl Fn() -> O + Send + 'static,
        selection: Selection,
        handy_output: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> Arc<Self>
    where
        W: Worker + Clone + Send + 'static,
        O: handy_tts_core::playback::segment::Output + Send + 'static,
    {
        let tx = Arc::new(SessionInbox::default());
        let rx = tx.clone();
        let notify = tx.clone();
        worker.set_completion_waker(Arc::new(move || {
            let _ = notify.send(SessionCommand::WorkerReady);
        }));
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
            client_revision: None,
            document_id: 0,
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
            let mut client_revision = None;
            let mut document_id = 0;
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
                    handy_output().as_deref(),
                ) {
                    Ok(session) => session,
                    Err(error) => {
                        let mut current = cached.lock().unwrap_or_else(|p| p.into_inner());
                        current.text = source.clone();
                        current.client_revision = client_revision;
                        current.document_id = document_id;
                        current.visible = visible;
                        current.snapshot.epoch = epoch;
                        current.snapshot.error = Some(error);
                        emit(current.event(Some(source.clone())));
                        break;
                    }
                };
                session.queue.epoch = epoch;
                if autoplay {
                    if let Err(error) = session
                        .set_handy_output(handy_output())
                        .and_then(|()| session.play())
                    {
                        session.fail(error);
                    }
                }
                let mut text_sent = false;
                loop {
                    let message = if !visible && !reading_active(session.queue.state) {
                        rx.recv(None)
                    } else {
                        rx.recv(Some(Duration::from_millis(20)))
                    };
                    let commanded =
                        message.is_ok() && !matches!(message, Ok(SessionCommand::WorkerReady));
                    let was_visible = visible;
                    match message {
                        Ok(SessionCommand::WorkerReady) => {}
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
                            client_revision: applied_revision,
                        }) => {
                            session.stop();
                            epoch = session.queue.epoch;
                            drop(session);
                            source = text;
                            client_revision = applied_revision;
                            if client_revision.is_none() {
                                document_id += 1;
                            }
                            autoplay = start;
                            visible = true;
                            continue 'document;
                        }
                        Ok(SessionCommand::Play) => {
                            if let Err(error) = session
                                .set_handy_output(handy_output())
                                .and_then(|()| session.play())
                            {
                                session.fail(error);
                            }
                        }
                        Ok(SessionCommand::Pause) => session.pause(),
                        Ok(SessionCommand::Stop) => session.stop(),
                        Ok(SessionCommand::Close) => {
                            session.close();
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
                    if working.swap(reading_active(snapshot.state), Ordering::AcqRel)
                        != reading_active(snapshot.state)
                    {
                        status_manager.wake_idle();
                    }
                    let mut current = cached.lock().unwrap_or_else(|p| p.into_inner());
                    if current.text != source {
                        current.text = source.clone();
                    }
                    current.visible = visible;
                    current.client_revision = client_revision;
                    current.document_id = document_id;
                    current.snapshot = snapshot;
                    if (visible || was_visible)
                        && (commanded || last_emit.elapsed() >= Duration::from_millis(200))
                    {
                        emit(current.event(if text_sent {
                            None
                        } else {
                            Some(source.clone())
                        }));
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
        self.tx.send(message)
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
pub(super) mod tests {
    use super::*;
    use handy_tts_core::{
        feed::Segment,
        playback::{
            segment::{Output, OutputEvent},
            PlaybackError,
        },
    };
    #[derive(Default)]
    struct Audio {
        fail_prepare: bool,
        active: bool,
        stops: usize,
        commits: usize,
        finished: Vec<usize>,
        feed: Option<handy_tts_core::feed::Feed>,
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
        fn set_sample_rate(&mut self, _: u32) -> Result<(), PlaybackError> {
            Ok(())
        }
        fn append(&mut self, segment: Segment) -> Result<(), PlaybackError> {
            self.feed
                .get_or_insert_with(|| handy_tts_core::feed::Feed::new(8000))
                .push(segment);
            self.active = true;
            Ok(())
        }
        fn events(&mut self) -> Vec<OutputEvent> {
            self.feed
                .as_mut()
                .map(|feed| {
                    feed.drain_events()
                        .into_iter()
                        .map(|event| {
                            if let handy_tts_core::feed::FeedEvent::Finished { index, .. } = event {
                                self.finished.push(index);
                            }
                            OutputEvent::from(event)
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        fn retract_unstarted(&mut self) -> Vec<Segment> {
            self.feed
                .as_mut()
                .map(|feed| feed.retract_unstarted())
                .unwrap_or_default()
        }
        fn content_samples(&self) -> u64 {
            self.feed.as_ref().map_or(0, |feed| feed.content_samples())
        }

        fn pause(&mut self) {}
        fn resume(&mut self) {}
        fn stop(&mut self) {
            self.active = false;
            self.feed = None;
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
    #[derive(Clone, Default)]
    struct LaneControl {
        cancelled: Arc<Mutex<Vec<u64>>>,
        limits: Arc<Mutex<Vec<usize>>>,
    }
    impl super::super::backend::Control for LaneControl {
        fn cancel(&self, id: u64) {
            self.cancelled.lock().unwrap().push(id);
        }
        fn kill(&self) {}
    }
    struct LaneTransport {
        control: LaneControl,
        submitted: mpsc::Sender<(
            u64,
            mpsc::Sender<Result<handy_tts_core::protocol::Pcm, TtsError>>,
        )>,
    }
    impl super::super::backend::Transport for LaneTransport {
        type Control = LaneControl;
        fn control(&self) -> LaneControl {
            self.control.clone()
        }
        fn start(
            &mut self,
            spec: &super::super::backend::LoadSpec<'_>,
        ) -> Result<super::super::backend::WorkerReady, TtsError> {
            assert_eq!(spec.lanes, 3);
            Ok(super::super::backend::WorkerReady {
                device: spec.device,
                placement: std::collections::BTreeMap::from([("main".into(), spec.device)]),
            })
        }
        fn submit(
            &mut self,
            id: u64,
            _: &str,
            _: Option<f32>,
        ) -> Result<mpsc::Receiver<Result<handy_tts_core::protocol::Pcm, TtsError>>, TtsError>
        {
            let (send, receive) = mpsc::channel();
            self.submitted.send((id, send)).unwrap();
            Ok(receive)
        }
        fn set_lanes(&mut self, lanes: usize) -> Result<(), TtsError> {
            self.control.limits.lock().unwrap().push(lanes);
            Ok(())
        }
        fn alive(&mut self) -> bool {
            true
        }
        fn stop(&mut self) {}
    }
    type LaneReply = (
        u64,
        mpsc::Sender<Result<handy_tts_core::protocol::Pcm, TtsError>>,
    );
    fn lane_worker() -> (
        ManagerWorker<LaneTransport>,
        LaneControl,
        mpsc::Receiver<LaneReply>,
    ) {
        let (submitted, replies) = mpsc::channel();
        let control = LaneControl::default();
        let mut config = manager().configuration();
        for model in config.ratings.models.values_mut() {
            let rating = model
                .devices
                .get_mut(&handy_tts_core::device::Device::Cpu)
                .unwrap();
            rating.lanes.best_lanes = 3;
            rating.lanes.base_rss_mib = Some(100.0);
            rating.lanes.lane_rss_mib = Some(100.0);
        }
        let manager = TtsManager::with_backend(
            config,
            LaneTransport {
                control: control.clone(),
                submitted,
            },
            super::super::manager::Checks {
                memory: || Ok(8 * 1024 * 1024 * 1024),
                downloaded: |_, _| Ok(true),
                verify: |_, _, _| Ok(true),
            },
        )
        .unwrap();
        manager.ensure(&selection()).unwrap();
        (
            ManagerWorker::with_memory_probe(manager, Arc::new(|| Ok(8 * 1024 * 1024 * 1024))),
            control,
            replies,
        )
    }
    fn ticket(index: usize) -> Ticket {
        Ticket { epoch: 7, index }
    }
    #[test]
    fn failed_lane_retry_waits_for_obsolete_jobs_without_busy_error() {
        let (worker, control, submitted) = lane_worker();
        let source =
            "Every sentence contains enough characters for its own speech chunk.\n\n".repeat(3);
        let mut session =
            Session::new(&source, selection(), worker, Audio::default(), None).unwrap();
        session.play().unwrap();
        session.tick(0.0).unwrap();
        let active = session.worker.active.lock().unwrap().clone();
        let mut replies = std::collections::BTreeMap::new();
        for _ in 0..3 {
            let (id, send) = submitted.recv_timeout(Duration::from_secs(3)).unwrap();
            replies.insert(id, send);
        }
        let epoch = session.queue.epoch;
        replies
            .remove(&active[&(epoch, 0)])
            .unwrap()
            .send(Err(TtsError::Protocol))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while session.queue.state != PlaybackState::Error && Instant::now() < deadline {
            session.tick(0.0).unwrap();
            std::thread::yield_now();
        }
        assert_eq!(session.queue.failed_index, Some(0));
        session.play().unwrap();
        session.tick(0.0).unwrap();
        assert_eq!(
            session.queue.failed_index,
            None,
            "retry failed: {:?}",
            session.snapshot().error
        );
        assert_eq!(session.snapshot().error, None);
        let cancelled = control.cancelled.lock().unwrap().len();
        assert_eq!(cancelled, 2);
        let (id, send) = submitted.recv_timeout(Duration::from_secs(3)).unwrap();
        let retry_epoch = session.queue.epoch;
        assert_eq!(session.worker.active.lock().unwrap()[&(retry_epoch, 0)], id);
        assert_eq!(session.queue.in_flight.len(), 1);
        assert!(submitted.try_recv().is_err());
        for (old_id, old_send) in replies {
            old_send
                .send(Ok(handy_tts_core::protocol::Pcm {
                    id: old_id,
                    sample_rate: 8000,
                    samples: vec![0.2; 8000],
                }))
                .unwrap();
        }
        send.send(Ok(handy_tts_core::protocol::Pcm {
            id,
            sample_rate: 8000,
            samples: vec![0.2; 8000],
        }))
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut retried = vec![0];
        while session.queue.state != PlaybackState::Finished && Instant::now() < deadline {
            session.tick(0.0).unwrap();
            assert_eq!(session.queue.failed_index, None);
            assert_eq!(session.snapshot().error, None);
            while let Ok((id, send)) = submitted.try_recv() {
                let index = session
                    .worker
                    .active
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(_, value)| **value == id)
                    .unwrap()
                    .0
                     .1;
                retried.push(index);
                send.send(Ok(handy_tts_core::protocol::Pcm {
                    id,
                    sample_rate: 8000,
                    samples: vec![0.2; 8000],
                }))
                .unwrap();
            }
            if let Some(feed) = &mut session.playback.output.feed {
                feed.pull(&mut [0.0; 16000]);
            }
            std::thread::yield_now();
        }
        assert_eq!(session.queue.state, PlaybackState::Finished);
        retried.sort_unstable();
        assert_eq!(retried, [0, 1, 2]);
        assert_eq!(session.playback.output.finished, [0, 1, 2]);
    }
    #[test]
    fn cancel_targets_only_that_ticket() {
        let (mut worker, control, submitted) = lane_worker();
        for i in 0..3 {
            worker.request(ticket(i), "sentence", &selection()).unwrap();
        }
        assert!(!worker
            .try_request(ticket(3), "sentence", &selection())
            .unwrap());
        assert_eq!(worker.active.lock().unwrap().len(), 3);
        let mut replies = Vec::new();
        for _ in 0..3 {
            replies.push(submitted.recv_timeout(Duration::from_secs(3)).unwrap());
        }
        let target = worker.active.lock().unwrap()[&(7, 1)];
        worker.cancel(ticket(1));
        assert_eq!(*control.cancelled.lock().unwrap(), [target]);
        worker.cancel(ticket(99));
        assert_eq!(*control.cancelled.lock().unwrap(), [target]);
        for (id, send) in replies {
            let _ = send.send(Ok(handy_tts_core::protocol::Pcm {
                id,
                sample_rate: 24000,
                samples: vec![id as f32],
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut completed = std::collections::BTreeMap::new();
        while completed.len() < 3 && Instant::now() < deadline {
            if let Some(result) = worker.poll() {
                completed.insert(result.ticket.index, result.result);
            }
            std::thread::yield_now();
        }
        assert_eq!(completed.len(), 3);
        assert!(completed[&0].is_ok());
        assert_eq!(completed[&1], Err(TtsError::Cancelled.to_string()));
        assert!(completed[&2].is_ok());
        assert!(worker.active.lock().unwrap().is_empty());
    }
    #[test]
    fn low_memory_lowers_lanes_before_next_request() {
        let (mut worker, control, submitted) = lane_worker();
        let probes = Arc::new(AtomicU64::new(0));
        let observed = probes.clone();
        worker.memory = Arc::new(move || {
            let n = observed.fetch_add(1, Ordering::Relaxed);
            Ok(if n == 0 {
                8 * 1024 * 1024 * 1024
            } else {
                1400 * 1024 * 1024
            })
        });
        let source =
            "Every sentence contains enough characters for its own speech chunk.\n\n".repeat(12);
        let mut session =
            Session::new(&source, selection(), worker, Audio::default(), None).unwrap();
        session.play().unwrap();
        session.tick(0.0).unwrap();
        assert_eq!(session.queue.lanes, 1);
        assert_eq!(session.queue.in_flight.len(), 1);
        assert_eq!(*control.limits.lock().unwrap(), [1]);
        assert!(probes.load(Ordering::Relaxed) >= 2);
        let (id, send) = submitted.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(submitted.try_recv().is_err());
        send.send(Ok(handy_tts_core::protocol::Pcm {
            id,
            sample_rate: 24000,
            samples: vec![0.1; 24000],
        }))
        .unwrap();
        session.stop();
    }
    #[test]
    fn dictation_start_with_three_lanes_in_flight_is_not_delayed() {
        use handy_tts_core::referee::{Activity, Effect, Event, Referee};
        let (worker, control, submitted) = lane_worker();
        let source =
            "Every sentence contains enough characters for its own speech chunk.\n\n".repeat(12);
        let mut session =
            Session::new(&source, selection(), worker, Audio::default(), None).unwrap();
        session.play().unwrap();
        session.tick(0.0).unwrap();
        assert_eq!(session.queue.in_flight.len(), 3);
        let replies: Vec<_> = (0..3)
            .map(|_| submitted.recv_timeout(Duration::from_secs(3)).unwrap())
            .collect();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            for (id, send) in replies {
                let _ = send.send(Ok(handy_tts_core::protocol::Pcm {
                    id,
                    sample_rate: 24000,
                    samples: vec![0.1; 24000],
                }));
            }
        });
        let mut referee = Referee {
            activity: Activity::Reading,
            popup_open: true,
            ..Default::default()
        };
        let mut recorder_started = false;
        let start = Instant::now();
        for effect in referee.apply(Event::DictationStart) {
            match effect {
                Effect::StopReading => session.stop(),
                Effect::StartDictation => recorder_started = true,
                Effect::ClosePopup => {}
                _ => panic!("unexpected effect"),
            }
        }
        let elapsed = start.elapsed();
        release.join().unwrap();
        assert!(recorder_started);
        assert!(
            elapsed < Duration::from_millis(50),
            "dictation start waited {elapsed:?}"
        );
        assert_eq!(control.cancelled.lock().unwrap().len(), 3);
        session.tick(1.0).unwrap();
        assert!(!session.playback.output.active);
        assert_eq!(session.queue.state, PlaybackState::Waiting);
    }
    #[test]
    fn worker_completion_notifies_actor_after_result_is_available() {
        let mut worker = ManagerWorker::new(manager());
        let (notify, receive) = mpsc::channel();
        worker.set_completion_waker(Arc::new(move || {
            notify.send(()).unwrap();
        }));
        let ticket = Ticket { epoch: 4, index: 7 };
        worker
            .request(
                ticket,
                "A complete sentence for this unavailable test model.",
                &selection(),
            )
            .unwrap();
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        let completion = worker
            .poll()
            .expect("the wake follows the queued completion");
        assert_eq!(completion.ticket, ticket);
        assert!(completion.result.is_err());
    }
    fn selection() -> Selection {
        Selection {
            model_id: "supertonic-3".into(),
            voice: "F1".into(),
            speed: 1.0,
            pitch: 0.0,
            output_device: None,
            read_code: false,
            technical: false,
            full_paths: true,
            full_links: false,
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
                        lanes: handy_tts_core::device::LaneSizing::default(),
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
        last_device: Option<String>,
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
        fn prepare(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
            let mut state = self.0.lock().unwrap();
            state.last_device = name.map(str::to_owned);
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
        fn set_sample_rate(&mut self, _: u32) -> Result<(), PlaybackError> {
            Ok(())
        }
        fn append(&mut self, _: Segment) -> Result<(), PlaybackError> {
            Ok(())
        }
        fn events(&mut self) -> Vec<OutputEvent> {
            vec![]
        }
        fn retract_unstarted(&mut self) -> Vec<Segment> {
            vec![]
        }
        fn content_samples(&self) -> u64 {
            0
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
    pub(in crate::tts) fn selection_actor() -> Arc<ReadAloudSession> {
        actor(Arc::new(Mutex::new(SharedAudio::default())))
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
    mod model_selection {
        use super::*;
        #[test]
        fn missing_model_returns_visible_error_without_persisting() {
            let reading = actor(Arc::new(Mutex::new(SharedAudio::default())));
            let result = super::super::super::commands::select_with_store(
                selection(),
                false,
                &reading,
                crate::settings::get_default_settings,
                |_| panic!("incomplete model must not persist"),
            );
            assert_eq!(result, Err("tts.errors.notDownloaded".into()));
            assert!(reading.shutdown());
        }
        #[test]
        fn settings_command_persists_downloaded_model_and_read_path_uses_it_after_restart() {
            let reading = actor(Arc::new(Mutex::new(SharedAudio::default())));
            let scratch =
                std::path::PathBuf::from(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap());
            let path = scratch.join(format!("bug1-settings-{}.json", std::process::id()));
            let persisted = path.clone();
            super::super::super::commands::select_with_store(
                selection(),
                true,
                &reading,
                crate::settings::get_default_settings,
                move |settings| {
                    std::fs::write(persisted, serde_json::to_vec(&settings).unwrap())
                        .map_err(|e| e.to_string())
                },
            )
            .unwrap();
            let stored = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let settings = crate::settings::load_settings_with(Some(stored), |_| {});
            assert_eq!(settings.tts_model_id, "supertonic-3");
            assert_eq!(
                super::super::super::service::selection(&settings),
                selection()
            );
            let (chosen, requests) = mpsc::channel();
            #[derive(Clone)]
            struct ReadEngine(mpsc::Sender<Selection>);
            impl Worker for ReadEngine {
                fn request(
                    &mut self,
                    _: Ticket,
                    _: &str,
                    selection: &Selection,
                ) -> Result<(), String> {
                    self.0.send(selection.clone()).unwrap();
                    Ok(())
                }
                fn poll(&mut self) -> Option<Completion> {
                    None
                }
                fn cancel(&mut self, _: Ticket) {}
                fn selection_changed(&mut self) {}
            }
            let restarted = ReadAloudSession::spawn_with(
                manager(),
                ReadEngine(chosen.clone()),
                Audio::default,
                super::super::super::service::selection(&settings),
                None,
                Arc::new(|_| {}),
            );
            restarted
                .send(SessionCommand::Replace {
                    text: "Read the selected model.".into(),
                    client_revision: None,
                    autoplay: true,
                })
                .unwrap();
            assert_eq!(
                requests.recv_timeout(Duration::from_secs(3)).unwrap(),
                selection()
            );
            restarted.pause_and_wait().unwrap();
            assert!(restarted.shutdown());
            assert!(reading.shutdown());
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn wbfix_full_inbox_keeps_close_and_discards_queued_autoplay() {
        let inbox = SessionInbox::default();
        for _ in 0..16 {
            inbox
                .send(SessionCommand::Replace {
                    text: "stale".into(),
                    autoplay: true,
                    client_revision: None,
                })
                .unwrap();
        }
        inbox.send(SessionCommand::Close).unwrap();
        assert!(matches!(
            inbox.recv(Some(Duration::ZERO)),
            Ok(SessionCommand::Close)
        ));
        assert!(inbox.recv(Some(Duration::ZERO)).is_err());
        inbox.send(SessionCommand::Stop).unwrap();
        assert!(matches!(
            inbox.recv(Some(Duration::ZERO)),
            Ok(SessionCommand::Stop)
        ));
    }
    #[test]
    fn wbfix_play_reads_current_handy_output_without_restarting_the_service() {
        let audio = Arc::new(Mutex::new(SharedAudio::default()));
        let capture = audio.clone();
        let device = Arc::new(Mutex::new(None));
        let setting = device.clone();
        let reading = ReadAloudSession::spawn_with_output_settings(
            manager(),
            Engine,
            move || RecoveringAudio(capture.clone()),
            selection(),
            Arc::new(move || setting.lock().unwrap().clone()),
            Arc::new(|_| {}),
        );
        reading
            .send(SessionCommand::Replace {
                text: "A complete sentence for output settings.".into(),
                autoplay: false,
                client_revision: None,
            })
            .unwrap();
        reading.pause_and_wait().unwrap();
        *device.lock().unwrap() = Some("new".into());
        reading.send(SessionCommand::Play).unwrap();
        wait_for_state(&reading, PlaybackState::Buffering);
        assert_eq!(
            audio.lock().unwrap().last_device.as_deref(),
            Some("new"),
            "Play must use live Handy settings"
        );
        assert!(reading.shutdown());
    }
    #[test]
    fn blocked_stop_acknowledgement_still_starts_recording() {
        let reading = actor(Arc::new(Mutex::new(SharedAudio::default())));
        let (entered, wait_entered) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let (done, selected) = mpsc::channel();
        reading
            .send(SessionCommand::Select {
                selection: selection(),
                downloaded: true,
                persist: Box::new(move |_| {
                    entered.send(()).unwrap();
                    wait_release.recv().unwrap();
                    Ok(())
                }),
                done,
            })
            .unwrap();
        wait_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let began = Instant::now();
        let mut started = false;
        let mut handed_off = false;
        let result = crate::actions::start_after_read_aloud(
            || {
                super::super::service::dictation_start_with(
                    || Ok(()),
                    || reading.stop_and_wait(),
                    || handed_off = true,
                )
            },
            || {
                started = true;
                true
            },
        );
        release.send(()).unwrap();
        selected
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(reading.shutdown());
        assert!(began.elapsed() >= Duration::from_millis(500));
        assert!(
            result && started && handed_off,
            "a blocked TTS stop must still hand off and start recording"
        );
    }
    #[test]
    fn failed_preview_cancellation_still_starts_recording_and_interrupts_reading() {
        let reading = actor(Arc::new(Mutex::new(SharedAudio::default())));
        let coordinator = handy_tts_core::preview::PreviewCoordinator::default();
        let cancelled = coordinator
            .begin(
                reading.as_ref(),
                Arc::new(|| Err("preview did not stop".into())),
            )
            .unwrap();
        let mut stopped = false;
        let mut handed_off = false;
        let mut started = false;
        let result = crate::actions::start_after_read_aloud(
            || {
                super::super::service::dictation_start_with(
                    || coordinator.cancel(),
                    || {
                        stopped = true;
                        reading.stop_and_wait()
                    },
                    || handed_off = true,
                )
            },
            || {
                started = true;
                true
            },
        );
        assert!(cancelled.load(Ordering::Acquire));
        assert!(
            result && stopped && handed_off && started,
            "preview failure cannot veto dictation or skip the reading interrupt"
        );
        assert!(
            crate::actions::start_after_read_aloud(|| Err(TtsError::Worker), || true),
            "recording must also ignore a service error"
        );
        assert!(reading.shutdown());
    }
    #[test]
    fn hidden_idle_session_neither_polls_nor_emits_and_resumes_on_open_and_play() {
        #[derive(Clone)]
        struct CountingEngine(Arc<std::sync::atomic::AtomicUsize>);
        impl Worker for CountingEngine {
            fn request(&mut self, _: Ticket, _: &str, _: &Selection) -> Result<(), String> {
                Ok(())
            }
            fn poll(&mut self) -> Option<Completion> {
                self.0.fetch_add(1, Ordering::AcqRel);
                None
            }
            fn cancel(&mut self, _: Ticket) {}
            fn selection_changed(&mut self) {}
        }
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (emitted, events) = mpsc::channel();
        let reading = ReadAloudSession::spawn_with(
            manager(),
            CountingEngine(polls.clone()),
            Audio::default,
            selection(),
            None,
            Arc::new(move |event| {
                emitted.send(event).unwrap();
            }),
        );
        assert!(
            events.recv_timeout(Duration::from_millis(650)).is_err(),
            "hidden idle session must not emit"
        );
        assert_eq!(
            polls.load(Ordering::Acquire),
            0,
            "hidden idle session must not poll"
        );
        reading.stop_and_wait().unwrap();
        assert!(
            events.recv_timeout(Duration::from_millis(250)).is_err(),
            "a hidden idle command must not emit state"
        );
        reading
            .send(SessionCommand::Replace {
                text: "Open again".into(),
                autoplay: false,
                client_revision: None,
            })
            .unwrap();
        assert!(events.recv_timeout(Duration::from_secs(2)).unwrap().visible);
        reading.send(SessionCommand::Close).unwrap();
        loop {
            if !events.recv_timeout(Duration::from_secs(2)).unwrap().visible {
                break;
            }
        }
        let stopped = polls.load(Ordering::Acquire);
        assert!(
            events.recv_timeout(Duration::from_millis(650)).is_err(),
            "closed idle session must stop emitting"
        );
        assert_eq!(
            polls.load(Ordering::Acquire),
            stopped,
            "closed idle session must stop polling"
        );
        reading.send(SessionCommand::Play).unwrap();
        wait_for_state(&reading, PlaybackState::Buffering);
        assert!(
            polls.load(Ordering::Acquire) > stopped,
            "Play resumes worker polling"
        );
        assert!(reading.shutdown());
    }
    #[test]
    fn replacement_revision_is_echoed_in_text_and_snapshot_events() {
        let (tx, rx) = mpsc::channel();
        let reading = ReadAloudSession::spawn_with(
            manager(),
            Engine,
            Audio::default,
            selection(),
            None,
            Arc::new(move |event| {
                tx.send(event).unwrap();
            }),
        );
        fn text_event(rx: &mpsc::Receiver<SessionEvent>) -> SessionEvent {
            for _ in 0..20 {
                let event = rx.recv_timeout(Duration::from_secs(3)).unwrap();
                if event.text.as_deref() == Some("A") {
                    return event;
                }
            }
            panic!("replacement text event was not emitted");
        }
        reading
            .send(SessionCommand::Replace {
                text: "A".into(),
                autoplay: false,
                client_revision: Some(7),
            })
            .unwrap();
        let first = text_event(&rx);
        assert_eq!(
            first.client_revision,
            Some(7),
            "text event echoes applied client revision"
        );
        assert_eq!(first.document_id, 0);
        let snapshot = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(snapshot.text.is_none());
        assert_eq!(
            snapshot.client_revision,
            Some(7),
            "snapshot event echoes applied client revision"
        );
        assert_eq!(reading.snapshot().client_revision, Some(7));
        reading
            .send(SessionCommand::Replace {
                text: "A".into(),
                autoplay: false,
                client_revision: Some(9),
            })
            .unwrap();
        let repeated = text_event(&rx);
        assert_eq!(repeated.client_revision, Some(9));
        assert_eq!(repeated.document_id, first.document_id);
        reading
            .send(SessionCommand::Replace {
                text: "A".into(),
                autoplay: false,
                client_revision: None,
            })
            .unwrap();
        let external = text_event(&rx);
        assert_eq!(external.client_revision, None);
        assert_eq!(external.document_id, first.document_id + 1);
        assert_eq!(reading.snapshot().client_revision, None);
        assert!(reading.shutdown());
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
                client_revision: None,
                text: "Retained document.".into(),
                autoplay: true,
            })
            .unwrap();
        reading.pause_and_wait().unwrap();
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
                client_revision: None,
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
                client_revision: None,
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
                        client_revision: None,
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
            .append(Segment {
                epoch: session.queue.epoch,
                index: 7,
                speech: vec![0.1; 10],
                pause: 0,
            })
            .unwrap();
        session
            .playback
            .output
            .feed
            .as_mut()
            .unwrap()
            .pull(&mut [0.0]);
        session.playback.events().unwrap();
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
