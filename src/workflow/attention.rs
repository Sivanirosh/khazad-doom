use super::cockpit::{
    focus_default_agent_target, rename_default_agent_target, send_default_agent_message,
};
use super::events as workflow_events;
use crate::artifact;
use crate::domain::{
    Event, ImplementationSummary, ReplanProposal, Run, RunProgress, RunStatus, SliceRun,
    SliceStatus, TerminalNotificationRecord, WorkerQuestion, replan_decision_commands,
};
use crate::state::Store as StateStore;
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const ATTENTION_PAYLOAD_SCHEMA_VERSION: u64 = 1;
const ATTENTION_DELIVERY_ADAPTER: &str = "herdr";
const ATTENTION_DELIVERY_SURFACE: &str = "agent_send";

#[derive(Clone)]
pub(crate) struct OperatorAttention {
    state: StateStore,
}

pub(crate) struct WorkerQuestionPending<'a> {
    pub question: &'a WorkerQuestion,
}

pub(crate) struct ReplanDecisionPending<'a> {
    pub run: &'a Run,
    pub proposal: &'a ReplanProposal,
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

    pub(crate) fn worker_question_pending(&self, intent: WorkerQuestionPending<'_>) {
        let question = intent.question;
        let Ok(Some(run)) = self.state.get_run(&question.run_id) else {
            return;
        };
        let Some(origin) = self.origin_target(&run) else {
            return;
        };
        let payload = json!({
            "schema_version": ATTENTION_PAYLOAD_SCHEMA_VERSION,
            "kind": "worker_question_pending",
            "run_id": question.run_id,
            "slice_id": question.slice_id,
            "attempt": question.attempt,
            "question_id": question.id,
            "question": question.question,
            "options": question.options,
            "timeout_seconds": question.timeout_seconds,
            "deadline_at": worker_question_deadline(question),
            "answer_command": worker_question_answer_command(question),
            "source_of_truth": "daemon_worker_questions",
        });
        self.send_and_focus_attention(
            &run,
            &origin,
            "worker_question_pending",
            payload,
            AttentionFailureContext {
                source_of_truth: "daemon_worker_questions",
                delivery_message: "worker question notification was not delivered",
                focus_message: "worker question focus was not delivered",
                payload_fields: json!({
                    "question_id": question.id,
                    "slice_id": question.slice_id,
                }),
            },
        );
    }

    pub(crate) fn replan_decision_pending(&self, intent: ReplanDecisionPending<'_>) {
        let Some(origin) = self.origin_target(intent.run) else {
            return;
        };
        let commands = replan_decision_commands(&intent.run.id, &intent.proposal.id);
        let payload = serde_json::to_value(workflow_events::ReplanNotificationPayload::new(
            &intent.run.id,
            &intent.proposal.id,
            intent.proposal.source.clone(),
            &intent.proposal.risk,
            intent.proposal.proposed_changes.clone(),
            commands,
        ))
        .unwrap_or(Value::Null);
        self.send_and_focus_attention(
            intent.run,
            &origin,
            "replan_decision_pending",
            payload,
            AttentionFailureContext {
                source_of_truth: "daemon_replan_proposals",
                delivery_message: "replan proposal notification was not delivered",
                focus_message: "replan proposal focus was not delivered",
                payload_fields: json!({
                    "proposal_id": intent.proposal.id,
                }),
            },
        );
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
        let transition_key = terminal_transition_key(intent.status, intent.progress);
        if store.terminal_notification_exists(&intent.run.id, &transition_key) {
            return;
        }
        let created_at = Utc::now().to_rfc3339();
        let final_report_path = store.output_path(&intent.run.id, "final-report.json");
        let implementation_summary_path =
            store.output_path(&intent.run.id, "implementation-summary.json");
        let payload = terminal_feedback_payload(
            intent.run,
            intent.status,
            intent.summary,
            intent.summary_path,
            final_report_path
                .exists()
                .then_some(final_report_path.as_path()),
            implementation_summary_path
                .exists()
                .then_some(implementation_summary_path.as_path()),
        );
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
                let _ = self.state.record_event(
                    &intent.run.id,
                    workflow_events::TERMINAL_NOTIFICATION_SKIPPED,
                    &workflow_events::TerminalNotificationPayload::skipped(
                        terminal_status,
                        &transition_key,
                        "missing_origin_target",
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
                let _ = self.state.record_event(
                    &intent.run.id,
                    workflow_events::TERMINAL_NOTIFICATION_SENT,
                    &workflow_events::TerminalNotificationPayload::sent(
                        terminal_status,
                        &transition_key,
                        sent.adapter,
                        sent.surface,
                        origin.target_kind,
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
        let statuses = intent
            .slice_runs
            .iter()
            .map(|slice| (slice.slice_id.as_str(), slice.status))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        for event in intent
            .events
            .iter()
            .filter(|event| event.typ == workflow_events::COCKPIT_WORKER_READY)
            .map(|event| workflow_events::CockpitWorkerReadyPayload::from_value(&event.payload))
        {
            let pane_id = event.pane_id.as_str();
            let slice_id = event.slice_id.as_str();
            if pane_id.is_empty() || slice_id.is_empty() || !seen.insert(pane_id.to_string()) {
                continue;
            }
            let status = statuses
                .get(slice_id)
                .copied()
                .unwrap_or(SliceStatus::Pending);
            let marker = match status {
                SliceStatus::Merged => "✓",
                SliceStatus::Blocked | SliceStatus::Failed | SliceStatus::Cancelled => "✗",
                SliceStatus::Interrupted => "!",
                _ => "◐",
            };
            let label = format!("{marker} {slice_id} {status}");
            match rename_default_agent_target(pane_id, &label) {
                Ok(renamed) => {
                    let _ = self.state.record_event(
                        &intent.run.id,
                        "cockpit_worker_renamed",
                        &json!({
                            "pane_id": pane_id,
                            "slice_id": slice_id,
                            "status": status,
                            "label": label,
                            "adapter": renamed.adapter,
                            "surface": renamed.surface,
                        }),
                    );
                }
                Err(err) => {
                    let _ = self.state.record_event(
                        &intent.run.id,
                        workflow_events::RUN_INCIDENT,
                        &workflow_events::RunIncidentPayload::warning(
                            "cockpit_worker_rename_failed",
                            format!("worker pane rename failed for {slice_id}: {}", err.message),
                        )
                        .with_extra("pane_id", pane_id)
                        .with_extra("slice_id", slice_id)
                        .with_extra("source_of_truth", "daemon_terminal_summary"),
                    );
                }
            }
        }
    }

    fn origin_target(&self, run: &Run) -> Option<crate::domain::OriginNotificationTarget> {
        let store = artifact::Store::new(&run.repo_path);
        match store.read_origin_notification_target(&run.id) {
            Ok(Some(origin)) if !origin.target.trim().is_empty() => Some(origin),
            _ => None,
        }
    }

    fn send_and_focus_attention(
        &self,
        run: &Run,
        origin: &crate::domain::OriginNotificationTarget,
        kind: &str,
        payload: Value,
        context: AttentionFailureContext,
    ) {
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
        match send_default_agent_message(&origin.target, &text) {
            Ok(sent) => {
                let event_payload = context.delivery_payload(
                    kind,
                    sent.adapter,
                    sent.surface,
                    origin.target_kind.clone(),
                );
                let _ = self.state.record_event(
                    &run.id,
                    workflow_events::ATTENTION_NOTIFICATION_SENT,
                    &event_payload,
                );
            }
            Err(err) => {
                let _ = self.state.record_event(
                    &run.id,
                    workflow_events::RUN_INCIDENT,
                    &context.failure_payload(
                        "attention_notification_failed",
                        "delivery_failed",
                        format!("{}: {}", context.delivery_message, err.message),
                    ),
                );
            }
        }
        match focus_default_agent_target(&origin.target) {
            Ok(focused) => {
                let event_payload = context.delivery_payload(
                    kind,
                    focused.adapter,
                    focused.surface,
                    origin.target_kind.clone(),
                );
                let _ = self.state.record_event(
                    &run.id,
                    workflow_events::ATTENTION_FOCUS_SENT,
                    &event_payload,
                );
            }
            Err(err) => {
                let _ = self.state.record_event(
                    &run.id,
                    workflow_events::RUN_INCIDENT,
                    &context.failure_payload(
                        "attention_focus_failed",
                        "focus_failed",
                        format!("{}: {}", context.focus_message, err.message),
                    ),
                );
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
        let _ = self.state.record_event(
            &run.id,
            workflow_events::RUN_INCIDENT,
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

struct AttentionFailureContext {
    source_of_truth: &'static str,
    delivery_message: &'static str,
    focus_message: &'static str,
    payload_fields: Value,
}

impl AttentionFailureContext {
    fn delivery_payload(
        &self,
        kind: &str,
        adapter: String,
        surface: String,
        target_kind: String,
    ) -> workflow_events::AttentionDeliveryPayload {
        workflow_events::AttentionDeliveryPayload {
            kind: kind.to_string(),
            question_id: self.payload_field("question_id"),
            slice_id: self.payload_field("slice_id"),
            proposal_id: self.payload_field("proposal_id"),
            adapter,
            surface,
            target_kind,
        }
    }

    fn failure_payload(
        &self,
        kind: &str,
        visibility_kind: &str,
        message: String,
    ) -> workflow_events::RunIncidentPayload {
        let mut payload = workflow_events::RunIncidentPayload::warning(kind, message)
            .with_extra("visibility_kind", visibility_kind)
            .with_extra("source_of_truth", self.source_of_truth);
        for key in ["question_id", "slice_id", "proposal_id"] {
            let value = self.payload_field(key);
            if !value.trim().is_empty() {
                payload = payload.with_extra(key, value);
            }
        }
        payload
    }

    fn payload_field(&self, key: &str) -> String {
        self.payload_fields
            .get(key)
            .and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_u64().map(|number| number.to_string()))
            })
            .unwrap_or_default()
    }
}

pub(crate) fn worker_question_answer_command(question: &WorkerQuestion) -> String {
    format!(
        "khazad-doom answer {} {} <answer>",
        question.run_id, question.id
    )
}

pub(crate) fn worker_question_deadline(question: &WorkerQuestion) -> Option<String> {
    if question.timeout_seconds == 0 {
        return None;
    }
    Some(
        (question.asked_at + chrono::Duration::seconds(question.timeout_seconds as i64))
            .to_rfc3339(),
    )
}

fn terminal_feedback_status_supported(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled
    )
}

fn terminal_transition_key(status: RunStatus, progress: Option<&RunProgress>) -> String {
    let phase_started_at = progress
        .filter(|progress| progress.phase == status.as_str())
        .map(|progress| progress.phase_started_at.to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    format!("terminal:{}:{}", status.as_str(), phase_started_at)
}

fn terminal_handoff_evidence(final_report_path: &Path) -> (String, String) {
    let Ok(summary) = artifact::read_json::<ImplementationSummary>(final_report_path) else {
        return (String::new(), String::new());
    };
    (summary.final_sha, summary.exit_states.handoff)
}

fn terminal_feedback_payload(
    run: &Run,
    status: RunStatus,
    summary: &Value,
    summary_path: &Path,
    final_report_path: Option<&Path>,
    implementation_summary_path: Option<&Path>,
) -> Value {
    let (final_sha, handoff_readiness) = final_report_path
        .map(terminal_handoff_evidence)
        .unwrap_or_default();
    let evidence_artifacts = [
        Some(summary_path),
        final_report_path,
        implementation_summary_path,
    ]
    .into_iter()
    .flatten()
    .map(|path| path.to_string_lossy().to_string())
    .collect::<Vec<_>>();
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
        "run_id": run.id,
        "terminal_status": status.as_str(),
        "repo_path": run.repo_path,
        "integration_branch": run.integration_branch,
        "selected_slice_id": run.selected_slice_id,
        "message": feed_summary,
        "feed_summary_line": feed_summary,
        "feed": feed,
        "primary_failure": summary.get("primary_failure").and_then(Value::as_str).unwrap_or_default(),
        "cancel_reason": summary.get("cancel_reason").and_then(Value::as_str).unwrap_or_default(),
        "final_sha": final_sha,
        "handoff_readiness": handoff_readiness,
        "evidence_artifacts": evidence_artifacts,
        "next_commands": summary.get("next_commands").cloned().unwrap_or_else(|| json!([])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Store as ArtifactStore;
    use crate::domain::{
        OriginNotificationTarget, ReplanEvidenceLink, ReplanProposalSource, ReplanProposedChange,
    };
    use anyhow::Result;
    use std::fs;
    use std::time::Duration;

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
    fn attention_no_origin_noops_for_worker_question() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let (_home, state) = state_store()?;
        let run = run_fixture(repo.path(), "kd-attention-no-origin");
        state.insert_run(&run)?;
        let question = state.insert_worker_question(
            "q-no-origin",
            &run.id,
            "slice-001",
            1,
            "choose?",
            &["a".to_string(), "b".to_string()],
            0,
        )?;

        OperatorAttention::new(state.clone()).worker_question_pending(WorkerQuestionPending {
            question: &question,
        });

        assert!(state.get_events(&run.id, 100)?.is_empty());
        Ok(())
    }

    #[test]
    fn attention_delivery_failures_are_warning_incidents_for_worker_question_and_replan()
    -> Result<()> {
        let repo = tempfile::tempdir()?;
        let artifact_store = ArtifactStore::new(repo.path());
        let (_home, state) = state_store()?;
        let run = run_fixture(repo.path(), "kd-attention-failures");
        state.insert_run(&run)?;
        artifact_store.write_origin_notification_target(&run.id, &origin())?;
        let question = state.insert_worker_question(
            "q-delivery-failed",
            &run.id,
            "slice-001",
            1,
            "choose?",
            &["a".to_string()],
            30,
        )?;
        let proposal = state.create_replan_proposal(
            &run.id,
            "rp-delivery-failed",
            ReplanProposalSource {
                kind: "worker_finding".to_string(),
                slice_id: "slice-001".to_string(),
                phase: "slice_worker".to_string(),
                attempt: 1,
                summary: "needs operator review".to_string(),
            },
            vec!["finding-1".to_string()],
            vec![ReplanEvidenceLink {
                kind: "worker_output".to_string(),
                path: "output.json".to_string(),
                event_id: 0,
                summary: "evidence".to_string(),
            }],
            vec![ReplanProposedChange {
                kind: "follow_up_or_revision".to_string(),
                target: "slice-001".to_string(),
                summary: "revise scope".to_string(),
            }],
            "operator_review_required",
        )?;
        let attention = OperatorAttention::new(state.clone());

        attention.worker_question_pending(WorkerQuestionPending {
            question: &question,
        });
        attention.replan_decision_pending(ReplanDecisionPending {
            run: &run,
            proposal: &proposal,
        });

        let events = state.get_events(&run.id, 100)?;
        assert!(events.iter().any(|event| {
            event.typ == "run_incident"
                && event.payload["kind"] == "attention_notification_failed"
                && event.payload["question_id"] == "q-delivery-failed"
                && event.payload["source_of_truth"] == "daemon_worker_questions"
        }));
        assert!(events.iter().any(|event| {
            event.typ == "run_incident"
                && event.payload["kind"] == "attention_focus_failed"
                && event.payload["proposal_id"] == "rp-delivery-failed"
                && event.payload["source_of_truth"] == "daemon_replan_proposals"
        }));
        assert_eq!(
            state.get_run(&run.id)?.expect("run").status,
            RunStatus::Running
        );
        Ok(())
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
                payload: json!({"pane_id": "pane-1", "slice_id": "slice-001"}),
                created_at: now,
            },
            Event {
                id: 2,
                run_id: run.id.clone(),
                typ: "cockpit_worker_ready".to_string(),
                payload: json!({"pane_id": "pane-1", "slice_id": "slice-001"}),
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
            .count();
        assert_eq!(rename_incidents, 1);
        Ok(())
    }

    #[test]
    fn worker_question_deadline_uses_timeout_seconds() {
        let now = Utc::now();
        let question = WorkerQuestion {
            id: "q-deadline".to_string(),
            run_id: "run".to_string(),
            slice_id: "slice".to_string(),
            attempt: 1,
            question: "choose".to_string(),
            options: Vec::new(),
            timeout_seconds: 5,
            state: "pending".to_string(),
            asked_at: now,
            answered_at: None,
            answer: String::new(),
        };

        assert_eq!(
            worker_question_deadline(&question),
            Some((now + chrono::Duration::from_std(Duration::from_secs(5)).unwrap()).to_rfc3339())
        );
    }
}
