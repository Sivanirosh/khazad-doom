use super::economics::{RunEconomicsRecorder, agent_call};
use super::gate::{
    IntegrationGateRequest, SliceVerificationRequest, VerificationCommandCache, WorkflowGate,
    failure_kind_needs_operator,
};
use super::{
    CancelledError, REPAIR_RESULT_SCHEMA, WORKER_RESULT_SCHEMA, check_cancelled,
    integration_repair_prompt, worker_prompt,
};
use crate::agent::{
    CancellationToken, Job, Runner, RunnerError, RunnerEvent, RunnerEventSink, RunnerMetadata,
    RunnerSpec, runner_from_spec,
};
use crate::artifact;
use crate::domain::{
    AgentProfile, BranchHandoff, CheckResult, EvidenceAttestation, Finding, GateResult, Handoff,
    HandoffActionResult, HandoffDiagnostics, IMPLEMENTER_PROFILE, ImplementationSummary,
    MergeConflictReport, RepairResult, Run, RunCheckpoint, RunInspection, RunStatus, Slice,
    SliceExitState, SliceRun, SliceStatus, SliceValidationReport, SliceWriteResult, WorkerResult,
    WorkflowConfig, WorkflowExitStates,
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

#[derive(Clone)]
struct WorkerExecutionContext {
    run: Run,
    root_worktree: PathBuf,
    slice_base_sha: String,
    dependency_summary: BTreeMap<String, String>,
    cancel: CancellationToken,
    runner: Arc<dyn Runner>,
    config: WorkflowConfig,
    economics: RunEconomicsRecorder,
    verification_cache: VerificationCommandCache,
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
                context.timeout_seconds,
                context.no_output_warning_seconds,
            );
        })
    }

    fn run_supervised_worker_job(
        &self,
        runner: Arc<dyn Runner>,
        mut job: Job,
        cancel: &CancellationToken,
        context: WorkerAttemptContext,
    ) -> Result<crate::agent::ResultData> {
        job.termination_grace_seconds = context.termination_grace_seconds;
        let events = Some(self.worker_event_sink(&context));
        if context.timeout_seconds == 0 {
            return runner.run(job, cancel.clone(), events);
        }

        let attempt_cancel = CancellationToken::new();
        let timed_out = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let parent_cancel = cancel.clone();
        let timeout = Duration::from_secs(context.timeout_seconds);
        let timeout_cancel = attempt_cancel.clone();
        let timeout_flag = timed_out.clone();
        let done_flag = done.clone();
        let supervisor = thread::spawn(move || {
            let started = Instant::now();
            loop {
                if done_flag.load(Ordering::SeqCst) {
                    return;
                }
                if parent_cancel.is_cancelled() {
                    timeout_cancel.cancel();
                    return;
                }
                if started.elapsed() >= timeout {
                    timeout_flag.store(true, Ordering::SeqCst);
                    timeout_cancel.cancel();
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });

        let result = runner.run(job, attempt_cancel, events);
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
                &json!({
                    "phase": context.phase,
                    "slice_id": context.slice_id,
                    "attempt": context.attempt,
                    "timeout_seconds": context.timeout_seconds,
                    "message": message,
                }),
            )?;
            bail!(message);
        }
        result
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
        match self.run_supervised_worker_job(runner, job, cancel, context) {
            Ok(result) => {
                economics.record_agent_call(agent_call(
                    call.phase,
                    call.slice_id,
                    call.attempt,
                    kind.as_str(),
                    runner_name.as_str(),
                    &runner_metadata,
                    "succeeded",
                    started_at.elapsed(),
                    Some(&result.usage),
                    "",
                ));
                Ok(result)
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
                    None,
                    &error,
                ));
                Err(err)
            }
        }
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
        store: &artifact::Store,
    ) -> Result<Arc<dyn Runner>> {
        self.runner_for_parts(&opts.agent, &opts.pi_bin, &opts.pi_args, config, store)
    }

    fn runner_for_parts(
        &self,
        agent: &str,
        pi_bin: &str,
        pi_args: &[String],
        config: &WorkflowConfig,
        store: &artifact::Store,
    ) -> Result<Arc<dyn Runner>> {
        if let Some(runner) = &self.runner_override {
            return Ok(runner.clone());
        }
        let agent = if agent.trim().is_empty() {
            config.agent.as_str()
        } else {
            agent
        };
        let mut spec = RunnerSpec::from_parts(agent, pi_bin.to_string(), pi_args.to_vec())?;
        if spec.kind == "pi" {
            let profiles = store.read_agent_profiles()?;
            let profile = profiles
                .profiles
                .get(IMPLEMENTER_PROFILE)
                .ok_or_else(|| anyhow!("missing required agent profile {IMPLEMENTER_PROFILE:?}"))?;
            apply_implementer_profile_to_pi_spec(&mut spec, profile)?;
        }
        Ok(runner_from_spec(spec))
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

    pub fn start_run(&self, opts: StartOptions) -> Result<Run> {
        let repo = self.init_repo(&opts.repo_path)?;
        let store = artifact::Store::new(&repo.path);
        let config = store.read_config()?;
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
        let runner = self.runner_for_options(&opts, &config, &store)?;
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
        artifact::Store::new(&run.repo_path).ensure_run_dirs(&run.id)?;
        artifact::write_json(
            artifact::Store::new(&run.repo_path).output_path(&run.id, "preflight.json"),
            &json!({
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
        let runner_metadata = runner.metadata();
        self.state.record_event(
            &run.id,
            "run_started",
            &json!({
                "run": run,
                "selected_slices": selected_ids,
                "skipped_closed_slices": skipped_closed_slices,
                "agent": runner.name(),
                "agent_profile": runner_metadata.profile,
                "agent_provider": runner_metadata.provider,
                "agent_model": runner_metadata.model,
                "agent_reasoning": runner_metadata.reasoning,
                "agent_mode": runner_metadata.mode,
            }),
        )?;
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
            "run_cancel_requested",
            &json!({ "reason": reason, "active": active }),
        )?;
        if !active && matches!(run.status, RunStatus::Running | RunStatus::Pending) {
            self.state
                .update_run(run_id, RunStatus::Cancelled, reason)?;
            self.state.cancel_active_slice_runs(run_id, reason)?;
            self.state
                .record_event(run_id, "run_cancelled", &json!({ "reason": reason }))?;
        }
        Ok(active)
    }

    pub fn resume_run(&self, opts: ResumeOptions) -> Result<Run> {
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
        let selected_slices = artifact::topological_order(&all_slices, &requested)?;
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
        let runner =
            self.runner_for_parts(&opts.agent, &opts.pi_bin, &opts.pi_args, &config, &store)?;
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
                    &json!({ "error": err.to_string() }),
                )?,
            }
            self.state.interrupt_active_slice_runs(&run.id, reason)?;
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
        let final_sha = summary
            .as_ref()
            .map(|summary| summary.final_sha.clone())
            .filter(|sha| !sha.is_empty())
            .or_else(|| gitutil::run(&run.repo_path, &["rev-parse", &run.integration_branch]).ok())
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
            completed_slices,
            exit_states,
            evidence_attestation,
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
        let events = self.state.get_events(&run.id, 500)?;
        let slice_runs = self.state.get_slice_runs(&run.id)?;
        let progress = self.state.get_progress(&run.id)?;
        let economics: Option<crate::domain::RunEconomics> =
            artifact::read_json(store.output_path(&run.id, "economics.json")).ok();
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
            "message": message,
            "primary_failure": primary_failure,
            "cancel_reason": cancel_reason,
            "slice_runs": slice_runs,
            "progress": progress,
            "economics": economics,
            "worktree_snapshots": self.run_worktree_snapshots(run),
            "next_commands": terminal_next_commands(run, status),
            "created_at": Utc::now(),
        });
        artifact::write_json(store.output_path(&run.id, "run-summary.json"), &summary)?;
        self.state.record_event(
            &run.id,
            "terminal_summary_written",
            &json!({ "path": store.output_path(&run.id, "run-summary.json") }),
        )?;
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
    ) {
        let outcome = self.run_slices(
            &run,
            &worker_slices,
            &gate_slices,
            &cancel,
            runner,
            parallelism,
            integration_mode,
        );
        let (terminal_status, terminal_message) = match &outcome {
            Ok(_) => {
                let message = "run completed; handoff artifacts are ready".to_string();
                self.mark_progress(&run.id, "completed", "", 0, "", &message);
                let _ =
                    self.state
                        .record_event(&run.id, "run_completed", &json!({ "run_id": run.id }));
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
                        "run_cancelled",
                        &json!({ "reason": message }),
                    );
                } else {
                    let phase = if status == RunStatus::Blocked {
                        "blocked"
                    } else {
                        "failed"
                    };
                    self.mark_progress(&run.id, phase, "", 0, "", &message);
                    let _ =
                        self.state
                            .record_event(&run.id, "run_error", &json!({ "error": message }));
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
                    "worktrees_cleaned",
                    &json!({ "run_id": run.id }),
                );
            }
            Err(err) => {
                let _ = self.state.record_event(
                    &run.id,
                    "worktree_cleanup_error",
                    &json!({ "error": err.to_string() }),
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

        let mut completed_slices = Vec::new();
        let mut checks = Vec::new();
        let mut dependency_summary = BTreeMap::new();
        let mut completed_ids: BTreeSet<_> = self
            .state
            .get_slice_runs(&run.id)?
            .into_iter()
            .filter(|slice_run| slice_run.status == SliceStatus::Merged)
            .map(|slice_run| slice_run.slice_id)
            .collect();
        for layer in artifact::dependency_layers(worker_slices)? {
            check_cancelled(cancel)?;
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
                economics: economics.clone(),
                verification_cache: verification_cache.clone(),
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
                    "slice_merged",
                    &json!({ "slice_id": slice.id, "commit_sha": worker.result.commit_sha }),
                )?;
                dependency_summary.insert(slice.id.clone(), worker.result.summary.clone());
                completed_ids.insert(slice.id.clone());
                checks.extend(worker.checks);
                completed_slices.push(worker.result);
                self.write_checkpoint(run, gate_slices, &completed_ids, &integration_worktree)?;
            }
        }

        check_cancelled(cancel)?;
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
        if gate.status == "passed" {
            let completed_slice_ids: Vec<_> = completed_slices
                .iter()
                .map(|slice| slice.slice_id.clone())
                .collect();
            let close_warnings = integration_store.close_slices_if_present(
                &completed_slice_ids,
                &run.id,
                &Utc::now().to_rfc3339(),
            )?;
            for warning in close_warnings {
                self.state.record_event(
                    &run.id,
                    "run_incident",
                    &json!({
                        "severity": "warning",
                        "kind": "slice_close_skipped",
                        "message": warning,
                    }),
                )?;
            }
        }
        let final_sha = gitutil::head_sha(&integration_worktree).unwrap_or_default();
        let exit_states = final_exit_states(&gate, &completed_slices);
        let evidence_attestation = final_evidence_attestation(&gate);
        let summary = ImplementationSummary {
            run_id: run.id.clone(),
            repo_path: run.repo_path.clone(),
            integration_branch: run.integration_branch.clone(),
            base_sha: run.base_sha.clone(),
            final_sha,
            completed_slices,
            checks,
            integration_repair: repair,
            pre_repair_integration_gate: pre_repair_gate,
            integration_gate: gate.clone(),
            exit_states,
            evidence_attestation,
            economics: economics.snapshot(),
            created_at: Utc::now(),
        };

        integration_store
            .write_implementation_summary(&summary)
            .context("write implementation summary")?;
        integration_store.write_final_report(&summary)?;
        artifact::write_json(
            store.output_path(&run.id, "implementation-summary.json"),
            &summary,
        )?;
        artifact::write_json(store.output_path(&run.id, "final-report.json"), &summary)?;
        self.state
            .record_event(&run.id, "implementation_summary", &summary)?;

        if gate.status != "passed" {
            bail!("integration gate failed: {}", gate.summary);
        }
        Ok(summary)
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
            "parallel_layer_started",
            &json!({ "slices": batch_ids }),
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
                "parallel_layer_failed",
                &json!({ "slices": batch_ids, "outcomes": outcomes, "summary": summary }),
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
            "parallel_layer_completed",
            &json!({ "slices": batch_ids, "outcomes": outcomes }),
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
            .record_event(&run.id, "checkpoint_written", &checkpoint)?;
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
        self.state
            .record_event(&run.id, "slice_started", &json!({ "slice_id": slice.id }))?;
        self.mark_progress(
            &run.id,
            "worker_started",
            &slice.id,
            0,
            "",
            "slice worker started",
        );

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
                agent_profile: runner_metadata.profile,
                agent_provider: runner_metadata.provider,
                agent_model: runner_metadata.model,
                agent_reasoning: runner_metadata.reasoning,
                agent_mode: runner_metadata.mode,
                output_path: output_path.to_string_lossy().to_string(),
                contract: "Implement only this slice, commit all intended changes, leave a clean worktree, and return JSON."
                    .to_string(),
            };
            let handoff_path = store.write_handoff(&run.id, &handoff)?;
            let prompt = worker_prompt(&handoff_path.to_string_lossy(), &handoff, &last_failure);
            self.mark_progress(
                &run.id,
                "worker_running",
                &slice.id,
                attempt,
                runner.name(),
                "slice worker is running",
            );
            let result = match self.run_recorded_agent_job(
                runner.clone(),
                Job {
                    kind: "slice-worker".to_string(),
                    prompt,
                    cwd: worker_worktree.clone(),
                    json_schema: WORKER_RESULT_SCHEMA.to_string(),
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
            ) {
                Ok(result) => result,
                Err(err) => {
                    last_failure = err.to_string();
                    remember_attempt_failure(
                        &mut primary_failure,
                        &mut secondary_failures,
                        &last_failure,
                    );
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
                        &json!({
                            "slice_id": slice.id,
                            "attempt": attempt,
                            "error": last_failure,
                            "primary_failure": &primary_failure,
                            "secondary_failures": &secondary_failures,
                        }),
                    )?;
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
                continue;
            };
            let mut worker_result: WorkerResult = match serde_json::from_value(output) {
                Ok(value) => value,
                Err(err) => {
                    last_failure = format!("worker JSON did not match result model: {err}");
                    remember_attempt_failure(
                        &mut primary_failure,
                        &mut secondary_failures,
                        &last_failure,
                    );
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
                continue;
            }
            artifact::write_json(&output_path, &worker_result)?;

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
                    status: SliceStatus::Failed,
                    branch: worker_branch.clone(),
                    commit_sha: check.worker_head.clone(),
                    attempts: attempt,
                    last_error: message.clone(),
                })?;
                bail!(message);
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

        if let Some(outside) = changed_files_outside_slice_areas(
            ctx.worker_worktree,
            ctx.base_sha,
            &head,
            &ctx.slice.areas,
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
                    "slice areas are [{}]; worker changed outside-area files: {}",
                    ctx.slice.areas.join(", "),
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
        let mut last_error = String::new();
        for attempt in 1..=DEFAULT_REPAIR_ATTEMPTS {
            economics.set_repair_attempts(attempt);
            check_cancelled(cancel)?;
            self.mark_progress(
                &run.id,
                "integration_repair",
                "",
                attempt,
                runner.name(),
                "integration repair worker is running",
            );
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
            result.trigger = context.trigger.to_string();
            result.attempts = attempt;
            self.state.record_event(
                &run.id,
                "integration_repair_completed",
                &json!({ "status": result.status, "summary": result.summary }),
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
            "run_incident",
            &json!({
                "severity": "warning",
                "kind": "stale_worktree_removed_before_resume",
                "message": format!("removed stale run worktree directory before resume: {}", root.display()),
            }),
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

fn latest_cancel_reason(events: &[crate::domain::Event]) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.typ == "run_cancel_requested")
        .and_then(|event| event.payload.get("reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
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
                .find(|event| event.typ == "run_error")
                .and_then(|event| event.payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
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

fn final_exit_states(gate: &GateResult, completed_slices: &[WorkerResult]) -> WorkflowExitStates {
    let gate_passed = gate.status == "passed";
    WorkflowExitStates {
        run: if gate_passed { "completed" } else { "failed" }.to_string(),
        handoff: if gate_passed {
            "ready_for_handoff"
        } else {
            "not_ready"
        }
        .to_string(),
        evidence: if gate_passed {
            "daemon_attested"
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
    let mut basis = vec![
        "worker acceptance_status is treated as an evidence claim, not approval".to_string(),
        "daemon required a committed clean worktree before merge".to_string(),
        "daemon required slice verification/lightweight checks before merge".to_string(),
    ];
    if gate_passed {
        basis.push("daemon integration gate passed before handoff".to_string());
    } else {
        basis.push(format!(
            "daemon integration gate did not attest handoff: {}",
            gate.summary
        ));
    }
    EvidenceAttestation {
        status: if gate_passed {
            "daemon_attested"
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

fn effective_parallelism(requested: usize, config: &WorkflowConfig) -> usize {
    if requested > 1 {
        requested
    } else if config.parallelism > 0 {
        config.parallelism
    } else {
        requested.max(1)
    }
}

fn apply_implementer_profile_to_pi_spec(
    spec: &mut RunnerSpec,
    profile: &AgentProfile,
) -> Result<()> {
    profile.validate_required(IMPLEMENTER_PROFILE)?;
    let metadata = RunnerMetadata {
        profile: IMPLEMENTER_PROFILE.to_string(),
        provider: profile.provider.trim().to_string(),
        model: profile.model.trim().to_string(),
        reasoning: profile.reasoning.trim().to_string(),
        mode: profile.mode.trim().to_string(),
    };
    let mut args = spec.pi_args.clone();
    args.extend([
        "--provider".to_string(),
        metadata.provider.clone(),
        "--model".to_string(),
        metadata.model.clone(),
        "--thinking".to_string(),
        metadata.reasoning.clone(),
    ]);
    args.extend(profile.args.iter().cloned());
    spec.pi_args = args;
    spec.metadata = metadata;
    Ok(())
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
    Ok(())
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
        Manager, StartOptions, apply_implementer_profile_to_pi_spec, validate_repair_result,
        validate_worker_result,
    };
    use crate::agent::{
        CancellationToken, Job, ResultData, Runner, RunnerEventSink, RunnerSpec, Usage,
    };
    use crate::artifact::{self, Store as ArtifactStore};
    use crate::domain::{
        AcceptanceEvidence, AgentProfile, CheckResult, Handoff, ImplementationSummary,
        RepairResult, Run, RunStatus, Slice, SliceRun, SliceStatus, VerifyCommand, VerifyProfile,
        WorkerResult, WorkflowConfig,
    };
    use crate::gitutil;
    use crate::paths::Paths;
    use crate::state::Store as StateStore;
    use anyhow::Result;
    use chrono::Utc;
    use serde_json::json;
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
    fn pi_runner_spec_appends_required_implementer_profile() {
        let mut spec =
            RunnerSpec::from_parts("pi", "pi".to_string(), vec!["--some-user-arg".to_string()])
                .unwrap();
        apply_implementer_profile_to_pi_spec(&mut spec, &AgentProfile::implementer()).unwrap();

        assert_eq!(spec.metadata.profile, "implementer");
        assert_eq!(spec.metadata.provider, "openai");
        assert_eq!(spec.metadata.model, "gpt-5.5");
        assert_eq!(spec.metadata.reasoning, "xhigh");
        assert_eq!(spec.metadata.mode, "fast");
        assert!(spec.pi_args.ends_with(&[
            "--provider".to_string(),
            "openai".to_string(),
            "--model".to_string(),
            "gpt-5.5".to_string(),
            "--thinking".to_string(),
            "xhigh".to_string(),
        ]));
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
        let events = state.get_events(&run.id, 100)?;
        assert!(events.iter().any(|event| event.typ == "run_completed"));
        assert!(events.iter().any(|event| event.typ == "worktrees_cleaned"));
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
        })?;

        let failed = wait_for_run(&state, &run.id)?;
        assert_eq!(failed.status, RunStatus::Failed);
        assert!(failed.error.contains("daemon/operator environment"));
        assert!(
            failed
                .error
                .contains("definitely_missing_khazad_tool_for_retry_regression")
        );
        assert!(!failed.error.contains("nothing to commit"));
        let slice_runs = state.get_slice_runs(&run.id)?;
        assert_eq!(slice_runs[0].attempts, 1);
        assert_eq!(slice_runs[0].status, SliceStatus::Failed);
        let check: CheckResult =
            artifact::read_json(store.output_path(&run.id, "slice-001.check.attempt-1.json"))?;
        assert_eq!(check.failure_kind, "tool_missing");
        assert_eq!(check.findings[0].action, "operator-fix");
        let run_summary: serde_json::Value =
            artifact::read_json(store.output_path(&run.id, "run-summary.json"))?;
        assert_eq!(run_summary["status"], "failed");
        assert!(
            run_summary["primary_failure"]
                .as_str()
                .unwrap()
                .contains("daemon/operator environment")
        );
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
                parallelism: 1,
                verify_timeout_seconds: 30,
                worker_attempt_timeout_seconds: 0,
                worker_no_output_warning_seconds: 900,
                worker_termination_grace_seconds: 30,
                integration_repair: "auto".to_string(),
                gate_fail_fast: true,
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
            })
        }

        fn name(&self) -> &str {
            "fake"
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
