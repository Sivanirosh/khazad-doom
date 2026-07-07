use crate::domain::{
    Event, ReplanProposalState, RunDetails, RunEconomics, RunIncident, RunProgress, RunStatus,
    SliceRun, SliceStatus, StatusFeed, StatusFeedBlock, StatusFeedLine, StatusFeedRole,
    TerminalReason, WorkerAttemptProgress,
};
use chrono::{DateTime, Utc};
use std::time::Duration;

const FEED_VERSION: u64 = 1;
const ACTIVITY_LIMIT: usize = 7;
const OUTPUT_LINES: usize = 4;
const TODO_ITEMS: usize = 8;
const LINE_WIDTH: usize = 180;

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
    let mut blocks = Vec::new();
    blocks.push(run_block(details, now));
    blocks.push(todos_block(details));
    if let Some(reason) = &details.primary_terminal_reason {
        blocks.push(terminal_reason_block(reason));
    }
    if let Some(replan) = replan_block(details) {
        blocks.push(replan);
    }
    if let Some(progress) = &details.progress
        && !should_hide_current_progress(details, progress)
    {
        blocks.push(progress_block(details, progress, now));
        if let Some(worker) = &progress.worker
            && let Some(warning) = worker_quiet_warning(worker, now)
        {
            blocks.push(block(
                "Warn",
                "",
                vec![
                    line(warning, StatusFeedRole::Warning),
                    line("wait, inspect, or cancel explicitly", StatusFeedRole::Dim),
                ],
            ));
        }
    }
    blocks.push(semantic_progress_block(details, now));
    if let Some(activity) = activity_block(details) {
        blocks.push(activity);
    }
    if let Some(tail) = tail_block(details) {
        blocks.push(tail);
    }
    if let Some(economics) = &details.economics {
        blocks.push(economics_block(details, economics));
    }
    if !details.incidents.is_empty() {
        blocks.push(incidents_block(&details.incidents));
    }

    let question_commands = pending_question_commands(details);
    let replan_commands = pending_replan_commands(details);
    let mut attention = pending_question_attention(details, now);
    attention.extend(replan_attention(details));
    let operator_commands = operator_commands(details, &question_commands, &replan_commands);
    if !attention.is_empty() {
        blocks.insert(0, block("Attention", "", attention.clone()));
    }
    if !operator_commands.is_empty() {
        let index = if attention.is_empty() { 0 } else { 1 };
        blocks.insert(index, commands_block(&operator_commands));
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

fn terminal_reason_block(reason: &TerminalReason) -> StatusFeedBlock {
    let mut lines = vec![
        line(
            format!(
                "kind={} • owner={} • retryable={} • operator_action_required={}",
                display_or_dash(&reason.kind),
                display_or_dash(&reason.resolution_owner),
                reason.retryable,
                reason.operator_action_required
            ),
            if reason.operator_action_required {
                StatusFeedRole::Attention
            } else {
                StatusFeedRole::Warning
            },
        ),
        line(
            truncate_display(&reason.summary, LINE_WIDTH),
            StatusFeedRole::Warning,
        ),
    ];
    if !reason.remediation.trim().is_empty() {
        lines.push(line(
            format!(
                "Remediation: {}",
                truncate_display(&reason.remediation, LINE_WIDTH)
            ),
            StatusFeedRole::Info,
        ));
    }
    if !reason.disposition.trim().is_empty() {
        lines.push(line(
            format!(
                "Disposition: {}",
                truncate_display(&reason.disposition, LINE_WIDTH)
            ),
            StatusFeedRole::Dim,
        ));
    }
    if !reason.evidence_links.is_empty() {
        lines.push(line(
            format!("Evidence: {}", reason.evidence_links.join(", ")),
            StatusFeedRole::Dim,
        ));
    }
    block("Terminal", display_or_dash(&reason.kind), lines)
}

fn replan_block(details: &RunDetails) -> Option<StatusFeedBlock> {
    if details.replan.pending.is_empty() && details.replan.history.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    for proposal in &details.replan.pending {
        let changes = proposal
            .proposed_changes
            .iter()
            .map(|change| format!("{}:{}", change.kind, change.target))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(line(
            format!(
                "pending {} • {} • risk={} • {}",
                proposal.id,
                replan_source_label(proposal),
                display_or_dash(&proposal.risk),
                truncate_display(&changes, LINE_WIDTH)
            ),
            StatusFeedRole::Attention,
        ));
        for command in &proposal.decision_commands {
            lines.push(line(command.clone(), StatusFeedRole::Attention));
        }
    }
    for proposal in details.replan.history.iter().rev().take(5).rev() {
        let decision = proposal.operator_decision.as_ref();
        let rationale = decision
            .map(|decision| decision.rationale.as_str())
            .unwrap_or_default();
        lines.push(line(
            format!(
                "{} {} • {}{}",
                proposal.state,
                proposal.id,
                replan_source_label(proposal),
                if rationale.trim().is_empty() {
                    String::new()
                } else {
                    format!(" • {}", truncate_display(rationale, LINE_WIDTH))
                }
            ),
            match proposal.state {
                ReplanProposalState::Accepted => StatusFeedRole::Success,
                ReplanProposalState::Rejected
                | ReplanProposalState::Deferred
                | ReplanProposalState::Superseded => StatusFeedRole::Dim,
                ReplanProposalState::Pending => StatusFeedRole::Attention,
            },
        ));
    }
    Some(block(
        "Replan",
        format!(
            "({} pending, {} decided)",
            details.replan.pending.len(),
            details.replan.history.len()
        ),
        lines,
    ))
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
                format!(
                    "Proposed change: {}:{} — {}",
                    change.kind, change.target, change.summary
                ),
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
    lines
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

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn todos_block(details: &RunDetails) -> StatusFeedBlock {
    let items = selected_slice_items(details);
    let item_label = if items.len() == 1 { "item" } else { "items" };
    let mut lines = Vec::new();
    if items.is_empty() {
        lines.push(line("no selected slices recorded", StatusFeedRole::Dim));
    } else {
        for slice in items.iter().take(TODO_ITEMS) {
            lines.push(line(todo_line(slice), role_for_slice_status(slice.status)));
        }
        if items.len() > TODO_ITEMS {
            lines.push(line(
                format!("… {} more", items.len() - TODO_ITEMS),
                StatusFeedRole::Dim,
            ));
        }
    }
    block("Todos", format!("({} {item_label})", items.len()), lines)
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
    if !details.worker_profile.profile_summary.trim().is_empty() {
        lines.push(line(
            format!("worker profile {}", details.worker_profile.profile_summary),
            StatusFeedRole::Dim,
        ));
    }
    if details.worker_profile.worker_evidence_kind
        == "deterministic_test_double_not_real_pi_worker_evidence"
    {
        lines.push(line(
            details.worker_profile.worker_evidence_label.clone(),
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

fn progress_block(
    details: &RunDetails,
    progress: &RunProgress,
    now: DateTime<Utc>,
) -> StatusFeedBlock {
    if let Some(worker) = &progress.worker {
        let mut meta = Vec::new();
        let slice = monitor_slice_label(details);
        if slice != "-" {
            meta.push(slice);
        }
        if progress.attempt > 0 {
            meta.push(format!("attempt {}", progress.attempt));
        }
        meta.push("now".to_string());
        let mut lines = Vec::new();
        if progress.parallel_layer && !progress.parallel_slices.is_empty() {
            lines.push(line(
                format!("Parallel layer: {}", progress.parallel_slices.join(", ")),
                StatusFeedRole::Info,
            ));
        }
        lines.extend([
            line(
                format!("Supervisor: {}", supervisor_label(worker, now)),
                StatusFeedRole::Info,
            ),
            line(
                format!("Process: {}", worker_process_label(worker)),
                StatusFeedRole::Info,
            ),
            line(
                format!("Runtime: {}", since_time(worker.attempt_started_at, now)),
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
        ]);
        return block("Worker", format!("({})", meta.join(" • ")), lines);
    }

    let label = if !progress.command.trim().is_empty() {
        command_block_label(&progress.phase, &progress.command).to_string()
    } else {
        phase_label(&progress.phase).to_string()
    };
    let mut meta = Vec::new();
    if label == "Worker" && !progress.slice_id.trim().is_empty() {
        meta.push(progress.slice_id.clone());
    }
    if label == "Worker" && progress.attempt > 0 {
        meta.push(format!("attempt {}", progress.attempt));
    }
    if label != "Worker" && !progress.command.trim().is_empty() {
        meta.push(command_meta(&progress.command));
    }
    meta.push("now".to_string());
    let mut lines = Vec::new();
    if !progress.command.trim().is_empty() && (label != "Worker" || progress.command.trim() != "pi")
    {
        lines.push(line(
            truncate_display(&progress.command, LINE_WIDTH),
            StatusFeedRole::Dim,
        ));
    }
    if progress.parallel_layer && !progress.parallel_slices.is_empty() {
        lines.push(line(
            format!("Parallel layer: {}", progress.parallel_slices.join(", ")),
            StatusFeedRole::Info,
        ));
    } else if !progress.slice_id.trim().is_empty() {
        lines.push(line(
            format!("slice {}", progress.slice_id),
            StatusFeedRole::Info,
        ));
    } else {
        lines.push(line(monitor_slice_label(details), StatusFeedRole::Info));
    }
    if progress.phase_started_at != progress.updated_at {
        lines.push(line(
            format!("elapsed {}", since_time(progress.phase_started_at, now)),
            StatusFeedRole::Dim,
        ));
    }
    if !progress.message.trim().is_empty() {
        lines.push(line(
            truncate_display(&progress.message, LINE_WIDTH),
            StatusFeedRole::Info,
        ));
    }
    lines.push(line(
        format!("updated {} ago", since_time(progress.updated_at, now)),
        StatusFeedRole::Dim,
    ));
    block(label, format!("({})", meta.join(" • ")), lines)
}

fn semantic_progress_block(details: &RunDetails, now: DateTime<Utc>) -> StatusFeedBlock {
    let worker = details
        .progress
        .as_ref()
        .and_then(|progress| progress.worker.as_ref());
    let mut lines = vec![line(
        format!(
            "Last semantic progress: {}",
            last_semantic_progress_label(worker, now)
        ),
        if worker
            .and_then(|worker| worker.last_semantic_progress_at)
            .is_some()
        {
            StatusFeedRole::Info
        } else {
            StatusFeedRole::Dim
        },
    )];
    if let Some(worker) = worker {
        lines.push(line(
            format!(
                "Last worker event: {}",
                last_worker_event_label(worker, now)
            ),
            StatusFeedRole::Dim,
        ));
    }
    block("Progress", "semantic", lines)
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
                "Agent calls: {} | Commands: {} | Duplicates: {} | Cache: {}/{} hit/miss",
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
                "Repair: policy={} attempts={}/{} | Fail-fast: {}",
                economics.repair_policy,
                economics.repair_attempts,
                economics.repair_max_attempts,
                economics.gate_fail_fast
            ),
            StatusFeedRole::Info,
        ),
    ];
    if let Some(fake_call) = economics.agent_calls.iter().find(|call| {
        call.worker_evidence_kind() == "deterministic_test_double_not_real_pi_worker_evidence"
    }) {
        lines.push(line(
            format!(
                "Worker evidence: {} ({})",
                fake_call.worker_evidence_kind(),
                fake_call.worker_evidence_label()
            ),
            StatusFeedRole::Warning,
        ));
    }
    if !economics.sla_violations.is_empty() {
        lines.push(line(
            format!("SLA violations: {}", economics.sla_violations.join("; ")),
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
        "worker_running" | "integration_repair" | "awaiting_operator"
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

fn activity_block(details: &RunDetails) -> Option<StatusFeedBlock> {
    let mut lines = Vec::new();
    for event in &details.events {
        let Some(text) = activity_line(event, details) else {
            continue;
        };
        if lines.last().is_some_and(|previous| previous == &text) {
            continue;
        }
        lines.push(text);
    }
    if lines.is_empty() {
        return None;
    }
    let visible = lines.iter().rev().take(ACTIVITY_LIMIT).collect::<Vec<_>>();
    Some(block(
        "Activity",
        format!("({} recent)", visible.len()),
        visible
            .into_iter()
            .rev()
            .map(|text| line(text.clone(), StatusFeedRole::Dim))
            .collect(),
    ))
}

fn tail_block(details: &RunDetails) -> Option<StatusFeedBlock> {
    let output_tail = details
        .progress
        .as_ref()
        .map(|progress| progress.output_tail.as_str())
        .unwrap_or_default();
    if output_tail.trim().is_empty() {
        return None;
    }
    let lines = output_tail
        .trim_end()
        .lines()
        .rev()
        .take(OUTPUT_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|text| line(truncate_display(text, LINE_WIDTH), StatusFeedRole::Dim))
        .collect();
    Some(block("Tail", "", lines))
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

fn activity_line(event: &Event, details: &RunDetails) -> Option<String> {
    let payload = event.payload.as_object();
    match event.typ.as_str() {
        "run_started" => {
            let selected = payload
                .and_then(|payload| payload.get("selected_slices"))
                .and_then(serde_json::Value::as_array)
                .map(|items| items.len())
                .unwrap_or_else(|| selected_slice_items(details).len());
            Some(format!(
                "Run (started): {selected} selected {}",
                if selected == 1 { "slice" } else { "slices" }
            ))
        }
        "slice_started" => Some(format!(
            "Worker ({}): slice worker started",
            payload_text(payload, "slice_id").unwrap_or_else(|| "-".to_string())
        )),
        "slice_merged" => {
            let slice_id = payload_text(payload, "slice_id").unwrap_or_else(|| "slice".to_string());
            let sha = payload_text(payload, "commit_sha")
                .filter(|sha| !sha.trim().is_empty())
                .map(|sha| format!(" • {}", short_sha(&sha)))
                .unwrap_or_default();
            Some(format!("Todos ({slice_id}): ☒ {slice_id}  merged{sha}"))
        }
        "integration_repair_completed" => {
            let status = payload_text(payload, "status").unwrap_or_else(|| "-".to_string());
            let summary = payload_text(payload, "summary")
                .unwrap_or_else(|| "integration repair completed".to_string());
            Some(format!("Repair ({status}): {summary}"))
        }
        "implementation_summary" => implementation_summary_line(payload),
        "run_completed" => Some("Run (completed): handoff artifacts are ready".to_string()),
        "worktrees_cleaned" => Some("Cleanup: worker worktrees cleaned".to_string()),
        "cockpit_ready" => cockpit_ready_line(payload),
        "cockpit_worker_ready" => cockpit_worker_ready_line(payload),
        "terminal_summary_written" => terminal_summary_line(payload),
        "terminal_notification_sent" => terminal_notification_line(payload, "sent"),
        "terminal_notification_skipped" => terminal_notification_line(payload, "skipped"),
        "checkpoint_written" => checkpoint_line(payload),
        "worker_question_asked" => Some(format!(
            "Attention: {}",
            payload_text(payload, "question")
                .unwrap_or_else(|| "worker question pending".to_string())
        )),
        "worker_question_answered" => {
            Some("Attention: operator answered worker question".to_string())
        }
        "progress" => progress_activity_line(event, payload),
        _ => {
            let summary = event_summary(event);
            if summary.is_empty() {
                Some(event_label(&event.typ))
            } else {
                Some(format!("{}: {summary}", event_label(&event.typ)))
            }
        }
    }
}

fn implementation_summary_line(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let mut parts = Vec::new();
    if let Some(completed) = payload
        .get("completed_slices")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
    {
        parts.push(format!(
            "{completed} completed {}",
            if completed == 1 { "slice" } else { "slices" }
        ));
    }
    if let Some(gate) = payload
        .get("integration_gate")
        .and_then(serde_json::Value::as_object)
    {
        if let Some(summary) = gate.get("summary").and_then(serde_json::Value::as_str) {
            if !summary.trim().is_empty() {
                parts.push(summary.to_string());
            }
        } else if let Some(status) = gate.get("status").and_then(serde_json::Value::as_str) {
            parts.push(format!("integration gate {status}"));
        }
    }
    if let Some(final_sha) = payload.get("final_sha").and_then(serde_json::Value::as_str)
        && !final_sha.trim().is_empty()
    {
        parts.push(format!("final {}", short_sha(final_sha)));
    }
    (!parts.is_empty()).then(|| format!("Summary: {}", parts.join(" • ")))
}

fn cockpit_ready_line(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let workspace = payload_text(Some(payload), "workspace")
        .or_else(|| payload_text(Some(payload), "workspace_label"))
        .unwrap_or_else(|| "workspace".to_string());
    let adapter = payload_text(Some(payload), "adapter").unwrap_or_else(|| "cockpit".to_string());
    Some(format!("Cockpit: {adapter} workspace ready ({workspace})"))
}

fn cockpit_worker_ready_line(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let slice_id = payload_text(Some(payload), "slice_id").unwrap_or_else(|| "slice".to_string());
    let attempt = payload_text(Some(payload), "attempt")
        .map(|attempt| format!(" attempt {attempt}"))
        .unwrap_or_default();
    Some(format!(
        "Cockpit: worker pane ready for {slice_id}{attempt}"
    ))
}

fn terminal_summary_line(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let name = payload_text(Some(payload), "path")
        .and_then(|path| path.rsplit('/').next().map(str::to_string))
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "run-summary.json".to_string());
    Some(format!("Terminal: summary written ({name})"))
}

fn terminal_notification_line(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
    verb: &str,
) -> Option<String> {
    let payload = payload?;
    let status = payload_text(Some(payload), "terminal_status")
        .or_else(|| payload_text(Some(payload), "status"))
        .unwrap_or_else(|| "terminal status".to_string());
    Some(format!("Terminal: notification {verb} for {status}"))
}

fn checkpoint_line(payload: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let payload = payload?;
    let completed = payload
        .get("completed_slices")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let remaining = payload
        .get("remaining_slices")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    Some(format!(
        "State: checkpoint written • {completed} done • {remaining} remaining"
    ))
}

fn progress_activity_line(
    event: &Event,
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let phase = payload_text(Some(payload), "phase").unwrap_or_else(|| "activity".to_string());
    if phase == "completed" {
        return None;
    }
    let label = if let Some(command) = payload_text(Some(payload), "command") {
        command_block_label(&phase, &command).to_string()
    } else {
        phase_label(&phase).to_string()
    };
    let mut meta = Vec::new();
    if let Some(slice_id) = payload_text(Some(payload), "slice_id")
        && !slice_id.trim().is_empty()
    {
        meta.push(slice_id);
    }
    if let Some(attempt) = payload.get("attempt").and_then(serde_json::Value::as_u64)
        && attempt > 0
    {
        meta.push(format!("attempt {attempt}"));
    }
    if label != "Worker"
        && let Some(command) = payload_text(Some(payload), "command")
    {
        meta.push(command_meta(&command));
    }
    let message = payload_text(Some(payload), "message")
        .unwrap_or_else(|| event_summary(event))
        .trim()
        .to_string();
    let summary = if message.is_empty() {
        phase.replace('_', " ")
    } else {
        message
    };
    Some(format!(
        "{}{}: {}",
        label,
        if meta.is_empty() {
            String::new()
        } else {
            format!(" ({})", meta.join(" • "))
        },
        truncate_display(&summary, LINE_WIDTH)
    ))
}

fn payload_text(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    payload
        .and_then(|payload| payload.get(key))
        .and_then(primitive_payload_text)
        .filter(|value| !value.trim().is_empty())
}

fn primitive_payload_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(number) = value.as_i64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_u64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_f64() {
        return Some(number.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Some(flag.to_string());
    }
    None
}

fn event_summary(event: &Event) -> String {
    let Some(map) = event.payload.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for key in [
        "slice_id", "phase", "status", "message", "summary", "error", "command",
    ] {
        let Some(value) = map.get(key) else { continue };
        let Some(text) = primitive_payload_text(value) else {
            continue;
        };
        if !text.trim().is_empty() {
            parts.push(format!("{key}={}", truncate_display(&text, 80)));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        truncate_display(&parts.join(" "), 160)
    }
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

fn phase_label(phase: &str) -> &'static str {
    let normalized = phase.to_ascii_lowercase();
    if normalized.starts_with("worker") || normalized == "awaiting_operator" {
        if normalized == "worker_verify" {
            "Shell"
        } else {
            "Worker"
        }
    } else if normalized.contains("gate") || normalized.contains("setup") {
        "Shell"
    } else if normalized.contains("merge") {
        "Merge"
    } else if normalized.contains("repair") {
        "Repair"
    } else if normalized == "ready_to_merge" {
        "Todos"
    } else if matches!(
        normalized.as_str(),
        "completed" | "started" | "integration_setup"
    ) {
        "Run"
    } else {
        "Activity"
    }
}

fn command_block_label(phase: &str, command: &str) -> &'static str {
    let normalized = phase.to_ascii_lowercase();
    let text = command.to_ascii_lowercase();
    if normalized == "worker_running" || text == "pi" {
        "Worker"
    } else if normalized.contains("merge") || text.starts_with("git merge") {
        "Merge"
    } else if normalized.contains("repair") {
        "Repair"
    } else {
        "Shell"
    }
}

fn command_meta(command: &str) -> String {
    let mut text = command.trim().to_string();
    while let Some((prefix, rest)) = text.split_once(' ') {
        if is_env_assignment(prefix) {
            text = rest.trim_start().to_string();
        } else {
            break;
        }
    }
    truncate_display(if text.is_empty() { command } else { &text }, 34)
}

fn is_env_assignment(value: &str) -> bool {
    let Some((key, _value)) = value.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn event_label(value: &str) -> String {
    value
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    use crate::domain::{
        ReplanProposal, ReplanProposalSource, ReplanProposedChange, ReplanStatus, Run,
        WorkerQuestion, replan_decision_commands,
    };

    #[test]
    fn projection_has_versioned_blocks_and_raw_safe_roles() {
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
            progress: None,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: Default::default(),
            events: Vec::new(),
            economics: None,
            primary_terminal_reason: None,
            feed: None,
        };
        let feed = project_run_at(&details, now);
        assert_eq!(feed.feed_version, 1);
        assert_eq!(feed.blocks[0].label, "Run");
        assert_eq!(feed.blocks[1].label, "Todos");
        assert_eq!(feed.blocks[2].label, "Progress");
    }

    #[test]
    fn projection_information_orders_tiers_and_humanizes_activity() {
        let now = Utc::now();
        let progress_at = now - chrono::Duration::seconds(5);
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
            worker_profile: Default::default(),
            slice_runs: vec![SliceRun {
                run_id: "kd-test".to_string(),
                slice_id: "slice-1".to_string(),
                status: SliceStatus::Running,
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 1,
                last_error: String::new(),
            }],
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
                question: "choose a path".to_string(),
                options: Vec::new(),
                timeout_seconds: 1800,
                state: "pending".to_string(),
                asked_at: now,
                answered_at: None,
                answer: String::new(),
            }],
            replan: Default::default(),
            events: vec![
                Event {
                    id: 1,
                    run_id: "kd-test".to_string(),
                    typ: "cockpit_ready".to_string(),
                    payload: serde_json::json!({
                        "adapter": "herdr",
                        "workspace": "Khazad-Doom kd-test",
                        "panes": ["Run Status / Event Feed"]
                    }),
                    created_at: now,
                },
                Event {
                    id: 2,
                    run_id: "kd-test".to_string(),
                    typ: "terminal_summary_written".to_string(),
                    payload: serde_json::json!({"path": "/tmp/run-summary.json"}),
                    created_at: now,
                },
                Event {
                    id: 3,
                    run_id: "kd-test".to_string(),
                    typ: "opaque_event".to_string(),
                    payload: serde_json::json!({"nested": {"raw": true}}),
                    created_at: now,
                },
                Event {
                    id: 4,
                    run_id: "kd-test".to_string(),
                    typ: "progress".to_string(),
                    payload: serde_json::json!({
                        "phase": "worker_running",
                        "slice_id": "slice-1",
                        "attempt": 1,
                        "message": "slice worker is running"
                    }),
                    created_at: now,
                },
                Event {
                    id: 5,
                    run_id: "kd-test".to_string(),
                    typ: "progress".to_string(),
                    payload: serde_json::json!({
                        "phase": "worker_running",
                        "slice_id": "slice-1",
                        "attempt": 1,
                        "message": "slice worker is running"
                    }),
                    created_at: now,
                },
            ],
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
        assert!(position(&labels, "Run") < position(&labels, "Todos"));
        assert!(position(&labels, "Progress") < position(&labels, "Activity"));
        assert!(position(&labels, "Activity") < position(&labels, "Economics"));

        let progress = block_by_label(&feed, "Progress");
        assert!(progress.lines.iter().any(|line| {
            line.text.contains(
                golden["semantic_progress_substring"]
                    .as_str()
                    .expect("semantic progress substring"),
            )
        }));

        let activity = block_by_label(&feed, "Activity");
        let activity_text = activity
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        for forbidden in golden["forbidden_activity_substrings"]
            .as_array()
            .expect("forbidden activity substrings")
        {
            let forbidden = forbidden.as_str().expect("forbidden substring");
            assert!(activity_text.iter().all(|text| !text.contains(forbidden)));
        }
        for expected in golden["required_activity_substrings"]
            .as_array()
            .expect("required activity substrings")
        {
            let expected = expected.as_str().expect("activity substring");
            assert!(activity_text.iter().any(|text| text.contains(expected)));
        }
        assert!(!activity_text.windows(2).any(|pair| pair[0] == pair[1]));
        let dedup = golden["dedup_activity_substring"]
            .as_str()
            .expect("dedup substring");
        assert_eq!(
            activity_text
                .iter()
                .filter(|text| text.contains(dedup))
                .count(),
            1
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
        assert!(!economics.lines[0].text.contains("Agent calls: 0 |"));
    }

    fn position(labels: &[&str], label: &str) -> usize {
        labels
            .iter()
            .position(|value| *value == label)
            .unwrap_or_else(|| panic!("missing block {label}"))
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
            progress: None,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: Default::default(),
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
        let terminal = feed
            .blocks
            .iter()
            .find(|block| block.label == "Terminal")
            .expect("terminal block");
        assert_eq!(terminal.meta, "agent_auth_required");
        assert!(
            terminal
                .lines
                .iter()
                .any(|line| line.text.contains("owner=operator"))
        );
        assert!(feed.blocks.iter().any(|block| block.label == "Commands"));
    }

    #[test]
    fn replan_projection_renders_pending_and_decided_proposals() {
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
            progress: None,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: ReplanStatus {
                pending_attention_reason: "awaiting replan decision for rp-test-001".to_string(),
                pending: vec![pending],
                history: vec![decided],
                auto_approvable: Vec::new(),
            },
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
        let replan = feed
            .blocks
            .iter()
            .find(|block| block.label == "Replan")
            .expect("replan block");
        assert_eq!(replan.meta, "(1 pending, 1 decided)");
        assert!(replan.lines.iter().any(|line| {
            line.text.contains("pending rp-test-001") && line.text.contains("risk=intent_affecting")
        }));
        assert!(
            replan
                .lines
                .iter()
                .any(|line| line.text.contains("rejected rp-test-000"))
        );
    }
}
