use super::{REPAIR_RESULT_SCHEMA, WORKER_RESULT_SCHEMA, integration_repair_prompt, worker_prompt};
use crate::agent::{
    CancellationToken, Job, Runner, RunnerEvent, RunnerEventSink, RunnerSpec, runner_from_spec,
};
use crate::artifact;
use crate::domain::{
    BranchHandoff, CheckResult, Finding, GateCommandResult, GateResult, Handoff,
    HandoffActionResult, HandoffDiagnostics, ImplementationSummary, MergeConflictReport,
    RepairResult, Run, RunCheckpoint, RunInspection, RunStatus, Slice, SliceRun, SliceStatus,
    SliceValidationReport, SliceWriteResult, VerifyCommand, WorkerResult, WorkflowConfig,
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
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_REPAIR_ATTEMPTS: usize = 3;
pub const DEFAULT_VERIFY_TIMEOUT_SECONDS: u64 = 600;
const PROGRESS_OUTPUT_TAIL_BYTES: usize = 4_000;
static WORKTREE_ADD_LOCK: Mutex<()> = Mutex::new(());
type ShellProgress = Arc<dyn Fn(String) + Send + Sync + 'static>;

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
    cancel: &'a CancellationToken,
    runner: Arc<dyn Runner>,
    config: &'a WorkflowConfig,
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

    fn shell_progress_sink(
        &self,
        run_id: &str,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        command: &str,
        message: &str,
    ) -> ShellProgress {
        let reporter = self.progress_reporter(run_id);
        let scope = ProgressScope::new(phase, slice_id, attempt, command, message);
        Arc::new(move |output_tail| {
            reporter.update_output_tail(&scope, &output_tail);
        })
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
        if let Some(runner) = &self.runner_override {
            return Ok(runner.clone());
        }
        let agent = if opts.agent.trim().is_empty() {
            config.agent.clone()
        } else {
            opts.agent.clone()
        };
        Ok(runner_from_spec(RunnerSpec::from_parts(
            &agent,
            opts.pi_bin.clone(),
            opts.pi_args.clone(),
        )?))
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
        let selected_slices = artifact::topological_order(&slices, &requested)?;
        if selected_slices.is_empty() {
            bail!("no slices selected");
        }
        let selected_ids: Vec<_> = selected_slices
            .iter()
            .map(|slice| slice.id.clone())
            .collect();
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
        self.state.record_event(
            &run.id,
            "run_started",
            &json!({ "run": run, "selected_slices": selected_ids, "agent": runner.name() }),
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
        let runner = if let Some(runner) = &self.runner_override {
            runner.clone()
        } else {
            let agent = if opts.agent.trim().is_empty() {
                config.agent.clone()
            } else {
                opts.agent.clone()
            };
            runner_from_spec(RunnerSpec::from_parts(&agent, opts.pi_bin, opts.pi_args)?)
        };
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
        let completed_slices = summary
            .as_ref()
            .map(|summary| {
                summary
                    .completed_slices
                    .iter()
                    .map(|slice| slice.slice_id.clone())
                    .collect()
            })
            .unwrap_or_default();
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
        match &outcome {
            Ok(_) => {
                self.mark_progress(
                    &run.id,
                    "completed",
                    "",
                    0,
                    "",
                    "run completed; handoff artifacts are ready",
                );
                let _ =
                    self.state
                        .record_event(&run.id, "run_completed", &json!({ "run_id": run.id }));
                let _ = self.state.update_run(&run.id, RunStatus::Completed, "");
            }
            Err(err) => {
                let status = classify_run_failure(err);
                let message = err.to_string();
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
                let _ = self.state.update_run(&run.id, status, &message);
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
            let outcomes = self.run_worker_layer(
                run,
                &layer,
                &root_worktree,
                &integration_worktree,
                &slice_base_sha,
                &dependency_summary,
                cancel,
                runner.clone(),
                parallelism,
                &config,
            )?;
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
        self.mark_progress(
            &run.id,
            "integration_repair",
            "",
            0,
            "",
            "checking whether integration repair is needed",
        );
        let repair = self.integration_repair(IntegrationRepairContext {
            run,
            slices: gate_slices,
            integration_worktree: &integration_worktree,
            checks: &checks,
            cancel,
            runner: runner.clone(),
            config: &config,
        })?;
        check_cancelled(cancel)?;
        self.mark_progress(
            &run.id,
            "integration_gate",
            "",
            0,
            "",
            "running integration gate commands",
        );
        let gate =
            self.integration_gate(run, gate_slices, &integration_worktree, cancel, &config)?;
        let final_sha = gitutil::head_sha(&integration_worktree).unwrap_or_default();
        let summary = ImplementationSummary {
            run_id: run.id.clone(),
            repo_path: run.repo_path.clone(),
            integration_branch: run.integration_branch.clone(),
            base_sha: run.base_sha.clone(),
            final_sha,
            completed_slices,
            checks,
            integration_repair: repair,
            integration_gate: gate.clone(),
            created_at: Utc::now(),
        };

        let integration_store = artifact::Store::new(&integration_worktree);
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

    #[allow(clippy::too_many_arguments)]
    fn run_worker_layer(
        &self,
        run: &Run,
        layer: &[Slice],
        root_worktree: &Path,
        integration_worktree: &Path,
        slice_base_sha: &str,
        dependency_summary: &BTreeMap<String, String>,
        cancel: &CancellationToken,
        runner: Arc<dyn Runner>,
        parallelism: usize,
        config: &WorkflowConfig,
    ) -> Result<Vec<SliceWorkerOutcome>> {
        if parallelism <= 1 || layer.len() <= 1 {
            let mut outcomes = Vec::new();
            for slice in layer {
                outcomes.push(self.run_slice_worker(
                    run,
                    slice,
                    root_worktree,
                    integration_worktree,
                    slice_base_sha,
                    dependency_summary,
                    cancel,
                    runner.clone(),
                    config,
                )?);
            }
            return Ok(outcomes);
        }

        let mut queue: VecDeque<_> = layer.iter().cloned().collect();
        let mut outcomes = Vec::new();
        while !queue.is_empty() {
            let batch: Vec<_> = (0..parallelism).filter_map(|_| queue.pop_front()).collect();
            let mut handles = Vec::new();
            for slice in batch {
                let manager = self.clone();
                let run = run.clone();
                let root_worktree = root_worktree.to_path_buf();
                let integration_worktree = integration_worktree.to_path_buf();
                let slice_base_sha = slice_base_sha.to_string();
                let dependency_summary = dependency_summary.clone();
                let cancel = cancel.clone();
                let runner = runner.clone();
                let config = config.clone();
                handles.push(thread::spawn(move || {
                    manager.run_slice_worker(
                        &run,
                        &slice,
                        &root_worktree,
                        &integration_worktree,
                        &slice_base_sha,
                        &dependency_summary,
                        &cancel,
                        runner,
                        &config,
                    )
                }));
            }
            for handle in handles {
                outcomes.push(
                    handle
                        .join()
                        .map_err(|_| anyhow!("slice worker thread panicked"))??,
                );
            }
        }
        outcomes.sort_by(|a, b| a.slice.id.cmp(&b.slice.id));
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

    #[allow(clippy::too_many_arguments)]
    fn run_slice_worker(
        &self,
        run: &Run,
        slice: &Slice,
        root_worktree: &Path,
        _integration_worktree: &Path,
        slice_base_sha: &str,
        dependency_summary: &BTreeMap<String, String>,
        cancel: &CancellationToken,
        runner: Arc<dyn Runner>,
        config: &WorkflowConfig,
    ) -> Result<SliceWorkerOutcome> {
        let store = artifact::Store::new(&run.repo_path);
        let worker_worktree = root_worktree.join(&slice.id);
        let worker_branch = format!("khazad/{}/{}", run.id, slice.id);
        {
            let _git_lock = WORKTREE_ADD_LOCK
                .lock()
                .expect("worktree add mutex poisoned");
            gitutil::worktree_add(
                &run.repo_path,
                &worker_worktree,
                &worker_branch,
                slice_base_sha,
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
        for attempt in 1..=MAX_REPAIR_ATTEMPTS {
            check_cancelled(cancel)?;
            let output_path = store.output_path(
                &run.id,
                &format!("{}.worker.attempt-{attempt}.json", slice.id),
            );
            let handoff = Handoff {
                run_id: run.id.clone(),
                role: "slice-worker".to_string(),
                repo_path: run.repo_path.clone(),
                worktree_path: worker_worktree.to_string_lossy().to_string(),
                branch: worker_branch.clone(),
                slice: slice.clone(),
                dependency_summary: dependency_summary.clone(),
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
            let result = match self.run_supervised_worker_job(
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
            ) {
                Ok(result) => result,
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    last_failure = err.to_string();
                    self.state.record_event(
                        &run.id,
                        "worker_error",
                        &json!({ "slice_id": slice.id, "attempt": attempt, "error": last_failure }),
                    )?;
                    continue;
                }
            };

            let Some(output) = result.output else {
                last_failure = "worker returned no JSON output".to_string();
                continue;
            };
            let mut worker_result: WorkerResult = match serde_json::from_value(output) {
                Ok(value) => value,
                Err(err) => {
                    last_failure = format!("worker JSON did not match result model: {err}");
                    continue;
                }
            };
            if let Err(err) = validate_worker_result(&worker_result, slice) {
                last_failure = format!("worker JSON failed validation: {err}");
                continue;
            }
            artifact::write_json(&output_path, &worker_result)?;

            let check = self.lightweight_check(
                LightweightCheckContext {
                    run_id: &run.id,
                    slice,
                    worker_worktree: &worker_worktree,
                    base_sha: slice_base_sha,
                    attempt,
                    config,
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
            if attempt == MAX_REPAIR_ATTEMPTS {
                self.state.update_slice_status(
                    &run.id,
                    &slice.id,
                    SliceStatus::Failed,
                    &last_failure,
                )?;
                bail!(
                    "slice {} failed lightweight checks after {} attempts: {}",
                    slice.id,
                    MAX_REPAIR_ATTEMPTS,
                    last_failure
                );
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
        self.state
            .update_slice_status(&run.id, &slice.id, SliceStatus::Failed, &last_failure)?;
        bail!("slice {} did not become ready: {}", slice.id, last_failure)
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
        };

        let status = match gitutil::status_porcelain(ctx.worker_worktree) {
            Ok(status) => status,
            Err(err) => {
                check.status = "failed".to_string();
                check.summary = err.to_string();
                return Ok(check);
            }
        };
        if !status.trim().is_empty() {
            check.worktree_ok = false;
            check.status = "failed".to_string();
            check.summary = "worker worktree is not clean".to_string();
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
                return Ok(check);
            }
        };
        check.worker_head = head.clone();
        if head == ctx.base_sha {
            check.commit_found = false;
            check.status = "failed".to_string();
            check.summary = "worker did not create a slice commit".to_string();
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

        for command in effective_verify_commands(ctx.slice, ctx.config)? {
            if command.command.trim().is_empty() {
                continue;
            }
            check_cancelled(cancel)?;
            check.tests_run.push(command.command.clone());
            let cwd = verify_command_cwd(ctx.worker_worktree, &command)?;
            self.mark_progress(
                ctx.run_id,
                "worker_verify",
                &ctx.slice.id,
                ctx.attempt,
                &command.command,
                "running slice verification command",
            );
            let progress = self.shell_progress_sink(
                ctx.run_id,
                "worker_verify",
                &ctx.slice.id,
                ctx.attempt,
                &command.command,
                "running slice verification command",
            );
            let output = match run_shell_command(
                &cwd,
                &command.command,
                cancel,
                verify_command_timeout(ctx.slice, &command, ctx.config),
                &command.env,
                Some(progress),
            ) {
                Ok(output) => output,
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    check.status = "failed".to_string();
                    check.summary = format!(
                        "verify command failed or timed out: {}: {err}",
                        command.command
                    );
                    check.findings.push(Finding {
                        id: String::new(),
                        severity: "error".to_string(),
                        action: "auto-fix".to_string(),
                        file: String::new(),
                        line: 0,
                        description: check.summary.clone(),
                    });
                    return Ok(check);
                }
            };
            if !output.status.success() {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                check.status = "failed".to_string();
                check.summary = format!(
                    "verify command failed: {}\n{}",
                    command.command,
                    combined.trim()
                );
                check.findings.push(Finding {
                    id: String::new(),
                    severity: "error".to_string(),
                    action: "auto-fix".to_string(),
                    file: String::new(),
                    line: 0,
                    description: check.summary.clone(),
                });
                return Ok(check);
            }
        }
        Ok(check)
    }

    fn integration_repair(&self, context: IntegrationRepairContext<'_>) -> Result<RepairResult> {
        let run = context.run;
        let slices = context.slices;
        let integration_worktree = context.integration_worktree;
        let cancel = context.cancel;
        let runner = context.runner;
        let config = context.config;
        let check_summary =
            serde_json::to_string_pretty(context.checks).unwrap_or_else(|_| "[]".to_string());
        let mut last_error = String::new();
        for attempt in 1..=MAX_REPAIR_ATTEMPTS {
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
            );
            let agent_result = match self.run_supervised_worker_job(
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
            self.state.record_event(
                &run.id,
                "integration_repair_completed",
                &json!({ "status": result.status, "summary": result.summary }),
            )?;
            return Ok(result);
        }
        Err(anyhow!(
            "integration repair failed after {} attempts: {}",
            MAX_REPAIR_ATTEMPTS,
            last_error
        ))
    }

    fn integration_gate(
        &self,
        run: &Run,
        slices: &[Slice],
        integration_worktree: &Path,
        cancel: &CancellationToken,
        config: &WorkflowConfig,
    ) -> Result<GateResult> {
        let mut commands: BTreeMap<String, VerifyCommand> = BTreeMap::new();
        for slice in slices {
            for command in effective_verify_commands(slice, config)? {
                if command.command.trim().is_empty() {
                    continue;
                }
                let key = verify_command_key(&command);
                commands
                    .entry(key)
                    .and_modify(|existing| {
                        if command.timeout_seconds > existing.timeout_seconds {
                            existing.timeout_seconds = command.timeout_seconds;
                        }
                    })
                    .or_insert(command);
            }
        }
        if commands.is_empty() {
            return Ok(GateResult {
                status: "passed".to_string(),
                summary: "no integration gate commands configured".to_string(),
                commands: Vec::new(),
                findings: Vec::new(),
            });
        }

        let mut results = Vec::new();
        let mut findings = Vec::new();
        for (_, command) in commands {
            check_cancelled(cancel)?;
            let cwd = verify_command_cwd(integration_worktree, &command)?;
            self.mark_progress(
                &run.id,
                "integration_gate",
                "",
                0,
                &command.command,
                "running integration gate command",
            );
            let progress = self.shell_progress_sink(
                &run.id,
                "integration_gate",
                "",
                0,
                &command.command,
                "running integration gate command",
            );
            let output = run_shell_command(
                &cwd,
                &command.command,
                cancel,
                verify_command_timeout_for_command(&command, config),
                &command.env,
                Some(progress),
            );
            match output {
                Ok(output) => {
                    let combined = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let status = if output.status.success() {
                        "passed"
                    } else {
                        "failed"
                    };
                    if !output.status.success() {
                        findings.push(Finding {
                            id: String::new(),
                            severity: "error".to_string(),
                            action: "auto-fix".to_string(),
                            file: String::new(),
                            line: 0,
                            description: format!("integration gate failed: {}", command.command),
                        });
                    }
                    results.push(GateCommandResult {
                        command: command.command,
                        status: status.to_string(),
                        exit_code: output.status.code(),
                        output: combined.trim().to_string(),
                    });
                }
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    findings.push(Finding {
                        id: String::new(),
                        severity: "error".to_string(),
                        action: "auto-fix".to_string(),
                        file: String::new(),
                        line: 0,
                        description: format!(
                            "integration gate command failed to start or timed out: {}: {err}",
                            command.command
                        ),
                    });
                    results.push(GateCommandResult {
                        command: command.command,
                        status: "failed".to_string(),
                        exit_code: None,
                        output: err.to_string(),
                    });
                }
            }
        }
        let failed = results.iter().any(|result| result.status != "passed");
        Ok(GateResult {
            status: if failed { "failed" } else { "passed" }.to_string(),
            summary: if failed {
                "one or more integration gate commands failed".to_string()
            } else {
                "integration gate passed".to_string()
            },
            commands: results,
            findings,
        })
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

struct LightweightCheckContext<'a> {
    run_id: &'a str,
    slice: &'a Slice,
    worker_worktree: &'a Path,
    base_sha: &'a str,
    attempt: usize,
    config: &'a WorkflowConfig,
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

fn effective_verify_commands(slice: &Slice, config: &WorkflowConfig) -> Result<Vec<VerifyCommand>> {
    let mut commands = Vec::new();
    if !slice.verify_profile.trim().is_empty() {
        let profile = config
            .verify_profiles
            .get(&slice.verify_profile)
            .ok_or_else(|| {
                anyhow!(
                    "slice {} references missing verify_profile {:?}",
                    slice.id,
                    slice.verify_profile
                )
            })?;
        commands.extend(profile.commands.clone());
    }
    commands.extend(slice.verify.iter().cloned().map(|command| VerifyCommand {
        command,
        timeout_seconds: slice.verify_timeout_seconds,
        cwd: String::new(),
        env: BTreeMap::new(),
    }));
    Ok(commands)
}

fn verify_command_timeout(
    slice: &Slice,
    command: &VerifyCommand,
    config: &WorkflowConfig,
) -> Duration {
    if command.timeout_seconds > 0 {
        Duration::from_secs(command.timeout_seconds)
    } else if slice.verify_timeout_seconds > 0 {
        Duration::from_secs(slice.verify_timeout_seconds)
    } else {
        verify_command_timeout_for_command(command, config)
    }
}

fn verify_command_timeout_for_command(
    command: &VerifyCommand,
    config: &WorkflowConfig,
) -> Duration {
    let seconds = if command.timeout_seconds > 0 {
        command.timeout_seconds
    } else if config.verify_timeout_seconds > 0 {
        config.verify_timeout_seconds
    } else {
        DEFAULT_VERIFY_TIMEOUT_SECONDS
    };
    Duration::from_secs(seconds)
}

fn verify_command_cwd(root: &Path, command: &VerifyCommand) -> Result<PathBuf> {
    if command.cwd.trim().is_empty() || command.cwd.trim() == "." {
        return Ok(root.to_path_buf());
    }
    let cwd = Path::new(&command.cwd);
    if cwd.is_absolute() || command.cwd.contains("..") {
        bail!("verify command cwd must be repo-relative and may not contain '..'");
    }
    Ok(root.join(cwd))
}

fn verify_command_key(command: &VerifyCommand) -> String {
    let env = command
        .env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(";");
    format!("{}\0{}\0{}", command.cwd, env, command.command)
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

fn run_shell_command(
    cwd: &Path,
    command: &str,
    cancel: &CancellationToken,
    timeout: Duration,
    env: &BTreeMap<String, String>,
    progress: Option<ShellProgress>,
) -> Result<Output> {
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Create a process group for the shell and its children so cancellation can
    // kill a hanging verify/gate command instead of only killing the shell.
    unsafe {
        process.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = process.spawn()?;
    let stdout = child.stdout.take().context("command stdout")?;
    let stderr = child.stderr.take().context("command stderr")?;
    let monitor = ShellCommandMonitor::spawn(stdout, stderr, progress);

    let started_at = Instant::now();
    let mut last_heartbeat = Instant::now();
    let status = loop {
        if cancel.is_cancelled() {
            terminate_process_group(&mut child);
            let _ = monitor.finish();
            return Err(CancelledError::new("run cancelled").into());
        }
        if !timeout.is_zero() && started_at.elapsed() >= timeout {
            terminate_process_group(&mut child);
            let _ = monitor.finish();
            bail!("command timed out after {} seconds", timeout.as_secs());
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            monitor.emit_progress();
            last_heartbeat = Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    };
    let (stdout, stderr) = monitor.finish();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

struct ShellCommandMonitor {
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    combined_tail: Arc<Mutex<Vec<u8>>>,
    progress: Option<ShellProgress>,
    stdout_thread: thread::JoinHandle<()>,
    stderr_thread: thread::JoinHandle<()>,
}

impl ShellCommandMonitor {
    fn spawn(
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        progress: Option<ShellProgress>,
    ) -> Self {
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let combined_tail = Arc::new(Mutex::new(Vec::new()));
        let stdout_thread = spawn_output_reader(
            stdout,
            stdout_buf.clone(),
            combined_tail.clone(),
            progress.clone(),
        );
        let stderr_thread = spawn_output_reader(
            stderr,
            stderr_buf.clone(),
            combined_tail.clone(),
            progress.clone(),
        );
        Self {
            stdout_buf,
            stderr_buf,
            combined_tail,
            progress,
            stdout_thread,
            stderr_thread,
        }
    }

    fn emit_progress(&self) {
        if let Some(progress) = &self.progress {
            progress(tail_text(&self.combined_tail));
        }
    }

    fn finish(self) -> (Vec<u8>, Vec<u8>) {
        let Self {
            stdout_buf,
            stderr_buf,
            stdout_thread,
            stderr_thread,
            ..
        } = self;
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        (
            stdout_buf.lock().expect("stdout mutex poisoned").clone(),
            stderr_buf.lock().expect("stderr mutex poisoned").clone(),
        )
    }
}

fn spawn_output_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream_buf: Arc<Mutex<Vec<u8>>>,
    combined_tail: Arc<Mutex<Vec<u8>>>,
    progress: Option<ShellProgress>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        loop {
            let read = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            stream_buf
                .lock()
                .expect("stream mutex poisoned")
                .extend_from_slice(&buf[..read]);
            let output_tail = {
                let mut combined = combined_tail.lock().expect("combined mutex poisoned");
                combined.extend_from_slice(&buf[..read]);
                if combined.len() > PROGRESS_OUTPUT_TAIL_BYTES {
                    let remove = combined.len() - PROGRESS_OUTPUT_TAIL_BYTES;
                    combined.drain(0..remove);
                }
                String::from_utf8_lossy(&combined).to_string()
            };
            if let Some(progress) = &progress
                && last_emit.elapsed() >= Duration::from_millis(500)
            {
                progress(output_tail);
                last_emit = Instant::now();
            }
        }
        if let Some(progress) = &progress {
            progress(tail_text(&combined_tail));
        }
    })
}

fn tail_text(combined_tail: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&combined_tail.lock().expect("combined mutex poisoned")).to_string()
}

fn terminate_process_group(child: &mut std::process::Child) {
    let pgid = -(child.id() as i32);
    unsafe {
        let _ = libc::kill(pgid, libc::SIGTERM);
    }
    for _ in 0..10 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    // Always escalate to the whole process group. The shell may exit after
    // SIGTERM while a descendant in the same process group ignores it.
    unsafe {
        let _ = libc::kill(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
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

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CancelledError::new("run cancelled").into())
    } else {
        Ok(())
    }
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
struct CancelledError {
    reason: String,
}

impl CancelledError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for CancelledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for CancelledError {}

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
        Manager, PROGRESS_OUTPUT_TAIL_BYTES, ShellProgress, StartOptions, run_shell_command,
        validate_repair_result, validate_worker_result,
    };
    use crate::agent::{CancellationToken, Job, ResultData, Runner, RunnerEventSink, Usage};
    use crate::artifact::{self, Store as ArtifactStore};
    use crate::domain::{
        Handoff, RepairResult, Run, RunStatus, Slice, SliceRun, SliceStatus, VerifyCommand,
        VerifyProfile, WorkerResult, WorkflowConfig,
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
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn slice(id: &str) -> Slice {
        Slice {
            id: id.to_string(),
            title: format!("Title {id}"),
            goal: "Goal".to_string(),
            github_issue: String::new(),
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
    fn shell_command_timeout_returns_promptly() -> Result<()> {
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let err = run_shell_command(
            Path::new("."),
            "sleep 30",
            &cancel,
            Duration::from_secs(1),
            &BTreeMap::new(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn shell_command_cancellation_returns_promptly() -> Result<()> {
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            thread_cancel.cancel();
        });
        let started = Instant::now();
        let err = run_shell_command(
            Path::new("."),
            "sleep 30",
            &cancel,
            Duration::from_secs(30),
            &BTreeMap::new(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("run cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn shell_command_progress_is_bounded_and_preserves_streams() -> Result<()> {
        let cancel = CancellationToken::new();
        let tails = Arc::new(Mutex::new(Vec::new()));
        let observed_tails = tails.clone();
        let progress: ShellProgress = Arc::new(move |tail| {
            observed_tails
                .lock()
                .expect("progress tails mutex poisoned")
                .push(tail);
        });

        let output = run_shell_command(
            Path::new("."),
            "printf '%05000d' 0 | tr '0' 'o'; printf 'err-line\\n' >&2",
            &cancel,
            Duration::from_secs(5),
            &BTreeMap::new(),
            Some(progress),
        )?;

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 5000);
        assert_eq!(String::from_utf8_lossy(&output.stderr), "err-line\n");
        let tails = tails.lock().expect("progress tails mutex poisoned");
        assert!(
            !tails.is_empty(),
            "progress sink should receive output tails"
        );
        assert!(
            tails
                .iter()
                .all(|tail| tail.len() <= PROGRESS_OUTPUT_TAIL_BYTES),
            "progress tails should stay bounded: {tails:#?}"
        );
        assert!(
            tails
                .iter()
                .any(|tail| tail.contains("err-line") || tail.contains('o')),
            "progress should include streamed stdout or stderr: {tails:#?}"
        );
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
        gitutil::run(repo.path(), &["add", ".workflow/slices"])?;
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
        gitutil::run(repo.path(), &["add", ".workflow/slices"])?;
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
        assert!(
            !paths
                .repo_worktree_dir(&completed.repo_id, &run.id)
                .exists()
        );
        let handoff = manager.branch_handoff(&run.id, false, false, false)?;
        assert_eq!(handoff.integration_branch, completed.integration_branch);
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
                base_branch: String::new(),
                handoff: Default::default(),
                verify_profiles: profiles,
            },
        )?;
        let mut first = slice("slice-001");
        first.verify_profile = "quick".to_string();
        artifact::write_json(store.slices_dir().join("slice-001.json"), &first)?;
        gitutil::run(repo.path(), &["add", ".workflow"])?;
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
        gitutil::run(repo.path(), &["add", ".workflow/slices"])?;
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
        gitutil::run(repo.path(), &["add", ".workflow/slices"])?;
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
                    text: "{}".to_string(),
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
            Ok(ResultData {
                text: "{}".to_string(),
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "implemented",
                    "commit_sha": sha,
                    "changed_files": [format!("{}.txt", handoff.slice.id)]
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
                    text: "{}".to_string(),
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
            Ok(ResultData {
                text: "{}".to_string(),
                output: Some(json!({
                    "slice_id": handoff.slice.id,
                    "status": "complete",
                    "summary": "implemented conflicting shared file",
                    "commit_sha": sha,
                    "changed_files": ["shared.txt"]
                })),
                usage: Usage::default(),
            })
        }

        fn name(&self) -> &str {
            "conflict"
        }
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
