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
/// Bounds both child exit and stdout EOF by one deadline, then kills and reaps on failure.
pub fn capture_command_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<String, TtsError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(TtsError::Clipboard)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    crate::process_supervisor::set_parent_death_signal(&mut command);
    let mut child = crate::process_supervisor::spawn_from_supervisor(command)
        .map_err(|_| TtsError::Clipboard)?;
    let result = (|| {
        let mut output = child.stdout.take().ok_or(TtsError::Clipboard)?;
        let mut text = Vec::new();
        let mut buffer = [0; 8192];
        let mut eof = false;
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return Err(TtsError::Clipboard);
            }
            let read = if eof {
                None
            } else {
                read_available(&mut output, &mut buffer).map_err(|_| TtsError::Clipboard)?
            };
            match read {
                Some(0) => eof = true,
                Some(count) => text.extend_from_slice(&buffer[..count]),
                None => {}
            }
            if status.is_none() {
                status = child.try_wait().map_err(|_| TtsError::Clipboard)?;
            }
            if let Some(status) = status {
                if !status.success() {
                    return Err(TtsError::Clipboard);
                }
                if eof {
                    return String::from_utf8(text).map_err(|_| TtsError::Clipboard);
                }
            }
            if read.is_none() || eof {
                std::thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    child.wait().map_err(|_| TtsError::Clipboard)?;
    result
}

#[cfg(unix)]
fn read_available(
    output: &mut std::process::ChildStdout,
    buffer: &mut [u8],
) -> std::io::Result<Option<usize>> {
    #[cfg(target_os = "linux")]
    use libc::{poll, pollfd as PollFd, POLLIN};
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct PollFd {
        fd: std::ffi::c_int,
        events: std::ffi::c_short,
        revents: std::ffi::c_short,
    }
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn poll(
            fds: *mut PollFd,
            nfds: std::ffi::c_uint,
            timeout: std::ffi::c_int,
        ) -> std::ffi::c_int;
    }
    #[cfg(target_os = "macos")]
    const POLLIN: std::ffi::c_short = 0x0001;

    let mut descriptor = PollFd {
        fd: output.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor holds one live, exclusively read pipe; zero timeout never waits.
    match unsafe { poll(&mut descriptor, 1, 0) } {
        -1 => {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(error)
            }
        }
        0 => Ok(None),
        _ => output.read(buffer).map(Some),
    }
}

#[cfg(windows)]
fn read_available(
    output: &mut std::process::ChildStdout,
    buffer: &mut [u8],
) -> std::io::Result<Option<usize>> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
    use windows::Win32::Foundation::ERROR_BROKEN_PIPE;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "PeekNamedPipe"]
        fn peek_named_pipe(
            pipe: *mut c_void,
            buffer: *mut c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            bytes_available: *mut u32,
            bytes_left: *mut u32,
        ) -> i32;
    }
    let mut available = 0;
    // SAFETY: the pipe is live with no other reader, and available is a valid output pointer.
    let success = unsafe {
        peek_named_pipe(
            output.as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    };
    if success == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE.0 as i32) {
            Ok(Some(0))
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Ok(None);
    }
    let count = buffer.len().min(available as usize);
    output.read(&mut buffer[..count]).map(Some)
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
    fn wrapper_waiting_for_descendant_stdout_is_bounded_by_capture_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 3 & wait"]);
        let began = Instant::now();
        assert_eq!(
            capture_command_with_timeout(command, Duration::from_millis(30)),
            Err(TtsError::Clipboard)
        );
        assert!(began.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn successful_wrapper_with_descendant_stdout_still_obeys_capture_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 3 & exit 0"]);
        let began = Instant::now();
        assert_eq!(
            capture_command_with_timeout(command, Duration::from_millis(30)),
            Err(TtsError::Clipboard)
        );
        assert!(began.elapsed() < Duration::from_secs(1));
    }
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
