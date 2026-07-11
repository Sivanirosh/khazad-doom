use crate::artifact::{PiTuiWorkerArtifacts, PiWrapperArtifacts};
use crate::domain::{Handoff, RuntimeConfig};
use crate::pi_contract::{self, PiContractObservation, PiContractWarning, PiParser};
use crate::{artifact, gitutil};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerEventKind {
    Started,
    ProcessObserved,
    Stdout,
    Stderr,
    Finished,
}

impl RunnerEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::ProcessObserved => "process_observed",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Finished => "finished",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerEvent {
    pub kind: RunnerEventKind,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub text: String,
}

impl RunnerEvent {
    pub fn started(pid: Option<u32>) -> Self {
        Self {
            kind: RunnerEventKind::Started,
            pid,
            exit_code: None,
            text: String::new(),
        }
    }

    pub fn process_observed(pid: Option<u32>) -> Self {
        Self {
            kind: RunnerEventKind::ProcessObserved,
            pid,
            exit_code: None,
            text: String::new(),
        }
    }

    pub fn stdout(pid: Option<u32>, text: impl Into<String>) -> Self {
        Self {
            kind: RunnerEventKind::Stdout,
            pid,
            exit_code: None,
            text: text.into(),
        }
    }

    pub fn stderr(pid: Option<u32>, text: impl Into<String>) -> Self {
        Self {
            kind: RunnerEventKind::Stderr,
            pid,
            exit_code: None,
            text: text.into(),
        }
    }

    pub fn finished(pid: Option<u32>, exit_code: Option<i32>) -> Self {
        Self {
            kind: RunnerEventKind::Finished,
            pid,
            exit_code,
            text: String::new(),
        }
    }
}

pub type RunnerEventSink = Arc<dyn Fn(RunnerEvent) + Send + Sync + 'static>;

fn emit_runner_event(sink: &Option<RunnerEventSink>, event: RunnerEvent) {
    if let Some(sink) = sink {
        sink(event);
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    #[allow(dead_code)]
    pub kind: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub json_schema: String,
    #[allow(dead_code)]
    pub env: BTreeMap<String, String>,
    pub termination_grace_seconds: u64,
    pub runtime: RuntimeConfig,
    pub raw_output_stem: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResultData {
    pub output: Option<Value>,
    pub usage: Usage,
    pub contract_warnings: Vec<PiContractWarning>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunnerTranscript {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout_tail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr_tail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assistant_tail: String,
}

pub const AGENT_AUTH_REQUIRED_FAILURE_KIND: &str = "agent_auth_required";
pub const REAL_PI_WORKER_EVIDENCE_KIND: &str = "real_pi_worker";
pub const REAL_PI_WORKER_EVIDENCE_LABEL: &str = "real Pi worker implementation evidence";
pub const FAKE_TEST_DOUBLE_EVIDENCE_KIND: &str =
    "deterministic_test_double_not_real_pi_worker_evidence";
pub const FAKE_TEST_DOUBLE_EVIDENCE_LABEL: &str =
    "deterministic test-double evidence; not real Pi worker implementation evidence";

pub fn worker_evidence_kind_for_runner(runner: &str) -> &'static str {
    if runner.eq_ignore_ascii_case("fake") {
        FAKE_TEST_DOUBLE_EVIDENCE_KIND
    } else {
        REAL_PI_WORKER_EVIDENCE_KIND
    }
}

pub fn worker_evidence_label_for_runner(runner: &str) -> &'static str {
    if runner.eq_ignore_ascii_case("fake") {
        FAKE_TEST_DOUBLE_EVIDENCE_LABEL
    } else {
        REAL_PI_WORKER_EVIDENCE_LABEL
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerLaunchFailure {
    pub failure_kind: String,
    pub summary: String,
    pub retryable: bool,
    pub operator_action_required: bool,
    pub fix_commands: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RunnerError {
    message: String,
    transcript: RunnerTranscript,
}

impl RunnerError {
    fn new(message: impl Into<String>, transcript: RunnerTranscript) -> Self {
        Self {
            message: message.into(),
            transcript,
        }
    }

    pub fn transcript(&self) -> &RunnerTranscript {
        &self.transcript
    }

    pub fn classify_launch_failure(
        &self,
        metadata: &RunnerMetadata,
    ) -> Option<RunnerLaunchFailure> {
        pi_contract::classify_launch_failure(&self.transcript, metadata)
    }
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunnerError {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input_tokens: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output_tokens: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerMetadata {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fix_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_attribution: BTreeMap<String, String>,
}

impl RunnerMetadata {
    pub fn profile_summary(&self) -> String {
        if !self.profile_summary.trim().is_empty() {
            return self.profile_summary.clone();
        }
        let profile = if self.profile.trim().is_empty() {
            "default"
        } else {
            self.profile.trim()
        };
        let mut parts = vec![format!("profile={profile}")];
        if !self.provider.trim().is_empty() {
            parts.push(format!("provider={}", self.provider.trim()));
        }
        if !self.model.trim().is_empty() {
            parts.push(format!("model={}", self.model.trim()));
        }
        if !self.reasoning.trim().is_empty() {
            parts.push(format!("reasoning={}", self.reasoning.trim()));
        }
        if !self.mode.trim().is_empty() {
            parts.push(format!("mode={}", self.mode.trim()));
        }
        parts.join(" ")
    }

    pub fn launch_summary(&self) -> String {
        if !self.launch_summary.trim().is_empty() {
            self.launch_summary.clone()
        } else {
            self.profile_summary()
        }
    }

    pub fn auth_fix_commands(&self) -> Vec<String> {
        if self.fix_commands.is_empty() {
            vec!["pi /login".to_string()]
        } else {
            self.fix_commands.clone()
        }
    }
}

#[allow(dead_code)]
pub trait Runner: Send + Sync {
    fn run(
        &self,
        job: Job,
        cancel: CancellationToken,
        events: Option<RunnerEventSink>,
    ) -> Result<ResultData>;
    fn name(&self) -> &str;
    fn metadata(&self) -> RunnerMetadata {
        RunnerMetadata::default()
    }

    fn pi_contract_observation(&self) -> Option<PiContractObservation> {
        None
    }

    fn pi_command_spec(&self) -> Option<PiCommandSpec> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiCommandSpec {
    pub bin: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSpec {
    pub kind: String,
    pub pi_bin: String,
    pub pi_args: Vec<String>,
    pub metadata: RunnerMetadata,
}

impl RunnerSpec {
    #[allow(dead_code)]
    pub fn from_agent_and_env(agent: &str) -> Result<Self> {
        let kind = if agent.trim().is_empty() {
            std::env::var("KHAZAD_AGENT").unwrap_or_else(|_| "pi".to_string())
        } else {
            agent.to_string()
        };
        let pi_bin = std::env::var("KHAZAD_PI_BIN").unwrap_or_else(|_| "pi".to_string());
        let pi_args = std::env::var("KHAZAD_PI_ARGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        Self::from_parts(&kind, pi_bin, pi_args)
    }

    pub fn from_parts(agent: &str, pi_bin: String, pi_args: Vec<String>) -> Result<Self> {
        let kind = if agent.trim().is_empty() {
            "pi".to_string()
        } else {
            agent.trim().to_ascii_lowercase()
        };
        match kind.as_str() {
            "pi" => Ok(Self {
                kind,
                pi_bin: if pi_bin.trim().is_empty() {
                    "pi".to_string()
                } else {
                    pi_bin
                },
                pi_args,
                metadata: RunnerMetadata::default(),
            }),
            "fake" => Ok(Self {
                kind,
                pi_bin: String::new(),
                pi_args: Vec::new(),
                metadata: RunnerMetadata::default(),
            }),
            other => bail!("unknown agent {other:?}; expected \"pi\" or \"fake\""),
        }
    }
}

pub fn runner_from_spec(spec: RunnerSpec) -> Arc<dyn Runner> {
    match spec.kind.as_str() {
        "fake" => Arc::new(FakeRunner),
        _ => Arc::new(PiRunner {
            bin: spec.pi_bin,
            extra_args: spec.pi_args,
            metadata: spec.metadata,
        }),
    }
}

#[derive(Debug, Clone)]
pub struct PiRunner {
    pub bin: String,
    pub extra_args: Vec<String>,
    pub metadata: RunnerMetadata,
}

impl PiRunner {
    pub fn command_spec(&self) -> PiCommandSpec {
        PiCommandSpec {
            bin: resolved_pi_bin(&self.bin),
            args: pi_contract::launch_args(&self.extra_args),
        }
    }
}

impl Runner for PiRunner {
    fn run(
        &self,
        job: Job,
        cancel: CancellationToken,
        events: Option<RunnerEventSink>,
    ) -> Result<ResultData> {
        if cancel.is_cancelled() {
            bail!("job cancelled");
        }
        let spec = self.command_spec();
        let stdout_spill_path = job
            .raw_output_stem
            .as_deref()
            .map(|stem| stem.with_extension("stdout.log"));
        let stdout_spill_existed = stdout_spill_path.as_deref().is_some_and(Path::exists);
        let stdout_spill = open_worker_output_spill(job.raw_output_stem.as_deref(), "stdout.log")?;
        let stderr_spill =
            match open_worker_output_spill(job.raw_output_stem.as_deref(), "stderr.log") {
                Ok(spill) => spill,
                Err(err) => {
                    drop(stdout_spill);
                    if !stdout_spill_existed && let Some(path) = stdout_spill_path {
                        let _ = fs::remove_file(path);
                    }
                    return Err(err);
                }
            };
        let stdout_stats = Arc::new(Mutex::new(OutputSpillStats::default()));
        let stderr_stats = Arc::new(Mutex::new(OutputSpillStats::default()));
        let _output_capture = WorkerOutputCaptureGuard::new(
            job.raw_output_stem.clone(),
            job.runtime.retained_output_bytes,
            job.runtime.retained_output_lines,
            stdout_stats.clone(),
            stderr_stats.clone(),
        );
        let mut cmd = Command::new(&spec.bin);
        cmd.args(&spec.args)
            .envs(&job.env)
            .current_dir(&job.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Pi and any children inherit this group, making cancellation cover the
        // whole normal worker tree rather than only the immediate Pi process.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }

        let mut child = cmd.spawn().with_context(|| format!("start {}", spec.bin))?;
        let pid = child.id();
        emit_runner_event(&events, RunnerEvent::started(Some(pid)));

        {
            let mut stdin = child.stdin.take().context("pi stdin")?;
            stdin.write_all(build_prompt(&job.prompt, &job.json_schema).as_bytes())?;
        }

        let stderr = child.stderr.take().context("pi stderr")?;
        let stderr_events = events.clone();
        let stderr_bytes = job.runtime.retained_output_bytes;
        let stderr_lines = job.runtime.retained_output_lines;
        let stderr_thread = thread::spawn(move || {
            let mut reader = TeeReader::new(stderr, stderr_spill, stderr_stats);
            let mut chunk = [0_u8; 8 * 1024];
            let mut tail = String::new();
            loop {
                let read = reader.read(&mut chunk).context("read Pi stderr")?;
                if read == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&chunk[..read]);
                let event_text = bounded_utf8_tail(&text, stderr_bytes, stderr_lines);
                emit_runner_event(&stderr_events, RunnerEvent::stderr(Some(pid), event_text));
                append_bounded_utf8(&mut tail, &text, stderr_bytes, stderr_lines);
            }
            Ok::<String, anyhow::Error>(tail)
        });

        let stdout = child.stdout.take().context("pi stdout")?;
        let stdout_events = events.clone();
        let retained_output_bytes = job.runtime.retained_output_bytes;
        let retained_output_lines = job.runtime.retained_output_lines;
        let parser_thread = thread::spawn(move || {
            let mut parser =
                PiParser::with_output_bounds(retained_output_bytes, retained_output_lines);
            parser.parse(
                TeeReader::new(stdout, stdout_spill, stdout_stats),
                stdout_events,
                Some(pid),
            )?;
            Ok::<PiParser, anyhow::Error>(parser)
        });

        let mut next_observation = Instant::now();
        let status = loop {
            if cancel.is_cancelled() {
                terminate_child(
                    &mut child,
                    Duration::from_secs(job.termination_grace_seconds),
                );
                let parser = join_parser(parser_thread)?;
                let stderr = join_stderr(stderr_thread)?;
                return Err(RunnerError::new("job cancelled", parser.transcript(&stderr)).into());
            }
            if let Some(status) = child.try_wait()? {
                emit_runner_event(&events, RunnerEvent::finished(Some(pid), status.code()));
                break status;
            }
            if Instant::now() >= next_observation {
                emit_runner_event(&events, RunnerEvent::process_observed(Some(pid)));
                next_observation = Instant::now() + Duration::from_secs(1);
            }
            thread::sleep(Duration::from_millis(50));
        };

        // Cancellation can race with the immediate Pi parent exiting between
        // the loop's cancellation check and `try_wait`. Linearize that race
        // before normal post-exit cleanup so surviving descendants still get
        // TERM, the configured grace, and then KILL.
        if cancel.is_cancelled() {
            terminate_child(
                &mut child,
                Duration::from_secs(job.termination_grace_seconds),
            );
            let parser = join_parser(parser_thread)?;
            let stderr = join_stderr(stderr_thread)?;
            return Err(RunnerError::new("job cancelled", parser.transcript(&stderr)).into());
        }
        // A successfully exited Pi parent can leave descendants holding the
        // process group and output pipes. Clean that group with the same
        // TERM/grace/KILL protocol used by cancellation. This also closes the
        // race where cancellation arrives just after the post-exit check.
        #[cfg(unix)]
        terminate_child(
            &mut child,
            Duration::from_secs(job.termination_grace_seconds),
        );
        if cancel.is_cancelled() {
            let parser = join_parser(parser_thread)?;
            let stderr = join_stderr(stderr_thread)?;
            return Err(RunnerError::new("job cancelled", parser.transcript(&stderr)).into());
        }
        let parser = join_parser(parser_thread)?;
        let stderr = join_stderr(stderr_thread)?;
        if !status.success() {
            let msg = stderr.trim();
            let message = if msg.is_empty() {
                format!("pi exited with {status}")
            } else {
                format!("pi exited with {status}: {msg}")
            };
            return Err(RunnerError::new(message, parser.transcript(&stderr)).into());
        }

        let text = parser.final_text().trim().to_string();
        let output = if job.json_schema.trim().is_empty() {
            None
        } else {
            match extract_json_object(&text) {
                Ok(value) => Some(value),
                Err(err) => {
                    let message = format!(
                        "parse pi JSON output failed: {err}; output_tail={:?}",
                        tail_text(&text, 2000)
                    );
                    return Err(RunnerError::new(message, parser.transcript(&stderr)).into());
                }
            }
        };
        Ok(ResultData {
            output,
            usage: parser.usage().clone(),
            contract_warnings: parser.warnings().to_vec(),
        })
    }

    fn name(&self) -> &str {
        "pi"
    }

    fn metadata(&self) -> RunnerMetadata {
        self.metadata.clone()
    }

    fn pi_contract_observation(&self) -> Option<PiContractObservation> {
        Some(pi_contract::observation(&self.bin, &self.extra_args))
    }

    fn pi_command_spec(&self) -> Option<PiCommandSpec> {
        Some(self.command_spec())
    }
}

fn resolved_pi_bin(bin: &str) -> String {
    if bin.trim().is_empty() {
        "pi".to_string()
    } else {
        bin.to_string()
    }
}

fn join_parser(parser_thread: thread::JoinHandle<Result<PiParser>>) -> Result<PiParser> {
    parser_thread
        .join()
        .map_err(|_| anyhow::anyhow!("pi stdout parser panicked"))?
}

fn join_stderr(stderr_thread: thread::JoinHandle<Result<String>>) -> Result<String> {
    stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("pi stderr capture panicked"))?
}

#[derive(Default)]
struct OutputSpillStats {
    bytes: u64,
    newline_count: usize,
    ends_with_newline: bool,
}

impl OutputSpillStats {
    fn line_count(&self) -> usize {
        self.newline_count
            .saturating_add(usize::from(self.bytes > 0 && !self.ends_with_newline))
    }
}

struct TeeReader<R> {
    inner: R,
    spill: Option<File>,
    stats: Arc<Mutex<OutputSpillStats>>,
}

impl<R> TeeReader<R> {
    fn new(inner: R, spill: Option<File>, stats: Arc<Mutex<OutputSpillStats>>) -> Self {
        Self {
            inner,
            spill,
            stats,
        }
    }
}

impl<R: Read> Read for TeeReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            if let Some(spill) = self.spill.as_mut() {
                spill.write_all(&buf[..read]).map_err(|err| {
                    std::io::Error::new(err.kind(), format!("write Pi raw output spill: {err}"))
                })?;
            }
            let mut stats = self.stats.lock().expect("Pi output stats mutex poisoned");
            stats.bytes = stats.bytes.saturating_add(read as u64);
            stats.newline_count = stats
                .newline_count
                .saturating_add(buf[..read].iter().filter(|byte| **byte == b'\n').count());
            stats.ends_with_newline = buf[read - 1] == b'\n';
        }
        Ok(read)
    }
}

struct WorkerOutputCaptureGuard {
    stem: Option<PathBuf>,
    retained_output_bytes: usize,
    retained_output_lines: usize,
    stdout_stats: Arc<Mutex<OutputSpillStats>>,
    stderr_stats: Arc<Mutex<OutputSpillStats>>,
}

impl WorkerOutputCaptureGuard {
    fn new(
        stem: Option<PathBuf>,
        retained_output_bytes: usize,
        retained_output_lines: usize,
        stdout_stats: Arc<Mutex<OutputSpillStats>>,
        stderr_stats: Arc<Mutex<OutputSpillStats>>,
    ) -> Self {
        Self {
            stem,
            retained_output_bytes,
            retained_output_lines,
            stdout_stats,
            stderr_stats,
        }
    }
}

impl Drop for WorkerOutputCaptureGuard {
    fn drop(&mut self) {
        let Some(stem) = &self.stem else { return };
        let stdout_path = stem.with_extension("stdout.log");
        let stderr_path = stem.with_extension("stderr.log");
        let stdout_bytes = fs::metadata(&stdout_path).map_or(0, |metadata| metadata.len());
        let stderr_bytes = fs::metadata(&stderr_path).map_or(0, |metadata| metadata.len());
        let total_bytes = stdout_bytes.saturating_add(stderr_bytes);
        let stdout_lines = self
            .stdout_stats
            .lock()
            .expect("Pi stdout stats mutex poisoned")
            .line_count();
        let stderr_lines = self
            .stderr_stats
            .lock()
            .expect("Pi stderr stats mutex poisoned")
            .line_count();
        let _ = artifact::write_json(
            stem.with_extension("runtime.json"),
            &json!({
                "schema_version": 1,
                "capture": "bounded_tail_with_append_only_raw_spill",
                "total_bytes": total_bytes,
                "stdout_bytes": stdout_bytes,
                "stderr_bytes": stderr_bytes,
                "stdout_lines": stdout_lines,
                "stderr_lines": stderr_lines,
                "retained_output_bytes_per_stream": self.retained_output_bytes,
                "retained_output_lines_per_stream": self.retained_output_lines,
                "truncated": stdout_bytes > self.retained_output_bytes as u64
                    || stderr_bytes > self.retained_output_bytes as u64
                    || stdout_lines > self.retained_output_lines
                    || stderr_lines > self.retained_output_lines,
                "spill_paths": [stdout_path, stderr_path],
            }),
        );
    }
}

fn open_worker_output_spill(stem: Option<&Path>, extension: &str) -> Result<Option<File>> {
    let Some(stem) = stem else {
        return Ok(None);
    };
    let path = stem.with_extension(extension);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create Pi output spill directory {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open Pi output spill {}", path.display()))
        .map(Some)
}

fn append_bounded_utf8(target: &mut String, text: &str, max_bytes: usize, max_lines: usize) {
    if max_bytes == 0 || max_lines == 0 {
        target.clear();
        return;
    }
    target.push_str(text);
    if target.len() > max_bytes {
        let mut remove = target.len() - max_bytes;
        while remove < target.len() && !target.is_char_boundary(remove) {
            remove += 1;
        }
        target.drain(..remove);
    }
    while target.lines().count() > max_lines {
        let Some(newline) = target.find('\n') else {
            target.clear();
            break;
        };
        target.drain(..=newline);
    }
}

fn bounded_utf8_tail(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let mut tail = String::new();
    append_bounded_utf8(&mut tail, text, max_bytes, max_lines);
    tail
}

fn terminate_child(child: &mut std::process::Child, grace: Duration) {
    crate::workflow::shell::terminate_child_tree(
        child,
        crate::workflow::shell::ProcessSupervisionPolicy::process_group(grace),
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PiWrapperLaunchError {
    BeforePi(String),
    LaunchUncertain(String),
}

impl std::fmt::Display for PiWrapperLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforePi(message) | Self::LaunchUncertain(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PiWrapperLaunchError {}

#[derive(Debug, Clone, Deserialize)]
struct PiWrapperStatus {
    state: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    exit_code: Option<i32>,
}

const KHAZAD_WORKER_EXTENSION_INDEX_JS: &str = include_str!("../extensions/khazad-worker/index.js");

pub(crate) fn prepare_pi_wrapper_artifacts(
    spec: &PiCommandSpec,
    job: &Job,
    artifacts: &PiWrapperArtifacts,
) -> Result<String> {
    if let Some(parent) = artifacts.wrapper_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    for path in [
        &artifacts.stdout_path,
        &artifacts.stderr_path,
        &artifacts.exit_path,
        &artifacts.status_path,
        &artifacts.result_path,
    ] {
        let _ = fs::remove_file(path);
    }

    // A daemon can outlive an in-place binary upgrade. Resolve the reusable
    // replacement binary rather than embedding Linux's non-executable
    // `/path/khazad-doom (deleted)` current-exe display path in a wrapper.
    let atomic_json_writer = crate::paths::khazad_child_binary();
    write_private(
        &artifacts.prompt_path,
        build_prompt(&job.prompt, &job.json_schema),
    )?;
    write_private(&artifacts.env_path, env_file_text(&job.env))?;
    artifact::write_json(
        &artifacts.command_path,
        &json!({
            "bin": spec.bin,
            "args": spec.args,
            "cwd": job.cwd,
            "prompt_path": artifacts.prompt_path,
            "stdout_path": artifacts.stdout_path,
            "stderr_path": artifacts.stderr_path,
            "exit_path": artifacts.exit_path,
            "status_path": artifacts.status_path,
            "result_path": artifacts.result_path,
            "atomic_json_writer": &atomic_json_writer,
            "env_keys": effective_env(&job.env).keys().cloned().collect::<Vec<_>>(),
            "contract": "khazad-owned-herdr-pi-wrapper-v1",
        }),
    )?;
    write_private(
        &artifacts.wrapper_path,
        wrapper_script(spec, job, artifacts, &atomic_json_writer),
    )?;
    make_executable_private(&artifacts.wrapper_path)?;
    Ok(format!(
        "/bin/sh {}",
        shell_quote_path(&artifacts.wrapper_path)
    ))
}

pub(crate) fn prepare_pi_tui_worker_artifacts(
    spec: &PiCommandSpec,
    job: &Job,
    artifacts: &PiTuiWorkerArtifacts,
    session_name: &str,
) -> Result<Vec<String>> {
    if let Some(parent) = artifacts.prompt_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::create_dir_all(&artifacts.extension_dir)
        .with_context(|| format!("create {}", artifacts.extension_dir.display()))?;
    let _ = fs::remove_file(&artifacts.result_path);
    write_private(
        &artifacts.prompt_path,
        build_tui_worker_prompt(&job.prompt, &job.json_schema),
    )?;
    write_private(
        &artifacts.extension_index_path,
        KHAZAD_WORKER_EXTENSION_INDEX_JS.to_string(),
    )?;

    let mut argv = Vec::new();
    argv.push(spec.bin.clone());
    argv.extend(pi_contract::remove_json_mode_flags(&spec.args));
    argv.extend([
        "--extension".to_string(),
        artifacts.extension_dir.to_string_lossy().to_string(),
        "--name".to_string(),
        session_name.to_string(),
        format!("@{}", artifacts.prompt_path.to_string_lossy()),
    ]);
    artifact::write_json(
        &artifacts.command_path,
        &json!({
            "argv": argv,
            "cwd": job.cwd,
            "prompt_path": artifacts.prompt_path,
            "result_path": artifacts.result_path,
            "extension_dir": artifacts.extension_dir,
            "extension_index_path": artifacts.extension_index_path,
            "contract": "khazad-owned-herdr-pi-tui-worker-v1",
            "result_source": "khazad_worker_submit_worker_result_v1",
        }),
    )?;
    Ok(argv)
}

pub(crate) fn parse_pi_tui_worker_result_artifact(
    artifacts: &PiTuiWorkerArtifacts,
) -> Result<ResultData> {
    let value: Value = artifact::read_json(&artifacts.result_path).with_context(|| {
        format!(
            "read Pi TUI worker result {}",
            artifacts.result_path.display()
        )
    })?;
    let source = value
        .get("source")
        .and_then(Value::as_str)
        .context("Pi TUI worker result artifact omitted source")?;
    if source != "khazad_worker_submit_worker_result_v1" {
        bail!("Pi TUI worker result artifact had unexpected source {source:?}");
    }
    let output = value
        .get("result")
        .cloned()
        .context("Pi TUI worker result artifact omitted result")?;
    Ok(ResultData {
        output: Some(output),
        usage: Usage::default(),
        contract_warnings: Vec::new(),
    })
}

pub(crate) fn wait_for_pi_wrapper_launch(
    artifacts: &PiWrapperArtifacts,
    timeout: Duration,
    events: &Option<RunnerEventSink>,
) -> std::result::Result<u32, PiWrapperLaunchError> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = read_wrapper_status(&artifacts.status_path).map_err(|err| {
            PiWrapperLaunchError::LaunchUncertain(format!(
                "could not read Herdr worker launch status; Pi launch is uncertain: {err}"
            ))
        })?;
        if let Some(status) = status {
            if let Some(pid) = status.pid
                && matches!(status.state.as_str(), "launched" | "finished")
            {
                emit_runner_event(events, RunnerEvent::started(Some(pid)));
                return Ok(pid);
            }
            if matches!(status.state.as_str(), "handoff_failed" | "setup_failed") {
                return Err(PiWrapperLaunchError::BeforePi(format!(
                    "Herdr worker wrapper failed before launching Pi: {}",
                    status.state
                )));
            }
        }
        if artifacts.exit_path.exists() {
            return Err(PiWrapperLaunchError::LaunchUncertain(format!(
                "Herdr worker wrapper exited without a readable launch record; Pi launch is uncertain: {}",
                bounded_file_text(&artifacts.stderr_path, 2000, 100)
            )));
        }
        if Instant::now() >= deadline {
            return Err(PiWrapperLaunchError::LaunchUncertain(format!(
                "Herdr worker wrapper did not report a launched Pi process within {}s; Pi launch is uncertain",
                timeout.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Default)]
struct IncrementalFileLines {
    offset: u64,
    pending: Vec<u8>,
    pending_nonempty: bool,
    total_lines: usize,
}

pub(crate) fn collect_pi_wrapper_result(
    job: &Job,
    artifacts: &PiWrapperArtifacts,
    cancel: CancellationToken,
    events: Option<RunnerEventSink>,
    pid: u32,
) -> Result<ResultData> {
    let mut stdout_lines = IncrementalFileLines::default();
    let mut stderr_lines = IncrementalFileLines::default();
    let mut next_observation = Instant::now();
    let exit_code = loop {
        emit_new_file_lines(
            &artifacts.stdout_path,
            &mut stdout_lines,
            &events,
            pid,
            RunnerEvent::stdout,
            false,
            &job.runtime,
        )?;
        emit_new_file_lines(
            &artifacts.stderr_path,
            &mut stderr_lines,
            &events,
            pid,
            RunnerEvent::stderr,
            false,
            &job.runtime,
        )?;
        if cancel.is_cancelled() {
            terminate_wrapped_process(
                pid,
                &artifacts.exit_path,
                Duration::from_secs(job.termination_grace_seconds),
            );
            let transcript = wrapper_transcript(artifacts, Some(pid), &job.runtime);
            return Err(RunnerError::new("job cancelled", transcript).into());
        }
        if let Some(code) = read_wrapper_exit_code(artifacts)? {
            crate::workflow::shell::terminate_remaining_process_group(
                pid,
                Duration::from_secs(job.termination_grace_seconds),
            );
            emit_runner_event(&events, RunnerEvent::finished(Some(pid), Some(code)));
            break code;
        }
        if Instant::now() >= next_observation {
            emit_runner_event(&events, RunnerEvent::process_observed(Some(pid)));
            next_observation = Instant::now() + Duration::from_secs(1);
        }
        thread::sleep(Duration::from_millis(50));
    };

    emit_new_file_lines(
        &artifacts.stdout_path,
        &mut stdout_lines,
        &events,
        pid,
        RunnerEvent::stdout,
        true,
        &job.runtime,
    )?;
    emit_new_file_lines(
        &artifacts.stderr_path,
        &mut stderr_lines,
        &events,
        pid,
        RunnerEvent::stderr,
        true,
        &job.runtime,
    )?;
    let data = parse_pi_artifact_result(job, artifacts, exit_code, Some(pid))?;
    let stdout_bytes = fs::metadata(&artifacts.stdout_path).map_or(0, |metadata| metadata.len());
    let stderr_bytes = fs::metadata(&artifacts.stderr_path).map_or(0, |metadata| metadata.len());
    artifact::write_json(
        &artifacts.result_path,
        &json!({
            "output": data.output,
            "usage": data.usage,
            "contract_warnings": data.contract_warnings,
            "source": "khazad_owned_wrapper_artifacts",
            "output_capture": {
                "schema_version": 1,
                "capture": "bounded_tail_with_append_only_raw_spill",
                "total_bytes": stdout_bytes.saturating_add(stderr_bytes),
                "stdout_bytes": stdout_bytes,
                "stderr_bytes": stderr_bytes,
                "stdout_lines": stdout_lines.total_lines,
                "stderr_lines": stderr_lines.total_lines,
                "retained_output_bytes_per_stream": job.runtime.retained_output_bytes,
                "retained_output_lines_per_stream": job.runtime.retained_output_lines,
                "truncated": stdout_bytes > job.runtime.retained_output_bytes as u64
                    || stderr_bytes > job.runtime.retained_output_bytes as u64
                    || stdout_lines.total_lines > job.runtime.retained_output_lines
                    || stderr_lines.total_lines > job.runtime.retained_output_lines,
                "spill_paths": [artifacts.stdout_path.clone(), artifacts.stderr_path.clone()],
            },
        }),
    )?;
    Ok(data)
}

fn parse_pi_artifact_result(
    job: &Job,
    artifacts: &PiWrapperArtifacts,
    exit_code: i32,
    pid: Option<u32>,
) -> Result<ResultData> {
    let stdout = File::open(&artifacts.stdout_path).with_context(|| {
        format!(
            "open Pi stdout artifact {}",
            artifacts.stdout_path.display()
        )
    })?;
    let mut parser = PiParser::with_output_bounds(
        job.runtime.retained_output_bytes,
        job.runtime.retained_output_lines,
    );
    parser.parse(stdout, None, pid)?;
    let stderr = bounded_file_text(
        &artifacts.stderr_path,
        job.runtime.retained_output_bytes,
        job.runtime.retained_output_lines,
    );
    if exit_code != 0 {
        let status = format!("exit status: {exit_code}");
        let msg = stderr.trim();
        let message = if msg.is_empty() {
            format!("pi exited with {status}")
        } else {
            format!("pi exited with {status}: {msg}")
        };
        return Err(RunnerError::new(message, parser.transcript(&stderr)).into());
    }

    let text = parser.final_text().trim().to_string();
    let output = if job.json_schema.trim().is_empty() {
        None
    } else {
        match extract_json_object(&text) {
            Ok(value) => Some(value),
            Err(err) => {
                let message = format!(
                    "parse pi JSON output failed: {err}; output_tail={:?}",
                    tail_text(&text, 2000)
                );
                return Err(RunnerError::new(message, parser.transcript(&stderr)).into());
            }
        }
    };
    Ok(ResultData {
        output,
        usage: parser.usage().clone(),
        contract_warnings: parser.warnings().to_vec(),
    })
}

fn wrapper_transcript(
    artifacts: &PiWrapperArtifacts,
    pid: Option<u32>,
    runtime: &RuntimeConfig,
) -> RunnerTranscript {
    let stdout = File::open(&artifacts.stdout_path);
    let stderr = bounded_file_text(
        &artifacts.stderr_path,
        runtime.retained_output_bytes,
        runtime.retained_output_lines,
    );
    let mut parser =
        PiParser::with_output_bounds(runtime.retained_output_bytes, runtime.retained_output_lines);
    if let Ok(stdout) = stdout {
        let _ = parser.parse(stdout, None, pid);
    }
    parser.transcript(&stderr)
}

fn read_wrapper_exit_code(artifacts: &PiWrapperArtifacts) -> Result<Option<i32>> {
    if artifacts.exit_path.exists() {
        let value: Value = artifact::read_json(&artifacts.exit_path)?;
        return Ok(value
            .get("exit_code")
            .and_then(Value::as_i64)
            .map(|value| value as i32));
    }
    Ok(
        read_wrapper_status(&artifacts.status_path)?.and_then(|status| {
            (status.state == "finished")
                .then_some(status.exit_code)
                .flatten()
        }),
    )
}

fn read_wrapper_status(path: &Path) -> Result<Option<PiWrapperStatus>> {
    if !path.exists() {
        return Ok(None);
    }
    artifact::read_json(path).map(Some)
}

fn emit_new_file_lines(
    path: &Path,
    state: &mut IncrementalFileLines,
    events: &Option<RunnerEventSink>,
    pid: u32,
    make_event: fn(Option<u32>, String) -> RunnerEvent,
    final_read: bool,
    runtime: &RuntimeConfig,
) -> Result<()> {
    let Ok(mut file) = File::open(path) else {
        return Ok(());
    };
    file.seek(SeekFrom::Start(state.offset))?;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        state.offset = state.offset.saturating_add(read as u64);
        for segment in chunk[..read].split_inclusive(|byte| *byte == b'\n') {
            let complete = segment.ends_with(b"\n");
            let payload = segment.strip_suffix(b"\n").unwrap_or(segment);
            append_bounded_bytes(&mut state.pending, payload, runtime.retained_output_bytes);
            state.pending_nonempty |= !payload.is_empty();
            if complete {
                emit_pending_file_line(state, events, pid, make_event, runtime);
            }
        }
    }
    if final_read && state.pending_nonempty {
        emit_pending_file_line(state, events, pid, make_event, runtime);
    }
    Ok(())
}

fn append_bounded_bytes(target: &mut Vec<u8>, bytes: &[u8], max_bytes: usize) {
    if max_bytes == 0 {
        target.clear();
        return;
    }
    target.extend_from_slice(bytes);
    if target.len() > max_bytes {
        target.drain(..target.len() - max_bytes);
    }
}

fn emit_pending_file_line(
    state: &mut IncrementalFileLines,
    events: &Option<RunnerEventSink>,
    pid: u32,
    make_event: fn(Option<u32>, String) -> RunnerEvent,
    runtime: &RuntimeConfig,
) {
    let line = state.pending.strip_suffix(b"\r").unwrap_or(&state.pending);
    emit_runner_event(
        events,
        make_event(
            Some(pid),
            bounded_utf8_tail(
                &String::from_utf8_lossy(line),
                runtime.retained_output_bytes,
                runtime.retained_output_lines,
            ),
        ),
    );
    state.pending.clear();
    state.pending_nonempty = false;
    state.total_lines = state.total_lines.saturating_add(1);
}

fn terminate_wrapped_process(pid: u32, exit_path: &Path, grace: Duration) {
    crate::workflow::shell::terminate_process_group_until(
        pid,
        crate::workflow::shell::ProcessSupervisionPolicy::process_group(grace),
        || exit_path.exists(),
    );
}

fn wrapper_script(
    spec: &PiCommandSpec,
    job: &Job,
    artifacts: &PiWrapperArtifacts,
    atomic_json_writer: &Path,
) -> String {
    let command = std::iter::once(shell_quote(&spec.bin))
        .chain(spec.args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"#!/usr/bin/env sh
set -u
umask 077
STATUS={status}
EXIT={exit}
STDOUT={stdout}
STDERR={stderr}
PROMPT={prompt}
ENV_FILE={env_file}
CWD={cwd}
ATOMIC_JSON_WRITER={atomic_json_writer}
write_json() {{
  path=$1
  payload=$2
  printf '%s\n' "$payload" | "$ATOMIC_JSON_WRITER" {atomic_json_writer_arg} "$path"
}}
if ! write_json "$STATUS" '{{"state":"starting"}}'; then
  exit 125
fi
. "$ENV_FILE"
code=$?
if [ "$code" -ne 0 ]; then
  write_json "$STATUS" "{{\"state\":\"handoff_failed\",\"exit_code\":$code}}" || exit 125
  exit "$code"
fi
cd "$CWD"
code=$?
if [ "$code" -ne 0 ]; then
  write_json "$STATUS" "{{\"state\":\"handoff_failed\",\"exit_code\":$code}}" || exit 125
  exit "$code"
fi
: > "$STDOUT"
: > "$STDERR"
if ! command -v setsid >/dev/null 2>&1; then
  write_json "$STATUS" '{{"state":"handoff_failed","exit_code":127,"error":"setsid unavailable"}}' || exit 125
  exit 127
fi
setsid /bin/sh -c '
  status=$1
  writer=$2
  writer_arg=$3
  shift 3
  pid=$$
  if ! printf "{{\"state\":\"launched\",\"pid\":%s}}\\n" "$pid" | "$writer" "$writer_arg" "$status"; then
    exit 125
  fi
  exec "$@"
' sh "$STATUS" "$ATOMIC_JSON_WRITER" {atomic_json_writer_arg} env -i /bin/sh -c '. "$1"; shift; exec "$@"' sh "$ENV_FILE" {command} < "$PROMPT" > "$STDOUT" 2> "$STDERR" &
pid=$!
wait "$pid"
code=$?
if ! /bin/grep -q '"state"[[:space:]]*:[[:space:]]*"launched"' "$STATUS"; then
  write_json "$STATUS" "{{\"state\":\"handoff_failed\",\"exit_code\":$code}}" || exit 125
  exit "$code"
fi
exit_written=0
if write_json "$EXIT" "{{\"exit_code\":$code}}"; then
  exit_written=1
fi
if ! write_json "$STATUS" "{{\"state\":\"finished\",\"pid\":$pid,\"exit_code\":$code}}"; then
  [ "$exit_written" -eq 1 ] || exit 125
fi
exit "$code"
"#,
        status = shell_quote_path(&artifacts.status_path),
        exit = shell_quote_path(&artifacts.exit_path),
        stdout = shell_quote_path(&artifacts.stdout_path),
        stderr = shell_quote_path(&artifacts.stderr_path),
        prompt = shell_quote_path(&artifacts.prompt_path),
        env_file = shell_quote_path(&artifacts.env_path),
        cwd = shell_quote_path(&job.cwd),
        atomic_json_writer = shell_quote_path(atomic_json_writer),
        atomic_json_writer_arg = artifact::ATOMIC_JSON_WRITER_ARG,
        command = command,
    )
}

fn env_file_text(job_env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in effective_env(job_env) {
        if is_valid_env_key(&key) {
            out.push_str("export ");
            out.push_str(&key);
            out.push('=');
            out.push_str(&shell_quote(&value));
            out.push('\n');
        }
    }
    out
}

fn effective_env(job_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.extend(
        job_env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    env
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn write_private(path: &Path, text: String) -> Result<()> {
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    set_private_permissions(path, 0o600)
}

fn make_executable_private(path: &Path) -> Result<()> {
    set_private_permissions(path, 0o700)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn bounded_file_text(path: &Path, max_bytes: usize, max_lines: usize) -> String {
    if max_bytes == 0 || max_lines == 0 {
        return String::new();
    }
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let start = length.saturating_sub(max_bytes as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity((length - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    bounded_utf8_tail(&String::from_utf8_lossy(&bytes), max_bytes, max_lines)
}

fn tail_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) && start < text.len() {
        start += 1;
    }
    text[start..].to_string()
}

#[derive(Debug, Clone)]
pub struct FakeRunner;

impl Runner for FakeRunner {
    fn run(
        &self,
        job: Job,
        cancel: CancellationToken,
        events: Option<RunnerEventSink>,
    ) -> Result<ResultData> {
        if cancel.is_cancelled() {
            bail!("job cancelled");
        }
        emit_runner_event(&events, RunnerEvent::started(None));
        emit_runner_event(&events, RunnerEvent::process_observed(None));
        if job.kind == "integration-repair" {
            emit_runner_event(&events, RunnerEvent::finished(None, Some(0)));
            return Ok(ResultData {
                output: Some(
                    json!({ "status": "no-op", "summary": "fake runner deterministic test-double: no repair needed; not real Pi worker implementation evidence" }),
                ),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            });
        }
        let handoff_path = handoff_path_from_prompt(&job.prompt)?;
        let handoff: Handoff = artifact::read_json(&handoff_path)?;
        if cancel.is_cancelled() {
            bail!("job cancelled");
        }
        fs::create_dir_all(job.cwd.join(".khazad-fake"))?;
        fs::write(
            job.cwd.join(format!("{}.txt", handoff.slice.id)),
            format!("fake implementation for {}\n", handoff.slice.id),
        )?;
        fs::write(
            job.cwd
                .join(".khazad-fake")
                .join(format!("{}.txt", handoff.slice.id)),
            format!("{}\n", handoff.slice.title),
        )?;
        gitutil::run(&job.cwd, &["add", "."])?;
        gitutil::run(
            &job.cwd,
            &[
                "commit",
                "-m",
                &format!("khazad(fake): implement {}", handoff.slice.id),
            ],
        )?;
        let sha = gitutil::head_sha(&job.cwd)?;
        let acceptance_status = handoff
            .slice
            .acceptance
            .iter()
            .map(|criterion| {
                json!({
                    "criterion": criterion,
                    "status": "satisfied",
                    "evidence": format!("{} implemented by deterministic test-double fake runner; not real Pi worker implementation evidence", handoff.slice.id),
                })
            })
            .collect::<Vec<_>>();
        emit_runner_event(&events, RunnerEvent::finished(None, Some(0)));
        Ok(ResultData {
            output: Some(json!({
                "slice_id": handoff.slice.id,
                "status": "complete",
                "summary": "fake runner completed deterministic test-double slice implementation; not real Pi worker implementation evidence",
                "commit_sha": sha,
                "changed_files": [
                    format!("{}.txt", handoff.slice.id),
                    format!(".khazad-fake/{}.txt", handoff.slice.id)
                ],
                "tests_run": handoff.slice.verify,
                "acceptance_status": acceptance_status
            })),
            usage: Usage::default(),
            contract_warnings: Vec::new(),
        })
    }

    fn name(&self) -> &str {
        "fake"
    }

    fn metadata(&self) -> RunnerMetadata {
        fake_runner_metadata()
    }
}

pub fn fake_runner_metadata() -> RunnerMetadata {
    let mut source_attribution = BTreeMap::new();
    source_attribution.insert("agent".to_string(), "deterministic_test_double".to_string());
    RunnerMetadata {
        profile: "fake".to_string(),
        provider: "deterministic-test-double".to_string(),
        model: "deterministic-test-double".to_string(),
        reasoning: "none".to_string(),
        mode: "test".to_string(),
        profile_summary:
            "fake: deterministic test-double evidence (not real Pi worker implementation evidence)"
                .to_string(),
        launch_summary:
            "fake: deterministic test-double evidence (not real Pi worker implementation evidence)"
                .to_string(),
        source_attribution,
        ..RunnerMetadata::default()
    }
}

fn handoff_path_from_prompt(prompt: &str) -> Result<String> {
    let mut lines = prompt.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "Read this handoff JSON first:" {
            return lines
                .next()
                .map(|line| line.trim().to_string())
                .context("missing handoff path");
        }
    }
    bail!("handoff path not found")
}

fn build_prompt(prompt: &str, schema: &str) -> String {
    if schema.trim().is_empty() {
        return prompt.to_string();
    }
    format!(
        "{prompt}\n\n## Khazad-Doom final output contract\n\n\
         Your final assistant response must be only valid JSON matching this JSON Schema. \
         Do not wrap it in Markdown fences. Do not include prose before or after the JSON object.\n\n\
         {schema}\n"
    )
}

fn build_tui_worker_prompt(prompt: &str, schema: &str) -> String {
    if schema.trim().is_empty() {
        return prompt.to_string();
    }
    format!(
        "{prompt}\n\n## Khazad-Doom final output contract\n\n\
         You are running inside a native Pi TUI session hosted by Herdr. \
         Use the submit_worker_result tool as your final action. Its parameters must match this JSON Schema. \
         Do not paste final JSON into the terminal, do not wrap it in Markdown fences, and do not emit prose as the final answer. \
         Khazad-Doom reads only the submit_worker_result artifact as worker output; terminal text and Herdr scrollback are not evidence.\n\n\
         {schema}\n"
    )
}

fn extract_json_object(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(value);
    }
    let start = trimmed.find('{').context("no JSON object found")?;
    let end = trimmed.rfind('}').context("no JSON object found")?;
    if end <= start {
        bail!("no JSON object found");
    }
    let candidate = trimmed[start..=end].trim();
    serde_json::from_str(candidate).context("invalid JSON object")
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_AUTH_REQUIRED_FAILURE_KIND, CancellationToken, IncrementalFileLines, Job,
        PiCommandSpec, PiRunner, PiWrapperLaunchError, Runner, RunnerError, RunnerEvent,
        RunnerMetadata, RunnerSpec, RunnerTranscript, emit_new_file_lines, extract_json_object,
        parse_pi_tui_worker_result_artifact, prepare_pi_tui_worker_artifacts,
        prepare_pi_wrapper_artifacts, wait_for_pi_wrapper_launch,
    };
    use crate::artifact;
    use crate::domain::RuntimeConfig;
    use serde_json::json;
    use std::collections::{BTreeMap, HashSet};
    use std::fs;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn bounded_wrapper_line_reader_preserves_split_utf8_and_json_without_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stdout.jsonl");
        let bytes = "{\"type\":\"message_update\",\"text\":\"snowman ☃\"}\n".as_bytes();
        let split = bytes
            .windows("☃".len())
            .position(|window| window == "☃".as_bytes())
            .unwrap()
            + 1;
        fs::write(&path, &bytes[..split]).unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink_observed = observed.clone();
        let sink: super::RunnerEventSink = Arc::new(move |event| {
            sink_observed.lock().unwrap().push(event.text);
        });
        let mut state = super::IncrementalFileLines::default();
        let runtime = RuntimeConfig {
            retained_output_bytes: 64 * 1024,
            retained_output_lines: 1_000,
            ..RuntimeConfig::default()
        };
        emit_new_file_lines(
            &path,
            &mut state,
            &Some(sink.clone()),
            7,
            super::RunnerEvent::stdout,
            false,
            &runtime,
        )
        .unwrap();
        assert_eq!(state.offset, split as u64);
        assert_eq!(state.pending, bytes[..split]);
        assert!(observed.lock().unwrap().is_empty());

        fs::write(&path, bytes).unwrap();
        emit_new_file_lines(
            &path,
            &mut state,
            &Some(sink),
            7,
            super::RunnerEvent::stdout,
            true,
            &runtime,
        )
        .unwrap();
        assert_eq!(
            observed.lock().unwrap().as_slice(),
            &[String::from_utf8(bytes[..bytes.len() - 1].to_vec()).unwrap()]
        );
        assert_eq!(state.offset, bytes.len() as u64);
        assert!(state.pending.is_empty());
        assert_eq!(state.total_lines, 1);
    }

    #[test]
    fn zero_retention_wrapper_reader_counts_an_unterminated_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stdout.jsonl");
        fs::write(&path, "unterminated").unwrap();
        let runtime = RuntimeConfig {
            retained_output_bytes: 0,
            retained_output_lines: 0,
            raw_output_spill: true,
            ..RuntimeConfig::default()
        };
        let mut state = IncrementalFileLines::default();

        super::emit_new_file_lines(
            &path,
            &mut state,
            &None,
            7,
            RunnerEvent::stdout,
            true,
            &runtime,
        )
        .unwrap();

        assert_eq!(state.total_lines, 1);
        assert!(state.pending.is_empty());
        assert!(!state.pending_nonempty);
    }

    #[test]
    fn bounded_wrapper_line_reader_streams_multi_megabyte_delimiter_free_output() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stderr.log");
        let runtime = RuntimeConfig {
            retained_output_bytes: 1_024,
            retained_output_lines: 32,
            ..RuntimeConfig::default()
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink_observed = observed.clone();
        let sink: super::RunnerEventSink = Arc::new(move |event| {
            sink_observed.lock().unwrap().push(event.text);
        });
        let mut state = super::IncrementalFileLines::default();
        fs::write(&path, vec![b'x'; 4 * 1024 * 1024]).unwrap();
        emit_new_file_lines(
            &path,
            &mut state,
            &Some(sink.clone()),
            7,
            super::RunnerEvent::stderr,
            false,
            &runtime,
        )
        .unwrap();
        assert_eq!(state.offset, 4 * 1024 * 1024);
        assert!(state.pending.len() <= 1_024);
        assert!(observed.lock().unwrap().is_empty());

        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&vec![b'y'; 4 * 1024 * 1024]).unwrap();
        file.write_all(b"\n").unwrap();
        emit_new_file_lines(
            &path,
            &mut state,
            &Some(sink),
            7,
            super::RunnerEvent::stderr,
            false,
            &runtime,
        )
        .unwrap();
        assert_eq!(state.offset, 8 * 1024 * 1024 + 1);
        assert!(state.pending.is_empty());
        assert_eq!(state.total_lines, 1);
        assert_eq!(observed.lock().unwrap()[0].len(), 1_024);
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_normal_completion_reaps_a_surviving_term_ignoring_group() {
        let temp = tempfile::tempdir().unwrap();
        let store = artifact::Store::new(temp.path());
        let artifacts = store
            .pi_wrapper_artifacts_for_output_path(&temp.path().join("worker.json"))
            .unwrap();
        fs::write(&artifacts.stdout_path, "").unwrap();
        fs::write(&artifacts.stderr_path, "").unwrap();
        artifact::write_json(&artifacts.exit_path, &json!({"exit_code": 0})).unwrap();
        let mut child = std::process::Command::new("setsid")
            .args(["sh", "-c", "(trap '' TERM; sleep 30) & exit 0"])
            .spawn()
            .unwrap();
        let pgid = child.id();
        let _ = child.wait();
        let job = Job {
            kind: "worker".to_string(),
            prompt: String::new(),
            cwd: temp.path().to_path_buf(),
            json_schema: String::new(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };

        super::collect_pi_wrapper_result(&job, &artifacts, CancellationToken::new(), None, pgid)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while crate::workflow::shell::supervised_process_group_exists(pgid)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::workflow::shell::supervised_process_group_exists(
            pgid
        ));
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_cancellation_kills_term_ignoring_group_even_after_exit_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let exit_path = temp.path().join("exit.json");
        fs::write(&exit_path, "{}").unwrap();
        let mut child = std::process::Command::new("setsid")
            .args(["sh", "-c", "(trap '' TERM; sleep 30) & wait"])
            .spawn()
            .unwrap();
        let pgid = child.id();
        thread::sleep(Duration::from_millis(25));

        super::terminate_wrapped_process(pgid, &exit_path, Duration::from_millis(50));
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(1);
        while crate::workflow::shell::supervised_process_group_exists(pgid)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::workflow::shell::supervised_process_group_exists(
            pgid
        ));
    }

    #[test]
    fn wrapper_preserves_authoritative_result_larger_than_diagnostic_tail() {
        let temp = tempfile::tempdir().unwrap();
        let store = artifact::Store::new(temp.path());
        let output_path = temp.path().join("worker-output.json");
        let artifacts = store
            .pi_wrapper_artifacts_for_output_path(&output_path)
            .unwrap();
        let payload = serde_json::json!({"payload": "x".repeat(100 * 1024)}).to_string();
        let event = serde_json::json!({
            "type": "agent_end",
            "messages": [{
                "role": "assistant",
                "content": [{"type": "text", "text": payload}]
            }]
        });
        fs::write(&artifacts.stdout_path, format!("{event}\n")).unwrap();
        fs::write(&artifacts.stderr_path, "").unwrap();
        let job = Job {
            kind: "worker".to_string(),
            prompt: String::new(),
            cwd: temp.path().to_path_buf(),
            json_schema: "{}".to_string(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
            runtime: RuntimeConfig {
                retained_output_bytes: 1_024,
                retained_output_lines: 32,
                ..RuntimeConfig::default()
            },
            raw_output_stem: None,
        };

        let result = super::parse_pi_artifact_result(&job, &artifacts, 0, None).unwrap();
        assert_eq!(
            result.output.unwrap()["payload"].as_str().unwrap().len(),
            100 * 1024
        );
    }

    #[test]
    fn direct_pi_spill_setup_removes_partial_new_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let stem = temp.path().join("capture");
        fs::create_dir(stem.with_extension("stderr.log")).unwrap();
        let runner = PiRunner {
            bin: "sh".to_string(),
            extra_args: Vec::new(),
            metadata: RunnerMetadata::default(),
        };
        let job = Job {
            kind: "worker".to_string(),
            prompt: String::new(),
            cwd: temp.path().to_path_buf(),
            json_schema: String::new(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
            runtime: RuntimeConfig::default(),
            raw_output_stem: Some(stem.clone()),
        };

        let error = runner
            .run(job, CancellationToken::new(), None)
            .expect_err("second direct spill open must fail");
        assert!(format!("{error:#}").contains("output spill"));
        assert!(!stem.with_extension("stdout.log").exists());
        assert!(!stem.with_extension("runtime.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn direct_pi_metadata_reports_line_limit_truncation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-pi-lines");
        fs::write(
            &script,
            "#!/bin/sh\ni=0; while [ $i -lt 40 ]; do echo x >&2; i=$((i + 1)); done\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let stem = temp.path().join("capture");
        let runner = PiRunner {
            bin: script.to_string_lossy().into_owned(),
            extra_args: Vec::new(),
            metadata: RunnerMetadata::default(),
        };
        let job = Job {
            kind: "worker".to_string(),
            prompt: String::new(),
            cwd: temp.path().to_path_buf(),
            json_schema: String::new(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
            runtime: RuntimeConfig {
                retained_output_bytes: 1_024,
                retained_output_lines: 32,
                ..RuntimeConfig::default()
            },
            raw_output_stem: Some(stem.clone()),
        };

        runner.run(job, CancellationToken::new(), None).unwrap();
        let metadata: serde_json::Value =
            artifact::read_json(stem.with_extension("runtime.json")).unwrap();
        assert_eq!(metadata["stderr_lines"], 40);
        assert_eq!(metadata["truncated"], true);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_direct_pi_stderr_spills_full_output_and_retains_only_configured_tail() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-pi");
        fs::write(
            &script,
            "#!/bin/sh\nhead -c 2097152 /dev/zero | tr '\\0' e >&2\nexit 17\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let runner = PiRunner {
            bin: script.to_string_lossy().into_owned(),
            extra_args: Vec::new(),
            metadata: RunnerMetadata::default(),
        };
        let runtime = RuntimeConfig {
            retained_output_bytes: 1024,
            retained_output_lines: 32,
            ..RuntimeConfig::default()
        };
        let stem = temp.path().join("direct-worker");
        let error = runner
            .run(
                Job {
                    kind: "test".to_string(),
                    prompt: "test".to_string(),
                    cwd: temp.path().to_path_buf(),
                    json_schema: String::new(),
                    env: BTreeMap::new(),
                    termination_grace_seconds: 0,
                    runtime,
                    raw_output_stem: Some(stem.clone()),
                },
                CancellationToken::new(),
                None,
            )
            .unwrap_err();
        let runner_error = error.downcast_ref::<RunnerError>().unwrap();
        assert!(runner_error.transcript().stderr_tail.len() <= 1024);
        assert_eq!(
            fs::metadata(stem.with_extension("stderr.log"))
                .unwrap()
                .len(),
            2 * 1024 * 1024
        );
        let metadata: serde_json::Value =
            artifact::read_json(stem.with_extension("runtime.json")).unwrap();
        assert_eq!(metadata["total_bytes"], 2 * 1024 * 1024);
        assert_eq!(metadata["truncated"], true);
    }

    #[test]
    fn classifies_pi_auth_failure_only_without_assistant_output() {
        let metadata = RunnerMetadata {
            profile: "implementer".to_string(),
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning: "xhigh".to_string(),
            mode: "fast".to_string(),
            profile_summary: String::new(),
            launch_summary: String::new(),
            fix_commands: Vec::new(),
            source_attribution: BTreeMap::new(),
        };
        let transcript = RunnerTranscript {
            stderr_tail: crate::pi_contract::auth_failure_stderr_fixture("openai"),
            ..RunnerTranscript::default()
        };
        let err = RunnerError::new("pi exited with status 1", transcript);

        let classification = err
            .classify_launch_failure(&metadata)
            .expect("auth failure should be classified");
        assert_eq!(
            classification.failure_kind,
            AGENT_AUTH_REQUIRED_FAILURE_KIND
        );
        assert!(!classification.retryable);
        assert!(classification.operator_action_required);
        assert!(classification.summary.contains("openai"));
        assert!(
            classification
                .fix_commands
                .iter()
                .any(|cmd| cmd == "pi /login")
        );

        let with_assistant = RunnerError::new(
            "worker mentioned auth after starting",
            RunnerTranscript {
                assistant_tail: "implementation failed after mentioning an auth problem"
                    .to_string(),
                stderr_tail: crate::pi_contract::auth_failure_stderr_fixture("openai"),
                ..RunnerTranscript::default()
            },
        );
        assert!(with_assistant.classify_launch_failure(&metadata).is_none());

        let unknown = RunnerError::new(
            "pi exited with status 1",
            RunnerTranscript {
                stderr_tail: "connection reset by peer".to_string(),
                ..RunnerTranscript::default()
            },
        );
        assert!(unknown.classify_launch_failure(&metadata).is_none());
    }

    #[test]
    fn parses_explicit_runner_specs() {
        let fake = RunnerSpec::from_agent_and_env("fake").unwrap();
        assert_eq!(fake.kind, "fake");
        assert!(RunnerSpec::from_agent_and_env("bogus").is_err());
    }

    #[test]
    fn extracts_embedded_json_object() {
        let value = extract_json_object("prose {\"status\":\"ok\"} trailing").unwrap();
        assert_eq!(value["status"], "ok");
    }

    #[test]
    fn prepares_tui_worker_artifacts_with_embedded_extension_and_without_json_mode() {
        let temp = tempfile::tempdir().unwrap();
        let store = artifact::Store::new(temp.path());
        let artifacts = store
            .pi_tui_worker_artifacts_for_output_path(
                &temp.path().join("slice.worker.attempt-1.json"),
            )
            .unwrap();
        let spec = PiCommandSpec {
            bin: "pi".to_string(),
            args: vec![
                "--provider".to_string(),
                "openai-codex".to_string(),
                "--mode".to_string(),
                "json".to_string(),
                "--no-session".to_string(),
            ],
        };
        let job = Job {
            kind: "slice-worker".to_string(),
            prompt: "Implement the slice.".to_string(),
            cwd: temp.path().to_path_buf(),
            json_schema: "{\"type\":\"object\"}".to_string(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };

        let argv =
            prepare_pi_tui_worker_artifacts(&spec, &job, &artifacts, "session-name").unwrap();
        let argv_set: HashSet<_> = argv.iter().map(String::as_str).collect();
        assert_eq!(argv[0], "pi");
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--provider", "openai-codex"])
        );
        assert!(!argv.windows(2).any(|pair| pair == ["--mode", "json"]));
        assert!(!argv_set.contains("--no-session"));
        assert!(argv.windows(2).any(|pair| {
            pair[0] == "--extension" && pair[1] == artifacts.extension_dir.to_string_lossy()
        }));
        assert!(!argv.iter().any(|arg| arg == "--no-extensions"));
        assert!(
            std::fs::read_to_string(&artifacts.prompt_path)
                .unwrap()
                .contains("Use the submit_worker_result tool as your final action")
        );
        assert!(
            std::fs::read_to_string(&artifacts.extension_index_path)
                .unwrap()
                .contains("submit_worker_result")
        );
    }

    #[test]
    fn legacy_wrapper_uses_daemon_atomic_json_writer_for_coordination() {
        let temp = tempfile::tempdir().unwrap();
        let store = artifact::Store::new(temp.path());
        let artifacts = store
            .pi_wrapper_artifacts_for_output_path(&temp.path().join("slice.worker.attempt-1.json"))
            .unwrap();
        let spec = PiCommandSpec {
            bin: "pi".to_string(),
            args: vec!["--provider".to_string(), "openai-codex".to_string()],
        };
        let job = Job {
            kind: "slice-worker".to_string(),
            prompt: "Implement the slice.".to_string(),
            cwd: temp.path().to_path_buf(),
            json_schema: "{\"type\":\"object\"}".to_string(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };

        prepare_pi_wrapper_artifacts(&spec, &job, &artifacts).unwrap();

        let script = std::fs::read_to_string(&artifacts.wrapper_path).unwrap();
        assert!(script.contains("ATOMIC_JSON_WRITER="));
        assert!(script.contains(crate::artifact::ATOMIC_JSON_WRITER_ARG));
        assert!(!script.contains("> \"$STATUS\""));
        assert!(!script.contains("> \"$EXIT\""));
        assert!(script.contains("command -v setsid"));
        assert!(script.contains("setsid /bin/sh -c"));
        let command: serde_json::Value = artifact::read_json(&artifacts.command_path).unwrap();
        assert!(command["atomic_json_writer"].is_string());
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_records_prelaunch_failure_when_discovered_setsid_cannot_launch() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let setsid = bin.join("setsid");
        fs::write(&setsid, "#!/bin/sh\nexit 42\n").unwrap();
        let writer = bin.join("atomic-writer");
        fs::write(&writer, "#!/bin/sh\n/bin/cat > \"$2\"\n").unwrap();
        for path in [&setsid, &writer] {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
        let store = artifact::Store::new(temp.path());
        let artifacts = store
            .pi_wrapper_artifacts_for_output_path(&temp.path().join("worker.json"))
            .unwrap();
        let spec = PiCommandSpec {
            bin: "pi-never-runs".to_string(),
            args: Vec::new(),
        };
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), bin.to_string_lossy().to_string());
        let job = Job {
            kind: "worker".to_string(),
            prompt: String::new(),
            cwd: temp.path().to_path_buf(),
            json_schema: String::new(),
            env,
            termination_grace_seconds: 0,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };
        fs::write(&artifacts.prompt_path, "").unwrap();
        fs::write(&artifacts.env_path, super::env_file_text(&job.env)).unwrap();
        fs::write(
            &artifacts.wrapper_path,
            super::wrapper_script(&spec, &job, &artifacts, &writer),
        )
        .unwrap();
        let mut permissions = fs::metadata(&artifacts.wrapper_path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&artifacts.wrapper_path, permissions).unwrap();

        let status = std::process::Command::new(&artifacts.wrapper_path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(42));
        let launch: serde_json::Value = artifact::read_json(&artifacts.status_path).unwrap();
        assert_eq!(launch["state"], "handoff_failed");
        assert_eq!(launch["exit_code"], 42);
    }

    #[test]
    fn wrapper_launch_fallback_requires_durable_prelaunch_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let store = artifact::Store::new(temp.path());
        let artifacts = store
            .pi_wrapper_artifacts_for_output_path(&temp.path().join("worker.json"))
            .unwrap();

        let uncertain = wait_for_pi_wrapper_launch(&artifacts, Duration::ZERO, &None)
            .expect_err("a missing launch record is uncertain, not proof Pi never started");
        assert!(matches!(
            uncertain,
            PiWrapperLaunchError::LaunchUncertain(_)
        ));

        artifact::write_json(&artifacts.status_path, &json!({"state": "handoff_failed"})).unwrap();
        let before_pi = wait_for_pi_wrapper_launch(&artifacts, Duration::ZERO, &None)
            .expect_err("an explicit handoff failure should remain a prelaunch fallback");
        assert!(matches!(before_pi, PiWrapperLaunchError::BeforePi(_)));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_term_ignoring_pi_grandchild_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-pi");
        let ready = temp.path().join("ready");
        let marker = temp.path().join("grandchild-marker");
        std::fs::write(
            &script,
            "#!/bin/sh\n(trap '' TERM; sleep 3; : > \"$MARKER\") &\n: > \"$READY\"\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let mut env = BTreeMap::new();
        env.insert("READY".to_string(), ready.to_string_lossy().into_owned());
        env.insert("MARKER".to_string(), marker.to_string_lossy().into_owned());
        let runner = PiRunner {
            bin: script.to_string_lossy().into_owned(),
            extra_args: Vec::new(),
            metadata: RunnerMetadata::default(),
        };
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        let job = Job {
            kind: "test".to_string(),
            prompt: "test".to_string(),
            cwd: temp.path().to_path_buf(),
            json_schema: "{}".to_string(),
            env,
            termination_grace_seconds: 1,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };
        let result = thread::spawn(move || runner.run(job, cancelled, None));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "fake Pi did not create its ready signal");
        cancel.cancel();
        assert!(result.join().unwrap().is_err());
        thread::sleep(Duration::from_secs(3));
        assert!(
            !marker.exists(),
            "a TERM-ignoring Pi grandchild survived cancellation and wrote its marker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_parent_exit_cleanup_honors_descendant_grace() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-pi");
        let ready = temp.path().join("ready-after-parent-exit");
        let marker = temp.path().join("descendant-grace-marker");
        std::fs::write(
            &script,
            "#!/bin/sh\n(trap 'sleep 0.3; : > \"$MARKER\"; exit 0' TERM; : > \"$READY\"; while :; do sleep 0.05; done) &\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let mut env = BTreeMap::new();
        env.insert("READY".to_string(), ready.to_string_lossy().into_owned());
        env.insert("MARKER".to_string(), marker.to_string_lossy().into_owned());
        let runner = PiRunner {
            bin: script.to_string_lossy().into_owned(),
            extra_args: Vec::new(),
            metadata: RunnerMetadata::default(),
        };
        let job = Job {
            kind: "test".to_string(),
            prompt: "test".to_string(),
            cwd: temp.path().to_path_buf(),
            json_schema: "{}".to_string(),
            env,
            termination_grace_seconds: 1,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };
        let result = thread::spawn(move || runner.run(job, CancellationToken::new(), None));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "fake Pi descendant did not start");
        let _ = result.join().unwrap();
        assert!(
            marker.exists(),
            "post-parent-exit cleanup killed the descendant without the configured TERM grace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_honors_descendant_grace_after_pi_parent_exits() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("fake-pi");
        let ready = temp.path().join("ready");
        let marker = temp.path().join("grandchild-grace-marker");
        std::fs::write(
            &script,
            "#!/bin/sh\n(trap 'sleep 0.3; : > \"$MARKER\"; exit 0' TERM; while :; do sleep 0.05; done) &\n: > \"$READY\"\nwait\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let mut env = BTreeMap::new();
        env.insert("READY".to_string(), ready.to_string_lossy().into_owned());
        env.insert("MARKER".to_string(), marker.to_string_lossy().into_owned());
        let runner = PiRunner {
            bin: script.to_string_lossy().into_owned(),
            extra_args: Vec::new(),
            metadata: RunnerMetadata::default(),
        };
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        let job = Job {
            kind: "test".to_string(),
            prompt: "test".to_string(),
            cwd: temp.path().to_path_buf(),
            json_schema: "{}".to_string(),
            env,
            termination_grace_seconds: 1,
            runtime: RuntimeConfig::default(),
            raw_output_stem: None,
        };
        let result = thread::spawn(move || runner.run(job, cancelled, None));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "fake Pi did not create its ready signal");
        cancel.cancel();
        assert!(result.join().unwrap().is_err());
        assert!(
            marker.exists(),
            "Pi descendant did not receive its configured graceful shutdown window"
        );
    }

    #[test]
    fn parses_tui_worker_result_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let store = artifact::Store::new(temp.path());
        let artifacts = store
            .pi_tui_worker_artifacts_for_output_path(
                &temp.path().join("slice.worker.attempt-1.json"),
            )
            .unwrap();
        std::fs::create_dir_all(artifacts.result_path.parent().unwrap()).unwrap();
        artifact::write_json(
            &artifacts.result_path,
            &json!({
                "schema_version": 1,
                "source": "khazad_worker_submit_worker_result_v1",
                "result": {
                    "slice_id": "slice-001",
                    "status": "complete",
                    "summary": "done",
                    "acceptance_status": []
                }
            }),
        )
        .unwrap();

        let data = parse_pi_tui_worker_result_artifact(&artifacts).unwrap();
        let output = data.output.unwrap();
        assert_eq!(output["slice_id"], "slice-001");
        assert_eq!(output["status"], "complete");
    }
}
