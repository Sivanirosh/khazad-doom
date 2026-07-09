use super::events::{IMPLEMENTATION_SUMMARY, ImplementationSummaryPayload, RUN_STARTED};
use crate::domain::{
    FrontierBudgetState, GateCommandResult, GateResult, MissionEnvelope, RepairResult, RunDetails,
    RunEconomics, RunIncident, RunProgress, RunStatus, SliceRun, SliceStatus, StatusFeed,
    StatusFeedBlock, StatusFeedLine, StatusFeedRole, TerminalReason, WorkerAttemptProgress,
    WorkflowExitStates,
};
use chrono::{DateTime, Utc};
use std::time::Duration;

const FEED_VERSION: u64 = 1;
const WORKER_ITEMS: usize = 6;
const LINE_WIDTH: usize = 96;

const GATE_PANE_LINE_WIDTH: usize = 180;
const GATE_PANE_TAIL_LINES: usize = 6;

pub(crate) struct GatePaneProjection {
    pub active: bool,
    pub feed: StatusFeed,
}

pub(crate) fn project_gate_pane(details: &RunDetails, now: DateTime<Utc>) -> GatePaneProjection {
    if let Some(progress) = gate_pane_active_progress(details) {
        return GatePaneProjection {
            active: true,
            feed: active_gate_pane_feed(details, progress, now),
        };
    }
    GatePaneProjection {
        active: false,
        feed: idle_gate_pane_feed(details),
    }
}

fn active_gate_pane_feed(
    details: &RunDetails,
    progress: &RunProgress,
    now: DateTime<Utc>,
) -> StatusFeed {
    let terminal_reason = gate_pane_terminal_reason(details);
    let operator_commands = gate_pane_operator_commands(details, terminal_reason.as_ref());
    let mut blocks = vec![gate_pane_block(
        "Run",
        format!(
            "{} {} • {}",
            gate_pane_status_icon(details.run.status),
            details.run.status,
            gate_pane_short_run_id(&details.run.id)
        ),
        vec![gate_pane_line(
            "Source: daemon status feed / shell progress; gate results and artifacts remain authoritative",
            StatusFeedRole::Dim,
        )],
    )];

    let mut activity_lines = vec![
        gate_pane_line(
            format!(
                "command: {}",
                gate_pane_truncate(&progress.command, GATE_PANE_LINE_WIDTH)
            ),
            StatusFeedRole::Info,
        ),
        gate_pane_line("state: running", StatusFeedRole::Info),
    ];
    if !progress.message.trim().is_empty() {
        activity_lines.push(gate_pane_line(
            format!(
                "message: {}",
                gate_pane_truncate(&progress.message, GATE_PANE_LINE_WIDTH)
            ),
            StatusFeedRole::Info,
        ));
    }
    if !progress.slice_id.trim().is_empty() {
        activity_lines.push(gate_pane_line(
            format!("slice: {}", progress.slice_id),
            StatusFeedRole::Dim,
        ));
    }
    if progress.attempt > 0 {
        activity_lines.push(gate_pane_line(
            format!("attempt: {}", progress.attempt),
            StatusFeedRole::Dim,
        ));
    }
    activity_lines.push(gate_pane_line(
        format!("updated {} ago", gate_pane_since(progress.updated_at, now)),
        StatusFeedRole::Dim,
    ));
    if let Some(worker) = &progress.worker {
        activity_lines.push(gate_pane_line(
            format!(
                "supervisor: {}",
                match worker.process_observed_at {
                    Some(observed_at) => format!(
                        "alive, observed child {} ago",
                        gate_pane_since(observed_at, now)
                    ),
                    None => "starting, no child observation yet".to_string(),
                }
            ),
            StatusFeedRole::Dim,
        ));
    }
    blocks.push(gate_pane_block(
        gate_pane_activity_label(&progress.phase),
        format!(
            "(running • elapsed {})",
            gate_pane_since(progress.phase_started_at, now)
        ),
        activity_lines,
    ));

    let tail = gate_pane_compact_tail(&progress.output_tail);
    if tail.is_empty() {
        blocks.push(gate_pane_block(
            "Tail",
            "",
            vec![gate_pane_line(
                "waiting for daemon-owned command output",
                StatusFeedRole::Dim,
            )],
        ));
    } else {
        blocks.push(gate_pane_block(
            "Tail",
            format!("(last {} compact lines)", tail.len()),
            tail.into_iter()
                .map(|text| gate_pane_line(text, StatusFeedRole::Dim))
                .collect(),
        ));
    }

    StatusFeed {
        feed_version: 1,
        summary_line: "Khazad-Doom gate/repair activity painter (read-only)".to_string(),
        terminal_reason,
        operator_commands,
        attention: Vec::new(),
        blocks,
    }
}

fn idle_gate_pane_feed(details: &RunDetails) -> StatusFeed {
    let summary = gate_pane_latest_implementation_summary(details);
    let latest_gate = summary
        .as_ref()
        .and_then(|summary| summary.integration_gate.clone())
        .or_else(|| gate_pane_gate_result_from_economics(details));
    let pre_repair_gate = summary
        .as_ref()
        .and_then(|summary| summary.pre_repair_integration_gate.clone());
    let repair = summary
        .as_ref()
        .and_then(|summary| summary.integration_repair.clone());
    let exit_states = summary
        .as_ref()
        .and_then(|summary| summary.exit_states.clone());
    let terminal_reason = gate_pane_terminal_reason(details);
    let operator_commands = gate_pane_operator_commands(details, terminal_reason.as_ref());

    let blocks = vec![
        gate_pane_gate_block(
            details,
            latest_gate.as_ref(),
            pre_repair_gate.as_ref(),
            summary.as_ref(),
        ),
        gate_pane_repair_block(details, latest_gate.as_ref(), repair.as_ref()),
        gate_pane_handoff_block(
            details,
            latest_gate.as_ref(),
            exit_states.as_ref(),
            terminal_reason.as_ref(),
        ),
        gate_pane_next_block(details, &operator_commands),
    ];

    StatusFeed {
        feed_version: 1,
        summary_line: "Khazad-Doom gate/repair status (idle)".to_string(),
        terminal_reason,
        operator_commands,
        attention: Vec::new(),
        blocks,
    }
}

fn gate_pane_gate_block(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    pre_repair_gate: Option<&GateResult>,
    summary: Option<&ImplementationSummaryPayload>,
) -> StatusFeedBlock {
    let mut lines = vec![gate_pane_line(
        format!(
            "Verification profile: {}",
            gate_pane_verification_profile(details, summary)
        ),
        StatusFeedRole::Info,
    )];
    match latest_gate {
        Some(gate) => lines.push(gate_pane_line(
            format!(
                "Latest gate: {}{}",
                gate_pane_display_or_dash(&gate.status),
                gate_pane_summary_suffix(&gate.summary)
            ),
            gate_pane_role_for_gate_status(&gate.status),
        )),
        None => lines.push(gate_pane_line(
            "Latest gate: not run yet",
            StatusFeedRole::Dim,
        )),
    }
    let last_failure = gate_pane_last_failure(pre_repair_gate, latest_gate);
    lines.push(gate_pane_line(
        format!(
            "Last failure: {}",
            last_failure.clone().unwrap_or_else(|| "none".to_string())
        ),
        if last_failure.is_some() {
            StatusFeedRole::Warning
        } else {
            StatusFeedRole::Dim
        },
    ));
    gate_pane_block(
        "Gate",
        format!(
            "({})",
            latest_gate
                .map(|gate| gate_pane_display_or_dash(&gate.status))
                .unwrap_or("not run")
        ),
        lines,
    )
}

fn gate_pane_repair_block(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    repair: Option<&RepairResult>,
) -> StatusFeedBlock {
    let policy = gate_pane_repair_policy(details);
    let (state, role) = gate_pane_repair_state(policy.as_str(), latest_gate, repair);
    let mut lines = vec![gate_pane_line(state, role)];
    if let Some(attempts) = gate_pane_repair_attempts(details, repair) {
        lines.push(gate_pane_line(attempts, StatusFeedRole::Dim));
    }
    gate_pane_block("Repair", format!("({policy})"), lines)
}

fn gate_pane_handoff_block(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    exit_states: Option<&WorkflowExitStates>,
    terminal_reason: Option<&TerminalReason>,
) -> StatusFeedBlock {
    let (meta, mut lines) = gate_pane_handoff_lines(details, latest_gate, exit_states);
    if let Some(reason) = terminal_reason {
        lines.push(gate_pane_line(
            format!(
                "Terminal reason: {}{}",
                gate_pane_display_or_dash(&reason.kind),
                gate_pane_summary_suffix(&reason.summary)
            ),
            if reason.operator_action_required {
                StatusFeedRole::Attention
            } else {
                StatusFeedRole::Warning
            },
        ));
    }
    lines.push(gate_pane_line(
        format!("Run: {}", details.run.status),
        StatusFeedRole::Dim,
    ));
    gate_pane_block("Handoff", format!("({meta})"), lines)
}

fn gate_pane_next_block(details: &RunDetails, operator_commands: &[String]) -> StatusFeedBlock {
    let mut commands = operator_commands.to_vec();
    match details.run.status {
        RunStatus::Completed => gate_pane_push_unique(
            &mut commands,
            format!("khazad-doom handoff --run {}", details.run.id),
        ),
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => {
            gate_pane_push_unique(
                &mut commands,
                format!("khazad-doom inspect --run {}", details.run.id),
            );
            gate_pane_push_unique(
                &mut commands,
                format!("khazad-doom resume --run {}", details.run.id),
            );
        }
        RunStatus::Pending | RunStatus::Running => {}
    }
    let lines = if commands.is_empty() {
        vec![gate_pane_line(
            "No operator gate/repair command is currently needed.",
            StatusFeedRole::Dim,
        )]
    } else {
        commands
            .into_iter()
            .map(|command| gate_pane_line(command, StatusFeedRole::Attention))
            .collect()
    };
    gate_pane_block("Next", "", lines)
}

fn gate_pane_active_progress(details: &RunDetails) -> Option<&RunProgress> {
    let progress = details.progress.as_ref()?;
    if gate_pane_terminal_status(details.run.status) || progress.command.trim().is_empty() {
        return None;
    }
    gate_pane_is_gate_or_repair_phase(&progress.phase).then_some(progress)
}

fn gate_pane_is_gate_or_repair_phase(phase: &str) -> bool {
    let normalized = phase.to_ascii_lowercase();
    normalized.contains("integration_gate") || normalized.contains("integration_repair")
}

fn gate_pane_activity_label(phase: &str) -> &'static str {
    if phase.to_ascii_lowercase().contains("repair") {
        "Repair"
    } else {
        "Integration Gate"
    }
}

fn gate_pane_compact_tail(output_tail: &str) -> Vec<String> {
    output_tail
        .trim_end()
        .lines()
        .rev()
        .take(GATE_PANE_TAIL_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| gate_pane_truncate(line, GATE_PANE_LINE_WIDTH))
        .collect()
}

fn gate_pane_terminal_reason(details: &RunDetails) -> Option<TerminalReason> {
    details
        .feed
        .as_ref()
        .and_then(|feed| feed.terminal_reason.clone())
        .or_else(|| details.primary_terminal_reason.clone())
}

fn gate_pane_operator_commands(
    details: &RunDetails,
    terminal_reason: Option<&TerminalReason>,
) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(feed) = &details.feed {
        for command in &feed.operator_commands {
            gate_pane_push_unique(&mut commands, command.clone());
        }
    }
    if let Some(reason) = terminal_reason {
        for command in &reason.operator_commands {
            gate_pane_push_unique(&mut commands, command.clone());
        }
    }
    commands
}

fn gate_pane_latest_implementation_summary(
    details: &RunDetails,
) -> Option<ImplementationSummaryPayload> {
    details
        .events
        .iter()
        .rev()
        .find(|event| event.typ == IMPLEMENTATION_SUMMARY)
        .map(|event| ImplementationSummaryPayload::from_value(&event.payload))
}

fn gate_pane_gate_result_from_economics(details: &RunDetails) -> Option<GateResult> {
    let economics = details.economics.as_ref()?;
    let commands = economics
        .command_executions
        .iter()
        .filter(|command| command.phase == "integration_gate")
        .map(|command| GateCommandResult {
            command: command.command.clone(),
            status: command.status.clone(),
            exit_code: command.exit_code,
            output: String::new(),
            cwd: command.cwd.clone(),
            dedupe_key: command.dedupe_key.clone(),
            duration_ms: command.duration_ms,
            cache_hit: command.cache_hit,
            skip_reason: command.skip_reason.clone(),
            failure_kind: String::new(),
        })
        .collect::<Vec<_>>();
    if commands.is_empty() {
        return None;
    }
    let status = if commands.iter().any(|command| command.status == "failed") {
        "failed"
    } else if commands.iter().all(|command| command.status == "skipped") {
        "skipped"
    } else if commands
        .iter()
        .all(|command| command.status == "passed" || command.status == "skipped")
    {
        "passed"
    } else {
        "unknown"
    };
    let summary = match status {
        "passed" => "integration gate passed",
        "failed" => "one or more integration gate commands failed",
        "skipped" => "integration gate commands skipped",
        _ => "integration gate status is unknown",
    };
    Some(GateResult {
        status: status.to_string(),
        summary: summary.to_string(),
        commands,
        findings: Vec::new(),
    })
}

fn gate_pane_verification_profile(
    details: &RunDetails,
    summary: Option<&ImplementationSummaryPayload>,
) -> String {
    if let Some(profile) = summary
        .map(|summary| summary.verify_profile.trim())
        .filter(|profile| !profile.is_empty())
    {
        return profile.to_string();
    }
    for event in details.events.iter().rev() {
        if event.typ != RUN_STARTED {
            continue;
        }
        let payload = super::events::RunStartedPayload::from_value(&event.payload);
        if !payload.verify_profile.trim().is_empty() {
            return payload.verify_profile;
        }
        let joined = payload
            .verify_profiles
            .iter()
            .map(|profile| profile.trim())
            .filter(|profile| !profile.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.trim().is_empty() {
            return joined;
        }
    }
    "unknown".to_string()
}

fn gate_pane_repair_policy(details: &RunDetails) -> String {
    details
        .economics
        .as_ref()
        .map(|economics| economics.repair_policy.trim())
        .filter(|policy| !policy.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn gate_pane_repair_attempts(
    details: &RunDetails,
    repair: Option<&RepairResult>,
) -> Option<String> {
    if let Some(economics) = &details.economics {
        return Some(format!(
            "Attempts: {}/{}",
            economics.repair_attempts, economics.repair_max_attempts
        ));
    }
    repair
        .filter(|repair| repair.attempts > 0)
        .map(|repair| format!("Attempts: {}", repair.attempts))
}

fn gate_pane_repair_state(
    policy: &str,
    latest_gate: Option<&GateResult>,
    repair: Option<&RepairResult>,
) -> (String, StatusFeedRole) {
    if let Some(repair) = repair {
        let mut text = format!("State: {}", gate_pane_display_or_dash(&repair.status));
        if !repair.summary.trim().is_empty() {
            text.push_str(&gate_pane_summary_suffix(&repair.summary));
        }
        if !repair.trigger.trim().is_empty() {
            text.push_str(&format!(" ({})", repair.trigger));
        }
        return (text, gate_pane_role_for_repair_status(&repair.status));
    }
    match latest_gate.map(|gate| gate.status.as_str()) {
        None => (
            "State: waiting for gate result".to_string(),
            StatusFeedRole::Dim,
        ),
        Some("passed") => (
            "State: not needed; latest gate passed".to_string(),
            StatusFeedRole::Success,
        ),
        Some("failed") if matches!(policy, "auto" | "always") => (
            "State: repairable: daemon policy can run integration repair".to_string(),
            StatusFeedRole::Warning,
        ),
        Some("failed") if policy == "never" => (
            "State: disabled by policy after failed gate".to_string(),
            StatusFeedRole::Warning,
        ),
        Some("failed") => (
            "State: unresolved after failed gate".to_string(),
            StatusFeedRole::Warning,
        ),
        Some(status) => (
            format!("State: waiting after gate {status}"),
            StatusFeedRole::Dim,
        ),
    }
}

fn gate_pane_handoff_lines(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    exit_states: Option<&WorkflowExitStates>,
) -> (String, Vec<StatusFeedLine>) {
    if let Some(exit_states) = exit_states {
        let meta = gate_pane_display_or_dash(&exit_states.handoff).to_string();
        let mut lines = vec![gate_pane_line(
            format!("Handoff: {meta}"),
            StatusFeedRole::Info,
        )];
        if !exit_states.evidence.trim().is_empty() {
            lines.push(gate_pane_line(
                format!("Evidence: {}", exit_states.evidence),
                StatusFeedRole::Dim,
            ));
        }
        return (meta, lines);
    }
    match details.run.status {
        RunStatus::Completed => (
            "ready".to_string(),
            vec![gate_pane_line("Handoff: ready", StatusFeedRole::Success)],
        ),
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => (
            "not_ready".to_string(),
            vec![gate_pane_line(
                format!("Handoff: not ready — run is {}", details.run.status),
                StatusFeedRole::Warning,
            )],
        ),
        RunStatus::Pending | RunStatus::Running => {
            if latest_gate.is_some_and(|gate| gate.status == "failed") {
                (
                    "not_ready".to_string(),
                    vec![gate_pane_line(
                        "Handoff: not ready — latest gate failed",
                        StatusFeedRole::Warning,
                    )],
                )
            } else {
                (
                    "unknown".to_string(),
                    vec![gate_pane_line(
                        "Handoff: unknown until integration gate finishes",
                        StatusFeedRole::Dim,
                    )],
                )
            }
        }
    }
}

fn gate_pane_last_failure(
    pre_repair_gate: Option<&GateResult>,
    latest_gate: Option<&GateResult>,
) -> Option<String> {
    pre_repair_gate
        .and_then(gate_pane_failure_line)
        .map(|failure| format!("{failure} (pre-repair)"))
        .or_else(|| latest_gate.and_then(gate_pane_failure_line))
}

fn gate_pane_failure_line(gate: &GateResult) -> Option<String> {
    gate.commands
        .iter()
        .find(|command| command.status == "failed")
        .map(|command| {
            let output = command
                .output
                .trim()
                .lines()
                .last()
                .unwrap_or_default()
                .trim();
            if !output.is_empty() {
                format!(
                    "{} — {}",
                    gate_pane_truncate(&command.command, 80),
                    gate_pane_truncate(output, 90)
                )
            } else if let Some(exit_code) = command.exit_code {
                format!(
                    "{} (exit {exit_code})",
                    gate_pane_truncate(&command.command, 120)
                )
            } else {
                gate_pane_truncate(&command.command, 120)
            }
        })
}

fn gate_pane_role_for_gate_status(status: &str) -> StatusFeedRole {
    match status {
        "passed" => StatusFeedRole::Success,
        "failed" => StatusFeedRole::Error,
        "skipped" => StatusFeedRole::Dim,
        _ => StatusFeedRole::Info,
    }
}

fn gate_pane_role_for_repair_status(status: &str) -> StatusFeedRole {
    match status {
        "completed" | "fixed" | "no-op" => StatusFeedRole::Success,
        "failed" | "blocked" => StatusFeedRole::Error,
        "skipped" => StatusFeedRole::Dim,
        _ => StatusFeedRole::Info,
    }
}

fn gate_pane_summary_suffix(summary: &str) -> String {
    if summary.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", gate_pane_truncate(summary, GATE_PANE_LINE_WIDTH))
    }
}

fn gate_pane_status_icon(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "✓",
        RunStatus::Running => "●",
        RunStatus::Blocked => "!",
        RunStatus::Failed => "✗",
        RunStatus::Cancelled | RunStatus::Interrupted => "×",
        RunStatus::Pending => "○",
    }
}

fn gate_pane_terminal_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed
            | RunStatus::Failed
            | RunStatus::Blocked
            | RunStatus::Cancelled
            | RunStatus::Interrupted
    )
}

fn gate_pane_since(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(time).num_seconds().max(0) as u64;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn gate_pane_short_run_id(value: &str) -> String {
    if value.chars().count() <= 30 {
        return gate_pane_display_or_dash(value).to_string();
    }
    let prefix = value.chars().take(11).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn gate_pane_truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push('…');
    truncated
}

fn gate_pane_display_or_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn gate_pane_push_unique(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn gate_pane_block(
    label: impl Into<String>,
    meta: impl Into<String>,
    lines: Vec<StatusFeedLine>,
) -> StatusFeedBlock {
    StatusFeedBlock {
        label: label.into(),
        meta: meta.into(),
        lines,
    }
}

fn gate_pane_line(text: impl Into<String>, role: StatusFeedRole) -> StatusFeedLine {
    StatusFeedLine {
        text: text.into(),
        role,
    }
}

pub fn project_run(details: &RunDetails) -> StatusFeed {
    project_run_at(details, Utc::now())
}

#[allow(dead_code)]
pub fn project_waiting(repo: &str) -> StatusFeed {
    StatusFeed {
        feed_version: FEED_VERSION,
        summary_line: format!("waiting for active run in {repo}"),
        terminal_reason: None,
        operator_commands: Vec::new(),
        attention: Vec::new(),
        blocks: vec![
            block(
                "Run",
                "waiting",
                vec![
                    line(format!("repo {repo}"), StatusFeedRole::Info),
                    line(
                        "waiting for the latest active daemon-owned run",
                        StatusFeedRole::Dim,
                    ),
                ],
            ),
            block(
                "Hint",
                "",
                vec![line(
                    "start a run normally; this dashboard will attach when status --latest returns one",
                    StatusFeedRole::Dim,
                )],
            ),
        ],
    }
}

pub fn project_run_at(details: &RunDetails, now: DateTime<Utc>) -> StatusFeed {
    let question_commands = pending_question_commands(details);
    let replan_commands = pending_replan_commands(details);
    let attention = dashboard_attention(details, now);
    let operator_commands = operator_commands(details, &question_commands, &replan_commands);

    let mut blocks = vec![
        run_block(details, now),
        mission_block(details),
        workers_block(details, now),
        attention_block(&attention),
    ];
    if !operator_commands.is_empty() {
        blocks.push(commands_block(&operator_commands));
    }
    blocks.push(checks_block(details, now));
    blocks.push(match &details.economics {
        Some(economics) => economics_block(details, economics),
        None => economics_missing_block(),
    });
    if !details.incidents.is_empty() {
        blocks.push(incidents_block(&details.incidents));
    }

    StatusFeed {
        feed_version: FEED_VERSION,
        summary_line: monitor_message(details),
        terminal_reason: details.primary_terminal_reason.clone(),
        operator_commands,
        attention,
        blocks,
    }
}
fn replan_source_label(proposal: &crate::domain::ReplanProposal) -> String {
    let mut parts = vec![display_or_dash(&proposal.source.kind).to_string()];
    if !proposal.source.slice_id.trim().is_empty() {
        parts.push(proposal.source.slice_id.clone());
    }
    if !proposal.source.phase.trim().is_empty() {
        parts.push(proposal.source.phase.clone());
    }
    if proposal.source.attempt > 0 {
        parts.push(format!("attempt {}", proposal.source.attempt));
    }
    parts.join("/")
}

fn question_answer_command(question: &crate::domain::WorkerQuestion) -> String {
    format!(
        "khazad-doom answer {} {} <answer>",
        question.run_id, question.id
    )
}

fn pending_question_commands(details: &RunDetails) -> Vec<String> {
    details
        .questions
        .iter()
        .filter(|question| question.state == "pending")
        .map(question_answer_command)
        .collect()
}

fn pending_question_attention(details: &RunDetails, now: DateTime<Utc>) -> Vec<StatusFeedLine> {
    let mut lines = Vec::new();
    for question in details
        .questions
        .iter()
        .filter(|question| question.state == "pending")
    {
        lines.push(line(
            format!(
                "Pending question {} • slice={} • attempt={}",
                question.id, question.slice_id, question.attempt
            ),
            StatusFeedRole::Attention,
        ));
        lines.push(line(
            format!("Question: {}", question.question),
            StatusFeedRole::Attention,
        ));
        if question.options.is_empty() {
            lines.push(line("Options: <none recorded>", StatusFeedRole::Attention));
        } else {
            for (index, option) in question.options.iter().enumerate() {
                lines.push(line(
                    format!("Option {}: {}", index + 1, option),
                    StatusFeedRole::Attention,
                ));
            }
        }
        lines.push(line(
            format!("Answer command: {}", question_answer_command(question)),
            StatusFeedRole::Attention,
        ));
        lines.push(line(
            question_deadline_label(question, now),
            StatusFeedRole::Attention,
        ));
    }
    lines
}

fn question_deadline_label(question: &crate::domain::WorkerQuestion, now: DateTime<Utc>) -> String {
    if question.timeout_seconds == 0 {
        return "Deadline: none configured; waiting indefinitely".to_string();
    }
    let deadline = question.asked_at + chrono::Duration::seconds(question.timeout_seconds as i64);
    let remaining = if deadline >= now {
        format!(
            "remaining {}",
            format_duration((deadline - now).to_std().unwrap_or_default())
        )
    } else {
        format!(
            "overdue by {}",
            format_duration((now - deadline).to_std().unwrap_or_default())
        )
    };
    format!("Deadline: {} ({remaining})", deadline.to_rfc3339())
}

fn pending_replan_commands(details: &RunDetails) -> Vec<String> {
    details
        .replan
        .pending
        .iter()
        .flat_map(|proposal| proposal.decision_commands.clone())
        .collect()
}

fn replan_attention(details: &RunDetails) -> Vec<StatusFeedLine> {
    let mut lines = Vec::new();
    for proposal in &details.replan.pending {
        let source = replan_source_label(proposal);
        lines.push(line(
            format!(
                "Pending replan {} • {source} • risk={}",
                proposal.id,
                display_or_dash(&proposal.risk)
            ),
            StatusFeedRole::Attention,
        ));
        for change in &proposal.proposed_changes {
            lines.push(line(
                proposed_change_feed_text(change),
                StatusFeedRole::Attention,
            ));
        }
        for command in &proposal.decision_commands {
            lines.push(line(
                format!("Decision command: {command}"),
                StatusFeedRole::Attention,
            ));
        }
    }
    for proposal in &details.replan.history {
        let Some(decision) = proposal.operator_decision.as_ref() else {
            continue;
        };
        match decision.apply_status.as_str() {
            "refused" => lines.push(line(
                format!(
                    "Replan {} apply refused • remediation: supersede with a valid follow-up proposal or start a new run • reason: {}",
                    proposal.id,
                    display_or_dash(&decision.apply_reason)
                ),
                StatusFeedRole::Attention,
            )),
            "incomplete" => lines.push(line(
                format!(
                    "Replan {} apply incomplete • remediation: khazad-doom resume {} • reason: {}",
                    proposal.id,
                    details.run.id,
                    display_or_dash(&decision.apply_reason)
                ),
                StatusFeedRole::Attention,
            )),
            "pending" if decision.decision == "accepted" => lines.push(line(
                format!(
                    "Replan {} accepted, applying at next checkpoint • remediation: khazad-doom resume {}",
                    proposal.id, details.run.id
                ),
                StatusFeedRole::Attention,
            )),
            _ => {}
        }
    }
    lines
}

fn proposed_change_feed_text(change: &crate::domain::ReplanProposedChange) -> String {
    if let Some(draft) = change.followup_slice_draft() {
        let areas = if draft.areas.is_empty() {
            "<none>".to_string()
        } else {
            draft.areas.join(",")
        };
        return format!(
            "Proposed follow-up slice: {} — {}; goal={}; areas=[{}]; acceptance={}; verify={}",
            display_or_dash(&draft.id),
            display_or_dash(&draft.title),
            display_or_dash(&draft.goal),
            areas,
            draft.acceptance.len(),
            draft.verify.len()
        );
    }
    format!(
        "Proposed change: {}:{} — {}",
        change.kind,
        change.target,
        change.summary_text()
    )
}

fn operator_commands(
    details: &RunDetails,
    question_commands: &[String],
    replan_commands: &[String],
) -> Vec<String> {
    let mut commands = Vec::new();
    for command in replan_commands {
        push_unique(&mut commands, command.clone());
    }
    for command in question_commands {
        push_unique(&mut commands, command.clone());
    }
    if let Some(reason) = &details.primary_terminal_reason {
        for command in &reason.operator_commands {
            push_unique(&mut commands, command.clone());
        }
    }
    commands
}

fn commands_block(commands: &[String]) -> StatusFeedBlock {
    block(
        "Commands",
        "",
        commands
            .iter()
            .map(|command| line(command.clone(), StatusFeedRole::Attention))
            .collect(),
    )
}

fn dashboard_attention(details: &RunDetails, now: DateTime<Utc>) -> Vec<StatusFeedLine> {
    let mut lines = Vec::new();
    if let Some(reason) = &details.primary_terminal_reason {
        lines.extend(terminal_reason_attention_lines(reason));
    }
    lines.extend(pending_question_attention(details, now));
    lines.extend(replan_attention(details));
    lines
}

fn terminal_reason_attention_lines(reason: &TerminalReason) -> Vec<StatusFeedLine> {
    let mut lines = vec![line(
        format!(
            "Terminal reason: kind={} • owner={} • retryable={} • operator_action_required={}",
            display_or_dash(&reason.kind),
            display_or_dash(&reason.resolution_owner),
            reason.retryable,
            reason.operator_action_required
        ),
        StatusFeedRole::Attention,
    )];
    if !reason.summary.trim().is_empty() {
        lines.push(line(
            format!("Summary: {}", reason.summary),
            StatusFeedRole::Attention,
        ));
    }
    if !reason.remediation.trim().is_empty() {
        lines.push(line(
            format!("Remediation: {}", reason.remediation),
            StatusFeedRole::Attention,
        ));
    }
    if !reason.disposition.trim().is_empty() {
        lines.push(line(
            format!("Disposition: {}", reason.disposition),
            StatusFeedRole::Attention,
        ));
    }
    if !reason.evidence_links.is_empty() {
        lines.push(line(
            format!("Evidence: {}", reason.evidence_links.join(", ")),
            StatusFeedRole::Attention,
        ));
    }
    lines
}

fn attention_block(attention: &[StatusFeedLine]) -> StatusFeedBlock {
    let lines = if attention.is_empty() {
        vec![line("no operator attention", StatusFeedRole::Dim)]
    } else {
        attention.to_vec()
    };
    block("Attention", "", lines)
}

fn mission_block(details: &RunDetails) -> StatusFeedBlock {
    match &details.mission_envelope {
        Some(envelope) => mission_envelope_block(envelope, details.frontier_budget.as_ref()),
        None => block(
            "Mission",
            "autonomy off",
            vec![
                line("no envelope (autonomy off)", StatusFeedRole::Dim),
                line(
                    "frontier authority inactive; legacy run behavior unchanged",
                    StatusFeedRole::Dim,
                ),
            ],
        ),
    }
}

fn mission_envelope_block(
    envelope: &MissionEnvelope,
    budget: Option<&FrontierBudgetState>,
) -> StatusFeedBlock {
    let budget = budget.cloned().unwrap_or_default();
    let mut lines = vec![
        line(
            format!("goal {}", truncate_display(&envelope.goal, LINE_WIDTH)),
            StatusFeedRole::Info,
        ),
        line(
            format!("allowed areas {}", list_or_none(&envelope.allowed_areas)),
            StatusFeedRole::Info,
        ),
        line(
            format!("non-goals {}", list_or_none(&envelope.non_goals)),
            StatusFeedRole::Dim,
        ),
        line(
            format!(
                "verify profile {}",
                display_or_dash(&envelope.verify_profile)
            ),
            StatusFeedRole::Dim,
        ),
        line(
            format!(
                "budgets auto_promotions {}/{} • generated_slices {}/{} • max_depth {} • max_generation_reached {}",
                budget.auto_promotions_used,
                envelope.max_auto_promotions,
                budget.generated_slices,
                envelope.max_generated_slices,
                envelope.max_depth,
                budget.max_generation_reached
            ),
            StatusFeedRole::Info,
        ),
    ];
    let autonomy = envelope.autonomy_level.as_str();
    if envelope.autonomy_level.recorded_not_active() {
        lines.push(line(
            format!("autonomy {autonomy} recorded, not yet active; effective behavior off"),
            StatusFeedRole::Warning,
        ));
    } else {
        lines.push(line(
            "autonomy off; frontier authority inactive".to_string(),
            StatusFeedRole::Dim,
        ));
    }
    if !envelope.must_ask_if.is_empty() {
        lines.push(line(
            format!("must ask if {}", list_or_none(&envelope.must_ask_if)),
            StatusFeedRole::Dim,
        ));
    }
    block("Mission", "envelope recorded", lines)
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "<none>".to_string()
    } else {
        truncate_display(&values.join(", "), LINE_WIDTH)
    }
}

fn workers_block(details: &RunDetails, now: DateTime<Utc>) -> StatusFeedBlock {
    let items = selected_slice_items(details);
    let active = active_worker_count(details, &items);
    let mut lines = Vec::new();

    if let Some(progress) = &details.progress
        && !should_hide_current_progress(details, progress)
    {
        if let Some(active_line) = active_worker_line(details, progress, now) {
            lines.push(active_line);
        }
        if let Some(worker) = &progress.worker {
            lines.extend(worker_supervision_lines(worker, now));
            if let Some(warning) = worker_quiet_warning(worker, now) {
                lines.push(line(warning, StatusFeedRole::Warning));
                lines.push(line(
                    "wait, inspect, or cancel explicitly",
                    StatusFeedRole::Dim,
                ));
            }
        }
    }

    if items.is_empty() {
        if lines.is_empty() {
            lines.push(line("no selected workers recorded", StatusFeedRole::Dim));
        }
    } else {
        for slice in items.iter().take(WORKER_ITEMS) {
            lines.push(line(todo_line(slice), role_for_slice_status(slice.status)));
        }
        if items.len() > WORKER_ITEMS {
            lines.push(line(
                format!("… {} more", items.len() - WORKER_ITEMS),
                StatusFeedRole::Dim,
            ));
        }
    }

    block(
        "Workers",
        format!("({active} active / {} total)", items.len()),
        lines,
    )
}

fn worker_supervision_lines(
    worker: &WorkerAttemptProgress,
    now: DateTime<Utc>,
) -> Vec<StatusFeedLine> {
    vec![
        line(
            format!("Process: {}", worker_process_label(worker)),
            StatusFeedRole::Info,
        ),
        line(
            format!(
                "Last worker event: {}",
                last_worker_event_label(worker, now)
            ),
            StatusFeedRole::Info,
        ),
        line(
            format!("Timeout: {}", timeout_label(worker, now)),
            StatusFeedRole::Dim,
        ),
    ]
}

fn active_worker_count(details: &RunDetails, items: &[SliceRun]) -> usize {
    if let Some(progress) = &details.progress
        && !should_hide_current_progress(details, progress)
        && is_worker_agent_phase(&progress.phase)
    {
        if progress.parallel_layer && !progress.parallel_slices.is_empty() {
            return progress.parallel_slices.len();
        }
        return 1;
    }
    items
        .iter()
        .filter(|slice| slice.status == SliceStatus::Running)
        .count()
}

fn active_worker_line(
    details: &RunDetails,
    progress: &RunProgress,
    now: DateTime<Utc>,
) -> Option<StatusFeedLine> {
    if !is_worker_agent_phase(&progress.phase) {
        return None;
    }
    let target = if progress.parallel_layer && !progress.parallel_slices.is_empty() {
        format!("parallel {}", progress.parallel_slices.join(", "))
    } else if !progress.slice_id.trim().is_empty() {
        progress.slice_id.clone()
    } else {
        monitor_slice_label(details)
    };
    let attempt = if progress.attempt > 0 {
        format!(" • attempt {}", progress.attempt)
    } else {
        String::new()
    };
    let mut text = format!(
        "active {target}{attempt} • elapsed {}",
        since_time(progress.phase_started_at, now)
    );
    if let Some(worker) = &progress.worker {
        text.push_str(&format!(" • Supervisor: {}", supervisor_label(worker, now)));
    }
    if !progress.message.trim().is_empty() {
        text.push_str(&format!(
            " • {}",
            truncate_display(&progress.message, LINE_WIDTH / 2)
        ));
    }
    Some(line(
        truncate_display(&text, LINE_WIDTH),
        StatusFeedRole::Info,
    ))
}

fn checks_block(details: &RunDetails, now: DateTime<Utc>) -> StatusFeedBlock {
    let summary = gate_pane_latest_implementation_summary(details);
    let latest_gate = summary
        .as_ref()
        .and_then(|summary| summary.integration_gate.clone())
        .or_else(|| gate_pane_gate_result_from_economics(details));
    let pre_repair_gate = summary
        .as_ref()
        .and_then(|summary| summary.pre_repair_integration_gate.clone());
    let repair = summary
        .as_ref()
        .and_then(|summary| summary.integration_repair.clone());

    let mut lines = vec![line(
        format!(
            "verify profile {}",
            gate_pane_verification_profile(details, summary.as_ref())
        ),
        StatusFeedRole::Dim,
    )];
    match &details.progress {
        Some(progress) if !should_hide_current_progress(details, progress) => {
            lines.push(line(
                format!(
                    "phase {} • updated {} ago",
                    progress_phase_label(progress),
                    since_time(progress.updated_at, now)
                ),
                StatusFeedRole::Info,
            ));
            if !progress.command.trim().is_empty() && !is_worker_agent_phase(&progress.phase) {
                lines.push(line(
                    format!(
                        "command {}",
                        truncate_display(&progress.command, LINE_WIDTH)
                    ),
                    StatusFeedRole::Dim,
                ));
            }
            if !progress.message.trim().is_empty() {
                lines.push(line(
                    format!(
                        "message {}",
                        truncate_display(&progress.message, LINE_WIDTH)
                    ),
                    StatusFeedRole::Info,
                ));
            }
            if !progress.output_tail.trim().is_empty() {
                lines.push(line(
                    format!(
                        "tail {}",
                        truncate_display(&progress.output_tail, LINE_WIDTH)
                    ),
                    StatusFeedRole::Info,
                ));
            }
        }
        _ => lines.push(line(
            format!("phase {}", details.run.status),
            StatusFeedRole::Dim,
        )),
    }

    match &latest_gate {
        Some(gate) => lines.push(line(
            format!(
                "gate {}{}",
                display_or_dash(&gate.status),
                dashboard_summary_suffix(&gate.summary)
            ),
            gate_pane_role_for_gate_status(&gate.status),
        )),
        None => lines.push(line("gate not run yet", StatusFeedRole::Dim)),
    }
    if let Some(last_failure) =
        gate_pane_last_failure(pre_repair_gate.as_ref(), latest_gate.as_ref())
    {
        lines.push(line(
            format!("last failure {last_failure}"),
            StatusFeedRole::Warning,
        ));
    }
    let repair_policy = gate_pane_repair_policy(details);
    let (repair_state, repair_role) =
        gate_pane_repair_state(&repair_policy, latest_gate.as_ref(), repair.as_ref());
    lines.push(line(
        format!(
            "repair {repair_policy} • {}",
            repair_state
                .strip_prefix("State: ")
                .unwrap_or(&repair_state)
        ),
        repair_role,
    ));

    let worker = details
        .progress
        .as_ref()
        .and_then(|progress| progress.worker.as_ref());
    let semantic = last_semantic_progress_label(worker, now);
    lines.push(line(
        format!("semantic {semantic}"),
        if semantic == "unknown" {
            StatusFeedRole::Dim
        } else {
            StatusFeedRole::Info
        },
    ));

    block("Checks", "", lines)
}

fn dashboard_summary_suffix(summary: &str) -> String {
    if summary.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", truncate_display(summary, LINE_WIDTH))
    }
}

fn economics_missing_block() -> StatusFeedBlock {
    block(
        "Economics",
        "",
        vec![line("not recorded yet", StatusFeedRole::Dim)],
    )
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}
fn run_block(details: &RunDetails, now: DateTime<Utc>) -> StatusFeedBlock {
    let progress = details.progress.as_ref();
    let phase = progress
        .map(progress_phase_label)
        .filter(|phase| !phase.trim().is_empty())
        .unwrap_or_else(|| {
            if is_terminal_status(details.run.status) {
                details.run.status.as_str().to_string()
            } else {
                "unknown".to_string()
            }
        });
    let elapsed_start = progress
        .map(|progress| progress.phase_started_at)
        .unwrap_or(details.run.started_at);
    let mut lines = vec![
        line(
            format!("phase {phase} • elapsed {}", since_time(elapsed_start, now)),
            StatusFeedRole::Info,
        ),
        line(
            format!("repo {}", short_path(&details.run.repo_path)),
            StatusFeedRole::Dim,
        ),
    ];
    if let Some(profile) = compact_worker_profile(&details.worker_profile) {
        lines.push(line(format!("profile {profile}"), StatusFeedRole::Dim));
    }
    if details.worker_profile.worker_evidence_kind
        == "deterministic_test_double_not_real_pi_worker_evidence"
    {
        lines.push(line(
            "worker evidence deterministic test-double; not real Pi",
            StatusFeedRole::Warning,
        ));
    }
    let message = monitor_message(details);
    if !message.trim().is_empty() {
        lines.push(line(
            truncate_display(&message, LINE_WIDTH),
            role_for_status(details.run.status),
        ));
    }
    block(
        "Run",
        format!(
            "{} {} • {}",
            status_icon(details.run.status),
            details.run.status,
            short_run_id(&details.run.id)
        ),
        lines,
    )
}

fn should_hide_current_progress(details: &RunDetails, progress: &RunProgress) -> bool {
    is_terminal_status(details.run.status) && is_terminal_phase(&progress.phase)
}
fn last_semantic_progress_label(
    worker: Option<&WorkerAttemptProgress>,
    now: DateTime<Utc>,
) -> String {
    let Some(worker) = worker else {
        return "unknown".to_string();
    };
    match (
        worker.last_semantic_progress_summary.trim(),
        worker.last_semantic_progress_at,
    ) {
        (summary, Some(time)) if !summary.is_empty() => {
            format!("{summary} • {} ago", since_time(time, now))
        }
        (_, Some(time)) => format!("{} ago", since_time(time, now)),
        _ => "unknown".to_string(),
    }
}

fn economics_block(details: &RunDetails, economics: &RunEconomics) -> StatusFeedBlock {
    let active_agents = active_agent_call_count(details);
    let active_commands = active_command_count(details);
    let active_work = active_agents > 0 || active_commands > 0;
    let mut lines = vec![
        line(
            format!(
                "agents {} • cmds {} • dup {} • cache {}/{}",
                agent_call_count_label(economics.agent_call_count, active_agents, active_work),
                command_count_label(economics.command_execution_count, active_commands),
                economics.duplicate_command_count,
                economics.cache_hits,
                economics.cache_misses
            ),
            StatusFeedRole::Info,
        ),
        line(
            format!(
                "repair {} {}/{} • fail-fast {}",
                display_or_dash(&economics.repair_policy),
                economics.repair_attempts,
                economics.repair_max_attempts,
                economics.gate_fail_fast
            ),
            StatusFeedRole::Info,
        ),
    ];
    if economics.agent_calls.iter().any(|call| {
        call.worker_evidence_kind() == "deterministic_test_double_not_real_pi_worker_evidence"
    }) {
        lines.push(line(
            "worker evidence deterministic test-double; not real Pi",
            StatusFeedRole::Warning,
        ));
    }
    if !economics.sla_violations.is_empty() {
        lines.push(line(
            format!("SLA {}", economics.sla_violations.join("; ")),
            StatusFeedRole::Warning,
        ));
    }
    block("Economics", if active_work { "active" } else { "" }, lines)
}

fn agent_call_count_label(completed: usize, in_flight: usize, active_work: bool) -> String {
    if in_flight > 0 {
        format!("{completed} completed + {in_flight} in flight")
    } else if active_work && completed == 0 {
        "0 completed (in-flight/unknown)".to_string()
    } else {
        completed.to_string()
    }
}

fn command_count_label(completed: usize, in_flight: usize) -> String {
    if in_flight > 0 {
        format!("{completed} completed + {in_flight} in flight")
    } else {
        completed.to_string()
    }
}

fn active_agent_call_count(details: &RunDetails) -> usize {
    let Some(progress) = &details.progress else {
        return 0;
    };
    if is_terminal_status(details.run.status) || !is_worker_agent_phase(&progress.phase) {
        return 0;
    }
    if progress.parallel_layer && !progress.parallel_slices.is_empty() {
        progress.parallel_slices.len()
    } else {
        1
    }
}

fn active_command_count(details: &RunDetails) -> usize {
    let Some(progress) = &details.progress else {
        return 0;
    };
    if is_terminal_status(details.run.status)
        || progress.command.trim().is_empty()
        || is_worker_agent_phase(&progress.phase)
    {
        return 0;
    }
    1
}

fn is_worker_agent_phase(phase: &str) -> bool {
    matches!(
        phase,
        "worker_started"
            | "worker_running"
            | "parallel_worker_layer"
            | "integration_repair"
            | "awaiting_operator"
    )
}

fn incidents_block(incidents: &[RunIncident]) -> StatusFeedBlock {
    let lines = incidents
        .iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|incident| {
            line(
                format!(
                    "{}: {}",
                    incident.kind,
                    truncate_display(&incident.message, LINE_WIDTH)
                ),
                if incident.severity == "error" {
                    StatusFeedRole::Error
                } else {
                    StatusFeedRole::Warning
                },
            )
        })
        .collect();
    block("Incidents", format!("({})", incidents.len()), lines)
}
fn selected_slice_items(details: &RunDetails) -> Vec<SliceRun> {
    if !details.slice_runs.is_empty() {
        return details.slice_runs.clone();
    }
    details
        .run
        .selected_slice_id
        .split(',')
        .map(str::trim)
        .filter(|slice_id| !slice_id.is_empty())
        .map(|slice_id| SliceRun {
            run_id: details.run.id.clone(),
            slice_id: slice_id.to_string(),
            status: SliceStatus::Pending,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 0,
            last_error: String::new(),
        })
        .collect()
}

fn todo_line(slice: &SliceRun) -> String {
    let mut meta = Vec::new();
    meta.push(slice.status.to_string());
    if slice.attempts > 0 {
        meta.push(format!(
            "{} {}",
            slice.attempts,
            if slice.attempts == 1 {
                "attempt"
            } else {
                "attempts"
            }
        ));
    }
    if !slice.commit_sha.trim().is_empty() {
        meta.push(short_sha(&slice.commit_sha));
    }
    format!(
        "{} {}{}",
        slice_checkbox(slice.status),
        slice.slice_id,
        if meta.is_empty() {
            String::new()
        } else {
            format!("  {}", meta.join(" • "))
        }
    )
}
fn monitor_message(details: &RunDetails) -> String {
    if let Some(progress) = &details.progress
        && !progress.message.trim().is_empty()
    {
        return progress.message.clone();
    }
    if !details.run.error.trim().is_empty() {
        return details.run.error.clone();
    }
    format!("run is {}", details.run.status)
}

fn monitor_slice_label(details: &RunDetails) -> String {
    if let Some(progress) = &details.progress
        && progress.parallel_layer
        && !progress.parallel_slices.is_empty()
    {
        return format!("parallel layer: {}", progress.parallel_slices.join(", "));
    }
    if let Some(progress) = &details.progress
        && !progress.slice_id.trim().is_empty()
    {
        return progress.slice_id.clone();
    }
    for status in [
        SliceStatus::Running,
        SliceStatus::RepairNeeded,
        SliceStatus::ReadyToMerge,
        SliceStatus::Pending,
    ] {
        if let Some(slice_run) = details
            .slice_runs
            .iter()
            .find(|slice_run| slice_run.status == status)
        {
            return format!("{} ({})", slice_run.slice_id, slice_run.status);
        }
    }
    if details.slice_runs.len() == 1 {
        let slice_run = &details.slice_runs[0];
        return format!("{} ({})", slice_run.slice_id, slice_run.status);
    }
    display_or_dash(&details.run.selected_slice_id).to_string()
}

fn progress_phase_label(progress: &RunProgress) -> String {
    if progress.parallel_layer && progress.phase != "parallel_worker_layer" {
        format!("parallel_worker_layer ({})", progress.phase)
    } else {
        progress.phase.clone()
    }
}

fn supervisor_label(worker: &WorkerAttemptProgress, now: DateTime<Utc>) -> String {
    match worker.process_observed_at {
        Some(observed_at) => format!("alive, observed child {} ago", since_time(observed_at, now)),
        None => "starting, no child observation yet".to_string(),
    }
}

fn worker_process_label(worker: &WorkerAttemptProgress) -> String {
    match worker.pid {
        Some(pid) => format!("running pid={pid}"),
        None => "running".to_string(),
    }
}

fn last_worker_event_label(worker: &WorkerAttemptProgress, now: DateTime<Utc>) -> String {
    match worker.last_event_at {
        Some(last_event_at) if worker.last_event_kind.trim().is_empty() => {
            format!("{} ago", since_time(last_event_at, now))
        }
        Some(last_event_at) => format!(
            "{} ago ({})",
            since_time(last_event_at, now),
            worker.last_event_kind
        ),
        None => "none".to_string(),
    }
}

fn timeout_label(worker: &WorkerAttemptProgress, now: DateTime<Utc>) -> String {
    if worker.attempt_timeout_seconds == 0 {
        return "disabled".to_string();
    }
    let elapsed = now
        .signed_duration_since(worker.attempt_started_at)
        .to_std()
        .unwrap_or_default();
    let timeout = Duration::from_secs(worker.attempt_timeout_seconds);
    if elapsed >= timeout {
        return format!(
            "{}s, exceeded by {}",
            worker.attempt_timeout_seconds,
            format_duration(elapsed.saturating_sub(timeout))
        );
    }
    format!(
        "{}s, remaining {}",
        worker.attempt_timeout_seconds,
        format_duration(timeout.saturating_sub(elapsed))
    )
}

fn worker_quiet_warning(worker: &WorkerAttemptProgress, now: DateTime<Utc>) -> Option<String> {
    if worker.no_output_warning_seconds == 0 {
        return None;
    }
    let reference = worker.last_event_at.unwrap_or(worker.attempt_started_at);
    let quiet_for = now
        .signed_duration_since(reference)
        .to_std()
        .unwrap_or_default();
    if quiet_for < Duration::from_secs(worker.no_output_warning_seconds) {
        return None;
    }
    let timeout_suffix = if worker.attempt_timeout_seconds == 0 {
        "; no timeout configured"
    } else {
        ""
    };
    Some(format!(
        "worker is quiet for {}; this may be normal{}",
        format_duration(quiet_for),
        timeout_suffix
    ))
}

fn status_icon(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "✓",
        RunStatus::Running => "●",
        RunStatus::Blocked => "!",
        RunStatus::Failed => "✗",
        RunStatus::Cancelled | RunStatus::Interrupted => "×",
        RunStatus::Pending => "○",
    }
}

fn slice_checkbox(status: SliceStatus) -> &'static str {
    match status {
        SliceStatus::Merged => "☒",
        SliceStatus::Running | SliceStatus::ReadyToMerge | SliceStatus::RepairNeeded => "◐",
        SliceStatus::Failed
        | SliceStatus::Blocked
        | SliceStatus::Cancelled
        | SliceStatus::Interrupted => "✗",
        SliceStatus::Pending => "☐",
    }
}

fn role_for_status(status: RunStatus) -> StatusFeedRole {
    match status {
        RunStatus::Completed => StatusFeedRole::Success,
        RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted => {
            StatusFeedRole::Warning
        }
        RunStatus::Failed => StatusFeedRole::Error,
        RunStatus::Running | RunStatus::Pending => StatusFeedRole::Info,
    }
}

fn role_for_slice_status(status: SliceStatus) -> StatusFeedRole {
    match status {
        SliceStatus::Merged => StatusFeedRole::Success,
        SliceStatus::Blocked | SliceStatus::Cancelled | SliceStatus::Interrupted => {
            StatusFeedRole::Warning
        }
        SliceStatus::Failed => StatusFeedRole::Error,
        _ => StatusFeedRole::Info,
    }
}
fn is_terminal_phase(phase: &str) -> bool {
    matches!(
        phase,
        "completed" | "failed" | "blocked" | "cancelled" | "interrupted"
    )
}

fn is_terminal_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed
            | RunStatus::Failed
            | RunStatus::Blocked
            | RunStatus::Cancelled
            | RunStatus::Interrupted
    )
}

fn since_time(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let duration = now.signed_duration_since(time).to_std().unwrap_or_default();
    format_duration(duration)
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn short_sha(value: &str) -> String {
    value.chars().take(8).collect()
}

fn short_run_id(value: &str) -> String {
    if value.chars().count() <= 30 {
        return display_or_dash(value).to_string();
    }
    let prefix = value.chars().take(11).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn short_path(value: &str) -> String {
    let text = value.trim();
    if text.is_empty() {
        return "-".to_string();
    }
    let parts = text
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() <= 2 {
        return text.to_string();
    }
    format!("…/{}", parts[parts.len().saturating_sub(2)..].join("/"))
}

fn compact_worker_profile(profile: &crate::domain::WorkerProfileEvidence) -> Option<String> {
    let mut parts = Vec::new();
    let agent = [profile.agent.trim(), profile.agent_profile.trim()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if !agent.is_empty() {
        parts.push(agent);
    }
    let model = [profile.agent_provider.trim(), profile.agent_model.trim()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if !model.is_empty() {
        parts.push(model);
    }
    let mode = [profile.agent_reasoning.trim(), profile.agent_mode.trim()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if !mode.is_empty() {
        parts.push(mode);
    }
    if parts.is_empty() && !profile.profile_summary.trim().is_empty() {
        parts.push(profile.profile_summary.trim().to_string());
    }
    (!parts.is_empty()).then(|| truncate_display(&parts.join(" • "), LINE_WIDTH))
}

fn display_or_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn truncate_display(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push('…');
    truncated
}

fn block(
    label: impl Into<String>,
    meta: impl Into<String>,
    lines: Vec<StatusFeedLine>,
) -> StatusFeedBlock {
    StatusFeedBlock {
        label: label.into(),
        meta: meta.into(),
        lines,
    }
}

fn line(text: impl Into<String>, role: StatusFeedRole) -> StatusFeedLine {
    StatusFeedLine {
        text: text.into(),
        role,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Event, ReplanProposalState};
    use crate::domain::{
        ReplanProposal, ReplanProposalSource, ReplanProposedChange, ReplanStatus, Run,
        WorkerQuestion, replan_decision_commands,
    };

    #[test]
    fn dashboard_v2_projection_has_versioned_compact_blocks_and_raw_safe_roles() {
        let now = Utc::now();
        let details = RunDetails {
            run: Run {
                id: "kd-test".to_string(),
                repo_id: "repo".to_string(),
                repo_path: "/tmp/repo".to_string(),
                status: RunStatus::Running,
                base_branch: "main".to_string(),
                base_sha: "base".to_string(),
                integration_branch: "khazad/kd-test/integration".to_string(),
                selected_slice_id: "slice-1".to_string(),
                error: String::new(),
                started_at: now,
                updated_at: now,
            },
            worker_profile: Default::default(),
            slice_runs: vec![SliceRun {
                run_id: "kd-test".to_string(),
                slice_id: "slice-1".to_string(),
                status: SliceStatus::Pending,
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 0,
                last_error: String::new(),
            }],
            generated_slices: Vec::new(),
            progress: None,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: Default::default(),
            mission_envelope: None,
            frontier_budget: None,
            events: Vec::new(),
            economics: None,
            primary_terminal_reason: None,
            feed: None,
        };
        let feed = project_run_at(&details, now);
        assert_eq!(feed.feed_version, 1);
        assert_eq!(feed.blocks[0].label, "Run");
        assert_eq!(feed.blocks[1].label, "Mission");
        assert_eq!(feed.blocks[2].label, "Workers");
        assert_eq!(feed.blocks[3].label, "Attention");
        assert_eq!(feed.blocks[4].label, "Checks");
        assert_eq!(feed.blocks[5].label, "Economics");
        assert!(feed.blocks.iter().all(|block| block.label != "Commands"));
        assert!(feed.blocks.iter().all(|block| block.label != "Incidents"));
    }

    #[test]
    fn dashboard_v2_projection_orders_compact_sections_and_keeps_attention_full() {
        let now = Utc::now();
        let progress_at = now - chrono::Duration::seconds(5);
        let long_question = "choose a path with the full terminal reason and operator context intact so the renderer must not truncate this attention line".to_string();
        let noisy_profile =
            "implementer: provider=openai-codex model=gpt-5.5 reasoning=xhigh mode=fast"
                .to_string();
        let details = RunDetails {
            run: Run {
                id: "kd-test".to_string(),
                repo_id: "repo".to_string(),
                repo_path: "/tmp/repo".to_string(),
                status: RunStatus::Running,
                base_branch: "main".to_string(),
                base_sha: "base".to_string(),
                integration_branch: "khazad/kd-test/integration".to_string(),
                selected_slice_id: "slice-1".to_string(),
                error: String::new(),
                started_at: now - chrono::Duration::minutes(2),
                updated_at: now,
            },
            worker_profile: crate::domain::WorkerProfileEvidence {
                agent: "pi".to_string(),
                agent_profile: "implementer".to_string(),
                agent_provider: "openai-codex".to_string(),
                agent_model: "gpt-5.5".to_string(),
                agent_reasoning: "xhigh".to_string(),
                agent_mode: "fast".to_string(),
                profile_summary: noisy_profile.clone(),
                ..Default::default()
            },
            slice_runs: vec![SliceRun {
                run_id: "kd-test".to_string(),
                slice_id: "slice-1".to_string(),
                status: SliceStatus::Running,
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 1,
                last_error: String::new(),
            }],
            generated_slices: Vec::new(),
            progress: Some(RunProgress {
                run_id: "kd-test".to_string(),
                phase: "worker_running".to_string(),
                slice_id: "slice-1".to_string(),
                attempt: 1,
                command: "pi".to_string(),
                message: "slice worker is running".to_string(),
                output_tail: String::new(),
                phase_started_at: now - chrono::Duration::minutes(1),
                updated_at: now,
                worker: Some(WorkerAttemptProgress {
                    attempt_started_at: now - chrono::Duration::minutes(1),
                    pid: Some(123),
                    process_observed_at: Some(now - chrono::Duration::seconds(1)),
                    last_event_at: Some(now - chrono::Duration::seconds(1)),
                    last_event_kind: "stdout".to_string(),
                    last_semantic_progress_at: Some(progress_at),
                    last_semantic_progress_summary: "tool read finished".to_string(),
                    attempt_timeout_seconds: 0,
                    no_output_warning_seconds: 0,
                }),
                parallel_layer: false,
                parallel_slices: Vec::new(),
            }),
            incidents: Vec::new(),
            questions: vec![WorkerQuestion {
                id: "q-1".to_string(),
                run_id: "kd-test".to_string(),
                slice_id: "slice-1".to_string(),
                attempt: 1,
                question: long_question.clone(),
                options: Vec::new(),
                timeout_seconds: 1800,
                state: "pending".to_string(),
                asked_at: now,
                answered_at: None,
                answer: String::new(),
            }],
            replan: Default::default(),
            mission_envelope: None,
            frontier_budget: None,
            events: vec![Event {
                id: 1,
                run_id: "kd-test".to_string(),
                typ: "implementation_summary".to_string(),
                payload: serde_json::json!({
                    "verify_profile": "full",
                    "integration_gate": {
                        "status": "passed",
                        "summary": "integration gate passed",
                        "commands": [],
                        "findings": []
                    }
                }),
                created_at: now,
            }],
            economics: Some(RunEconomics::default()),
            primary_terminal_reason: None,
            feed: None,
        };

        let feed = project_run_at(&details, now);
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/projection_information_feed_golden.json"
        ))
        .expect("projection golden fixture");
        let labels = feed
            .blocks
            .iter()
            .map(|block| block.label.as_str())
            .collect::<Vec<_>>();
        for (index, expected) in golden["required_block_order"]
            .as_array()
            .expect("required block order")
            .iter()
            .enumerate()
        {
            assert_eq!(labels[index], expected.as_str().expect("block label"));
        }
        for forbidden in golden["forbidden_block_labels"]
            .as_array()
            .expect("forbidden block labels")
        {
            let forbidden = forbidden.as_str().expect("forbidden block label");
            assert!(labels.iter().all(|label| *label != forbidden), "{labels:?}");
        }

        let run = block_by_label(&feed, "Run");
        let run_text = run
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(run_text.contains("profile pi/implementer • openai-codex/gpt-5.5 • xhigh/fast"));
        assert!(!run_text.contains(&noisy_profile));

        let workers = block_by_label(&feed, "Workers");
        assert!(
            workers
                .lines
                .iter()
                .any(|line| line.text.contains("active slice-1"))
        );
        assert!(
            workers
                .lines
                .iter()
                .any(|line| line.text.contains("slice-1"))
        );

        let attention = block_by_label(&feed, "Attention");
        let question_line = format!("Question: {long_question}");
        assert!(
            attention
                .lines
                .iter()
                .any(|line| line.text == question_line)
        );
        assert!(feed.attention.iter().any(|line| line.text == question_line));

        let checks = block_by_label(&feed, "Checks");
        assert!(checks.lines.iter().any(|line| {
            line.text.contains(
                golden["semantic_progress_substring"]
                    .as_str()
                    .expect("semantic progress substring"),
            )
        }));
        assert!(
            checks
                .lines
                .iter()
                .any(|line| line.text.contains("gate passed"))
        );

        let economics = block_by_label(&feed, "Economics");
        assert_eq!(economics.meta, "active");
        assert!(
            economics.lines[0].text.contains(
                golden["economics_substring"]
                    .as_str()
                    .expect("economics substring")
            )
        );
        assert!(!economics.lines[0].text.contains("Agent calls:"));
    }

    fn block_by_label<'a>(feed: &'a StatusFeed, label: &str) -> &'a StatusFeedBlock {
        feed.blocks
            .iter()
            .find(|block| block.label == label)
            .unwrap_or_else(|| panic!("missing block {label}"))
    }

    #[test]
    fn terminal_reason_projection_carries_reason_and_operator_commands() {
        let now = Utc::now();
        let mut details = RunDetails {
            run: Run {
                id: "kd-test".to_string(),
                repo_id: "repo".to_string(),
                repo_path: "/tmp/repo".to_string(),
                status: RunStatus::Blocked,
                base_branch: "main".to_string(),
                base_sha: "base".to_string(),
                integration_branch: "khazad/kd-test/integration".to_string(),
                selected_slice_id: "slice-1".to_string(),
                error: "Pi login required".to_string(),
                started_at: now,
                updated_at: now,
            },
            worker_profile: Default::default(),
            slice_runs: Vec::new(),
            generated_slices: Vec::new(),
            progress: None,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: Default::default(),
            mission_envelope: None,
            frontier_budget: None,
            events: Vec::new(),
            economics: None,
            primary_terminal_reason: Some(TerminalReason {
                kind: "agent_auth_required".to_string(),
                resolution_owner: "operator".to_string(),
                retryable: false,
                operator_action_required: true,
                summary: "Pi login required".to_string(),
                evidence_links: vec!["event:7:run_incident".to_string()],
                remediation: "run pi /login".to_string(),
                disposition: "blocked; handoff is not ready".to_string(),
                operator_commands: vec![
                    "pi /login".to_string(),
                    "khazad-doom resume --run kd-test".to_string(),
                ],
            }),
            feed: None,
        };
        let feed = project_run_at(&details, now);
        details.feed = Some(feed.clone());

        assert_eq!(feed.feed_version, 1);
        assert_eq!(
            feed.terminal_reason.as_ref().unwrap().kind,
            "agent_auth_required"
        );
        assert!(
            feed.operator_commands
                .iter()
                .any(|command| command == "pi /login")
        );
        assert!(feed.blocks.iter().all(|block| block.label != "Terminal"));
        let attention = feed
            .blocks
            .iter()
            .find(|block| block.label == "Attention")
            .expect("attention block");
        assert!(
            attention
                .lines
                .iter()
                .any(|line| line.text.contains("owner=operator"))
        );
        assert!(
            attention
                .lines
                .iter()
                .any(|line| line.text == "Summary: Pi login required")
        );
        assert!(
            attention
                .lines
                .iter()
                .any(|line| line.text.contains("event:7:run_incident"))
        );
        assert!(feed.blocks.iter().any(|block| block.label == "Commands"));
    }

    #[test]
    fn replan_projection_renders_pending_attention_and_commands() {
        let now = Utc::now();
        let pending = ReplanProposal {
            id: "rp-test-001".to_string(),
            run_id: "kd-test".to_string(),
            state: ReplanProposalState::Pending,
            source: ReplanProposalSource {
                kind: "worker".to_string(),
                slice_id: "slice-1".to_string(),
                phase: "blocked".to_string(),
                attempt: 2,
                summary: "worker proposed follow-up".to_string(),
            },
            trigger_finding_ids: vec!["finding-1".to_string()],
            evidence: Vec::new(),
            proposed_changes: vec![ReplanProposedChange {
                kind: "add_followup_slice".to_string(),
                target: "slice-1-followup".to_string(),
                summary: "repair needs out-of-area files".to_string(),
            }],
            risk: "intent_affecting".to_string(),
            operator_decision: None,
            created_at: now,
            updated_at: now,
            decision_commands: replan_decision_commands("kd-test", "rp-test-001"),
        };
        let decided = ReplanProposal {
            id: "rp-test-000".to_string(),
            run_id: "kd-test".to_string(),
            state: ReplanProposalState::Rejected,
            operator_decision: Some(crate::domain::ReplanDecision {
                decision: "rejected".to_string(),
                rationale: "duplicate".to_string(),
                authorizer: "operator".to_string(),
                source: "cli".to_string(),
                decided_at: now,
                applied: false,
                applied_at: None,
                apply_status: "not_applicable".to_string(),
                apply_reason: "rejected proposal is not applied".to_string(),
                generated_slice_id: String::new(),
                generated_slice_commit: String::new(),
                apply_before_checkpoint_id: String::new(),
                apply_after_checkpoint_id: String::new(),
                queue_before: Vec::new(),
                queue_after: Vec::new(),
                queue_before_hash: String::new(),
                queue_after_hash: String::new(),
                replacement_id: String::new(),
                revisit_condition: String::new(),
            }),
            decision_commands: Vec::new(),
            ..pending.clone()
        };
        let details = RunDetails {
            run: Run {
                id: "kd-test".to_string(),
                repo_id: "repo".to_string(),
                repo_path: "/tmp/repo".to_string(),
                status: RunStatus::Running,
                base_branch: "main".to_string(),
                base_sha: "base".to_string(),
                integration_branch: "khazad/kd-test/integration".to_string(),
                selected_slice_id: "slice-1".to_string(),
                error: String::new(),
                started_at: now,
                updated_at: now,
            },
            worker_profile: Default::default(),
            slice_runs: Vec::new(),
            generated_slices: Vec::new(),
            progress: None,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: ReplanStatus {
                pending_attention_reason: "awaiting replan decision for rp-test-001".to_string(),
                pending: vec![pending],
                history: vec![decided],
                auto_approvable: Vec::new(),
            },
            mission_envelope: None,
            frontier_budget: None,
            events: Vec::new(),
            economics: None,
            primary_terminal_reason: None,
            feed: None,
        };

        let feed = project_run_at(&details, now);
        assert!(
            feed.attention
                .iter()
                .any(|line| { line.text.contains("Pending replan rp-test-001") })
        );
        assert!(feed.operator_commands.iter().any(|command| {
            command == "khazad-doom replan accept kd-test rp-test-001 --reason <reason>"
        }));
        assert!(feed.blocks.iter().all(|block| block.label != "Replan"));
        let attention = feed
            .blocks
            .iter()
            .find(|block| block.label == "Attention")
            .expect("attention block");
        assert!(attention.lines.iter().any(|line| {
            line.text.contains("Pending replan rp-test-001")
                && line.text.contains("risk=intent_affecting")
        }));
        assert!(attention.lines.iter().any(|line| {
            line.text
                .contains("Proposed change: add_followup_slice:slice-1-followup")
        }));
        assert!(feed.blocks.iter().any(|block| block.label == "Commands"));
    }
}
