use crate::domain::{
    AutonomyLevel, FrontierBudgetState, FrontierClassification, GateResult, RepairResult,
    ReplanDecision, ReplanProposalSource, ReplanProposalState, ReplanProposedChange, Run,
    WorkerProfileEvidence, WorkerQuestion, WorkflowExitStates,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) const ATTENTION_FOCUS_SENT: &str = "attention_focus_sent";
pub(crate) const ATTENTION_NOTIFICATION_SENT: &str = "attention_notification_sent";
pub(crate) const CHECKPOINT_WRITTEN: &str = "checkpoint_written";
pub(crate) const COCKPIT_READY: &str = "cockpit_ready";
pub(crate) const COCKPIT_WORKER_READY: &str = "cockpit_worker_ready";
pub(crate) const FRONTIER_AUTO_ACCEPT_RECORDED: &str = "frontier_auto_accept_recorded";
pub(crate) const FRONTIER_CLASSIFIED: &str = "frontier_classified";
pub(crate) const IMPLEMENTATION_SUMMARY: &str = "implementation_summary";
pub(crate) const INTEGRATION_REPAIR_COMPLETED: &str = "integration_repair_completed";
pub(crate) const PARALLEL_LAYER_COMPLETED: &str = "parallel_layer_completed";
pub(crate) const PARALLEL_LAYER_FAILED: &str = "parallel_layer_failed";
pub(crate) const PARALLEL_LAYER_STARTED: &str = "parallel_layer_started";
pub(crate) const RUN_CANCEL_REQUESTED: &str = "run_cancel_requested";
pub(crate) const RUN_CANCELLED: &str = "run_cancelled";
pub(crate) const RUN_COMPLETED: &str = "run_completed";
pub(crate) const RUN_ERROR: &str = "run_error";
pub(crate) const RUN_INCIDENT: &str = "run_incident";
pub(crate) const RUN_STARTED: &str = "run_started";
pub(crate) const REPLAN_PROPOSAL_DECIDED: &str = "replan_proposal_decided";
pub(crate) const SLICE_MERGED: &str = "slice_merged";
pub(crate) const SLICE_STARTED: &str = "slice_started";
pub(crate) const TERMINAL_NOTIFICATION_SENT: &str = "terminal_notification_sent";
pub(crate) const TERMINAL_NOTIFICATION_SKIPPED: &str = "terminal_notification_skipped";
pub(crate) const TERMINAL_SUMMARY_WRITTEN: &str = "terminal_summary_written";
pub(crate) const WORKER_ATTEMPT_FAILURE: &str = "worker_attempt_failure";
pub(crate) const WORKER_ENVELOPE_RETRY_SUCCEEDED: &str = "worker_envelope_retry_succeeded";
pub(crate) const WORKER_QUESTION_ANSWERED: &str = "worker_question_answered";
pub(crate) const WORKER_QUESTION_ASKED: &str = "worker_question_asked";
pub(crate) const WORKTREES_CLEANED: &str = "worktrees_cleaned";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunStartedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_slices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_closed_slices: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify_profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_mode: String,
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_evidence_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_evidence_label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_summary: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profile_source_attribution: BTreeMap<String, String>,
}

impl RunStartedPayload {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run: &Run,
        selected_slices: Vec<String>,
        skipped_closed_slices: Vec<String>,
        verify_profile: String,
        verify_profiles: Vec<String>,
        agent: impl Into<String>,
        agent_profile: impl Into<String>,
        agent_provider: impl Into<String>,
        agent_model: impl Into<String>,
        agent_reasoning: impl Into<String>,
        agent_mode: impl Into<String>,
        worker_profile: WorkerProfileEvidence,
        profile_summary: impl Into<String>,
        launch_summary: impl Into<String>,
        profile_source_attribution: BTreeMap<String, String>,
    ) -> Self {
        let worker_evidence_kind = worker_profile.worker_evidence_kind.clone();
        let worker_evidence_label = worker_profile.worker_evidence_label.clone();
        Self {
            run: Some(run.clone()),
            selected_slices,
            skipped_closed_slices,
            verify_profile,
            verify_profiles,
            agent: agent.into(),
            agent_profile: agent_profile.into(),
            agent_provider: agent_provider.into(),
            agent_model: agent_model.into(),
            agent_reasoning: agent_reasoning.into(),
            agent_mode: agent_mode.into(),
            worker_profile,
            worker_evidence_kind,
            worker_evidence_label,
            profile_summary: profile_summary.into(),
            launch_summary: launch_summary.into(),
            profile_source_attribution,
        }
    }

    pub(crate) fn from_value(value: &Value) -> Self {
        decode_or_default(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunIncidentPayload {
    #[serde(default = "default_warning", skip_serializing_if = "String::is_empty")]
    pub severity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution_owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_action_required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fix_commands: Vec<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl RunIncidentPayload {
    pub(crate) fn warning(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: "warning".to_string(),
            kind: kind.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub(crate) fn error(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: "error".to_string(),
            kind: kind.into(),
            failure_kind: String::new(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub(crate) fn with_extra<T: Serialize>(mut self, key: impl Into<String>, value: T) -> Self {
        self.extra.insert(
            key.into(),
            serde_json::to_value(value).unwrap_or(Value::Null),
        );
        self
    }

    pub(crate) fn with_severity(mut self, value: impl Into<String>) -> Self {
        self.severity = value.into();
        self
    }

    pub(crate) fn with_failure_kind(mut self, value: impl Into<String>) -> Self {
        self.failure_kind = value.into();
        self
    }

    pub(crate) fn with_operator_action_required(mut self, value: bool) -> Self {
        self.operator_action_required = Some(value);
        self
    }

    pub(crate) fn with_retryable(mut self, value: bool) -> Self {
        self.retryable = Some(value);
        self
    }

    pub(crate) fn with_fix_commands(mut self, value: Vec<String>) -> Self {
        self.fix_commands = value;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunErrorPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl RunErrorPayload {
    pub(crate) fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            extra: BTreeMap::new(),
        }
    }

    pub(crate) fn from_value(value: &Value) -> Self {
        let mut payload: Self = decode_or_else(value, || Self::legacy(value));
        if payload.error.trim().is_empty() {
            payload.error = payload
                .extra
                .get("message")
                .and_then(value_to_text)
                .or_else(|| payload.extra.get("summary").and_then(value_to_text))
                .unwrap_or_default();
        }
        payload
    }

    fn legacy(value: &Value) -> Self {
        Self {
            error: text_field(value, &["error", "message", "summary"]).unwrap_or_default(),
            extra: object_extra(value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunCancelRequestedPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub active: bool,
}

impl RunCancelRequestedPayload {
    pub(crate) fn new(reason: impl Into<String>, active: bool) -> Self {
        Self {
            reason: reason.into(),
            active,
        }
    }

    pub(crate) fn from_value(value: &Value) -> Self {
        decode_or_else(value, || Self {
            reason: text_field(value, &["reason"]).unwrap_or_default(),
            active: bool_field(value, "active").unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunCancelledPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl RunCancelledPayload {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunCompletedPayload {
    pub run_id: String,
}

impl RunCompletedPayload {
    pub(crate) fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct CockpitReadyPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(
        default,
        alias = "workspace_label",
        skip_serializing_if = "String::is_empty"
    )]
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub planner: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct CockpitWorkerReadyPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(
        default,
        alias = "workspace_label",
        skip_serializing_if = "String::is_empty"
    )]
    pub workspace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_stem: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_of_truth: String,
}

impl CockpitWorkerReadyPayload {
    pub(crate) fn from_value(value: &Value) -> Self {
        let mut payload: Self = decode_or_else(value, || Self {
            adapter: text_field(value, &["adapter"]).unwrap_or_else(|| "cockpit".to_string()),
            mode: text_field(value, &["mode"]).unwrap_or_default(),
            workspace: text_field(value, &["workspace", "workspace_label"]).unwrap_or_default(),
            pane: text_field(value, &["pane"]).unwrap_or_default(),
            pane_id: text_field(value, &["pane_id"]).unwrap_or_default(),
            slice_id: text_field(value, &["slice_id"]).unwrap_or_else(|| "slice".to_string()),
            attempt: usize_field(value, "attempt"),
            launch_id: value.get("launch_id").and_then(Value::as_i64),
            launch_stem: text_field(value, &["launch_stem"]).unwrap_or_default(),
            source_of_truth: text_field(value, &["source_of_truth"]).unwrap_or_default(),
        });
        if payload.adapter.trim().is_empty() {
            payload.adapter = "cockpit".to_string();
        }
        if payload.slice_id.trim().is_empty() {
            payload.slice_id = "slice".to_string();
        }
        payload
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SliceStartedPayload {
    pub slice_id: String,
}

impl SliceStartedPayload {
    pub(crate) fn new(slice_id: impl Into<String>) -> Self {
        Self {
            slice_id: slice_id.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SliceMergedPayload {
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_sha: String,
}

impl SliceMergedPayload {
    pub(crate) fn new(slice_id: impl Into<String>, commit_sha: impl Into<String>) -> Self {
        Self {
            slice_id: slice_id.into(),
            commit_sha: commit_sha.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct IntegrationRepairCompletedPayload {
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
}

impl IntegrationRepairCompletedPayload {
    pub(crate) fn new(
        status: impl Into<String>,
        summary: impl Into<String>,
        launch_id: i64,
    ) -> Self {
        Self {
            status: status.into(),
            summary: summary.into(),
            launch_id: Some(launch_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct TerminalSummaryWrittenPayload {
    pub path: String,
}

impl TerminalSummaryWrittenPayload {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path_to_string(path),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct TerminalNotificationPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transition_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_kind: String,
}

impl TerminalNotificationPayload {
    pub(crate) fn sent(
        terminal_status: impl Into<String>,
        transition_key: impl Into<String>,
        adapter: impl Into<String>,
        surface: impl Into<String>,
        target_kind: impl Into<String>,
    ) -> Self {
        Self {
            status: terminal_status.into(),
            terminal_status: String::new(),
            transition_key: transition_key.into(),
            reason: String::new(),
            adapter: adapter.into(),
            surface: surface.into(),
            target_kind: target_kind.into(),
        }
    }

    pub(crate) fn skipped(
        terminal_status: impl Into<String>,
        transition_key: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status: terminal_status.into(),
            terminal_status: String::new(),
            transition_key: transition_key.into(),
            reason: reason.into(),
            adapter: String::new(),
            surface: String::new(),
            target_kind: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptTimeoutPayload {
    pub phase: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    pub message: String,
}

impl WorkerAttemptTimeoutPayload {
    pub(crate) fn new(
        phase: impl Into<String>,
        slice_id: impl Into<String>,
        attempt: usize,
        launch_id: Option<i64>,
        timeout_seconds: u64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            phase: phase.into(),
            slice_id: slice_id.into(),
            attempt,
            launch_id,
            timeout_seconds,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptFailurePayload {
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub envelope_retry: usize,
    pub phase: String,
    pub failure_kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence_path: String,
    pub retry_disposition: String,
    pub repair_disposition: String,
    #[serde(default)]
    pub primary_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secondary_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerErrorPayload {
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    pub error: String,
    #[serde(default)]
    pub primary_failure: Option<String>,
    #[serde(default)]
    pub secondary_failures: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_kind: String,
    #[serde(default)]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub operator_action_required: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerQuestionAskedPayload {
    pub question_id: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommended_answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommendation_rationale: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bounded_within_current_slice_or_mission_authority: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reversible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fallback_eligible: bool,
    pub answer_command: String,
}

impl WorkerQuestionAskedPayload {
    pub(crate) fn from_question(question: &WorkerQuestion, deadline_at: Option<String>) -> Self {
        Self {
            question_id: question.id.clone(),
            slice_id: question.slice_id.clone(),
            attempt: question.attempt,
            launch_id: question.launch_id,
            question: question.question.clone(),
            options: question.options.clone(),
            timeout_seconds: question.timeout_seconds,
            deadline_at,
            recommended_answer: question.recommended_answer.clone(),
            recommendation_rationale: question.recommendation_rationale.clone(),
            bounded_within_current_slice_or_mission_authority: question
                .bounded_within_current_slice_or_mission_authority,
            reversible: question.reversible,
            fallback_eligible: question.fallback_eligible,
            answer_command: format!(
                "khazad-doom answer {} {} <answer>",
                question.run_id, question.id
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerQuestionAnsweredPayload {
    pub question_id: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer_source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommended_answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommendation_rationale: String,
}

impl WorkerQuestionAnsweredPayload {
    pub(crate) fn from_question(
        question: &WorkerQuestion,
        answer: impl Into<String>,
        answer_source: crate::domain::WorkerQuestionAnswerSource,
    ) -> Self {
        Self {
            question_id: question.id.clone(),
            slice_id: question.slice_id.clone(),
            launch_id: question.launch_id,
            answer: answer.into(),
            answer_source: answer_source.as_str().to_string(),
            recommended_answer: question.recommended_answer.clone(),
            recommendation_rationale: question.recommendation_rationale.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AttentionDeliveryPayload {
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposal_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParallelLayerPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

impl ParallelLayerPayload {
    pub(crate) fn started(slices: Vec<String>) -> Self {
        Self {
            slices,
            outcomes: Vec::new(),
            summary: String::new(),
        }
    }

    pub(crate) fn completed(slices: Vec<String>, outcomes: Vec<Value>) -> Self {
        Self {
            slices,
            outcomes,
            summary: String::new(),
        }
    }

    pub(crate) fn failed(
        slices: Vec<String>,
        outcomes: Vec<Value>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            slices,
            outcomes,
            summary: summary.into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ImplementationSummaryPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_slices: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_gate: Option<GateResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_repair_integration_gate: Option<GateResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_repair: Option<RepairResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_states: Option<WorkflowExitStates>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_sha: String,
}

impl ImplementationSummaryPayload {
    pub(crate) fn from_value(value: &Value) -> Self {
        decode_or_else(value, || Self {
            completed_slices: value
                .get("completed_slices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            integration_gate: object_field(value, "integration_gate"),
            pre_repair_integration_gate: object_field(value, "pre_repair_integration_gate"),
            integration_repair: object_field(value, "integration_repair"),
            exit_states: object_field(value, "exit_states"),
            verify_profile: text_field(value, &["verify_profile"]).unwrap_or_default(),
            final_sha: text_field(value, &["final_sha"]).unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FrontierClassifiedPayload {
    pub proposal_id: String,
    pub checkpoint: String,
    pub tier: String,
    pub reason_codes: Vec<String>,
    pub classified_at: chrono::DateTime<chrono::Utc>,
    pub envelope_hash: String,
    pub budget_snapshot: FrontierBudgetState,
    pub autonomy_level: AutonomyLevel,
    pub record_only: bool,
    pub queue_mutated: bool,
    pub slice_mutated: bool,
    pub decision_recorded: bool,
}

impl FrontierClassifiedPayload {
    pub(crate) fn new(
        proposal_id: impl Into<String>,
        checkpoint: impl Into<String>,
        classification: &FrontierClassification,
        record_only: bool,
        decision_recorded: bool,
    ) -> Self {
        Self {
            proposal_id: proposal_id.into(),
            checkpoint: checkpoint.into(),
            tier: classification.tier.clone(),
            reason_codes: classification.reason_codes.clone(),
            classified_at: classification.classified_at,
            envelope_hash: classification.envelope_hash.clone(),
            budget_snapshot: classification.budget_snapshot.clone(),
            autonomy_level: classification.autonomy_level,
            record_only,
            queue_mutated: false,
            slice_mutated: false,
            decision_recorded,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FrontierAutoAcceptRecordedPayload {
    pub proposal_id: String,
    pub checkpoint: String,
    pub authorizer: String,
    pub source: String,
    pub rationale: String,
    pub tier: String,
    pub reason_codes: Vec<String>,
    pub budget_before: FrontierBudgetState,
    pub budget_after: FrontierBudgetState,
    pub af00_evidence_gate: String,
    pub apply_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplanProposalDecidedPayload {
    pub proposal_id: String,
    pub state: ReplanProposalState,
    pub decision: ReplanDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ReplanCheckpointBlockedPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposal_ids: Vec<String>,
    pub checkpoint: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_commands: Vec<String>,
}

impl ReplanCheckpointBlockedPayload {
    pub(crate) fn new(
        proposal_ids: Vec<String>,
        checkpoint: impl Into<String>,
        message: impl Into<String>,
        decision_commands: Vec<String>,
    ) -> Self {
        Self {
            proposal_ids,
            checkpoint: checkpoint.into(),
            message: message.into(),
            decision_commands,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ReplanNotificationPayload {
    pub schema_version: u64,
    pub kind: String,
    pub run_id: String,
    pub proposal_id: String,
    pub source: ReplanProposalSource,
    pub risk: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_changes: Vec<ReplanProposedChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_commands: Vec<String>,
    pub source_of_truth: String,
}

impl ReplanNotificationPayload {
    pub(crate) fn new(
        run_id: impl Into<String>,
        proposal_id: impl Into<String>,
        source: ReplanProposalSource,
        risk: impl Into<String>,
        proposed_changes: Vec<ReplanProposedChange>,
        decision_commands: Vec<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            kind: "replan_decision_pending".to_string(),
            run_id: run_id.into(),
            proposal_id: proposal_id.into(),
            source,
            risk: risk.into(),
            proposed_changes,
            decision_commands,
            source_of_truth: "daemon_replan_proposals".to_string(),
        }
    }
}

pub(crate) fn path_to_string(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().to_string()
}

fn decode_or_default<T>(value: &Value) -> T
where
    T: DeserializeOwned + Default,
{
    decode_or_else(value, T::default)
}

fn decode_or_else<T, F>(value: &Value, fallback: F) -> T
where
    T: DeserializeOwned,
    F: FnOnce() -> T,
{
    serde_json::from_value(value.clone()).unwrap_or_else(|_| fallback())
}

fn object_field<T>(value: &Value, key: &str) -> Option<T>
where
    T: DeserializeOwned,
{
    value
        .get(key)
        .and_then(|field| serde_json::from_value(field.clone()).ok())
}

fn text_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(value_to_text))
        .find(|text| !text.trim().is_empty())
}

fn value_to_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.trim().to_string());
    }
    if let Some(number) = value.as_i64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_u64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_f64() {
        return Some(number.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Some(flag.to_string());
    }
    None
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn usize_field(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or_default()
}
fn object_extra(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}
fn default_warning() -> String {
    "warning".to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}
