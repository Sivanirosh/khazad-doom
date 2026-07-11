use super::attention::{
    OperatorAttention, TerminalTransitionNotification, WorkerPaneTerminalRename,
};
use super::cockpit::{
    CockpitLaunch, CockpitTuiWorkerLaunch, CockpitTuiWorkerRequest, CockpitWorkerLaunch,
    CockpitWorkerPaneRequest, close_default_pane, notify_origin_replan_attention,
    notify_origin_worker_question_attention, open_default_run_cockpit,
    open_default_tui_worker_agent, open_default_worker_pane, take_cockpit_mode_transport_arg,
    worker_activity_pane_command,
};
use super::economics::{RunEconomicsRecorder, agent_call};
use super::events as workflow_events;
use super::frontier::promotion_policy::{
    FollowupProposalChange, FollowupProposalView, ProposalGraphNode, SliceGraphSlice,
    SliceGraphSliceStatus, SliceGraphView, classify_followup_proposal,
};
use super::gate::{
    IntegrationGateRequest, SliceVerificationRequest, VerificationCommandCache, WorkflowGate,
    WorktreeSetupRequest, failure_kind_needs_operator,
};
use super::read_model::authorized_paths_from_proposal;
use super::{
    CancelledError, REPAIR_RESULT_SCHEMA, RunReadModel, RunReadModelBuilder, RunReadModelOptions,
    WORKER_RESULT_SCHEMA, check_cancelled, integration_repair_prompt, slice_repair_prompt,
    worker_envelope_retry_prompt, worker_prompt,
};
use crate::agent::{
    CancellationToken, Job, PiCommandSpec, PiWrapperLaunchError, Runner, RunnerError, RunnerEvent,
    RunnerEventSink, RunnerLaunchFailure, RunnerMetadata, RunnerTranscript,
    collect_pi_wrapper_result, parse_pi_tui_worker_result_artifact,
    prepare_pi_tui_worker_artifacts, prepare_pi_wrapper_artifacts, runner_from_spec,
    wait_for_pi_wrapper_launch, worker_evidence_kind_for_runner, worker_evidence_label_for_runner,
};
use crate::agent_profile::{ProfileResolveInput, resolve_effective_worker_profile};
use crate::artifact;
use crate::domain::{
    AgentProfilesConfig, AutonomyLevel, BranchHandoff, CheckResult, CockpitMode,
    EvidenceAttestation, Finding, FindingDisposition, FollowupSliceDraft, FrontierBudgetState,
    FrontierClassification, GateResult, Handoff, HandoffActionResult, HandoffDiagnostics,
    ImplementationSummary, MergeConflictReport, MissionEnvelope, OriginNotificationTarget,
    PlanRevisions, RepairResult, ReplanDecision, ReplanEvidenceLink, ReplanProposal,
    ReplanProposalSource, ReplanProposedChange, Run, RunCheckpoint, RunInspection, RunStatus,
    Slice, SliceExitState, SliceProvenance, SliceRun, SliceStatus, SliceValidationReport,
    SliceWriteResult, WorkerAttemptLedger, WorkerProfileEvidence, WorkerQuestion, WorkerResult,
    WorkflowConfig, WorkflowExitStates, is_open_status, replan_decision_commands,
};
use crate::gitutil;
use crate::paths::{self, Paths};
use crate::state::{
    ProgressReporter, ProgressScope, Repo, Store as StateStore, TerminalTransition,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_WORKER_ATTEMPTS: usize = 3;
pub const DEFAULT_WORKER_ENVELOPE_RETRY_ATTEMPTS: usize = 2;
pub const DEFAULT_SLICE_REPAIR_ATTEMPTS: usize = 1;
pub const DEFAULT_REPAIR_ATTEMPTS: usize = 1;
const INTEGRATION_REPAIR_SCOPE_ID: &str = "integration-repair";
static WORKTREE_ADD_LOCK: Mutex<()> = Mutex::new(());
const WORKTREE_REMOVE_ATTEMPTS: usize = 3;
const WORKTREE_REMOVE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalizationFaultStage {
    SummaryWrite,
    Notification,
    Cleanup,
}

#[cfg(test)]
thread_local! {
    static TERMINALIZATION_FAULT: std::cell::RefCell<Option<TerminalizationFaultStage>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn inject_terminalization_fault(stage: TerminalizationFaultStage) {
    TERMINALIZATION_FAULT.with(|fault| *fault.borrow_mut() = Some(stage));
}

#[cfg(test)]
fn take_terminalization_fault(stage: TerminalizationFaultStage) -> bool {
    TERMINALIZATION_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if *fault == Some(stage) {
            *fault = None;
            true
        } else {
            false
        }
    })
}

fn worker_profile_evidence(runner_name: &str, metadata: &RunnerMetadata) -> WorkerProfileEvidence {
    WorkerProfileEvidence {
        agent: runner_name.to_string(),
        agent_profile: metadata.profile.clone(),
        agent_provider: metadata.provider.clone(),
        agent_model: metadata.model.clone(),
        agent_reasoning: metadata.reasoning.clone(),
        agent_mode: metadata.mode.clone(),
        profile_summary: metadata.profile_summary(),
        launch_summary: metadata.launch_summary(),
        worker_evidence_kind: worker_evidence_kind_for_runner(runner_name).to_string(),
        worker_evidence_label: worker_evidence_label_for_runner(runner_name).to_string(),
        source_attribution: metadata.source_attribution.clone(),
    }
}

fn append_worker_evidence_attestation_basis(
    attestation: &mut EvidenceAttestation,
    worker_profile: &WorkerProfileEvidence,
) {
    if worker_profile.worker_evidence_kind.trim().is_empty() {
        return;
    }
    let basis = format!(
        "worker evidence kind: {} ({})",
        worker_profile.worker_evidence_kind, worker_profile.worker_evidence_label
    );
    if !attestation.basis.iter().any(|existing| existing == &basis) {
        attestation.basis.push(basis);
    }
}

fn origin_notification_target_from_start(target: &str) -> Option<OriginNotificationTarget> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    Some(OriginNotificationTarget {
        schema_version: 1,
        target: target.to_string(),
        target_kind: "opaque".to_string(),
        delivery_adapter: "herdr".to_string(),
        delivery_surface: "agent_send".to_string(),
        source: "run_start".to_string(),
        created_at: Utc::now().to_rfc3339(),
    })
}

#[derive(Clone)]
pub struct Manager {
    pub paths: Paths,
    pub state: StateStore,
    runner_override: Option<Arc<dyn Runner>>,
    active: Arc<ActiveRuns>,
}

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub repo_path: PathBuf,
    pub slice_ids: Vec<String>,
    pub all: bool,
    pub agent: String,
    pub pi_bin: String,
    pub pi_args: Vec<String>,
    pub native_pi_tui_worker: bool,
    pub parallelism: usize,
    pub allow_dirty: bool,
    pub origin_notification_target: String,
    pub mission_envelope: Option<MissionEnvelope>,
}

#[derive(Debug, Clone)]
pub struct SliceDraft {
    pub repo_path: PathBuf,
    pub id: String,
    pub title: String,
    pub goal: String,
    pub github_issue: String,
    pub acceptance: Vec<String>,
    pub verify: Vec<String>,
    pub overwrite: bool,
}

#[derive(Debug, Clone)]
pub struct GithubImportOptions {
    pub repo_path: PathBuf,
    pub issue: String,
    pub id: String,
    pub verify: Vec<String>,
    pub overwrite: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ResumeOptions {
    pub run_id: String,
    pub agent: String,
    pub pi_bin: String,
    pub pi_args: Vec<String>,
    pub native_pi_tui_worker: bool,
    pub parallelism: usize,
}

#[derive(Debug, Clone, Copy)]
enum IntegrationMode {
    Fresh,
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowupApplyMode {
    PromoteOnly,
    AppendAndRun,
}

impl FollowupApplyMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::PromoteOnly => "promote_only",
            Self::AppendAndRun => "append_and_run",
        }
    }
}

struct IntegrationRepairContext<'a> {
    run: &'a Run,
    slices: &'a [Slice],
    integration_worktree: &'a Path,
    checks: &'a [CheckResult],
    gate_failure: &'a GateResult,
    trigger: &'a str,
    cancel: &'a CancellationToken,
    runner: Arc<dyn Runner>,
    config: &'a WorkflowConfig,
    economics: RunEconomicsRecorder,
    verification_cache: &'a VerificationCommandCache,
}

struct SupervisedWorkerJobOutcome {
    result: Result<crate::agent::ResultData>,
    operator_pause: Duration,
}

#[derive(Clone)]
struct WorkerExecutionContext {
    run: Run,
    execution_epoch: usize,
    root_worktree: PathBuf,
    slice_base_sha: String,
    dependency_summary: BTreeMap<String, String>,
    cancel: CancellationToken,
    runner: Arc<dyn Runner>,
    config: WorkflowConfig,
    cockpit_mode: CockpitMode,
    economics: RunEconomicsRecorder,
    verification_cache: VerificationCommandCache,
    native_pi_tui_worker: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // CA-03 identity propagation is completed at the downstream sinks.
struct WorkerAttemptContext {
    run_id: String,
    phase: String,
    slice_id: String,
    // Display retry budget remains distinct from immutable daemon launch identity.
    attempt: usize,
    launch_id: Option<i64>,
    launch_stem: Option<String>,
    timeout_seconds: u64,
    no_output_warning_seconds: u64,
    termination_grace_seconds: u64,
    native_pi_tui_worker: bool,
}

#[allow(dead_code)] // CA-03 identity propagation is completed at the downstream sinks.
struct AgentCallContext<'a> {
    phase: &'a str,
    slice_id: &'a str,
    attempt: usize,
    launch_id: Option<i64>,
    launch_stem: Option<&'a str>,
}

struct ValidWorkerAttempt {
    result: WorkerResult,
    launch_id: i64,
    launch_stem: String,
    branch: String,
    worktree: PathBuf,
    output_path: PathBuf,
    _terminal_guard: Option<WorkerAttemptTerminalGuard>,
}

enum WorkerAttemptRunResult {
    Valid(Box<ValidWorkerAttempt>),
    Continue,
}

struct WorkerAttemptRunRequest<'a> {
    run: &'a Run,
    slice: &'a Slice,
    attempt: usize,
    launch_id: i64,
    launch_stem: &'a str,
    runner: Arc<dyn Runner>,
    runner_metadata: &'a RunnerMetadata,
    handoff: &'a Handoff,
    prompt: String,
    worker_worktree: &'a Path,
    worker_branch: &'a str,
    output_path: &'a Path,
    config: &'a WorkflowConfig,
    economics: &'a RunEconomicsRecorder,
    cancel: &'a CancellationToken,
    worker_token: &'a str,
    cockpit_mode: CockpitMode,
    native_pi_tui_worker: bool,
    primary_failure: &'a mut Option<String>,
    secondary_failures: &'a mut Vec<String>,
    last_failure: &'a mut String,
}

struct WorkerAttemptFailureRecord<'a> {
    run: &'a Run,
    slice: &'a Slice,
    attempt: usize,
    envelope_retry: usize,
    phase: &'a str,
    failure_kind: &'a str,
    summary: &'a str,
    evidence_path: &'a Path,
    retry_disposition: &'a str,
    repair_disposition: &'a str,
    primary_failure: Option<&'a str>,
    secondary_failures: &'a [String],
}

struct TargetedSliceRepairRequest<'a> {
    run: &'a Run,
    slice: &'a Slice,
    attempt: usize,
    runner: Arc<dyn Runner>,
    handoff: &'a Handoff,
    worker_worktree: &'a Path,
    slice_base_sha: &'a str,
    check_path: &'a Path,
    check: &'a CheckResult,
    config: &'a WorkflowConfig,
    economics: &'a RunEconomicsRecorder,
    verification_cache: &'a VerificationCommandCache,
    cancel: &'a CancellationToken,
    cockpit_mode: CockpitMode,
    native_pi_tui_worker: bool,
    all_checks: &'a mut Vec<CheckResult>,
}

struct PreparedWorkerLaunch {
    ledger: WorkerAttemptLedger,
    token: String,
    handoff: Handoff,
    handoff_path: PathBuf,
    output_path: PathBuf,
}

struct PreparedRunWorkerLaunch {
    ledger: WorkerAttemptLedger,
    token: String,
    output_path: PathBuf,
}

struct WorkerAttemptTerminalGuard {
    state: StateStore,
    launch_id: i64,
}

impl WorkerAttemptTerminalGuard {
    fn new(state: &StateStore, launch_id: i64) -> Self {
        Self {
            state: state.clone(),
            launch_id,
        }
    }
}

impl Drop for WorkerAttemptTerminalGuard {
    fn drop(&mut self) {
        let _ = self.state.finish_worker_attempt(
            self.launch_id,
            "failed",
            "worker launch exited without an explicit terminal transition",
        );
    }
}

struct AgentLaunchIncidentContext<'a> {
    run: &'a Run,
    phase: &'a str,
    slice_id: &'a str,
    attempt: usize,
    runner_name: &'a str,
    metadata: &'a RunnerMetadata,
}

enum CockpitWorkerJobError {
    Fallback(String),
    Worker(anyhow::Error),
}

impl WorkerAttemptContext {
    fn wire_attempt(&self) -> usize {
        self.attempt
    }

    fn cockpit_launch_identity(&self) -> usize {
        self.launch_id
            .filter(|launch_id| *launch_id > 0)
            .and_then(|launch_id| usize::try_from(launch_id).ok())
            .unwrap_or(self.attempt)
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        run_id: &str,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        launch_id: Option<i64>,
        launch_stem: Option<&str>,
        config: &WorkflowConfig,
        native_pi_tui_worker: bool,
    ) -> Self {
        Self {
            run_id: run_id.to_string(),
            phase: phase.to_string(),
            slice_id: slice_id.to_string(),
            attempt,
            launch_id,
            launch_stem: launch_stem.map(str::to_string),
            timeout_seconds: config.worker_attempt_timeout_seconds,
            no_output_warning_seconds: config.worker_no_output_warning_seconds,
            termination_grace_seconds: config.worker_termination_grace_seconds,
            native_pi_tui_worker,
        }
    }
}

impl Manager {
    pub fn new(paths: Paths, state: StateStore) -> Self {
        Self {
            paths,
            state,
            runner_override: None,
            active: Arc::new(ActiveRuns::default()),
        }
    }

    #[allow(dead_code)]
    pub fn with_runner(paths: Paths, state: StateStore, runner: Arc<dyn Runner>) -> Self {
        Self {
            paths,
            state,
            runner_override: Some(runner),
            active: Arc::new(ActiveRuns::default()),
        }
    }

    pub fn active_run_count(&self) -> usize {
        self.active.count()
    }

    pub(crate) fn notify_worker_question_attention(&self, question: &WorkerQuestion) {
        notify_origin_worker_question_attention(&self.state, question);
    }

    pub(crate) fn notify_replan_attention(&self, run_id: &str, proposal: &ReplanProposal) {
        if proposal.state != crate::domain::ReplanProposalState::Pending {
            return;
        }
        let Ok(Some(run)) = self.state.get_run(run_id) else {
            return;
        };
        notify_origin_replan_attention(&self.state, &run, proposal);
    }

    fn progress_reporter(&self, run_id: &str) -> ProgressReporter {
        ProgressReporter::new(self.state.clone(), run_id)
    }

    fn mark_progress(
        &self,
        run_id: &str,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        command: &str,
        message: &str,
    ) {
        self.progress_reporter(run_id).mark(&ProgressScope::new(
            phase, slice_id, attempt, command, message,
        ));
    }

    fn block_if_pending_replan(&self, run: &Run, checkpoint: &str) -> Result<()> {
        let pending = self.state.pending_replan_proposals(&run.id)?;
        if pending.is_empty() {
            return Ok(());
        }
        let ids = pending
            .iter()
            .map(|proposal| proposal.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut commands = Vec::new();
        for proposal in &pending {
            commands.extend(replan_decision_commands(&run.id, &proposal.id));
        }
        let frontier_attention = frontier_replan_attention_reasons(&pending);
        let message = if frontier_attention.is_empty() {
            format!("awaiting replan decision for {ids} before {checkpoint}")
        } else {
            format!(
                "awaiting replan decision for {ids} before {checkpoint}; {}",
                frontier_attention.join("; ")
            )
        };
        self.mark_progress(&run.id, "awaiting_replan", "", 0, "replan", &message);
        self.state.record_event(
            &run.id,
            "replan_checkpoint_blocked",
            &workflow_events::ReplanCheckpointBlockedPayload::new(
                pending.iter().map(|proposal| proposal.id.clone()).collect(),
                checkpoint,
                &message,
                commands.clone(),
            ),
        )?;
        for proposal in &pending {
            self.notify_attention_for_replan(run, proposal);
        }
        Err(BlockedError::new(format!("{message}; decide with: {}", commands.join("; "))).into())
    }

    fn classify_pending_frontier_proposals_at_replan_checkpoint(
        &self,
        run: &Run,
        checkpoint: &str,
    ) -> Result<usize> {
        let (envelope, budget_snapshot) = self.state.get_frontier_state(&run.id)?;
        let Some(envelope) = envelope else {
            return Ok(0);
        };
        let budget_snapshot = budget_snapshot.unwrap_or_default();
        if envelope.autonomy_level == AutonomyLevel::Off {
            return Ok(0);
        }
        let pending = self.state.pending_replan_proposals(&run.id)?;
        if pending.is_empty() {
            return Ok(0);
        }
        let all_proposals = self.state.list_replan_proposals(&run.id)?;
        let graph =
            self.frontier_slice_graph_for_path(Path::new(&run.repo_path), &all_proposals)?;
        let envelope_hash = mission_envelope_hash(&envelope)?;
        let mut recorded = 0;
        for proposal in pending {
            let Some(classification) = self.classify_frontier_proposal_at_checkpoint(
                &envelope,
                &budget_snapshot,
                &graph,
                &envelope_hash,
                &proposal,
            )?
            else {
                continue;
            };
            let proposal = self.state.replace_replan_frontier_classification(
                &run.id,
                &proposal.id,
                &classification,
            )?;
            self.record_frontier_classification_event(
                run,
                checkpoint,
                &proposal,
                &classification,
                true,
            )?;
            recorded += 1;
        }
        Ok(recorded)
    }

    fn settle_replan_checkpoint(
        &self,
        run: &mut Run,
        checkpoint: &str,
        integration_worktree: &Path,
        worker_layers: &mut VecDeque<Vec<Slice>>,
        gate_slices: &mut Vec<Slice>,
    ) -> Result<()> {
        self.apply_accepted_replan_proposals_at_checkpoint(
            run,
            checkpoint,
            integration_worktree,
            worker_layers,
            gate_slices,
        )?;
        let (envelope, _) = self.state.get_frontier_state(&run.id)?;
        match envelope.as_ref().map(|envelope| envelope.autonomy_level) {
            Some(AutonomyLevel::Promote | AutonomyLevel::Run) => {
                self.auto_accept_frontier_proposals_at_replan_checkpoint(
                    run,
                    checkpoint,
                    integration_worktree,
                    worker_layers,
                    gate_slices,
                )?;
            }
            Some(AutonomyLevel::Shadow) => {
                self.classify_pending_frontier_proposals_at_replan_checkpoint(run, checkpoint)?;
            }
            Some(AutonomyLevel::Off) | None => {}
        }
        self.block_if_pending_replan(run, checkpoint)
    }

    fn auto_accept_frontier_proposals_at_replan_checkpoint(
        &self,
        run: &mut Run,
        checkpoint: &str,
        integration_worktree: &Path,
        worker_layers: &mut VecDeque<Vec<Slice>>,
        gate_slices: &mut Vec<Slice>,
    ) -> Result<usize> {
        let (envelope, _) = self.state.get_frontier_state(&run.id)?;
        let Some(envelope) = envelope else {
            return Ok(0);
        };
        if !matches!(
            envelope.autonomy_level,
            AutonomyLevel::Promote | AutonomyLevel::Run
        ) {
            return Ok(0);
        }
        let envelope_hash = mission_envelope_hash(&envelope)?;
        let mut accepted = 0;
        loop {
            let pending = self.state.pending_replan_proposals(&run.id)?;
            if pending.is_empty() {
                break;
            }
            let all_proposals = self.state.list_replan_proposals(&run.id)?;
            let graph = self.frontier_slice_graph_for_path(integration_worktree, &all_proposals)?;
            let (_, budget_snapshot) = self.state.get_frontier_state(&run.id)?;
            let budget_snapshot = budget_snapshot.unwrap_or_default();
            let mut accepted_one = false;
            for proposal in pending {
                let Some(classification) = self.classify_frontier_proposal_at_checkpoint(
                    &envelope,
                    &budget_snapshot,
                    &graph,
                    &envelope_hash,
                    &proposal,
                )?
                else {
                    self.record_frontier_auto_accept_skip(
                        run,
                        checkpoint,
                        &proposal,
                        "unsupported_proposal_kind",
                        &[],
                    )?;
                    continue;
                };
                if !frontier_auto_accept_gate_allows(&envelope, &classification) {
                    let proposal = self.state.replace_replan_frontier_classification(
                        &run.id,
                        &proposal.id,
                        &classification,
                    )?;
                    self.record_frontier_classification_event(
                        run,
                        checkpoint,
                        &proposal,
                        &classification,
                        false,
                    )?;
                    self.record_frontier_auto_accept_skip(
                        run,
                        checkpoint,
                        &proposal,
                        "classification_not_auto_accept_eligible",
                        &classification.reason_codes,
                    )?;
                    continue;
                }
                let budget_before = budget_snapshot.clone();
                let budget_after = frontier_budget_after_auto_accept(&budget_before);
                let rationale = format!(
                    "frontier policy auto-accepted Tier-1 add_followup_slice proposal within mission envelope at {checkpoint}"
                );
                let apply_mode = followup_apply_mode_for_autonomy(envelope.autonomy_level);
                let proposal = self.state.auto_accept_replan_proposal_with_budget(
                    &run.id,
                    &proposal.id,
                    &rationale,
                    &classification,
                    &budget_before,
                    &budget_after,
                    checkpoint,
                    apply_mode.as_str(),
                )?;
                self.apply_followup_proposal_at_checkpoint(
                    run,
                    checkpoint,
                    integration_worktree,
                    worker_layers,
                    gate_slices,
                    &proposal,
                    apply_mode,
                )?;
                accepted += 1;
                accepted_one = true;
                break;
            }
            if !accepted_one {
                break;
            }
        }
        Ok(accepted)
    }

    fn classify_frontier_proposal_at_checkpoint(
        &self,
        envelope: &MissionEnvelope,
        budget_snapshot: &FrontierBudgetState,
        graph: &SliceGraphView,
        envelope_hash: &str,
        proposal: &ReplanProposal,
    ) -> Result<Option<FrontierClassification>> {
        let Some(draft) = add_followup_slice_draft_from_proposal(proposal) else {
            return Ok(None);
        };
        let view = FollowupProposalView {
            proposal_id: proposal.id.as_str(),
            source_slice_id: proposal.source.slice_id.as_str(),
            change: FollowupProposalChange::AddFollowupSlice(&draft),
            source_must_ask_if_hits: &[],
            envelope_must_ask_if_hits: &[],
            external_dependency_claims: &[],
            changes_existing_dependencies: false,
            changes_existing_acceptance: false,
            changes_verify_profile: false,
            changes_policy_or_schema: false,
            needs_operator_context: false,
            ambiguity_markers: &[],
        };
        let decision = classify_followup_proposal(envelope, graph, &view, budget_snapshot);
        Ok(Some(FrontierClassification {
            tier: decision.tier.as_str().to_string(),
            reason_codes: decision
                .reason_codes
                .iter()
                .map(|reason| reason.as_str().to_string())
                .collect(),
            classified_at: Utc::now(),
            envelope_hash: envelope_hash.to_string(),
            budget_snapshot: budget_snapshot.clone(),
            autonomy_level: envelope.autonomy_level,
        }))
    }

    fn record_frontier_classification_event(
        &self,
        run: &Run,
        checkpoint: &str,
        proposal: &ReplanProposal,
        classification: &FrontierClassification,
        record_only: bool,
    ) -> Result<()> {
        self.state.record_event(
            &run.id,
            workflow_events::FRONTIER_CLASSIFIED,
            &workflow_events::FrontierClassifiedPayload::new(
                &proposal.id,
                checkpoint,
                classification,
                record_only,
                false,
            ),
        )
    }

    fn record_frontier_auto_accept_skip(
        &self,
        run: &Run,
        checkpoint: &str,
        proposal: &ReplanProposal,
        reason: &str,
        reason_codes: &[String],
    ) -> Result<()> {
        let tier = proposal
            .frontier_classification
            .as_ref()
            .map(|classification| classification.tier.as_str())
            .unwrap_or("");
        let event_type = if matches!(tier, "tier_3" | "stop") {
            "frontier_auto_accept_stopped"
        } else {
            "frontier_auto_accept_skipped"
        };
        self.state.record_event(
            &run.id,
            event_type,
            &json!({
                "proposal_id": proposal.id,
                "checkpoint": checkpoint,
                "reason": reason,
                "tier": tier,
                "reason_codes": reason_codes,
                "state": proposal.state,
            }),
        )
    }

    fn frontier_slice_graph_for_path(
        &self,
        repo_path: &Path,
        proposals: &[ReplanProposal],
    ) -> Result<SliceGraphView> {
        let slices = artifact::Store::new(repo_path)
            .load_slices()?
            .into_iter()
            .map(|slice| {
                let generation = slice
                    .provenance()
                    .map(|provenance| provenance.generation as i64)
                    .unwrap_or(0);
                SliceGraphSlice {
                    id: slice.id,
                    goal: slice.goal,
                    status: if is_open_status(&slice.status) {
                        SliceGraphSliceStatus::Open
                    } else {
                        SliceGraphSliceStatus::Closed
                    },
                    generation,
                }
            })
            .collect();
        let proposal_nodes = proposals
            .iter()
            .filter_map(|proposal| {
                add_followup_slice_draft_from_proposal(proposal).map(|draft| {
                    ProposalGraphNode::from_draft(proposal.id.clone(), proposal.state, &draft)
                })
            })
            .collect();
        Ok(SliceGraphView {
            slices,
            proposals: proposal_nodes,
            no_frontier: false,
            cancel_requested: false,
            replan_apply_incomplete: false,
        })
    }

    fn notify_attention_for_replan(&self, run: &Run, proposal: &ReplanProposal) {
        if proposal.state != crate::domain::ReplanProposalState::Pending {
            return;
        }
        notify_origin_replan_attention(&self.state, run, proposal);
    }

    fn run_read_model(&self, run: &Run, options: RunReadModelOptions) -> Result<RunReadModel> {
        RunReadModelBuilder::new(&self.state).snapshot(run, options)
    }

    fn plan_revisions_for_run(&self, run: &Run) -> Result<PlanRevisions> {
        RunReadModelBuilder::new(&self.state).plan_revisions_for_run(run)
    }

    fn slice_areas_with_accepted_revision_grants(
        &self,
        run_id: &str,
        slice_id: &str,
        slice_areas: &[String],
    ) -> Result<Vec<String>> {
        let proposals = self.state.list_replan_proposals(run_id)?;
        let mut areas = slice_areas.to_vec();
        for proposal in proposals {
            if proposal.state != crate::domain::ReplanProposalState::Accepted {
                continue;
            }
            if proposal.source.slice_id != slice_id {
                continue;
            }
            for path in authorized_paths_from_proposal(&proposal) {
                if !areas.iter().any(|area| area == &path) {
                    areas.push(path);
                }
            }
        }
        Ok(areas)
    }

    fn write_worker_handoff_with_plan_revisions(
        &self,
        store: &artifact::Store,
        run: &Run,
        handoff: &Handoff,
        name: &str,
    ) -> Result<PathBuf> {
        let path = store.write_handoff_named(&run.id, handoff, name)?;
        let mut value = serde_json::to_value(handoff)?;
        if let serde_json::Value::Object(fields) = &mut value {
            fields.insert(
                "plan_revisions".to_string(),
                serde_json::to_value(self.plan_revisions_for_run(run)?)?,
            );
        }
        artifact::write_json(&path, &value)?;
        Ok(path)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_followup_worker_launch(
        &self,
        run: &Run,
        slice: &Slice,
        worker_retry_ordinal: usize,
        repair_ordinal: usize,
        envelope_retry_ordinal: usize,
        kind: &str,
        role: &str,
        base_ref: &str,
        source_handoff: &Handoff,
    ) -> Result<PreparedWorkerLaunch> {
        let root_worktree = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        let ledger = self.state.allocate_worker_attempt(
            &run.id,
            &slice.id,
            self.state.current_run_execution_epoch(&run.id)?,
            worker_retry_ordinal,
            repair_ordinal,
            envelope_retry_ordinal,
            kind,
            &root_worktree,
        )?;
        let token = new_worker_token();
        if let Err(err) = self
            .state
            .store_worker_launch_token(&run.id, ledger.launch_id, &token)
        {
            let _ = self
                .state
                .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
            return Err(err);
        }
        let worktree = PathBuf::from(&ledger.worktree);
        let worktree_result = {
            let _git_lock = WORKTREE_ADD_LOCK
                .lock()
                .expect("worktree add mutex poisoned");
            gitutil::worktree_add(&run.repo_path, &worktree, &ledger.branch, base_ref)
        };
        if let Err(err) = worktree_result {
            let _ = self
                .state
                .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
            return Err(err).context("create follow-up worker worktree");
        }
        let store = artifact::Store::new(&run.repo_path);
        let output_path = store.output_path(&run.id, &format!("{}.json", ledger.output_stem));
        let mut handoff = source_handoff.clone();
        handoff.role = role.to_string();
        handoff.worktree_path = ledger.worktree.clone();
        handoff.branch = ledger.branch.clone();
        handoff.output_path = output_path.to_string_lossy().to_string();
        let handoff_path = match self.write_worker_handoff_with_plan_revisions(
            &store,
            run,
            &handoff,
            &ledger.output_stem,
        ) {
            Ok(path) => path,
            Err(err) => {
                let _ =
                    self.state
                        .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
                return Err(err);
            }
        };
        Ok(PreparedWorkerLaunch {
            ledger,
            token,
            handoff,
            handoff_path,
            output_path,
        })
    }

    fn prepare_integration_repair_launch(
        &self,
        run: &Run,
        repair_ordinal: usize,
        base_ref: &str,
    ) -> Result<PreparedRunWorkerLaunch> {
        let root_worktree = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        let ledger = self.state.allocate_run_worker_attempt(
            &run.id,
            INTEGRATION_REPAIR_SCOPE_ID,
            self.state.current_run_execution_epoch(&run.id)?,
            0,
            repair_ordinal,
            0,
            "integration-repair",
            &root_worktree,
        )?;
        let token = new_worker_token();
        if let Err(err) = self
            .state
            .store_worker_launch_token(&run.id, ledger.launch_id, &token)
        {
            let _ = self
                .state
                .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
            return Err(err);
        }
        let worktree = PathBuf::from(&ledger.worktree);
        let worktree_result = {
            let _git_lock = WORKTREE_ADD_LOCK
                .lock()
                .expect("worktree add mutex poisoned");
            gitutil::worktree_add(&run.repo_path, &worktree, &ledger.branch, base_ref)
        };
        if let Err(err) = worktree_result {
            let _ = self
                .state
                .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
            return Err(err).context("create integration repair worker worktree");
        }
        let output_path = artifact::Store::new(&run.repo_path)
            .output_path(&run.id, &format!("{}.json", ledger.output_stem));
        Ok(PreparedRunWorkerLaunch {
            ledger,
            token,
            output_path,
        })
    }

    fn record_cockpit_launch(&self, run: &Run, mode: CockpitMode) -> Result<CockpitMode> {
        match open_default_run_cockpit(run, mode, &self.paths.root) {
            Ok(CockpitLaunch::Opened(opened)) => {
                let effective_mode = opened.mode;
                self.state.record_event(
                    &run.id,
                    workflow_events::COCKPIT_READY,
                    &workflow_events::CockpitReadyPayload {
                        adapter: opened.adapter,
                        mode: effective_mode.as_str().to_string(),
                        workspace: opened.workspace_label,
                        panes: opened.pane_labels,
                        source_of_truth: "daemon_state".to_string(),
                        planner: "cockpit_layout_v2_observability_only".to_string(),
                    },
                )?;
                Ok(effective_mode)
            }
            Ok(CockpitLaunch::SkippedDirect) => Ok(CockpitMode::Direct),
            Err(unavailable) => {
                self.state.record_event(
                    &run.id,
                    workflow_events::RUN_INCIDENT,
                    &workflow_events::RunIncidentPayload::warning(
                        "cockpit_unavailable",
                        unavailable.message,
                    )
                    .with_extra("adapter", unavailable.adapter)
                    .with_extra("mode", unavailable.mode.as_str())
                    .with_extra("remediation", unavailable.remediation)
                    .with_extra("fallback", "direct")
                    .with_extra("layout", "cockpit_layout_v2_observability_only")
                    .with_extra("source_of_truth", "daemon_state"),
                )?;
                Ok(CockpitMode::Direct)
            }
        }
    }

    fn worker_event_sink(&self, context: &WorkerAttemptContext) -> RunnerEventSink {
        let state = self.state.clone();
        let context = context.clone();
        Arc::new(move |event: RunnerEvent| {
            let _ = state.observe_worker_attempt(
                &context.run_id,
                &context.phase,
                &context.slice_id,
                context.wire_attempt(),
                context.launch_id,
                event.pid,
                event.kind.as_str(),
                &event.text,
                context.timeout_seconds,
                context.no_output_warning_seconds,
            );
        })
    }

    fn run_supervised_worker_job(
        &self,
        runner: Arc<dyn Runner>,
        job: Job,
        cancel: &CancellationToken,
        context: WorkerAttemptContext,
    ) -> SupervisedWorkerJobOutcome {
        self.run_supervised_worker_job_with(job, cancel, context, move |job, cancel, events| {
            runner.run(job, cancel, events)
        })
    }

    fn run_supervised_worker_job_with<F>(
        &self,
        mut job: Job,
        cancel: &CancellationToken,
        context: WorkerAttemptContext,
        run_job: F,
    ) -> SupervisedWorkerJobOutcome
    where
        F: FnOnce(
            Job,
            CancellationToken,
            Option<RunnerEventSink>,
        ) -> Result<crate::agent::ResultData>,
    {
        job.termination_grace_seconds = context.termination_grace_seconds;
        let events = Some(self.worker_event_sink(&context));
        let attempt_cancel = CancellationToken::new();
        let timed_out = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let parent_cancel = cancel.clone();
        let timeout =
            (context.timeout_seconds > 0).then(|| Duration::from_secs(context.timeout_seconds));
        let timeout_cancel = attempt_cancel.clone();
        let timeout_flag = timed_out.clone();
        let done_flag = done.clone();
        let state = self.state.clone();
        let supervisor_context = context.clone();
        let operator_pause = Arc::new(Mutex::new(Duration::ZERO));
        let supervisor_operator_pause = operator_pause.clone();
        let supervisor = thread::spawn(move || {
            let mut active_elapsed = Duration::ZERO;
            let mut last_tick = Instant::now();
            loop {
                let now = Instant::now();
                let delta = now.saturating_duration_since(last_tick);
                last_tick = now;
                let paused = state
                    .has_pending_worker_question_with_launch_id(
                        &supervisor_context.run_id,
                        &supervisor_context.slice_id,
                        supervisor_context.wire_attempt(),
                        supervisor_context.launch_id,
                    )
                    .unwrap_or(false);
                if paused {
                    if let Ok(mut total) = supervisor_operator_pause.lock() {
                        *total += delta;
                    }
                } else {
                    active_elapsed += delta;
                }
                if done_flag.load(Ordering::SeqCst) {
                    return;
                }
                if parent_cancel.is_cancelled() {
                    timeout_cancel.cancel();
                    return;
                }
                if timeout.is_some_and(|limit| active_elapsed >= limit) {
                    timeout_flag.store(true, Ordering::SeqCst);
                    timeout_cancel.cancel();
                    return;
                }
                thread::park_timeout(Duration::from_millis(100));
            }
        });
        let supervisor_thread = supervisor.thread().clone();

        let mut result = run_job(job, attempt_cancel, events);
        done.store(true, Ordering::SeqCst);
        supervisor_thread.unpark();
        let _ = supervisor.join();
        if timed_out.load(Ordering::SeqCst) {
            let message = format!(
                "worker attempt {} exceeded worker_attempt_timeout_seconds={}",
                context.attempt, context.timeout_seconds
            );
            result = self
                .state
                .record_event(
                    &context.run_id,
                    "worker_attempt_timeout",
                    &workflow_events::WorkerAttemptTimeoutPayload::new(
                        &context.phase,
                        &context.slice_id,
                        context.attempt,
                        context.launch_id,
                        context.timeout_seconds,
                        &message,
                    ),
                )
                .and_then(|()| Err(anyhow!(message)));
        }
        let operator_pause = operator_pause
            .lock()
            .map(|duration| *duration)
            .unwrap_or(Duration::ZERO);
        SupervisedWorkerJobOutcome {
            result,
            operator_pause,
        }
    }

    fn run_recorded_agent_job(
        &self,
        runner: Arc<dyn Runner>,
        job: Job,
        cancel: &CancellationToken,
        context: WorkerAttemptContext,
        economics: &RunEconomicsRecorder,
        call: AgentCallContext<'_>,
    ) -> Result<crate::agent::ResultData> {
        let kind = job.kind.clone();
        let runner_name = runner.name().to_string();
        let runner_metadata = runner.metadata();
        let started_at = Instant::now();
        let outcome = self.run_supervised_worker_job(runner, job, cancel, context.clone());
        let operator_pause = outcome.operator_pause;
        match outcome.result {
            Ok(data) => {
                let duration = started_at.elapsed().saturating_sub(operator_pause);
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "succeeded",
                    duration,
                    operator_pause,
                    Some(&data.usage),
                    "",
                ));
                self.record_contract_warnings(&context, &runner_name, &data.contract_warnings);
                Ok(data)
            }
            Err(err) => {
                let error = err.to_string();
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "failed",
                    started_at.elapsed().saturating_sub(operator_pause),
                    operator_pause,
                    None,
                    &error,
                ));
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_recorded_slice_worker_job(
        &self,
        runner: Arc<dyn Runner>,
        job: Job,
        cancel: &CancellationToken,
        context: WorkerAttemptContext,
        economics: &RunEconomicsRecorder,
        call: AgentCallContext<'_>,
        run: &Run,
        cockpit_mode: CockpitMode,
        output_path: &Path,
    ) -> Result<crate::agent::ResultData> {
        let Some(spec) = runner.pi_command_spec() else {
            return self.run_recorded_agent_job(runner, job, cancel, context, economics, call);
        };
        if cockpit_mode == CockpitMode::Direct {
            return self.run_recorded_agent_job(runner, job, cancel, context, economics, call);
        }

        let kind = job.kind.clone();
        let runner_name = runner.name().to_string();
        let runner_metadata = runner.metadata();
        let started_at = Instant::now();
        let direct_runner = runner.clone();
        let run = run.clone();
        let output_path = output_path.to_path_buf();
        let context_for_job = context.clone();
        let outcome = self.run_supervised_worker_job_with(
            job,
            cancel,
            context.clone(),
            move |job, cancel, events| {
                self.run_herdr_worker_or_direct(
                    direct_runner,
                    spec,
                    job,
                    cancel,
                    events,
                    &context_for_job,
                    &run,
                    cockpit_mode,
                    &output_path,
                )
            },
        );
        let operator_pause = outcome.operator_pause;
        match outcome.result {
            Ok(data) => {
                let duration = started_at.elapsed().saturating_sub(operator_pause);
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "succeeded",
                    duration,
                    operator_pause,
                    Some(&data.usage),
                    "",
                ));
                self.record_contract_warnings(&context, &runner_name, &data.contract_warnings);
                Ok(data)
            }
            Err(err) => {
                let error = err.to_string();
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "failed",
                    started_at.elapsed().saturating_sub(operator_pause),
                    operator_pause,
                    None,
                    &error,
                ));
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_herdr_worker_or_direct(
        &self,
        direct_runner: Arc<dyn Runner>,
        spec: PiCommandSpec,
        job: Job,
        cancel: CancellationToken,
        events: Option<RunnerEventSink>,
        context: &WorkerAttemptContext,
        run: &Run,
        cockpit_mode: CockpitMode,
        output_path: &Path,
    ) -> Result<crate::agent::ResultData> {
        let result = if context.native_pi_tui_worker && !matches!(cockpit_mode, CockpitMode::Direct)
        {
            self.try_run_herdr_tui_worker_job(
                &spec,
                &job,
                cancel.clone(),
                events.clone(),
                context,
                run,
                cockpit_mode,
                output_path,
            )
        } else {
            self.try_run_herdr_worker_job(
                &spec,
                &job,
                cancel.clone(),
                events.clone(),
                context,
                run,
                cockpit_mode,
                output_path,
            )
        };
        match result {
            Ok(data) => Ok(data),
            Err(CockpitWorkerJobError::Fallback(message)) => {
                self.record_cockpit_worker_fallback(run, context, cockpit_mode, &message)?;
                direct_runner.run(job, cancel, events)
            }
            Err(CockpitWorkerJobError::Worker(err)) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_run_herdr_worker_job(
        &self,
        spec: &PiCommandSpec,
        job: &Job,
        cancel: CancellationToken,
        events: Option<RunnerEventSink>,
        context: &WorkerAttemptContext,
        run: &Run,
        cockpit_mode: CockpitMode,
        output_path: &Path,
    ) -> std::result::Result<crate::agent::ResultData, CockpitWorkerJobError> {
        if cancel.is_cancelled() {
            return Err(CockpitWorkerJobError::Worker(anyhow!("job cancelled")));
        }
        let artifacts = artifact::Store::new(&run.repo_path)
            .pi_wrapper_artifacts_for_output_path(output_path)
            .map_err(|err| CockpitWorkerJobError::Fallback(err.to_string()))?;
        let wrapper_command = prepare_pi_wrapper_artifacts(spec, job, &artifacts)
            .map_err(|err| CockpitWorkerJobError::Fallback(err.to_string()))?;
        let command = worker_activity_pane_command(
            &wrapper_command,
            &artifacts.stdout_path,
            &artifacts.status_path,
            &artifacts.exit_path,
        );
        let worker_request = CockpitWorkerPaneRequest {
            run_id: run.id.clone(),
            slice_id: context.slice_id.clone(),
            attempt: context.attempt,
            launch_id: context.launch_id,
            command,
            cwd: job.cwd.clone(),
            env: vec![
                ("KHAZAD_COCKPIT_WORKER".to_string(), "1".to_string()),
                (
                    "KHAZAD_COCKPIT_SOURCE_OF_TRUTH".to_string(),
                    "kd_artifacts".to_string(),
                ),
            ],
        };
        let opened = match open_default_worker_pane(
            run,
            cockpit_mode,
            &self.paths.root,
            &worker_request,
        ) {
            Ok(CockpitWorkerLaunch::Opened(opened)) => opened,
            Ok(CockpitWorkerLaunch::SkippedDirect) => {
                return Err(CockpitWorkerJobError::Fallback(
                    "cockpit mode resolved to direct before worker pane launch".to_string(),
                ));
            }
            Err(unavailable) => {
                return Err(CockpitWorkerJobError::Worker(anyhow!(
                    "Herdr worker pane launch is uncertain; refusing direct fallback under the same launch identity: {}",
                    unavailable.message
                )));
            }
        };
        self.state
            .record_event(
                &run.id,
                workflow_events::COCKPIT_WORKER_READY,
                &json!({
                    "adapter": opened.adapter,
                    "mode": opened.mode.as_str(),
                    "workspace": opened.workspace_label,
                    "pane": opened.pane_label,
                    "pane_id": opened.pane_id,
                    "slice_id": context.slice_id.clone(),
                    "attempt": context.attempt,
                    "launch_id": context.launch_id,
                    "launch_stem": context.launch_stem,
                    "launch_identity": context.cockpit_launch_identity(),
                    "source_of_truth": "kd_artifact_files",
                    "layout_planner": "cockpit_layout_v2",
                    "worker_slot_name": opened.slot_name,
                    "worker_slot_index": opened.slot_index,
                    "worker_region": opened.slot_region,
                }),
            )
            .map_err(CockpitWorkerJobError::Worker)?;
        let pid = match wait_for_pi_wrapper_launch(&artifacts, Duration::from_secs(5), &events) {
            Ok(pid) => pid,
            Err(err) if cancel.is_cancelled() => {
                return Err(CockpitWorkerJobError::Worker(err.into()));
            }
            Err(PiWrapperLaunchError::BeforePi(message)) => {
                return Err(CockpitWorkerJobError::Fallback(message));
            }
            Err(err @ PiWrapperLaunchError::LaunchUncertain(_)) => {
                return Err(CockpitWorkerJobError::Worker(err.into()));
            }
        };
        collect_pi_wrapper_result(job, &artifacts, cancel, events, pid)
            .map_err(CockpitWorkerJobError::Worker)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_run_herdr_tui_worker_job(
        &self,
        spec: &PiCommandSpec,
        job: &Job,
        cancel: CancellationToken,
        events: Option<RunnerEventSink>,
        context: &WorkerAttemptContext,
        run: &Run,
        cockpit_mode: CockpitMode,
        output_path: &Path,
    ) -> std::result::Result<crate::agent::ResultData, CockpitWorkerJobError> {
        if cancel.is_cancelled() {
            return Err(CockpitWorkerJobError::Worker(anyhow!("job cancelled")));
        }
        let artifacts = artifact::Store::new(&run.repo_path)
            .pi_tui_worker_artifacts_for_output_path(output_path)
            .map_err(|err| CockpitWorkerJobError::Fallback(err.to_string()))?;
        let session_name = tui_worker_session_name(
            &run.id,
            &context.slice_id,
            context.cockpit_launch_identity(),
        );
        let argv = prepare_pi_tui_worker_artifacts(spec, job, &artifacts, &session_name)
            .map_err(|err| CockpitWorkerJobError::Fallback(err.to_string()))?;
        let mut env = job
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        env.push((
            "KHAZAD_WORKER_RESULT_PATH".to_string(),
            artifacts.result_path.to_string_lossy().to_string(),
        ));
        env.push(("KHAZAD_COCKPIT_WORKER".to_string(), "1".to_string()));
        env.push((
            "KHAZAD_COCKPIT_SOURCE_OF_TRUTH".to_string(),
            "kd_tui_result_artifact".to_string(),
        ));
        let worker_request = CockpitTuiWorkerRequest {
            run_id: run.id.clone(),
            slice_id: context.slice_id.clone(),
            attempt: context.attempt,
            launch_id: context.launch_id,
            name: session_name,
            argv,
            cwd: job.cwd.clone(),
            env,
        };
        let opened = match open_default_tui_worker_agent(
            run,
            cockpit_mode,
            &self.paths.root,
            &worker_request,
        ) {
            Ok(CockpitTuiWorkerLaunch::Opened(opened)) => opened,
            Ok(CockpitTuiWorkerLaunch::SkippedDirect) => {
                return Err(CockpitWorkerJobError::Fallback(
                    "cockpit mode resolved to direct before TUI worker launch".to_string(),
                ));
            }
            Err(unavailable) => {
                return Err(CockpitWorkerJobError::Worker(anyhow!(
                    "Herdr TUI worker launch is uncertain; refusing direct fallback under the same launch identity: {}",
                    unavailable.message
                )));
            }
        };
        self.state
            .record_event(
                &run.id,
                workflow_events::COCKPIT_WORKER_READY,
                &json!({
                    "adapter": opened.adapter.clone(),
                    "mode": opened.mode.as_str(),
                    "workspace": opened.workspace_label.clone(),
                    "pane": opened.pane_label.clone(),
                    "pane_id": opened.pane_id.clone(),
                    "terminal_id": opened.terminal_id.clone(),
                    "agent_name": opened.agent_name.clone(),
                    "slice_id": context.slice_id.clone(),
                    "attempt": context.attempt,
                    "launch_id": context.launch_id,
                    "launch_stem": context.launch_stem,
                    "launch_identity": context.cockpit_launch_identity(),
                    "source_of_truth": "kd_tui_result_artifact",
                    "layout_planner": "cockpit_layout_v2",
                    "worker_slot_name": opened.slot_name.clone(),
                    "worker_slot_index": opened.slot_index,
                    "worker_region": opened.slot_region.clone(),
                }),
            )
            .map_err(CockpitWorkerJobError::Worker)?;
        if let Some(sink) = &events {
            sink(RunnerEvent::started(None));
        }
        let result = wait_for_pi_tui_worker_result(&artifacts, cancel, events, opened.pane_id);
        result.map_err(CockpitWorkerJobError::Worker)
    }

    fn record_cockpit_worker_fallback(
        &self,
        run: &Run,
        context: &WorkerAttemptContext,
        mode: CockpitMode,
        message: &str,
    ) -> Result<()> {
        self.state.record_event(
            &run.id,
            workflow_events::RUN_INCIDENT,
            &workflow_events::RunIncidentPayload::warning("cockpit_worker_fallback", message)
                .with_extra("adapter", "herdr")
                .with_extra("mode", mode.as_str())
                .with_extra("slice_id", &context.slice_id)
                .with_extra("attempt", context.attempt)
                .with_extra("launch_id", context.launch_id)
                .with_extra("launch_stem", &context.launch_stem)
                .with_extra("launch_identity", context.cockpit_launch_identity())
                .with_extra("fallback", "direct")
                .with_extra("layout", "cockpit_layout_v2_observability_only")
                .with_extra("source_of_truth", "kd_artifact_files"),
        )
    }

    fn record_contract_warnings(
        &self,
        context: &WorkerAttemptContext,
        runner_name: &str,
        warnings: &[crate::pi_contract::PiContractWarning],
    ) {
        for warning in warnings {
            let _ = self.state.record_event(
                &context.run_id,
                workflow_events::RUN_INCIDENT,
                &workflow_events::RunIncidentPayload::warning(&warning.kind, &warning.message)
                    .with_extra("phase", &context.phase)
                    .with_extra("slice_id", &context.slice_id)
                    .with_extra("attempt", context.attempt)
                    .with_extra("launch_id", context.launch_id)
                    .with_extra("launch_stem", &context.launch_stem)
                    .with_extra("agent", runner_name),
            );
        }
    }

    fn classify_runner_launch_failure(
        &self,
        err: &(dyn Error + Send + Sync + 'static),
        metadata: &RunnerMetadata,
    ) -> Option<RunnerLaunchFailure> {
        err.downcast_ref::<RunnerError>()
            .and_then(|err| err.classify_launch_failure(metadata))
    }

    fn record_agent_launch_incident(
        &self,
        context: AgentLaunchIncidentContext<'_>,
        failure: &RunnerLaunchFailure,
    ) -> Result<()> {
        self.state.record_event(
            &context.run.id,
            workflow_events::RUN_INCIDENT,
            &workflow_events::RunIncidentPayload::error(&failure.failure_kind, &failure.summary)
                .with_failure_kind(&failure.failure_kind)
                .with_extra("phase", context.phase)
                .with_extra("slice_id", context.slice_id)
                .with_extra("attempt", context.attempt)
                .with_extra("agent", context.runner_name)
                .with_extra("agent_profile", &context.metadata.profile)
                .with_extra("agent_provider", &context.metadata.provider)
                .with_extra("agent_model", &context.metadata.model)
                .with_extra("agent_reasoning", &context.metadata.reasoning)
                .with_extra("agent_mode", &context.metadata.mode)
                .with_operator_action_required(failure.operator_action_required)
                .with_retryable(failure.retryable)
                .with_fix_commands(failure.fix_commands.clone()),
        )
    }

    fn ensure_repo_run_available(&self, repo_id: &str, allowed_run_id: Option<&str>) -> Result<()> {
        if let Some(active) = self.state.active_run_for_repo(repo_id, allowed_run_id)? {
            bail!(
                "repo already has active run {} on integration branch {}; wait, cancel it, or resume that run",
                active.id,
                active.integration_branch
            );
        }
        Ok(())
    }

    fn runner_for_options(
        &self,
        opts: &StartOptions,
        config: &WorkflowConfig,
    ) -> Result<Arc<dyn Runner>> {
        self.runner_for_parts(&opts.agent, &opts.pi_bin, &opts.pi_args, config)
    }

    fn runner_for_parts(
        &self,
        agent: &str,
        pi_bin: &str,
        pi_args: &[String],
        config: &WorkflowConfig,
    ) -> Result<Arc<dyn Runner>> {
        if let Some(runner) = &self.runner_override {
            return Ok(runner.clone());
        }
        let requested_agent = if agent.trim().is_empty() {
            std::env::var("KHAZAD_AGENT").unwrap_or_default()
        } else {
            agent.to_string()
        };
        let requested_pi_bin = if pi_bin.trim().is_empty() {
            std::env::var("KHAZAD_PI_BIN").unwrap_or_default()
        } else {
            pi_bin.to_string()
        };
        let requested_pi_args = if pi_args.is_empty() {
            std::env::var("KHAZAD_PI_ARGS")
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect()
        } else {
            pi_args.to_vec()
        };
        let agent_probe = if requested_agent.trim().is_empty() {
            config.agent.trim()
        } else {
            requested_agent.trim()
        };
        let profiles = if agent_probe.eq_ignore_ascii_case("fake") {
            AgentProfilesConfig::default()
        } else {
            self.read_operator_agent_profiles()?
        };
        let effective = resolve_effective_worker_profile(ProfileResolveInput {
            agent: requested_agent,
            pi_bin: requested_pi_bin,
            pi_args: requested_pi_args,
            config: config.clone(),
            profiles,
        })?;
        Ok(runner_from_spec(effective.spec))
    }

    fn read_operator_agent_profiles(&self) -> Result<AgentProfilesConfig> {
        let path = self.paths.agent_profiles_file();
        if !path.exists() {
            return Ok(AgentProfilesConfig::default());
        }
        artifact::read_agent_profiles_file(&path)
    }

    pub fn init_repo(&self, repo_path: impl AsRef<Path>) -> Result<Repo> {
        let root = gitutil::repo_root(repo_path).context("resolve git repo root")?;
        let store = artifact::Store::new(&root);
        store.ensure_layout().context("ensure workflow layout")?;
        let repo = Repo {
            id: paths::repo_id(&root),
            path: root.to_string_lossy().to_string(),
            created_at: Utc::now(),
        };
        self.state.upsert_repo(&repo)?;
        Ok(repo)
    }

    pub fn validate_slices(&self, repo_path: impl AsRef<Path>) -> Result<SliceValidationReport> {
        let root = gitutil::repo_root(repo_path).context("resolve git repo root")?;
        artifact::Store::new(root).validate_slices_report()
    }

    pub fn create_slice(&self, draft: SliceDraft) -> Result<SliceWriteResult> {
        let repo = self.init_repo(&draft.repo_path)?;
        let slice = Slice {
            id: draft.id,
            title: draft.title,
            goal: draft.goal,
            github_issue: draft.github_issue,
            status: crate::domain::SLICE_STATUS_OPEN.to_string(),
            closed_by_run: String::new(),
            closed_at: String::new(),
            depends_on: Vec::new(),
            areas: Vec::new(),
            acceptance: if draft.acceptance.is_empty() {
                vec!["Acceptance criteria are satisfied.".to_string()]
            } else {
                draft.acceptance
            },
            must_ask_if: vec![
                "Acceptance criteria conflict or require product intent not present in this slice."
                    .to_string(),
            ],
            verify_profile: String::new(),
            verify: draft.verify,
            verify_timeout_seconds: 0,
        };
        artifact::Store::new(&repo.path).write_slice(&slice, draft.overwrite)
    }

    pub fn import_github_issue(&self, opts: GithubImportOptions) -> Result<SliceWriteResult> {
        let repo = self.init_repo(&opts.repo_path)?;
        let issue = fetch_github_issue(&opts.issue)?;
        let id = if opts.id.trim().is_empty() {
            slug_slice_id(&issue.title)
        } else {
            opts.id
        };
        let slice = Slice {
            id,
            title: issue.title.clone(),
            goal: first_meaningful_paragraph(&issue.body).unwrap_or_else(|| issue.title.clone()),
            github_issue: issue.url,
            status: crate::domain::SLICE_STATUS_OPEN.to_string(),
            closed_by_run: String::new(),
            closed_at: String::new(),
            depends_on: Vec::new(),
            areas: issue
                .labels
                .iter()
                .map(|label| label.name.clone())
                .collect(),
            acceptance: acceptance_from_issue_body(&issue.body),
            must_ask_if: vec![
                "The GitHub issue discussion conflicts with this JSON slice.".to_string(),
                "Implementing the issue requires product intent not present in this slice."
                    .to_string(),
            ],
            verify_profile: String::new(),
            verify: opts.verify,
            verify_timeout_seconds: 0,
        };
        let store = artifact::Store::new(&repo.path);
        if opts.dry_run {
            artifact::validate_slice(&slice)?;
            let path = store.slice_path(&slice.id);
            if path.exists() && !opts.overwrite {
                bail!(
                    "slice {:?} already exists at {}; pass --overwrite to preview replacing it",
                    slice.id,
                    path.display()
                );
            }
            return Ok(SliceWriteResult {
                path: path.to_string_lossy().to_string(),
                slice,
                written: false,
            });
        }
        store.write_slice(&slice, opts.overwrite)
    }

    pub fn start_run(&self, mut opts: StartOptions) -> Result<Run> {
        let repo = self.init_repo(&opts.repo_path)?;
        let store = artifact::Store::new(&repo.path);
        let config = store.read_config()?;
        let cockpit_mode = effective_cockpit_mode(&mut opts.pi_args, &config)?;
        let mission_envelope = opts.mission_envelope.clone();
        validate_mission_envelope(mission_envelope.as_ref(), &config)?;
        let frontier_budget = mission_envelope
            .as_ref()
            .map(|_| FrontierBudgetState::default());
        self.ensure_repo_run_available(&repo.id, None)?;
        let slices = store.load_slices()?;
        if slices.is_empty() {
            bail!("no JSON slices found in {}", store.slices_dir().display());
        }
        let requested = if opts.all {
            Vec::new()
        } else {
            opts.slice_ids.clone()
        };
        let requested_set: BTreeSet<_> = requested.iter().cloned().collect();
        let planned_slices = artifact::topological_order(&slices, &requested)?;
        let mut skipped_closed_slices = Vec::new();
        let mut selected_slices = Vec::new();
        for slice in planned_slices {
            if slice.status == crate::domain::SLICE_STATUS_CLOSED {
                if requested_set.contains(&slice.id) {
                    bail!(
                        "slice {:?} is closed; create a follow-up slice instead of rerunning historical work",
                        slice.id
                    );
                }
                skipped_closed_slices.push(slice.id.clone());
                continue;
            }
            selected_slices.push(slice);
        }
        if selected_slices.is_empty() {
            bail!("no open slices selected");
        }
        let selected_ids: Vec<_> = selected_slices
            .iter()
            .map(|slice| slice.id.clone())
            .collect();
        let dirty_status = gitutil::status_porcelain(&repo.path)?;
        if !dirty_status.trim().is_empty() && !opts.allow_dirty {
            bail!(
                "source repo has uncommitted changes; commit/stash them or rerun with --allow-dirty\n{}",
                dirty_status.trim()
            );
        }
        let runner = self.runner_for_options(&opts, &config)?;
        let parallelism = effective_parallelism(opts.parallelism, &config);
        let native_pi_tui_worker =
            opts.native_pi_tui_worker && !matches!(cockpit_mode, CockpitMode::Direct);
        let base_branch = if config.base_branch.trim().is_empty() {
            gitutil::current_branch(&repo.path).unwrap_or_default()
        } else {
            config.base_branch.clone()
        };
        let base_sha = if config.base_branch.trim().is_empty() {
            gitutil::head_sha(&repo.path)?
        } else {
            gitutil::run(&repo.path, &["rev-parse", &config.base_branch])?
        };
        let run_id = new_run_id();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo.id,
            repo_path: repo.path,
            status: RunStatus::Running,
            base_branch,
            base_sha,
            integration_branch: format!("khazad/{run_id}/integration"),
            selected_slice_id: selected_ids.join(","),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        self.state.insert_run(&run)?;
        self.state.set_frontier_state(
            &run.id,
            mission_envelope.as_ref(),
            frontier_budget.as_ref(),
        )?;
        let run_store = artifact::Store::new(&run.repo_path);
        run_store.ensure_run_dirs(&run.id)?;
        if let Some(origin) =
            origin_notification_target_from_start(&opts.origin_notification_target)
        {
            let path = run_store.write_origin_notification_target(&run.id, &origin)?;
            self.state.record_event(
                &run.id,
                "origin_notification_target_recorded",
                &json!({
                    "path": path,
                    "target_kind": origin.target_kind,
                    "delivery_adapter": origin.delivery_adapter,
                    "delivery_surface": origin.delivery_surface,
                }),
            )?;
        }
        let runner_metadata = runner.metadata();
        let worker_profile = worker_profile_evidence(runner.name(), &runner_metadata);
        let pi_contract = runner.pi_contract_observation();
        artifact::write_json(
            artifact::Store::new(&run.repo_path).output_path(&run.id, "preflight.json"),
            &json!({
                "agent": runner.name(),
                "worker_profile": &worker_profile,
                "worker_evidence_kind": &worker_profile.worker_evidence_kind,
                "worker_evidence_label": &worker_profile.worker_evidence_label,
                "profile_summary": runner_metadata.profile_summary(),
                "launch_summary": runner_metadata.launch_summary(),
                "profile_source_attribution": &runner_metadata.source_attribution,
                "pi_contract": pi_contract,
                "run_id": run.id,
                "repo_path": run.repo_path,
                "base_branch": run.base_branch,
                "base_sha": run.base_sha,
                "dirty": !dirty_status.trim().is_empty(),
                "allow_dirty": opts.allow_dirty,
                "status_porcelain": dirty_status,
                "selected_slices": &selected_ids,
                "mission_envelope": &mission_envelope,
                "frontier_budget": &frontier_budget,
                "autonomy_effective": mission_envelope.as_ref().map(|envelope| envelope.autonomy_level.as_str()).unwrap_or("off"),
                "autonomy_note": autonomy_effective_note(mission_envelope.as_ref()),
                "native_pi_tui_worker": native_pi_tui_worker,
                "experimental_pi_tui_worker": native_pi_tui_worker,
                "worker_interface": if native_pi_tui_worker { "native_pi_tui" } else { "json_wrapper" },
                "daemon_path": std::env::var("PATH").unwrap_or_default(),
                "created_at": now,
            }),
        )?;
        for slice in &selected_slices {
            self.state.upsert_slice_run(&SliceRun {
                run_id: run.id.clone(),
                slice_id: slice.id.clone(),
                status: SliceStatus::Pending,
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 0,
                last_error: String::new(),
            })?;
        }
        let verify_profiles = selected_verify_profiles(&selected_slices);
        let verify_profile = verify_profiles.join(", ");
        self.state.record_event(
            &run.id,
            workflow_events::RUN_STARTED,
            &workflow_events::RunStartedPayload::new(
                &run,
                selected_ids,
                skipped_closed_slices,
                verify_profile,
                verify_profiles,
                runner.name(),
                &runner_metadata.profile,
                &runner_metadata.provider,
                &runner_metadata.model,
                &runner_metadata.reasoning,
                &runner_metadata.mode,
                worker_profile.clone(),
                runner_metadata.profile_summary(),
                runner_metadata.launch_summary(),
                runner_metadata.source_attribution.clone(),
            ),
        )?;
        if let Some(envelope) = mission_envelope.as_ref() {
            self.state.record_event(
                &run.id,
                "mission_envelope_recorded",
                &json!({
                    "mission_envelope": envelope,
                    "frontier_budget": frontier_budget,
                    "autonomy_effective": envelope.autonomy_level.as_str(),
                    "authority": autonomy_authority_label(envelope.autonomy_level),
                }),
            )?;
        }
        let cockpit_mode = self.record_cockpit_launch(&run, cockpit_mode)?;
        self.mark_progress(&run.id, "started", "", 0, "", "run accepted by daemon");

        let cancel = CancellationToken::new();
        self.active.register(run.id.clone(), cancel.clone());
        let manager = self.clone();
        let bg_run = run.clone();
        thread::spawn(move || {
            let _guard = ActiveRunGuard {
                active: manager.active.clone(),
                run_id: bg_run.id.clone(),
            };
            manager.execute_run(
                bg_run,
                selected_slices.clone(),
                selected_slices,
                cancel,
                runner,
                parallelism,
                IntegrationMode::Fresh,
                cockpit_mode,
                native_pi_tui_worker,
            );
        });
        Ok(run)
    }

    pub fn cancel_run(&self, run_id: &str, reason: &str) -> Result<bool> {
        let reason = if reason.trim().is_empty() {
            "cancel requested"
        } else {
            reason
        };
        let run = self
            .state
            .get_run(run_id)?
            .ok_or_else(|| anyhow!("run {run_id:?} not found"))?;
        let active = self.active.cancel(run_id);
        self.state.record_event(
            run_id,
            workflow_events::RUN_CANCEL_REQUESTED,
            &workflow_events::RunCancelRequestedPayload::new(reason, active),
        )?;
        if !active && matches!(run.status, RunStatus::Running | RunStatus::Pending) {
            self.state.prepare_run_terminal_transition(
                run_id,
                RunStatus::Cancelled,
                reason,
                reason,
                &format!(
                    "run reached terminal state cancelled before the question was answered: {reason}"
                ),
            )?;
            let transition = self
                .state
                .terminal_transition(run_id)?
                .ok_or_else(|| anyhow!("run {run_id:?} lost its durable cancellation intent"))?;
            self.terminalize_or_reconcile(&run, &transition)?;
        }
        Ok(active)
    }

    pub fn resume_run(&self, mut opts: ResumeOptions) -> Result<Run> {
        let run = self
            .state
            .get_run(&opts.run_id)?
            .ok_or_else(|| anyhow!("run {:?} not found", opts.run_id))?;
        let inactive_deadline = Instant::now() + Duration::from_secs(5);
        while self.active.contains(&run.id) {
            if Instant::now() >= inactive_deadline {
                bail!(
                    "run {:?} is still finishing its prior execution; retry resume after cleanup",
                    run.id
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        if self
            .state
            .terminal_transition_needs_reconciliation(&run.id)?
        {
            let transition = self
                .state
                .terminal_transition(&run.id)?
                .ok_or_else(|| anyhow!("run {:?} lost its durable terminal intent", run.id))?;
            self.terminalize_or_reconcile(&run, &transition)?;
            return self.state.get_run(&run.id)?.ok_or_else(|| {
                anyhow!(
                    "run {:?} disappeared during terminal reconciliation",
                    run.id
                )
            });
        }
        if !matches!(
            run.status,
            RunStatus::Interrupted | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Blocked
        ) {
            bail!(
                "run {:?} is {}; resume requires interrupted, failed, cancelled, or blocked",
                run.id,
                run.status
            );
        }
        self.ensure_repo_run_available(&run.repo_id, Some(&run.id))?;
        let (resume_envelope, _) = self.state.get_frontier_state(&run.id)?;
        match resume_envelope
            .as_ref()
            .map(|envelope| envelope.autonomy_level)
        {
            Some(AutonomyLevel::Promote | AutonomyLevel::Run) => {}
            Some(AutonomyLevel::Shadow) => {
                self.classify_pending_frontier_proposals_at_replan_checkpoint(&run, "resume")?;
                self.block_if_pending_replan(&run, "resume")?;
            }
            Some(AutonomyLevel::Off) | None => self.block_if_pending_replan(&run, "resume")?,
        }
        let store = artifact::Store::new(&run.repo_path);
        let _last_checkpoint = store.read_checkpoint(&run.id).ok();
        self.prepare_resume_worktrees(&run)?;
        let all_slices = store.load_slices()?;
        let requested: Vec<String> = run
            .selected_slice_id
            .split(',')
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string)
            .collect();
        let requested_set: BTreeSet<_> = requested.iter().cloned().collect();
        let known_slice_ids: BTreeSet<_> =
            all_slices.iter().map(|slice| slice.id.clone()).collect();
        let known_requested: Vec<_> = requested
            .iter()
            .filter(|id| known_slice_ids.contains(*id))
            .cloned()
            .collect();
        let planned_slices = if requested.is_empty() || !known_requested.is_empty() {
            artifact::topological_order(&all_slices, &known_requested)?
        } else {
            Vec::new()
        };
        let mut selected_slices = Vec::new();
        for slice in planned_slices {
            if slice.status == crate::domain::SLICE_STATUS_CLOSED {
                if requested_set.contains(&slice.id) {
                    bail!(
                        "slice {:?} is closed; create a follow-up slice instead of rerunning historical work",
                        slice.id
                    );
                }
                continue;
            }
            selected_slices.push(slice);
        }
        // An allocation may have been persisted immediately before a daemon crash.
        // Preserve that immutable evidence; a resumed worker receives a fresh launch
        // identity rather than reviving or overwriting the abandoned allocation.
        self.state.reconcile_unlaunched_worker_attempts(
            &run.id,
            "resume began before the allocated worker process was launched",
        )?;
        let slice_runs = self.state.get_slice_runs(&run.id)?;
        let merged: BTreeSet<_> = slice_runs
            .iter()
            .filter(|slice_run| slice_run.status == SliceStatus::Merged)
            .map(|slice_run| slice_run.slice_id.clone())
            .collect();
        let remaining: Vec<_> = selected_slices
            .iter()
            .filter(|slice| !merged.contains(&slice.id))
            .cloned()
            .collect();
        for slice in &remaining {
            // `slice_runs` is a mutable current-summary compatibility projection.
            // Do not reset its retry budget or discard its last branch/error merely
            // because the run is being resumed; historical launch evidence lives in
            // the append-only worker_attempt_ledger.
            let prior = slice_runs.iter().find(|row| row.slice_id == slice.id);
            self.state.upsert_slice_run(&SliceRun {
                run_id: run.id.clone(),
                slice_id: slice.id.clone(),
                status: SliceStatus::Pending,
                branch: prior.map(|row| row.branch.clone()).unwrap_or_default(),
                commit_sha: prior.map(|row| row.commit_sha.clone()).unwrap_or_default(),
                attempts: prior.map(|row| row.attempts).unwrap_or_default(),
                last_error: prior.map(|row| row.last_error.clone()).unwrap_or_default(),
            })?;
        }
        self.state.reopen_run_for_resume(&run.id)?;
        let config = store.read_config()?;
        let cockpit_mode = effective_cockpit_mode(&mut opts.pi_args, &config)?;
        let native_pi_tui_worker = (opts.native_pi_tui_worker
            || run_preflight_native_pi_tui_worker(&run))
            && !matches!(cockpit_mode, CockpitMode::Direct);
        self.state.record_event(
            &run.id,
            "run_resumed",
            &json!({
                "remaining_slices": remaining.iter().map(|slice| slice.id.clone()).collect::<Vec<_>>(),
                "native_pi_tui_worker": native_pi_tui_worker,
                "experimental_pi_tui_worker": native_pi_tui_worker,
            }),
        )?;
        self.mark_progress(&run.id, "resumed", "", 0, "", "run resumed by daemon");
        let runner = self.runner_for_parts(&opts.agent, &opts.pi_bin, &opts.pi_args, &config)?;
        let cockpit_mode = self.record_cockpit_launch(&run, cockpit_mode)?;
        let cancel = CancellationToken::new();
        self.active.register(run.id.clone(), cancel.clone());
        let manager = self.clone();
        let bg_run = run.clone();
        let parallelism = effective_parallelism(opts.parallelism, &config);
        thread::spawn(move || {
            let _guard = ActiveRunGuard {
                active: manager.active.clone(),
                run_id: bg_run.id.clone(),
            };
            manager.execute_run(
                bg_run,
                remaining,
                selected_slices,
                cancel,
                runner,
                parallelism,
                IntegrationMode::Existing,
                cockpit_mode,
                native_pi_tui_worker,
            );
        });
        self.state
            .get_run(&run.id)?
            .ok_or_else(|| anyhow!("run {:?} not found after resume", run.id))
    }

    pub fn terminalize_inactive_runs_for_shutdown(&self, reason: &str) -> Result<usize> {
        let reason = if reason.trim().is_empty() {
            "daemon stopped"
        } else {
            reason
        };
        let runs = self.state.active_runs()?;
        for run in &runs {
            if let Some(transition) = self.state.terminal_transition(&run.id)? {
                self.terminalize_or_reconcile(run, &transition)?;
                continue;
            }
            self.state.prepare_run_terminal_transition(
                &run.id,
                RunStatus::Cancelled,
                reason,
                reason,
                &format!(
                    "run reached terminal state cancelled before the question was answered: {reason}"
                ),
            )?;
            let transition = self
                .state
                .terminal_transition(&run.id)?
                .ok_or_else(|| anyhow!("run {:?} lost its durable shutdown intent", run.id))?;
            self.terminalize_or_reconcile(run, &transition)?;
        }
        Ok(runs.len())
    }

    pub fn recover_interrupted_runs(&self) -> Result<usize> {
        let mut recovered_run_ids = BTreeSet::new();
        for run_id in self
            .state
            .terminal_transition_run_ids_needing_reconciliation()?
        {
            let run = self
                .state
                .get_run(&run_id)?
                .ok_or_else(|| anyhow!("terminal transition refers to missing run {run_id:?}"))?;
            let transition = self
                .state
                .terminal_transition(&run_id)?
                .ok_or_else(|| anyhow!("run {run_id:?} lost its durable terminal intent"))?;
            self.terminalize_or_reconcile(&run, &transition)?;
            recovered_run_ids.insert(run_id);
        }

        let runs = self.state.active_runs()?;
        let reason = "daemon restarted before run reached a terminal state";
        for run in &runs {
            if recovered_run_ids.contains(&run.id) {
                continue;
            }
            if let Some(transition) = self.state.terminal_transition(&run.id)? {
                self.terminalize_or_reconcile(run, &transition)?;
                recovered_run_ids.insert(run.id.clone());
                continue;
            }
            self.state.record_event(
                &run.id,
                "daemon_recovery_started",
                &json!({ "reason": reason }),
            )?;
            let interrupted_questions = self.state.prepare_run_terminal_transition(
                &run.id,
                RunStatus::Interrupted,
                reason,
                reason,
                reason,
            )?;
            let transition = self
                .state
                .terminal_transition(&run.id)?
                .ok_or_else(|| anyhow!("run {:?} lost its durable recovery intent", run.id))?;
            self.terminalize_or_reconcile(run, &transition)?;
            if interrupted_questions > 0 {
                self.state.record_event(
                    &run.id,
                    "worker_questions_interrupted",
                    &json!({ "count": interrupted_questions, "reason": reason }),
                )?;
            }
            self.state.record_event(
                &run.id,
                "daemon_recovery_completed",
                &json!({ "status": RunStatus::Interrupted, "reason": reason }),
            )?;
            recovered_run_ids.insert(run.id.clone());
        }
        Ok(recovered_run_ids.len())
    }

    pub fn branch_handoff(
        &self,
        run_id: &str,
        push: bool,
        create_pr: bool,
        dry_run: bool,
    ) -> Result<BranchHandoff> {
        let run = self
            .state
            .get_run(run_id)?
            .ok_or_else(|| anyhow!("run {run_id:?} not found"))?;
        if run.status != RunStatus::Completed {
            bail!(
                "run {run_id:?} is {}; handoff requires completed",
                run.status
            );
        }
        let store = artifact::Store::new(&run.repo_path);
        let config = store.read_config()?;
        let effective_push = !dry_run && (push || config.handoff.push);
        let effective_create_pr = !dry_run && (create_pr || config.handoff.create_pr);
        if effective_create_pr && !effective_push {
            return Err(BlockedError::new(
                "handoff PR creation requires pushing and validating the exact publication receipt SHA in the same action"
                    .to_string(),
            )
            .into());
        }
        let diagnostics = handoff_diagnostics(&run.repo_path);
        let summary_path = store.output_path(&run.id, "implementation-summary.json");
        let final_report_path = store.output_path(&run.id, "final-report.json");
        let summary = artifact::read_json::<ImplementationSummary>(&summary_path).ok();
        let read_model = self.run_read_model(&run, RunReadModelOptions::status(500))?;
        let completed_slices: Vec<String> = summary
            .as_ref()
            .map(|summary| {
                summary
                    .completed_slices
                    .iter()
                    .map(|slice| slice.slice_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let publication_events = self.state.get_events(&run.id, 100_000)?;
        let receipt_event = publication_events
            .iter()
            .rev()
            .find(|event| event.typ == "completion_publication_committed")
            .ok_or_else(|| {
                BlockedError::new(
                    "handoff is not ready; completed run has no durable completion publication receipt"
                        .to_string(),
                )
            })?;
        let receipt: artifact::CompletionPublicationReceipt =
            serde_json::from_value(receipt_event.payload.clone())
                .context("decode durable completion publication receipt for handoff")?;
        if summary
            .as_ref()
            .is_none_or(|summary| summary.final_sha != receipt.commit_sha)
        {
            return Err(BlockedError::new(format!(
                "handoff summary final SHA does not match durable completion publication receipt {}; operator reconciliation is required",
                receipt.commit_sha
            ))
            .into());
        }
        store.validate_completion_publication_receipt_at_ref(&run.integration_branch, &receipt)?;
        let final_sha = receipt.commit_sha;
        let exit_states = summary
            .as_ref()
            .map(|summary| summary.exit_states.clone())
            .filter(|exit_states| !exit_states.run.trim().is_empty())
            .unwrap_or_else(|| historical_handoff_exit_states(run.status, &completed_slices));
        let evidence_attestation = summary
            .as_ref()
            .map(|summary| summary.evidence_attestation.clone())
            .filter(|attestation| !attestation.status.trim().is_empty())
            .unwrap_or_else(historical_evidence_attestation);
        let worker_profile = summary
            .as_ref()
            .map(|summary| summary.worker_profile.clone())
            .filter(|profile| !profile.is_empty())
            .or_else(|| {
                (!read_model.details.worker_profile.is_empty())
                    .then(|| read_model.details.worker_profile.clone())
            })
            .unwrap_or_default();
        let mission_envelope = read_model.details.mission_envelope.clone();
        let frontier_budget = read_model.details.frontier_budget.clone();
        let plan_revisions = read_model.plan_revisions;
        if plan_revisions.unresolved_pending_blocks_handoff {
            let ids = plan_revisions
                .pending
                .iter()
                .map(|proposal| proposal.proposal_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let commands = plan_revisions
                .pending
                .iter()
                .flat_map(|proposal| proposal.decision_commands.clone())
                .collect::<Vec<_>>()
                .join("; ");
            let frontier_pending = plan_revisions
                .frontier
                .deferred_rejected_pending_fog
                .iter()
                .filter(|fog| fog.state == "pending")
                .map(|fog| {
                    let reasons = if fog.reason_codes.is_empty() {
                        "no frontier reason codes".to_string()
                    } else {
                        fog.reason_codes.join(",")
                    };
                    format!(
                        "{} -> {} tier={} reasons={}",
                        fog.proposal_id,
                        if fog.proposed_slice_id.trim().is_empty() {
                            "<no generated slice>"
                        } else {
                            fog.proposed_slice_id.as_str()
                        },
                        if fog.tier.trim().is_empty() {
                            "unclassified"
                        } else {
                            fog.tier.as_str()
                        },
                        reasons
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            let frontier_clause = if frontier_pending.trim().is_empty() {
                String::new()
            } else {
                format!("; frontier pending: {frontier_pending}")
            };
            bail!(
                "handoff is not ready; unresolved replan proposal(s) {ids} require operator disposition{frontier_clause}; decide with: {commands}"
            );
        }
        let pr_title = format!("Khazad-Doom {}: {}", run.id, run.selected_slice_id);
        let pr_body = format!(
            "Khazad-Doom run `{}` completed.\n\nIntegration branch: `{}`\nBase branch: `{}`\nFinal SHA: `{}`\nFinal report: `{}`\n",
            run.id,
            run.integration_branch,
            run.base_branch,
            final_sha,
            final_report_path.display()
        );
        let push_refspec = format!("{}:refs/heads/{}", final_sha, run.integration_branch);
        let push_command = format!(
            "git -C {} push origin {}",
            sh_quote(&run.repo_path),
            sh_quote(&push_refspec)
        );
        let pr_command = format!(
            "cd {} && gh pr create --base {} --head {} --title {} --body-file {}",
            sh_quote(&run.repo_path),
            sh_quote(&run.base_branch),
            sh_quote(&run.integration_branch),
            sh_quote(&pr_title),
            sh_quote(&final_report_path.to_string_lossy())
        );
        let mut actions = Vec::new();
        if effective_push {
            let push_action = run_handoff_command(
                "push",
                &run.repo_path,
                &["push", "origin", &push_refspec],
                &push_command,
            )?;
            if push_action.status != "passed" {
                return Err(BlockedError::new(format!(
                    "handoff did not push validated publication receipt {final_sha}; PR creation is blocked: {}",
                    push_action.output
                ))
                .into());
            }
            actions.push(push_action);
            validate_remote_handoff_ref(&run.repo_path, &run.integration_branch, &final_sha)?;
        }
        if effective_create_pr {
            let body = final_report_path.to_string_lossy().to_string();
            let pr_action = run_external_command(
                "create_pr",
                &run.repo_path,
                "gh",
                &[
                    "pr",
                    "create",
                    "--base",
                    &run.base_branch,
                    "--head",
                    &run.integration_branch,
                    "--title",
                    &pr_title,
                    "--body-file",
                    &body,
                ],
                &pr_command,
            )?;
            if pr_action.status != "passed" {
                return Err(BlockedError::new(format!(
                    "handoff PR creation failed for validated publication receipt {final_sha}: {}",
                    pr_action.output
                ))
                .into());
            }
            validate_remote_handoff_ref(&run.repo_path, &run.integration_branch, &final_sha)?;
            actions.push(pr_action);
        }
        Ok(BranchHandoff {
            run_id: run.id,
            repo_path: run.repo_path,
            status: run.status,
            integration_branch: run.integration_branch,
            base_branch: run.base_branch,
            base_sha: run.base_sha,
            final_sha,
            worker_profile,
            completed_slices,
            exit_states,
            evidence_attestation,
            plan_revisions,
            mission_envelope,
            frontier_budget,
            summary_path: summary_path.to_string_lossy().to_string(),
            final_report_path: final_report_path.to_string_lossy().to_string(),
            push_command,
            pr_command,
            pr_title,
            pr_body,
            dry_run,
            diagnostics,
            actions,
        })
    }

    pub fn inspect_run(&self, run_id: &str, log_tail_lines: usize) -> Result<RunInspection> {
        let run = self
            .state
            .get_run(run_id)?
            .ok_or_else(|| anyhow!("run {run_id:?} not found"))?;
        let artifacts = artifact::Store::new(&run.repo_path).list_run_artifacts(&run.id)?;
        let daemon_log = self.paths.daemon_log();
        let log_tail = tail_lines(&daemon_log, log_tail_lines)?;
        Ok(RunInspection {
            run,
            artifacts,
            daemon_log: daemon_log.to_string_lossy().to_string(),
            log_tail,
        })
    }

    fn write_terminal_run_summary_artifact(
        &self,
        run: &Run,
        status: RunStatus,
        message: &str,
    ) -> Result<(serde_json::Value, PathBuf)> {
        #[cfg(test)]
        if take_terminalization_fault(TerminalizationFaultStage::SummaryWrite) {
            bail!("injected terminal summary persistence failure");
        }
        let store = artifact::Store::new(&run.repo_path);
        store.ensure_run_dirs(&run.id)?;
        let read_model = self.run_read_model(
            run,
            RunReadModelOptions::terminal_summary(status, message.to_string()),
        )?;
        let details = read_model.details;
        let events = details.events.clone();
        let slice_runs = details.slice_runs.clone();
        let progress = details.progress.clone();
        let cancel_reason = latest_cancel_reason(&events);
        let primary_failure = primary_failure_for_terminal_summary(message, &slice_runs, &events);
        let summary = json!({
            "run_id": run.id,
            "repo_path": run.repo_path,
            "status": status,
            "base_branch": run.base_branch,
            "base_sha": run.base_sha,
            "integration_branch": run.integration_branch,
            "selected_slice_id": run.selected_slice_id,
            "worker_profile": details.worker_profile,
            "message": message,
            "primary_failure": primary_failure,
            "cancel_reason": cancel_reason,
            "slice_runs": slice_runs,
            "progress": progress,
            "incidents": details.incidents,
            "questions": details.questions,
            "replan": details.replan,
            "mission_envelope": details.mission_envelope,
            "frontier_budget": details.frontier_budget,
            "economics": details.economics,
            "primary_terminal_reason": details.primary_terminal_reason,
            "feed": details.feed,
            "plan_revisions": read_model.plan_revisions,
            "worktree_snapshots": self.run_worktree_snapshots(run),
            "next_commands": terminal_next_commands(run, status),
            "created_at": Utc::now(),
        });
        let summary_path = store.output_path(&run.id, "run-summary.json");
        artifact::write_json(&summary_path, &summary)?;
        Ok((summary, summary_path))
    }

    fn notify_terminal_transition(
        &self,
        run: &Run,
        status: RunStatus,
        message: &str,
        summary: &serde_json::Value,
        summary_path: &Path,
    ) -> Result<()> {
        #[cfg(test)]
        if take_terminalization_fault(TerminalizationFaultStage::Notification) {
            bail!("injected terminal notification failure");
        }
        let read_model = self.run_read_model(
            run,
            RunReadModelOptions::terminal_summary(status, message.to_string()),
        )?;
        let details = read_model.details;
        let attention = OperatorAttention::new(self.state.clone());
        attention.worker_pane_terminal_rename(WorkerPaneTerminalRename {
            run,
            events: &details.events,
            slice_runs: &details.slice_runs,
        });
        attention.terminal_transition_notification(TerminalTransitionNotification {
            run,
            status,
            progress: details.progress.as_ref(),
            summary,
            summary_path,
        });
        Ok(())
    }

    #[cfg(test)]
    fn write_terminal_run_summary(
        &self,
        run: &Run,
        status: RunStatus,
        message: &str,
    ) -> Result<()> {
        let (summary, summary_path) =
            self.write_terminal_run_summary_artifact(run, status, message)?;
        self.state.record_event(
            &run.id,
            workflow_events::TERMINAL_SUMMARY_WRITTEN,
            &workflow_events::TerminalSummaryWrittenPayload::new(&summary_path),
        )?;
        self.notify_terminal_transition(run, status, message, &summary, &summary_path)
    }

    fn terminal_summary_matches_intent(
        summary: &serde_json::Value,
        run: &Run,
        transition: &TerminalTransition,
    ) -> bool {
        summary.get("run_id").and_then(serde_json::Value::as_str) == Some(run.id.as_str())
            && summary.get("status").and_then(serde_json::Value::as_str)
                == Some(transition.status.as_str())
            && summary.get("message").and_then(serde_json::Value::as_str)
                == Some(transition.error.as_str())
    }

    fn terminalize_or_reconcile(&self, run: &Run, transition: &TerminalTransition) -> Result<()> {
        let store = artifact::Store::new(&run.repo_path);
        let summary_path = store.output_path(&run.id, "run-summary.json");
        let (summary, summary_path) = if transition.summary_written {
            match artifact::read_json::<serde_json::Value>(&summary_path) {
                Ok(summary) if Self::terminal_summary_matches_intent(&summary, run, transition) => {
                    (summary, summary_path)
                }
                Ok(_) | Err(_) => self.write_terminal_run_summary_artifact(
                    run,
                    transition.status,
                    &transition.error,
                )?,
            }
        } else {
            self.write_terminal_run_summary_artifact(run, transition.status, &transition.error)?
        };
        if !transition.summary_written {
            self.state.mark_terminal_summary_written(
                &run.id,
                workflow_events::TERMINAL_SUMMARY_WRITTEN,
                &workflow_events::TerminalSummaryWrittenPayload::new(&summary_path),
            )?;
        }
        match transition.status {
            RunStatus::Completed => {
                self.state.commit_terminal_transition(
                    &run.id,
                    workflow_events::RUN_COMPLETED,
                    &workflow_events::RunCompletedPayload::new(&run.id),
                )?;
            }
            RunStatus::Cancelled => {
                self.state.commit_terminal_transition(
                    &run.id,
                    workflow_events::RUN_CANCELLED,
                    &workflow_events::RunCancelledPayload::new(&transition.error),
                )?;
            }
            RunStatus::Blocked | RunStatus::Failed | RunStatus::Interrupted => {
                self.state.commit_terminal_transition(
                    &run.id,
                    workflow_events::RUN_ERROR,
                    &workflow_events::RunErrorPayload::new(&transition.error),
                )?;
            }
            RunStatus::Pending | RunStatus::Running => {
                bail!(
                    "run {:?} has a non-terminal durable transition {}",
                    run.id,
                    transition.status
                );
            }
        }
        let terminal_run = self
            .state
            .get_run(&run.id)?
            .ok_or_else(|| anyhow!("run {:?} disappeared while terminalizing", run.id))?;
        if !self.state.terminal_notification_bookkept(&run.id)? {
            if let Err(err) = self.notify_terminal_transition(
                &terminal_run,
                transition.status,
                &transition.error,
                &summary,
                &summary_path,
            ) {
                let incident = workflow_events::RunIncidentPayload::warning(
                    "terminal_notification_bookkeeping_failed",
                    format!("terminal notification bookkeeping failed: {err:#}"),
                );
                if let Err(record_err) =
                    self.state
                        .record_event(&run.id, workflow_events::RUN_INCIDENT, &incident)
                {
                    eprintln!(
                        "khazad-doom: could not record non-authoritative terminal notification incident for {}: {record_err:#}",
                        run.id
                    );
                }
            }
            self.state.mark_terminal_notification_bookkept(&run.id)?;
        }
        if self.state.claim_terminal_cleanup(&run.id)? {
            #[cfg(test)]
            if take_terminalization_fault(TerminalizationFaultStage::Cleanup) {
                let cleanup_error = "injected terminal cleanup failure";
                self.state.record_event(
                    &run.id,
                    "worktree_cleanup_error",
                    &workflow_events::RunErrorPayload::new(cleanup_error),
                )?;
                return Ok(());
            }
            match self.cleanup_run_worktrees(&terminal_run) {
                Ok(()) => {
                    if let Err(err) = self.state.mark_terminal_cleanup_completed(
                        &run.id,
                        workflow_events::WORKTREES_CLEANED,
                        &workflow_events::RunCompletedPayload::new(&run.id),
                    ) {
                        eprintln!(
                            "khazad-doom: could not record non-authoritative worktree cleanup for {}: {err:#}",
                            run.id
                        );
                    }
                }
                Err(err) => {
                    let cleanup_error = err.to_string();
                    if let Err(record_err) = self.state.record_event(
                        &run.id,
                        "worktree_cleanup_error",
                        &workflow_events::RunErrorPayload::new(&cleanup_error),
                    ) {
                        eprintln!(
                            "khazad-doom: could not record non-authoritative worktree cleanup failure for {}: {record_err:#}",
                            run.id
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn run_worktree_snapshots(&self, run: &Run) -> Vec<serde_json::Value> {
        let root = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        discover_run_worktrees(&root)
            .unwrap_or_default()
            .into_iter()
            .map(|path| {
                json!({
                    "path": path.to_string_lossy(),
                    "status": git_output_or_empty(&path, &["status", "--porcelain"]),
                    "diff_tail": bounded_text(&git_output_or_empty(&path, &["diff"]), 20_000),
                    "head": gitutil::head_sha(&path).unwrap_or_default(),
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_run(
        &self,
        run: Run,
        worker_slices: Vec<Slice>,
        gate_slices: Vec<Slice>,
        cancel: CancellationToken,
        runner: Arc<dyn Runner>,
        parallelism: usize,
        integration_mode: IntegrationMode,
        cockpit_mode: CockpitMode,
        native_pi_tui_worker: bool,
    ) {
        let outcome = self.run_slices(
            &run,
            &worker_slices,
            &gate_slices,
            &cancel,
            runner,
            parallelism,
            integration_mode,
            cockpit_mode,
            native_pi_tui_worker,
        );
        let terminalization = (|| -> Result<()> {
            let (terminal_status, terminal_message, progress_message) = match &outcome {
                Ok(_) => (
                    RunStatus::Completed,
                    String::new(),
                    "run completed; handoff artifacts are ready".to_string(),
                ),
                Err(err) => {
                    let status = classify_run_failure(err);
                    let raw_message = format!("{err:#}");
                    let cancel_reason = if status == RunStatus::Cancelled {
                        latest_cancel_reason(&self.state.get_events(&run.id, 200)?)
                            .trim()
                            .to_string()
                    } else {
                        String::new()
                    };
                    let message = if cancel_reason.is_empty() {
                        raw_message
                    } else {
                        cancel_reason
                    };
                    (status, message.clone(), message)
                }
            };
            let question_interruption_reason = if terminal_message.is_empty() {
                format!(
                    "run reached terminal state {terminal_status} before the question was answered"
                )
            } else {
                format!(
                    "run reached terminal state {terminal_status} before the question was answered: {terminal_message}"
                )
            };
            self.state.prepare_run_terminal_transition(
                &run.id,
                terminal_status,
                &terminal_message,
                &progress_message,
                &question_interruption_reason,
            )?;
            let transition = self
                .state
                .terminal_transition(&run.id)?
                .ok_or_else(|| anyhow!("run {:?} lost its durable terminal intent", run.id))?;
            self.terminalize_or_reconcile(&run, &transition)
        })();
        if let Err(err) = terminalization {
            eprintln!(
                "khazad-doom: terminalization for run {} requires recovery: {err:#}",
                run.id
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_slices(
        &self,
        run: &Run,
        worker_slices: &[Slice],
        gate_slices: &[Slice],
        cancel: &CancellationToken,
        runner: Arc<dyn Runner>,
        parallelism: usize,
        integration_mode: IntegrationMode,
        cockpit_mode: CockpitMode,
        native_pi_tui_worker: bool,
    ) -> Result<ImplementationSummary> {
        check_cancelled(cancel)?;
        let store = artifact::Store::new(&run.repo_path);
        let config = store.read_config()?;
        let repair_policy = RepairPolicy::parse(&config.integration_repair)?;
        let economics = RunEconomicsRecorder::new(
            repair_policy.as_str(),
            config.gate_fail_fast,
            MAX_WORKER_ATTEMPTS,
            DEFAULT_REPAIR_ATTEMPTS,
        )
        .with_snapshot_path(store.output_path(&run.id, "economics.json"));
        let verification_cache = VerificationCommandCache::default();
        store.ensure_run_dirs(&run.id)?;
        let root_worktree = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        let integration_worktree = root_worktree.join("integration");
        std::fs::create_dir_all(&root_worktree)
            .with_context(|| format!("create {}", root_worktree.display()))?;

        self.mark_progress(
            &run.id,
            "integration_setup",
            "",
            0,
            "",
            "creating integration worktree",
        );
        let setup_phase = economics.start_phase("integration_setup");
        let reuse_recovery_worktree = matches!(integration_mode, IntegrationMode::Existing)
            && integration_worktree.is_dir()
            && gitutil::has_retained_completion_publication_journal(&integration_worktree)?;
        if !reuse_recovery_worktree {
            match integration_mode {
                IntegrationMode::Fresh => gitutil::worktree_add(
                    &run.repo_path,
                    &integration_worktree,
                    &run.integration_branch,
                    &run.base_sha,
                )
                .context("create integration worktree")?,
                IntegrationMode::Existing => gitutil::worktree_add_existing(
                    &run.repo_path,
                    &integration_worktree,
                    &run.integration_branch,
                )
                .context("create existing integration worktree")?,
            }
        }
        setup_phase.finish();

        let mut run = run.clone();
        let slice_runs = self.state.get_slice_runs(&run.id)?;
        let mut completed_slices = self.prior_completed_worker_results(&run, &store, &slice_runs);
        let mut checks = Vec::new();
        let mut dependency_summary: BTreeMap<_, _> = completed_slices
            .iter()
            .map(|result| (result.slice_id.clone(), result.summary.clone()))
            .collect();
        let mut completed_ids: BTreeSet<_> = slice_runs
            .into_iter()
            .filter(|slice_run| slice_run.status == SliceStatus::Merged)
            .map(|slice_run| slice_run.slice_id)
            .collect();
        let mut gate_slices = gate_slices.to_vec();
        let mut worker_layers = self.initial_worker_layers(
            &run,
            worker_slices,
            &mut gate_slices,
            &completed_ids,
            &integration_worktree,
        )?;
        loop {
            check_cancelled(cancel)?;
            self.settle_replan_checkpoint(
                &mut run,
                "worker_dispatch",
                &integration_worktree,
                &mut worker_layers,
                &mut gate_slices,
            )?;
            let Some(layer) = worker_layers.pop_front() else {
                break;
            };
            for batch in worker_batches_for_layer(&layer, parallelism) {
                check_cancelled(cancel)?;
                let slice_base_sha = gitutil::head_sha(&integration_worktree)?;
                let batch_ids = batch
                    .iter()
                    .map(|slice| slice.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                let worker_phase = economics.start_phase(format!("worker_layer:{batch_ids}"));
                let worker_context = WorkerExecutionContext {
                    run: run.clone(),
                    execution_epoch: self.state.current_run_execution_epoch(&run.id)?,
                    root_worktree: root_worktree.clone(),
                    slice_base_sha,
                    dependency_summary: dependency_summary.clone(),
                    cancel: cancel.clone(),
                    runner: runner.clone(),
                    config: config.clone(),
                    cockpit_mode,
                    economics: economics.clone(),
                    verification_cache: verification_cache.clone(),
                    native_pi_tui_worker,
                };
                let outcomes = self.run_worker_batch(&batch, &worker_context, parallelism)?;
                worker_phase.finish();
                for worker in outcomes {
                    let slice = worker.slice.clone();
                    self.mark_progress(
                        &run.id,
                        "merging",
                        &slice.id,
                        worker.attempts,
                        "git merge",
                        "merging slice branch into integration branch",
                    );
                    if let Err(err) = gitutil::merge(
                        &integration_worktree,
                        &worker.branch,
                        &format!("khazad(slice:{}): merge {}", slice.id, slice.title),
                    ) {
                        let report = self.write_merge_conflict_report(
                            &run,
                            &slice,
                            &worker.branch,
                            &integration_worktree,
                            &err,
                        )?;
                        let _ = gitutil::merge_abort(&integration_worktree);
                        self.state.update_slice_status(
                            &run.id,
                            &slice.id,
                            SliceStatus::Blocked,
                            &report.summary,
                        )?;
                        return Err(BlockedError::new(report.summary).into());
                    }
                    self.state.upsert_slice_run(&SliceRun {
                        run_id: run.id.clone(),
                        slice_id: slice.id.clone(),
                        status: SliceStatus::Merged,
                        branch: worker.branch,
                        commit_sha: worker.result.commit_sha.clone(),
                        attempts: worker.attempts,
                        last_error: String::new(),
                    })?;
                    self.state.record_event(
                        &run.id,
                        workflow_events::SLICE_MERGED,
                        &workflow_events::SliceMergedPayload::new(
                            &slice.id,
                            &worker.result.commit_sha,
                        ),
                    )?;
                    dependency_summary.insert(slice.id.clone(), worker.result.summary.clone());
                    completed_ids.insert(slice.id.clone());
                    checks.extend(worker.checks);
                    completed_slices.push(worker.result);
                    self.write_checkpoint(
                        &run,
                        &gate_slices,
                        &completed_ids,
                        &integration_worktree,
                    )?;
                }
            }
        }

        check_cancelled(cancel)?;
        self.settle_replan_checkpoint(
            &mut run,
            "integration_gate",
            &integration_worktree,
            &mut worker_layers,
            &mut gate_slices,
        )?;
        if !worker_layers.is_empty() {
            return Err(BlockedError::new(
                "generated follow-up slices were appended at integration gate; resume to dispatch the extended queue from the explicit checkpoint"
                    .to_string(),
            )
            .into());
        }
        self.run_worktree_setup(
            &run,
            "",
            0,
            None,
            &integration_worktree,
            &config,
            economics.clone(),
            verification_cache.clone(),
            cancel,
        )?;
        let gate_phase = economics.start_phase("integration_gate:initial");
        self.mark_progress(
            &run.id,
            "integration_gate",
            "",
            0,
            "",
            "running integration gate commands",
        );
        let mut gate = WorkflowGate::with_economics(
            self.progress_reporter(&run.id),
            economics.clone(),
            verification_cache.clone(),
        )
        .run_integration_gate(
            IntegrationGateRequest {
                slices: &gate_slices,
                integration_worktree: &integration_worktree,
                config: &config,
            },
            cancel,
        )?;
        gate_phase.finish();
        self.stop_after_cancelled_integration_gate(&store, &run.id, &gate)?;
        check_cancelled(cancel)?;

        let mut pre_repair_gate = None;
        let repair = if should_run_integration_repair(repair_policy, &gate) {
            pre_repair_gate = Some(gate.clone());
            check_cancelled(cancel)?;
            self.block_if_pending_replan(&run, "integration repair")?;
            self.mark_progress(
                &run.id,
                "integration_repair",
                "",
                0,
                "",
                "repairing failed integration gate evidence",
            );
            let repair_phase = economics.start_phase("integration_repair");
            let repair = self.integration_repair(IntegrationRepairContext {
                run: &run,
                slices: &gate_slices,
                integration_worktree: &integration_worktree,
                checks: &checks,
                gate_failure: &gate,
                trigger: repair_trigger_for_gate(repair_policy, &gate),
                cancel,
                runner: runner.clone(),
                config: &config,
                economics: economics.clone(),
                verification_cache: &verification_cache,
            })?;
            repair_phase.finish();

            check_cancelled(cancel)?;
            self.run_worktree_setup(
                &run,
                "",
                0,
                None,
                &integration_worktree,
                &config,
                economics.clone(),
                verification_cache.clone(),
                cancel,
            )?;
            self.mark_progress(
                &run.id,
                "integration_gate",
                "",
                0,
                "",
                "rerunning integration gate after repair",
            );
            let rerun_phase = economics.start_phase("integration_gate:after_repair");
            gate = WorkflowGate::with_economics(
                self.progress_reporter(&run.id),
                economics.clone(),
                verification_cache.clone(),
            )
            .run_integration_gate(
                IntegrationGateRequest {
                    slices: &gate_slices,
                    integration_worktree: &integration_worktree,
                    config: &config,
                },
                cancel,
            )?;
            rerun_phase.finish();
            self.stop_after_cancelled_integration_gate(&store, &run.id, &gate)?;
            check_cancelled(cancel)?;
            repair
        } else {
            skipped_repair_result(repair_policy, &gate)
        };
        let integration_store = artifact::Store::new(&integration_worktree);
        let completed_slice_ids: Vec<_> = completed_slices
            .iter()
            .map(|slice| slice.slice_id.clone())
            .collect();
        let publication_events = self.state.get_events(&run.id, 100_000).unwrap_or_default();
        let recorded_publication_commit =
            latest_completion_publication_commit(&publication_events).map(str::to_string);
        let existing_publication = if gate.status == "passed" {
            existing_completion_publication(
                &integration_store,
                &run.id,
                &run.integration_branch,
                &completed_slice_ids,
                recorded_publication_commit.as_deref(),
            )?
        } else {
            None
        };
        let publication_already_current = existing_publication.is_some();
        let mut publication_slice_ids = Vec::new();
        if gate.status == "passed" && !publication_already_current {
            check_cancelled(cancel)?;
            let closure_report = integration_store.close_slices_if_present(
                &completed_slice_ids,
                &run.id,
                &Utc::now().to_rfc3339(),
            );
            for incident in &closure_report.incidents {
                self.state.record_event(
                    &run.id,
                    workflow_events::RUN_INCIDENT,
                    &workflow_events::RunIncidentPayload::warning(
                        &incident.kind,
                        &incident.message,
                    )
                    .with_severity(&incident.severity)
                    .with_extra("slice_id", &incident.slice_id)
                    .with_extra("path", &incident.path)
                    .with_extra("policy", &incident.policy),
                )?;
            }
            if closure_report.blocks_handoff() {
                return Err(BlockedError::new(
                    "slice closure failed after integration gate; handoff is not ready".to_string(),
                )
                .into());
            }
            publication_slice_ids = closure_report.closed_slice_ids;
        }
        let exit_states = final_exit_states(&gate, &completed_slices);
        let worker_profile = worker_profile_evidence(runner.name(), &runner.metadata());
        let mut evidence_attestation = final_evidence_attestation(&gate);
        append_worker_evidence_attestation_basis(&mut evidence_attestation, &worker_profile);
        let plan_revisions = self.plan_revisions_for_run(&run)?;
        let worker_questions = self.state.list_worker_questions(&run.id)?;
        let worker_attempts = self
            .run_read_model(&run, RunReadModelOptions::status(0))?
            .details
            .worker_attempts;
        let (mission_envelope, frontier_budget) = self.state.get_frontier_state(&run.id)?;
        let mut summary = ImplementationSummary {
            run_id: run.id.clone(),
            repo_path: run.repo_path.clone(),
            integration_branch: run.integration_branch.clone(),
            base_sha: run.base_sha.clone(),
            final_sha: String::new(),
            worker_profile,
            mission_envelope,
            frontier_budget,
            completed_slices,
            checks,
            integration_repair: repair,
            pre_repair_integration_gate: pre_repair_gate,
            integration_gate: gate.clone(),
            exit_states,
            evidence_attestation,
            economics: economics.snapshot(),
            plan_revisions,
            worker_questions,
            worker_attempts,
            created_at: Utc::now(),
        };

        if gate.status == "passed" {
            if let Some(receipt) = existing_publication {
                let manifest = integration_store.completion_publication_manifest_for_gate(
                    &run.id,
                    &completed_slice_ids,
                    &gate,
                )?;
                integration_store.validate_completion_publication(
                    &run.integration_branch,
                    &manifest,
                    &receipt,
                )?;
                summary.final_sha = receipt.commit_sha.clone();
                if !completion_publication_event_exists(
                    &self.state.get_events(&run.id, 500).unwrap_or_default(),
                    &receipt.commit_sha,
                ) {
                    self.state.record_event(
                        &run.id,
                        "completion_publication_committed",
                        &receipt,
                    )?;
                }
            } else {
                integration_store
                    .write_implementation_summary(&summary)
                    .context("write implementation summary")?;
                integration_store.write_final_report(&summary)?;
                let manifest = integration_store.completion_publication_manifest_for_gate(
                    &run.id,
                    &publication_slice_ids,
                    &gate,
                )?;
                check_cancelled(cancel)?;
                let receipt = integration_store.commit_completion_publication(
                    &run.id,
                    &run.integration_branch,
                    &manifest,
                )?;
                if !receipt.committed {
                    return Err(BlockedError::new(
                        "completion publication manifest had no changes but no exact prior publication receipt was found; operator reconciliation is required"
                            .to_string(),
                    )
                    .into());
                }
                integration_store.validate_completion_publication(
                    &run.integration_branch,
                    &manifest,
                    &receipt,
                )?;
                summary.final_sha = receipt.commit_sha.clone();
                self.state
                    .record_event(&run.id, "completion_publication_committed", &receipt)?;
            }
        }
        artifact::write_json(
            store.output_path(&run.id, "implementation-summary.json"),
            &summary,
        )?;
        artifact::write_json(store.output_path(&run.id, "final-report.json"), &summary)?;
        self.state
            .record_event(&run.id, workflow_events::IMPLEMENTATION_SUMMARY, &summary)?;

        if gate.status != "passed" {
            if gate_needs_operator(&gate) {
                return Err(BlockedError::new(format!(
                    "integration gate needs operator environment fix: {}",
                    gate.summary
                ))
                .into());
            }
            bail!("integration gate failed: {}", gate.summary);
        }
        Ok(summary)
    }

    fn stop_after_cancelled_integration_gate(
        &self,
        store: &artifact::Store,
        run_id: &str,
        gate: &GateResult,
    ) -> Result<()> {
        if !gate.verification_cancelled {
            return Ok(());
        }
        let path = store.output_path(run_id, "integration-gate.cancelled.json");
        artifact::write_json(&path, gate)?;
        self.state
            .record_event(run_id, "integration_gate_cancelled", gate)?;
        if gate.failure_kind == "verification_restoration_failed"
            || gate
                .commands
                .iter()
                .any(|command| command.failure_kind == "verification_restoration_failed")
        {
            return Err(BlockedError::new(
                "cancelled integration verification could not restore its worktree; operator intervention is required"
                    .to_string(),
            )
            .into());
        }
        Err(CancelledError::new("run cancelled").into())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_worktree_setup(
        &self,
        run: &Run,
        slice_id: &str,
        attempt: usize,
        launch_stem: Option<&str>,
        worktree: &Path,
        config: &WorkflowConfig,
        economics: RunEconomicsRecorder,
        verification_cache: VerificationCommandCache,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if config.worktree_setup.is_empty() {
            return Ok(());
        }
        self.mark_progress(
            &run.id,
            "worktree_setup",
            slice_id,
            attempt,
            "",
            "running worktree setup commands",
        );
        let phase_name = if slice_id.is_empty() {
            "worktree_setup:integration".to_string()
        } else if attempt == 0 {
            format!("worktree_setup:{slice_id}:initial")
        } else if let Some(launch_stem) = launch_stem {
            format!("worktree_setup:{slice_id}:{launch_stem}")
        } else {
            format!("worktree_setup:{slice_id}:attempt-{attempt}")
        };
        let setup_phase = economics.start_phase(phase_name);
        let setup = WorkflowGate::with_economics(
            self.progress_reporter(&run.id),
            economics,
            verification_cache,
        )
        .run_worktree_setup(
            WorktreeSetupRequest {
                worktree,
                slice_id,
                attempt,
                config,
            },
            cancel,
        )?;
        setup_phase.finish();
        if setup.status == "passed" {
            self.state.record_event(
                &run.id,
                "worktree_setup_completed",
                &json!({
                    "slice_id": slice_id,
                    "attempt": attempt,
                    "worktree": worktree.to_string_lossy(),
                    "summary": setup.summary,
                }),
            )?;
            return Ok(());
        }

        let store = artifact::Store::new(&run.repo_path);
        store.ensure_run_dirs(&run.id)?;
        let artifact_name = if slice_id.is_empty() {
            "integration.worktree-setup.json".to_string()
        } else if attempt == 0 {
            format!("{slice_id}.worktree-setup.initial.json")
        } else if let Some(launch_stem) = launch_stem {
            format!("{launch_stem}.worktree-setup.json")
        } else {
            format!("{slice_id}.worktree-setup.attempt-{attempt}.json")
        };
        artifact::write_json(store.output_path(&run.id, &artifact_name), &setup)?;
        let blocked_summary = worktree_setup_blocked_summary(slice_id, attempt, &setup);
        self.state.record_event(
            &run.id,
            "worktree_setup_failed",
            &json!({
                "slice_id": slice_id,
                "attempt": attempt,
                "worktree": worktree.to_string_lossy(),
                "artifact": artifact_name,
                "setup": &setup,
            }),
        )?;
        Err(BlockedError::new(blocked_summary).into())
    }

    fn run_worker_batch(
        &self,
        batch: &[Slice],
        ctx: &WorkerExecutionContext,
        parallelism: usize,
    ) -> Result<Vec<SliceWorkerOutcome>> {
        if parallelism <= 1 || batch.len() <= 1 {
            let mut outcomes = Vec::new();
            for slice in batch {
                outcomes.push(self.run_slice_worker(slice, ctx)?);
            }
            return Ok(outcomes);
        }

        let mut outcomes = self.run_parallel_worker_batch(batch, ctx)?;
        outcomes.sort_by(|a, b| a.slice.id.cmp(&b.slice.id));
        Ok(outcomes)
    }

    fn run_parallel_worker_batch(
        &self,
        batch: &[Slice],
        ctx: &WorkerExecutionContext,
    ) -> Result<Vec<SliceWorkerOutcome>> {
        let batch_ids: Vec<_> = batch.iter().map(|slice| slice.id.clone()).collect();
        let batch_label = batch_ids.join(",");
        self.mark_progress(
            &ctx.run.id,
            "parallel_worker_layer",
            &batch_label,
            0,
            ctx.runner.name(),
            &format!("parallel worker layer running: {}", batch_ids.join(", ")),
        );
        self.state.record_event(
            &ctx.run.id,
            workflow_events::PARALLEL_LAYER_STARTED,
            &workflow_events::ParallelLayerPayload::started(batch_ids.clone()),
        )?;

        let batch_cancel = CancellationToken::new();
        let batch_done = Arc::new(AtomicBool::new(false));
        let parent_cancel = ctx.cancel.clone();
        let bridge_cancel = batch_cancel.clone();
        let bridge_done = batch_done.clone();
        let cancel_bridge = thread::spawn(move || {
            while !bridge_done.load(Ordering::SeqCst) {
                if parent_cancel.is_cancelled() {
                    bridge_cancel.cancel();
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });

        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for slice in batch.iter().cloned() {
            let slice_id = slice.id.clone();
            let manager = self.clone();
            let mut worker_ctx = ctx.clone();
            worker_ctx.cancel = batch_cancel.clone();
            let tx = tx.clone();
            handles.push(ParallelWorkerHandle {
                slice_id: slice_id.clone(),
                handle: thread::spawn(move || {
                    let result = manager.run_slice_worker(&slice, &worker_ctx);
                    let _ = tx.send(ParallelWorkerResult { slice_id, result });
                }),
            });
        }
        drop(tx);

        let mut results: BTreeMap<String, Result<SliceWorkerOutcome>> = BTreeMap::new();
        while results.len() < handles.len() {
            match rx.recv() {
                Ok(result) => {
                    if result.result.is_err() {
                        batch_cancel.cancel();
                    }
                    results.insert(result.slice_id, result.result);
                }
                Err(_) => break,
            }
        }

        batch_done.store(true, Ordering::SeqCst);
        let _ = cancel_bridge.join();

        for worker in handles {
            let panicked = worker.handle.join().is_err();
            if panicked {
                results
                    .entry(worker.slice_id)
                    .or_insert_with(|| Err(anyhow!("slice worker thread panicked")));
            }
        }
        for slice_id in &batch_ids {
            results.entry(slice_id.clone()).or_insert_with(|| {
                Err(anyhow!(
                    "slice worker thread ended without reporting result"
                ))
            });
        }

        if results.values().any(Result::is_err) {
            let parent_cancelled = ctx.cancel.is_cancelled();
            let outcomes = self.record_parallel_layer_outcomes(
                &ctx.run,
                &batch_ids,
                &results,
                parent_cancelled,
            )?;
            let summary = parallel_layer_failure_summary(&outcomes);
            self.state.record_event(
                &ctx.run.id,
                workflow_events::PARALLEL_LAYER_FAILED,
                &workflow_events::ParallelLayerPayload::failed(
                    batch_ids.clone(),
                    outcomes,
                    &summary,
                ),
            )?;
            if parallel_results_any_blocked(&results) {
                return Err(BlockedError::new(summary).into());
            }
            if parent_cancelled || parallel_results_all_cancelled(&results) {
                return Err(CancelledError::new("run cancelled").into());
            }
            bail!(summary);
        }

        let outcomes = parallel_layer_success_outcomes(&results);
        self.state.record_event(
            &ctx.run.id,
            workflow_events::PARALLEL_LAYER_COMPLETED,
            &workflow_events::ParallelLayerPayload::completed(batch_ids, outcomes),
        )?;
        results.into_values().collect()
    }

    fn record_parallel_layer_outcomes(
        &self,
        run: &Run,
        batch_ids: &[String],
        results: &BTreeMap<String, Result<SliceWorkerOutcome>>,
        parent_cancelled: bool,
    ) -> Result<Vec<serde_json::Value>> {
        for (slice_id, result) in results {
            if let Err(err) = result
                && err.downcast_ref::<CancelledError>().is_some()
                && !parent_cancelled
            {
                self.state.update_slice_status(
                    &run.id,
                    slice_id,
                    SliceStatus::Cancelled,
                    "cancelled after sibling failure in parallel worker layer",
                )?;
            }
        }

        let existing_slice_runs = self.state.get_slice_runs(&run.id)?;
        let existing_ids: BTreeSet<_> = existing_slice_runs
            .iter()
            .map(|slice_run| slice_run.slice_id.clone())
            .collect();
        for slice_id in batch_ids {
            if existing_ids.contains(slice_id) {
                continue;
            }
            if let Some(result) = results.get(slice_id) {
                self.state.upsert_slice_run(&SliceRun {
                    run_id: run.id.clone(),
                    slice_id: slice_id.clone(),
                    status: parallel_result_slice_status(result),
                    branch: format!("khazad/{}/{}", run.id, slice_id),
                    commit_sha: result
                        .as_ref()
                        .map(|outcome| outcome.result.commit_sha.clone())
                        .unwrap_or_default(),
                    attempts: result.as_ref().map(|outcome| outcome.attempts).unwrap_or(0),
                    last_error: if result.is_err() {
                        parallel_result_summary(result)
                    } else {
                        String::new()
                    },
                })?;
            }
        }

        let slice_runs: BTreeMap<_, _> = self
            .state
            .get_slice_runs(&run.id)?
            .into_iter()
            .map(|slice_run| (slice_run.slice_id.clone(), slice_run))
            .collect();
        let mut outcomes = Vec::new();
        for slice_id in batch_ids {
            let mut status = results
                .get(slice_id)
                .map(parallel_result_status)
                .unwrap_or("failed")
                .to_string();
            let mut summary = results
                .get(slice_id)
                .map(parallel_result_summary)
                .unwrap_or_else(|| "worker did not report a result".to_string());
            let mut attempts = 0;
            if let Some(slice_run) = slice_runs.get(slice_id) {
                status = slice_run.status.as_str().to_string();
                attempts = slice_run.attempts;
                if !slice_run.last_error.trim().is_empty() {
                    summary = slice_run.last_error.clone();
                }
            }
            let preserved = results
                .get(slice_id)
                .and_then(|result| result.as_ref().ok())
                .map(|outcome| {
                    json!({
                        "branch": &outcome.branch,
                        "commit_sha": &outcome.result.commit_sha,
                        "disposition": "preserved_unmerged_due_to_layer_atomicity",
                    })
                });
            let mut outcome = json!({
                "slice_id": slice_id,
                "status": status,
                "attempts": attempts,
                "summary": &summary,
            });
            if let Some(preserved) = preserved
                && let Some(object) = outcome.as_object_mut()
            {
                object.insert("preserved_unmerged".to_string(), preserved);
            }
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    fn prior_completed_worker_results(
        &self,
        run: &Run,
        store: &artifact::Store,
        slice_runs: &[SliceRun],
    ) -> Vec<WorkerResult> {
        let mut previous_results = artifact::read_json::<ImplementationSummary>(
            store.output_path(&run.id, "final-report.json"),
        )
        .or_else(|_| {
            artifact::read_json::<ImplementationSummary>(
                store.output_path(&run.id, "implementation-summary.json"),
            )
        })
        .map(|summary| {
            summary
                .completed_slices
                .into_iter()
                .map(|result| (result.slice_id.clone(), result))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

        slice_runs
            .iter()
            .filter(|slice_run| slice_run.status == SliceStatus::Merged)
            .map(|slice_run| {
                previous_results
                    .remove(&slice_run.slice_id)
                    .or_else(|| read_worker_result(&self.state, store, &run.id, slice_run))
                    .unwrap_or_else(|| WorkerResult {
                        slice_id: slice_run.slice_id.clone(),
                        status: "complete".to_string(),
                        summary: "slice was merged before this resume".to_string(),
                        commit_sha: slice_run.commit_sha.clone(),
                        ..WorkerResult::default()
                    })
            })
            .collect()
    }

    fn initial_worker_layers(
        &self,
        run: &Run,
        worker_slices: &[Slice],
        gate_slices: &mut Vec<Slice>,
        completed_ids: &BTreeSet<String>,
        integration_worktree: &Path,
    ) -> Result<VecDeque<Vec<Slice>>> {
        let selected_ids = selected_slice_ids(&run.selected_slice_id);
        let missing_from_gate = selected_ids
            .iter()
            .any(|id| !gate_slices.iter().any(|slice| slice.id == *id));
        if !missing_from_gate {
            return Ok(artifact::dependency_layers(worker_slices)?.into());
        }
        let integration_slices = artifact::Store::new(integration_worktree).load_slices()?;
        let by_id: BTreeMap<_, _> = integration_slices
            .iter()
            .map(|slice| (slice.id.as_str(), slice))
            .collect();
        for id in &selected_ids {
            if gate_slices.iter().any(|slice| slice.id == *id) {
                continue;
            }
            let Some(slice) = by_id.get(id.as_str()) else {
                return Err(BlockedError::new(format!(
                    "replan_apply_incomplete: selected slice {id:?} is missing from integration worktree {}; resume cannot verify or launch workers against an ambiguous generated-slice contract",
                    integration_worktree.display()
                ))
                .into());
            };
            gate_slices.push((*slice).clone());
        }
        let mut layers = VecDeque::new();
        for id in selected_ids {
            if completed_ids.contains(&id) {
                continue;
            }
            let Some(slice) = by_id.get(id.as_str()) else {
                return Err(BlockedError::new(format!(
                    "replan_apply_incomplete: selected slice {id:?} is missing from integration worktree {}; resume cannot launch workers against an ambiguous generated-slice contract",
                    integration_worktree.display()
                ))
                .into());
            };
            layers.push_back(vec![(*slice).clone()]);
        }
        Ok(layers)
    }

    fn apply_accepted_replan_proposals_at_checkpoint(
        &self,
        run: &mut Run,
        checkpoint: &str,
        integration_worktree: &Path,
        worker_layers: &mut VecDeque<Vec<Slice>>,
        gate_slices: &mut Vec<Slice>,
    ) -> Result<usize> {
        let proposals = self.state.list_replan_proposals(&run.id)?;
        let mut applied = 0;
        for proposal in proposals {
            if !proposal_needs_followup_apply(&proposal) {
                continue;
            }
            let mode = self.followup_apply_mode_for_proposal(run, &proposal)?;
            if self.apply_followup_proposal_at_checkpoint(
                run,
                checkpoint,
                integration_worktree,
                worker_layers,
                gate_slices,
                &proposal,
                mode,
            )? {
                applied += 1;
            }
        }
        Ok(applied)
    }

    fn followup_apply_mode_for_proposal(
        &self,
        run: &Run,
        proposal: &ReplanProposal,
    ) -> Result<FollowupApplyMode> {
        let Some(decision) = proposal.operator_decision.as_ref() else {
            return Ok(FollowupApplyMode::AppendAndRun);
        };
        if decision.source == "frontier_policy"
            && decision.authorizer == format!("envelope:{}", run.id)
        {
            if let Some(classification) = proposal.frontier_classification.as_ref() {
                return Ok(followup_apply_mode_for_autonomy(
                    classification.autonomy_level,
                ));
            }
            let (envelope, _) = self.state.get_frontier_state(&run.id)?;
            if let Some(envelope) = envelope {
                return Ok(followup_apply_mode_for_autonomy(envelope.autonomy_level));
            }
        }
        Ok(FollowupApplyMode::AppendAndRun)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_followup_proposal_at_checkpoint(
        &self,
        run: &mut Run,
        checkpoint: &str,
        integration_worktree: &Path,
        worker_layers: &mut VecDeque<Vec<Slice>>,
        gate_slices: &mut Vec<Slice>,
        proposal: &ReplanProposal,
        mode: FollowupApplyMode,
    ) -> Result<bool> {
        let Some(mut decision) = proposal.operator_decision.clone() else {
            return Ok(false);
        };
        let Some(draft) = applyable_followup_draft(proposal) else {
            decision.apply_status = "refused".to_string();
            decision.apply_reason =
                "accepted proposal no longer carries one typed add_followup_slice draft"
                    .to_string();
            self.state
                .replace_replan_decision(&run.id, &proposal.id, &decision)?;
            return Ok(false);
        };
        let generated_id = draft.id.trim().to_string();
        if generated_id.is_empty() {
            decision.apply_status = "refused".to_string();
            decision.apply_reason = "follow-up slice draft id is empty".to_string();
            self.state
                .replace_replan_decision(&run.id, &proposal.id, &decision)?;
            return Ok(false);
        }
        let before_queue = selected_slice_ids(&run.selected_slice_id);
        let before_sha = gitutil::head_sha(integration_worktree).unwrap_or_default();
        if decision.queue_before.is_empty() {
            decision.queue_before = before_queue.clone();
            decision.queue_before_hash = queue_snapshot_hash(&before_queue);
        }
        if decision.apply_before_checkpoint_id.trim().is_empty() {
            decision.apply_before_checkpoint_id = checkpoint_id(checkpoint, "before", &before_sha);
        }
        decision.apply_status = "incomplete".to_string();
        decision.apply_reason = format!(
            "daemon started applying proposal {} at checkpoint {checkpoint}",
            proposal.id
        );
        decision.generated_slice_id = generated_id.clone();
        self.state
            .replace_replan_decision(&run.id, &proposal.id, &decision)?;
        self.state.record_event(
            &run.id,
            "replan_apply_started",
            &json!({
                "proposal_id": proposal.id,
                "slice_id": generated_id,
                "checkpoint": checkpoint,
                "queue_before": before_queue,
                "queue_before_hash": decision.queue_before_hash,
                "integration_head": before_sha,
                "apply_mode": mode.as_str(),
            }),
        )?;

        let apply_result = self.ensure_generated_slice_committed(
            run,
            proposal,
            &decision,
            &draft,
            integration_worktree,
        );
        let generated_slice = match apply_result {
            Ok(slice) => slice,
            Err(err) if is_apply_refusal(&err) => {
                decision.applied = false;
                decision.applied_at = None;
                decision.apply_status = "refused".to_string();
                decision.apply_reason = err.to_string();
                self.state
                    .replace_replan_decision(&run.id, &proposal.id, &decision)?;
                self.state.record_event(
                    &run.id,
                    "replan_apply_refused",
                    &json!({
                        "proposal_id": proposal.id,
                        "slice_id": generated_id,
                        "checkpoint": checkpoint,
                        "reason": decision.apply_reason,
                        "remediation": "supersede with a valid follow-up proposal or start a new run",
                    }),
                )?;
                return Ok(false);
            }
            Err(err) => {
                decision.applied = false;
                decision.applied_at = None;
                decision.apply_status = "incomplete".to_string();
                decision.apply_reason = err.to_string();
                self.state
                    .replace_replan_decision(&run.id, &proposal.id, &decision)?;
                self.state.record_event(
                    &run.id,
                    "replan_apply_incomplete",
                    &json!({
                        "proposal_id": proposal.id,
                        "slice_id": generated_id,
                        "checkpoint": checkpoint,
                        "reason": decision.apply_reason,
                        "remediation": format!("khazad-doom resume {}", run.id),
                    }),
                )?;
                return Err(BlockedError::new(format!(
                    "replan_apply_incomplete for proposal {}: {}; resume will retry idempotent apply before any generated slice worker launches",
                    proposal.id, decision.apply_reason
                ))
                .into());
            }
        };

        let mut queue_after = selected_slice_ids(&run.selected_slice_id);
        let already_selected = queue_after.iter().any(|id| id == &generated_id);
        let mut appended = false;
        let mut worker_enqueued = false;
        if mode == FollowupApplyMode::AppendAndRun {
            if !already_selected {
                queue_after.push(generated_id.clone());
                let updated = self
                    .state
                    .update_run_selected_slices(&run.id, &queue_after.join(","))?;
                run.selected_slice_id = updated.selected_slice_id;
                appended = true;
            }
            let already_layered = worker_layers
                .iter()
                .flatten()
                .any(|slice| slice.id == generated_id);
            let already_merged = slice_run_is_merged(&self.state, &run.id, &generated_id)?;
            if !already_layered && !already_merged {
                worker_layers.push_back(vec![generated_slice.clone()]);
                worker_enqueued = true;
            }
            if !gate_slices.iter().any(|slice| slice.id == generated_id) {
                gate_slices.push(generated_slice.clone());
            }
            if !already_merged {
                self.state.upsert_slice_run(&SliceRun {
                    run_id: run.id.clone(),
                    slice_id: generated_id.clone(),
                    status: SliceStatus::Pending,
                    branch: String::new(),
                    commit_sha: String::new(),
                    attempts: 0,
                    last_error: String::new(),
                })?;
            }
        }
        let after_sha = gitutil::head_sha(integration_worktree).unwrap_or_default();
        let final_queue = selected_slice_ids(&run.selected_slice_id);
        decision.applied = true;
        decision.applied_at = Some(Utc::now());
        decision.apply_status = "applied".to_string();
        decision.apply_reason = match mode {
            FollowupApplyMode::PromoteOnly => {
                "generated follow-up slice committed for a future run; promote autonomy does not append or run it in the current run"
                    .to_string()
            }
            FollowupApplyMode::AppendAndRun => {
                "generated follow-up slice committed and appended serially".to_string()
            }
        };
        decision.generated_slice_id = generated_id.clone();
        decision.generated_slice_commit = after_sha.clone();
        decision.apply_after_checkpoint_id = checkpoint_id(checkpoint, "after", &after_sha);
        decision.queue_after = final_queue.clone();
        decision.queue_after_hash = queue_snapshot_hash(&final_queue);
        self.state
            .replace_replan_decision(&run.id, &proposal.id, &decision)?;
        self.state.record_event(
            &run.id,
            "frontier_slice_promoted",
            &json!({
                "proposal_id": proposal.id,
                "slice_id": generated_id,
                "parent_slice_id": proposal.source.slice_id,
                "checkpoint": checkpoint,
                "commit_sha": after_sha,
                "queue_before": decision.queue_before,
                "queue_before_hash": decision.queue_before_hash,
                "queue_after": final_queue,
                "queue_after_hash": decision.queue_after_hash,
                "appended": appended,
                "serial_append": mode == FollowupApplyMode::AppendAndRun,
                "worker_enqueued": worker_enqueued,
                "apply_mode": mode.as_str(),
            }),
        )?;
        Ok(true)
    }

    fn ensure_generated_slice_committed(
        &self,
        _run: &Run,
        proposal: &ReplanProposal,
        decision: &ReplanDecision,
        draft: &FollowupSliceDraft,
        integration_worktree: &Path,
    ) -> Result<Slice> {
        let integration_store = artifact::Store::new(integration_worktree);
        let rel_path = format!(".workflow/slices/{}.json", draft.id);
        let slice_path = integration_store.slice_path(&draft.id);
        let mut slice = if slice_path.exists() {
            let existing: Slice = artifact::read_json(&slice_path)
                .with_context(|| format!("read generated slice {}", slice_path.display()))?;
            if existing
                .provenance()
                .as_ref()
                .is_some_and(|provenance| provenance.origin_proposal_id == proposal.id)
            {
                if !slice_matches_draft(&existing, draft) {
                    bail!(
                        "replan_apply_incomplete: generated slice {:?} exists for proposal {} but no longer matches the accepted draft",
                        draft.id,
                        proposal.id
                    );
                }
                existing
            } else {
                bail!(
                    "apply_refused: slice id {:?} already exists and was not generated from proposal {}",
                    draft.id,
                    proposal.id
                );
            }
        } else {
            let existing_slices = integration_store
                .load_slices()
                .context("load current slice graph before applying follow-up")?;
            if let Err(err) = validate_followup_slice_draft(draft, &existing_slices) {
                bail!("apply_refused: {err:#}");
            }
            let mut slice = draft.to_slice();
            slice.set_provenance(SliceProvenance {
                parent_slice_id: proposal.source.slice_id.clone(),
                origin_proposal_id: proposal.id.clone(),
                generation: followup_generation(&existing_slices, &proposal.source.slice_id),
                created_by: provenance_created_by(decision),
                created_at: Utc::now().to_rfc3339(),
            });
            artifact::write_json(&slice_path, &slice)
                .with_context(|| format!("write generated slice {}", slice_path.display()))?;
            slice
        };
        artifact::validate_slice(&slice)
            .with_context(|| format!("validate generated slice {:?}", slice.id))?;
        integration_store
            .load_slices()
            .context("revalidate current slice graph after generated follow-up apply")?;
        let message = format!(
            "khazad(slice:{}): promote follow-up from {} via {}",
            draft.id,
            display_or_dash(&proposal.source.slice_id),
            proposal.id
        );
        gitutil::commit_paths(integration_worktree, &[rel_path.as_str()], &message)
            .with_context(|| format!("commit generated slice {:?}", draft.id))?;
        let status = gitutil::run(
            integration_worktree,
            &["status", "--porcelain", "--", rel_path.as_str()],
        )?;
        if !status.trim().is_empty() {
            bail!(
                "replan_apply_incomplete: generated slice {:?} is not committed cleanly: {}",
                draft.id,
                status.trim()
            );
        }
        if slice.provenance().is_none() {
            let existing: Slice = artifact::read_json(&slice_path)
                .with_context(|| format!("reread generated slice {}", slice_path.display()))?;
            slice = existing;
        }
        Ok(slice)
    }

    fn write_checkpoint(
        &self,
        run: &Run,
        all_slices: &[Slice],
        completed_ids: &BTreeSet<String>,
        integration_worktree: &Path,
    ) -> Result<()> {
        let current_sha = gitutil::head_sha(integration_worktree).unwrap_or_default();
        let remaining_slices = all_slices
            .iter()
            .filter(|slice| !completed_ids.contains(&slice.id))
            .map(|slice| slice.id.clone())
            .collect();
        let checkpoint = RunCheckpoint {
            run_id: run.id.clone(),
            integration_branch: run.integration_branch.clone(),
            base_sha: run.base_sha.clone(),
            current_sha,
            completed_slices: completed_ids.iter().cloned().collect(),
            remaining_slices,
            updated_at: Utc::now(),
        };
        artifact::Store::new(&run.repo_path).write_checkpoint(&checkpoint)?;
        self.state
            .record_event(&run.id, workflow_events::CHECKPOINT_WRITTEN, &checkpoint)?;
        Ok(())
    }

    fn run_slice_worker(
        &self,
        slice: &Slice,
        ctx: &WorkerExecutionContext,
    ) -> Result<SliceWorkerOutcome> {
        let run = &ctx.run;
        let cancel = &ctx.cancel;
        let runner = ctx.runner.clone();
        let config = &ctx.config;
        let economics = ctx.economics.clone();
        let verification_cache = ctx.verification_cache.clone();
        let store = artifact::Store::new(&run.repo_path);
        let mut all_checks = Vec::new();
        let mut last_failure = String::new();
        let mut primary_failure: Option<String> = None;
        let mut secondary_failures: Vec<String> = Vec::new();
        let consumed_retry_budget = self
            .state
            .get_slice_runs(&run.id)?
            .into_iter()
            .find(|slice_run| slice_run.slice_id == slice.id)
            .map(|slice_run| slice_run.attempts)
            .unwrap_or_default();
        if consumed_retry_budget >= MAX_WORKER_ATTEMPTS {
            bail!(
                "worker retry budget exhausted for slice {} ({consumed_retry_budget}/{MAX_WORKER_ATTEMPTS})",
                slice.id
            );
        }
        for attempt in consumed_retry_budget + 1..=MAX_WORKER_ATTEMPTS {
            check_cancelled(cancel)?;
            // The allocation commits immutable identity and current projection before
            // any worktree, handoff, artifact, or worker side effect.
            // A durable allocation can exist without a successfully-created Git
            // branch. Only a resolvable retained branch may seed a retry.
            let prior_branch = self
                .state
                .list_worker_attempt_ledger(&run.id, &slice.id)?
                .into_iter()
                .rev()
                .find_map(|row| {
                    (!row.branch.is_empty()
                        && gitutil::run(&run.repo_path, &["rev-parse", "--verify", &row.branch])
                            .is_ok())
                    .then_some(row.branch)
                });
            let ledger = self.state.allocate_worker_attempt(
                &run.id,
                &slice.id,
                ctx.execution_epoch,
                attempt,
                0,
                0,
                "slice-worker",
                &ctx.root_worktree,
            )?;
            let _terminal_guard = WorkerAttemptTerminalGuard::new(&self.state, ledger.launch_id);
            let worker_token = new_worker_token();
            if let Err(err) =
                self.state
                    .store_worker_launch_token(&run.id, ledger.launch_id, &worker_token)
            {
                let _ =
                    self.state
                        .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
                return Err(err);
            }
            self.state.record_event(
                &run.id,
                workflow_events::SLICE_STARTED,
                &workflow_events::SliceStartedPayload::new(&slice.id),
            )?;
            let worker_worktree = PathBuf::from(&ledger.worktree);
            let worker_branch = ledger.branch.clone();
            let worker_base = prior_branch.unwrap_or_else(|| ctx.slice_base_sha.clone());
            {
                let _git_lock = WORKTREE_ADD_LOCK
                    .lock()
                    .expect("worktree add mutex poisoned");
                if let Err(err) = gitutil::worktree_add(
                    &run.repo_path,
                    &worker_worktree,
                    &worker_branch,
                    &worker_base,
                ) {
                    let _ = self.state.finish_worker_attempt(
                        ledger.launch_id,
                        "failed",
                        &err.to_string(),
                    );
                    return Err(err).context("create worker worktree");
                }
            }
            self.state
                .activate_slice_attempt(&run.id, &slice.id, attempt)?;
            self.mark_progress(
                &run.id,
                "worker_started",
                &slice.id,
                attempt,
                "",
                "slice worker started",
            );
            if let Err(err) = self.run_worktree_setup(
                run,
                &slice.id,
                attempt,
                Some(&ledger.output_stem),
                &worker_worktree,
                config,
                economics.clone(),
                verification_cache.clone(),
                cancel,
            ) {
                let _ =
                    self.state
                        .finish_worker_attempt(ledger.launch_id, "failed", &err.to_string());
                return Err(err);
            }
            let output_path = store.output_path(&run.id, &format!("{}.json", ledger.output_stem));
            let runner_metadata = runner.metadata();
            let (mission_envelope, frontier_budget) = self.state.get_frontier_state(&run.id)?;
            let handoff = Handoff {
                run_id: run.id.clone(),
                role: "slice-worker".to_string(),
                repo_path: run.repo_path.clone(),
                worktree_path: worker_worktree.to_string_lossy().to_string(),
                branch: worker_branch.clone(),
                slice: slice.clone(),
                dependency_summary: ctx.dependency_summary.clone(),
                worker_profile: worker_profile_evidence(runner.name(), &runner_metadata),
                mission_envelope,
                frontier_budget,
                agent_profile: runner_metadata.profile.clone(),
                agent_provider: runner_metadata.provider.clone(),
                agent_model: runner_metadata.model.clone(),
                agent_reasoning: runner_metadata.reasoning.clone(),
                agent_mode: runner_metadata.mode.clone(),
                profile_summary: runner_metadata.profile_summary(),
                launch_summary: runner_metadata.launch_summary(),
                output_path: output_path.to_string_lossy().to_string(),
                contract: "Implement only this slice, commit all intended changes, leave a clean worktree, and return JSON."
                    .to_string(),
            };
            let handoff_path = match self.write_worker_handoff_with_plan_revisions(
                &store,
                run,
                &handoff,
                &ledger.output_stem,
            ) {
                Ok(path) => path,
                Err(err) => {
                    let _ = self.state.finish_worker_attempt(
                        ledger.launch_id,
                        "failed",
                        &err.to_string(),
                    );
                    return Err(err);
                }
            };
            let prompt = worker_prompt(&handoff_path.to_string_lossy(), &handoff, &last_failure);
            self.mark_progress(
                &run.id,
                "worker_running",
                &slice.id,
                attempt,
                runner.name(),
                "slice worker is running",
            );
            self.state.mark_worker_attempt_launched(ledger.launch_id)?;
            let worker_attempt =
                match self.run_worker_attempt_with_envelope(WorkerAttemptRunRequest {
                    run,
                    slice,
                    attempt,
                    launch_id: ledger.launch_id,
                    launch_stem: &ledger.output_stem,
                    runner: runner.clone(),
                    runner_metadata: &runner_metadata,
                    handoff: &handoff,
                    prompt,
                    worker_worktree: &worker_worktree,
                    worker_branch: &worker_branch,
                    output_path: &output_path,
                    config,
                    economics: &economics,
                    cancel,
                    worker_token: &worker_token,
                    cockpit_mode: ctx.cockpit_mode,
                    native_pi_tui_worker: ctx.native_pi_tui_worker,
                    primary_failure: &mut primary_failure,
                    secondary_failures: &mut secondary_failures,
                    last_failure: &mut last_failure,
                }) {
                    Ok(result) => result,
                    Err(err) => {
                        let _ = self.state.finish_worker_attempt(
                            ledger.launch_id,
                            "failed",
                            &err.to_string(),
                        );
                        return Err(err);
                    }
                };
            let valid_attempt = match worker_attempt {
                WorkerAttemptRunResult::Valid(worker_attempt) => *worker_attempt,
                WorkerAttemptRunResult::Continue => {
                    let _ = self.state.finish_worker_attempt(
                        ledger.launch_id,
                        "failed",
                        "worker envelope requested retry",
                    );
                    continue;
                }
            };
            let ValidWorkerAttempt {
                result: mut worker_result,
                launch_id: active_launch_id,
                launch_stem: active_launch_stem,
                branch: worker_branch,
                worktree: worker_worktree,
                output_path,
                _terminal_guard: _active_terminal_guard,
            } = valid_attempt;
            self.create_worker_candidate_followup_slice_proposals(
                run,
                slice,
                attempt,
                &output_path,
                &mut worker_result,
            )?;
            self.create_worker_finding_replan_proposals(
                run,
                slice,
                attempt,
                &output_path,
                &mut worker_result,
            )?;
            artifact::write_json(&output_path, &worker_result)?;
            if let Err(err) = self.run_worktree_setup(
                run,
                &slice.id,
                attempt,
                Some(&active_launch_stem),
                &worker_worktree,
                config,
                economics.clone(),
                verification_cache.clone(),
                cancel,
            ) {
                self.state.finish_worker_attempt(
                    active_launch_id,
                    "failed",
                    &format!("post-worker worktree setup failed: {err}"),
                )?;
                self.state.upsert_slice_run(&SliceRun {
                    run_id: run.id.clone(),
                    slice_id: slice.id.clone(),
                    status: SliceStatus::Blocked,
                    branch: worker_branch.clone(),
                    commit_sha: gitutil::head_sha(&worker_worktree).unwrap_or_default(),
                    attempts: attempt,
                    last_error: err.to_string(),
                })?;
                return Err(err);
            }

            let check = match self.lightweight_check(
                LightweightCheckContext {
                    run_id: &run.id,
                    slice,
                    worker_worktree: &worker_worktree,
                    base_sha: &ctx.slice_base_sha,
                    attempt,
                    config,
                    economics: economics.clone(),
                    verification_cache: verification_cache.clone(),
                },
                cancel,
            ) {
                Ok(check) => check,
                Err(err) => {
                    self.state.finish_worker_attempt(
                        active_launch_id,
                        "failed",
                        &format!("lightweight check failed: {err}"),
                    )?;
                    return Err(err);
                }
            };
            let check_path =
                store.output_path(&run.id, &format!("{}.check.json", active_launch_stem));
            artifact::write_json(&check_path, &check)?;
            all_checks.push(check.clone());

            if check.status == "passed" && worker_result.status == "complete" {
                if worker_result.commit_sha.is_empty() {
                    worker_result.commit_sha = check.worker_head.clone();
                }
                self.mark_progress(
                    &run.id,
                    "ready_to_merge",
                    &slice.id,
                    attempt,
                    "",
                    "slice passed worker checks and is ready to merge",
                );
                self.state.upsert_slice_run(&SliceRun {
                    run_id: run.id.clone(),
                    slice_id: slice.id.clone(),
                    status: SliceStatus::ReadyToMerge,
                    branch: worker_branch.clone(),
                    commit_sha: worker_result.commit_sha.clone(),
                    attempts: attempt,
                    last_error: String::new(),
                })?;
                self.state
                    .finish_worker_attempt(active_launch_id, "succeeded", "")?;
                return Ok(SliceWorkerOutcome {
                    slice: slice.clone(),
                    result: worker_result,
                    checks: all_checks,
                    branch: worker_branch,
                    attempts: attempt,
                });
            }

            last_failure = if worker_result.status == "failed" && !worker_result.summary.is_empty()
            {
                worker_result.summary.clone()
            } else {
                check.summary.clone()
            };
            if check.status != "passed" || worker_result.status == "failed" {
                remember_attempt_failure(
                    &mut primary_failure,
                    &mut secondary_failures,
                    &last_failure,
                );
                self.record_worker_attempt_failure(WorkerAttemptFailureRecord {
                    run,
                    slice,
                    attempt,
                    envelope_retry: 0,
                    phase: "worker_verify",
                    failure_kind: worker_attempt_failure_kind(&check, &worker_result),
                    summary: &last_failure,
                    evidence_path: &check_path,
                    retry_disposition: worker_attempt_retry_disposition(attempt, &check),
                    repair_disposition: worker_attempt_repair_disposition(attempt, &check),
                    primary_failure: primary_failure.as_deref(),
                    secondary_failures: &secondary_failures,
                })?;
            }
            if check.verification_cancelled
                && check.failure_kind != "verification_restoration_failed"
            {
                self.state.finish_worker_attempt(
                    active_launch_id,
                    "interrupted",
                    "run cancelled",
                )?;
                return Err(CancelledError::new("run cancelled").into());
            }
            if check_failure_needs_operator(&check) {
                let message = final_attempt_failure_message(
                    &slice.id,
                    primary_failure.as_deref(),
                    &last_failure,
                    &secondary_failures,
                );
                self.state.upsert_slice_run(&SliceRun {
                    run_id: run.id.clone(),
                    slice_id: slice.id.clone(),
                    status: SliceStatus::Blocked,
                    branch: worker_branch.clone(),
                    commit_sha: check.worker_head.clone(),
                    attempts: attempt,
                    last_error: message.clone(),
                })?;
                self.state
                    .finish_worker_attempt(active_launch_id, "failed", &message)?;
                return Err(BlockedError::new(message).into());
            }
            if worker_result.status == "blocked" {
                self.state.update_slice_status(
                    &run.id,
                    &slice.id,
                    SliceStatus::Blocked,
                    &worker_result.summary,
                )?;
                let message = format!("worker reported blocked: {}", worker_result.summary);
                self.state
                    .finish_worker_attempt(active_launch_id, "failed", &message)?;
                return Err(BlockedError::new(message).into());
            }
            if attempt == MAX_WORKER_ATTEMPTS {
                self.state
                    .finish_worker_attempt(active_launch_id, "failed", &last_failure)?;
                if let Some(outcome) =
                    self.run_targeted_slice_repair(TargetedSliceRepairRequest {
                        run,
                        slice,
                        attempt,
                        runner: runner.clone(),
                        handoff: &handoff,
                        worker_worktree: &worker_worktree,
                        slice_base_sha: &ctx.slice_base_sha,
                        check_path: &check_path,
                        check: &check,
                        config,
                        economics: &economics,
                        verification_cache: &verification_cache,
                        cancel,
                        cockpit_mode: ctx.cockpit_mode,
                        native_pi_tui_worker: ctx.native_pi_tui_worker,
                        all_checks: &mut all_checks,
                    })?
                {
                    return Ok(outcome);
                }
                let message = final_attempt_failure_message(
                    &slice.id,
                    primary_failure.as_deref(),
                    &last_failure,
                    &secondary_failures,
                );
                self.state.update_slice_status(
                    &run.id,
                    &slice.id,
                    SliceStatus::Failed,
                    &message,
                )?;
                bail!(message);
            }
            self.state
                .finish_worker_attempt(active_launch_id, "failed", &last_failure)?;
            self.state.upsert_slice_run(&SliceRun {
                run_id: run.id.clone(),
                slice_id: slice.id.clone(),
                status: SliceStatus::RepairNeeded,
                branch: worker_branch.clone(),
                commit_sha: check.worker_head.clone(),
                attempts: attempt,
                last_error: check.summary.clone(),
            })?;
        }
        let message = final_attempt_failure_message(
            &slice.id,
            primary_failure.as_deref(),
            &last_failure,
            &secondary_failures,
        );
        self.state
            .update_slice_status(&run.id, &slice.id, SliceStatus::Failed, &message)?;
        bail!(message)
    }

    fn lightweight_check(
        &self,
        ctx: LightweightCheckContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<CheckResult> {
        let mut check = CheckResult {
            slice_id: ctx.slice.id.clone(),
            status: "passed".to_string(),
            summary: "lightweight checks passed".to_string(),
            tests_run: Vec::new(),
            verification_commands: Vec::new(),
            findings: Vec::new(),
            attempt: ctx.attempt,
            worker_head: String::new(),
            worktree_ok: true,
            commit_found: true,
            verification_cancelled: false,
            failure_kind: String::new(),
        };

        let status = match gitutil::status_porcelain(ctx.worker_worktree) {
            Ok(status) => status,
            Err(err) => {
                check.status = "failed".to_string();
                check.summary = err.to_string();
                check.failure_kind = "git_status_failed".to_string();
                return Ok(check);
            }
        };
        if !status.trim().is_empty() {
            check.worktree_ok = false;
            check.status = "failed".to_string();
            check.summary = "worker worktree is not clean".to_string();
            check.failure_kind = "dirty_worktree".to_string();
            check.findings.push(Finding {
                id: String::new(),
                severity: "error".to_string(),
                action: "auto-fix".to_string(),
                file: String::new(),
                line: 0,
                description: "worker must commit or remove all worktree changes before handoff"
                    .to_string(),
            });
            return Ok(check);
        }

        let head = match gitutil::head_sha(ctx.worker_worktree) {
            Ok(head) => head,
            Err(err) => {
                check.status = "failed".to_string();
                check.summary = err.to_string();
                check.failure_kind = "git_head_failed".to_string();
                return Ok(check);
            }
        };
        check.worker_head = head.clone();
        if head == ctx.base_sha {
            check.commit_found = false;
            check.status = "failed".to_string();
            check.summary = "worker did not create a slice commit".to_string();
            check.failure_kind = "missing_slice_commit".to_string();
            check.findings.push(Finding {
                id: String::new(),
                severity: "error".to_string(),
                action: "auto-fix".to_string(),
                file: String::new(),
                line: 0,
                description: "slice worker must commit completed work on its branch".to_string(),
            });
            return Ok(check);
        }

        let authorized_areas = self.slice_areas_with_accepted_revision_grants(
            ctx.run_id,
            &ctx.slice.id,
            &ctx.slice.areas,
        )?;
        if let Some(outside) = changed_files_outside_slice_areas(
            ctx.worker_worktree,
            ctx.base_sha,
            &head,
            &authorized_areas,
        )? {
            check.status = "failed".to_string();
            check.summary = format!(
                "worker changed files outside slice areas: {}",
                outside.join(", ")
            );
            check.failure_kind = "scope_violation".to_string();
            check.findings.push(Finding {
                id: String::new(),
                severity: "error".to_string(),
                action: "auto-fix".to_string(),
                file: outside.first().cloned().unwrap_or_default(),
                line: 0,
                description: format!(
                    "slice areas/grants are [{}]; worker changed outside-area files: {}",
                    authorized_areas.join(", "),
                    outside.join(", ")
                ),
            });
            return Ok(check);
        }

        let verification = WorkflowGate::with_economics(
            self.progress_reporter(ctx.run_id),
            ctx.economics.clone(),
            ctx.verification_cache.clone(),
        )
        .verify_slice_commands(
            SliceVerificationRequest {
                slice: ctx.slice,
                worker_worktree: ctx.worker_worktree,
                attempt: ctx.attempt,
                config: ctx.config,
            },
            cancel,
        )?;
        check.tests_run = verification.tests_run;
        check.verification_commands = verification.commands;
        check.verification_cancelled = verification.verification_cancelled;
        if let Some(failure) = verification.failure {
            check.status = "failed".to_string();
            check.summary = failure.summary;
            check.failure_kind = failure.failure_kind;
            check.findings.push(failure.finding);
            return Ok(check);
        }
        Ok(check)
    }

    fn create_worker_candidate_followup_slice_proposals(
        &self,
        run: &Run,
        slice: &Slice,
        attempt: usize,
        output_path: &Path,
        result: &mut WorkerResult,
    ) -> Result<bool> {
        let drafts = result.candidate_followup_slices.clone();
        if drafts.is_empty() {
            return Ok(false);
        }
        let mut validation_slices = artifact::Store::new(&run.repo_path).load_slices()?;
        let mut created = false;
        for (draft_index, draft) in drafts.iter().enumerate() {
            if let Err(err) = validate_followup_slice_draft(draft, &validation_slices) {
                result
                    .findings
                    .push(invalid_candidate_followup_finding(draft_index, draft, &err));
                continue;
            }
            let matching_disposition = matching_proposed_disposition_index(
                &result.findings,
                &result.finding_dispositions,
                draft,
            );
            let trigger_finding_ids = matching_disposition
                .map(|index| {
                    let finding = finding_for_disposition(
                        &result.findings,
                        &result.finding_dispositions[index],
                    );
                    disposition_finding_id(index, finding, &result.finding_dispositions[index])
                })
                .into_iter()
                .collect::<Vec<_>>();
            let proposal = self.state.create_replan_proposal(
                &run.id,
                "",
                ReplanProposalSource {
                    kind: "worker_candidate_followup_slice".to_string(),
                    slice_id: slice.id.clone(),
                    phase: "slice_worker".to_string(),
                    attempt,
                    summary: result.summary.clone(),
                },
                trigger_finding_ids,
                vec![
                    ReplanEvidenceLink {
                        kind: "worker_output".to_string(),
                        path: output_path.to_string_lossy().to_string(),
                        event_id: 0,
                        summary: format!(
                            "candidate_followup_slices[{draft_index}] draft {:?} emitted by worker output",
                            draft.id
                        ),
                    },
                    ReplanEvidenceLink {
                        kind: "worker_attempt".to_string(),
                        path: output_path.to_string_lossy().to_string(),
                        event_id: attempt,
                        summary: format!("slice {} worker attempt {attempt}", slice.id),
                    },
                ],
                vec![ReplanProposedChange::with_followup_slice_draft(
                    "add_followup_slice".to_string(),
                    draft.id.clone(),
                    followup_slice_draft_summary(draft),
                    draft.clone(),
                )],
                "operator_review_required_for_followup_slice_draft",
            )?;
            if let Some(index) = matching_disposition {
                result.finding_dispositions[index].replan_proposal_id = proposal.id.clone();
            }
            self.state.record_event(
                &run.id,
                "candidate_followup_slice_replan_proposal_created",
                &json!({
                    "source": "worker",
                    "slice_id": slice.id,
                    "attempt": attempt,
                    "draft_index": draft_index,
                    "draft_id": draft.id,
                    "proposal_id": proposal.id,
                    "output_path": output_path,
                    "summary": followup_slice_draft_summary(draft),
                }),
            )?;
            validation_slices.push(draft.to_slice());
            self.notify_attention_for_replan(run, &proposal);
            created = true;
        }
        Ok(created)
    }

    fn create_repair_candidate_followup_slice_proposals(
        &self,
        run: &Run,
        attempt: usize,
        output_path: &Path,
        result: &mut RepairResult,
    ) -> Result<bool> {
        let drafts = result.candidate_followup_slices.clone();
        if drafts.is_empty() {
            return Ok(false);
        }
        let mut validation_slices = artifact::Store::new(&run.repo_path).load_slices()?;
        let mut created = false;
        for (draft_index, draft) in drafts.iter().enumerate() {
            if let Err(err) = validate_followup_slice_draft(draft, &validation_slices) {
                result
                    .findings
                    .push(invalid_candidate_followup_finding(draft_index, draft, &err));
                continue;
            }
            let matching_disposition = matching_proposed_disposition_index(
                &result.findings,
                &result.finding_dispositions,
                draft,
            );
            let trigger_finding_ids = matching_disposition
                .map(|index| {
                    let finding = finding_for_disposition(
                        &result.findings,
                        &result.finding_dispositions[index],
                    );
                    disposition_finding_id(index, finding, &result.finding_dispositions[index])
                })
                .into_iter()
                .collect::<Vec<_>>();
            let proposal = self.state.create_replan_proposal(
                &run.id,
                "",
                ReplanProposalSource {
                    kind: "repair_candidate_followup_slice".to_string(),
                    slice_id: String::new(),
                    phase: "integration_repair".to_string(),
                    attempt,
                    summary: result.summary.clone(),
                },
                trigger_finding_ids,
                vec![
                    ReplanEvidenceLink {
                        kind: "repair_output".to_string(),
                        path: output_path.to_string_lossy().to_string(),
                        event_id: 0,
                        summary: format!(
                            "candidate_followup_slices[{draft_index}] draft {:?} emitted by repair output",
                            draft.id
                        ),
                    },
                    ReplanEvidenceLink {
                        kind: "repair_attempt".to_string(),
                        path: output_path.to_string_lossy().to_string(),
                        event_id: attempt,
                        summary: format!("integration repair attempt {attempt}"),
                    },
                ],
                vec![ReplanProposedChange::with_followup_slice_draft(
                    "add_followup_slice".to_string(),
                    draft.id.clone(),
                    followup_slice_draft_summary(draft),
                    draft.clone(),
                )],
                "operator_review_required_for_followup_slice_draft",
            )?;
            if let Some(index) = matching_disposition {
                result.finding_dispositions[index].replan_proposal_id = proposal.id.clone();
            }
            self.state.record_event(
                &run.id,
                "candidate_followup_slice_replan_proposal_created",
                &json!({
                    "source": "integration_repair",
                    "attempt": attempt,
                    "draft_index": draft_index,
                    "draft_id": draft.id,
                    "proposal_id": proposal.id,
                    "output_path": output_path,
                    "summary": followup_slice_draft_summary(draft),
                }),
            )?;
            validation_slices.push(draft.to_slice());
            self.notify_attention_for_replan(run, &proposal);
            created = true;
        }
        Ok(created)
    }

    fn create_worker_finding_replan_proposals(
        &self,
        run: &Run,
        slice: &Slice,
        attempt: usize,
        output_path: &Path,
        result: &mut WorkerResult,
    ) -> Result<()> {
        let proposed_indices = proposed_finding_disposition_indices(&result.finding_dispositions);
        for index in proposed_indices {
            let finding =
                finding_for_disposition(&result.findings, &result.finding_dispositions[index]);
            let finding_id =
                disposition_finding_id(index, finding, &result.finding_dispositions[index]);
            let summary = finding
                .map(|finding| finding.description.clone())
                .unwrap_or_else(|| result.finding_dispositions[index].rationale.clone());
            let proposal = self.state.create_replan_proposal(
                &run.id,
                "",
                ReplanProposalSource {
                    kind: "worker_finding".to_string(),
                    slice_id: slice.id.clone(),
                    phase: "slice_worker".to_string(),
                    attempt,
                    summary: result.summary.clone(),
                },
                vec![finding_id.clone()],
                vec![ReplanEvidenceLink {
                    kind: "worker_output".to_string(),
                    path: output_path.to_string_lossy().to_string(),
                    event_id: 0,
                    summary: format!("worker finding {finding_id}: {summary}"),
                }],
                vec![ReplanProposedChange {
                    kind: "follow_up_or_revision".to_string(),
                    target: finding
                        .map(|finding| finding.file.clone())
                        .filter(|file| !file.trim().is_empty())
                        .unwrap_or_else(|| slice.id.clone()),
                    summary: result.finding_dispositions[index].rationale.clone(),
                }],
                "operator_review_required_for_worker_finding",
            )?;
            result.finding_dispositions[index].replan_proposal_id = proposal.id.clone();
            self.state.record_event(
                &run.id,
                "finding_replan_proposal_created",
                &json!({
                    "source": "worker",
                    "slice_id": slice.id,
                    "attempt": attempt,
                    "finding_id": finding_id,
                    "proposal_id": proposal.id,
                    "output_path": output_path,
                    "summary": summary,
                }),
            )?;
            self.notify_attention_for_replan(run, &proposal);
        }
        Ok(())
    }

    fn create_repair_finding_replan_proposals(
        &self,
        run: &Run,
        attempt: usize,
        output_path: &Path,
        result: &mut RepairResult,
    ) -> Result<bool> {
        let proposed_indices = proposed_finding_disposition_indices(&result.finding_dispositions);
        let created = !proposed_indices.is_empty();
        for index in proposed_indices {
            let finding =
                finding_for_disposition(&result.findings, &result.finding_dispositions[index]);
            let finding_id =
                disposition_finding_id(index, finding, &result.finding_dispositions[index]);
            let summary = finding
                .map(|finding| finding.description.clone())
                .unwrap_or_else(|| result.finding_dispositions[index].rationale.clone());
            let proposal = self.state.create_replan_proposal(
                &run.id,
                "",
                ReplanProposalSource {
                    kind: "repair_finding".to_string(),
                    slice_id: String::new(),
                    phase: "integration_repair".to_string(),
                    attempt,
                    summary: result.summary.clone(),
                },
                vec![finding_id.clone()],
                vec![ReplanEvidenceLink {
                    kind: "repair_output".to_string(),
                    path: output_path.to_string_lossy().to_string(),
                    event_id: 0,
                    summary: format!("repair finding {finding_id}: {summary}"),
                }],
                vec![ReplanProposedChange {
                    kind: "repair_follow_up_or_revision".to_string(),
                    target: finding
                        .map(|finding| finding.file.clone())
                        .filter(|file| !file.trim().is_empty())
                        .unwrap_or_else(|| "integration".to_string()),
                    summary: result.finding_dispositions[index].rationale.clone(),
                }],
                "operator_review_required_for_repair_finding",
            )?;
            result.finding_dispositions[index].replan_proposal_id = proposal.id.clone();
            self.state.record_event(
                &run.id,
                "finding_replan_proposal_created",
                &json!({
                    "source": "integration_repair",
                    "attempt": attempt,
                    "finding_id": finding_id,
                    "proposal_id": proposal.id,
                    "output_path": output_path,
                    "summary": summary,
                }),
            )?;
            self.notify_attention_for_replan(run, &proposal);
        }
        Ok(created)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_repair_authority_proposal(
        &self,
        run: &Run,
        attempt: usize,
        output_path: &Path,
        repair_base: &str,
        repair_head: &str,
        unauthorized: &[String],
        result: &RepairResult,
    ) -> Result<String> {
        let proposal = self.state.create_replan_proposal(
            &run.id,
            "",
            ReplanProposalSource {
                kind: "repair_authority".to_string(),
                slice_id: String::new(),
                phase: "integration_repair".to_string(),
                attempt,
                summary: result.summary.clone(),
            },
            vec!["repair-authority".to_string()],
            vec![ReplanEvidenceLink {
                kind: "repair_output".to_string(),
                path: output_path.to_string_lossy().to_string(),
                event_id: 0,
                summary: format!(
                    "repair revision {repair_head} attempted unauthorized paths: {}",
                    unauthorized.join(", ")
                ),
            }],
            vec![ReplanProposedChange {
                kind: "repair_revision".to_string(),
                target: repair_head.to_string(),
                summary: format!(
                    "Apply repair revision {repair_head} on top of {repair_base} only if operator approves paths: {}",
                    unauthorized.join(", ")
                ),
            }],
            "out_of_authority_repair_requires_operator_approval",
        )?;
        self.state.record_event(
            &run.id,
            "repair_authority_proposal_created",
            &json!({
                "proposal_id": proposal.id,
                "attempt": attempt,
                "repair_base": repair_base,
                "repair_head": repair_head,
                "unauthorized_paths": unauthorized,
                "repair_output_path": output_path,
                "policy": "repair revisions outside slice areas or workflow policy require operator approval before application",
            }),
        )?;
        self.notify_attention_for_replan(run, &proposal);
        Ok(proposal.id)
    }

    fn record_worker_attempt_failure(&self, record: WorkerAttemptFailureRecord<'_>) -> Result<()> {
        self.state.record_event(
            &record.run.id,
            workflow_events::WORKER_ATTEMPT_FAILURE,
            &workflow_events::WorkerAttemptFailurePayload {
                slice_id: record.slice.id.clone(),
                attempt: record.attempt,
                envelope_retry: record.envelope_retry,
                phase: record.phase.to_string(),
                failure_kind: record.failure_kind.to_string(),
                summary: bounded_text(record.summary, 4_000),
                evidence_path: record.evidence_path.to_string_lossy().to_string(),
                retry_disposition: record.retry_disposition.to_string(),
                repair_disposition: record.repair_disposition.to_string(),
                primary_failure: record.primary_failure.map(str::to_string),
                secondary_failures: record.secondary_failures.to_vec(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_invalid_worker_output_attempt(
        &self,
        run: &Run,
        slice: &Slice,
        attempt: usize,
        envelope_retry: usize,
        error: &str,
        worker_worktree: &Path,
        expected_output_path: &Path,
        retry_disposition: &str,
        repair_disposition: &str,
        raw_payload: Option<serde_json::Value>,
        transcript: RunnerTranscript,
    ) -> Result<PathBuf> {
        let store = artifact::Store::new(&run.repo_path);
        store.ensure_run_dirs(&run.id)?;
        let progress = self.state.get_progress(&run.id)?;
        // The expected result path is ledger-scoped for normal worker launches.
        // Derive diagnostics from that immutable identity instead of the retry ordinal.
        let expected_stem = expected_output_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("worker-output");
        let invalid_output_name = if envelope_retry == 0 {
            format!("{expected_stem}.invalid-output.json")
        } else {
            format!("{expected_stem}.envelope-{envelope_retry}.invalid-output.json")
        };
        let invalid_output_path = store.output_path(&run.id, &invalid_output_name);
        let raw_payload_text = raw_payload
            .as_ref()
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
            .or_else(|| {
                (!transcript.assistant_tail.trim().is_empty())
                    .then(|| transcript.assistant_tail.clone())
            })
            .map(|text| bounded_text(&text, 20_000))
            .unwrap_or_default();
        let diagnostic = json!({
            "run_id": run.id,
            "slice_id": slice.id,
            "attempt": attempt,
            "envelope_retry": envelope_retry,
            "phase": "invalid_worker_output",
            "parse_error": bounded_text(error, 20_000),
            "expected_output_path": expected_output_path.to_string_lossy(),
            "worktree_path": worker_worktree.to_string_lossy(),
            "worktree_status": git_output_or_empty(worker_worktree, &["status", "--porcelain"]),
            "worktree_diff_tail": bounded_text(&git_output_or_empty(worker_worktree, &["diff"]), 20_000),
            "committed_diff_name_only": git_output_or_empty(worker_worktree, &["diff", "--name-only", &self.current_slice_base_for_artifact(run, slice), "HEAD"]),
            "raw_invalid_payload": raw_payload_text,
            "stdout_tail": transcript.stdout_tail,
            "stderr_tail": transcript.stderr_tail,
            "assistant_tail": transcript.assistant_tail,
            "progress": progress,
            "created_at": Utc::now(),
        });
        artifact::write_json(&invalid_output_path, &diagnostic)?;
        self.state.record_event(
            &run.id,
            "invalid_worker_output",
            &json!({
                "slice_id": slice.id,
                "attempt": attempt,
                "envelope_retry": envelope_retry,
                "parse_error": bounded_text(error, 4_000),
                "artifact_path": invalid_output_path,
                "expected_output_path": expected_output_path,
                "raw_invalid_payload": bounded_text(&raw_payload_text, 4_000),
                "stdout_tail": bounded_text(&transcript.stdout_tail, 4_000),
                "stderr_tail": bounded_text(&transcript.stderr_tail, 4_000),
                "assistant_tail": bounded_text(&transcript.assistant_tail, 4_000),
            }),
        )?;
        self.record_worker_attempt_failure(WorkerAttemptFailureRecord {
            run,
            slice,
            attempt,
            envelope_retry,
            phase: "invalid_worker_output",
            failure_kind: "invalid_worker_output",
            summary: error,
            evidence_path: &invalid_output_path,
            retry_disposition,
            repair_disposition,
            primary_failure: Some(error),
            secondary_failures: &[],
        })?;
        Ok(invalid_output_path)
    }

    #[allow(clippy::too_many_arguments)]
    fn update_invalid_worker_attempt_status(
        &self,
        run: &Run,
        slice: &Slice,
        worker_branch: &str,
        worker_worktree: &Path,
        attempt: usize,
        primary_failure: Option<&str>,
        last_failure: &str,
        secondary_failures: &[String],
    ) -> Result<()> {
        let retrying = attempt < MAX_WORKER_ATTEMPTS;
        let message = if retrying {
            last_failure.to_string()
        } else {
            final_attempt_failure_message(
                &slice.id,
                primary_failure,
                last_failure,
                secondary_failures,
            )
        };
        self.state.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: slice.id.clone(),
            status: if retrying {
                SliceStatus::RepairNeeded
            } else {
                SliceStatus::Failed
            },
            branch: worker_branch.to_string(),
            commit_sha: gitutil::head_sha(worker_worktree).unwrap_or_default(),
            attempts: attempt,
            last_error: message.clone(),
        })?;
        if retrying { Ok(()) } else { bail!(message) }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_worker_attempt_failure_artifact(
        &self,
        run: &Run,
        slice: &Slice,
        attempt: usize,
        phase: &str,
        error: &str,
        worker_worktree: &Path,
        output_path: &Path,
        source: Option<&(dyn Error + Send + Sync + 'static)>,
    ) -> Result<()> {
        let store = artifact::Store::new(&run.repo_path);
        store.ensure_run_dirs(&run.id)?;
        let transcript = source
            .and_then(|err| err.downcast_ref::<RunnerError>())
            .map(|err| err.transcript().clone())
            .unwrap_or_default();
        let progress = self.state.get_progress(&run.id)?;
        let cancel_reason = latest_cancel_reason(&self.state.get_events(&run.id, 200)?);
        let diagnostic = json!({
            "run_id": run.id,
            "slice_id": slice.id,
            "attempt": attempt,
            "phase": phase,
            "error": error,
            "cancel_reason": cancel_reason,
            "output_path": output_path.to_string_lossy(),
            "worktree_path": worker_worktree.to_string_lossy(),
            "worktree_status": git_output_or_empty(worker_worktree, &["status", "--porcelain"]),
            "worktree_diff_tail": bounded_text(&git_output_or_empty(worker_worktree, &["diff"]), 20_000),
            "committed_diff_name_only": git_output_or_empty(worker_worktree, &["diff", "--name-only", &self.current_slice_base_for_artifact(run, slice), "HEAD"]),
            "stdout_tail": transcript.stdout_tail,
            "stderr_tail": transcript.stderr_tail,
            "assistant_tail": transcript.assistant_tail,
            "progress": progress,
            "created_at": Utc::now(),
        });
        let output_stem = output_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("worker-output");
        artifact::write_json(
            store.output_path(&run.id, &format!("{output_stem}.failure.json")),
            &diagnostic,
        )?;
        Ok(())
    }

    fn current_slice_base_for_artifact(&self, run: &Run, _slice: &Slice) -> String {
        // Best-effort only: attempt artifacts are diagnostic and must not fail the workflow.
        gitutil::run(&run.repo_path, &["rev-parse", &run.integration_branch])
            .unwrap_or_else(|_| run.base_sha.clone())
    }

    fn run_worker_attempt_with_envelope(
        &self,
        request: WorkerAttemptRunRequest<'_>,
    ) -> Result<WorkerAttemptRunResult> {
        let run = request.run;
        let slice = request.slice;
        let attempt = request.attempt;
        let result = match self.run_recorded_slice_worker_job(
            request.runner.clone(),
            Job {
                kind: "slice-worker".to_string(),
                prompt: request.prompt.clone(),
                cwd: request.worker_worktree.to_path_buf(),
                json_schema: WORKER_RESULT_SCHEMA.to_string(),
                env: worker_job_env(
                    &self.paths,
                    run,
                    &slice.id,
                    attempt,
                    Some(request.launch_id),
                    Some(request.launch_stem),
                    request.worker_token,
                ),
                termination_grace_seconds: request.config.worker_termination_grace_seconds,
            },
            request.cancel,
            WorkerAttemptContext::new(
                &run.id,
                "worker_running",
                &slice.id,
                attempt,
                Some(request.launch_id),
                Some(request.launch_stem),
                request.config,
                request.native_pi_tui_worker,
            ),
            request.economics,
            AgentCallContext {
                phase: "slice_worker",
                slice_id: &slice.id,
                attempt,
                launch_id: Some(request.launch_id),
                launch_stem: Some(request.launch_stem),
            },
            run,
            request.cockpit_mode,
            request.output_path,
        ) {
            Ok(result) => result,
            Err(err) => {
                let launch_failure =
                    self.classify_runner_launch_failure(err.as_ref(), request.runner_metadata);
                *request.last_failure = launch_failure
                    .as_ref()
                    .map(|failure| failure.summary.clone())
                    .unwrap_or_else(|| err.to_string());
                remember_attempt_failure(
                    request.primary_failure,
                    request.secondary_failures,
                    request.last_failure,
                );
                if invalid_worker_output_error(request.last_failure) {
                    let transcript = err
                        .downcast_ref::<RunnerError>()
                        .map(|err| err.transcript().clone())
                        .unwrap_or_default();
                    let invalid_artifact = self.record_invalid_worker_output_attempt(
                        run,
                        slice,
                        attempt,
                        0,
                        request.last_failure,
                        request.worker_worktree,
                        request.output_path,
                        "envelope_retry_pending",
                        "none",
                        None,
                        transcript,
                    )?;
                    return self.finish_invalid_worker_output_or_reemit(request, &invalid_artifact);
                }
                self.write_worker_attempt_failure_artifact(
                    run,
                    slice,
                    attempt,
                    "worker_error",
                    request.last_failure,
                    request.worker_worktree,
                    request.output_path,
                    Some(err.as_ref()),
                )?;
                if request.cancel.is_cancelled() {
                    return Err(CancelledError::new("run cancelled").into());
                }
                self.state.record_event(
                    &run.id,
                    "worker_error",
                    &workflow_events::WorkerErrorPayload {
                        slice_id: slice.id.clone(),
                        attempt,
                        error: request.last_failure.clone(),
                        primary_failure: (*request.primary_failure).clone(),
                        secondary_failures: (*request.secondary_failures).clone(),
                        failure_kind: launch_failure
                            .as_ref()
                            .map(|failure| failure.failure_kind.clone())
                            .unwrap_or_default(),
                        retryable: launch_failure.as_ref().map(|failure| failure.retryable),
                        operator_action_required: launch_failure
                            .as_ref()
                            .map(|failure| failure.operator_action_required),
                    },
                )?;
                if let Some(launch_failure) = launch_failure {
                    self.record_agent_launch_incident(
                        AgentLaunchIncidentContext {
                            run,
                            phase: "worker_running",
                            slice_id: &slice.id,
                            attempt,
                            runner_name: request.runner.name(),
                            metadata: request.runner_metadata,
                        },
                        &launch_failure,
                    )?;
                    self.state.upsert_slice_run(&SliceRun {
                        run_id: run.id.clone(),
                        slice_id: slice.id.clone(),
                        status: SliceStatus::Blocked,
                        branch: request.worker_branch.to_string(),
                        commit_sha: gitutil::head_sha(request.worker_worktree).unwrap_or_default(),
                        attempts: attempt,
                        last_error: launch_failure.summary.clone(),
                    })?;
                    return Err(BlockedError::new(launch_failure.summary).into());
                }
                return Ok(WorkerAttemptRunResult::Continue);
            }
        };

        let Some(output) = result.output else {
            *request.last_failure = "worker returned no JSON output".to_string();
            remember_attempt_failure(
                request.primary_failure,
                request.secondary_failures,
                request.last_failure,
            );
            let invalid_artifact = self.record_invalid_worker_output_attempt(
                run,
                slice,
                attempt,
                0,
                request.last_failure,
                request.worker_worktree,
                request.output_path,
                "envelope_retry_pending",
                "none",
                None,
                RunnerTranscript::default(),
            )?;
            return self.finish_invalid_worker_output_or_reemit(request, &invalid_artifact);
        };
        let worker_result: WorkerResult = match serde_json::from_value(output.clone()) {
            Ok(value) => value,
            Err(err) => {
                *request.last_failure = format!("worker JSON did not match result model: {err}");
                remember_attempt_failure(
                    request.primary_failure,
                    request.secondary_failures,
                    request.last_failure,
                );
                let invalid_artifact = self.record_invalid_worker_output_attempt(
                    run,
                    slice,
                    attempt,
                    0,
                    request.last_failure,
                    request.worker_worktree,
                    request.output_path,
                    "envelope_retry_pending",
                    "none",
                    Some(output),
                    RunnerTranscript::default(),
                )?;
                return self.finish_invalid_worker_output_or_reemit(request, &invalid_artifact);
            }
        };
        if let Err(err) = validate_worker_result(&worker_result, slice) {
            *request.last_failure = format!("worker JSON failed validation: {err}");
            remember_attempt_failure(
                request.primary_failure,
                request.secondary_failures,
                request.last_failure,
            );
            let invalid_artifact = self.record_invalid_worker_output_attempt(
                run,
                slice,
                attempt,
                0,
                request.last_failure,
                request.worker_worktree,
                request.output_path,
                "envelope_retry_pending",
                "none",
                Some(serde_json::to_value(&worker_result).unwrap_or_default()),
                RunnerTranscript::default(),
            )?;
            return self.finish_invalid_worker_output_or_reemit(request, &invalid_artifact);
        }
        Ok(WorkerAttemptRunResult::Valid(Box::new(
            ValidWorkerAttempt {
                result: worker_result,
                launch_id: request.launch_id,
                launch_stem: request.launch_stem.to_string(),
                branch: request.worker_branch.to_string(),
                worktree: request.worker_worktree.to_path_buf(),
                output_path: request.output_path.to_path_buf(),
                _terminal_guard: None,
            },
        )))
    }

    fn run_targeted_slice_repair(
        &self,
        request: TargetedSliceRepairRequest<'_>,
    ) -> Result<Option<SliceWorkerOutcome>> {
        if !check_failure_allows_targeted_slice_repair(request.check) {
            return Ok(None);
        }
        for repair_attempt in 1..=DEFAULT_SLICE_REPAIR_ATTEMPTS {
            check_cancelled(request.cancel)?;
            let store = artifact::Store::new(&request.run.repo_path);
            let repair_base = gitutil::head_sha(request.worker_worktree)?;
            let launch = self.prepare_followup_worker_launch(
                request.run,
                request.slice,
                request.attempt,
                repair_attempt,
                0,
                "slice-repair",
                "slice-repair",
                &repair_base,
                request.handoff,
            )?;
            let _terminal_guard =
                WorkerAttemptTerminalGuard::new(&self.state, launch.ledger.launch_id);
            let repair_worktree = PathBuf::from(&launch.ledger.worktree);
            if let Err(err) = self.run_worktree_setup(
                request.run,
                &request.slice.id,
                request.attempt,
                Some(&launch.ledger.output_stem),
                &repair_worktree,
                request.config,
                request.economics.clone(),
                request.verification_cache.clone(),
                request.cancel,
            ) {
                let _ = self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &err.to_string(),
                );
                return Err(err);
            }
            self.mark_progress(
                &request.run.id,
                "slice_repair",
                &request.slice.id,
                request.attempt,
                request.runner.name(),
                "targeted in-scope slice repair is running",
            );
            let prompt = slice_repair_prompt(
                &launch.handoff_path.to_string_lossy(),
                &launch.handoff,
                &request.check.summary,
                &request.check_path.to_string_lossy(),
            );
            self.state
                .mark_worker_attempt_launched(launch.ledger.launch_id)?;
            let agent_result = self.run_recorded_slice_worker_job(
                request.runner.clone(),
                Job {
                    kind: "slice-repair".to_string(),
                    prompt,
                    cwd: repair_worktree.clone(),
                    json_schema: WORKER_RESULT_SCHEMA.to_string(),
                    env: worker_job_env(
                        &self.paths,
                        request.run,
                        &request.slice.id,
                        request.attempt,
                        Some(launch.ledger.launch_id),
                        Some(&launch.ledger.output_stem),
                        &launch.token,
                    ),
                    termination_grace_seconds: request.config.worker_termination_grace_seconds,
                },
                request.cancel,
                WorkerAttemptContext::new(
                    &request.run.id,
                    "slice_repair",
                    &request.slice.id,
                    request.attempt,
                    Some(launch.ledger.launch_id),
                    Some(&launch.ledger.output_stem),
                    request.config,
                    request.native_pi_tui_worker,
                ),
                request.economics,
                AgentCallContext {
                    phase: "slice_repair",
                    slice_id: &request.slice.id,
                    attempt: request.attempt,
                    launch_id: Some(launch.ledger.launch_id),
                    launch_stem: Some(&launch.ledger.output_stem),
                },
                request.run,
                request.cockpit_mode,
                &launch.output_path,
            );
            let agent_result = match agent_result {
                Ok(result) => result,
                Err(err) => {
                    let state = if request.cancel.is_cancelled() {
                        "interrupted"
                    } else {
                        "failed"
                    };
                    let _ = self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        state,
                        &err.to_string(),
                    );
                    return Err(err);
                }
            };
            let Some(output) = agent_result.output else {
                self.record_invalid_worker_output_attempt(
                    request.run,
                    request.slice,
                    request.attempt,
                    repair_attempt,
                    "slice repair returned no JSON output",
                    &repair_worktree,
                    &launch.output_path,
                    "slice_repair_exhausted",
                    "slice_repair_failed",
                    None,
                    RunnerTranscript::default(),
                )?;
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    "slice repair returned no JSON output",
                )?;
                return Ok(None);
            };
            let mut worker_result: WorkerResult = match serde_json::from_value(output.clone()) {
                Ok(value) => value,
                Err(err) => {
                    self.record_invalid_worker_output_attempt(
                        request.run,
                        request.slice,
                        request.attempt,
                        repair_attempt,
                        &format!("slice repair JSON did not match result model: {err}"),
                        &repair_worktree,
                        &launch.output_path,
                        "slice_repair_exhausted",
                        "slice_repair_failed",
                        Some(output),
                        RunnerTranscript::default(),
                    )?;
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        "failed",
                        &format!("slice repair JSON did not match result model: {err}"),
                    )?;
                    return Ok(None);
                }
            };
            if let Err(err) = validate_worker_result(&worker_result, request.slice) {
                self.record_invalid_worker_output_attempt(
                    request.run,
                    request.slice,
                    request.attempt,
                    repair_attempt,
                    &format!("slice repair JSON failed validation: {err}"),
                    &repair_worktree,
                    &launch.output_path,
                    "slice_repair_exhausted",
                    "slice_repair_failed",
                    Some(serde_json::to_value(&worker_result).unwrap_or_default()),
                    RunnerTranscript::default(),
                )?;
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &format!("slice repair JSON failed validation: {err}"),
                )?;
                return Ok(None);
            }
            self.create_worker_candidate_followup_slice_proposals(
                request.run,
                request.slice,
                request.attempt,
                &launch.output_path,
                &mut worker_result,
            )?;
            self.create_worker_finding_replan_proposals(
                request.run,
                request.slice,
                request.attempt,
                &launch.output_path,
                &mut worker_result,
            )?;
            artifact::write_json(&launch.output_path, &worker_result)?;
            if worker_result.status == "blocked" {
                self.state.update_slice_status(
                    &request.run.id,
                    &request.slice.id,
                    SliceStatus::Blocked,
                    &worker_result.summary,
                )?;
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &worker_result.summary,
                )?;
                return Err(BlockedError::new(format!(
                    "slice repair reported blocked: {}",
                    worker_result.summary
                ))
                .into());
            }
            if worker_result.status == "failed" {
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &worker_result.summary,
                )?;
                return Ok(None);
            }
            self.run_worktree_setup(
                request.run,
                &request.slice.id,
                request.attempt,
                Some(&launch.ledger.output_stem),
                &repair_worktree,
                request.config,
                request.economics.clone(),
                request.verification_cache.clone(),
                request.cancel,
            )?;
            let repair_check = self.lightweight_check(
                LightweightCheckContext {
                    run_id: &request.run.id,
                    slice: request.slice,
                    worker_worktree: &repair_worktree,
                    base_sha: request.slice_base_sha,
                    attempt: request.attempt,
                    config: request.config,
                    economics: request.economics.clone(),
                    verification_cache: request.verification_cache.clone(),
                },
                request.cancel,
            )?;
            let repair_check_path = store.output_path(
                &request.run.id,
                &format!("{}.check.json", launch.ledger.output_stem),
            );
            artifact::write_json(&repair_check_path, &repair_check)?;
            request.all_checks.push(repair_check.clone());
            if repair_check.status == "passed" && worker_result.status == "complete" {
                if worker_result.commit_sha.is_empty() {
                    worker_result.commit_sha = repair_check.worker_head.clone();
                }
                self.state.upsert_slice_run(&SliceRun {
                    run_id: request.run.id.clone(),
                    slice_id: request.slice.id.clone(),
                    status: SliceStatus::ReadyToMerge,
                    branch: launch.ledger.branch.clone(),
                    commit_sha: worker_result.commit_sha.clone(),
                    attempts: request.attempt,
                    last_error: String::new(),
                })?;
                self.state.record_event(
                    &request.run.id,
                    "slice_repair_completed",
                    &json!({
                        "slice_id": request.slice.id,
                        "attempt": request.attempt,
                        "repair_attempt": repair_attempt,
                        "status": "fixed",
                        "trigger_failure_kind": request.check.failure_kind,
                        "launch_id": launch.ledger.launch_id,
                        "check_path": repair_check_path,
                        "output_path": launch.output_path,
                    }),
                )?;
                self.state
                    .finish_worker_attempt(launch.ledger.launch_id, "succeeded", "")?;
                return Ok(Some(SliceWorkerOutcome {
                    slice: request.slice.clone(),
                    result: worker_result,
                    checks: request.all_checks.clone(),
                    branch: launch.ledger.branch.clone(),
                    attempts: request.attempt,
                }));
            }
            self.record_worker_attempt_failure(WorkerAttemptFailureRecord {
                run: request.run,
                slice: request.slice,
                attempt: request.attempt,
                envelope_retry: 0,
                phase: "slice_repair_verify",
                failure_kind: worker_attempt_failure_kind(&repair_check, &worker_result),
                summary: &repair_check.summary,
                evidence_path: &repair_check_path,
                retry_disposition: "terminal",
                repair_disposition: "slice_repair_exhausted",
                primary_failure: Some(&request.check.summary),
                secondary_failures: &[],
            })?;
            self.state.finish_worker_attempt(
                launch.ledger.launch_id,
                "failed",
                &repair_check.summary,
            )?;
        }
        Ok(None)
    }

    fn finish_invalid_worker_output_or_reemit(
        &self,
        request: WorkerAttemptRunRequest<'_>,
        invalid_artifact: &Path,
    ) -> Result<WorkerAttemptRunResult> {
        self.state
            .finish_worker_attempt(request.launch_id, "failed", request.last_failure)?;
        if let Some(worker_attempt) = self.retry_worker_envelope_reemission(
            request.run,
            request.slice,
            request.attempt,
            request.runner.clone(),
            request.handoff,
            request.worker_worktree,
            request.config,
            request.economics,
            request.cancel,
            request.cockpit_mode,
            request.native_pi_tui_worker,
            request.last_failure,
            invalid_artifact,
            request.primary_failure,
            request.secondary_failures,
        )? {
            return Ok(WorkerAttemptRunResult::Valid(Box::new(worker_attempt)));
        }
        self.update_invalid_worker_attempt_status(
            request.run,
            request.slice,
            request.worker_branch,
            request.worker_worktree,
            request.attempt,
            request.primary_failure.as_deref(),
            request.last_failure,
            request.secondary_failures,
        )?;
        Ok(WorkerAttemptRunResult::Continue)
    }

    #[allow(clippy::too_many_arguments)]
    fn retry_worker_envelope_reemission(
        &self,
        run: &Run,
        slice: &Slice,
        attempt: usize,
        runner: Arc<dyn Runner>,
        handoff: &Handoff,
        worker_worktree: &Path,
        config: &WorkflowConfig,
        economics: &RunEconomicsRecorder,
        cancel: &CancellationToken,
        cockpit_mode: CockpitMode,
        native_pi_tui_worker: bool,
        initial_failure: &str,
        initial_invalid_artifact: &Path,
        primary_failure: &mut Option<String>,
        secondary_failures: &mut Vec<String>,
    ) -> Result<Option<ValidWorkerAttempt>> {
        let mut last_failure = initial_failure.to_string();
        let mut last_artifact = initial_invalid_artifact.to_path_buf();
        let mut retry_base = gitutil::head_sha(worker_worktree)?;
        for envelope_retry in 1..=DEFAULT_WORKER_ENVELOPE_RETRY_ATTEMPTS {
            check_cancelled(cancel)?;
            let launch = self.prepare_followup_worker_launch(
                run,
                slice,
                attempt,
                0,
                envelope_retry,
                "slice-envelope-retry",
                "slice-envelope-retry",
                &retry_base,
                handoff,
            )?;
            let terminal_guard =
                WorkerAttemptTerminalGuard::new(&self.state, launch.ledger.launch_id);
            let retry_worktree = PathBuf::from(&launch.ledger.worktree);
            self.mark_progress(
                &run.id,
                "worker_envelope_retry",
                &slice.id,
                attempt,
                runner.name(),
                "worker is re-emitting a valid JSON envelope for existing evidence",
            );
            let prompt = worker_envelope_retry_prompt(
                &launch.handoff_path.to_string_lossy(),
                &launch.handoff,
                &last_failure,
                &last_artifact.to_string_lossy(),
            );
            self.state
                .mark_worker_attempt_launched(launch.ledger.launch_id)?;
            let result = self.run_recorded_slice_worker_job(
                runner.clone(),
                Job {
                    kind: "slice-envelope-retry".to_string(),
                    prompt,
                    cwd: retry_worktree.clone(),
                    json_schema: WORKER_RESULT_SCHEMA.to_string(),
                    env: worker_job_env(
                        &self.paths,
                        run,
                        &slice.id,
                        attempt,
                        Some(launch.ledger.launch_id),
                        Some(&launch.ledger.output_stem),
                        &launch.token,
                    ),
                    termination_grace_seconds: config.worker_termination_grace_seconds,
                },
                cancel,
                WorkerAttemptContext::new(
                    &run.id,
                    "worker_envelope_retry",
                    &slice.id,
                    attempt,
                    Some(launch.ledger.launch_id),
                    Some(&launch.ledger.output_stem),
                    config,
                    native_pi_tui_worker,
                ),
                economics,
                AgentCallContext {
                    phase: "slice_worker_envelope_retry",
                    slice_id: &slice.id,
                    attempt,
                    launch_id: Some(launch.ledger.launch_id),
                    launch_stem: Some(&launch.ledger.output_stem),
                },
                run,
                cockpit_mode,
                &launch.output_path,
            );
            let retry_disposition = if envelope_retry == DEFAULT_WORKER_ENVELOPE_RETRY_ATTEMPTS {
                "envelope_retry_exhausted"
            } else {
                "envelope_retry_pending"
            };
            retry_base = gitutil::head_sha(&retry_worktree)?;
            let result = match result {
                Ok(result) => result,
                Err(err) => {
                    last_failure = err.to_string();
                    remember_attempt_failure(primary_failure, secondary_failures, &last_failure);
                    let transcript = err
                        .downcast_ref::<RunnerError>()
                        .map(|err| err.transcript().clone())
                        .unwrap_or_default();
                    let state = if cancel.is_cancelled() {
                        "interrupted"
                    } else {
                        "failed"
                    };
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        state,
                        &last_failure,
                    )?;
                    last_artifact = self.record_invalid_worker_output_attempt(
                        run,
                        slice,
                        attempt,
                        envelope_retry,
                        &last_failure,
                        &retry_worktree,
                        &launch.output_path,
                        retry_disposition,
                        "none",
                        None,
                        transcript,
                    )?;
                    continue;
                }
            };
            let Some(output) = result.output else {
                last_failure = "worker envelope retry returned no JSON output".to_string();
                remember_attempt_failure(primary_failure, secondary_failures, &last_failure);
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &last_failure,
                )?;
                last_artifact = self.record_invalid_worker_output_attempt(
                    run,
                    slice,
                    attempt,
                    envelope_retry,
                    &last_failure,
                    &retry_worktree,
                    &launch.output_path,
                    retry_disposition,
                    "none",
                    None,
                    RunnerTranscript::default(),
                )?;
                continue;
            };
            let worker_result: WorkerResult = match serde_json::from_value(output.clone()) {
                Ok(value) => value,
                Err(err) => {
                    last_failure =
                        format!("worker envelope JSON did not match result model: {err}");
                    remember_attempt_failure(primary_failure, secondary_failures, &last_failure);
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        "failed",
                        &last_failure,
                    )?;
                    last_artifact = self.record_invalid_worker_output_attempt(
                        run,
                        slice,
                        attempt,
                        envelope_retry,
                        &last_failure,
                        &retry_worktree,
                        &launch.output_path,
                        retry_disposition,
                        "none",
                        Some(output),
                        RunnerTranscript::default(),
                    )?;
                    continue;
                }
            };
            if let Err(err) = validate_worker_result(&worker_result, slice) {
                last_failure = format!("worker envelope JSON failed validation: {err}");
                remember_attempt_failure(primary_failure, secondary_failures, &last_failure);
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &last_failure,
                )?;
                last_artifact = self.record_invalid_worker_output_attempt(
                    run,
                    slice,
                    attempt,
                    envelope_retry,
                    &last_failure,
                    &retry_worktree,
                    &launch.output_path,
                    retry_disposition,
                    "none",
                    Some(serde_json::to_value(&worker_result).unwrap_or_default()),
                    RunnerTranscript::default(),
                )?;
                continue;
            }
            artifact::write_json(&launch.output_path, &worker_result)?;
            self.state.record_event(
                &run.id,
                workflow_events::WORKER_ENVELOPE_RETRY_SUCCEEDED,
                &json!({
                    "slice_id": slice.id,
                    "attempt": attempt,
                    "launch_id": launch.ledger.launch_id,
                    "envelope_retry": envelope_retry,
                    "output_path": launch.output_path,
                    "previous_invalid_output": last_artifact,
                    "disposition": "valid_envelope_reemitted_existing_worker_head",
                }),
            )?;
            return Ok(Some(ValidWorkerAttempt {
                result: worker_result,
                launch_id: launch.ledger.launch_id,
                launch_stem: launch.ledger.output_stem,
                branch: launch.ledger.branch,
                worktree: retry_worktree,
                output_path: launch.output_path,
                _terminal_guard: Some(terminal_guard),
            }));
        }
        Ok(None)
    }

    fn integration_repair(&self, context: IntegrationRepairContext<'_>) -> Result<RepairResult> {
        let run = context.run;
        let slices = context.slices;
        let integration_worktree = context.integration_worktree;
        let cancel = context.cancel;
        let runner = context.runner;
        let config = context.config;
        let economics = context.economics;
        let check_summary =
            serde_json::to_string_pretty(context.checks).unwrap_or_else(|_| "[]".to_string());
        let gate_summary =
            serde_json::to_string_pretty(context.gate_failure).unwrap_or_else(|_| "{}".to_string());
        let persisted_repair_launches = self
            .state
            .list_worker_attempt_ledger(&run.id, INTEGRATION_REPAIR_SCOPE_ID)?
            .into_iter()
            .filter(|launch| launch.kind == "integration-repair")
            .collect::<Vec<_>>();
        let latest_repair_launch = persisted_repair_launches
            .iter()
            .max_by_key(|launch| (launch.repair_ordinal, launch.launch_id));
        let consumed_repair_budget = latest_repair_launch
            .map(|launch| launch.repair_ordinal)
            .unwrap_or_default();
        economics.set_repair_attempts(consumed_repair_budget);
        if let Some(launch) = latest_repair_launch.filter(|launch| launch.state == "succeeded") {
            let output_path = artifact::Store::new(&run.repo_path)
                .output_path(&run.id, &format!("{}.json", launch.output_stem));
            let result: RepairResult = artifact::read_json(&output_path).with_context(|| {
                format!(
                    "read persisted successful integration repair launch {} from {}",
                    launch.launch_id,
                    output_path.display()
                )
            })?;
            validate_repair_result(&result).with_context(|| {
                format!(
                    "validate persisted successful integration repair launch {}",
                    launch.launch_id
                )
            })?;
            return Ok(result);
        }
        if consumed_repair_budget >= DEFAULT_REPAIR_ATTEMPTS {
            bail!(
                "integration repair budget exhausted for run {} ({consumed_repair_budget}/{DEFAULT_REPAIR_ATTEMPTS})",
                run.id
            );
        }
        let integration_repair_base = gitutil::head_sha(integration_worktree)?;
        let mut next_repair_base = integration_repair_base.clone();
        let mut last_error = String::new();
        for attempt in consumed_repair_budget + 1..=DEFAULT_REPAIR_ATTEMPTS {
            economics.set_repair_attempts(attempt);
            check_cancelled(cancel)?;
            let launch = self.prepare_integration_repair_launch(run, attempt, &next_repair_base)?;
            let _terminal_guard =
                WorkerAttemptTerminalGuard::new(&self.state, launch.ledger.launch_id);
            let repair_worktree = PathBuf::from(&launch.ledger.worktree);
            if let Err(err) = self.run_worktree_setup(
                run,
                INTEGRATION_REPAIR_SCOPE_ID,
                attempt,
                Some(&launch.ledger.output_stem),
                &repair_worktree,
                config,
                economics.clone(),
                context.verification_cache.clone(),
                cancel,
            ) {
                let state = if cancel.is_cancelled() {
                    "interrupted"
                } else {
                    "failed"
                };
                let _ = self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    state,
                    &err.to_string(),
                );
                return Err(err);
            }
            self.mark_progress(
                &run.id,
                "integration_repair",
                INTEGRATION_REPAIR_SCOPE_ID,
                attempt,
                runner.name(),
                "integration repair worker is running",
            );
            let runner_metadata = runner.metadata();
            let prompt = integration_repair_prompt(
                &run.id,
                &repair_worktree.to_string_lossy(),
                slices,
                &check_summary,
                &gate_summary,
                context.trigger,
            );
            self.state
                .mark_worker_attempt_launched(launch.ledger.launch_id)?;
            let agent_result = match self.run_recorded_agent_job(
                runner.clone(),
                Job {
                    kind: "integration-repair".to_string(),
                    prompt,
                    cwd: repair_worktree.clone(),
                    json_schema: REPAIR_RESULT_SCHEMA.to_string(),
                    env: worker_job_env(
                        &self.paths,
                        run,
                        INTEGRATION_REPAIR_SCOPE_ID,
                        attempt,
                        Some(launch.ledger.launch_id),
                        Some(&launch.ledger.output_stem),
                        &launch.token,
                    ),
                    termination_grace_seconds: config.worker_termination_grace_seconds,
                },
                cancel,
                WorkerAttemptContext::new(
                    &run.id,
                    "integration_repair",
                    INTEGRATION_REPAIR_SCOPE_ID,
                    attempt,
                    Some(launch.ledger.launch_id),
                    Some(&launch.ledger.output_stem),
                    config,
                    false,
                ),
                &economics,
                AgentCallContext {
                    phase: "integration_repair",
                    slice_id: INTEGRATION_REPAIR_SCOPE_ID,
                    attempt,
                    launch_id: Some(launch.ledger.launch_id),
                    launch_stem: Some(&launch.ledger.output_stem),
                },
            ) {
                Ok(result) => result,
                Err(err) => {
                    next_repair_base =
                        gitutil::head_sha(&repair_worktree).unwrap_or(next_repair_base);
                    let state = if cancel.is_cancelled() {
                        "interrupted"
                    } else {
                        "failed"
                    };
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        state,
                        &err.to_string(),
                    )?;
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    if let Some(launch_failure) =
                        self.classify_runner_launch_failure(err.as_ref(), &runner_metadata)
                    {
                        self.record_agent_launch_incident(
                            AgentLaunchIncidentContext {
                                run,
                                phase: "integration_repair",
                                slice_id: INTEGRATION_REPAIR_SCOPE_ID,
                                attempt,
                                runner_name: runner.name(),
                                metadata: &runner_metadata,
                            },
                            &launch_failure,
                        )?;
                        return Err(BlockedError::new(launch_failure.summary).into());
                    }
                    last_error = err.to_string();
                    continue;
                }
            };
            next_repair_base = gitutil::head_sha(&repair_worktree)?;
            let Some(output) = agent_result.output else {
                last_error = "integration repair returned no JSON output".to_string();
                self.state
                    .finish_worker_attempt(launch.ledger.launch_id, "failed", &last_error)?;
                continue;
            };
            let mut result: RepairResult = match serde_json::from_value(output) {
                Ok(value) => value,
                Err(err) => {
                    last_error = err.to_string();
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        "failed",
                        &last_error,
                    )?;
                    continue;
                }
            };
            if let Err(err) = validate_repair_result(&result) {
                last_error = format!("integration repair JSON failed validation: {err}");
                self.state
                    .finish_worker_attempt(launch.ledger.launch_id, "failed", &last_error)?;
                continue;
            }
            result.trigger = context.trigger.to_string();
            result.attempts = attempt;
            let created_candidate_proposal = self
                .create_repair_candidate_followup_slice_proposals(
                    run,
                    attempt,
                    &launch.output_path,
                    &mut result,
                )?;
            let created_finding_proposal = self.create_repair_finding_replan_proposals(
                run,
                attempt,
                &launch.output_path,
                &mut result,
            )?;
            artifact::write_json(&launch.output_path, &result)?;
            if created_candidate_proposal || created_finding_proposal {
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    "integration repair requires an operator replan decision",
                )?;
                self.block_if_pending_replan(run, "integration repair follow-up proposal")?;
            }
            if result.status == "blocked" {
                self.state.finish_worker_attempt(
                    launch.ledger.launch_id,
                    "failed",
                    &result.summary,
                )?;
                return Err(BlockedError::new(format!(
                    "integration repair blocked: {}",
                    result.summary
                ))
                .into());
            }
            if result.status == "failed" {
                last_error = result.summary.clone();
                self.state
                    .finish_worker_attempt(launch.ledger.launch_id, "failed", &last_error)?;
                continue;
            }
            let status = match gitutil::status_porcelain(&repair_worktree) {
                Ok(status) => status,
                Err(err) => {
                    last_error = err.to_string();
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        "failed",
                        &last_error,
                    )?;
                    continue;
                }
            };
            if !status.trim().is_empty() {
                last_error = "integration repair left uncommitted changes".to_string();
                self.state
                    .finish_worker_attempt(launch.ledger.launch_id, "failed", &last_error)?;
                continue;
            }
            let repair_head = gitutil::head_sha(&repair_worktree)?;
            if result.status == "no-op" && repair_head != integration_repair_base {
                last_error = "integration repair reported no-op after creating commits".to_string();
                self.state
                    .finish_worker_attempt(launch.ledger.launch_id, "failed", &last_error)?;
                continue;
            }
            if result.status == "fixed" {
                if result.commit_sha.is_empty() {
                    result.commit_sha = repair_head.clone();
                }
                let unauthorized = repair_authority_violations(
                    &repair_worktree,
                    &integration_repair_base,
                    &repair_head,
                    slices,
                )?;
                if !unauthorized.is_empty() {
                    let proposal_id = self.create_repair_authority_proposal(
                        run,
                        attempt,
                        &launch.output_path,
                        &integration_repair_base,
                        &repair_head,
                        &unauthorized,
                        &result,
                    )?;
                    result.findings.push(Finding {
                        id: "repair-authority".to_string(),
                        severity: "error".to_string(),
                        action: "ask-user".to_string(),
                        file: unauthorized.first().cloned().unwrap_or_default(),
                        line: 0,
                        description: format!(
                            "integration repair changed out-of-authority paths: {}",
                            unauthorized.join(", ")
                        ),
                    });
                    result.finding_dispositions.push(FindingDisposition {
                        finding_id: "repair-authority".to_string(),
                        finding_index: 0,
                        disposition: "proposed".to_string(),
                        replan_proposal_id: proposal_id.clone(),
                        rationale: format!(
                            "repair revision {repair_head} requires operator approval before applying {}",
                            unauthorized.join(", ")
                        ),
                    });
                    artifact::write_json(&launch.output_path, &result)?;
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        "failed",
                        "integration repair exceeded its authorized areas",
                    )?;
                    self.block_if_pending_replan(run, "integration repair authority proposal")?;
                }
                if let Err(err) = gitutil::merge(
                    integration_worktree,
                    &launch.ledger.branch,
                    &format!(
                        "khazad(integration-repair): merge launch {}",
                        launch.ledger.launch_id
                    ),
                ) {
                    let _ = gitutil::merge_abort(integration_worktree);
                    last_error = format!("integration repair merge failed: {err}");
                    self.state.finish_worker_attempt(
                        launch.ledger.launch_id,
                        "failed",
                        &last_error,
                    )?;
                    return Err(BlockedError::new(last_error).into());
                }
            }
            artifact::write_json(&launch.output_path, &result)?;
            self.state
                .finish_worker_attempt(launch.ledger.launch_id, "succeeded", "")?;
            self.state.record_event(
                &run.id,
                workflow_events::INTEGRATION_REPAIR_COMPLETED,
                &workflow_events::IntegrationRepairCompletedPayload::new(
                    &result.status,
                    &result.summary,
                    launch.ledger.launch_id,
                ),
            )?;
            return Ok(result);
        }
        Err(anyhow!(
            "integration repair failed after {} attempts: {}",
            DEFAULT_REPAIR_ATTEMPTS,
            last_error
        ))
    }

    fn write_merge_conflict_report(
        &self,
        run: &Run,
        slice: &Slice,
        branch: &str,
        integration_worktree: &Path,
        err: &anyhow::Error,
    ) -> Result<MergeConflictReport> {
        let conflicted_files = gitutil::conflicted_files(integration_worktree).unwrap_or_default();
        let summary = if conflicted_files.is_empty() {
            format!("merge blocked for slice {}", slice.id)
        } else {
            format!(
                "merge blocked for slice {} due to conflicts in {}",
                slice.id,
                conflicted_files.join(", ")
            )
        };
        let report = MergeConflictReport {
            run_id: run.id.clone(),
            slice_id: slice.id.clone(),
            branch: branch.to_string(),
            status: "blocked".to_string(),
            summary,
            conflicted_files,
            error: err.to_string(),
        };
        artifact::write_json(
            artifact::Store::new(&run.repo_path)
                .output_path(&run.id, &format!("{}.merge-conflict.json", slice.id)),
            &report,
        )?;
        self.state
            .record_event(&run.id, "slice_merge_conflict", &report)?;
        Ok(report)
    }

    fn prepare_resume_worktrees(&self, run: &Run) -> Result<()> {
        let root = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        if !root.exists() {
            return Ok(());
        }
        let integration = root.join("integration");
        if integration.is_dir()
            && gitutil::has_retained_completion_publication_journal(&integration)?
        {
            let mut errors = Vec::new();
            for path in discover_run_worktrees(&root)? {
                if path != integration
                    && let Err(err) = gitutil::worktree_remove(&run.repo_path, &path)
                {
                    errors.push(format!("{}: {err:#}", path.display()));
                }
            }
            for entry in std::fs::read_dir(&root)? {
                let path = entry?.path();
                if path == integration {
                    continue;
                }
                let removal = if path.is_dir() {
                    std::fs::remove_dir_all(&path).map_err(anyhow::Error::from)
                } else {
                    std::fs::remove_file(&path).map_err(anyhow::Error::from)
                };
                if let Err(err) = removal {
                    errors.push(format!("{}: {err:#}", path.display()));
                }
            }
            let _ = gitutil::worktree_prune(&run.repo_path);
            if !errors.is_empty() {
                bail!(
                    "preserve retained completion publication journal while preparing resume: {}",
                    errors.join("; ")
                );
            }
            self.state.record_event(
                &run.id,
                workflow_events::RUN_INCIDENT,
                &workflow_events::RunIncidentPayload::warning(
                    "publication_recovery_worktree_retained",
                    format!(
                        "retained integration worktree for completion publication recovery: {}",
                        integration.display()
                    ),
                ),
            )?;
            return Ok(());
        }
        std::fs::remove_dir_all(&root).with_context(|| {
            format!(
                "remove stale run worktree dir before resume {}",
                root.display()
            )
        })?;
        let _ = gitutil::worktree_prune(&run.repo_path);
        self.state.record_event(
            &run.id,
            workflow_events::RUN_INCIDENT,
            &workflow_events::RunIncidentPayload::warning(
                "stale_worktree_removed_before_resume",
                format!(
                    "removed stale run worktree directory before resume: {}",
                    root.display()
                ),
            ),
        )?;
        Ok(())
    }

    fn cleanup_run_worktrees(&self, run: &Run) -> Result<()> {
        let root = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        if !root.exists() {
            return Ok(());
        }
        let worktrees = discover_run_worktrees(&root)?;
        for path in &worktrees {
            if gitutil::has_retained_completion_publication_journal(path)? {
                bail!(
                    "retained completion publication recovery journal prevents worktree cleanup: {}",
                    path.display()
                );
            }
        }
        let mut errors = Vec::new();
        for path in worktrees {
            if let Err(err) = gitutil::worktree_remove(&run.repo_path, &path) {
                errors.push(format!("{}: {err}", path.display()));
            }
        }
        // a worker child that has not fully exited can transiently hold
        // files under the root; retry briefly before treating removal as
        // failed, and keep the final error observable instead of dropping it
        for attempt in 0..WORKTREE_REMOVE_ATTEMPTS {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => break,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                Err(err) => {
                    if attempt + 1 == WORKTREE_REMOVE_ATTEMPTS {
                        errors.push(format!("{}: {err}", root.display()));
                    } else {
                        thread::sleep(WORKTREE_REMOVE_RETRY_DELAY);
                    }
                }
            }
        }
        let _ = gitutil::worktree_prune(&run.repo_path);
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("; "))
        }
    }
}

struct SliceWorkerOutcome {
    slice: Slice,
    result: WorkerResult,
    checks: Vec<CheckResult>,
    branch: String,
    attempts: usize,
}

struct ParallelWorkerHandle {
    slice_id: String,
    handle: thread::JoinHandle<()>,
}

struct ParallelWorkerResult {
    slice_id: String,
    result: Result<SliceWorkerOutcome>,
}

struct LightweightCheckContext<'a> {
    run_id: &'a str,
    slice: &'a Slice,
    worker_worktree: &'a Path,
    base_sha: &'a str,
    attempt: usize,
    config: &'a WorkflowConfig,
    economics: RunEconomicsRecorder,
    verification_cache: VerificationCommandCache,
}

fn remember_attempt_failure(
    primary: &mut Option<String>,
    secondary: &mut Vec<String>,
    failure: &str,
) {
    let failure = failure.trim();
    if failure.is_empty() {
        return;
    }
    match primary {
        None => *primary = Some(failure.to_string()),
        Some(existing) if existing == failure => {}
        Some(_) if secondary.iter().any(|entry| entry == failure) => {}
        Some(_) => secondary.push(failure.to_string()),
    }
}

fn final_attempt_failure_message(
    slice_id: &str,
    primary: Option<&str>,
    latest: &str,
    secondary: &[String],
) -> String {
    let primary = primary
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(latest);
    let mut message = format!("slice {slice_id} did not become ready: {primary}");
    let details = secondary
        .iter()
        .filter(|failure| failure.as_str() != primary)
        .cloned()
        .collect::<Vec<_>>();
    if !details.is_empty() {
        message.push_str("; secondary failures: ");
        message.push_str(&details.join("; "));
    }
    message
}

fn check_failure_needs_operator(check: &CheckResult) -> bool {
    failure_kind_needs_operator(&check.failure_kind)
        || check
            .findings
            .iter()
            .any(|finding| finding.action == "operator-fix")
}

fn check_failure_allows_targeted_slice_repair(check: &CheckResult) -> bool {
    check.status == "failed" && check.failure_kind == "command_failed"
}

fn worker_attempt_failure_kind<'a>(check: &'a CheckResult, result: &'a WorkerResult) -> &'a str {
    if !check.failure_kind.trim().is_empty() {
        &check.failure_kind
    } else if result.status == "failed" {
        "worker_reported_failed"
    } else {
        "worker_attempt_failed"
    }
}

fn worker_attempt_retry_disposition(attempt: usize, check: &CheckResult) -> &'static str {
    if check_failure_needs_operator(check) {
        "operator_intervention_required"
    } else if attempt < MAX_WORKER_ATTEMPTS {
        "next_worker_attempt"
    } else {
        "normal_worker_attempts_exhausted"
    }
}

fn worker_attempt_repair_disposition(attempt: usize, check: &CheckResult) -> &'static str {
    if check.failure_kind == "scope_violation" {
        "scope_violation_requires_replan_grant"
    } else if attempt == MAX_WORKER_ATTEMPTS && check_failure_allows_targeted_slice_repair(check) {
        "targeted_slice_repair_pending"
    } else {
        "none"
    }
}

fn gate_needs_operator(gate: &GateResult) -> bool {
    gate.commands
        .iter()
        .any(|command| failure_kind_needs_operator(&command.failure_kind))
        || gate
            .findings
            .iter()
            .any(|finding| finding.action == "operator-fix")
}

fn worktree_setup_blocked_summary(slice_id: &str, attempt: usize, setup: &GateResult) -> String {
    let target = if slice_id.is_empty() {
        "integration worktree".to_string()
    } else if attempt == 0 {
        format!("worker worktree for {slice_id}")
    } else {
        format!("worker worktree for {slice_id} attempt {attempt}")
    };
    let finding = setup
        .findings
        .first()
        .map(|finding| finding.description.as_str())
        .unwrap_or(setup.summary.as_str());
    format!("{target} setup blocked: {finding}")
}

fn latest_cancel_reason(events: &[crate::domain::Event]) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.typ == workflow_events::RUN_CANCEL_REQUESTED)
        .map(|event| workflow_events::RunCancelRequestedPayload::from_value(&event.payload).reason)
        .unwrap_or_default()
}

fn primary_failure_for_terminal_summary(
    message: &str,
    slice_runs: &[SliceRun],
    events: &[crate::domain::Event],
) -> String {
    if !message.trim().is_empty() {
        return message.to_string();
    }
    slice_runs
        .iter()
        .find(|slice_run| !slice_run.last_error.trim().is_empty())
        .map(|slice_run| slice_run.last_error.clone())
        .or_else(|| {
            events
                .iter()
                .rev()
                .find(|event| event.typ == workflow_events::RUN_ERROR)
                .map(|event| workflow_events::RunErrorPayload::from_value(&event.payload).error)
        })
        .unwrap_or_default()
}

fn frontier_auto_accept_gate_allows(
    envelope: &MissionEnvelope,
    classification: &FrontierClassification,
) -> bool {
    // AF-00/AF-06 hard gate: the daemon's delegated authority is active only for
    // promote/run envelopes whose persisted classifier evidence is Tier 1 and still
    // carries budget/depth positives. All other cases fail upward to replan attention.
    matches!(
        envelope.autonomy_level,
        AutonomyLevel::Promote | AutonomyLevel::Run
    ) && classification.tier == "tier_1"
        && classification
            .reason_codes
            .iter()
            .any(|code| code == "within_budget")
        && classification
            .reason_codes
            .iter()
            .any(|code| code == "within_depth")
}

fn frontier_budget_after_auto_accept(before: &FrontierBudgetState) -> FrontierBudgetState {
    let mut after = before.clone();
    after.auto_promotions_used = after.auto_promotions_used.saturating_add(1);
    after.generated_slices = after.generated_slices.saturating_add(1);
    after
}

fn followup_apply_mode_for_autonomy(level: AutonomyLevel) -> FollowupApplyMode {
    match level {
        AutonomyLevel::Promote => FollowupApplyMode::PromoteOnly,
        AutonomyLevel::Run | AutonomyLevel::Shadow | AutonomyLevel::Off => {
            FollowupApplyMode::AppendAndRun
        }
    }
}

fn frontier_replan_attention_reasons(pending: &[ReplanProposal]) -> Vec<String> {
    pending
        .iter()
        .filter_map(|proposal| {
            let classification = proposal.frontier_classification.as_ref()?;
            if classification.tier != "tier_3" && classification.tier != "stop" {
                return None;
            }
            let codes = if classification.reason_codes.is_empty() {
                "no_reason_codes".to_string()
            } else {
                classification.reason_codes.join(",")
            };
            Some(format!(
                "frontier {} for {} ({codes})",
                classification.tier, proposal.id
            ))
        })
        .collect()
}

fn autonomy_effective_note(envelope: Option<&MissionEnvelope>) -> &'static str {
    match envelope.map(|envelope| envelope.autonomy_level) {
        Some(AutonomyLevel::Promote) => {
            "promote may auto-accept Tier-1 follow-up proposals and generate slices; generated slices are not run in the current run"
        }
        Some(AutonomyLevel::Run) => {
            "run may auto-accept Tier-1 follow-up proposals, generate slices, append them serially, and execute them in the current run"
        }
        Some(AutonomyLevel::Shadow) => {
            "shadow records frontier classifications only; queues, slices, and decisions stay operator-owned"
        }
        Some(AutonomyLevel::Off) | None => {
            "frontier authority inactive; legacy run behavior unchanged"
        }
    }
}

fn autonomy_authority_label(level: AutonomyLevel) -> &'static str {
    match level {
        AutonomyLevel::Promote => "frontier_tier1_promote_authority",
        AutonomyLevel::Run => "frontier_tier1_run_authority",
        AutonomyLevel::Shadow => "record_only_no_auto_authority",
        AutonomyLevel::Off => "frontier_disabled",
    }
}

fn validate_mission_envelope(
    envelope: Option<&MissionEnvelope>,
    config: &WorkflowConfig,
) -> Result<()> {
    let Some(envelope) = envelope else {
        return Ok(());
    };
    if envelope.goal.trim().is_empty() {
        bail!("mission envelope goal is required");
    }
    if envelope.allowed_areas.is_empty() {
        bail!("mission envelope allowed_areas must contain at least one area");
    }
    for (index, area) in envelope.allowed_areas.iter().enumerate() {
        artifact::validate_slice_area(area)
            .with_context(|| format!("mission envelope allowed_areas[{index}] is invalid"))?;
    }
    validate_mission_text_list("non_goals", &envelope.non_goals)?;
    validate_mission_text_list("must_ask_if", &envelope.must_ask_if)?;
    let verify_profile = envelope.verify_profile.trim();
    if verify_profile.is_empty() {
        bail!(
            "mission envelope verify_profile is required; use \"default\" or a configured profile"
        );
    }
    if verify_profile != "default" && !config.verify_profiles.contains_key(verify_profile) {
        bail!(
            "mission envelope verify_profile {verify_profile:?} is not configured; expected \"default\" or one of [{}]",
            config
                .verify_profiles
                .keys()
                .map(|key| key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for (field, value) in [
        ("max_auto_promotions", envelope.max_auto_promotions),
        ("max_depth", envelope.max_depth),
        ("max_generated_slices", envelope.max_generated_slices),
    ] {
        if value < 0 {
            bail!("mission envelope {field} must be >= 0");
        }
    }
    Ok(())
}

fn validate_mission_text_list(field: &str, values: &[String]) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        if value.trim().is_empty() {
            bail!("mission envelope {field}[{index}] must be non-empty");
        }
    }
    Ok(())
}

fn selected_verify_profiles(slices: &[Slice]) -> Vec<String> {
    let mut profiles = Vec::new();
    for slice in slices {
        let profile = if slice.verify_profile.trim().is_empty() {
            "default"
        } else {
            slice.verify_profile.trim()
        };
        if !profiles.iter().any(|existing| existing == profile) {
            profiles.push(profile.to_string());
        }
    }
    profiles
}

fn terminal_next_commands(run: &Run, status: RunStatus) -> Vec<String> {
    match status {
        RunStatus::Completed => vec![format!("khazad-doom handoff --run {}", run.id)],
        RunStatus::Failed | RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted => {
            vec![
                format!("khazad-doom inspect --run {}", run.id),
                format!("khazad-doom resume --run {}", run.id),
            ]
        }
        RunStatus::Pending | RunStatus::Running => Vec::new(),
    }
}

fn discover_run_worktrees(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut worktrees = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if path.join(".git").exists() {
                worktrees.push(path);
            } else {
                pending.push(path);
            }
        }
    }
    worktrees.sort();
    Ok(worktrees)
}

fn git_output_or_empty(worktree: &Path, args: &[&str]) -> String {
    gitutil::run(worktree, args).unwrap_or_default()
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) && start < text.len() {
        start += 1;
    }
    text[start..].to_string()
}

fn repair_authority_violations(
    worktree: &Path,
    base_sha: &str,
    head_sha: &str,
    slices: &[Slice],
) -> Result<Vec<String>> {
    if base_sha.trim().is_empty() || head_sha.trim().is_empty() || base_sha == head_sha {
        return Ok(Vec::new());
    }
    let output = gitutil::run(worktree, &["diff", "--name-only", base_sha, head_sha])?;
    let areas = slices
        .iter()
        .flat_map(|slice| slice.areas.iter().cloned())
        .collect::<Vec<_>>();
    let unrestricted = areas.is_empty() || areas.iter().any(|area| area.trim() == ".");
    let violations = output
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .filter(|path| {
            workflow_policy_path(path)
                || (!unrestricted && !areas.iter().any(|area| path_matches_area(path, area)))
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok(violations)
}

fn workflow_policy_path(path: &str) -> bool {
    let path = path.trim().trim_matches('/');
    path == ".workflow"
        || path.starts_with(".workflow/")
        || path == ".agents"
        || path.starts_with(".agents/")
        || path == ".pi"
        || path.starts_with(".pi/")
}

fn changed_files_outside_slice_areas(
    worktree: &Path,
    base_sha: &str,
    head_sha: &str,
    areas: &[String],
) -> Result<Option<Vec<String>>> {
    if areas.is_empty() || areas.iter().any(|area| area.trim() == ".") {
        return Ok(None);
    }
    let output = gitutil::run(worktree, &["diff", "--name-only", base_sha, head_sha])?;
    let outside = output
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .filter(|path| !areas.iter().any(|area| path_matches_area(path, area)))
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok((!outside.is_empty()).then_some(outside))
}

fn path_matches_area(path: &str, area: &str) -> bool {
    let area = area.trim().trim_matches('/');
    if area.is_empty() || area == "." {
        return true;
    }
    let path = path.trim().trim_matches('/');
    path == area || path.starts_with(&format!("{area}/"))
}

fn parallel_layer_success_outcomes(
    results: &BTreeMap<String, Result<SliceWorkerOutcome>>,
) -> Vec<serde_json::Value> {
    results
        .iter()
        .map(|(slice_id, result)| {
            let outcome = result.as_ref().expect("successful parallel worker result");
            json!({
                "slice_id": slice_id,
                "status": "ready_to_merge",
                "attempts": outcome.attempts,
                "summary": &outcome.result.summary,
            })
        })
        .collect()
}

fn parallel_layer_failure_summary(outcomes: &[serde_json::Value]) -> String {
    let non_ready = outcomes
        .iter()
        .filter(|outcome| {
            !matches!(
                outcome.get("status").and_then(serde_json::Value::as_str),
                Some("ready_to_merge")
            )
        })
        .count();
    let details = outcomes
        .iter()
        .map(|outcome| {
            let slice_id = outcome
                .get("slice_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let status = outcome
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let summary = outcome
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let preserved = outcome
                .get("preserved_unmerged")
                .and_then(|preserved| preserved.get("commit_sha"))
                .and_then(serde_json::Value::as_str)
                .filter(|commit| !commit.trim().is_empty())
                .map(|commit| format!(" preserved-unmerged@{commit}"))
                .unwrap_or_default();
            if summary.trim().is_empty() {
                format!("{slice_id}={status}{preserved}")
            } else {
                format!("{slice_id}={status}{preserved} ({summary})")
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "parallel worker layer failed with {non_ready} non-ready slice(s) out of {}: {details}",
        outcomes.len()
    )
}

fn parallel_result_status(result: &Result<SliceWorkerOutcome>) -> &'static str {
    match result {
        Ok(_) => "ready_to_merge",
        Err(err) if err.downcast_ref::<CancelledError>().is_some() => "cancelled",
        Err(err) if err.downcast_ref::<BlockedError>().is_some() => "blocked",
        Err(_) => "failed",
    }
}

fn parallel_result_summary(result: &Result<SliceWorkerOutcome>) -> String {
    match result {
        Ok(outcome) => outcome.result.summary.clone(),
        Err(err) => err.to_string(),
    }
}

fn parallel_result_slice_status(result: &Result<SliceWorkerOutcome>) -> SliceStatus {
    match result {
        Ok(_) => SliceStatus::ReadyToMerge,
        Err(err) if err.downcast_ref::<CancelledError>().is_some() => SliceStatus::Cancelled,
        Err(err) if err.downcast_ref::<BlockedError>().is_some() => SliceStatus::Blocked,
        Err(_) => SliceStatus::Failed,
    }
}

fn parallel_results_any_blocked(results: &BTreeMap<String, Result<SliceWorkerOutcome>>) -> bool {
    results
        .values()
        .any(|result| matches!(result, Err(err) if err.downcast_ref::<BlockedError>().is_some()))
}

fn parallel_results_all_cancelled(results: &BTreeMap<String, Result<SliceWorkerOutcome>>) -> bool {
    results
        .values()
        .all(|result| matches!(result, Err(err) if err.downcast_ref::<CancelledError>().is_some()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairPolicy {
    Auto,
    Never,
    Always,
}

impl RepairPolicy {
    fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "" | "auto" => Ok(Self::Auto),
            "never" => Ok(Self::Never),
            "always" => Ok(Self::Always),
            other => bail!(
                "unknown integration_repair policy {other:?}; expected auto, never, or always"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Never => "never",
            Self::Always => "always",
        }
    }
}

fn should_run_integration_repair(policy: RepairPolicy, gate: &GateResult) -> bool {
    if gate.status != "passed" && gate_needs_operator(gate) {
        return false;
    }
    match policy {
        RepairPolicy::Auto => gate.status != "passed",
        RepairPolicy::Never => false,
        RepairPolicy::Always => true,
    }
}

fn repair_trigger_for_gate(policy: RepairPolicy, gate: &GateResult) -> &'static str {
    if gate.status == "passed" && policy == RepairPolicy::Always {
        "policy_always_gate_passed"
    } else {
        "integration_gate_failed"
    }
}

fn skipped_repair_result(policy: RepairPolicy, gate: &GateResult) -> RepairResult {
    let (trigger, summary) = if gate.status == "passed" {
        (
            "gate_passed",
            format!(
                "integration gate passed; integration_repair={} skipped repair",
                policy.as_str()
            ),
        )
    } else if gate_needs_operator(gate) {
        (
            "operator_fix_gate_failed",
            "integration gate needs an operator environment fix; repair skipped".to_string(),
        )
    } else {
        (
            "policy_never_gate_failed",
            "integration gate failed; integration_repair=never skipped repair".to_string(),
        )
    };
    RepairResult {
        status: "skipped".to_string(),
        summary,
        trigger: trigger.to_string(),
        attempts: 0,
        ..RepairResult::default()
    }
}

/// Reads immutable ledger evidence first. Ordinal-named output is retained only as a
/// compatibility fallback for runs created before the CA-03 launch ledger existed.
fn read_worker_result(
    state: &StateStore,
    store: &artifact::Store,
    run_id: &str,
    slice_run: &SliceRun,
) -> Option<WorkerResult> {
    let ledger_result = state
        .list_worker_attempt_ledger(run_id, &slice_run.slice_id)
        .ok()
        .and_then(|attempts| {
            attempts.into_iter().rev().find_map(|attempt| {
                artifact::read_json(
                    store.output_path(run_id, &format!("{}.json", attempt.output_stem)),
                )
                .ok()
            })
        });
    ledger_result.or_else(|| {
        (slice_run.attempts > 0)
            .then(|| {
                artifact::read_json(store.output_path(
                    run_id,
                    &format!(
                        "{}.worker.attempt-{}.json",
                        slice_run.slice_id, slice_run.attempts
                    ),
                ))
                .ok()
            })
            .flatten()
    })
}

fn existing_completion_publication(
    store: &artifact::Store,
    run_id: &str,
    expected_branch: &str,
    completed_slice_ids: &[String],
    recorded_commit_sha: Option<&str>,
) -> Result<Option<artifact::CompletionPublicationReceipt>> {
    let head = gitutil::head_sha(store.repo_path())?;
    if let Some(recorded_commit_sha) = recorded_commit_sha
        && head != recorded_commit_sha
    {
        return Err(BlockedError::new(format!(
            "integration branch moved from recorded completion publication {recorded_commit_sha} to {head}; operator reconciliation is required"
        ))
        .into());
    }
    if let Some(receipt) =
        store.find_completion_publication(run_id, expected_branch, completed_slice_ids)?
    {
        if head != receipt.commit_sha {
            return Err(BlockedError::new(format!(
                "integration branch advanced beyond completion publication {} to {}; operator reconciliation is required",
                receipt.commit_sha, head
            ))
            .into());
        }
        return Ok(Some(receipt));
    }
    if !store.publication_reports_exist(run_id)
        || !completed_slice_ids
            .iter()
            .all(|slice_id| slice_closed_by_run_or_absent(store, slice_id, run_id))
    {
        if let Some(recorded_commit_sha) = recorded_commit_sha {
            return Err(BlockedError::new(format!(
                "recorded completion publication {recorded_commit_sha} is missing its reports or closed-slice metadata; operator reconciliation is required"
            ))
            .into());
        }
        return Ok(None);
    }
    if let Some(recorded_commit_sha) = recorded_commit_sha {
        return Err(BlockedError::new(format!(
            "recorded completion publication {recorded_commit_sha} does not match the current manifest; operator reconciliation is required"
        ))
        .into());
    }
    Ok(None)
}

fn latest_completion_publication_commit(events: &[crate::domain::Event]) -> Option<&str> {
    events.iter().rev().find_map(|event| {
        (event.typ == "completion_publication_committed")
            .then(|| event.payload["commit_sha"].as_str())
            .flatten()
    })
}

fn completion_publication_event_exists(events: &[crate::domain::Event], commit_sha: &str) -> bool {
    events.iter().any(|event| {
        event.typ == "completion_publication_committed"
            && event.payload["commit_sha"].as_str() == Some(commit_sha)
    })
}

fn slice_closed_by_run_or_absent(store: &artifact::Store, slice_id: &str, run_id: &str) -> bool {
    let path = store.slice_path(slice_id);
    if !path.exists() {
        return true;
    }
    artifact::read_json::<Slice>(&path)
        .map(|slice| {
            slice.status == crate::domain::SLICE_STATUS_CLOSED && slice.closed_by_run == run_id
        })
        .unwrap_or(false)
}

fn final_exit_states(gate: &GateResult, completed_slices: &[WorkerResult]) -> WorkflowExitStates {
    let gate_passed = gate.status == "passed";
    let gate_blocked = !gate_passed && gate_needs_operator(gate);
    WorkflowExitStates {
        run: if gate_passed {
            "completed"
        } else if gate_blocked {
            "blocked"
        } else {
            "failed"
        }
        .to_string(),
        handoff: if gate_passed {
            "ready_for_handoff"
        } else {
            "not_ready"
        }
        .to_string(),
        evidence: if gate_passed {
            "daemon_attested"
        } else if gate_blocked {
            "daemon_blocked"
        } else {
            "daemon_rejected"
        }
        .to_string(),
        slices: completed_slices
            .iter()
            .map(|result| SliceExitState {
                slice_id: result.slice_id.clone(),
                worker: result.status.clone(),
                daemon: "merged".to_string(),
            })
            .collect(),
    }
}

fn final_evidence_attestation(gate: &GateResult) -> EvidenceAttestation {
    let gate_passed = gate.status == "passed";
    let gate_blocked = !gate_passed && gate_needs_operator(gate);
    let mut basis = vec![
        "worker acceptance_status is treated as an evidence claim, not approval".to_string(),
        "daemon required a committed clean worktree before merge".to_string(),
        "daemon required slice verification/lightweight checks before merge".to_string(),
    ];
    if gate_passed {
        basis.push("daemon integration gate passed before handoff".to_string());
    } else if gate_blocked {
        basis.push(format!(
            "daemon integration gate is blocked on an operator environment fix: {}",
            gate.summary
        ));
    } else {
        basis.push(format!(
            "daemon integration gate did not attest handoff: {}",
            gate.summary
        ));
    }
    EvidenceAttestation {
        status: if gate_passed {
            "daemon_attested"
        } else if gate_blocked {
            "daemon_blocked"
        } else {
            "daemon_rejected"
        }
        .to_string(),
        attester: "khazad-doom-daemon".to_string(),
        worker_self_approved: false,
        basis,
    }
}

fn historical_handoff_exit_states(
    status: RunStatus,
    completed_slices: &[String],
) -> WorkflowExitStates {
    WorkflowExitStates {
        run: status.as_str().to_string(),
        handoff: if status == RunStatus::Completed {
            "ready_for_handoff"
        } else {
            "not_ready"
        }
        .to_string(),
        evidence: "attestation_unavailable".to_string(),
        slices: completed_slices
            .iter()
            .map(|slice_id| SliceExitState {
                slice_id: slice_id.clone(),
                worker: "complete".to_string(),
                daemon: "merged".to_string(),
            })
            .collect(),
    }
}

fn historical_evidence_attestation() -> EvidenceAttestation {
    EvidenceAttestation {
        status: "attestation_unavailable".to_string(),
        attester: "khazad-doom-daemon".to_string(),
        worker_self_approved: false,
        basis: vec!["historical summary did not include attestation metadata".to_string()],
    }
}

fn sh_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Deserialize)]
struct GithubIssue {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    labels: Vec<GithubLabel>,
}

#[derive(Debug, Deserialize)]
struct GithubLabel {
    name: String,
}

fn fetch_github_issue(issue: &str) -> Result<GithubIssue> {
    let args = github_issue_view_args(issue);
    let output = Command::new("gh")
        .args(&args)
        .output()
        .with_context(|| "run gh issue view")?;
    if !output.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        bail!(
            "gh issue view failed ({}): {}",
            args.join(" "),
            combined.trim()
        );
    }
    let mut parsed: GithubIssue = serde_json::from_slice(&output.stdout)
        .with_context(|| "parse gh issue view JSON output")?;
    if parsed.url.trim().is_empty() {
        parsed.url = issue.to_string();
    }
    Ok(parsed)
}

fn github_issue_view_args(issue: &str) -> Vec<String> {
    if let Some((repo, number)) = parse_github_issue_url(issue) {
        return vec![
            "issue".to_string(),
            "view".to_string(),
            number,
            "--repo".to_string(),
            repo,
            "--json".to_string(),
            "title,body,url,labels".to_string(),
        ];
    }
    vec![
        "issue".to_string(),
        "view".to_string(),
        issue.to_string(),
        "--json".to_string(),
        "title,body,url,labels".to_string(),
    ]
}

fn parse_github_issue_url(issue: &str) -> Option<(String, String)> {
    let without_scheme = issue
        .strip_prefix("https://github.com/")
        .or_else(|| issue.strip_prefix("http://github.com/"))?;
    let parts: Vec<_> = without_scheme.split('/').collect();
    if parts.len() < 4 || parts[2] != "issues" {
        return None;
    }
    let number = parts[3]
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((format!("{}/{}", parts[0], parts[1]), number))
}

fn slug_slice_id(title: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "slice-imported-issue".to_string()
    } else if slug.starts_with("slice-") {
        slug
    } else {
        format!("slice-{slug}")
    }
}

fn first_meaningful_paragraph(body: &str) -> Option<String> {
    body.split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .filter(|paragraph| !paragraph.starts_with('#'))
        .filter(|paragraph| !paragraph.starts_with("- ["))
        .map(|paragraph| {
            paragraph
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .find(|paragraph| !paragraph.is_empty())
}

fn acceptance_from_issue_body(body: &str) -> Vec<String> {
    let criteria: Vec<_> = body
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            for prefix in ["- [ ]", "- [x]", "- [X]", "* [ ]", "* [x]", "* [X]"] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    let criterion = rest.trim();
                    if !criterion.is_empty() {
                        return Some(criterion.to_string());
                    }
                }
            }
            None
        })
        .collect();
    if criteria.is_empty() {
        vec!["Issue acceptance criteria are satisfied.".to_string()]
    } else {
        criteria
    }
}

fn validate_remote_handoff_ref(repo_path: &str, branch: &str, expected_sha: &str) -> Result<()> {
    let remote_ref = format!("refs/heads/{branch}");
    let output = gitutil::run(
        repo_path,
        &["ls-remote", "--exit-code", "--heads", "origin", &remote_ref],
    )
    .with_context(|| format!("validate remote handoff ref {remote_ref}"))?;
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let line = lines.next().ok_or_else(|| {
        BlockedError::new(format!(
            "remote handoff ref {remote_ref} is absent after receipt push"
        ))
    })?;
    let mut fields = line.split_whitespace();
    let actual_sha = fields.next().unwrap_or_default();
    let actual_ref = fields.next().unwrap_or_default();
    if actual_sha != expected_sha
        || actual_ref != remote_ref
        || fields.next().is_some()
        || lines.next().is_some()
    {
        return Err(BlockedError::new(format!(
            "remote handoff ref {remote_ref} does not equal validated publication receipt {expected_sha}; found {actual_sha} {actual_ref}"
        ))
        .into());
    }
    Ok(())
}

fn run_handoff_command(
    action: &str,
    cwd: &str,
    args: &[&str],
    display_command: &str,
) -> Result<HandoffActionResult> {
    run_external_command(action, cwd, "git", args, display_command)
}

fn run_external_command(
    action: &str,
    cwd: &str,
    program: &str,
    args: &[&str],
    display_command: &str,
) -> Result<HandoffActionResult> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("run handoff action {action}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(HandoffActionResult {
        action: action.to_string(),
        command: display_command.to_string(),
        status: if output.status.success() {
            "passed"
        } else {
            "failed"
        }
        .to_string(),
        exit_code: output.status.code(),
        output: combined.trim().to_string(),
    })
}

fn handoff_diagnostics(repo_path: &str) -> HandoffDiagnostics {
    let origin_url = gitutil::run(repo_path, &["remote", "get-url", "origin"]).unwrap_or_default();
    let gh_output = Command::new("gh").arg("--version").output();
    match gh_output {
        Ok(output) if output.status.success() => HandoffDiagnostics {
            origin_url,
            gh_available: true,
            gh_version: String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
        },
        Ok(output) => HandoffDiagnostics {
            origin_url,
            gh_available: false,
            gh_version: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Err(err) => HandoffDiagnostics {
            origin_url,
            gh_available: false,
            gh_version: err.to_string(),
        },
    }
}

fn effective_cockpit_mode(
    pi_args: &mut Vec<String>,
    config: &WorkflowConfig,
) -> Result<CockpitMode> {
    Ok(take_cockpit_mode_transport_arg(pi_args)?.unwrap_or(config.cockpit))
}

fn effective_parallelism(requested: usize, config: &WorkflowConfig) -> usize {
    if requested > 1 {
        requested
    } else if config.parallelism > 0 {
        config.parallelism
    } else {
        requested.max(1)
    }
}

// Parallelism is allowed only when slice authority is disjoint. This keeps the
// scheduler rule local: overlapping slices start from the latest merged head
// instead of producing later merge-conflict recovery special cases.
fn worker_batches_for_layer(layer: &[Slice], parallelism: usize) -> Vec<Vec<Slice>> {
    let max_batch_size = parallelism.max(1);
    let mut batches: Vec<Vec<Slice>> = Vec::new();
    for slice in layer {
        if max_batch_size == 1 {
            batches.push(vec![slice.clone()]);
            continue;
        }
        if let Some(batch) = batches.iter_mut().find(|batch| {
            batch.len() < max_batch_size
                && batch
                    .iter()
                    .all(|candidate| !slice_areas_overlap(candidate, slice))
        }) {
            batch.push(slice.clone());
        } else {
            batches.push(vec![slice.clone()]);
        }
    }
    batches
}

fn slice_areas_overlap(left: &Slice, right: &Slice) -> bool {
    if left.areas.is_empty() || right.areas.is_empty() {
        return true;
    }
    left.areas.iter().any(|left_area| {
        right
            .areas
            .iter()
            .any(|right_area| area_prefixes_overlap(left_area, right_area))
    })
}

fn area_prefixes_overlap(left: &str, right: &str) -> bool {
    let left = left.trim().trim_matches('/');
    let right = right.trim().trim_matches('/');
    if left.is_empty() || right.is_empty() || left == "." || right == "." {
        return true;
    }
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

fn tail_lines(path: &Path, line_count: usize) -> Result<Vec<String>> {
    if line_count == 0 || !path.exists() {
        return Ok(Vec::new());
    }
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let max_bytes = 64 * 1024;
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let lines: Vec<_> = text.lines().map(str::to_string).collect();
    let keep_from = lines.len().saturating_sub(line_count);
    Ok(lines[keep_from..].to_vec())
}

#[derive(Default)]
struct ActiveRuns {
    count: AtomicUsize,
    tokens: Mutex<HashMap<String, CancellationToken>>,
}

impl ActiveRuns {
    fn register(&self, run_id: String, token: CancellationToken) {
        self.tokens
            .lock()
            .expect("active runs mutex poisoned")
            .insert(run_id, token);
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    fn unregister(&self, run_id: &str) {
        self.tokens
            .lock()
            .expect("active runs mutex poisoned")
            .remove(run_id);
        self.count.fetch_sub(1, Ordering::SeqCst);
    }

    fn contains(&self, run_id: &str) -> bool {
        self.tokens
            .lock()
            .expect("active runs mutex poisoned")
            .contains_key(run_id)
    }

    fn cancel(&self, run_id: &str) -> bool {
        let token = self
            .tokens
            .lock()
            .expect("active runs mutex poisoned")
            .get(run_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

struct ActiveRunGuard {
    active: Arc<ActiveRuns>,
    run_id: String,
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        self.active.unregister(&self.run_id);
    }
}

fn proposal_needs_followup_apply(proposal: &ReplanProposal) -> bool {
    if proposal.state != crate::domain::ReplanProposalState::Accepted {
        return false;
    }
    let Some(decision) = proposal.operator_decision.as_ref() else {
        return false;
    };
    if decision.applied || decision.decision != "accepted" {
        return false;
    }
    if decision.apply_status.is_empty() {
        return applyable_followup_draft(proposal).is_some();
    }
    matches!(decision.apply_status.as_str(), "pending" | "incomplete")
}

fn applyable_followup_draft(proposal: &ReplanProposal) -> Option<FollowupSliceDraft> {
    let [change] = proposal.proposed_changes.as_slice() else {
        return None;
    };
    if change.kind != "add_followup_slice" {
        return None;
    }
    change.followup_slice_draft()
}

fn add_followup_slice_draft_from_proposal(proposal: &ReplanProposal) -> Option<FollowupSliceDraft> {
    applyable_followup_draft(proposal)
}

fn is_apply_refusal(err: &anyhow::Error) -> bool {
    err.to_string().contains("apply_refused:")
}

fn selected_slice_ids(selected_slice_id: &str) -> Vec<String> {
    selected_slice_id
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

fn queue_snapshot_hash(ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update(id.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn mission_envelope_hash(envelope: &MissionEnvelope) -> Result<String> {
    let bytes = serde_json::to_vec(envelope)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn checkpoint_id(checkpoint: &str, stage: &str, sha: &str) -> String {
    let short_sha = sha.chars().take(12).collect::<String>();
    format!("{checkpoint}:{stage}:{short_sha}")
}

fn slice_run_is_merged(state: &StateStore, run_id: &str, slice_id: &str) -> Result<bool> {
    Ok(state
        .get_slice_runs(run_id)?
        .into_iter()
        .any(|slice_run| slice_run.slice_id == slice_id && slice_run.status == SliceStatus::Merged))
}

fn slice_matches_draft(slice: &Slice, draft: &FollowupSliceDraft) -> bool {
    slice.id == draft.id
        && slice.title == draft.title
        && slice.goal == draft.goal
        && slice.depends_on == draft.depends_on
        && slice.areas == draft.areas
        && slice.acceptance == draft.acceptance
        && slice.must_ask_if == draft.must_ask_if
        && slice.verify_profile == draft.verify_profile
        && slice.verify == draft.verify
}

fn followup_generation(existing_slices: &[Slice], parent_slice_id: &str) -> u64 {
    existing_slices
        .iter()
        .find(|slice| slice.id == parent_slice_id)
        .and_then(Slice::provenance)
        .map(|provenance| provenance.generation.saturating_add(1))
        .unwrap_or(1)
}

fn provenance_created_by(decision: &ReplanDecision) -> String {
    let authorizer = decision.authorizer.trim();
    if decision.source == "frontier_policy" || authorizer.starts_with("envelope:") {
        "worker+daemon".to_string()
    } else {
        "operator".to_string()
    }
}

fn display_or_dash(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() { "-" } else { trimmed }
}

fn validate_followup_slice_draft(
    draft: &FollowupSliceDraft,
    existing_slices: &[Slice],
) -> Result<()> {
    if draft.rationale.trim().is_empty() {
        bail!("follow-up slice draft rationale is required");
    }
    let slice = draft.to_slice();
    artifact::validate_slice(&slice)
        .with_context(|| format!("follow-up slice draft {:?} is not a valid slice", draft.id))?;
    let mut with_draft = existing_slices.to_vec();
    with_draft.push(slice);
    let issues = artifact::validate_slice_set(&with_draft);
    if !issues.is_empty() {
        let messages = issues
            .into_iter()
            .map(|issue| {
                if issue.slice_id.trim().is_empty() {
                    issue.message
                } else {
                    format!("{}: {}", issue.slice_id, issue.message)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "follow-up slice draft {:?} violates slice graph rules: {messages}",
            draft.id
        );
    }
    Ok(())
}

fn invalid_candidate_followup_finding(
    draft_index: usize,
    draft: &FollowupSliceDraft,
    err: &anyhow::Error,
) -> Finding {
    let label = if draft.id.trim().is_empty() {
        format!("#{}", draft_index + 1)
    } else {
        format!("{:?}", draft.id)
    };
    Finding {
        id: format!("candidate-followup-slice-{}", draft_index + 1),
        severity: "warning".to_string(),
        action: "no-op".to_string(),
        file: String::new(),
        line: 0,
        description: format!(
            "candidate_followup_slices[{draft_index}] draft {label} rejected: {err:#}"
        ),
    }
}

fn followup_slice_draft_summary(draft: &FollowupSliceDraft) -> String {
    let title = display_or_untitled(&draft.title);
    let areas = if draft.areas.is_empty() {
        "<none>".to_string()
    } else {
        draft.areas.join(",")
    };
    let rationale = draft.rationale.trim();
    if rationale.is_empty() {
        format!(
            "Add follow-up slice {} ({title}); areas=[{areas}]",
            draft.id
        )
    } else {
        format!(
            "Add follow-up slice {} ({title}); areas=[{areas}]; rationale: {rationale}",
            draft.id
        )
    }
}

fn display_or_untitled(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "<untitled>"
    } else {
        trimmed
    }
}

fn matching_proposed_disposition_index(
    findings: &[Finding],
    dispositions: &[FindingDisposition],
    draft: &FollowupSliceDraft,
) -> Option<usize> {
    let draft_id = draft.id.trim().to_ascii_lowercase();
    if !draft_id.is_empty()
        && let Some((index, _)) = dispositions.iter().enumerate().find(|(_, disposition)| {
            is_unassigned_proposed_disposition(disposition)
                && disposition_or_finding_mentions_draft(findings, disposition, &draft_id)
        })
    {
        return Some(index);
    }
    dispositions
        .iter()
        .position(is_unassigned_proposed_disposition)
}

fn is_unassigned_proposed_disposition(disposition: &FindingDisposition) -> bool {
    disposition.disposition == "proposed" && disposition.replan_proposal_id.trim().is_empty()
}

fn disposition_or_finding_mentions_draft(
    findings: &[Finding],
    disposition: &FindingDisposition,
    draft_id: &str,
) -> bool {
    let mut haystacks = vec![
        disposition.finding_id.as_str(),
        disposition.rationale.as_str(),
    ];
    if let Some(finding) = finding_for_disposition(findings, disposition) {
        haystacks.extend([
            finding.id.as_str(),
            finding.file.as_str(),
            finding.description.as_str(),
        ]);
    }
    haystacks
        .into_iter()
        .any(|value| value.to_ascii_lowercase().contains(draft_id))
}

fn validate_worker_result(result: &WorkerResult, slice: &Slice) -> Result<()> {
    match result.status.as_str() {
        "complete" | "blocked" | "failed" => {}
        other => bail!("unknown worker status {other:?}"),
    }
    if result.slice_id != slice.id {
        bail!(
            "worker slice_id {:?} did not match selected slice {:?}",
            result.slice_id,
            slice.id
        );
    }
    if result.summary.trim().is_empty() {
        bail!("worker summary is required");
    }
    if result.status == "complete" {
        validate_actionable_finding_dispositions(
            "worker",
            &result.findings,
            &result.finding_dispositions,
        )?;
    }
    validate_acceptance_evidence(result, slice)?;
    Ok(())
}

fn validate_acceptance_evidence(result: &WorkerResult, slice: &Slice) -> Result<()> {
    for evidence in &result.acceptance_status {
        match evidence.status.as_str() {
            "satisfied" | "blocked" | "failed" => {}
            other => bail!("unknown acceptance status {other:?}"),
        }
        if evidence.criterion.trim().is_empty() {
            bail!("acceptance criterion is required");
        }
        if evidence.evidence.trim().is_empty() {
            bail!(
                "acceptance evidence is required for {:?}",
                evidence.criterion
            );
        }
    }
    if result.status != "complete" {
        return Ok(());
    }
    for criterion in &slice.acceptance {
        let Some(evidence) = result
            .acceptance_status
            .iter()
            .find(|evidence| evidence.criterion == *criterion)
        else {
            bail!("missing acceptance evidence for {criterion:?}");
        };
        if evidence.status != "satisfied" {
            bail!(
                "acceptance criterion {:?} is not satisfied: {}",
                criterion,
                evidence.status
            );
        }
    }
    Ok(())
}

fn validate_repair_result(result: &RepairResult) -> Result<()> {
    match result.status.as_str() {
        "no-op" | "fixed" | "blocked" | "failed" => {}
        other => bail!("unknown integration repair status {other:?}"),
    }
    if result.summary.trim().is_empty() {
        bail!("integration repair summary is required");
    }
    if matches!(result.status.as_str(), "no-op" | "fixed") {
        validate_actionable_finding_dispositions(
            "integration repair",
            &result.findings,
            &result.finding_dispositions,
        )?;
    }
    Ok(())
}

fn validate_actionable_finding_dispositions(
    source: &str,
    findings: &[Finding],
    dispositions: &[FindingDisposition],
) -> Result<()> {
    for disposition in dispositions {
        if disposition.disposition.trim().is_empty() {
            bail!("{source} finding disposition is required");
        }
        if !matches!(
            disposition.disposition.as_str(),
            "fixed" | "not_applicable" | "documented" | "proposed"
        ) {
            bail!(
                "unknown {source} finding disposition {:?}",
                disposition.disposition
            );
        }
        if disposition.rationale.trim().is_empty() {
            bail!("{source} finding disposition rationale is required");
        }
        if disposition.finding_id.trim().is_empty() && disposition.finding_index == 0 {
            bail!("{source} finding disposition must identify finding_id or finding_index");
        }
    }

    for (index, finding) in findings.iter().enumerate() {
        if !is_actionable_finding(finding) {
            continue;
        }
        let Some(disposition) = dispositions
            .iter()
            .find(|disposition| disposition_matches_finding(disposition, finding, index))
        else {
            let finding_label = if finding.id.trim().is_empty() {
                format!("#{}", index + 1)
            } else {
                finding.id.clone()
            };
            bail!(
                "{source} actionable finding {finding_label} requires a terminal disposition or replan proposal"
            );
        };
        if disposition.disposition == "proposed"
            || !disposition.replan_proposal_id.trim().is_empty()
        {
            continue;
        }
        if !matches!(
            disposition.disposition.as_str(),
            "fixed" | "not_applicable" | "documented"
        ) {
            bail!(
                "{source} actionable finding disposition {:?} is not terminal",
                disposition.disposition
            );
        }
    }
    Ok(())
}

fn is_actionable_finding(finding: &Finding) -> bool {
    !matches!(finding.action.as_str(), "" | "no-op")
}

fn disposition_matches_finding(
    disposition: &FindingDisposition,
    finding: &Finding,
    index: usize,
) -> bool {
    (!disposition.finding_id.trim().is_empty() && disposition.finding_id == finding.id)
        || (disposition.finding_index == index + 1)
}

fn proposed_finding_disposition_indices(dispositions: &[FindingDisposition]) -> Vec<usize> {
    dispositions
        .iter()
        .enumerate()
        .filter(|(_, disposition)| {
            disposition.disposition == "proposed"
                && disposition.replan_proposal_id.trim().is_empty()
        })
        .map(|(index, _)| index)
        .collect()
}

fn finding_for_disposition<'a>(
    findings: &'a [Finding],
    disposition: &FindingDisposition,
) -> Option<&'a Finding> {
    findings.iter().enumerate().find_map(|(index, finding)| {
        disposition_matches_finding(disposition, finding, index).then_some(finding)
    })
}

fn disposition_finding_id(
    index: usize,
    finding: Option<&Finding>,
    disposition: &FindingDisposition,
) -> String {
    finding
        .and_then(|finding| (!finding.id.trim().is_empty()).then(|| finding.id.clone()))
        .or_else(|| {
            (!disposition.finding_id.trim().is_empty()).then(|| disposition.finding_id.clone())
        })
        .unwrap_or_else(|| format!("finding-{}", disposition.finding_index.max(index + 1)))
}

fn invalid_worker_output_error(message: &str) -> bool {
    message.contains("parse pi JSON output")
        || message.contains("no JSON object found")
        || message.contains("invalid JSON object")
}

fn new_run_id() -> String {
    let mut bytes = [0_u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!(
        "kd-{}-{}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        hex::encode(bytes)
    )
}

fn new_worker_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn run_preflight_native_pi_tui_worker(run: &Run) -> bool {
    let store = artifact::Store::new(&run.repo_path);
    artifact::read_json::<serde_json::Value>(store.output_path(&run.id, "preflight.json"))
        .ok()
        .and_then(|value| {
            value
                .get("native_pi_tui_worker")
                .or_else(|| value.get("experimental_pi_tui_worker"))
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
}

fn tui_worker_session_name(run_id: &str, slice_id: &str, launch_identity: usize) -> String {
    format!("kd-tui-{run_id}-{slice_id}-launch-{launch_identity}")
}

fn wait_for_pi_tui_worker_result(
    artifacts: &artifact::PiTuiWorkerArtifacts,
    cancel: CancellationToken,
    events: Option<RunnerEventSink>,
    pane_id: String,
) -> Result<crate::agent::ResultData> {
    let mut next_observation = Instant::now();
    loop {
        if artifacts.result_path.exists() {
            let data = parse_pi_tui_worker_result_artifact(artifacts)?;
            if let Some(sink) = &events {
                sink(RunnerEvent::finished(None, Some(0)));
            }
            return Ok(data);
        }
        if cancel.is_cancelled() {
            let _ = close_default_pane(&pane_id);
            if let Some(sink) = &events {
                sink(RunnerEvent::finished(None, None));
            }
            bail!("job cancelled");
        }
        if Instant::now() >= next_observation {
            if let Some(sink) = &events {
                sink(RunnerEvent::process_observed(None));
            }
            next_observation = Instant::now() + Duration::from_secs(1);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn worker_job_env(
    paths: &Paths,
    run: &Run,
    slice_id: &str,
    attempt: usize,
    launch_id: Option<i64>,
    launch_stem: Option<&str>,
    token: &str,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        (
            "KHAZAD_DAEMON_SOCKET".to_string(),
            paths.socket().to_string_lossy().to_string(),
        ),
        ("KHAZAD_RUN_ID".to_string(), run.id.clone()),
        ("KHAZAD_SLICE_ID".to_string(), slice_id.to_string()),
        // This is the established worker-wire retry ordinal.  Immutable
        // daemon launch identity travels separately below, so existing workers
        // and their ask_operator payloads remain protocol-compatible.
        ("KHAZAD_ATTEMPT".to_string(), attempt.to_string()),
        ("KHAZAD_RETRY_ORDINAL".to_string(), attempt.to_string()),
        ("KHAZAD_WORKER_TOKEN".to_string(), token.to_string()),
    ]);
    if let Some(launch_id) = launch_id {
        env.insert("KHAZAD_LAUNCH_ID".to_string(), launch_id.to_string());
    }
    if let Some(launch_stem) = launch_stem {
        env.insert("KHAZAD_LAUNCH_STEM".to_string(), launch_stem.to_string());
    }
    env
}

fn classify_run_failure(err: &anyhow::Error) -> RunStatus {
    if err.downcast_ref::<CancelledError>().is_some() {
        RunStatus::Cancelled
    } else if err.downcast_ref::<BlockedError>().is_some() {
        RunStatus::Blocked
    } else {
        RunStatus::Failed
    }
}

#[derive(Debug)]
struct BlockedError {
    reason: String,
}

impl BlockedError {
    fn new(reason: String) -> Self {
        Self { reason }
    }
}

impl fmt::Display for BlockedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for BlockedError {}

#[cfg(test)]
mod tests {
    use super::{
        AgentCallContext, DEFAULT_REPAIR_ATTEMPTS, DEFAULT_WORKER_ENVELOPE_RETRY_ATTEMPTS,
        INTEGRATION_REPAIR_SCOPE_ID, IntegrationRepairContext, MAX_WORKER_ATTEMPTS, Manager,
        RepairPolicy, ResumeOptions, RunEconomicsRecorder, RunReadModelBuilder,
        RunReadModelOptions, StartOptions, VerificationCommandCache, WorkerAttemptContext,
        check_failure_needs_operator, existing_completion_publication, queue_snapshot_hash,
        repair_authority_violations, selected_slice_ids, should_run_integration_repair,
        validate_followup_slice_draft, validate_mission_envelope, validate_repair_result,
        validate_worker_result, worker_attempt_retry_disposition,
    };
    use crate::agent::{CancellationToken, Job, ResultData, Runner, RunnerEventSink, Usage};
    use crate::artifact::{self, Store as ArtifactStore};
    use crate::domain::{
        AcceptanceEvidence, AutonomyLevel, CheckResult, CockpitMode, Finding, FindingDisposition,
        FollowupSliceDraft, FrontierBudgetState, FrontierClassification, GateCommandResult,
        GateResult, Handoff, ImplementationSummary, MissionEnvelope, OriginNotificationTarget,
        RepairResult, ReplanEvidenceLink, ReplanProposal, ReplanProposalSource,
        ReplanProposalState, ReplanProposedChange, Run, RunEconomics, RunStatus, Slice,
        SliceProvenance, SliceRun, SliceStatus, TerminalNotificationRecord, VerifyCommand,
        VerifyProfile, WorkerQuestionAnswerSource, WorkerQuestionRecommendation, WorkerResult,
        WorkflowConfig,
    };
    use crate::gitutil;
    use crate::paths::Paths;
    use crate::state::{Store as StateStore, WorkerQuestionDecisionCommand};
    use crate::workflow::events as workflow_events;
    use anyhow::Result;
    use chrono::Utc;
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::fs;
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;
    use std::time::{Duration, Instant};

    fn slice(id: &str) -> Slice {
        Slice {
            id: id.to_string(),
            title: format!("Title {id}"),
            goal: "Goal".to_string(),
            github_issue: String::new(),
            status: crate::domain::SLICE_STATUS_OPEN.to_string(),
            closed_by_run: String::new(),
            closed_at: String::new(),
            depends_on: Vec::new(),
            areas: Vec::new(),
            acceptance: vec!["done".to_string()],
            must_ask_if: Vec::new(),
            verify_profile: String::new(),
            verify: Vec::new(),
            verify_timeout_seconds: 0,
        }
    }

    fn write_publication_reports(
        store: &ArtifactStore,
        run_id: &str,
        slice_ids: &[&str],
    ) -> Result<()> {
        let gate = GateResult {
            status: "passed".to_string(),
            ..Default::default()
        };
        let summary = ImplementationSummary {
            run_id: run_id.to_string(),
            repo_path: store.repo_path().to_string_lossy().into_owned(),
            integration_branch: "main".to_string(),
            base_sha: "base".to_string(),
            final_sha: String::new(),
            worker_profile: crate::domain::WorkerProfileEvidence::default(),
            mission_envelope: None,
            frontier_budget: None,
            completed_slices: slice_ids
                .iter()
                .map(|slice_id| WorkerResult {
                    slice_id: (*slice_id).to_string(),
                    status: "completed".to_string(),
                    ..WorkerResult::default()
                })
                .collect(),
            checks: Vec::new(),
            integration_repair: RepairResult::default(),
            pre_repair_integration_gate: None,
            integration_gate: gate,
            exit_states: crate::domain::WorkflowExitStates::default(),
            evidence_attestation: crate::domain::EvidenceAttestation::default(),
            economics: RunEconomics::default(),
            plan_revisions: crate::domain::PlanRevisions::default(),
            worker_questions: Vec::new(),
            worker_attempts: Vec::new(),
            created_at: Utc::now(),
        };
        store.write_implementation_summary(&summary)?;
        store.write_final_report(&summary)?;
        Ok(())
    }

    fn mission_envelope() -> MissionEnvelope {
        MissionEnvelope {
            goal: "Complete the bounded mission".to_string(),
            allowed_areas: vec!["src/".to_string(), "README.md".to_string()],
            non_goals: vec!["change public API".to_string()],
            verify_profile: "default".to_string(),
            max_auto_promotions: 2,
            max_depth: 1,
            max_generated_slices: 3,
            autonomy_level: AutonomyLevel::Shadow,
            must_ask_if: vec!["scope expands".to_string()],
        }
    }

    fn followup_draft(id: &str) -> FollowupSliceDraft {
        FollowupSliceDraft {
            id: id.to_string(),
            title: format!("Follow-up {id}"),
            goal: "Complete follow-up work".to_string(),
            areas: vec!["src/".to_string()],
            acceptance: vec!["follow-up acceptance is met".to_string()],
            verify: vec!["cargo test followup".to_string()],
            verify_profile: String::new(),
            depends_on: vec!["slice-001".to_string()],
            must_ask_if: vec!["intent changes".to_string()],
            rationale: "Worker found a bounded follow-up.".to_string(),
        }
    }

    fn create_followup_replan_proposal(
        state: &StateStore,
        run_id: &str,
        parent_slice_id: &str,
        draft: FollowupSliceDraft,
    ) -> Result<ReplanProposal> {
        state.create_replan_proposal(
            run_id,
            "",
            ReplanProposalSource {
                kind: "worker_finding".to_string(),
                slice_id: parent_slice_id.to_string(),
                phase: "test".to_string(),
                attempt: 1,
                summary: "follow-up needed".to_string(),
            },
            Vec::new(),
            Vec::new(),
            vec![ReplanProposedChange::with_followup_slice_draft(
                "add_followup_slice".to_string(),
                draft.id.clone(),
                "Add generated follow-up slice".to_string(),
                draft,
            )],
            "operator_review",
        )
    }

    fn test_run(run_id: &str, repo: &Path, selected_slice_id: &str) -> Result<Run> {
        let now = Utc::now();
        Ok(Run {
            id: run_id.to_string(),
            repo_id: crate::paths::repo_id(repo),
            repo_path: repo.to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo)?,
            integration_branch: format!("khazad/{run_id}/integration"),
            selected_slice_id: selected_slice_id.to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        })
    }

    fn workflow_slices_snapshot(store: &ArtifactStore) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut snapshot = BTreeMap::new();
        let dir = store.slices_dir();
        snapshot_dir(&dir, &dir, &mut snapshot)?;
        Ok(snapshot)
    }

    fn snapshot_dir(
        base: &Path,
        path: &Path,
        snapshot: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let entry_path = entry.path();
            if entry.file_type()?.is_dir() {
                snapshot_dir(base, &entry_path, snapshot)?;
            } else {
                let rel = entry_path
                    .strip_prefix(base)?
                    .to_string_lossy()
                    .replace('\\', "/");
                snapshot.insert(rel, fs::read(entry_path)?);
            }
        }
        Ok(())
    }

    #[derive(Default)]
    struct BudgetExhaustingRunner {
        calls: AtomicUsize,
    }

    impl Runner for BudgetExhaustingRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            fs::write(job.cwd.join("slice-001.txt"), "blocked\n")?;
            gitutil::run(&job.cwd, &["add", "-A"])?;
            gitutil::run(&job.cwd, &["commit", "-m", "consume final worker retry"])?;
            let mut output = valid_worker_output(&handoff, &job.cwd)?;
            output["status"] = json!("blocked");
            output["summary"] = json!("operator grant required");
            Ok(ResultData {
                output: Some(output),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "retry-budget-test"
        }
    }

    struct OperatorPauseRunner {
        fail: bool,
    }

    impl Runner for OperatorPauseRunner {
        fn run(
            &self,
            _job: Job,
            _cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            thread::sleep(Duration::from_millis(300));
            if self.fail {
                anyhow::bail!("injected worker failure after operator pause");
            }
            Ok(ResultData {
                output: None,
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "operator-pause-test"
        }
    }

    #[test]
    fn outer_gate_restoration_failure_outranks_coincident_cancellation() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("run-outer-gate-restoration", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let manager = Manager::new(paths, state);
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let gate = GateResult {
            status: "failed".to_string(),
            summary: "integration workspace could not be restored".to_string(),
            verification_cancelled: true,
            failure_kind: "verification_restoration_failed".to_string(),
            verification_workspace: Some(crate::domain::VerificationWorkspaceEvidence {
                restoration_error: "injected outer guard restoration failure".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = manager
            .stop_after_cancelled_integration_gate(&store, &run.id, &gate)
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("operator intervention is required"),
            "{err:#}"
        );
        let recorded: GateResult =
            artifact::read_json(store.output_path(&run.id, "integration-gate.cancelled.json"))?;
        assert_eq!(recorded.failure_kind, "verification_restoration_failed");
        assert_eq!(
            recorded
                .verification_workspace
                .as_ref()
                .map(|evidence| evidence.restoration_error.as_str()),
            Some("injected outer guard restoration failure")
        );
        Ok(())
    }

    fn record_zero_timeout_operator_pause(fail: bool) -> Result<RunEconomics> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("run-zero-timeout-pause", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        state.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: "khazad/test/slice-001".to_string(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        state.open_active_worker_question_with_recommendation(
            "q-zero-timeout-pause",
            &run.id,
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation::default(),
            "worker_question_asked",
            |question| Ok(json!({ "question_id": question.id })),
            "awaiting operator answer",
        )?;
        let manager = Manager::new(paths, state);
        let context = WorkerAttemptContext::new(
            &run.id,
            "worker",
            "slice-001",
            1,
            None,
            None,
            &WorkflowConfig {
                worker_attempt_timeout_seconds: 0,
                ..WorkflowConfig::default()
            },
            false,
        );
        let job = Job {
            kind: "worker".to_string(),
            prompt: String::new(),
            cwd: repo.path().to_path_buf(),
            json_schema: String::new(),
            env: BTreeMap::new(),
            termination_grace_seconds: 0,
        };

        let economics = RunEconomicsRecorder::new("test", true, 1, 0);
        let outcome = manager.run_recorded_agent_job(
            Arc::new(OperatorPauseRunner { fail }),
            job,
            &CancellationToken::new(),
            context,
            &economics,
            AgentCallContext {
                phase: "worker",
                slice_id: "slice-001",
                attempt: 1,
                launch_id: None,
                launch_stem: None,
            },
        );
        if fail {
            let error = outcome.expect_err("runner should fail after the operator pause");
            assert!(error.to_string().contains("injected worker failure"));
        } else {
            outcome?;
        }
        Ok(economics.snapshot())
    }

    #[test]
    fn cockpit_identity_prefers_the_immutable_launch_over_retry_ordinal() {
        let config = WorkflowConfig::default();
        let modern = WorkerAttemptContext::new(
            "run-cockpit-identity",
            "worker",
            "slice-001",
            1,
            Some(41),
            Some("slice-001.launch-41"),
            &config,
            false,
        );
        let legacy = WorkerAttemptContext::new(
            "run-cockpit-identity",
            "worker",
            "slice-001",
            3,
            None,
            None,
            &config,
            false,
        );

        assert_eq!(modern.cockpit_launch_identity(), 41);
        assert_eq!(legacy.cockpit_launch_identity(), 3);
    }

    #[test]
    fn supervised_worker_uses_configured_process_termination_grace() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("run-worker-grace", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let manager = Manager::new(paths, state);
        let context = WorkerAttemptContext::new(
            &run.id,
            "worker",
            "slice-001",
            1,
            None,
            None,
            &WorkflowConfig {
                worker_attempt_timeout_seconds: 0,
                worker_termination_grace_seconds: 7,
                ..WorkflowConfig::default()
            },
            false,
        );
        let observed_grace = Arc::new(AtomicUsize::new(0));
        let capture = observed_grace.clone();
        let outcome = manager.run_supervised_worker_job_with(
            Job {
                kind: "worker".to_string(),
                prompt: String::new(),
                cwd: repo.path().to_path_buf(),
                json_schema: String::new(),
                env: BTreeMap::new(),
                termination_grace_seconds: 0,
            },
            &CancellationToken::new(),
            context,
            move |job, _cancel, _events| {
                capture.store(job.termination_grace_seconds as usize, Ordering::SeqCst);
                Ok(ResultData {
                    output: None,
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                })
            },
        );

        outcome.result?;
        assert_eq!(observed_grace.load(Ordering::SeqCst), 7);
        Ok(())
    }

    #[test]
    fn stale_launch_question_does_not_pause_current_launch_economics() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("run-stale-launch-pause", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let root = paths.repo_worktree_dir(&run.repo_id, &run.id);
        let first = state.allocate_worker_attempt(
            &run.id,
            "slice-001",
            1,
            1,
            0,
            0,
            "slice-worker",
            &root,
        )?;
        state.mark_worker_attempt_launched(first.launch_id)?;
        state.open_active_worker_question_with_launch_id_and_recommendation(
            "q-stale-launch-pause",
            &run.id,
            "slice-001",
            1,
            Some(first.launch_id),
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation::default(),
            "worker_question_asked",
            |question| Ok(json!({ "question_id": question.id })),
            "awaiting operator answer",
        )?;
        state.finish_worker_attempt(first.launch_id, "interrupted", "superseded")?;
        let second = state.allocate_worker_attempt(
            &run.id,
            "slice-001",
            2,
            1,
            0,
            0,
            "slice-worker",
            &root,
        )?;
        state.mark_worker_attempt_launched(second.launch_id)?;
        let manager = Manager::new(paths, state);
        let context = WorkerAttemptContext::new(
            &run.id,
            "worker",
            "slice-001",
            1,
            Some(second.launch_id),
            Some(&second.output_stem),
            &WorkflowConfig {
                worker_attempt_timeout_seconds: 0,
                ..WorkflowConfig::default()
            },
            false,
        );
        let economics = RunEconomicsRecorder::new("test", true, 1, 0);
        manager.run_recorded_agent_job(
            Arc::new(OperatorPauseRunner { fail: false }),
            Job {
                kind: "worker".to_string(),
                prompt: String::new(),
                cwd: repo.path().to_path_buf(),
                json_schema: String::new(),
                env: BTreeMap::new(),
                termination_grace_seconds: 0,
            },
            &CancellationToken::new(),
            context,
            &economics,
            AgentCallContext {
                phase: "worker",
                slice_id: "slice-001",
                attempt: 1,
                launch_id: Some(second.launch_id),
                launch_stem: Some(&second.output_stem),
            },
        )?;

        let snapshot = economics.snapshot();
        assert_eq!(snapshot.agent_calls.len(), 1);
        assert!(
            snapshot.agent_calls[0].operator_pause_ms < 100,
            "a stale launch question must not pause the current launch: {}ms",
            snapshot.agent_calls[0].operator_pause_ms
        );
        Ok(())
    }

    #[test]
    fn zero_attempt_timeout_still_records_operator_pause_economics() -> Result<()> {
        let snapshot = record_zero_timeout_operator_pause(false)?;
        assert_eq!(snapshot.agent_calls.len(), 1);
        assert_eq!(snapshot.agent_calls[0].status, "succeeded");
        assert!(
            snapshot.agent_calls[0].operator_pause_ms >= 150,
            "expected zero-timeout economics to track operator pause, got {}ms",
            snapshot.agent_calls[0].operator_pause_ms
        );
        Ok(())
    }

    #[test]
    fn failed_worker_call_preserves_operator_pause_economics() -> Result<()> {
        let snapshot = record_zero_timeout_operator_pause(true)?;
        assert_eq!(snapshot.agent_calls.len(), 1);
        let call = &snapshot.agent_calls[0];
        assert_eq!(call.status, "failed");
        assert!(call.error.contains("injected worker failure"));
        assert!(
            call.operator_pause_ms >= 150,
            "expected failed-call economics to track operator pause, got {}ms",
            call.operator_pause_ms
        );
        assert!(
            call.duration_ms <= 200,
            "expected net failed-call duration to exclude operator pause, got {}ms",
            call.duration_ms
        );
        Ok(())
    }

    #[test]
    fn rejects_worker_result_for_wrong_slice() {
        let result = WorkerResult {
            slice_id: "other".to_string(),
            status: "complete".to_string(),
            summary: "done".to_string(),
            ..WorkerResult::default()
        };
        assert!(validate_worker_result(&result, &slice("slice-001")).is_err());
    }

    #[test]
    fn rejects_missing_acceptance_evidence_for_completed_worker() {
        let result = WorkerResult {
            slice_id: "slice-001".to_string(),
            status: "complete".to_string(),
            summary: "done".to_string(),
            ..WorkerResult::default()
        };
        let err = validate_worker_result(&result, &slice("slice-001")).unwrap_err();
        assert!(err.to_string().contains("missing acceptance evidence"));

        let result = WorkerResult {
            slice_id: "slice-001".to_string(),
            status: "complete".to_string(),
            summary: "done".to_string(),
            acceptance_status: vec![AcceptanceEvidence {
                criterion: "done".to_string(),
                status: "satisfied".to_string(),
                evidence: "implemented".to_string(),
            }],
            ..WorkerResult::default()
        };
        validate_worker_result(&result, &slice("slice-001")).unwrap();
    }

    #[test]
    fn rejects_unknown_worker_and_repair_statuses() {
        let worker = WorkerResult {
            slice_id: "slice-001".to_string(),
            status: "done".to_string(),
            summary: "done".to_string(),
            ..WorkerResult::default()
        };
        assert!(validate_worker_result(&worker, &slice("slice-001")).is_err());

        let repair = RepairResult {
            status: "ok".to_string(),
            summary: "done".to_string(),
            ..RepairResult::default()
        };
        assert!(validate_repair_result(&repair).is_err());
    }

    #[test]
    fn finding_disposition_rejects_successful_unresolved_actionable_findings() {
        let actionable = Finding {
            id: "needs-followup".to_string(),
            severity: "warning".to_string(),
            action: "ask-user".to_string(),
            file: "src/lib.rs".to_string(),
            line: 0,
            description: "needs a follow-up decision".to_string(),
        };
        let mut worker = WorkerResult {
            slice_id: "slice-001".to_string(),
            status: "complete".to_string(),
            summary: "done".to_string(),
            acceptance_status: vec![AcceptanceEvidence {
                criterion: "done".to_string(),
                status: "satisfied".to_string(),
                evidence: "implemented".to_string(),
            }],
            findings: vec![actionable.clone()],
            ..WorkerResult::default()
        };
        let err = validate_worker_result(&worker, &slice("slice-001")).unwrap_err();
        assert!(err.to_string().contains("requires a terminal disposition"));

        worker.finding_dispositions = vec![FindingDisposition {
            finding_id: "needs-followup".to_string(),
            disposition: "proposed".to_string(),
            rationale: "operator should decide the follow-up".to_string(),
            ..FindingDisposition::default()
        }];
        validate_worker_result(&worker, &slice("slice-001")).unwrap();

        let repair = RepairResult {
            status: "fixed".to_string(),
            summary: "repair done".to_string(),
            findings: vec![actionable],
            ..RepairResult::default()
        };
        let err = validate_repair_result(&repair).unwrap_err();
        assert!(err.to_string().contains("requires a terminal disposition"));
    }

    #[test]
    fn replan_followup_slice_draft_validation_rejects_bad_area_empty_acceptance_duplicate_and_cycle()
     {
        let existing = vec![slice("slice-001")];
        validate_followup_slice_draft(&followup_draft("slice-001-followup"), &existing).unwrap();

        let mut bad_area = followup_draft("slice-bad-area");
        bad_area.areas = vec!["src/**".to_string()];
        assert!(
            format!(
                "{:#}",
                validate_followup_slice_draft(&bad_area, &existing).unwrap_err()
            )
            .contains("glob")
        );

        let mut empty_acceptance = followup_draft("slice-empty-acceptance");
        empty_acceptance.acceptance.clear();
        assert!(
            format!(
                "{:#}",
                validate_followup_slice_draft(&empty_acceptance, &existing).unwrap_err()
            )
            .contains("acceptance")
        );

        let mut duplicate_open = followup_draft("slice-001");
        duplicate_open.depends_on.clear();
        assert!(
            format!(
                "{:#}",
                validate_followup_slice_draft(&duplicate_open, &existing).unwrap_err()
            )
            .contains("duplicate slice id")
        );

        let mut closed = slice("slice-closed");
        closed.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        closed.closed_by_run = "kd-old".to_string();
        closed.closed_at = "2026-07-09T15:00:00Z".to_string();
        let mut duplicate_closed = followup_draft("slice-closed");
        duplicate_closed.depends_on.clear();
        assert!(
            format!(
                "{:#}",
                validate_followup_slice_draft(&duplicate_closed, &[closed]).unwrap_err()
            )
            .contains("duplicate slice id")
        );

        let mut self_cycle = followup_draft("slice-self-cycle");
        self_cycle.depends_on = vec!["slice-self-cycle".to_string()];
        assert!(
            format!(
                "{:#}",
                validate_followup_slice_draft(&self_cycle, &existing).unwrap_err()
            )
            .contains("depend on itself")
        );

        let mut bad_id = followup_draft("../bad");
        bad_id.depends_on.clear();
        assert!(
            format!(
                "{:#}",
                validate_followup_slice_draft(&bad_id, &existing).unwrap_err()
            )
            .contains("path/ref safe")
        );
    }

    #[test]
    fn replan_candidate_followup_worker_output_creates_typed_pending_proposal_and_warning()
    -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        artifact::write_json(store.slices_dir().join("slice-001.json"), &parent)?;
        let before_slice_json = fs::read_to_string(store.slice_path("slice-001"))?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths.clone(), state.clone(), Arc::new(FakeRunner));
        let run = Run {
            id: "kd-candidate".to_string(),
            repo_id: crate::paths::repo_id(repo.path()),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-candidate/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        state.insert_run(&run)?;
        store.ensure_run_dirs(&run.id)?;
        let output_path = store.output_path(&run.id, "slice-001.worker.attempt-1.json");
        let mut invalid = followup_draft("slice-bad-area");
        invalid.areas = vec!["src/**".to_string()];
        let mut result = WorkerResult {
            slice_id: "slice-001".to_string(),
            status: "complete".to_string(),
            summary: "worker completed and proposed follow-ups".to_string(),
            acceptance_status: vec![AcceptanceEvidence {
                criterion: "done".to_string(),
                status: "satisfied".to_string(),
                evidence: "implemented".to_string(),
            }],
            findings: vec![Finding {
                id: "needs-slice-001-followup".to_string(),
                severity: "warning".to_string(),
                action: "ask-user".to_string(),
                file: String::new(),
                line: 0,
                description: "Create slice-001-followup".to_string(),
            }],
            finding_dispositions: vec![FindingDisposition {
                finding_id: "needs-slice-001-followup".to_string(),
                disposition: "proposed".to_string(),
                rationale: "Create slice-001-followup from candidate draft".to_string(),
                ..FindingDisposition::default()
            }],
            candidate_followup_slices: vec![
                followup_draft("slice-001-followup"),
                invalid,
                followup_draft("slice-001-followup"),
            ],
            ..WorkerResult::default()
        };

        assert!(manager.create_worker_candidate_followup_slice_proposals(
            &run,
            &parent,
            1,
            &output_path,
            &mut result,
        )?);
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.id == "candidate-followup-slice-2"
                    && finding.severity == "warning"
                    && finding.description.contains("glob"))
        );
        assert!(
            result
                .findings
                .iter()
                .any(|finding| finding.id == "candidate-followup-slice-3"
                    && finding.severity == "warning"
                    && finding.description.contains("duplicate slice id"))
        );
        assert!(
            !result.finding_dispositions[0]
                .replan_proposal_id
                .trim()
                .is_empty()
        );

        manager.create_worker_finding_replan_proposals(
            &run,
            &parent,
            1,
            &output_path,
            &mut result,
        )?;
        let pending = state.pending_replan_proposals(&run.id)?;
        assert_eq!(pending.len(), 1);
        let proposal = &pending[0];
        assert_eq!(proposal.proposed_changes[0].kind, "add_followup_slice");
        assert_eq!(proposal.proposed_changes[0].target, "slice-001-followup");
        assert_eq!(
            proposal.proposed_changes[0]
                .followup_slice_draft()
                .as_ref()
                .unwrap()
                .rationale,
            "Worker found a bounded follow-up."
        );
        assert!(
            proposal
                .evidence
                .iter()
                .any(|link| link.kind == "worker_output")
        );
        assert!(
            proposal
                .evidence
                .iter()
                .any(|link| link.kind == "worker_attempt")
        );
        assert_eq!(
            fs::read_to_string(store.slice_path("slice-001"))?,
            before_slice_json
        );
        assert!(!store.slice_path("slice-001-followup").exists());

        let model =
            RunReadModelBuilder::new(&state).snapshot(&run, RunReadModelOptions::status(20))?;
        let feed_text = model
            .details
            .feed
            .as_ref()
            .unwrap()
            .attention
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(feed_text.contains("Proposed follow-up slice: slice-001-followup"));
        assert!(feed_text.contains("khazad-doom replan accept"));
        Ok(())
    }

    #[test]
    fn frontier_shadow_classifies_without_mutating_slices_queue_or_decisions() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = test_run("kd-frontier-shadow", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        state.set_frontier_state(
            &run.id,
            Some(&mission_envelope()),
            Some(&FrontierBudgetState::default()),
        )?;
        create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        let before_slices = workflow_slices_snapshot(&store)?;
        let before_queue = run.selected_slice_id.clone();

        assert_eq!(
            manager.classify_pending_frontier_proposals_at_replan_checkpoint(&run, "test")?,
            1
        );

        let after = state.get_run(&run.id)?.expect("run exists");
        assert_eq!(after.selected_slice_id, before_queue);
        assert_eq!(workflow_slices_snapshot(&store)?, before_slices);
        let proposal = state.pending_replan_proposals(&run.id)?.remove(0);
        assert!(proposal.operator_decision.is_none());
        let classification = proposal
            .frontier_classification
            .as_ref()
            .expect("classification recorded");
        assert_eq!(classification.tier, "tier_1");
        assert_eq!(classification.autonomy_level, AutonomyLevel::Shadow);
        assert!(!classification.envelope_hash.trim().is_empty());
        assert_eq!(
            classification.budget_snapshot,
            FrontierBudgetState::default()
        );
        assert!(
            classification
                .reason_codes
                .contains(&"shadow_observation_only".to_string())
        );
        assert!(
            classification
                .reason_codes
                .contains(&"inside_allowed_areas".to_string())
        );

        let events = state.get_events(&run.id, 20)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.typ == "frontier_classified")
                .count(),
            1
        );
        let model =
            RunReadModelBuilder::new(&state).snapshot(&after, RunReadModelOptions::status(20))?;
        let feed = model.details.feed.as_ref().expect("feed projected");
        let attention_text = feed
            .attention
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(attention_text.contains("Frontier shadow"));
        assert!(attention_text.contains("would auto-promote"));
        let block_text = feed
            .blocks
            .iter()
            .flat_map(|block| block.lines.iter())
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(block_text.contains("frontier activity"));
        assert!(block_text.contains("would-have-promoted"));
        assert_eq!(model.plan_revisions.frontier.activity_status, "active");
        assert_eq!(model.details.frontier.activity_status, "active");
        assert!(
            model
                .plan_revisions
                .frontier
                .summary_line
                .contains("frontier activity")
        );
        Ok(())
    }

    #[test]
    fn frontier_off_shadow_promote_and_run_do_not_mutate_slices_or_queue() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        store.write_slice(&slice("slice-001"), true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let before_slices = workflow_slices_snapshot(&store)?;

        let cases = [
            ("off", AutonomyLevel::Off, false),
            ("shadow", AutonomyLevel::Shadow, true),
            ("promote", AutonomyLevel::Promote, true),
            ("run", AutonomyLevel::Run, true),
        ];
        for (label, level, should_classify) in cases {
            let run = test_run(&format!("kd-frontier-{label}"), repo.path(), "slice-001")?;
            state.insert_run(&run)?;
            let mut envelope = mission_envelope();
            envelope.autonomy_level = level;
            state.set_frontier_state(
                &run.id,
                Some(&envelope),
                Some(&FrontierBudgetState::default()),
            )?;
            create_followup_replan_proposal(
                &state,
                &run.id,
                "slice-001",
                followup_draft("slice-001-followup"),
            )?;

            let classified =
                manager.classify_pending_frontier_proposals_at_replan_checkpoint(&run, "test")?;
            assert_eq!(classified == 1, should_classify, "{label}");
            assert_eq!(workflow_slices_snapshot(&store)?, before_slices, "{label}");
            let after = state.get_run(&run.id)?.expect("run exists");
            assert_eq!(after.selected_slice_id, "slice-001", "{label}");
            let proposal = state.pending_replan_proposals(&run.id)?.remove(0);
            assert!(proposal.operator_decision.is_none(), "{label}");
            assert_eq!(
                proposal.frontier_classification.is_some(),
                should_classify,
                "{label}"
            );
            if let Some(classification) = proposal.frontier_classification {
                assert_eq!(classification.autonomy_level, level, "{label}");
                assert_eq!(
                    classification
                        .reason_codes
                        .contains(&"shadow_observation_only".to_string()),
                    level == AutonomyLevel::Shadow,
                    "{label}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn frontier_auto_accept_failure_leaves_no_classification_or_audit_evidence() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = test_run("kd-auto-accept-rollback", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Promote;
        state.set_frontier_state(
            &run.id,
            Some(&envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        let proposal = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        let mut worker_layers = VecDeque::new();
        let mut gate_slices = vec![parent];

        crate::state::inject_decision_transaction_fault(
            crate::state::DecisionTransactionFaultStage::BeforeEventAppend,
        );
        assert!(
            manager
                .auto_accept_frontier_proposals_at_replan_checkpoint(
                    &mut run,
                    "test",
                    repo.path(),
                    &mut worker_layers,
                    &mut gate_slices,
                )
                .is_err()
        );

        let proposal = state
            .get_replan_proposal(&run.id, &proposal.id)?
            .expect("proposal remains durable");
        assert_eq!(proposal.state, ReplanProposalState::Pending);
        assert!(proposal.frontier_classification.is_none());
        let (_, budget) = state.get_frontier_state(&run.id)?;
        assert_eq!(budget, Some(FrontierBudgetState::default()));
        let events = state.get_events(&run.id, 50)?;
        for event_type in [
            workflow_events::FRONTIER_CLASSIFIED,
            workflow_events::REPLAN_PROPOSAL_DECIDED,
            workflow_events::FRONTIER_AUTO_ACCEPT_RECORDED,
        ] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.typ == event_type)
                    .count(),
                0,
                "{event_type} must roll back with the failed auto-accept"
            );
        }
        assert!(!store.slice_path("slice-001-followup").exists());
        Ok(())
    }

    #[test]
    fn frontier_auto_accept_promote_generates_slice_without_running_it() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = test_run("kd-auto-accept-promote", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Promote;
        state.set_frontier_state(
            &run.id,
            Some(&envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        let mut worker_layers = VecDeque::new();
        let mut gate_slices = vec![parent.clone()];

        assert_eq!(
            manager.auto_accept_frontier_proposals_at_replan_checkpoint(
                &mut run,
                "test",
                repo.path(),
                &mut worker_layers,
                &mut gate_slices,
            )?,
            1
        );

        assert!(store.slice_path("slice-001-followup").exists());
        let generated: Slice = artifact::read_json(store.slice_path("slice-001-followup"))?;
        let provenance = generated.provenance().expect("generated provenance");
        assert_eq!(provenance.created_by, "worker+daemon");
        assert_eq!(provenance.parent_slice_id, "slice-001");
        assert!(worker_layers.is_empty());
        assert_eq!(gate_slices.len(), 1);
        assert_eq!(gate_slices[0].id, parent.id);
        assert_eq!(run.selected_slice_id, "slice-001");
        assert!(
            !state
                .get_slice_runs(&run.id)?
                .iter()
                .any(|slice_run| slice_run.slice_id == "slice-001-followup")
        );
        let (_, budget) = state.get_frontier_state(&run.id)?;
        let budget = budget.expect("budget recorded");
        assert_eq!(budget.auto_promotions_used, 1);
        assert_eq!(budget.generated_slices, 1);
        let proposals = state.list_replan_proposals(&run.id)?;
        let proposal = &proposals[0];
        assert_eq!(proposal.state, ReplanProposalState::Accepted);
        let decision = proposal
            .operator_decision
            .as_ref()
            .expect("decision recorded");
        assert_eq!(decision.authorizer, format!("envelope:{}", run.id));
        assert_eq!(decision.source, "frontier_policy");
        assert_eq!(decision.frontier_tier, "tier_1");
        assert!(
            decision
                .frontier_reason_codes
                .contains(&"within_budget".to_string())
        );
        assert!(decision.frontier_budget_before.is_some());
        assert!(decision.frontier_budget_after.is_some());
        assert!(decision.applied);
        assert_eq!(decision.queue_after, vec!["slice-001".to_string()]);
        assert!(decision.apply_reason.contains("future run"));
        assert!(!decision.generated_slice_commit.trim().is_empty());
        let events = state.get_events(&run.id, 50)?;
        assert!(
            events
                .iter()
                .any(|event| event.typ == "frontier_auto_accept_recorded")
        );
        let classification_event = events
            .iter()
            .find(|event| event.typ == workflow_events::FRONTIER_CLASSIFIED)
            .expect("classification audit committed with auto-accept");
        assert_eq!(classification_event.payload["checkpoint"], "test");
        assert_eq!(classification_event.payload["decision_recorded"], true);
        Ok(())
    }

    #[test]
    fn frontier_auto_accept_run_appends_generated_slice_serially() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = test_run("kd-auto-accept-run", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Run;
        state.set_frontier_state(
            &run.id,
            Some(&envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        let mut worker_layers = VecDeque::new();
        let mut gate_slices = vec![parent];

        assert_eq!(
            manager.auto_accept_frontier_proposals_at_replan_checkpoint(
                &mut run,
                "test",
                repo.path(),
                &mut worker_layers,
                &mut gate_slices,
            )?,
            1
        );

        assert_eq!(run.selected_slice_id, "slice-001,slice-001-followup");
        assert_eq!(worker_layers.len(), 1);
        assert_eq!(worker_layers[0][0].id, "slice-001-followup");
        assert!(
            gate_slices
                .iter()
                .any(|slice| slice.id == "slice-001-followup")
        );
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert!(slice_runs.iter().any(|slice_run| {
            slice_run.slice_id == "slice-001-followup" && slice_run.status == SliceStatus::Pending
        }));
        let proposal = state.list_replan_proposals(&run.id)?.remove(0);
        let decision = proposal.operator_decision.expect("decision recorded");
        assert_eq!(
            decision.queue_after,
            vec!["slice-001".to_string(), "slice-001-followup".to_string()]
        );
        assert!(decision.apply_reason.contains("appended serially"));
        let events = state.get_events(&run.id, 50)?;
        let promoted = events
            .iter()
            .find(|event| event.typ == "frontier_slice_promoted")
            .expect("promotion event");
        assert_eq!(promoted.payload["apply_mode"], "append_and_run");
        assert_eq!(promoted.payload["serial_append"], true);
        Ok(())
    }

    #[test]
    fn frontier_auto_accept_run_level_e2e_executes_generated_slice() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut parent = slice("slice-001");
        parent.areas = vec!["src/".to_string()];
        parent.verify = vec!["test -f src/slice-001.txt".to_string()];
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths.clone(),
            state.clone(),
            Arc::new(FollowupEmittingRunner),
        );
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Run;
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: true,
            origin_notification_target: String::new(),
            mission_envelope: Some(envelope),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        assert_eq!(state.pending_replan_proposals(&run.id)?.len(), 0);
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert!(slice_runs.iter().any(|slice_run| {
            slice_run.slice_id == "slice-001-followup" && slice_run.status == SliceStatus::Merged
        }));
        let proposal = state.list_replan_proposals(&run.id)?.remove(0);
        assert_eq!(proposal.state, ReplanProposalState::Accepted);
        let decision = proposal.operator_decision.expect("auto decision recorded");
        assert_eq!(decision.authorizer, format!("envelope:{}", run.id));
        assert_eq!(decision.source, "frontier_policy");
        assert!(decision.applied);
        assert_eq!(
            decision.queue_after,
            vec!["slice-001".to_string(), "slice-001-followup".to_string()]
        );
        let (_, budget) = state.get_frontier_state(&run.id)?;
        let budget = budget.expect("budget recorded");
        assert_eq!(budget.auto_promotions_used, 1);
        assert_eq!(budget.generated_slices, 1);
        let final_summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "implementation-summary.json"))?;
        assert!(
            final_summary
                .completed_slices
                .iter()
                .any(|result| result.slice_id == "slice-001-followup")
        );
        let model = RunReadModelBuilder::new(&state)
            .snapshot(&completed, RunReadModelOptions::status(20))?;
        assert!(
            model
                .details
                .generated_slices
                .iter()
                .any(|slice| slice.slice_id == "slice-001-followup")
        );
        Ok(())
    }

    #[test]
    fn frontier_auto_accept_respects_envelope_and_stop_boundaries() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        store.write_slice(&slice("slice-001"), true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));

        let mut no_envelope_run = test_run("kd-auto-accept-no-envelope", repo.path(), "slice-001")?;
        state.insert_run(&no_envelope_run)?;
        create_followup_replan_proposal(
            &state,
            &no_envelope_run.id,
            "slice-001",
            followup_draft("slice-001-followup-no-envelope"),
        )?;
        assert_eq!(
            manager.auto_accept_frontier_proposals_at_replan_checkpoint(
                &mut no_envelope_run,
                "test",
                repo.path(),
                &mut VecDeque::new(),
                &mut vec![slice("slice-001")],
            )?,
            0
        );
        assert_eq!(
            state.pending_replan_proposals(&no_envelope_run.id)?.len(),
            1
        );

        let mut unsupported_run = test_run("kd-auto-accept-unsupported", repo.path(), "slice-001")?;
        state.insert_run(&unsupported_run)?;
        let mut unsupported_envelope = mission_envelope();
        unsupported_envelope.autonomy_level = AutonomyLevel::Run;
        state.set_frontier_state(
            &unsupported_run.id,
            Some(&unsupported_envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        create_test_replan_proposal(&state, &unsupported_run.id, "slice-001", "unsupported")?;
        assert_eq!(
            manager.auto_accept_frontier_proposals_at_replan_checkpoint(
                &mut unsupported_run,
                "test",
                repo.path(),
                &mut VecDeque::new(),
                &mut vec![slice("slice-001")],
            )?,
            0
        );
        let unsupported = state
            .pending_replan_proposals(&unsupported_run.id)?
            .remove(0);
        assert!(unsupported.operator_decision.is_none());
        assert!(unsupported.frontier_classification.is_none());

        let mut budget_stop_run = test_run("kd-auto-accept-budget-stop", repo.path(), "slice-001")?;
        state.insert_run(&budget_stop_run)?;
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Run;
        envelope.max_auto_promotions = 0;
        state.set_frontier_state(
            &budget_stop_run.id,
            Some(&envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        create_followup_replan_proposal(
            &state,
            &budget_stop_run.id,
            "slice-001",
            followup_draft("slice-001-followup-budget"),
        )?;
        assert_eq!(
            manager.auto_accept_frontier_proposals_at_replan_checkpoint(
                &mut budget_stop_run,
                "test",
                repo.path(),
                &mut VecDeque::new(),
                &mut vec![slice("slice-001")],
            )?,
            0
        );
        let proposal = state
            .pending_replan_proposals(&budget_stop_run.id)?
            .remove(0);
        let classification = proposal
            .frontier_classification
            .as_ref()
            .expect("stop classification");
        assert_eq!(classification.tier, "stop");
        assert!(
            classification
                .reason_codes
                .contains(&"frontier_budget_exhausted".to_string())
        );
        let err = manager
            .block_if_pending_replan(&budget_stop_run, "test")
            .expect_err("pending frontier stop blocks for operator");
        assert!(format!("{err:#}").contains("frontier stop"));
        let (_, budget) = state.get_frontier_state(&budget_stop_run.id)?;
        let budget = budget.expect("budget recorded");
        assert_eq!(budget.auto_promotions_used, 0);
        assert_eq!(budget.generated_slices, 0);
        let events = state.get_events(&budget_stop_run.id, 50)?;
        assert!(
            events
                .iter()
                .any(|event| event.typ == "frontier_auto_accept_stopped")
        );

        let mut depth_stop_run = test_run("kd-auto-accept-depth-stop", repo.path(), "slice-001")?;
        state.insert_run(&depth_stop_run)?;
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Run;
        envelope.max_depth = 0;
        state.set_frontier_state(
            &depth_stop_run.id,
            Some(&envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        create_followup_replan_proposal(
            &state,
            &depth_stop_run.id,
            "slice-001",
            followup_draft("slice-001-followup-depth"),
        )?;
        assert_eq!(
            manager.auto_accept_frontier_proposals_at_replan_checkpoint(
                &mut depth_stop_run,
                "test",
                repo.path(),
                &mut VecDeque::new(),
                &mut vec![slice("slice-001")],
            )?,
            0
        );
        let depth_proposal = state
            .pending_replan_proposals(&depth_stop_run.id)?
            .remove(0);
        let depth_classification = depth_proposal
            .frontier_classification
            .as_ref()
            .expect("depth stop classification");
        assert_eq!(depth_classification.tier, "stop");
        assert!(
            depth_classification
                .reason_codes
                .contains(&"frontier_depth_exhausted".to_string())
        );
        Ok(())
    }

    #[test]
    fn frontier_auto_accept_resume_apply_is_idempotent_and_does_not_double_spend_budget()
    -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = test_run("kd-auto-accept-resume", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let mut envelope = mission_envelope();
        envelope.autonomy_level = AutonomyLevel::Run;
        state.set_frontier_state(
            &run.id,
            Some(&envelope),
            Some(&FrontierBudgetState::default()),
        )?;
        let pending = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        let budget_before = FrontierBudgetState::default();
        let budget_after = FrontierBudgetState {
            auto_promotions_used: 1,
            generated_slices: 1,
            ..FrontierBudgetState::default()
        };
        let classification = FrontierClassification {
            tier: "tier_1".to_string(),
            reason_codes: vec![
                "add_followup_slice_only".to_string(),
                "inside_allowed_areas".to_string(),
                "acceptance_present".to_string(),
                "verify_present".to_string(),
                "within_budget".to_string(),
                "within_depth".to_string(),
                "not_duplicate".to_string(),
            ],
            classified_at: Utc::now(),
            envelope_hash: "test-envelope".to_string(),
            budget_snapshot: budget_before.clone(),
            autonomy_level: AutonomyLevel::Run,
        };
        state.auto_accept_replan_proposal_with_budget(
            &run.id,
            &pending.id,
            "frontier policy accepted before crash",
            &classification,
            &budget_before,
            &budget_after,
            "before-crash",
            "append_and_run",
        )?;
        let mut worker_layers = VecDeque::new();
        let mut gate_slices = vec![parent];

        assert_eq!(
            manager.apply_accepted_replan_proposals_at_checkpoint(
                &mut run,
                "resume",
                repo.path(),
                &mut worker_layers,
                &mut gate_slices,
            )?,
            1
        );
        assert_eq!(
            manager.apply_accepted_replan_proposals_at_checkpoint(
                &mut run,
                "resume-again",
                repo.path(),
                &mut worker_layers,
                &mut gate_slices,
            )?,
            0
        );
        let (_, budget) = state.get_frontier_state(&run.id)?;
        let budget = budget.expect("budget recorded");
        assert_eq!(budget.auto_promotions_used, 1);
        assert_eq!(budget.generated_slices, 1);
        assert_eq!(worker_layers.len(), 1);
        assert_eq!(
            gate_slices
                .iter()
                .filter(|slice| slice.id == "slice-001-followup")
                .count(),
            1
        );
        let proposal = state.list_replan_proposals(&run.id)?.remove(0);
        let decision = proposal.operator_decision.expect("decision recorded");
        assert!(decision.applied);
        assert_eq!(decision.frontier_budget_before, Some(budget_before));
        assert_eq!(decision.frontier_budget_after, Some(budget_after));
        Ok(())
    }

    #[test]
    fn frontier_shadow_reclassification_survives_restart_and_preserves_event_history() -> Result<()>
    {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        store.write_slice(&slice("slice-001"), true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths.clone(), state.clone(), Arc::new(FakeRunner));
        let run = test_run("kd-frontier-restart", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        state.set_frontier_state(
            &run.id,
            Some(&mission_envelope()),
            Some(&FrontierBudgetState::default()),
        )?;
        let proposal = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        assert_eq!(
            manager.classify_pending_frontier_proposals_at_replan_checkpoint(
                &run,
                "worker dispatch"
            )?,
            1
        );
        let first = state
            .get_replan_proposal(&run.id, &proposal.id)?
            .unwrap()
            .frontier_classification
            .expect("first classification");

        thread::sleep(Duration::from_millis(2));
        let reopened = StateStore::open(paths.db_file())?;
        let restarted = Manager::with_runner(paths, reopened.clone(), Arc::new(FakeRunner));
        let run_after_restart = reopened.get_run(&run.id)?.expect("run persisted");
        assert_eq!(
            restarted.classify_pending_frontier_proposals_at_replan_checkpoint(
                &run_after_restart,
                "resume"
            )?,
            1
        );
        let latest = reopened
            .get_replan_proposal(&run.id, &proposal.id)?
            .unwrap()
            .frontier_classification
            .expect("latest classification");
        assert_eq!(latest.envelope_hash, first.envelope_hash);
        assert!(latest.classified_at >= first.classified_at);
        let events = reopened.get_events(&run.id, 20)?;
        let frontier_events = events
            .iter()
            .filter(|event| event.typ == "frontier_classified")
            .collect::<Vec<_>>();
        assert_eq!(frontier_events.len(), 2);
        assert_eq!(frontier_events[0].payload["checkpoint"], "worker dispatch");
        assert_eq!(frontier_events[1].payload["checkpoint"], "resume");
        Ok(())
    }

    #[test]
    fn replan_apply_accepted_followup_slice_is_idempotent_and_records_snapshots() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = Run {
            id: "kd-apply-idempotent".to_string(),
            repo_id: crate::paths::repo_id(repo.path()),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-apply-idempotent/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        state.insert_run(&run)?;
        state.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Merged,
            branch: "khazad/kd-apply-idempotent/slice-001".to_string(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        let draft = followup_draft("slice-001-followup");
        let proposal = create_followup_replan_proposal(&state, &run.id, "slice-001", draft)?;
        let proposal = state.decide_replan_proposal(
            &run.id,
            &proposal.id,
            ReplanProposalState::Accepted,
            "operator accepted follow-up",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;
        assert_eq!(
            proposal.operator_decision.as_ref().unwrap().apply_status,
            "pending"
        );

        let mut layers = VecDeque::new();
        let mut gate_slices = vec![parent.clone()];
        assert_eq!(
            manager.apply_accepted_replan_proposals_at_checkpoint(
                &mut run,
                "resume",
                repo.path(),
                &mut layers,
                &mut gate_slices,
            )?,
            1
        );
        assert_eq!(run.selected_slice_id, "slice-001,slice-001-followup");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0][0].id, "slice-001-followup");
        assert_eq!(gate_slices.len(), 2);
        let generated: Slice = artifact::read_json(store.slice_path("slice-001-followup"))?;
        let provenance = generated.provenance().expect("generated provenance");
        assert_eq!(provenance.parent_slice_id, "slice-001");
        assert_eq!(provenance.origin_proposal_id, proposal.id);
        assert_eq!(provenance.generation, 1);
        assert!(
            gitutil::run(
                repo.path(),
                &[
                    "status",
                    "--porcelain",
                    "--",
                    ".workflow/slices/slice-001-followup.json"
                ],
            )?
            .trim()
            .is_empty()
        );
        let head_after_apply = gitutil::head_sha(repo.path())?;
        let applied = state
            .get_replan_proposal(&run.id, &proposal.id)?
            .unwrap()
            .operator_decision
            .unwrap();
        assert!(applied.applied);
        assert_eq!(applied.apply_status, "applied");
        assert_eq!(applied.generated_slice_id, "slice-001-followup");
        assert_eq!(applied.generated_slice_commit, head_after_apply);
        assert_eq!(applied.queue_before, vec!["slice-001".to_string()]);
        assert_eq!(
            applied.queue_after,
            vec!["slice-001".to_string(), "slice-001-followup".to_string()]
        );
        assert!(!applied.queue_before_hash.is_empty());
        assert!(!applied.queue_after_hash.is_empty());

        assert_eq!(
            manager.apply_accepted_replan_proposals_at_checkpoint(
                &mut run,
                "resume",
                repo.path(),
                &mut layers,
                &mut gate_slices,
            )?,
            0
        );
        assert_eq!(run.selected_slice_id, "slice-001,slice-001-followup");
        assert_eq!(gitutil::head_sha(repo.path())?, head_after_apply);
        Ok(())
    }

    #[test]
    fn replan_apply_resume_commit_before_queue_extension_does_not_duplicate() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = Run {
            id: "kd-apply-resume".to_string(),
            repo_id: crate::paths::repo_id(repo.path()),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Interrupted,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-apply-resume/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        state.insert_run(&run)?;
        let draft = followup_draft("slice-001-followup");
        let proposal =
            create_followup_replan_proposal(&state, &run.id, "slice-001", draft.clone())?;
        let proposal = state.decide_replan_proposal(
            &run.id,
            &proposal.id,
            ReplanProposalState::Accepted,
            "operator accepted before crash",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;
        let mut generated = draft.to_slice();
        generated.set_provenance(SliceProvenance {
            parent_slice_id: "slice-001".to_string(),
            origin_proposal_id: proposal.id.clone(),
            generation: 1,
            created_by: "operator".to_string(),
            created_at: Utc::now().to_rfc3339(),
        });
        artifact::write_json(store.slice_path("slice-001-followup"), &generated)?;
        gitutil::commit_paths(
            repo.path(),
            &[".workflow/slices/slice-001-followup.json"],
            "khazad(slice:slice-001-followup): promote follow-up from slice-001 via crash",
        )?;
        let commit_count_before = gitutil::run(repo.path(), &["rev-list", "--count", "HEAD"])?;

        let mut layers = VecDeque::new();
        let mut gate_slices = vec![parent];
        assert_eq!(
            manager.apply_accepted_replan_proposals_at_checkpoint(
                &mut run,
                "resume",
                repo.path(),
                &mut layers,
                &mut gate_slices,
            )?,
            1
        );
        assert_eq!(
            commit_count_before,
            gitutil::run(repo.path(), &["rev-list", "--count", "HEAD"])?
        );
        assert_eq!(run.selected_slice_id, "slice-001,slice-001-followup");
        assert_eq!(layers.len(), 1);
        assert_eq!(
            selected_slice_ids(&run.selected_slice_id)
                .into_iter()
                .filter(|id| id == "slice-001-followup")
                .count(),
            1
        );
        let applied = state
            .get_replan_proposal(&run.id, &proposal.id)?
            .unwrap()
            .operator_decision
            .unwrap();
        assert!(applied.applied);
        assert_eq!(applied.apply_status, "applied");
        assert_eq!(
            applied.queue_after_hash,
            queue_snapshot_hash(&applied.queue_after)
        );
        Ok(())
    }

    #[test]
    fn replan_apply_resume_keeps_completed_generated_slices_in_gate_set() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        let mut generated = followup_draft("slice-001-followup").to_slice();
        generated.set_provenance(SliceProvenance {
            parent_slice_id: "slice-001".to_string(),
            origin_proposal_id: "proposal-1".to_string(),
            generation: 1,
            created_by: "operator".to_string(),
            created_at: Utc::now().to_rfc3339(),
        });
        store.write_slice(&generated, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state, Arc::new(FakeRunner));
        let run = Run {
            id: "kd-apply-gate-resume".to_string(),
            repo_id: crate::paths::repo_id(repo.path()),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-apply-gate-resume/integration".to_string(),
            selected_slice_id: "slice-001,slice-001-followup".to_string(),
            error: String::new(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let mut gate_slices = vec![parent];
        let completed_ids =
            BTreeSet::from(["slice-001".to_string(), "slice-001-followup".to_string()]);
        let layers = manager.initial_worker_layers(
            &run,
            &[],
            &mut gate_slices,
            &completed_ids,
            repo.path(),
        )?;
        assert!(layers.is_empty());
        assert!(
            gate_slices
                .iter()
                .any(|slice| slice.id == "slice-001-followup")
        );
        Ok(())
    }

    #[test]
    fn replan_apply_rejected_deferred_nonfollowup_and_refused_proposals_stay_unapplied()
    -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let parent = slice("slice-001");
        store.write_slice(&parent, true)?;
        store.write_slice(&slice("slice-001-followup"), true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let mut run = Run {
            id: "kd-apply-unapplied".to_string(),
            repo_id: crate::paths::repo_id(repo.path()),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-apply-unapplied/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        };
        state.insert_run(&run)?;
        let rejected = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-rejected"),
        )?;
        state.decide_replan_proposal(
            &run.id,
            &rejected.id,
            ReplanProposalState::Rejected,
            "duplicate",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;
        let deferred = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-deferred"),
        )?;
        state.decide_replan_proposal(
            &run.id,
            &deferred.id,
            ReplanProposalState::Deferred,
            "not now",
            "operator",
            "daemon_ipc",
            "",
            "after release",
        )?;
        let nonfollowup = state.create_replan_proposal(
            &run.id,
            "",
            ReplanProposalSource {
                kind: "worker_finding".to_string(),
                slice_id: "slice-001".to_string(),
                phase: "test".to_string(),
                attempt: 1,
                summary: "change queue".to_string(),
            },
            Vec::new(),
            Vec::new(),
            vec![ReplanProposedChange {
                kind: "queue_revision".to_string(),
                target: "slice-001".to_string(),
                summary: "not a follow-up draft".to_string(),
            }],
            "operator_review",
        )?;
        state.decide_replan_proposal(
            &run.id,
            &nonfollowup.id,
            ReplanProposalState::Accepted,
            "accepted prose-only change",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;
        let refused = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-followup"),
        )?;
        state.decide_replan_proposal(
            &run.id,
            &refused.id,
            ReplanProposalState::Accepted,
            "accepted colliding follow-up",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;

        let mut layers = VecDeque::new();
        let mut gate_slices = vec![parent];
        assert_eq!(
            manager.apply_accepted_replan_proposals_at_checkpoint(
                &mut run,
                "resume",
                repo.path(),
                &mut layers,
                &mut gate_slices,
            )?,
            0
        );
        assert_eq!(run.selected_slice_id, "slice-001");
        assert!(!store.slice_path("slice-rejected").exists());
        assert!(!store.slice_path("slice-deferred").exists());
        let refused_decision = state
            .get_replan_proposal(&run.id, &refused.id)?
            .unwrap()
            .operator_decision
            .unwrap();
        assert_eq!(refused_decision.apply_status, "refused");
        assert!(refused_decision.apply_reason.contains("already exists"));
        let nonfollowup_decision = state
            .get_replan_proposal(&run.id, &nonfollowup.id)?
            .unwrap()
            .operator_decision
            .unwrap();
        assert_eq!(nonfollowup_decision.apply_status, "not_applicable");
        Ok(())
    }

    #[test]
    fn replan_apply_e2e_operator_accepted_followup_runs_after_resume() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut parent = slice("slice-001");
        parent.areas = vec!["src/".to_string()];
        parent.verify = vec!["test -f src/slice-001.txt".to_string()];
        store.write_slice(&parent, true)?;
        gitutil::commit_all(repo.path(), "workflow fixture")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths.clone(),
            state.clone(),
            Arc::new(FollowupEmittingRunner),
        );
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: true,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;
        let blocked = wait_for_run(&state, &run.id)?;
        assert_eq!(blocked.status, RunStatus::Blocked);
        let pending = state.pending_replan_proposals(&run.id)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].proposed_changes[0].kind, "add_followup_slice");
        state.decide_replan_proposal(
            &run.id,
            &pending[0].id,
            ReplanProposalState::Accepted,
            "operator accepted generated follow-up",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;

        manager.resume_run(ResumeOptions {
            run_id: run.id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;
        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert!(slice_runs.iter().any(|slice_run| {
            slice_run.slice_id == "slice-001-followup" && slice_run.status == SliceStatus::Merged
        }));
        let proposal = state.get_replan_proposal(&run.id, &pending[0].id)?.unwrap();
        let decision = proposal.operator_decision.unwrap();
        assert!(decision.applied);
        assert_eq!(decision.apply_status, "applied");
        assert_eq!(
            decision.queue_after,
            vec!["slice-001".to_string(), "slice-001-followup".to_string()]
        );
        let generated_path = store.slice_path("slice-001-followup");
        assert!(
            !generated_path.exists(),
            "source worktree remains unchanged until handoff"
        );
        let final_summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "implementation-summary.json"))?;
        assert!(
            final_summary
                .completed_slices
                .iter()
                .any(|result| result.slice_id == "slice-001-followup")
        );
        Ok(())
    }

    #[test]
    fn repo_local_agents_toml_is_not_a_worker_profile_input() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        fs::write(
            store.workflow_dir().join("agents.toml"),
            "this is intentionally not profile toml",
        )?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::new(paths, state);

        let runner = manager.runner_for_parts("pi", "pi", &[], &WorkflowConfig::default())?;

        let metadata = runner.metadata();
        assert_eq!(metadata.provider, "openai-codex");
        assert_eq!(metadata.model, "gpt-5.5");
        assert_eq!(metadata.profile, "implementer");
        Ok(())
    }

    #[test]
    fn mission_envelope_validation_uses_area_contract() -> Result<()> {
        let config = WorkflowConfig::default();
        let valid = mission_envelope();
        validate_mission_envelope(Some(&valid), &config)?;

        let mut invalid = valid.clone();
        invalid.allowed_areas = vec!["src/*.rs".to_string()];
        let err = validate_mission_envelope(Some(&invalid), &config).unwrap_err();
        assert!(
            err.to_string()
                .contains("mission envelope allowed_areas[0] is invalid"),
            "{err:?}"
        );
        assert!(format!("{err:?}").contains("glob"), "{err:?}");

        let mut negative_budget = valid;
        negative_budget.max_auto_promotions = -1;
        let err = validate_mission_envelope(Some(&negative_budget), &config).unwrap_err();
        assert!(
            err.to_string()
                .contains("mission envelope max_auto_promotions must be >= 0"),
            "{err:?}"
        );
        Ok(())
    }

    #[test]
    fn run_start_records_mission_envelope_for_status_report_and_handoff() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        store.write_slice(&slice("slice-001"), true)?;
        gitutil::commit_all(repo.path(), "add mission slice")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let db_file = paths.db_file();
        let state = StateStore::open(&db_file)?;
        let manager = Manager::new(paths, state.clone());
        let envelope = mission_envelope();

        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: true,
            origin_notification_target: String::new(),
            mission_envelope: Some(envelope.clone()),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(
            completed.status,
            RunStatus::Completed,
            "{}",
            completed.error
        );
        let reopened_state = StateStore::open(&db_file)?;
        let (stored_envelope, stored_budget) = reopened_state.get_frontier_state(&run.id)?;
        assert_eq!(stored_envelope.as_ref(), Some(&envelope));
        assert_eq!(stored_budget, Some(FrontierBudgetState::default()));

        let builder = RunReadModelBuilder::new(&state);
        let model = builder.snapshot(&completed, RunReadModelOptions::status(20))?;
        assert_eq!(model.details.mission_envelope.as_ref(), Some(&envelope));
        assert_eq!(
            model.details.frontier_budget,
            Some(FrontierBudgetState::default())
        );
        assert_eq!(model.plan_revisions.frontier.activity_status, "empty");
        assert_eq!(model.details.frontier.activity_status, "empty");
        assert_eq!(
            model.plan_revisions.frontier.envelope_snapshot.as_ref(),
            Some(&envelope)
        );
        assert!(
            model
                .plan_revisions
                .frontier
                .summary_line
                .contains("none recorded")
        );
        let frontier_block = model
            .details
            .feed
            .as_ref()
            .unwrap()
            .blocks
            .iter()
            .find(|block| block.label == "Frontier")
            .expect("frontier empty-state block");
        assert_eq!(frontier_block.meta, "empty");
        let mission_block = model
            .details
            .feed
            .as_ref()
            .unwrap()
            .blocks
            .iter()
            .find(|block| block.label == "Mission")
            .expect("mission block");
        let mission_text = mission_block
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(mission_text.contains("Complete the bounded mission"));
        assert!(mission_text.contains(
            "autonomy shadow; classifier records observations only; queues, slices, and decisions stay operator-owned"
        ));

        let summary_path =
            ArtifactStore::new(repo.path()).output_path(&run.id, "implementation-summary.json");
        let summary = artifact::read_json::<ImplementationSummary>(&summary_path)?;
        assert_eq!(summary.mission_envelope.as_ref(), Some(&envelope));
        assert_eq!(
            summary.frontier_budget,
            Some(FrontierBudgetState::default())
        );
        assert_eq!(summary.plan_revisions.frontier.activity_status, "empty");
        assert!(
            summary
                .plan_revisions
                .frontier
                .empty_reason
                .contains("no frontier proposals")
        );

        let handoff = manager.branch_handoff(&run.id, false, false, false)?;
        assert_eq!(handoff.mission_envelope.as_ref(), Some(&envelope));
        assert_eq!(
            handoff.frontier_budget,
            Some(FrontierBudgetState::default())
        );
        assert_eq!(handoff.plan_revisions.frontier.activity_status, "empty");
        Ok(())
    }

    #[test]
    fn recovery_marks_stale_running_runs_interrupted() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-stale".to_string();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-stale/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        state.upsert_slice_run(&SliceRun {
            run_id: run_id.clone(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 0,
            last_error: String::new(),
        })?;
        let worktree_root = paths.repo_worktree_dir(&repo_id, &run_id);
        let allocated = state.allocate_worker_attempt(
            &run_id,
            "slice-001",
            1,
            1,
            0,
            0,
            "slice-worker",
            &worktree_root,
        )?;
        let running = state.allocate_worker_attempt(
            &run_id,
            "slice-002",
            1,
            1,
            0,
            0,
            "slice-worker",
            &worktree_root,
        )?;
        state.mark_worker_attempt_launched(running.launch_id)?;
        fs::create_dir_all(&worktree_root)?;
        let manager = Manager::with_runner(paths.clone(), state.clone(), Arc::new(FakeRunner));

        assert_eq!(manager.recover_interrupted_runs()?, 1);

        let recovered = state.get_run(&run_id)?.expect("run exists");
        assert_eq!(recovered.status, RunStatus::Interrupted);
        let slice_runs = state.get_slice_runs(&run_id)?;
        assert!(
            slice_runs
                .iter()
                .all(|slice_run| slice_run.status == SliceStatus::Interrupted)
        );
        for launch in [allocated, running] {
            let ledger = state.list_worker_attempt_ledger(&run_id, &launch.slice_id)?;
            assert_eq!(ledger[0].state, "interrupted");
            assert!(ledger[0].finished_at.is_some());
            assert!(ledger[0].failure_cause.contains("daemon restarted"));
        }
        assert!(!paths.repo_worktree_dir(&repo_id, &run_id).exists());
        let events = state.get_events(&run_id, 30)?;
        assert!(
            events
                .iter()
                .any(|event| event.typ == "daemon_recovery_completed")
        );
        Ok(())
    }

    #[test]
    fn terminalization_recovery_preserves_prepared_terminal_intent() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-prepared-cancel".to_string();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id,
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-prepared-cancel/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        state.upsert_slice_run(&SliceRun {
            run_id: run_id.clone(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        state.prepare_run_terminal_transition(
            &run_id,
            RunStatus::Cancelled,
            "operator cancelled before summary publication",
            "operator cancelled before summary publication",
            "run cancelled before question answer",
        )?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));

        manager.recover_interrupted_runs()?;

        let recovered = state.get_run(&run_id)?.expect("run exists");
        assert_eq!(recovered.status, RunStatus::Cancelled);
        assert_eq!(
            state
                .get_progress(&run_id)?
                .expect("terminal progress")
                .phase,
            "cancelled"
        );
        assert!(
            state
                .get_events(&run_id, 100)?
                .iter()
                .all(|event| event.typ != "daemon_recovery_completed"),
            "reconciliation must not replace a prepared terminal outcome with restart interruption"
        );
        Ok(())
    }

    #[test]
    fn terminalization_resume_reconciles_incomplete_intent_before_new_work() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run_id = "kd-resume-terminal-intent".to_string();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: crate::paths::repo_id(repo.path()),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-resume-terminal-intent/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        state.prepare_run_terminal_transition(
            &run_id,
            RunStatus::Cancelled,
            "operator cancelled before resume",
            "operator cancelled before resume",
            "run cancelled before question answer",
        )?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));

        let reconciled = manager.resume_run(ResumeOptions {
            run_id: run_id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;

        assert_eq!(reconciled.status, RunStatus::Cancelled);
        assert_eq!(reconciled.error, "operator cancelled before resume");
        let events = state.get_events(&run_id, 100)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.typ == super::workflow_events::RUN_CANCELLED)
                .count(),
            1
        );
        assert!(events.iter().all(|event| event.typ != "run_resumed"));
        assert!(
            artifact::Store::new(repo.path())
                .output_path(&run_id, "run-summary.json")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn terminalization_summary_failure_keeps_intent_retryable_and_nonterminal() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-summary-failure".to_string();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-summary-failure/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        let worktree_root = paths.repo_worktree_dir(&repo_id, &run_id);
        fs::create_dir_all(&worktree_root)?;
        fs::write(worktree_root.join("retry-evidence.txt"), "retain me\n")?;
        state.prepare_run_terminal_transition(
            &run_id,
            RunStatus::Failed,
            "gate persistence failed",
            "gate persistence failed",
            "run failed before question answer",
        )?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));

        super::inject_terminalization_fault(super::TerminalizationFaultStage::SummaryWrite);
        assert!(manager.recover_interrupted_runs().is_err());
        let pending = state.get_run(&run_id)?.expect("run persists");
        assert_eq!(pending.status, RunStatus::Running);
        assert!(
            worktree_root.exists(),
            "cleanup must not run before summary durability"
        );
        let transition = state
            .terminal_transition(&run_id)?
            .expect("durable retry intent");
        assert_eq!(transition.status, RunStatus::Failed);
        assert_eq!(transition.error, "gate persistence failed");
        assert!(!transition.summary_written);
        assert!(!transition.committed);
        assert!(
            state
                .get_events(&run_id, 100)?
                .iter()
                .all(|event| event.typ != "run_error")
        );

        assert_eq!(manager.recover_interrupted_runs()?, 1);
        assert_eq!(
            state.get_run(&run_id)?.expect("terminal run").status,
            RunStatus::Failed
        );
        assert_eq!(
            state
                .get_events(&run_id, 100)?
                .iter()
                .filter(|event| event.typ == "run_error")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn terminalization_notification_failure_is_non_authoritative_after_commit() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-notification-failure".to_string();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-notification-failure/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        fs::create_dir_all(paths.repo_worktree_dir(&repo_id, &run_id))?;
        state.prepare_run_terminal_transition(
            &run_id,
            RunStatus::Cancelled,
            "operator cancelled",
            "operator cancelled",
            "run cancelled before question answer",
        )?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));

        super::inject_terminalization_fault(super::TerminalizationFaultStage::Notification);
        assert_eq!(manager.recover_interrupted_runs()?, 1);

        let terminal = state.get_run(&run_id)?.expect("terminal run");
        assert_eq!(terminal.status, RunStatus::Cancelled);
        assert_eq!(terminal.error, "operator cancelled");
        assert!(state.terminal_notification_bookkept(&run_id)?);
        assert!(state.get_events(&run_id, 100)?.iter().any(|event| {
            event.typ == "run_incident"
                && event.payload["kind"] == "terminal_notification_bookkeeping_failed"
        }));
        Ok(())
    }

    #[test]
    fn terminalization_cleanup_failure_is_non_authoritative_and_not_replayed() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-cleanup-failure".to_string();
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-cleanup-failure/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        let worktree_root = paths.repo_worktree_dir(&repo_id, &run_id);
        fs::create_dir_all(&worktree_root)?;
        fs::write(worktree_root.join("must-not-be-cleaned.txt"), "retain me\n")?;
        state.prepare_run_terminal_transition(
            &run_id,
            RunStatus::Failed,
            "gate failed",
            "gate failed",
            "run failed before question answer",
        )?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));

        super::inject_terminalization_fault(super::TerminalizationFaultStage::Cleanup);
        assert_eq!(manager.recover_interrupted_runs()?, 1);
        let terminal = state.get_run(&run_id)?.expect("terminal run");
        assert_eq!(terminal.status, RunStatus::Failed);
        assert_eq!(terminal.error, "gate failed");
        assert!(
            worktree_root.exists(),
            "failed cleanup must retain recovery evidence"
        );
        assert_eq!(
            state
                .get_events(&run_id, 100)?
                .iter()
                .filter(|event| event.typ == "worktree_cleanup_error")
                .count(),
            1
        );

        assert_eq!(manager.recover_interrupted_runs()?, 0);
        assert!(worktree_root.exists());
        assert_eq!(
            state
                .get_events(&run_id, 100)?
                .iter()
                .filter(|event| event.typ == "worktree_cleanup_error")
                .count(),
            1,
            "a claimed cleanup must not be replayed after process loss uncertainty"
        );
        Ok(())
    }

    #[test]
    fn fake_runner_parallelizes_independent_slices() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.areas = vec!["slice-001.txt".to_string()];
        first.verify = vec!["test -f slice-001.txt".to_string()];
        let mut second = slice("slice-002");
        second.areas = vec!["slice-002.txt".to_string()];
        second.verify = vec!["test -f slice-002.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        artifact::write_json(store.slices_dir().join("slice-002.json"), &second)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slices"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths.clone(), state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 2,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs.len(), 2);
        assert!(
            slice_runs
                .iter()
                .all(|slice_run| slice_run.status == SliceStatus::Merged)
        );
        Ok(())
    }

    #[test]
    fn overlapping_area_slices_are_merged_between_worker_batches() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        fs::write(repo.path().join("shared.txt"), "base\n")?;
        gitutil::run(repo.path(), &["add", "shared.txt"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add shared fixture"])?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.areas = vec!["shared.txt".to_string()];
        first.verify = vec!["grep -q slice-001 shared.txt".to_string()];
        let mut second = slice("slice-002");
        second.areas = vec!["shared.txt".to_string()];
        second.verify = vec!["grep -q slice-002 shared.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        artifact::write_json(store.slices_dir().join("slice-002.json"), &second)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add overlapping slices"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(SharedFileAppendRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 2,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let shared = gitutil::run(
            repo.path(),
            &[
                "show",
                &format!("{}:shared.txt", completed.integration_branch),
            ],
        )?;
        assert_eq!(shared, "base\nslice-001\nslice-002");
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert!(
            slice_runs
                .iter()
                .all(|slice_run| slice_run.status == SliceStatus::Merged)
        );
        Ok(())
    }

    #[test]
    fn native_pi_tui_worker_selection_is_recorded_in_run_preflight() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut only = slice("slice-001");
        only.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &only)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: true,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let preflight: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "preflight.json"))?;
        assert_eq!(preflight["native_pi_tui_worker"], true);
        assert_eq!(preflight["experimental_pi_tui_worker"], true);
        assert_eq!(preflight["worker_interface"], "native_pi_tui");
        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        Ok(())
    }

    #[test]
    fn parallel_layer_failure_reports_ready_sibling_as_preserved_unmerged() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.areas = vec!["slice-001.txt".to_string()];
        first.verify = vec!["false".to_string()];
        let mut second = slice("slice-002");
        second.areas = vec!["slice-002.txt".to_string()];
        second.verify = vec!["test -f slice-002.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        artifact::write_json(store.slices_dir().join("slice-002.json"), &second)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add parallel slices"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths.clone(),
            state.clone(),
            Arc::new(ReadySiblingFailRunner),
        );
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 2,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let failed = wait_for_run(&state, &run.id)?;
        assert_eq!(failed.status, RunStatus::Failed);
        let events = state.get_events(&run.id, 200)?;
        let failed_event = events
            .iter()
            .find(|event| event.typ == "parallel_layer_failed")
            .expect("parallel_layer_failed event");
        let outcomes = failed_event.payload["outcomes"].as_array().unwrap();
        let ready = outcomes
            .iter()
            .find(|outcome| outcome["slice_id"].as_str() == Some("slice-002"))
            .expect("ready sibling outcome");
        assert_eq!(ready["status"].as_str(), Some("ready_to_merge"));
        assert_eq!(
            ready["preserved_unmerged"]["disposition"].as_str(),
            Some("preserved_unmerged_due_to_layer_atomicity")
        );
        assert!(
            ready["preserved_unmerged"]["branch"]
                .as_str()
                .unwrap_or_default()
                .contains("slice-002")
        );
        assert!(
            !ready["preserved_unmerged"]["commit_sha"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn fake_runner_e2e_executes_dependent_slices_and_cleans_worktrees() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        let mut second = slice("slice-002");
        second.depends_on = vec!["slice-001".to_string()];
        second.verify = vec!["test -f slice-002.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        artifact::write_json(store.slices_dir().join("slice-002.json"), &second)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slices"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths.clone(), state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-002".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let events = wait_for_event(&state, &run.id, "worktrees_cleaned")?;
        assert_eq!(completed.selected_slice_id, "slice-001,slice-002");
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs.len(), 2);
        assert!(
            slice_runs
                .iter()
                .all(|slice_run| slice_run.status == SliceStatus::Merged)
        );
        assert!(
            store
                .output_path(&run.id, "implementation-summary.json")
                .exists()
        );
        assert!(store.output_path(&run.id, "final-report.json").exists());
        assert!(store.output_path(&run.id, "economics.json").exists());
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.integration_repair.status, "skipped");
        assert_eq!(summary.integration_repair.trigger, "gate_passed");
        assert_eq!(summary.economics.repair_policy, "auto");
        assert_eq!(summary.economics.repair_attempts, 0);
        assert_eq!(summary.exit_states.run, "completed");
        assert_eq!(summary.exit_states.handoff, "ready_for_handoff");
        assert_eq!(summary.exit_states.evidence, "daemon_attested");
        assert_eq!(summary.exit_states.slices.len(), 2);
        assert!(
            summary
                .exit_states
                .slices
                .iter()
                .all(|slice| slice.worker == "complete" && slice.daemon == "merged")
        );
        assert_eq!(summary.evidence_attestation.status, "daemon_attested");
        assert_eq!(summary.evidence_attestation.attester, "khazad-doom-daemon");
        assert!(!summary.evidence_attestation.worker_self_approved);
        assert!(
            summary
                .evidence_attestation
                .basis
                .iter()
                .any(|basis| basis.contains("claim, not approval"))
        );
        assert_eq!(summary.economics.agent_call_count, 2);
        assert!(summary.economics.command_execution_count >= 2);
        assert_eq!(summary.economics.duplicate_command_count, 0);
        assert!(summary.economics.sla_violations.is_empty());
        assert!(
            summary
                .completed_slices
                .iter()
                .all(|worker| !worker.acceptance_status.is_empty())
        );
        gitutil::run(
            repo.path(),
            &[
                "show",
                &format!(
                    "{}:.workflow/reports/{}-implementation-summary.json",
                    completed.integration_branch, run.id
                ),
            ],
        )?;
        gitutil::run(
            repo.path(),
            &[
                "show",
                &format!(
                    "{}:.workflow/reports/{}-final-report.json",
                    completed.integration_branch, run.id
                ),
            ],
        )?;
        let closed_slice = gitutil::run(
            repo.path(),
            &[
                "show",
                &format!(
                    "{}:.workflow/slices/slice-001.json",
                    completed.integration_branch
                ),
            ],
        )?;
        assert!(closed_slice.contains("\"status\": \"closed\""));
        assert!(closed_slice.contains(&format!("\"closed_by_run\": \"{}\"", run.id)));
        assert!(
            !paths
                .repo_worktree_dir(&completed.repo_id, &run.id)
                .exists()
        );
        let handoff = manager.branch_handoff(&run.id, false, false, false)?;
        assert_eq!(handoff.integration_branch, completed.integration_branch);
        assert_eq!(handoff.exit_states.handoff, "ready_for_handoff");
        assert_eq!(handoff.evidence_attestation.status, "daemon_attested");
        assert!(!handoff.evidence_attestation.worker_self_approved);
        assert!(handoff.push_command.contains(&completed.integration_branch));
        assert!(handoff.pr_command.contains("gh pr create"));
        let inspection = manager.inspect_run(&run.id, 10)?;
        assert!(
            inspection
                .artifacts
                .iter()
                .any(|artifact| artifact.name == "final-report.json")
        );
        assert!(!store.origin_path(&run.id).exists());
        assert!(!store.notifications_dir(&run.id).exists());
        assert!(events.iter().any(|event| event.typ == "run_completed"));
        assert!(events.iter().any(|event| event.typ == "worktrees_cleaned"));
        assert!(
            !events
                .iter()
                .any(|event| event.typ.starts_with("terminal_notification_"))
        );
        Ok(())
    }

    #[test]
    fn terminal_notification_origin_target_is_artifact_and_delivery_is_nonfatal() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: "agent-1".to_string(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let origin: OriginNotificationTarget = artifact::read_json(store.origin_path(&run.id))?;
        assert_eq!(origin.target, "agent-1");
        assert_eq!(origin.target_kind, "opaque");
        assert_eq!(origin.delivery_adapter, "herdr");
        assert_eq!(origin.delivery_surface, "agent_send");
        let record = terminal_notification_record_for_status(&store, &run.id, "completed")?;
        assert_eq!(record.terminal_status, "completed");
        assert!(record.transition_key.starts_with("terminal:completed:"));
        assert_eq!(record.delivery_status, "failed");
        assert_eq!(record.origin_target, "agent-1");
        assert_eq!(record.payload["kind"], "khazad_terminal_feedback");
        assert_eq!(record.payload["terminal_status"], "completed");
        assert_eq!(
            record.payload["feed_summary_line"].as_str(),
            Some("run completed; handoff artifacts are ready")
        );
        assert_eq!(
            record.payload["message"].as_str(),
            record.payload["feed"]["summary_line"].as_str()
        );
        assert!(
            record.payload["feed"]["blocks"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["label"] == "Run"))
        );
        assert!(
            record.payload["next_commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command
                    .as_str()
                    .unwrap_or_default()
                    .contains("handoff --run"))
        );
        let after_notification = state.get_run(&run.id)?.expect("run remains present");
        assert_eq!(after_notification.status, RunStatus::Completed);
        let events = state.get_events(&run.id, 100)?;
        assert!(events.iter().any(|event| {
            event.typ == "origin_notification_target_recorded"
                && event.payload["target_kind"] == "opaque"
        }));
        assert!(events.iter().any(|event| {
            event.typ == "run_incident"
                && event.payload["kind"] == "terminal_notification_failed"
                && event.payload["terminal_status"] == "completed"
        }));
        Ok(())
    }

    #[test]
    fn terminal_notification_dedupe_is_per_terminal_transition_and_skips_interrupted() -> Result<()>
    {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let now = Utc::now();
        let run = Run {
            id: "kd-terminal-notification".to_string(),
            repo_id,
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "master".to_string(),
            base_sha: gitutil::head_sha(repo.path())?,
            integration_branch: "khazad/kd-terminal-notification/integration".to_string(),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        store.ensure_run_dirs(&run.id)?;
        store.write_origin_notification_target(
            &run.id,
            &OriginNotificationTarget {
                schema_version: 1,
                target: "agent-1".to_string(),
                target_kind: "opaque".to_string(),
                delivery_adapter: "herdr".to_string(),
                delivery_surface: "agent_send".to_string(),
                source: "run_start".to_string(),
                created_at: Utc::now().to_rfc3339(),
            },
        )?;
        let manager = Manager::new(paths, state.clone());

        manager.mark_progress(&run.id, "blocked", "", 0, "", "blocked once");
        manager.write_terminal_run_summary(&run, RunStatus::Blocked, "blocked once")?;
        manager.write_terminal_run_summary(&run, RunStatus::Blocked, "blocked duplicate")?;
        manager.mark_progress(&run.id, "completed", "", 0, "", "completed later");
        manager.write_terminal_run_summary(&run, RunStatus::Completed, "completed later")?;
        manager.mark_progress(&run.id, "interrupted", "", 0, "", "interrupted");
        manager.write_terminal_run_summary(&run, RunStatus::Interrupted, "interrupted")?;

        let records = terminal_notification_records(&store, &run.id)?;
        assert_eq!(
            records
                .iter()
                .filter(|record| record.terminal_status == "blocked")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.terminal_status == "completed")
                .count(),
            1
        );
        assert!(
            records
                .iter()
                .all(|record| record.terminal_status != "interrupted")
        );
        let events = state.get_events(&run.id, 200)?;
        let notification_incidents = events
            .iter()
            .filter(|event| {
                event.typ == "run_incident"
                    && event.payload["kind"] == "terminal_notification_failed"
            })
            .count();
        assert_eq!(notification_incidents, 2);
        let blocked = records
            .iter()
            .find(|record| record.terminal_status == "blocked")
            .expect("blocked record");
        let completed = records
            .iter()
            .find(|record| record.terminal_status == "completed")
            .expect("completed record");
        assert!(blocked.transition_key.starts_with("terminal:blocked:"));
        assert!(completed.transition_key.starts_with("terminal:completed:"));
        Ok(())
    }

    #[test]
    fn terminal_notification_run_summary_and_status_share_read_model_truth() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::new(paths, state.clone());
        let repo_id = crate::paths::repo_id(repo.path());
        let base_sha = gitutil::head_sha(repo.path())?;
        let cases = [
            (RunStatus::Blocked, "blocked by operator input"),
            (RunStatus::Failed, "integration gate failed"),
            (RunStatus::Completed, ""),
            (RunStatus::Cancelled, "cancelled by request"),
        ];

        for (index, (status, message)) in cases.into_iter().enumerate() {
            let run_id = format!("kd-terminal-read-model-{index}");
            let now = Utc::now();
            let run = Run {
                id: run_id.clone(),
                repo_id: repo_id.clone(),
                repo_path: repo.path().to_string_lossy().to_string(),
                status: RunStatus::Running,
                base_branch: "master".to_string(),
                base_sha: base_sha.clone(),
                integration_branch: format!("khazad/{run_id}/integration"),
                selected_slice_id: "slice-001".to_string(),
                error: String::new(),
                started_at: now,
                updated_at: now,
            };
            state.insert_run(&run)?;
            state.upsert_slice_run(&SliceRun {
                run_id: run_id.clone(),
                slice_id: "slice-001".to_string(),
                status: match status {
                    RunStatus::Blocked => SliceStatus::Blocked,
                    RunStatus::Failed => SliceStatus::Failed,
                    RunStatus::Cancelled => SliceStatus::Cancelled,
                    RunStatus::Completed => SliceStatus::Merged,
                    _ => SliceStatus::Running,
                },
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 1,
                last_error: if status == RunStatus::Completed {
                    String::new()
                } else {
                    message.to_string()
                },
            })?;
            let progress_message = if status == RunStatus::Completed {
                "run completed; handoff artifacts are ready"
            } else {
                message
            };
            manager.mark_progress(&run.id, status.as_str(), "", 0, "", progress_message);
            match status {
                RunStatus::Blocked => {
                    state.record_event(
                        &run.id,
                        "run_incident",
                        &json!({
                            "severity": "error",
                            "kind": "operator_question",
                            "failure_kind": "operator_question",
                            "message": message,
                            "operator_action_required": true,
                            "retryable": true,
                            "fix_commands": [format!("khazad-doom attend --run {}", run.id)]
                        }),
                    )?;
                    state.insert_worker_question(
                        "q-1",
                        &run.id,
                        "slice-001",
                        1,
                        "Which option should the worker use?",
                        &["Use A".to_string(), "Use B".to_string()],
                        0,
                    )?;
                    state.create_replan_proposal(
                        &run.id,
                        "rpl-1",
                        ReplanProposalSource {
                            kind: "worker_finding".to_string(),
                            slice_id: "slice-001".to_string(),
                            phase: "worker".to_string(),
                            attempt: 1,
                            summary: "operator decision needed".to_string(),
                        },
                        vec!["finding-1".to_string()],
                        vec![ReplanEvidenceLink {
                            kind: "event".to_string(),
                            event_id: 1,
                            summary: "operator decision needed".to_string(),
                            ..ReplanEvidenceLink::default()
                        }],
                        vec![ReplanProposedChange {
                            kind: "scope".to_string(),
                            target: "src/workflow/read_model.rs".to_string(),
                            summary: "grant read-model follow-up".to_string(),
                        }],
                        "operator_review",
                    )?;
                    let mut economics = RunEconomics {
                        repair_policy: "auto".to_string(),
                        ..RunEconomics::default()
                    };
                    economics.agent_call_count = 1;
                    artifact::write_json(store.output_path(&run.id, "economics.json"), &economics)?;
                    artifact::write_json(
                        store.output_path(&run.id, "preflight.json"),
                        &json!({
                            "agent": "pi",
                            "agent_profile": "implementer",
                            "agent_provider": "openai-codex",
                            "agent_model": "gpt-5.5",
                            "agent_reasoning": "xhigh",
                            "agent_mode": "fast",
                            "profile_summary": "implementer: provider=openai-codex model=gpt-5.5 reasoning=xhigh mode=fast",
                            "launch_summary": "pi implementer: provider=openai-codex model=gpt-5.5 reasoning=xhigh mode=fast",
                            "worker_evidence_kind": "real_pi_worker",
                            "worker_evidence_label": "real Pi worker implementation evidence"
                        }),
                    )?;
                }
                RunStatus::Failed => {
                    state.record_event(&run.id, "run_error", &json!({ "error": message }))?;
                }
                RunStatus::Cancelled => {
                    state.record_event(&run.id, "run_cancelled", &json!({ "reason": message }))?;
                }
                RunStatus::Completed => {}
                _ => {}
            }

            manager.write_terminal_run_summary(&run, status, message)?;
            state.update_run(&run.id, status, message)?;
            let terminal_run = state.get_run(&run.id)?.expect("terminal run");
            let live = manager.run_read_model(&terminal_run, RunReadModelOptions::status(500))?;
            let summary: Value =
                artifact::read_json(store.output_path(&run.id, "run-summary.json"))?;
            let live_reason = serde_json::to_value(&live.details.primary_terminal_reason)?;
            let live_feed = serde_json::to_value(&live.details.feed)?;

            assert_eq!(summary["primary_terminal_reason"], live_reason);
            assert_eq!(
                summary["feed"]["terminal_reason"],
                live_feed["terminal_reason"]
            );
            assert_eq!(
                summary["feed"]["operator_commands"],
                live_feed["operator_commands"]
            );
            if status == RunStatus::Blocked {
                assert_eq!(
                    summary["incidents"],
                    serde_json::to_value(&live.details.incidents)?
                );
                assert_eq!(
                    summary["questions"],
                    serde_json::to_value(&live.details.questions)?
                );
                assert_eq!(
                    summary["replan"],
                    serde_json::to_value(&live.details.replan)?
                );
                assert_eq!(
                    summary["economics"],
                    serde_json::to_value(&live.details.economics)?
                );
                assert_eq!(
                    summary["worker_profile"],
                    serde_json::to_value(&live.details.worker_profile)?
                );
                assert_eq!(
                    summary["plan_revisions"],
                    serde_json::to_value(&live.plan_revisions)?
                );
            }
        }
        Ok(())
    }

    #[test]
    fn handoff_and_reports_include_plan_revision_history() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths,
            state.clone(),
            Arc::new(ReplanRecordingRunner {
                state: state.clone(),
            }),
        );
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let final_report: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        let implementation_summary: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "implementation-summary.json"))?;
        for report in [&final_report, &implementation_summary] {
            let attempts = report["worker_attempts"].as_array().unwrap();
            assert_eq!(attempts.len(), 1);
            assert!(attempts[0]["launch_id"].as_i64().unwrap() > 0);
            assert_eq!(attempts[0]["kind"], "slice-worker");
            let revisions = &report["plan_revisions"];
            assert_eq!(revisions["source_of_truth"], "daemon_replan_proposals");
            assert_eq!(revisions["unresolved_pending_blocks_handoff"], false);
            assert_eq!(revisions["pending"].as_array().unwrap().len(), 0);
            assert_eq!(revisions["accepted"].as_array().unwrap().len(), 1);
            assert_eq!(revisions["rejected"].as_array().unwrap().len(), 1);
            assert_eq!(revisions["deferred"].as_array().unwrap().len(), 1);
            assert_eq!(revisions["superseded"].as_array().unwrap().len(), 1);
            let accepted = &revisions["accepted"][0];
            assert!(!accepted["evidence"].as_array().unwrap().is_empty());
            assert_eq!(accepted["decision"]["authorizer"], "test-authorizer");
            assert!(
                accepted["decision"]["applied_at_checkpoint"]
                    .as_str()
                    .unwrap()
                    .contains("not_applied")
            );
            assert!(
                accepted["before_queue_or_slice_summary"]
                    .as_str()
                    .unwrap()
                    .contains("slice-001")
            );
            assert!(
                accepted["after_queue_or_slice_summary"]
                    .as_str()
                    .unwrap()
                    .contains("apply_status=not_applicable")
            );
            assert_eq!(
                revisions["deferred"][0]["decision"]["revisit_condition"],
                "after release"
            );
        }

        let worker_launches = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        let worker_handoff: serde_json::Value = artifact::read_json(
            store
                .handoff_dir(&run.id)
                .join(format!("{}.json", worker_launches[0].output_stem)),
        )?;
        assert_eq!(
            worker_handoff["plan_revisions"]["source_of_truth"],
            "daemon_replan_proposals"
        );
        let handoff = manager.branch_handoff(&run.id, false, false, false)?;
        assert_eq!(handoff.plan_revisions.accepted.len(), 1);
        assert_eq!(handoff.plan_revisions.rejected.len(), 1);
        assert_eq!(handoff.plan_revisions.deferred.len(), 1);
        assert_eq!(handoff.plan_revisions.superseded.len(), 1);
        Ok(())
    }

    #[test]
    fn shadow_frontier_metrics_reach_final_reports_and_handoff() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.areas = vec!["src/".to_string()];
        first.verify = vec!["test -f src/slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(TwoFollowupsRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: Some(mission_envelope()),
        })?;

        let blocked = wait_for_run(&state, &run.id)?;
        assert_eq!(blocked.status, RunStatus::Blocked);
        let pending = state.pending_replan_proposals(&run.id)?;
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|proposal| {
            proposal
                .frontier_classification
                .as_ref()
                .is_some_and(|classification| {
                    classification
                        .reason_codes
                        .contains(&"shadow_observation_only".to_string())
                })
        }));

        state.decide_replan_proposal(
            &run.id,
            &pending[0].id,
            ReplanProposalState::Accepted,
            "operator accepted first shadow candidate",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;
        state.decide_replan_proposal(
            &run.id,
            &pending[1].id,
            ReplanProposalState::Rejected,
            "operator rejected second shadow candidate",
            "operator",
            "daemon_ipc",
            "",
            "",
        )?;

        manager.resume_run(ResumeOptions {
            run_id: run.id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;
        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);

        let final_report: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(
            final_report.plan_revisions.frontier.activity_status,
            "active"
        );
        assert_eq!(final_report.plan_revisions.frontier.candidates_seen, 2);
        assert_eq!(
            final_report
                .plan_revisions
                .frontier
                .budget_consumption
                .max_auto_promotions,
            mission_envelope().max_auto_promotions
        );
        assert_eq!(
            final_report
                .plan_revisions
                .frontier
                .generated_slice_graph
                .len(),
            1
        );
        let edge = &final_report.plan_revisions.frontier.generated_slice_graph[0];
        assert_eq!(edge.parent_slice_id, "slice-001");
        assert_eq!(edge.child_slice_id, "slice-001-followup-a");
        assert!(edge.origin_proposal_id.starts_with("rp-"));
        assert_eq!(
            final_report
                .plan_revisions
                .frontier
                .deferred_rejected_pending_fog
                .len(),
            1
        );
        assert_eq!(final_report.plan_revisions.frontier.authorizers.len(), 2);
        assert_eq!(
            final_report
                .plan_revisions
                .frontier
                .tier_distribution
                .get("tier_1"),
            Some(&2)
        );
        assert_eq!(
            final_report.plan_revisions.frontier.agreement.tier1_total,
            2
        );
        assert_eq!(
            final_report
                .plan_revisions
                .frontier
                .agreement
                .accepted_unchanged,
            1
        );
        assert_eq!(final_report.plan_revisions.frontier.agreement.rejected, 1);
        assert_eq!(
            final_report
                .plan_revisions
                .frontier
                .agreement
                .agreement_ratio,
            "1/2"
        );
        let implementation_summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "implementation-summary.json"))?;
        assert_eq!(
            implementation_summary
                .plan_revisions
                .frontier
                .agreement
                .agreement_ratio,
            "1/2"
        );
        let handoff = manager.branch_handoff(&run.id, false, false, false)?;
        assert_eq!(handoff.plan_revisions.frontier.candidates_seen, 2);
        assert_eq!(handoff.plan_revisions.frontier.agreement.rejected, 1);
        assert_eq!(
            handoff.plan_revisions.frontier.generated_slice_graph.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn handoff_blocks_pending_replan_proposals() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let pending = create_followup_replan_proposal(
            &state,
            &run.id,
            "slice-001",
            followup_draft("slice-001-frontier"),
        )?;
        state.replace_replan_frontier_classification(
            &run.id,
            &pending.id,
            &FrontierClassification {
                tier: "tier_3".to_string(),
                reason_codes: vec!["operator_needed".to_string()],
                classified_at: Utc::now(),
                envelope_hash: "test-envelope".to_string(),
                budget_snapshot: FrontierBudgetState::default(),
                autonomy_level: AutonomyLevel::Shadow,
            },
        )?;
        let err = manager
            .branch_handoff(&run.id, false, false, false)
            .unwrap_err();
        let err_text = err.to_string();
        assert!(err_text.contains("handoff is not ready"));
        assert!(err_text.contains(&pending.id));
        assert!(err_text.contains("frontier pending"));
        assert!(err_text.contains("slice-001-frontier"));
        assert!(err_text.contains("tier=tier_3"));
        assert!(err_text.contains("khazad-doom replan accept"));
        Ok(())
    }

    #[test]
    fn completion_publication_recovery_blocks_an_advanced_branch() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        gitutil::commit_all(repo.path(), "initial")?;

        let mut closed = slice("slice-001");
        closed.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        closed.closed_by_run = "run-1".to_string();
        closed.closed_at = Utc::now().to_rfc3339();
        artifact::write_json(store.slice_path("slice-001"), &closed)?;
        write_publication_reports(&store, "run-1", &["slice-001"])?;
        let manifest =
            store.completion_publication_manifest("run-1", &["slice-001".to_string()])?;
        let receipt = store.commit_completion_publication("run-1", "main", &manifest)?;

        std::fs::write(
            repo.path().join("unrelated-after-publication.txt"),
            "advance\n",
        )?;
        gitutil::commit_all(repo.path(), "unrelated branch advance")?;
        let advanced_head = gitutil::head_sha(repo.path())?;
        assert_ne!(advanced_head, receipt.commit_sha);

        let err_without_event = existing_completion_publication(
            &store,
            "run-1",
            "main",
            &["slice-001".to_string()],
            None,
        )
        .unwrap_err();
        assert!(
            err_without_event
                .to_string()
                .contains("advanced beyond completion publication")
        );
        assert!(err_without_event.to_string().contains(&receipt.commit_sha));
        assert!(err_without_event.to_string().contains(&advanced_head));

        let err_with_event = existing_completion_publication(
            &store,
            "run-1",
            "main",
            &["slice-001".to_string()],
            Some(&receipt.commit_sha),
        )
        .unwrap_err();
        assert!(err_with_event.to_string().contains("moved from recorded"));
        assert!(err_with_event.to_string().contains(&receipt.commit_sha));
        assert!(err_with_event.to_string().contains(&advanced_head));
        Ok(())
    }

    #[test]
    fn completion_publication_recovery_blocks_dirty_or_missing_manifest_without_event() -> Result<()>
    {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        gitutil::commit_all(repo.path(), "initial")?;

        let mut closed = slice("slice-001");
        closed.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        closed.closed_by_run = "run-1".to_string();
        closed.closed_at = Utc::now().to_rfc3339();
        artifact::write_json(store.slice_path("slice-001"), &closed)?;
        write_publication_reports(&store, "run-1", &["slice-001"])?;
        let manifest =
            store.completion_publication_manifest("run-1", &["slice-001".to_string()])?;
        let receipt = store.commit_completion_publication("run-1", "main", &manifest)?;
        let final_report = store.final_report_artifact_path("run-1");
        let published_report = std::fs::read(&final_report)?;

        std::fs::write(&final_report, "operator edit\n")?;
        let dirty_err = existing_completion_publication(
            &store,
            "run-1",
            "main",
            &["slice-001".to_string()],
            None,
        )
        .unwrap_err();
        assert!(
            format!("{dirty_err:#}").contains("same pinned summary bytes"),
            "{dirty_err:#}"
        );
        assert_eq!(gitutil::head_sha(repo.path())?, receipt.commit_sha);

        std::fs::write(&final_report, published_report)?;
        std::fs::remove_file(&final_report)?;
        let missing_err = existing_completion_publication(
            &store,
            "run-1",
            "main",
            &["slice-001".to_string()],
            None,
        )
        .unwrap_err();
        assert!(
            format!("{missing_err:#}").contains("current manifest could not be captured"),
            "{missing_err:#}"
        );
        assert_eq!(gitutil::head_sha(repo.path())?, receipt.commit_sha);
        Ok(())
    }

    #[test]
    fn resume_after_completion_publication_is_idempotent() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: true,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(
            completed.status,
            RunStatus::Completed,
            "{}",
            completed.error
        );
        let preflight_path = store.output_path(&run.id, "preflight.json");
        let mut legacy_preflight: serde_json::Value = artifact::read_json(&preflight_path)?;
        let legacy_object = legacy_preflight
            .as_object_mut()
            .expect("preflight is a JSON object");
        legacy_object.remove("native_pi_tui_worker");
        legacy_object.insert("experimental_pi_tui_worker".to_string(), json!(true));
        artifact::write_json(&preflight_path, &legacy_preflight)?;
        let head_before = gitutil::run(repo.path(), &["rev-parse", &completed.integration_branch])?;
        let slice_ref_before = format!("{head_before}:.workflow/slices/slice-001.json");
        let closed_before = gitutil::run(repo.path(), &["show", &slice_ref_before])?;
        assert!(closed_before.contains("\"status\": \"closed\""));
        assert!(closed_before.contains(&format!("\"closed_by_run\": \"{}\"", run.id)));
        let subjects_before = gitutil::run(
            repo.path(),
            &["log", "--format=%s", &completed.integration_branch],
        )?;
        assert_eq!(
            subjects_before
                .matches("khazad(run): publish completion")
                .count(),
            1
        );
        let deleted = rusqlite::Connection::open(home.path().join("state.sqlite"))?.execute(
            "DELETE FROM events WHERE run_id = ?1 AND type = 'completion_publication_committed'",
            [&run.id],
        )?;
        assert_eq!(deleted, 1, "fixture removes the post-commit receipt event");

        state.update_run(&run.id, RunStatus::Running, "")?;
        state.update_slice_status(&run.id, "slice-001", SliceStatus::Running, "")?;
        state.activate_slice_attempt(&run.id, "slice-001", 1)?;
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is the smallest reversible option".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };
        state.open_active_worker_question_with_recommendation(
            "q-report-operator",
            &run.id,
            "slice-001",
            1,
            "Operator or fallback?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
            "worker_question_asked",
            |question| Ok(json!({ "question_id": question.id })),
            "awaiting operator answer",
        )?;
        state.decide_worker_question_command(
            &run.id,
            "q-report-operator",
            WorkerQuestionDecisionCommand::answer(
                "B",
                WorkerQuestionAnswerSource::Operator,
                "operator answered; worker resuming",
            ),
        )?;
        state.open_active_worker_question_with_recommendation(
            "q-report-fallback",
            &run.id,
            "slice-001",
            1,
            "Apply recommendation at deadline?",
            &["A".to_string(), "B".to_string()],
            1,
            &recommendation,
            "worker_question_asked",
            |question| Ok(json!({ "question_id": question.id })),
            "awaiting operator answer",
        )?;
        thread::sleep(Duration::from_millis(1_100));
        state.decide_worker_question_command(
            &run.id,
            "q-report-fallback",
            WorkerQuestionDecisionCommand::answer(
                "A",
                WorkerQuestionAnswerSource::LlmRecommendationTimeout,
                "recommendation applied; worker resuming",
            ),
        )?;
        state.update_slice_status(&run.id, "slice-001", SliceStatus::Merged, "")?;
        state.update_run(
            &run.id,
            RunStatus::Interrupted,
            "simulated interruption after publication",
        )?;
        manager.resume_run(super::ResumeOptions {
            run_id: run.id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;

        let resumed = wait_for_run(&state, &run.id)?;
        assert_eq!(resumed.status, RunStatus::Completed);
        let head_after = gitutil::run(repo.path(), &["rev-parse", &resumed.integration_branch])?;
        assert_eq!(head_after, head_before);
        let slice_ref_after = format!("{head_after}:.workflow/slices/slice-001.json");
        let closed_after = gitutil::run(repo.path(), &["show", &slice_ref_after])?;
        assert_eq!(closed_after, closed_before);
        let subjects_after = gitutil::run(
            repo.path(),
            &["log", "--format=%s", &resumed.integration_branch],
        )?;
        assert_eq!(
            subjects_after
                .matches("khazad(run): publish completion")
                .count(),
            1
        );
        let events = state.get_events(&run.id, 20)?;
        let resumed_event = events
            .iter()
            .find(|event| event.typ == "run_resumed")
            .expect("run_resumed event");
        assert_eq!(resumed_event.payload["native_pi_tui_worker"], true);
        let reconciled_receipts = events
            .iter()
            .filter(|event| event.typ == "completion_publication_committed")
            .collect::<Vec<_>>();
        assert_eq!(reconciled_receipts.len(), 1);
        assert_eq!(
            reconciled_receipts[0].payload["commit_sha"].as_str(),
            Some(head_after.as_str())
        );
        assert_eq!(resumed_event.payload["experimental_pi_tui_worker"], true);
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.final_sha, head_after);
        assert_eq!(summary.completed_slices.len(), 1);
        assert_eq!(summary.worker_questions.len(), 2);
        let operator = summary
            .worker_questions
            .iter()
            .find(|question| question.id == "q-report-operator")
            .expect("operator question audit");
        assert_eq!(operator.answer, "B");
        assert_eq!(
            operator.answer_source,
            Some(WorkerQuestionAnswerSource::Operator)
        );
        let fallback = summary
            .worker_questions
            .iter()
            .find(|question| question.id == "q-report-fallback")
            .expect("fallback question audit");
        assert_eq!(fallback.answer, "A");
        assert_eq!(fallback.recommended_answer, "A");
        assert_eq!(
            fallback.recommendation_rationale,
            "A is the smallest reversible option"
        );
        assert_eq!(
            fallback.answer_source,
            Some(WorkerQuestionAnswerSource::LlmRecommendationTimeout)
        );
        let implementation_summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "implementation-summary.json"))?;
        assert_eq!(implementation_summary.worker_questions.len(), 2);
        Ok(())
    }

    #[test]
    fn snapshots_and_cleanup_include_nested_launch_worktrees() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("kd-nested-worktrees", repo.path(), "slice-001")?;
        let root = paths.repo_worktree_dir(&run.repo_id, &run.id);
        let first = root.join("slice-001/launch-1");
        let second = root.join("slice-001/launch-2");
        let integration = root.join("integration");
        gitutil::worktree_add(
            repo.path(),
            &first,
            "khazad/kd-nested-worktrees/slice-001/launch-1",
            &run.base_sha,
        )?;
        gitutil::worktree_add(
            repo.path(),
            &second,
            "khazad/kd-nested-worktrees/slice-001/launch-2",
            &run.base_sha,
        )?;
        gitutil::worktree_add(
            repo.path(),
            &integration,
            "khazad/kd-nested-worktrees/integration",
            &run.base_sha,
        )?;
        let manager = Manager::new(paths, state);

        let snapshot_paths = manager
            .run_worktree_snapshots(&run)
            .into_iter()
            .filter_map(|snapshot| snapshot["path"].as_str().map(str::to_string))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            snapshot_paths,
            [
                first.to_string_lossy().to_string(),
                second.to_string_lossy().to_string(),
                integration.to_string_lossy().to_string(),
            ]
            .into_iter()
            .collect()
        );

        manager.cleanup_run_worktrees(&run)?;
        assert!(!root.exists());
        let worktree_list = gitutil::run(repo.path(), &["worktree", "list", "--porcelain"])?;
        assert!(!worktree_list.contains("kd-nested-worktrees"));
        Ok(())
    }

    #[test]
    fn terminal_cleanup_preserves_retained_publication_recovery_journal() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-retained-publication-journal".to_string();
        let base_sha = gitutil::head_sha(repo.path())?;
        let integration_branch = format!("khazad/{run_id}/integration");
        gitutil::run(repo.path(), &["branch", &integration_branch, &base_sha])?;
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Failed,
            base_branch: "master".to_string(),
            base_sha,
            integration_branch: integration_branch.clone(),
            selected_slice_id: "slice-001".to_string(),
            error: "publication failed".to_string(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        let integration = paths
            .repo_worktree_dir(&repo_id, &run_id)
            .join("integration");
        fs::create_dir_all(integration.parent().unwrap())?;
        gitutil::worktree_add_existing(repo.path(), &integration, &integration_branch)?;
        gitutil::retain_test_completion_publication_journal(&integration)?;

        let manager = Manager::with_runner(paths, state, Arc::new(FakeRunner));
        let err = manager.cleanup_run_worktrees(&run).unwrap_err();

        assert!(err.to_string().contains("recovery journal"), "{err:#}");
        assert!(integration.is_dir());
        assert!(gitutil::has_retained_completion_publication_journal(
            &integration
        )?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cleanup_and_resume_preserve_process_loss_publication_journal() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-process-loss-publication-journal".to_string();
        let base_sha = gitutil::head_sha(repo.path())?;
        let integration_branch = format!("khazad/{run_id}/integration");
        gitutil::run(repo.path(), &["branch", &integration_branch, &base_sha])?;
        let now = Utc::now();
        let run = Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Failed,
            base_branch: "master".to_string(),
            base_sha,
            integration_branch: integration_branch.clone(),
            selected_slice_id: "slice-001".to_string(),
            error: "daemon exited during publication".to_string(),
            started_at: now,
            updated_at: now,
        };
        state.insert_run(&run)?;
        let integration = paths
            .repo_worktree_dir(&repo_id, &run_id)
            .join("integration");
        fs::create_dir_all(integration.parent().unwrap())?;
        gitutil::worktree_add_existing(repo.path(), &integration, &integration_branch)?;
        let mut former_owner = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()?;
        let dead_owner_pid = former_owner.id();
        assert!(former_owner.wait()?.success());
        gitutil::retain_test_process_loss_completion_publication_journal(
            &integration,
            dead_owner_pid,
        )?;

        let manager = Manager::with_runner(paths, state, Arc::new(FakeRunner));
        let cleanup_err = manager.cleanup_run_worktrees(&run).unwrap_err();
        assert!(
            cleanup_err.to_string().contains("recovery journal"),
            "{cleanup_err:#}"
        );
        assert!(integration.is_dir());

        manager.prepare_resume_worktrees(&run)?;
        assert!(integration.is_dir());
        assert!(gitutil::has_retained_completion_publication_journal(
            &integration
        )?);
        Ok(())
    }

    #[test]
    fn two_resumes_preserve_launch_evidence_and_do_not_reset_retry_budget() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        store.ensure_run_dirs("kd-two-resumes")?;
        let mut test_slice = slice("slice-001");
        test_slice.areas = vec!["slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &test_slice)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add retry budget slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let mut run = test_run("kd-two-resumes", repo.path(), "slice-001")?;
        gitutil::run(
            repo.path(),
            &["branch", &run.integration_branch, &run.base_sha],
        )?;
        state.insert_run(&run)?;
        let worktree_root = paths.repo_worktree_dir(&run.repo_id, &run.id);
        let first = state.allocate_worker_attempt(
            &run.id,
            "slice-001",
            1,
            1,
            0,
            0,
            "slice-worker",
            &worktree_root,
        )?;
        state.mark_worker_attempt_launched(first.launch_id)?;
        state.observe_worker_attempt(
            &run.id,
            "worker_running",
            "slice-001",
            1,
            Some(first.launch_id),
            Some(101),
            "stdout",
            r#"{"type":"tool_execution_end","toolName":"first-immutable-activity"}"#,
            30,
            10,
        )?;
        state.finish_worker_attempt(first.launch_id, "failed", "first failure")?;
        let second = state.allocate_worker_attempt(
            &run.id,
            "slice-001",
            1,
            2,
            0,
            0,
            "slice-worker",
            &worktree_root,
        )?;
        state.mark_worker_attempt_launched(second.launch_id)?;
        state.observe_worker_attempt(
            &run.id,
            "worker_running",
            "slice-001",
            2,
            Some(second.launch_id),
            Some(202),
            "stdout",
            r#"{"type":"tool_execution_end","toolName":"second-immutable-activity"}"#,
            30,
            10,
        )?;
        state.finish_worker_attempt(second.launch_id, "failed", "second failure")?;
        state.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Failed,
            branch: second.branch.clone(),
            commit_sha: String::new(),
            attempts: 2,
            last_error: "second failure".to_string(),
        })?;
        state.update_run(&run.id, RunStatus::Failed, "second failure")?;
        run.status = RunStatus::Failed;
        let first_evidence = store.output_path(&run.id, &format!("{}.json", first.output_stem));
        let second_evidence = store.output_path(&run.id, &format!("{}.json", second.output_stem));
        fs::write(&first_evidence, b"first immutable worker result\n")?;
        fs::write(&second_evidence, b"second immutable worker result\n")?;
        let first_raw_output = store
            .pi_wrapper_artifacts_for_output_path(&first_evidence)?
            .stdout_path;
        let second_raw_output = store
            .pi_wrapper_artifacts_for_output_path(&second_evidence)?
            .stdout_path;
        fs::write(&first_raw_output, b"first immutable raw output\n")?;
        fs::write(&second_raw_output, b"second immutable raw output\n")?;
        let before_resumes = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        assert_eq!(before_resumes.len(), 2);
        assert_eq!(
            before_resumes[0]
                .activity
                .as_ref()
                .map(|activity| activity.last_semantic_progress_summary.as_str()),
            Some("tool first-immutable-activity finished")
        );
        assert_eq!(before_resumes[0].failure_cause, "first failure");
        assert_eq!(before_resumes[1].failure_cause, "second failure");

        let runner = Arc::new(BudgetExhaustingRunner::default());
        let manager = Manager::with_runner(paths, state.clone(), runner.clone());
        manager.resume_run(ResumeOptions {
            run_id: run.id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;
        let first_resume = wait_for_run(&state, &run.id)?;
        assert_eq!(first_resume.status, RunStatus::Blocked);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
        let after_first_resume = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        assert_eq!(after_first_resume.len(), 3);
        assert_eq!(&after_first_resume[..2], before_resumes.as_slice());
        assert_eq!(after_first_resume[2].worker_retry_ordinal, 3);

        manager.resume_run(ResumeOptions {
            run_id: run.id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;
        let second_resume = wait_for_run(&state, &run.id)?;
        assert_eq!(second_resume.status, RunStatus::Failed);
        assert!(second_resume.error.contains("retry budget exhausted"));
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
        let after_second_resume = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        assert_eq!(after_second_resume, after_first_resume);
        assert_eq!(
            fs::read(&first_evidence)?,
            b"first immutable worker result\n"
        );
        assert_eq!(
            fs::read(&second_evidence)?,
            b"second immutable worker result\n"
        );
        assert_eq!(
            fs::read(&first_raw_output)?,
            b"first immutable raw output\n"
        );
        assert_eq!(
            fs::read(&second_raw_output)?,
            b"second immutable raw output\n"
        );
        assert_eq!(state.current_run_execution_epoch(&run.id)?, 3);
        assert_eq!(
            state
                .get_events(&run.id, 500)?
                .iter()
                .filter(|event| event.typ == "run_resumed")
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn resume_removes_stale_integration_worktree_before_recreating() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-stale-resume".to_string();
        let base_sha = gitutil::head_sha(repo.path())?;
        let integration_branch = format!("khazad/{run_id}/integration");
        gitutil::run(repo.path(), &["branch", &integration_branch, &base_sha])?;
        let now = Utc::now();
        state.insert_run(&Run {
            id: run_id.clone(),
            repo_id: repo_id.clone(),
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Failed,
            base_branch: "master".to_string(),
            base_sha,
            integration_branch,
            selected_slice_id: "slice-001".to_string(),
            error: "previous failure".to_string(),
            started_at: now,
            updated_at: now,
        })?;
        let stale_integration = paths
            .repo_worktree_dir(&repo_id, &run_id)
            .join("integration");
        fs::create_dir_all(&stale_integration)?;
        fs::write(stale_integration.join("stale.txt"), "stale")?;

        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        manager.resume_run(super::ResumeOptions {
            run_id: run_id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;

        let completed = wait_for_run(&state, &run_id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let events = state.get_events(&run_id, 100)?;
        assert!(events.iter().any(|event| event.typ == "run_resumed"));
        assert!(
            !completed
                .error
                .contains("create existing integration worktree")
        );
        Ok(())
    }

    #[test]
    fn resume_skips_closed_historical_dependencies() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut closed_dep = slice("slice-001");
        closed_dep.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        closed_dep.closed_by_run = "historical-run".to_string();
        closed_dep.closed_at = Utc::now().to_rfc3339();
        let mut requested = slice("slice-002");
        requested.depends_on = vec!["slice-001".to_string()];
        requested.verify = vec!["test -f slice-002.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &closed_dep)?;
        artifact::write_json(store.slices_dir().join("slice-002.json"), &requested)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(
            repo.path(),
            &["commit", "-m", "add closed dependency and open slice"],
        )?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let repo_id = crate::paths::repo_id(repo.path());
        let run_id = "kd-resume-skip-closed-deps".to_string();
        let base_sha = gitutil::head_sha(repo.path())?;
        let integration_branch = format!("khazad/{run_id}/integration");
        gitutil::run(repo.path(), &["branch", &integration_branch, &base_sha])?;
        let now = Utc::now();
        state.insert_run(&Run {
            id: run_id.clone(),
            repo_id,
            repo_path: repo.path().to_string_lossy().to_string(),
            status: RunStatus::Blocked,
            base_branch: "master".to_string(),
            base_sha,
            integration_branch: integration_branch.clone(),
            selected_slice_id: "slice-002".to_string(),
            error: "previous merge conflict".to_string(),
            started_at: now,
            updated_at: now,
        })?;

        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        manager.resume_run(super::ResumeOptions {
            run_id: run_id.clone(),
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
        })?;

        let completed = wait_for_run(&state, &run_id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let slice_runs = state.get_slice_runs(&run_id)?;
        assert!(
            slice_runs
                .iter()
                .all(|slice_run| slice_run.slice_id != "slice-001"),
            "closed historical dependency must not be rerun: {slice_runs:?}"
        );
        assert!(slice_runs.iter().any(|slice_run| {
            slice_run.slice_id == "slice-002" && slice_run.status == SliceStatus::Merged
        }));
        let tree_files = gitutil::run(
            repo.path(),
            &["ls-tree", "-r", "--name-only", &integration_branch],
        )?;
        assert!(!tree_files.contains("slice-001.txt"));
        assert!(tree_files.contains("slice-002.txt"));
        Ok(())
    }

    #[test]
    fn missing_verify_tool_fails_once_and_preserves_primary_cause() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["definitely_missing_khazad_tool_for_retry_regression".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add missing-tool slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let blocked = wait_for_run(&state, &run.id)?;
        assert_eq!(blocked.status, RunStatus::Blocked);
        assert!(blocked.error.contains("daemon/operator environment"));
        assert!(
            blocked
                .error
                .contains("definitely_missing_khazad_tool_for_retry_regression")
        );
        assert!(!blocked.error.contains("nothing to commit"));
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs[0].attempts, 1);
        assert_eq!(slice_runs[0].status, SliceStatus::Blocked);
        let launches = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        let check: CheckResult = artifact::read_json(
            store.output_path(&run.id, &format!("{}.check.json", launches[0].output_stem)),
        )?;
        assert_eq!(check.failure_kind, "tool_missing");
        assert_eq!(check.findings[0].action, "operator-fix");
        let run_summary: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "run-summary.json"))?;
        assert_eq!(run_summary["status"], "blocked");
        assert!(
            run_summary["primary_failure"]
                .as_str()
                .unwrap()
                .contains("daemon/operator environment")
        );
        Ok(())
    }

    #[test]
    fn worktree_setup_runs_in_worker_and_integration_worktrees() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        fs::write(
            repo.path().join(".gitignore"),
            format!(
                "{}\nnode_modules/\n",
                fs::read_to_string(repo.path().join(".gitignore"))?
            ),
        )?;
        let config = WorkflowConfig {
            worktree_setup: vec![VerifyCommand {
                command: "mkdir -p node_modules/.bin && printf '#!/bin/sh\\nexit 0\\n' > node_modules/.bin/local-tool && chmod +x node_modules/.bin/local-tool".to_string(),
                timeout_seconds: 30,
                ..VerifyCommand::default()
            }],
            ..WorkflowConfig::default()
        };
        artifact::write_json(store.config_path(), &config)?;
        let mut first = slice("slice-001");
        first.verify = vec!["./node_modules/.bin/local-tool && test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add setup slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.integration_gate.status, "passed");
        let setup_commands: Vec<_> = summary
            .economics
            .command_executions
            .iter()
            .filter(|command| command.phase == "worktree_setup")
            .collect();
        assert_eq!(setup_commands.len(), 3);
        assert!(setup_commands.iter().all(|command| !command.cache_hit));
        Ok(())
    }

    #[test]
    fn verification_precommand_change_blocks_slice_retry_and_integration_repair() {
        let failure_kind = "verification_precommand_changed".to_string();
        let check = CheckResult {
            slice_id: "slice-001".to_string(),
            status: "failed".to_string(),
            summary: "workspace changed before verification command".to_string(),
            tests_run: Vec::new(),
            verification_commands: Vec::new(),
            findings: Vec::new(),
            attempt: 1,
            worker_head: String::new(),
            worktree_ok: false,
            commit_found: true,
            verification_cancelled: false,
            failure_kind: failure_kind.clone(),
        };
        assert!(check_failure_needs_operator(&check));
        assert_eq!(
            worker_attempt_retry_disposition(1, &check),
            "operator_intervention_required"
        );

        let gate = GateResult {
            status: "failed".to_string(),
            summary: "workspace changed before integration verification".to_string(),
            verification_cancelled: false,
            failure_kind: String::new(),
            verification_workspace: None,
            commands: vec![GateCommandResult {
                command: "cargo test".to_string(),
                status: "failed".to_string(),
                exit_code: None,
                output: "command was not started".to_string(),
                cwd: ".".to_string(),
                dedupe_key: String::new(),
                duration_ms: 0,
                cache_hit: false,
                skip_reason: String::new(),
                failure_kind,
                verification_workspace: None,
            }],
            findings: Vec::new(),
            approved_workspace: None,
            publication_identity: Vec::new(),
        };
        assert!(!should_run_integration_repair(RepairPolicy::Auto, &gate));
        assert!(!should_run_integration_repair(RepairPolicy::Always, &gate));
    }

    #[test]
    fn integration_gate_operator_environment_failure_blocks_without_repair() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        fs::write(
            repo.path().join(".gitignore"),
            format!(
                "{}\nnode_modules/\n",
                fs::read_to_string(repo.path().join(".gitignore"))?
            ),
        )?;
        let mut first = slice("slice-001");
        first.verify = vec!["./node_modules/.bin/local-tool && test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(
            repo.path(),
            &["commit", "-m", "add missing integration tool slice"],
        )?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager =
            Manager::with_runner(paths, state.clone(), Arc::new(DependencyInstallingRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let blocked = wait_for_run(&state, &run.id)?;
        assert_eq!(blocked.status, RunStatus::Blocked);
        assert!(blocked.error.contains("operator environment fix"));
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.integration_gate.status, "failed");
        assert_eq!(
            summary.integration_gate.commands[0].failure_kind,
            "tool_missing"
        );
        assert_eq!(summary.integration_gate.findings[0].action, "operator-fix");
        assert_eq!(summary.integration_repair.status, "skipped");
        assert_eq!(
            summary.integration_repair.trigger,
            "operator_fix_gate_failed"
        );
        assert_eq!(summary.economics.agent_call_count, 1);
        assert!(
            summary
                .economics
                .agent_calls
                .iter()
                .all(|call| call.phase != "integration_repair")
        );
        assert_eq!(summary.exit_states.run, "blocked");
        assert_eq!(summary.exit_states.evidence, "daemon_blocked");
        assert_eq!(summary.evidence_attestation.status, "daemon_blocked");
        Ok(())
    }

    #[test]
    fn dirty_source_repo_requires_explicit_allow_dirty() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;
        fs::write(repo.path().join("dirty.txt"), "uncommitted\n")?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state, Arc::new(FakeRunner));
        let err = manager
            .start_run(StartOptions {
                repo_path: repo.path().to_path_buf(),
                slice_ids: vec!["slice-001".to_string()],
                all: false,
                agent: "fake".to_string(),
                pi_bin: String::new(),
                pi_args: Vec::new(),
                native_pi_tui_worker: false,
                parallelism: 1,
                allow_dirty: false,
                origin_notification_target: String::new(),
                mission_envelope: None,
            })
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("source repo has uncommitted changes")
        );
        assert!(err.to_string().contains("dirty.txt"));
        Ok(())
    }

    #[test]
    fn completion_publication_preserves_dirty_operator_checkout() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        fs::write(repo.path().join("staged.txt"), "base staged\n")?;
        fs::write(repo.path().join("unstaged.txt"), "base unstaged\n")?;
        fs::write(repo.path().join("mixed.txt"), "base mixed\n")?;
        gitutil::run(repo.path(), &["add", "."])?;
        gitutil::run(
            repo.path(),
            &["commit", "-m", "add slice and operator files"],
        )?;

        fs::write(repo.path().join("staged.txt"), "stash seed\n")?;
        gitutil::run(repo.path(), &["stash", "push", "-m", "operator backup"])?;

        fs::write(repo.path().join("staged.txt"), "operator staged\n")?;
        gitutil::run(repo.path(), &["add", "staged.txt"])?;
        fs::write(repo.path().join("unstaged.txt"), "operator unstaged\n")?;
        fs::write(repo.path().join("mixed.txt"), "operator mixed index\n")?;
        gitutil::run(repo.path(), &["add", "mixed.txt"])?;
        fs::write(repo.path().join("mixed.txt"), "operator mixed worktree\n")?;
        fs::write(repo.path().join("untracked.txt"), "operator untracked\n")?;

        let operator_head = gitutil::head_sha(repo.path())?;
        let operator_branch = gitutil::current_branch(repo.path())?;
        let operator_status = gitutil::status_porcelain(repo.path())?;
        let operator_index = gitutil::run(repo.path(), &["write-tree"])?;
        let operator_head_reflog =
            gitutil::run(repo.path(), &["reflog", "show", "--format=%H:%gs", "HEAD"])?;
        let stash_ref = gitutil::run(repo.path(), &["rev-parse", "refs/stash"])?;
        let stash_reflog = gitutil::run(
            repo.path(),
            &["reflog", "show", "--format=%H:%gs", "refs/stash"],
        )?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: true,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let integration_head = gitutil::run(repo.path(), &["rev-parse", &run.integration_branch])?;
        assert_ne!(integration_head, operator_head);
        assert_eq!(
            gitutil::run(
                repo.path(),
                &["show", "-s", "--format=%s", &run.integration_branch],
            )?,
            format!("khazad(run): publish completion {}", run.id)
        );

        assert_eq!(gitutil::head_sha(repo.path())?, operator_head);
        assert_eq!(gitutil::current_branch(repo.path())?, operator_branch);
        assert_eq!(gitutil::status_porcelain(repo.path())?, operator_status);
        assert_eq!(gitutil::run(repo.path(), &["write-tree"])?, operator_index);
        assert_eq!(
            gitutil::run(repo.path(), &["reflog", "show", "--format=%H:%gs", "HEAD"])?,
            operator_head_reflog
        );
        assert_eq!(
            gitutil::run(repo.path(), &["rev-parse", "refs/stash"])?,
            stash_ref
        );
        assert_eq!(
            gitutil::run(
                repo.path(),
                &["reflog", "show", "--format=%H:%gs", "refs/stash"],
            )?,
            stash_reflog
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("staged.txt"))?,
            "operator staged\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("unstaged.txt"))?,
            "operator unstaged\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("mixed.txt"))?,
            "operator mixed worktree\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("untracked.txt"))?,
            "operator untracked\n"
        );
        Ok(())
    }

    #[test]
    fn repair_authority_flags_workflow_policy_and_out_of_area_paths() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let base = gitutil::head_sha(repo.path())?;
        fs::create_dir_all(repo.path().join("src"))?;
        fs::create_dir_all(repo.path().join("docs"))?;
        fs::create_dir_all(repo.path().join(".workflow"))?;
        fs::write(repo.path().join("src/authorized.rs"), "pub fn ok() {}\n")?;
        fs::write(repo.path().join("docs/out-of-area.md"), "not authorized\n")?;
        fs::write(
            repo.path().join(".workflow/khazad.json"),
            "{\"integration_repair\":\"auto\"}\n",
        )?;
        gitutil::run(repo.path(), &["add", "."])?;
        gitutil::run(repo.path(), &["commit", "-m", "repair attempt"])?;
        let head = gitutil::head_sha(repo.path())?;
        let mut authorized = slice("slice-001");
        authorized.areas = vec!["src".to_string()];

        let violations = repair_authority_violations(repo.path(), &base, &head, &[authorized])?;
        assert!(violations.contains(&"docs/out-of-area.md".to_string()));
        assert!(violations.contains(&".workflow/khazad.json".to_string()));
        assert!(!violations.contains(&"src/authorized.rs".to_string()));
        Ok(())
    }

    #[test]
    fn changed_files_outside_slice_areas_block_worker() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.areas = vec!["src".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add scoped slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let failed = wait_for_run(&state, &run.id)?;
        assert_eq!(failed.status, RunStatus::Failed);
        assert!(
            failed.error.contains("outside slice areas"),
            "unexpected failure: {}",
            failed.error
        );
        assert!(failed.error.contains("slice-001.txt"));
        let launches = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        let check: CheckResult = artifact::read_json(
            store.output_path(&run.id, &format!("{}.check.json", launches[0].output_stem)),
        )?;
        assert_eq!(check.failure_kind, "scope_violation");
        Ok(())
    }

    #[test]
    fn missing_slice_close_record_blocks_completion_publication() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: true,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let failed = wait_for_run(&state, &run.id)?;
        assert_eq!(failed.status, RunStatus::Blocked);
        assert!(
            failed.error.contains("slice closure failed"),
            "unexpected failure: {}",
            failed.error
        );
        assert!(
            store
                .find_completion_publication(&run.id, "main", &["slice-001".to_string()])?
                .is_none(),
            "completion publication was created without the required close record"
        );
        let events = state.get_events(&run.id, 100)?;
        assert!(events.iter().any(|event| {
            event.typ == "run_incident"
                && event.payload["kind"].as_str() == Some("slice_close_missing")
        }));
        Ok(())
    }

    #[test]
    fn integration_repair_always_reruns_gate_and_uses_cache_for_noop() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let config = WorkflowConfig {
            integration_repair: "always".to_string(),
            ..WorkflowConfig::default()
        };
        artifact::write_json(store.config_path(), &config)?;
        let mut first = slice("slice-001");
        first.verify = vec!["test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice and config"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.integration_repair.status, "no-op");
        assert_eq!(
            summary.integration_repair.trigger,
            "policy_always_gate_passed"
        );
        assert_eq!(summary.integration_repair.attempts, 1);
        assert!(summary.pre_repair_integration_gate.is_some());
        assert_eq!(summary.integration_gate.status, "passed");
        assert_eq!(summary.economics.repair_policy, "always");
        assert_eq!(summary.economics.repair_attempts, 1);
        assert!(summary.economics.cache_hits >= 1);
        let repair_launches =
            state.list_worker_attempt_ledger(&run.id, INTEGRATION_REPAIR_SCOPE_ID)?;
        assert_eq!(repair_launches.len(), 1);
        let repair_launch = &repair_launches[0];
        assert_eq!(repair_launch.kind, "integration-repair");
        assert_eq!(repair_launch.worker_retry_ordinal, 0);
        assert_eq!(repair_launch.repair_ordinal, 1);
        assert_eq!(repair_launch.envelope_retry_ordinal, 0);
        assert_eq!(repair_launch.state, "succeeded");
        assert!(
            store
                .output_path(&run.id, &format!("{}.json", repair_launch.output_stem))
                .is_file()
        );
        assert!(!Path::new(&repair_launch.worktree).exists());
        assert!(summary.worker_attempts.iter().any(|attempt| {
            attempt.launch_id == repair_launch.launch_id
                && attempt.slice_id == INTEGRATION_REPAIR_SCOPE_ID
        }));
        assert!(
            state
                .get_slice_runs(&run.id)?
                .iter()
                .all(|slice_run| slice_run.slice_id != INTEGRATION_REPAIR_SCOPE_ID)
        );
        Ok(())
    }

    #[test]
    fn integration_repair_does_not_reset_its_persisted_budget() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("run-persisted-integration-repair", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let retained = state.allocate_run_worker_attempt(
            &run.id,
            INTEGRATION_REPAIR_SCOPE_ID,
            1,
            0,
            DEFAULT_REPAIR_ATTEMPTS,
            0,
            "integration-repair",
            repo.path(),
        )?;
        state.mark_worker_attempt_launched(retained.launch_id)?;
        state.finish_worker_attempt(
            retained.launch_id,
            "interrupted",
            "daemon restarted after repair launch",
        )?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let slices = vec![slice("slice-001")];
        let gate = GateResult {
            status: "failed".to_string(),
            summary: "gate still fails".to_string(),
            ..GateResult::default()
        };
        let config = WorkflowConfig::default();
        let economics = RunEconomicsRecorder::new("auto", true, 1, DEFAULT_REPAIR_ATTEMPTS);
        let cache = VerificationCommandCache::default();
        let cancel = CancellationToken::new();

        let error = manager
            .integration_repair(IntegrationRepairContext {
                run: &run,
                slices: &slices,
                integration_worktree: repo.path(),
                checks: &[],
                gate_failure: &gate,
                trigger: "gate_failed",
                cancel: &cancel,
                runner: Arc::new(FakeRunner),
                config: &config,
                economics,
                verification_cache: &cache,
            })
            .expect_err("a persisted repair launch must consume the retry budget");

        assert!(
            error.to_string().contains("repair budget exhausted"),
            "unexpected error: {error:#}"
        );
        let launches = state.list_worker_attempt_ledger(&run.id, INTEGRATION_REPAIR_SCOPE_ID)?;
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].launch_id, retained.launch_id);
        assert_eq!(launches[0].repair_ordinal, DEFAULT_REPAIR_ATTEMPTS);
        Ok(())
    }

    #[test]
    fn integration_repair_reuses_a_persisted_success_before_rerunning_the_gate() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let artifacts = ArtifactStore::new(repo.path());
        artifacts.ensure_layout()?;
        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let run = test_run("run-reuse-integration-repair", repo.path(), "slice-001")?;
        state.insert_run(&run)?;
        let retained = state.allocate_run_worker_attempt(
            &run.id,
            INTEGRATION_REPAIR_SCOPE_ID,
            1,
            0,
            1,
            0,
            "integration-repair",
            repo.path(),
        )?;
        state.mark_worker_attempt_launched(retained.launch_id)?;
        let prior_result = RepairResult {
            status: "no-op".to_string(),
            summary: "persisted repair already completed".to_string(),
            trigger: "policy_always_gate_passed".to_string(),
            attempts: 1,
            ..RepairResult::default()
        };
        artifact::write_json(
            artifacts.output_path(&run.id, &format!("{}.json", retained.output_stem)),
            &prior_result,
        )?;
        state.finish_worker_attempt(retained.launch_id, "succeeded", "")?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let slices = vec![slice("slice-001")];
        let gate = GateResult {
            status: "passed".to_string(),
            summary: "gate passed before the interrupted rerun".to_string(),
            ..GateResult::default()
        };
        let config = WorkflowConfig::default();
        let economics = RunEconomicsRecorder::new("always", true, 1, DEFAULT_REPAIR_ATTEMPTS);
        let cache = VerificationCommandCache::default();
        let cancel = CancellationToken::new();

        let reused = manager.integration_repair(IntegrationRepairContext {
            run: &run,
            slices: &slices,
            integration_worktree: repo.path(),
            checks: &[],
            gate_failure: &gate,
            trigger: "policy_always_gate_passed",
            cancel: &cancel,
            runner: Arc::new(FakeRunner),
            config: &config,
            economics: economics.clone(),
            verification_cache: &cache,
        })?;

        assert_eq!(reused.status, prior_result.status);
        assert_eq!(reused.summary, prior_result.summary);
        assert_eq!(reused.attempts, prior_result.attempts);
        assert_eq!(economics.snapshot().repair_attempts, 1);
        let launches = state.list_worker_attempt_ledger(&run.id, INTEGRATION_REPAIR_SCOPE_ID)?;
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].launch_id, retained.launch_id);
        Ok(())
    }

    #[test]
    fn integration_repair_launch_merges_its_retained_branch_before_gate_rerun() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut config = WorkflowConfig::default();
        config.verify_profiles.insert(
            "repair-test".to_string(),
            VerifyProfile {
                commands: vec![VerifyCommand {
                    command: "test \"$(cat integration-fix.txt 2>/dev/null)\" = fixed".to_string(),
                    ..VerifyCommand::default()
                }],
            },
        );
        artifact::write_json(store.config_path(), &config)?;
        let mut first = slice("slice-001");
        first.areas = vec![
            "slice-001.txt".to_string(),
            "integration-fix.txt".to_string(),
        ];
        first.verify = vec!["test -f slice-001.txt".to_string()];
        first.verify_profile = "repair-test".to_string();
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(
            repo.path(),
            &["commit", "-m", "add integration repair fixture"],
        )?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(IntegrationFixRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(
            completed.status,
            RunStatus::Completed,
            "unexpected failure: {}",
            completed.error
        );
        let launches = state.list_worker_attempt_ledger(&run.id, INTEGRATION_REPAIR_SCOPE_ID)?;
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].state, "succeeded");
        gitutil::run(
            repo.path(),
            &[
                "show",
                &format!("{}:integration-fix.txt", completed.integration_branch),
            ],
        )?;
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.integration_repair.status, "fixed");
        assert_eq!(summary.integration_gate.status, "passed");
        Ok(())
    }

    #[test]
    fn integration_repair_never_preserves_failed_gate_evidence() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut config = WorkflowConfig {
            integration_repair: "never".to_string(),
            ..WorkflowConfig::default()
        };
        config.gate_fail_fast = true;
        artifact::write_json(store.config_path(), &config)?;
        let mut first = slice("slice-001");
        first.verify = vec!["git branch --show-current | grep /slice-001".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice and config"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Failed);
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.integration_repair.status, "skipped");
        assert_eq!(
            summary.integration_repair.trigger,
            "policy_never_gate_failed"
        );
        assert_eq!(summary.integration_gate.status, "failed");
        assert!(summary.pre_repair_integration_gate.is_none());
        assert_eq!(summary.economics.repair_policy, "never");
        assert_eq!(summary.economics.repair_attempts, 0);
        assert!(
            summary
                .economics
                .command_executions
                .iter()
                .any(|command| command.phase == "integration_gate" && command.status == "failed")
        );
        Ok(())
    }

    #[test]
    fn closed_dependency_is_satisfied_and_not_rerun() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        first.closed_by_run = "kd-prior".to_string();
        first.closed_at = Utc::now().to_rfc3339();
        let mut second = slice("slice-002");
        second.depends_on = vec!["slice-001".to_string()];
        second.verify = vec!["test -f slice-002.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        artifact::write_json(store.slices_dir().join("slice-002.json"), &second)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(
            repo.path(),
            &["commit", "-m", "add closed dependency and open slice"],
        )?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-002".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        assert_eq!(completed.selected_slice_id, "slice-002");
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs.len(), 1);
        assert_eq!(slice_runs[0].slice_id, "slice-002");
        assert_eq!(slice_runs[0].status, SliceStatus::Merged);
        let events = state.get_events(&run.id, 200)?;
        let started = events
            .iter()
            .find(|event| event.typ == "run_started")
            .expect("run_started event");
        assert_eq!(
            started.payload["skipped_closed_slices"],
            json!(["slice-001"])
        );
        Ok(())
    }

    #[test]
    fn explicitly_requested_closed_slice_is_rejected() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut closed = slice("slice-001");
        closed.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        closed.closed_by_run = "kd-prior".to_string();
        closed.closed_at = Utc::now().to_rfc3339();
        artifact::write_json(store.slices_dir().join("slice-001.json"), &closed)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add closed slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state, Arc::new(FakeRunner));
        let err = manager
            .start_run(StartOptions {
                repo_path: repo.path().to_path_buf(),
                slice_ids: vec!["slice-001".to_string()],
                all: false,
                agent: "fake".to_string(),
                pi_bin: String::new(),
                pi_args: Vec::new(),
                native_pi_tui_worker: false,
                parallelism: 1,
                allow_dirty: false,
                origin_notification_target: String::new(),
                mission_envelope: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("is closed"));
        Ok(())
    }

    #[test]
    fn repo_config_verify_profile_runs_with_env_and_cwd() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut env = BTreeMap::new();
        env.insert("KHAZAD_PROFILE".to_string(), "quick".to_string());
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "quick".to_string(),
            VerifyProfile {
                commands: vec![VerifyCommand {
                    command: "test \"$KHAZAD_PROFILE\" = quick && test -f slice-001.txt"
                        .to_string(),
                    timeout_seconds: 30,
                    cwd: String::new(),
                    env,
                }],
            },
        );
        artifact::write_json(
            store.config_path(),
            &WorkflowConfig {
                agent: "fake".to_string(),
                cockpit: CockpitMode::Auto,
                parallelism: 1,
                verify_timeout_seconds: 30,
                worker_attempt_timeout_seconds: 0,
                worker_question_timeout_seconds: 1800,
                worker_no_output_warning_seconds: 900,
                worker_termination_grace_seconds: 30,
                integration_repair: "auto".to_string(),
                gate_fail_fast: true,
                worktree_setup: Vec::new(),
                base_branch: String::new(),
                handoff: Default::default(),
                verify_profiles: profiles,
            },
        )?;
        let mut first = slice("slice-001");
        first.verify_profile = "quick".to_string();
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add configured slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: String::new(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 0,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        Ok(())
    }

    #[test]
    fn active_repo_run_blocks_second_run() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut first = slice("slice-001");
        first.verify = vec!["sleep 2 && test -f slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(FakeRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let err = manager
            .start_run(StartOptions {
                repo_path: repo.path().to_path_buf(),
                slice_ids: Vec::new(),
                all: true,
                agent: "fake".to_string(),
                pi_bin: String::new(),
                pi_args: Vec::new(),
                native_pi_tui_worker: false,
                parallelism: 1,
                allow_dirty: false,
                origin_notification_target: String::new(),
                mission_envelope: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("already has active run"));
        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        Ok(())
    }

    #[test]
    fn merge_conflicts_are_structured_blocked_artifacts() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        fs::write(repo.path().join("shared.txt"), "base\n")?;
        gitutil::run(
            repo.path(),
            &["add", ".gitignore", ".workflow", "shared.txt"],
        )?;
        gitutil::run(repo.path(), &["commit", "-m", "add shared file"])?;
        let base_branch = gitutil::current_branch(repo.path())?;
        let base_sha = gitutil::head_sha(repo.path())?;

        gitutil::run(repo.path(), &["checkout", "-b", "slice-one"])?;
        fs::write(repo.path().join("shared.txt"), "one\n")?;
        gitutil::run(repo.path(), &["add", "shared.txt"])?;
        gitutil::run(repo.path(), &["commit", "-m", "slice one"])?;

        gitutil::run(repo.path(), &["checkout", &base_branch])?;
        gitutil::run(repo.path(), &["checkout", "-b", "slice-two"])?;
        fs::write(repo.path().join("shared.txt"), "two\n")?;
        gitutil::run(repo.path(), &["add", "shared.txt"])?;
        gitutil::run(repo.path(), &["commit", "-m", "slice two"])?;

        gitutil::run(repo.path(), &["checkout", &base_branch])?;
        gitutil::merge(repo.path(), "slice-one", "merge slice one")?;
        let err = gitutil::merge(repo.path(), "slice-two", "merge slice two")
            .expect_err("second branch should conflict");

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(ConflictRunner));
        let now = Utc::now();
        let run = Run {
            id: "run-merge-conflict".to_string(),
            repo_id: "repo".to_string(),
            repo_path: repo.path().to_string_lossy().into_owned(),
            status: RunStatus::Running,
            base_branch: base_branch.clone(),
            base_sha,
            integration_branch: base_branch,
            selected_slice_id: String::new(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };
        let report = manager.write_merge_conflict_report(
            &run,
            &slice("slice-002"),
            "slice-two",
            repo.path(),
            &err,
        )?;

        assert_eq!(report.status, "blocked");
        assert_eq!(report.conflicted_files, vec!["shared.txt"]);
        assert!(
            store
                .output_path(&run.id, "slice-002.merge-conflict.json")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn worker_attempt_failure_sequence_uses_envelope_retry_and_targeted_repair() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut test_slice = slice("slice-001");
        test_slice.areas = vec!["slice-001.txt".to_string()];
        test_slice.verify = vec!["grep '^repaired$' slice-001.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &test_slice)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add repair slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths,
            state.clone(),
            Arc::new(EnvelopeScopeVerifyRepairRunner::default()),
        );
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(
            completed.status,
            RunStatus::Completed,
            "unexpected failure: {}",
            completed.error
        );
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs[0].status, SliceStatus::Merged);
        assert_eq!(slice_runs[0].attempts, 3);
        let worker_attempts = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        assert_eq!(worker_attempts.len(), 5);
        assert_eq!(
            worker_attempts
                .iter()
                .map(|launch| (
                    launch.worker_retry_ordinal,
                    launch.repair_ordinal,
                    launch.envelope_retry_ordinal,
                    launch.kind.as_str(),
                    launch.state.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (1, 0, 0, "slice-worker", "failed"),
                (1, 0, 1, "slice-envelope-retry", "failed"),
                (2, 0, 0, "slice-worker", "failed"),
                (3, 0, 0, "slice-worker", "failed"),
                (3, 1, 0, "slice-repair", "succeeded"),
            ]
        );
        assert_eq!(
            worker_attempts
                .iter()
                .map(|launch| launch.launch_id)
                .collect::<BTreeSet<_>>()
                .len(),
            worker_attempts.len()
        );
        assert_eq!(
            worker_attempts
                .iter()
                .map(|launch| launch.branch.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            worker_attempts.len()
        );
        assert_eq!(
            worker_attempts
                .iter()
                .map(|launch| launch.worktree.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            worker_attempts.len()
        );
        assert_eq!(
            worker_attempts
                .iter()
                .map(|launch| launch.output_stem.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            worker_attempts.len()
        );
        assert!(worker_attempts.iter().all(|launch| {
            store
                .handoff_dir(&run.id)
                .join(format!("{}.json", launch.output_stem))
                .exists()
        }));
        assert!(
            store
                .output_path(
                    &run.id,
                    &format!("{}.invalid-output.json", worker_attempts[0].output_stem),
                )
                .exists()
        );
        assert!(
            store
                .output_path(&run.id, &format!("{}.json", worker_attempts[1].output_stem))
                .exists()
        );
        assert!(
            store
                .output_path(
                    &run.id,
                    &format!("{}.check.json", worker_attempts[4].output_stem),
                )
                .exists()
        );
        let events = state.get_events(&run.id, 500)?;
        assert!(events.iter().any(|event| {
            event.typ == "worker_envelope_retry_succeeded"
                && event.payload["slice_id"].as_str() == Some("slice-001")
        }));
        assert!(events.iter().any(|event| {
            event.typ == "worker_attempt_failure"
                && event.payload["failure_kind"].as_str() == Some("invalid_worker_output")
        }));
        assert!(events.iter().any(|event| {
            event.typ == "worker_attempt_failure"
                && event.payload["failure_kind"].as_str() == Some("scope_violation")
                && event.payload["repair_disposition"].as_str()
                    == Some("scope_violation_requires_replan_grant")
        }));
        assert!(events.iter().any(|event| {
            event.typ == "worker_attempt_failure"
                && event.payload["failure_kind"].as_str() == Some("command_failed")
                && event.payload["repair_disposition"].as_str()
                    == Some("targeted_slice_repair_pending")
        }));
        assert!(events.iter().any(|event| {
            event.typ == "slice_repair_completed"
                && event.payload["status"].as_str() == Some("fixed")
        }));
        Ok(())
    }

    #[test]
    fn envelope_retries_build_on_the_preceding_retained_retry_branch() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        let mut test_slice = slice("slice-001");
        test_slice.areas = vec![
            "initial.txt".to_string(),
            "retry-1.txt".to_string(),
            "retry-2.txt".to_string(),
        ];
        test_slice.verify =
            vec!["test -f initial.txt && test -f retry-1.txt && test -f retry-2.txt".to_string()];
        artifact::write_json(store.slices_dir().join("slice-001.json"), &test_slice)?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add iterative retry slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths,
            state.clone(),
            Arc::new(IterativeEnvelopeRunner::default()),
        );
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(
            completed.status,
            RunStatus::Completed,
            "unexpected failure: {}",
            completed.error
        );
        let launches = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        assert_eq!(launches.len(), 3);
        assert_eq!(launches[1].envelope_retry_ordinal, 1);
        assert_eq!(launches[2].envelope_retry_ordinal, 2);
        gitutil::run(
            repo.path(),
            &[
                "merge-base",
                "--is-ancestor",
                &launches[1].branch,
                &launches[2].branch,
            ],
        )?;
        gitutil::run(
            repo.path(),
            &["show", &format!("{}:retry-1.txt", launches[2].branch)],
        )?;
        gitutil::run(
            repo.path(),
            &["show", &format!("{}:retry-2.txt", launches[2].branch)],
        )?;
        let success = state
            .get_events(&run.id, 500)?
            .into_iter()
            .find(|event| event.typ == "worker_envelope_retry_succeeded")
            .expect("successful second envelope retry event");
        let preceding_invalid_output = store.output_path(
            &run.id,
            &format!("{}.envelope-1.invalid-output.json", launches[1].output_stem),
        );
        assert_eq!(
            success.payload["previous_invalid_output"].as_str(),
            preceding_invalid_output.to_str()
        );
        Ok(())
    }

    #[test]
    fn invalid_worker_output_final_envelope_failure_preserves_terminal_artifacts() -> Result<()> {
        let repo = tempfile::tempdir()?;
        init_git_repo(repo.path())?;
        let store = ArtifactStore::new(repo.path());
        store.ensure_layout()?;
        artifact::write_json(
            store.slices_dir().join("slice-001.json"),
            &slice("slice-001"),
        )?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add invalid slice"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(
            paths,
            state.clone(),
            Arc::new(AlwaysInvalidEnvelopeRunner::default()),
        );
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: vec!["slice-001".to_string()],
            all: false,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            native_pi_tui_worker: false,
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
            mission_envelope: None,
        })?;

        let failed = wait_for_run(&state, &run.id)?;
        assert_eq!(failed.status, RunStatus::Failed);
        let worker_attempts = state.list_worker_attempt_ledger(&run.id, "slice-001")?;
        assert_eq!(
            worker_attempts.len(),
            MAX_WORKER_ATTEMPTS * 3,
            "unexpected failure: {}",
            failed.error
        );
        assert!(
            worker_attempts
                .iter()
                .all(|launch| launch.state == "failed")
        );
        let final_launch = worker_attempts
            .iter()
            .find(|launch| {
                launch.worker_retry_ordinal == MAX_WORKER_ATTEMPTS
                    && launch.envelope_retry_ordinal == DEFAULT_WORKER_ENVELOPE_RETRY_ATTEMPTS
            })
            .expect("final envelope retry launch");
        assert!(
            store
                .output_path(
                    &run.id,
                    &format!(
                        "{}.envelope-{}.invalid-output.json",
                        final_launch.output_stem, DEFAULT_WORKER_ENVELOPE_RETRY_ATTEMPTS
                    ),
                )
                .exists()
        );
        let events = state.get_events(&run.id, 500)?;
        let invalid_events = events
            .iter()
            .filter(|event| event.typ == "invalid_worker_output")
            .count();
        assert_eq!(invalid_events, MAX_WORKER_ATTEMPTS * 3);
        assert!(events.iter().any(|event| {
            event.typ == "worker_attempt_failure"
                && event.payload["retry_disposition"].as_str() == Some("envelope_retry_exhausted")
                && event.payload["attempt"].as_u64() == Some(3)
        }));
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs[0].status, SliceStatus::Failed);
        assert!(slice_runs[0].last_error.contains("did not become ready"));
        Ok(())
    }

    fn terminal_notification_records(
        store: &ArtifactStore,
        run_id: &str,
    ) -> Result<Vec<TerminalNotificationRecord>> {
        let dir = store.notifications_dir(run_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("terminal-") && name.ends_with(".json") {
                records.push(artifact::read_json(entry.path())?);
            }
        }
        Ok(records)
    }

    fn terminal_notification_record_for_status(
        store: &ArtifactStore,
        run_id: &str,
        status: &str,
    ) -> Result<TerminalNotificationRecord> {
        terminal_notification_records(store, run_id)?
            .into_iter()
            .find(|record| record.terminal_status == status)
            .ok_or_else(|| anyhow::anyhow!("missing terminal notification for {status}"))
    }

    fn wait_for_event(
        state: &StateStore,
        run_id: &str,
        event_type: &str,
    ) -> Result<Vec<crate::domain::Event>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = state.get_events(run_id, 100)?;
            if events.iter().any(|event| event.typ == event_type) {
                return Ok(events);
            }
            assert!(
                Instant::now() < deadline,
                "run {run_id} did not record event {event_type:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_run(state: &StateStore, run_id: &str) -> Result<crate::domain::Run> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let run = state.get_run(run_id)?.expect("run exists");
            if matches!(
                run.status,
                RunStatus::Completed
                    | RunStatus::Failed
                    | RunStatus::Blocked
                    | RunStatus::Cancelled
                    | RunStatus::Interrupted
            ) && !state.terminal_transition_needs_reconciliation(run_id)?
            {
                let cleanup_settled = state.get_events(run_id, 50)?.iter().any(|event| {
                    event.typ == super::workflow_events::WORKTREES_CLEANED
                        || event.typ == "worktree_cleanup_error"
                });
                if cleanup_settled {
                    return Ok(run);
                }
            }
            assert!(Instant::now() < deadline, "run did not finish: {run:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn init_git_repo(path: &Path) -> Result<()> {
        gitutil::run(path, &["init"])?;
        gitutil::run(path, &["config", "user.email", "test@example.com"])?;
        gitutil::run(path, &["config", "user.name", "Test User"])?;
        fs::write(path.join("README.md"), "fixture\n")?;
        gitutil::run(path, &["add", "README.md"])?;
        gitutil::run(path, &["commit", "-m", "initial"])?;
        Ok(())
    }

    struct IntegrationFixRunner;

    impl Runner for IntegrationFixRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "integration-repair" {
                fs::write(job.cwd.join("integration-fix.txt"), "fixed\n")?;
                gitutil::run(&job.cwd, &["add", "integration-fix.txt"])?;
                gitutil::run(&job.cwd, &["commit", "-m", "fix integration gate"])?;
                return Ok(ResultData {
                    output: Some(json!({
                        "status": "fixed",
                        "summary": "added the authorized integration evidence",
                        "findings": [],
                        "finding_dispositions": []
                    })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            fs::write(job.cwd.join("slice-001.txt"), "implemented\n")?;
            gitutil::run(&job.cwd, &["add", "slice-001.txt"])?;
            gitutil::run(&job.cwd, &["commit", "-m", "implement slice"])?;
            Ok(ResultData {
                output: Some(valid_worker_output(&handoff, &job.cwd)?),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    struct ReadySiblingFailRunner;

    impl Runner for ReadySiblingFailRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            if handoff.slice.id == "slice-001" {
                thread::sleep(Duration::from_millis(200));
            }
            fs::write(
                job.cwd.join(format!("{}.txt", handoff.slice.id)),
                format!("{}\n", handoff.slice.id),
            )?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            Ok(ResultData {
                output: Some(valid_worker_output(&handoff, &job.cwd)?),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct EnvelopeScopeVerifyRepairRunner {
        worker_calls: AtomicUsize,
    }

    impl Runner for EnvelopeScopeVerifyRepairRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            if job.kind == "slice-envelope-retry" {
                return Ok(ResultData {
                    output: Some(valid_worker_output(&handoff, &job.cwd)?),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            if job.kind == "slice-repair" {
                fs::write(job.cwd.join("slice-001.txt"), "repaired\n")?;
                gitutil::run(&job.cwd, &["add", "-A"])?;
                gitutil::run(&job.cwd, &["commit", "-m", "targeted slice repair"])?;
                return Ok(ResultData {
                    output: Some(valid_worker_output(&handoff, &job.cwd)?),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let call = self.worker_calls.fetch_add(1, Ordering::SeqCst) + 1;
            match call {
                1 => {
                    fs::write(job.cwd.join("slice-001.txt"), "scope violation attempt\n")?;
                    fs::write(job.cwd.join("outside.txt"), "outside area\n")?;
                    gitutil::run(&job.cwd, &["add", "-A"])?;
                    gitutil::run(&job.cwd, &["commit", "-m", "invalid envelope scope work"])?;
                    Ok(ResultData {
                        output: Some(json!({
                            "slice_id": handoff.slice.id,
                            "summary": "missing status forces envelope retry"
                        })),
                        usage: Usage::default(),
                        contract_warnings: Vec::new(),
                    })
                }
                2 => {
                    let _ = fs::remove_file(job.cwd.join("outside.txt"));
                    fs::write(job.cwd.join("slice-001.txt"), "broken\n")?;
                    gitutil::run(&job.cwd, &["add", "-A"])?;
                    gitutil::run(&job.cwd, &["commit", "-m", "repair scope only"])?;
                    Ok(ResultData {
                        output: Some(valid_worker_output(&handoff, &job.cwd)?),
                        usage: Usage::default(),
                        contract_warnings: Vec::new(),
                    })
                }
                _ => {
                    fs::write(job.cwd.join("slice-001.txt"), "still failing\n")?;
                    gitutil::run(&job.cwd, &["add", "-A"])?;
                    gitutil::run(&job.cwd, &["commit", "-m", "still failing verify"])?;
                    Ok(ResultData {
                        output: Some(valid_worker_output(&handoff, &job.cwd)?),
                        usage: Usage::default(),
                        contract_warnings: Vec::new(),
                    })
                }
            }
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct IterativeEnvelopeRunner {
        envelope_calls: AtomicUsize,
    }

    impl Runner for IterativeEnvelopeRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            if job.kind == "slice-worker" {
                fs::write(job.cwd.join("initial.txt"), "initial\n")?;
                gitutil::run(&job.cwd, &["add", "-A"])?;
                gitutil::run(&job.cwd, &["commit", "-m", "initial malformed attempt"])?;
                return Ok(ResultData {
                    output: Some(json!({
                        "slice_id": handoff.slice.id,
                        "summary": "missing status forces first envelope retry"
                    })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let retry = self.envelope_calls.fetch_add(1, Ordering::SeqCst) + 1;
            match retry {
                1 => {
                    fs::write(job.cwd.join("retry-1.txt"), "first retained retry\n")?;
                    gitutil::run(&job.cwd, &["add", "-A"])?;
                    gitutil::run(&job.cwd, &["commit", "-m", "first retained retry"])?;
                    Ok(ResultData {
                        output: Some(json!({
                            "slice_id": handoff.slice.id,
                            "summary": "still missing status"
                        })),
                        usage: Usage::default(),
                        contract_warnings: Vec::new(),
                    })
                }
                2 => {
                    if !job.cwd.join("retry-1.txt").is_file() {
                        anyhow::bail!("second envelope retry lost first retry evidence");
                    }
                    fs::write(job.cwd.join("retry-2.txt"), "second iterative retry\n")?;
                    gitutil::run(&job.cwd, &["add", "-A"])?;
                    gitutil::run(&job.cwd, &["commit", "-m", "second iterative retry"])?;
                    let mut output = valid_worker_output(&handoff, &job.cwd)?;
                    output["changed_files"] = json!(["initial.txt", "retry-1.txt", "retry-2.txt"]);
                    Ok(ResultData {
                        output: Some(output),
                        usage: Usage::default(),
                        contract_warnings: Vec::new(),
                    })
                }
                _ => anyhow::bail!("unexpected extra envelope retry {retry}"),
            }
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct AlwaysInvalidEnvelopeRunner {
        calls: AtomicUsize,
    }

    impl Runner for AlwaysInvalidEnvelopeRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "slice-worker" {
                let handoff_path = handoff_path_from_prompt(&job.prompt)?;
                let handoff: Handoff = artifact::read_json(&handoff_path)?;
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                fs::write(
                    job.cwd.join(format!("{}-{call}.txt", handoff.slice.id)),
                    format!("invalid {call}\n"),
                )?;
                gitutil::run(&job.cwd, &["add", "-A"])?;
                gitutil::run(&job.cwd, &["commit", "-m", &format!("invalid {call}")])?;
            }
            Ok(ResultData {
                output: Some(json!({"slice_id": "slice-001", "summary": "still invalid"})),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    fn valid_worker_output(handoff: &Handoff, cwd: &Path) -> Result<Value> {
        let sha = gitutil::head_sha(cwd)?;
        Ok(json!({
            "slice_id": handoff.slice.id,
            "status": "complete",
            "summary": "worker output valid for current head",
            "commit_sha": sha,
            "changed_files": [format!("{}.txt", handoff.slice.id)],
            "acceptance_status": acceptance_status_json(&handoff.slice)
        }))
    }

    struct SharedFileAppendRunner;

    impl Runner for SharedFileAppendRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "integration-repair" {
                return Ok(ResultData {
                    output: Some(json!({ "status": "no-op", "summary": "no repair needed" })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            let shared_path = job.cwd.join("shared.txt");
            let mut content = fs::read_to_string(&shared_path).unwrap_or_default();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("{}\n", handoff.slice.id));
            fs::write(&shared_path, content)?;
            gitutil::run(&job.cwd, &["add", "shared.txt"])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("append {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            Ok(ResultData {
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "appended shared file",
                    "commit_sha": sha,
                    "changed_files": ["shared.txt"],
                    "acceptance_status": acceptance_status_json(&handoff.slice)
                })),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    struct FakeRunner;

    impl Runner for FakeRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "integration-repair" {
                return Ok(ResultData {
                    output: Some(json!({ "status": "no-op", "summary": "no repair needed" })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            fs::write(
                job.cwd.join(format!("{}.txt", handoff.slice.id)),
                format!("{}\n", handoff.slice.id),
            )?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            let acceptance_status = acceptance_status_json(&handoff.slice);
            Ok(ResultData {
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "implemented",
                    "commit_sha": sha,
                    "changed_files": [format!("{}.txt", handoff.slice.id)],
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

    struct FollowupEmittingRunner;

    impl Runner for FollowupEmittingRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "integration-repair" {
                return Ok(ResultData {
                    output: Some(json!({ "status": "no-op", "summary": "no repair needed" })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            fs::create_dir_all(job.cwd.join("src"))?;
            let rel_path = format!("src/{}.txt", handoff.slice.id);
            fs::write(job.cwd.join(&rel_path), format!("{}\n", handoff.slice.id))?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            let mut output = json!({
                "slice_id": handoff.slice.id,
                "status": "complete",
                "summary": "implemented with bounded follow-up" ,
                "commit_sha": sha,
                "changed_files": [rel_path],
                "acceptance_status": acceptance_status_json(&handoff.slice)
            });
            if handoff.slice.id == "slice-001" {
                let mut draft = followup_draft("slice-001-followup");
                draft.verify = vec!["test -f src/slice-001-followup.txt".to_string()];
                output["candidate_followup_slices"] = serde_json::to_value(vec![draft])?;
            }
            Ok(ResultData {
                output: Some(output),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "fake"
        }
    }

    struct TwoFollowupsRunner;

    impl Runner for TwoFollowupsRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "integration-repair" {
                return Ok(ResultData {
                    output: Some(json!({ "status": "no-op", "summary": "no repair needed" })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            fs::create_dir_all(job.cwd.join("src"))?;
            let rel_path = format!("src/{}.txt", handoff.slice.id);
            fs::write(job.cwd.join(&rel_path), format!("{}\n", handoff.slice.id))?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            let mut output = json!({
                "slice_id": handoff.slice.id,
                "status": "complete",
                "summary": "implemented with two bounded follow-ups" ,
                "commit_sha": sha,
                "changed_files": [rel_path],
                "acceptance_status": acceptance_status_json(&handoff.slice)
            });
            if handoff.slice.id == "slice-001" {
                let mut first = followup_draft("slice-001-followup-a");
                first.title = "Follow-up A".to_string();
                first.goal = "Complete follow-up A work".to_string();
                first.verify = vec!["test -f src/slice-001-followup-a.txt".to_string()];
                let mut second = followup_draft("slice-001-followup-b");
                second.title = "Follow-up B".to_string();
                second.goal = "Complete follow-up B work".to_string();
                second.verify = vec!["test -f src/slice-001-followup-b.txt".to_string()];
                output["candidate_followup_slices"] = serde_json::to_value(vec![first, second])?;
            }
            Ok(ResultData {
                output: Some(output),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "two-followups"
        }
    }

    struct ReplanRecordingRunner {
        state: StateStore,
    }

    impl Runner for ReplanRecordingRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            let run_id = job
                .env
                .get("KHAZAD_RUN_ID")
                .ok_or_else(|| anyhow::anyhow!("missing KHAZAD_RUN_ID"))?;
            let accepted =
                create_test_replan_proposal(&self.state, run_id, &handoff.slice.id, "accepted")?;
            self.state.decide_replan_proposal(
                run_id,
                &accepted.id,
                ReplanProposalState::Accepted,
                "operator accepted fixture",
                "test-authorizer",
                "test-runner",
                "",
                "",
            )?;
            let rejected =
                create_test_replan_proposal(&self.state, run_id, &handoff.slice.id, "rejected")?;
            self.state.decide_replan_proposal(
                run_id,
                &rejected.id,
                ReplanProposalState::Rejected,
                "duplicate proposal",
                "test-authorizer",
                "test-runner",
                "",
                "",
            )?;
            let deferred =
                create_test_replan_proposal(&self.state, run_id, &handoff.slice.id, "deferred")?;
            self.state.decide_replan_proposal(
                run_id,
                &deferred.id,
                ReplanProposalState::Deferred,
                "not now",
                "test-authorizer",
                "test-runner",
                "",
                "after release",
            )?;
            let superseded =
                create_test_replan_proposal(&self.state, run_id, &handoff.slice.id, "superseded")?;
            self.state.decide_replan_proposal(
                run_id,
                &superseded.id,
                ReplanProposalState::Superseded,
                "newer proposal exists",
                "test-authorizer",
                "test-runner",
                "rp-replacement",
                "",
            )?;
            fs::write(
                job.cwd.join(format!("{}.txt", handoff.slice.id)),
                format!("{}\n", handoff.slice.id),
            )?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            let acceptance_status = acceptance_status_json(&handoff.slice);
            Ok(ResultData {
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "implemented with plan revision history",
                    "commit_sha": sha,
                    "changed_files": [format!("{}.txt", handoff.slice.id)],
                    "acceptance_status": acceptance_status
                })),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "replan-recording"
        }
    }

    fn create_test_replan_proposal(
        state: &StateStore,
        run_id: &str,
        slice_id: &str,
        label: &str,
    ) -> Result<crate::domain::ReplanProposal> {
        state.create_replan_proposal(
            run_id,
            "",
            ReplanProposalSource {
                kind: format!("fixture-{label}"),
                slice_id: slice_id.to_string(),
                phase: "test".to_string(),
                attempt: 1,
                summary: format!("{label} fixture proposal"),
            },
            vec![format!("finding-{label}")],
            vec![ReplanEvidenceLink {
                kind: "worker_output".to_string(),
                path: format!(".workflow/runs/{run_id}/outputs/{slice_id}.worker.json"),
                event_id: 0,
                summary: format!("{label} evidence"),
            }],
            vec![ReplanProposedChange {
                kind: "queue_revision".to_string(),
                target: slice_id.to_string(),
                summary: format!("{label} proposed queue change"),
            }],
            "intent_affecting",
        )
    }

    struct DependencyInstallingRunner;

    impl Runner for DependencyInstallingRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            let bin_dir = job.cwd.join("node_modules/.bin");
            fs::create_dir_all(&bin_dir)?;
            let tool = bin_dir.join("local-tool");
            fs::write(&tool, "#!/bin/sh\nexit 0\n")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&tool)?.permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&tool, permissions)?;
            }
            fs::write(
                job.cwd.join(format!("{}.txt", handoff.slice.id)),
                format!("{}\n", handoff.slice.id),
            )?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            let acceptance_status = acceptance_status_json(&handoff.slice);
            Ok(ResultData {
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "implemented with local tool side effect",
                    "commit_sha": sha,
                    "changed_files": [format!("{}.txt", handoff.slice.id)],
                    "acceptance_status": acceptance_status
                })),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "dependency-installing-fake"
        }
    }

    struct ConflictRunner;

    impl Runner for ConflictRunner {
        fn run(
            &self,
            job: Job,
            cancel: CancellationToken,
            _events: Option<RunnerEventSink>,
        ) -> Result<ResultData> {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            if job.kind == "integration-repair" {
                return Ok(ResultData {
                    output: Some(json!({ "status": "no-op", "summary": "no repair needed" })),
                    usage: Usage::default(),
                    contract_warnings: Vec::new(),
                });
            }
            let handoff_path = handoff_path_from_prompt(&job.prompt)?;
            let handoff: Handoff = artifact::read_json(&handoff_path)?;
            fs::write(
                job.cwd.join("shared.txt"),
                format!("{}\n", handoff.slice.id),
            )?;
            gitutil::run(&job.cwd, &["add", "."])?;
            gitutil::run(
                &job.cwd,
                &["commit", "-m", &format!("implement {}", handoff.slice.id)],
            )?;
            let sha = gitutil::head_sha(&job.cwd)?;
            let acceptance_status = acceptance_status_json(&handoff.slice);
            Ok(ResultData {
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "implemented conflicting shared file",
                    "commit_sha": sha,
                    "changed_files": ["shared.txt"],
                    "acceptance_status": acceptance_status
                })),
                usage: Usage::default(),
                contract_warnings: Vec::new(),
            })
        }

        fn name(&self) -> &str {
            "conflict"
        }
    }

    fn acceptance_status_json(slice: &Slice) -> serde_json::Value {
        json!(
            slice
                .acceptance
                .iter()
                .map(|criterion| json!({
                    "criterion": criterion,
                    "status": "satisfied",
                    "evidence": format!("{} implemented", slice.id),
                }))
                .collect::<Vec<_>>()
        )
    }

    fn handoff_path_from_prompt(prompt: &str) -> Result<String> {
        let mut lines = prompt.lines();
        while let Some(line) = lines.next() {
            if line.trim() == "Read this handoff JSON first:" {
                return lines
                    .next()
                    .map(|line| line.trim().to_string())
                    .ok_or_else(|| anyhow::anyhow!("missing handoff path"));
            }
        }
        anyhow::bail!("handoff path not found")
    }
}
