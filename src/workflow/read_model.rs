use super::projection::project_run;
use crate::artifact;
use crate::domain::{
    Event, FrontierProposalOutcome, FrontierSummary, GeneratedSliceRecord, ImplementationSummary,
    PlanRevisionDecisionSummary, PlanRevisionRecord, PlanRevisions, ReplanProposal,
    ReplanProposalState, ReplanStatus, Run, RunDetails, RunEconomics, RunIncident, RunProgress,
    RunStatus, SliceRun, SliceStatus, TerminalReason, WorkerProfileEvidence, WorkerQuestion,
    frontier_classification_annotation, frontier_classification_would_auto_promote,
    replan_decision_commands,
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
        let generated_slices = generated_slices_from_proposals(&proposals, &slice_runs);
        let replan = replan_status_from_proposals(&run_id, proposals.clone());
        let (mission_envelope, frontier_budget) = self.state.get_frontier_state(&run_id)?;
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
            generated_slices,
            progress,
            incidents,
            questions,
            replan,
            mission_envelope,
            frontier_budget,
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

fn generated_slices_from_proposals(
    proposals: &[ReplanProposal],
    slice_runs: &[SliceRun],
) -> Vec<GeneratedSliceRecord> {
    let mut records = Vec::new();
    for proposal in proposals {
        let Some(decision) = proposal.operator_decision.as_ref() else {
            continue;
        };
        if decision.generated_slice_id.trim().is_empty() {
            continue;
        }
        let slice_run = slice_runs
            .iter()
            .find(|slice_run| slice_run.slice_id == decision.generated_slice_id);
        records.push(GeneratedSliceRecord {
            slice_id: decision.generated_slice_id.clone(),
            parent_slice_id: proposal.source.slice_id.clone(),
            origin_proposal_id: proposal.id.clone(),
            generation: proposal
                .proposed_changes
                .iter()
                .find_map(|change| change.followup_slice_draft())
                .and_then(|draft| (!draft.id.trim().is_empty()).then_some(1))
                .unwrap_or(0),
            status: slice_run
                .map(|slice_run| slice_run.status.as_str().to_string())
                .unwrap_or_else(|| decision.apply_status.clone()),
            commit_sha: if decision.generated_slice_commit.trim().is_empty() {
                slice_run
                    .map(|slice_run| slice_run.commit_sha.clone())
                    .unwrap_or_default()
            } else {
                decision.generated_slice_commit.clone()
            },
            applied_at: decision.applied_at,
        });
    }
    records
}

pub(crate) fn replan_status_from_proposals(
    run_id: &str,
    proposals: Vec<ReplanProposal>,
) -> ReplanStatus {
    let mut pending = Vec::new();
    let mut history = Vec::new();
    let mut auto_approvable = Vec::new();
    for proposal in proposals {
        let proposal = enrich_replan_proposal(run_id, proposal);
        if proposal.state == ReplanProposalState::Pending {
            if proposal
                .frontier_classification
                .as_ref()
                .is_some_and(|classification| {
                    classification.tier == "tier_1"
                        && matches!(
                            classification.autonomy_level,
                            crate::domain::AutonomyLevel::Promote
                                | crate::domain::AutonomyLevel::Run
                        )
                })
            {
                auto_approvable.push(proposal.clone());
            }
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
        auto_approvable,
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
    let frontier = frontier_summary_from_proposals(&proposals);
    let mut plan = PlanRevisions {
        source_of_truth: "daemon_replan_proposals".to_string(),
        queue_summary: if run.selected_slice_id.trim().is_empty() {
            "selected slices: <none>".to_string()
        } else {
            format!("selected slices: {}", run.selected_slice_id)
        },
        frontier,
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

pub(crate) fn frontier_summary_from_proposals(proposals: &[ReplanProposal]) -> FrontierSummary {
    let mut summary = FrontierSummary::default();
    for proposal in proposals {
        let Some(classification) = &proposal.frontier_classification else {
            continue;
        };
        summary.candidates_seen += 1;
        *summary
            .tier_distribution
            .entry(classification.tier.clone())
            .or_insert(0) += 1;
        if frontier_classification_would_auto_promote(classification) {
            let outcome = frontier_operator_outcome(proposal);
            match outcome.as_str() {
                "accepted_unchanged" => summary.agreement.accepted_unchanged += 1,
                "accepted_modified" => summary.agreement.accepted_modified += 1,
                "rejected" => summary.agreement.rejected += 1,
                "deferred" => summary.agreement.deferred += 1,
                "pending" => summary.agreement.pending += 1,
                _ => summary.agreement.pending += 1,
            }
            summary.would_have_promoted.push(FrontierProposalOutcome {
                proposal_id: proposal.id.clone(),
                tier: classification.tier.clone(),
                reason_codes: classification.reason_codes.clone(),
                operator_outcome: outcome,
                classified_at: classification.classified_at.to_rfc3339(),
                envelope_hash: classification.envelope_hash.clone(),
                annotation: frontier_classification_annotation(classification),
            });
        }
    }
    if summary.candidates_seen > 0 {
        summary.agreement.tier1_total = summary.would_have_promoted.len();
        summary.agreement.agreement_numerator = summary.agreement.accepted_unchanged;
        summary.agreement.agreement_denominator = summary.agreement.accepted_unchanged
            + summary.agreement.accepted_modified
            + summary.agreement.rejected
            + summary.agreement.deferred;
        summary.agreement.agreement_ratio = format!(
            "{}/{}",
            summary.agreement.agreement_numerator, summary.agreement.agreement_denominator
        );
        summary.agreement.agreement_percent = if summary.agreement.agreement_denominator == 0 {
            0.0
        } else {
            (summary.agreement.agreement_numerator as f64
                / summary.agreement.agreement_denominator as f64)
                * 100.0
        };
        let agreement = if summary.agreement.agreement_denominator == 0 {
            "n/a".to_string()
        } else {
            format!("{:.0}%", summary.agreement.agreement_percent)
        };
        summary.summary_line = format!(
            "frontier shadow observations: candidates_seen={}, tier_1_would_promote={}, agreement={} ({agreement})",
            summary.candidates_seen,
            summary.agreement.tier1_total,
            summary.agreement.agreement_ratio
        );
    }
    summary
}

fn frontier_operator_outcome(proposal: &ReplanProposal) -> String {
    match proposal.state {
        ReplanProposalState::Pending => "pending".to_string(),
        ReplanProposalState::Accepted => {
            let replacement = proposal
                .operator_decision
                .as_ref()
                .map(|decision| !decision.replacement_id.trim().is_empty())
                .unwrap_or(false);
            if replacement {
                "accepted_modified".to_string()
            } else {
                "accepted_unchanged".to_string()
            }
        }
        ReplanProposalState::Rejected => "rejected".to_string(),
        ReplanProposalState::Deferred => "deferred".to_string(),
        ReplanProposalState::Superseded => "accepted_modified".to_string(),
    }
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
        frontier_classification: proposal.frontier_classification.clone(),
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
        let checkpoint = if decision.apply_after_checkpoint_id.trim().is_empty() {
            decision
                .applied_at
                .map(|applied_at| format!("applied_at:{applied_at}"))
                .unwrap_or_else(|| "applied_without_timestamp".to_string())
        } else {
            format!(
                "applied_at_checkpoint:{}",
                decision.apply_after_checkpoint_id
            )
        };
        if decision.queue_after_hash.trim().is_empty() {
            checkpoint
        } else {
            format!(
                "{checkpoint}; queue_after_hash:{}",
                decision.queue_after_hash
            )
        }
    } else if decision.apply_status == "refused" {
        format!(
            "apply_refused:{}; remediation: supersede with a valid follow-up proposal or start a new run; reason: {}",
            decision.decided_at,
            display_or_dash(&decision.apply_reason)
        )
    } else if decision.apply_status == "incomplete" {
        format!(
            "replan_apply_incomplete:{}; remediation: resume the run to retry idempotent apply or inspect the generated slice evidence; reason: {}",
            decision.decided_at,
            display_or_dash(&decision.apply_reason)
        )
    } else if decision.apply_status == "pending" {
        format!(
            "accepted_pending_apply:{}; remediation: resume the run so the daemon can apply at the next checkpoint",
            decision.decided_at
        )
    } else {
        format!(
            "not_applied:{}; proposal-only or non-applicable decision left queue/slice state unchanged",
            decision.decided_at
        )
    };
    Ok(PlanRevisionDecisionSummary {
        decision: decision.decision,
        rationale: decision.rationale,
        authorizer: decision.authorizer,
        source: decision.source,
        decided_at: decision.decided_at,
        frontier_tier: decision.frontier_tier,
        frontier_reason_codes: decision.frontier_reason_codes,
        frontier_budget_before: decision.frontier_budget_before,
        frontier_budget_after: decision.frontier_budget_after,
        applied: decision.applied,
        applied_at: decision.applied_at,
        applied_at_checkpoint,
        apply_status: decision.apply_status,
        apply_reason: decision.apply_reason,
        generated_slice_id: decision.generated_slice_id,
        generated_slice_commit: decision.generated_slice_commit,
        apply_before_checkpoint_id: decision.apply_before_checkpoint_id,
        apply_after_checkpoint_id: decision.apply_after_checkpoint_id,
        queue_before: decision.queue_before,
        queue_after: decision.queue_after,
        queue_before_hash: decision.queue_before_hash,
        queue_after_hash: decision.queue_after_hash,
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
            "{} proposal applied by {} at {}; generated_slice={}; queue_after_hash={}; changes: {}",
            proposal.state,
            decision.authorizer,
            if decision.apply_after_checkpoint_id.trim().is_empty() {
                decision
                    .applied_at
                    .map(|applied_at| applied_at.to_rfc3339())
                    .unwrap_or_else(|| "unknown checkpoint".to_string())
            } else {
                decision.apply_after_checkpoint_id.clone()
            },
            display_or_dash(&decision.generated_slice_id),
            display_or_dash(&decision.queue_after_hash),
            proposed_change_summary(&proposal.proposed_changes)
        ),
        Some(decision) if decision.decision == "accepted" && decision.apply_status == "refused" => format!(
            "accepted by {} but apply_refused; remediation: supersede with a valid follow-up proposal or start a new run; reason: {}; proposed changes remain: {}",
            decision.authorizer,
            display_or_dash(&decision.apply_reason),
            proposed_change_summary(&proposal.proposed_changes)
        ),
        Some(decision) if decision.decision == "accepted" && decision.apply_status == "incomplete" => format!(
            "accepted by {} but replan_apply_incomplete; remediation: resume the run to retry idempotent apply or inspect generated slice evidence; reason: {}; proposed changes remain: {}",
            decision.authorizer,
            display_or_dash(&decision.apply_reason),
            proposed_change_summary(&proposal.proposed_changes)
        ),
        Some(decision) if decision.decision == "accepted" && decision.apply_status == "pending" => format!(
            "accepted by {}; accepted_pending_apply at next daemon checkpoint; proposed changes remain: {}",
            decision.authorizer,
            proposed_change_summary(&proposal.proposed_changes)
        ),
        Some(decision) if decision.decision == "accepted" => format!(
            "accepted by {}; apply_status={}; no queue/slice mutation was applied; proposed changes remain: {}",
            decision.authorizer,
            display_or_dash(&decision.apply_status),
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
            if let Some(draft) = change.followup_slice_draft() {
                let areas = if draft.areas.is_empty() {
                    "<none>".to_string()
                } else {
                    draft.areas.join(",")
                };
                format!(
                    "{}:{}:{} (draft title: {}; goal: {}; areas=[{}]; acceptance={}; verify={})",
                    change.kind,
                    target,
                    change.summary_text(),
                    display_or_dash(&draft.title),
                    display_or_dash(&draft.goal),
                    areas,
                    draft.acceptance.len(),
                    draft.verify.len()
                )
            } else {
                format!("{}:{}:{}", change.kind, target, change.summary_text())
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn display_or_dash(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() { "-" } else { trimmed }
}
