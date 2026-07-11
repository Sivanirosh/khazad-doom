use super::cockpit::{rename_default_agent_target, send_default_agent_message};
use super::events as workflow_events;
use crate::artifact;
use crate::domain::{
    Event, Run, RunProgress, RunStatus, SliceRun, SliceStatus, TerminalNotificationRecord,
    WorkerQuestion,
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
            let label = match event.launch_id.filter(|launch_id| *launch_id > 0) {
                Some(launch_id) => format!("{marker} {slice_id} launch {launch_id} {status}"),
                None => format!("{marker} {slice_id} {status}"),
            };
            match rename_default_agent_target(pane_id, &label) {
                Ok(renamed) => {
                    let _ = self.state.record_event(
                        &intent.run.id,
                        "cockpit_worker_renamed",
                        &json!({
                            "pane_id": pane_id,
                            "slice_id": slice_id,
                            "launch_id": event.launch_id,
                            "launch_stem": event.launch_stem,
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
                        .with_extra("launch_id", event.launch_id)
                        .with_extra("launch_stem", &event.launch_stem)
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
