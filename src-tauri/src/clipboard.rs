use crate::input::{self, EnigoState};
#[cfg(target_os = "linux")]
use crate::settings::TypingTool;
use crate::settings::{get_settings, AutoSubmitKey, ClipboardHandling, PasteMethod};
use enigo::{Direction, Enigo, Key, Keyboard};
use log::info;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::sync::{mpsc, Mutex, PoisonError};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[cfg(target_os = "linux")]
use crate::utils::{is_gnome_wayland, is_kde_wayland, is_wayland};

/// Serializes pastes: overlapping ones would restore each other's transcript as
/// the user's clipboard. Only taken by `paste`, which never runs on the main thread.
static PASTE_LOCK: Mutex<()> = Mutex::new(());

const PASTE_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(all(test, unix))]
mod wbfix3 {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn followup_xdotool_cleans_up_every_typing_outcome() {
        use std::os::unix::process::ExitStatusExt;
        for outcome in ["success", "nonzero", "killed", "spawn"] {
            let mut calls = Vec::new();
            let result = type_text_via_xdotool_with("fixture text", |cmd, input, timeout| {
                assert_eq!(cmd.get_program(), "xdotool");
                assert!(input.is_none());
                let args: Vec<_> = cmd
                    .get_args()
                    .map(|s| s.to_str().unwrap().to_owned())
                    .collect();
                calls.push(args.clone());
                if args[0] == "keyup" {
                    assert_eq!(timeout, PASTE_TOOL_TIMEOUT);
                    return Err(std::io::Error::other("cleanup failure"));
                }
                assert_eq!(args, ["type", "--clearmodifiers", "--", "fixture text"]);
                assert_eq!(timeout, typing_timeout("fixture text"));
                match outcome {
                    "spawn" => Err(std::io::Error::other("spawn failure")),
                    "killed" => paste_tool_with_timeout(
                        Command::new("sh").args(["-c", "exec sleep 30"]),
                        None,
                        Duration::from_millis(100),
                    ),
                    _ => Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(if outcome == "success" {
                            0
                        } else {
                            9 << 8
                        }),
                        stdout: Vec::new(),
                        stderr: b"typing failure".to_vec(),
                    }),
                }
            });
            assert_eq!(
                calls.len(),
                2,
                "every typing outcome must release modifiers"
            );
            assert_eq!(
                calls[1],
                [
                    "keyup",
                    "Control_L",
                    "Control_R",
                    "Shift_L",
                    "Shift_R",
                    "Alt_L",
                    "Alt_R",
                    "Super_L",
                    "Super_R"
                ]
            );
            match outcome {
                "success" => assert_eq!(result, Ok(())),
                "nonzero" => assert_eq!(result.unwrap_err(), "xdotool failed: typing failure"),
                "spawn" => assert_eq!(
                    result.unwrap_err(),
                    "Failed to execute xdotool: spawn failure"
                ),
                _ => assert_eq!(
                    result.unwrap_err(),
                    format!(
                        "Failed to execute xdotool: {}",
                        std::io::Error::from(std::io::ErrorKind::TimedOut)
                    ),
                    "cleanup must preserve the original timeout error"
                ),
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn followup_dotool_keys_use_direct_stdin() {
        use std::os::unix::process::ExitStatusExt;
        for (method, script) in [
            (PasteMethod::CtrlV, "key ctrl+v\n"),
            (PasteMethod::ShiftInsert, "key shift+insert\n"),
            (PasteMethod::CtrlShiftV, "key ctrl+shift+v\n"),
        ] {
            let result = send_key_combo_via_dotool_with(&method, |cmd, input| {
                assert_eq!(
                    cmd.get_program(),
                    "dotool",
                    "key combinations must spawn dotool directly"
                );
                assert_eq!(cmd.get_args().count(), 0);
                assert_eq!(
                    input.as_deref(),
                    Some(script.as_bytes()),
                    "dotool must receive the original keystrokes on stdin"
                );
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(9 << 8),
                    stdout: Vec::new(),
                    stderr: b"fixture failure".to_vec(),
                })
            });
            assert_eq!(result.unwrap_err(), "dotool failed: fixture failure");
        }
    }

    #[test]
    fn typing_and_watchdog_budgets_scale_with_characters() {
        for count in [0, 30, 3000] {
            let text = "語".repeat(count);
            assert_eq!(
                typing_timeout(&text),
                Duration::from_millis(30_000 + 100 * count as u64),
                "typing budget must scale by characters"
            );
            let settings = crate::settings::AppSettings::default();
            assert!(
                paste_timeout(&settings, &text) > typing_timeout(&text),
                "watchdog must cover typing and cleanup"
            );
        }
    }

    #[test]
    #[ignore = "Runs the real 330-second typing deadline; invoked explicitly for wbfix3"]
    fn long_typing_completes_and_hung_typing_expires() {
        let dir = tempfile::tempdir().unwrap();
        let delivered = dir.path().join("delivered");
        let text = "a".repeat(3000);
        let result = typing_tool(Command::new("python3").args(["-c", "import pathlib,sys,time; f=open(sys.argv[1],'w'); [(f.write(c),f.flush(),time.sleep(.04)) for c in sys.argv[2]]"]).arg(&delivered).arg(&text), None, &text).unwrap();
        assert!(result.status.success());
        assert_eq!(
            std::fs::read_to_string(&delivered).unwrap(),
            text,
            "long typing must deliver every character"
        );
        let began = std::time::Instant::now();
        let result = typing_tool(
            Command::new("sh").args(["-c", "exec sleep 600"]),
            None,
            &text,
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(began.elapsed() >= typing_timeout(&text));
        assert!(
            began.elapsed() < typing_timeout(&text) + Duration::from_secs(2),
            "hung typing must expire within scaled budget"
        );
    }

    #[test]
    fn stderr_flood_is_bounded_and_still_obeys_deadline() {
        let output = paste_tool(
            Command::new("python3").args([
                "-c",
                "import sys; sys.stderr.write('x'*200000); sys.exit(9)",
            ]),
            None,
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(9));
        assert_eq!(
            output.stderr.len(),
            MAX_PROBE_BYTES,
            "stderr must be bounded without failing the tool"
        );
        let began = std::time::Instant::now();
        let result = paste_tool_with_timeout(
            Command::new("python3").args([
                "-c",
                "import os; exec('while True: os.write(2,b\"x\"*8192)')",
            ]),
            None,
            Duration::from_millis(100),
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(began.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_parent_fixture() {
        let Some(pidfile) = std::env::var_os("HANDY_TEST_HELPER_PIDFILE") else {
            return;
        };
        let _ = paste_tool(Command::new("python3").args(["-c", "import os,pathlib,sys,time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)"]).arg(pidfile), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_helper_dies_with_parent() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("helper.pid");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clipboard::wbfix3::helper_parent_fixture",
                "--nocapture",
            ])
            .env("HANDY_TEST_HELPER_PIDFILE", &pidfile)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pidfile.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|p| p.parse::<i32>().ok());
        parent.kill().unwrap();
        parent.wait().unwrap();
        let pid = pid.expect("helper must start before parent is killed");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let stopped = loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if state
                .as_ref()
                .map_or(true, |s| s.split_whitespace().nth(2) == Some("Z"))
            {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !stopped {
            // SAFETY: this PID names the test helper which has not exited.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        assert!(stopped, "helper must die when its parent process dies");
    }

    #[test]
    fn failures_preserve_bounded_stderr() {
        let output = paste_tool(
            Command::new("sh").args(["-c", "printf 'fixture failure' >&2; exit 7"]),
            None,
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(
            output.stderr, b"fixture failure",
            "tool failure must retain stderr"
        );
    }
}

fn typing_timeout(text: &str) -> Duration {
    PASTE_TOOL_TIMEOUT.saturating_add(Duration::from_millis(
        u64::try_from(text.chars().count())
            .unwrap_or(u64::MAX)
            .saturating_mul(100),
    ))
}

pub(crate) fn paste_timeout(settings: &crate::settings::AppSettings, text: &str) -> Duration {
    typing_timeout(text)
        + PASTE_TOOL_TIMEOUT * 5
        + PASTE_PROBE_TIMEOUT * 7
        + crate::paste_tx::RESTORE_TIMEOUT
        + Duration::from_secs(2)
        + Duration::from_millis(
            settings
                .paste_delay_ms
                .saturating_add(settings.paste_delay_after_ms),
        )
}

fn paste_tool(
    command: &mut Command,
    input: Option<Vec<u8>>,
) -> Result<std::process::Output, std::io::Error> {
    paste_tool_with_timeout(command, input, PASTE_TOOL_TIMEOUT)
}
#[cfg(any(target_os = "linux", all(test, unix)))]
fn typing_tool(
    command: &mut Command,
    input: Option<Vec<u8>>,
    text: &str,
) -> std::io::Result<std::process::Output> {
    paste_tool_with_timeout(command, input, typing_timeout(text))
}

fn quiet_tool(command: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    run_paste_process(command, None, timeout, ProcessCapture::None)
}

#[derive(Clone, Copy)]
enum ProcessCapture {
    None,
    Stderr,
    Probe,
}

const PASTE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PROBE_BYTES: usize = 64 * 1024;

#[cfg(any(target_os = "linux", all(test, unix)))]
fn paste_probe_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    run_paste_process(command, None, timeout, ProcessCapture::Probe)
}
fn paste_tool_with_timeout(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    run_paste_process(command, input, timeout, ProcessCapture::Stderr)
}
fn run_paste_process(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    capture: ProcessCapture,
) -> std::io::Result<std::process::Output> {
    use std::{io::Write, process::Stdio};
    let deadline = std::time::Instant::now() + timeout;
    command.stdout(if matches!(capture, ProcessCapture::Probe) {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stderr(if !matches!(capture, ProcessCapture::None) {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(target_os = "linux")]
    crate::process_supervisor::set_parent_death_signal(command);
    let owned = std::mem::replace(command, Command::new(""));
    let mut child = crate::process_supervisor::spawn_from_supervisor(owned)?;
    let result = (|| {
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut stdin = child.stdin.take();
        #[cfg(unix)]
        {
            if let Some(pipe) = &stdout {
                nonblocking_pipe(pipe)?;
            }
            if let Some(pipe) = &stderr {
                nonblocking_pipe(pipe)?;
            }
            if let Some(pipe) = &stdin {
                nonblocking_pipe(pipe)?;
            }
        }
        let input = input.unwrap_or_default();
        let mut offset = 0;
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            if let Some(pipe) = &mut stdin {
                match pipe.write(&input[offset..input.len().min(offset + 8192)]) {
                    Ok(0) if offset < input.len() => {
                        return Err(std::io::ErrorKind::WriteZero.into())
                    }
                    Ok(count) => offset += count,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
                if offset == input.len() {
                    stdin = None;
                }
            }
            let stdout_eof = drain_probe(&mut stdout, &mut out, err.len())?;
            let stderr_eof = if matches!(capture, ProcessCapture::Probe) {
                drain_probe(&mut stderr, &mut err, out.len())?
            } else {
                drain_tool_stderr(&mut stderr, &mut err)?
            };
            if child_exited(&mut child)? && stdout_eof && stderr_eof {
                return Ok(std::process::Output {
                    status: child.wait()?,
                    stdout: out,
                    stderr: err,
                });
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        log::warn!("Paste process failed or timed out; stopping and reaping the owned process");
        #[cfg(unix)]
        {
            // SAFETY: the unreaped child reserves the process group created above.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = child.kill();
        child.wait()?;
    }
    result
}

#[cfg(unix)]
fn nonblocking_pipe(pipe: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    // SAFETY: the pipe is owned by the caller for the duration of both calls.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the pipe is live and the existing flags are preserved.
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(unix)]
fn child_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: zero is a valid initial value for this output-only status structure.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid observes only this owned child and leaves it unreaped.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.si_signo != 0)
}
#[cfg(windows)]
fn child_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

trait ProbeRead {
    fn read_ready(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<usize>>;
}
#[cfg(unix)]
impl<R: std::io::Read> ProbeRead for R {
    fn read_ready(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
        match self.read(buffer) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            result => result.map(Some),
        }
    }
}
#[cfg(windows)]
impl<R: std::io::Read + std::os::windows::io::AsRawHandle> ProbeRead for R {
    fn read_ready(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<usize>> {
        use std::{ffi::c_void, ptr};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            #[link_name = "PeekNamedPipe"]
            fn peek_named_pipe(
                pipe: *mut c_void,
                buffer: *mut c_void,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                left: *mut u32,
            ) -> i32;
        }
        let mut available = 0;
        // SAFETY: this live pipe has one reader and available is a valid output pointer.
        if unsafe {
            peek_named_pipe(
                self.as_raw_handle(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error()
                == Some(windows::Win32::Foundation::ERROR_BROKEN_PIPE.0 as i32)
            {
                Ok(Some(0))
            } else {
                Err(error)
            };
        }
        if available == 0 {
            return Ok(None);
        }
        let count = buffer.len().min(available as usize);
        self.read(&mut buffer[..count]).map(Some)
    }
}
fn drain_probe<R: ProbeRead>(
    pipe: &mut Option<R>,
    bytes: &mut Vec<u8>,
    other_bytes: usize,
) -> std::io::Result<bool> {
    let Some(reader) = pipe else {
        return Ok(true);
    };
    let mut buffer = [0; 8192];
    match reader.read_ready(&mut buffer)? {
        Some(0) => {
            *pipe = None;
            Ok(true)
        }
        Some(count) => {
            if bytes.len() + other_bytes + count > MAX_PROBE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "paste probe output exceeded limit",
                ));
            }
            bytes.extend_from_slice(&buffer[..count]);
            Ok(false)
        }
        None => Ok(false),
    }
}

fn drain_tool_stderr<R: ProbeRead>(
    pipe: &mut Option<R>,
    bytes: &mut Vec<u8>,
) -> std::io::Result<bool> {
    let Some(reader) = pipe else {
        return Ok(true);
    };
    let mut buffer = [0; 8192];
    match reader.read_ready(&mut buffer)? {
        Some(0) => {
            *pipe = None;
            Ok(true)
        }
        Some(count) => {
            let keep = count.min(MAX_PROBE_BYTES.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&buffer[..keep]);
            Ok(false)
        }
        None => Ok(false),
    }
}

fn input_if_current<T>(
    cancelled: &std::sync::atomic::AtomicBool,
    input: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err("Input request expired".into());
    }
    input()
}

/// Runs `f` with the shared Enigo on the main thread (macOS TIS APIs require it)
/// and waits for the result. Runs inline if already on the main thread, so no deadlock.
fn with_enigo<T: Send + 'static>(
    app_handle: &AppHandle,
    f: impl FnOnce(&mut Enigo) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    with_enigo_timeout(app_handle, PASTE_TOOL_TIMEOUT, f)
}

fn with_enigo_timeout<T: Send + 'static>(
    app_handle: &AppHandle,
    timeout: Duration,
    f: impl FnOnce(&mut Enigo) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let app = app_handle.clone();
    queued_input(
        |job| {
            app_handle
                .run_on_main_thread(job)
                .map_err(|e| format!("Failed to run input on the main thread: {e}"))
        },
        move || {
            let enigo_state = app
                .try_state::<EnigoState>()
                .ok_or("Enigo state not initialized")?;
            let mut enigo = enigo_state
                .0
                .lock()
                .map_err(|e| format!("Failed to lock Enigo: {e}"))?;
            f(&mut enigo)
        },
        timeout,
    )
}
fn queued_input<T: Send + 'static>(
    queue: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), String>,
    input: impl FnOnce() -> Result<T, String> + Send + 'static,
    timeout: Duration,
) -> Result<T, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = std::sync::Arc::new(Mutex::new(false));
    let queued = cancelled.clone();
    let running = started.clone();
    queue(Box::new(move || {
        let mut running = running.lock().unwrap_or_else(PoisonError::into_inner);
        let result = input_if_current(&queued, || {
            *running = true;
            drop(running);
            input()
        });
        let _ = tx.send(result);
    }))?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let started = started.lock().unwrap_or_else(PoisonError::into_inner);
            if *started {
                drop(started);
                log::warn!("Native input is still running; paste admission remains held");
                rx.recv()
                    .map_err(|_| "Main thread dropped the running input request".to_string())?
            } else {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                Err("Main thread input request expired before starting".into())
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("Main thread dropped the input request".into())
        }
    }
}

fn write_text_to_clipboard(app_handle: &AppHandle, text: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if is_wayland() && is_wl_copy_available()? {
        info!("Using wl-copy for clipboard write on Wayland");
        return write_clipboard_via_wl_copy(text);
    }

    app_handle
        .clipboard()
        .write_text(text)
        .map_err(|e| format!("Failed to write to clipboard: {}", e))
}

fn finish_clipboard_paste(
    paste_result: Result<(), String>,
    paste_delay_after_ms: u64,
    restore_clipboard: impl FnOnce(),
) -> Result<(), String> {
    std::thread::sleep(Duration::from_millis(paste_delay_after_ms));
    restore_clipboard();
    paste_result
}

/// Pastes text using the clipboard: saves current content, writes text, sends paste keystroke, restores clipboard.
fn paste_via_clipboard(
    text: &str,
    app_handle: &AppHandle,
    paste_method: &PasteMethod,
    paste_delay_ms: u64,
    paste_delay_after_ms: u64,
) -> Result<(), String> {
    let clipboard = app_handle.clipboard();
    let saved_text = clipboard.read_text().ok().filter(|t| !t.is_empty());
    // Only probe for an image when there is no text to restore. Text is by far the
    // common case, and reading an image decodes the full bitmap, so this keeps the
    // text path exactly as cheap as it was before.
    let saved_image = if saved_text.is_none() {
        clipboard.read_image().ok().map(|image| image.to_owned())
    } else {
        None
    };

    // Write text to clipboard first
    write_text_to_clipboard(app_handle, text)?;

    std::thread::sleep(Duration::from_millis(paste_delay_ms));

    // Capture key injection errors so the original clipboard is restored before
    // propagating them to the caller.
    let paste_result = (|| -> Result<(), String> {
        // Send paste key combo
        #[cfg(target_os = "linux")]
        let key_combo_sent = try_send_key_combo_linux(paste_method)?;

        #[cfg(not(target_os = "linux"))]
        let key_combo_sent = false;

        // Fall back to enigo if no native tool handled it
        if !key_combo_sent {
            let paste_method = *paste_method;
            with_enigo(app_handle, move |enigo| match paste_method {
                // The legacy path cannot detect a mistimed chord, so it keeps the
                // conservative 100ms modifier hold.
                PasteMethod::CtrlV => input::send_paste_ctrl_v(enigo, 100),
                PasteMethod::CtrlShiftV => input::send_paste_ctrl_shift_v(enigo, 100),
                PasteMethod::ShiftInsert => input::send_paste_shift_insert(enigo, 100),
                _ => Err("Invalid paste method for clipboard paste".into()),
            })?;
        }

        Ok(())
    })();

    finish_clipboard_paste(paste_result, paste_delay_after_ms, || {
        // Restore original clipboard content even when key injection failed.
        // Text takes priority so this path stays identical to the previous behavior;
        // an image is only restored when the clipboard held no text at all, which is
        // the case that used to silently wipe screenshots.
        if let Some(clipboard_content) = saved_text {
            let _ = write_text_to_clipboard(app_handle, &clipboard_content);
        } else if let Some(image) = saved_image {
            info!("Restoring image to clipboard");
            let _ = clipboard.write_image(&image);
        } else {
            // Nothing was there to begin with — don't leave the transcription behind.
            let _ = clipboard.clear();
        }
    })
}

/// Attempts to send a key combination using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_send_key_combo_linux(paste_method: &PasteMethod) -> Result<bool, String> {
    if is_wayland() {
        // Wayland: prefer wtype (but not on KDE or GNOME), then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        // or on GNOME/Mutter (same reason — Mutter deliberately does not implement
        // the virtual-keyboard-v1 protocol).
        if !is_kde_wayland() && !is_gnome_wayland() && is_wtype_available()? {
            info!("Using wtype for key combo");
            send_key_combo_via_wtype(paste_method)?;
            return Ok(true);
        }
        if is_dotool_available()? {
            info!("Using dotool for key combo");
            send_key_combo_via_dotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available()? {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available()? {
            info!("Using xdotool for key combo");
            send_key_combo_via_xdotool(paste_method)?;
            return Ok(true);
        }
        if is_ydotool_available()? {
            info!("Using ydotool for key combo");
            send_key_combo_via_ydotool(paste_method)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Attempts to type text directly using Linux-native tools.
/// Returns `Ok(true)` if a native tool handled it, `Ok(false)` to fall back to enigo.
#[cfg(target_os = "linux")]
fn try_direct_typing_linux(text: &str, preferred_tool: TypingTool) -> Result<bool, String> {
    // If user specified a tool, try only that one
    if preferred_tool != TypingTool::Auto {
        return match preferred_tool {
            TypingTool::Wtype if is_wtype_available()? => {
                info!("Using user-specified wtype");
                type_text_via_wtype(text)?;
                Ok(true)
            }
            TypingTool::Kwtype if is_kwtype_available()? => {
                info!("Using user-specified kwtype");
                type_text_via_kwtype(text)?;
                Ok(true)
            }
            TypingTool::Dotool if is_dotool_available()? => {
                info!("Using user-specified dotool");
                type_text_via_dotool(text)?;
                Ok(true)
            }
            TypingTool::Ydotool if is_ydotool_available()? => {
                info!("Using user-specified ydotool");
                type_text_via_ydotool(text)?;
                Ok(true)
            }
            TypingTool::Xdotool if is_xdotool_available()? => {
                info!("Using user-specified xdotool");
                type_text_via_xdotool(text)?;
                Ok(true)
            }
            _ => Err(format!(
                "Typing tool {:?} is not available on this system",
                preferred_tool
            )),
        };
    }

    // Auto mode - existing fallback chain
    if is_wayland() {
        // KDE Wayland: prefer kwtype (uses KDE Fake Input protocol, supports umlauts)
        if is_kde_wayland() && is_kwtype_available()? {
            info!("Using kwtype for direct text input on KDE Wayland");
            type_text_via_kwtype(text)?;
            return Ok(true);
        }
        // Wayland: prefer wtype, then dotool, then ydotool
        // Note: wtype doesn't work on KDE (no zwp_virtual_keyboard_manager_v1 support)
        // or on GNOME/Mutter (same reason — Mutter deliberately does not implement
        // the virtual-keyboard-v1 protocol).
        if !is_kde_wayland() && !is_gnome_wayland() && is_wtype_available()? {
            info!("Using wtype for direct text input");
            type_text_via_wtype(text)?;
            return Ok(true);
        }
        if is_dotool_available()? {
            info!("Using dotool for direct text input");
            type_text_via_dotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available()? {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    } else {
        // X11: prefer xdotool, then ydotool
        if is_xdotool_available()? {
            info!("Using xdotool for direct text input");
            type_text_via_xdotool(text)?;
            return Ok(true);
        }
        if is_ydotool_available()? {
            info!("Using ydotool for direct text input");
            type_text_via_ydotool(text)?;
            return Ok(true);
        }
    }

    Ok(false)
}

/// Returns the list of available typing tools on this system.
/// Always includes "auto" as the first entry.
#[cfg(target_os = "linux")]
pub fn get_available_typing_tools() -> Vec<String> {
    let mut tools = vec!["auto".to_string()];
    if is_wtype_available().unwrap_or(false) {
        tools.push("wtype".to_string());
    }
    if is_kwtype_available().unwrap_or(false) {
        tools.push("kwtype".to_string());
    }
    if is_dotool_available().unwrap_or(false) {
        tools.push("dotool".to_string());
    }
    if is_ydotool_available().unwrap_or(false) {
        tools.push("ydotool".to_string());
    }
    if is_xdotool_available().unwrap_or(false) {
        tools.push("xdotool".to_string());
    }
    tools
}

#[cfg(target_os = "linux")]
fn tool_available_with(command: &mut Command, timeout: Duration) -> Result<bool, String> {
    paste_probe_with_timeout(command, timeout)
        .map(|output| output.status.success())
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                Err(format!("Typing tool availability probe failed: {error}"))
            } else {
                Ok(false)
            }
        })
}

/// Check if wtype is available (Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_wtype_available() -> Result<bool, String> {
    tool_available_with(Command::new("which").arg("wtype"), PASTE_PROBE_TIMEOUT)
}

/// Check if dotool is available (another Wayland text input tool)
#[cfg(target_os = "linux")]
fn is_dotool_available() -> Result<bool, String> {
    tool_available_with(Command::new("which").arg("dotool"), PASTE_PROBE_TIMEOUT)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YdotoolKeySyntax {
    Symbolic,
    RawKeycodes,
}

#[cfg(target_os = "linux")]
const YDOTOOL_UNKNOWN_HELP_FALLBACK: YdotoolKeySyntax = YdotoolKeySyntax::RawKeycodes;

#[cfg(target_os = "linux")]
static YDOTOOL_KEY_SYNTAX: OnceLock<YdotoolKeySyntax> = OnceLock::new();

/// Classifies `ydotool key --help` output without relying on version or distro metadata.
#[cfg(target_os = "linux")]
fn classify_ydotool_key_syntax(help: &str) -> Option<YdotoolKeySyntax> {
    let help = help.to_ascii_lowercase();

    // Check modern markers first in case a future help message mentions the legacy syntax.
    if help.contains("syntax: <keycode>:<pressed>")
        || help.contains("[keycodes]")
        || help.contains("using raw keycodes")
    {
        Some(YdotoolKeySyntax::RawKeycodes)
    } else if help.contains("separated by plus (+)")
        || (help.contains("<key sequence>") && help.contains("alt+r"))
    {
        Some(YdotoolKeySyntax::Symbolic)
    } else {
        None
    }
}

/// Detects and caches a recognized ydotool key syntax. Unknown or failed probes are not cached,
/// allowing a transient daemon or PATH problem to recover on a later paste attempt.
#[cfg(target_os = "linux")]
fn detect_ydotool_key_syntax() -> Result<YdotoolKeySyntax, String> {
    detect_ydotool_key_syntax_with(
        Command::new("ydotool").args(["key", "--help"]),
        &YDOTOOL_KEY_SYNTAX,
        PASTE_PROBE_TIMEOUT,
    )
}

#[cfg(target_os = "linux")]
fn detect_ydotool_key_syntax_with(
    command: &mut Command,
    cache: &OnceLock<YdotoolKeySyntax>,
    timeout: Duration,
) -> Result<YdotoolKeySyntax, String> {
    if let Some(syntax) = cache.get() {
        return Ok(*syntax);
    }

    match paste_probe_with_timeout(command, timeout) {
        Ok(output) => {
            // ydotool 0.x writes help to stderr and its exit status varies by build.
            let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
            if !help.is_empty() && !output.stderr.is_empty() {
                help.push('\n');
            }
            help.push_str(&String::from_utf8_lossy(&output.stderr));

            if let Some(syntax) = classify_ydotool_key_syntax(&help) {
                Ok(*cache.get_or_init(|| {
                    info!("Detected ydotool key syntax: {:?}", syntax);
                    syntax
                }))
            } else {
                // Preserve Handy's existing behavior and compatibility with current ydotool.
                log::warn!(
                    "Could not recognize ydotool key --help output (exit status {:?}); using raw-keycode syntax",
                    output.status.code()
                );
                Ok(YDOTOOL_UNKNOWN_HELP_FALLBACK)
            }
        }
        Err(error) if error.kind() != std::io::ErrorKind::TimedOut => {
            log::warn!("Could not query ydotool key syntax: {error}; using raw-keycode syntax");
            Ok(YDOTOOL_UNKNOWN_HELP_FALLBACK)
        }
        Err(error) => Err(format!("Could not query ydotool key syntax: {error}")),
    }
}

/// Check if ydotool is available (uinput-based, works on both Wayland and X11)
#[cfg(target_os = "linux")]
fn is_ydotool_available() -> Result<bool, String> {
    tool_available_with(Command::new("which").arg("ydotool"), PASTE_PROBE_TIMEOUT)
}

#[cfg(target_os = "linux")]
fn is_xdotool_available() -> Result<bool, String> {
    tool_available_with(Command::new("which").arg("xdotool"), PASTE_PROBE_TIMEOUT)
}

/// Check if kwtype is available (KDE Wayland virtual keyboard input tool)
#[cfg(target_os = "linux")]
fn is_kwtype_available() -> Result<bool, String> {
    tool_available_with(Command::new("which").arg("kwtype"), PASTE_PROBE_TIMEOUT)
}

/// Check if wl-copy is available (Wayland clipboard tool)
#[cfg(target_os = "linux")]
fn is_wl_copy_available() -> Result<bool, String> {
    tool_available_with(Command::new("which").arg("wl-copy"), PASTE_PROBE_TIMEOUT)
}

/// Type text directly via wtype on Wayland.
#[cfg(target_os = "linux")]
fn type_text_via_wtype(text: &str) -> Result<(), String> {
    let output = typing_tool(
        Command::new("wtype")
            .arg("--") // Protect against text starting with -
            .arg(text),
        None,
        text,
    )
    .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via xdotool on X11.
#[cfg(target_os = "linux")]
fn type_text_via_xdotool(text: &str) -> Result<(), String> {
    type_text_via_xdotool_with(text, paste_tool_with_timeout)
}

#[cfg(target_os = "linux")]
fn type_text_via_xdotool_with(
    text: &str,
    mut run: impl FnMut(
        &mut Command,
        Option<Vec<u8>>,
        Duration,
    ) -> std::io::Result<std::process::Output>,
) -> Result<(), String> {
    let output = run(
        Command::new("xdotool")
            .arg("type")
            .arg("--clearmodifiers")
            .arg("--")
            .arg(text),
        None,
        typing_timeout(text),
    )
    .map_err(|e| format!("Failed to execute xdotool: {}", e));

    // `--clearmodifiers` restores the modifiers that were held when xdotool
    // started. If the user releases one while xdotool is typing, that synthetic
    // restore can leave the modifier latched on the XTEST keyboard (#1817).
    // Release both sides of Handy's supported push-style modifiers to clear any
    // stale restore. Lock keys are intentionally excluded because key events
    // toggle them.
    //
    // The release is unconditional, so it can make a modifier that is still
    // physically held appear released until the next physical event. This is
    // preferable to leaving a synthetic modifier latched system-wide.
    //
    // Clean up before checking the typing status because xdotool may have
    // changed modifier state before returning an error. Cleanup remains
    // best-effort because the text may already have been partially or fully
    // typed, but failures are logged so they can be diagnosed.
    match run(
        Command::new("xdotool").arg("keyup").args([
            "Control_L",
            "Control_R",
            "Shift_L",
            "Shift_R",
            "Alt_L",
            "Alt_R",
            "Super_L",
            "Super_R",
        ]),
        None,
        PASTE_TOOL_TIMEOUT,
    ) {
        Ok(cleanup_output) if !cleanup_output.status.success() => {
            log::warn!(
                "xdotool modifier cleanup failed with status {:?}",
                cleanup_output.status.code()
            );
        }
        Err(error) => log::warn!("Failed to execute xdotool modifier cleanup: {}", error),
        _ => {}
    }

    let output = output?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Builds the dotool stdin script that types `text`.
///
/// dotool reads one command per line, so each line of `text` gets its own
/// `type` command and line breaks are sent as `key enter`, like the other
/// typing tools do. Otherwise everything after the first newline would be run
/// as dotool commands instead of being typed.
#[cfg(target_os = "linux")]
fn dotool_type_script(text: &str) -> String {
    let mut script = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            script.push_str("key enter\n");
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !line.is_empty() {
            script.push_str("type ");
            script.push_str(line);
            script.push('\n');
        }
    }
    script
}

/// Type text directly via dotool (works on both Wayland and X11 via uinput).
#[cfg(target_os = "linux")]
fn type_text_via_dotool(text: &str) -> Result<(), String> {
    let output = typing_tool(
        &mut Command::new("dotool"),
        Some(dotool_type_script(text).into_bytes()),
        text,
    )
    .map_err(|e| format!("Failed to execute dotool: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "dotool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Type text directly via ydotool (uinput-based, requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn type_text_via_ydotool(text: &str) -> Result<(), String> {
    let output = typing_tool(
        Command::new("ydotool").arg("type").arg("--").arg(text),
        None,
        text,
    )
    .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
}

/// Type text directly via kwtype (KDE Wayland virtual keyboard, uses KDE Fake Input protocol).
#[cfg(target_os = "linux")]
fn type_text_via_kwtype(text: &str) -> Result<(), String> {
    let output = typing_tool(Command::new("kwtype").arg("--").arg(text), None, text)
        .map_err(|e| format!("Failed to execute kwtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kwtype failed: {}", stderr));
    }

    Ok(())
}

/// Write text to clipboard via wl-copy (Wayland clipboard tool).
/// Uses Stdio::null() to avoid blocking on repeated calls — wl-copy forks a
/// daemon that inherits piped fds, causing read_to_end to hang indefinitely.
#[cfg(target_os = "linux")]
fn write_clipboard_via_wl_copy(text: &str) -> Result<(), String> {
    use std::process::Stdio;
    let status = quiet_tool(
        Command::new("wl-copy")
            .arg("--")
            .arg(text)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        PASTE_TOOL_TIMEOUT,
    )
    .map(|output| output.status)
    .map_err(|e| format!("Failed to execute wl-copy: {}", e))?;

    if !status.success() {
        return Err("wl-copy failed".into());
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via wtype on Wayland.
#[cfg(target_os = "linux")]
fn send_key_combo_via_wtype(paste_method: &PasteMethod) -> Result<(), String> {
    let args: Vec<&str> = match paste_method {
        PasteMethod::CtrlV => vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"],
        PasteMethod::ShiftInsert => vec!["-M", "shift", "-k", "Insert", "-m", "shift"],
        PasteMethod::CtrlShiftV => vec![
            "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
        ],
        _ => return Err("Unsupported paste method".into()),
    };

    let output = paste_tool(Command::new("wtype").args(&args), None)
        .map_err(|e| format!("Failed to execute wtype: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wtype failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via dotool.
#[cfg(target_os = "linux")]
fn send_key_combo_via_dotool(paste_method: &PasteMethod) -> Result<(), String> {
    send_key_combo_via_dotool_with(paste_method, paste_tool)
}

#[cfg(target_os = "linux")]
fn send_key_combo_via_dotool_with(
    paste_method: &PasteMethod,
    run: impl FnOnce(&mut Command, Option<Vec<u8>>) -> std::io::Result<std::process::Output>,
) -> Result<(), String> {
    let command = match paste_method {
        PasteMethod::CtrlV => "key ctrl+v\n",
        PasteMethod::ShiftInsert => "key shift+insert\n",
        PasteMethod::CtrlShiftV => "key ctrl+shift+v\n",
        _ => return Err("Unsupported paste method".into()),
    };
    let output = run(
        &mut Command::new("dotool"),
        Some(command.as_bytes().to_vec()),
    )
    .map_err(|e| format!("Failed to execute dotool: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "dotool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn ydotool_key_args(
    paste_method: &PasteMethod,
    syntax: YdotoolKeySyntax,
) -> Result<&'static [&'static str], String> {
    let args = match (paste_method, syntax) {
        (PasteMethod::CtrlV, YdotoolKeySyntax::Symbolic) => &["key", "ctrl+v"][..],
        (PasteMethod::CtrlShiftV, YdotoolKeySyntax::Symbolic) => &["key", "ctrl+shift+v"][..],
        (PasteMethod::ShiftInsert, YdotoolKeySyntax::Symbolic) => &["key", "shift+insert"][..],
        (PasteMethod::CtrlV, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "29:1", "47:1", "47:0", "29:0"][..]
        }
        (PasteMethod::CtrlShiftV, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"][..]
        }
        (PasteMethod::ShiftInsert, YdotoolKeySyntax::RawKeycodes) => {
            &["key", "42:1", "110:1", "110:0", "42:0"][..]
        }
        _ => return Err("Unsupported paste method".into()),
    };

    Ok(args)
}

/// Send a key combination (e.g., Ctrl+V) via ydotool (requires ydotoold daemon).
#[cfg(target_os = "linux")]
fn send_key_combo_via_ydotool(paste_method: &PasteMethod) -> Result<(), String> {
    let syntax = detect_ydotool_key_syntax()?;
    let args = ydotool_key_args(paste_method, syntax)?;

    let output = paste_tool(Command::new("ydotool").args(args), None)
        .map_err(|e| format!("Failed to execute ydotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ydotool failed: {}", stderr));
    }

    Ok(())
}

/// Send a key combination (e.g., Ctrl+V) via xdotool on X11.
#[cfg(target_os = "linux")]
fn send_key_combo_via_xdotool(paste_method: &PasteMethod) -> Result<(), String> {
    let key_combo = match paste_method {
        PasteMethod::CtrlV => "ctrl+v",
        PasteMethod::CtrlShiftV => "ctrl+shift+v",
        PasteMethod::ShiftInsert => "shift+Insert",
        _ => return Err("Unsupported paste method".into()),
    };

    let output = paste_tool(
        Command::new("xdotool")
            .arg("key")
            .arg("--clearmodifiers")
            .arg(key_combo),
        None,
    )
    .map_err(|e| format!("Failed to execute xdotool: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdotool failed: {}", stderr));
    }

    Ok(())
}

/// Pastes text by invoking an external script.
/// The script receives the text to paste as a single argument.
fn paste_via_external_script(text: &str, script_path: &str) -> Result<(), String> {
    info!("Pasting via external script");

    // Do not capture the script's stdio. Wayland clipboard helpers such as
    // wl-copy may fork a background selection daemon that inherits those file
    // descriptors; waiting for captured output would then block until the
    // clipboard selection is replaced instead of returning after the script
    // itself exits.
    use std::process::Stdio;
    let status = quiet_tool(
        Command::new(script_path)
            .arg(text)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        typing_timeout(text),
    )
    .map(|output| output.status)
    .map_err(|e| format!("Failed to execute external script '{}': {}", script_path, e))?;

    if !status.success() {
        return Err(format!(
            "External script '{}' failed with exit code {:?}",
            script_path,
            status.code()
        ));
    }

    Ok(())
}

/// Types text directly by simulating individual key presses.
fn paste_direct(
    text: &str,
    app_handle: &AppHandle,
    #[cfg(target_os = "linux")] typing_tool: TypingTool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let handled = try_direct_typing_linux(text, typing_tool);
    #[cfg(not(target_os = "linux"))]
    let handled = Ok(false);

    paste_with_fallback(handled, || {
        info!("Falling back to enigo for direct text input");
        let text = text.to_string();
        with_enigo_timeout(app_handle, typing_timeout(&text), move |enigo| {
            input::paste_text_direct(enigo, &text)
        })
    })
}

fn paste_with_fallback(
    handled: Result<bool, String>,
    native: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if handled? {
        Ok(())
    } else {
        native()
    }
}

pub(crate) fn send_return_key(enigo: &mut Enigo, key_type: AutoSubmitKey) -> Result<(), String> {
    match key_type {
        AutoSubmitKey::Enter => {
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
        }
        AutoSubmitKey::CtrlEnter => {
            enigo
                .key(Key::Control, Direction::Press)
                .map_err(|e| format!("Failed to press Control key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Control, Direction::Release)
                .map_err(|e| format!("Failed to release Control key: {}", e))?;
        }
        AutoSubmitKey::CmdEnter => {
            enigo
                .key(Key::Meta, Direction::Press)
                .map_err(|e| format!("Failed to press Meta/Cmd key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Press)
                .map_err(|e| format!("Failed to press Return key: {}", e))?;
            enigo
                .key(Key::Return, Direction::Release)
                .map_err(|e| format!("Failed to release Return key: {}", e))?;
            enigo
                .key(Key::Meta, Direction::Release)
                .map_err(|e| format!("Failed to release Meta/Cmd key: {}", e))?;
        }
    }

    Ok(())
}

fn should_send_auto_submit(auto_submit: bool, paste_method: PasteMethod) -> bool {
    auto_submit && paste_method != PasteMethod::None
}

fn with_paste_lock<T>(paste: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _paste = PASTE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    paste()
}

pub fn paste(
    text: String,
    app_handle: AppHandle,
    completion: handy_tts_core::handoff::PasteCompletion,
) -> Result<(), String> {
    with_paste_lock(|| {
        let result = paste_inner(text, app_handle, &completion);
        acknowledge_paste(result, &completion)
    })
}
fn acknowledge_paste(
    result: Result<bool, String>,
    completion: &handy_tts_core::handoff::PasteCompletion,
) -> Result<(), String> {
    if !matches!(result, Ok(true)) {
        completion.settle_with(|| {});
    }
    result.map(|_| ())
}
fn paste_inner(
    text: String,
    app_handle: AppHandle,
    _completion: &handy_tts_core::handoff::PasteCompletion,
) -> Result<bool, String> {
    let settings = get_settings(&app_handle);
    let paste_method = settings.paste_method;
    let paste_delay_ms = settings.paste_delay_ms;
    let paste_delay_after_ms = settings.paste_delay_after_ms;

    // Append trailing space if setting is enabled
    let text = if settings.append_trailing_space {
        format!("{} ", text)
    } else {
        text
    };

    info!(
        "Using paste method: {:?}, delay before: {}ms, delay after: {}ms",
        paste_method, paste_delay_ms, paste_delay_after_ms
    );

    // Perform the paste operation
    match paste_method {
        PasteMethod::None => {
            info!("PasteMethod::None selected - skipping paste action");
        }
        PasteMethod::Direct => {
            paste_direct(
                &text,
                &app_handle,
                #[cfg(target_os = "linux")]
                settings.typing_tool,
            )?;
        }
        PasteMethod::CtrlV | PasteMethod::CtrlShiftV | PasteMethod::ShiftInsert => {
            // Debug-gated receipt-sequenced paste (#502): restore the clipboard
            // after the target actually reads the transcript, not on a timer.
            // On success it fully handles the paste (including auto-submit and
            // clipboard handling) asynchronously; on failure fall through to
            // the legacy path untouched.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if settings.reliable_paste {
                // Runs entirely on the main thread, as it requires.
                let (text, app) = (text.clone(), app_handle.clone());
                let (auto_submit, auto_submit_key, clipboard_handling) = (
                    settings.auto_submit,
                    settings.auto_submit_key,
                    settings.clipboard_handling,
                );
                let completion = _completion.clone();
                let reliable_result = with_enigo(&app_handle, move |enigo| {
                    crate::paste_tx::try_reliable_paste(
                        &text,
                        &app,
                        &paste_method,
                        enigo,
                        crate::paste_tx::TransactionOptions {
                            auto_submit,
                            auto_submit_key,
                            clipboard_handling,
                            completion,
                        },
                    )
                });
                match reliable_result {
                    Ok(()) => return Ok(true),
                    Err(e) => {
                        log::warn!("Reliable paste unavailable ({e}); falling back to legacy paste")
                    }
                }
            }
            paste_via_clipboard(
                &text,
                &app_handle,
                &paste_method,
                paste_delay_ms,
                paste_delay_after_ms,
            )?
        }
        PasteMethod::ExternalScript => {
            let script_path = settings
                .external_script_path
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or("External script path is not configured")?;
            paste_via_external_script(&text, script_path)?;
        }
    }

    if should_send_auto_submit(settings.auto_submit, paste_method) {
        std::thread::sleep(Duration::from_millis(50));
        let auto_submit_key = settings.auto_submit_key;
        if let Err(_error) = with_enigo(&app_handle, move |enigo| {
            send_return_key(enigo, auto_submit_key)
        }) {
            log::warn!("Paste succeeded, but auto-submit failed");
        }
    }

    // After pasting, optionally copy to clipboard based on settings
    if settings.clipboard_handling == ClipboardHandling::CopyToClipboard {
        write_text_to_clipboard(&app_handle, &text)?;
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix2_unspawnable_probe_keeps_native_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-probe");
        for (path, kind) in [
            (missing.as_path(), std::io::ErrorKind::NotFound),
            (dir.path(), std::io::ErrorKind::PermissionDenied),
        ] {
            assert!(path.is_absolute());
            assert_eq!(Command::new(path).output().unwrap_err().kind(), kind);
            let available = tool_available_with(&mut Command::new(path), Duration::from_secs(1));
            assert_eq!(
                available,
                Ok(false),
                "unspawnable probe must be unavailable"
            );
            let called = Cell::new(false);
            let result = paste_with_fallback(available, || {
                called.set(true);
                Ok(())
            });
            assert!(result.is_ok());
            assert!(called.get(), "missing probe must reach native paste");
        }
        let called = Cell::new(false);
        let result = paste_with_fallback(
            tool_available_with(
                Command::new("sh").args(["-c", "sleep 2"]),
                Duration::from_millis(60),
            ),
            || {
                called.set(true);
                Ok(())
            },
        );
        assert!(result.is_err(), "probe timeout must abort native fallback");
        assert!(!called.get());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix2_failed_help_probe_uses_uncached_fallback_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-help-probe");
        for path in [missing.as_path(), dir.path()] {
            let cache = OnceLock::new();
            assert_eq!(
                detect_ydotool_key_syntax_with(
                    &mut Command::new(path),
                    &cache,
                    Duration::from_secs(1)
                ),
                Ok(YDOTOOL_UNKNOWN_HELP_FALLBACK),
                "failed help probe must retain raw-keycode fallback"
            );
            assert!(
                cache.get().is_none(),
                "failed help must not poison syntax cache"
            );
            assert_eq!(
                detect_ydotool_key_syntax_with(
                    Command::new("sh").args(["-c", "printf 'separated by plus (+)'"]),
                    &cache,
                    Duration::from_secs(1)
                ),
                Ok(YdotoolKeySyntax::Symbolic),
                "later help probe must recover"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix2_unreadable_probe_output_keeps_fallbacks() {
        let script = "import sys; sys.stdout.write('x'*70000)";
        let available = tool_available_with(
            Command::new("python3").args(["-c", script]),
            Duration::from_secs(2),
        );
        assert_eq!(
            available,
            Ok(false),
            "unreadable availability output must allow fallback"
        );
        let cache = OnceLock::new();
        assert_eq!(
            detect_ydotool_key_syntax_with(
                Command::new("python3").args(["-c", script]),
                &cache,
                Duration::from_secs(2)
            ),
            Ok(YDOTOOL_UNKNOWN_HELP_FALLBACK),
            "unreadable help output must allow fallback"
        );
        assert!(cache.get().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix2_tool_receives_complete_large_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let received = dir.path().join("stdin");
        let capacity = dir.path().join("capacity");
        let payload = b"type complete payload\n".repeat(16384);
        let output = paste_tool_with_timeout(
            Command::new("python3")
                .args([
                    "-c",
                    r#"import fcntl, pathlib, sys
capacity = fcntl.fcntl(sys.stdin.fileno(), fcntl.F_GETPIPE_SZ)
pathlib.Path(sys.argv[2]).write_text(str(capacity))
pathlib.Path(sys.argv[1]).write_bytes(sys.stdin.buffer.read())
"#,
                ])
                .arg(&received)
                .arg(&capacity),
            Some(payload.clone()),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        let capacity: usize = std::fs::read_to_string(capacity).unwrap().parse().unwrap();
        assert!(
            payload.len() > capacity,
            "fixture must exceed pipe capacity"
        );
        assert!(
            std::fs::read(received).unwrap() == payload,
            "tool must receive every stdin byte"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wbfix2_external_script_descendant_serves_after_return() {
        use std::io::{Read, Write};
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("clipboard.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let script = dir.path().join("clipboard-helper");
        std::fs::write(
            &script,
            r#"#!/usr/bin/python3
import os, socket, sys
ready_read, ready_write = os.pipe()
if os.fork():
    os.close(ready_write)
    os.read(ready_read, 1)
    os.close(ready_read)
    sys.exit(0)
os.close(ready_read)
with socket.socket(socket.AF_UNIX) as stream:
    stream.settimeout(5)
    stream.connect(sys.argv[1])
    os.write(ready_write, b'1')
    os.close(ready_write)
    request = stream.recv(64)
    stream.sendall(request)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let began = std::time::Instant::now();
        let result = paste_via_external_script(socket.to_str().unwrap(), script.to_str().unwrap());
        assert!(result.is_ok(), "clipboard helper must return successfully");
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "helper must not wait for its serving descendant"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "serving descendant must connect"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept clipboard descendant: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = b"clipboard requested after parent returned";
        let sent = stream.write_all(request);
        let mut response = Vec::new();
        let read = stream.read_to_end(&mut response);
        assert!(
            sent.is_ok() && read.is_ok() && response == request,
            "serving descendant must survive successful tool return"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix_fix1_ydotool_help_keeps_both_streams_and_legacy_exit_status() {
        for (help, stream, expected) in [
            (YDOTOOL_0_1_8_HELP, "stderr", YdotoolKeySyntax::Symbolic),
            (YDOTOOL_1_0_4_HELP, "stdout", YdotoolKeySyntax::RawKeycodes),
        ] {
            let cache = OnceLock::new();
            let syntax = detect_ydotool_key_syntax_with(
                Command::new("python3").args([
                    "-c",
                    "import sys; print(sys.argv[1], file=getattr(sys, sys.argv[2])); sys.exit(1)",
                    help,
                    stream,
                ]),
                &cache,
                Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(
                syntax, expected,
                "captured help must preserve syntax detection"
            );
            assert_eq!(cache.get(), Some(&expected));
            assert_eq!(
                detect_ydotool_key_syntax_with(
                    &mut Command::new("missing-cached-probe"),
                    &cache,
                    Duration::from_millis(10)
                )
                .unwrap(),
                expected
            );
        }
        let cache = OnceLock::new();
        assert_eq!(
            detect_ydotool_key_syntax_with(
                Command::new("sh").args(["-c", "printf unrecognized"]),
                &cache,
                Duration::from_secs(2)
            )
            .unwrap(),
            YDOTOOL_UNKNOWN_HELP_FALLBACK
        );
        assert!(cache.get().is_none());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix_fix1_availability_probe_is_bounded_and_missing_tool_is_not_an_error() {
        let began = std::time::Instant::now();
        let result = tool_available_with(
            Command::new("sh").args(["-c", "sleep 2; exit 0"]),
            Duration::from_millis(60),
        );
        assert!(
            result.is_err(),
            "availability probe deadline must abort the paste"
        );
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(!tool_available_with(
            Command::new("sh").args(["-c", "exit 1"]),
            Duration::from_secs(1)
        )
        .unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn wbfix_fix1_probe_deadline_covers_inherited_pipes_after_parent_exit() {
        let result = paste_probe_with_timeout(
            Command::new("sh").args(["-c", "sleep 2 & exit 0"]),
            Duration::from_millis(60),
        );
        assert!(
            result.is_err(),
            "probe deadline must cover descendant-held pipes"
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }
    #[cfg(unix)]
    #[test]
    fn wbfix_fix1_probe_output_is_bounded_across_stdout_and_stderr() {
        let result = paste_probe_with_timeout(
            Command::new("python3").args([
                "-c",
                "import sys; sys.stdout.write('a'*40000); sys.stderr.write('b'*40000)",
            ]),
            Duration::from_secs(2),
        );
        assert!(result.is_err(), "probe output must stay bounded");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }
    #[test]
    fn wbfix_fix1_queued_input_expiry_prevents_late_execution() {
        let queued = std::sync::Mutex::new(None);
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = called.clone();
        let result = queued_input(
            |job| {
                *queued.lock().unwrap() = Some(job);
                Ok(())
            },
            move || {
                observed.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            },
            Duration::from_millis(10),
        );
        assert!(result.is_err());
        queued.lock().unwrap().take().unwrap()();
        assert!(
            !called.load(std::sync::atomic::Ordering::Acquire),
            "expired queued input must never run later"
        );
    }
    #[test]
    fn wbfix_fix1_unowned_native_hang_keeps_admission_and_reports_later_failure() {
        use handy_tts_core::handoff::{PasteCompletion, PasteFence, PipelineFence};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let fence = Arc::new(PasteFence::default());
        let pipeline = Arc::new(Mutex::new(PipelineFence::default()));
        let generation = pipeline.lock().unwrap().begin();
        pipeline.lock().unwrap().queued(generation);
        let pending = pipeline.clone();
        let (settled, finished) = mpsc::channel();
        let completion = PasteCompletion::with_timeout(Duration::from_millis(50), move || {
            let mut state = pending.lock().unwrap();
            state.pasted(generation);
            settled.send(state.finish(generation)).unwrap();
        });
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let native_join = Arc::new(Mutex::new(None));
        let native_worker = native_join.clone();
        let observed = trace.clone();
        let old = fence.clone();
        let first = std::thread::spawn(move || {
            old.queue_native(
                Arc::new(|job| {
                    job();
                    Ok(())
                }),
                Box::new(move || {
                    let _completion = completion;
                    let _ = with_paste_lock(|| {
                        queued_input(
                            |job| {
                                *native_worker.lock().unwrap() = Some(std::thread::spawn(job));
                                Ok(())
                            },
                            move || {
                                entered.send(()).unwrap();
                                let _ = blocked.recv_timeout(Duration::from_secs(3));
                                observed.lock().unwrap().push("old finished");
                                Ok(())
                            },
                            Duration::from_millis(20),
                        )
                    });
                }),
                Arc::new(|e| panic!("first admission failed: {e}")),
            );
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(finished.recv_timeout(Duration::from_secs(2)).unwrap());
        let new_generation = pipeline.lock().unwrap().begin();
        let recorder_started = std::cell::Cell::new(false);
        let recording_usable = new_generation != generation
            && crate::actions::start_after_read_aloud(
                || Ok(()),
                || {
                    recorder_started.set(true);
                    true
                },
            );
        let began = std::time::Instant::now();
        let captured = fence.reserve_capture();
        if captured {
            fence.release_capture();
        }
        let capture_elapsed = began.elapsed();
        let called = Arc::new(AtomicBool::new(false));
        let input = called.clone();
        let notices = Arc::new(Mutex::new(Vec::new()));
        let displayed = notices.clone();
        let completed = Arc::new(AtomicBool::new(false));
        let done = completed.clone();
        let began = std::time::Instant::now();
        fence.queue_native(
            Arc::new(|job| {
                job();
                Ok(())
            }),
            Box::new(move || {
                with_paste_lock(|| {
                    input.store(true, Ordering::Release);
                    Ok(())
                })
                .unwrap();
            }),
            Arc::new(move |_| {
                crate::actions::paste_admission_failed_with(
                    |notice| displayed.lock().unwrap().push(notice),
                    || done.store(true, Ordering::Release),
                )
            }),
        );
        let refusal_elapsed = began.elapsed();
        let _ = release.send(());
        first.join().unwrap();
        native_join.lock().unwrap().take().unwrap().join().unwrap();
        let third = trace.clone();
        fence.queue_native(
            Arc::new(|job| {
                job();
                Ok(())
            }),
            Box::new(move || {
                with_paste_lock(|| {
                    third.lock().unwrap().push("third admitted");
                    Ok(())
                })
                .unwrap();
            }),
            Arc::new(|e| panic!("third admission failed: {e}")),
        );
        assert!(
            recording_usable && recorder_started.get(),
            "watchdog must leave recording usable"
        );
        assert!(
            !captured && capture_elapsed < Duration::from_millis(100),
            "unkillable input must retain prompt capture refusal"
        );
        assert!(
            !called.load(Ordering::Acquire),
            "later paste must remain refused while native input runs"
        );
        assert!(refusal_elapsed < Duration::from_millis(100));
        assert!(
            completed.load(Ordering::Acquire),
            "rejected paste must settle processing"
        );
        assert_eq!(
            notices.lock().unwrap().as_slice(),
            [crate::notices::Notice::new(
                crate::notices::NoticeCode::PasteFailed,
                crate::notices::NoticeSource::Dictation
            )],
            "rejected paste must show Paste Failed"
        );
        assert_eq!(*trace.lock().unwrap(), ["old finished", "third admitted"]);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn wbfix_fix1_hung_probe_reaps_and_releases_real_paste_admission() {
        use handy_tts_core::handoff::{Job, NativeQueue, PasteFence};
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let scratch =
            tempfile::tempdir_in(std::env::var_os("HANDY_TTS_TEST_SCRATCH").unwrap()).unwrap();
        let binary = scratch.path().join("ydotool");
        let pid_file = scratch.path().join("probe.pid");
        std::fs::write(
            &binary,
            r#"#!/usr/bin/python3
import os, pathlib, sys, time
assert sys.argv[1:3] == ['key', '--help']
pathlib.Path(sys.argv[3]).write_text(str(os.getpid()))
time.sleep(2)
print('Each key sequence: modifiers separated by plus (+)', file=sys.stderr)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fence = Arc::new(PasteFence::default());
        let (finished, wait) = mpsc::channel();
        let joins = Arc::new(Mutex::new(Vec::new()));
        let workers = joins.clone();
        let queue: NativeQueue = Arc::new(move |job: Job| {
            let finished = finished.clone();
            workers.lock().unwrap().push(std::thread::spawn(move || {
                job();
                finished.send(()).unwrap();
            }));
            Ok(())
        });
        let (result, receive) = mpsc::channel();
        let input_called = Arc::new(AtomicBool::new(false));
        let input = input_called.clone();
        let pid = pid_file.clone();
        let began = std::time::Instant::now();
        fence.queue_native(
            queue.clone(),
            Box::new(move || {
                let outcome = with_paste_lock(|| {
                    detect_ydotool_key_syntax_with(
                        Command::new(binary).args(["key", "--help"]).arg(pid),
                        &OnceLock::new(),
                        Duration::from_millis(250),
                    )?;
                    input.store(true, Ordering::Release);
                    Ok(())
                });
                result.send(outcome).unwrap();
            }),
            Arc::new(|error| panic!("first admission failed: {error}")),
        );
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        let elapsed = began.elapsed();
        let outcome = receive.recv().unwrap();
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let mut status = 0;
        // SAFETY: pid was written by the child started by this test.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let reaped =
            waited == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
        let reserved = fence.reserve_capture();
        if reserved {
            fence.release_capture();
        }
        let second_called = Arc::new(AtomicBool::new(false));
        let called = second_called.clone();
        fence.queue_native(
            queue,
            Box::new(move || {
                with_paste_lock(|| {
                    called.store(true, Ordering::Release);
                    Ok(())
                })
                .unwrap();
            }),
            Arc::new(|error| panic!("second admission failed: {error}")),
        );
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        for join in joins.lock().unwrap().drain(..) {
            join.join().unwrap();
        }
        assert!(
            outcome.is_err(),
            "probe deadline must abort the first paste"
        );
        assert!(
            !input_called.load(Ordering::Acquire),
            "expired probe must never continue to input"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "probe expiry must unwind the owned job promptly"
        );
        assert!(
            reaped,
            "probe child must be reaped before releasing admission"
        );
        assert!(
            reserved,
            "capture must reserve after the original job unwinds"
        );
        assert!(
            second_called.load(Ordering::Acquire),
            "the second paste callback must actually run"
        );
    }
    #[test]
    fn wbfix_timed_out_input_never_runs_later_on_the_main_thread() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let called = std::cell::Cell::new(false);
        assert!(input_if_current(&cancelled, || {
            called.set(true);
            Ok(())
        })
        .is_err());
        assert!(
            !called.get(),
            "expired input must never paste into a later session"
        );
    }
    #[cfg(unix)]
    #[test]
    fn wbfix_hung_owned_paste_tool_is_killed_and_reaped() {
        let began = std::time::Instant::now();
        let result = paste_tool_with_timeout(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            None,
            Duration::from_millis(60),
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(began.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn fix1_paste_adapter_acknowledges_only_synchronous_success_or_failure() {
        use handy_tts_core::handoff::{PasteCompletion, PipelineFence};
        use handy_tts_core::referee::{Effect, Event, Referee};
        let pipeline = std::sync::Arc::new(std::sync::Mutex::new(PipelineFence::default()));
        let referee = std::sync::Arc::new(std::sync::Mutex::new(Referee::default()));
        let generation = pipeline.lock().unwrap().begin();
        pipeline.lock().unwrap().queued(generation);
        referee.lock().unwrap().apply(Event::DictationStart);
        referee.lock().unwrap().apply(Event::DictationProcessing);
        let Effect::Capture { ticket } = referee.lock().unwrap().apply(Event::ReadShortcut)[0]
        else {
            panic!()
        };
        referee.lock().unwrap().apply(Event::Clipboard {
            ticket,
            text: "captured original".into(),
        });
        let native_settled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let settled = native_settled.clone();
        let pending = pipeline.clone();
        let popup = referee.clone();
        let completion = PasteCompletion::new(move || {
            assert!(settled.load(std::sync::atomic::Ordering::Acquire));
            let mut pending = pending.lock().unwrap();
            pending.pasted(generation);
            assert!(pending.finish(generation));
            assert_eq!(
                popup.lock().unwrap().apply(Event::DictationFinished),
                [Effect::OpenPopup {
                    text: "captured original".into()
                }]
            );
        });
        super::acknowledge_paste(Ok(true), &completion).unwrap();
        assert!(!pipeline.lock().unwrap().finish(generation));
        assert!(!referee.lock().unwrap().popup_open);
        completion.settle_with(|| native_settled.store(true, std::sync::atomic::Ordering::Release));
        completion.settle_with(|| panic!("native settlement runs exactly once"));
        assert!(referee.lock().unwrap().popup_open);
        for result in [Ok(false), Err("synchronous failure".to_string())] {
            let acknowledged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done = acknowledged.clone();
            let completion = PasteCompletion::new(move || {
                done.store(true, std::sync::atomic::Ordering::Release)
            });
            let _ = super::acknowledge_paste(result, &completion);
            assert!(acknowledged.load(std::sync::atomic::Ordering::Acquire));
        }
    }
    use super::*;
    use std::cell::Cell;

    #[cfg(target_os = "linux")]
    const YDOTOOL_0_1_8_HELP: &str = r#"
Usage: key [--delay <ms>] [--key-delay <ms>] [--repeat <times>] [--repeat-delay <ms>] <key sequence> ...
Each key sequence can be any number of modifiers and keys, separated by plus (+)
For example: alt+r Alt+F4 CTRL+alt+f3 aLT+1+2+3 ctrl+Backspace
"#;

    #[cfg(target_os = "linux")]
    const YDOTOOL_1_0_4_HELP: &str = r#"
Usage: key [OPTION]... [KEYCODES]...
Since there's no way to know how many keyboard layouts are there in the world,
we're using raw keycodes now.
Syntax: <keycode>:<pressed>
e.g. 28:1 28:0 means pressing on the Enter button on a standard US keyboard.
"#;

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_ydotool_0_1_8_symbolic_help() {
        assert_eq!(
            classify_ydotool_key_syntax(YDOTOOL_0_1_8_HELP),
            Some(YdotoolKeySyntax::Symbolic)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn classifies_ydotool_1_0_4_raw_keycode_help() {
        assert_eq!(
            classify_ydotool_key_syntax(YDOTOOL_1_0_4_HELP),
            Some(YdotoolKeySyntax::RawKeycodes)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_ydotool_help_falls_back_to_raw_keycodes() {
        let syntax = classify_ydotool_key_syntax("unrecognized help output")
            .unwrap_or(YDOTOOL_UNKNOWN_HELP_FALLBACK);

        assert_eq!(syntax, YdotoolKeySyntax::RawKeycodes);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generates_symbolic_ydotool_arguments_for_all_paste_methods() {
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlV, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "ctrl+v"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlShiftV, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "ctrl+shift+v"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::ShiftInsert, YdotoolKeySyntax::Symbolic).unwrap(),
            ["key", "shift+insert"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn generates_raw_ydotool_arguments_for_all_paste_methods() {
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlV, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "29:1", "47:1", "47:0", "29:0"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::CtrlShiftV, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]
        );
        assert_eq!(
            ydotool_key_args(&PasteMethod::ShiftInsert, YdotoolKeySyntax::RawKeycodes).unwrap(),
            ["key", "42:1", "110:1", "110:0", "42:0"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dotool_script_types_single_line_text_unchanged() {
        assert_eq!(dotool_type_script("hello world"), "type hello world\n");
        assert_eq!(dotool_type_script(" indented"), "type  indented\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dotool_script_does_not_run_lines_of_text_as_commands() {
        // dotool reads one command per line; text after a line break must still
        // be typed, not executed as `key`/`click` commands.
        assert_eq!(
            dotool_type_script("first line\nkey super+l\r\nclick left\n"),
            "type first line\nkey enter\ntype key super+l\nkey enter\ntype click left\nkey enter\n"
        );
    }

    #[test]
    fn auto_submit_requires_setting_enabled() {
        assert!(!should_send_auto_submit(false, PasteMethod::CtrlV));
        assert!(!should_send_auto_submit(false, PasteMethod::Direct));
    }

    #[test]
    fn auto_submit_skips_none_paste_method() {
        assert!(!should_send_auto_submit(true, PasteMethod::None));
    }

    #[test]
    fn auto_submit_runs_for_active_paste_methods() {
        assert!(should_send_auto_submit(true, PasteMethod::CtrlV));
        assert!(should_send_auto_submit(true, PasteMethod::Direct));
        assert!(should_send_auto_submit(true, PasteMethod::CtrlShiftV));
        assert!(should_send_auto_submit(true, PasteMethod::ShiftInsert));
    }

    #[test]
    fn clipboard_is_restored_before_key_injection_error_is_returned() {
        let restored = Cell::new(false);
        let result = finish_clipboard_paste(Err("input failed".into()), 0, || {
            restored.set(true);
        });

        assert_eq!(result.unwrap_err(), "input failed");
        assert!(restored.get());
    }

    #[cfg(unix)]
    #[test]
    fn external_script_does_not_wait_for_inherited_stdio() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::thread;

        let script_path = std::env::temp_dir().join(format!(
            "handy-external-script-{}-{}.sh",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        fs::write(&script_path, "#!/bin/sh\nsleep 3 &\nexit 0\n").expect("write external script");
        let mut permissions = fs::metadata(&script_path)
            .expect("read external script metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script_path, permissions).expect("make external script executable");

        let (sender, receiver) = mpsc::channel();
        let script_path_for_thread = script_path.clone();
        thread::spawn(move || {
            let result =
                paste_via_external_script("test", script_path_for_thread.to_str().unwrap());
            sender.send(result).expect("send script result");
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("external script should return without waiting for its child");
        fs::remove_file(script_path).expect("remove external script");
        assert!(result.is_ok());
    }
}
