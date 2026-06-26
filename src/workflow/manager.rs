use super::{REPAIR_RESULT_SCHEMA, WORKER_RESULT_SCHEMA, integration_repair_prompt, worker_prompt};
use crate::agent::{CancellationToken, Job, Runner, RunnerSpec, runner_from_spec};
use crate::artifact;
use crate::domain::{
    BranchHandoff, CheckResult, Finding, GateCommandResult, GateResult, Handoff,
    ImplementationSummary, RepairResult, Run, RunInspection, RunStatus, Slice, SliceRun,
    SliceStatus, SliceValidationReport, WorkerResult,
};
use crate::gitutil;
use crate::paths::{self, Paths};
use crate::state::{Repo, Store as StateStore};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rand::RngCore;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

pub const MAX_REPAIR_ATTEMPTS: usize = 3;

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

    fn runner_for_options(&self, opts: &StartOptions) -> Result<Arc<dyn Runner>> {
        if let Some(runner) = &self.runner_override {
            return Ok(runner.clone());
        }
        Ok(runner_from_spec(RunnerSpec::from_parts(
            &opts.agent,
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

    pub fn start_run(&self, opts: StartOptions) -> Result<Run> {
        let repo = self.init_repo(&opts.repo_path)?;
        let store = artifact::Store::new(&repo.path);
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
        let runner = self.runner_for_options(&opts)?;
        let base_branch = gitutil::current_branch(&repo.path).unwrap_or_default();
        let base_sha = gitutil::head_sha(&repo.path)?;
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

        let cancel = CancellationToken::new();
        self.active.register(run.id.clone(), cancel.clone());
        let manager = self.clone();
        let bg_run = run.clone();
        thread::spawn(move || {
            let _guard = ActiveRunGuard {
                active: manager.active.clone(),
                run_id: bg_run.id.clone(),
            };
            manager.execute_run(bg_run, selected_slices, cancel, runner);
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

    pub fn branch_handoff(&self, run_id: &str) -> Result<BranchHandoff> {
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

    fn execute_run(
        &self,
        run: Run,
        slices: Vec<Slice>,
        cancel: CancellationToken,
        runner: Arc<dyn Runner>,
    ) {
        let outcome = self.run_slices(&run, &slices, &cancel, runner);
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
                    let _ = self.state.record_event(
                        &run.id,
                        "run_cancelled",
                        &json!({ "reason": message }),
                    );
                } else {
                    let _ =
                        self.state
                            .record_event(&run.id, "run_error", &json!({ "error": message }));
                }
                let _ = self.state.update_run(&run.id, status, &message);
            }
        }
    }

    fn run_slices(
        &self,
        run: &Run,
        slices: &[Slice],
        cancel: &CancellationToken,
        runner: Arc<dyn Runner>,
    ) -> Result<ImplementationSummary> {
        check_cancelled(cancel)?;
        let store = artifact::Store::new(&run.repo_path);
        store.ensure_run_dirs(&run.id)?;
        let root_worktree = self.paths.repo_worktree_dir(&run.repo_id, &run.id);
        let integration_worktree = root_worktree.join("integration");
        std::fs::create_dir_all(&root_worktree)
            .with_context(|| format!("create {}", root_worktree.display()))?;

        gitutil::worktree_add(
            &run.repo_path,
            &integration_worktree,
            &run.integration_branch,
            &run.base_sha,
        )
        .context("create integration worktree")?;

        let mut completed_slices = Vec::new();
        let mut checks = Vec::new();
        let mut dependency_summary = BTreeMap::new();

        for slice in slices {
            check_cancelled(cancel)?;
            let slice_base_sha = gitutil::head_sha(&integration_worktree)?;
            let worker = self.run_slice_worker(
                run,
                slice,
                &root_worktree,
                &integration_worktree,
                &slice_base_sha,
                &dependency_summary,
                cancel,
                runner.clone(),
            )?;
            gitutil::merge(
                &integration_worktree,
                &worker.branch,
                &format!("khazad(slice:{}): merge {}", slice.id, slice.title),
            )
            .context("merge worker branch")?;
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
            checks.extend(worker.checks);
            completed_slices.push(worker.result);
        }

        check_cancelled(cancel)?;
        let repair = self.integration_repair(
            run,
            slices,
            &integration_worktree,
            &checks,
            cancel,
            runner.clone(),
        )?;
        check_cancelled(cancel)?;
        let gate = self.integration_gate(slices, &integration_worktree, cancel)?;
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
    ) -> Result<SliceWorkerOutcome> {
        let store = artifact::Store::new(&run.repo_path);
        let worker_worktree = root_worktree.join(&slice.id);
        let worker_branch = format!("khazad/{}/{}", run.id, slice.id);
        gitutil::worktree_add(
            &run.repo_path,
            &worker_worktree,
            &worker_branch,
            &run.integration_branch,
        )
        .context("create worker worktree")?;

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
            let result = match runner.run(
                Job {
                    kind: "slice-worker".to_string(),
                    prompt,
                    cwd: worker_worktree.clone(),
                    json_schema: WORKER_RESULT_SCHEMA.to_string(),
                },
                cancel.clone(),
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

            let check =
                self.lightweight_check(slice, &worker_worktree, slice_base_sha, attempt, cancel)?;
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
        slice: &Slice,
        worker_worktree: &Path,
        base_sha: &str,
        attempt: usize,
        cancel: &CancellationToken,
    ) -> Result<CheckResult> {
        let mut check = CheckResult {
            slice_id: slice.id.clone(),
            status: "passed".to_string(),
            summary: "lightweight checks passed".to_string(),
            tests_run: Vec::new(),
            findings: Vec::new(),
            attempt,
            worker_head: String::new(),
            worktree_ok: true,
            commit_found: true,
        };

        let status = match gitutil::status_porcelain(worker_worktree) {
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

        let head = match gitutil::head_sha(worker_worktree) {
            Ok(head) => head,
            Err(err) => {
                check.status = "failed".to_string();
                check.summary = err.to_string();
                return Ok(check);
            }
        };
        check.worker_head = head.clone();
        if head == base_sha {
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

        for command in &slice.verify {
            if command.trim().is_empty() {
                continue;
            }
            check_cancelled(cancel)?;
            check.tests_run.push(command.clone());
            let output = match run_shell_command(worker_worktree, command, cancel) {
                Ok(output) => output,
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    check.status = "failed".to_string();
                    check.summary = format!("verify command failed to start: {command}: {err}");
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
                check.summary = format!("verify command failed: {command}\n{}", combined.trim());
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

    fn integration_repair(
        &self,
        run: &Run,
        slices: &[Slice],
        integration_worktree: &Path,
        checks: &[CheckResult],
        cancel: &CancellationToken,
        runner: Arc<dyn Runner>,
    ) -> Result<RepairResult> {
        let check_summary =
            serde_json::to_string_pretty(checks).unwrap_or_else(|_| "[]".to_string());
        let mut last_error = String::new();
        for _attempt in 1..=MAX_REPAIR_ATTEMPTS {
            check_cancelled(cancel)?;
            let prompt = integration_repair_prompt(
                &run.id,
                &integration_worktree.to_string_lossy(),
                slices,
                &check_summary,
            );
            let agent_result = match runner.run(
                Job {
                    kind: "integration-repair".to_string(),
                    prompt,
                    cwd: integration_worktree.to_path_buf(),
                    json_schema: REPAIR_RESULT_SCHEMA.to_string(),
                },
                cancel.clone(),
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
        slices: &[Slice],
        integration_worktree: &Path,
        cancel: &CancellationToken,
    ) -> Result<GateResult> {
        let mut commands = Vec::new();
        let mut seen = BTreeSet::new();
        for slice in slices {
            for command in &slice.verify {
                if !command.trim().is_empty() && seen.insert(command.clone()) {
                    commands.push(command.clone());
                }
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
        for command in commands {
            check_cancelled(cancel)?;
            let output = run_shell_command(integration_worktree, &command, cancel);
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
                            description: format!("integration gate failed: {command}"),
                        });
                    }
                    results.push(GateCommandResult {
                        command,
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
                            "integration gate command failed to start: {command}: {err}"
                        ),
                    });
                    results.push(GateCommandResult {
                        command,
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
    result: WorkerResult,
    checks: Vec<CheckResult>,
    branch: String,
    attempts: usize,
}

fn sh_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
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

fn run_shell_command(cwd: &Path, command: &str, cancel: &CancellationToken) -> Result<Output> {
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
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
    loop {
        if cancel.is_cancelled() {
            terminate_process_group(&mut child);
            return Err(CancelledError::new("run cancelled").into());
        }
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        thread::sleep(Duration::from_millis(100));
    }
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
        Manager, StartOptions, run_shell_command, validate_repair_result, validate_worker_result,
    };
    use crate::agent::{CancellationToken, Job, ResultData, Runner, Usage};
    use crate::artifact::{self, Store as ArtifactStore};
    use crate::domain::{
        Handoff, RepairResult, Run, RunStatus, Slice, SliceRun, SliceStatus, WorkerResult,
    };
    use crate::gitutil;
    use crate::paths::Paths;
    use crate::state::Store as StateStore;
    use anyhow::Result;
    use chrono::Utc;
    use serde_json::json;
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
            depends_on: Vec::new(),
            areas: Vec::new(),
            acceptance: vec!["done".to_string()],
            must_ask_if: Vec::new(),
            verify: Vec::new(),
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
    fn shell_command_cancellation_returns_promptly() -> Result<()> {
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            thread_cancel.cancel();
        });
        let started = Instant::now();
        let err = run_shell_command(Path::new("."), "sleep 30", &cancel).unwrap_err();
        assert!(err.to_string().contains("run cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
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
        let handoff = manager.branch_handoff(&run.id)?;
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
        fn run(&self, job: Job, cancel: CancellationToken) -> Result<ResultData> {
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
