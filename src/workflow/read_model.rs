use super::events::{EventKind, RunErrorPayload};
use super::projection::project_run;
use crate::artifact::CompletionPublicationReceipt;
use crate::domain::{
    Event, FrontierAuthorizerRecord, FrontierBudgetConsumption, FrontierBudgetState,
    FrontierFogRecord, FrontierGeneratedSliceEdge, FrontierOperatorStop, FrontierProposalOutcome,
    FrontierSummary, FrontierTierReasonRecord, GeneratedSliceRecord, ImplementationSummary,
    MissionEnvelope, PlanRevisionDecisionSummary, PlanRevisionRecord, PlanRevisions,
    ReplanProposal, ReplanProposalState, ReplanStatus, Run, RunDetails, RunEconomics, RunIncident,
    RunProgress, RunStatus, SliceRun, SliceStatus, StatusFeed, StatusSnapshotMetadata,
    StatusSourceMetadata, StatusTerminalizationProjection, TerminalReason, WorkerAttemptLedger,
    WorkerProfileEvidence, WorkerQuestion, frontier_classification_annotation,
    frontier_classification_would_auto_promote, replan_decision_commands,
};
use crate::state::{RunStateSnapshot, Store as StateStore};
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::{HashMap, HashSet};

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
    pub(crate) final_report: Option<ImplementationSummary>,
    pub(crate) run_summary: Option<serde_json::Value>,
    pub(crate) completion_publication: Option<CompletionPublicationReceipt>,
    pub(crate) semantic_events: Vec<Event>,
}

pub(crate) struct RunReadModelBuilder<'a> {
    state: &'a StateStore,
}

impl<'a> RunReadModelBuilder<'a> {
    pub(crate) fn new(state: &'a StateStore) -> Self {
        Self { state }
    }

    pub(crate) fn snapshot(
        &self,
        run_id: &str,
        options: RunReadModelOptions,
    ) -> Result<RunReadModel> {
        let snapshot = self
            .state
            .status_snapshot(run_id, options.events_limit())?
            .with_context(|| format!("run {run_id:?} not found"))?;
        self.build(snapshot, options)
    }

    pub(crate) fn latest_snapshot(
        &self,
        repo_path: &str,
        active_only: bool,
        options: RunReadModelOptions,
    ) -> Result<Option<RunReadModel>> {
        self.state
            .latest_status_snapshot(repo_path, active_only, options.events_limit())?
            .map(|snapshot| self.build(snapshot, options))
            .transpose()
    }

    fn build(
        &self,
        snapshot: RunStateSnapshot,
        options: RunReadModelOptions,
    ) -> Result<RunReadModel> {
        let run = apply_terminal_override(
            &snapshot.run,
            snapshot.terminal_transition.as_ref(),
            &options,
        )?;
        let run_id = run.id.clone();
        let economics = indexed_economics(&snapshot)?;
        let final_report = indexed_final_report(&snapshot)?;
        let run_summary = indexed_run_summary(&snapshot)?;
        let completion_publication = completion_publication(&snapshot.events)?;
        let worker_profile = read_worker_profile(&snapshot, economics.as_ref()).unwrap_or_default();
        let incidents = run_incidents_from_events(&snapshot.events);
        let semantic_events = snapshot.events.clone();
        let metadata = StatusSnapshotMetadata {
            revision: snapshot.revision.clone(),
            sources: status_source_metadata(&snapshot),
        };
        let terminalization = snapshot
            .terminal_transition
            .as_ref()
            .map(|transition| StatusTerminalizationProjection {
                state: if transition.committed {
                    "committed".to_string()
                } else {
                    "intended".to_string()
                },
                target_status: transition.status.as_str().to_string(),
                summary_written: transition.summary_written,
                committed: transition.committed,
            })
            .unwrap_or_default();
        let selected_slice_ids = snapshot.selected_slice_ids;
        let slice_runs = snapshot.slice_runs;
        let worker_attempts = worker_attempt_history(snapshot.worker_attempts, &run, &slice_runs);
        let mut progress = snapshot.progress;
        if let Some(progress) = progress.as_mut() {
            annotate_parallel_progress(progress, &slice_runs, &snapshot.events);
        }
        let questions = snapshot.questions;
        let proposals = snapshot.replan_proposals;
        let generated_slices = generated_slices_from_proposals(&proposals, &slice_runs);
        let replan = replan_status_from_proposals(&run_id, proposals.clone());
        let plan_revisions = plan_revisions_from_proposals(
            &run,
            &selected_slice_ids,
            snapshot.mission_envelope.as_ref(),
            snapshot.frontier_budget.as_ref(),
            proposals,
            &generated_slices,
        )?;
        let primary_terminal_reason = primary_terminal_reason_impl(
            &run,
            &slice_runs,
            progress.as_ref(),
            &snapshot.events,
            &snapshot.events,
            &questions,
            options.include_run_summary_evidence(),
        );
        let mut details = RunDetails {
            selected_slice_ids,
            snapshot: metadata,
            launch_intents: snapshot.launch_intents,
            integration_merge_intents: snapshot.merge_intents,
            terminalization,
            worker_profile,
            slice_runs,
            worker_attempts,
            generated_slices,
            progress,
            incidents,
            questions,
            replan,
            mission_envelope: snapshot.mission_envelope,
            frontier_budget: snapshot.frontier_budget,
            frontier: plan_revisions.frontier.clone(),
            events: snapshot.event_tail,
            economics,
            primary_terminal_reason,
            feed: None,
            run,
        };
        // Projection receives complete semantic history; only the independently
        // bounded `details.events` tail crosses the wire.
        let mut projection_details = details.clone();
        projection_details.events = semantic_events.clone();
        details.feed = Some(project_run(&projection_details));
        Ok(RunReadModel {
            details,
            plan_revisions,
            final_report,
            run_summary,
            completion_publication,
            semantic_events,
        })
    }

    pub(crate) fn plan_revisions_for_run(&self, run_id: &str) -> Result<PlanRevisions> {
        let snapshot = self
            .state
            .status_snapshot(run_id, DEFAULT_STATUS_EVENTS_LIMIT)?
            .with_context(|| format!("run {run_id:?} not found"))?;
        let generated_slices =
            generated_slices_from_proposals(&snapshot.replan_proposals, &snapshot.slice_runs);
        plan_revisions_from_proposals(
            &snapshot.run,
            &snapshot.selected_slice_ids,
            snapshot.mission_envelope.as_ref(),
            snapshot.frontier_budget.as_ref(),
            snapshot.replan_proposals,
            &generated_slices,
        )
    }
}

fn worker_attempt_history(
    mut attempts: Vec<WorkerAttemptLedger>,
    run: &Run,
    slice_runs: &[SliceRun],
) -> Vec<WorkerAttemptLedger> {
    for slice_run in slice_runs {
        for ordinal in 1..=slice_run.attempts {
            let has_durable_worker_launch = attempts.iter().any(|attempt| {
                attempt.slice_id == slice_run.slice_id
                    && attempt.kind == "slice-worker"
                    && attempt.worker_retry_ordinal == ordinal
            });
            if !has_durable_worker_launch {
                let is_latest = ordinal == slice_run.attempts;
                attempts.push(WorkerAttemptLedger {
                    run_id: slice_run.run_id.clone(),
                    slice_id: slice_run.slice_id.clone(),
                    launch_id: 0,
                    launch_ordinal: ordinal,
                    execution_epoch: 0,
                    worker_retry_ordinal: ordinal,
                    repair_ordinal: 0,
                    envelope_retry_ordinal: 0,
                    kind: "legacy-slice-run".to_string(),
                    state: if is_latest {
                        slice_run.status.as_str().to_string()
                    } else {
                        "failed".to_string()
                    },
                    branch: if is_latest {
                        slice_run.branch.clone()
                    } else {
                        String::new()
                    },
                    worktree: String::new(),
                    output_stem: String::new(),
                    created_at: run.started_at,
                    launched_at: None,
                    finished_at: None,
                    failure_cause: if is_latest {
                        slice_run.last_error.clone()
                    } else {
                        "legacy attempt details unavailable".to_string()
                    },
                    activity: None,
                });
            }
        }
    }
    attempts.sort_by(|left, right| {
        left.launch_id
            .cmp(&right.launch_id)
            .then_with(|| left.slice_id.cmp(&right.slice_id))
            .then_with(|| left.launch_ordinal.cmp(&right.launch_ordinal))
    });
    attempts
}

fn apply_terminal_override(
    run: &Run,
    transition: Option<&crate::state::TerminalTransition>,
    options: &RunReadModelOptions,
) -> Result<Run> {
    let mut run = run.clone();
    if let Some(terminal) = &options.terminal_override {
        let run_matches = run.status == terminal.status && run.error == terminal.error;
        let transition_matches = transition.is_some_and(|transition| {
            transition.status == terminal.status && transition.error == terminal.error
        });
        if !run_matches && !transition_matches {
            anyhow::bail!(
                "terminal projection context {} {:?} does not match snapshot revision for run {:?}",
                terminal.status,
                terminal.error,
                run.id
            );
        }
        run.status = terminal.status;
        run.error = terminal.error.clone();
    }
    Ok(run)
}

fn generated_slices_from_proposals(
    proposals: &[ReplanProposal],
    slice_runs: &[SliceRun],
) -> Vec<GeneratedSliceRecord> {
    let generated_ids = proposals
        .iter()
        .filter_map(|proposal| {
            let decision = proposal.operator_decision.as_ref()?;
            (!decision.generated_slice_id.trim().is_empty())
                .then(|| decision.generated_slice_id.clone())
        })
        .collect::<HashSet<_>>();
    let mut generations = proposals
        .iter()
        .filter_map(|proposal| {
            let decision = proposal.operator_decision.as_ref()?;
            (!decision.generated_slice_id.trim().is_empty()
                && decision.generated_slice_generation > 0)
                .then(|| {
                    (
                        decision.generated_slice_id.clone(),
                        decision.generated_slice_generation,
                    )
                })
        })
        .collect::<HashMap<_, _>>();

    // Older decision_json rows predate durable generation. Recover their value
    // only from persisted proposal lineage, never from live slice files.
    loop {
        let mut changed = false;
        for proposal in proposals {
            let Some(decision) = proposal.operator_decision.as_ref() else {
                continue;
            };
            if decision.generated_slice_id.trim().is_empty()
                || generations.contains_key(&decision.generated_slice_id)
            {
                continue;
            }
            let parent = proposal.source.slice_id.as_str();
            let generation = if generated_ids.contains(parent) {
                generations.get(parent).map(|generation| generation + 1)
            } else {
                Some(1)
            };
            if let Some(generation) = generation {
                generations.insert(decision.generated_slice_id.clone(), generation);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

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
            generation: generations
                .get(&decision.generated_slice_id)
                .copied()
                .unwrap_or(decision.generated_slice_generation),
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
        EventKind::from(event.typ.as_str()) == EventKind::RunIncident
            && (event.payload.get("failure_kind").is_some()
                || event.payload.get("operator_action_required").is_some()
                || payload_string(&event.payload, "severity") == Some("error".to_string()))
    })
}

fn terminal_run_error_event(events: &[Event]) -> Option<&Event> {
    events
        .iter()
        .rev()
        .find(|event| EventKind::from(event.typ.as_str()) == EventKind::RunError)
}

fn terminal_reason_from_event(
    run: &Run,
    event: &Event,
    include_run_summary_evidence: bool,
) -> TerminalReasonSource {
    let payload = &event.payload;
    let event_kind = EventKind::from(event.typ.as_str());
    let kind = payload_string(payload, "failure_kind")
        .or_else(|| payload_string(payload, "kind"))
        .unwrap_or_else(|| match &event_kind {
            EventKind::RunError => run.status.as_str().to_string(),
            _ => event_kind.as_str().to_string(),
        });
    let operator_action_required = payload
        .get("operator_action_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| default_operator_action_required(run.status));
    let retryable = payload
        .get("retryable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| default_retryable(run.status));
    let decoded_run_error =
        (event_kind == EventKind::RunError).then(|| RunErrorPayload::from_value(payload));
    let summary = payload_string(payload, "message")
        .or_else(|| payload_string(payload, "error"))
        .or_else(|| payload_string(payload, "summary"))
        .or_else(|| {
            decoded_run_error
                .as_ref()
                .map(|payload| payload.error.clone())
                .filter(|error| !error.trim().is_empty())
        })
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

fn default_evidence_links(_run: &Run, _include_run_summary_evidence: bool) -> Vec<String> {
    // Artifact paths are not evidence until their contents are pinned into the
    // same SQLite revision. Source freshness is exposed in snapshot metadata.
    Vec::new()
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
            let event_kind = EventKind::from(event.typ.as_str());
            let (severity, kind, message) = match event_kind {
                EventKind::RunIncident => (
                    payload_text(payload, "severity", "warning"),
                    payload_text(payload, "kind", "run_incident"),
                    payload_text(payload, "message", "incident recorded"),
                ),
                EventKind::RunError => (
                    "error".to_string(),
                    "run_error".to_string(),
                    payload_text(payload, "error", "run failed"),
                ),
                EventKind::RunResumed => (
                    "warning".to_string(),
                    "run_resumed".to_string(),
                    "run resumed after a terminal/interrupted state".to_string(),
                ),
                EventKind::WorktreeCleanupError | EventKind::DaemonRecoveryCleanupError => (
                    "warning".to_string(),
                    event.typ.clone(),
                    payload_text(payload, "error", "worktree cleanup reported an error"),
                ),
                EventKind::IntegrationRepairCompleted => (
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

const EXTERNAL_STATUS_SOURCES: &[&str] = &[
    "economics",
    "slice_definitions",
    "git_integration_head",
    "final_report",
    "preflight",
    "run_summary",
];

fn status_source_metadata(snapshot: &RunStateSnapshot) -> Vec<StatusSourceMetadata> {
    let mut sources = vec![StatusSourceMetadata {
        source: "sqlite".to_string(),
        state: "fresh".to_string(),
        indexed_event_id: Some(snapshot.revision.max_event_id),
        content_sha256: String::new(),
        observed_at: snapshot.revision.captured_at,
        detail: "authoritative run state read from one SQLite transaction".to_string(),
    }];
    for source in EXTERNAL_STATUS_SOURCES {
        if let Some(indexed) = snapshot
            .status_sources
            .iter()
            .find(|indexed| indexed.source == *source)
        {
            let state = if indexed.indexed_event_id == snapshot.revision.max_event_id {
                "fresh"
            } else {
                "stale"
            };
            sources.push(StatusSourceMetadata {
                source: (*source).to_string(),
                state: state.to_string(),
                indexed_event_id: Some(indexed.indexed_event_id),
                content_sha256: indexed.content_sha256.clone(),
                observed_at: Some(indexed.observed_at),
                detail: if state == "fresh" {
                    "payload is pinned inside this SQLite snapshot".to_string()
                } else {
                    "payload event frontier does not match this SQLite snapshot".to_string()
                },
            });
        } else {
            sources.push(StatusSourceMetadata {
                source: (*source).to_string(),
                state: "unavailable".to_string(),
                indexed_event_id: None,
                content_sha256: String::new(),
                observed_at: None,
                detail: "source is not indexed in this SQLite snapshot; live reads are forbidden"
                    .to_string(),
            });
        }
    }
    for indexed in &snapshot.status_sources {
        if EXTERNAL_STATUS_SOURCES
            .iter()
            .any(|source| *source == indexed.source)
        {
            continue;
        }
        sources.push(StatusSourceMetadata {
            source: indexed.source.clone(),
            state: if indexed.indexed_event_id == snapshot.revision.max_event_id {
                "fresh".to_string()
            } else {
                "stale".to_string()
            },
            indexed_event_id: Some(indexed.indexed_event_id),
            content_sha256: indexed.content_sha256.clone(),
            observed_at: Some(indexed.observed_at),
            detail: "unknown additive source retained for tolerant clients".to_string(),
        });
    }
    sources
}

fn indexed_economics(snapshot: &RunStateSnapshot) -> Result<Option<RunEconomics>> {
    let Some(source) = snapshot
        .status_sources
        .iter()
        .find(|source| source.source == "economics")
    else {
        return Ok(None);
    };
    serde_json::from_value(source.payload.clone())
        .context("decode required indexed economics status source")
        .map(Some)
}

fn indexed_final_report(snapshot: &RunStateSnapshot) -> Result<Option<ImplementationSummary>> {
    let Some(source) = snapshot
        .status_sources
        .iter()
        .find(|source| source.source == "final_report")
    else {
        return Ok(None);
    };
    serde_json::from_value(source.payload.clone())
        .context("decode required indexed final-report status source")
        .map(Some)
}

fn indexed_run_summary(snapshot: &RunStateSnapshot) -> Result<Option<serde_json::Value>> {
    let Some(source) = snapshot
        .status_sources
        .iter()
        .find(|source| source.source == "run_summary")
    else {
        return Ok(None);
    };
    let payload = source
        .payload
        .as_object()
        .context("decode required indexed run-summary object")?;
    let required_string = |field: &str| -> Result<&str> {
        payload
            .get(field)
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("decode required indexed run-summary field {field:?}"))
    };
    let run_id = required_string("run_id")?;
    if run_id != snapshot.run.id {
        anyhow::bail!(
            "indexed run-summary belongs to run {run_id:?}, not snapshot run {:?}",
            snapshot.run.id
        );
    }
    for field in [
        "repo_path",
        "status",
        "integration_branch",
        "selected_slice_id",
        "message",
        "primary_failure",
        "cancel_reason",
    ] {
        required_string(field)?;
    }
    let feed = payload
        .get("feed")
        .context("decode required indexed run-summary field \"feed\"")?;
    serde_json::from_value::<StatusFeed>(feed.clone())
        .context("decode required indexed run-summary feed")?;
    let commands = payload
        .get("next_commands")
        .and_then(serde_json::Value::as_array)
        .context("decode required indexed run-summary field \"next_commands\"")?;
    if commands.iter().any(|command| !command.is_string()) {
        anyhow::bail!("decode required indexed run-summary next_commands as strings");
    }
    Ok(Some(source.payload.clone()))
}

fn completion_publication(events: &[Event]) -> Result<Option<CompletionPublicationReceipt>> {
    events
        .iter()
        .rev()
        .find(|event| {
            EventKind::from(event.typ.as_str()) == EventKind::CompletionPublicationCommitted
        })
        .map(|event| {
            serde_json::from_value(event.payload.clone())
                .context("decode durable completion publication receipt from status snapshot")
        })
        .transpose()
}

fn read_worker_profile(
    snapshot: &RunStateSnapshot,
    economics: Option<&RunEconomics>,
) -> Option<WorkerProfileEvidence> {
    snapshot
        .status_sources
        .iter()
        .filter(|source| source.indexed_event_id == snapshot.revision.max_event_id)
        .rev()
        .find_map(|source| WorkerProfileEvidence::from_json_surface(&source.payload))
        .or_else(|| {
            snapshot
                .events
                .iter()
                .rev()
                .find_map(|event| WorkerProfileEvidence::from_json_surface(&event.payload))
        })
        .or_else(|| {
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
        match EventKind::from(event.typ.as_str()) {
            EventKind::ParallelLayerCompleted | EventKind::ParallelLayerFailed => return None,
            EventKind::ParallelLayerStarted => {
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
    selected_slice_ids: &[String],
    mission_envelope: Option<&MissionEnvelope>,
    frontier_budget: Option<&FrontierBudgetState>,
    proposals: Vec<ReplanProposal>,
    generated_slices: &[GeneratedSliceRecord],
) -> Result<PlanRevisions> {
    let proposals = proposals
        .into_iter()
        .map(|mut proposal| {
            if proposal.state.as_str() == "pending" {
                proposal.decision_commands = replan_decision_commands(&run.id, &proposal.id);
            }
            proposal
        })
        .collect::<Vec<_>>();
    let frontier = frontier_summary_from_records(
        mission_envelope,
        frontier_budget,
        &proposals,
        generated_slices,
    );
    let mut plan = PlanRevisions {
        source_of_truth: "daemon_replan_proposals".to_string(),
        queue_summary: if selected_slice_ids.is_empty() {
            "selected slices: <none>".to_string()
        } else {
            format!("selected slices: {}", selected_slice_ids.join(","))
        },
        frontier,
        ..PlanRevisions::default()
    };
    for proposal in proposals {
        let record = plan_revision_record(selected_slice_ids, proposal)?;
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

pub(crate) fn frontier_summary_from_records(
    mission_envelope: Option<&MissionEnvelope>,
    frontier_budget: Option<&FrontierBudgetState>,
    proposals: &[ReplanProposal],
    generated_slices: &[GeneratedSliceRecord],
) -> FrontierSummary {
    let has_frontier_context = mission_envelope.is_some() || frontier_budget.is_some();
    let relevant_proposals = proposals
        .iter()
        .filter(|proposal| proposal_is_frontier_relevant(proposal, has_frontier_context))
        .collect::<Vec<_>>();
    if mission_envelope.is_none() && relevant_proposals.is_empty() && generated_slices.is_empty() {
        return FrontierSummary::default();
    }

    let mut summary = FrontierSummary {
        activity_status: if relevant_proposals.is_empty() && generated_slices.is_empty() {
            "empty".to_string()
        } else {
            "active".to_string()
        },
        envelope_snapshot: mission_envelope.cloned(),
        autonomy_effective: mission_envelope
            .map(|envelope| envelope.autonomy_level.as_str())
            .unwrap_or("off")
            .to_string(),
        budget_consumption: frontier_budget_consumption(
            mission_envelope,
            frontier_budget,
            proposals,
            generated_slices,
        ),
        ..FrontierSummary::default()
    };

    for proposal in &relevant_proposals {
        push_unique_string(&mut summary.proposal_ids, proposal.id.clone());
        if let Some(classification) = &proposal.frontier_classification {
            summary.candidates_seen += 1;
            *summary
                .tier_distribution
                .entry(classification.tier.clone())
                .or_insert(0) += 1;
            summary.tier_reason_codes.push(FrontierTierReasonRecord {
                proposal_id: proposal.id.clone(),
                tier: classification.tier.clone(),
                reason_codes: classification.reason_codes.clone(),
                classified_at: classification.classified_at.to_rfc3339(),
                envelope_hash: classification.envelope_hash.clone(),
            });
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
        } else if let Some(decision) = proposal.operator_decision.as_ref()
            && !decision.frontier_tier.trim().is_empty()
        {
            summary.tier_reason_codes.push(FrontierTierReasonRecord {
                proposal_id: proposal.id.clone(),
                tier: decision.frontier_tier.clone(),
                reason_codes: decision.frontier_reason_codes.clone(),
                classified_at: decision.decided_at.to_rfc3339(),
                envelope_hash: String::new(),
            });
        }

        if let Some(authorizer) = frontier_authorizer_record(proposal) {
            summary.authorizers.push(authorizer);
        }
        if let Some(fog) = frontier_fog_record(proposal) {
            summary.deferred_rejected_pending_fog.push(fog);
        }
        if let Some(stop) = frontier_operator_stop(proposal) {
            summary.operator_needed_stops.push(stop);
        }
    }

    for generated in generated_slices {
        push_unique_string(
            &mut summary.proposal_ids,
            generated.origin_proposal_id.clone(),
        );
        if let Some(edge) = frontier_generated_slice_edge(generated, proposals) {
            summary.generated_slice_graph.push(edge);
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
        summary.shadow_agreement_metrics = summary.agreement.clone();
    }

    summary.proposal_ids.sort();
    summary.proposal_ids.dedup();
    summary.generated_slice_graph.sort_by(|left, right| {
        left.parent_slice_id
            .cmp(&right.parent_slice_id)
            .then(left.child_slice_id.cmp(&right.child_slice_id))
            .then(left.origin_proposal_id.cmp(&right.origin_proposal_id))
    });
    summary
        .authorizers
        .sort_by(|left, right| left.proposal_id.cmp(&right.proposal_id));
    summary
        .tier_reason_codes
        .sort_by(|left, right| left.proposal_id.cmp(&right.proposal_id));
    summary
        .deferred_rejected_pending_fog
        .sort_by(|left, right| left.proposal_id.cmp(&right.proposal_id));
    summary
        .operator_needed_stops
        .sort_by(|left, right| left.proposal_id.cmp(&right.proposal_id));

    if summary.activity_status == "empty" {
        summary.empty_reason = "mission envelope recorded; no frontier proposals, generated slices, pending candidates, or classifier observations were recorded".to_string();
        summary.summary_line = "frontier activity: none recorded".to_string();
    } else if summary.candidates_seen > 0 {
        let agreement = if summary.agreement.agreement_denominator == 0 {
            "n/a".to_string()
        } else {
            format!("{:.0}%", summary.agreement.agreement_percent)
        };
        summary.summary_line = format!(
            "frontier activity: candidates_seen={}, generated_slices={}, pending_deferred_rejected={}, operator_stops={}, tier_1_would_promote={}, agreement={} ({agreement})",
            summary.candidates_seen,
            summary.generated_slice_graph.len(),
            summary.deferred_rejected_pending_fog.len(),
            summary.operator_needed_stops.len(),
            summary.agreement.tier1_total,
            summary.agreement.agreement_ratio
        );
    } else {
        summary.summary_line = format!(
            "frontier activity: generated_slices={}, proposals={}, pending_deferred_rejected={}, operator_stops={}",
            summary.generated_slice_graph.len(),
            summary.proposal_ids.len(),
            summary.deferred_rejected_pending_fog.len(),
            summary.operator_needed_stops.len()
        );
    }
    summary
}

fn proposal_is_frontier_relevant(proposal: &ReplanProposal, has_frontier_context: bool) -> bool {
    proposal.frontier_classification.is_some()
        || proposal.operator_decision.as_ref().is_some_and(|decision| {
            !decision.generated_slice_id.trim().is_empty()
                || !decision.frontier_tier.trim().is_empty()
                || !decision.frontier_reason_codes.is_empty()
        })
        || (has_frontier_context
            && proposal.proposed_changes.iter().any(|change| {
                change.kind == "add_followup_slice" || change.followup_slice_draft().is_some()
            }))
}

fn frontier_budget_consumption(
    mission_envelope: Option<&MissionEnvelope>,
    frontier_budget: Option<&FrontierBudgetState>,
    proposals: &[ReplanProposal],
    generated_slices: &[GeneratedSliceRecord],
) -> FrontierBudgetConsumption {
    let auto_promotions_used = frontier_budget
        .map(|budget| budget.auto_promotions_used)
        .unwrap_or_else(|| {
            proposals
                .iter()
                .filter(|proposal| {
                    proposal.operator_decision.as_ref().is_some_and(|decision| {
                        decision.authorizer.starts_with("envelope:")
                            && decision.decision == "accepted"
                    })
                })
                .count() as i64
        });
    let generated_count = frontier_budget
        .map(|budget| budget.generated_slices)
        .unwrap_or(generated_slices.len() as i64);
    FrontierBudgetConsumption {
        auto_promotions_used,
        max_auto_promotions: mission_envelope
            .map(|envelope| envelope.max_auto_promotions)
            .unwrap_or(0),
        generated_slices: generated_count,
        max_generated_slices: mission_envelope
            .map(|envelope| envelope.max_generated_slices)
            .unwrap_or(0),
        max_depth: mission_envelope
            .map(|envelope| envelope.max_depth)
            .unwrap_or(0),
        max_depth_reached: generated_slices
            .iter()
            .map(|slice| slice.generation)
            .max()
            .unwrap_or(0),
        max_generation_reached: frontier_budget
            .map(|budget| budget.max_generation_reached)
            .unwrap_or(false),
    }
}

fn frontier_authorizer_record(proposal: &ReplanProposal) -> Option<FrontierAuthorizerRecord> {
    if let Some(decision) = proposal.operator_decision.as_ref() {
        return Some(FrontierAuthorizerRecord {
            proposal_id: proposal.id.clone(),
            state: proposal.state.as_str().to_string(),
            decision: decision.decision.clone(),
            authorizer: decision.authorizer.clone(),
            source: decision.source.clone(),
            generated_slice_id: decision.generated_slice_id.clone(),
            tier: proposal_tier(proposal),
            applied: decision.applied,
        });
    }
    (proposal.state == ReplanProposalState::Pending).then(|| FrontierAuthorizerRecord {
        proposal_id: proposal.id.clone(),
        state: proposal.state.as_str().to_string(),
        decision: "pending".to_string(),
        authorizer: "operator_required".to_string(),
        source: "replan".to_string(),
        generated_slice_id: proposal_followup_slice_id(proposal),
        tier: proposal_tier(proposal),
        applied: false,
    })
}

fn frontier_fog_record(proposal: &ReplanProposal) -> Option<FrontierFogRecord> {
    if !matches!(
        proposal.state,
        ReplanProposalState::Pending
            | ReplanProposalState::Deferred
            | ReplanProposalState::Rejected
    ) {
        return None;
    }
    let decision = proposal.operator_decision.as_ref();
    Some(FrontierFogRecord {
        proposal_id: proposal.id.clone(),
        state: proposal.state.as_str().to_string(),
        source_slice_id: proposal.source.slice_id.clone(),
        proposed_slice_id: proposal_followup_slice_id(proposal),
        tier: proposal_tier(proposal),
        reason_codes: proposal_reason_codes(proposal),
        rationale: decision
            .map(|decision| decision.rationale.clone())
            .unwrap_or_default(),
        revisit_condition: decision
            .map(|decision| decision.revisit_condition.clone())
            .unwrap_or_default(),
        authorizer: decision
            .map(|decision| decision.authorizer.clone())
            .unwrap_or_else(|| "operator_required".to_string()),
        source: decision
            .map(|decision| decision.source.clone())
            .unwrap_or_else(|| "replan".to_string()),
        decision_commands: proposal.decision_commands.clone(),
    })
}

fn frontier_operator_stop(proposal: &ReplanProposal) -> Option<FrontierOperatorStop> {
    let tier = proposal_tier(proposal);
    let reason_codes = proposal_reason_codes(proposal);
    let is_stop = tier == "tier_3"
        || tier == "stop"
        || reason_codes.iter().any(|code| {
            matches!(
                code.as_str(),
                "frontier_budget_exhausted" | "frontier_depth_exhausted"
            )
        });
    if !is_stop {
        return None;
    }
    let decision = proposal.operator_decision.as_ref();
    Some(FrontierOperatorStop {
        proposal_id: proposal.id.clone(),
        stop_kind: if reason_codes
            .iter()
            .any(|code| code == "frontier_budget_exhausted")
        {
            "budget_exhausted".to_string()
        } else if reason_codes
            .iter()
            .any(|code| code == "frontier_depth_exhausted")
        {
            "depth_exhausted".to_string()
        } else if tier == "stop" {
            "frontier_stop".to_string()
        } else {
            "tier_3_operator_required".to_string()
        },
        state: proposal.state.as_str().to_string(),
        source_slice_id: proposal.source.slice_id.clone(),
        proposed_slice_id: proposal_followup_slice_id(proposal),
        resolution: decision
            .map(|decision| decision.decision.clone())
            .unwrap_or_else(|| "pending_operator_decision".to_string()),
        rationale: decision
            .map(|decision| decision.rationale.clone())
            .unwrap_or_default(),
        reason_codes,
        decision_commands: proposal.decision_commands.clone(),
    })
}

fn frontier_generated_slice_edge(
    generated: &GeneratedSliceRecord,
    proposals: &[ReplanProposal],
) -> Option<FrontierGeneratedSliceEdge> {
    let proposal = proposals
        .iter()
        .find(|proposal| proposal.id == generated.origin_proposal_id);
    let decision = proposal.and_then(|proposal| proposal.operator_decision.as_ref());
    let parent = if generated.parent_slice_id.trim().is_empty() {
        proposal
            .map(|proposal| proposal.source.slice_id.clone())
            .unwrap_or_default()
    } else {
        generated.parent_slice_id.clone()
    };
    let child = generated.slice_id.trim();
    if child.is_empty() {
        return None;
    }
    Some(FrontierGeneratedSliceEdge {
        parent_slice_id: parent,
        child_slice_id: generated.slice_id.clone(),
        origin_proposal_id: generated.origin_proposal_id.clone(),
        generation: generated.generation,
        authorizer: decision
            .map(|decision| decision.authorizer.clone())
            .unwrap_or_default(),
        decision_source: decision
            .map(|decision| decision.source.clone())
            .unwrap_or_default(),
        tier: proposal.map(proposal_tier).unwrap_or_default(),
        reason_codes: proposal.map(proposal_reason_codes).unwrap_or_default(),
        status: generated.status.clone(),
        commit_sha: generated.commit_sha.clone(),
        applied_at: generated.applied_at,
        queue_before_hash: decision
            .map(|decision| decision.queue_before_hash.clone())
            .unwrap_or_default(),
        queue_after_hash: decision
            .map(|decision| decision.queue_after_hash.clone())
            .unwrap_or_default(),
    })
}

fn proposal_tier(proposal: &ReplanProposal) -> String {
    proposal
        .frontier_classification
        .as_ref()
        .map(|classification| classification.tier.clone())
        .filter(|tier| !tier.trim().is_empty())
        .or_else(|| {
            proposal
                .operator_decision
                .as_ref()
                .map(|decision| decision.frontier_tier.clone())
                .filter(|tier| !tier.trim().is_empty())
        })
        .unwrap_or_default()
}

fn proposal_reason_codes(proposal: &ReplanProposal) -> Vec<String> {
    proposal
        .frontier_classification
        .as_ref()
        .map(|classification| classification.reason_codes.clone())
        .filter(|codes| !codes.is_empty())
        .or_else(|| {
            proposal
                .operator_decision
                .as_ref()
                .map(|decision| decision.frontier_reason_codes.clone())
                .filter(|codes| !codes.is_empty())
        })
        .unwrap_or_default()
}

fn proposal_followup_slice_id(proposal: &ReplanProposal) -> String {
    proposal
        .proposed_changes
        .iter()
        .find_map(|change| {
            change
                .followup_slice_draft()
                .map(|draft| draft.id)
                .filter(|id| !id.trim().is_empty())
                .or_else(|| {
                    (change.kind == "add_followup_slice" && !change.target.trim().is_empty())
                        .then(|| change.target.clone())
                })
        })
        .or_else(|| {
            proposal
                .operator_decision
                .as_ref()
                .map(|decision| decision.generated_slice_id.clone())
                .filter(|id| !id.trim().is_empty())
        })
        .unwrap_or_default()
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
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

fn plan_revision_record(
    selected_slice_ids: &[String],
    proposal: ReplanProposal,
) -> Result<PlanRevisionRecord> {
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
        before_queue_or_slice_summary: plan_revision_before_summary(selected_slice_ids, &proposal),
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
        generated_slice_generation: decision.generated_slice_generation,
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

fn plan_revision_before_summary(
    selected_slice_ids: &[String],
    proposal: &ReplanProposal,
) -> String {
    let queue = if selected_slice_ids.is_empty() {
        "<none>".to_string()
    } else {
        selected_slice_ids.join(",")
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

#[cfg(test)]
mod worker_attempt_history_tests {
    use super::*;
    use crate::domain::{Run, RunStatus, SliceRun, SliceStatus};
    use chrono::Utc;

    #[test]
    fn snapshot_exposes_legacy_marked_attempt_history_when_no_ledger_exists() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-history".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: "slice-a".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .upsert_slice_run(&SliceRun {
                run_id: run.id.clone(),
                slice_id: "slice-a".to_string(),
                status: SliceStatus::Blocked,
                branch: "legacy-branch".to_string(),
                commit_sha: String::new(),
                attempts: 2,
                last_error: "legacy failure".to_string(),
            })
            .unwrap();

        let model = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        assert_eq!(model.details.worker_attempts.len(), 2);
        let first = &model.details.worker_attempts[0];
        assert_eq!(first.kind, "legacy-slice-run");
        assert_eq!(first.worker_retry_ordinal, 1);
        assert_eq!(first.state, "failed");
        assert!(first.branch.is_empty());
        assert_eq!(first.failure_cause, "legacy attempt details unavailable");
        let latest = &model.details.worker_attempts[1];
        assert_eq!(latest.kind, "legacy-slice-run");
        assert_eq!(latest.worker_retry_ordinal, 2);
        assert_eq!(latest.branch, "legacy-branch");
        assert_eq!(latest.created_at, run.started_at);
        let repeated = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        assert_eq!(
            repeated.details.worker_attempts,
            model.details.worker_attempts
        );
    }

    #[test]
    fn legacy_generated_slice_decisions_recover_recursive_generation_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.db");
        let state = StateStore::open(&database).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-legacy-generation".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: "root".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        for (proposal_id, parent_slice) in [("proposal-1", "root"), ("proposal-2", "child-1")] {
            state
                .create_replan_proposal(
                    &run.id,
                    proposal_id,
                    crate::domain::ReplanProposalSource {
                        kind: "worker".to_string(),
                        slice_id: parent_slice.to_string(),
                        ..Default::default()
                    },
                    Vec::new(),
                    Vec::new(),
                    vec![crate::domain::ReplanProposedChange {
                        kind: "followup_slice".to_string(),
                        target: String::new(),
                        summary: "legacy generated slice".to_string(),
                        followup_slice_draft: None,
                    }],
                    "low",
                )
                .unwrap();
        }
        let conn = rusqlite::Connection::open(&database).unwrap();
        for (proposal_id, generated_slice_id) in
            [("proposal-1", "child-1"), ("proposal-2", "child-2")]
        {
            let decision = json!({
                "decision": "accepted",
                "rationale": "legacy decision",
                "authorizer": "operator",
                "source": "test",
                "decided_at": now,
                "applied": true,
                "applied_at": now,
                "apply_status": "applied",
                "apply_reason": "legacy apply",
                "generated_slice_id": generated_slice_id,
                "generated_slice_commit": "commit"
            });
            conn.execute(
                "UPDATE replan_proposals SET state='accepted', decision_json=?1 WHERE run_id=?2 AND id=?3",
                rusqlite::params![decision.to_string(), &run.id, proposal_id],
            )
            .unwrap();
        }
        drop(conn);
        drop(state);

        let reopened = StateStore::open(&database).unwrap();
        let model = RunReadModelBuilder::new(&reopened)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        assert_eq!(
            model
                .details
                .generated_slices
                .iter()
                .map(|slice| (slice.slice_id.as_str(), slice.generation))
                .collect::<Vec<_>>(),
            vec![("child-1", 1), ("child-2", 2)]
        );
    }

    #[test]
    fn snapshot_preserves_legacy_retries_before_a_new_ledger_launch() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-mixed-history".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: "slice-a".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .upsert_slice_run(&SliceRun {
                run_id: run.id.clone(),
                slice_id: "slice-a".to_string(),
                status: SliceStatus::Failed,
                branch: "legacy-branch".to_string(),
                commit_sha: String::new(),
                attempts: 2,
                last_error: "legacy retry failed".to_string(),
            })
            .unwrap();
        let modern = state
            .allocate_worker_attempt(
                &run.id,
                "slice-a",
                2,
                3,
                0,
                0,
                "slice-worker",
                directory.path(),
            )
            .unwrap();
        state
            .mark_worker_attempt_launched(modern.launch_id)
            .unwrap();
        state
            .finish_worker_attempt(modern.launch_id, "succeeded", "")
            .unwrap();

        let first = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        let repeated = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();

        assert_eq!(
            first.details.worker_attempts,
            repeated.details.worker_attempts
        );
        assert_eq!(first.details.worker_attempts.len(), 3);
        for ordinal in 1..=2 {
            let legacy = first
                .details
                .worker_attempts
                .iter()
                .find(|attempt| {
                    attempt.kind == "legacy-slice-run" && attempt.worker_retry_ordinal == ordinal
                })
                .expect("missing legacy retry evidence");
            assert_eq!(legacy.launch_id, 0);
            assert_eq!(legacy.state, "failed");
        }
        let retained = first
            .details
            .worker_attempts
            .iter()
            .find(|attempt| attempt.launch_id == modern.launch_id)
            .expect("missing immutable ledger launch");
        assert_eq!(retained.worker_retry_ordinal, 3);
        assert_eq!(retained.state, "succeeded");
    }

    #[test]
    fn snapshot_orders_durable_ledger_history_by_launch_identity() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-ledger-history".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: "slice-a".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        for slice_id in ["slice-z", "slice-a"] {
            state
                .upsert_slice_run(&SliceRun {
                    run_id: run.id.clone(),
                    slice_id: slice_id.to_string(),
                    status: SliceStatus::Running,
                    branch: String::new(),
                    commit_sha: String::new(),
                    attempts: 0,
                    last_error: String::new(),
                })
                .unwrap();
            state
                .allocate_worker_attempt(
                    &run.id,
                    slice_id,
                    1,
                    1,
                    0,
                    0,
                    "slice-worker",
                    directory.path(),
                )
                .unwrap();
        }

        let model = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        assert_eq!(
            model
                .details
                .worker_attempts
                .iter()
                .map(|row| row.slice_id.as_str())
                .collect::<Vec<_>>(),
            vec!["slice-z", "slice-a"]
        );
        assert!(
            model
                .details
                .worker_attempts
                .iter()
                .all(|row| row.kind == "slice-worker")
        );
    }

    #[test]
    fn indexed_source_metadata_is_fresh_only_at_its_exact_event_frontier() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-source-frontier".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .record_event(&run.id, "economics_ready", &json!({"generation": 1}))
            .unwrap();
        let economics = RunEconomics {
            repair_policy: "on-failure".to_string(),
            ..RunEconomics::default()
        };
        state
            .record_status_source_snapshot(&run.id, "economics", || &economics)
            .unwrap();

        let fresh = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        let source = fresh
            .details
            .snapshot
            .sources
            .iter()
            .find(|source| source.source == "economics")
            .unwrap();
        assert_eq!(source.state, "fresh");
        assert!(!source.content_sha256.is_empty());
        assert!(fresh.details.economics.is_some());

        state
            .record_event(&run.id, "newer_state", &json!({"generation": 2}))
            .unwrap();
        let stale = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        let source = stale
            .details
            .snapshot
            .sources
            .iter()
            .find(|source| source.source == "economics")
            .unwrap();
        assert_eq!(source.state, "stale");
        assert_eq!(
            stale.details.economics.as_ref().unwrap().repair_policy,
            "on-failure"
        );
    }

    #[test]
    fn malformed_fresh_economics_source_is_an_explicit_read_error() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-bad-economics".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .record_status_source_snapshot(&run.id, "economics", || json!({"repair_policy": 7}))
            .unwrap();

        let error = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .expect_err("malformed required economics must fail");
        assert!(
            format!("{error:#}").contains("decode required indexed economics status source"),
            "{error:#}"
        );
    }

    #[test]
    fn malformed_indexed_run_summary_is_an_explicit_read_error() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-bad-summary".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Completed,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .record_status_source_snapshot(
                &run.id,
                "run_summary",
                || json!({"run_id": run.id, "status": "completed"}),
            )
            .unwrap();

        let error = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .expect_err("malformed required run summary must fail");
        assert!(
            format!("{error:#}").contains("decode required indexed run-summary field"),
            "{error:#}"
        );
    }

    #[test]
    fn complete_history_drives_terminal_reason_while_response_tail_stays_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-history-tail".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Failed,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .record_event(
                &run.id,
                "run_error",
                &json!({"error": "old decisive failure"}),
            )
            .unwrap();
        for index in 0..4 {
            state
                .record_event(&run.id, "noise", &json!({"index": index}))
                .unwrap();
        }

        let model = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(1))
            .unwrap();
        assert_eq!(model.details.events.len(), 1);
        assert_eq!(model.details.events[0].typ, "noise");
        assert_eq!(
            model
                .details
                .primary_terminal_reason
                .as_ref()
                .unwrap()
                .summary,
            "old decisive failure"
        );
    }

    #[test]
    fn feed_semantics_use_complete_history_while_wire_events_remain_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-complete-feed-history".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        state
            .record_event(
                &run.id,
                "implementation_summary",
                &json!({
                    "verify_profile": "full",
                    "integration_gate": {
                        "status": "passed",
                        "summary": "integration gate passed",
                        "commands": [],
                        "findings": []
                    }
                }),
            )
            .unwrap();
        for index in 0..12 {
            state
                .record_event(&run.id, "later_event", &json!({"index": index}))
                .unwrap();
        }

        let model = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(2))
            .unwrap();
        assert_eq!(model.details.events.len(), 2);
        assert!(
            model
                .details
                .events
                .iter()
                .all(|event| event.typ != "implementation_summary")
        );
        assert_eq!(model.details.feed.as_ref().unwrap().gate.state, "passed");
    }

    #[test]
    fn terminal_override_must_match_the_snapshot_revision() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-override-mismatch".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();

        let error = RunReadModelBuilder::new(&state)
            .snapshot(
                &run.id,
                RunReadModelOptions::terminal_summary(RunStatus::Failed, "not committed"),
            )
            .expect_err("unrelated terminal override must fail");
        assert!(
            format!("{error:#}").contains("does not match snapshot revision"),
            "{error:#}"
        );
    }

    #[test]
    fn unindexed_corrupt_artifacts_and_git_movement_are_explicitly_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(directory.path().join("state.db")).unwrap();
        let now = Utc::now();
        let run = Run {
            id: "run-live-sources-forbidden".to_string(),
            repo_id: "repo".to_string(),
            repo_path: directory.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "integration".to_string(),
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run).unwrap();
        let output = directory
            .path()
            .join(".workflow/runs")
            .join(&run.id)
            .join("economics.json");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "{not-json").unwrap();
        let git_head = directory.path().join(".git/HEAD");
        std::fs::create_dir_all(git_head.parent().unwrap()).unwrap();
        std::fs::write(&git_head, "ref: refs/heads/before\n").unwrap();
        std::fs::write(&git_head, "ref: refs/heads/after\n").unwrap();

        let model = RunReadModelBuilder::new(&state)
            .snapshot(&run.id, RunReadModelOptions::status(10))
            .unwrap();
        assert!(model.details.economics.is_none());
        for name in [
            "economics",
            "slice_definitions",
            "git_integration_head",
            "final_report",
            "preflight",
            "run_summary",
        ] {
            let source = model
                .details
                .snapshot
                .sources
                .iter()
                .find(|source| source.source == name)
                .unwrap();
            assert_eq!(source.state, "unavailable", "{name}");
        }
    }
}
