use super::shell::{ShellCommand, ShellProgress};
use super::{CancelledError, check_cancelled};
use crate::agent::CancellationToken;
use crate::domain::{Finding, GateCommandResult, GateResult, Slice, VerifyCommand, WorkflowConfig};
use crate::state::{ProgressReporter, ProgressScope};
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub(crate) const DEFAULT_VERIFY_TIMEOUT_SECONDS: u64 = 600;

pub(crate) struct WorkflowGate {
    progress: ProgressReporter,
}

impl WorkflowGate {
    pub(crate) fn new(progress: ProgressReporter) -> Self {
        Self { progress }
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
            let cwd = verify_command_cwd(request.worker_worktree, &command)?;
            self.mark_progress(
                "worker_verify",
                &request.slice.id,
                request.attempt,
                &command.command,
                "running slice verification command",
            );
            let progress = self.shell_progress_sink(
                "worker_verify",
                &request.slice.id,
                request.attempt,
                &command.command,
                "running slice verification command",
            );
            let output = match ShellCommand::new(&cwd, &command.command)
                .timeout(verify_command_timeout(
                    request.slice,
                    &command,
                    request.config,
                ))
                .envs(&command.env)
                .progress(Some(progress))
                .run(cancel)
            {
                Ok(output) => output,
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(CancelledError::new("run cancelled").into());
                    }
                    let summary = format!(
                        "verify command failed or timed out: {}: {err}",
                        command.command
                    );
                    result.failure = Some(SliceVerificationFailure::auto_fix(summary));
                    return Ok(result);
                }
            };
            if !output.success() {
                let summary = format!(
                    "verify command failed: {}\n{}",
                    command.command,
                    output.trimmed_combined_output()
                );
                result.failure = Some(SliceVerificationFailure::auto_fix(summary));
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
        for command in commands {
            check_cancelled(cancel)?;
            let cwd = verify_command_cwd(request.integration_worktree, &command)?;
            self.mark_progress(
                "integration_gate",
                "",
                0,
                &command.command,
                "running integration gate command",
            );
            let progress = self.shell_progress_sink(
                "integration_gate",
                "",
                0,
                &command.command,
                "running integration gate command",
            );
            let output = ShellCommand::new(&cwd, &command.command)
                .timeout(verify_command_timeout_for_command(&command, request.config))
                .envs(&command.env)
                .progress(Some(progress))
                .run(cancel);
            match output {
                Ok(output) => {
                    let status = if output.success() { "passed" } else { "failed" };
                    if !output.success() {
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
                        exit_code: output.exit_code(),
                        output: output.trimmed_combined_output(),
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
}

impl SliceVerificationFailure {
    fn auto_fix(summary: String) -> Self {
        Self {
            finding: Finding {
                id: String::new(),
                severity: "error".to_string(),
                action: "auto-fix".to_string(),
                file: String::new(),
                line: 0,
                description: summary.clone(),
            },
            summary,
        }
    }
}

pub(crate) struct IntegrationGateRequest<'a> {
    pub(crate) slices: &'a [Slice],
    pub(crate) integration_worktree: &'a Path,
    pub(crate) config: &'a WorkflowConfig,
}

fn integration_gate_commands(
    slices: &[Slice],
    config: &WorkflowConfig,
) -> Result<Vec<VerifyCommand>> {
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
    Ok(commands.into_values().collect())
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
        assert_eq!(
            result.findings[0].description,
            "integration gate command failed to start or timed out: sleep 30: command timed out after 1 seconds"
        );
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
        assert_eq!(failure.finding.description, failure.summary);
        Ok(())
    }
}
