use crate::agent::{RunnerMetadata, Usage};
use crate::domain::{
    AgentCallEconomics, CommandExecutionEconomics, DuplicateCommandEconomics, PhaseDuration,
    RunEconomics,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct RunEconomicsRecorder {
    inner: Arc<Mutex<RunEconomics>>,
    snapshot_path: Arc<Mutex<Option<PathBuf>>>,
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
        }
    }

    pub(crate) fn with_snapshot_path(self, path: PathBuf) -> Self {
        *self
            .snapshot_path
            .lock()
            .expect("run economics snapshot mutex poisoned") = Some(path);
        self.persist_snapshot();
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
        self.persist_snapshot();
    }

    pub(crate) fn record_agent_call(&self, call: AgentCallEconomics) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            economics.agent_calls.push(call);
            economics.agent_call_count = economics.agent_calls.len();
        }
        self.persist_snapshot();
    }

    pub(crate) fn record_command(&self, command: CommandExecutionEconomics) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            if command.status != "skipped" {
                if command.cache_hit {
                    economics.cache_hits += 1;
                } else {
                    economics.cache_misses += 1;
                }
            }
            economics.command_executions.push(command);
            refresh_command_counts(&mut economics);
        }
        self.persist_snapshot();
    }

    pub(crate) fn set_repair_attempts(&self, attempts: usize) {
        {
            let mut economics = self.inner.lock().expect("run economics mutex poisoned");
            economics.repair_attempts = attempts;
        }
        self.persist_snapshot();
    }

    pub(crate) fn snapshot(&self) -> RunEconomics {
        let mut economics = self.inner.lock().expect("run economics mutex poisoned");
        refresh_command_counts(&mut economics);
        let snapshot = economics.clone();
        drop(economics);
        self.write_snapshot(&snapshot);
        snapshot
    }

    fn persist_snapshot(&self) {
        let snapshot = self
            .inner
            .lock()
            .expect("run economics mutex poisoned")
            .clone();
        self.write_snapshot(&snapshot);
    }

    fn write_snapshot(&self, snapshot: &RunEconomics) {
        let path_guard = self
            .snapshot_path
            .lock()
            .expect("run economics snapshot mutex poisoned");
        let Some(path) = path_guard.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(snapshot) {
            let tmp_path = path.with_extension("json.tmp");
            if fs::write(&tmp_path, bytes).is_ok() {
                let _ = fs::rename(tmp_path, path);
            }
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

fn refresh_command_counts(economics: &mut RunEconomics) {
    let executed = economics
        .command_executions
        .iter()
        .filter(|command| !command.cache_hit && command.status != "skipped")
        .count();
    economics.command_execution_count = executed;

    let mut by_key: BTreeMap<String, (String, String, usize)> = BTreeMap::new();
    for command in economics
        .command_executions
        .iter()
        .filter(|command| !command.cache_hit && command.status != "skipped")
    {
        let entry = by_key
            .entry(command.cache_key.clone())
            .or_insert_with(|| (command.dedupe_key.clone(), command.command.clone(), 0));
        entry.2 += 1;
    }
    economics.duplicate_commands = by_key
        .into_iter()
        .filter_map(|(_, (dedupe_key, command, executions))| {
            (executions > 1).then_some(DuplicateCommandEconomics {
                dedupe_key,
                command,
                executions,
            })
        })
        .collect();
    economics.duplicate_command_count = economics.duplicate_commands.len();
    economics
        .sla_violations
        .retain(|violation| violation != "duplicate daemon command executions detected");
    if economics.duplicate_command_count > 0 {
        economics
            .sla_violations
            .push("duplicate daemon command executions detected".to_string());
    }
}
