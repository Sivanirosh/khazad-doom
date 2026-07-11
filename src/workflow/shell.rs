use super::CancelledError;
use crate::agent::CancellationToken;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const PROGRESS_OUTPUT_TAIL_BYTES: usize = 4_000;
pub(crate) const COMMAND_SUPERVISOR_ARG: &str = "__khazad_command_supervisor_v1";
const COMMAND_SUPERVISOR_RESULT_MAGIC: &[u8] = b"KHAZAD-SUPERVISOR-RESULT-V1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellFailureKind {
    Spawn,
    Timeout,
    Supervision,
}

impl ShellFailureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_failed",
            Self::Timeout => "timeout",
            Self::Supervision => "process_supervision_failed",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ShellCommandError {
    kind: ShellFailureKind,
    message: String,
}

impl ShellCommandError {
    pub(crate) fn new(kind: ShellFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn kind(&self) -> ShellFailureKind {
        self.kind
    }
}

impl std::fmt::Display for ShellCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ShellCommandError {}

pub(crate) type ShellProgress = Arc<dyn Fn(String) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy)]
pub(crate) enum GracefulSignalTarget {
    #[allow(dead_code)] // Production shell supervision signals its authenticated root process.
    Process,
    ProcessGroup,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessSupervisionPolicy {
    pub graceful_signal: i32,
    pub graceful_target: GracefulSignalTarget,
    pub grace: Duration,
    pub poll_interval: Duration,
}

impl ProcessSupervisionPolicy {
    pub(crate) fn process_group(grace: Duration) -> Self {
        Self {
            graceful_signal: libc::SIGTERM,
            graceful_target: GracefulSignalTarget::ProcessGroup,
            grace,
            poll_interval: Duration::from_millis(50),
        }
    }
}

/// Shared TERM/grace/KILL/reap policy for Pi, repair, verification, and
/// cancellable Git process trees. Unix callers create a dedicated process
/// group first; other platforms use the strongest child-only fallback exposed
/// by `std::process::Child`.
pub(crate) fn terminate_child_tree(
    child: &mut std::process::Child,
    policy: ProcessSupervisionPolicy,
) {
    let pid = child.id();
    #[cfg(unix)]
    {
        signal_supervised_process(pid, policy.graceful_signal, policy.graceful_target);
        let deadline = Instant::now() + policy.grace;
        loop {
            let child_exited = matches!(child.try_wait(), Ok(Some(_)));
            if !supervised_process_group_exists(pid) {
                if !child_exited {
                    let _ = child.wait();
                }
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(
                policy
                    .poll_interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        signal_supervised_process(pid, libc::SIGKILL, GracefulSignalTarget::ProcessGroup);
        let _ = child.wait();
    }
    #[cfg(not(unix))]
    {
        let _ = policy;
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn terminate_process_group_until(
    pgid: u32,
    policy: ProcessSupervisionPolicy,
    mut complete: impl FnMut() -> bool,
) {
    #[cfg(unix)]
    {
        signal_supervised_process(pgid, policy.graceful_signal, policy.graceful_target);
        let deadline = Instant::now() + policy.grace;
        while Instant::now() < deadline {
            if complete() && !supervised_process_group_exists(pgid) {
                return;
            }
            thread::sleep(
                policy
                    .poll_interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        signal_supervised_process(pgid, libc::SIGKILL, GracefulSignalTarget::ProcessGroup);
    }
    #[cfg(not(unix))]
    {
        // A wrapper handoff has no portable child handle after daemon restart;
        // retain the bounded wait while direct children use `Child::kill` above.
        let deadline = Instant::now() + policy.grace;
        while Instant::now() < deadline && !complete() {
            thread::sleep(policy.poll_interval);
        }
    }
}

pub(crate) fn terminate_remaining_process_group(pgid: u32, grace: Duration) {
    #[cfg(unix)]
    {
        signal_supervised_process(pgid, libc::SIGTERM, GracefulSignalTarget::ProcessGroup);
        let deadline = Instant::now() + grace;
        while supervised_process_group_exists(pgid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if supervised_process_group_exists(pgid) {
            signal_supervised_process(pgid, libc::SIGKILL, GracefulSignalTarget::ProcessGroup);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, grace);
    }
}

#[cfg(unix)]
fn signal_supervised_process(pid: u32, signal: i32, target: GracefulSignalTarget) {
    let target = match target {
        GracefulSignalTarget::Process => pid as i32,
        GracefulSignalTarget::ProcessGroup => -(pid as i32),
    };
    unsafe {
        let _ = libc::kill(target, signal);
    }
}

#[cfg(unix)]
pub(crate) fn supervised_process_group_exists(pgid: u32) -> bool {
    let status = unsafe { libc::kill(-(pgid as i32), 0) };
    status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

pub(crate) struct ShellCommand {
    cwd: PathBuf,
    command: String,
    timeout: Duration,
    termination_grace: Duration,
    env: BTreeMap<OsString, OsString>,
    env_remove: Vec<OsString>,
    progress: Option<ShellProgress>,
    pinned_cwd: Option<File>,
    retained_output_bytes: usize,
    retained_output_lines: usize,
    spill_stem: Option<PathBuf>,
}

impl ShellCommand {
    pub(crate) fn new(cwd: impl AsRef<Path>, command: impl Into<String>) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            command: command.into(),
            timeout: Duration::ZERO,
            termination_grace: Duration::from_secs(1),
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            progress: None,
            pinned_cwd: None,
            retained_output_bytes: 64 * 1024,
            retained_output_lines: 1_000,
            spill_stem: None,
        }
    }

    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) fn termination_grace(mut self, grace: Duration) -> Self {
        self.termination_grace = grace;
        self
    }

    pub(crate) fn envs(mut self, env: &BTreeMap<String, String>) -> Self {
        self.env = env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        self
    }

    pub(crate) fn envs_os(mut self, env: BTreeMap<OsString, OsString>) -> Self {
        self.env = env;
        self
    }

    pub(crate) fn env_remove(mut self, names: &[&OsStr]) -> Self {
        self.env_remove
            .extend(names.iter().map(|name| (*name).to_os_string()));
        self
    }

    pub(crate) fn progress(mut self, progress: Option<ShellProgress>) -> Self {
        self.progress = progress;
        self
    }

    pub(crate) fn output_bounds(mut self, retained_bytes: usize, retained_lines: usize) -> Self {
        self.retained_output_bytes = retained_bytes;
        self.retained_output_lines = retained_lines;
        self
    }

    pub(crate) fn spill_to(mut self, stem: PathBuf) -> Self {
        self.spill_stem = Some(stem);
        self
    }

    pub(crate) fn pinned_cwd(mut self, directory: &File) -> Result<Self> {
        self.pinned_cwd = Some(directory.try_clone().context("duplicate verified cwd")?);
        Ok(self)
    }

    pub(crate) fn run(self, cancel: &CancellationToken) -> Result<ShellOutput> {
        let mut supervision = SupervisionPipe::new()?;
        let mut supervisor_result = SupervisorResultPipe::new()?;
        #[cfg(test)]
        let mut process = {
            let mut process = Command::new("sh");
            process.arg("-c").arg(&self.command);
            process
        };
        #[cfg(not(test))]
        let mut process = {
            let mut process = Command::new(std::env::current_exe().map_err(|err| {
                ShellCommandError::new(
                    ShellFailureKind::Supervision,
                    format!("locate verification command supervisor: {err}"),
                )
            })?);
            process
                .arg(COMMAND_SUPERVISOR_ARG)
                .arg(supervisor_result.write_fd().to_string())
                .arg(&self.command);
            process
        };
        if self.pinned_cwd.is_none() {
            process.current_dir(&self.cwd);
        }
        for name in &self.env_remove {
            process.env_remove(name);
        }
        process
            .envs(&self.env)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Create a process group for the shell and its children so cancellation can
        // kill a hanging verify/gate command instead of only killing the shell.
        let supervision_read_fd = supervision.read_fd();
        let supervision_write_fd = supervision.write_fd();
        let supervisor_result_read_fd = supervisor_result.read_fd();
        let supervisor_result_write_fd = supervisor_result.write_fd();
        let pinned_cwd = self.pinned_cwd;
        let pinned_cwd_fd = pinned_cwd.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1);
        unsafe {
            process.pre_exec(move || {
                if pinned_cwd_fd >= 0 && libc::fchdir(pinned_cwd_fd) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if supervision_read_fd >= 0 {
                    libc::close(supervision_read_fd);
                }
                if supervision_write_fd >= 0
                    && libc::fcntl(supervision_write_fd, libc::F_SETFD, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                if supervisor_result_read_fd >= 0 {
                    libc::close(supervisor_result_read_fd);
                }
                if supervisor_result_write_fd >= 0
                    && libc::fcntl(supervisor_result_write_fd, libc::F_SETFD, 0) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = process.spawn().map_err(|err| {
            ShellCommandError::new(
                ShellFailureKind::Spawn,
                format!(
                    "failed to start verify shell in {}: {err}",
                    self.cwd.display()
                ),
            )
        })?;
        supervision.parent_after_spawn();
        supervisor_result.parent_after_spawn();
        let stdout = child.stdout.take().context("command stdout")?;
        let stderr = child.stderr.take().context("command stderr")?;
        let monitor = match ShellCommandMonitor::spawn(
            stdout,
            stderr,
            self.progress,
            self.retained_output_bytes,
            self.retained_output_lines,
            self.spill_stem.as_deref(),
        ) {
            Ok(monitor) => monitor,
            Err(err) => {
                terminate_process_group(&mut child, self.termination_grace);
                let cleanup = terminate_supervised_descendants(supervision.identity());
                let supervisor = finish_supervisor_result(&mut supervisor_result);
                cleanup?;
                supervisor?;
                return Err(err);
            }
        };

        let started_at = Instant::now();
        let mut last_heartbeat = Instant::now();
        let status = loop {
            if cancel.is_cancelled() {
                terminate_process_group(&mut child, self.termination_grace);
                let cleanup = terminate_supervised_descendants(supervision.identity());
                let capture = monitor.finish();
                finish_supervisor_result(&mut supervisor_result)?;
                cleanup?;
                capture?;
                return Err(CancelledError::new("run cancelled").into());
            }
            if !self.timeout.is_zero() && started_at.elapsed() >= self.timeout {
                terminate_process_group(&mut child, self.termination_grace);
                let cleanup = terminate_supervised_descendants(supervision.identity());
                let capture = monitor.finish();
                finish_supervisor_result(&mut supervisor_result)?;
                cleanup?;
                capture?;
                return Err(ShellCommandError::new(
                    ShellFailureKind::Timeout,
                    format!("command timed out after {} seconds", self.timeout.as_secs()),
                )
                .into());
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if last_heartbeat.elapsed() >= Duration::from_secs(5) {
                monitor.emit_progress();
                last_heartbeat = Instant::now();
            }
            thread::sleep(Duration::from_millis(100));
        };
        terminate_remaining_process_group(
            child.id(),
            self.termination_grace.min(Duration::from_secs(1)),
        );
        let cleanup = terminate_supervised_descendants(supervision.identity());
        let (stdout, stderr) = monitor.finish()?;
        finish_supervisor_result(&mut supervisor_result)?;
        cleanup?;
        if status.code().is_none() {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                "verification command supervisor terminated by signal",
            )
            .into());
        }
        if cancel.is_cancelled() {
            return Err(CancelledError::new("run cancelled").into());
        }
        Ok(ShellOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_total_bytes: stdout.total_bytes,
            stderr_total_bytes: stderr.total_bytes,
            stdout_spill_path: stdout.spill_path,
            stderr_spill_path: stderr.spill_path,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ShellOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_total_bytes: usize,
    stderr_total_bytes: usize,
    stdout_spill_path: Option<PathBuf>,
    stderr_spill_path: Option<PathBuf>,
}

impl ShellOutput {
    pub(crate) fn success(&self) -> bool {
        self.success
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    #[cfg(test)]
    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[cfg(test)]
    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    pub(crate) fn stdout_total_bytes(&self) -> usize {
        self.stdout_total_bytes
    }

    pub(crate) fn stderr_total_bytes(&self) -> usize {
        self.stderr_total_bytes
    }

    pub(crate) fn retained_output_bytes(&self) -> usize {
        self.stdout.len().saturating_add(self.stderr.len())
    }

    pub(crate) fn output_truncated(&self) -> bool {
        self.stdout_total_bytes > self.stdout.len() || self.stderr_total_bytes > self.stderr.len()
    }

    pub(crate) fn stdout_spill_path(&self) -> Option<&Path> {
        self.stdout_spill_path.as_deref()
    }

    pub(crate) fn stderr_spill_path(&self) -> Option<&Path> {
        self.stderr_spill_path.as_deref()
    }

    pub(crate) fn combined_output(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        )
    }

    pub(crate) fn trimmed_combined_output(&self) -> String {
        self.combined_output().trim().to_string()
    }
}

#[derive(Debug)]
struct BoundedStreamCapture {
    bytes: Vec<u8>,
    total_bytes: usize,
    retained_bytes: usize,
    retained_lines: usize,
}

impl BoundedStreamCapture {
    fn new(retained_bytes: usize, retained_lines: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(retained_bytes.min(64 * 1024)),
            total_bytes: 0,
            retained_bytes,
            retained_lines,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        self.bytes.extend_from_slice(bytes);
        if self.retained_bytes == 0 || self.retained_lines == 0 {
            self.bytes.clear();
            return;
        }
        if self.bytes.len() > self.retained_bytes {
            let remove = self.bytes.len() - self.retained_bytes;
            self.bytes.drain(0..remove);
        }
        while logical_line_count(&self.bytes) > self.retained_lines {
            let Some(newline) = self.bytes.iter().position(|byte| *byte == b'\n') else {
                self.bytes.clear();
                break;
            };
            self.bytes.drain(0..=newline);
        }
    }
}

fn logical_line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
}

struct CapturedStream {
    bytes: Vec<u8>,
    total_bytes: usize,
    spill_path: Option<PathBuf>,
}

struct ShellCommandMonitor {
    stdout_buf: Arc<Mutex<BoundedStreamCapture>>,
    stderr_buf: Arc<Mutex<BoundedStreamCapture>>,
    combined_tail: Arc<Mutex<Vec<u8>>>,
    progress: Option<ShellProgress>,
    stdout_thread: thread::JoinHandle<Option<String>>,
    stderr_thread: thread::JoinHandle<Option<String>>,
    stdout_spill_path: Option<PathBuf>,
    stderr_spill_path: Option<PathBuf>,
}

impl ShellCommandMonitor {
    fn spawn(
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        progress: Option<ShellProgress>,
        retained_bytes: usize,
        retained_lines: usize,
        spill_stem: Option<&Path>,
    ) -> Result<Self> {
        let stdout_spill_path = spill_stem.map(|stem| stem.with_extension("stdout.log"));
        let stderr_spill_path = spill_stem.map(|stem| stem.with_extension("stderr.log"));
        let stdout_spill_existed = stdout_spill_path.as_deref().is_some_and(Path::exists);
        let stdout_spill = open_output_spill(stdout_spill_path.as_deref()).map_err(|err| {
            ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!("output spill setup failed: {err:#}"),
            )
        })?;
        let stderr_spill = match open_output_spill(stderr_spill_path.as_deref()) {
            Ok(spill) => spill,
            Err(err) => {
                drop(stdout_spill);
                if !stdout_spill_existed && let Some(path) = stdout_spill_path.as_deref() {
                    let _ = std::fs::remove_file(path);
                }
                return Err(ShellCommandError::new(
                    ShellFailureKind::Supervision,
                    format!("output spill setup failed: {err:#}"),
                )
                .into());
            }
        };
        let stdout_buf = Arc::new(Mutex::new(BoundedStreamCapture::new(
            retained_bytes,
            retained_lines,
        )));
        let stderr_buf = Arc::new(Mutex::new(BoundedStreamCapture::new(
            retained_bytes,
            retained_lines,
        )));
        let combined_tail = Arc::new(Mutex::new(Vec::new()));
        let stdout_thread = spawn_output_reader(
            stdout,
            stdout_buf.clone(),
            combined_tail.clone(),
            progress.clone(),
            stdout_spill,
        );
        let stderr_thread = spawn_output_reader(
            stderr,
            stderr_buf.clone(),
            combined_tail.clone(),
            progress.clone(),
            stderr_spill,
        );
        Ok(Self {
            stdout_buf,
            stderr_buf,
            combined_tail,
            progress,
            stdout_thread,
            stderr_thread,
            stdout_spill_path,
            stderr_spill_path,
        })
    }

    fn emit_progress(&self) {
        if let Some(progress) = &self.progress {
            progress(tail_text(&self.combined_tail));
        }
    }

    fn finish(self) -> Result<(CapturedStream, CapturedStream)> {
        let Self {
            stdout_buf,
            stderr_buf,
            stdout_thread,
            stderr_thread,
            stdout_spill_path,
            stderr_spill_path,
            ..
        } = self;
        let stdout_error = stdout_thread
            .join()
            .map_err(|_| anyhow::anyhow!("stdout capture thread panicked"))?;
        let stderr_error = stderr_thread
            .join()
            .map_err(|_| anyhow::anyhow!("stderr capture thread panicked"))?;
        if let Some(error) = stdout_error.or(stderr_error) {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!("output spill failed: {error}"),
            )
            .into());
        }
        let stdout = Arc::try_unwrap(stdout_buf)
            .map_err(|_| anyhow::anyhow!("stdout capture still referenced"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("stdout capture mutex poisoned"))?;
        let stderr = Arc::try_unwrap(stderr_buf)
            .map_err(|_| anyhow::anyhow!("stderr capture still referenced"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("stderr capture mutex poisoned"))?;
        Ok((
            CapturedStream {
                bytes: stdout.bytes,
                total_bytes: stdout.total_bytes,
                spill_path: stdout_spill_path,
            },
            CapturedStream {
                bytes: stderr.bytes,
                total_bytes: stderr.total_bytes,
                spill_path: stderr_spill_path,
            },
        ))
    }
}

fn open_output_spill(path: Option<&Path>) -> Result<Option<File>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create output spill directory {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open output spill {}", path.display()))
        .map(Some)
}

fn spawn_output_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream_buf: Arc<Mutex<BoundedStreamCapture>>,
    combined_tail: Arc<Mutex<Vec<u8>>>,
    progress: Option<ShellProgress>,
    mut spill: Option<File>,
) -> thread::JoinHandle<Option<String>> {
    thread::spawn(move || {
        let mut spill_error = None;
        let mut buf = [0_u8; 4096];
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        loop {
            let read = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) => return Some(format!("read command output: {err}")),
            };
            if let Some(file) = spill.as_mut()
                && let Err(err) = file.write_all(&buf[..read])
            {
                spill_error = Some(err.to_string());
                spill = None;
            }
            stream_buf
                .lock()
                .expect("stream mutex poisoned")
                .push(&buf[..read]);
            let output_tail = {
                let mut combined = combined_tail.lock().expect("combined mutex poisoned");
                combined.extend_from_slice(&buf[..read]);
                if combined.len() > PROGRESS_OUTPUT_TAIL_BYTES {
                    let remove = combined.len() - PROGRESS_OUTPUT_TAIL_BYTES;
                    combined.drain(0..remove);
                }
                String::from_utf8_lossy(&combined).to_string()
            };
            if let Some(progress) = &progress
                && last_emit.elapsed() >= Duration::from_millis(500)
            {
                progress(output_tail);
                last_emit = Instant::now();
            }
        }
        if let Some(progress) = &progress {
            progress(tail_text(&combined_tail));
        }
        spill_error
    })
}

fn tail_text(combined_tail: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&combined_tail.lock().expect("combined mutex poisoned")).to_string()
}

static SUPERVISOR_TERMINATE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "C" fn request_supervisor_termination(_signal: i32) {
    SUPERVISOR_TERMINATE.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn run_command_supervisor(command: &str) -> Result<i32> {
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("enable verification command subreaper");
        }
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("protect verification command subreaper state");
        }
    }
    SUPERVISOR_TERMINATE.store(false, std::sync::atomic::Ordering::SeqCst);
    install_supervisor_signal_handler(libc::SIGUSR1)?;
    install_supervisor_signal_handler(libc::SIGTERM)?;
    install_supervisor_signal_handler(libc::SIGINT)?;
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    #[cfg(target_os = "linux")]
    {
        let signal_filter = command_signal_filter(std::process::id());
        unsafe {
            shell.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                install_command_signal_filter(&signal_filter)
            });
        }
    }
    let mut child = shell
        .spawn()
        .context("spawn supervised verification shell")?;
    loop {
        if SUPERVISOR_TERMINATE.load(std::sync::atomic::Ordering::SeqCst) {
            terminate_supervisor_descendants()?;
            let _ = child.wait();
            return Ok(128 + libc::SIGTERM);
        }
        if let Some(status) = child.try_wait()? {
            terminate_supervisor_descendants()?;
            use std::os::unix::process::ExitStatusExt;
            return Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(libc::SIGKILL)));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn command_signal_filter(supervisor_pid: u32) -> Vec<libc::sock_filter> {
    const LOAD_WORD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JUMP_EQUAL: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const JUMP_SET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
    const RETURN: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    const SYSCALL_OFFSET: u32 = 0;
    const ARCHITECTURE_OFFSET: u32 = 4;
    const ARGUMENT_ZERO_LOW_OFFSET: u32 = 16;
    let allow = libc::SECCOMP_RET_ALLOW;
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let kill = libc::SECCOMP_RET_KILL_PROCESS;
    let negative_supervisor_pid = (-(supervisor_pid as i32)) as u32;
    let mut filter = vec![
        libc::sock_filter {
            code: LOAD_WORD,
            jt: 0,
            jf: 0,
            k: ARCHITECTURE_OFFSET,
        },
        libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 1,
            jf: 0,
            k: native_linux_audit_architecture(),
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: kill,
        },
        libc::sock_filter {
            code: LOAD_WORD,
            jt: 0,
            jf: 0,
            k: SYSCALL_OFFSET,
        },
    ];
    #[cfg(target_arch = "x86_64")]
    filter.extend([
        libc::sock_filter {
            code: JUMP_SET,
            jt: 0,
            jf: 1,
            k: 0x4000_0000,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: kill,
        },
    ]);
    for syscall in [
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_prlimit64,
        libc::SYS_pidfd_open,
    ] {
        filter.extend([
            libc::sock_filter {
                code: JUMP_EQUAL,
                jt: 0,
                jf: 6,
                k: syscall as u32,
            },
            libc::sock_filter {
                code: LOAD_WORD,
                jt: 0,
                jf: 0,
                k: ARGUMENT_ZERO_LOW_OFFSET,
            },
            libc::sock_filter {
                code: JUMP_EQUAL,
                jt: 3,
                jf: 0,
                k: supervisor_pid,
            },
            libc::sock_filter {
                code: JUMP_EQUAL,
                jt: 2,
                jf: 0,
                k: negative_supervisor_pid,
            },
            libc::sock_filter {
                code: JUMP_EQUAL,
                jt: 1,
                jf: 0,
                k: u32::MAX,
            },
            libc::sock_filter {
                code: RETURN,
                jt: 0,
                jf: 0,
                k: allow,
            },
            libc::sock_filter {
                code: RETURN,
                jt: 0,
                jf: 0,
                k: deny,
            },
        ]);
    }
    filter.push(libc::sock_filter {
        code: RETURN,
        jt: 0,
        jf: 0,
        k: allow,
    });
    filter
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const fn native_linux_audit_architecture() -> u32 {
    0xc000_003e
}

#[cfg(all(target_os = "linux", target_arch = "x86"))]
const fn native_linux_audit_architecture() -> u32 {
    0x4000_0003
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const fn native_linux_audit_architecture() -> u32 {
    0xc000_00b7
}

#[cfg(all(target_os = "linux", target_arch = "arm"))]
const fn native_linux_audit_architecture() -> u32 {
    0x4000_0028
}

#[cfg(all(target_os = "linux", target_arch = "riscv64"))]
const fn native_linux_audit_architecture() -> u32 {
    0xc000_00f3
}

#[cfg(all(target_os = "linux", target_arch = "riscv32"))]
const fn native_linux_audit_architecture() -> u32 {
    0x4000_00f3
}

#[cfg(all(
    target_os = "linux",
    target_arch = "powerpc64",
    target_endian = "little"
))]
const fn native_linux_audit_architecture() -> u32 {
    0xc000_0015
}

#[cfg(all(target_os = "linux", target_arch = "powerpc64", target_endian = "big"))]
const fn native_linux_audit_architecture() -> u32 {
    0x8000_0015
}

#[cfg(all(target_os = "linux", target_arch = "powerpc"))]
const fn native_linux_audit_architecture() -> u32 {
    0x0000_0014
}

#[cfg(all(target_os = "linux", target_arch = "s390x"))]
const fn native_linux_audit_architecture() -> u32 {
    0x8000_0016
}

#[cfg(all(target_os = "linux", target_arch = "mips", target_endian = "little"))]
const fn native_linux_audit_architecture() -> u32 {
    0x4000_0008
}

#[cfg(all(target_os = "linux", target_arch = "mips", target_endian = "big"))]
const fn native_linux_audit_architecture() -> u32 {
    0x0000_0008
}

#[cfg(all(target_os = "linux", target_arch = "mips64", target_endian = "little"))]
const fn native_linux_audit_architecture() -> u32 {
    0xc000_0008
}

#[cfg(all(target_os = "linux", target_arch = "mips64", target_endian = "big"))]
const fn native_linux_audit_architecture() -> u32 {
    0x8000_0008
}

#[cfg(all(target_os = "linux", target_arch = "loongarch64"))]
const fn native_linux_audit_architecture() -> u32 {
    0xc000_0102
}

#[cfg(all(target_os = "linux", target_arch = "sparc64"))]
const fn native_linux_audit_architecture() -> u32 {
    0x8000_002b
}

#[cfg(all(target_os = "linux", target_arch = "sparc"))]
const fn native_linux_audit_architecture() -> u32 {
    0x0000_0002
}

#[cfg(all(
    target_os = "linux",
    not(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "riscv64",
        target_arch = "riscv32",
        target_arch = "powerpc64",
        target_arch = "powerpc",
        target_arch = "s390x",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "loongarch64",
        target_arch = "sparc64",
        target_arch = "sparc"
    ))
))]
compile_error!("the Linux command supervisor needs an AUDIT_ARCH mapping for this target");

#[cfg(target_os = "linux")]
fn install_command_signal_filter(filter: &[libc::sock_filter]) -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let program = libc::sock_fprog {
        len: filter.len().try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seccomp filter is too large",
            )
        })?,
        filter: filter.as_ptr().cast_mut(),
    };
    let status = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &program,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn install_supervisor_signal_handler(signal: i32) -> Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = request_supervisor_termination as *const () as usize;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("install verification supervisor signal handler {signal}"));
    }
    Ok(())
}

fn terminate_supervisor_descendants() -> Result<()> {
    if supervisor_descendants()?.is_empty() && supervisor_descendants_stably_empty()? {
        return Ok(());
    }
    // Give TERM handlers one scheduling window to run, then escalate. Repeating
    // full /proc scans with TERM can itself outlive a short-lived, TERM-ignoring
    // child and let it publish side effects before the eventual KILL scan.
    signal_supervisor_descendants(libc::SIGTERM)?;
    thread::sleep(Duration::from_millis(10));
    reap_supervisor_children();
    signal_supervisor_descendants(libc::SIGKILL)?;
    for _ in 0..100 {
        if supervisor_descendants_stably_empty()? {
            return Ok(());
        }
        signal_supervisor_descendants(libc::SIGKILL)?;
        thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("verification supervisor could not reap all command descendants")
}

fn supervisor_descendants_stably_empty() -> Result<bool> {
    for scan in 0..3 {
        reap_supervisor_children();
        if !supervisor_descendants()?.is_empty() {
            return Ok(false);
        }
        if scan < 2 {
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(true)
}

fn signal_supervisor_descendants(signal: i32) -> Result<()> {
    for pid in supervisor_descendants()? {
        let result = unsafe { libc::kill(pid, signal) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("signal verification descendant {pid}"));
        }
    }
    Ok(())
}

fn reap_supervisor_children() {
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if result <= 0 {
            break;
        }
    }
}

fn supervisor_descendants() -> Result<Vec<i32>> {
    let self_pid = std::process::id() as i32;
    let entries = std::fs::read_dir("/proc").context("enumerate supervised processes")?;
    let mut parents = BTreeMap::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if transient_proc_error(&err) => continue,
            Err(err) => return Err(err).context("enumerate supervised process"),
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let stat_path = entry.path().join("stat");
        match std::fs::read(&stat_path) {
            Ok(stat) => {
                if let Some(parent) = process_parent_from_stat(&stat) {
                    parents.insert(pid, parent);
                }
            }
            Err(err) if transient_proc_error(&err) => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("inspect supervised process stat {}", stat_path.display())
                });
            }
        }
    }
    let mut descendants = Vec::new();
    for pid in parents.keys().copied() {
        let mut current = pid;
        let mut seen = std::collections::BTreeSet::new();
        while let Some(parent) = parents.get(&current).copied() {
            if parent == self_pid {
                descendants.push(pid);
                break;
            }
            if parent <= 1 || !seen.insert(parent) {
                break;
            }
            current = parent;
        }
    }
    descendants.sort_unstable_by(|left, right| right.cmp(left));
    Ok(descendants)
}

fn transient_proc_error(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
    ) || matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    )
}

fn process_parent_from_stat(stat: &[u8]) -> Option<i32> {
    let command_end = stat.windows(2).rposition(|window| window == b") ")?;
    let fields = std::str::from_utf8(&stat[command_end + 2..])
        .ok()?
        .split_whitespace()
        .collect::<Vec<_>>();
    fields.get(1)?.parse().ok()
}

#[cfg(not(test))]
fn terminate_process_group(child: &mut std::process::Child, grace: Duration) {
    terminate_child_tree(
        child,
        ProcessSupervisionPolicy {
            graceful_signal: libc::SIGUSR1,
            graceful_target: GracefulSignalTarget::Process,
            grace,
            poll_interval: Duration::from_millis(50),
        },
    );
}

#[cfg(test)]
fn terminate_process_group(child: &mut std::process::Child, grace: Duration) {
    terminate_child_tree(child, ProcessSupervisionPolicy::process_group(grace));
}

struct SupervisorResultPipe {
    read_fd: i32,
    write_fd: i32,
}

impl SupervisorResultPipe {
    fn new() -> Result<Self, ShellCommandError> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!(
                    "create verification supervisor result pipe: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        if unsafe { libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC) } == -1
            || unsafe { libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC) } == -1
        {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!("configure verification supervisor result pipe: {err}"),
            ));
        }
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    fn read_fd(&self) -> i32 {
        self.read_fd
    }

    fn write_fd(&self) -> i32 {
        self.write_fd
    }

    fn parent_after_spawn(&mut self) {
        if self.write_fd >= 0 {
            unsafe {
                libc::close(self.write_fd);
            }
            self.write_fd = -1;
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    fn finish(&mut self) -> Result<(), ShellCommandError> {
        if self.read_fd < 0 {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                "verification supervisor result channel was unavailable",
            ));
        }
        let fd = self.read_fd;
        self.read_fd = -1;
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|err| {
            ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!("read verification supervisor result: {err}"),
            )
        })?;
        if !bytes.starts_with(COMMAND_SUPERVISOR_RESULT_MAGIC)
            || bytes.len() <= COMMAND_SUPERVISOR_RESULT_MAGIC.len()
        {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                "verification supervisor omitted its authenticated result",
            ));
        }
        let status = bytes[COMMAND_SUPERVISOR_RESULT_MAGIC.len()];
        let message = &bytes[COMMAND_SUPERVISOR_RESULT_MAGIC.len() + 1..];
        match status {
            0 if message.is_empty() => Ok(()),
            1 if !message.is_empty() => Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!(
                    "verification command supervision failed: {}",
                    String::from_utf8_lossy(message)
                ),
            )),
            _ => Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                "verification supervisor returned a malformed authenticated result",
            )),
        }
    }
}

#[cfg(not(test))]
fn finish_supervisor_result(result: &mut SupervisorResultPipe) -> Result<(), ShellCommandError> {
    result.finish()
}

#[cfg(test)]
thread_local! {
    // 0 bypasses the hidden-supervisor channel for ordinary unit tests, 1 injects
    // an authenticated failure, and 2 consumes the real private pipe.
    static TEST_SUPERVISOR_RESULT_MODE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn finish_supervisor_result(result: &mut SupervisorResultPipe) -> Result<(), ShellCommandError> {
    match TEST_SUPERVISOR_RESULT_MODE.replace(0) {
        0 => Ok(()),
        1 => Err(ShellCommandError::new(
            ShellFailureKind::Supervision,
            "forced authenticated supervisor failure",
        )),
        2 => result.finish(),
        mode => Err(ShellCommandError::new(
            ShellFailureKind::Supervision,
            format!("invalid test supervisor-result mode {mode}"),
        )),
    }
}

impl Drop for SupervisorResultPipe {
    fn drop(&mut self) {
        for fd in [self.read_fd, self.write_fd] {
            if fd >= 0 {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

pub(crate) fn protect_command_supervisor_result(fd: i32) -> Result<()> {
    if fd < 0 {
        anyhow::bail!("verification supervisor result descriptor was invalid");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error())
            .context("protect verification supervisor result descriptor from command children");
    }
    Ok(())
}

pub(crate) fn write_command_supervisor_result(fd: i32, error: Option<&str>) -> Result<()> {
    protect_command_supervisor_result(fd)?;
    let mut bytes = Vec::from(COMMAND_SUPERVISOR_RESULT_MAGIC);
    match error {
        None => bytes.push(0),
        Some(message) => {
            bytes.push(1);
            bytes.extend_from_slice(message.as_bytes());
        }
    }
    let mut written = 0;
    while written < bytes.len() {
        let count =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if count < 0 {
            return Err(std::io::Error::last_os_error())
                .context("write authenticated verification supervisor result");
        }
        written += count as usize;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct SupervisionPipe {
    read_fd: i32,
    write_fd: i32,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl SupervisionPipe {
    fn new() -> Result<Self, ShellCommandError> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!(
                    "create verification supervision pipe: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        if unsafe { libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC) } == -1
            || unsafe { libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC) } == -1
        {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!("configure verification supervision pipe: {err}"),
            ));
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fds[1], stat.as_mut_ptr()) } != 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!("inspect verification supervision pipe: {err}"),
            ));
        }
        let inode = unsafe { stat.assume_init().st_ino };
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
            inode,
        })
    }

    fn read_fd(&self) -> i32 {
        self.read_fd
    }

    fn write_fd(&self) -> i32 {
        self.write_fd
    }

    fn parent_after_spawn(&mut self) {
        if self.write_fd >= 0 {
            unsafe {
                libc::close(self.write_fd);
            }
            self.write_fd = -1;
        }
    }

    fn identity(&self) -> u64 {
        self.inode
    }
}

#[cfg(target_os = "linux")]
impl Drop for SupervisionPipe {
    fn drop(&mut self) {
        for fd in [self.read_fd, self.write_fd] {
            if fd >= 0 {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct SupervisionPipe;

#[cfg(not(target_os = "linux"))]
impl SupervisionPipe {
    fn new() -> Result<Self, ShellCommandError> {
        Ok(Self)
    }

    fn read_fd(&self) -> i32 {
        -1
    }

    fn write_fd(&self) -> i32 {
        -1
    }

    fn parent_after_spawn(&mut self) {}

    fn identity(&self) -> u64 {
        0
    }
}

#[cfg(target_os = "linux")]
fn terminate_supervised_descendants(inode: u64) -> Result<(), ShellCommandError> {
    if supervised_processes(inode)?.is_empty() && supervised_processes_stably_empty(inode)? {
        return Ok(());
    }
    // Mirror the in-supervisor TERM scheduling window, then escalate without
    // spending multiple expensive /proc scans on a signal the child may ignore.
    signal_supervised_processes(inode, libc::SIGTERM)?;
    thread::sleep(Duration::from_millis(10));
    signal_supervised_processes(inode, libc::SIGKILL)?;
    for _ in 0..100 {
        if supervised_processes_stably_empty(inode)? {
            return Ok(());
        }
        signal_supervised_processes(inode, libc::SIGKILL)?;
        thread::sleep(Duration::from_millis(10));
    }
    Err(ShellCommandError::new(
        ShellFailureKind::Supervision,
        "verification command descendants remained alive after supervised cleanup",
    ))
}

#[cfg(target_os = "linux")]
fn supervised_processes_stably_empty(inode: u64) -> Result<bool, ShellCommandError> {
    for scan in 0..3 {
        if !supervised_processes(inode)?.is_empty() {
            return Ok(false);
        }
        if scan < 2 {
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
fn terminate_supervised_descendants(_identity: u64) -> Result<(), ShellCommandError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn signal_supervised_processes(inode: u64, signal: i32) -> Result<(), ShellCommandError> {
    for pid in supervised_processes(inode)? {
        let status = unsafe { libc::kill(pid, signal) };
        if status != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(ShellCommandError::new(
                ShellFailureKind::Supervision,
                format!(
                    "signal supervised verification descendant {pid}: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn supervised_processes(inode: u64) -> Result<Vec<i32>, ShellCommandError> {
    let expected = format!("pipe:[{inode}]");
    let self_pid = std::process::id() as i32;
    let self_uid = unsafe { libc::geteuid() };
    let processes = std::fs::read_dir("/proc").map_err(|err| {
        ShellCommandError::new(
            ShellFailureKind::Supervision,
            format!("enumerate /proc for verification descendants: {err}"),
        )
    })?;
    let mut found = Vec::new();
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_string_lossy()
            .parse::<i32>()
            .ok()
            .filter(|pid| *pid != self_pid)
        else {
            continue;
        };
        let process_path = process.path();
        let Ok(metadata) = std::fs::metadata(&process_path) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != self_uid {
            continue;
        }
        let fd_path = process_path.join("fd");
        let descriptors = match std::fs::read_dir(&fd_path) {
            Ok(descriptors) => descriptors,
            Err(err) if transient_proc_error(&err) => continue,
            Err(_err) if !process_path.exists() => continue,
            Err(err) => {
                return Err(ShellCommandError::new(
                    ShellFailureKind::Supervision,
                    format!("inspect {fd_path:?} for verification descendants: {err}"),
                ));
            }
        };
        let mut carries_supervision_pipe = false;
        for descriptor in descriptors.flatten() {
            match std::fs::read_link(descriptor.path()) {
                Ok(target) if target.as_os_str() == expected.as_str() => {
                    carries_supervision_pipe = true;
                    break;
                }
                Ok(_) => {}
                Err(err) if transient_proc_error(&err) => continue,
                Err(_err) if !process_path.exists() => break,
                Err(err) => {
                    return Err(ShellCommandError::new(
                        ShellFailureKind::Supervision,
                        format!(
                            "inspect verification descendant descriptor {}: {err}",
                            descriptor.path().display()
                        ),
                    ));
                }
            }
        }
        if carries_supervision_pipe {
            found.push(pid);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::{
        PROGRESS_OUTPUT_TAIL_BYTES, ProcessSupervisionPolicy, ShellCommand, ShellProgress,
        TEST_SUPERVISOR_RESULT_MODE, supervised_process_group_exists, terminate_child_tree,
    };
    use crate::agent::CancellationToken;
    use anyhow::Result;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    #[cfg(unix)]
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn process_exists(pid: i32) -> bool {
        let status = unsafe { libc::kill(pid, 0) };
        status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(unix)]
    #[test]
    fn supervision_policy_escalates_across_a_term_ignoring_process_tree() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("(trap '' TERM; sleep 30) & wait");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let pgid = child.id();
        let started = Instant::now();
        terminate_child_tree(
            &mut child,
            ProcessSupervisionPolicy::process_group(Duration::from_millis(50)),
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let deadline = Instant::now() + Duration::from_secs(1);
        while supervised_process_group_exists(pgid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!supervised_process_group_exists(pgid));
    }

    #[test]
    fn shell_command_timeout_returns_promptly() -> Result<()> {
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let err = ShellCommand::new(Path::new("."), "sleep 30")
            .timeout(Duration::from_secs(1))
            .envs(&BTreeMap::new())
            .run(&cancel)
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn authenticated_supervisor_failure_outranks_timeout_and_cancellation() {
        TEST_SUPERVISOR_RESULT_MODE.set(1);
        let timeout_err = ShellCommand::new(Path::new("."), "sleep 30")
            .timeout(Duration::from_millis(1))
            .run(&CancellationToken::new())
            .unwrap_err();
        assert!(
            timeout_err
                .to_string()
                .contains("forced authenticated supervisor failure"),
            "{timeout_err:#}"
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        TEST_SUPERVISOR_RESULT_MODE.set(1);
        let cancellation_err = ShellCommand::new(Path::new("."), "sleep 30")
            .timeout(Duration::from_secs(30))
            .run(&cancel)
            .unwrap_err();
        assert!(
            cancellation_err
                .to_string()
                .contains("forced authenticated supervisor failure"),
            "{cancellation_err:#}"
        );
    }

    #[test]
    fn authenticated_supervisor_result_outranks_supervisor_signal() {
        TEST_SUPERVISOR_RESULT_MODE.set(1);
        let authenticated_err = ShellCommand::new(Path::new("."), "kill -KILL $$")
            .timeout(Duration::from_secs(5))
            .run(&CancellationToken::new())
            .unwrap_err();
        assert!(
            authenticated_err
                .to_string()
                .contains("forced authenticated supervisor failure"),
            "{authenticated_err:#}"
        );

        TEST_SUPERVISOR_RESULT_MODE.set(2);
        let missing_err = ShellCommand::new(Path::new("."), "kill -KILL $$")
            .timeout(Duration::from_secs(5))
            .run(&CancellationToken::new())
            .unwrap_err();
        assert!(
            missing_err
                .to_string()
                .contains("omitted its authenticated result"),
            "{missing_err:#}"
        );
    }

    #[test]
    fn missing_private_supervisor_result_outranks_timeout_and_cancellation() {
        TEST_SUPERVISOR_RESULT_MODE.set(2);
        let timeout_err = ShellCommand::new(Path::new("."), "sleep 30")
            .timeout(Duration::from_millis(1))
            .run(&CancellationToken::new())
            .unwrap_err();
        assert!(
            timeout_err
                .to_string()
                .contains("omitted its authenticated result"),
            "{timeout_err:#}"
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        TEST_SUPERVISOR_RESULT_MODE.set(2);
        let cancellation_err = ShellCommand::new(Path::new("."), "sleep 30")
            .timeout(Duration::from_secs(30))
            .run(&cancel)
            .unwrap_err();
        assert!(
            cancellation_err
                .to_string()
                .contains("omitted its authenticated result"),
            "{cancellation_err:#}"
        );
    }

    #[test]
    fn shell_command_cancellation_returns_promptly() -> Result<()> {
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            thread_cancel.cancel();
        });
        let started = Instant::now();
        let err = ShellCommand::new(Path::new("."), "sleep 30")
            .timeout(Duration::from_secs(30))
            .envs(&BTreeMap::new())
            .run(&cancel)
            .unwrap_err();
        assert!(err.to_string().contains("run cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn shell_command_observes_cancellation_during_post_exit_cleanup() -> Result<()> {
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            thread_cancel.cancel();
        });

        let err = ShellCommand::new(Path::new("."), "(trap '' TERM; sleep 30) &")
            .timeout(Duration::from_secs(5))
            .run(&cancel)
            .unwrap_err();

        assert!(err.to_string().contains("run cancelled"), "{err:#}");
        Ok(())
    }

    #[test]
    fn shell_command_reaps_background_descendants_after_normal_exit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let pid_path = temp.path().join("pid");
        let release_path = temp.path().join("release");
        let mutation_path = temp.path().join("late-mutation");
        let command = format!(
            "(while [ ! -e '{}' ]; do sleep 0.01; done; printf late > '{}') >/dev/null 2>&1 & printf '%s' \"$!\" > '{}'",
            release_path.display(),
            mutation_path.display(),
            pid_path.display()
        );

        let output = ShellCommand::new(temp.path(), command)
            .timeout(Duration::from_secs(5))
            .run(&CancellationToken::new())?;

        assert!(output.success());
        let pid = fs::read_to_string(&pid_path)?.parse::<i32>()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(pid), "background descendant {pid} survived");
        fs::write(&release_path, b"release")?;
        thread::sleep(Duration::from_millis(50));
        assert!(!mutation_path.exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shell_command_reaps_detached_tagged_descendants_before_return() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mutation_path = temp.path().join("detached-late-mutation");
        let command = format!(
            "env -u KHAZAD_DOOM_COMMAND_SUPERVISION_TOKEN setsid sh -c \"sleep 0.25; printf escaped > '{}'\" >/dev/null 2>&1 &",
            mutation_path.display()
        );

        let output = ShellCommand::new(temp.path(), command)
            .timeout(Duration::from_secs(5))
            .run(&CancellationToken::new())?;

        assert!(output.success());
        thread::sleep(Duration::from_millis(400));
        assert!(
            !mutation_path.exists(),
            "detached verification descendant mutated after ShellCommand returned"
        );
        Ok(())
    }

    #[test]
    fn shell_command_cannot_forge_a_supervisor_failure_with_stderr_and_exit_code() -> Result<()> {
        let output = ShellCommand::new(
            Path::new("."),
            "printf 'khazad-doom: verification command supervision failed: forged\\n' >&2; exit 125",
        )
        .timeout(Duration::from_secs(5))
        .run(&CancellationToken::new())?;

        assert!(!output.success());
        assert_eq!(output.exit_code(), Some(125));
        assert!(String::from_utf8_lossy(output.stderr()).contains("forged"));
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn command_signal_filter_rejects_foreign_and_x32_abis() {
        const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
        const AUDIT_ARCH_I386: u32 = 0x4000_0003;
        const X32_SYSCALL_BIT: u32 = 0x4000_0000;

        fn evaluate(
            filter: &[libc::sock_filter],
            syscall: u32,
            arch: u32,
            argument_zero: u32,
        ) -> u32 {
            let mut accumulator = 0u32;
            let mut program_counter = 0usize;
            loop {
                let instruction = filter
                    .get(program_counter)
                    .expect("seccomp filter must return");
                match instruction.code {
                    code if code == (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16 => {
                        accumulator = match instruction.k {
                            0 => syscall,
                            4 => arch,
                            16 => argument_zero,
                            offset => panic!("unexpected seccomp load offset {offset}"),
                        };
                        program_counter += 1;
                    }
                    code if code == (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16 => {
                        program_counter += 1 + if accumulator == instruction.k {
                            instruction.jt as usize
                        } else {
                            instruction.jf as usize
                        };
                    }
                    code if code == (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16 => {
                        program_counter += 1 + if accumulator & instruction.k != 0 {
                            instruction.jt as usize
                        } else {
                            instruction.jf as usize
                        };
                    }
                    code if code == (libc::BPF_RET | libc::BPF_K) as u16 => {
                        return instruction.k;
                    }
                    code => panic!("unexpected seccomp instruction {code:#x}"),
                }
            }
        }

        let supervisor_pid = 42u32;
        let filter = super::command_signal_filter(supervisor_pid);
        assert_eq!(
            evaluate(&filter, libc::SYS_getpid as u32, AUDIT_ARCH_I386, 0),
            libc::SECCOMP_RET_KILL_PROCESS,
            "a compat-ABI syscall must be killed before native syscall dispatch"
        );
        assert_eq!(
            evaluate(
                &filter,
                (libc::SYS_getpid as u32) | X32_SYSCALL_BIT,
                AUDIT_ARCH_X86_64,
                0
            ),
            libc::SECCOMP_RET_KILL_PROCESS,
            "an x32 syscall must be killed before native syscall dispatch"
        );
        assert_eq!(
            evaluate(
                &filter,
                libc::SYS_kill as u32,
                AUDIT_ARCH_X86_64,
                supervisor_pid
            ),
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
        );
    }

    #[test]
    fn bounded_shell_capture_spills_multi_megabyte_output_and_retains_only_tail() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let spill_stem = temp.path().join("verify-output");
        let output = ShellCommand::new(
            temp.path(),
            "head -c 2097152 /dev/zero | tr '\\0' o; head -c 1048576 /dev/zero | tr '\\0' e >&2",
        )
        .timeout(Duration::from_secs(10))
        .output_bounds(1024, 32)
        .spill_to(spill_stem.clone())
        .run(&CancellationToken::new())?;

        assert!(output.success());
        assert!(output.stdout().len() <= 1024);
        assert!(output.stderr().len() <= 1024);
        assert_eq!(output.stdout_total_bytes(), 2 * 1024 * 1024);
        assert_eq!(output.stderr_total_bytes(), 1024 * 1024);
        assert!(output.output_truncated());
        assert_eq!(
            fs::metadata(spill_stem.with_extension("stdout.log"))?.len(),
            2 * 1024 * 1024
        );
        assert_eq!(
            fs::metadata(spill_stem.with_extension("stderr.log"))?.len(),
            1024 * 1024
        );
        Ok(())
    }

    #[test]
    fn bounded_shell_capture_surfaces_spill_failure() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let blocked_parent = temp.path().join("not-a-directory");
        fs::write(&blocked_parent, b"file")?;
        let error = ShellCommand::new(temp.path(), "printf evidence")
            .output_bounds(1024, 32)
            .spill_to(blocked_parent.join("capture"))
            .run(&CancellationToken::new())
            .expect_err("spill setup failure must be explicit");
        assert!(format!("{error:#}").contains("output spill"), "{error:#}");
        Ok(())
    }

    #[test]
    fn spill_setup_removes_a_new_stdout_file_when_stderr_open_fails() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let spill_stem = temp.path().join("capture");
        fs::create_dir(spill_stem.with_extension("stderr.log"))?;
        let error = ShellCommand::new(temp.path(), "sleep 30")
            .output_bounds(1024, 32)
            .spill_to(spill_stem.clone())
            .run(&CancellationToken::new())
            .expect_err("second spill setup failure must be explicit");

        assert!(format!("{error:#}").contains("output spill"), "{error:#}");
        assert!(!spill_stem.with_extension("stdout.log").exists());
        Ok(())
    }

    #[test]
    fn shell_command_progress_is_bounded_and_preserves_streams() -> Result<()> {
        let cancel = CancellationToken::new();
        let tails = Arc::new(Mutex::new(Vec::new()));
        let observed_tails = tails.clone();
        let progress: ShellProgress = Arc::new(move |tail| {
            observed_tails
                .lock()
                .expect("progress tails mutex poisoned")
                .push(tail);
        });

        let output = ShellCommand::new(
            Path::new("."),
            "printf '%05000d' 0 | tr '0' 'o'; printf 'err-line\\n' >&2",
        )
        .timeout(Duration::from_secs(5))
        .envs(&BTreeMap::new())
        .progress(Some(progress))
        .run(&cancel)?;

        assert!(output.success());
        assert_eq!(output.stdout().len(), 5000);
        assert_eq!(String::from_utf8_lossy(output.stderr()), "err-line\n");
        let tails = tails.lock().expect("progress tails mutex poisoned");
        assert!(
            !tails.is_empty(),
            "progress sink should receive output tails"
        );
        assert!(
            tails
                .iter()
                .all(|tail| tail.len() <= PROGRESS_OUTPUT_TAIL_BYTES),
            "progress tails should stay bounded: {tails:#?}"
        );
        assert!(
            tails
                .iter()
                .any(|tail| tail.contains("err-line") || tail.contains('o')),
            "progress should include streamed stdout or stderr: {tails:#?}"
        );
        Ok(())
    }
}
