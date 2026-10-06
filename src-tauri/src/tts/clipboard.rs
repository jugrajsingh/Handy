use super::TtsError;
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Drains owned child output with a two-second deadline.
pub fn capture_command(command: Command) -> Result<String, TtsError> {
    capture_command_with_timeout(command, Duration::from_secs(2))
}
/// Drains stdout while waiting, then kills and reaps a child that misses its deadline.
pub fn capture_command_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<String, TtsError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    crate::process_supervisor::set_parent_death_signal(&mut command);
    let mut child = crate::process_supervisor::spawn_from_supervisor(command)
        .map_err(|_| TtsError::Clipboard)?;
    let Some(mut output) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(TtsError::Clipboard);
    };
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        output.read_to_string(&mut text).map(|_| text)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = reader
        .join()
        .map_err(|_| TtsError::Clipboard)?
        .map_err(|_| TtsError::Clipboard)?;
    if status.is_some_and(|s| s.success()) {
        Ok(text)
    } else {
        Err(TtsError::Clipboard)
    }
}
/// Captures Wayland plain text; the caller runs this blocking operation off the UI thread.
pub fn capture_wayland() -> Result<String, TtsError> {
    let mut command = Command::new("wl-paste");
    command.args(["--no-newline", "--type", "text/plain"]);
    capture_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hanging_device_discovery_is_killed_and_reaped_with_an_explicit_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 3"]);
        let began = Instant::now();
        assert_eq!(
            capture_command_with_timeout(command, Duration::from_millis(30)),
            Err(TtsError::Clipboard)
        );
        assert!(began.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn capture_is_owned_background_work_and_drains_large_output_without_pipe_deadlock() {
        let calling = std::thread::current().id();
        let result = std::thread::spawn(move || {
            assert_ne!(calling, std::thread::current().id());
            let mut command = Command::new("sh");
            command.args(["-c", "printf 'original clipboard'; printf '%080000d' 1"]);
            capture_command(command).unwrap()
        })
        .join()
        .unwrap();
        assert!(result.starts_with("original clipboard"));
        assert!(result.len() > 65536);
    }
    #[test]
    fn failed_capture_is_typed_and_does_not_read_owner_clipboard() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 4"]);
        assert_eq!(capture_command(command), Err(TtsError::Clipboard));
    }
}
