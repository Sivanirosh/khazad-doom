use crate::domain::Handoff;
use crate::pi_contract::{self, PiContractObservation, PiContractWarning, PiParser};
use crate::{artifact, gitutil};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
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
        let bin = if self.bin.trim().is_empty() {
            "pi"
        } else {
            &self.bin
        };
        let mut cmd = Command::new(bin);
        cmd.args(pi_contract::launch_args(&self.extra_args))
            .envs(&job.env)
            .current_dir(&job.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| format!("start {bin}"))?;
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
            Some(
                extract_json_object(&text)
                    .with_context(|| format!("parse pi JSON output from {text:?}"))?,
            )
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
                    json!({ "status": "no-op", "summary": "fake runner: no repair needed" }),
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
                    "evidence": format!("{} implemented by fake runner", handoff.slice.id),
                })
            })
            .collect::<Vec<_>>();
        emit_runner_event(&events, RunnerEvent::finished(None, Some(0)));
        Ok(ResultData {
            output: Some(json!({
                "slice_id": handoff.slice.id,
                "status": "complete",
                "summary": "fake runner completed deterministic slice implementation",
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
        AGENT_AUTH_REQUIRED_FAILURE_KIND, RunnerError, RunnerMetadata, RunnerSpec,
        RunnerTranscript, extract_json_object,
    };
    use std::collections::BTreeMap;

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
}
