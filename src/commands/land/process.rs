//! Runs landing commands with live output, bounded capture, and a hard timeout.
//!
//! Children stay in Knit's foreground process group so terminal interrupts are
//! delivered naturally. On timeout we explicitly terminate the full descendant
//! tree before reaping the command.

use anyhow::{Context, Result};
#[cfg(unix)]
use std::collections::BTreeMap;
use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30 * 60;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const WAIT_INTERVAL: Duration = Duration::from_millis(50);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const TRUNCATION_NOTICE: &[u8] = b"[... earlier output truncated by Knit ...]\n";
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static ACTIVE_CHILDREN: OnceLock<Mutex<BTreeSet<u32>>> = OnceLock::new();
static CANCELLATION_HANDLER: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub(super) struct StreamedCommandOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) timed_out: bool,
    pub(super) cancelled: bool,
}

pub(super) fn begin_execution() -> Result<()> {
    ensure_cancellation_handler()?;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    Ok(())
}

pub(super) fn cancellation_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::SeqCst)
}

fn ensure_cancellation_handler() -> Result<()> {
    let installed = CANCELLATION_HANDLER.get_or_init(|| {
        ctrlc::set_handler(|| {
            let force = CANCEL_REQUESTED.swap(true, Ordering::SeqCst);
            terminate_active_children(force);
        })
        .map_err(|error| error.to_string())
    });
    match installed {
        Ok(()) => Ok(()),
        Err(error) => anyhow::bail!("failed to install landing cancellation handler: {error}"),
    }
}

pub(super) fn run_streamed(
    command: &mut Command,
    timeout_seconds: Option<u64>,
) -> Result<StreamedCommandOutput> {
    ensure_cancellation_handler()?;
    let timeout_seconds = timeout_seconds.unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn command")?;
    let active_child = ActiveChild::register(child.id());
    let stdout = child.stdout.take().expect("piped child stdout");
    let stderr = child.stderr.take().expect("piped child stderr");
    let stdout_reader = thread::spawn(move || tee_and_capture(stdout, io::stdout()));
    let stderr_reader = thread::spawn(move || tee_and_capture(stderr, io::stderr()));

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => {
                force_terminate_process_tree(&mut child);
                break Err(error).context("failed to poll command");
            }
        }
        if started.elapsed() >= Duration::from_secs(timeout_seconds) {
            timed_out = true;
            if let Err(error) = terminate_process_tree(&mut child) {
                force_terminate_process_tree(&mut child);
                break Err(error);
            }
            break child.wait().context("failed to reap timed-out command");
        }
        thread::sleep(WAIT_INTERVAL);
    };

    // Always drain and join both readers, including after a wait failure, so a
    // subprocess can never be left blocked on a full pipe.
    let cancelled = cancellation_requested();
    drop(active_child);
    let stdout = join_reader(stdout_reader, "stdout");
    let stderr = join_reader(stderr_reader, "stderr");
    Ok(StreamedCommandOutput {
        status: status?,
        stdout: stdout?,
        stderr: stderr?,
        timed_out,
        cancelled,
    })
}

struct ActiveChild {
    pid: u32,
}

impl ActiveChild {
    fn register(pid: u32) -> Self {
        active_children()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pid);
        Self { pid }
    }
}

impl Drop for ActiveChild {
    fn drop(&mut self) {
        active_children()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.pid);
    }
}

fn active_children() -> &'static Mutex<BTreeSet<u32>> {
    ACTIVE_CHILDREN.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn terminate_active_children(force: bool) {
    let roots = active_children()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .copied()
        .collect::<Vec<_>>();
    for root in roots {
        #[cfg(unix)]
        signal_unix_tree(root, &unix_descendants(root), force);
        #[cfg(windows)]
        terminate_windows_tree(root, force);
    }
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<String>>,
    stream_name: &str,
) -> Result<String> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("{stream_name} reader thread panicked"))?
        .with_context(|| format!("failed to read command {stream_name}"))
}

fn tee_and_capture<R: Read, W: Write>(mut reader: R, mut live: W) -> io::Result<String> {
    let mut capture = BoundedCapture::new(MAX_CAPTURE_BYTES);
    let mut buffer = [0_u8; 8192];
    let mut live_open = true;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        capture.push(chunk);
        if live_open && live.write_all(chunk).and_then(|_| live.flush()).is_err() {
            // A closed UI/terminal stream must not stop us draining the child
            // pipe; doing so could deadlock a verbose deployment.
            live_open = false;
        }
    }
    Ok(capture.into_string())
}

struct BoundedCapture {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl BoundedCapture {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.limit {
            self.bytes.clear();
            self.bytes.extend(
                chunk[chunk.len().saturating_sub(self.limit)..]
                    .iter()
                    .copied(),
            );
            self.truncated = true;
            return;
        }
        let excess = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.limit);
        if excess > 0 {
            self.bytes.drain(..excess);
            self.truncated = true;
        }
        self.bytes.extend(chunk.iter().copied());
    }

    fn into_string(self) -> String {
        let mut bytes = Vec::with_capacity(
            self.bytes.len()
                + if self.truncated {
                    TRUNCATION_NOTICE.len()
                } else {
                    0
                },
        );
        if self.truncated {
            bytes.extend_from_slice(TRUNCATION_NOTICE);
        }
        bytes.extend(self.bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn terminate_process_tree(child: &mut Child) -> Result<()> {
    #[cfg(unix)]
    let descendants = unix_descendants(child.id());
    #[cfg(unix)]
    signal_unix_tree(child.id(), &descendants, false);
    #[cfg(windows)]
    terminate_windows_tree(child.id(), false);

    let deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < deadline {
        let child_exited = child
            .try_wait()
            .context("failed to poll command during termination")?
            .is_some();
        #[cfg(unix)]
        let descendants_exited = descendants.iter().all(|pid| !unix_process_is_running(*pid));
        #[cfg(windows)]
        let descendants_exited = child_exited;
        if child_exited && descendants_exited {
            return Ok(());
        }
        thread::sleep(WAIT_INTERVAL);
    }

    #[cfg(unix)]
    {
        let mut remaining = descendants;
        remaining.extend(unix_descendants(child.id()));
        remaining.sort_unstable();
        remaining.dedup();
        signal_unix_tree(child.id(), &remaining, true);
    }
    #[cfg(windows)]
    terminate_windows_tree(child.id(), true);
    Ok(())
}

fn force_terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    signal_unix_tree(child.id(), &unix_descendants(child.id()), true);
    #[cfg(windows)]
    terminate_windows_tree(child.id(), true);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn signal_unix_tree(root: u32, descendants: &[u32], force: bool) {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    for &pid in descendants.iter().rev().chain(std::iter::once(&root)) {
        // SAFETY: kill does not dereference memory; every pid came from the OS
        // process table (or the Child handle), and failures are harmless races
        // with processes that already exited.
        unsafe {
            libc::kill(pid as i32, signal);
        }
    }
}

#[cfg(unix)]
fn unix_process_is_running(pid: u32) -> bool {
    // SAFETY: signal 0 only asks the kernel to check whether this pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
fn unix_descendants(root: u32) -> Vec<u32> {
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(parent)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(pid), Ok(parent)) = (pid.parse::<u32>(), parent.parse::<u32>()) else {
            continue;
        };
        children.entry(parent).or_default().push(pid);
    }
    let mut descendants = Vec::new();
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        if let Some(direct) = children.get(&parent) {
            for &pid in direct {
                descendants.push(pid);
                pending.push(pid);
            }
        }
    }
    descendants
}

#[cfg(windows)]
fn terminate_windows_tree(root: u32, force: bool) {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &root.to_string(), "/T"]);
    if force {
        command.arg("/F");
    }
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_capture_keeps_only_the_tail() {
        let mut capture = BoundedCapture::new(5);
        capture.push(b"abc");
        capture.push(b"defgh");
        let output = capture.into_string();
        assert!(output.starts_with(String::from_utf8_lossy(TRUNCATION_NOTICE).as_ref()));
        assert!(output.ends_with("defgh"));
        assert!(!output.ends_with("abcdefgh"));
    }

    #[cfg(unix)]
    #[test]
    fn streamed_command_times_out_and_is_reaped() {
        let pid_file = std::env::temp_dir().join(format!(
            "knit-timeout-child-{}-{:?}.pid",
            std::process::id(),
            thread::current().id()
        ));
        let started = Instant::now();
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf ready; sleep 20 & echo $! > \"$KNIT_TEST_PID_FILE\"; wait",
            ])
            .env("KNIT_TEST_PID_FILE", &pid_file);
        let output = run_streamed(&mut command, Some(1)).unwrap();
        assert!(output.timed_out);
        assert!(!output.status.success());
        assert_eq!(output.stdout, "ready");
        assert!(started.elapsed() < Duration::from_secs(5));
        let descendant_pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let descendants_deadline = Instant::now() + Duration::from_secs(2);
        while unix_process_is_running(descendant_pid) && Instant::now() < descendants_deadline {
            thread::sleep(WAIT_INTERVAL);
        }
        assert!(!unix_process_is_running(descendant_pid));
        let _ = std::fs::remove_file(pid_file);
    }

    #[test]
    fn tee_keeps_capturing_after_the_live_writer_closes() {
        struct ClosedWriter;
        impl Write for ClosedWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let output = tee_and_capture(&b"complete output"[..], ClosedWriter).unwrap();
        assert_eq!(output, "complete output");
    }
}
