use crate::agent::{
    AGENT_AUTH_REQUIRED_FAILURE_KIND, RunnerEvent, RunnerEventSink, RunnerLaunchFailure,
    RunnerMetadata, RunnerTranscript, Usage,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};

pub const SUPPORTED_PI_CONTRACT_VERSION: u64 = 1;
pub const PI_JSON_MODE_FLAGS: &[&str] = &["--mode", "json", "--no-session"];
pub const KNOWN_EVENT_TYPES: &[&str] = &[
    "session",
    "agent_start",
    "turn_start",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "turn_end",
    "agent_end",
    "queue_update",
    "compaction_start",
    "compaction_end",
    "session_info_changed",
    "thinking_level_changed",
    "auto_retry_start",
    "auto_retry_end",
];
const KNOWN_ASSISTANT_EVENT_TYPES: &[&str] = &[
    "text_start",
    "text_delta",
    "text_end",
    "thinking_start",
    "thinking_delta",
    "thinking_end",
    "toolcall_start",
    "toolcall_delta",
    "toolcall_end",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiContractObservation {
    pub binary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    pub supported_contract_version: u64,
    pub event_vocabulary: Vec<String>,
    pub launch_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<PiContractWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiContractWarning {
    pub kind: String,
    pub message: String,
}

impl PiContractWarning {
    fn unknown_event(event_type: &str) -> Self {
        Self {
            kind: "pi_contract_unknown_event".to_string(),
            message: format!(
                "Pi emitted unknown event type {event_type:?}; Khazad-Doom ignored it and continued."
            ),
        }
    }

    fn future_version(observed: u64) -> Self {
        Self {
            kind: "pi_contract_future_version".to_string(),
            message: format!(
                "Pi event contract version {observed} is newer than supported version {SUPPORTED_PI_CONTRACT_VERSION}; Khazad-Doom will tolerate unknown fields/events."
            ),
        }
    }
}

pub fn launch_args(extra_args: &[String]) -> Vec<String> {
    let mut args = extra_args.to_vec();
    args.extend(PI_JSON_MODE_FLAGS.iter().map(|arg| arg.to_string()));
    args
}

pub fn observation(binary: &str, extra_args: &[String]) -> PiContractObservation {
    PiContractObservation {
        binary: if binary.trim().is_empty() {
            "pi".to_string()
        } else {
            binary.to_string()
        },
        version: String::new(),
        supported_contract_version: SUPPORTED_PI_CONTRACT_VERSION,
        event_vocabulary: KNOWN_EVENT_TYPES
            .iter()
            .map(|value| value.to_string())
            .collect(),
        launch_flags: launch_args(extra_args),
        warnings: Vec::new(),
    }
}

const TOOL_EXECUTION_START_EVENT_TYPE: &str = "tool_execution_start";
const TOOL_EXECUTION_UPDATE_EVENT_TYPE: &str = "tool_execution_update";
pub(crate) const TOOL_EXECUTION_END_EVENT_TYPE: &str = "tool_execution_end";
const ACTIVITY_DELTA_FLUSH_CHARS: usize = 240;
const ACTIVITY_SNIPPET_CHARS: usize = 200;
const TOOL_DETAIL_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSemanticProgress {
    pub summary: String,
}

pub fn semantic_progress_from_stdout_line(line: &str) -> Option<PiSemanticProgress> {
    let event = serde_json::from_str::<Value>(line).ok()?;
    semantic_progress_from_event(&event)
}

fn semantic_progress_from_event(event: &Value) -> Option<PiSemanticProgress> {
    let action = match event.get("type").and_then(Value::as_str)? {
        TOOL_EXECUTION_START_EVENT_TYPE => "started",
        TOOL_EXECUTION_UPDATE_EVENT_TYPE => "running",
        TOOL_EXECUTION_END_EVENT_TYPE => "finished",
        _ => return None,
    };
    Some(PiSemanticProgress {
        summary: format!("tool {} {action}", tool_label(event)),
    })
}

#[derive(Debug, Default)]
pub struct PiActivityFormatter {
    text_delta: DeltaDisplayBuffer,
    reasoning_delta: DeltaDisplayBuffer,
    toolcall_delta: DeltaDisplayBuffer,
    warned_unknown_event: bool,
    warned_future_version: bool,
}

impl PiActivityFormatter {
    pub fn render_line(&mut self, line: &str) -> Vec<String> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return vec!["[pi] ignored non-json stdout line".to_string()];
        };
        self.render_event(&event)
    }

    pub fn flush(&mut self) -> Vec<String> {
        self.flush_compacted()
    }

    fn render_event(&mut self, event: &Value) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(line) = self.future_version_line(event) {
            lines.push(line);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("session") => {
                lines.extend(self.flush_compacted());
                let id = event.get("id").and_then(Value::as_str).unwrap_or("unknown");
                lines.push(format!("[pi] session {id} started"));
            }
            Some("agent_start") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] agent started".to_string());
            }
            Some("turn_start" | "message_start" | "message_end" | "turn_end") => {
                lines.extend(self.flush_compacted());
            }
            Some("message_update") => {
                lines.extend(self.render_assistant_activity(event.get("assistantMessageEvent")));
            }
            Some(TOOL_EXECUTION_START_EVENT_TYPE) | Some(TOOL_EXECUTION_UPDATE_EVENT_TYPE) => {
                lines.extend(self.flush_compacted());
                lines.push(render_tool_activity(event, ToolDisplayState::Running));
            }
            Some(TOOL_EXECUTION_END_EVENT_TYPE) => {
                lines.extend(self.flush_compacted());
                lines.push(render_tool_activity(event, ToolDisplayState::Finished));
            }
            Some("agent_end") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] agent finished".to_string());
            }
            Some("queue_update") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] queue updated".to_string());
            }
            Some("compaction_start") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] compaction started".to_string());
            }
            Some("compaction_end") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] compaction ended".to_string());
            }
            Some("session_info_changed") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] session info changed".to_string());
            }
            Some("thinking_level_changed") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] thinking level changed".to_string());
            }
            Some("auto_retry_start") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] auto retry started".to_string());
            }
            Some("auto_retry_end") => {
                lines.extend(self.flush_compacted());
                lines.push("[pi] auto retry ended".to_string());
            }
            Some(event_type) => {
                lines.extend(self.flush_compacted());
                if !self.warned_unknown_event {
                    self.warned_unknown_event = true;
                    lines.push(format!("[pi] unknown event ignored: {event_type}"));
                }
            }
            None => {}
        }
        lines
    }

    fn render_assistant_activity(&mut self, raw: Option<&Value>) -> Vec<String> {
        let Some(event) = raw else { return Vec::new() };
        match event.get("type").and_then(Value::as_str) {
            Some("thinking_start") => self.begin_delta(DeltaKind::Reasoning),
            Some("thinking_delta") => self.record_delta(
                event.get("delta").and_then(Value::as_str),
                DeltaKind::Reasoning,
            ),
            Some("thinking_end") => self.finish_delta(
                DeltaKind::Reasoning,
                event.get("content").and_then(Value::as_str),
            ),
            Some("text_start") => self.begin_delta(DeltaKind::Text),
            Some("text_delta") => {
                self.record_delta(event.get("delta").and_then(Value::as_str), DeltaKind::Text)
            }
            Some("text_end") => self.finish_delta(
                DeltaKind::Text,
                event.get("content").and_then(Value::as_str),
            ),
            Some("toolcall_start") => self.begin_delta(DeltaKind::ToolCall),
            Some("toolcall_delta") => self.record_delta(
                event.get("delta").and_then(Value::as_str),
                DeltaKind::ToolCall,
            ),
            Some("toolcall_end") => self.finish_delta(
                DeltaKind::ToolCall,
                event.get("content").and_then(Value::as_str),
            ),
            Some(event_type) => {
                let mut lines = self.flush_compacted();
                if !KNOWN_ASSISTANT_EVENT_TYPES.contains(&event_type) && !self.warned_unknown_event
                {
                    self.warned_unknown_event = true;
                    lines.push(format!("[pi] unknown event ignored: {event_type}"));
                }
                lines
            }
            None => Vec::new(),
        }
    }

    fn begin_delta(&mut self, kind: DeltaKind) -> Vec<String> {
        let lines = self.flush_compacted();
        self.buffer_mut(kind).reset();
        lines
    }

    fn record_delta(&mut self, delta: Option<&str>, kind: DeltaKind) -> Vec<String> {
        let mut lines = self.flush_except(kind);
        let Some(delta) = delta.filter(|value| !value.is_empty()) else {
            return lines;
        };
        let buffer = self.buffer_mut(kind);
        buffer.text.push_str(delta);
        if buffer.text.chars().count() >= ACTIVITY_DELTA_FLUSH_CHARS {
            lines.extend(self.take_delta_lines(kind));
        }
        lines
    }

    fn finish_delta(&mut self, kind: DeltaKind, content: Option<&str>) -> Vec<String> {
        let had_buffer = !self.buffer(kind).text.is_empty();
        let had_emitted = self.buffer(kind).emitted;
        let mut lines = self.take_delta_lines(kind);
        if !had_buffer
            && !had_emitted
            && let Some(content) = content.filter(|value| !value.is_empty())
            && let Some(line) = format_delta_line(kind, content)
        {
            lines.push(line);
        }
        self.buffer_mut(kind).reset();
        lines
    }

    fn flush_compacted(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.take_delta_lines(DeltaKind::Reasoning));
        lines.extend(self.take_delta_lines(DeltaKind::Text));
        lines.extend(self.take_delta_lines(DeltaKind::ToolCall));
        lines
    }

    fn flush_except(&mut self, keep: DeltaKind) -> Vec<String> {
        let mut lines = Vec::new();
        for kind in [DeltaKind::Reasoning, DeltaKind::Text, DeltaKind::ToolCall] {
            if kind != keep {
                lines.extend(self.take_delta_lines(kind));
            }
        }
        lines
    }

    fn take_delta_lines(&mut self, kind: DeltaKind) -> Vec<String> {
        let text = std::mem::take(&mut self.buffer_mut(kind).text);
        if text.is_empty() {
            return Vec::new();
        }
        self.buffer_mut(kind).emitted = true;
        format_delta_line(kind, &text).into_iter().collect()
    }

    fn buffer(&self, kind: DeltaKind) -> &DeltaDisplayBuffer {
        match kind {
            DeltaKind::Text => &self.text_delta,
            DeltaKind::Reasoning => &self.reasoning_delta,
            DeltaKind::ToolCall => &self.toolcall_delta,
        }
    }

    fn buffer_mut(&mut self, kind: DeltaKind) -> &mut DeltaDisplayBuffer {
        match kind {
            DeltaKind::Text => &mut self.text_delta,
            DeltaKind::Reasoning => &mut self.reasoning_delta,
            DeltaKind::ToolCall => &mut self.toolcall_delta,
        }
    }

    fn future_version_line(&mut self, event: &Value) -> Option<String> {
        let observed = event
            .get("contract_version")
            .or_else(|| event.get("contractVersion"))
            .and_then(Value::as_u64)?;
        if observed <= SUPPORTED_PI_CONTRACT_VERSION || self.warned_future_version {
            return None;
        }
        self.warned_future_version = true;
        Some(format!(
            "[pi] future event contract v{observed} tolerated by display parser"
        ))
    }
}

#[derive(Debug, Default)]
struct DeltaDisplayBuffer {
    text: String,
    emitted: bool,
}

impl DeltaDisplayBuffer {
    fn reset(&mut self) {
        self.text.clear();
        self.emitted = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaKind {
    Text,
    Reasoning,
    ToolCall,
}

#[derive(Debug, Clone, Copy)]
enum ToolDisplayState {
    Running,
    Finished,
}

fn format_delta_line(kind: DeltaKind, text: &str) -> Option<String> {
    let display = match kind {
        DeltaKind::ToolCall => snippet(&redact_sensitive_text(text)),
        _ => snippet(text),
    };
    if display.is_empty() {
        return None;
    }
    let label = match kind {
        DeltaKind::Text => "assistant",
        DeltaKind::Reasoning => "reasoning/progress",
        DeltaKind::ToolCall => "tool request",
    };
    Some(format!("[pi] {label}: {display}"))
}

fn render_tool_activity(event: &Value, state: ToolDisplayState) -> String {
    let detail = tool_detail(event)
        .map(|detail| format!(" {detail}"))
        .unwrap_or_default();
    let outcome = match state {
        ToolDisplayState::Running => "running".to_string(),
        ToolDisplayState::Finished => tool_finish_outcome(event).to_string(),
    };
    format!(
        "[pi] tool {}{detail} {outcome}{}",
        tool_label(event),
        duration_suffix(event)
    )
}

fn tool_finish_outcome(event: &Value) -> &'static str {
    if tool_failed(event) {
        "failed"
    } else if tool_succeeded(event) {
        "ok"
    } else {
        "finished"
    }
}

fn tool_failed(event: &Value) -> bool {
    event.get("error").is_some()
        || event.pointer("/result/error").is_some()
        || numeric_field(event.pointer("/result/exit_code")).is_some_and(|code| code != 0.0)
        || status_field(event.pointer("/result/status"))
            .is_some_and(|status| matches!(status, "failed" | "failure" | "error"))
}

fn tool_succeeded(event: &Value) -> bool {
    numeric_field(event.pointer("/result/exit_code")).is_some_and(|code| code == 0.0)
        || event
            .pointer("/result/success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || status_field(event.pointer("/result/status"))
            .is_some_and(|status| matches!(status, "ok" | "success" | "succeeded" | "completed"))
}

fn status_field(value: Option<&Value>) -> Option<&str> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn duration_suffix(event: &Value) -> String {
    let duration_ms = ["durationMs", "duration_ms", "elapsedMs", "elapsed_ms"]
        .iter()
        .find_map(|key| numeric_field(event.get(*key)));
    let Some(duration_ms) = duration_ms else {
        return String::new();
    };
    if duration_ms >= 1000.0 {
        format!(" in {:.1}s", duration_ms / 1000.0)
    } else {
        format!(" in {}ms", duration_ms.round() as u64)
    }
}

fn numeric_field(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

fn tool_detail(event: &Value) -> Option<String> {
    event
        .get("args")
        .and_then(tool_args_summary)
        .or_else(|| event.get("input").and_then(tool_args_summary))
        .or_else(|| text_detail(event, "command"))
        .or_else(|| text_detail(event, "path"))
        .or_else(|| text_detail(event, "file"))
}

fn tool_args_summary(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => [
            "command",
            "cmd",
            "path",
            "file",
            "file_path",
            "filename",
            "cwd",
        ]
        .iter()
        .find_map(|key| {
            map.get(*key)
                .and_then(|value| display_arg_value(key, value))
        }),
        Value::String(text) => Some(format!("args={}", display_text_value("args", text))),
        _ => None,
    }
}

fn text_detail(event: &Value, key: &str) -> Option<String> {
    event
        .get(key)
        .and_then(|value| display_arg_value(key, value))
}

fn display_arg_value(key: &str, value: &Value) -> Option<String> {
    let rendered = match value {
        Value::String(text) => display_text_value(key, text),
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => return None,
    };
    Some(format!("{key}={rendered}"))
}

fn display_text_value(key: &str, text: &str) -> String {
    if sensitive_key(key) || sensitive_text(text) {
        return "[redacted sensitive value]".to_string();
    }
    truncate_display(&snippet(text), TOOL_DETAIL_CHARS)
}

fn tool_label(event: &Value) -> String {
    let raw = event
        .get("toolName")
        .or_else(|| event.get("tool_name"))
        .or_else(|| event.get("toolCallId"))
        .or_else(|| event.get("tool_call_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown");
    truncate_display(raw, 48)
}

fn sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
        "api_key",
        "apikey",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn sensitive_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "token=",
        "secret=",
        "password=",
        "api_key=",
        "apikey=",
        "authorization:",
        "bearer ",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn redact_sensitive_text(text: &str) -> String {
    if sensitive_text(text) {
        "[redacted sensitive value]".to_string()
    } else {
        text.to_string()
    }
}

fn truncate_display(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn snippet(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (index, ch) in compact.chars().enumerate() {
        if index >= ACTIVITY_SNIPPET_CHARS {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub fn classify_launch_failure(
    transcript: &RunnerTranscript,
    metadata: &RunnerMetadata,
) -> Option<RunnerLaunchFailure> {
    if !transcript.assistant_tail.trim().is_empty() {
        return None;
    }
    if !looks_like_pi_auth_failure(&transcript.stderr_tail) {
        return None;
    }
    let provider = if metadata.provider.trim().is_empty() {
        provider_from_auth_failure(&transcript.stderr_tail)
            .unwrap_or_else(|| "configured provider".to_string())
    } else {
        metadata.provider.trim().to_string()
    };
    let fix_commands = metadata.auth_fix_commands();
    Some(RunnerLaunchFailure {
        failure_kind: AGENT_AUTH_REQUIRED_FAILURE_KIND.to_string(),
        summary: format!(
            "Pi is not authenticated for provider {provider}; run `{}` or update ~/.khazad-doom/agents.toml to a configured provider/model.",
            fix_commands
                .first()
                .cloned()
                .unwrap_or_else(|| "pi /login".to_string())
        ),
        retryable: false,
        operator_action_required: true,
        fix_commands,
    })
}

fn looks_like_pi_auth_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no api key found for") && lower.contains("/login")
}

#[cfg(test)]
pub(crate) fn auth_failure_stderr_fixture(provider: &str) -> String {
    format!(
        "No API key found for {provider}.\nUse /login to log into a provider via OAuth or API key.\n"
    )
}

fn provider_from_auth_failure(stderr: &str) -> Option<String> {
    let lower = stderr.to_ascii_lowercase();
    let marker = "no api key found for";
    let start = lower.find(marker)? + marker.len();
    let provider = lower[start..]
        .trim_start()
        .split(|ch: char| ch == '.' || ch.is_whitespace())
        .next()?
        .trim();
    (!provider.is_empty()).then(|| provider.to_string())
}

#[derive(Debug, Default)]
pub struct PiParser {
    stream_text: BTreeMap<usize, String>,
    complete_text: BTreeMap<usize, String>,
    final_assistant: Option<Value>,
    usage: Usage,
    raw_tail: String,
    warnings: Vec<PiContractWarning>,
    warned_unknown_event: bool,
    warned_future_version: bool,
}

impl PiParser {
    pub fn parse(
        &mut self,
        stdout: impl Read,
        events: Option<RunnerEventSink>,
        pid: Option<u32>,
    ) -> Result<()> {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line?;
            if let Some(sink) = &events {
                sink(RunnerEvent::stdout(pid, line.clone()));
            }
            self.remember_raw_line(&line);
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

    pub fn final_text(&self) -> String {
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

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn warnings(&self) -> &[PiContractWarning] {
        &self.warnings
    }

    pub fn transcript(&self, stderr: &str) -> RunnerTranscript {
        RunnerTranscript {
            stdout_tail: self.raw_tail.clone(),
            stderr_tail: tail_string(stderr, 12_000),
            assistant_tail: tail_string(&self.final_text(), 12_000),
        }
    }

    fn remember_raw_line(&mut self, line: &str) {
        append_bounded(&mut self.raw_tail, line, 12_000);
        append_bounded(&mut self.raw_tail, "\n", 12_000);
    }

    fn handle(&mut self, event: &Value) {
        self.observe_version(event);
        match event.get("type").and_then(Value::as_str) {
            Some("message_update") => {
                self.remember_assistant(event.get("message"));
                self.handle_assistant_event(event.get("assistantMessageEvent"));
            }
            Some("message_end") | Some("turn_end") => self.remember_assistant(event.get("message")),
            Some("agent_end") => self.remember_last_assistant(event.get("messages")),
            Some(event_type) if KNOWN_EVENT_TYPES.contains(&event_type) => {}
            Some(event_type) => self.warn_unknown_event(event_type),
            None => {}
        }
    }

    fn observe_version(&mut self, event: &Value) {
        let observed = event
            .get("contract_version")
            .or_else(|| event.get("contractVersion"))
            .and_then(Value::as_u64);
        if let Some(version) = observed
            && version > SUPPORTED_PI_CONTRACT_VERSION
            && !self.warned_future_version
        {
            self.warned_future_version = true;
            self.warnings
                .push(PiContractWarning::future_version(version));
        }
    }

    fn warn_unknown_event(&mut self, event_type: &str) {
        if self.warned_unknown_event {
            return;
        }
        self.warned_unknown_event = true;
        self.warnings
            .push(PiContractWarning::unknown_event(event_type));
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
            Some("text_end") => {
                if let Some(text) = event.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    self.complete_text.insert(idx, text.to_string());
                }
            }
            Some(event_type) if !KNOWN_ASSISTANT_EVENT_TYPES.contains(&event_type) => {
                self.warn_unknown_event(event_type);
            }
            _ => {}
        }
    }
}

fn join_indexed(parts: &BTreeMap<usize, String>) -> String {
    let mut out = String::new();
    for value in parts.values() {
        out.push_str(value);
    }
    out
}

fn append_bounded(target: &mut String, text: &str, max_bytes: usize) {
    target.push_str(text);
    if target.len() > max_bytes {
        let mut remove = target.len() - max_bytes;
        while !target.is_char_boundary(remove) && remove < target.len() {
            remove += 1;
        }
        target.drain(..remove);
    }
}

fn tail_string(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) && start < text.len() {
        start += 1;
    }
    text[start..].to_string()
}

fn usage_from_value(value: &Value) -> Usage {
    Usage {
        input_tokens: value.get("input").and_then(Value::as_u64).unwrap_or(0) as usize,
        output_tokens: value.get("output").and_then(Value::as_u64).unwrap_or(0) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::RunnerMetadata;
    use std::io::Cursor;

    #[test]
    fn parses_current_pi_event_shapes_and_usage() {
        let stdout = r#"{"type":"session","version":3,"id":"smoke"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"type":"message_update","assistantMessageEvent":{"type":"thinking_start","contentIndex":0}}
{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"working"}}
{"type":"message_update","assistantMessageEvent":{"type":"thinking_end","contentIndex":0,"content":""}}
{"type":"message_update","assistantMessageEvent":{"type":"toolcall_start","contentIndex":1}}
{"type":"message_update","assistantMessageEvent":{"type":"toolcall_delta","contentIndex":1,"delta":"{}"}}
{"type":"message_update","assistantMessageEvent":{"type":"toolcall_end","contentIndex":1}}
{"type":"tool_execution_start","toolCallId":"call_1"}
{"type":"tool_execution_update","toolCallId":"call_1"}
{"type":"tool_execution_end","toolCallId":"call_1"}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":2}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":2,"delta":"{\"status\":"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"\"ok\"}"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":1,"content":"{\"status\":\"ok\"}"}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"status\":\"ok\"}"}],"usage":{"input":3,"output":5}}}
{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"{\"status\":\"ok\"}"}],"usage":{"input":3,"output":5}}}
{"type":"queue_update","steering":[],"followUp":[]}
{"type":"compaction_start","reason":"threshold"}
{"type":"compaction_end","reason":"threshold","aborted":true,"willRetry":false}
{"type":"session_info_changed","name":"smoke"}
{"type":"thinking_level_changed","level":"minimal"}
{"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"temporary"}
{"type":"auto_retry_end","success":true,"attempt":1}
{"type":"agent_end","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"text","text":"{\"status\":\"ok\"}"}],"usage":{"input":3,"output":5}}]}
"#;
        let mut parser = PiParser::default();
        parser.parse(Cursor::new(stdout), None, None).unwrap();
        assert_eq!(parser.final_text(), "{\"status\":\"ok\"}");
        assert_eq!(parser.usage().input_tokens, 3);
        assert_eq!(parser.usage().output_tokens, 5);
        assert!(parser.warnings().is_empty());
    }

    #[test]
    fn unknown_future_events_warn_once_and_do_not_block_text() {
        let stdout = r#"{"type":"future_event","contract_version":2,"payload":1}
{"type":"another_future_event","contract_version":2,"payload":2}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"done"}}
"#;
        let mut parser = PiParser::default();
        parser.parse(Cursor::new(stdout), None, None).unwrap();
        assert_eq!(parser.final_text(), "done");
        assert_eq!(parser.warnings().len(), 2);
        assert_eq!(parser.warnings()[0].kind, "pi_contract_future_version");
        assert_eq!(parser.warnings()[1].kind, "pi_contract_unknown_event");
    }

    #[test]
    fn worker_activity_painter_renders_recorded_fixture_tolerantly() {
        let fixture = include_str!("../tests/fixtures/rpl_worker_activity.ndjson");
        let mut formatter = PiActivityFormatter::default();
        let mut rendered = Vec::new();
        for line in fixture.lines() {
            rendered.extend(formatter.render_line(line));
        }
        rendered.extend(formatter.flush());
        let joined = rendered.join("\n");

        assert!(joined.contains("[pi] session 019f3b71-268e-74fa-ae5a-c0e225a75f04 started"));
        assert!(joined.contains("[pi] tool read path="));
        assert!(joined.contains("[pi] tool read finished"));
        assert!(joined.contains("[pi] assistant: hello world"));
        assert!(joined.contains("[pi] reasoning/progress: looking"));
        assert!(joined.contains("[pi] future event contract v2 tolerated"));
        assert!(joined.contains("[pi] unknown event ignored: future_activity_event"));
        assert!(joined.contains("[pi] agent finished"));
        assert!(!joined.contains("turn started"));
        assert!(!joined.contains("turn ended"));
        assert!(!joined.contains("message started"));
        assert!(!joined.contains("message ended"));
        assert!(!joined.contains("chunks"));
    }

    #[test]
    fn worker_pane_golden_fixture_covers_semantic_display() {
        let fixture = include_str!("../tests/fixtures/worker_pane_semantic.ndjson");
        let expected = include_str!("../tests/fixtures/worker_pane_semantic.golden.txt").trim_end();
        let mut formatter = PiActivityFormatter::default();
        let mut rendered = Vec::new();
        for line in fixture.lines() {
            rendered.extend(formatter.render_line(line));
        }
        rendered.extend(formatter.flush());

        assert_eq!(rendered.join("\n"), expected);
    }

    #[test]
    fn semantic_progress_from_wrapper_stdout_fixture_tracks_tool_events() {
        let fixture =
            include_str!("../tests/fixtures/projection_information_wrapper_stdout.ndjson");
        let summaries = fixture
            .lines()
            .filter_map(semantic_progress_from_stdout_line)
            .map(|progress| progress.summary)
            .collect::<Vec<_>>();

        assert_eq!(
            summaries,
            vec![
                "tool bash started".to_string(),
                "tool bash running".to_string(),
                "tool bash finished".to_string(),
            ]
        );
    }

    #[test]
    fn worker_activity_painter_coalesces_high_volume_token_deltas() {
        let mut formatter = PiActivityFormatter::default();
        let mut rendered = Vec::new();
        rendered.extend(formatter.render_line(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}"#,
        ));
        for _ in 0..250 {
            rendered.extend(formatter.render_line(
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"x"}}"#,
            ));
        }
        rendered.extend(formatter.flush());
        let assistant_lines: Vec<_> = rendered
            .iter()
            .filter(|line| line.starts_with("[pi] assistant:"))
            .collect();

        assert_eq!(assistant_lines.len(), 2);
        assert!(assistant_lines[0].ends_with('…'));
        assert_eq!(assistant_lines[1], "[pi] assistant: xxxxxxxxxx");
        assert!(rendered.iter().all(|line| !line.contains("chunks")));
        assert!(rendered.len() < 10);
    }

    #[test]
    fn classifies_auth_launch_failures_inside_contract_boundary() {
        let metadata = RunnerMetadata {
            provider: "openai".to_string(),
            ..RunnerMetadata::default()
        };
        let transcript = RunnerTranscript {
            stderr_tail: "No API key found for openai.\nUse /login to log into a provider via OAuth or API key.\n".to_string(),
            ..RunnerTranscript::default()
        };
        let failure = classify_launch_failure(&transcript, &metadata).unwrap();
        assert_eq!(failure.failure_kind, AGENT_AUTH_REQUIRED_FAILURE_KIND);
        assert!(!failure.retryable);
        assert!(failure.summary.contains("openai"));
    }
}
