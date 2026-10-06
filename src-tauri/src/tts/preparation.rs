use super::{service::Ready, TtsManager};
use handy_tts_core::{
    catalog,
    device::{Preparation, RunOn},
    preparation::Compilation,
    selection::Selection,
};
use std::sync::{atomic::AtomicBool, Arc};
struct Job {
    manager: Arc<TtsManager>,
    selection: Selection,
}
impl Compilation for Job {
    fn run(&self, cancelled: &AtomicBool) -> Result<(), String> {
        self.manager
            .ensure_cancellable(&self.selection, cancelled)
            .map_err(|e| e.to_string())
    }
    fn snapshot(&self) -> Preparation {
        self.manager.snapshot().preparation
    }
    fn stop(&self) {
        self.manager.unload();
    }
}
pub fn prepare(ready: &Ready, model: &str, choice: RunOn) -> Result<(), String> {
    let entry = catalog::entry(model).map_err(|e| e.to_string())?;
    if !ready.manager.downloaded(model) {
        return Ok(());
    }
    let manager = ready.manager.fork().map_err(|e| e.to_string())?;
    manager
        .set_run_on(model, choice)
        .map_err(|e| e.to_string())?;
    let selection = Selection {
        model_id: model.into(),
        voice: entry.default_voice.into(),
        speed: 1.0,
        pitch: 0.0,
        output_device: None,
        read_code: false,
    };
    ready
        .preparer
        .request(model, choice, Arc::new(Job { manager, selection }));
    Ok(())
}
