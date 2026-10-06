use log::warn;
use std::io;
use std::process::{Child, Command};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};
const STOP_GRACE: Duration = Duration::from_secs(2);
struct SpawnJob {
    cmd: Command,
    reply: mpsc::Sender<io::Result<Child>>,
}

static SPAWNER: OnceLock<Mutex<mpsc::Sender<SpawnJob>>> = OnceLock::new();

/// Spawns from a persistent thread; Linux parent-death signals track the spawning thread.
pub fn spawn_from_supervisor(cmd: Command) -> io::Result<Child> {
    let sender = SPAWNER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<SpawnJob>();
        let started = std::thread::Builder::new()
            .name("handy-process-spawner".into())
            .spawn(move || {
                for mut job in rx {
                    let _ = job.reply.send(job.cmd.spawn());
                }
            });
        if let Err(e) = started {
            warn!("process-supervisor: cannot start the spawner thread: {e}");
        }
        Mutex::new(tx)
    });
    let (reply_tx, reply_rx) = mpsc::channel();
    sender
        .lock()
        .map_err(|_| io::Error::other("spawner lock poisoned"))?
        .send(SpawnJob {
            cmd,
            reply: reply_tx,
        })
        .map_err(|_| io::Error::other("spawner thread is gone"))?;
    reply_rx
        .recv()
        .map_err(|_| io::Error::other("spawner thread is gone"))?
}

/// SIGKILL the child if Handy dies without running its exit hook.
#[cfg(target_os = "linux")]
pub fn set_parent_death_signal(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: before exec, this calls only prctl/getppid and constructs non-allocating OS errors.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// SIGTERM, up to 2 s grace, then SIGKILL; always reaps.
pub fn stop_child(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: this unreaped child owns its PID, preventing PID reuse before kill(2).
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + STOP_GRACE;
        for _ in 0..(STOP_GRACE.as_millis() / 50) {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
