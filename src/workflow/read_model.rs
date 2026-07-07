use super::projection::project_run;
use crate::artifact;
use crate::domain::{
    Event, ImplementationSummary, PlanRevisionDecisionSummary, PlanRevisionRecord, PlanRevisions,
    ReplanProposal, ReplanProposalState, ReplanStatus, Run, RunDetails, RunEconomics, RunIncident,
    RunProgress, RunStatus, SliceRun, SliceStatus, TerminalReason, WorkerProfileEvidence,
    WorkerQuestion, replan_decision_commands,
};
use crate::state::Store as StateStore;
use anyhow::Result;
use chrono::Utc;
use serde_json::json;

const DEFAULT_STATUS_EVENTS_LIMIT: usize = 50;
const TERMINAL_SUMMARY_EVENTS_LIMIT: usize = 500;

#[derive(Debug, Clone)]
pub(crate) struct RunReadModelOptions {
    events_limit: usize,
    terminal_override: Option<TerminalStatusOverride>,
}

impl RunReadModelOptions {
    pub(crate) fn status(events_limit: usize) -> Self {
        Self {
            events_limit,
            terminal_override: None,
        }
    }

    pub(crate) fn terminal_summary(status: RunStatus, message: impl Into<String>) -> Self {
        Self {
            events_limit: TERMINAL_SUMMARY_EVENTS_LIMIT,
            terminal_override: Some(TerminalStatusOverride {
                status,
                error: message.into(),
                include_run_summary_evidence: true,
            }),
        }
    }

    fn events_limit(&self) -> usize {
        if self.events_limit == 0 {
            DEFAULT_STATUS_EVENTS_LIMIT
        } else {
            self.events_limit
        }
    }

    fn include_run_summary_evidence(&self) -> bool {
        self.terminal_override
            .as_ref()
            .is_some_and(|terminal| terminal.include_run_summary_evidence)
    }
}

#[derive(Debug, Clone)]
struct TerminalStatusOverride {
    status: RunStatus,
    error: String,
    include_run_summary_evidence: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct RunReadModel {
    pub(crate) details: RunDetails,
    pub(crate) plan_revisions: PlanRevisions,
}

pub(crate) struct RunReadModelBuilder<'a> {
    state: &'a StateStore,
}

impl<'a> RunReadModelBuilder<'a> {
    pub(crate) fn new(state: &'a StateStore) -> Self {
        Self { state }
    }

    pub(crate) fn snapshot(&self, run: &Run, options: RunReadModelOptions) -> Result<RunReadModel> {
        let run = apply_terminal_override(run, &options);
        let run_id = run.id.clone();
        let slice_runs = self.state.get_slice_runs(&run_id)?;
        let mut progress = self.state.get_progress(&run_id)?;
        let events = self.state.get_events(&run_id, options.events_limit())?;
        if let Some(progress) = progress.as_mut() {
            annotate_parallel_progress(progress, &slice_runs, &events);
        }
        let economics = read_run_economics(&run).ok();
        let worker_profile = read_worker_profile(&run, economics.as_ref()).unwrap_or_default();
        let incident_events = self.state.get_incident_events(&run_id)?;
        let incidents = run_incidents_from_events(&incident_events);
        let questions = self
            .state
            .list_worker_questions(&run_id)
            .unwrap_or_default();
        let proposals = self.state.list_replan_proposals(&run_id)?;
        let replan = replan_status_from_proposals(&run_id, proposals.clone());
        let plan_revisions = plan_revisions_from_proposals(&run, proposals)?;
        let primary_terminal_reason = primary_terminal_reason_impl(
            &run,
            &slice_runs,
            progress.as_ref(),
            &events,
            &incident_events,
            &questions,
            options.include_run_summary_evidence(),
        );
        let mut details = RunDetails {
            worker_profile,
            slice_runs,
            progress,
            incidents,
            questions,
            replan,
            events,
            economics,
            primary_terminal_reason,
            feed: None,
            run,
        };
        details.feed = Some(project_run(&details));
        Ok(RunReadModel {
            details,
            plan_revisions,
        })
    }

    pub(crate) fn plan_revisions_for_run(&self, run: &Run) -> Result<PlanRevisions> {
        plan_revisions_from_proposals(run, self.state.list_replan_proposals(&run.id)?)
    }
}

fn apply_terminal_override(run: &Run, options: &RunReadModelOptions) -> Run {
    let mut run = run.clone();
    if let Some(terminal) = &options.terminal_override {
        run.status = terminal.status;
        run.error = terminal.error.clone();
        run.updated_at = Utc::now();
    }
    run
}

pub(crate) fn replan_status_from_proposals(
    run_id: &str,
    proposals: Vec<ReplanProposal>,
) -> ReplanStatus {
    let mut pending = Vec::new();
    let mut history = Vec::new();
    for proposal in proposals {
        let proposal = enrich_replan_proposal(run_id, proposal);
        if proposal.state == ReplanProposalState::Pending {
            pending.push(proposal);
        } else {
            history.push(proposal);
        }
    }
    let pending_attention_reason = if pending.is_empty() {
        String::new()
    } else {
        format!(
            "awaiting replan decision for {}",
            pending
                .iter()
                .map(|proposal| proposal.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    ReplanStatus {
        pending_attention_reason,
        pending,
        history,
        auto_approvable: Vec::new(),
    }
}

pub(crate) fn enrich_replan_proposal(run_id: &str, mut proposal: ReplanProposal) -> ReplanProposal {
    proposal.decision_commands = if proposal.state == ReplanProposalState::Pending {
        replan_decision_commands(run_id, &proposal.id)
    } else {
        Vec::new()
    };
    proposal
}

#[cfg(test)]
pub(crate) fn primary_terminal_reason(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
    recent_events: &[Event],
    incident_events: &[Event],
    questions: &[WorkerQuestion],
) -> Option<TerminalReason> {
    primary_terminal_reason_impl(
        run,
        slice_runs,
        progress,
        recent_events,
        incident_events,
        questions,
        false,
    )
}

fn primary_terminal_reason_impl(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
    recent_events: &[Event],
    incident_events: &[Event],
    questions: &[WorkerQuestion],
    include_run_summary_evidence: bool,
) -> Option<TerminalReason> {
    if !matches!(
        run.status,
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted
    ) {
        return None;
    }

    let source = terminal_reason_source(
        run,
        slice_runs,
        progress,
        recent_events,
        incident_events,
        include_run_summary_evidence,
    );
    let mut commands = Vec::new();
    for question in questions
        .iter()
        .filter(|question| question.state == "pending")
    {
        push_unique_command(
            &mut commands,
            format!(
                "khazad-doom answer {} {} <answer>",
                question.run_id, question.id
            ),
        );
    }
    for command in source.fix_commands {
        push_unique_command(&mut commands, command);
    }
    for command in terminal_inspection_commands(run) {
        push_unique_command(&mut commands, command);
    }

    Some(TerminalReason {
        kind: source.kind,
        resolution_owner: source.resolution_owner,
        retryable: source.retryable,
        operator_action_required: source.operator_action_required,
        summary: source.summary,
        evidence_links: source.evidence_links,
        remediation: source.remediation,
        disposition: source.disposition,
        operator_commands: commands,
    })
}

struct TerminalReasonSource {
    kind: String,
    resolution_owner: String,
    retryable: bool,
    operator_action_required: bool,
    summary: String,
    evidence_links: Vec<String>,
    remediation: String,
    disposition: String,
    fix_commands: Vec<String>,
}

fn terminal_reason_source(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
    recent_events: &[Event],
    incident_events: &[Event],
    include_run_summary_evidence: bool,
) -> TerminalReasonSource {
    if let Some(event) = terminal_incident_event(incident_events) {
        return terminal_reason_from_event(run, event, include_run_summary_evidence);
    }
    if let Some(event) = terminal_run_error_event(incident_events)
        .or_else(|| terminal_run_error_event(recent_events))
    {
        return terminal_reason_from_event(run, event, include_run_summary_evidence);
    }

    let summary = terminal_summary_text(run, slice_runs, progress);
    let kind = match run.status {
        RunStatus::Blocked => "blocked",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => "terminal",
    }
    .to_string();
    TerminalReasonSource {
        kind,
        resolution_owner: default_resolution_owner(run.status),
        retryable: default_retryable(run.status),
        operator_action_required: default_operator_action_required(run.status),
        summary,
        evidence_links: default_evidence_links(run, include_run_summary_evidence),
        remediation: default_remediation(run.status),
        disposition: default_disposition(run.status),
        fix_commands: Vec::new(),
    }
}

fn terminal_incident_event(events: &[Event]) -> Option<&Event> {
    events.iter().rev().find(|event| {
        event.typ == "run_incident"
            && (event.payload.get("failure_kind").is_some()
                || event.payload.get("operator_action_required").is_some()
                || payload_string(&event.payload, "severity") == Some("error".to_string()))
    })
}

fn terminal_run_error_event(events: &[Event]) -> Option<&Event> {
    events.iter().rev().find(|event| event.typ == "run_error")
}

fn terminal_reason_from_event(
    run: &Run,
    event: &Event,
    include_run_summary_evidence: bool,
) -> TerminalReasonSource {
    let payload = &event.payload;
    let kind = payload_string(payload, "failure_kind")
        .or_else(|| payload_string(payload, "kind"))
        .unwrap_or_else(|| match event.typ.as_str() {
            "run_error" => run.status.as_str().to_string(),
            other => other.to_string(),
        });
    let operator_action_required = payload
        .get("operator_action_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| default_operator_action_required(run.status));
    let retryable = payload
        .get("retryable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| default_retryable(run.status));
    let summary = payload_string(payload, "message")
        .or_else(|| payload_string(payload, "error"))
        .or_else(|| payload_string(payload, "summary"))
        .unwrap_or_else(|| fallback_run_error(run));
    let resolution_owner = payload_string(payload, "resolution_owner").unwrap_or_else(|| {
        if operator_action_required {
            "operator".to_string()
        } else {
            default_resolution_owner(run.status)
        }
    });
    let mut evidence_links = default_evidence_links(run, include_run_summary_evidence);
    push_unique_command(
        &mut evidence_links,
        format!("event:{}:{}", event.id, event.typ),
    );
    TerminalReasonSource {
        kind,
        resolution_owner,
        retryable,
        operator_action_required,
        summary,
        evidence_links,
        remediation: remediation_for(run.status, operator_action_required, retryable),
        disposition: default_disposition(run.status),
        fix_commands: string_array(payload, "fix_commands"),
    }
}

fn terminal_summary_text(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
) -> String {
    if !run.error.trim().is_empty() {
        return run.error.clone();
    }
    if let Some(slice_run) = slice_runs
        .iter()
        .find(|slice_run| !slice_run.last_error.trim().is_empty())
    {
        return slice_run.last_error.clone();
    }
    if let Some(progress) = progress
        && !progress.message.trim().is_empty()
    {
        return progress.message.clone();
    }
    fallback_run_error(run)
}

fn fallback_run_error(run: &Run) -> String {
    format!("run ended with status {}", run.status)
}

fn default_resolution_owner(status: RunStatus) -> String {
    match status {
        RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted => "operator",
        RunStatus::Failed => "daemon",
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => "daemon",
    }
    .to_string()
}

fn default_retryable(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted
    )
}

fn default_operator_action_required(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted
    )
}

fn default_remediation(status: RunStatus) -> String {
    remediation_for(
        status,
        default_operator_action_required(status),
        default_retryable(status),
    )
}

fn remediation_for(status: RunStatus, operator_action_required: bool, retryable: bool) -> String {
    if operator_action_required {
        return "complete the listed operator action, then resume the run".to_string();
    }
    if retryable {
        return "inspect artifacts, fix the underlying failure, then resume the run".to_string();
    }
    match status {
        RunStatus::Failed => "inspect artifacts and create a follow-up slice if needed".to_string(),
        _ => "inspect artifacts before taking further action".to_string(),
    }
}

fn default_disposition(status: RunStatus) -> String {
    match status {
        RunStatus::Blocked => "blocked; handoff is not ready until the operator action is resolved",
        RunStatus::Failed => "failed; handoff is not ready until the failure is resolved",
        RunStatus::Cancelled => "cancelled by request; handoff is not ready",
        RunStatus::Interrupted => "interrupted; resume from checkpoint before handoff",
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => {
            "terminal disposition unavailable"
        }
    }
    .to_string()
}

fn default_evidence_links(run: &Run, include_run_summary_evidence: bool) -> Vec<String> {
    let store = artifact::Store::new(&run.repo_path);
    let summary_path = store.output_path(&run.id, "run-summary.json");
    if include_run_summary_evidence || summary_path.exists() {
        vec![summary_path.to_string_lossy().to_string()]
    } else {
        Vec::new()
    }
}

fn terminal_inspection_commands(run: &Run) -> Vec<String> {
    match run.status {
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => {
            vec![
                format!("khazad-doom inspect --run {}", run.id),
                format!("khazad-doom resume --run {}", run.id),
            ]
        }
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => Vec::new(),
    }
}

fn payload_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn push_unique_command(commands: &mut Vec<String>, command: String) {
    if !command.trim().is_empty() && !commands.iter().any(|existing| existing == &command) {
        commands.push(command);
    }
}

fn run_incidents_from_events(events: &[Event]) -> Vec<RunIncident> {
    events
        .iter()
        .filter_map(|event| {
            let payload = &event.payload;
            let (severity, kind, message) = match event.typ.as_str() {
                "run_incident" => (
                    payload_text(payload, "severity", "warning"),
                    payload_text(payload, "kind", "run_incident"),
                    payload_text(payload, "message", "incident recorded"),
                ),
                "run_error" => (
                    "error".to_string(),
                    "run_error".to_string(),
                    payload_text(payload, "error", "run failed"),
                ),
                "run_resumed" => (
                    "warning".to_string(),
                    "run_resumed".to_string(),
                    "run resumed after a terminal/interrupted state".to_string(),
                ),
                "worktree_cleanup_error" | "daemon_recovery_cleanup_error" => (
                    "warning".to_string(),
                    event.typ.clone(),
                    payload_text(payload, "error", "worktree cleanup reported an error"),
                ),
                "integration_repair_completed" => (
                    "warning".to_string(),
                    "integration_repair_completed".to_string(),
                    [
                        payload_text(payload, "status", ""),
                        payload_text(payload, "summary", "integration repair completed"),
                    ]
                    .into_iter()
                    .filter(|part| !part.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
                ),
                _ => return None,
            };
            Some(RunIncident {
                severity,
                kind,
                message,
                event_id: event.id,
                created_at: event.created_at,
            })
        })
        .collect()
}

fn payload_text(payload: &serde_json::Value, field: &str, fallback: &str) -> String {
    payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn read_run_economics(run: &Run) -> Result<RunEconomics> {
    let store = artifact::Store::new(&run.repo_path);
    let live_path = store.output_path(&run.id, "economics.json");
    if live_path.exists()
        && let Ok(economics) = artifact::read_json(live_path)
    {
        return Ok(economics);
    }
    let summary: ImplementationSummary =
        artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
    Ok(summary.economics)
}

fn read_worker_profile(
    run: &Run,
    economics: Option<&RunEconomics>,
) -> Option<WorkerProfileEvidence> {
    let store = artifact::Store::new(&run.repo_path);
    if let Ok(summary) = artifact::read_json::<ImplementationSummary>(
        store.output_path(&run.id, "final-report.json"),
    ) && !summary.worker_profile.is_empty()
    {
        return Some(summary.worker_profile);
    }
    if let Ok(value) =
        artifact::read_json::<serde_json::Value>(store.output_path(&run.id, "preflight.json"))
        && let Some(profile) = WorkerProfileEvidence::from_json_surface(&value)
    {
        return Some(profile);
    }
    economics.and_then(|economics| {
        economics.agent_calls.iter().find_map(|call| {
            let value = json!({
                "agent": call.runner,
                "agent_profile": call.agent_profile,
                "agent_provider": call.agent_provider,
                "agent_model": call.agent_model,
                "agent_reasoning": call.agent_reasoning,
                "agent_mode": call.agent_mode,
                "profile_summary": call.profile_summary,
                "launch_summary": call.launch_summary,
                "worker_evidence_kind": call.worker_evidence_kind(),
                "worker_evidence_label": call.worker_evidence_label(),
            });
            WorkerProfileEvidence::from_json_surface(&value)
        })
    })
}

fn annotate_parallel_progress(
    progress: &mut RunProgress,
    slice_runs: &[SliceRun],
    events: &[Event],
) {
    if progress.phase == "parallel_worker_layer" && !progress.slice_id.trim().is_empty() {
        progress.parallel_layer = true;
        progress.parallel_slices = split_parallel_slice_ids(&progress.slice_id);
        return;
    }
    if !is_worker_layer_phase(&progress.phase) {
        return;
    }
    let active: Vec<_> = slice_runs
        .iter()
        .filter(|slice_run| is_parallel_layer_slice_status(slice_run.status))
        .map(|slice_run| slice_run.slice_id.clone())
        .collect();
    if active.len() > 1 {
        progress.parallel_layer = true;
        progress.parallel_slices = active;
        return;
    }
    if let Some(layer) = current_parallel_layer_from_events(events) {
        progress.parallel_layer = true;
        progress.parallel_slices = layer;
    }
}

fn is_worker_layer_phase(phase: &str) -> bool {
    matches!(
        phase,
        "worker_started" | "worker_running" | "worker_verify" | "ready_to_merge"
    )
}

fn current_parallel_layer_from_events(events: &[Event]) -> Option<Vec<String>> {
    for event in events.iter().rev() {
        match event.typ.as_str() {
            "parallel_layer_completed" | "parallel_layer_failed" => return None,
            "parallel_layer_started" => {
                let slices = event
                    .payload
                    .get("slices")
                    .and_then(serde_json::Value::as_array)?
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                return (slices.len() > 1).then_some(slices);
            }
            _ => {}
        }
    }
    None
}

fn split_parallel_slice_ids(slice_ids: &str) -> Vec<String> {
    slice_ids
        .split(',')
        .map(str::trim)
        .filter(|slice_id| !slice_id.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_parallel_layer_slice_status(status: SliceStatus) -> bool {
    !matches!(status, SliceStatus::Pending | SliceStatus::Merged)
}

pub(crate) fn plan_revisions_from_proposals(
    run: &Run,
    proposals: Vec<ReplanProposal>,
) -> Result<PlanRevisions> {
    let mut plan = PlanRevisions {
        source_of_truth: "daemon_replan_proposals".to_string(),
        queue_summary: if run.selected_slice_id.trim().is_empty() {
            "selected slices: <none>".to_string()
        } else {
            format!("selected slices: {}", run.selected_slice_id)
        },
        ..PlanRevisions::default()
    };
    for mut proposal in proposals {
        if proposal.state.as_str() == "pending" {
            proposal.decision_commands = replan_decision_commands(&run.id, &proposal.id);
        }
        let record = plan_revision_record(run, proposal)?;
        match record.state.as_str() {
            "pending" => plan.pending.push(record),
            "accepted" => plan.accepted.push(record),
            "rejected" => plan.rejected.push(record),
            "deferred" => plan.deferred.push(record),
            "superseded" => plan.superseded.push(record),
            _ => {}
        }
    }
    plan.unresolved_pending_blocks_handoff = !plan.pending.is_empty();
    Ok(plan)
}

fn plan_revision_record(run: &Run, proposal: ReplanProposal) -> Result<PlanRevisionRecord> {
    let state = proposal.state.as_str().to_string();
    let after = plan_revision_after_summary(&proposal);
    let decision = proposal
        .operator_decision
        .clone()
        .map(plan_revision_decision_summary)
        .transpose()?;
    let authorized_paths = authorized_paths_from_proposal(&proposal);
    Ok(PlanRevisionRecord {
        proposal_id: proposal.id.clone(),
        state,
        source: proposal.source.clone(),
        trigger_finding_ids: proposal.trigger_finding_ids.clone(),
        evidence: proposal.evidence.clone(),
        proposed_changes: proposal.proposed_changes.clone(),
        authorized_paths,
        action_class: plan_revision_action_class(&proposal),
        risk: proposal.risk.clone(),
        before_queue_or_slice_summary: plan_revision_before_summary(run, &proposal),
        after_queue_or_slice_summary: after,
        decision_commands: proposal.decision_commands.clone(),
        decision,
        created_at: proposal.created_at,
        updated_at: proposal.updated_at,
    })
}

pub(crate) fn authorized_paths_from_proposal(proposal: &ReplanProposal) -> Vec<String> {
    if !matches!(
        proposal
            .operator_decision
            .as_ref()
            .map(|decision| decision.decision.as_str()),
        Some("accepted")
    ) {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for change in &proposal.proposed_changes {
        let target = change.target.trim();
        if target.is_empty() || target == "integration" || target == proposal.source.slice_id {
            continue;
        }
        if target.contains('/') || target.contains('.') {
            paths.push(target.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn plan_revision_action_class(proposal: &ReplanProposal) -> String {
    proposal
        .proposed_changes
        .iter()
        .map(|change| change.kind.as_str())
        .find(|kind| !kind.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

fn plan_revision_decision_summary(
    decision: crate::domain::ReplanDecision,
) -> Result<PlanRevisionDecisionSummary> {
    let applied_at_checkpoint = if decision.applied {
        decision
            .applied_at
            .map(|applied_at| format!("applied_at:{applied_at}"))
            .unwrap_or_else(|| "applied_without_timestamp".to_string())
    } else {
        format!(
            "not_applied:{}; proposal-only replan v1 left queue/slice state unchanged",
            decision.decided_at
        )
    };
    Ok(PlanRevisionDecisionSummary {
        decision: decision.decision,
        rationale: decision.rationale,
        authorizer: decision.authorizer,
        source: decision.source,
        decided_at: decision.decided_at,
        applied: decision.applied,
        applied_at: decision.applied_at,
        applied_at_checkpoint,
        replacement_id: decision.replacement_id,
        revisit_condition: decision.revisit_condition,
    })
}

fn plan_revision_before_summary(run: &Run, proposal: &ReplanProposal) -> String {
    let queue = if run.selected_slice_id.trim().is_empty() {
        "<none>"
    } else {
        run.selected_slice_id.as_str()
    };
    format!(
        "queue before proposal {}: {}; proposed changes: {}",
        proposal.id,
        queue,
        proposed_change_summary(&proposal.proposed_changes)
    )
}

fn plan_revision_after_summary(proposal: &ReplanProposal) -> String {
    match proposal.operator_decision.as_ref() {
        Some(decision) if decision.applied => format!(
            "{} proposal applied by {} at {}; changes: {}",
            proposal.state,
            decision.authorizer,
            decision
                .applied_at
                .map(|applied_at| applied_at.to_rfc3339())
                .unwrap_or_else(|| "unknown checkpoint".to_string()),
            proposed_change_summary(&proposal.proposed_changes)
        ),
        Some(decision) if decision.decision == "accepted" => format!(
            "accepted by {}; proposal-only replan v1 records applied=false, so no queue/slice mutation was applied; proposed changes remain: {}",
            decision.authorizer,
            proposed_change_summary(&proposal.proposed_changes)
        ),
        Some(decision) if decision.decision == "rejected" => {
            format!(
                "rejected; queue/slice state unchanged; rationale: {}",
                decision.rationale
            )
        }
        Some(decision) if decision.decision == "deferred" => format!(
            "deferred; queue/slice state unchanged until revisit condition: {}",
            display_or_dash(&decision.revisit_condition)
        ),
        Some(decision) if decision.decision == "superseded" => format!(
            "superseded by {}; queue/slice state unchanged by this proposal",
            display_or_dash(&decision.replacement_id)
        ),
        Some(decision) => format!(
            "{} decision recorded; queue/slice state unchanged unless applied=true",
            decision.decision
        ),
        None => "pending; unresolved proposal blocks handoff readiness until an operator disposition records it as non-blocking or decided".to_string(),
    }
}

fn proposed_change_summary(changes: &[crate::domain::ReplanProposedChange]) -> String {
    if changes.is_empty() {
        return "<none>".to_string();
    }
    changes
        .iter()
        .map(|change| {
            let target = display_or_dash(&change.target);
            format!("{}:{}:{}", change.kind, target, change.summary)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn display_or_dash(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() { "-" } else { trimmed }
}
