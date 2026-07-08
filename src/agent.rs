use crate::artifact::{PiTuiWorkerArtifacts, PiWrapperArtifacts};
use crate::domain::Handoff;
use crate::pi_contract::{self, PiContractObservation, PiContractWarning, PiParser};
use crate::{artifact, gitutil};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
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
        let mut cmd = Command::new(&spec.bin);
        cmd.args(&spec.args)
            .envs(&job.env)
            .current_dir(&job.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| format!("start {}", spec.bin))?;
        let pid = child.id();
        emit_runner_event(&events, RunnerEvent::started(Some(pid)));

        {
            let mut stdin = child.stdin.take().context("pi stdin")?;
            stdin.write_all(build_prompt(&job.prompt, &job.json_schema).as_bytes())?;
        }

        let stderr = child.stderr.take().context("pi stderr")?;
        let stderr_events = events.clone();
        let stderr_thread = thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut buf = String::new();
            for line in reader.lines() {
                let Ok(line) = line else { break };
                emit_runner_event(&stderr_events, RunnerEvent::stderr(Some(pid), line.clone()));
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        let stdout = child.stdout.take().context("pi stdout")?;
        let stdout_events = events.clone();
        let parser_thread = thread::spawn(move || {
            let mut parser = PiParser::default();
            parser.parse(stdout, stdout_events, Some(pid))?;
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
                let stderr = stderr_thread.join().unwrap_or_default();
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

        let parser = join_parser(parser_thread)?;
        let stderr = stderr_thread.join().unwrap_or_default();
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

fn terminate_child(child: &mut std::process::Child, grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if grace.as_millis() > 0 {
        request_child_terminate(child.id());
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn request_child_terminate(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn request_child_terminate(_pid: u32) {}

#[cfg(unix)]
fn request_child_kill(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn request_child_kill(_pid: u32) {}

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
            "env_keys": effective_env(&job.env).keys().cloned().collect::<Vec<_>>(),
            "contract": "khazad-owned-herdr-pi-wrapper-v1",
        }),
    )?;
    write_private(
        &artifacts.wrapper_path,
        wrapper_script(spec, job, artifacts),
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
    argv.extend(pi_tui_args_from_json_spec(&spec.args));
    argv.extend([
        "--no-extensions".to_string(),
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

fn pi_tui_args_from_json_spec(args: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--mode" && args.get(index + 1).is_some_and(|value| value == "json") {
            index += 2;
            continue;
        }
        if args[index] == "--no-session" {
            index += 1;
            continue;
        }
        filtered.push(args[index].clone());
        index += 1;
    }
    filtered
}

pub(crate) fn wait_for_pi_wrapper_launch(
    artifacts: &PiWrapperArtifacts,
    timeout: Duration,
    events: &Option<RunnerEventSink>,
) -> Result<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = read_wrapper_status(&artifacts.status_path)? {
            if let Some(pid) = status.pid
                && matches!(status.state.as_str(), "launched" | "finished")
            {
                emit_runner_event(events, RunnerEvent::started(Some(pid)));
                return Ok(pid);
            }
            if matches!(status.state.as_str(), "handoff_failed" | "setup_failed") {
                bail!(
                    "Herdr worker wrapper failed before launching Pi: {}",
                    status.state
                );
            }
        }
        if artifacts.exit_path.exists() {
            bail!(
                "Herdr worker wrapper exited before reporting a launched Pi process: {}",
                bounded_file_text(&artifacts.stderr_path, 2000)
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "Herdr worker wrapper did not report a launched Pi process within {}s",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn collect_pi_wrapper_result(
    job: &Job,
    artifacts: &PiWrapperArtifacts,
    cancel: CancellationToken,
    events: Option<RunnerEventSink>,
    pid: u32,
) -> Result<ResultData> {
    let mut stdout_offset = 0_u64;
    let mut stderr_offset = 0_u64;
    let mut next_observation = Instant::now();
    let exit_code = loop {
        emit_new_file_lines(
            &artifacts.stdout_path,
            &mut stdout_offset,
            &events,
            pid,
            RunnerEvent::stdout,
        )?;
        emit_new_file_lines(
            &artifacts.stderr_path,
            &mut stderr_offset,
            &events,
            pid,
            RunnerEvent::stderr,
        )?;
        if cancel.is_cancelled() {
            terminate_wrapped_process(
                pid,
                &artifacts.exit_path,
                Duration::from_secs(job.termination_grace_seconds),
            );
            let transcript = wrapper_transcript(artifacts, Some(pid));
            return Err(RunnerError::new("job cancelled", transcript).into());
        }
        if let Some(code) = read_wrapper_exit_code(artifacts)? {
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
        &mut stdout_offset,
        &events,
        pid,
        RunnerEvent::stdout,
    )?;
    emit_new_file_lines(
        &artifacts.stderr_path,
        &mut stderr_offset,
        &events,
        pid,
        RunnerEvent::stderr,
    )?;
    let data = parse_pi_artifact_result(job, artifacts, exit_code, Some(pid))?;
    artifact::write_json(
        &artifacts.result_path,
        &json!({
            "output": data.output,
            "usage": data.usage,
            "contract_warnings": data.contract_warnings,
            "source": "khazad_owned_wrapper_artifacts",
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
    let mut parser = PiParser::default();
    parser.parse(stdout, None, pid)?;
    let stderr = fs::read_to_string(&artifacts.stderr_path).unwrap_or_default();
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

fn wrapper_transcript(artifacts: &PiWrapperArtifacts, pid: Option<u32>) -> RunnerTranscript {
    let stdout = File::open(&artifacts.stdout_path);
    let stderr = fs::read_to_string(&artifacts.stderr_path).unwrap_or_default();
    let mut parser = PiParser::default();
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
    offset: &mut u64,
    events: &Option<RunnerEventSink>,
    pid: u32,
    make_event: fn(Option<u32>, String) -> RunnerEvent,
) -> Result<()> {
    let Ok(mut file) = File::open(path) else {
        return Ok(());
    };
    file.seek(SeekFrom::Start(*offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    *offset += text.len() as u64;
    for line in text.lines() {
        emit_runner_event(events, make_event(Some(pid), line.to_string()));
    }
    Ok(())
}

fn terminate_wrapped_process(pid: u32, exit_path: &Path, grace: Duration) {
    request_child_terminate(pid);
    if grace.as_millis() > 0 {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if exit_path.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    request_child_kill(pid);
}

fn wrapper_script(spec: &PiCommandSpec, job: &Job, artifacts: &PiWrapperArtifacts) -> String {
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
printf '{{"state":"starting"}}\n' > "$STATUS"
. "$ENV_FILE"
code=$?
if [ "$code" -ne 0 ]; then
  printf '{{"state":"handoff_failed","exit_code":%s}}\n' "$code" > "$STATUS"
  exit "$code"
fi
cd "$CWD"
code=$?
if [ "$code" -ne 0 ]; then
  printf '{{"state":"handoff_failed","exit_code":%s}}\n' "$code" > "$STATUS"
  exit "$code"
fi
: > "$STDOUT"
: > "$STDERR"
env -i /bin/sh -c '. "$1"; shift; exec "$@"' sh "$ENV_FILE" {command} < "$PROMPT" > "$STDOUT" 2> "$STDERR" &
pid=$!
printf '{{"state":"launched","pid":%s}}\n' "$pid" > "$STATUS"
wait "$pid"
code=$?
printf '{{"exit_code":%s}}\n' "$code" > "$EXIT"
printf '{{"state":"finished","pid":%s,"exit_code":%s}}\n' "$pid" "$code" > "$STATUS"
exit "$code"
"#,
        status = shell_quote_path(&artifacts.status_path),
        exit = shell_quote_path(&artifacts.exit_path),
        stdout = shell_quote_path(&artifacts.stdout_path),
        stderr = shell_quote_path(&artifacts.stderr_path),
        prompt = shell_quote_path(&artifacts.prompt_path),
        env_file = shell_quote_path(&artifacts.env_path),
        cwd = shell_quote_path(&job.cwd),
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

fn bounded_file_text(path: &Path, max_bytes: usize) -> String {
    fs::read_to_string(path)
        .map(|text| tail_text(&text, max_bytes))
        .unwrap_or_default()
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
        AGENT_AUTH_REQUIRED_FAILURE_KIND, Job, PiCommandSpec, RunnerError, RunnerMetadata,
        RunnerSpec, RunnerTranscript, extract_json_object, parse_pi_tui_worker_result_artifact,
        prepare_pi_tui_worker_artifacts,
    };
    use crate::artifact;
    use serde_json::json;
    use std::collections::{BTreeMap, HashSet};

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
        assert!(argv.iter().any(|arg| arg == "--no-extensions"));
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
