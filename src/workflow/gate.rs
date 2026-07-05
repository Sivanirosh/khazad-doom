use super::economics::RunEconomicsRecorder;
use super::shell::{ShellCommand, ShellCommandError, ShellProgress};
use super::{CancelledError, check_cancelled};
use crate::agent::CancellationToken;
use crate::domain::{
    CommandExecutionEconomics, Finding, GateCommandResult, GateResult, Slice, VerifyCommand,
    WorkflowConfig,
};
use crate::gitutil;
use crate::state::{ProgressReporter, ProgressScope};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_VERIFY_TIMEOUT_SECONDS: u64 = 600;

pub(crate) struct WorkflowGate {
    progress: ProgressReporter,
    economics: Option<RunEconomicsRecorder>,
    cache: VerificationCommandCache,
}

impl WorkflowGate {
    #[cfg(test)]
    pub(crate) fn new(progress: ProgressReporter) -> Self {
        Self {
            progress,
            economics: None,
            cache: VerificationCommandCache::default(),
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
        }
    }

    pub(crate) fn verify_slice_commands(
        &self,
        request: SliceVerificationRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<SliceVerificationResult> {
        let mut result = SliceVerificationResult::default();
        for command in effective_verify_commands(request.slice, request.config)? {
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
                },
                cancel,
            )?;
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
        let commands = integration_gate_commands(request.slices, request.config)?;
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
        let mut failed = false;
        for command in commands {
            check_cancelled(cancel)?;
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
                },
                cancel,
            )?;
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
        }
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
                commands: Vec::new(),
                findings: Vec::new(),
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
                },
                cancel,
            )?;
            if outcome.result.status == "failed" {
                findings.push(worktree_setup_failure_finding(
                    &command.command,
                    &outcome.result,
                ));
                results.push(outcome.result);
                return Ok(GateResult {
                    status: "failed".to_string(),
                    summary: "worktree setup command failed".to_string(),
                    commands: results,
                    findings,
                });
            }
            results.push(outcome.result);
        }

        let status = gitutil::status_porcelain(request.worktree)?;
        if !status.trim().is_empty() {
            return Ok(GateResult {
                status: "failed".to_string(),
                summary: "worktree setup left non-ignored changes".to_string(),
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
            });
        }

        Ok(GateResult {
            status: "passed".to_string(),
            summary: "worktree setup passed".to_string(),
            commands: results,
            findings: Vec::new(),
        })
    }

    fn run_verify_command(
        &self,
        request: VerifyCommandExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<VerifyCommandExecutionOutcome> {
        let cwd_label = verify_command_cwd_label(request.command);
        let dedupe_key = verify_command_key(request.command);
        let tree_sha = command_tree_sha(request.worktree_root);
        let cache_key = command_cache_key(request.worktree_root, &tree_sha, &dedupe_key);
        let cwd = match verify_command_cwd(request.worktree_root, request.command) {
            Ok(cwd) => cwd,
            Err(err) => {
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
                };
                self.record_command_economics(CommandExecutionEconomics {
                    phase: request.phase.to_string(),
                    slice_id: request.slice_id.to_string(),
                    attempt: request.attempt,
                    command: request.command.command.clone(),
                    cwd: cwd_label,
                    status: result.status.clone(),
                    exit_code: result.exit_code,
                    duration_ms: 0,
                    dedupe_key,
                    tree_sha,
                    cache_key,
                    cache_hit: false,
                    skip_reason: String::new(),
                });
                return Ok(VerifyCommandExecutionOutcome { result });
            }
        };
        if request.cacheable
            && let Some(mut cached) = self.cache.get(&cache_key)
        {
            cached.cache_hit = true;
            cached.duration_ms = 0;
            self.record_command_economics(CommandExecutionEconomics {
                phase: request.phase.to_string(),
                slice_id: request.slice_id.to_string(),
                attempt: request.attempt,
                command: request.command.command.clone(),
                cwd: cwd_label,
                status: cached.status.clone(),
                exit_code: cached.exit_code,
                duration_ms: 0,
                dedupe_key,
                tree_sha,
                cache_key,
                cache_hit: true,
                skip_reason: String::new(),
            });
            return Ok(VerifyCommandExecutionOutcome { result: cached });
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
        let started_at = Instant::now();
        let output = ShellCommand::new(&cwd, &request.command.command)
            .timeout(request.timeout)
            .envs(&request.command.env)
            .progress(Some(progress))
            .run(cancel);
        let duration_ms = started_at.elapsed().as_millis();
        let result = match output {
            Ok(output) => {
                let failure_kind = command_failure_kind(output.exit_code(), output.success());
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
                }
            }
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(CancelledError::new("run cancelled").into());
                }
                let failure_kind = shell_error_failure_kind(err.as_ref()).to_string();
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
                }
            }
        };
        self.record_command_economics(CommandExecutionEconomics {
            phase: request.phase.to_string(),
            slice_id: request.slice_id.to_string(),
            attempt: request.attempt,
            command: request.command.command.clone(),
            cwd: cwd_label,
            status: result.status.clone(),
            exit_code: result.exit_code,
            duration_ms,
            dedupe_key,
            tree_sha,
            cache_key: cache_key.clone(),
            cache_hit: false,
            skip_reason: String::new(),
        });
        if request.cacheable {
            self.cache.insert(cache_key, result.clone());
        }
        Ok(VerifyCommandExecutionOutcome { result })
    }

    fn skipped_command_result(
        &self,
        worktree_root: &Path,
        command: &VerifyCommand,
        reason: &str,
    ) -> Result<GateCommandResult> {
        let _cwd = verify_command_cwd(worktree_root, command)?;
        let cwd_label = verify_command_cwd_label(command);
        let dedupe_key = verify_command_key(command);
        let tree_sha = command_tree_sha(worktree_root);
        let cache_key = command_cache_key(worktree_root, &tree_sha, &dedupe_key);
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
        })
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
}

#[derive(Debug)]
struct VerifyCommandExecutionOutcome {
    result: GateCommandResult,
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
        for command in effective_verify_commands(slice, config)? {
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

fn verify_command_cwd_label(command: &VerifyCommand) -> String {
    if command.cwd.trim().is_empty() {
        ".".to_string()
    } else {
        command.cwd.clone()
    }
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

fn command_cache_key(worktree_root: &Path, tree_sha: &str, dedupe_key: &str) -> String {
    let context = worktree_root
        .canonicalize()
        .unwrap_or_else(|_| worktree_root.to_path_buf())
        .display()
        .to_string();
    format!("{context}\0{tree_sha}\0{dedupe_key}")
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
    use super::{IntegrationGateRequest, SliceVerificationRequest, WorkflowGate};
    use crate::agent::CancellationToken;
    use crate::domain::{Slice, VerifyCommand, VerifyProfile, WorkflowConfig};
    use crate::state::{ProgressReporter, Store as StateStore};
    use anyhow::Result;
    use std::collections::BTreeMap;
    use std::fs;
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

    #[test]
    fn integration_gate_success_uses_cwd_env_and_dedupes_commands() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
        fs::create_dir(worktree.path().join("sub"))?;
        fs::write(worktree.path().join("sub/marker.txt"), "ok\n")?;

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

    #[test]
    fn integration_gate_preserves_profile_order() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
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
    fn integration_gate_fail_fast_skips_remaining_commands() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
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
        let worktree = tempfile::tempdir()?;
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
        assert_eq!(
            result.summary,
            "one or more integration gate commands failed"
        );
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
        let worktree = tempfile::tempdir()?;
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
        let worktree = tempfile::tempdir()?;
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
    fn integration_gate_cancellation_returns_promptly() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
        let mut slice = slice("slice-001");
        slice.verify_timeout_seconds = 30;
        slice.verify = vec!["sleep 30".to_string()];
        let slices = vec![slice];
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            thread_cancel.cancel();
        });

        let started = Instant::now();
        let err = gate
            .run_integration_gate(
                IntegrationGateRequest {
                    slices: &slices,
                    integration_worktree: worktree.path(),
                    config: &WorkflowConfig::default(),
                },
                &cancel,
            )
            .unwrap_err();

        assert!(err.to_string().contains("run cancelled"));
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[test]
    fn slice_verification_failure_preserves_check_summary_text() -> Result<()> {
        let (_home, gate) = test_gate()?;
        let worktree = tempfile::tempdir()?;
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
