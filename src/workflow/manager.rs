use super::attention::{
    OperatorAttention, ReplanDecisionPending, TerminalTransitionNotification,
    WorkerPaneTerminalRename,
};
use super::cockpit::{
    CockpitLaunch, CockpitWorkerLaunch, CockpitWorkerPaneRequest, open_default_run_cockpit,
    open_default_worker_pane, take_cockpit_mode_transport_arg, worker_activity_pane_command,
};
use super::economics::{RunEconomicsRecorder, agent_call};
use super::events as workflow_events;
use super::gate::{
    IntegrationGateRequest, SliceVerificationRequest, VerificationCommandCache, WorkflowGate,
    WorktreeSetupRequest, failure_kind_needs_operator,
};
use super::read_model::authorized_paths_from_proposal;
use super::{
    CancelledError, REPAIR_RESULT_SCHEMA, RunReadModel, RunReadModelBuilder, RunReadModelOptions,
    WORKER_RESULT_SCHEMA, check_cancelled, integration_repair_prompt, worker_prompt,
};
use crate::agent::{
    CancellationToken, Job, PiCommandSpec, PiWrapperArtifacts, Runner, RunnerError, RunnerEvent,
    RunnerEventSink, RunnerLaunchFailure, RunnerMetadata, RunnerTranscript,
    collect_pi_wrapper_result, prepare_pi_wrapper_artifacts, runner_from_spec,
    wait_for_pi_wrapper_launch, worker_evidence_kind_for_runner, worker_evidence_label_for_runner,
};
use crate::agent_profile::{ProfileResolveInput, resolve_effective_worker_profile};
use crate::artifact;
use crate::domain::{
    AgentProfilesConfig, BranchHandoff, CheckResult, CockpitMode, EvidenceAttestation, Finding,
    FindingDisposition, GateResult, Handoff, HandoffActionResult, HandoffDiagnostics,
    ImplementationSummary, MergeConflictReport, OriginNotificationTarget, PlanRevisions,
    RepairResult, ReplanEvidenceLink, ReplanProposal, ReplanProposalSource, ReplanProposedChange,
    Run, RunCheckpoint, RunInspection, RunStatus, Slice, SliceExitState, SliceRun, SliceStatus,
    SliceValidationReport, SliceWriteResult, WorkerProfileEvidence, WorkerResult, WorkflowConfig,
    WorkflowExitStates, replan_decision_commands,
};
use crate::gitutil;
use crate::paths::{self, Paths};
use crate::state::{ProgressReporter, ProgressScope, Repo, Store as StateStore};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
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
pub const DEFAULT_REPAIR_ATTEMPTS: usize = 1;
static WORKTREE_ADD_LOCK: Mutex<()> = Mutex::new(());

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
    pub parallelism: usize,
    pub allow_dirty: bool,
    pub origin_notification_target: String,
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
    pub parallelism: usize,
}

#[derive(Debug, Clone, Copy)]
enum IntegrationMode {
    Fresh,
    Existing,
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
}

struct SupervisedWorkerJobResult {
    data: crate::agent::ResultData,
    operator_pause: Duration,
}

#[derive(Clone)]
struct WorkerExecutionContext {
    run: Run,
    root_worktree: PathBuf,
    slice_base_sha: String,
    dependency_summary: BTreeMap<String, String>,
    cancel: CancellationToken,
    runner: Arc<dyn Runner>,
    config: WorkflowConfig,
    cockpit_mode: CockpitMode,
    economics: RunEconomicsRecorder,
    verification_cache: VerificationCommandCache,
    worker_token: String,
}

#[derive(Debug, Clone)]
struct WorkerAttemptContext {
    run_id: String,
    phase: String,
    slice_id: String,
    attempt: usize,
    timeout_seconds: u64,
    no_output_warning_seconds: u64,
    termination_grace_seconds: u64,
}

struct AgentCallContext<'a> {
    phase: &'a str,
    slice_id: &'a str,
    attempt: usize,
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
    fn new(
        run_id: &str,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        config: &WorkflowConfig,
    ) -> Self {
        Self {
            run_id: run_id.to_string(),
            phase: phase.to_string(),
            slice_id: slice_id.to_string(),
            attempt,
            timeout_seconds: config.worker_attempt_timeout_seconds,
            no_output_warning_seconds: config.worker_no_output_warning_seconds,
            termination_grace_seconds: config.worker_termination_grace_seconds,
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
        let message = format!("awaiting replan decision for {ids} before {checkpoint}");
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

    fn notify_attention_for_replan(&self, run: &Run, proposal: &ReplanProposal) {
        OperatorAttention::new(self.state.clone())
            .replan_decision_pending(ReplanDecisionPending { run, proposal });
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
    ) -> Result<PathBuf> {
        let path = store.write_handoff(&run.id, handoff)?;
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

    fn record_cockpit_launch(&self, run: &Run, mode: CockpitMode) -> Result<()> {
        match open_default_run_cockpit(run, mode, &self.paths.root) {
            Ok(CockpitLaunch::Opened(opened)) => self.state.record_event(
                &run.id,
                workflow_events::COCKPIT_READY,
                &workflow_events::CockpitReadyPayload {
                    adapter: opened.adapter,
                    mode: opened.mode.as_str().to_string(),
                    workspace: opened.workspace_label,
                    panes: opened.pane_labels,
                    source_of_truth: "daemon_state".to_string(),
                    planner: "deferred_until_rpl_planner_authority".to_string(),
                },
            ),
            Ok(CockpitLaunch::SkippedDirect) => Ok(()),
            Err(unavailable) => self.state.record_event(
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
                .with_extra("source_of_truth", "daemon_state"),
            ),
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
                context.attempt,
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
    ) -> Result<SupervisedWorkerJobResult> {
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
    ) -> Result<SupervisedWorkerJobResult>
    where
        F: FnOnce(
            Job,
            CancellationToken,
            Option<RunnerEventSink>,
        ) -> Result<crate::agent::ResultData>,
    {
        job.termination_grace_seconds = context.termination_grace_seconds;
        let events = Some(self.worker_event_sink(&context));
        if context.timeout_seconds == 0 {
            return run_job(job, cancel.clone(), events).map(|data| SupervisedWorkerJobResult {
                data,
                operator_pause: Duration::ZERO,
            });
        }

        let attempt_cancel = CancellationToken::new();
        let timed_out = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let parent_cancel = cancel.clone();
        let timeout = Duration::from_secs(context.timeout_seconds);
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
                    .get_progress(&supervisor_context.run_id)
                    .ok()
                    .flatten()
                    .is_some_and(|progress| {
                        progress.phase == "awaiting_operator"
                            && progress.slice_id == supervisor_context.slice_id
                            && progress.attempt == supervisor_context.attempt
                    });
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
                if active_elapsed >= timeout {
                    timeout_flag.store(true, Ordering::SeqCst);
                    timeout_cancel.cancel();
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });

        let result = run_job(job, attempt_cancel, events);
        done.store(true, Ordering::SeqCst);
        let _ = supervisor.join();
        if timed_out.load(Ordering::SeqCst) {
            let message = format!(
                "worker attempt {} exceeded worker_attempt_timeout_seconds={}",
                context.attempt, context.timeout_seconds
            );
            self.state.record_event(
                &context.run_id,
                "worker_attempt_timeout",
                &workflow_events::WorkerAttemptTimeoutPayload::new(
                    &context.phase,
                    &context.slice_id,
                    context.attempt,
                    context.timeout_seconds,
                    &message,
                ),
            )?;
            bail!(message);
        }
        let operator_pause = operator_pause
            .lock()
            .map(|duration| *duration)
            .unwrap_or(Duration::ZERO);
        result.map(|data| SupervisedWorkerJobResult {
            data,
            operator_pause,
        })
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
        match self.run_supervised_worker_job(runner, job, cancel, context.clone()) {
            Ok(result) => {
                let duration = started_at.elapsed().saturating_sub(result.operator_pause);
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "succeeded",
                    duration,
                    result.operator_pause,
                    Some(&result.data.usage),
                    "",
                ));
                self.record_contract_warnings(
                    &context,
                    &runner_name,
                    &result.data.contract_warnings,
                );
                Ok(result.data)
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
                    started_at.elapsed(),
                    Duration::ZERO,
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
        match self.run_supervised_worker_job_with(
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
        ) {
            Ok(result) => {
                let duration = started_at.elapsed().saturating_sub(result.operator_pause);
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "succeeded",
                    duration,
                    result.operator_pause,
                    Some(&result.data.usage),
                    "",
                ));
                self.record_contract_warnings(
                    &context,
                    &runner_name,
                    &result.data.contract_warnings,
                );
                Ok(result.data)
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
                    started_at.elapsed(),
                    Duration::ZERO,
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
        match self.try_run_herdr_worker_job(
            &spec,
            &job,
            cancel.clone(),
            events.clone(),
            context,
            run,
            cockpit_mode,
            output_path,
        ) {
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
        let artifacts = PiWrapperArtifacts::for_output_path(output_path)
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
        let opened =
            match open_default_worker_pane(run, cockpit_mode, &self.paths.root, &worker_request) {
                Ok(CockpitWorkerLaunch::Opened(opened)) => opened,
                Ok(CockpitWorkerLaunch::SkippedDirect) => {
                    return Err(CockpitWorkerJobError::Fallback(
                        "cockpit mode resolved to direct before worker pane launch".to_string(),
                    ));
                }
                Err(unavailable) => {
                    return Err(CockpitWorkerJobError::Fallback(unavailable.message));
                }
            };
        self.state
            .record_event(
                &run.id,
                workflow_events::COCKPIT_WORKER_READY,
                &workflow_events::CockpitWorkerReadyPayload {
                    adapter: opened.adapter,
                    mode: opened.mode.as_str().to_string(),
                    workspace: opened.workspace_label,
                    pane: opened.pane_label,
                    pane_id: opened.pane_id,
                    slice_id: context.slice_id.clone(),
                    attempt: context.attempt,
                    source_of_truth: "kd_artifact_files".to_string(),
                },
            )
            .map_err(CockpitWorkerJobError::Worker)?;
        let pid = match wait_for_pi_wrapper_launch(&artifacts, Duration::from_secs(5), &events) {
            Ok(pid) => pid,
            Err(err) if cancel.is_cancelled() => return Err(CockpitWorkerJobError::Worker(err)),
            Err(err) => return Err(CockpitWorkerJobError::Fallback(err.to_string())),
        };
        collect_pi_wrapper_result(job, &artifacts, cancel, events, pid)
            .map_err(CockpitWorkerJobError::Worker)
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
                .with_extra("fallback", "direct")
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
        self.record_cockpit_launch(&run, cockpit_mode)?;
        self.mark_progress(&run.id, "started", "", 0, "", "run accepted by daemon");

        let worker_token = new_worker_token();
        self.state.store_worker_token(&run.id, &worker_token)?;
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
                worker_token,
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
            self.state
                .update_run(run_id, RunStatus::Cancelled, reason)?;
            self.state.cancel_active_slice_runs(run_id, reason)?;
            self.state.record_event(
                run_id,
                workflow_events::RUN_CANCELLED,
                &workflow_events::RunCancelledPayload::new(reason),
            )?;
        }
        Ok(active)
    }

    pub fn resume_run(&self, mut opts: ResumeOptions) -> Result<Run> {
        let run = self
            .state
            .get_run(&opts.run_id)?
            .ok_or_else(|| anyhow!("run {:?} not found", opts.run_id))?;
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
        self.block_if_pending_replan(&run, "resume")?;
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
        let planned_slices = artifact::topological_order(&all_slices, &requested)?;
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
        self.state.update_run(&run.id, RunStatus::Running, "")?;
        self.state.record_event(
            &run.id,
            "run_resumed",
            &json!({ "remaining_slices": remaining.iter().map(|slice| slice.id.clone()).collect::<Vec<_>>() }),
        )?;
        self.mark_progress(&run.id, "resumed", "", 0, "", "run resumed by daemon");
        let config = store.read_config()?;
        let cockpit_mode = effective_cockpit_mode(&mut opts.pi_args, &config)?;
        let runner = self.runner_for_parts(&opts.agent, &opts.pi_bin, &opts.pi_args, &config)?;
        self.record_cockpit_launch(&run, cockpit_mode)?;
        let worker_token = new_worker_token();
        self.state.store_worker_token(&run.id, &worker_token)?;
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
                worker_token,
            );
        });
        self.state
            .get_run(&run.id)?
            .ok_or_else(|| anyhow!("run {:?} not found after resume", run.id))
    }

    pub fn recover_interrupted_runs(&self) -> Result<usize> {
        let runs = self.state.active_runs()?;
        let reason = "daemon restarted before run reached a terminal state";
        for run in &runs {
            self.state.record_event(
                &run.id,
                "daemon_recovery_started",
                &json!({ "reason": reason }),
            )?;
            match self.cleanup_run_worktrees(run) {
                Ok(()) => self.state.record_event(
                    &run.id,
                    "daemon_recovery_worktrees_cleaned",
                    &json!({ "run_id": run.id }),
                )?,
                Err(err) => self.state.record_event(
                    &run.id,
                    "daemon_recovery_cleanup_error",
                    &workflow_events::RunErrorPayload::new(err.to_string()),
                )?,
            }
            self.state.interrupt_active_slice_runs(&run.id, reason)?;
            let interrupted_questions = self
                .state
                .interrupt_pending_worker_questions(&run.id, reason)?;
            if interrupted_questions > 0 {
                self.state.record_event(
                    &run.id,
                    "worker_questions_interrupted",
                    &json!({ "count": interrupted_questions, "reason": reason }),
                )?;
            }
            self.state.mark_run_interrupted(&run.id, reason)?;
            self.state.record_event(
                &run.id,
                "daemon_recovery_completed",
                &json!({ "status": RunStatus::Interrupted, "reason": reason }),
            )?;
        }
        Ok(runs.len())
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
        let diagnostics = handoff_diagnostics(&run.repo_path);
        let summary_path = store.output_path(&run.id, "implementation-summary.json");
        let final_report_path = store.output_path(&run.id, "final-report.json");
        let summary = artifact::read_json::<ImplementationSummary>(&summary_path).ok();
        let read_model = self.run_read_model(&run, RunReadModelOptions::status(500))?;
        let final_sha = gitutil::run(&run.repo_path, &["rev-parse", &run.integration_branch])
            .ok()
            .filter(|sha| !sha.is_empty())
            .or_else(|| {
                summary
                    .as_ref()
                    .map(|summary| summary.final_sha.clone())
                    .filter(|sha| !sha.is_empty())
            })
            .unwrap_or_default();
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
            bail!(
                "handoff is not ready; unresolved replan proposal(s) {ids} require operator disposition; decide with: {commands}"
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
        let push_command = format!(
            "git -C {} push -u origin {}",
            sh_quote(&run.repo_path),
            sh_quote(&run.integration_branch)
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
            actions.push(run_handoff_command(
                "push",
                &run.repo_path,
                &["push", "-u", "origin", &run.integration_branch],
                &push_command,
            )?);
        }
        if effective_create_pr {
            let body = final_report_path.to_string_lossy().to_string();
            actions.push(run_external_command(
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
            )?);
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

    fn write_terminal_run_summary(
        &self,
        run: &Run,
        status: RunStatus,
        message: &str,
    ) -> Result<()> {
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
        self.state.record_event(
            &run.id,
            workflow_events::TERMINAL_SUMMARY_WRITTEN,
            &workflow_events::TerminalSummaryWrittenPayload::new(&summary_path),
        )?;
        let attention = OperatorAttention::new(self.state.clone());
        attention.worker_pane_terminal_rename(WorkerPaneTerminalRename {
            run,
            events: &events,
            slice_runs: &slice_runs,
        });
        attention.terminal_transition_notification(TerminalTransitionNotification {
            run,
            status,
            progress: progress.as_ref(),
            summary: &summary,
            summary_path: &summary_path,
        });
        Ok(())
    }

    fn run_worktree_snapshots(&self, run: &Run) -> Vec<serde_json::Value> {
        let root = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
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
        worker_token: String,
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
            worker_token,
        );
        let (terminal_status, terminal_message) = match &outcome {
            Ok(_) => {
                let message = "run completed; handoff artifacts are ready".to_string();
                self.mark_progress(&run.id, "completed", "", 0, "", &message);
                let _ = self.state.record_event(
                    &run.id,
                    workflow_events::RUN_COMPLETED,
                    &workflow_events::RunCompletedPayload::new(&run.id),
                );
                (RunStatus::Completed, String::new())
            }
            Err(err) => {
                let status = classify_run_failure(err);
                let raw_message = format!("{err:#}");
                let message = if status == RunStatus::Cancelled {
                    latest_cancel_reason(&self.state.get_events(&run.id, 200).unwrap_or_default())
                        .trim()
                        .to_string()
                } else {
                    String::new()
                };
                let message = if message.is_empty() {
                    raw_message
                } else {
                    message
                };
                if status == RunStatus::Cancelled {
                    let _ = self.state.cancel_active_slice_runs(&run.id, &message);
                    self.mark_progress(&run.id, "cancelled", "", 0, "", &message);
                    let _ = self.state.record_event(
                        &run.id,
                        workflow_events::RUN_CANCELLED,
                        &workflow_events::RunCancelledPayload::new(&message),
                    );
                } else {
                    let phase = if status == RunStatus::Blocked {
                        "blocked"
                    } else {
                        "failed"
                    };
                    self.mark_progress(&run.id, phase, "", 0, "", &message);
                    let _ = self.state.record_event(
                        &run.id,
                        workflow_events::RUN_ERROR,
                        &workflow_events::RunErrorPayload::new(&message),
                    );
                }
                (status, message)
            }
        };
        let _ = self.write_terminal_run_summary(&run, terminal_status, &terminal_message);
        let _ = self
            .state
            .update_run(&run.id, terminal_status, &terminal_message);
        match self.cleanup_run_worktrees(&run) {
            Ok(()) => {
                let _ = self.state.record_event(
                    &run.id,
                    workflow_events::WORKTREES_CLEANED,
                    &workflow_events::RunCompletedPayload::new(&run.id),
                );
            }
            Err(err) => {
                let _ = self.state.record_event(
                    &run.id,
                    "worktree_cleanup_error",
                    &workflow_events::RunErrorPayload::new(err.to_string()),
                );
            }
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
        worker_token: String,
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
        setup_phase.finish();

        let slice_runs = self.state.get_slice_runs(&run.id)?;
        let mut completed_slices = self.prior_completed_worker_results(run, &store, &slice_runs);
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
        for layer in artifact::dependency_layers(worker_slices)? {
            check_cancelled(cancel)?;
            self.block_if_pending_replan(run, "worker dispatch")?;
            let slice_base_sha = gitutil::head_sha(&integration_worktree)?;
            let layer_ids = layer
                .iter()
                .map(|slice| slice.id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let worker_phase = economics.start_phase(format!("worker_layer:{layer_ids}"));
            let worker_context = WorkerExecutionContext {
                run: run.clone(),
                root_worktree: root_worktree.clone(),
                slice_base_sha,
                dependency_summary: dependency_summary.clone(),
                cancel: cancel.clone(),
                runner: runner.clone(),
                config: config.clone(),
                cockpit_mode,
                economics: economics.clone(),
                verification_cache: verification_cache.clone(),
                worker_token: worker_token.clone(),
            };
            let outcomes = self.run_worker_layer(&layer, &worker_context, parallelism)?;
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
                        run,
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
                    &workflow_events::SliceMergedPayload::new(&slice.id, &worker.result.commit_sha),
                )?;
                dependency_summary.insert(slice.id.clone(), worker.result.summary.clone());
                completed_ids.insert(slice.id.clone());
                checks.extend(worker.checks);
                completed_slices.push(worker.result);
                self.write_checkpoint(run, gate_slices, &completed_ids, &integration_worktree)?;
            }
        }

        check_cancelled(cancel)?;
        self.block_if_pending_replan(run, "integration gate")?;
        self.run_worktree_setup(
            run,
            "",
            0,
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
                slices: gate_slices,
                integration_worktree: &integration_worktree,
                config: &config,
            },
            cancel,
        )?;
        gate_phase.finish();

        let mut pre_repair_gate = None;
        let repair = if should_run_integration_repair(repair_policy, &gate) {
            pre_repair_gate = Some(gate.clone());
            check_cancelled(cancel)?;
            self.block_if_pending_replan(run, "integration repair")?;
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
                run,
                slices: gate_slices,
                integration_worktree: &integration_worktree,
                checks: &checks,
                gate_failure: &gate,
                trigger: repair_trigger_for_gate(repair_policy, &gate),
                cancel,
                runner: runner.clone(),
                config: &config,
                economics: economics.clone(),
            })?;
            repair_phase.finish();

            check_cancelled(cancel)?;
            self.run_worktree_setup(
                run,
                "",
                0,
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
                    slices: gate_slices,
                    integration_worktree: &integration_worktree,
                    config: &config,
                },
                cancel,
            )?;
            rerun_phase.finish();
            repair
        } else {
            skipped_repair_result(repair_policy, &gate)
        };
        let integration_store = artifact::Store::new(&integration_worktree);
        let completed_slice_ids: Vec<_> = completed_slices
            .iter()
            .map(|slice| slice.slice_id.clone())
            .collect();
        let publication_already_current = gate.status == "passed"
            && completion_publication_is_current(&integration_store, &run.id, &completed_slice_ids);
        if gate.status == "passed" && !publication_already_current {
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
        }
        let exit_states = final_exit_states(&gate, &completed_slices);
        let worker_profile = worker_profile_evidence(runner.name(), &runner.metadata());
        let mut evidence_attestation = final_evidence_attestation(&gate);
        append_worker_evidence_attestation_basis(&mut evidence_attestation, &worker_profile);
        let plan_revisions = self.plan_revisions_for_run(run)?;
        let mut summary = ImplementationSummary {
            run_id: run.id.clone(),
            repo_path: run.repo_path.clone(),
            integration_branch: run.integration_branch.clone(),
            base_sha: run.base_sha.clone(),
            final_sha: String::new(),
            worker_profile,
            completed_slices,
            checks,
            integration_repair: repair,
            pre_repair_integration_gate: pre_repair_gate,
            integration_gate: gate.clone(),
            exit_states,
            evidence_attestation,
            economics: economics.snapshot(),
            plan_revisions,
            created_at: Utc::now(),
        };

        if !publication_already_current {
            integration_store
                .write_implementation_summary(&summary)
                .context("write implementation summary")?;
            integration_store.write_final_report(&summary)?;
            integration_store.commit_completion_publication(&run.id)?;
        }
        summary.final_sha = gitutil::head_sha(&integration_worktree).unwrap_or_default();
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

    #[allow(clippy::too_many_arguments)]
    fn run_worktree_setup(
        &self,
        run: &Run,
        slice_id: &str,
        attempt: usize,
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

    fn run_worker_layer(
        &self,
        layer: &[Slice],
        ctx: &WorkerExecutionContext,
        parallelism: usize,
    ) -> Result<Vec<SliceWorkerOutcome>> {
        if parallelism <= 1 || layer.len() <= 1 {
            let mut outcomes = Vec::new();
            for slice in layer {
                outcomes.push(self.run_slice_worker(slice, ctx)?);
            }
            return Ok(outcomes);
        }

        let mut queue: VecDeque<_> = layer.iter().cloned().collect();
        let mut outcomes = Vec::new();
        while !queue.is_empty() {
            let batch: Vec<_> = (0..parallelism).filter_map(|_| queue.pop_front()).collect();
            let mut batch_outcomes = self.run_parallel_worker_batch(&batch, ctx)?;
            outcomes.append(&mut batch_outcomes);
        }
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
            if parent_cancelled || parallel_results_all_cancelled(&results) {
                return Err(CancelledError::new("run cancelled").into());
            }
            if parallel_results_any_blocked(&results) {
                return Err(BlockedError::new(summary).into());
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
            outcomes.push(json!({
                "slice_id": slice_id,
                "status": status,
                "attempts": attempts,
                "summary": &summary,
            }));
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
                    .or_else(|| read_worker_result(store, &run.id, slice_run))
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
        let worker_worktree = ctx.root_worktree.join(&slice.id);
        let worker_branch = format!("khazad/{}/{}", run.id, slice.id);
        {
            let _git_lock = WORKTREE_ADD_LOCK
                .lock()
                .expect("worktree add mutex poisoned");
            gitutil::worktree_add(
                &run.repo_path,
                &worker_worktree,
                &worker_branch,
                &ctx.slice_base_sha,
            )
            .context("create worker worktree")?;
        }

        self.state.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: slice.id.clone(),
            status: SliceStatus::Running,
            branch: worker_branch.clone(),
            commit_sha: String::new(),
            attempts: 0,
            last_error: String::new(),
        })?;
        self.state.record_event(
            &run.id,
            workflow_events::SLICE_STARTED,
            &workflow_events::SliceStartedPayload::new(&slice.id),
        )?;
        self.mark_progress(
            &run.id,
            "worker_started",
            &slice.id,
            0,
            "",
            "slice worker started",
        );
        if let Err(err) = self.run_worktree_setup(
            run,
            &slice.id,
            0,
            &worker_worktree,
            config,
            economics.clone(),
            verification_cache.clone(),
            cancel,
        ) {
            self.state.upsert_slice_run(&SliceRun {
                run_id: run.id.clone(),
                slice_id: slice.id.clone(),
                status: SliceStatus::Blocked,
                branch: worker_branch.clone(),
                commit_sha: gitutil::head_sha(&worker_worktree).unwrap_or_default(),
                attempts: 0,
                last_error: err.to_string(),
            })?;
            return Err(err);
        }

        let mut all_checks = Vec::new();
        let mut last_failure = String::new();
        let mut primary_failure: Option<String> = None;
        let mut secondary_failures: Vec<String> = Vec::new();
        for attempt in 1..=MAX_WORKER_ATTEMPTS {
            check_cancelled(cancel)?;
            let output_path = store.output_path(
                &run.id,
                &format!("{}.worker.attempt-{attempt}.json", slice.id),
            );
            let runner_metadata = runner.metadata();
            let handoff = Handoff {
                run_id: run.id.clone(),
                role: "slice-worker".to_string(),
                repo_path: run.repo_path.clone(),
                worktree_path: worker_worktree.to_string_lossy().to_string(),
                branch: worker_branch.clone(),
                slice: slice.clone(),
                dependency_summary: ctx.dependency_summary.clone(),
                worker_profile: worker_profile_evidence(runner.name(), &runner_metadata),
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
            let handoff_path =
                self.write_worker_handoff_with_plan_revisions(&store, run, &handoff)?;
            let prompt = worker_prompt(&handoff_path.to_string_lossy(), &handoff, &last_failure);
            self.mark_progress(
                &run.id,
                "worker_running",
                &slice.id,
                attempt,
                runner.name(),
                "slice worker is running",
            );
            let result = match self.run_recorded_slice_worker_job(
                runner.clone(),
                Job {
                    kind: "slice-worker".to_string(),
                    prompt,
                    cwd: worker_worktree.clone(),
                    json_schema: WORKER_RESULT_SCHEMA.to_string(),
                    env: worker_job_env(&self.paths, run, &slice.id, attempt, &ctx.worker_token),
                    termination_grace_seconds: 0,
                },
                cancel,
                WorkerAttemptContext::new(&run.id, "worker_running", &slice.id, attempt, config),
                &economics,
                AgentCallContext {
                    phase: "slice_worker",
                    slice_id: &slice.id,
                    attempt,
                },
                run,
                ctx.cockpit_mode,
                &output_path,
            ) {
                Ok(result) => result,
                Err(err) => {
                    let launch_failure =
                        self.classify_runner_launch_failure(err.as_ref(), &runner_metadata);
                    last_failure = launch_failure
                        .as_ref()
                        .map(|failure| failure.summary.clone())
                        .unwrap_or_else(|| err.to_string());
                    remember_attempt_failure(
                        &mut primary_failure,
                        &mut secondary_failures,
                        &last_failure,
                    );
                    if invalid_worker_output_error(&last_failure) {
                        let transcript = err
                            .downcast_ref::<RunnerError>()
                            .map(|err| err.transcript().clone())
                            .unwrap_or_default();
                        self.record_invalid_worker_output_attempt(
                            run,
                            slice,
                            attempt,
                            &last_failure,
                            &worker_worktree,
                            &output_path,
                            None,
                            transcript,
                        )?;
                        self.update_invalid_worker_attempt_status(
                            run,
                            slice,
                            &worker_branch,
                            &worker_worktree,
                            attempt,
                            primary_failure.as_deref(),
                            &last_failure,
                            &secondary_failures,
                        )?;
                        continue;
                    }
                    self.write_worker_attempt_failure_artifact(
                        run,
                        slice,
                        attempt,
                        "worker_error",
                        &last_failure,
                        &worker_worktree,
                        &output_path,
                        Some(err.as_ref()),
                    )?;
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    self.state.record_event(
                        &run.id,
                        "worker_error",
                        &workflow_events::WorkerErrorPayload {
                            slice_id: slice.id.clone(),
                            attempt,
                            error: last_failure.clone(),
                            primary_failure: primary_failure.clone(),
                            secondary_failures: secondary_failures.clone(),
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
                                runner_name: runner.name(),
                                metadata: &runner_metadata,
                            },
                            &launch_failure,
                        )?;
                        self.state.upsert_slice_run(&SliceRun {
                            run_id: run.id.clone(),
                            slice_id: slice.id.clone(),
                            status: SliceStatus::Blocked,
                            branch: worker_branch.clone(),
                            commit_sha: gitutil::head_sha(&worker_worktree).unwrap_or_default(),
                            attempts: attempt,
                            last_error: launch_failure.summary.clone(),
                        })?;
                        return Err(BlockedError::new(launch_failure.summary).into());
                    }
                    continue;
                }
            };

            let Some(output) = result.output else {
                last_failure = "worker returned no JSON output".to_string();
                remember_attempt_failure(
                    &mut primary_failure,
                    &mut secondary_failures,
                    &last_failure,
                );
                self.record_invalid_worker_output_attempt(
                    run,
                    slice,
                    attempt,
                    &last_failure,
                    &worker_worktree,
                    &output_path,
                    None,
                    RunnerTranscript::default(),
                )?;
                self.update_invalid_worker_attempt_status(
                    run,
                    slice,
                    &worker_branch,
                    &worker_worktree,
                    attempt,
                    primary_failure.as_deref(),
                    &last_failure,
                    &secondary_failures,
                )?;
                continue;
            };
            let mut worker_result: WorkerResult = match serde_json::from_value(output.clone()) {
                Ok(value) => value,
                Err(err) => {
                    last_failure = format!("worker JSON did not match result model: {err}");
                    remember_attempt_failure(
                        &mut primary_failure,
                        &mut secondary_failures,
                        &last_failure,
                    );
                    self.record_invalid_worker_output_attempt(
                        run,
                        slice,
                        attempt,
                        &last_failure,
                        &worker_worktree,
                        &output_path,
                        Some(output),
                        RunnerTranscript::default(),
                    )?;
                    self.update_invalid_worker_attempt_status(
                        run,
                        slice,
                        &worker_branch,
                        &worker_worktree,
                        attempt,
                        primary_failure.as_deref(),
                        &last_failure,
                        &secondary_failures,
                    )?;
                    continue;
                }
            };
            if let Err(err) = validate_worker_result(&worker_result, slice) {
                last_failure = format!("worker JSON failed validation: {err}");
                remember_attempt_failure(
                    &mut primary_failure,
                    &mut secondary_failures,
                    &last_failure,
                );
                self.record_invalid_worker_output_attempt(
                    run,
                    slice,
                    attempt,
                    &last_failure,
                    &worker_worktree,
                    &output_path,
                    Some(serde_json::to_value(&worker_result).unwrap_or_default()),
                    RunnerTranscript::default(),
                )?;
                self.update_invalid_worker_attempt_status(
                    run,
                    slice,
                    &worker_branch,
                    &worker_worktree,
                    attempt,
                    primary_failure.as_deref(),
                    &last_failure,
                    &secondary_failures,
                )?;
                continue;
            }
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
                &worker_worktree,
                config,
                economics.clone(),
                verification_cache.clone(),
                cancel,
            ) {
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

            let check = self.lightweight_check(
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
            )?;
            artifact::write_json(
                store.output_path(
                    &run.id,
                    &format!("{}.check.attempt-{attempt}.json", slice.id),
                ),
                &check,
            )?;
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
                return Err(BlockedError::new(message).into());
            }
            if worker_result.status == "blocked" {
                self.state.update_slice_status(
                    &run.id,
                    &slice.id,
                    SliceStatus::Blocked,
                    &worker_result.summary,
                )?;
                return Err(BlockedError::new(format!(
                    "worker reported blocked: {}",
                    worker_result.summary
                ))
                .into());
            }
            if attempt == MAX_WORKER_ATTEMPTS {
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
            findings: Vec::new(),
            attempt: ctx.attempt,
            worker_head: String::new(),
            worktree_ok: true,
            commit_found: true,
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
        if let Some(failure) = verification.failure {
            check.status = "failed".to_string();
            check.summary = failure.summary;
            check.failure_kind = failure.failure_kind;
            check.findings.push(failure.finding);
            return Ok(check);
        }
        Ok(check)
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

    #[allow(clippy::too_many_arguments)]
    fn record_invalid_worker_output_attempt(
        &self,
        run: &Run,
        slice: &Slice,
        attempt: usize,
        error: &str,
        worker_worktree: &Path,
        expected_output_path: &Path,
        raw_payload: Option<serde_json::Value>,
        transcript: RunnerTranscript,
    ) -> Result<PathBuf> {
        let store = artifact::Store::new(&run.repo_path);
        store.ensure_run_dirs(&run.id)?;
        let progress = self.state.get_progress(&run.id)?;
        let invalid_output_path = store.output_path(
            &run.id,
            &format!("{}.worker.attempt-{attempt}.invalid-output.json", slice.id),
        );
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
                "parse_error": bounded_text(error, 4_000),
                "artifact_path": invalid_output_path,
                "expected_output_path": expected_output_path,
                "raw_invalid_payload": bounded_text(&raw_payload_text, 4_000),
                "stdout_tail": bounded_text(&transcript.stdout_tail, 4_000),
                "stderr_tail": bounded_text(&transcript.stderr_tail, 4_000),
                "assistant_tail": bounded_text(&transcript.assistant_tail, 4_000),
            }),
        )?;
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
        artifact::write_json(
            store.output_path(
                &run.id,
                &format!("{}.worker.attempt-{attempt}.failure.json", slice.id),
            ),
            &diagnostic,
        )?;
        Ok(())
    }

    fn current_slice_base_for_artifact(&self, run: &Run, _slice: &Slice) -> String {
        // Best-effort only: attempt artifacts are diagnostic and must not fail the workflow.
        gitutil::run(&run.repo_path, &["rev-parse", &run.integration_branch])
            .unwrap_or_else(|_| run.base_sha.clone())
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
        let store = artifact::Store::new(&run.repo_path);
        let mut last_error = String::new();
        for attempt in 1..=DEFAULT_REPAIR_ATTEMPTS {
            economics.set_repair_attempts(attempt);
            check_cancelled(cancel)?;
            let repair_base = gitutil::head_sha(integration_worktree).unwrap_or_default();
            let output_path = store.output_path(
                &run.id,
                &format!("integration-repair.attempt-{attempt}.json"),
            );
            self.mark_progress(
                &run.id,
                "integration_repair",
                "",
                attempt,
                runner.name(),
                "integration repair worker is running",
            );
            let runner_metadata = runner.metadata();
            let prompt = integration_repair_prompt(
                &run.id,
                &integration_worktree.to_string_lossy(),
                slices,
                &check_summary,
                &gate_summary,
                context.trigger,
            );
            let agent_result = match self.run_recorded_agent_job(
                runner.clone(),
                Job {
                    kind: "integration-repair".to_string(),
                    prompt,
                    cwd: integration_worktree.to_path_buf(),
                    json_schema: REPAIR_RESULT_SCHEMA.to_string(),
                    env: BTreeMap::new(),
                    termination_grace_seconds: 0,
                },
                cancel,
                WorkerAttemptContext::new(&run.id, "integration_repair", "", attempt, config),
                &economics,
                AgentCallContext {
                    phase: "integration_repair",
                    slice_id: "",
                    attempt,
                },
            ) {
                Ok(result) => result,
                Err(err) => {
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
                                slice_id: "",
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
            let Some(output) = agent_result.output else {
                last_error = "integration repair returned no JSON output".to_string();
                continue;
            };
            let mut result: RepairResult = match serde_json::from_value(output) {
                Ok(value) => value,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            if let Err(err) = validate_repair_result(&result) {
                last_error = format!("integration repair JSON failed validation: {err}");
                continue;
            }
            result.trigger = context.trigger.to_string();
            result.attempts = attempt;
            let created_finding_proposal = self.create_repair_finding_replan_proposals(
                run,
                attempt,
                &output_path,
                &mut result,
            )?;
            artifact::write_json(&output_path, &result)?;
            if created_finding_proposal {
                self.block_if_pending_replan(run, "integration repair finding proposal")?;
            }
            if result.status == "blocked" {
                return Err(BlockedError::new(format!(
                    "integration repair blocked: {}",
                    result.summary
                ))
                .into());
            }
            if result.status == "failed" {
                last_error = result.summary.clone();
                continue;
            }
            let status = match gitutil::status_porcelain(integration_worktree) {
                Ok(status) => status,
                Err(err) => {
                    last_error = err.to_string();
                    continue;
                }
            };
            if !status.trim().is_empty() {
                last_error = "integration repair left uncommitted changes".to_string();
                continue;
            }
            if result.commit_sha.is_empty()
                && result.status == "fixed"
                && let Ok(head) = gitutil::head_sha(integration_worktree)
            {
                result.commit_sha = head;
            }
            if result.status == "fixed" {
                let repair_head = gitutil::head_sha(integration_worktree).unwrap_or_default();
                let unauthorized = repair_authority_violations(
                    integration_worktree,
                    &repair_base,
                    &repair_head,
                    slices,
                )?;
                if !unauthorized.is_empty() {
                    let proposal_id = self.create_repair_authority_proposal(
                        run,
                        attempt,
                        &output_path,
                        &repair_base,
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
                    artifact::write_json(&output_path, &result)?;
                    gitutil::run(integration_worktree, &["reset", "--hard", &repair_base])?;
                    self.block_if_pending_replan(run, "integration repair authority proposal")?;
                }
            }
            artifact::write_json(&output_path, &result)?;
            self.state.record_event(
                &run.id,
                workflow_events::INTEGRATION_REPAIR_COMPLETED,
                &workflow_events::IntegrationRepairCompletedPayload::new(
                    &result.status,
                    &result.summary,
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
        let mut errors = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Err(err) = gitutil::worktree_remove(&run.repo_path, &path) {
                errors.push(format!("{}: {err}", path.display()));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
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
            if summary.trim().is_empty() {
                format!("{slice_id}={status}")
            } else {
                format!("{slice_id}={status} ({summary})")
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

fn read_worker_result(
    store: &artifact::Store,
    run_id: &str,
    slice_run: &SliceRun,
) -> Option<WorkerResult> {
    if slice_run.attempts == 0 {
        return None;
    }
    artifact::read_json(store.output_path(
        run_id,
        &format!(
            "{}.worker.attempt-{}.json",
            slice_run.slice_id, slice_run.attempts
        ),
    ))
    .ok()
}

fn completion_publication_is_current(
    store: &artifact::Store,
    run_id: &str,
    completed_slice_ids: &[String],
) -> bool {
    store.publication_reports_exist(run_id)
        && completed_slice_ids
            .iter()
            .all(|slice_id| slice_closed_by_run_or_absent(store, slice_id, run_id))
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

fn worker_job_env(
    paths: &Paths,
    run: &Run,
    slice_id: &str,
    attempt: usize,
    token: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "KHAZAD_DAEMON_SOCKET".to_string(),
            paths.socket().to_string_lossy().to_string(),
        ),
        ("KHAZAD_RUN_ID".to_string(), run.id.clone()),
        ("KHAZAD_SLICE_ID".to_string(), slice_id.to_string()),
        ("KHAZAD_ATTEMPT".to_string(), attempt.to_string()),
        ("KHAZAD_WORKER_TOKEN".to_string(), token.to_string()),
    ])
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
        Manager, RunReadModelOptions, StartOptions, repair_authority_violations,
        validate_repair_result, validate_worker_result,
    };
    use crate::agent::{CancellationToken, Job, ResultData, Runner, RunnerEventSink, Usage};
    use crate::artifact::{self, Store as ArtifactStore};
    use crate::domain::{
        AcceptanceEvidence, CheckResult, CockpitMode, Finding, FindingDisposition, Handoff,
        ImplementationSummary, OriginNotificationTarget, RepairResult, ReplanEvidenceLink,
        ReplanProposalSource, ReplanProposalState, ReplanProposedChange, Run, RunEconomics,
        RunStatus, Slice, SliceRun, SliceStatus, TerminalNotificationRecord, VerifyCommand,
        VerifyProfile, WorkerResult, WorkflowConfig,
    };
    use crate::gitutil;
    use crate::paths::Paths;
    use crate::state::Store as StateStore;
    use anyhow::Result;
    use chrono::Utc;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
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
        fs::create_dir_all(paths.repo_worktree_dir(&repo_id, &run_id))?;
        let manager = Manager::with_runner(paths.clone(), state.clone(), Arc::new(FakeRunner));

        assert_eq!(manager.recover_interrupted_runs()?, 1);

        let recovered = state.get_run(&run_id)?.expect("run exists");
        assert_eq!(recovered.status, RunStatus::Interrupted);
        let slice_runs = state.get_slice_runs(&run_id)?;
        assert_eq!(slice_runs[0].status, SliceStatus::Interrupted);
        assert!(!paths.repo_worktree_dir(&repo_id, &run_id).exists());
        let events = state.get_events(&run_id, 20)?;
        assert!(
            events
                .iter()
                .any(|event| event.typ == "daemon_recovery_completed")
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
        first.verify = vec!["test -f slice-001.txt".to_string()];
        let mut second = slice("slice-002");
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
            parallelism: 2,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
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
        let events = state.get_events(&run.id, 100)?;
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: "agent-1".to_string(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let final_report: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        let implementation_summary: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "implementation-summary.json"))?;
        for report in [&final_report, &implementation_summary] {
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
                    .contains("applied=false")
            );
            assert_eq!(
                revisions["deferred"][0]["decision"]["revisit_condition"],
                "after release"
            );
        }

        let worker_handoff: serde_json::Value =
            artifact::read_json(store.handoff_dir(&run.id).join("slice-001.json"))?;
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let pending = create_test_replan_proposal(&state, &run.id, "slice-001", "pending")?;
        let err = manager
            .branch_handoff(&run.id, false, false, false)
            .unwrap_err();
        assert!(err.to_string().contains("handoff is not ready"));
        assert!(err.to_string().contains(&pending.id));
        assert!(err.to_string().contains("khazad-doom replan accept"));
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
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
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.final_sha, head_after);
        assert_eq!(summary.completed_slices.len(), 1);
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
        let check: CheckResult =
            artifact::read_json(store.output_path(&run.id, "slice-001.check.attempt-1.json"))?;
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
                parallelism: 1,
                allow_dirty: false,
                origin_notification_target: String::new(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let failed = wait_for_run(&state, &run.id)?;
        assert_eq!(failed.status, RunStatus::Failed);
        assert!(failed.error.contains("outside slice areas"));
        assert!(failed.error.contains("slice-001.txt"));
        let check: CheckResult =
            artifact::read_json(store.output_path(&run.id, "slice-001.check.attempt-1.json"))?;
        assert_eq!(check.failure_kind, "scope_violation");
        Ok(())
    }

    #[test]
    fn untracked_workflow_slices_do_not_fail_successful_finalization() -> Result<()> {
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
            parallelism: 1,
            allow_dirty: true,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        let summary: ImplementationSummary =
            artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
        assert_eq!(summary.completed_slices.len(), 1);
        let events = state.get_events(&run.id, 100)?;
        assert!(events.iter().any(|event| {
            event.typ == "run_incident"
                && event.payload["kind"].as_str() == Some("slice_close_skipped")
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Completed);
        assert_eq!(completed.selected_slice_id, "slice-002");
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs.len(), 1);
        assert_eq!(slice_runs[0].slice_id, "slice-002");
        assert_eq!(slice_runs[0].status, SliceStatus::Merged);
        let events = state.get_events(&run.id, 20)?;
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
                parallelism: 1,
                allow_dirty: false,
                origin_notification_target: String::new(),
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
            parallelism: 0,
            allow_dirty: false,
            origin_notification_target: String::new(),
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
            parallelism: 1,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let err = manager
            .start_run(StartOptions {
                repo_path: repo.path().to_path_buf(),
                slice_ids: Vec::new(),
                all: true,
                agent: "fake".to_string(),
                pi_bin: String::new(),
                pi_args: Vec::new(),
                parallelism: 1,
                allow_dirty: false,
                origin_notification_target: String::new(),
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
        artifact::write_json(
            store.slices_dir().join("slice-001.json"),
            &slice("slice-001"),
        )?;
        artifact::write_json(
            store.slices_dir().join("slice-002.json"),
            &slice("slice-002"),
        )?;
        gitutil::run(repo.path(), &["add", ".gitignore", ".workflow"])?;
        gitutil::run(repo.path(), &["commit", "-m", "add slices"])?;

        let home = tempfile::tempdir()?;
        let paths = Paths {
            root: home.path().to_path_buf(),
        };
        paths.ensure()?;
        let state = StateStore::open(paths.db_file())?;
        let manager = Manager::with_runner(paths, state.clone(), Arc::new(ConflictRunner));
        let run = manager.start_run(StartOptions {
            repo_path: repo.path().to_path_buf(),
            slice_ids: Vec::new(),
            all: true,
            agent: "fake".to_string(),
            pi_bin: String::new(),
            pi_args: Vec::new(),
            parallelism: 2,
            allow_dirty: false,
            origin_notification_target: String::new(),
        })?;

        let completed = wait_for_run(&state, &run.id)?;
        assert_eq!(completed.status, RunStatus::Blocked);
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert!(
            slice_runs
                .iter()
                .any(|slice_run| slice_run.slice_id == "slice-002"
                    && slice_run.status == SliceStatus::Blocked)
        );
        assert!(
            store
                .output_path(&run.id, "slice-002.merge-conflict.json")
                .exists()
        );
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
            ) {
                return Ok(run);
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
