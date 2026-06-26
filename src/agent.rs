use crate::domain::Handoff;
use crate::{artifact, gitutil};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerEvent {
    pub kind: RunnerEventKind,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
}

impl RunnerEvent {
    pub fn started(pid: Option<u32>) -> Self {
        Self {
            kind: RunnerEventKind::Started,
            pid,
            exit_code: None,
        }
    }

    pub fn process_observed(pid: Option<u32>) -> Self {
        Self {
            kind: RunnerEventKind::ProcessObserved,
            pid,
            exit_code: None,
        }
    }

    pub fn stdout(pid: Option<u32>) -> Self {
        Self {
            kind: RunnerEventKind::Stdout,
            pid,
            exit_code: None,
        }
    }

    pub fn stderr(pid: Option<u32>) -> Self {
        Self {
            kind: RunnerEventKind::Stderr,
            pid,
            exit_code: None,
        }
    }

    pub fn finished(pid: Option<u32>, exit_code: Option<i32>) -> Self {
        Self {
            kind: RunnerEventKind::Finished,
            pid,
            exit_code,
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
    pub termination_grace_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct ResultData {
    #[allow(dead_code)]
    pub text: String,
    pub output: Option<Value>,
    #[allow(dead_code)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input_tokens: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output_tokens: usize,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSpec {
    pub kind: String,
    pub pi_bin: String,
    pub pi_args: Vec<String>,
}

impl RunnerSpec {
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
            }),
            "fake" => Ok(Self {
                kind,
                pi_bin: String::new(),
                pi_args: Vec::new(),
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
        }),
    }
}

#[derive(Debug, Clone)]
pub struct PiRunner {
    pub bin: String,
    pub extra_args: Vec<String>,
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
        cmd.args(&self.extra_args)
            .args(["--mode", "json", "--no-session"])
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
                emit_runner_event(&stderr_events, RunnerEvent::stderr(Some(pid)));
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
                let _ = parser_thread.join();
                let _ = stderr_thread.join();
                bail!("job cancelled");
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

        let parser = parser_thread
            .join()
            .map_err(|_| anyhow::anyhow!("pi stdout parser panicked"))??;
        let stderr = stderr_thread.join().unwrap_or_default();
        if !status.success() {
            let msg = stderr.trim();
            if msg.is_empty() {
                bail!("pi exited with {status}");
            }
            bail!("pi exited with {status}: {msg}");
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
            text,
            output,
            usage: parser.usage,
        })
    }

    fn name(&self) -> &str {
        "pi"
    }
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
                text: "{}".to_string(),
                output: Some(
                    json!({ "status": "no-op", "summary": "fake runner: no repair needed" }),
                ),
                usage: Usage::default(),
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
        emit_runner_event(&events, RunnerEvent::finished(None, Some(0)));
        Ok(ResultData {
            text: "{}".to_string(),
            output: Some(json!({
                "slice_id": handoff.slice.id,
                "status": "complete",
                "summary": "fake runner completed deterministic slice implementation",
                "commit_sha": sha,
                "changed_files": [
                    format!("{}.txt", handoff.slice.id),
                    format!(".khazad-fake/{}.txt", handoff.slice.id)
                ],
                "tests_run": handoff.slice.verify
            })),
            usage: Usage::default(),
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

#[derive(Debug, Default)]
struct PiParser {
    stream_text: BTreeMap<usize, String>,
    complete_text: BTreeMap<usize, String>,
    final_assistant: Option<Value>,
    usage: Usage,
}

impl PiParser {
    fn parse(
        &mut self,
        stdout: impl Read,
        events: Option<RunnerEventSink>,
        pid: Option<u32>,
    ) -> Result<()> {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            emit_runner_event(&events, RunnerEvent::stdout(pid));
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            self.handle(&event);
        }
        Ok(())
    }

    fn handle(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_update") => {
                self.remember_assistant(event.get("message"));
                self.handle_assistant_event(event.get("assistantMessageEvent"));
            }
            Some("message_end") | Some("turn_end") => self.remember_assistant(event.get("message")),
            Some("agent_end") => self.remember_last_assistant(event.get("messages")),
            _ => {}
        }
    }

    fn remember_assistant(&mut self, raw: Option<&Value>) {
        let Some(msg) = raw else { return };
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        self.final_assistant = Some(msg.clone());
        if let Some(usage) = msg.get("usage") {
            self.usage = usage_from_value(usage);
        }
    }

    fn remember_last_assistant(&mut self, raw: Option<&Value>) {
        let Some(messages) = raw.and_then(Value::as_array) else {
            return;
        };
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(Value::as_str) == Some("assistant") {
                self.remember_assistant(Some(msg));
                return;
            }
        }
    }

    fn handle_assistant_event(&mut self, raw: Option<&Value>) {
        let Some(event) = raw else { return };
        let idx = event
            .get("contentIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        match event.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.stream_text.entry(idx).or_default().push_str(delta);
                }
            }
            Some("text_complete") => {
                if let Some(text) = event.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    self.complete_text.insert(idx, text.to_string());
                }
            }
            _ => {}
        }
    }

    fn final_text(&self) -> String {
        if let Some(msg) = &self.final_assistant
            && let Some(content) = msg.get("content").and_then(Value::as_array)
        {
            let parts: Vec<_> = content
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
        if !self.complete_text.is_empty() {
            return join_indexed(&self.complete_text);
        }
        join_indexed(&self.stream_text)
    }
}

fn join_indexed(parts: &BTreeMap<usize, String>) -> String {
    let mut out = String::new();
    for value in parts.values() {
        out.push_str(value);
    }
    out
}

fn usage_from_value(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        output_tokens: value
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::{RunnerSpec, extract_json_object};

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
