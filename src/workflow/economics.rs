use crate::agent::{RunnerMetadata, Usage};
use crate::artifact;
use crate::domain::{
    AgentCallEconomics, CommandExecutionEconomics, DuplicateCommandEconomics, PhaseDuration,
    RunEconomics,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

type CommandCounts = BTreeMap<String, (String, String, usize)>;

#[derive(Clone)]
pub(crate) struct RunEconomicsRecorder {
    inner: Arc<Mutex<RunEconomics>>,
    snapshot_path: Arc<Mutex<Option<PathBuf>>>,
    checkpoint: Arc<Mutex<EconomicsCheckpointState>>,
    command_counts: Arc<Mutex<CommandCounts>>,
    revision: Arc<AtomicU64>,
}

struct EconomicsCheckpointState {
    minimum_interval: Duration,
    last_write: Option<Instant>,
    persisted_revision: Option<u64>,
}

impl RunEconomicsRecorder {
    pub(crate) fn new(
        repair_policy: impl Into<String>,
        gate_fail_fast: bool,
        worker_max_attempts: usize,
        repair_max_attempts: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RunEconomics {
                repair_policy: repair_policy.into(),
                gate_fail_fast,
                worker_max_attempts,
                repair_max_attempts,
                ..RunEconomics::default()
            })),
            snapshot_path: Arc::new(Mutex::new(None)),
            checkpoint: Arc::new(Mutex::new(EconomicsCheckpointState {
                minimum_interval: Duration::ZERO,
                last_write: None,
                persisted_revision: None,
            })),
            command_counts: Arc::new(Mutex::new(BTreeMap::new())),
            revision: Arc::new(AtomicU64::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_snapshot_path(self, path: PathBuf) -> Self {
        self.with_snapshot_path_and_interval(path, Duration::ZERO)
    }

    pub(crate) fn with_snapshot_path_and_interval(
        self,
        path: PathBuf,
        minimum_interval: Duration,
    ) -> Self {
        *self
            .snapshot_path
            .lock()
            .expect("run economics snapshot mutex poisoned") = Some(path);
        self.checkpoint
            .lock()
            .expect("run economics checkpoint mutex poisoned")
            .minimum_interval = minimum_interval;
        self.persist_snapshot_force();
        self
    }

    pub(crate) fn start_phase(&self, phase: impl Into<String>) -> PhaseTimer {
        PhaseTimer {
            recorder: self.clone(),
            phase: phase.into(),
            started_at: Instant::now(),
            finished: false,
        }
    }

    fn finish_phase(&self, phase: &str, duration: Duration) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            economics.phase_durations.insert(
                phase.to_string(),
                PhaseDuration {
                    phase: phase.to_string(),
                    duration_ms: duration.as_millis(),
                },
            );
        }
        self.mark_dirty_and_persist();
    }

    pub(crate) fn record_agent_call(&self, call: AgentCallEconomics) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            economics.agent_calls.push(call);
            economics.agent_call_count = economics.agent_calls.len();
        }
        self.mark_dirty_and_persist();
    }

    pub(crate) fn record_command(&self, command: CommandExecutionEconomics) {
        let execution_count = if !command.cache_hit && command.status != "skipped" {
            let mut counts = self
                .command_counts
                .lock()
                .expect("command economics mutex poisoned");
            let entry = counts
                .entry(command.cache_key.clone())
                .or_insert_with(|| (command.dedupe_key.clone(), command.command.clone(), 0));
            entry.2 = entry.2.saturating_add(1);
            Some((entry.0.clone(), entry.1.clone(), entry.2))
        } else {
            None
        };
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            if command.status != "skipped" {
                if command.cache_hit {
                    economics.cache_hits += 1;
                } else {
                    economics.cache_misses += 1;
                }
            }
            if let Some((dedupe_key, command_text, executions)) = execution_count {
                economics.command_execution_count =
                    economics.command_execution_count.saturating_add(1);
                if executions == 2 {
                    economics
                        .duplicate_commands
                        .push(DuplicateCommandEconomics {
                            dedupe_key,
                            command: command_text,
                            executions,
                        });
                    economics.duplicate_command_count = economics.duplicate_commands.len();
                    economics
                        .sla_violations
                        .push("duplicate daemon command executions detected".to_string());
                } else if executions > 2
                    && let Some(duplicate) = economics
                        .duplicate_commands
                        .iter_mut()
                        .find(|duplicate| duplicate.dedupe_key == dedupe_key)
                {
                    duplicate.executions = executions;
                }
            }
            economics.command_executions.push(command);
        }
        self.mark_dirty_and_persist();
    }

    pub(crate) fn set_repair_attempts(&self, attempts: usize) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            economics.repair_attempts = attempts;
        }
        self.mark_dirty_and_persist();
    }

    pub(crate) fn record_runtime_observation(
        &self,
        event_count: usize,
        event_bytes: usize,
        state_write_count: usize,
        supervisor_poll_count: usize,
    ) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            economics.worker_observation_event_count = economics
                .worker_observation_event_count
                .saturating_add(event_count);
            economics.worker_observation_bytes = economics
                .worker_observation_bytes
                .saturating_add(event_bytes);
            economics.worker_observation_state_write_count = economics
                .worker_observation_state_write_count
                .saturating_add(state_write_count);
            economics.supervisor_poll_count = economics
                .supervisor_poll_count
                .saturating_add(supervisor_poll_count);
        }
        self.mark_dirty_and_persist();
    }

    pub(crate) fn snapshot(&self) -> RunEconomics {
        self.persist_snapshot_force();
        self.inner
            .lock()
            .expect("run economics mutex poisoned")
            .clone()
    }

    fn mark_dirty_and_persist(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        self.persist_snapshot_inner(false);
    }

    fn persist_snapshot_force(&self) {
        self.persist_snapshot_inner(true);
    }

    fn persist_snapshot_inner(&self, force: bool) {
        let path = self
            .snapshot_path
            .lock()
            .expect("run economics snapshot mutex poisoned")
            .clone();
        let Some(path) = path else { return };
        let mut checkpoint = self
            .checkpoint
            .lock()
            .expect("run economics checkpoint mutex poisoned");
        let revision = self.revision.load(Ordering::SeqCst);
        if checkpoint.persisted_revision == Some(revision)
            || (!force
                && checkpoint
                    .last_write
                    .is_some_and(|last| last.elapsed() < checkpoint.minimum_interval))
        {
            return;
        }
        let mut economics = self.inner.lock().expect("run economics mutex poisoned");
        economics.economics_checkpoint_count =
            economics.economics_checkpoint_count.saturating_add(1);
        let snapshot = economics.clone();
        // This is a live, non-authoritative telemetry projection: terminal truth
        // remains in SQLite and final reports. It is still written through the
        // shared replacement seam so a reader observes either a complete prior
        // snapshot or a complete newer snapshot, never a truncated JSON file.
        if artifact::write_json(&path, &snapshot).is_ok() {
            checkpoint.last_write = Some(Instant::now());
            checkpoint.persisted_revision = Some(revision);
        } else {
            economics.economics_checkpoint_count =
                economics.economics_checkpoint_count.saturating_sub(1);
        }
    }
}

pub(crate) struct PhaseTimer {
    recorder: RunEconomicsRecorder,
    phase: String,
    started_at: Instant,
    finished: bool,
}

impl PhaseTimer {
    pub(crate) fn finish(mut self) {
        self.finished = true;
        self.recorder
            .finish_phase(&self.phase, self.started_at.elapsed());
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        if !self.finished {
            self.recorder
                .finish_phase(&self.phase, self.started_at.elapsed());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn agent_call(
    phase: impl Into<String>,
    slice_id: impl Into<String>,
    attempt: usize,
    kind: impl Into<String>,
    runner: impl Into<String>,
    metadata: &RunnerMetadata,
    status: impl Into<String>,
    duration: Duration,
    operator_pause: Duration,
    usage: Option<&Usage>,
    error: impl Into<String>,
) -> AgentCallEconomics {
    let usage = usage.cloned().unwrap_or_default();
    AgentCallEconomics {
        phase: phase.into(),
        slice_id: slice_id.into(),
        attempt,
        kind: kind.into(),
        runner: runner.into(),
        agent_profile: metadata.profile.clone(),
        agent_provider: metadata.provider.clone(),
        agent_model: metadata.model.clone(),
        agent_reasoning: metadata.reasoning.clone(),
        agent_mode: metadata.mode.clone(),
        profile_summary: metadata.profile_summary(),
        launch_summary: metadata.launch_summary(),
        status: status.into(),
        duration_ms: duration.as_millis(),
        operator_pause_ms: operator_pause.as_millis(),
        error: error.into(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::RunEconomicsRecorder;
    use crate::artifact;
    use crate::domain::{CommandExecutionEconomics, RunEconomics};
    use std::time::Duration;

    #[test]
    fn command_counts_and_duplicates_are_maintained_incrementally() {
        let recorder = RunEconomicsRecorder::new("auto", true, 2, 1);
        let command = CommandExecutionEconomics {
            command: "cargo test".to_string(),
            status: "passed".to_string(),
            dedupe_key: "cargo-test".to_string(),
            cache_key: "same-input".to_string(),
            ..CommandExecutionEconomics::default()
        };
        recorder.record_command(command.clone());
        recorder.record_command(command.clone());
        recorder.record_command(command);

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.command_execution_count, 3);
        assert_eq!(snapshot.duplicate_command_count, 1);
        assert_eq!(snapshot.duplicate_commands[0].executions, 3);
        assert_eq!(
            snapshot
                .sla_violations
                .iter()
                .filter(|violation| {
                    violation.as_str() == "duplicate daemon command executions detected"
                })
                .count(),
            1
        );
    }

    #[test]
    fn bounded_economics_checkpointing_forces_final_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outputs/economics.json");
        let recorder = RunEconomicsRecorder::new("auto", true, 2, 1)
            .with_snapshot_path_and_interval(path.clone(), Duration::from_secs(60));

        for index in 0..100 {
            let phase = recorder.start_phase(format!("worker-{index}"));
            phase.finish();
        }
        let live: RunEconomics = artifact::read_json(&path).unwrap();
        assert_eq!(live.economics_checkpoint_count, 1);
        assert!(live.phase_durations.is_empty());

        let final_snapshot = recorder.snapshot();
        assert_eq!(final_snapshot.phase_durations.len(), 100);
        assert_eq!(final_snapshot.economics_checkpoint_count, 2);
        let persisted: RunEconomics = artifact::read_json(&path).unwrap();
        assert_eq!(persisted.phase_durations.len(), 100);
        assert_eq!(persisted.economics_checkpoint_count, 2);
    }

    #[test]
    fn live_snapshot_uses_complete_json_artifact_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outputs/economics.json");
        let recorder =
            RunEconomicsRecorder::new("auto", true, 2, 1).with_snapshot_path(path.clone());

        let snapshot: RunEconomics = artifact::read_json(&path).unwrap();
        assert_eq!(snapshot.repair_policy, "auto");
        assert!(snapshot.gate_fail_fast);

        let phase = recorder.start_phase("worker_dispatch");
        phase.finish();
        let updated: RunEconomics = artifact::read_json(&path).unwrap();
        assert!(updated.phase_durations.contains_key("worker_dispatch"));
    }
}
