use anyhow::Result;
use handy_tts_core::device::Device;
use std::sync::atomic::{AtomicBool, Ordering};

pub static CANCELLED: AtomicBool = AtomicBool::new(false);
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
