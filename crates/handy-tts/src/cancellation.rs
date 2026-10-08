use anyhow::Result;
use handy_tts_core::device::Device;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cancellation state shared only by the graphs of one synthesis lane.
#[derive(Clone)]
pub struct Cancellation {
    pub flag: std::sync::Arc<AtomicBool>,
    pub options: std::sync::Arc<ort::session::RunOptions>,
}
impl Cancellation {
    /// Creates a flag and ORT options for one lane.
    pub fn new() -> Result<Self> {
        Ok(Self {
            flag: std::sync::Arc::new(AtomicBool::new(false)),
            options: std::sync::Arc::new(
                ort::session::RunOptions::new()
                    .map_err(|error| anyhow::anyhow!("create cancellable ORT options: {error}"))?,
            ),
        })
    }
    /// Marks this lane cancelled and terminates its ORT graph runs.
    pub fn cancel(&self) -> Result<()> {
        self.flag.store(true, Ordering::Release);
        self.options
            .terminate()
            .map_err(|error| anyhow::anyhow!("terminate lane inference: {error}"))
    }
    /// Resets the lane after its previous request has settled.
    pub fn reset(&self) -> Result<()> {
        self.options
            .unterminate()
            .map_err(|error| anyhow::anyhow!("reset lane inference cancellation: {error}"))?;
        self.flag.store(false, Ordering::Release);
        Ok(())
    }
}
pub enum Wait {
    Ready,
    Pending,
}
pub trait AsyncRequest {
    fn start(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<Wait>;
    fn cancel(&mut self) -> Result<()>;
}

#[derive(Debug, PartialEq, Eq)]
enum Strategy {
    Cancel,
    Drain,
}

fn strategy(device: Device) -> Strategy {
    match device {
        Device::Cpu | Device::Gpu => Strategy::Cancel,
        Device::Npu => Strategy::Drain,
    }
}

fn infer_cancelled() -> anyhow::Error {
    openvino::InferenceError {
        kind: openvino::InferenceErrorKind::InferCancelled,
        message: None,
    }
    .into()
}

/// Drives one request, draining cancelled NPU inference before discarding its output.
pub fn drive(
    request: &mut impl AsyncRequest,
    cancelled: &AtomicBool,
    device: Device,
) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        return Err(infer_cancelled());
    }
    request.start()?;
    loop {
        if cancelled.load(Ordering::Acquire) {
            if strategy(device) == Strategy::Cancel {
                request.cancel()?;
            }
            while matches!(request.wait(), Ok(Wait::Pending)) {}
            return Err(infer_cancelled());
        }
        match request.wait()? {
            Wait::Ready => {
                if cancelled.load(Ordering::Acquire) {
                    return Err(infer_cancelled());
                }
                return Ok(());
            }
            Wait::Pending => {}
        }
    }
}
impl AsyncRequest for openvino::InferRequest {
    fn start(&mut self) -> Result<()> {
        Ok(self.infer_async()?)
    }
    fn wait(&mut self) -> Result<Wait> {
        match openvino::InferRequest::wait(self, 10) {
            Ok(()) => Ok(Wait::Ready),
            Err(error) if error.kind == openvino::InferenceErrorKind::ResultNotReady => {
                Ok(Wait::Pending)
            }
            Err(error) => Err(error.into()),
        }
    }
    fn cancel(&mut self) -> Result<()> {
        Ok(openvino::InferRequest::cancel(self)?)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct CpuFixture(std::path::PathBuf);
    impl CpuFixture {
        fn new() -> Self {
            let scratch =
                std::path::PathBuf::from(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap());
            std::fs::create_dir_all(&scratch).unwrap();
            let fixture = Self(scratch.join(format!("s11-cpu-lanes-{}.onnx", std::process::id())));
            // ONNX IR 8, opset 13: float32 Add(x, x), input and output shape [2].
            std::fs::write(
                &fixture.0,
                [
                    8, 8, 58, 61, 10, 14, 10, 1, 120, 10, 1, 120, 18, 1, 121, 34, 3, 65, 100, 100,
                    18, 9, 108, 97, 110, 101, 45, 116, 101, 115, 116, 90, 15, 10, 1, 120, 18, 10,
                    10, 8, 8, 1, 18, 4, 10, 2, 8, 2, 98, 15, 10, 1, 121, 18, 10, 10, 8, 8, 1, 18,
                    4, 10, 2, 8, 2, 66, 2, 16, 13,
                ],
            )
            .unwrap();
            fixture
        }
    }
    impl Drop for CpuFixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).unwrap();
        }
    }
    fn run_cpu(session: &mut crate::graph::Session, lane: &Cancellation) -> Result<Vec<f32>> {
        let input = ort::value::Tensor::from_array(([2], vec![1.25f32, -2.0]))?;
        let output = session.run_with_options(ort::inputs!["x" => input], &lane.options)?;
        Ok(output.at(0)?.try_extract_tensor::<f32>()?.1.to_vec())
    }

    #[test]
    fn lane_run_options_and_flags_are_independent_and_reusable() {
        let one = Cancellation::new().unwrap();
        let two = Cancellation::new().unwrap();
        assert!(!std::sync::Arc::ptr_eq(&one.flag, &two.flag));
        assert!(!std::sync::Arc::ptr_eq(&one.options, &two.options));
        let fixture = CpuFixture::new();
        let mut first = crate::graph::session_for_lane(&fixture.0, 1, &one).unwrap();
        let mut second = crate::graph::session_for_lane(&fixture.0, 1, &two).unwrap();
        assert!(matches!(first, crate::graph::Session::Ort(_)));
        assert!(matches!(second, crate::graph::Session::Ort(_)));
        assert_eq!(run_cpu(&mut first, &one).unwrap(), [2.5, -4.0]);
        assert_eq!(run_cpu(&mut second, &two).unwrap(), [2.5, -4.0]);
        two.cancel().unwrap();
        let cancelled = run_cpu(&mut second, &two);
        assert!(
            cancelled.is_err(),
            "cancelled CPU inference must stop through ORT RunOptions"
        );
        assert!(cancelled.unwrap_err().is::<ort::Error>());
        assert_eq!(run_cpu(&mut first, &one).unwrap(), [2.5, -4.0]);
        assert!(two.flag.load(Ordering::Acquire));
        assert!(!one.flag.load(Ordering::Acquire));
        one.reset().unwrap();
        assert!(two.flag.load(Ordering::Acquire));
        assert!(
            run_cpu(&mut second, &two).is_err(),
            "resetting lane one must leave lane two terminated"
        );
        two.reset().unwrap();
        assert!(!two.flag.load(Ordering::Acquire));
        let reused = run_cpu(&mut second, &two);
        assert!(
            reused.is_ok(),
            "a reused CPU lane must complete after reset: {reused:?}"
        );
        assert_eq!(reused.unwrap(), [2.5, -4.0]);
    }

    struct Fake<'a> {
        flag: &'a AtomicBool,
        starts: usize,
        waits: usize,
        cancels: usize,
    }
    impl AsyncRequest for Fake<'_> {
        fn start(&mut self) -> Result<()> {
            self.starts += 1;
            Ok(())
        }
        fn wait(&mut self) -> Result<Wait> {
            self.waits += 1;
            if self.waits == 1 {
                self.flag.store(true, Ordering::Release);
                Ok(Wait::Pending)
            } else {
                Ok(Wait::Ready)
            }
        }
        fn cancel(&mut self) -> Result<()> {
            self.cancels += 1;
            Ok(())
        }
    }
    #[test]
    fn cancellation_reaches_backend_and_drains_request_before_buffers_are_released() {
        let flag = AtomicBool::new(false);
        let mut fake = Fake {
            flag: &flag,
            starts: 0,
            waits: 0,
            cancels: 0,
        };
        assert!(drive(&mut fake, &flag, Device::Gpu).is_err());
        assert_eq!((fake.starts, fake.waits, fake.cancels), (1, 2, 1));
        assert!(drive(&mut fake, &flag, Device::Gpu).is_err());
        assert_eq!(fake.starts, 1);
    }

    #[test]
    fn npu_cancellation_drains_and_discards_without_backend_cancel() {
        let flag = AtomicBool::new(false);
        let mut fake = Fake {
            flag: &flag,
            starts: 0,
            waits: 0,
            cancels: 0,
        };
        let result = drive(&mut fake, &flag, Device::Npu);
        assert_eq!((fake.starts, fake.waits, fake.cancels), (1, 2, 0));
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<openvino::InferenceError>()
                .unwrap()
                .kind,
            openvino::InferenceErrorKind::InferCancelled
        );
    }

    #[test]
    fn device_selects_cancellation_strategy_without_matching_errors() {
        assert_eq!(strategy(Device::Cpu), Strategy::Cancel);
        assert_eq!(strategy(Device::Gpu), Strategy::Cancel);
        assert_eq!(strategy(Device::Npu), Strategy::Drain);
    }
}
