use super::economics::RunEconomicsRecorder;
use super::shell::{ShellCommand, ShellCommandError, ShellProgress};
use super::{CancelledError, check_cancelled};
use crate::agent::CancellationToken;
use crate::domain::{
    CommandExecutionEconomics, Finding, GateCommandResult, GateResult, RuntimeConfig, Slice,
    VerifyCommand, WorkflowConfig,
};
use crate::gitutil;
use crate::state::{ProgressReporter, ProgressScope};
use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_VERIFY_TIMEOUT_SECONDS: u64 = 600;

#[cfg(test)]
static PAUSE_INTEGRATION_GATE_BEFORE_OUTER_GUARD: std::sync::LazyLock<
    Mutex<Vec<(PathBuf, PathBuf, PathBuf)>>,
> = std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

#[cfg(test)]
fn pause_next_integration_gate_before_outer_guard(
    integration_worktree: &Path,
    marker: &Path,
    release: &Path,
) {
    PAUSE_INTEGRATION_GATE_BEFORE_OUTER_GUARD
        .lock()
        .expect("integration gate pause lock")
        .push((
            integration_worktree.to_path_buf(),
            marker.to_path_buf(),
            release.to_path_buf(),
        ));
}

#[cfg(test)]
fn maybe_pause_integration_gate_before_outer_guard(integration_worktree: &Path) -> Result<()> {
    let pause = {
        let mut pauses = PAUSE_INTEGRATION_GATE_BEFORE_OUTER_GUARD
            .lock()
            .expect("integration gate pause lock");
        pauses
            .iter()
            .position(|(target, _, _)| target == integration_worktree)
            .map(|position| pauses.remove(position))
    };
    let Some((_, marker, release)) = pause else {
        return Ok(());
    };
    std::fs::write(&marker, b"paused\n")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !release.exists() {
        if Instant::now() >= deadline {
            bail!("timed out waiting to release integration gate outer-guard pause");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

pub(crate) struct WorkflowGate {
    progress: ProgressReporter,
    economics: Option<RunEconomicsRecorder>,
    cache: VerificationCommandCache,
    runtime: RuntimeConfig,
    output_dir: Option<PathBuf>,
    termination_grace: Duration,
}

impl WorkflowGate {
    #[cfg(test)]
    pub(crate) fn new(progress: ProgressReporter) -> Self {
        Self {
            progress,
            economics: None,
            cache: VerificationCommandCache::default(),
            runtime: RuntimeConfig::default(),
            output_dir: None,
            termination_grace: Duration::from_secs(1),
        }
    }

    pub(crate) fn with_economics(
        progress: ProgressReporter,
        economics: RunEconomicsRecorder,
        cache: VerificationCommandCache,
    ) -> Self {
        Self {
            progress,
            economics: Some(economics),
            cache,
            runtime: RuntimeConfig::default(),
            output_dir: None,
            termination_grace: Duration::from_secs(1),
        }
    }

    pub(crate) fn runtime_output(
        mut self,
        runtime: RuntimeConfig,
        output_dir: PathBuf,
        termination_grace_seconds: u64,
    ) -> Self {
        self.runtime = runtime;
        self.output_dir = Some(output_dir);
        self.termination_grace = Duration::from_secs(termination_grace_seconds);
        self
    }

    pub(crate) fn verify_slice_commands(
        &self,
        request: SliceVerificationRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<SliceVerificationResult> {
        let mut result = SliceVerificationResult::default();
        // Slice verification is deliberately authority-local: a worker can only
        // be failed here for commands the slice author wrote on the slice
        // itself. `verify_profile` is gate-only so broad repo checks such as
        // clippy do not become out-of-area worker blockers.
        for command in slice_verify_commands(request.slice) {
            if command.command.trim().is_empty() {
                continue;
            }
            check_cancelled(cancel)?;
            result.tests_run.push(command.command.clone());
            let outcome = self.run_verify_command(
                VerifyCommandExecutionRequest {
                    phase: "worker_verify",
                    slice_id: &request.slice.id,
                    attempt: request.attempt,
                    worktree_root: request.worker_worktree,
                    command: &command,
                    timeout: verify_command_timeout(request.slice, &command, request.config),
                    message: "running slice verification command",
                    cacheable: true,
                    enforce_purity: true,
                },
                cancel,
            )?;
            if outcome.cancelled && outcome.result.verification_workspace.is_none() {
                return Err(CancelledError::new("run cancelled").into());
            }
            result.verification_cancelled |= outcome.cancelled;
            result.commands.push(outcome.result.clone());
            if outcome.result.status != "passed" {
                result.failure = Some(SliceVerificationFailure::from_command_result(
                    &command.command,
                    &outcome.result,
                ));
                return Ok(result);
            }
        }
        Ok(result)
    }

    pub(crate) fn run_integration_gate(
        &self,
        request: IntegrationGateRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<GateResult> {
        let gate_root = verify_command_cwd(request.integration_worktree, &VerifyCommand::default())
            .context("pin integration gate worktree for the full gate")?;
        let pinned_gate_root =
            pinned_directory_path(&gate_root.root_directory, request.integration_worktree);
        let publication_identity = gitutil::completion_publication_root_identity(&pinned_gate_root)
            .context("capture integration gate publication identity")?;
        let gate_guard = match gitutil::VerificationWorktreeGuard::capture_pinned(
            request.integration_worktree,
            &gate_root.root_directory,
        ) {
            Ok(guard) if guard.is_clean() => guard,
            Ok(guard) => {
                return Ok(GateResult {
                    status: "failed".to_string(),
                    summary: "integration gate requires a clean worktree".to_string(),
                    verification_cancelled: false,
                    failure_kind: String::new(),
                    verification_workspace: None,
                    commands: Vec::new(),
                    findings: vec![Finding {
                        id: "integration_gate_workspace_dirty".to_string(),
                        severity: "error".to_string(),
                        action: "operator-fix".to_string(),
                        file: String::new(),
                        line: 0,
                        description: format!(
                            "integration gate prestate {} is dirty",
                            guard.snapshot_digest()
                        ),
                    }],
                    approved_workspace: None,
                    publication_identity: Vec::new(),
                });
            }
            Err(err) => {
                return Ok(GateResult {
                    status: "failed".to_string(),
                    summary: "integration gate snapshot failed".to_string(),
                    verification_cancelled: false,
                    failure_kind: String::new(),
                    verification_workspace: None,
                    commands: Vec::new(),
                    findings: vec![Finding {
                        id: "integration_gate_snapshot_failed".to_string(),
                        severity: "error".to_string(),
                        action: "operator-fix".to_string(),
                        file: String::new(),
                        line: 0,
                        description: format!(
                            "could not pin the integration gate prestate: {err:#}"
                        ),
                    }],
                    approved_workspace: None,
                    publication_identity: Vec::new(),
                });
            }
        };
        let approved_workspace = gate_guard
            .precommand_evidence()
            .before
            .expect("captured gate snapshot includes before evidence");
        let commands = integration_gate_commands(request.slices, request.config)?;

        let mut results = Vec::new();
        let mut findings = Vec::new();
        let mut failed = false;
        let mut verification_cancelled = false;
        let mut failure_kind = String::new();
        let mut verification_workspace = None;
        for command in commands {
            if cancel.is_cancelled() {
                verification_cancelled = true;
                break;
            }
            if failed && request.config.gate_fail_fast {
                results.push(self.skipped_command_result(
                    request.integration_worktree,
                    &command,
                    "skipped because gate_fail_fast stopped after an earlier failure",
                )?);
                continue;
            }
            let outcome = self.run_verify_command(
                VerifyCommandExecutionRequest {
                    phase: "integration_gate",
                    slice_id: "",
                    attempt: 0,
                    worktree_root: request.integration_worktree,
                    command: &command,
                    timeout: verify_command_timeout_for_command(&command, request.config),
                    message: "running integration gate command",
                    cacheable: true,
                    enforce_purity: true,
                },
                cancel,
            )?;
            verification_cancelled |= outcome.cancelled;
            let stop_after_cancelled_command = outcome.cancelled;
            if outcome.result.status == "failed" {
                failed = true;
                findings.push(Finding {
                    id: String::new(),
                    severity: "error".to_string(),
                    action: finding_action_for_failure_kind(&outcome.result.failure_kind)
                        .to_string(),
                    file: String::new(),
                    line: 0,
                    description: integration_gate_failure_description(
                        &command.command,
                        &outcome.result,
                    ),
                });
            }
            results.push(outcome.result);
            if stop_after_cancelled_command {
                break;
            }
        }
        #[cfg(test)]
        maybe_pause_integration_gate_before_outer_guard(request.integration_worktree)?;
        verification_cancelled |= cancel.is_cancelled();
        match gate_guard.finish() {
            gitutil::VerificationGuardOutcome::Unchanged => {}
            gitutil::VerificationGuardOutcome::Mutation(mutation) => {
                failed = true;
                failure_kind = if mutation.restoration_succeeded {
                    "verification_mutated_worktree"
                } else {
                    "verification_restoration_failed"
                }
                .to_string();
                findings.push(Finding {
                    id: failure_kind.clone(),
                    severity: "error".to_string(),
                    action: if mutation.restoration_succeeded {
                        "fix"
                    } else {
                        "operator-fix"
                    }
                    .to_string(),
                    file: String::new(),
                    line: 0,
                    description: if mutation.restoration_succeeded {
                        "integration workspace changed while the gate was running and was restored"
                            .to_string()
                    } else {
                        "integration workspace changed while the gate was running and could not be restored"
                            .to_string()
                    },
                });
                verification_workspace = Some(mutation.evidence);
            }
        }
        match gitutil::completion_publication_root_identity(&pinned_gate_root) {
            Ok(publication_identity_after)
                if publication_identity_after == publication_identity => {}
            Ok(_) => {
                failed = true;
                findings.push(Finding {
                    id: "integration_gate_repository_identity_changed".to_string(),
                    severity: "error".to_string(),
                    action: "operator-fix".to_string(),
                    file: String::new(),
                    line: 0,
                    description:
                        "integration worktree or Git administration changed while the gate was running"
                            .to_string(),
                });
            }
            Err(err) => {
                failed = true;
                findings.push(Finding {
                    id: "integration_gate_repository_identity_unavailable".to_string(),
                    severity: "error".to_string(),
                    action: "operator-fix".to_string(),
                    file: String::new(),
                    line: 0,
                    description: format!(
                        "could not revalidate integration worktree or Git administration after the gate: {err:#}"
                    ),
                });
            }
        }
        let no_commands = results.is_empty();
        Ok(GateResult {
            status: if failed { "failed" } else { "passed" }.to_string(),
            summary: if failed {
                "one or more integration gate checks failed".to_string()
            } else if no_commands {
                "no integration gate commands configured".to_string()
            } else {
                "integration gate passed".to_string()
            },
            verification_cancelled,
            failure_kind,
            verification_workspace,
            commands: results,
            findings,
            approved_workspace: (!failed).then_some(approved_workspace),
            publication_identity: if failed {
                Vec::new()
            } else {
                publication_identity
            },
        })
    }

    pub(crate) fn run_worktree_setup(
        &self,
        request: WorktreeSetupRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<GateResult> {
        let commands: Vec<_> = request
            .config
            .worktree_setup
            .iter()
            .filter(|command| !command.command.trim().is_empty())
            .collect();
        if commands.is_empty() {
            return Ok(GateResult {
                status: "passed".to_string(),
                summary: "no worktree setup commands configured".to_string(),
                verification_cancelled: false,
                failure_kind: String::new(),
                verification_workspace: None,
                commands: Vec::new(),
                findings: Vec::new(),
                approved_workspace: None,
                publication_identity: Vec::new(),
            });
        }

        let mut results = Vec::new();
        let mut findings = Vec::new();
        for command in commands {
            check_cancelled(cancel)?;
            let outcome = self.run_verify_command(
                VerifyCommandExecutionRequest {
                    phase: "worktree_setup",
                    slice_id: request.slice_id,
                    attempt: request.attempt,
                    worktree_root: request.worktree,
                    command,
                    timeout: verify_command_timeout_for_command(command, request.config),
                    message: "running worktree setup command",
                    cacheable: false,
                    enforce_purity: false,
                },
                cancel,
            )?;
            if outcome.cancelled {
                return Err(CancelledError::new("run cancelled").into());
            }
            if outcome.result.status == "failed" {
                findings.push(worktree_setup_failure_finding(
                    &command.command,
                    &outcome.result,
                ));
                results.push(outcome.result);
                return Ok(GateResult {
                    status: "failed".to_string(),
                    summary: "worktree setup command failed".to_string(),
                    verification_cancelled: false,
                    failure_kind: String::new(),
                    verification_workspace: None,
                    commands: results,
                    findings,
                    approved_workspace: None,
                    publication_identity: Vec::new(),
                });
            }
            results.push(outcome.result);
        }

        let status = gitutil::status_porcelain(request.worktree)?;
        if !status.trim().is_empty() {
            return Ok(GateResult {
                status: "failed".to_string(),
                summary: "worktree setup left non-ignored changes".to_string(),
                verification_cancelled: false,
                failure_kind: String::new(),
                verification_workspace: None,
                commands: results,
                findings: vec![Finding {
                    id: "worktree_setup_dirty".to_string(),
                    severity: "error".to_string(),
                    action: "operator-fix".to_string(),
                    file: String::new(),
                    line: 0,
                    description: format!(
                        "worktree setup must leave the git worktree clean except ignored files; git status --porcelain:\n{}",
                        status.trim()
                    ),
                }],
                approved_workspace: None,
                publication_identity: Vec::new(),
            });
        }

        Ok(GateResult {
            status: "passed".to_string(),
            summary: "worktree setup passed".to_string(),
            verification_cancelled: false,
            failure_kind: String::new(),
            verification_workspace: None,
            commands: results,
            findings: Vec::new(),
            approved_workspace: None,
            publication_identity: Vec::new(),
        })
    }

    fn run_verify_command(
        &self,
        request: VerifyCommandExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<VerifyCommandExecutionOutcome> {
        let cwd_label = verify_command_cwd_label(request.command);
        let dedupe_key = verify_command_key(request.command);
        let fallback_tree_sha = command_tree_sha(request.worktree_root);
        let cwd = match verify_command_cwd(request.worktree_root, request.command) {
            Ok(cwd) => cwd,
            Err(err) => {
                let cache_key = command_cache_key(
                    &gitutil::path_identity_digest(request.worktree_root),
                    &gitutil::path_identity_digest(request.worktree_root),
                    &fallback_tree_sha,
                    "invalid-cwd",
                    &dedupe_key,
                    request.timeout,
                );
                let result = GateCommandResult {
                    command: request.command.command.clone(),
                    status: "failed".to_string(),
                    exit_code: None,
                    output: format_command_environment_hint(
                        request.command,
                        format!("invalid verify command cwd: {err}"),
                    ),
                    cwd: cwd_label.clone(),
                    dedupe_key: dedupe_key.clone(),
                    duration_ms: 0,
                    cache_hit: false,
                    skip_reason: String::new(),
                    failure_kind: "invalid_cwd".to_string(),
                    output_total_bytes: 0,
                    output_retained_bytes: 0,
                    output_truncated: false,
                    output_spill_paths: Vec::new(),
                    verification_workspace: None,
                };
                self.record_verify_command_economics(
                    &request,
                    &result,
                    &fallback_tree_sha,
                    &cache_key,
                );
                return Ok(VerifyCommandExecutionOutcome {
                    result,
                    cancelled: false,
                });
            }
        };
        let root_identity =
            gitutil::pinned_path_identity_digest(&cwd.root_directory, request.worktree_root)?;
        let cwd_identity = gitutil::pinned_path_identity_digest(&cwd.directory, &cwd.path)?;
        let tree_sha = command_tree_sha(&pinned_directory_path(
            &cwd.root_directory,
            request.worktree_root,
        ));

        let verification_guard = if request.enforce_purity {
            match gitutil::VerificationWorktreeGuard::capture_pinned(
                request.worktree_root,
                &cwd.root_directory,
            ) {
                Ok(guard) if guard.is_clean() => Some(guard),
                Ok(guard) => {
                    let snapshot_digest = guard.snapshot_digest();
                    let cache_key = command_cache_key(
                        &root_identity,
                        &cwd_identity,
                        &tree_sha,
                        &snapshot_digest,
                        &dedupe_key,
                        request.timeout,
                    );
                    let result = GateCommandResult {
                        command: request.command.command.clone(),
                        status: "failed".to_string(),
                        exit_code: None,
                        output: format!(
                            "verification requires a clean worktree; pre-command snapshot {snapshot_digest} is dirty"
                        ),
                        cwd: cwd_label.clone(),
                        dedupe_key: dedupe_key.clone(),
                        duration_ms: 0,
                        cache_hit: false,
                        skip_reason: String::new(),
                        failure_kind: "verification_workspace_dirty".to_string(),
                        output_total_bytes: 0,
                        output_retained_bytes: 0,
                        output_truncated: false,
                        output_spill_paths: Vec::new(),
                        verification_workspace: Some(guard.precommand_evidence()),
                    };
                    self.record_verify_command_economics(&request, &result, &tree_sha, &cache_key);
                    return Ok(VerifyCommandExecutionOutcome {
                        result,
                        cancelled: false,
                    });
                }
                Err(err) => {
                    let cache_key = command_cache_key(
                        &root_identity,
                        &cwd_identity,
                        &tree_sha,
                        "snapshot-unavailable",
                        &dedupe_key,
                        request.timeout,
                    );
                    let result = GateCommandResult {
                        command: request.command.command.clone(),
                        status: "failed".to_string(),
                        exit_code: None,
                        output: format!(
                            "could not capture verification worktree snapshot: {err:#}"
                        ),
                        cwd: cwd_label.clone(),
                        dedupe_key: dedupe_key.clone(),
                        duration_ms: 0,
                        cache_hit: false,
                        skip_reason: String::new(),
                        failure_kind: "verification_snapshot_failed".to_string(),
                        output_total_bytes: 0,
                        output_retained_bytes: 0,
                        output_truncated: false,
                        output_spill_paths: Vec::new(),
                        verification_workspace: None,
                    };
                    self.record_verify_command_economics(&request, &result, &tree_sha, &cache_key);
                    return Ok(VerifyCommandExecutionOutcome {
                        result,
                        cancelled: false,
                    });
                }
            }
        } else {
            None
        };
        let snapshot_digest = verification_guard
            .as_ref()
            .map(gitutil::VerificationWorktreeGuard::snapshot_digest)
            .unwrap_or_else(|| "purity-not-enforced".to_string());
        let ignored_digest_before = verification_guard
            .as_ref()
            .and_then(|guard| guard.cache_worktree_digest().ok());
        let cache_state_digest = format!(
            "{snapshot_digest}:ignored:{}",
            ignored_digest_before.as_deref().unwrap_or("unavailable")
        );
        let cache_key = command_cache_key(
            &root_identity,
            &cwd_identity,
            &tree_sha,
            &cache_state_digest,
            &dedupe_key,
            request.timeout,
        );
        let validate_precommand = || {
            if let Some(evidence) = verification_guard
                .as_ref()
                .and_then(gitutil::VerificationWorktreeGuard::precommand_change_evidence)
            {
                return Some((
                    "verification workspace changed concurrently before command execution; command was not started and no restoration was attempted".to_string(),
                    Some(evidence),
                ));
            }
            if request.enforce_purity {
                match (
                    ignored_digest_before.as_deref(),
                    verification_guard
                        .as_ref()
                        .expect("purity guard exists")
                        .cache_worktree_digest(),
                ) {
                    (Some(before), Ok(after)) if after == before => {}
                    (_, Ok(_)) => {
                        return Some((
                            "verification filesystem changed concurrently before command execution; command was not started and no restoration was attempted".to_string(),
                            verification_guard.as_ref().map(
                                gitutil::VerificationWorktreeGuard::precommand_evidence,
                            ),
                        ));
                    }
                    (_, Err(err)) => {
                        return Some((
                            format!(
                                "verification filesystem could not be revalidated before command execution: {err:#}; command was not started"
                            ),
                            verification_guard
                                .as_ref()
                                .map(gitutil::VerificationWorktreeGuard::precommand_evidence),
                        ));
                    }
                }
            }
            None
        };
        if let Some((output, evidence)) = validate_precommand() {
            return Ok(
                self.precommand_change_outcome(&request, &tree_sha, &cache_key, output, evidence)
            );
        }
        if request.cacheable
            && (!request.enforce_purity || ignored_digest_before.is_some())
            && let Some(mut cached) = self.cache.get(&cache_key)
        {
            if let Some((output, evidence)) = validate_precommand() {
                return Ok(self
                    .precommand_change_outcome(&request, &tree_sha, &cache_key, output, evidence));
            }
            cached.cache_hit = true;
            cached.duration_ms = 0;
            self.record_verify_command_economics(&request, &cached, &tree_sha, &cache_key);
            return Ok(VerifyCommandExecutionOutcome {
                result: cached,
                cancelled: false,
            });
        }

        self.mark_progress(
            request.phase,
            request.slice_id,
            request.attempt,
            &request.command.command,
            request.message,
        );
        let progress = self.shell_progress_sink(
            request.phase,
            request.slice_id,
            request.attempt,
            &request.command.command,
            request.message,
        );
        if let Some((output, evidence)) = validate_precommand() {
            return Ok(
                self.precommand_change_outcome(&request, &tree_sha, &cache_key, output, evidence)
            );
        }
        let started_at = Instant::now();
        let mut command = ShellCommand::new(&cwd.path, &request.command.command)
            .pinned_cwd(&cwd.directory)?
            .timeout(request.timeout)
            .termination_grace(self.termination_grace)
            .envs(&request.command.env)
            .progress(Some(progress))
            .output_bounds(
                self.runtime.retained_output_bytes,
                self.runtime.retained_output_lines,
            );
        if self.runtime.raw_output_spill
            && let Some(output_dir) = &self.output_dir
        {
            command = command.spill_to(command_output_stem(
                output_dir,
                request.phase,
                request.slice_id,
                request.attempt,
                &dedupe_key,
            ));
        }
        let output = command.run(cancel);
        let duration_ms = started_at.elapsed().as_millis();
        let supervision_failed = output.as_ref().err().is_some_and(|err| {
            err.downcast_ref::<ShellCommandError>()
                .is_some_and(|shell| shell.kind() == super::shell::ShellFailureKind::Supervision)
        });
        let cancelled = output.is_err() && cancel.is_cancelled() && !supervision_failed;
        let mut result = match output {
            Ok(output) => {
                let failure_kind = command_failure_kind(output.exit_code(), output.success());
                let output_total_bytes = output
                    .stdout_total_bytes()
                    .saturating_add(output.stderr_total_bytes());
                let output_retained_bytes = output.retained_output_bytes();
                let output_truncated = output.output_truncated();
                let output_spill_paths = [output.stdout_spill_path(), output.stderr_spill_path()]
                    .into_iter()
                    .flatten()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect();
                GateCommandResult {
                    command: request.command.command.clone(),
                    status: if output.success() { "passed" } else { "failed" }.to_string(),
                    exit_code: output.exit_code(),
                    output: maybe_add_environment_hint(
                        request.command,
                        &failure_kind,
                        output.trimmed_combined_output(),
                    ),
                    cwd: cwd_label.clone(),
                    dedupe_key: dedupe_key.clone(),
                    duration_ms,
                    cache_hit: false,
                    skip_reason: String::new(),
                    failure_kind,
                    output_total_bytes,
                    output_retained_bytes,
                    output_truncated,
                    output_spill_paths,
                    verification_workspace: None,
                }
            }
            Err(err) => {
                let failure_kind = if cancelled {
                    "cancelled".to_string()
                } else {
                    shell_error_failure_kind(err.as_ref()).to_string()
                };
                GateCommandResult {
                    command: request.command.command.clone(),
                    status: "failed".to_string(),
                    exit_code: None,
                    output: maybe_add_environment_hint(
                        request.command,
                        &failure_kind,
                        err.to_string(),
                    ),
                    cwd: cwd_label.clone(),
                    dedupe_key: dedupe_key.clone(),
                    duration_ms,
                    cache_hit: false,
                    skip_reason: String::new(),
                    failure_kind,
                    output_total_bytes: 0,
                    output_retained_bytes: 0,
                    output_truncated: false,
                    output_spill_paths: Vec::new(),
                    verification_workspace: None,
                }
            }
        };

        let mut safe_to_cache = true;
        if let Some(guard) = verification_guard.as_ref() {
            match guard.finish() {
                gitutil::VerificationGuardOutcome::Unchanged => {}
                gitutil::VerificationGuardOutcome::Mutation(mutation) => {
                    safe_to_cache = false;
                    result.status = "failed".to_string();
                    result.failure_kind = if mutation.restoration_succeeded {
                        "verification_mutated_worktree"
                    } else {
                        "verification_restoration_failed"
                    }
                    .to_string();
                    append_command_output(
                        &mut result.output,
                        verification_mutation_summary(&mutation),
                    );
                    result.verification_workspace = Some(mutation.evidence);
                }
            }
        }

        if request.enforce_purity {
            safe_to_cache &= ignored_digest_before.is_some_and(|before| {
                verification_guard.as_ref().is_some_and(|guard| {
                    guard
                        .cache_worktree_digest()
                        .is_ok_and(|after| after == before)
                })
            });
        }

        self.record_verify_command_economics(&request, &result, &tree_sha, &cache_key);
        if request.cacheable && safe_to_cache && !cancelled && result.status == "passed" {
            self.cache.insert(cache_key, result.clone());
        }
        Ok(VerifyCommandExecutionOutcome { result, cancelled })
    }

    fn skipped_command_result(
        &self,
        worktree_root: &Path,
        command: &VerifyCommand,
        reason: &str,
    ) -> Result<GateCommandResult> {
        let cwd = verify_command_cwd(worktree_root, command)?;
        let cwd_label = verify_command_cwd_label(command);
        let dedupe_key = verify_command_key(command);
        let tree_sha = command_tree_sha(worktree_root);
        let cache_key = command_cache_key(
            &gitutil::pinned_path_identity_digest(&cwd.root_directory, worktree_root)?,
            &gitutil::pinned_path_identity_digest(&cwd.directory, &cwd.path)?,
            &tree_sha,
            "skipped",
            &dedupe_key,
            Duration::from_secs(command.timeout_seconds),
        );
        self.record_command_economics(CommandExecutionEconomics {
            phase: "integration_gate".to_string(),
            slice_id: String::new(),
            attempt: 0,
            command: command.command.clone(),
            cwd: cwd_label.clone(),
            status: "skipped".to_string(),
            exit_code: None,
            duration_ms: 0,
            dedupe_key: dedupe_key.clone(),
            tree_sha,
            cache_key,
            cache_hit: false,
            skip_reason: reason.to_string(),
        });
        Ok(GateCommandResult {
            command: command.command.clone(),
            status: "skipped".to_string(),
            exit_code: None,
            output: String::new(),
            cwd: cwd_label,
            dedupe_key,
            duration_ms: 0,
            cache_hit: false,
            skip_reason: reason.to_string(),
            failure_kind: String::new(),
            output_total_bytes: 0,
            output_retained_bytes: 0,
            output_truncated: false,
            output_spill_paths: Vec::new(),
            verification_workspace: None,
        })
    }

    fn precommand_change_outcome(
        &self,
        request: &VerifyCommandExecutionRequest<'_>,
        tree_sha: &str,
        cache_key: &str,
        output: String,
        evidence: Option<crate::domain::VerificationWorkspaceEvidence>,
    ) -> VerifyCommandExecutionOutcome {
        let result = GateCommandResult {
            command: request.command.command.clone(),
            status: "failed".to_string(),
            exit_code: None,
            output,
            cwd: verify_command_cwd_label(request.command),
            dedupe_key: verify_command_key(request.command),
            duration_ms: 0,
            cache_hit: false,
            skip_reason: String::new(),
            failure_kind: "verification_precommand_changed".to_string(),
            output_total_bytes: 0,
            output_retained_bytes: 0,
            output_truncated: false,
            output_spill_paths: Vec::new(),
            verification_workspace: evidence,
        };
        self.record_verify_command_economics(request, &result, tree_sha, cache_key);
        VerifyCommandExecutionOutcome {
            result,
            cancelled: false,
        }
    }

    fn record_verify_command_economics(
        &self,
        request: &VerifyCommandExecutionRequest<'_>,
        result: &GateCommandResult,
        tree_sha: &str,
        cache_key: &str,
    ) {
        self.record_command_economics(CommandExecutionEconomics {
            phase: request.phase.to_string(),
            slice_id: request.slice_id.to_string(),
            attempt: request.attempt,
            command: request.command.command.clone(),
            cwd: result.cwd.clone(),
            status: result.status.clone(),
            exit_code: result.exit_code,
            duration_ms: result.duration_ms,
            dedupe_key: result.dedupe_key.clone(),
            tree_sha: tree_sha.to_string(),
            cache_key: cache_key.to_string(),
            cache_hit: result.cache_hit,
            skip_reason: result.skip_reason.clone(),
        });
    }

    fn record_command_economics(&self, command: CommandExecutionEconomics) {
        if let Some(economics) = &self.economics {
            economics.record_command(command);
        }
    }

    fn mark_progress(
        &self,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        command: &str,
        message: &str,
    ) {
        self.progress.mark(&ProgressScope::new(
            phase, slice_id, attempt, command, message,
        ));
    }

    fn shell_progress_sink(
        &self,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        command: &str,
        message: &str,
    ) -> ShellProgress {
        let reporter = self.progress.clone();
        let scope = ProgressScope::new(phase, slice_id, attempt, command, message);
        Arc::new(move |output_tail| {
            reporter.update_output_tail(&scope, &output_tail);
        })
    }
}

pub(crate) struct SliceVerificationRequest<'a> {
    pub(crate) slice: &'a Slice,
    pub(crate) worker_worktree: &'a Path,
    pub(crate) attempt: usize,
    pub(crate) config: &'a WorkflowConfig,
}

#[derive(Debug, Default)]
pub(crate) struct SliceVerificationResult {
    pub(crate) tests_run: Vec<String>,
    pub(crate) commands: Vec<GateCommandResult>,
    pub(crate) verification_cancelled: bool,
    pub(crate) failure: Option<SliceVerificationFailure>,
}

#[derive(Debug)]
pub(crate) struct SliceVerificationFailure {
    pub(crate) summary: String,
    pub(crate) finding: Finding,
    pub(crate) failure_kind: String,
}

impl SliceVerificationFailure {
    fn from_command_result(command: &str, result: &GateCommandResult) -> Self {
        let summary = slice_verify_failure_summary(command, result);
        Self {
            finding: Finding {
                id: String::new(),
                severity: "error".to_string(),
                action: finding_action_for_failure_kind(&result.failure_kind).to_string(),
                file: String::new(),
                line: 0,
                description: summary.clone(),
            },
            failure_kind: result.failure_kind.clone(),
            summary,
        }
    }
}

pub(crate) struct IntegrationGateRequest<'a> {
    pub(crate) slices: &'a [Slice],
    pub(crate) integration_worktree: &'a Path,
    pub(crate) config: &'a WorkflowConfig,
}

pub(crate) struct WorktreeSetupRequest<'a> {
    pub(crate) worktree: &'a Path,
    pub(crate) slice_id: &'a str,
    pub(crate) attempt: usize,
    pub(crate) config: &'a WorkflowConfig,
}

#[derive(Debug)]
struct VerifyCommandExecutionRequest<'a> {
    phase: &'a str,
    slice_id: &'a str,
    attempt: usize,
    worktree_root: &'a Path,
    command: &'a VerifyCommand,
    timeout: Duration,
    message: &'a str,
    cacheable: bool,
    enforce_purity: bool,
}

#[derive(Debug)]
struct VerifyCommandExecutionOutcome {
    result: GateCommandResult,
    cancelled: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct VerificationCommandCache {
    inner: Arc<Mutex<BTreeMap<String, GateCommandResult>>>,
}

impl VerificationCommandCache {
    fn get(&self, key: &str) -> Option<GateCommandResult> {
        self.inner
            .lock()
            .expect("verification command cache mutex poisoned")
            .get(key)
            .cloned()
    }

    fn insert(&self, key: String, result: GateCommandResult) {
        if result.status != "skipped" {
            self.inner
                .lock()
                .expect("verification command cache mutex poisoned")
                .insert(key, result);
        }
    }
}

fn integration_gate_commands(
    slices: &[Slice],
    config: &WorkflowConfig,
) -> Result<Vec<VerifyCommand>> {
    let mut commands: Vec<VerifyCommand> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for slice in slices {
        for command in gate_verify_commands(slice, config)? {
            if command.command.trim().is_empty() {
                continue;
            }
            let key = verify_command_key(&command);
            if let Some(index) = by_key.get(&key).copied() {
                if command.timeout_seconds > commands[index].timeout_seconds {
                    commands[index].timeout_seconds = command.timeout_seconds;
                }
            } else {
                by_key.insert(key, commands.len());
                commands.push(command);
            }
        }
    }
    Ok(commands)
}

fn gate_verify_commands(slice: &Slice, config: &WorkflowConfig) -> Result<Vec<VerifyCommand>> {
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
    commands.extend(slice_verify_commands(slice));
    Ok(commands)
}

fn slice_verify_commands(slice: &Slice) -> Vec<VerifyCommand> {
    slice
        .verify
        .iter()
        .cloned()
        .map(|command| VerifyCommand {
            command,
            timeout_seconds: slice.verify_timeout_seconds,
            cwd: String::new(),
            env: BTreeMap::new(),
        })
        .collect()
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

#[derive(Debug)]
struct VerifiedCommandCwd {
    path: PathBuf,
    root_directory: File,
    directory: File,
}

fn verify_command_cwd(root: &Path, command: &VerifyCommand) -> Result<VerifiedCommandCwd> {
    let anchored_root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let relative = if command.cwd.trim().is_empty() || command.cwd.trim() == "." {
        Path::new("")
    } else {
        let cwd = Path::new(&command.cwd);
        if cwd.is_absolute()
            || cwd
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            bail!("verify command cwd must be repo-relative and may not contain '..'");
        }
        cwd
    };
    let mut directory = gitutil::open_pinned_directory_nofollow(&anchored_root)
        .with_context(|| format!("pin verification worktree {}", anchored_root.display()))?;
    let root_directory = directory.try_clone()?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let component = CString::new(component.as_bytes())
            .map_err(|_| anyhow!("verify command cwd contains a NUL byte"))?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("pin verify command cwd {}", command.cwd));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(VerifiedCommandCwd {
        path: anchored_root.join(relative),
        root_directory,
        directory,
    })
}

fn verify_command_cwd_label(command: &VerifyCommand) -> String {
    if command.cwd.trim().is_empty() {
        ".".to_string()
    } else {
        command.cwd.clone()
    }
}

fn command_output_stem(
    output_dir: &Path,
    phase: &str,
    slice_id: &str,
    attempt: usize,
    dedupe_key: &str,
) -> PathBuf {
    fn safe(value: &str) -> String {
        let value = value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>();
        if value.is_empty() {
            "run".to_string()
        } else {
            value
        }
    }
    let digest = dedupe_key.get(..16).unwrap_or(dedupe_key);
    output_dir.join(format!(
        "{}-{}-attempt-{}-{digest}",
        safe(phase),
        safe(slice_id),
        attempt
    ))
}

fn verify_command_key(command: &VerifyCommand) -> String {
    let mut digest = Sha256::new();
    update_length_prefixed_digest(&mut digest, b"verify-command-v1");
    update_length_prefixed_digest(&mut digest, command.cwd.as_bytes());
    for (key, value) in &command.env {
        update_length_prefixed_digest(&mut digest, key.as_bytes());
        update_length_prefixed_digest(&mut digest, value.as_bytes());
    }
    update_length_prefixed_digest(&mut digest, command.command.as_bytes());
    format!("verify-command-v1:{}", hex::encode(digest.finalize()))
}

fn update_length_prefixed_digest(digest: &mut Sha256, field: &[u8]) {
    digest.update((field.len() as u64).to_be_bytes());
    digest.update(field);
}

#[cfg(target_os = "linux")]
fn pinned_directory_path(directory: &File, _fallback: &Path) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

#[cfg(not(target_os = "linux"))]
fn pinned_directory_path(_directory: &File, fallback: &Path) -> PathBuf {
    fallback.to_path_buf()
}

fn command_tree_sha(worktree_root: &Path) -> String {
    gitutil::run(worktree_root, &["rev-parse", "HEAD^{tree}"]).unwrap_or_else(|_| {
        format!(
            "non-git:{}",
            worktree_root
                .canonicalize()
                .unwrap_or_else(|_| worktree_root.to_path_buf())
                .display()
        )
    })
}

fn command_cache_key(
    worktree_identity: &str,
    cwd_identity: &str,
    tree_sha: &str,
    snapshot_digest: &str,
    dedupe_key: &str,
    timeout: Duration,
) -> String {
    let mut digest = Sha256::new();
    for field in [
        b"verification-purity-v3".as_slice(),
        worktree_identity.as_bytes(),
        cwd_identity.as_bytes(),
        tree_sha.as_bytes(),
        snapshot_digest.as_bytes(),
        inherited_environment_digest().as_bytes(),
        dedupe_key.as_bytes(),
        timeout.as_nanos().to_string().as_bytes(),
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field);
    }
    format!("verification-purity-v3:{}", hex::encode(digest.finalize()))
}

fn inherited_environment_digest() -> String {
    let mut environment = std::env::vars_os().collect::<Vec<_>>();
    environment.sort_by(|left, right| {
        left.0
            .as_encoded_bytes()
            .cmp(right.0.as_encoded_bytes())
            .then_with(|| left.1.as_encoded_bytes().cmp(right.1.as_encoded_bytes()))
    });
    let mut digest = Sha256::new();
    for (name, value) in environment {
        for field in [name.as_encoded_bytes(), value.as_encoded_bytes()] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field);
        }
    }
    hex::encode(digest.finalize())
}

fn append_command_output(output: &mut String, evidence: String) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&evidence);
}

fn verification_mutation_summary(mutation: &gitutil::VerificationMutationOutcome) -> String {
    let evidence = &mutation.evidence;
    let before = evidence
        .before
        .as_ref()
        .map(|snapshot| snapshot.digest.as_str())
        .unwrap_or("unavailable");
    if !evidence.after_capture_error.is_empty() {
        return format!(
            "verification after-snapshot failed: {}; restoration result: {}",
            evidence.after_capture_error,
            if evidence.restoration_error.is_empty() {
                "restored"
            } else {
                evidence.restoration_error.as_str()
            }
        );
    }
    let after = evidence
        .after
        .as_ref()
        .map(|snapshot| snapshot.digest.as_str())
        .unwrap_or("unavailable");
    if mutation.restoration_succeeded {
        let restored = evidence
            .restored
            .as_ref()
            .map(|snapshot| snapshot.digest.as_str())
            .unwrap_or("unavailable");
        format!(
            "verification command changed the worktree; before={before} after={after} restored={restored}"
        )
    } else {
        format!(
            "verification command changed the worktree and restoration failed; before={before} after={after}: {}",
            evidence.restoration_error
        )
    }
}

fn command_failure_kind(exit_code: Option<i32>, success: bool) -> String {
    if success {
        return String::new();
    }
    match exit_code {
        Some(126) => "command_not_executable".to_string(),
        Some(127) => "tool_missing".to_string(),
        Some(_) => "command_failed".to_string(),
        None => "command_failed".to_string(),
    }
}

fn shell_error_failure_kind(err: &(dyn std::error::Error + Send + Sync + 'static)) -> &'static str {
    if let Some(shell) = err.downcast_ref::<ShellCommandError>() {
        return shell.kind().as_str();
    }
    "spawn_failed"
}

pub(crate) fn failure_kind_needs_operator(failure_kind: &str) -> bool {
    matches!(
        failure_kind,
        "tool_missing"
            | "command_not_executable"
            | "spawn_failed"
            | "invalid_cwd"
            | "agent_auth_required"
            | "verification_workspace_dirty"
            | "verification_snapshot_failed"
            | "verification_precommand_changed"
            | "verification_mutated_worktree"
            | "verification_restoration_failed"
            | "process_supervision_failed"
    )
}

fn finding_action_for_failure_kind(failure_kind: &str) -> &'static str {
    if failure_kind_needs_operator(failure_kind) {
        "operator-fix"
    } else {
        "auto-fix"
    }
}

fn slice_verify_failure_summary(command: &str, result: &GateCommandResult) -> String {
    let prefix = if failure_kind_needs_operator(&result.failure_kind) {
        "verify command failed due to daemon/operator environment"
    } else if result.failure_kind == "timeout" {
        "verify command timed out"
    } else {
        "verify command failed"
    };
    if result.output.trim().is_empty() {
        format!("{prefix}: {command}")
    } else {
        format!("{prefix}: {command}\n{}", result.output)
    }
}

fn integration_gate_failure_description(command: &str, result: &GateCommandResult) -> String {
    if failure_kind_needs_operator(&result.failure_kind) {
        format!(
            "integration gate failed due to daemon/operator environment: {command}: {}",
            result.output
        )
    } else if result.failure_kind == "timeout" {
        format!(
            "integration gate command timed out: {command}: {}",
            result.output
        )
    } else if result.exit_code.is_some() {
        format!("integration gate failed: {command}")
    } else {
        format!(
            "integration gate command failed to start or timed out: {command}: {}",
            result.output
        )
    }
}

fn worktree_setup_failure_finding(command: &str, result: &GateCommandResult) -> Finding {
    Finding {
        id: "worktree_setup_failed".to_string(),
        severity: "error".to_string(),
        action: "operator-fix".to_string(),
        file: String::new(),
        line: 0,
        description: worktree_setup_failure_description(command, result),
    }
}

fn worktree_setup_failure_description(command: &str, result: &GateCommandResult) -> String {
    if result.failure_kind == "timeout" {
        format!(
            "worktree setup command timed out: {command}: {}",
            result.output
        )
    } else if result.output.trim().is_empty() {
        format!("worktree setup command failed: {command}")
    } else {
        format!(
            "worktree setup command failed: {command}\n{}",
            result.output
        )
    }
}

fn maybe_add_environment_hint(
    command: &VerifyCommand,
    failure_kind: &str,
    output: String,
) -> String {
    if failure_kind_needs_operator(failure_kind) {
        format_command_environment_hint(command, output)
    } else {
        output
    }
}

fn format_command_environment_hint(command: &VerifyCommand, output: String) -> String {
    let path = command
        .env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let mut parts = Vec::new();
    if !output.trim().is_empty() {
        parts.push(output.trim().to_string());
    }
    parts.push(format!("daemon PATH={path}"));
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        IntegrationGateRequest, SliceVerificationRequest, WorkflowGate,
        pause_next_integration_gate_before_outer_guard, verify_command_cwd, verify_command_key,
    };
    use crate::agent::CancellationToken;
    use crate::domain::{RuntimeConfig, Slice, VerifyCommand, VerifyProfile, WorkflowConfig};
    use crate::state::{ProgressReporter, Store as StateStore};
    use anyhow::Result;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
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

    fn test_gate() -> Result<(tempfile::TempDir, WorkflowGate)> {
        let home = tempfile::tempdir()?;
        let state = StateStore::open(home.path().join("state.sqlite"))?;
        let gate = WorkflowGate::new(ProgressReporter::new(state, "run-1"));
        Ok((home, gate))
    }

    fn clean_git_worktree() -> Result<tempfile::TempDir> {
        let worktree = tempfile::tempdir()?;
        crate::gitutil::run(worktree.path(), &["init"])?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "user.email", "test@example.com"],
        )?;
        crate::gitutil::run(worktree.path(), &["config", "user.name", "Test User"])?;
        crate::gitutil::run(
            worktree.path(),
            &["commit", "--allow-empty", "-m", "initial"],
        )?;
        Ok(worktree)
    }

    #[test]
    fn bounded_gate_output_reports_retention_and_append_only_spill_metadata() -> Result<()> {
        let (home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let runtime = RuntimeConfig {
            retained_output_bytes: 1024,
            retained_output_lines: 32,
            ..RuntimeConfig::default()
        };
        let output_dir = home.path().join("runtime-output");
        let gate = gate.runtime_output(runtime.clone(), output_dir, 1);
        let mut config = WorkflowConfig {
            runtime,
            ..WorkflowConfig::default()
        };
        config.verify_profiles.insert(
            "bounded".to_string(),
            VerifyProfile {
                commands: vec![VerifyCommand {
                    command: "head -c 2097152 /dev/zero | tr '\\0' x".to_string(),
                    timeout_seconds: 10,
                    cwd: String::new(),
                    env: BTreeMap::new(),
                }],
            },
        );
        let mut selected = slice("slice-bounded");
        selected.verify_profile = "bounded".to_string();

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[selected],
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.status, "passed");
        let command = result.commands.first().expect("command result");
        assert_eq!(command.output_total_bytes, 2 * 1024 * 1024);
        assert!(command.output_retained_bytes <= 1024);
        assert!(command.output_truncated);
        assert_eq!(command.output_spill_paths.len(), 2);
        assert_eq!(
            fs::metadata(&command.output_spill_paths[0])?.len()
                + fs::metadata(&command.output_spill_paths[1])?.len(),
            2 * 1024 * 1024
        );
        Ok(())
    }

    #[test]
    fn integration_gate_success_uses_cwd_env_and_dedupes_commands() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::create_dir(worktree.path().join("sub"))?;
        fs::write(worktree.path().join("sub/marker.txt"), "ok\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixtures")?;

        let mut env = BTreeMap::new();
        env.insert("KHAZAD_PROFILE".to_string(), "quick".to_string());
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "quick".to_string(),
            VerifyProfile {
                commands: vec![VerifyCommand {
                    command: "test \"$KHAZAD_PROFILE\" = quick && test -f marker.txt".to_string(),
                    timeout_seconds: 5,
                    cwd: "sub".to_string(),
                    env,
                }],
            },
        );
        let config = WorkflowConfig {
            verify_profiles: profiles,
            ..WorkflowConfig::default()
        };
        let mut first = slice("slice-001");
        first.verify_profile = "quick".to_string();
        let mut second = slice("slice-002");
        second.verify_profile = "quick".to_string();
        let slices = vec![first, second];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &slices,
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.status, "passed");
        assert_eq!(result.summary, "integration gate passed");
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].status, "passed");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_cwd_symlink_cannot_escape_worktree() -> Result<()> {
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("outside-link"))?;
        let command = VerifyCommand {
            command: "printf escaped > marker".to_string(),
            timeout_seconds: 5,
            cwd: "outside-link".to_string(),
            env: BTreeMap::new(),
        };

        let err = verify_command_cwd(worktree.path(), &command).unwrap_err();

        assert!(err.to_string().contains("pin verify command cwd"));
        assert!(!outside.path().join("marker").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_cwd_descriptor_survives_path_substitution() -> Result<()> {
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        fs::create_dir(worktree.path().join("sub"))?;
        let command = VerifyCommand {
            command: "printf pinned > marker".to_string(),
            timeout_seconds: 5,
            cwd: "sub".to_string(),
            env: BTreeMap::new(),
        };
        let cwd = verify_command_cwd(worktree.path(), &command)?;
        let original = worktree.path().join("original-sub");
        fs::rename(worktree.path().join("sub"), &original)?;
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("sub"))?;

        let output = crate::workflow::shell::ShellCommand::new(&cwd.path, &command.command)
            .pinned_cwd(&cwd.directory)?
            .run(&CancellationToken::new())?;

        assert!(output.success());
        assert_eq!(fs::read(original.join("marker"))?, b"pinned");
        assert!(!outside.path().join("marker").exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verification_whole_root_substitution_cannot_split_snapshot_from_command() -> Result<()> {
        let worktree = clean_git_worktree()?;
        let original_path = worktree.path().to_path_buf();
        fs::write(original_path.join("tracked.txt"), "before\n")?;
        crate::gitutil::commit_all(&original_path, "tracked fixture")?;
        let mut parked = original_path.as_os_str().to_os_string();
        parked.push("-parked");
        let parked = PathBuf::from(parked);
        let command = VerifyCommand {
            command: "printf after > tracked.txt".to_string(),
            timeout_seconds: 5,
            cwd: ".".to_string(),
            env: BTreeMap::new(),
        };
        let cwd = verify_command_cwd(&original_path, &command)?;
        let guard = crate::gitutil::VerificationWorktreeGuard::capture_pinned(
            &original_path,
            &cwd.root_directory,
        )?;
        fs::rename(&original_path, &parked)?;
        fs::create_dir(&original_path)?;

        let output = crate::workflow::shell::ShellCommand::new(&cwd.path, &command.command)
            .pinned_cwd(&cwd.directory)?
            .run(&CancellationToken::new())?;
        let outcome = guard.finish();

        assert!(output.success());
        let crate::gitutil::VerificationGuardOutcome::Mutation(mutation) = outcome else {
            panic!("whole-root substitution was not detected");
        };
        assert!(!mutation.restoration_succeeded);
        assert!(mutation.evidence.restored.is_none());
        assert!(
            mutation
                .evidence
                .restoration_error
                .contains("descriptor-confined tracked files were restored"),
            "{}",
            mutation.evidence.restoration_error
        );
        assert_eq!(fs::read(parked.join("tracked.txt"))?, b"before\n");
        assert!(!original_path.join("tracked.txt").exists());
        fs::remove_dir_all(&parked)?;
        Ok(())
    }

    #[test]
    fn integration_gate_preserves_profile_order() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "full".to_string(),
            VerifyProfile {
                commands: vec![
                    VerifyCommand {
                        command: "printf fmt".to_string(),
                        timeout_seconds: 5,
                        ..VerifyCommand::default()
                    },
                    VerifyCommand {
                        command: "printf test".to_string(),
                        timeout_seconds: 5,
                        ..VerifyCommand::default()
                    },
                    VerifyCommand {
                        command: "printf clippy".to_string(),
                        timeout_seconds: 5,
                        ..VerifyCommand::default()
                    },
                ],
            },
        );
        let config = WorkflowConfig {
            verify_profiles: profiles,
            gate_fail_fast: false,
            ..WorkflowConfig::default()
        };
        let mut first = slice("slice-001");
        first.verify_profile = "full".to_string();
        let slices = vec![first];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &slices,
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result
                .commands
                .iter()
                .map(|command| command.command.as_str())
                .collect::<Vec<_>>(),
            vec!["printf fmt", "printf test", "printf clippy"]
        );
        Ok(())
    }

    #[test]
    fn verify_profile_is_gate_only_not_worker_local() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "full".to_string(),
            VerifyProfile {
                commands: vec![VerifyCommand {
                    command: "printf profile-ran".to_string(),
                    timeout_seconds: 5,
                    ..VerifyCommand::default()
                }],
            },
        );
        let config = WorkflowConfig {
            verify_profiles: profiles,
            gate_fail_fast: false,
            ..WorkflowConfig::default()
        };
        let mut slice = slice("slice-001");
        slice.verify_profile = "full".to_string();
        slice.verify = vec!["printf inline-ran".to_string()];

        let slice_result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &config,
            },
            &CancellationToken::new(),
        )?;
        assert_eq!(slice_result.tests_run, vec!["printf inline-ran"]);

        let gate_result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;
        assert_eq!(
            gate_result
                .commands
                .iter()
                .map(|command| command.command.as_str())
                .collect::<Vec<_>>(),
            vec!["printf profile-ran", "printf inline-ran"]
        );
        Ok(())
    }

    #[test]
    fn integration_gate_fail_fast_skips_remaining_commands() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "full".to_string(),
            VerifyProfile {
                commands: vec![
                    VerifyCommand {
                        command: "printf fmt; false".to_string(),
                        timeout_seconds: 5,
                        ..VerifyCommand::default()
                    },
                    VerifyCommand {
                        command: "printf test".to_string(),
                        timeout_seconds: 5,
                        ..VerifyCommand::default()
                    },
                ],
            },
        );
        let config = WorkflowConfig {
            verify_profiles: profiles,
            ..WorkflowConfig::default()
        };
        let mut first = slice("slice-001");
        first.verify_profile = "full".to_string();
        let slices = vec![first];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &slices,
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.status, "failed");
        assert_eq!(result.commands[0].status, "failed");
        assert_eq!(result.commands[1].status, "skipped");
        assert!(result.commands[1].skip_reason.contains("gate_fail_fast"));
        Ok(())
    }

    #[test]
    fn integration_gate_failure_preserves_result_and_finding_text() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut failing = slice("slice-001");
        failing.verify = vec!["printf 'gate-fail'; false".to_string()];
        let slices = vec![failing];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &slices,
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.status, "failed");
        assert_eq!(result.summary, "one or more integration gate checks failed");
        assert_eq!(result.commands[0].status, "failed");
        assert_eq!(result.commands[0].exit_code, Some(1));
        assert_eq!(result.commands[0].output, "gate-fail");
        assert_eq!(
            result.findings[0].description,
            "integration gate failed: printf 'gate-fail'; false"
        );
        Ok(())
    }

    #[test]
    fn integration_gate_timeout_returns_failed_command_result() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut timed_out = slice("slice-001");
        timed_out.verify_timeout_seconds = 1;
        timed_out.verify = vec!["sleep 30".to_string()];
        let slices = vec![timed_out];

        let started = Instant::now();
        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &slices,
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(result.status, "failed");
        assert_eq!(result.commands[0].status, "failed");
        assert_eq!(result.commands[0].exit_code, None);
        assert!(
            result.commands[0]
                .output
                .contains("command timed out after 1 seconds")
        );
        assert_eq!(result.commands[0].failure_kind, "timeout");
        assert_eq!(
            result.findings[0].description,
            "integration gate command timed out: sleep 30: command timed out after 1 seconds"
        );
        Ok(())
    }

    #[test]
    fn slice_verification_missing_command_is_operator_failure() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["definitely_missing_khazad_tool_127".to_string()];

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        let failure = result.failure.expect("verification should fail");
        assert_eq!(failure.failure_kind, "tool_missing");
        assert_eq!(failure.finding.action, "operator-fix");
        assert!(failure.summary.contains("daemon/operator environment"));
        assert!(
            failure
                .summary
                .contains("definitely_missing_khazad_tool_127")
        );
        assert!(failure.summary.contains("daemon PATH="));
        Ok(())
    }

    #[test]
    fn integration_gate_cancellation_preserves_mutation_evidence() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify_timeout_seconds = 30;
        slice.verify = vec!["printf changed > tracked.txt; sleep 30".to_string()];
        let slices = vec![slice];
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let mutation_path = worktree.path().join("tracked.txt");
        let cancel_thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if fs::read(&mutation_path).is_ok_and(|contents| contents == b"changed") {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            thread_cancel.cancel();
        });

        let started = Instant::now();
        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &slices,
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &cancel,
        )?;

        cancel_thread.join().expect("cancellation observer");
        assert!(result.verification_cancelled);
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        let workspace = result.commands[0]
            .verification_workspace
            .as_ref()
            .expect("cancelled mutation evidence");
        assert_eq!(
            workspace.before.as_ref().unwrap().digest,
            workspace.restored.as_ref().unwrap().digest
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn integration_gate_cancellation_without_mutation_finalizes_outer_guard() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join(".gitignore"), "control/\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify_timeout_seconds = 30;
        slice.verify = vec!["mkdir -p control; touch control/started; sleep 30".to_string()];
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let marker = worktree.path().join("control/started");
        let cancel_thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            thread_cancel.cancel();
        });

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &cancel,
        )?;

        cancel_thread.join().expect("cancellation observer");
        assert!(result.verification_cancelled);
        assert_eq!(result.status, "failed");
        assert_eq!(result.commands[0].failure_kind, "cancelled");
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outer_gate_restoration_failure_retains_evidence_and_outranks_cancellation() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        let control = tempfile::tempdir()?;
        let tracked_parent = worktree.path().join("tracked-dir");
        fs::create_dir(&tracked_parent)?;
        fs::write(tracked_parent.join("file.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        fs::write(outside.path().join("file.txt"), "outside\n")?;
        let marker = control.path().join("paused");
        let release = control.path().join("release");
        pause_next_integration_gate_before_outer_guard(worktree.path(), &marker, &release);
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let mutation_path = tracked_parent.join("file.txt");
        let tracked_parent_for_thread = tracked_parent.clone();
        let outside_path = outside.path().to_path_buf();
        let release_for_thread = release.clone();
        let mut parked_name = worktree.path().as_os_str().to_os_string();
        parked_name.push("-outer-gate-parent-parked");
        let parked = PathBuf::from(parked_name);
        let parked_for_thread = parked.clone();
        let mutator = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            fs::write(&mutation_path, "after\n").unwrap();
            crate::gitutil::substitute_next_verification_parent_during_restore(
                b"tracked-dir/file.txt",
                &tracked_parent_for_thread,
                &parked_for_thread,
                &outside_path,
            );
            thread_cancel.cancel();
            fs::write(release_for_thread, "release\n").unwrap();
        });
        let mut slice = slice("slice-001");
        slice.verify = vec!["true".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &cancel,
        )?;

        mutator.join().expect("outer gate mutator");
        assert!(result.verification_cancelled);
        assert_eq!(result.failure_kind, "verification_restoration_failed");
        let evidence = result
            .verification_workspace
            .as_ref()
            .expect("outer guard workspace evidence");
        assert!(!evidence.restoration_error.is_empty());
        assert_eq!(fs::read(parked.join("file.txt"))?, b"before\n");
        assert_eq!(fs::read(outside.path().join("file.txt"))?, b"outside\n");
        if fs::symlink_metadata(&tracked_parent).is_ok() {
            fs::remove_file(&tracked_parent)?;
        }
        fs::rename(&parked, &tracked_parent)?;
        Ok(())
    }

    #[test]
    fn verification_mutation_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "original\n")?;
        crate::gitutil::commit_all(worktree.path(), "initial")?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf 'mutated\\n' > tracked.txt".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.status, "failed");
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert_eq!(result.findings[0].action, "operator-fix");
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"original\n"
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_nonignored_empty_directory_is_failed_and_removed() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "original\n")?;
        crate::gitutil::commit_all(worktree.path(), "initial")?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["mkdir verifier-empty-side-effect".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.status, "failed");
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert!(!worktree.path().join("verifier-empty-side-effect").exists());
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_mutation_records_nul_safe_before_after_evidence() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("rename old"), "rename\n")?;
        fs::write(worktree.path().join("delete me"), "delete\n")?;
        fs::write(worktree.path().join("unstaged file"), "original\n")?;
        fs::write(worktree.path().join(".gitignore"), "ignored-cache/\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixtures")?;
        let mut slice = slice("slice-001");
        slice.verify = vec![
            concat!(
                "git mv 'rename old' \"$(printf 'rename\\nnew')\" && ",
                "rm 'delete me' && ",
                "printf 'changed\\n' >> 'unstaged file' && ",
                "printf 'untracked\\n' > \"$(printf 'untracked\\nfile')\" && ",
                "mkdir -p ignored-cache && printf 'cache\\n' > ignored-cache/value"
            )
            .to_string(),
        ];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        let command = serde_json::to_value(&result.commands[0])?;
        let evidence = &command["verification_workspace"];
        assert!(evidence["before"]["staged"].as_array().unwrap().is_empty());
        assert_eq!(evidence["after"]["staged"][0]["status"], "R100");
        assert_eq!(
            evidence["after"]["staged"][0]["path_bytes_hex"],
            serde_json::json!([hex::encode("rename old"), hex::encode("rename\nnew")])
        );
        assert!(
            evidence["after"]["unstaged"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["status"] == "D")
        );
        assert_eq!(
            evidence["after"]["untracked_path_bytes_hex"],
            serde_json::json!([hex::encode("untracked\nfile")])
        );
        assert_eq!(
            evidence["restored"]["digest"], evidence["before"]["digest"],
            "{:#?}",
            result.commands[0]
        );
        assert_eq!(fs::read(worktree.path().join("rename old"))?, b"rename\n");
        assert!(!worktree.path().join("rename\nnew").exists());
        assert_eq!(fs::read(worktree.path().join("delete me"))?, b"delete\n");
        assert!(!worktree.path().join("untracked\nfile").exists());
        assert_eq!(
            fs::read(worktree.path().join("ignored-cache/value"))?,
            b"cache\n"
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_symbolic_head_mutation_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let original_ref = crate::gitutil::run(worktree.path(), &["symbolic-ref", "HEAD"])?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["git switch --detach --quiet HEAD".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        let workspace = result.commands[0]
            .verification_workspace
            .as_ref()
            .expect("HEAD mutation evidence");
        assert_eq!(
            workspace.before.as_ref().unwrap().head_attachment,
            original_ref
        );
        assert_eq!(workspace.after.as_ref().unwrap().head_attachment, "HEAD");
        assert_eq!(
            workspace.restored.as_ref().unwrap().head_attachment,
            original_ref
        );
        assert_eq!(
            crate::gitutil::run(worktree.path(), &["symbolic-ref", "HEAD"])?,
            original_ref
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_branch_switch_restores_original_without_rewinding_new_branch() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let original_ref = crate::gitutil::run(worktree.path(), &["symbolic-ref", "HEAD"])?;
        let original_head = crate::gitutil::head_sha(worktree.path())?;
        let mut slice = slice("slice-001");
        slice.verify = vec![
            concat!(
                "git switch --quiet -c verification-side-effect; ",
                "printf changed > tracked.txt; git add tracked.txt; ",
                "git commit --quiet -m verification-side-effect"
            )
            .to_string(),
        ];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert_eq!(
            crate::gitutil::run(worktree.path(), &["symbolic-ref", "HEAD"])?,
            original_ref
        );
        assert_eq!(crate::gitutil::head_sha(worktree.path())?, original_head);
        assert_ne!(
            crate::gitutil::run(
                worktree.path(),
                &["rev-parse", "refs/heads/verification-side-effect"]
            )?,
            original_head
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_snapshot_capture_does_not_refresh_the_real_index() -> Result<()> {
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let index_path = crate::gitutil::run(
            worktree.path(),
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let before = fs::read(index_path.trim())?;

        let guard = crate::gitutil::VerificationWorktreeGuard::capture(worktree.path())?;

        assert_eq!(fs::read(index_path.trim())?, before);
        assert!(matches!(
            guard.finish(),
            crate::gitutil::VerificationGuardOutcome::Unchanged
        ));
        assert_eq!(fs::read(index_path.trim())?, before);
        Ok(())
    }

    #[test]
    fn verification_index_stat_cache_mutation_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        thread::sleep(Duration::from_millis(1_100));
        fs::write(worktree.path().join("tracked.txt"), "before\n")?;
        let index_path = crate::gitutil::run(
            worktree.path(),
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let before = fs::read(index_path.trim())?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["git update-index --refresh".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{:#?}",
            result.commands[0]
        );
        assert_eq!(fs::read(index_path.trim())?, before);
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_raw_index_format_mutation_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let index_path = crate::gitutil::run(
            worktree.path(),
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let before = fs::read(index_path.trim())?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["git update-index --index-version=4".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{:#?}",
            result.commands[0]
        );
        assert_eq!(fs::read(index_path.trim())?, before);
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_malformed_index_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let index_path = crate::gitutil::run(
            worktree.path(),
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )?;
        let before = fs::read(index_path.trim())?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf malformed > \"$(git rev-parse --git-path index)\"".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{:#?}",
            result.commands[0]
        );
        assert_eq!(fs::read(index_path.trim())?, before);
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_index_flag_mutation_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec![
            "git update-index --skip-worktree tracked.txt; printf changed > tracked.txt"
                .to_string(),
        ];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{:#?}",
            result.commands[0]
        );
        let workspace = result.commands[0]
            .verification_workspace
            .as_ref()
            .expect("index mutation evidence");
        assert_ne!(
            workspace.before.as_ref().unwrap().index_digest,
            workspace.after.as_ref().unwrap().index_digest
        );
        assert_eq!(
            workspace.before.as_ref().unwrap().index_digest,
            workspace.restored.as_ref().unwrap().index_digest
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        assert!(
            crate::gitutil::run(worktree.path(), &["ls-files", "-v", "tracked.txt"])?
                .starts_with("H ")
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_guard_blocks_dirty_prestate_without_running_command() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        fs::write(worktree.path().join("tracked.txt"), "operator edit\n")?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf ran > command-ran.txt".to_string()];
        let config = WorkflowConfig::default();

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_workspace_dirty"
        );
        assert_eq!(result.commands[0].duration_ms, 0);
        assert!(!worktree.path().join("command-ran.txt").exists());
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"operator edit\n"
        );
        assert_eq!(
            result.commands[0]
                .verification_workspace
                .as_ref()
                .unwrap()
                .before
                .as_ref()
                .unwrap()
                .unstaged[0]
                .status,
            "M"
        );
        Ok(())
    }

    #[test]
    fn verification_filter_equivalent_raw_bytes_are_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "filter.normalize.clean", "sed 's/|raw-[^|]*$//'"],
        )?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "filter.normalize.smudge", "cat"],
        )?;
        fs::write(
            worktree.path().join(".gitattributes"),
            "tracked.txt filter=normalize\n",
        )?;
        fs::write(
            worktree.path().join("tracked.txt"),
            "canonical|raw-before\n",
        )?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf 'canonical|raw-after\\n' > tracked.txt".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{}",
            result.commands[0].output
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"canonical|raw-before\n"
        );
        let status = crate::gitutil::status_porcelain(worktree.path())?;
        let diff = crate::gitutil::run(worktree.path(), &["diff", "--", "tracked.txt"])?;
        assert!(
            status.is_empty(),
            "unexpected restored status: {status:?}; diff={diff:?}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_snapshot_never_executes_configured_content_filters() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        let marker = outside.path().join("filter-ran");
        let filter = outside.path().join("clean-filter");
        fs::write(
            &filter,
            format!(
                "#!/bin/sh\nprintf ran >> '{}'\nsed 's/|raw-[^|]*$//'\n",
                marker.display()
            ),
        )?;
        fs::set_permissions(&filter, fs::Permissions::from_mode(0o755))?;
        crate::gitutil::run(
            worktree.path(),
            &[
                "config",
                "filter.observable.clean",
                filter.to_str().expect("temporary filter path is UTF-8"),
            ],
        )?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "filter.observable.smudge", "cat"],
        )?;
        fs::write(
            worktree.path().join(".gitattributes"),
            "tracked.txt filter=observable\n",
        )?;
        fs::write(
            worktree.path().join("tracked.txt"),
            "canonical|raw-before\n",
        )?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        if marker.exists() {
            fs::remove_file(&marker)?;
        }
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf 'canonical|raw-after\\n' > tracked.txt".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{}",
            result.commands[0].output
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"canonical|raw-before\n"
        );
        assert!(
            !marker.exists(),
            "verification snapshot capture executed configured filter {}",
            filter.display()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_cache_revalidation_never_executes_configured_content_filters() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        let marker = outside.path().join("filter-ran");
        let filter = outside.path().join("clean-filter");
        fs::write(
            &filter,
            format!("#!/bin/sh\nprintf ran >> '{}'\ncat\n", marker.display()),
        )?;
        fs::set_permissions(&filter, fs::Permissions::from_mode(0o755))?;
        crate::gitutil::run(
            worktree.path(),
            &[
                "config",
                "filter.observable.clean",
                filter.to_str().expect("temporary filter path is UTF-8"),
            ],
        )?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "filter.observable.smudge", "cat"],
        )?;
        fs::write(
            worktree.path().join(".gitattributes"),
            "tracked.txt filter=observable\n",
        )?;
        fs::write(worktree.path().join("tracked.txt"), "tracked\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        crate::gitutil::run(worktree.path(), &["update-index", "--index-version=4"])?;
        if marker.exists() {
            fs::remove_file(&marker)?;
        }
        let mut slice = slice("slice-001");
        slice.verify = vec!["true".to_string()];
        let config = WorkflowConfig::default();

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: std::slice::from_ref(&slice),
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: std::slice::from_ref(&slice),
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(first.status, "passed");
        assert_eq!(second.status, "passed");
        assert!(!first.commands[0].cache_hit);
        assert!(second.commands[0].cache_hit);
        assert!(
            !marker.exists(),
            "snapshot/cache revalidation executed configured filter {}",
            filter.display()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_rejects_initialized_submodule_before_execution() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let submodule = clean_git_worktree()?;
        fs::write(submodule.path().join("tracked.txt"), "submodule baseline\n")?;
        crate::gitutil::commit_all(submodule.path(), "submodule fixture")?;
        crate::gitutil::run(
            worktree.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                submodule.path().to_str().expect("temporary path is UTF-8"),
                "sub",
            ],
        )?;
        crate::gitutil::commit_all(worktree.path(), "parent fixture")?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "diff.ignoreSubmodules", "dirty"],
        )?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf mutated > sub/tracked.txt".to_string()];
        let config = WorkflowConfig::default();

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_snapshot_failed",
            "{:#?}",
            result.commands[0]
        );
        assert!(
            result.commands[0]
                .output
                .contains("initialized Git submodule")
        );
        assert_eq!(
            fs::read(worktree.path().join("sub/tracked.txt"))?,
            b"submodule baseline\n"
        );
        Ok(())
    }

    #[test]
    fn verification_restores_repo_local_config_and_never_caches_the_side_effect() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["git config ca01.verifier-side-effect mutated".to_string()];
        let config = WorkflowConfig::default();

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: std::slice::from_ref(&slice),
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: std::slice::from_ref(&slice),
                integration_worktree: worktree.path(),
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        for result in [&first, &second] {
            assert_eq!(
                result.commands[0].failure_kind, "verification_mutated_worktree",
                "{:#?}",
                result.commands[0]
            );
            assert!(!result.commands[0].cache_hit);
        }
        assert!(
            crate::gitutil::run(
                worktree.path(),
                &["config", "--local", "--get", "ca01.verifier-side-effect"]
            )
            .is_err(),
            "verifier-created repository config survived restoration"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_rejects_hardlinked_tracked_prestate_before_execution() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "tracked\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let original = fs::read(worktree.path().join("tracked.txt"))?;
        let external_dir = tempfile::tempdir()?;
        let external = external_dir.path().join("external-hardlink-source");
        fs::write(&external, &original)?;
        fs::remove_file(worktree.path().join("tracked.txt"))?;
        fs::hard_link(&external, worktree.path().join("tracked.txt"))?;
        let marker = worktree.path().join("should-not-run");
        let mut slice = slice("slice-001");
        slice.verify = vec![format!("touch {}", marker.display())];
        let config = WorkflowConfig::default();

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_snapshot_failed"
        );
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_parent_symlink_escape_is_failed_and_restored_in_bounds() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        fs::create_dir_all(worktree.path().join("tracked-dir"))?;
        fs::write(worktree.path().join("tracked-dir/file.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec![format!(
            "rm -rf tracked-dir && ln -s '{}' tracked-dir",
            outside.path().display()
        )];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{}",
            result.commands[0].output
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked-dir/file.txt"))?,
            b"before\n"
        );
        assert!(outside.path().read_dir()?.next().is_none());
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verification_capture_parent_substitution_never_reads_outside_pinned_directory() -> Result<()>
    {
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        let tracked_parent = worktree.path().join("tracked-dir");
        fs::create_dir(&tracked_parent)?;
        fs::write(tracked_parent.join("file.txt"), "inside\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        fs::write(outside.path().join("file.txt"), "outside\n")?;
        let guard = crate::gitutil::VerificationWorktreeGuard::capture(worktree.path())?;
        let mut parked_name = worktree.path().as_os_str().to_os_string();
        parked_name.push("-capture-parent-parked");
        let parked = PathBuf::from(parked_name);
        crate::gitutil::substitute_next_verification_parent_during_capture(
            b"tracked-dir/file.txt",
            &tracked_parent,
            &parked,
            outside.path(),
        );

        let evidence = guard
            .precommand_change_evidence()
            .expect("parent substitution must block execution");

        assert!(!evidence.after_capture_error.is_empty());
        assert_eq!(fs::read(parked.join("file.txt"))?, b"inside\n");
        assert_eq!(fs::read(outside.path().join("file.txt"))?, b"outside\n");
        fs::remove_file(&tracked_parent)?;
        fs::rename(&parked, &tracked_parent)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verification_cache_parent_substitution_stays_on_opened_directory() -> Result<()> {
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        let tracked_parent = worktree.path().join("tracked-dir");
        fs::create_dir(&tracked_parent)?;
        fs::write(tracked_parent.join("file.txt"), "inside\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        fs::write(outside.path().join("file.txt"), "outside\n")?;
        let before = crate::gitutil::cache_worktree_digest(worktree.path())?;
        let mut parked_name = worktree.path().as_os_str().to_os_string();
        parked_name.push("-cache-parent-parked");
        let parked = PathBuf::from(parked_name);
        crate::gitutil::substitute_next_verification_parent_during_cache_digest(
            Path::new("tracked-dir"),
            &tracked_parent,
            &parked,
            outside.path(),
        );

        let during_substitution = crate::gitutil::cache_worktree_digest(worktree.path())?;

        assert_eq!(during_substitution, before);
        assert_eq!(fs::read(parked.join("file.txt"))?, b"inside\n");
        assert_eq!(fs::read(outside.path().join("file.txt"))?, b"outside\n");
        fs::remove_file(&tracked_parent)?;
        fs::rename(&parked, &tracked_parent)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verification_restore_parent_substitution_never_writes_outside_pinned_directory() -> Result<()>
    {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let outside = tempfile::tempdir()?;
        let tracked_parent = worktree.path().join("tracked-dir");
        fs::create_dir(&tracked_parent)?;
        fs::write(tracked_parent.join("file.txt"), "before\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        fs::write(outside.path().join("file.txt"), "outside\n")?;
        let mut parked_name = worktree.path().as_os_str().to_os_string();
        parked_name.push("-tracked-parent-parked");
        let parked = PathBuf::from(parked_name);
        crate::gitutil::substitute_next_verification_parent_during_restore(
            b"tracked-dir/file.txt",
            &tracked_parent,
            &parked,
            outside.path(),
        );
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf after > tracked-dir/file.txt".to_string()];
        let config = WorkflowConfig::default();

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_restoration_failed",
            "{}",
            result.commands[0].output
        );
        assert_eq!(fs::read(parked.join("file.txt"))?, b"before\n");
        assert_eq!(fs::read(outside.path().join("file.txt"))?, b"outside\n");
        if fs::symlink_metadata(&tracked_parent).is_ok() {
            fs::remove_file(&tracked_parent)?;
        }
        fs::rename(&parked, &tracked_parent)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_tracked_metadata_mutation_is_failed_and_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let tracked = worktree.path().join("tracked.txt");
        fs::write(&tracked, "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        fs::set_permissions(&tracked, fs::Permissions::from_mode(0o664))?;
        let before = fs::metadata(&tracked)?;
        let mut slice = slice("slice-001");
        slice.verify =
            vec!["chmod 600 tracked.txt; touch -d '2001-02-03 04:05:06' tracked.txt".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        let restored = fs::metadata(&tracked)?;
        assert_eq!(restored.mode(), before.mode());
        assert_eq!(restored.mtime(), before.mtime());
        assert_eq!(restored.mtime_nsec(), before.mtime_nsec());
        assert_eq!(fs::read(&tracked)?, b"baseline\n");
        Ok(())
    }

    #[test]
    fn verification_rejects_hidden_operator_bytes_before_running_command() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        crate::gitutil::run(
            worktree.path(),
            &["update-index", "--assume-unchanged", "tracked.txt"],
        )?;
        fs::write(
            worktree.path().join("tracked.txt"),
            "hidden operator bytes\n",
        )?;
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        let mut slice = slice("slice-001");
        slice.verify =
            vec!["printf ran > command-ran.txt; printf verifier > tracked.txt".to_string()];
        let config = WorkflowConfig::default();

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &config,
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_workspace_dirty"
        );
        assert_eq!(result.commands[0].duration_ms, 0);
        assert!(!worktree.path().join("command-ran.txt").exists());
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"hidden operator bytes\n"
        );
        assert!(
            crate::gitutil::run(worktree.path(), &["ls-files", "-v", "tracked.txt"])?
                .starts_with("h ")
        );
        Ok(())
    }

    #[test]
    fn verification_restoration_removes_path_hidden_only_by_mutated_ignore_rules() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join(".gitignore"), "# baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify =
            vec!["printf 'residue\\n' > .gitignore; printf hidden > residue".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{}",
            result.commands[0].output
        );
        assert_eq!(
            fs::read(worktree.path().join(".gitignore"))?,
            b"# baseline\n"
        );
        assert!(!worktree.path().join("residue").exists());
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn verification_restoration_repeats_cleanup_after_untracked_ignore_rule_removal() -> Result<()>
    {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify =
            vec!["printf 'residue\\n' > .gitignore; printf hidden > residue".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind, "verification_mutated_worktree",
            "{}",
            result.commands[0].output
        );
        assert!(!worktree.path().join(".gitignore").exists());
        assert!(!worktree.path().join("residue").exists());
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn failing_verification_mutation_is_restored_and_overrides_command_failure() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf changed > tracked.txt; printf failed; false".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.commands[0].exit_code, Some(1));
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert!(result.commands[0].output.contains("failed"));
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn timed_out_verification_mutation_is_restored() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify_timeout_seconds = 1;
        slice.verify = vec!["printf changed > tracked.txt; sleep 30".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.commands[0].exit_code, None);
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert!(result.commands[0].output.contains("timed out"));
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn verification_evidence_preserves_non_utf8_path_bytes() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify =
            vec!["name=$(printf 'invalid\\377name'); printf value > \"$name\"".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        let expected = hex::encode(b"invalid\xffname");
        let workspace = result.commands[0]
            .verification_workspace
            .as_ref()
            .expect("mutation evidence");
        assert!(
            workspace
                .after
                .as_ref()
                .unwrap()
                .untracked_path_bytes_hex
                .contains(&expected)
        );
        assert_eq!(
            workspace.restored.as_ref().unwrap().digest,
            workspace.before.as_ref().unwrap().digest
        );
        assert!(crate::gitutil::status_porcelain(worktree.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn cancelled_verification_restoration_failure_remains_operator_failure() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["rm -rf .git; sleep 30".to_string()];
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let git_dir = worktree.path().join(".git");
        let cancel_thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while git_dir.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            thread_cancel.cancel();
        });

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &cancel,
        )?;

        cancel_thread.join().expect("cancellation observer");
        assert!(result.verification_cancelled);
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_restoration_failed"
        );
        assert_eq!(result.findings[0].action, "operator-fix");
        let workspace = result.commands[0]
            .verification_workspace
            .as_ref()
            .expect("restoration failure evidence");
        assert!(!workspace.after_capture_error.is_empty());
        assert!(!workspace.restoration_error.is_empty());
        Ok(())
    }

    #[test]
    fn verification_restoration_failure_is_distinct_operator_failure() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["rm -rf .git".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_restoration_failed"
        );
        assert_eq!(result.findings[0].action, "operator-fix");
        let workspace = result.commands[0]
            .verification_workspace
            .as_ref()
            .expect("restoration failure evidence");
        assert!(!workspace.after_capture_error.is_empty());
        assert!(!workspace.restoration_error.is_empty());
        Ok(())
    }

    #[test]
    fn verification_cache_does_not_reuse_commands_that_change_ignored_state() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join(".gitignore"), "cache/\n")?;
        fs::write(worktree.path().join("tracked.txt"), "one\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec![
            concat!(
                "mkdir -p cache; ",
                "count=$(cat cache/count 2>/dev/null || printf 0); ",
                "printf '%s' $((count + 1)) > cache/count"
            )
            .to_string(),
        ];

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        assert!(!first.commands[0].cache_hit);
        assert!(!second.commands[0].cache_hit);
        assert_eq!(
            fs::read_to_string(worktree.path().join("cache/count"))?,
            "2"
        );

        fs::write(worktree.path().join("tracked.txt"), "two\n")?;
        crate::gitutil::commit_all(worktree.path(), "change head")?;
        let changed_head = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        assert!(!changed_head.commands[0].cache_hit);
        assert_eq!(
            fs::read_to_string(worktree.path().join("cache/count"))?,
            "3"
        );

        let clone_parent = tempfile::tempdir()?;
        let clone_path = clone_parent.path().join("clone");
        crate::gitutil::run(
            clone_parent.path(),
            &[
                "clone",
                worktree.path().to_str().unwrap(),
                clone_path.to_str().unwrap(),
            ],
        )?;
        let other_worktree = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: &clone_path,
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        assert!(!other_worktree.commands[0].cache_hit);
        assert_eq!(fs::read_to_string(clone_path.join("cache/count"))?, "1");
        Ok(())
    }

    #[test]
    fn verification_cache_does_not_reuse_a_stale_pass_after_ignored_state_changes() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join(".gitignore"), "cache/\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify =
            vec!["test ! -e cache/sentinel && mkdir -p cache && touch cache/sentinel".to_string()];

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(first.commands[0].status, "passed");
        assert!(!first.commands[0].cache_hit);
        assert_eq!(second.commands[0].status, "failed");
        assert!(!second.commands[0].cache_hit);
        Ok(())
    }

    #[test]
    fn verification_cache_does_not_reuse_a_failed_command() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join(".gitignore"), "cache/\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec![
            concat!(
                "mkdir -p cache; ",
                "count=$(cat cache/count 2>/dev/null || printf 0); ",
                "printf '%s' $((count + 1)) > cache/count; ",
                "exit 1"
            )
            .to_string(),
        ];

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(first.commands[0].failure_kind, "command_failed");
        assert_eq!(second.commands[0].failure_kind, "command_failed");
        assert!(!first.commands[0].cache_hit);
        assert!(!second.commands[0].cache_hit);
        assert_eq!(
            fs::read_to_string(worktree.path().join("cache/count"))?,
            "2"
        );
        Ok(())
    }

    #[test]
    fn verification_cache_never_reuses_a_mutating_command_result() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        fs::write(worktree.path().join(".gitignore"), "cache/\n")?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec![
            concat!(
                "mkdir -p cache; ",
                "count=$(cat cache/count 2>/dev/null || printf 0); ",
                "printf '%s' $((count + 1)) > cache/count; ",
                "printf changed > tracked.txt"
            )
            .to_string(),
        ];

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            first.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert_eq!(
            second.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert!(!first.commands[0].cache_hit);
        assert!(!second.commands[0].cache_hit);
        assert_eq!(
            fs::read_to_string(worktree.path().join("cache/count"))?,
            "2"
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        Ok(())
    }

    #[test]
    fn verification_command_identity_has_no_environment_delimiter_collision() {
        let mut combined = BTreeMap::new();
        combined.insert("A".to_string(), "x;B=y".to_string());
        let mut split = BTreeMap::new();
        split.insert("A".to_string(), "x".to_string());
        split.insert("B".to_string(), "y".to_string());
        let command = |env| VerifyCommand {
            command: "verify".to_string(),
            timeout_seconds: 5,
            cwd: ".".to_string(),
            env,
        };

        assert_ne!(
            verify_command_key(&command(combined)),
            verify_command_key(&command(split))
        );
    }

    #[test]
    fn verification_cache_identity_includes_declared_environment() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify_profile = "cache".to_string();
        let config = |value: &str| {
            let mut env = BTreeMap::new();
            env.insert("CACHE_VALUE".to_string(), value.to_string());
            let mut profiles = BTreeMap::new();
            profiles.insert(
                "cache".to_string(),
                VerifyProfile {
                    commands: vec![VerifyCommand {
                        command: "test \"$CACHE_VALUE\" = B".to_string(),
                        timeout_seconds: 5,
                        cwd: String::new(),
                        env,
                    }],
                },
            );
            WorkflowConfig {
                verify_profiles: profiles,
                ..WorkflowConfig::default()
            }
        };

        let first = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &config("A"),
            },
            &CancellationToken::new(),
        )?;
        let second = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice.clone()],
                integration_worktree: worktree.path(),
                config: &config("B"),
            },
            &CancellationToken::new(),
        )?;
        let third = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &config("B"),
            },
            &CancellationToken::new(),
        )?;
        assert_eq!(first.commands[0].status, "failed");
        assert!(!first.commands[0].cache_hit);
        assert_eq!(second.commands[0].status, "passed");
        assert!(!second.commands[0].cache_hit);
        assert!(third.commands[0].cache_hit);
        Ok(())
    }

    #[test]
    fn ca01_red_mutating_success_must_fail_and_restore() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
        crate::gitutil::run(worktree.path(), &["init", "-b", "main"])?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "user.email", "test@example.com"],
        )?;
        crate::gitutil::run(worktree.path(), &["config", "user.name", "Test User"])?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf changed > tracked.txt".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(result.commands[0].status, "failed");
        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        Ok(())
    }

    #[test]
    fn ca01_red_mutating_failure_must_report_mutation_and_restore() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
        crate::gitutil::run(worktree.path(), &["init", "-b", "main"])?;
        crate::gitutil::run(
            worktree.path(),
            &["config", "user.email", "test@example.com"],
        )?;
        crate::gitutil::run(worktree.path(), &["config", "user.name", "Test User"])?;
        fs::write(worktree.path().join("tracked.txt"), "baseline\n")?;
        crate::gitutil::commit_all(worktree.path(), "fixture")?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf changed > tracked.txt; false".to_string()];

        let result = gate.run_integration_gate(
            IntegrationGateRequest {
                slices: &[slice],
                integration_worktree: worktree.path(),
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        assert_eq!(
            result.commands[0].failure_kind,
            "verification_mutated_worktree"
        );
        assert_eq!(
            fs::read(worktree.path().join("tracked.txt"))?,
            b"baseline\n"
        );
        Ok(())
    }

    #[test]
    fn slice_verification_failure_preserves_check_summary_text() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = clean_git_worktree()?;
        let mut slice = slice("slice-001");
        slice.verify = vec!["printf 'verify-fail'; false".to_string()];

        let result = gate.verify_slice_commands(
            SliceVerificationRequest {
                slice: &slice,
                worker_worktree: worktree.path(),
                attempt: 1,
                config: &WorkflowConfig::default(),
            },
            &CancellationToken::new(),
        )?;

        let failure = result.failure.expect("verification should fail");
        assert_eq!(result.tests_run, vec!["printf 'verify-fail'; false"]);
        assert_eq!(
            failure.summary,
            "verify command failed: printf 'verify-fail'; false\nverify-fail"
        );
        assert_eq!(failure.failure_kind, "command_failed");
        assert_eq!(failure.finding.description, failure.summary);
        Ok(())
    }
}
