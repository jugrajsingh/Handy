use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct Selection {
    pub model_id: String,
    pub voice: String,
    pub speed: f32,
    pub pitch: f32,
    pub output_device: Option<String>,
    pub read_code: bool,
}
#[derive(Debug, Error, Clone, PartialEq)]
pub enum SelectionError {
    #[error("tts.errors.unknownModel")]
    Model,
    #[error("tts.errors.invalidControls")]
    Controls,
    #[error("tts.errors.invalidVoice")]
    Voice,
    #[error("tts.errors.settings")]
    Persist,
}
pub fn maximum_speed(model_id: &str) -> Result<f32, SelectionError> {
    match model_id {
        "pocket-en" | "supertonic-3" | "kokoro-82m-int8" => Ok(2.0),
        _ => Err(SelectionError::Model),
    }
}
pub fn clamp_selection(mut selection: Selection) -> Result<Selection, SelectionError> {
    let maximum = maximum_speed(&selection.model_id)?;
    if !selection.speed.is_finite()
        || !selection.pitch.is_finite()
        || selection.speed < 0.5
        || selection.speed > 2.0
        || !(-6.0..=6.0).contains(&selection.pitch)
        || (selection.speed * 10.0 - (selection.speed * 10.0).round()).abs() > 0.001
    {
        return Err(SelectionError::Controls);
    }
    selection.speed = selection.speed.min(maximum);
    Ok(selection)
}
