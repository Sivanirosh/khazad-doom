use super::CancelledError;
use crate::agent::CancellationToken;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const PROGRESS_OUTPUT_TAIL_BYTES: usize = 4_000;
pub(crate) type ShellProgress = Arc<dyn Fn(String) + Send + Sync + 'static>;

pub(crate) struct ShellCommand {
    cwd: PathBuf,
    command: String,
    timeout: Duration,
    env: BTreeMap<String, String>,
    progress: Option<ShellProgress>,
}

impl ShellCommand {
    pub(crate) fn new(cwd: impl AsRef<Path>, command: impl Into<String>) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            command: command.into(),
            timeout: Duration::ZERO,
            env: BTreeMap::new(),
            progress: None,
        }
    }

    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) fn envs(mut self, env: &BTreeMap<String, String>) -> Self {
        self.env = env.clone();
        self
    }

    pub(crate) fn progress(mut self, progress: Option<ShellProgress>) -> Self {
        self.progress = progress;
        self
    }

    pub(crate) fn run(self, cancel: &CancellationToken) -> Result<ShellOutput> {
        let mut process = Command::new("sh");
        process
            .arg("-c")
            .arg(&self.command)
            .current_dir(&self.cwd)
            .envs(&self.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Create a process group for the shell and its children so cancellation can
        // kill a hanging verify/gate command instead of only killing the shell.
        unsafe {
            process.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = process.spawn()?;
        let stdout = child.stdout.take().context("command stdout")?;
        let stderr = child.stderr.take().context("command stderr")?;
        let monitor = ShellCommandMonitor::spawn(stdout, stderr, self.progress);

        let started_at = Instant::now();
        let mut last_heartbeat = Instant::now();
        let status = loop {
            if cancel.is_cancelled() {
                terminate_process_group(&mut child);
                let _ = monitor.finish();
                return Err(CancelledError::new("run cancelled").into());
            }
            if !self.timeout.is_zero() && started_at.elapsed() >= self.timeout {
                terminate_process_group(&mut child);
                let _ = monitor.finish();
                bail!("command timed out after {} seconds", self.timeout.as_secs());
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
        let (stdout, stderr) = monitor.finish();
        Ok(ShellOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout,
            stderr,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ShellOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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

struct ShellCommandMonitor {
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    combined_tail: Arc<Mutex<Vec<u8>>>,
    progress: Option<ShellProgress>,
    stdout_thread: thread::JoinHandle<()>,
    stderr_thread: thread::JoinHandle<()>,
}

impl ShellCommandMonitor {
    fn spawn(
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        progress: Option<ShellProgress>,
    ) -> Self {
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let combined_tail = Arc::new(Mutex::new(Vec::new()));
        let stdout_thread = spawn_output_reader(
            stdout,
            stdout_buf.clone(),
            combined_tail.clone(),
            progress.clone(),
        );
        let stderr_thread = spawn_output_reader(
            stderr,
            stderr_buf.clone(),
            combined_tail.clone(),
            progress.clone(),
        );
        Self {
            stdout_buf,
            stderr_buf,
            combined_tail,
            progress,
            stdout_thread,
            stderr_thread,
        }
    }

    fn emit_progress(&self) {
        if let Some(progress) = &self.progress {
            progress(tail_text(&self.combined_tail));
        }
    }

    fn finish(self) -> (Vec<u8>, Vec<u8>) {
        let Self {
            stdout_buf,
            stderr_buf,
            stdout_thread,
            stderr_thread,
            ..
        } = self;
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        (
            stdout_buf.lock().expect("stdout mutex poisoned").clone(),
            stderr_buf.lock().expect("stderr mutex poisoned").clone(),
        )
    }
}

fn spawn_output_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream_buf: Arc<Mutex<Vec<u8>>>,
    combined_tail: Arc<Mutex<Vec<u8>>>,
    progress: Option<ShellProgress>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        loop {
            let read = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            stream_buf
                .lock()
                .expect("stream mutex poisoned")
                .extend_from_slice(&buf[..read]);
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
    })
}

fn tail_text(combined_tail: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&combined_tail.lock().expect("combined mutex poisoned")).to_string()
}

fn terminate_process_group(child: &mut std::process::Child) {
    let pgid = -(child.id() as i32);
    unsafe {
        let _ = libc::kill(pgid, libc::SIGTERM);
    }
    for _ in 0..10 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    // Always escalate to the whole process group. The shell may exit after
    // SIGTERM while a descendant in the same process group ignores it.
    unsafe {
        let _ = libc::kill(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::{PROGRESS_OUTPUT_TAIL_BYTES, ShellCommand, ShellProgress};
    use crate::agent::CancellationToken;
    use anyhow::Result;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

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
