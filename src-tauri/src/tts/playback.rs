use cpal::traits::{DeviceTrait, HostTrait};
use handy_tts_core::{
    playback::{Output, PlaybackError, Poll},
    protocol::Pcm,
};
use rodio::{buffer::SamplesBuffer, OutputStream, OutputStreamBuilder, Sink};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[derive(Default)]
pub struct RodioOutput {
    selected: Option<String>,
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    lost: Arc<AtomicBool>,
}
pub struct PreparedStream {
    selected: Option<String>,
    stream: OutputStream,
    sink: Sink,
    lost: Arc<AtomicBool>,
}
fn reset_sink(sink: &mut Option<Sink>, mixer: Option<&rodio::mixer::Mixer>) {
    if let Some(old) = sink.take() {
        old.stop();
    }
    *sink = mixer.map(Sink::connect_new);
}
impl Output for RodioOutput {
    type Prepared = PreparedStream;
    fn names(&self) -> Result<Vec<String>, PlaybackError> {
        crate::audio_toolkit::get_cpal_host()
            .output_devices()
            .map_err(|_| PlaybackError::Unavailable)?
            .map(|d| d.name().map_err(|_| PlaybackError::Unavailable))
            .collect()
    }
    fn open(&mut self, name: Option<&str>) -> Result<(), PlaybackError> {
        if self.stream.is_none() {
            self.selected = name.map(str::to_owned);
            return Ok(());
        }
        let prepared = self.prepare(name)?;
        self.commit(prepared);
        Ok(())
    }
    fn prepare(&mut self, name: Option<&str>) -> Result<Self::Prepared, PlaybackError> {
        Self::prepare_stream(name)
    }
    fn commit(&mut self, prepared: Self::Prepared) {
        self.stop();
        self.selected = prepared.selected;
        self.sink = Some(prepared.sink);
        self.stream = Some(prepared.stream);
        self.lost = prepared.lost;
    }
    fn play(&mut self, pcm: Pcm, pause_ms: u32) -> Result<(), PlaybackError> {
        if self.stream.is_none() {
            self.open_stream()?;
        }
        let sink = self.sink.as_ref().ok_or(PlaybackError::Unavailable)?;
        sink.append(SamplesBuffer::new(1, pcm.sample_rate, pcm.samples));
        if pause_ms > 0 {
            sink.append(SamplesBuffer::new(
                1,
                pcm.sample_rate,
                vec![0.0; (u64::from(pcm.sample_rate) * u64::from(pause_ms) / 1000) as usize],
            ));
        }
        Ok(())
    }
    fn poll(&mut self) -> Poll {
        if self.lost.swap(false, Ordering::AcqRel) {
            Poll::DeviceLost
        } else if self.sink.as_ref().is_some_and(Sink::empty) {
            Poll::Finished
        } else {
            Poll::Playing
        }
    }
    fn pause(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
        }
    }
    fn resume(&mut self) {
        if let Some(sink) = &self.sink {
            sink.play();
        }
    }
    fn stop(&mut self) {
        reset_sink(
            &mut self.sink,
            self.stream.as_ref().map(OutputStream::mixer),
        );
    }
}

impl RodioOutput {
    fn open_stream(&mut self) -> Result<(), PlaybackError> {
        let prepared = Self::prepare_stream(self.selected.as_deref())?;
        self.commit(prepared);
        Ok(())
    }
    fn prepare_stream(name: Option<&str>) -> Result<PreparedStream, PlaybackError> {
        let builder = if let Some(name) = name {
            let host = crate::audio_toolkit::get_cpal_host();
            let mut devices = host
                .output_devices()
                .map_err(|_| PlaybackError::Unavailable)?;
            let device = devices
                .find(|d| d.name().is_ok_and(|n| n == name))
                .ok_or(PlaybackError::Unavailable)?;
            OutputStreamBuilder::from_device(device).map_err(|_| PlaybackError::Unavailable)?
        } else {
            OutputStreamBuilder::from_default_device().map_err(|_| PlaybackError::Unavailable)?
        };
        let lost = Arc::new(AtomicBool::new(false));
        let signal = lost.clone();
        let stream = builder
            .with_error_callback(move |_| signal.store(true, Ordering::Release))
            .open_stream()
            .map_err(|_| PlaybackError::Unavailable)?;
        let sink = Sink::connect_new(stream.mixer());
        Ok(PreparedStream {
            selected: name.map(str::to_owned),
            stream,
            sink,
            lost,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_from_paused_resets_sink_for_audible_next_play() {
        let (mixer, _output) = rodio::mixer::mixer(1, 24000);
        let mut sink = Some(Sink::connect_new(&mixer));
        sink.as_ref().unwrap().pause();
        reset_sink(&mut sink, Some(&mixer));
        let sink = sink.unwrap();
        assert!(!sink.is_paused());
        sink.append(SamplesBuffer::new(1, 24000, vec![0.25; 240]));
        assert!(!sink.empty());
        assert!(!sink.is_paused());
    }
}
