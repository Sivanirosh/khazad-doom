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
];
const KNOWN_ASSISTANT_EVENT_TYPES: &[&str] = &[
    "thinking_start",
    "thinking_delta",
    "thinking_end",
    "toolcall_start",
    "toolcall_delta",
    "toolcall_end",
    "text_start",
    "text_delta",
    "text_end",
    "text_complete",
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
            "Pi is not authenticated for provider {provider}; run `{}` or update .workflow/agents.toml to a configured provider/model.",
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
            Some("text_complete") => {
                if let Some(text) = event.get("text").and_then(Value::as_str)
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
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"{\"status\":\"ok\"}"}],"usage":{"inputTokens":3,"outputTokens":5}}}
{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"{\"status\":\"ok\"}"}],"usage":{"inputTokens":3,"outputTokens":5}}}
{"type":"agent_end","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"text","text":"{\"status\":\"ok\"}"}],"usage":{"inputTokens":3,"outputTokens":5}}]}
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
{"type":"message_update","assistantMessageEvent":{"type":"text_complete","contentIndex":0,"text":"done"}}
"#;
        let mut parser = PiParser::default();
        parser.parse(Cursor::new(stdout), None, None).unwrap();
        assert_eq!(parser.final_text(), "done");
        assert_eq!(parser.warnings().len(), 2);
        assert_eq!(parser.warnings()[0].kind, "pi_contract_future_version");
        assert_eq!(parser.warnings()[1].kind, "pi_contract_unknown_event");
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
