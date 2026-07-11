use super::cockpit::{rename_default_agent_target, send_default_agent_message};
use super::events as workflow_events;
use crate::artifact;
use crate::domain::{
    Event, Run, RunDetails, RunProgress, RunStatus, SliceRun, SliceStatus, StatusAction,
    StatusAttentionItem, TerminalNotificationRecord, WorkerQuestion,
};
use crate::state::Store as StateStore;
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const ATTENTION_PAYLOAD_SCHEMA_VERSION: u64 = 1;
const ATTENTION_DELIVERY_ADAPTER: &str = "herdr";
const ATTENTION_DELIVERY_SURFACE: &str = "agent_send";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionActionKind {
    InspectStatus,
    MonitorRun,
    AttendRun,
    AnswerQuestion,
    AcceptReplan,
    RejectReplan,
    DeferReplan,
    SupersedeReplan,
    ResumeRun,
    Handoff,
    OperatorCommand,
}

impl AttentionActionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InspectStatus => "inspect_status",
            Self::MonitorRun => "monitor_run",
            Self::AttendRun => "attend_run",
            Self::AnswerQuestion => "answer_question",
            Self::AcceptReplan => "accept_replan",
            Self::RejectReplan => "reject_replan",
            Self::DeferReplan => "defer_replan",
            Self::SupersedeReplan => "supersede_replan",
            Self::ResumeRun => "resume_run",
            Self::Handoff => "handoff",
            Self::OperatorCommand => "operator_command",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttentionIntentKind {
    TerminalTransition,
    WorkerQuestion,
    ReplanDecision,
    Incident,
}

impl AttentionIntentKind {
    fn as_str(self, fallback: &str) -> &str {
        match self {
            Self::TerminalTransition | Self::Incident => fallback,
            Self::WorkerQuestion => "worker_question",
            Self::ReplanDecision => "replan_decision",
        }
    }
}

#[derive(Debug, Clone)]
struct AttentionIntent {
    id: String,
    kind: AttentionIntentKind,
    kind_detail: String,
    priority: u32,
    summary: String,
    commands: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AttentionPolicyProjection {
    pub(crate) operator_commands: Vec<String>,
    pub(crate) actions: Vec<StatusAction>,
    pub(crate) attention_items: Vec<StatusAttentionItem>,
}

pub(crate) fn project_attention_policy(details: &RunDetails) -> AttentionPolicyProjection {
    let mut operator_commands = Vec::new();
    for proposal in &details.replan.pending {
        for command in &proposal.decision_commands {
            push_unique(&mut operator_commands, command.clone());
        }
    }
    for question in details
        .questions
        .iter()
        .filter(|question| question.state == "pending")
    {
        push_unique(
            &mut operator_commands,
            format!(
                "khazad-doom answer {} {} <answer>",
                question.run_id, question.id
            ),
        );
    }
    if let Some(reason) = &details.primary_terminal_reason {
        for command in &reason.operator_commands {
            push_unique(&mut operator_commands, command.clone());
        }
    }
    if details.run.status == RunStatus::Completed {
        push_unique(
            &mut operator_commands,
            format!("khazad-doom handoff --run {}", details.run.id),
        );
    }

    let mut action_commands = operator_commands.clone();
    for command in status_navigation_commands(&details.run.id) {
        push_unique(&mut action_commands, command);
    }
    let actions = actions_for_commands(&details.run.id, &action_commands);
    let intents = attention_intents(details);
    let mut attention_items = intents
        .into_iter()
        .map(|intent| StatusAttentionItem {
            id: intent.id,
            kind: intent.kind.as_str(&intent.kind_detail).to_string(),
            priority: intent.priority,
            summary: intent.summary,
            action_ids: action_ids_for_commands(&actions, &intent.commands),
        })
        .collect::<Vec<_>>();
    attention_items.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    AttentionPolicyProjection {
        operator_commands,
        actions,
        attention_items,
    }
}

fn attention_intents(details: &RunDetails) -> Vec<AttentionIntent> {
    let mut intents = Vec::new();
    if let Some(reason) = details.primary_terminal_reason.as_ref()
        && reason.operator_action_required
    {
        intents.push(AttentionIntent {
            id: "terminal-reason".to_string(),
            kind: AttentionIntentKind::TerminalTransition,
            kind_detail: reason.kind.clone(),
            priority: 100,
            summary: reason.summary.clone(),
            commands: reason.operator_commands.clone(),
        });
    }
    for question in details
        .questions
        .iter()
        .filter(|question| question.state == "pending")
    {
        intents.push(AttentionIntent {
            id: format!("question:{}", question.id),
            kind: AttentionIntentKind::WorkerQuestion,
            kind_detail: String::new(),
            priority: 90,
            summary: question.question.clone(),
            commands: vec![format!(
                "khazad-doom answer {} {} <answer>",
                question.run_id, question.id
            )],
        });
    }
    for proposal in details
        .replan
        .pending
        .iter()
        .filter(|proposal| proposal.state == crate::domain::ReplanProposalState::Pending)
    {
        intents.push(AttentionIntent {
            id: format!("replan:{}", proposal.id),
            kind: AttentionIntentKind::ReplanDecision,
            kind_detail: String::new(),
            priority: 80,
            summary: proposal.source.summary.clone(),
            commands: proposal.decision_commands.clone(),
        });
    }
    for incident in &details.incidents {
        if matches!(incident.severity.as_str(), "error" | "warning") {
            intents.push(AttentionIntent {
                id: format!("incident:{}", incident.event_id),
                kind: AttentionIntentKind::Incident,
                kind_detail: incident.kind.clone(),
                priority: if incident.severity == "error" { 70 } else { 60 },
                summary: incident.message.clone(),
                commands: Vec::new(),
            });
        }
    }
    intents
}

pub(crate) fn actions_for_commands(current_run_id: &str, commands: &[String]) -> Vec<StatusAction> {
    commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let (run_id, target_id) = action_authority(command);
            if !run_id.is_empty() && run_id != current_run_id {
                return None;
            }
            Some(StatusAction {
                id: action_id(command),
                kind: action_kind(command).as_str().to_string(),
                label: command.clone(),
                command: command.clone(),
                priority: 100u32.saturating_sub(index as u32),
                run_id,
                target_id,
            })
        })
        .collect()
}

fn action_kind(command: &str) -> AttentionActionKind {
    if command.starts_with("khazad-doom status ") {
        AttentionActionKind::InspectStatus
    } else if command.starts_with("khazad-doom monitor ") {
        AttentionActionKind::MonitorRun
    } else if command.starts_with("khazad-doom attend ") {
        AttentionActionKind::AttendRun
    } else if command.starts_with("khazad-doom answer ") {
        AttentionActionKind::AnswerQuestion
    } else if command.starts_with("khazad-doom replan accept ") {
        AttentionActionKind::AcceptReplan
    } else if command.starts_with("khazad-doom replan reject ") {
        AttentionActionKind::RejectReplan
    } else if command.starts_with("khazad-doom replan defer ") {
        AttentionActionKind::DeferReplan
    } else if command.starts_with("khazad-doom replan supersede ") {
        AttentionActionKind::SupersedeReplan
    } else if command.starts_with("khazad-doom resume ") {
        AttentionActionKind::ResumeRun
    } else if command.starts_with("khazad-doom handoff ") {
        AttentionActionKind::Handoff
    } else {
        AttentionActionKind::OperatorCommand
    }
}

fn action_authority(command: &str) -> (String, String) {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["khazad-doom", "answer", run_id, target_id, ..]
        | ["khazad-doom", "replan", _, run_id, target_id, ..] => {
            ((*run_id).to_string(), (*target_id).to_string())
        }
        _ => {
            let run_id = parts
                .windows(2)
                .find(|pair| pair[0] == "--run")
                .map(|pair| pair[1].to_string())
                .unwrap_or_default();
            (run_id, String::new())
        }
    }
}

fn action_id(command: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(command.as_bytes()));
    format!("action-{}", &digest[..12])
}

fn status_navigation_commands(run_id: &str) -> Vec<String> {
    vec![
        format!("khazad-doom status --run {run_id}"),
        format!("khazad-doom monitor --run {run_id}"),
        format!("khazad-doom attend --run {run_id}"),
    ]
}

fn action_ids_for_commands(actions: &[StatusAction], commands: &[String]) -> Vec<String> {
    actions
        .iter()
        .filter(|action| commands.iter().any(|command| command == &action.command))
        .map(|action| action.id.clone())
        .collect()
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneHygieneIntent {
    pane_id: String,
    slice_id: String,
    launch_id: Option<i64>,
    launch_stem: String,
    status: SliceStatus,
    label: String,
}

fn pane_hygiene_intents(events: &[Event], slice_runs: &[SliceRun]) -> Vec<PaneHygieneIntent> {
    let statuses = slice_runs
        .iter()
        .map(|slice| (slice.slice_id.as_str(), slice.status))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    events
        .iter()
        .filter(|event| {
            workflow_events::EventKind::from(event.typ.as_str())
                == workflow_events::EventKind::CockpitWorkerReady
        })
        .map(|event| workflow_events::CockpitWorkerReadyPayload::from_value(&event.payload))
        .filter_map(|event| {
            if event.pane_id.is_empty()
                || event.slice_id.is_empty()
                || !seen.insert(event.pane_id.clone())
            {
                return None;
            }
            let status = statuses
                .get(event.slice_id.as_str())
                .copied()
                .unwrap_or(SliceStatus::Pending);
            let marker = match status {
                SliceStatus::Merged => "✓",
                SliceStatus::Blocked | SliceStatus::Failed | SliceStatus::Cancelled => "✗",
                SliceStatus::Interrupted => "!",
                _ => "◐",
            };
            let label = match event.launch_id.filter(|launch_id| *launch_id > 0) {
                Some(launch_id) => {
                    format!("{marker} {} launch {launch_id} {status}", event.slice_id)
                }
                None => format!("{marker} {} {status}", event.slice_id),
            };
            Some(PaneHygieneIntent {
                pane_id: event.pane_id,
                slice_id: event.slice_id,
                launch_id: event.launch_id,
                launch_stem: event.launch_stem,
                status,
                label,
            })
        })
        .collect()
}

#[derive(Clone)]
pub(crate) struct OperatorAttention {
    state: StateStore,
}

pub(crate) struct TerminalTransitionNotification<'a> {
    pub run: &'a Run,
    pub status: RunStatus,
    pub progress: Option<&'a RunProgress>,
    pub summary: &'a Value,
    pub summary_path: &'a Path,
}

pub(crate) struct WorkerPaneTerminalRename<'a> {
    pub run: &'a Run,
    pub events: &'a [Event],
    pub slice_runs: &'a [SliceRun],
}

impl OperatorAttention {
    pub(crate) fn new(state: StateStore) -> Self {
        Self { state }
    }

    pub(crate) fn terminal_transition_notification(
        &self,
        intent: TerminalTransitionNotification<'_>,
    ) {
        if !terminal_feedback_status_supported(intent.status) {
            return;
        }
        let store = artifact::Store::new(&intent.run.repo_path);
        let terminal_status = intent.status.as_str();
        let transition_key =
            terminal_transition_key(intent.status, intent.progress, intent.run, intent.summary);
        if store.terminal_notification_exists(&intent.run.id, &transition_key) {
            return;
        }
        let created_at = Utc::now().to_rfc3339();
        let payload = terminal_feedback_payload(intent.summary, intent.summary_path);
        let origin = match store.read_origin_notification_target(&intent.run.id) {
            Ok(Some(origin)) if !origin.target.trim().is_empty() => origin,
            Ok(Some(_)) => {
                self.write_terminal_notification_record(
                    &store,
                    intent.run,
                    terminal_status,
                    &transition_key,
                    "skipped",
                    "",
                    "missing origin notification target",
                    payload,
                    created_at,
                );
                let _ = self.state.record_typed_workflow_event(
                    &intent.run.id,
                    &workflow_events::WorkflowEvent::terminal_notification_skipped(
                        workflow_events::TerminalNotificationPayload::skipped(
                            terminal_status,
                            &transition_key,
                            "missing_origin_target",
                        ),
                    ),
                );
                return;
            }
            Ok(None) => return,
            Err(err) => {
                self.write_terminal_notification_record(
                    &store,
                    intent.run,
                    terminal_status,
                    &transition_key,
                    "failed",
                    "",
                    &format!("origin target read failed: {err}"),
                    payload,
                    created_at,
                );
                self.record_terminal_notification_incident(
                    intent.run,
                    terminal_status,
                    &transition_key,
                    "origin_target_read_failed",
                    &err.to_string(),
                );
                return;
            }
        };
        if !self.write_terminal_notification_record(
            &store,
            intent.run,
            terminal_status,
            &transition_key,
            "pending",
            &origin.target,
            "",
            payload.clone(),
            created_at.clone(),
        ) {
            return;
        }
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
        match send_default_agent_message(&origin.target, &text) {
            Ok(sent) => {
                self.write_terminal_notification_record(
                    &store,
                    intent.run,
                    terminal_status,
                    &transition_key,
                    "sent",
                    &origin.target,
                    "",
                    payload,
                    created_at,
                );
                let _ = self.state.record_typed_workflow_event(
                    &intent.run.id,
                    &workflow_events::WorkflowEvent::terminal_notification_sent(
                        workflow_events::TerminalNotificationPayload::sent(
                            terminal_status,
                            &transition_key,
                            sent.adapter,
                            sent.surface,
                            origin.target_kind,
                        ),
                    ),
                );
            }
            Err(err) => {
                self.write_terminal_notification_record(
                    &store,
                    intent.run,
                    terminal_status,
                    &transition_key,
                    "failed",
                    &origin.target,
                    &err.message,
                    payload,
                    created_at,
                );
                self.record_terminal_notification_incident(
                    intent.run,
                    terminal_status,
                    &transition_key,
                    "delivery_failed",
                    &err.message,
                );
            }
        }
    }

    pub(crate) fn worker_pane_terminal_rename(&self, intent: WorkerPaneTerminalRename<'_>) {
        for pane in pane_hygiene_intents(intent.events, intent.slice_runs) {
            let pane_id = pane.pane_id.as_str();
            let slice_id = pane.slice_id.as_str();
            let status = pane.status;
            let label = pane.label;
            match rename_default_agent_target(pane_id, &label) {
                Ok(renamed) => {
                    let _ = self.state.record_workflow_event(
                        &intent.run.id,
                        &workflow_events::CockpitWorkerRenamedPayload {
                            pane_id: pane_id.to_string(),
                            slice_id: slice_id.to_string(),
                            launch_id: pane.launch_id,
                            launch_stem: pane.launch_stem,
                            status: status.to_string(),
                            label,
                            adapter: renamed.adapter,
                            surface: renamed.surface,
                        },
                    );
                }
                Err(err) => {
                    let _ = self.state.record_workflow_event(
                        &intent.run.id,
                        &workflow_events::RunIncidentPayload::warning(
                            "cockpit_worker_rename_failed",
                            format!("worker pane rename failed for {slice_id}: {}", err.message),
                        )
                        .with_extra("pane_id", pane_id)
                        .with_extra("slice_id", slice_id)
                        .with_extra("launch_id", pane.launch_id)
                        .with_extra("launch_stem", &pane.launch_stem)
                        .with_extra("label", &label)
                        .with_extra("source_of_truth", "daemon_terminal_summary"),
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_terminal_notification_record(
        &self,
        store: &artifact::Store,
        run: &Run,
        terminal_status: &str,
        transition_key: &str,
        delivery_status: &str,
        origin_target: &str,
        error: &str,
        payload: Value,
        created_at: String,
    ) -> bool {
        let record = TerminalNotificationRecord {
            schema_version: ATTENTION_PAYLOAD_SCHEMA_VERSION,
            run_id: run.id.clone(),
            terminal_status: terminal_status.to_string(),
            transition_key: transition_key.to_string(),
            delivery_status: delivery_status.to_string(),
            origin_target: origin_target.to_string(),
            delivery_adapter: ATTENTION_DELIVERY_ADAPTER.to_string(),
            delivery_surface: ATTENTION_DELIVERY_SURFACE.to_string(),
            error: error.to_string(),
            payload,
            created_at,
        };
        if let Err(err) = store.write_terminal_notification_record(&run.id, transition_key, &record)
        {
            self.record_terminal_notification_incident(
                run,
                terminal_status,
                transition_key,
                "record_write_failed",
                &err.to_string(),
            );
            return false;
        }
        true
    }

    fn record_terminal_notification_incident(
        &self,
        run: &Run,
        terminal_status: &str,
        transition_key: &str,
        failure_kind: &str,
        message: &str,
    ) {
        let _ = self.state.record_workflow_event(
            &run.id,
            &workflow_events::RunIncidentPayload::warning(
                "terminal_notification_failed",
                format!("terminal notification for {terminal_status} was not delivered: {message}"),
            )
            .with_extra("visibility_kind", failure_kind)
            .with_extra("terminal_status", terminal_status)
            .with_extra("transition_key", transition_key)
            .with_extra("source_of_truth", "daemon_terminal_summary"),
        );
    }
}

pub(crate) fn worker_question_deadline(question: &WorkerQuestion) -> Option<String> {
    question.deadline_at.map(|deadline| deadline.to_rfc3339())
}

fn terminal_feedback_status_supported(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled
    )
}

fn terminal_transition_key(
    status: RunStatus,
    progress: Option<&RunProgress>,
    run: &Run,
    summary: &Value,
) -> String {
    let revision = progress
        .filter(|progress| progress.phase == status.as_str())
        .map(|progress| progress.phase_started_at.to_rfc3339())
        .or_else(|| {
            summary
                .pointer("/snapshot/revision/max_event_id")
                .and_then(Value::as_i64)
                .map(|event_id| format!("event-{event_id}"))
        })
        .unwrap_or_else(|| run.updated_at.to_rfc3339());
    format!("terminal:{}:{revision}", status.as_str())
}

fn terminal_feedback_payload(summary: &Value, summary_path: &Path) -> Value {
    let evidence_artifacts = vec![summary_path.to_string_lossy().to_string()];
    let feed = summary.get("feed").cloned().unwrap_or(Value::Null);
    let feed_summary = feed
        .get("summary_line")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            summary
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
        });
    json!({
        "kind": "khazad_terminal_feedback",
        "run_id": summary.get("run_id").cloned().unwrap_or(Value::Null),
        "terminal_status": summary.get("status").cloned().unwrap_or(Value::Null),
        "repo_path": summary.get("repo_path").cloned().unwrap_or(Value::Null),
        "integration_branch": summary.get("integration_branch").cloned().unwrap_or(Value::Null),
        "selected_slice_id": summary.get("selected_slice_id").cloned().unwrap_or(Value::Null),
        "message": feed_summary,
        "feed_summary_line": feed_summary,
        "feed": feed,
        "primary_failure": summary.get("primary_failure").and_then(Value::as_str).unwrap_or_default(),
        "cancel_reason": summary.get("cancel_reason").and_then(Value::as_str).unwrap_or_default(),
        "final_sha": "",
        "handoff_readiness": "unavailable",
        "evidence_artifacts": evidence_artifacts,
        "snapshot": summary.get("snapshot").cloned().unwrap_or(Value::Null),
        "next_commands": summary.get("next_commands").cloned().unwrap_or_else(|| json!([])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Store as ArtifactStore;
    use crate::domain::OriginNotificationTarget;
    use anyhow::Result;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn attention_action_parity_preserves_kind_authority_and_stable_ids() {
        let commands = vec![
            "khazad-doom answer run-1 question-1 <answer>".to_string(),
            "khazad-doom replan accept run-1 proposal-1 --rationale <text>".to_string(),
            "khazad-doom status --run run-1".to_string(),
            "khazad-doom answer other-run question-2 <answer>".to_string(),
        ];
        let actions = actions_for_commands("run-1", &commands);
        assert_eq!(actions.len(), 3, "cross-run action must fail closed");
        assert_eq!(actions[0].kind, "answer_question");
        assert_eq!(actions[0].run_id, "run-1");
        assert_eq!(actions[0].target_id, "question-1");
        assert_eq!(actions[1].kind, "accept_replan");
        assert_eq!(actions[1].target_id, "proposal-1");
        assert_eq!(actions[2].kind, "inspect_status");
        assert_eq!(actions[2].run_id, "run-1");
        assert_eq!(
            actions_for_commands("run-1", &commands)[0].id,
            actions[0].id,
            "the same daemon command must retain one stable action identity"
        );
    }

    fn state_store() -> Result<(tempfile::TempDir, StateStore)> {
        let home = tempfile::tempdir()?;
        let state = StateStore::open(home.path().join("state.sqlite"))?;
        Ok((home, state))
    }

    fn run_fixture(repo_path: &Path, run_id: &str) -> Run {
        let now = Utc::now();
        Run {
            id: run_id.to_string(),
            repo_id: "repo-fixture".to_string(),
            repo_path: repo_path.to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base-sha".to_string(),
            integration_branch: format!("khazad/{run_id}/integration"),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        }
    }

    fn origin() -> OriginNotificationTarget {
        OriginNotificationTarget {
            schema_version: ATTENTION_PAYLOAD_SCHEMA_VERSION,
            target: "agent-1".to_string(),
            target_kind: "opaque".to_string(),
            delivery_adapter: ATTENTION_DELIVERY_ADAPTER.to_string(),
            delivery_surface: ATTENTION_DELIVERY_SURFACE.to_string(),
            source: "test".to_string(),
            created_at: Utc::now().to_rfc3339(),
        }
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

    #[test]
    fn attention_terminal_transition_dedupes_by_transition_key() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let artifact_store = ArtifactStore::new(repo.path());
        let (_home, state) = state_store()?;
        let run = run_fixture(repo.path(), "kd-attention-terminal-dedupe");
        state.insert_run(&run)?;
        artifact_store.write_origin_notification_target(&run.id, &origin())?;
        let progress = state.update_progress(
            &run.id,
            "blocked",
            "slice-001",
            1,
            "gate",
            "blocked once",
            "",
        )?;
        let summary_path = artifact_store.output_path(&run.id, "run-summary.json");
        let summary = json!({
            "message": "blocked once",
            "feed": {"summary_line": "run blocked"},
            "next_commands": ["khazad-doom inspect --run kd-attention-terminal-dedupe"]
        });
        artifact::write_json(&summary_path, &summary)?;
        let attention = OperatorAttention::new(state.clone());

        for _ in 0..2 {
            attention.terminal_transition_notification(TerminalTransitionNotification {
                run: &run,
                status: RunStatus::Blocked,
                progress: Some(&progress),
                summary: &summary,
                summary_path: &summary_path,
            });
        }

        let records = terminal_notification_records(&artifact_store, &run.id)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].terminal_status, "blocked");
        assert!(records[0].transition_key.starts_with("terminal:blocked:"));
        assert_eq!(records[0].delivery_status, "failed");
        let incidents = state
            .get_events(&run.id, 100)?
            .into_iter()
            .filter(|event| {
                event.typ == "run_incident"
                    && event.payload["kind"] == "terminal_notification_failed"
            })
            .count();
        assert_eq!(incidents, 1);
        Ok(())
    }

    #[test]
    fn attention_worker_pane_terminal_rename_dedupes_pane_ids() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let (_home, state) = state_store()?;
        let run = run_fixture(repo.path(), "kd-attention-pane-rename");
        state.insert_run(&run)?;
        let now = Utc::now();
        let events = vec![
            Event {
                id: 1,
                run_id: run.id.clone(),
                typ: "cockpit_worker_ready".to_string(),
                payload: json!({
                    "pane_id": "pane-1",
                    "slice_id": "slice-001",
                    "launch_id": 41,
                    "launch_stem": "slice-001.launch-41"
                }),
                created_at: now,
            },
            Event {
                id: 2,
                run_id: run.id.clone(),
                typ: "cockpit_worker_ready".to_string(),
                payload: json!({
                    "pane_id": "pane-1",
                    "slice_id": "slice-001",
                    "launch_id": 41,
                    "launch_stem": "slice-001.launch-41"
                }),
                created_at: now,
            },
        ];
        let slice_runs = vec![SliceRun {
            run_id: run.id.clone(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Merged,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        }];

        OperatorAttention::new(state.clone()).worker_pane_terminal_rename(
            WorkerPaneTerminalRename {
                run: &run,
                events: &events,
                slice_runs: &slice_runs,
            },
        );

        let rename_incidents = state
            .get_events(&run.id, 100)?
            .into_iter()
            .filter(|event| {
                event.typ == "run_incident"
                    && event.payload["kind"] == "cockpit_worker_rename_failed"
            })
            .collect::<Vec<_>>();
        assert_eq!(rename_incidents.len(), 1);
        assert_eq!(rename_incidents[0].payload["launch_id"], 41);
        assert_eq!(
            rename_incidents[0].payload["launch_stem"],
            "slice-001.launch-41"
        );
        assert_eq!(
            rename_incidents[0].payload["label"],
            "✓ slice-001 launch 41 merged"
        );
        Ok(())
    }

    #[test]
    fn worker_question_deadline_uses_the_durable_absolute_deadline() {
        let now = Utc::now();
        let deadline = now + chrono::Duration::from_std(Duration::from_secs(5)).unwrap();
        let question = WorkerQuestion {
            id: "q-deadline".to_string(),
            run_id: "run".to_string(),
            slice_id: "slice".to_string(),
            attempt: 1,
            launch_id: None,
            question: "choose".to_string(),
            options: Vec::new(),
            timeout_seconds: 5,
            recommended_answer: String::new(),
            recommendation_rationale: String::new(),
            bounded_within_current_slice_or_mission_authority: false,
            reversible: false,
            fallback_eligible: false,
            deadline_at: Some(deadline),
            state: "pending".to_string(),
            asked_at: now,
            answered_at: None,
            answer: String::new(),
            answer_source: None,
        };

        assert_eq!(
            worker_question_deadline(&question),
            Some(deadline.to_rfc3339())
        );
    }
}
