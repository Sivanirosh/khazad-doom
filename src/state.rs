use crate::domain::{
    DecisionCommandOutcome, Event, FrontierBudgetState, FrontierClassification,
    IntegrationMergeCompletion, IntegrationMergeIntent, IntegrationMergeKind,
    IntegrationMergeState, MissionEnvelope, ReplanDecision, ReplanEvidenceLink, ReplanProposal,
    ReplanProposalSource, ReplanProposalState, ReplanProposedChange, Run, RunLaunchAction,
    RunLaunchIntent, RunLaunchState, RunProgress, RunStatus, SliceRun, SliceStatus,
    StatusSnapshotRevision, WorkerAttemptLedger, WorkerAttemptProgress, WorkerQuestion,
    WorkerQuestionAnswerSource, WorkerQuestionRecommendation,
};
use crate::pi_contract;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Repo {
    pub id: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct StatusSourceSnapshot {
    pub(crate) source: String,
    pub(crate) payload: serde_json::Value,
    pub(crate) indexed_event_id: i64,
    pub(crate) content_sha256: String,
    pub(crate) observed_at: DateTime<Utc>,
}

/// Every authoritative input used by the status projection, captured from one
/// SQLite read transaction. `events` is complete semantic history;
/// `event_tail` is the independently bounded response payload.
#[derive(Debug, Clone)]
pub(crate) struct RunStateSnapshot {
    pub(crate) revision: StatusSnapshotRevision,
    pub(crate) run: Run,
    pub(crate) slice_runs: Vec<SliceRun>,
    pub(crate) worker_attempts: Vec<WorkerAttemptLedger>,
    pub(crate) progress: Option<RunProgress>,
    pub(crate) questions: Vec<WorkerQuestion>,
    pub(crate) replan_proposals: Vec<ReplanProposal>,
    pub(crate) mission_envelope: Option<MissionEnvelope>,
    pub(crate) frontier_budget: Option<FrontierBudgetState>,
    pub(crate) events: Vec<Event>,
    pub(crate) event_tail: Vec<Event>,
    pub(crate) terminal_transition: Option<TerminalTransition>,
    pub(crate) launch_intents: Vec<RunLaunchIntent>,
    pub(crate) merge_intents: Vec<IntegrationMergeIntent>,
    pub(crate) status_sources: Vec<StatusSourceSnapshot>,
}

enum StatusSnapshotSelector<'a> {
    RunId(&'a str),
    LatestRepo {
        repo_path: &'a str,
        active_only: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunAdmissionOutcome {
    Prepared,
    Conflict,
}

#[derive(Debug, Clone)]
pub(crate) struct RunAdmissionTransition {
    pub(crate) outcome: RunAdmissionOutcome,
    pub(crate) intent: Option<RunLaunchIntent>,
    pub(crate) active_run: Option<Run>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationMergePrepareOutcome {
    Prepared,
    AlreadyPrepared,
    AlreadyApplied,
    Conflict,
}

#[derive(Debug, Clone)]
pub(crate) struct IntegrationMergePrepareTransition {
    pub(crate) outcome: IntegrationMergePrepareOutcome,
    pub(crate) intent: IntegrationMergeIntent,
}

#[derive(Debug, Clone)]
pub(crate) enum WorkerQuestionDecisionCommand {
    Answer {
        answer: String,
        answer_source: WorkerQuestionAnswerSource,
        progress_message: String,
    },
    Timeout {
        expected_launch_id: Option<i64>,
        apply_recommendation_at_deadline: bool,
        incident_code: String,
        message_prefix: String,
        progress_message: String,
    },
}

impl WorkerQuestionDecisionCommand {
    pub(crate) fn answer(
        answer: impl Into<String>,
        answer_source: WorkerQuestionAnswerSource,
        progress_message: impl Into<String>,
    ) -> Self {
        Self::Answer {
            answer: answer.into(),
            answer_source,
            progress_message: progress_message.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn timeout(
        incident_code: impl Into<String>,
        message_prefix: impl Into<String>,
        progress_message: impl Into<String>,
    ) -> Self {
        Self::Timeout {
            expected_launch_id: None,
            apply_recommendation_at_deadline: false,
            incident_code: incident_code.into(),
            message_prefix: message_prefix.into(),
            progress_message: progress_message.into(),
        }
    }

    pub(crate) fn resolve_timeout(
        expected_launch_id: Option<i64>,
        incident_code: impl Into<String>,
        message_prefix: impl Into<String>,
        progress_message: impl Into<String>,
    ) -> Self {
        Self::Timeout {
            expected_launch_id,
            apply_recommendation_at_deadline: true,
            incident_code: incident_code.into(),
            message_prefix: message_prefix.into(),
            progress_message: progress_message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerQuestionDecisionTransition {
    pub outcome: DecisionCommandOutcome,
    pub question: Option<WorkerQuestion>,
}

#[derive(Debug, Clone)]
struct ReplanAutoAcceptCommand {
    classification: FrontierClassification,
    budget_before: FrontierBudgetState,
    budget_after: FrontierBudgetState,
    record: Option<FrontierAutoAcceptRecord>,
}

#[derive(Debug, Clone)]
struct FrontierAutoAcceptRecord {
    checkpoint: String,
    apply_mode: String,
}

impl FrontierAutoAcceptRecord {
    fn classification_payload(
        &self,
        proposal: &ReplanProposal,
        classification: &FrontierClassification,
    ) -> crate::workflow::events::FrontierClassifiedPayload {
        crate::workflow::events::FrontierClassifiedPayload::new(
            &proposal.id,
            &self.checkpoint,
            classification,
            false,
            true,
        )
    }

    fn payload(
        &self,
        proposal: &ReplanProposal,
        decision: &ReplanDecision,
        auto_accept: &ReplanAutoAcceptCommand,
    ) -> crate::workflow::events::FrontierAutoAcceptRecordedPayload {
        crate::workflow::events::FrontierAutoAcceptRecordedPayload {
            proposal_id: proposal.id.clone(),
            checkpoint: self.checkpoint.clone(),
            authorizer: decision.authorizer.clone(),
            source: decision.source.clone(),
            rationale: decision.rationale.clone(),
            tier: auto_accept.classification.tier.clone(),
            reason_codes: auto_accept.classification.reason_codes.clone(),
            budget_before: auto_accept.budget_before.clone(),
            budget_after: auto_accept.budget_after.clone(),
            af00_evidence_gate: "satisfied".to_string(),
            apply_mode: self.apply_mode.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReplanDecisionCommand {
    state: ReplanProposalState,
    rationale: String,
    authorizer: String,
    source: String,
    replacement_id: String,
    revisit_condition: String,
    auto_accept: Option<ReplanAutoAcceptCommand>,
}

impl ReplanDecisionCommand {
    pub(crate) fn operator(
        state: ReplanProposalState,
        rationale: impl Into<String>,
        authorizer: impl Into<String>,
        source: impl Into<String>,
        replacement_id: impl Into<String>,
        revisit_condition: impl Into<String>,
    ) -> Self {
        Self {
            state,
            rationale: rationale.into(),
            authorizer: authorizer.into(),
            source: source.into(),
            replacement_id: replacement_id.into(),
            revisit_condition: revisit_condition.into(),
            auto_accept: None,
        }
    }

    pub(crate) fn timeout(
        rationale: impl Into<String>,
        revisit_condition: impl Into<String>,
    ) -> Self {
        let rationale = rationale.into();
        let revisit_condition = revisit_condition.into();
        Self::operator(
            ReplanProposalState::Deferred,
            if rationale.trim().is_empty() {
                "replan decision timed out".to_string()
            } else {
                rationale
            },
            "daemon",
            "decision_timeout",
            "",
            if revisit_condition.trim().is_empty() {
                "operator explicitly revisits the timed-out proposal".to_string()
            } else {
                revisit_condition
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn auto_accept(
        rationale: impl Into<String>,
        classification: FrontierClassification,
        budget_before: FrontierBudgetState,
        budget_after: FrontierBudgetState,
    ) -> Self {
        Self {
            state: ReplanProposalState::Accepted,
            rationale: rationale.into(),
            authorizer: String::new(),
            source: "frontier_policy".to_string(),
            replacement_id: String::new(),
            revisit_condition: String::new(),
            auto_accept: Some(ReplanAutoAcceptCommand {
                classification,
                budget_before,
                budget_after,
                record: None,
            }),
        }
    }

    fn auto_accept_recorded(
        rationale: impl Into<String>,
        classification: FrontierClassification,
        budget_before: FrontierBudgetState,
        budget_after: FrontierBudgetState,
        checkpoint: impl Into<String>,
        apply_mode: impl Into<String>,
    ) -> Self {
        Self {
            state: ReplanProposalState::Accepted,
            rationale: rationale.into(),
            authorizer: String::new(),
            source: "frontier_policy".to_string(),
            replacement_id: String::new(),
            revisit_condition: String::new(),
            auto_accept: Some(ReplanAutoAcceptCommand {
                classification,
                budget_before,
                budget_after,
                record: Some(FrontierAutoAcceptRecord {
                    checkpoint: checkpoint.into(),
                    apply_mode: apply_mode.into(),
                }),
            }),
        }
    }

    fn matches(&self, proposal: &ReplanProposal) -> bool {
        let authorizer = if self.authorizer.trim().is_empty() {
            "operator"
        } else {
            self.authorizer.trim()
        };
        let source = if self.source.trim().is_empty() {
            "daemon_ipc"
        } else {
            self.source.trim()
        };
        proposal.state == self.state
            && proposal.operator_decision.as_ref().is_some_and(|decision| {
                let base_matches = decision.decision == self.state.as_str()
                    && decision.rationale == self.rationale.trim()
                    && decision.source == source
                    && decision.replacement_id == self.replacement_id.trim()
                    && decision.revisit_condition == self.revisit_condition.trim();
                if let Some(auto_accept) = &self.auto_accept {
                    base_matches
                        && decision.authorizer == format!("envelope:{}", proposal.run_id)
                        && proposal.frontier_classification.as_ref()
                            == Some(&auto_accept.classification)
                        && decision.frontier_tier == auto_accept.classification.tier
                        && decision.frontier_reason_codes == auto_accept.classification.reason_codes
                        && decision.frontier_budget_before.as_ref()
                            == Some(&auto_accept.budget_before)
                        && decision.frontier_budget_after.as_ref()
                            == Some(&auto_accept.budget_after)
                } else {
                    base_matches && decision.authorizer == authorizer
                }
            })
    }

    fn supplemental_record_matches(
        &self,
        conn: &Connection,
        proposal: &ReplanProposal,
    ) -> Result<bool> {
        let Some((auto_accept, record)) = self.auto_accept.as_ref().and_then(|auto_accept| {
            auto_accept
                .record
                .as_ref()
                .map(|record| (auto_accept, record))
        }) else {
            return Ok(true);
        };
        let Some(decision) = proposal.operator_decision.as_ref() else {
            return Ok(false);
        };
        let expected_classification =
            record.classification_payload(proposal, &auto_accept.classification);
        let expected_record = record.payload(proposal, decision, auto_accept);
        Ok(exact_event_payload_exists(
            conn,
            &proposal.run_id,
            crate::workflow::events::FRONTIER_CLASSIFIED,
            &expected_classification,
        )? && exact_event_payload_exists(
            conn,
            &proposal.run_id,
            crate::workflow::events::FRONTIER_AUTO_ACCEPT_RECORDED,
            &expected_record,
        )?)
    }
}

fn exact_event_payload_exists<T: Serialize>(
    conn: &Connection,
    run_id: &str,
    event_type: &str,
    expected: &T,
) -> Result<bool> {
    let expected = serde_json::to_value(expected)?;
    let mut statement = conn
        .prepare("SELECT payload_json FROM events WHERE run_id=?1 AND type=?2 ORDER BY id ASC")?;
    let mut rows = statement.query(params![run_id, event_type])?;
    while let Some(row) = rows.next()? {
        let payload_json: String = row.get(0)?;
        if serde_json::from_str::<serde_json::Value>(&payload_json)
            .is_ok_and(|payload| payload == expected)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone)]
pub(crate) struct ReplanDecisionTransition {
    pub outcome: DecisionCommandOutcome,
    pub proposal: Option<ReplanProposal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTransition {
    pub status: RunStatus,
    pub error: String,
    pub progress_message: String,
    pub question_interruption_reason: String,
    pub summary_written: bool,
    pub committed: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecisionTransactionFaultStage {
    BeforeEventAppend,
    BeforeSupplementalEventAppend,
}

#[cfg(test)]
thread_local! {
    static DECISION_TRANSACTION_FAULT:
        std::cell::RefCell<Option<DecisionTransactionFaultStage>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn inject_decision_transaction_fault(stage: DecisionTransactionFaultStage) {
    DECISION_TRANSACTION_FAULT.with(|fault| *fault.borrow_mut() = Some(stage));
}

#[cfg(test)]
fn take_decision_transaction_fault(stage: DecisionTransactionFaultStage) -> bool {
    DECISION_TRANSACTION_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if *fault == Some(stage) {
            *fault = None;
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionTransactionFaultStage {
    BeforePreparedEvent,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum IntegrationMergeTransactionFaultStage {
    PreparedEvent,
    AppliedEvent,
    ResolutionEvent,
}

#[cfg(test)]
thread_local! {
    static ADMISSION_TRANSACTION_FAULT:
        std::cell::RefCell<Option<AdmissionTransactionFaultStage>> =
        const { std::cell::RefCell::new(None) };
    static INTEGRATION_MERGE_TRANSACTION_FAULT:
        std::cell::RefCell<Option<IntegrationMergeTransactionFaultStage>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn inject_admission_transaction_fault(stage: AdmissionTransactionFaultStage) {
    ADMISSION_TRANSACTION_FAULT.with(|fault| *fault.borrow_mut() = Some(stage));
}

#[cfg(test)]
pub(crate) fn inject_integration_merge_transaction_fault(
    stage: IntegrationMergeTransactionFaultStage,
) {
    INTEGRATION_MERGE_TRANSACTION_FAULT.with(|fault| *fault.borrow_mut() = Some(stage));
}

#[cfg(test)]
fn take_admission_transaction_fault(stage: AdmissionTransactionFaultStage) -> bool {
    ADMISSION_TRANSACTION_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if *fault == Some(stage) {
            *fault = None;
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
fn take_integration_merge_transaction_fault(stage: IntegrationMergeTransactionFaultStage) -> bool {
    INTEGRATION_MERGE_TRANSACTION_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if *fault == Some(stage) {
            *fault = None;
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalTransitionFaultStage {
    BeforeTerminalEvent,
}

#[cfg(test)]
thread_local! {
    static TERMINAL_TRANSITION_FAULT: std::cell::RefCell<Option<TerminalTransitionFaultStage>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn inject_terminal_transition_fault(stage: TerminalTransitionFaultStage) {
    TERMINAL_TRANSITION_FAULT.with(|fault| *fault.borrow_mut() = Some(stage));
}

#[cfg(test)]
fn take_terminal_transition_fault(stage: TerminalTransitionFaultStage) -> bool {
    TERMINAL_TRANSITION_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if *fault == Some(stage) {
            *fault = None;
            true
        } else {
            false
        }
    })
}

#[derive(Debug, Clone)]
pub(crate) struct ProgressScope {
    phase: String,
    slice_id: String,
    attempt: usize,
    command: String,
    message: String,
}

impl ProgressScope {
    pub(crate) fn new(
        phase: impl Into<String>,
        slice_id: impl Into<String>,
        attempt: usize,
        command: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            phase: phase.into(),
            slice_id: slice_id.into(),
            attempt,
            command: command.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProgressReporter {
    store: Store,
    run_id: String,
}

impl ProgressReporter {
    pub(crate) fn new(store: Store, run_id: impl Into<String>) -> Self {
        Self {
            store,
            run_id: run_id.into(),
        }
    }

    pub(crate) fn mark(&self, scope: &ProgressScope) {
        let _ = self.persist(scope, "", true);
    }

    pub(crate) fn update_output_tail(&self, scope: &ProgressScope, output_tail: &str) {
        let _ = self.persist(scope, output_tail, false);
    }

    fn persist(
        &self,
        scope: &ProgressScope,
        output_tail: &str,
        record_event: bool,
    ) -> Result<RunProgress> {
        let progress = self.store.update_progress(
            &self.run_id,
            &scope.phase,
            &scope.slice_id,
            scope.attempt,
            &scope.command,
            &scope.message,
            output_tail,
        )?;
        if record_event {
            self.store
                .record_event(&self.run_id, "progress", &progress)?;
        }
        Ok(progress)
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create state directory {}", parent.display()))?;
        }
        let store = Self { path };
        store.init()?;
        Ok(store)
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("open sqlite state {}", self.path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(conn)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS repos (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                repo_id TEXT NOT NULL,
                repo_path TEXT NOT NULL,
                status TEXT NOT NULL,
                base_branch TEXT NOT NULL,
                base_sha TEXT NOT NULL,
                integration_branch TEXT NOT NULL,
                selected_slice_id TEXT NOT NULL,
                error TEXT NOT NULL DEFAULT '',
                execution_epoch INTEGER NOT NULL DEFAULT 1,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TRIGGER IF NOT EXISTS runs_one_active_repo_insert
            BEFORE INSERT ON runs
            WHEN NEW.status IN ('pending', 'running')
            BEGIN
                SELECT RAISE(ABORT, 'active run already exists for canonical repository')
                WHERE EXISTS (
                    SELECT 1 FROM runs
                    WHERE repo_id=NEW.repo_id
                      AND status IN ('pending', 'running')
                      AND id<>NEW.id
                );
            END;
            CREATE TRIGGER IF NOT EXISTS runs_one_active_repo_update
            BEFORE UPDATE OF repo_id, status ON runs
            WHEN NEW.status IN ('pending', 'running')
            BEGIN
                SELECT RAISE(ABORT, 'active run already exists for canonical repository')
                WHERE EXISTS (
                    SELECT 1 FROM runs
                    WHERE repo_id=NEW.repo_id
                      AND status IN ('pending', 'running')
                      AND id<>NEW.id
                );
            END;
            CREATE TABLE IF NOT EXISTS run_launch_intents (
                run_id TEXT NOT NULL,
                execution_epoch INTEGER NOT NULL,
                action TEXT NOT NULL,
                state TEXT NOT NULL,
                repo_id TEXT NOT NULL,
                integration_branch TEXT NOT NULL,
                integration_worktree TEXT NOT NULL,
                integration_resources_owned INTEGER NOT NULL DEFAULT 0,
                prior_status TEXT NOT NULL DEFAULT '',
                prior_error TEXT NOT NULL DEFAULT '',
                primary_cause TEXT NOT NULL DEFAULT '',
                compensation_error TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (run_id, execution_epoch)
            );
            CREATE INDEX IF NOT EXISTS run_launch_intents_state_idx
                ON run_launch_intents (state, updated_at);
            CREATE TABLE IF NOT EXISTS slice_runs (
                run_id TEXT NOT NULL,
                slice_id TEXT NOT NULL,
                status TEXT NOT NULL,
                branch TEXT NOT NULL DEFAULT '',
                commit_sha TEXT NOT NULL DEFAULT '',
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (run_id, slice_id)
            );
            CREATE TABLE IF NOT EXISTS integration_merge_intents (
                operation_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                slice_id TEXT NOT NULL DEFAULT '',
                attempt INTEGER NOT NULL,
                launch_id INTEGER,
                source_branch TEXT NOT NULL,
                source_commit TEXT NOT NULL,
                source_tree TEXT NOT NULL,
                expected_head TEXT NOT NULL,
                expected_result_tree TEXT NOT NULL DEFAULT '',
                resulting_head TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL,
                completion_json TEXT NOT NULL,
                primary_cause TEXT NOT NULL DEFAULT '',
                abort_error TEXT NOT NULL DEFAULT '',
                conflicted_files_json TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS integration_merge_one_prepared_per_run
                ON integration_merge_intents (run_id) WHERE state='prepared';
            CREATE INDEX IF NOT EXISTS integration_merge_intents_run_state_idx
                ON integration_merge_intents (run_id, state, created_at);
            CREATE TABLE IF NOT EXISTS worker_attempt_ledger (
                launch_id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                slice_id TEXT NOT NULL,
                launch_ordinal INTEGER NOT NULL,
                execution_epoch INTEGER NOT NULL,
                worker_retry_ordinal INTEGER NOT NULL,
                repair_ordinal INTEGER NOT NULL,
                envelope_retry_ordinal INTEGER NOT NULL,
                kind TEXT NOT NULL,
                state TEXT NOT NULL,
                branch TEXT NOT NULL,
                worktree TEXT NOT NULL,
                output_stem TEXT NOT NULL,
                worker_token_hash TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                launched_at TEXT NOT NULL DEFAULT '',
                finished_at TEXT NOT NULL DEFAULT '',
                failure_cause TEXT NOT NULL DEFAULT '',
                worker_pid INTEGER,
                worker_process_observed_at TEXT NOT NULL DEFAULT '',
                worker_last_event_at TEXT NOT NULL DEFAULT '',
                worker_last_event_kind TEXT NOT NULL DEFAULT '',
                worker_last_semantic_progress_at TEXT NOT NULL DEFAULT '',
                worker_last_semantic_progress_summary TEXT NOT NULL DEFAULT '',
                worker_attempt_timeout_seconds INTEGER NOT NULL DEFAULT 0,
                worker_no_output_warning_seconds INTEGER NOT NULL DEFAULT 0,
                UNIQUE (run_id, slice_id, launch_ordinal),
                UNIQUE (run_id, output_stem)
            );
            CREATE INDEX IF NOT EXISTS worker_attempt_ledger_run_slice_idx
                ON worker_attempt_ledger (run_id, slice_id, launch_ordinal);
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS terminal_transitions (
                run_id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                error TEXT NOT NULL DEFAULT '',
                progress_message TEXT NOT NULL DEFAULT '',
                question_interruption_reason TEXT NOT NULL DEFAULT '',
                intended_at TEXT NOT NULL,
                summary_written_at TEXT NOT NULL DEFAULT '',
                committed_at TEXT NOT NULL DEFAULT '',
                notification_bookkept_at TEXT NOT NULL DEFAULT '',
                cleanup_started_at TEXT NOT NULL DEFAULT '',
                cleanup_completed_at TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS status_source_snapshots (
                run_id TEXT NOT NULL,
                source TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                indexed_event_id INTEGER NOT NULL DEFAULT 0,
                content_sha256 TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                PRIMARY KEY (run_id, source)
            );
            CREATE TABLE IF NOT EXISTS run_progress (
                run_id TEXT PRIMARY KEY,
                phase TEXT NOT NULL,
                slice_id TEXT NOT NULL DEFAULT '',
                attempt INTEGER NOT NULL DEFAULT 0,
                command TEXT NOT NULL DEFAULT '',
                message TEXT NOT NULL DEFAULT '',
                output_tail TEXT NOT NULL DEFAULT '',
                phase_started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS run_worker_tokens (
                run_id TEXT PRIMARY KEY,
                token_hash TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS worker_questions (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                slice_id TEXT NOT NULL,
                attempt INTEGER NOT NULL DEFAULT 0,
                launch_id INTEGER,
                question TEXT NOT NULL,
                options_json TEXT NOT NULL,
                timeout_seconds INTEGER NOT NULL DEFAULT 0,
                recommended_answer TEXT NOT NULL DEFAULT '',
                recommendation_rationale TEXT NOT NULL DEFAULT '',
                bounded_within_current_slice_or_mission_authority INTEGER NOT NULL DEFAULT 0,
                reversible INTEGER NOT NULL DEFAULT 0,
                fallback_eligible INTEGER NOT NULL DEFAULT 0,
                deadline_at TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL,
                asked_at TEXT NOT NULL,
                answered_at TEXT NOT NULL DEFAULT '',
                answer TEXT NOT NULL DEFAULT '',
                answer_source TEXT NOT NULL DEFAULT '',
                resolution_command_json TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_worker_questions_run_state
                ON worker_questions(run_id, state);
            CREATE TABLE IF NOT EXISTS replan_proposals (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                state TEXT NOT NULL,
                source_json TEXT NOT NULL,
                trigger_finding_ids_json TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                proposed_changes_json TEXT NOT NULL,
                risk TEXT NOT NULL,
                decision_json TEXT NOT NULL DEFAULT '',
                frontier_classification_json TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_replan_proposals_run_state
                ON replan_proposals(run_id, state, created_at);
            "#,
        )?;
        ensure_column(
            &conn,
            "runs",
            "execution_epoch",
            "execution_epoch INTEGER NOT NULL DEFAULT 1",
        )?;
        ensure_column(
            &conn,
            "run_launch_intents",
            "integration_resources_owned",
            "integration_resources_owned INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_attempt_started_at",
            "worker_attempt_started_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(&conn, "run_progress", "worker_pid", "worker_pid INTEGER")?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_process_observed_at",
            "worker_process_observed_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_last_event_at",
            "worker_last_event_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_last_event_kind",
            "worker_last_event_kind TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_last_semantic_progress_at",
            "worker_last_semantic_progress_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_last_semantic_progress_summary",
            "worker_last_semantic_progress_summary TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_attempt_timeout_seconds",
            "worker_attempt_timeout_seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_no_output_warning_seconds",
            "worker_no_output_warning_seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "attempt",
            "attempt INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "worker_questions", "launch_id", "launch_id INTEGER")?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_token_hash",
            "worker_token_hash TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_pid",
            "worker_pid INTEGER",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_process_observed_at",
            "worker_process_observed_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_last_event_at",
            "worker_last_event_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_last_event_kind",
            "worker_last_event_kind TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_last_semantic_progress_at",
            "worker_last_semantic_progress_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_last_semantic_progress_summary",
            "worker_last_semantic_progress_summary TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_attempt_timeout_seconds",
            "worker_attempt_timeout_seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_attempt_ledger",
            "worker_no_output_warning_seconds",
            "worker_no_output_warning_seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "timeout_seconds",
            "timeout_seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "recommended_answer",
            "recommended_answer TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "recommendation_rationale",
            "recommendation_rationale TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "bounded_within_current_slice_or_mission_authority",
            "bounded_within_current_slice_or_mission_authority INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "reversible",
            "reversible INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "fallback_eligible",
            "fallback_eligible INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "deadline_at",
            "deadline_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "answer_source",
            "answer_source TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "worker_questions",
            "resolution_command_json",
            "resolution_command_json TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "runs",
            "mission_envelope_json",
            "mission_envelope_json TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "runs",
            "frontier_budget_json",
            "frontier_budget_json TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "replan_proposals",
            "frontier_classification_json",
            "frontier_classification_json TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "terminal_transitions",
            "notification_bookkept_at",
            "notification_bookkept_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "terminal_transitions",
            "cleanup_started_at",
            "cleanup_started_at TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            &conn,
            "terminal_transitions",
            "cleanup_completed_at",
            "cleanup_completed_at TEXT NOT NULL DEFAULT ''",
        )?;
        Ok(())
    }

    pub fn upsert_repo(&self, repo: &Repo) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            r#"INSERT INTO repos (id, path, created_at) VALUES (?1, ?2, ?3)
               ON CONFLICT(id) DO UPDATE SET path=excluded.path"#,
            params![&repo.id, &repo.path, repo.created_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub(crate) fn admit_run(
        &self,
        run: &Run,
        slice_runs: &[SliceRun],
        mission_envelope: Option<&MissionEnvelope>,
        frontier_budget: Option<&FrontierBudgetState>,
        integration_worktree: &Path,
    ) -> Result<RunAdmissionTransition> {
        if run.status != RunStatus::Pending {
            anyhow::bail!("new run admission requires pending status");
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(active_run) = active_run_for_repo_conn(&tx, &run.repo_id, None)? {
            tx.commit()?;
            return Ok(RunAdmissionTransition {
                outcome: RunAdmissionOutcome::Conflict,
                intent: None,
                active_run: Some(active_run),
            });
        }
        insert_run_tx(&tx, run, mission_envelope, frontier_budget)?;
        for slice_run in slice_runs {
            upsert_slice_run_tx(&tx, slice_run)?;
        }
        let intent = RunLaunchIntent {
            run_id: run.id.clone(),
            execution_epoch: 1,
            action: RunLaunchAction::Start,
            state: RunLaunchState::Prepared,
            repo_id: run.repo_id.clone(),
            integration_branch: run.integration_branch.clone(),
            integration_worktree: integration_worktree.display().to_string(),
            integration_resources_owned: false,
            prior_status: None,
            prior_error: String::new(),
            primary_cause: String::new(),
            compensation_error: String::new(),
            created_at: run.started_at,
            updated_at: run.updated_at,
        };
        insert_run_launch_intent_tx(&tx, &intent)?;
        #[cfg(test)]
        if take_admission_transaction_fault(AdmissionTransactionFaultStage::BeforePreparedEvent) {
            anyhow::bail!("injected run admission transaction failure");
        }
        insert_event_tx(
            &tx,
            &run.id,
            "run_launch_prepared",
            &intent,
            &run.updated_at.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(RunAdmissionTransition {
            outcome: RunAdmissionOutcome::Prepared,
            intent: Some(intent),
            active_run: None,
        })
    }

    pub(crate) fn begin_resume_run_launch(
        &self,
        run_id: &str,
        integration_worktree: &Path,
    ) -> Result<RunAdmissionTransition> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run =
            run_by_id(&tx, run_id)?.ok_or_else(|| anyhow::anyhow!("run {run_id:?} not found"))?;
        if !matches!(
            run.status,
            RunStatus::Interrupted | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Blocked
        ) {
            let active_run = if matches!(run.status, RunStatus::Pending | RunStatus::Running) {
                Some(run)
            } else {
                None
            };
            tx.commit()?;
            return Ok(RunAdmissionTransition {
                outcome: RunAdmissionOutcome::Conflict,
                intent: None,
                active_run,
            });
        }
        let incomplete_terminal = tx
            .query_row(
                "SELECT committed_at FROM terminal_transitions WHERE run_id=?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some_and(|committed_at| committed_at.is_empty());
        if incomplete_terminal {
            anyhow::bail!(
                "run {run_id:?} has an incomplete terminalization; reconcile it before resuming"
            );
        }
        if let Some(active_run) = active_run_for_repo_conn(&tx, &run.repo_id, Some(run_id))? {
            tx.commit()?;
            return Ok(RunAdmissionTransition {
                outcome: RunAdmissionOutcome::Conflict,
                intent: None,
                active_run: Some(active_run),
            });
        }
        let previous_epoch = tx.query_row(
            "SELECT execution_epoch FROM runs WHERE id=?1",
            params![run_id],
            |row| row.get::<_, i64>(0),
        )?;
        let execution_epoch = previous_epoch.max(1) as usize + 1;
        let now = Utc::now();
        let changed = tx.execute(
            r#"UPDATE runs
               SET status=?1, error='', execution_epoch=?2, updated_at=?3
               WHERE id=?4 AND status=?5"#,
            params![
                RunStatus::Pending.as_str(),
                execution_epoch as i64,
                now.to_rfc3339(),
                run_id,
                run.status.as_str(),
            ],
        )?;
        if changed != 1 {
            anyhow::bail!("run {run_id:?} changed while preparing resume admission");
        }
        tx.execute(
            "DELETE FROM terminal_transitions WHERE run_id=?1",
            params![run_id],
        )?;
        let intent = RunLaunchIntent {
            run_id: run.id.clone(),
            execution_epoch,
            action: RunLaunchAction::Resume,
            state: RunLaunchState::Prepared,
            repo_id: run.repo_id.clone(),
            integration_branch: run.integration_branch.clone(),
            integration_worktree: integration_worktree.display().to_string(),
            integration_resources_owned: false,
            prior_status: Some(run.status),
            prior_error: run.error.clone(),
            primary_cause: String::new(),
            compensation_error: String::new(),
            created_at: now,
            updated_at: now,
        };
        insert_run_launch_intent_tx(&tx, &intent)?;
        #[cfg(test)]
        if take_admission_transaction_fault(AdmissionTransactionFaultStage::BeforePreparedEvent) {
            anyhow::bail!("injected resume admission transaction failure");
        }
        insert_event_tx(
            &tx,
            run_id,
            "run_launch_prepared",
            &intent,
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(RunAdmissionTransition {
            outcome: RunAdmissionOutcome::Prepared,
            intent: Some(intent),
            active_run: None,
        })
    }

    pub(crate) fn activate_run_launch(&self, run_id: &str, execution_epoch: usize) -> Result<()> {
        self.transition_run_launch(
            run_id,
            execution_epoch,
            RunLaunchState::Prepared,
            RunLaunchState::Activated,
            true,
        )
    }

    pub(crate) fn complete_run_launch(&self, run_id: &str, execution_epoch: usize) -> Result<()> {
        self.transition_run_launch(
            run_id,
            execution_epoch,
            RunLaunchState::Activated,
            RunLaunchState::Completed,
            false,
        )
    }

    pub(crate) fn record_run_launch_integration_resources_created(
        &self,
        run_id: &str,
        execution_epoch: usize,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut intent =
            run_launch_intent_by_key(&tx, run_id, execution_epoch)?.ok_or_else(|| {
                anyhow::anyhow!("run launch intent {run_id}/{execution_epoch} not found")
            })?;
        if intent.action != RunLaunchAction::Start {
            anyhow::bail!("resume launch intent cannot own pre-existing integration resources");
        }
        if intent.integration_resources_owned {
            tx.commit()?;
            return Ok(false);
        }
        if intent.state != RunLaunchState::Activated {
            anyhow::bail!(
                "run launch intent {run_id}/{execution_epoch} is {}; integration resources can be claimed only while activated",
                intent.state.as_str()
            );
        }
        let now = Utc::now();
        let changed = tx.execute(
            r#"UPDATE run_launch_intents
               SET integration_resources_owned=1, updated_at=?1
               WHERE run_id=?2 AND execution_epoch=?3 AND state=?4
                 AND integration_resources_owned=0"#,
            params![
                now.to_rfc3339(),
                run_id,
                execution_epoch as i64,
                RunLaunchState::Activated.as_str(),
            ],
        )?;
        if changed != 1 {
            anyhow::bail!(
                "run launch intent {run_id}/{execution_epoch} changed while recording integration resource ownership"
            );
        }
        intent.integration_resources_owned = true;
        intent.updated_at = now;
        insert_event_tx(
            &tx,
            run_id,
            "run_launch_integration_resources_created",
            &intent,
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn transition_run_launch(
        &self,
        run_id: &str,
        execution_epoch: usize,
        expected: RunLaunchState,
        target: RunLaunchState,
        activate_run: bool,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let intent = run_launch_intent_by_key(&tx, run_id, execution_epoch)?.ok_or_else(|| {
            anyhow::anyhow!("run launch intent {run_id}/{execution_epoch} not found")
        })?;
        if intent.state == target {
            tx.commit()?;
            return Ok(());
        }
        if intent.state != expected {
            anyhow::bail!(
                "run launch intent {run_id}/{execution_epoch} is {}; expected {}",
                intent.state.as_str(),
                expected.as_str()
            );
        }
        let now = Utc::now().to_rfc3339();
        if activate_run {
            let changed = tx.execute(
                r#"UPDATE runs SET status=?1, updated_at=?2
                   WHERE id=?3 AND execution_epoch=?4 AND status=?5"#,
                params![
                    RunStatus::Running.as_str(),
                    &now,
                    run_id,
                    execution_epoch as i64,
                    RunStatus::Pending.as_str(),
                ],
            )?;
            if changed != 1 {
                anyhow::bail!("run {run_id:?} is no longer pending for launch activation");
            }
        }
        tx.execute(
            r#"UPDATE run_launch_intents SET state=?1, updated_at=?2
               WHERE run_id=?3 AND execution_epoch=?4 AND state=?5"#,
            params![
                target.as_str(),
                &now,
                run_id,
                execution_epoch as i64,
                expected.as_str(),
            ],
        )?;
        let mut transitioned = intent;
        transitioned.state = target;
        transitioned.updated_at = DateTime::parse_from_rfc3339(&now)?.with_timezone(&Utc);
        insert_event_tx(
            &tx,
            run_id,
            match target {
                RunLaunchState::Activated => "run_launch_activated",
                RunLaunchState::Completed => "run_launch_completed",
                _ => "run_launch_transitioned",
            },
            &transitioned,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn fail_run_launch(
        &self,
        run_id: &str,
        execution_epoch: usize,
        primary_cause: &str,
        compensation_error: &str,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut intent) = run_launch_intent_by_key(&tx, run_id, execution_epoch)? else {
            tx.commit()?;
            return Ok(false);
        };
        if !intent.state.is_incomplete() {
            if intent.state == RunLaunchState::Failed
                && intent.primary_cause == primary_cause
                && intent.compensation_error == compensation_error
            {
                tx.commit()?;
                return Ok(false);
            }
            anyhow::bail!(
                "run launch intent {run_id}/{execution_epoch} is already {}",
                intent.state.as_str()
            );
        }
        let now = Utc::now();
        let (status, error) = match intent.action {
            RunLaunchAction::Start => (RunStatus::Failed, primary_cause.to_string()),
            RunLaunchAction::Resume => (
                intent.prior_status.ok_or_else(|| {
                    anyhow::anyhow!(
                        "resume launch intent {run_id}/{execution_epoch} lost prior status"
                    )
                })?,
                intent.prior_error.clone(),
            ),
        };
        tx.execute(
            r#"UPDATE runs SET status=?1, error=?2, updated_at=?3
               WHERE id=?4 AND execution_epoch=?5 AND status IN ('pending', 'running')"#,
            params![
                status.as_str(),
                error,
                now.to_rfc3339(),
                run_id,
                execution_epoch as i64,
            ],
        )?;
        tx.execute(
            r#"UPDATE run_launch_intents
               SET state=?1, primary_cause=?2, compensation_error=?3, updated_at=?4
               WHERE run_id=?5 AND execution_epoch=?6"#,
            params![
                RunLaunchState::Failed.as_str(),
                primary_cause,
                compensation_error,
                now.to_rfc3339(),
                run_id,
                execution_epoch as i64,
            ],
        )?;
        intent.state = RunLaunchState::Failed;
        intent.primary_cause = primary_cause.to_string();
        intent.compensation_error = compensation_error.to_string();
        intent.updated_at = now;
        insert_event_tx(&tx, run_id, "run_launch_failed", &intent, &now.to_rfc3339())?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn record_run_launch_compensation(
        &self,
        run_id: &str,
        execution_epoch: usize,
        compensation_error: &str,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut intent =
            run_launch_intent_by_key(&tx, run_id, execution_epoch)?.ok_or_else(|| {
                anyhow::anyhow!("run launch intent {run_id}/{execution_epoch} not found")
            })?;
        if intent.action != RunLaunchAction::Start {
            anyhow::bail!("resume launch resources are not owned by the resume intent");
        }
        let target = if compensation_error.is_empty() {
            RunLaunchState::Compensated
        } else {
            RunLaunchState::RecoveryRequired
        };
        if intent.state == target && intent.compensation_error == compensation_error {
            tx.commit()?;
            return Ok(false);
        }
        if !matches!(
            intent.state,
            RunLaunchState::Interrupted | RunLaunchState::Failed | RunLaunchState::RecoveryRequired
        ) {
            anyhow::bail!(
                "run launch intent {run_id}/{execution_epoch} is {}; cannot record compensation",
                intent.state.as_str()
            );
        }
        let now = Utc::now();
        tx.execute(
            r#"UPDATE run_launch_intents SET state=?1, compensation_error=?2, updated_at=?3
               WHERE run_id=?4 AND execution_epoch=?5"#,
            params![
                target.as_str(),
                compensation_error,
                now.to_rfc3339(),
                run_id,
                execution_epoch as i64,
            ],
        )?;
        intent.state = target;
        intent.compensation_error = compensation_error.to_string();
        intent.updated_at = now;
        insert_event_tx(
            &tx,
            run_id,
            if target == RunLaunchState::Compensated {
                "run_launch_compensated"
            } else {
                "run_launch_compensation_failed"
            },
            &intent,
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn run_launch_intent(
        &self,
        run_id: &str,
        execution_epoch: usize,
    ) -> Result<Option<RunLaunchIntent>> {
        let conn = self.conn()?;
        run_launch_intent_by_key(&conn, run_id, execution_epoch)
    }

    pub(crate) fn incomplete_run_launch_intents(&self) -> Result<Vec<RunLaunchIntent>> {
        self.run_launch_intents_in_states(&[RunLaunchState::Prepared, RunLaunchState::Activated])
    }

    pub(crate) fn run_launch_intents_requiring_compensation(&self) -> Result<Vec<RunLaunchIntent>> {
        self.run_launch_intents_in_states(&[
            RunLaunchState::Interrupted,
            RunLaunchState::Failed,
            RunLaunchState::RecoveryRequired,
        ])
    }

    fn run_launch_intents_in_states(
        &self,
        states: &[RunLaunchState],
    ) -> Result<Vec<RunLaunchIntent>> {
        let conn = self.conn()?;
        let state_values = states
            .iter()
            .map(|state| format!("'{}'", state.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            r#"SELECT run_id, execution_epoch, action, state, repo_id, integration_branch,
                      integration_worktree, integration_resources_owned, prior_status, prior_error,
                      primary_cause, compensation_error, created_at, updated_at
               FROM run_launch_intents WHERE state IN ({state_values})
               ORDER BY created_at, run_id, execution_epoch"#
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], run_launch_intent_tuple_from_row)?;
        let mut intents = Vec::new();
        for row in rows {
            intents.push(run_launch_intent_from_tuple(row?)?);
        }
        Ok(intents)
    }

    pub(crate) fn prepare_integration_merge(
        &self,
        intent: &IntegrationMergeIntent,
    ) -> Result<IntegrationMergePrepareTransition> {
        if intent.state != IntegrationMergeState::Prepared || !intent.resulting_head.is_empty() {
            anyhow::bail!("new integration merge intent must be prepared without a result head");
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(mut existing) = integration_merge_intent_by_id(&tx, &intent.operation_id)? {
            if !integration_merge_authority_matches(&existing, intent) {
                anyhow::bail!(
                    "integration merge operation {:?} conflicts with its durable command identity",
                    intent.operation_id
                );
            }
            let outcome = match existing.state {
                IntegrationMergeState::Prepared => IntegrationMergePrepareOutcome::AlreadyPrepared,
                IntegrationMergeState::Applied => IntegrationMergePrepareOutcome::AlreadyApplied,
                IntegrationMergeState::NotStarted => {
                    let now = Utc::now();
                    tx.execute(
                        r#"UPDATE integration_merge_intents
                           SET state='prepared', resulting_head='', primary_cause='', abort_error='',
                               conflicted_files_json='[]', updated_at=?1
                           WHERE operation_id=?2 AND state='not_started'"#,
                        params![now.to_rfc3339(), &intent.operation_id],
                    )?;
                    existing.state = IntegrationMergeState::Prepared;
                    existing.resulting_head.clear();
                    existing.primary_cause.clear();
                    existing.abort_error.clear();
                    existing.conflicted_files.clear();
                    existing.updated_at = now;
                    insert_event_tx(
                        &tx,
                        &intent.run_id,
                        "integration_merge_reprepared",
                        &existing,
                        &now.to_rfc3339(),
                    )?;
                    IntegrationMergePrepareOutcome::Prepared
                }
                IntegrationMergeState::Conflicted | IntegrationMergeState::Divergent => {
                    IntegrationMergePrepareOutcome::Conflict
                }
            };
            tx.commit()?;
            return Ok(IntegrationMergePrepareTransition {
                outcome,
                intent: existing,
            });
        }
        if let Some(existing) = prepared_integration_merge_for_run(&tx, &intent.run_id)? {
            tx.commit()?;
            return Ok(IntegrationMergePrepareTransition {
                outcome: IntegrationMergePrepareOutcome::Conflict,
                intent: existing,
            });
        }
        let run_status = tx
            .query_row(
                "SELECT status FROM runs WHERE id=?1",
                params![&intent.run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("run {:?} not found", intent.run_id))?;
        if RunStatus::parse(&run_status)? != RunStatus::Running {
            anyhow::bail!(
                "run {:?} is not running; cannot prepare integration merge",
                intent.run_id
            );
        }
        insert_integration_merge_intent_tx(&tx, intent)?;
        #[cfg(test)]
        if take_integration_merge_transaction_fault(
            IntegrationMergeTransactionFaultStage::PreparedEvent,
        ) {
            anyhow::bail!("injected merge preparation transaction failure");
        }
        insert_event_tx(
            &tx,
            &intent.run_id,
            "integration_merge_prepared",
            intent,
            &intent.created_at.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(IntegrationMergePrepareTransition {
            outcome: IntegrationMergePrepareOutcome::Prepared,
            intent: intent.clone(),
        })
    }

    pub(crate) fn commit_integration_merge(
        &self,
        operation_id: &str,
        resulting_head: &str,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut intent = integration_merge_intent_by_id(&tx, operation_id)?.ok_or_else(|| {
            anyhow::anyhow!("integration merge intent {operation_id:?} not found")
        })?;
        if intent.state == IntegrationMergeState::Applied {
            if intent.resulting_head != resulting_head {
                anyhow::bail!(
                    "integration merge {operation_id:?} was already applied at {}, not {}",
                    intent.resulting_head,
                    resulting_head
                );
            }
            tx.commit()?;
            return Ok(false);
        }
        if intent.state != IntegrationMergeState::Prepared {
            anyhow::bail!(
                "integration merge {operation_id:?} is {}; cannot apply it",
                intent.state.as_str()
            );
        }
        let now = Utc::now();
        tx.execute(
            r#"UPDATE integration_merge_intents
               SET state='applied', resulting_head=?1, updated_at=?2
               WHERE operation_id=?3 AND state='prepared'"#,
            params![resulting_head, now.to_rfc3339(), operation_id],
        )?;
        intent.state = IntegrationMergeState::Applied;
        intent.resulting_head = resulting_head.to_string();
        intent.updated_at = now;
        apply_integration_merge_completion_tx(&tx, &intent, &now.to_rfc3339())?;
        #[cfg(test)]
        if take_integration_merge_transaction_fault(
            IntegrationMergeTransactionFaultStage::AppliedEvent,
        ) {
            anyhow::bail!("injected merge completion transaction failure");
        }
        insert_event_tx(
            &tx,
            &intent.run_id,
            "integration_merge_applied",
            &intent,
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn resolve_integration_merge(
        &self,
        operation_id: &str,
        state: IntegrationMergeState,
        resulting_head: &str,
        primary_cause: &str,
        abort_error: &str,
        conflicted_files: &[String],
    ) -> Result<bool> {
        if matches!(
            state,
            IntegrationMergeState::Prepared | IntegrationMergeState::Applied
        ) {
            anyhow::bail!("integration merge resolution requires a non-applied terminal state");
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut intent = integration_merge_intent_by_id(&tx, operation_id)?.ok_or_else(|| {
            anyhow::anyhow!("integration merge intent {operation_id:?} not found")
        })?;
        if intent.state == state
            && intent.resulting_head == resulting_head
            && intent.primary_cause == primary_cause
            && intent.abort_error == abort_error
            && intent.conflicted_files == conflicted_files
        {
            tx.commit()?;
            return Ok(false);
        }
        let valid_transition = intent.state == IntegrationMergeState::Prepared
            || (intent.state == IntegrationMergeState::Conflicted
                && matches!(
                    state,
                    IntegrationMergeState::Conflicted | IntegrationMergeState::NotStarted
                ));
        if !valid_transition {
            anyhow::bail!(
                "integration merge {operation_id:?} is {}; cannot resolve it as {}",
                intent.state.as_str(),
                state.as_str()
            );
        }
        let now = Utc::now();
        tx.execute(
            r#"UPDATE integration_merge_intents
               SET state=?1, resulting_head=?2, primary_cause=?3, abort_error=?4,
                   conflicted_files_json=?5, updated_at=?6
               WHERE operation_id=?7"#,
            params![
                state.as_str(),
                resulting_head,
                primary_cause,
                abort_error,
                serde_json::to_string(conflicted_files)?,
                now.to_rfc3339(),
                operation_id,
            ],
        )?;
        intent.state = state;
        intent.resulting_head = resulting_head.to_string();
        intent.primary_cause = primary_cause.to_string();
        intent.abort_error = abort_error.to_string();
        intent.conflicted_files = conflicted_files.to_vec();
        intent.updated_at = now;
        if intent.kind == IntegrationMergeKind::Slice
            && matches!(
                state,
                IntegrationMergeState::Conflicted | IntegrationMergeState::Divergent
            )
        {
            tx.execute(
                "UPDATE slice_runs SET status=?1, last_error=?2 WHERE run_id=?3 AND slice_id=?4",
                params![
                    SliceStatus::Blocked.as_str(),
                    primary_cause,
                    &intent.run_id,
                    &intent.slice_id,
                ],
            )?;
        }
        #[cfg(test)]
        if take_integration_merge_transaction_fault(
            IntegrationMergeTransactionFaultStage::ResolutionEvent,
        ) {
            anyhow::bail!("injected merge resolution transaction failure");
        }
        insert_event_tx(
            &tx,
            &intent.run_id,
            "integration_merge_reconciled",
            &intent,
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn integration_merge_intents(
        &self,
        run_id: &str,
    ) -> Result<Vec<IntegrationMergeIntent>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT operation_id, run_id, kind, slice_id, attempt, launch_id,
                      source_branch, source_commit, source_tree, expected_head,
                      expected_result_tree, resulting_head, state, completion_json,
                      primary_cause, abort_error, conflicted_files_json, created_at, updated_at
               FROM integration_merge_intents WHERE run_id=?1
               ORDER BY created_at, operation_id"#,
        )?;
        let rows = stmt.query_map(params![run_id], integration_merge_intent_tuple_from_row)?;
        let mut intents = Vec::new();
        for row in rows {
            intents.push(integration_merge_intent_from_tuple(row?)?);
        }
        Ok(intents)
    }

    #[cfg(test)]
    pub fn insert_run(&self, run: &Run) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_run_tx(&tx, run, None, None)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn set_frontier_state(
        &self,
        run_id: &str,
        mission_envelope: Option<&MissionEnvelope>,
        frontier_budget: Option<&FrontierBudgetState>,
    ) -> Result<()> {
        let envelope_json = mission_envelope
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_default();
        let budget_json = frontier_budget
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_default();
        let conn = self.conn()?;
        conn.execute(
            "UPDATE runs SET mission_envelope_json=?1, frontier_budget_json=?2, updated_at=?3 WHERE id=?4",
            params![envelope_json, budget_json, Utc::now().to_rfc3339(), run_id],
        )?;
        Ok(())
    }

    pub fn get_frontier_state(
        &self,
        run_id: &str,
    ) -> Result<(Option<MissionEnvelope>, Option<FrontierBudgetState>)> {
        let conn = self.conn()?;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT mission_envelope_json, frontier_budget_json FROM runs WHERE id=?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((envelope_json, budget_json)) = row else {
            return Ok((None, None));
        };
        let mission_envelope = if envelope_json.trim().is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(&envelope_json)
                    .with_context(|| format!("parse mission envelope for run {run_id}"))?,
            )
        };
        let frontier_budget = if budget_json.trim().is_empty() {
            mission_envelope
                .as_ref()
                .map(|_| FrontierBudgetState::default())
        } else {
            Some(
                serde_json::from_str(&budget_json)
                    .with_context(|| format!("parse frontier budget for run {run_id}"))?,
            )
        };
        Ok((mission_envelope, frontier_budget))
    }

    #[cfg(test)]
    pub fn update_run(&self, run_id: &str, status: RunStatus, error: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE runs SET status=?1, error=?2, updated_at=?3 WHERE id=?4",
            params![status.as_str(), error, Utc::now().to_rfc3339(), run_id],
        )?;
        Ok(())
    }

    pub fn prepare_run_terminal_transition(
        &self,
        run_id: &str,
        status: RunStatus,
        error: &str,
        progress_message: &str,
        question_interruption_reason: &str,
    ) -> Result<usize> {
        if matches!(status, RunStatus::Pending | RunStatus::Running) {
            anyhow::bail!("terminal run transition requires a terminal status, got {status}");
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_status = tx
            .query_row(
                "SELECT status FROM runs WHERE id=?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} not found"))?;
        let current_status = RunStatus::parse(&current_status)?;
        let existing_intent = tx
            .query_row(
                r#"SELECT status, error, progress_message, question_interruption_reason,
                          summary_written_at, committed_at
                   FROM terminal_transitions WHERE run_id=?1"#,
                params![run_id],
                terminal_transition_tuple_from_row,
            )
            .optional()?
            .map(terminal_transition_from_tuple)
            .transpose()?;
        if let Some(existing_intent) = existing_intent {
            if existing_intent.status != status
                || existing_intent.error != error
                || existing_intent.progress_message != progress_message
                || existing_intent.question_interruption_reason != question_interruption_reason
            {
                anyhow::bail!(
                    "run {run_id:?} already has durable terminal intent {} and cannot be changed to {status}",
                    existing_intent.status
                );
            }
            interrupt_active_worker_attempts(
                &tx,
                run_id,
                &existing_intent.question_interruption_reason,
                existing_intent.status,
            )?;
            tx.commit()?;
            return Ok(0);
        }
        if !matches!(current_status, RunStatus::Pending | RunStatus::Running) {
            anyhow::bail!(
                "run {run_id:?} is already {current_status}; cannot prepare terminal transition to {status}"
            );
        }

        let now = Utc::now().to_rfc3339();
        tx.execute(
            r#"INSERT INTO terminal_transitions
               (run_id, status, error, progress_message, question_interruption_reason, intended_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            params![
                run_id,
                status.as_str(),
                error,
                progress_message,
                question_interruption_reason,
                now,
            ],
        )?;
        let intent_payload = serde_json::to_string(&serde_json::json!({
            "status": status,
            "error": error,
            "progress_message": progress_message,
            "question_interruption_reason": question_interruption_reason,
        }))?;
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'terminal_transition_intended', ?2, ?3)",
            params![run_id, intent_payload, now],
        )?;

        let pending_questions = {
            let mut stmt = tx.prepare(
                r#"SELECT id, slice_id, attempt
                   FROM worker_questions
                   WHERE run_id=?1 AND state='pending'
                   ORDER BY asked_at ASC, id ASC"#,
            )?;
            let rows = stmt.query_map(params![run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as usize,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let interrupted = tx.execute(
            r#"UPDATE worker_questions
               SET state='interrupted', answered_at=?1, answer=?2, answer_source=''
               WHERE run_id=?3 AND state='pending'"#,
            params![now, question_interruption_reason, run_id],
        )?;
        for (question_id, slice_id, attempt) in &pending_questions {
            let payload = serde_json::to_string(&serde_json::json!({
                "question_id": question_id,
                "slice_id": slice_id,
                "attempt": attempt,
                "reason": question_interruption_reason,
                "terminal_status": status,
            }))?;
            tx.execute(
                "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_question_interrupted', ?2, ?3)",
                params![run_id, payload, now],
            )?;
        }
        let interrupted_slice_status = match status {
            RunStatus::Blocked => SliceStatus::Blocked,
            RunStatus::Failed => SliceStatus::Failed,
            RunStatus::Cancelled => SliceStatus::Cancelled,
            RunStatus::Interrupted | RunStatus::Completed => SliceStatus::Interrupted,
            RunStatus::Pending | RunStatus::Running => unreachable!("validated terminal status"),
        };
        let slice_error = if error.trim().is_empty() {
            question_interruption_reason
        } else {
            error
        };
        tx.execute(
            r#"UPDATE slice_runs
               SET status=?1, last_error=?2
               WHERE run_id=?3
                 AND status IN ('running', 'repair_needed', 'ready_to_merge')"#,
            params![interrupted_slice_status.as_str(), slice_error, run_id],
        )?;
        interrupt_active_worker_attempts(&tx, run_id, question_interruption_reason, status)?;
        tx.execute(
            r#"INSERT INTO run_progress
               (run_id, phase, slice_id, attempt, command, message, output_tail, phase_started_at,
                updated_at, worker_attempt_started_at, worker_pid, worker_process_observed_at,
                worker_last_event_at, worker_last_event_kind, worker_last_semantic_progress_at,
                worker_last_semantic_progress_summary, worker_attempt_timeout_seconds,
                worker_no_output_warning_seconds)
               VALUES (?1, ?2, '', 0, '', ?3, '', ?4, ?4, '', NULL, '', '', '', '', '', 0, 0)
               ON CONFLICT(run_id) DO UPDATE SET
                 phase=excluded.phase,
                 slice_id=excluded.slice_id,
                 attempt=excluded.attempt,
                 command=excluded.command,
                 message=excluded.message,
                 output_tail=excluded.output_tail,
                 phase_started_at=excluded.phase_started_at,
                 updated_at=excluded.updated_at,
                 worker_attempt_started_at=excluded.worker_attempt_started_at,
                 worker_pid=excluded.worker_pid,
                 worker_process_observed_at=excluded.worker_process_observed_at,
                 worker_last_event_at=excluded.worker_last_event_at,
                 worker_last_event_kind=excluded.worker_last_event_kind,
                 worker_last_semantic_progress_at=excluded.worker_last_semantic_progress_at,
                 worker_last_semantic_progress_summary=excluded.worker_last_semantic_progress_summary,
                 worker_attempt_timeout_seconds=excluded.worker_attempt_timeout_seconds,
                 worker_no_output_warning_seconds=excluded.worker_no_output_warning_seconds"#,
            params![run_id, status.as_str(), progress_message, now],
        )?;
        tx.commit()?;
        debug_assert_eq!(interrupted, pending_questions.len());
        Ok(interrupted)
    }

    pub(crate) fn terminal_transition(&self, run_id: &str) -> Result<Option<TerminalTransition>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                r#"SELECT status, error, progress_message, question_interruption_reason,
                          summary_written_at, committed_at
                   FROM terminal_transitions WHERE run_id=?1"#,
                params![run_id],
                terminal_transition_tuple_from_row,
            )
            .optional()?;
        row.map(terminal_transition_from_tuple).transpose()
    }

    pub(crate) fn mark_terminal_summary_written<T: Serialize>(
        &self,
        run_id: &str,
        event_type: &str,
        event_payload: &T,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let transition = tx
            .query_row(
                r#"SELECT status, error, progress_message, question_interruption_reason,
                          summary_written_at, committed_at
                   FROM terminal_transitions WHERE run_id=?1"#,
                params![run_id],
                terminal_transition_tuple_from_row,
            )
            .optional()?
            .map(terminal_transition_from_tuple)
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} has no durable terminal intent"))?;
        if transition.summary_written {
            tx.commit()?;
            return Ok(false);
        }
        if transition.committed {
            anyhow::bail!(
                "run {run_id:?} has committed terminal state without a durable terminal summary marker"
            );
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE terminal_transitions SET summary_written_at=?1 WHERE run_id=?2",
            params![now, run_id],
        )?;
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id,
                event_type,
                serde_json::to_string(event_payload)?,
                now
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn commit_terminal_transition<T: Serialize>(
        &self,
        run_id: &str,
        event_type: &str,
        event_payload: &T,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let transition = tx
            .query_row(
                r#"SELECT status, error, progress_message, question_interruption_reason,
                          summary_written_at, committed_at
                   FROM terminal_transitions WHERE run_id=?1"#,
                params![run_id],
                terminal_transition_tuple_from_row,
            )
            .optional()?
            .map(terminal_transition_from_tuple)
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} has no durable terminal intent"))?;
        if !transition.summary_written {
            anyhow::bail!(
                "run {run_id:?} cannot publish terminal state before its terminal summary is durable"
            );
        }
        interrupt_active_worker_attempts(
            &tx,
            run_id,
            &transition.question_interruption_reason,
            transition.status,
        )?;
        let (current_status, current_error) = tx
            .query_row(
                "SELECT status, error FROM runs WHERE id=?1",
                params![run_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} not found"))?;
        let current_status = RunStatus::parse(&current_status)?;
        if transition.committed {
            if current_status != transition.status || current_error != transition.error {
                anyhow::bail!(
                    "run {run_id:?} has committed terminal intent {} but inconsistent public state {}",
                    transition.status,
                    current_status
                );
            }
            tx.commit()?;
            return Ok(false);
        }
        if !matches!(current_status, RunStatus::Pending | RunStatus::Running) {
            anyhow::bail!(
                "run {run_id:?} is already {current_status}; cannot publish terminal state {}",
                transition.status
            );
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE runs SET status=?1, error=?2, updated_at=?3 WHERE id=?4",
            params![transition.status.as_str(), transition.error, now, run_id],
        )?;
        let launch_state = if matches!(transition.status, RunStatus::Failed | RunStatus::Blocked) {
            RunLaunchState::Failed
        } else {
            RunLaunchState::Interrupted
        };
        let launch_changed = tx.execute(
            r#"UPDATE run_launch_intents
               SET state=?1,
                   primary_cause=CASE WHEN primary_cause='' THEN ?2 ELSE primary_cause END,
                   updated_at=?3
               WHERE run_id=?4
                 AND execution_epoch=(SELECT execution_epoch FROM runs WHERE id=?4)
                 AND state IN ('prepared', 'activated')"#,
            params![launch_state.as_str(), &transition.error, &now, run_id,],
        )?;
        if launch_changed == 1 {
            insert_event_tx(
                &tx,
                run_id,
                "run_launch_interrupted",
                &serde_json::json!({
                    "state": launch_state,
                    "terminal_status": transition.status,
                    "primary_cause": transition.error,
                }),
                &now,
            )?;
        }
        #[cfg(test)]
        if take_terminal_transition_fault(TerminalTransitionFaultStage::BeforeTerminalEvent) {
            anyhow::bail!("injected terminal state/event commit failure");
        }
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id,
                event_type,
                serde_json::to_string(event_payload)?,
                now
            ],
        )?;
        tx.execute(
            "UPDATE terminal_transitions SET committed_at=?1 WHERE run_id=?2",
            params![now, run_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn current_run_execution_epoch(&self, run_id: &str) -> Result<usize> {
        let conn = self.conn()?;
        let epoch = conn
            .query_row(
                "SELECT execution_epoch FROM runs WHERE id=?1",
                params![run_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .context("run disappeared while reading execution epoch")?;
        Ok(epoch.max(1) as usize)
    }

    #[cfg(test)]
    pub(crate) fn reopen_run_for_resume(&self, run_id: &str) -> Result<usize> {
        let transition = self.begin_resume_run_launch(run_id, Path::new(""))?;
        if transition.outcome != RunAdmissionOutcome::Prepared {
            anyhow::bail!("run {run_id:?} could not acquire resume admission");
        }
        let intent = transition
            .intent
            .ok_or_else(|| anyhow::anyhow!("resume admission lost its launch intent"))?;
        self.activate_run_launch(run_id, intent.execution_epoch)?;
        Ok(intent.execution_epoch)
    }

    pub(crate) fn terminal_transition_needs_reconciliation(&self, run_id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                r#"SELECT committed_at, notification_bookkept_at, cleanup_started_at,
                          cleanup_completed_at
                   FROM terminal_transitions WHERE run_id=?1"#,
                params![run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.is_some_and(
            |(committed_at, notification_bookkept_at, cleanup_started_at, cleanup_completed_at)| {
                committed_at.trim().is_empty()
                    || notification_bookkept_at.trim().is_empty()
                    || (cleanup_started_at.trim().is_empty()
                        && cleanup_completed_at.trim().is_empty())
            },
        ))
    }

    pub(crate) fn terminal_transition_run_ids_needing_reconciliation(&self) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT run_id
               FROM terminal_transitions
               WHERE committed_at=''
                  OR notification_bookkept_at=''
                  OR (cleanup_started_at='' AND cleanup_completed_at='')
               ORDER BY intended_at ASC, run_id ASC"#,
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut run_ids = Vec::new();
        for row in rows {
            run_ids.push(row?);
        }
        Ok(run_ids)
    }

    pub(crate) fn terminal_notification_bookkept(&self, run_id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT notification_bookkept_at FROM terminal_transitions WHERE run_id=?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(row.is_some_and(|bookkept_at| !bookkept_at.trim().is_empty()))
    }

    pub(crate) fn mark_terminal_notification_bookkept(&self, run_id: &str) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (committed_at, notification_bookkept_at): (String, String) = tx
            .query_row(
                "SELECT committed_at, notification_bookkept_at FROM terminal_transitions WHERE run_id=?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} has no durable terminal intent"))?;
        if committed_at.trim().is_empty() {
            anyhow::bail!(
                "run {run_id:?} cannot record terminal notification bookkeeping before terminal state commit"
            );
        }
        if !notification_bookkept_at.trim().is_empty() {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE terminal_transitions SET notification_bookkept_at=?1 WHERE run_id=?2",
            params![Utc::now().to_rfc3339(), run_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn claim_terminal_cleanup(&self, run_id: &str) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (committed_at, notification_bookkept_at, cleanup_started_at, cleanup_completed_at): (
            String,
            String,
            String,
            String,
        ) = tx
            .query_row(
                r#"SELECT committed_at, notification_bookkept_at, cleanup_started_at,
                          cleanup_completed_at
                   FROM terminal_transitions WHERE run_id=?1"#,
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} has no durable terminal intent"))?;
        if committed_at.trim().is_empty() || notification_bookkept_at.trim().is_empty() {
            anyhow::bail!(
                "run {run_id:?} cannot clean worktrees before terminal state and notification bookkeeping"
            );
        }
        if !cleanup_started_at.trim().is_empty() || !cleanup_completed_at.trim().is_empty() {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE terminal_transitions SET cleanup_started_at=?1 WHERE run_id=?2",
            params![Utc::now().to_rfc3339(), run_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn mark_terminal_cleanup_completed<T: Serialize>(
        &self,
        run_id: &str,
        event_type: &str,
        event_payload: &T,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (cleanup_started_at, cleanup_completed_at): (String, String) = tx
            .query_row(
                "SELECT cleanup_started_at, cleanup_completed_at FROM terminal_transitions WHERE run_id=?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} has no durable terminal intent"))?;
        if cleanup_started_at.trim().is_empty() {
            anyhow::bail!("run {run_id:?} has no durable cleanup claim");
        }
        if !cleanup_completed_at.trim().is_empty() {
            tx.commit()?;
            return Ok(false);
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE terminal_transitions SET cleanup_completed_at=?1 WHERE run_id=?2",
            params![now, run_id],
        )?;
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id,
                event_type,
                serde_json::to_string(event_payload)?,
                now
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn allocate_worker_attempt(
        &self,
        run_id: &str,
        slice_id: &str,
        execution_epoch: usize,
        worker_retry_ordinal: usize,
        repair_ordinal: usize,
        envelope_retry_ordinal: usize,
        kind: &str,
        worktree_root: &Path,
    ) -> Result<WorkerAttemptLedger> {
        self.allocate_worker_attempt_with_projection(
            run_id,
            slice_id,
            execution_epoch,
            worker_retry_ordinal,
            repair_ordinal,
            envelope_retry_ordinal,
            kind,
            worktree_root,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn allocate_run_worker_attempt(
        &self,
        run_id: &str,
        scope_id: &str,
        execution_epoch: usize,
        worker_retry_ordinal: usize,
        repair_ordinal: usize,
        envelope_retry_ordinal: usize,
        kind: &str,
        worktree_root: &Path,
    ) -> Result<WorkerAttemptLedger> {
        self.allocate_worker_attempt_with_projection(
            run_id,
            scope_id,
            execution_epoch,
            worker_retry_ordinal,
            repair_ordinal,
            envelope_retry_ordinal,
            kind,
            worktree_root,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_worker_attempt_with_projection(
        &self,
        run_id: &str,
        slice_id: &str,
        execution_epoch: usize,
        worker_retry_ordinal: usize,
        repair_ordinal: usize,
        envelope_retry_ordinal: usize,
        kind: &str,
        worktree_root: &Path,
        project_slice_run: bool,
    ) -> Result<WorkerAttemptLedger> {
        if execution_epoch == 0
            || (project_slice_run && worker_retry_ordinal == 0)
            || kind.trim().is_empty()
        {
            anyhow::bail!(
                "worker-attempt allocation requires a nonzero execution epoch, a kind, and a nonzero slice retry ordinal"
            );
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_run = tx
            .query_row(
                "SELECT status FROM runs WHERE id=?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if active_run.as_deref() != Some("running") {
            anyhow::bail!("cannot allocate worker attempt for non-running or missing run {run_id}");
        }
        let next_ordinal = tx.query_row(
            "SELECT COALESCE(MAX(launch_ordinal), 0) + 1 FROM worker_attempt_ledger WHERE run_id=?1 AND slice_id=?2",
            params![run_id, slice_id],
            |row| row.get::<_, i64>(0),
        )?;
        let now = Utc::now();
        let created_at = now.to_rfc3339();
        tx.execute(
            r#"INSERT INTO worker_attempt_ledger
               (run_id, slice_id, launch_ordinal, execution_epoch, worker_retry_ordinal,
                repair_ordinal, envelope_retry_ordinal, kind, state, branch, worktree,
                output_stem, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'allocated', '', '', '', ?9)"#,
            params![
                run_id,
                slice_id,
                next_ordinal,
                execution_epoch as i64,
                worker_retry_ordinal as i64,
                repair_ordinal as i64,
                envelope_retry_ordinal as i64,
                kind,
                created_at,
            ],
        )?;
        let launch_id = tx.last_insert_rowid();
        let branch = format!("khazad/{run_id}/{slice_id}/launch-{launch_id}");
        let worktree = worktree_root
            .join(slice_id)
            .join(format!("launch-{launch_id}"))
            .to_string_lossy()
            .to_string();
        let output_stem = format!("{slice_id}.worker.launch-{launch_id}");
        tx.execute(
            "UPDATE worker_attempt_ledger SET branch=?1, worktree=?2, output_stem=?3 WHERE launch_id=?4",
            params![branch, worktree, output_stem, launch_id],
        )?;
        if project_slice_run {
            tx.execute(
                r#"INSERT INTO slice_runs (run_id, slice_id, status, branch, attempts)
                   VALUES (?1, ?2, 'running', ?3, ?4)
                   ON CONFLICT(run_id, slice_id) DO UPDATE SET
                     status='running', branch=excluded.branch, attempts=excluded.attempts"#,
                params![run_id, slice_id, branch, worker_retry_ordinal as i64],
            )?;
        }
        let payload = serde_json::json!({
            "launch_id": launch_id,
            "slice_id": slice_id,
            "launch_ordinal": next_ordinal,
            "execution_epoch": execution_epoch,
            "worker_retry_ordinal": worker_retry_ordinal,
            "repair_ordinal": repair_ordinal,
            "envelope_retry_ordinal": envelope_retry_ordinal,
            "kind": kind,
            "state": "allocated",
            "output_stem": &output_stem,
        });
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_attempt_allocated', ?2, ?3)",
            params![run_id, serde_json::to_string(&payload)?, now.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(WorkerAttemptLedger {
            run_id: run_id.to_string(),
            slice_id: slice_id.to_string(),
            launch_id,
            launch_ordinal: next_ordinal as usize,
            execution_epoch,
            worker_retry_ordinal,
            repair_ordinal,
            envelope_retry_ordinal,
            kind: kind.to_string(),
            state: "allocated".to_string(),
            branch,
            worktree,
            output_stem,
            created_at: now,
            launched_at: None,
            finished_at: None,
            failure_cause: String::new(),
            activity: None,
        })
    }

    #[allow(dead_code)]
    pub fn mark_worker_attempt_launched(&self, launch_id: i64) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (run_id, slice_id): (String, String) = tx.query_row(
            "SELECT run_id, slice_id FROM worker_attempt_ledger WHERE launch_id=?1 AND state='allocated'",
            params![launch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?.context("cannot mark non-allocated worker attempt launched")?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE worker_attempt_ledger SET state='running', launched_at=?1 WHERE launch_id=?2 AND state='allocated'",
            params![now, launch_id],
        )?;
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_attempt_launched', ?2, ?3)",
            params![run_id, serde_json::to_string(&serde_json::json!({"launch_id": launch_id, "slice_id": slice_id}))?, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn finish_worker_attempt(
        &self,
        launch_id: i64,
        state: &str,
        failure_cause: &str,
    ) -> Result<()> {
        if !matches!(state, "succeeded" | "failed" | "interrupted") {
            anyhow::bail!(
                "worker attempt terminal state must be succeeded, failed, or interrupted"
            );
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (run_id, slice_id, prior_state): (String, String, String) = tx
            .query_row(
                "SELECT run_id, slice_id, state FROM worker_attempt_ledger WHERE launch_id=?1",
                params![launch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .context("cannot finish missing worker attempt")?;
        if !matches!(prior_state.as_str(), "allocated" | "running")
            || (state == "succeeded" && prior_state != "running")
        {
            anyhow::bail!("cannot finish worker attempt {launch_id} from {prior_state} as {state}");
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE worker_attempt_ledger SET state=?1, finished_at=?2, failure_cause=?3 WHERE launch_id=?4",
            params![state, now, failure_cause, launch_id],
        )?;
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_attempt_finished', ?2, ?3)",
            params![run_id, serde_json::to_string(&serde_json::json!({"launch_id": launch_id, "slice_id": slice_id, "state": state, "failure_cause": failure_cause}))?, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn list_worker_attempt_ledger(
        &self,
        run_id: &str,
        slice_id: &str,
    ) -> Result<Vec<WorkerAttemptLedger>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT run_id, slice_id, launch_id, launch_ordinal, execution_epoch,
                      worker_retry_ordinal, repair_ordinal, envelope_retry_ordinal, kind,
                      state, branch, worktree, output_stem, created_at, launched_at,
                      finished_at, failure_cause, worker_pid, worker_process_observed_at,
                      worker_last_event_at, worker_last_event_kind,
                      worker_last_semantic_progress_at, worker_last_semantic_progress_summary,
                      worker_attempt_timeout_seconds, worker_no_output_warning_seconds
               FROM worker_attempt_ledger
               WHERE run_id=?1 AND slice_id=?2
               ORDER BY launch_ordinal ASC, launch_id ASC"#,
        )?;
        let rows = stmt.query_map(params![run_id, slice_id], worker_attempt_ledger_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Retains a durable allocation after a crash but marks it unrecoverable;
    /// resume must allocate a fresh identity rather than reviving this row.
    #[allow(dead_code)]
    pub fn reconcile_unlaunched_worker_attempts(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let interrupted =
            interrupt_active_worker_attempts(&tx, run_id, reason, RunStatus::Interrupted)?;
        tx.commit()?;
        Ok(interrupted)
    }

    pub fn upsert_slice_run(&self, slice_run: &SliceRun) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            r#"INSERT INTO slice_runs (run_id, slice_id, status, branch, commit_sha, attempts, last_error)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               ON CONFLICT(run_id, slice_id) DO UPDATE SET
                 status=excluded.status,
                 branch=excluded.branch,
                 commit_sha=excluded.commit_sha,
                 attempts=excluded.attempts,
                 last_error=excluded.last_error"#,
            params![
                &slice_run.run_id,
                &slice_run.slice_id,
                slice_run.status.as_str(),
                &slice_run.branch,
                &slice_run.commit_sha,
                slice_run.attempts as i64,
                &slice_run.last_error
            ],
        )?;
        Ok(())
    }

    pub fn update_slice_status(
        &self,
        run_id: &str,
        slice_id: &str,
        status: SliceStatus,
        last_error: &str,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE slice_runs SET status=?1, last_error=?2 WHERE run_id=?3 AND slice_id=?4",
            params![status.as_str(), last_error, run_id, slice_id],
        )?;
        Ok(())
    }

    pub fn activate_slice_attempt(
        &self,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            r#"UPDATE slice_runs
               SET attempts=?1
               WHERE run_id=?2 AND slice_id=?3
                 AND status IN ('running', 'repair_needed') AND attempts<=?1"#,
            params![attempt as i64, run_id, slice_id],
        )?;
        if updated != 1 {
            anyhow::bail!(
                "cannot activate stale or non-running worker attempt {slice_id} attempt {attempt} for run {run_id}"
            );
        }
        let stale_questions = {
            let mut stmt = tx.prepare(
                r#"SELECT id, attempt
                   FROM worker_questions
                   WHERE run_id=?1 AND slice_id=?2 AND state='pending' AND attempt<>?3
                   ORDER BY asked_at ASC, id ASC"#,
            )?;
            let rows = stmt.query_map(params![run_id, slice_id, attempt as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = Utc::now().to_rfc3339();
        for (question_id, stale_attempt) in stale_questions {
            let reason = format!("superseded by worker attempt {attempt}");
            let interrupted = tx.execute(
                r#"UPDATE worker_questions
                   SET state='interrupted', answered_at=?1, answer=?2
                   WHERE id=?3 AND state='pending'"#,
                params![now, reason, question_id],
            )?;
            if interrupted == 1 {
                let payload = serde_json::to_string(&serde_json::json!({
                    "question_id": question_id,
                    "slice_id": slice_id,
                    "attempt": stale_attempt,
                    "active_attempt": attempt,
                    "reason": "superseded_by_worker_attempt"
                }))?;
                tx.execute(
                    "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_question_interrupted', ?2, ?3)",
                    params![run_id, payload, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(dead_code)] // legacy ordinal-only compatibility API
    pub fn worker_attempt_is_active(
        &self,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
    ) -> Result<bool> {
        self.worker_attempt_is_active_with_launch_id(run_id, slice_id, attempt, None)
    }

    /// A launch-scoped question may only be serviced by its exact running ledger row.
    /// Legacy questions retain ordinal-only compatibility.
    pub fn worker_attempt_is_active_with_launch_id(
        &self,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        launch_id: Option<i64>,
    ) -> Result<bool> {
        let conn = self.conn()?;
        active_worker_attempt_with_launch_id(&conn, run_id, slice_id, attempt, launch_id)
    }

    #[allow(dead_code)] // Legacy ordinal-only worker-question compatibility.
    pub fn has_pending_worker_question(
        &self,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
    ) -> Result<bool> {
        self.has_pending_worker_question_with_launch_id(run_id, slice_id, attempt, None)
    }

    pub fn has_pending_worker_question_with_launch_id(
        &self,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        launch_id: Option<i64>,
    ) -> Result<bool> {
        let conn = self.conn()?;
        let pending = conn
            .query_row(
                r#"SELECT 1 FROM worker_questions
               WHERE run_id=?1 AND slice_id=?2 AND attempt=?3 AND state='pending'
                 AND (launch_id IS NULL OR launch_id=?4)
               LIMIT 1"#,
                params![run_id, slice_id, attempt as i64, launch_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(pending.is_some())
    }

    pub fn record_event<T: Serialize>(&self, run_id: &str, typ: &str, payload: &T) -> Result<()> {
        let payload_json = serde_json::to_string(payload)?;
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, typ, payload_json, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    #[allow(dead_code)] // Legacy ordinal-only worker-question compatibility.
    pub fn store_worker_token(&self, run_id: &str, token: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            r#"INSERT INTO run_worker_tokens (run_id, token_hash, created_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(run_id) DO UPDATE SET token_hash=excluded.token_hash"#,
            params![run_id, token_hash(token), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn validate_worker_token(&self, run_id: &str, token: &str) -> Result<bool> {
        let conn = self.conn()?;
        let hash: Option<String> = conn
            .query_row(
                "SELECT token_hash FROM run_worker_tokens WHERE run_id=?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hash.is_some_and(|hash| hash == token_hash(token)))
    }

    pub fn store_worker_launch_token(
        &self,
        run_id: &str,
        launch_id: i64,
        token: &str,
    ) -> Result<()> {
        if launch_id <= 0 || token.is_empty() {
            anyhow::bail!("worker launch token requires a positive launch id and non-empty token");
        }
        let conn = self.conn()?;
        let updated = conn.execute(
            r#"UPDATE worker_attempt_ledger
               SET worker_token_hash=?1
               WHERE launch_id=?2 AND run_id=?3 AND state='allocated'
                 AND worker_token_hash=''"#,
            params![token_hash(token), launch_id, run_id],
        )?;
        if updated != 1 {
            anyhow::bail!(
                "cannot bind worker token to missing, launched, or already-authorized launch {launch_id} for run {run_id}"
            );
        }
        Ok(())
    }

    pub fn validate_worker_launch_token(
        &self,
        run_id: &str,
        launch_id: i64,
        token: &str,
    ) -> Result<bool> {
        if launch_id <= 0 {
            return Ok(false);
        }
        let conn = self.conn()?;
        let hash: Option<String> = conn
            .query_row(
                r#"SELECT l.worker_token_hash
                   FROM worker_attempt_ledger l
                   JOIN runs r ON r.id=l.run_id
                   LEFT JOIN slice_runs s ON s.run_id=l.run_id AND s.slice_id=l.slice_id
                   WHERE l.launch_id=?1 AND l.run_id=?2 AND l.state='running'
                     AND r.status='running'
                     AND (
                       (l.kind='integration-repair' AND l.repair_ordinal>0)
                       OR (s.status IN ('running', 'repair_needed')
                           AND s.attempts=l.worker_retry_ordinal)
                     )
                   LIMIT 1"#,
                params![launch_id, run_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hash.is_some_and(|hash| !hash.is_empty() && hash == token_hash(token)))
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)] // Compatibility path for callers that do not send fallback metadata.
    pub fn insert_worker_question(
        &self,
        id: &str,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        question: &str,
        options: &[String],
        timeout_seconds: u64,
    ) -> Result<WorkerQuestion> {
        self.insert_worker_question_with_recommendation(
            id,
            run_id,
            slice_id,
            attempt,
            question,
            options,
            timeout_seconds,
            &WorkerQuestionRecommendation::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_worker_question_with_recommendation(
        &self,
        id: &str,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        question: &str,
        options: &[String],
        timeout_seconds: u64,
        recommendation: &WorkerQuestionRecommendation,
    ) -> Result<WorkerQuestion> {
        let conn = self.conn()?;
        insert_worker_question_row(
            &conn,
            id,
            run_id,
            slice_id,
            attempt,
            None,
            question,
            options,
            timeout_seconds,
            recommendation,
        )
    }

    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn insert_worker_question_with_launch_id_and_recommendation(
        &self,
        id: &str,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        launch_id: Option<i64>,
        question: &str,
        options: &[String],
        timeout_seconds: u64,
        recommendation: &WorkerQuestionRecommendation,
    ) -> Result<WorkerQuestion> {
        let conn = self.conn()?;
        insert_worker_question_row(
            &conn,
            id,
            run_id,
            slice_id,
            attempt,
            launch_id,
            question,
            options,
            timeout_seconds,
            recommendation,
        )
    }

    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn open_active_worker_question_with_recommendation<T, F>(
        &self,
        id: &str,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        question: &str,
        options: &[String],
        timeout_seconds: u64,
        recommendation: &WorkerQuestionRecommendation,
        asked_event_type: &str,
        asked_event: F,
        progress_message: &str,
    ) -> Result<WorkerQuestion>
    where
        T: Serialize,
        F: FnOnce(&WorkerQuestion) -> Result<T>,
    {
        self.open_active_worker_question_with_launch_id_and_recommendation(
            id,
            run_id,
            slice_id,
            attempt,
            None,
            question,
            options,
            timeout_seconds,
            recommendation,
            asked_event_type,
            asked_event,
            progress_message,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_active_worker_question_with_launch_id_and_recommendation<T, F>(
        &self,
        id: &str,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        launch_id: Option<i64>,
        question: &str,
        options: &[String],
        timeout_seconds: u64,
        recommendation: &WorkerQuestionRecommendation,
        asked_event_type: &str,
        asked_event: F,
        progress_message: &str,
    ) -> Result<WorkerQuestion>
    where
        T: Serialize,
        F: FnOnce(&WorkerQuestion) -> Result<T>,
    {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !active_worker_attempt_with_launch_id(&tx, run_id, slice_id, attempt, launch_id)? {
            anyhow::bail!(
                "worker question rejected for stale or inactive launch {launch_id:?} ({slice_id} attempt {attempt}) in run {run_id}"
            );
        }
        let pending = tx
            .query_row(
                r#"SELECT id FROM worker_questions
                   WHERE run_id=?1 AND slice_id=?2 AND attempt=?3 AND state='pending'
                   LIMIT 1"#,
                params![run_id, slice_id, attempt as i64],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(pending_id) = pending {
            anyhow::bail!(
                "worker attempt {slice_id} attempt {attempt} already has pending question {pending_id}"
            );
        }
        let question = insert_worker_question_row(
            &tx,
            id,
            run_id,
            slice_id,
            attempt,
            launch_id,
            question,
            options,
            timeout_seconds,
            recommendation,
        )?;
        let payload_json = serde_json::to_string(&asked_event(&question)?)?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, asked_event_type, payload_json, now],
        )?;
        project_awaiting_operator_progress(&tx, run_id, slice_id, attempt, progress_message, &now)?;
        tx.commit()?;
        Ok(question)
    }

    pub fn get_worker_question(&self, id: &str) -> Result<Option<WorkerQuestion>> {
        let conn = self.conn()?;
        worker_question_by_id(&conn, id, None)
    }

    pub fn list_worker_questions(&self, run_id: &str) -> Result<Vec<WorkerQuestion>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, run_id, slice_id, attempt, launch_id, question, options_json, timeout_seconds,
                      state, asked_at, answered_at, answer, recommended_answer,
                      recommendation_rationale, bounded_within_current_slice_or_mission_authority,
                      reversible, fallback_eligible, deadline_at, answer_source
               FROM worker_questions WHERE run_id=?1 ORDER BY asked_at ASC, id ASC"#,
        )?;
        let rows = stmt.query_map(params![run_id], worker_question_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_worker_questions_for_repo(&self, repo_path: &str) -> Result<Vec<WorkerQuestion>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT q.id, q.run_id, q.slice_id, q.attempt, q.launch_id, q.question,
                      q.options_json, q.timeout_seconds, q.state, q.asked_at, q.answered_at,
                      q.answer, q.recommended_answer, q.recommendation_rationale,
                      q.bounded_within_current_slice_or_mission_authority, q.reversible,
                      q.fallback_eligible, q.deadline_at, q.answer_source
               FROM worker_questions q
               JOIN runs r ON r.id = q.run_id
               WHERE r.repo_path=?1
               ORDER BY q.asked_at ASC, q.id ASC"#,
        )?;
        let rows = stmt.query_map(params![repo_path], worker_question_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn interrupt_worker_question_if_inactive_cas(
        &self,
        run_id: &str,
        question_id: &str,
        reason: &str,
    ) -> Result<WorkerQuestion> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = worker_question_by_id(&tx, question_id, Some(run_id))?.ok_or_else(|| {
            anyhow::anyhow!("question {question_id:?} for run {run_id:?} not found")
        })?;
        if existing.state != "pending"
            || active_worker_attempt_with_launch_id(
                &tx,
                run_id,
                &existing.slice_id,
                existing.attempt,
                existing.launch_id,
            )?
        {
            tx.commit()?;
            return Ok(existing);
        }
        let now = Utc::now().to_rfc3339();
        let updated = tx.execute(
            r#"UPDATE worker_questions
               SET state='interrupted', answered_at=?1, answer=?2
               WHERE id=?3 AND run_id=?4 AND state='pending'"#,
            params![now, reason, question_id, run_id],
        )?;
        if updated == 1 {
            let payload = serde_json::to_string(&serde_json::json!({
                "question_id": existing.id,
                "slice_id": existing.slice_id,
                "attempt": existing.attempt,
                "launch_id": existing.launch_id,
                "reason": reason
            }))?;
            tx.execute(
                "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_question_interrupted', ?2, ?3)",
                params![run_id, payload, now],
            )?;
        }
        let question = worker_question_by_id(&tx, question_id, Some(run_id))?
            .ok_or_else(|| anyhow::anyhow!("question disappeared during interruption"))?;
        tx.commit()?;
        Ok(question)
    }

    pub(crate) fn decide_worker_question_command(
        &self,
        run_id: &str,
        question_id: &str,
        command: WorkerQuestionDecisionCommand,
    ) -> Result<WorkerQuestionDecisionTransition> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(existing) = worker_question_by_id(&tx, question_id, Some(run_id))? else {
            tx.commit()?;
            return Ok(WorkerQuestionDecisionTransition {
                outcome: DecisionCommandOutcome::NotFound,
                question: None,
            });
        };

        match command {
            WorkerQuestionDecisionCommand::Answer {
                answer,
                answer_source,
                progress_message,
            } => {
                let resolution_command =
                    worker_question_answer_command_json(&answer, answer_source, &progress_message)?;
                if existing.state != "pending" {
                    let idempotent = existing.state == "answered"
                        && existing.answer == answer
                        && existing.answer_source == Some(answer_source)
                        && worker_question_resolution_command(&tx, question_id)?
                            == resolution_command;
                    tx.commit()?;
                    return Ok(WorkerQuestionDecisionTransition {
                        outcome: if idempotent {
                            DecisionCommandOutcome::AlreadyAppliedIdempotently
                        } else {
                            DecisionCommandOutcome::Conflict
                        },
                        question: Some(existing),
                    });
                }
                if !active_worker_attempt_with_launch_id(
                    &tx,
                    run_id,
                    &existing.slice_id,
                    existing.attempt,
                    existing.launch_id,
                )? {
                    tx.commit()?;
                    return Ok(WorkerQuestionDecisionTransition {
                        outcome: DecisionCommandOutcome::StaleToken,
                        question: Some(existing),
                    });
                }

                let now = Utc::now();
                if answer_source == WorkerQuestionAnswerSource::LlmRecommendationTimeout {
                    let recommendation = existing.recommendation();
                    if !existing.fallback_eligible || !recommendation.is_eligible(&existing.options)
                    {
                        anyhow::bail!(
                            "question {question_id:?} does not have an eligible durable recommendation"
                        );
                    }
                    if answer != existing.recommended_answer {
                        anyhow::bail!(
                            "question {question_id:?} timeout fallback must apply the exact durable recommendation"
                        );
                    }
                    let deadline_at = existing.deadline_at.ok_or_else(|| {
                        anyhow::anyhow!("question {question_id:?} has no fallback deadline")
                    })?;
                    if now < deadline_at {
                        anyhow::bail!(
                            "question {question_id:?} fallback cannot commit before its absolute deadline"
                        );
                    }
                }

                let now_text = now.to_rfc3339();
                let updated = tx.execute(
                    r#"UPDATE worker_questions
                       SET state='answered', answered_at=?1, answer=?2, answer_source=?3,
                           resolution_command_json=?4
                       WHERE id=?5 AND run_id=?6 AND state='pending'"#,
                    params![
                        &now_text,
                        &answer,
                        answer_source.as_str(),
                        &resolution_command,
                        question_id,
                        run_id
                    ],
                )?;
                if updated != 1 {
                    anyhow::bail!(
                        "question {question_id:?} changed before the answer could commit"
                    );
                }
                Self::transition_progress_after_worker_question_resolution(
                    &tx,
                    run_id,
                    &existing,
                    &progress_message,
                    &now_text,
                )?;
                let payload = crate::workflow::events::WorkerQuestionAnsweredPayload::from_question(
                    &existing,
                    &answer,
                    answer_source,
                );
                #[cfg(test)]
                if take_decision_transaction_fault(DecisionTransactionFaultStage::BeforeEventAppend)
                {
                    anyhow::bail!("injected decision event append failure");
                }
                tx.execute(
                    "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        run_id,
                        crate::workflow::events::WORKER_QUESTION_ANSWERED,
                        serde_json::to_string(&payload)?,
                        &now_text,
                    ],
                )?;
                let question = worker_question_by_id(&tx, question_id, Some(run_id))?
                    .ok_or_else(|| anyhow::anyhow!("question disappeared after answer"))?;
                tx.commit()?;
                Ok(WorkerQuestionDecisionTransition {
                    outcome: DecisionCommandOutcome::Applied,
                    question: Some(question),
                })
            }
            WorkerQuestionDecisionCommand::Timeout {
                expected_launch_id,
                apply_recommendation_at_deadline,
                incident_code,
                message_prefix,
                progress_message,
            } => {
                if existing.launch_id != expected_launch_id {
                    tx.commit()?;
                    return Ok(WorkerQuestionDecisionTransition {
                        outcome: DecisionCommandOutcome::StaleToken,
                        question: Some(existing),
                    });
                }
                if existing.state != "pending" {
                    if existing.state == "interrupted" {
                        tx.commit()?;
                        return Ok(WorkerQuestionDecisionTransition {
                            outcome: DecisionCommandOutcome::StaleToken,
                            question: Some(existing),
                        });
                    }
                    let resolution_command = worker_question_timeout_command_json(
                        expected_launch_id,
                        apply_recommendation_at_deadline,
                        &incident_code,
                        &message_prefix,
                        &progress_message,
                    )?;
                    let same_terminal_outcome = existing.state == "timed_out"
                        || (apply_recommendation_at_deadline
                            && existing.state == "answered"
                            && existing.answer_source
                                == Some(WorkerQuestionAnswerSource::LlmRecommendationTimeout));
                    let idempotent = same_terminal_outcome
                        && worker_question_resolution_command(&tx, question_id)?
                            == resolution_command;
                    tx.commit()?;
                    return Ok(WorkerQuestionDecisionTransition {
                        outcome: if idempotent {
                            DecisionCommandOutcome::AlreadyAppliedIdempotently
                        } else {
                            DecisionCommandOutcome::Conflict
                        },
                        question: Some(existing),
                    });
                }
                if !active_worker_attempt_with_launch_id(
                    &tx,
                    run_id,
                    &existing.slice_id,
                    existing.attempt,
                    existing.launch_id,
                )? {
                    let now_text = Utc::now().to_rfc3339();
                    let reason = "worker attempt became inactive before question resolution";
                    tx.execute(
                        r#"UPDATE worker_questions
                           SET state='interrupted', answered_at=?1, answer=?2
                           WHERE id=?3 AND run_id=?4 AND state='pending'"#,
                        params![&now_text, reason, question_id, run_id],
                    )?;
                    let payload = serde_json::json!({
                        "question_id": existing.id,
                        "slice_id": existing.slice_id,
                        "attempt": existing.attempt,
                        "launch_id": existing.launch_id,
                        "reason": reason,
                    });
                    #[cfg(test)]
                    if take_decision_transaction_fault(
                        DecisionTransactionFaultStage::BeforeEventAppend,
                    ) {
                        anyhow::bail!("injected decision event append failure");
                    }
                    tx.execute(
                        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_question_interrupted', ?2, ?3)",
                        params![run_id, serde_json::to_string(&payload)?, &now_text],
                    )?;
                    let question = worker_question_by_id(&tx, question_id, Some(run_id))?
                        .ok_or_else(|| {
                            anyhow::anyhow!("question disappeared after interruption")
                        })?;
                    tx.commit()?;
                    return Ok(WorkerQuestionDecisionTransition {
                        outcome: DecisionCommandOutcome::StaleToken,
                        question: Some(question),
                    });
                }

                let now = Utc::now();
                let now_text = now.to_rfc3339();
                let recommendation = existing.recommendation();
                let apply_recommendation = apply_recommendation_at_deadline
                    && existing.deadline_at.is_some_and(|deadline| now >= deadline)
                    && existing.fallback_eligible
                    && recommendation.is_eligible(&existing.options);
                if apply_recommendation {
                    let answer = existing.recommended_answer.clone();
                    let source = WorkerQuestionAnswerSource::LlmRecommendationTimeout;
                    let resolution_command = worker_question_timeout_command_json(
                        expected_launch_id,
                        apply_recommendation_at_deadline,
                        &incident_code,
                        &message_prefix,
                        &progress_message,
                    )?;
                    let updated = tx.execute(
                        r#"UPDATE worker_questions
                           SET state='answered', answered_at=?1, answer=?2, answer_source=?3,
                               resolution_command_json=?4
                           WHERE id=?5 AND run_id=?6 AND state='pending'"#,
                        params![
                            &now_text,
                            &answer,
                            source.as_str(),
                            &resolution_command,
                            question_id,
                            run_id
                        ],
                    )?;
                    if updated != 1 {
                        anyhow::bail!(
                            "question {question_id:?} changed before the recommendation could commit"
                        );
                    }
                    Self::transition_progress_after_worker_question_resolution(
                        &tx,
                        run_id,
                        &existing,
                        &format!(
                            "LLM recommendation applied at deadline for {}; worker resuming",
                            existing.id
                        ),
                        &now_text,
                    )?;
                    let payload =
                        crate::workflow::events::WorkerQuestionAnsweredPayload::from_question(
                            &existing, &answer, source,
                        );
                    #[cfg(test)]
                    if take_decision_transaction_fault(
                        DecisionTransactionFaultStage::BeforeEventAppend,
                    ) {
                        anyhow::bail!("injected decision event append failure");
                    }
                    tx.execute(
                        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            run_id,
                            crate::workflow::events::WORKER_QUESTION_ANSWERED,
                            serde_json::to_string(&payload)?,
                            &now_text,
                        ],
                    )?;
                } else {
                    let resolution_command = worker_question_timeout_command_json(
                        expected_launch_id,
                        apply_recommendation_at_deadline,
                        &incident_code,
                        &message_prefix,
                        &progress_message,
                    )?;
                    let updated = tx.execute(
                        r#"UPDATE worker_questions
                           SET state='timed_out', answered_at=?1, resolution_command_json=?2
                           WHERE id=?3 AND run_id=?4 AND state='pending'"#,
                        params![&now_text, &resolution_command, question_id, run_id],
                    )?;
                    if updated != 1 {
                        anyhow::bail!(
                            "question {question_id:?} changed before the timeout could commit"
                        );
                    }
                    Self::transition_progress_after_worker_question_resolution(
                        &tx,
                        run_id,
                        &existing,
                        &progress_message,
                        &now_text,
                    )?;
                    let incident = crate::workflow::events::RunIncidentPayload::warning(
                        incident_code,
                        format!("{message_prefix}: {}", existing.question),
                    )
                    .with_extra("question_id", &existing.id)
                    .with_extra("slice_id", &existing.slice_id);
                    #[cfg(test)]
                    if take_decision_transaction_fault(
                        DecisionTransactionFaultStage::BeforeEventAppend,
                    ) {
                        anyhow::bail!("injected decision event append failure");
                    }
                    tx.execute(
                        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            run_id,
                            crate::workflow::events::RUN_INCIDENT,
                            serde_json::to_string(&incident)?,
                            &now_text,
                        ],
                    )?;
                }
                let question = worker_question_by_id(&tx, question_id, Some(run_id))?
                    .ok_or_else(|| anyhow::anyhow!("question disappeared after resolution"))?;
                tx.commit()?;
                Ok(WorkerQuestionDecisionTransition {
                    outcome: DecisionCommandOutcome::Applied,
                    question: Some(question),
                })
            }
        }
    }

    fn transition_progress_after_worker_question_resolution(
        tx: &rusqlite::Transaction<'_>,
        run_id: &str,
        resolved: &WorkerQuestion,
        resume_message: &str,
        now: &str,
    ) -> Result<()> {
        let next_pending = tx
            .query_row(
                r#"SELECT id, slice_id, attempt
                   FROM worker_questions
                   WHERE run_id=?1 AND state='pending'
                   ORDER BY asked_at ASC, id ASC
                   LIMIT 1"#,
                params![run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((question_id, slice_id, attempt)) = next_pending {
            project_awaiting_operator_progress(
                tx,
                run_id,
                &slice_id,
                attempt as usize,
                &format!("awaiting operator answer for question {question_id}"),
                now,
            )?;
        } else {
            tx.execute(
                r#"UPDATE run_progress
                   SET phase='worker_running', command='ask_operator', message=?1,
                       output_tail='', phase_started_at=?2, updated_at=?2
                   WHERE run_id=?3 AND phase='awaiting_operator'
                     AND slice_id=?4 AND attempt=?5"#,
                params![
                    resume_message,
                    now,
                    run_id,
                    &resolved.slice_id,
                    resolved.attempt as i64,
                ],
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_replan_proposal(
        &self,
        run_id: &str,
        requested_id: &str,
        source: ReplanProposalSource,
        trigger_finding_ids: Vec<String>,
        evidence: Vec<ReplanEvidenceLink>,
        proposed_changes: Vec<ReplanProposedChange>,
        risk: &str,
    ) -> Result<ReplanProposal> {
        if self.get_run(run_id)?.is_none() {
            anyhow::bail!("run {run_id:?} not found");
        }
        if source.kind.trim().is_empty() {
            anyhow::bail!("replan proposal source kind is required");
        }
        if proposed_changes.is_empty() {
            anyhow::bail!("replan proposal requires at least one proposed change");
        }
        let conn = self.conn()?;
        let id = if requested_id.trim().is_empty() {
            next_replan_id(&conn, run_id)?
        } else {
            requested_id.trim().to_string()
        };
        let now = Utc::now();
        let proposal = ReplanProposal {
            id,
            run_id: run_id.to_string(),
            state: ReplanProposalState::Pending,
            source,
            trigger_finding_ids,
            evidence,
            proposed_changes,
            risk: if risk.trim().is_empty() {
                "operator_review".to_string()
            } else {
                risk.trim().to_string()
            },
            operator_decision: None,
            frontier_classification: None,
            created_at: now,
            updated_at: now,
            decision_commands: Vec::new(),
        };
        conn.execute(
            r#"INSERT INTO replan_proposals
               (id, run_id, state, source_json, trigger_finding_ids_json, evidence_json,
                proposed_changes_json, risk, decision_json, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', ?9, ?10)"#,
            params![
                &proposal.id,
                &proposal.run_id,
                proposal.state.as_str(),
                serde_json::to_string(&proposal.source)?,
                serde_json::to_string(&proposal.trigger_finding_ids)?,
                serde_json::to_string(&proposal.evidence)?,
                serde_json::to_string(&proposal.proposed_changes)?,
                &proposal.risk,
                proposal.created_at.to_rfc3339(),
                proposal.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(proposal)
    }

    pub fn get_replan_proposal(
        &self,
        run_id: &str,
        proposal_id: &str,
    ) -> Result<Option<ReplanProposal>> {
        let conn = self.conn()?;
        replan_proposal_by_id(&conn, run_id, proposal_id)
    }

    pub fn list_replan_proposals(&self, run_id: &str) -> Result<Vec<ReplanProposal>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, run_id, state, source_json, trigger_finding_ids_json,
                      evidence_json, proposed_changes_json, risk, decision_json,
                      frontier_classification_json, created_at, updated_at
               FROM replan_proposals WHERE run_id=?1 ORDER BY created_at ASC, id ASC"#,
        )?;
        let rows = stmt.query_map(params![run_id], replan_proposal_tuple_from_row)?;
        let mut proposals = Vec::new();
        for row in rows {
            proposals.push(replan_proposal_from_tuple(row?)?);
        }
        Ok(proposals)
    }

    pub fn pending_replan_proposals(&self, run_id: &str) -> Result<Vec<ReplanProposal>> {
        Ok(self
            .list_replan_proposals(run_id)?
            .into_iter()
            .filter(|proposal| proposal.state == ReplanProposalState::Pending)
            .collect())
    }

    pub(crate) fn decide_replan_proposal_command(
        &self,
        run_id: &str,
        proposal_id: &str,
        command: ReplanDecisionCommand,
    ) -> Result<ReplanDecisionTransition> {
        if command.state == ReplanProposalState::Pending {
            anyhow::bail!("replan decision cannot leave proposal pending");
        }
        if command.rationale.trim().is_empty() {
            anyhow::bail!("replan decision rationale is required");
        }
        if command.state == ReplanProposalState::Deferred
            && command.revisit_condition.trim().is_empty()
        {
            anyhow::bail!("replan defer requires --until <condition>");
        }
        if command.state == ReplanProposalState::Superseded
            && command.replacement_id.trim().is_empty()
        {
            anyhow::bail!("replan supersede requires a replacement proposal id");
        }
        if let Some(auto_accept) = &command.auto_accept {
            if auto_accept.classification.tier != "tier_1" {
                anyhow::bail!("frontier auto-accept requires a Tier-1 classification");
            }
            if !auto_accept
                .classification
                .reason_codes
                .iter()
                .any(|code| code == "add_followup_slice_only")
            {
                anyhow::bail!("frontier auto-accept requires add_followup_slice_only evidence");
            }
            if auto_accept.budget_after.auto_promotions_used
                != auto_accept
                    .budget_before
                    .auto_promotions_used
                    .saturating_add(1)
                || auto_accept.budget_after.generated_slices
                    != auto_accept.budget_before.generated_slices.saturating_add(1)
            {
                anyhow::bail!(
                    "frontier auto-accept must consume exactly one promotion and one generated-slice budget unit"
                );
            }
        }

        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(existing) = replan_proposal_by_id(&tx, run_id, proposal_id)? else {
            tx.commit()?;
            return Ok(ReplanDecisionTransition {
                outcome: DecisionCommandOutcome::NotFound,
                proposal: None,
            });
        };
        if existing.state != ReplanProposalState::Pending {
            let idempotent = command.matches(&existing)
                && command.supplemental_record_matches(&tx, &existing)?;
            tx.commit()?;
            return Ok(ReplanDecisionTransition {
                outcome: if idempotent {
                    DecisionCommandOutcome::AlreadyAppliedIdempotently
                } else {
                    DecisionCommandOutcome::Conflict
                },
                proposal: Some(existing),
            });
        }

        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let run_for_apply = run_by_id(&tx, run_id)?;
        let (authorizer, source, frontier_tier, frontier_reason_codes, budget_before, budget_after) =
            if let Some(auto_accept) = &command.auto_accept {
                let followup_only = matches!(existing.proposed_changes.as_slice(), [change]
                    if change.kind == "add_followup_slice" && change.followup_slice_draft().is_some());
                if !followup_only {
                    anyhow::bail!(
                        "frontier auto-accept is only valid for one typed add_followup_slice proposal"
                    );
                }
                let durable_budget_json: String = tx.query_row(
                    "SELECT frontier_budget_json FROM runs WHERE id=?1",
                    params![run_id],
                    |row| row.get(0),
                )?;
                let durable_budget = if durable_budget_json.trim().is_empty() {
                    FrontierBudgetState::default()
                } else {
                    serde_json::from_str(&durable_budget_json)
                        .with_context(|| format!("parse frontier budget for run {run_id}"))?
                };
                if durable_budget != auto_accept.budget_before {
                    tx.commit()?;
                    return Ok(ReplanDecisionTransition {
                        outcome: DecisionCommandOutcome::Conflict,
                        proposal: Some(existing),
                    });
                }
                tx.execute(
                    "UPDATE runs SET frontier_budget_json=?1, updated_at=?2 WHERE id=?3",
                    params![
                        serde_json::to_string(&auto_accept.budget_after)?,
                        &now_text,
                        run_id,
                    ],
                )?;
                (
                    format!("envelope:{run_id}"),
                    "frontier_policy".to_string(),
                    auto_accept.classification.tier.clone(),
                    auto_accept.classification.reason_codes.clone(),
                    Some(auto_accept.budget_before.clone()),
                    Some(auto_accept.budget_after.clone()),
                )
            } else {
                (
                    if command.authorizer.trim().is_empty() {
                        "operator".to_string()
                    } else {
                        command.authorizer.trim().to_string()
                    },
                    if command.source.trim().is_empty() {
                        "daemon_ipc".to_string()
                    } else {
                        command.source.trim().to_string()
                    },
                    String::new(),
                    Vec::new(),
                    None,
                    None,
                )
            };
        let decision = ReplanDecision {
            decision: command.state.as_str().to_string(),
            rationale: command.rationale.trim().to_string(),
            authorizer,
            source,
            decided_at: now,
            frontier_tier,
            frontier_reason_codes,
            frontier_budget_before: budget_before,
            frontier_budget_after: budget_after,
            applied: false,
            applied_at: None,
            apply_status: initial_replan_apply_status(
                &existing,
                command.state,
                run_for_apply.as_ref(),
            ),
            apply_reason: initial_replan_apply_reason(
                &existing,
                command.state,
                run_for_apply.as_ref(),
            ),
            generated_slice_id: initial_replan_generated_slice_id(&existing, command.state),
            generated_slice_generation: 0,
            generated_slice_commit: String::new(),
            apply_before_checkpoint_id: String::new(),
            apply_after_checkpoint_id: String::new(),
            queue_before: Vec::new(),
            queue_after: Vec::new(),
            queue_before_hash: String::new(),
            queue_after_hash: String::new(),
            replacement_id: command.replacement_id.trim().to_string(),
            revisit_condition: command.revisit_condition.trim().to_string(),
        };
        let updated = if let Some(auto_accept) = &command.auto_accept {
            tx.execute(
                r#"UPDATE replan_proposals
                   SET state=?1, decision_json=?2, frontier_classification_json=?3, updated_at=?4
                   WHERE run_id=?5 AND id=?6 AND state='pending'"#,
                params![
                    command.state.as_str(),
                    serde_json::to_string(&decision)?,
                    serde_json::to_string(&auto_accept.classification)?,
                    &now_text,
                    run_id,
                    proposal_id,
                ],
            )?
        } else {
            tx.execute(
                r#"UPDATE replan_proposals
                   SET state=?1, decision_json=?2, updated_at=?3
                   WHERE run_id=?4 AND id=?5 AND state='pending'"#,
                params![
                    command.state.as_str(),
                    serde_json::to_string(&decision)?,
                    &now_text,
                    run_id,
                    proposal_id,
                ],
            )?
        };
        if updated != 1 {
            anyhow::bail!(
                "replan proposal {proposal_id:?} changed before the decision could commit"
            );
        }
        let proposal = replan_proposal_by_id(&tx, run_id, proposal_id)?
            .ok_or_else(|| anyhow::anyhow!("replan proposal disappeared after decision"))?;
        let payload = crate::workflow::events::ReplanProposalDecidedPayload {
            proposal_id: proposal.id.clone(),
            state: proposal.state,
            decision: proposal.operator_decision.clone().ok_or_else(|| {
                anyhow::anyhow!("replan decision disappeared before event append")
            })?,
        };
        #[cfg(test)]
        if take_decision_transaction_fault(DecisionTransactionFaultStage::BeforeEventAppend) {
            anyhow::bail!("injected decision event append failure");
        }
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id,
                crate::workflow::events::REPLAN_PROPOSAL_DECIDED,
                serde_json::to_string(&payload)?,
                &now_text,
            ],
        )?;
        if let Some((auto_accept, record)) = command.auto_accept.as_ref().and_then(|auto_accept| {
            auto_accept
                .record
                .as_ref()
                .map(|record| (auto_accept, record))
        }) {
            let decision = proposal
                .operator_decision
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("replan decision disappeared before audit event"))?;
            let classification_record =
                record.classification_payload(&proposal, &auto_accept.classification);
            let auto_accept_record = record.payload(&proposal, decision, auto_accept);
            #[cfg(test)]
            if take_decision_transaction_fault(
                DecisionTransactionFaultStage::BeforeSupplementalEventAppend,
            ) {
                anyhow::bail!("injected supplemental decision event append failure");
            }
            tx.execute(
                "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    run_id,
                    crate::workflow::events::FRONTIER_CLASSIFIED,
                    serde_json::to_string(&classification_record)?,
                    &now_text,
                ],
            )?;
            tx.execute(
                "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    run_id,
                    crate::workflow::events::FRONTIER_AUTO_ACCEPT_RECORDED,
                    serde_json::to_string(&auto_accept_record)?,
                    &now_text,
                ],
            )?;
        }
        tx.commit()?;
        Ok(ReplanDecisionTransition {
            outcome: DecisionCommandOutcome::Applied,
            proposal: Some(proposal),
        })
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn decide_replan_proposal(
        &self,
        run_id: &str,
        proposal_id: &str,
        state: ReplanProposalState,
        rationale: &str,
        authorizer: &str,
        source: &str,
        replacement_id: &str,
        revisit_condition: &str,
    ) -> Result<ReplanProposal> {
        let transition = self.decide_replan_proposal_command(
            run_id,
            proposal_id,
            ReplanDecisionCommand::operator(
                state,
                rationale,
                authorizer,
                source,
                replacement_id,
                revisit_condition,
            ),
        )?;
        match transition.outcome {
            DecisionCommandOutcome::Applied
            | DecisionCommandOutcome::AlreadyAppliedIdempotently => transition
                .proposal
                .ok_or_else(|| anyhow::anyhow!("replan proposal disappeared after decision")),
            DecisionCommandOutcome::NotFound => {
                anyhow::bail!("replan proposal {proposal_id:?} for run {run_id:?} not found")
            }
            outcome => anyhow::bail!(
                "replan decision for {proposal_id:?} returned {}",
                outcome.as_str()
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn auto_accept_replan_proposal_with_budget(
        &self,
        run_id: &str,
        proposal_id: &str,
        rationale: &str,
        classification: &FrontierClassification,
        budget_before: &FrontierBudgetState,
        budget_after: &FrontierBudgetState,
        checkpoint: &str,
        apply_mode: &str,
    ) -> Result<ReplanProposal> {
        let transition = self.decide_replan_proposal_command(
            run_id,
            proposal_id,
            ReplanDecisionCommand::auto_accept_recorded(
                rationale,
                classification.clone(),
                budget_before.clone(),
                budget_after.clone(),
                checkpoint,
                apply_mode,
            ),
        )?;
        match transition.outcome {
            DecisionCommandOutcome::Applied
            | DecisionCommandOutcome::AlreadyAppliedIdempotently => transition
                .proposal
                .ok_or_else(|| anyhow::anyhow!("replan proposal disappeared after auto-accept")),
            outcome => anyhow::bail!(
                "frontier auto-accept for {proposal_id:?} returned {}",
                outcome.as_str()
            ),
        }
    }

    pub fn replace_replan_decision(
        &self,
        run_id: &str,
        proposal_id: &str,
        decision: &ReplanDecision,
    ) -> Result<ReplanProposal> {
        let existing = self
            .get_replan_proposal(run_id, proposal_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("replan proposal {proposal_id:?} for run {run_id:?} not found")
            })?;
        if existing.operator_decision.is_none() {
            anyhow::bail!("replan proposal {proposal_id:?} has no decision to update");
        }
        let now = Utc::now();
        let conn = self.conn()?;
        conn.execute(
            r#"UPDATE replan_proposals
               SET decision_json=?1, updated_at=?2
               WHERE run_id=?3 AND id=?4"#,
            params![
                serde_json::to_string(decision)?,
                now.to_rfc3339(),
                run_id,
                proposal_id,
            ],
        )?;
        self.get_replan_proposal(run_id, proposal_id)?
            .ok_or_else(|| anyhow::anyhow!("replan proposal disappeared after decision update"))
    }

    pub fn replace_replan_frontier_classification(
        &self,
        run_id: &str,
        proposal_id: &str,
        classification: &FrontierClassification,
    ) -> Result<ReplanProposal> {
        self.get_replan_proposal(run_id, proposal_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("replan proposal {proposal_id:?} for run {run_id:?} not found")
            })?;
        let now = Utc::now();
        let conn = self.conn()?;
        conn.execute(
            r#"UPDATE replan_proposals
               SET frontier_classification_json=?1, updated_at=?2
               WHERE run_id=?3 AND id=?4"#,
            params![
                serde_json::to_string(classification)?,
                now.to_rfc3339(),
                run_id,
                proposal_id,
            ],
        )?;
        self.get_replan_proposal(run_id, proposal_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("replan proposal disappeared after classification update")
            })
    }

    pub fn update_run_selected_slices(&self, run_id: &str, selected_slice_id: &str) -> Result<Run> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE runs SET selected_slice_id=?1, updated_at=?2 WHERE id=?3",
            params![selected_slice_id, Utc::now().to_rfc3339(), run_id],
        )?;
        self.get_run(run_id)?
            .ok_or_else(|| anyhow::anyhow!("run {run_id:?} disappeared after queue update"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_progress(
        &self,
        run_id: &str,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        command: &str,
        message: &str,
        output_tail: &str,
    ) -> Result<RunProgress> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run_status = tx
            .query_row(
                "SELECT status FROM runs WHERE id=?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|status| RunStatus::parse(&status))
            .transpose()?;
        let projected_phase = tx
            .query_row(
                "SELECT phase FROM run_progress WHERE run_id=?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let terminal_run = run_status
            .is_some_and(|status| !matches!(status, RunStatus::Pending | RunStatus::Running));
        let terminal_run_rejects_lifecycle_write =
            terminal_run && phase != "awaiting_replan" && !is_terminal_progress_phase(phase);
        let terminal_transition_in_progress = projected_phase
            .as_deref()
            .is_some_and(is_terminal_progress_phase)
            && !is_terminal_progress_phase(phase)
            && !matches!(phase, "started" | "resumed" | "awaiting_replan");
        if terminal_run_rejects_lifecycle_write || terminal_transition_in_progress {
            tx.commit()?;
            return self.get_progress(run_id)?.ok_or_else(|| {
                anyhow::anyhow!("terminal run {run_id:?} has no durable progress projection")
            });
        }
        let pending_question = tx
            .query_row(
                r#"SELECT id, slice_id, attempt
                   FROM worker_questions
                   WHERE run_id=?1 AND state='pending'
                   ORDER BY asked_at ASC, id ASC
                   LIMIT 1"#,
                params![run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as usize,
                    ))
                },
            )
            .optional()?;
        if let Some((question_id, pending_slice_id, pending_attempt)) = pending_question {
            let already_projected = tx
                .query_row(
                    r#"SELECT 1 FROM run_progress
                       WHERE run_id=?1 AND phase='awaiting_operator'
                         AND slice_id=?2 AND attempt=?3"#,
                    params![run_id, &pending_slice_id, pending_attempt as i64],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !already_projected {
                let now = Utc::now().to_rfc3339();
                project_awaiting_operator_progress(
                    &tx,
                    run_id,
                    &pending_slice_id,
                    pending_attempt,
                    &format!("awaiting operator answer for question {question_id}"),
                    &now,
                )?;
            }
            tx.commit()?;
            return self.get_progress(run_id)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "run progress disappeared while projecting pending question {question_id:?}"
                )
            });
        }
        let previous: Option<(String, String, i64, String, String)> = tx
            .query_row(
                "SELECT phase, slice_id, attempt, command, phase_started_at FROM run_progress WHERE run_id=?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        let same_phase = previous.as_ref().is_some_and(
            |(old_phase, old_slice_id, old_attempt, old_command, _)| {
                old_phase == phase
                    && old_slice_id == slice_id
                    && *old_attempt == attempt as i64
                    && old_command == command
            },
        );
        let now = Utc::now();
        let phase_started_at = if same_phase {
            previous
                .as_ref()
                .map(|(_, _, _, _, started)| started.clone())
                .unwrap_or_else(|| now.to_rfc3339())
        } else {
            now.to_rfc3339()
        };
        tx.execute(
            r#"INSERT INTO run_progress
               (run_id, phase, slice_id, attempt, command, message, output_tail, phase_started_at,
                updated_at, worker_attempt_started_at, worker_pid, worker_process_observed_at,
                worker_last_event_at, worker_last_event_kind, worker_last_semantic_progress_at,
                worker_last_semantic_progress_summary, worker_attempt_timeout_seconds,
                worker_no_output_warning_seconds)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '', NULL, '', '', '', '', '', 0, 0)
               ON CONFLICT(run_id) DO UPDATE SET
                 phase=excluded.phase,
                 slice_id=excluded.slice_id,
                 attempt=excluded.attempt,
                 command=excluded.command,
                 message=excluded.message,
                 output_tail=excluded.output_tail,
                 phase_started_at=excluded.phase_started_at,
                 updated_at=excluded.updated_at,
                 worker_attempt_started_at=excluded.worker_attempt_started_at,
                 worker_pid=excluded.worker_pid,
                 worker_process_observed_at=excluded.worker_process_observed_at,
                 worker_last_event_at=excluded.worker_last_event_at,
                 worker_last_event_kind=excluded.worker_last_event_kind,
                 worker_last_semantic_progress_at=excluded.worker_last_semantic_progress_at,
                 worker_last_semantic_progress_summary=excluded.worker_last_semantic_progress_summary,
                 worker_attempt_timeout_seconds=excluded.worker_attempt_timeout_seconds,
                 worker_no_output_warning_seconds=excluded.worker_no_output_warning_seconds"#,
            params![
                run_id,
                phase,
                slice_id,
                attempt as i64,
                command,
                message,
                output_tail,
                phase_started_at,
                now.to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(RunProgress {
            run_id: run_id.to_string(),
            phase: phase.to_string(),
            slice_id: slice_id.to_string(),
            attempt,
            command: command.to_string(),
            message: message.to_string(),
            output_tail: output_tail.to_string(),
            phase_started_at: parse_time("phase_started_at", &phase_started_at)?,
            updated_at: now,
            worker: None,
            parallel_layer: false,
            parallel_slices: Vec::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe_worker_attempt(
        &self,
        run_id: &str,
        phase: &str,
        slice_id: &str,
        attempt: usize,
        launch_id: Option<i64>,
        pid: Option<u32>,
        event_kind: &str,
        event_text: &str,
        attempt_timeout_seconds: u64,
        no_output_warning_seconds: u64,
    ) -> Result<Option<RunProgress>> {
        let conn = self.conn()?;
        if launch_id.is_some()
            && !active_worker_attempt_with_launch_id(&conn, run_id, slice_id, attempt, launch_id)?
        {
            return self.get_progress(run_id);
        }
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let is_process_observation = matches!(event_kind, "started" | "process_observed");
        let is_worker_event = matches!(event_kind, "stdout" | "stderr");
        let semantic_progress = if event_kind == "stdout" {
            pi_contract::semantic_progress_from_stdout_line(event_text)
        } else {
            None
        };
        let has_semantic_progress = semantic_progress.is_some();
        let semantic_progress_summary = semantic_progress
            .as_ref()
            .map(|progress| progress.summary.as_str())
            .unwrap_or_default();
        if let Some(launch_id) = launch_id {
            let updated = conn.execute(
                r#"UPDATE worker_attempt_ledger SET
                     worker_pid=COALESCE(?1, worker_pid),
                     worker_process_observed_at=CASE
                       WHEN ?2 THEN ?3
                       ELSE worker_process_observed_at
                     END,
                     worker_last_event_at=CASE
                       WHEN ?4 THEN ?3
                       ELSE worker_last_event_at
                     END,
                     worker_last_event_kind=CASE
                       WHEN ?4 THEN ?5
                       ELSE worker_last_event_kind
                     END,
                     worker_last_semantic_progress_at=CASE
                       WHEN ?6 THEN ?3
                       ELSE worker_last_semantic_progress_at
                     END,
                     worker_last_semantic_progress_summary=CASE
                       WHEN ?6 THEN ?7
                       ELSE worker_last_semantic_progress_summary
                     END,
                     worker_attempt_timeout_seconds=?8,
                     worker_no_output_warning_seconds=?9
                   WHERE launch_id=?10 AND run_id=?11 AND slice_id=?12 AND state='running'"#,
                params![
                    pid.map(|pid| pid as i64),
                    is_process_observation,
                    &now_text,
                    is_worker_event,
                    event_kind,
                    has_semantic_progress,
                    semantic_progress_summary,
                    attempt_timeout_seconds as i64,
                    no_output_warning_seconds as i64,
                    launch_id,
                    run_id,
                    slice_id,
                ],
            )?;
            if updated != 1 {
                return self.get_progress(run_id);
            }
        }
        conn.execute(
            r#"UPDATE run_progress SET
                 updated_at=?1,
                 worker_attempt_started_at=CASE
                   WHEN worker_attempt_started_at='' THEN ?1
                   ELSE worker_attempt_started_at
                 END,
                 worker_pid=COALESCE(?2, worker_pid),
                 worker_process_observed_at=CASE
                   WHEN ?3 THEN ?1
                   ELSE worker_process_observed_at
                 END,
                 worker_last_event_at=CASE
                   WHEN ?4 THEN ?1
                   ELSE worker_last_event_at
                 END,
                 worker_last_event_kind=CASE
                   WHEN ?4 THEN ?5
                   ELSE worker_last_event_kind
                 END,
                 worker_last_semantic_progress_at=CASE
                   WHEN ?6 THEN ?1
                   ELSE worker_last_semantic_progress_at
                 END,
                 worker_last_semantic_progress_summary=CASE
                   WHEN ?6 THEN ?7
                   ELSE worker_last_semantic_progress_summary
                 END,
                 worker_attempt_timeout_seconds=?8,
                 worker_no_output_warning_seconds=?9
               WHERE run_id=?10 AND phase=?11 AND slice_id=?12 AND attempt=?13"#,
            params![
                now_text,
                pid.map(|pid| pid as i64),
                is_process_observation,
                is_worker_event,
                event_kind,
                has_semantic_progress,
                semantic_progress_summary,
                attempt_timeout_seconds as i64,
                no_output_warning_seconds as i64,
                run_id,
                phase,
                slice_id,
                attempt as i64,
            ],
        )?;
        self.get_progress(run_id)
    }

    pub fn get_progress(&self, run_id: &str) -> Result<Option<RunProgress>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                r#"SELECT run_id, phase, slice_id, attempt, command, message, output_tail,
                          phase_started_at, updated_at, worker_attempt_started_at, worker_pid,
                          worker_process_observed_at, worker_last_event_at, worker_last_event_kind,
                          worker_last_semantic_progress_at, worker_last_semantic_progress_summary,
                          worker_attempt_timeout_seconds, worker_no_output_warning_seconds
                   FROM run_progress WHERE run_id=?1"#,
                params![run_id],
                run_progress_tuple_from_row,
            )
            .optional()?;
        let mut progress = row.map(run_progress_from_tuple).transpose()?;
        if let Some(progress) = progress.as_mut()
            && let Some(worker) = progress.worker.as_mut()
        {
            worker.launch_id = conn
                .query_row(
                    r#"SELECT launch_id FROM worker_attempt_ledger
                       WHERE run_id=?1 AND slice_id=?2 AND state='running'
                         AND (
                           (kind='integration-repair' AND repair_ordinal=?3)
                           OR (kind<>'integration-repair' AND worker_retry_ordinal=?3)
                         )
                       ORDER BY launch_id DESC LIMIT 1"#,
                    params![run_id, &progress.slice_id, progress.attempt as i64],
                    |row| row.get(0),
                )
                .optional()?;
        }
        Ok(progress)
    }

    /// Persist a live external projection inside SQLite so status readers never
    /// have to combine a transactional revision with an opportunistic file
    /// read. The payload and its causal event frontier are one durable row.
    pub(crate) fn record_status_source_snapshot<T, C>(
        &self,
        run_id: &str,
        source: &str,
        capture_payload: C,
    ) -> Result<()>
    where
        T: Serialize,
        C: FnOnce() -> T,
    {
        self.record_status_source_snapshot_inner(run_id, source, capture_payload, || {})
    }

    #[cfg(test)]
    fn record_status_source_snapshot_with_hook<T, C, H>(
        &self,
        run_id: &str,
        source: &str,
        capture_payload: C,
        after_capture: H,
    ) -> Result<()>
    where
        T: Serialize,
        C: FnOnce() -> T,
        H: FnOnce(),
    {
        self.record_status_source_snapshot_inner(run_id, source, capture_payload, after_capture)
    }

    fn record_status_source_snapshot_inner<T, C, H>(
        &self,
        run_id: &str,
        source: &str,
        capture_payload: C,
        after_capture: H,
    ) -> Result<()>
    where
        T: Serialize,
        C: FnOnce() -> T,
        H: FnOnce(),
    {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run_exists = tx
            .query_row(
                "SELECT 1 FROM runs WHERE id=?1",
                params![run_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !run_exists {
            anyhow::bail!("cannot index status source {source:?}: run {run_id:?} not found");
        }
        // Capture and serialize while the immediate transaction excludes event
        // writers, then bind exactly that payload to the in-transaction frontier.
        let payload_json = serde_json::to_string(&capture_payload())?;
        let content_sha256 = format!("{:x}", Sha256::digest(payload_json.as_bytes()));
        after_capture();
        let indexed_event_id = tx.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM events WHERE run_id=?1",
            params![run_id],
            |row| row.get::<_, i64>(0),
        )?;
        tx.execute(
            r#"INSERT INTO status_source_snapshots
               (run_id, source, payload_json, indexed_event_id, content_sha256, observed_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(run_id, source) DO UPDATE SET
                 payload_json=excluded.payload_json,
                 indexed_event_id=excluded.indexed_event_id,
                 content_sha256=excluded.content_sha256,
                 observed_at=excluded.observed_at"#,
            params![
                run_id,
                source,
                payload_json,
                indexed_event_id,
                content_sha256,
                Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn status_snapshot(
        &self,
        run_id: &str,
        events_limit: usize,
    ) -> Result<Option<RunStateSnapshot>> {
        self.status_snapshot_inner(StatusSnapshotSelector::RunId(run_id), events_limit, || {
            Ok(())
        })
    }

    pub(crate) fn latest_status_snapshot(
        &self,
        repo_path: &str,
        active_only: bool,
        events_limit: usize,
    ) -> Result<Option<RunStateSnapshot>> {
        self.status_snapshot_inner(
            StatusSnapshotSelector::LatestRepo {
                repo_path,
                active_only,
            },
            events_limit,
            || Ok(()),
        )
    }

    #[cfg(test)]
    fn status_snapshot_with_hook<F>(
        &self,
        run_id: &str,
        events_limit: usize,
        hook: F,
    ) -> Result<Option<RunStateSnapshot>>
    where
        F: FnOnce() -> Result<()>,
    {
        self.status_snapshot_inner(StatusSnapshotSelector::RunId(run_id), events_limit, hook)
    }

    #[cfg(test)]
    fn latest_status_snapshot_with_hook<F>(
        &self,
        repo_path: &str,
        active_only: bool,
        events_limit: usize,
        hook: F,
    ) -> Result<Option<RunStateSnapshot>>
    where
        F: FnOnce() -> Result<()>,
    {
        self.status_snapshot_inner(
            StatusSnapshotSelector::LatestRepo {
                repo_path,
                active_only,
            },
            events_limit,
            hook,
        )
    }

    fn status_snapshot_inner<F>(
        &self,
        selector: StatusSnapshotSelector<'_>,
        events_limit: usize,
        hook: F,
    ) -> Result<Option<RunStateSnapshot>>
    where
        F: FnOnce() -> Result<()>,
    {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        // This lookup is intentionally the first table read: it establishes the
        // snapshot root and prevents a separately fetched Run from authorizing
        // a projection assembled from a newer revision.
        let run = match selector {
            StatusSnapshotSelector::RunId(run_id) => run_by_id(&tx, run_id)?,
            StatusSnapshotSelector::LatestRepo {
                repo_path,
                active_only,
            } => latest_run_for_repo_conn(&tx, repo_path, active_only)?,
        };
        let Some(run) = run else {
            tx.commit()?;
            return Ok(None);
        };
        let sqlite_data_version = tx.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        hook()?;

        let slice_runs = status_slice_runs(&tx, &run.id)?;
        let worker_attempts = status_worker_attempts(&tx, &run.id)?;
        let progress = status_progress(&tx, &run.id)?;
        let questions = status_worker_questions(&tx, &run.id)?;
        let replan_proposals = status_replan_proposals(&tx, &run.id)?;
        let (mission_envelope, frontier_budget) = status_frontier_state(&tx, &run.id)?;
        let events = status_events(&tx, &run.id)?;
        let limit = if events_limit == 0 { 50 } else { events_limit };
        let event_tail = events
            .iter()
            .skip(events.len().saturating_sub(limit))
            .cloned()
            .collect();
        let max_event_id = events.last().map_or(0, |event| event.id);
        let terminal_transition = status_terminal_transition(&tx, &run.id)?;
        let launch_intents = status_run_launch_intents(&tx, &run.id)?;
        let merge_intents = status_integration_merge_intents(&tx, &run.id)?;
        let status_sources = status_source_snapshots(&tx, &run.id)?;
        let revision = StatusSnapshotRevision {
            sqlite_data_version,
            max_event_id,
            run_updated_at: Some(run.updated_at),
            captured_at: Some(Utc::now()),
        };
        tx.commit()?;
        Ok(Some(RunStateSnapshot {
            revision,
            run,
            slice_runs,
            worker_attempts,
            progress,
            questions,
            replan_proposals,
            mission_envelope,
            frontier_budget,
            events,
            event_tail,
            terminal_transition,
            launch_intents,
            merge_intents,
            status_sources,
        }))
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let conn = self.conn()?;
        run_by_id(&conn, id)
    }

    pub fn get_slice_runs(&self, run_id: &str) -> Result<Vec<SliceRun>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT run_id, slice_id, status, branch, commit_sha, attempts, last_error
               FROM slice_runs WHERE run_id=?1 ORDER BY slice_id"#,
        )?;
        let rows = stmt.query_map(params![run_id], slice_run_tuple_from_row)?;
        let mut slice_runs = Vec::new();
        for row in rows {
            slice_runs.push(slice_run_from_tuple(row?)?);
        }
        Ok(slice_runs)
    }

    pub fn get_events(&self, run_id: &str, limit: usize) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let limit = if limit == 0 { 50 } else { limit };
        let mut stmt = conn.prepare(
            r#"SELECT id, run_id, type, payload_json, created_at
               FROM events WHERE run_id=?1 ORDER BY id DESC LIMIT ?2"#,
        )?;
        let rows = stmt.query_map(params![run_id, limit as i64], event_tuple_from_row)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(event_from_tuple(row?)?);
        }
        events.reverse();
        Ok(events)
    }

    pub fn active_runs(&self) -> Result<Vec<Run>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                      selected_slice_id, error, started_at, updated_at
               FROM runs WHERE status IN (?1, ?2) ORDER BY started_at ASC"#,
        )?;
        let rows = stmt.query_map(
            params![RunStatus::Pending.as_str(), RunStatus::Running.as_str()],
            run_tuple_from_row,
        )?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(run_from_tuple(row?)?);
        }
        Ok(runs)
    }

    pub fn latest_runs(&self, limit: usize) -> Result<Vec<Run>> {
        let conn = self.conn()?;
        let limit = if limit == 0 { 10 } else { limit };
        let mut stmt = conn.prepare(
            r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                      selected_slice_id, error, started_at, updated_at
               FROM runs ORDER BY started_at DESC, id DESC LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit as i64], run_tuple_from_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(run_from_tuple(row?)?);
        }
        Ok(runs)
    }
}

fn interrupt_active_worker_attempts(
    tx: &Transaction<'_>,
    run_id: &str,
    reason: &str,
    terminal_status: RunStatus,
) -> Result<usize> {
    let active_attempts = {
        let mut statement = tx.prepare(
            r#"SELECT launch_id, slice_id, state
               FROM worker_attempt_ledger
               WHERE run_id=?1 AND state IN ('allocated', 'running')
               ORDER BY launch_id ASC"#,
        )?;
        let rows = statement.query_map(params![run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if active_attempts.is_empty() {
        return Ok(0);
    }
    let now = Utc::now().to_rfc3339();
    for (launch_id, slice_id, prior_state) in &active_attempts {
        tx.execute(
            r#"UPDATE worker_attempt_ledger
               SET state='interrupted', finished_at=?1, failure_cause=?2
               WHERE launch_id=?3 AND state IN ('allocated', 'running')"#,
            params![now, reason, launch_id],
        )?;
        let payload = serde_json::json!({
            "launch_id": launch_id,
            "slice_id": slice_id,
            "reason": reason,
            "prior_state": prior_state,
            "terminal_status": terminal_status,
        });
        tx.execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, 'worker_attempt_interrupted', ?2, ?3)",
            params![run_id, serde_json::to_string(&payload)?, now],
        )?;
    }
    Ok(active_attempts.len())
}

fn insert_run_tx(
    conn: &Connection,
    run: &Run,
    mission_envelope: Option<&MissionEnvelope>,
    frontier_budget: Option<&FrontierBudgetState>,
) -> Result<()> {
    let envelope_json = mission_envelope
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_default();
    let budget_json = frontier_budget
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_default();
    conn.execute(
        r#"INSERT INTO runs
           (id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
            selected_slice_id, error, execution_epoch, mission_envelope_json,
            frontier_budget_json, started_at, updated_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11, ?12, ?13)"#,
        params![
            &run.id,
            &run.repo_id,
            &run.repo_path,
            run.status.as_str(),
            &run.base_branch,
            &run.base_sha,
            &run.integration_branch,
            &run.selected_slice_id,
            &run.error,
            envelope_json,
            budget_json,
            run.started_at.to_rfc3339(),
            run.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn upsert_slice_run_tx(conn: &Connection, slice_run: &SliceRun) -> Result<()> {
    conn.execute(
        r#"INSERT INTO slice_runs (run_id, slice_id, status, branch, commit_sha, attempts, last_error)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
           ON CONFLICT(run_id, slice_id) DO UPDATE SET
             status=excluded.status,
             branch=excluded.branch,
             commit_sha=excluded.commit_sha,
             attempts=excluded.attempts,
             last_error=excluded.last_error"#,
        params![
            &slice_run.run_id,
            &slice_run.slice_id,
            slice_run.status.as_str(),
            &slice_run.branch,
            &slice_run.commit_sha,
            slice_run.attempts as i64,
            &slice_run.last_error,
        ],
    )?;
    Ok(())
}

fn insert_run_launch_intent_tx(conn: &Connection, intent: &RunLaunchIntent) -> Result<()> {
    conn.execute(
        r#"INSERT INTO run_launch_intents
           (run_id, execution_epoch, action, state, repo_id, integration_branch,
            integration_worktree, integration_resources_owned, prior_status, prior_error,
            primary_cause, compensation_error, created_at, updated_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
        params![
            &intent.run_id,
            intent.execution_epoch as i64,
            intent.action.as_str(),
            intent.state.as_str(),
            &intent.repo_id,
            &intent.integration_branch,
            &intent.integration_worktree,
            intent.integration_resources_owned,
            intent
                .prior_status
                .map(RunStatus::as_str)
                .unwrap_or_default(),
            &intent.prior_error,
            &intent.primary_cause,
            &intent.compensation_error,
            intent.created_at.to_rfc3339(),
            intent.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn insert_event_tx<T: Serialize>(
    conn: &Connection,
    run_id: &str,
    event_type: &str,
    payload: &T,
    created_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events (run_id, type, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            run_id,
            event_type,
            serde_json::to_string(payload)?,
            created_at
        ],
    )?;
    Ok(())
}

fn active_run_for_repo_conn(
    conn: &Connection,
    repo_id: &str,
    exclude_run_id: Option<&str>,
) -> Result<Option<Run>> {
    let row = conn
        .query_row(
            r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha,
                      integration_branch, selected_slice_id, error, started_at, updated_at
               FROM runs
               WHERE repo_id=?1 AND status IN ('pending', 'running')
                 AND (?2 IS NULL OR id<>?2)
               ORDER BY updated_at DESC, id DESC LIMIT 1"#,
            params![repo_id, exclude_run_id],
            run_tuple_from_row,
        )
        .optional()?;
    row.map(run_from_tuple).transpose()
}

fn run_launch_intent_by_key(
    conn: &Connection,
    run_id: &str,
    execution_epoch: usize,
) -> Result<Option<RunLaunchIntent>> {
    conn.query_row(
        r#"SELECT run_id, execution_epoch, action, state, repo_id, integration_branch,
                  integration_worktree, integration_resources_owned, prior_status, prior_error,
                  primary_cause, compensation_error, created_at, updated_at
           FROM run_launch_intents WHERE run_id=?1 AND execution_epoch=?2"#,
        params![run_id, execution_epoch as i64],
        run_launch_intent_tuple_from_row,
    )
    .optional()?
    .map(run_launch_intent_from_tuple)
    .transpose()
}

fn insert_integration_merge_intent_tx(
    conn: &Connection,
    intent: &IntegrationMergeIntent,
) -> Result<()> {
    conn.execute(
        r#"INSERT INTO integration_merge_intents
           (operation_id, run_id, kind, slice_id, attempt, launch_id, source_branch,
            source_commit, source_tree, expected_head, expected_result_tree, resulting_head,
            state, completion_json, primary_cause, abort_error, conflicted_files_json,
            created_at, updated_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                   ?14, ?15, ?16, ?17, ?18, ?19)"#,
        params![
            &intent.operation_id,
            &intent.run_id,
            intent.kind.as_str(),
            &intent.slice_id,
            intent.attempt as i64,
            intent.launch_id,
            &intent.source_branch,
            &intent.source_commit,
            &intent.source_tree,
            &intent.expected_head,
            &intent.expected_result_tree,
            &intent.resulting_head,
            intent.state.as_str(),
            serde_json::to_string(&intent.completion)?,
            &intent.primary_cause,
            &intent.abort_error,
            serde_json::to_string(&intent.conflicted_files)?,
            intent.created_at.to_rfc3339(),
            intent.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn integration_merge_authority_matches(
    existing: &IntegrationMergeIntent,
    command: &IntegrationMergeIntent,
) -> bool {
    existing.operation_id == command.operation_id
        && existing.run_id == command.run_id
        && existing.kind == command.kind
        && existing.slice_id == command.slice_id
        && existing.attempt == command.attempt
        && existing.launch_id == command.launch_id
        && existing.source_branch == command.source_branch
        && existing.source_commit == command.source_commit
        && existing.source_tree == command.source_tree
        && existing.expected_head == command.expected_head
        && existing.expected_result_tree == command.expected_result_tree
        && existing.completion == command.completion
}

fn integration_merge_intent_by_id(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<IntegrationMergeIntent>> {
    conn.query_row(
        r#"SELECT operation_id, run_id, kind, slice_id, attempt, launch_id,
                  source_branch, source_commit, source_tree, expected_head,
                  expected_result_tree, resulting_head, state, completion_json,
                  primary_cause, abort_error, conflicted_files_json, created_at, updated_at
           FROM integration_merge_intents WHERE operation_id=?1"#,
        params![operation_id],
        integration_merge_intent_tuple_from_row,
    )
    .optional()?
    .map(integration_merge_intent_from_tuple)
    .transpose()
}

fn prepared_integration_merge_for_run(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<IntegrationMergeIntent>> {
    conn.query_row(
        r#"SELECT operation_id, run_id, kind, slice_id, attempt, launch_id,
                  source_branch, source_commit, source_tree, expected_head,
                  expected_result_tree, resulting_head, state, completion_json,
                  primary_cause, abort_error, conflicted_files_json, created_at, updated_at
           FROM integration_merge_intents WHERE run_id=?1 AND state='prepared'
           ORDER BY created_at, operation_id LIMIT 1"#,
        params![run_id],
        integration_merge_intent_tuple_from_row,
    )
    .optional()?
    .map(integration_merge_intent_from_tuple)
    .transpose()
}

fn apply_integration_merge_completion_tx(
    conn: &Connection,
    intent: &IntegrationMergeIntent,
    now: &str,
) -> Result<()> {
    match &intent.completion {
        IntegrationMergeCompletion::Slice {
            branch,
            commit_sha,
            attempts,
        } => {
            let slice_run = SliceRun {
                run_id: intent.run_id.clone(),
                slice_id: intent.slice_id.clone(),
                status: SliceStatus::Merged,
                branch: branch.clone(),
                commit_sha: commit_sha.clone(),
                attempts: *attempts,
                last_error: String::new(),
            };
            upsert_slice_run_tx(conn, &slice_run)?;
            insert_event_tx(
                conn,
                &intent.run_id,
                "slice_merged",
                &serde_json::json!({
                    "slice_id": intent.slice_id,
                    "commit_sha": commit_sha,
                }),
                now,
            )?;
        }
        IntegrationMergeCompletion::IntegrationRepair {
            launch_id,
            status,
            summary,
        } => {
            if intent.launch_id != Some(*launch_id) {
                anyhow::bail!(
                    "integration repair merge {:?} has inconsistent launch identity",
                    intent.operation_id
                );
            }
            let (run_id, slice_id, prior_state): (String, String, String) = conn
                .query_row(
                    "SELECT run_id, slice_id, state FROM worker_attempt_ledger WHERE launch_id=?1",
                    params![launch_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .context("integration repair merge lost its worker attempt")?;
            if run_id != intent.run_id || !matches!(prior_state.as_str(), "running" | "interrupted")
            {
                anyhow::bail!(
                    "integration repair launch {launch_id} is {prior_state} for run {run_id:?}"
                );
            }
            conn.execute(
                r#"UPDATE worker_attempt_ledger
                   SET state='succeeded', finished_at=?1, failure_cause=''
                   WHERE launch_id=?2 AND state IN ('running', 'interrupted')"#,
                params![now, launch_id],
            )?;
            insert_event_tx(
                conn,
                &intent.run_id,
                "worker_attempt_finished",
                &serde_json::json!({
                    "launch_id": launch_id,
                    "slice_id": slice_id,
                    "state": "succeeded",
                    "failure_cause": "",
                }),
                now,
            )?;
            insert_event_tx(
                conn,
                &intent.run_id,
                "integration_repair_completed",
                &serde_json::json!({
                    "status": status,
                    "summary": summary,
                    "launch_id": launch_id,
                }),
                now,
            )?;
        }
    }
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {definition}"), [])?;
    Ok(())
}

type RunTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);
type TerminalTransitionTuple = (String, String, String, String, String, String);
type RunLaunchIntentTuple = (
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    bool,
    String,
    String,
    String,
    String,
    String,
    String,
);

type IntegrationMergeIntentTuple = (
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);
type SliceRunTuple = (String, String, String, String, String, i64, String);
type EventTuple = (i64, String, String, String, String);
type ReplanProposalTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);
type RunProgressTuple = (
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<i64>,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
);

fn run_tuple_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn run_launch_intent_tuple_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RunLaunchIntentTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn terminal_transition_tuple_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<TerminalTransitionTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn integration_merge_intent_tuple_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<IntegrationMergeIntentTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
    ))
}

fn slice_run_tuple_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SliceRunTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn event_tuple_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn replan_proposal_tuple_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ReplanProposalTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn run_progress_tuple_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunProgressTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
    ))
}

fn run_by_id(conn: &Connection, id: &str) -> Result<Option<Run>> {
    conn.query_row(
        r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                  selected_slice_id, error, started_at, updated_at
           FROM runs WHERE id=?1"#,
        params![id],
        run_tuple_from_row,
    )
    .optional()?
    .map(run_from_tuple)
    .transpose()
}

fn latest_run_for_repo_conn(
    conn: &Connection,
    repo_path: &str,
    active_only: bool,
) -> Result<Option<Run>> {
    let sql = if active_only {
        r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                  selected_slice_id, error, started_at, updated_at
           FROM runs
           WHERE repo_path=?1 AND status IN (?2, ?3)
           ORDER BY started_at DESC, id DESC
           LIMIT 1"#
    } else {
        r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                  selected_slice_id, error, started_at, updated_at
           FROM runs
           WHERE repo_path=?1
           ORDER BY started_at DESC, id DESC
           LIMIT 1"#
    };
    let row = if active_only {
        conn.query_row(
            sql,
            params![
                repo_path,
                RunStatus::Pending.as_str(),
                RunStatus::Running.as_str()
            ],
            run_tuple_from_row,
        )
        .optional()?
    } else {
        conn.query_row(sql, params![repo_path], run_tuple_from_row)
            .optional()?
    };
    row.map(run_from_tuple).transpose()
}

fn status_slice_runs(conn: &Connection, run_id: &str) -> Result<Vec<SliceRun>> {
    let mut stmt = conn.prepare(
        r#"SELECT run_id, slice_id, status, branch, commit_sha, attempts, last_error
           FROM slice_runs WHERE run_id=?1 ORDER BY slice_id"#,
    )?;
    let rows = stmt.query_map(params![run_id], slice_run_tuple_from_row)?;
    rows.map(|row| slice_run_from_tuple(row?)).collect()
}

fn status_worker_attempts(conn: &Connection, run_id: &str) -> Result<Vec<WorkerAttemptLedger>> {
    let mut stmt = conn.prepare(
        r#"SELECT run_id, slice_id, launch_id, launch_ordinal, execution_epoch,
                  worker_retry_ordinal, repair_ordinal, envelope_retry_ordinal, kind,
                  state, branch, worktree, output_stem, created_at, launched_at,
                  finished_at, failure_cause, worker_pid, worker_process_observed_at,
                  worker_last_event_at, worker_last_event_kind,
                  worker_last_semantic_progress_at, worker_last_semantic_progress_summary,
                  worker_attempt_timeout_seconds, worker_no_output_warning_seconds
           FROM worker_attempt_ledger
           WHERE run_id=?1
           ORDER BY launch_id ASC"#,
    )?;
    let rows = stmt.query_map(params![run_id], worker_attempt_ledger_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn status_progress(conn: &Connection, run_id: &str) -> Result<Option<RunProgress>> {
    let row = conn
        .query_row(
            r#"SELECT run_id, phase, slice_id, attempt, command, message, output_tail,
                      phase_started_at, updated_at, worker_attempt_started_at, worker_pid,
                      worker_process_observed_at, worker_last_event_at, worker_last_event_kind,
                      worker_last_semantic_progress_at, worker_last_semantic_progress_summary,
                      worker_attempt_timeout_seconds, worker_no_output_warning_seconds
               FROM run_progress WHERE run_id=?1"#,
            params![run_id],
            run_progress_tuple_from_row,
        )
        .optional()?;
    let mut progress = row.map(run_progress_from_tuple).transpose()?;
    if let Some(progress) = progress.as_mut()
        && let Some(worker) = progress.worker.as_mut()
    {
        worker.launch_id = conn
            .query_row(
                r#"SELECT launch_id FROM worker_attempt_ledger
                   WHERE run_id=?1 AND slice_id=?2 AND state='running'
                     AND (
                       (kind='integration-repair' AND repair_ordinal=?3)
                       OR (kind<>'integration-repair' AND worker_retry_ordinal=?3)
                     )
                   ORDER BY launch_id DESC LIMIT 1"#,
                params![run_id, &progress.slice_id, progress.attempt as i64],
                |row| row.get(0),
            )
            .optional()?;
    }
    Ok(progress)
}

fn status_worker_questions(conn: &Connection, run_id: &str) -> Result<Vec<WorkerQuestion>> {
    let mut stmt = conn.prepare(
        r#"SELECT id, run_id, slice_id, attempt, launch_id, question, options_json, timeout_seconds,
                  state, asked_at, answered_at, answer, recommended_answer,
                  recommendation_rationale, bounded_within_current_slice_or_mission_authority,
                  reversible, fallback_eligible, deadline_at, answer_source
           FROM worker_questions WHERE run_id=?1 ORDER BY asked_at ASC, id ASC"#,
    )?;
    let rows = stmt.query_map(params![run_id], worker_question_from_row)?;
    let mut questions = Vec::new();
    for row in rows {
        questions.push(row.with_context(|| {
            format!("decode required worker question status row for run {run_id}")
        })?);
    }
    Ok(questions)
}

fn status_replan_proposals(conn: &Connection, run_id: &str) -> Result<Vec<ReplanProposal>> {
    let mut stmt = conn.prepare(
        r#"SELECT id, run_id, state, source_json, trigger_finding_ids_json,
                  evidence_json, proposed_changes_json, risk, decision_json,
                  frontier_classification_json, created_at, updated_at
           FROM replan_proposals WHERE run_id=?1 ORDER BY created_at ASC, id ASC"#,
    )?;
    let rows = stmt.query_map(params![run_id], replan_proposal_tuple_from_row)?;
    let mut proposals = Vec::new();
    for row in rows {
        proposals.push(replan_proposal_from_tuple(row?)?);
    }
    Ok(proposals)
}

fn status_frontier_state(
    conn: &Connection,
    run_id: &str,
) -> Result<(Option<MissionEnvelope>, Option<FrontierBudgetState>)> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT mission_envelope_json, frontier_budget_json FROM runs WHERE id=?1",
            params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((envelope_json, budget_json)) = row else {
        anyhow::bail!("required status run {run_id:?} disappeared inside read transaction");
    };
    let mission_envelope = if envelope_json.trim().is_empty() {
        None
    } else {
        Some(
            serde_json::from_str(&envelope_json)
                .with_context(|| format!("parse mission envelope for run {run_id}"))?,
        )
    };
    let frontier_budget = if budget_json.trim().is_empty() {
        mission_envelope
            .as_ref()
            .map(|_| FrontierBudgetState::default())
    } else {
        Some(
            serde_json::from_str(&budget_json)
                .with_context(|| format!("parse frontier budget for run {run_id}"))?,
        )
    };
    Ok((mission_envelope, frontier_budget))
}

fn status_events(conn: &Connection, run_id: &str) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        r#"SELECT id, run_id, type, payload_json, created_at
           FROM events WHERE run_id=?1 ORDER BY id ASC"#,
    )?;
    let rows = stmt.query_map(params![run_id], event_tuple_from_row)?;
    let mut events = Vec::new();
    for row in rows {
        events.push(event_from_tuple(row?)?);
    }
    Ok(events)
}

fn status_terminal_transition(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<TerminalTransition>> {
    conn.query_row(
        r#"SELECT status, error, progress_message, question_interruption_reason,
                  summary_written_at, committed_at
           FROM terminal_transitions WHERE run_id=?1"#,
        params![run_id],
        terminal_transition_tuple_from_row,
    )
    .optional()?
    .map(terminal_transition_from_tuple)
    .transpose()
}

fn status_run_launch_intents(conn: &Connection, run_id: &str) -> Result<Vec<RunLaunchIntent>> {
    let mut stmt = conn.prepare(
        r#"SELECT run_id, execution_epoch, action, state, repo_id, integration_branch,
                  integration_worktree, integration_resources_owned, prior_status, prior_error,
                  primary_cause, compensation_error, created_at, updated_at
           FROM run_launch_intents WHERE run_id=?1
           ORDER BY execution_epoch ASC"#,
    )?;
    let rows = stmt.query_map(params![run_id], run_launch_intent_tuple_from_row)?;
    let mut intents = Vec::new();
    for row in rows {
        intents.push(run_launch_intent_from_tuple(row?)?);
    }
    Ok(intents)
}

fn status_integration_merge_intents(
    conn: &Connection,
    run_id: &str,
) -> Result<Vec<IntegrationMergeIntent>> {
    let mut stmt = conn.prepare(
        r#"SELECT operation_id, run_id, kind, slice_id, attempt, launch_id,
                  source_branch, source_commit, source_tree, expected_head,
                  expected_result_tree, resulting_head, state, completion_json,
                  primary_cause, abort_error, conflicted_files_json, created_at, updated_at
           FROM integration_merge_intents WHERE run_id=?1
           ORDER BY created_at, operation_id"#,
    )?;
    let rows = stmt.query_map(params![run_id], integration_merge_intent_tuple_from_row)?;
    let mut intents = Vec::new();
    for row in rows {
        intents.push(integration_merge_intent_from_tuple(row?)?);
    }
    Ok(intents)
}

fn status_source_snapshots(conn: &Connection, run_id: &str) -> Result<Vec<StatusSourceSnapshot>> {
    let mut stmt = conn.prepare(
        r#"SELECT source, payload_json, indexed_event_id, content_sha256, observed_at
           FROM status_source_snapshots WHERE run_id=?1 ORDER BY source"#,
    )?;
    let rows = stmt.query_map(params![run_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut snapshots = Vec::new();
    for row in rows {
        let (source, payload_json, indexed_event_id, content_sha256, observed_at) = row?;
        let observed_sha256 = format!("{:x}", Sha256::digest(payload_json.as_bytes()));
        if observed_sha256 != content_sha256 {
            anyhow::bail!(
                "indexed status source {source:?} checksum mismatch: stored {content_sha256}, observed {observed_sha256}"
            );
        }
        snapshots.push(StatusSourceSnapshot {
            source: source.clone(),
            payload: serde_json::from_str(&payload_json)
                .with_context(|| format!("parse indexed status source payload {source:?}"))?,
            indexed_event_id,
            content_sha256,
            observed_at: parse_time("status source observed_at", &observed_at)?,
        });
    }
    Ok(snapshots)
}

fn run_from_tuple(row: RunTuple) -> Result<Run> {
    let (
        id,
        repo_id,
        repo_path,
        status,
        base_branch,
        base_sha,
        integration_branch,
        selected_slice_id,
        error,
        started_at,
        updated_at,
    ) = row;
    Ok(Run {
        id,
        repo_id,
        repo_path,
        status: RunStatus::parse(&status)?,
        base_branch,
        base_sha,
        integration_branch,
        selected_slice_id,
        error,
        started_at: parse_time("started_at", &started_at)?,
        updated_at: parse_time("updated_at", &updated_at)?,
    })
}

fn run_launch_intent_from_tuple(row: RunLaunchIntentTuple) -> Result<RunLaunchIntent> {
    let (
        run_id,
        execution_epoch,
        action,
        state,
        repo_id,
        integration_branch,
        integration_worktree,
        integration_resources_owned,
        prior_status,
        prior_error,
        primary_cause,
        compensation_error,
        created_at,
        updated_at,
    ) = row;
    Ok(RunLaunchIntent {
        run_id,
        execution_epoch: execution_epoch.max(1) as usize,
        action: RunLaunchAction::parse(&action)?,
        state: RunLaunchState::parse(&state)?,
        repo_id,
        integration_branch,
        integration_worktree,
        integration_resources_owned,
        prior_status: if prior_status.is_empty() {
            None
        } else {
            Some(RunStatus::parse(&prior_status)?)
        },
        prior_error,
        primary_cause,
        compensation_error,
        created_at: parse_time("created_at", &created_at)?,
        updated_at: parse_time("updated_at", &updated_at)?,
    })
}

fn terminal_transition_from_tuple(row: TerminalTransitionTuple) -> Result<TerminalTransition> {
    let (
        status,
        error,
        progress_message,
        question_interruption_reason,
        summary_written_at,
        committed_at,
    ) = row;
    Ok(TerminalTransition {
        status: RunStatus::parse(&status)?,
        error,
        progress_message,
        question_interruption_reason,
        summary_written: !summary_written_at.trim().is_empty(),
        committed: !committed_at.trim().is_empty(),
    })
}

fn integration_merge_intent_from_tuple(
    row: IntegrationMergeIntentTuple,
) -> Result<IntegrationMergeIntent> {
    let (
        operation_id,
        run_id,
        kind,
        slice_id,
        attempt,
        launch_id,
        source_branch,
        source_commit,
        source_tree,
        expected_head,
        expected_result_tree,
        resulting_head,
        state,
        completion_json,
        primary_cause,
        abort_error,
        conflicted_files_json,
        created_at,
        updated_at,
    ) = row;
    Ok(IntegrationMergeIntent {
        operation_id,
        run_id,
        kind: IntegrationMergeKind::parse(&kind)?,
        slice_id,
        attempt: attempt.max(0) as usize,
        launch_id,
        source_branch,
        source_commit,
        source_tree,
        expected_head,
        expected_result_tree,
        resulting_head,
        state: IntegrationMergeState::parse(&state)?,
        completion: serde_json::from_str(&completion_json)
            .with_context(|| format!("parse merge completion {completion_json:?}"))?,
        primary_cause,
        abort_error,
        conflicted_files: serde_json::from_str(&conflicted_files_json)
            .with_context(|| format!("parse merge conflicted files {conflicted_files_json:?}"))?,
        created_at: parse_time("created_at", &created_at)?,
        updated_at: parse_time("updated_at", &updated_at)?,
    })
}

fn slice_run_from_tuple(row: SliceRunTuple) -> Result<SliceRun> {
    let (run_id, slice_id, status, branch, commit_sha, attempts, last_error) = row;
    Ok(SliceRun {
        run_id,
        slice_id,
        status: SliceStatus::parse(&status)?,
        branch,
        commit_sha,
        attempts: attempts as usize,
        last_error,
    })
}

fn event_from_tuple(row: EventTuple) -> Result<Event> {
    let (id, run_id, typ, payload_json, created_at) = row;
    Ok(Event {
        id,
        run_id,
        typ,
        payload: serde_json::from_str(&payload_json)
            .with_context(|| format!("parse event payload {payload_json:?}"))?,
        created_at: parse_time("created_at", &created_at)?,
    })
}

fn replan_proposal_by_id(
    conn: &Connection,
    run_id: &str,
    proposal_id: &str,
) -> Result<Option<ReplanProposal>> {
    conn.query_row(
        r#"SELECT id, run_id, state, source_json, trigger_finding_ids_json,
                  evidence_json, proposed_changes_json, risk, decision_json,
                  frontier_classification_json, created_at, updated_at
           FROM replan_proposals WHERE run_id=?1 AND id=?2"#,
        params![run_id, proposal_id],
        replan_proposal_tuple_from_row,
    )
    .optional()?
    .map(replan_proposal_from_tuple)
    .transpose()
}

fn replan_proposal_from_tuple(row: ReplanProposalTuple) -> Result<ReplanProposal> {
    let (
        id,
        run_id,
        state,
        source_json,
        trigger_finding_ids_json,
        evidence_json,
        proposed_changes_json,
        risk,
        decision_json,
        frontier_classification_json,
        created_at,
        updated_at,
    ) = row;
    Ok(ReplanProposal {
        id,
        run_id,
        state: ReplanProposalState::parse(&state)?,
        source: serde_json::from_str(&source_json)
            .with_context(|| format!("parse replan proposal source {source_json:?}"))?,
        trigger_finding_ids: serde_json::from_str(&trigger_finding_ids_json).with_context(
            || format!("parse replan proposal finding ids {trigger_finding_ids_json:?}"),
        )?,
        evidence: serde_json::from_str(&evidence_json)
            .with_context(|| format!("parse replan proposal evidence {evidence_json:?}"))?,
        proposed_changes: serde_json::from_str(&proposed_changes_json)
            .with_context(|| format!("parse replan proposal changes {proposed_changes_json:?}"))?,
        risk,
        operator_decision: if decision_json.trim().is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(&decision_json)
                    .with_context(|| format!("parse replan proposal decision {decision_json:?}"))?,
            )
        },
        frontier_classification: if frontier_classification_json.trim().is_empty() {
            None
        } else {
            Some(serde_json::from_str(&frontier_classification_json).with_context(|| {
                format!(
                    "parse replan proposal frontier classification {frontier_classification_json:?}"
                )
            })?)
        },
        created_at: parse_time("created_at", &created_at)?,
        updated_at: parse_time("updated_at", &updated_at)?,
        decision_commands: Vec::new(),
    })
}

fn next_replan_id(conn: &Connection, run_id: &str) -> Result<String> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM replan_proposals WHERE run_id=?1",
        params![run_id],
        |row| row.get(0),
    )?;
    let short = short_replan_run_id(run_id);
    let mut sequence = count + 1;
    loop {
        let id = format!("rp-{short}-{sequence:03}");
        let exists: Option<String> = conn
            .query_row(
                "SELECT id FROM replan_proposals WHERE id=?1",
                params![&id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(id);
        }
        sequence += 1;
    }
}

fn short_replan_run_id(run_id: &str) -> String {
    let trimmed = run_id.strip_prefix("kd-").unwrap_or(run_id);
    let short = trimmed
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>();
    if short.is_empty() {
        "run".to_string()
    } else {
        short
    }
}

fn is_terminal_progress_phase(phase: &str) -> bool {
    matches!(
        phase,
        "blocked" | "completed" | "failed" | "cancelled" | "interrupted"
    )
}

fn project_awaiting_operator_progress(
    conn: &Connection,
    run_id: &str,
    slice_id: &str,
    attempt: usize,
    message: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        r#"INSERT INTO run_progress
           (run_id, phase, slice_id, attempt, command, message, output_tail, phase_started_at,
            updated_at, worker_attempt_started_at, worker_pid, worker_process_observed_at,
            worker_last_event_at, worker_last_event_kind, worker_last_semantic_progress_at,
            worker_last_semantic_progress_summary, worker_attempt_timeout_seconds,
            worker_no_output_warning_seconds)
           VALUES (?1, 'awaiting_operator', ?2, ?3, 'ask_operator', ?4, '', ?5, ?5,
                   '', NULL, '', '', '', '', '', 0, 0)
           ON CONFLICT(run_id) DO UPDATE SET
             phase=excluded.phase,
             slice_id=excluded.slice_id,
             attempt=excluded.attempt,
             command=excluded.command,
             message=excluded.message,
             output_tail=excluded.output_tail,
             phase_started_at=excluded.phase_started_at,
             updated_at=excluded.updated_at,
             worker_attempt_started_at=excluded.worker_attempt_started_at,
             worker_pid=excluded.worker_pid,
             worker_process_observed_at=excluded.worker_process_observed_at,
             worker_last_event_at=excluded.worker_last_event_at,
             worker_last_event_kind=excluded.worker_last_event_kind,
             worker_last_semantic_progress_at=excluded.worker_last_semantic_progress_at,
             worker_last_semantic_progress_summary=excluded.worker_last_semantic_progress_summary,
             worker_attempt_timeout_seconds=excluded.worker_attempt_timeout_seconds,
             worker_no_output_warning_seconds=excluded.worker_no_output_warning_seconds"#,
        params![run_id, slice_id, attempt as i64, message, now],
    )?;
    Ok(())
}

fn worker_attempt_ledger_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<WorkerAttemptLedger> {
    let created_at: String = row.get(13)?;
    let launched_at_text: String = row.get(14)?;
    let finished_at_text: String = row.get(15)?;
    let worker_pid = row.get::<_, Option<i64>>(17)?.map(|pid| pid.max(0) as u32);
    let process_observed_at: String = row.get(18)?;
    let last_event_at: String = row.get(19)?;
    let last_event_kind: String = row.get(20)?;
    let last_semantic_progress_at: String = row.get(21)?;
    let last_semantic_progress_summary: String = row.get(22)?;
    let attempt_timeout_seconds = row.get::<_, i64>(23)?.max(0) as u64;
    let no_output_warning_seconds = row.get::<_, i64>(24)?.max(0) as u64;
    let parse_time = |value: String| {
        DateTime::parse_from_rfc3339(&value)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })
    };
    let parse_optional_time = |value: String| {
        if value.is_empty() {
            Ok(None)
        } else {
            parse_time(value).map(Some)
        }
    };
    let launched_at = parse_optional_time(launched_at_text)?;
    let has_activity = worker_pid.is_some()
        || !process_observed_at.is_empty()
        || !last_event_at.is_empty()
        || !last_event_kind.is_empty()
        || !last_semantic_progress_at.is_empty()
        || !last_semantic_progress_summary.is_empty()
        || attempt_timeout_seconds > 0
        || no_output_warning_seconds > 0;
    let activity = if has_activity {
        launched_at.as_ref().map(|attempt_started_at| {
            Ok::<WorkerAttemptProgress, rusqlite::Error>(WorkerAttemptProgress {
                launch_id: Some(row.get(2)?),
                attempt_started_at: *attempt_started_at,
                pid: worker_pid,
                process_observed_at: parse_optional_time(process_observed_at)?,
                last_event_at: parse_optional_time(last_event_at)?,
                last_event_kind,
                last_semantic_progress_at: parse_optional_time(last_semantic_progress_at)?,
                last_semantic_progress_summary,
                attempt_timeout_seconds,
                no_output_warning_seconds,
            })
        })
    } else {
        None
    };
    Ok(WorkerAttemptLedger {
        run_id: row.get(0)?,
        slice_id: row.get(1)?,
        launch_id: row.get(2)?,
        launch_ordinal: row.get::<_, i64>(3)?.max(0) as usize,
        execution_epoch: row.get::<_, i64>(4)?.max(0) as usize,
        worker_retry_ordinal: row.get::<_, i64>(5)?.max(0) as usize,
        repair_ordinal: row.get::<_, i64>(6)?.max(0) as usize,
        envelope_retry_ordinal: row.get::<_, i64>(7)?.max(0) as usize,
        kind: row.get(8)?,
        state: row.get(9)?,
        branch: row.get(10)?,
        worktree: row.get(11)?,
        output_stem: row.get(12)?,
        created_at: parse_time(created_at)?,
        launched_at,
        finished_at: parse_optional_time(finished_at_text)?,
        failure_cause: row.get(16)?,
        activity: activity.transpose()?,
    })
}

fn active_worker_attempt_with_launch_id(
    conn: &Connection,
    run_id: &str,
    slice_id: &str,
    attempt: usize,
    launch_id: Option<i64>,
) -> Result<bool> {
    let active = if let Some(launch_id) = launch_id {
        conn.query_row(
            r#"SELECT 1
               FROM runs r
               JOIN worker_attempt_ledger l ON l.run_id=r.id
               LEFT JOIN slice_runs s ON s.run_id=l.run_id AND s.slice_id=l.slice_id
               WHERE r.id=?1 AND r.status='running'
                 AND l.slice_id=?2 AND l.launch_id=?4 AND l.state='running'
                 AND (
                   (l.kind='integration-repair' AND l.repair_ordinal=?3)
                   OR (
                     l.kind<>'integration-repair' AND l.worker_retry_ordinal=?3
                     AND s.status IN ('running', 'repair_needed') AND s.attempts=?3
                   )
                 )
               LIMIT 1"#,
            params![run_id, slice_id, attempt as i64, launch_id],
            |_| Ok(()),
        )
        .optional()?
    } else {
        conn.query_row(
            r#"SELECT 1
               FROM runs r
               JOIN slice_runs s ON s.run_id=r.id
               WHERE r.id=?1 AND r.status='running'
                 AND s.slice_id=?2 AND s.status IN ('running', 'repair_needed')
                 AND s.attempts=?3
               LIMIT 1"#,
            params![run_id, slice_id, attempt as i64],
            |_| Ok(()),
        )
        .optional()?
    };
    Ok(active.is_some())
}

#[allow(clippy::too_many_arguments)]
fn insert_worker_question_row(
    conn: &Connection,
    id: &str,
    run_id: &str,
    slice_id: &str,
    attempt: usize,
    launch_id: Option<i64>,
    question: &str,
    options: &[String],
    timeout_seconds: u64,
    recommendation: &WorkerQuestionRecommendation,
) -> Result<WorkerQuestion> {
    let now = Utc::now();
    let deadline_at = if timeout_seconds == 0 {
        None
    } else {
        let seconds = timeout_seconds.min(i64::MAX as u64) as i64;
        now.checked_add_signed(chrono::Duration::seconds(seconds))
    };
    let fallback_eligible = recommendation.is_eligible(options);
    conn.execute(
        r#"INSERT INTO worker_questions
           (id, run_id, slice_id, attempt, launch_id, question, options_json, timeout_seconds,
            recommended_answer, recommendation_rationale,
            bounded_within_current_slice_or_mission_authority, reversible,
            fallback_eligible, deadline_at, state, asked_at, answered_at, answer,
            answer_source)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   'pending', ?15, '', '', '')"#,
        params![
            id,
            run_id,
            slice_id,
            attempt as i64,
            launch_id,
            question,
            serde_json::to_string(options)?,
            timeout_seconds.min(i64::MAX as u64) as i64,
            &recommendation.recommended_answer,
            &recommendation.rationale,
            recommendation.bounded_within_current_slice_or_mission_authority,
            recommendation.reversible,
            fallback_eligible,
            deadline_at
                .map(|deadline| deadline.to_rfc3339())
                .unwrap_or_default(),
            now.to_rfc3339(),
        ],
    )?;
    Ok(WorkerQuestion {
        id: id.to_string(),
        run_id: run_id.to_string(),
        slice_id: slice_id.to_string(),
        attempt,
        launch_id,
        question: question.to_string(),
        options: options.to_vec(),
        timeout_seconds,
        recommended_answer: recommendation.recommended_answer.clone(),
        recommendation_rationale: recommendation.rationale.clone(),
        bounded_within_current_slice_or_mission_authority: recommendation
            .bounded_within_current_slice_or_mission_authority,
        reversible: recommendation.reversible,
        fallback_eligible,
        deadline_at,
        state: "pending".to_string(),
        asked_at: now,
        answered_at: None,
        answer: String::new(),
        answer_source: None,
    })
}

fn worker_question_resolution_command(conn: &Connection, question_id: &str) -> Result<String> {
    conn.query_row(
        "SELECT resolution_command_json FROM worker_questions WHERE id=?1",
        params![question_id],
        |row| row.get(0),
    )
    .with_context(|| format!("read resolution command for worker question {question_id:?}"))
}

fn worker_question_answer_command_json(
    answer: &str,
    answer_source: WorkerQuestionAnswerSource,
    progress_message: &str,
) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "kind": "answer",
        "answer": answer,
        "answer_source": answer_source.as_str(),
        "progress_message": progress_message,
    }))?)
}

fn worker_question_timeout_command_json(
    expected_launch_id: Option<i64>,
    apply_recommendation_at_deadline: bool,
    incident_code: &str,
    message_prefix: &str,
    progress_message: &str,
) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "kind": "timeout",
        "expected_launch_id": expected_launch_id,
        "apply_recommendation_at_deadline": apply_recommendation_at_deadline,
        "incident_code": incident_code,
        "message_prefix": message_prefix,
        "progress_message": progress_message,
    }))?)
}

fn worker_question_by_id(
    conn: &Connection,
    question_id: &str,
    run_id: Option<&str>,
) -> Result<Option<WorkerQuestion>> {
    let columns = r#"SELECT id, run_id, slice_id, attempt, launch_id, question, options_json,
                            timeout_seconds, state, asked_at, answered_at, answer,
                            recommended_answer, recommendation_rationale,
                            bounded_within_current_slice_or_mission_authority, reversible,
                            fallback_eligible, deadline_at, answer_source
                     FROM worker_questions"#;
    let question = if let Some(run_id) = run_id {
        conn.query_row(
            &format!("{columns} WHERE id=?1 AND run_id=?2"),
            params![question_id, run_id],
            worker_question_from_row,
        )
        .optional()?
    } else {
        conn.query_row(
            &format!("{columns} WHERE id=?1"),
            params![question_id],
            worker_question_from_row,
        )
        .optional()?
    };
    Ok(question)
}

fn worker_question_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerQuestion> {
    let options_json: String = row.get(6)?;
    let timeout_seconds: i64 = row.get(7)?;
    let asked_at_text: String = row.get(9)?;
    let answered_at: String = row.get(10)?;
    let deadline_at: String = row.get(17)?;
    let answer_source: String = row.get(18)?;
    let options = serde_json::from_str::<Vec<String>>(&options_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("decode worker question options JSON: {error}"),
            )),
        )
    })?;
    let attempt: i64 = row.get(3)?;
    let asked_at = DateTime::parse_from_rfc3339(&asked_at_text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("decode worker question asked_at: {error}"),
                )),
            )
        })?;
    let timeout_seconds = timeout_seconds.max(0) as u64;
    let deadline_at = if deadline_at.trim().is_empty() {
        if timeout_seconds == 0 {
            None
        } else {
            asked_at.checked_add_signed(chrono::Duration::seconds(
                timeout_seconds.min(i64::MAX as u64) as i64,
            ))
        }
    } else {
        Some(
            DateTime::parse_from_rfc3339(&deadline_at)
                .map(|time| time.with_timezone(&Utc))
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        17,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("decode worker question deadline_at: {error}"),
                        )),
                    )
                })?,
        )
    };
    Ok(WorkerQuestion {
        id: row.get(0)?,
        run_id: row.get(1)?,
        slice_id: row.get(2)?,
        attempt: attempt.max(0) as usize,
        launch_id: row.get(4)?,
        question: row.get(5)?,
        options,
        timeout_seconds,
        recommended_answer: row.get(12)?,
        recommendation_rationale: row.get(13)?,
        bounded_within_current_slice_or_mission_authority: row.get(14)?,
        reversible: row.get(15)?,
        fallback_eligible: row.get(16)?,
        deadline_at,
        state: row.get(8)?,
        asked_at,
        answered_at: if answered_at.trim().is_empty() {
            None
        } else {
            Some(
                DateTime::parse_from_rfc3339(&answered_at)
                    .map(|time| time.with_timezone(&Utc))
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            10,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("decode worker question answered_at: {error}"),
                            )),
                        )
                    })?,
            )
        },
        answer: row.get(11)?,
        answer_source: WorkerQuestionAnswerSource::parse(&answer_source),
    })
}

fn initial_replan_apply_status(
    proposal: &ReplanProposal,
    state: ReplanProposalState,
    run: Option<&Run>,
) -> String {
    if state != ReplanProposalState::Accepted {
        return "not_applicable".to_string();
    }
    if applyable_followup_slice_id(proposal).is_none() {
        return "not_applicable".to_string();
    }
    if run.is_some_and(|run| run.status == RunStatus::Completed) {
        return "refused".to_string();
    }
    "pending".to_string()
}

fn initial_replan_apply_reason(
    proposal: &ReplanProposal,
    state: ReplanProposalState,
    run: Option<&Run>,
) -> String {
    if state != ReplanProposalState::Accepted {
        return "decision does not apply queue or slice changes".to_string();
    }
    if applyable_followup_slice_id(proposal).is_none() {
        return "accepted proposal is not an add_followup_slice typed draft; no apply side effect"
            .to_string();
    }
    if run.is_some_and(|run| run.status == RunStatus::Completed) {
        return "run is already completed; accepted follow-up proposal remains unapplied and requires a new run or replacement proposal".to_string();
    }
    "accepted add_followup_slice typed draft queued for daemon apply checkpoint".to_string()
}

fn initial_replan_generated_slice_id(
    proposal: &ReplanProposal,
    state: ReplanProposalState,
) -> String {
    if state == ReplanProposalState::Accepted {
        applyable_followup_slice_id(proposal).unwrap_or_default()
    } else {
        String::new()
    }
}

fn applyable_followup_slice_id(proposal: &ReplanProposal) -> Option<String> {
    let [change] = proposal.proposed_changes.as_slice() else {
        return None;
    };
    if change.kind != "add_followup_slice" {
        return None;
    }
    let draft = change.followup_slice_draft()?;
    let id = if draft.id.trim().is_empty() {
        change.target.trim()
    } else {
        draft.id.trim()
    };
    (!id.is_empty()).then(|| id.to_string())
}

fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn run_progress_from_tuple(row: RunProgressTuple) -> Result<RunProgress> {
    let (
        run_id,
        phase,
        slice_id,
        attempt,
        command,
        message,
        output_tail,
        phase_started_at,
        updated_at,
        worker_attempt_started_at,
        worker_pid,
        worker_process_observed_at,
        worker_last_event_at,
        worker_last_event_kind,
        worker_last_semantic_progress_at,
        worker_last_semantic_progress_summary,
        worker_attempt_timeout_seconds,
        worker_no_output_warning_seconds,
    ) = row;
    let worker = if worker_attempt_started_at.trim().is_empty() {
        None
    } else {
        Some(WorkerAttemptProgress {
            launch_id: None,
            attempt_started_at: parse_time(
                "worker_attempt_started_at",
                &worker_attempt_started_at,
            )?,
            pid: worker_pid.and_then(|pid| u32::try_from(pid).ok()),
            process_observed_at: parse_optional_time(
                "worker_process_observed_at",
                &worker_process_observed_at,
            )?,
            last_event_at: parse_optional_time("worker_last_event_at", &worker_last_event_at)?,
            last_event_kind: worker_last_event_kind,
            last_semantic_progress_at: parse_optional_time(
                "worker_last_semantic_progress_at",
                &worker_last_semantic_progress_at,
            )?,
            last_semantic_progress_summary: worker_last_semantic_progress_summary,
            attempt_timeout_seconds: worker_attempt_timeout_seconds as u64,
            no_output_warning_seconds: worker_no_output_warning_seconds as u64,
        })
    };
    Ok(RunProgress {
        run_id,
        phase,
        slice_id,
        attempt: attempt as usize,
        command,
        message,
        output_tail,
        phase_started_at: parse_time("phase_started_at", &phase_started_at)?,
        updated_at: parse_time("updated_at", &updated_at)?,
        worker,
        parallel_layer: false,
        parallel_slices: Vec::new(),
    })
}

fn parse_time(field: &str, value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("parse {field}: {value:?}"))?
        .with_timezone(&Utc))
}

fn parse_optional_time(field: &str, value: &str) -> Result<Option<DateTime<Utc>>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    parse_time(field, value).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi_contract::TOOL_EXECUTION_END_EVENT_TYPE;
    use chrono::Duration as ChronoDuration;

    fn run(id: &str, repo_path: &str, status: RunStatus, started_at: DateTime<Utc>) -> Run {
        Run {
            id: id.to_string(),
            repo_id: format!("repo-{repo_path}"),
            repo_path: repo_path.to_string(),
            status,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: format!("khazad/{id}/integration"),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at,
            updated_at: started_at,
        }
    }

    fn merge_intent(
        run_id: &str,
        operation_id: &str,
        now: DateTime<Utc>,
    ) -> IntegrationMergeIntent {
        IntegrationMergeIntent {
            operation_id: operation_id.to_string(),
            run_id: run_id.to_string(),
            kind: IntegrationMergeKind::Slice,
            slice_id: "S-1".to_string(),
            attempt: 2,
            launch_id: Some(41),
            source_branch: "worker/S-1".to_string(),
            source_commit: "source".to_string(),
            source_tree: "source-tree".to_string(),
            expected_head: "expected".to_string(),
            expected_result_tree: "result-tree".to_string(),
            resulting_head: String::new(),
            state: IntegrationMergeState::Prepared,
            completion: IntegrationMergeCompletion::Slice {
                branch: "worker/S-1".to_string(),
                commit_sha: "source".to_string(),
                attempts: 2,
            },
            primary_cause: String::new(),
            abort_error: String::new(),
            conflicted_files: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_active_test_question(
        store: &Store,
        id: &str,
        run_id: &str,
        slice_id: &str,
        attempt: usize,
        question: &str,
        options: &[String],
        timeout_seconds: u64,
        recommendation: &WorkerQuestionRecommendation,
    ) -> Result<WorkerQuestion> {
        store.open_active_worker_question_with_recommendation(
            id,
            run_id,
            slice_id,
            attempt,
            question,
            options,
            timeout_seconds,
            recommendation,
            "worker_question_asked",
            |question| Ok(serde_json::json!({ "question_id": question.id })),
            "awaiting operator answer",
        )
    }

    #[test]
    fn concurrent_run_admission_has_one_durable_repo_winner() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let store = Store::open(&path)?;
        let now = Utc::now();
        let mut first = run(
            "run-admission-a",
            "/tmp/canonical-repo",
            RunStatus::Pending,
            now,
        );
        first.repo_id = "canonical-repo".to_string();
        let mut second = run(
            "run-admission-b",
            "/tmp/canonical-repo",
            RunStatus::Pending,
            now,
        );
        second.repo_id = "canonical-repo".to_string();
        drop(store);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles = [first, second]
            .into_iter()
            .map(|run| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || -> Result<RunAdmissionOutcome> {
                    let store = Store::open(path)?;
                    barrier.wait();
                    Ok(store
                        .admit_run(
                            &run,
                            &[],
                            None,
                            None,
                            Path::new("/tmp/worktrees/integration"),
                        )?
                        .outcome)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("admission thread"))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == RunAdmissionOutcome::Prepared)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == RunAdmissionOutcome::Conflict)
                .count(),
            1
        );

        let store = Store::open(&path)?;
        assert_eq!(store.active_runs()?.len(), 1);
        assert_eq!(store.incomplete_run_launch_intents()?.len(), 1);
        Ok(())
    }

    #[test]
    fn admission_fault_rolls_back_run_intent_and_active_repo_claim() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        let pending = run(
            "run-admission-fault",
            "/tmp/admission-fault",
            RunStatus::Pending,
            now,
        );
        inject_admission_transaction_fault(AdmissionTransactionFaultStage::BeforePreparedEvent);
        let error = store
            .admit_run(
                &pending,
                &[],
                None,
                None,
                Path::new("/tmp/worktrees/integration"),
            )
            .expect_err("admission fault must roll back");
        assert!(error.to_string().contains("injected run admission"));
        assert!(store.get_run(&pending.id)?.is_none());
        assert!(store.active_runs()?.is_empty());
        assert!(store.incomplete_run_launch_intents()?.is_empty());
        assert!(store.get_events(&pending.id, 20)?.is_empty());

        let retry = store.admit_run(
            &pending,
            &[],
            None,
            None,
            Path::new("/tmp/worktrees/integration"),
        )?;
        assert_eq!(retry.outcome, RunAdmissionOutcome::Prepared);
        Ok(())
    }

    #[test]
    fn concurrent_resume_admission_has_one_epoch_and_one_repo_winner() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let store = Store::open(&path)?;
        let now = Utc::now();
        let mut first = run("run-resume-a", "/tmp/resume-repo", RunStatus::Failed, now);
        first.repo_id = "resume-repo".to_string();
        let mut second = run("run-resume-b", "/tmp/resume-repo", RunStatus::Failed, now);
        second.repo_id = "resume-repo".to_string();
        store.insert_run(&first)?;
        store.insert_run(&second)?;
        drop(store);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles = [first.id.clone(), second.id.clone()]
            .into_iter()
            .map(|run_id| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || -> Result<RunAdmissionOutcome> {
                    let store = Store::open(path)?;
                    barrier.wait();
                    Ok(store
                        .begin_resume_run_launch(&run_id, Path::new("/tmp/worktrees/integration"))?
                        .outcome)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("resume admission thread"))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == RunAdmissionOutcome::Prepared)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == RunAdmissionOutcome::Conflict)
                .count(),
            1
        );
        let store = Store::open(&path)?;
        assert_eq!(store.active_runs()?.len(), 1);
        assert_eq!(store.incomplete_run_launch_intents()?.len(), 1);
        assert_eq!(store.incomplete_run_launch_intents()?[0].execution_epoch, 2);
        Ok(())
    }

    #[test]
    fn integration_merge_transaction_faults_roll_back_authority() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        let run = run(
            "run-merge-fault",
            "/tmp/merge-fault",
            RunStatus::Running,
            now,
        );
        store.insert_run(&run)?;
        store.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: "S-1".to_string(),
            status: SliceStatus::ReadyToMerge,
            branch: "worker/S-1".to_string(),
            commit_sha: "source".to_string(),
            attempts: 2,
            last_error: String::new(),
        })?;
        let intent = merge_intent(&run.id, "merge-fault-operation", now);

        inject_integration_merge_transaction_fault(
            IntegrationMergeTransactionFaultStage::PreparedEvent,
        );
        assert!(store.prepare_integration_merge(&intent).is_err());
        assert!(store.integration_merge_intents(&run.id)?.is_empty());

        store.prepare_integration_merge(&intent)?;
        inject_integration_merge_transaction_fault(
            IntegrationMergeTransactionFaultStage::AppliedEvent,
        );
        assert!(
            store
                .commit_integration_merge(&intent.operation_id, "result")
                .is_err()
        );
        assert_eq!(
            store.integration_merge_intents(&run.id)?[0].state,
            IntegrationMergeState::Prepared
        );
        assert_eq!(
            store.get_slice_runs(&run.id)?[0].status,
            SliceStatus::ReadyToMerge
        );
        assert!(!store.get_events(&run.id, 20)?.iter().any(|event| {
            matches!(
                event.typ.as_str(),
                "slice_merged" | "integration_merge_applied"
            )
        }));

        inject_integration_merge_transaction_fault(
            IntegrationMergeTransactionFaultStage::ResolutionEvent,
        );
        assert!(
            store
                .resolve_integration_merge(
                    &intent.operation_id,
                    IntegrationMergeState::Divergent,
                    "operator-head",
                    "operator commit",
                    "",
                    &[],
                )
                .is_err()
        );
        assert_eq!(
            store.integration_merge_intents(&run.id)?[0].state,
            IntegrationMergeState::Prepared
        );
        assert_eq!(
            store.get_slice_runs(&run.id)?[0].status,
            SliceStatus::ReadyToMerge
        );
        Ok(())
    }

    #[test]
    fn integration_merge_completion_is_atomic_with_slice_state_and_events() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        let run = run(
            "run-merge-journal",
            "/tmp/merge-repo",
            RunStatus::Running,
            now,
        );
        store.insert_run(&run)?;
        store.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: "S-1".to_string(),
            status: SliceStatus::ReadyToMerge,
            branch: "worker/S-1".to_string(),
            commit_sha: "source".to_string(),
            attempts: 2,
            last_error: String::new(),
        })?;
        let intent = IntegrationMergeIntent {
            operation_id: "merge-operation-1".to_string(),
            run_id: run.id.clone(),
            kind: IntegrationMergeKind::Slice,
            slice_id: "S-1".to_string(),
            attempt: 2,
            launch_id: Some(41),
            source_branch: "worker/S-1".to_string(),
            source_commit: "source".to_string(),
            source_tree: "source-tree".to_string(),
            expected_head: "expected".to_string(),
            expected_result_tree: "result-tree".to_string(),
            resulting_head: String::new(),
            state: IntegrationMergeState::Prepared,
            completion: IntegrationMergeCompletion::Slice {
                branch: "worker/S-1".to_string(),
                commit_sha: "source".to_string(),
                attempts: 2,
            },
            primary_cause: String::new(),
            abort_error: String::new(),
            conflicted_files: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            store.prepare_integration_merge(&intent)?.outcome,
            IntegrationMergePrepareOutcome::Prepared
        );
        assert!(store.commit_integration_merge(&intent.operation_id, "result")?);
        assert!(!store.commit_integration_merge(&intent.operation_id, "result")?);

        let stored = store.integration_merge_intents(&run.id)?;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].state, IntegrationMergeState::Applied);
        assert_eq!(stored[0].resulting_head, "result");
        let slice = store.get_slice_runs(&run.id)?.remove(0);
        assert_eq!(slice.status, SliceStatus::Merged);
        assert_eq!(slice.commit_sha, "source");
        let events = store.get_events(&run.id, 20)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.typ == "integration_merge_applied")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.typ == "slice_merged")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn every_store_connection_uses_the_sqlite_concurrency_contract() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;

        for _ in 0..2 {
            let conn = store.conn()?;
            let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
            let journal_mode: String =
                conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            let busy_timeout_ms: i64 =
                conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            assert_eq!(foreign_keys, 1);
            assert_eq!(journal_mode, "wal");
            assert_eq!(busy_timeout_ms, 5_000);
        }
        Ok(())
    }

    #[test]
    fn semantic_progress_from_wrapper_stdout_path_updates_worker_progress() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        let run = run(
            "run-semantic",
            "/tmp/repo-semantic",
            RunStatus::Running,
            now,
        );
        store.insert_run(&run)?;
        store.update_progress(
            &run.id,
            "worker_running",
            "slice-001",
            1,
            "pi",
            "slice worker is running",
            "",
        )?;
        let line = include_str!("../tests/fixtures/projection_information_wrapper_stdout.ndjson")
            .lines()
            .find(|line| line.contains(TOOL_EXECUTION_END_EVENT_TYPE))
            .expect("tool execution fixture line");

        store.observe_worker_attempt(
            &run.id,
            "worker_running",
            "slice-001",
            1,
            None,
            Some(123),
            "stdout",
            line,
            0,
            0,
        )?;

        let progress = store.get_progress(&run.id)?.expect("progress");
        let worker = progress.worker.expect("worker progress");
        assert!(worker.last_event_at.is_some());
        assert_eq!(worker.last_event_kind, "stdout");
        assert!(worker.last_semantic_progress_at.is_some());
        assert_eq!(worker.last_semantic_progress_summary, "tool bash finished");
        Ok(())
    }

    #[test]
    fn latest_status_snapshot_lookup_is_deterministic_active_scoped_and_transactional() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        let repo_a = "/tmp/repo-a";
        let repo_b = "/tmp/repo-b";
        let repo_tie = "/tmp/repo-tie";

        store.insert_run(&run("run-active-a", repo_a, RunStatus::Interrupted, now))?;
        store.insert_run(&run(
            "run-active-b",
            repo_a,
            RunStatus::Pending,
            now + ChronoDuration::seconds(1),
        ))?;
        store.insert_run(&run(
            "run-completed-newer",
            repo_a,
            RunStatus::Completed,
            now + ChronoDuration::seconds(2),
        ))?;
        store.insert_run(&run(
            "run-other-repo",
            repo_b,
            RunStatus::Running,
            now + ChronoDuration::seconds(3),
        ))?;
        store.insert_run(&run("run-tie-a", repo_tie, RunStatus::Completed, now))?;
        store.insert_run(&run("run-tie-b", repo_tie, RunStatus::Completed, now))?;

        let active = store
            .latest_status_snapshot(repo_a, true, 50)?
            .expect("active run for repo_a");
        assert_eq!(active.run.id, "run-active-b");

        let latest = store
            .latest_status_snapshot(repo_a, false, 50)?
            .expect("latest run for repo_a");
        assert_eq!(latest.run.id, "run-completed-newer");

        let scoped = store
            .latest_status_snapshot(repo_b, true, 50)?
            .expect("active run for repo_b");
        assert_eq!(scoped.run.id, "run-other-repo");

        let tied = store
            .latest_status_snapshot(repo_tie, false, 50)?
            .expect("tie-broken historical run");
        assert_eq!(tied.run.id, "run-tie-b");

        assert!(
            store
                .latest_status_snapshot("/tmp/missing", true, 50)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn replan_proposals_persist_pending_and_decided_states() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("kd-replan-run", "/tmp/repo", RunStatus::Running, now))?;

        let source = ReplanProposalSource {
            kind: "worker".to_string(),
            slice_id: "slice-001".to_string(),
            phase: "worker_running".to_string(),
            attempt: 1,
            summary: "worker found a follow-up".to_string(),
        };
        let evidence = vec![ReplanEvidenceLink {
            kind: "worker_output".to_string(),
            path: ".workflow/runs/kd-replan-run/outputs/slice-001.worker.json".to_string(),
            event_id: 7,
            summary: "blocked finding".to_string(),
        }];
        let changes = vec![ReplanProposedChange {
            kind: "add_followup_slice".to_string(),
            target: "slice-001-followup".to_string(),
            summary: "capture the out-of-area work separately".to_string(),
        }];

        let pending = store.create_replan_proposal(
            "kd-replan-run",
            "",
            source.clone(),
            vec!["finding-1".to_string()],
            evidence.clone(),
            changes.clone(),
            "intent_affecting",
        )?;
        assert_eq!(pending.state, ReplanProposalState::Pending);
        assert!(pending.id.starts_with("rp-replanru-"));
        assert_eq!(store.pending_replan_proposals("kd-replan-run")?.len(), 1);

        let accepted = store.create_replan_proposal(
            "kd-replan-run",
            "rp-accept",
            source.clone(),
            Vec::new(),
            evidence.clone(),
            changes.clone(),
            "mechanical",
        )?;
        let accepted = store.decide_replan_proposal(
            "kd-replan-run",
            &accepted.id,
            ReplanProposalState::Accepted,
            "approved for later application",
            "test-operator",
            "cli",
            "",
            "",
        )?;
        assert_eq!(accepted.state, ReplanProposalState::Accepted);
        let decision = accepted.operator_decision.as_ref().expect("decision");
        assert_eq!(decision.authorizer, "test-operator");
        assert_eq!(decision.source, "cli");
        assert!(!decision.applied);
        assert!(decision.applied_at.is_none());

        let rejected = store.create_replan_proposal(
            "kd-replan-run",
            "rp-reject",
            source.clone(),
            Vec::new(),
            evidence.clone(),
            changes.clone(),
            "operator_review",
        )?;
        let rejected = store.decide_replan_proposal(
            "kd-replan-run",
            &rejected.id,
            ReplanProposalState::Rejected,
            "duplicate proposal",
            "test-operator",
            "cli",
            "",
            "",
        )?;
        assert_eq!(rejected.state, ReplanProposalState::Rejected);

        let deferred = store.create_replan_proposal(
            "kd-replan-run",
            "rp-defer",
            source.clone(),
            Vec::new(),
            evidence.clone(),
            changes.clone(),
            "operator_review",
        )?;
        let deferred = store.decide_replan_proposal(
            "kd-replan-run",
            &deferred.id,
            ReplanProposalState::Deferred,
            "not needed for this run",
            "test-operator",
            "cli",
            "",
            "if the same finding repeats",
        )?;
        assert_eq!(deferred.state, ReplanProposalState::Deferred);
        assert_eq!(
            deferred
                .operator_decision
                .as_ref()
                .expect("decision")
                .revisit_condition,
            "if the same finding repeats"
        );

        let superseded = store.create_replan_proposal(
            "kd-replan-run",
            "rp-supersede",
            source,
            Vec::new(),
            evidence,
            changes,
            "operator_review",
        )?;
        let superseded = store.decide_replan_proposal(
            "kd-replan-run",
            &superseded.id,
            ReplanProposalState::Superseded,
            "replacement has narrower scope",
            "test-operator",
            "cli",
            "rp-replacement",
            "",
        )?;
        assert_eq!(superseded.state, ReplanProposalState::Superseded);
        assert_eq!(
            superseded
                .operator_decision
                .as_ref()
                .expect("decision")
                .replacement_id,
            "rp-replacement"
        );

        let proposals = store.list_replan_proposals("kd-replan-run")?;
        assert_eq!(proposals.len(), 5);
        assert_eq!(store.pending_replan_proposals("kd-replan-run")?.len(), 1);
        Ok(())
    }

    #[test]
    fn duplicate_identical_replan_decision_is_idempotent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-idempotent-replan",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let proposal = store.create_replan_proposal(
            "run-idempotent-replan",
            "rp-idempotent",
            ReplanProposalSource {
                kind: "operator".to_string(),
                ..ReplanProposalSource::default()
            },
            Vec::new(),
            Vec::new(),
            vec![ReplanProposedChange {
                kind: "add_followup_slice".to_string(),
                target: "slice-followup".to_string(),
                summary: "capture follow-up".to_string(),
            }],
            "operator_review",
        )?;

        for _ in 0..2 {
            let decided = store.decide_replan_proposal(
                "run-idempotent-replan",
                &proposal.id,
                ReplanProposalState::Accepted,
                "approved",
                "test-operator",
                "daemon_ipc",
                "",
                "",
            )?;
            assert_eq!(decided.state, ReplanProposalState::Accepted);
        }

        assert_eq!(
            store
                .get_events("run-idempotent-replan", 100)?
                .into_iter()
                .filter(|event| event.typ == "replan_proposal_decided")
                .count(),
            1,
            "an idempotent duplicate must not append a second authoritative event"
        );
        Ok(())
    }

    #[test]
    fn replan_decision_command_returns_typed_idempotent_and_conflict_outcomes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-typed-replan",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let proposal = store.create_replan_proposal(
            "run-typed-replan",
            "rp-typed",
            ReplanProposalSource {
                kind: "operator".to_string(),
                ..ReplanProposalSource::default()
            },
            Vec::new(),
            Vec::new(),
            vec![ReplanProposedChange {
                kind: "add_followup_slice".to_string(),
                target: "slice-followup".to_string(),
                summary: "capture follow-up".to_string(),
            }],
            "operator_review",
        )?;
        let accepted = ReplanDecisionCommand::operator(
            ReplanProposalState::Accepted,
            "approved",
            "test-operator",
            "daemon_ipc",
            "",
            "",
        );

        let applied = store.decide_replan_proposal_command(
            "run-typed-replan",
            &proposal.id,
            accepted.clone(),
        )?;
        assert_eq!(applied.outcome, DecisionCommandOutcome::Applied);

        let duplicate =
            store.decide_replan_proposal_command("run-typed-replan", &proposal.id, accepted)?;
        assert_eq!(
            duplicate.outcome,
            DecisionCommandOutcome::AlreadyAppliedIdempotently
        );

        let conflict = store.decide_replan_proposal_command(
            "run-typed-replan",
            &proposal.id,
            ReplanDecisionCommand::operator(
                ReplanProposalState::Rejected,
                "rejected instead",
                "test-operator",
                "daemon_ipc",
                "",
                "",
            ),
        )?;
        assert_eq!(conflict.outcome, DecisionCommandOutcome::Conflict);
        assert_eq!(
            conflict.proposal.expect("conflict winner").state,
            ReplanProposalState::Accepted
        );
        assert_eq!(
            store
                .get_events("run-typed-replan", 100)?
                .into_iter()
                .filter(|event| event.typ == "replan_proposal_decided")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn concurrent_auto_accepts_cannot_double_spend_one_budget_snapshot() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-auto-budget-race",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let budget_before = FrontierBudgetState::default();
        let budget_after = FrontierBudgetState {
            auto_promotions_used: 1,
            generated_slices: 1,
            ..FrontierBudgetState::default()
        };
        store.set_frontier_state("run-auto-budget-race", None, Some(&budget_before))?;
        let classification = FrontierClassification {
            tier: "tier_1".to_string(),
            reason_codes: vec!["add_followup_slice_only".to_string()],
            classified_at: now,
            envelope_hash: "envelope".to_string(),
            budget_snapshot: budget_before.clone(),
            autonomy_level: crate::domain::AutonomyLevel::Run,
        };
        for proposal_id in ["rp-auto-a", "rp-auto-b"] {
            let draft = crate::domain::FollowupSliceDraft {
                id: format!("{proposal_id}-slice"),
                title: "Follow-up".to_string(),
                goal: "Capture the bounded follow-up".to_string(),
                areas: vec!["src/state.rs".to_string()],
                acceptance: vec!["follow-up accepted".to_string()],
                verify: vec!["cargo test".to_string()],
                ..crate::domain::FollowupSliceDraft::default()
            };
            store.create_replan_proposal(
                "run-auto-budget-race",
                proposal_id,
                ReplanProposalSource {
                    kind: "worker".to_string(),
                    ..ReplanProposalSource::default()
                },
                Vec::new(),
                Vec::new(),
                vec![ReplanProposedChange::with_followup_slice_draft(
                    "add_followup_slice".to_string(),
                    draft.id.clone(),
                    "bounded follow-up".to_string(),
                    draft,
                )],
                "auto_approvable",
            )?;
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for proposal_id in ["rp-auto-a", "rp-auto-b"] {
            let store = store.clone();
            let barrier = barrier.clone();
            let classification = classification.clone();
            let budget_before = budget_before.clone();
            let budget_after = budget_after.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.decide_replan_proposal_command(
                    "run-auto-budget-race",
                    proposal_id,
                    ReplanDecisionCommand::auto_accept(
                        "frontier policy accepted",
                        classification,
                        budget_before,
                        budget_after,
                    ),
                )
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .expect("auto-accept command")
                    .map(|result| result.outcome)
            })
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Applied)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Conflict)
                .count(),
            1
        );
        let (_, budget) = store.get_frontier_state("run-auto-budget-race")?;
        assert_eq!(budget, Some(budget_after.clone()));
        let proposals = store.list_replan_proposals("run-auto-budget-race")?;
        assert_eq!(
            proposals
                .iter()
                .filter(|proposal| proposal.state == ReplanProposalState::Accepted)
                .count(),
            1
        );
        assert_eq!(
            proposals
                .iter()
                .filter(|proposal| proposal.state == ReplanProposalState::Pending)
                .count(),
            1
        );
        assert_eq!(
            store
                .get_events("run-auto-budget-race", 100)?
                .iter()
                .filter(|event| event.typ == "replan_proposal_decided")
                .count(),
            1
        );
        let accepted_id = proposals
            .iter()
            .find(|proposal| proposal.state == ReplanProposalState::Accepted)
            .expect("accepted proposal")
            .id
            .clone();
        let exact_duplicate = store.decide_replan_proposal_command(
            "run-auto-budget-race",
            &accepted_id,
            ReplanDecisionCommand::auto_accept(
                "frontier policy accepted",
                classification.clone(),
                budget_before.clone(),
                budget_after.clone(),
            ),
        )?;
        assert_eq!(
            exact_duplicate.outcome,
            DecisionCommandOutcome::AlreadyAppliedIdempotently
        );
        let mut conflicting_classification = classification.clone();
        conflicting_classification.envelope_hash = "different-envelope".to_string();
        let conflicting_duplicate = store.decide_replan_proposal_command(
            "run-auto-budget-race",
            &accepted_id,
            ReplanDecisionCommand::auto_accept(
                "frontier policy accepted",
                conflicting_classification,
                budget_before.clone(),
                budget_after.clone(),
            ),
        )?;
        assert_eq!(
            conflicting_duplicate.outcome,
            DecisionCommandOutcome::Conflict
        );

        let rollback_draft = crate::domain::FollowupSliceDraft {
            id: "rp-auto-rollback-slice".to_string(),
            title: "Rollback follow-up".to_string(),
            goal: "Prove budget rollback".to_string(),
            areas: vec!["src/state.rs".to_string()],
            acceptance: vec!["rollback proven".to_string()],
            verify: vec!["cargo test".to_string()],
            ..crate::domain::FollowupSliceDraft::default()
        };
        store.create_replan_proposal(
            "run-auto-budget-race",
            "rp-auto-rollback",
            ReplanProposalSource {
                kind: "worker".to_string(),
                ..ReplanProposalSource::default()
            },
            Vec::new(),
            Vec::new(),
            vec![ReplanProposedChange::with_followup_slice_draft(
                "add_followup_slice".to_string(),
                rollback_draft.id.clone(),
                "rollback follow-up".to_string(),
                rollback_draft,
            )],
            "auto_approvable",
        )?;
        let next_budget = FrontierBudgetState {
            auto_promotions_used: 2,
            generated_slices: 2,
            ..FrontierBudgetState::default()
        };
        inject_decision_transaction_fault(
            DecisionTransactionFaultStage::BeforeSupplementalEventAppend,
        );
        assert!(
            store
                .auto_accept_replan_proposal_with_budget(
                    "run-auto-budget-race",
                    "rp-auto-rollback",
                    "frontier policy rollback",
                    &classification,
                    &budget_after,
                    &next_budget,
                    "rollback-checkpoint",
                    "append_and_run",
                )
                .is_err()
        );
        let (_, durable_budget) = store.get_frontier_state("run-auto-budget-race")?;
        assert_eq!(durable_budget, Some(budget_after.clone()));
        assert_eq!(
            store
                .get_replan_proposal("run-auto-budget-race", "rp-auto-rollback")?
                .expect("rolled back proposal")
                .state,
            ReplanProposalState::Pending
        );
        assert_eq!(
            store
                .get_events("run-auto-budget-race", 100)?
                .iter()
                .filter(|event| event.typ == "replan_proposal_decided")
                .count(),
            1,
            "a failed supplemental event append must roll back proposal state and budget"
        );
        assert_eq!(
            store
                .get_events("run-auto-budget-race", 100)?
                .iter()
                .filter(|event| event.typ == "frontier_auto_accept_recorded")
                .count(),
            0
        );
        assert_eq!(
            store
                .get_events("run-auto-budget-race", 100)?
                .iter()
                .filter(|event| event.typ == "frontier_classified")
                .count(),
            0,
            "a failed supplemental event append must roll back classification audit evidence"
        );

        let recorded = store.decide_replan_proposal_command(
            "run-auto-budget-race",
            "rp-auto-rollback",
            ReplanDecisionCommand::auto_accept_recorded(
                "frontier policy rollback",
                classification.clone(),
                budget_after.clone(),
                next_budget.clone(),
                "rollback-checkpoint",
                "append_and_run",
            ),
        )?;
        assert_eq!(recorded.outcome, DecisionCommandOutcome::Applied);
        let exact_recorded_duplicate = store.decide_replan_proposal_command(
            "run-auto-budget-race",
            "rp-auto-rollback",
            ReplanDecisionCommand::auto_accept_recorded(
                "frontier policy rollback",
                classification.clone(),
                budget_after.clone(),
                next_budget.clone(),
                "rollback-checkpoint",
                "append_and_run",
            ),
        )?;
        assert_eq!(
            exact_recorded_duplicate.outcome,
            DecisionCommandOutcome::AlreadyAppliedIdempotently
        );
        for (checkpoint, apply_mode) in [
            ("different-checkpoint", "append_and_run"),
            ("rollback-checkpoint", "append_only"),
        ] {
            let conflict = store.decide_replan_proposal_command(
                "run-auto-budget-race",
                "rp-auto-rollback",
                ReplanDecisionCommand::auto_accept_recorded(
                    "frontier policy rollback",
                    classification.clone(),
                    budget_after.clone(),
                    next_budget.clone(),
                    checkpoint,
                    apply_mode,
                ),
            )?;
            assert_eq!(conflict.outcome, DecisionCommandOutcome::Conflict);
        }
        assert_eq!(
            store
                .get_events("run-auto-budget-race", 100)?
                .iter()
                .filter(|event| event.typ == "frontier_auto_accept_recorded")
                .count(),
            1
        );
        assert_eq!(
            store
                .get_events("run-auto-budget-race", 100)?
                .iter()
                .filter(|event| event.typ == "frontier_classified")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn concurrent_conflicting_replan_decisions_have_one_winner() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let store = Store::open(&path)?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-concurrent-replan",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let proposal = store.create_replan_proposal(
            "run-concurrent-replan",
            "rp-race",
            ReplanProposalSource {
                kind: "worker".to_string(),
                slice_id: "slice-001".to_string(),
                phase: "worker_running".to_string(),
                attempt: 1,
                summary: "concurrent operator decision".to_string(),
            },
            Vec::new(),
            Vec::new(),
            vec![ReplanProposedChange {
                kind: "add_followup_slice".to_string(),
                target: "slice-001-followup".to_string(),
                summary: "capture follow-up work".to_string(),
            }],
            "operator_review",
        )?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for (state, rationale) in [
            (ReplanProposalState::Accepted, "accept"),
            (ReplanProposalState::Rejected, "reject"),
        ] {
            let store = Store::open(&path)?;
            let barrier = barrier.clone();
            let proposal_id = proposal.id.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store.decide_replan_proposal_command(
                    "run-concurrent-replan",
                    &proposal_id,
                    ReplanDecisionCommand::operator(
                        state,
                        rationale,
                        "test-operator",
                        "concurrent-test",
                        "",
                        "",
                    ),
                )
            }));
        }
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().expect("decision thread"))
            .collect::<Vec<_>>();

        let outcomes = results
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|result| result.outcome)
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Applied)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Conflict)
                .count(),
            1
        );
        let final_proposal = store
            .get_replan_proposal("run-concurrent-replan", &proposal.id)?
            .expect("durable proposal");
        let decision_events = store
            .get_events("run-concurrent-replan", 100)?
            .into_iter()
            .filter(|event| event.typ == "replan_proposal_decided")
            .collect::<Vec<_>>();
        assert_eq!(
            decision_events.len(),
            1,
            "the winning state change and its authoritative event must commit together"
        );
        assert_eq!(decision_events[0].payload["proposal_id"], final_proposal.id);
        assert_eq!(
            decision_events[0].payload["state"],
            final_proposal.state.as_str()
        );
        Ok(())
    }

    #[test]
    fn worker_question_recommendation_eligibility_is_exact_and_bounded() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-eligibility",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let eligible = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is reversible".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };
        let cases = [
            ("eligible", eligible.clone(), vec!["A", "B"], true),
            (
                "empty",
                WorkerQuestionRecommendation {
                    recommended_answer: String::new(),
                    ..eligible.clone()
                },
                vec!["A", "B"],
                false,
            ),
            (
                "non-option",
                WorkerQuestionRecommendation {
                    recommended_answer: "C".to_string(),
                    ..eligible.clone()
                },
                vec!["A", "B"],
                false,
            ),
            (
                "no-rationale",
                WorkerQuestionRecommendation {
                    rationale: String::new(),
                    ..eligible.clone()
                },
                vec!["A", "B"],
                false,
            ),
            (
                "unbounded",
                WorkerQuestionRecommendation {
                    bounded_within_current_slice_or_mission_authority: false,
                    ..eligible.clone()
                },
                vec!["A", "B"],
                false,
            ),
            (
                "irreversible",
                WorkerQuestionRecommendation {
                    reversible: false,
                    ..eligible.clone()
                },
                vec!["A", "B"],
                false,
            ),
            ("duplicate-option", eligible.clone(), vec!["A", "A"], false),
            (
                "empty-option",
                WorkerQuestionRecommendation {
                    recommended_answer: String::new(),
                    ..eligible.clone()
                },
                vec!["", "B"],
                false,
            ),
        ];

        for (id, recommendation, options, expected) in cases {
            let options = options.into_iter().map(str::to_string).collect::<Vec<_>>();
            let question = store.insert_worker_question_with_recommendation(
                id,
                "run-eligibility",
                "slice-001",
                1,
                "Which path?",
                &options,
                60,
                &recommendation,
            )?;
            assert_eq!(question.fallback_eligible, expected, "case {id}");
            assert!(question.deadline_at.is_some());
            assert_eq!(
                store
                    .get_worker_question(id)?
                    .expect("durable question")
                    .fallback_eligible,
                expected,
                "durable case {id}"
            );
        }
        Ok(())
    }

    #[test]
    fn legacy_worker_question_rows_gain_readable_fallback_defaults() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE worker_questions (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                slice_id TEXT NOT NULL,
                attempt INTEGER NOT NULL DEFAULT 0,
                question TEXT NOT NULL,
                options_json TEXT NOT NULL,
                timeout_seconds INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                asked_at TEXT NOT NULL,
                answered_at TEXT NOT NULL DEFAULT '',
                answer TEXT NOT NULL DEFAULT ''
            );
            INSERT INTO worker_questions
                (id, run_id, slice_id, attempt, question, options_json, timeout_seconds,
                 state, asked_at, answered_at, answer)
            VALUES
                ('q-legacy', 'run-legacy', 'slice-001', 1, 'Choose?', '["A","B"]',
                 60, 'answered', '2026-07-10T00:00:00+00:00',
                 '2026-07-10T00:00:10+00:00', 'A');
            "#,
        )?;
        drop(conn);

        let store = Store::open(&path)?;
        let question = store
            .get_worker_question("q-legacy")?
            .expect("legacy row remains readable");
        assert_eq!(question.answer, "A");
        assert_eq!(question.recommended_answer, "");
        assert!(!question.fallback_eligible);
        assert_eq!(question.answer_source, None);
        assert_eq!(
            question
                .deadline_at
                .expect("derived legacy deadline")
                .to_rfc3339(),
            "2026-07-10T00:01:00+00:00"
        );
        Ok(())
    }

    #[test]
    fn worker_question_cas_rejects_terminal_and_stale_attempts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-guard", "/tmp/repo", RunStatus::Running, now))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-guard".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        store.update_progress(
            "run-guard",
            "awaiting_operator",
            "slice-001",
            1,
            "ask_operator",
            "awaiting answer",
            "",
        )?;
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is reversible".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };
        store.insert_worker_question_with_recommendation(
            "q-terminal",
            "run-guard",
            "slice-001",
            1,
            "Which path?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
        )?;
        store.update_run("run-guard", RunStatus::Completed, "")?;
        let terminal = store.decide_worker_question_command(
            "run-guard",
            "q-terminal",
            WorkerQuestionDecisionCommand::answer(
                "A",
                WorkerQuestionAnswerSource::LlmRecommendationTimeout,
                "recommendation applied",
            ),
        )?;
        assert_eq!(terminal.outcome, DecisionCommandOutcome::StaleToken);
        assert_eq!(
            store
                .get_worker_question("q-terminal")?
                .expect("question")
                .state,
            "pending"
        );

        store.update_run("run-guard", RunStatus::Running, "")?;
        store.activate_slice_attempt("run-guard", "slice-001", 2)?;
        store.update_progress(
            "run-guard",
            "awaiting_operator",
            "slice-001",
            2,
            "ask_operator",
            "new attempt awaiting answer",
            "",
        )?;
        let stale = store.decide_worker_question_command(
            "run-guard",
            "q-terminal",
            WorkerQuestionDecisionCommand::answer(
                "A",
                WorkerQuestionAnswerSource::LlmRecommendationTimeout,
                "recommendation applied",
            ),
        )?;
        assert_eq!(stale.outcome, DecisionCommandOutcome::Conflict);
        assert_eq!(
            stale.question.expect("interrupted question").state,
            "interrupted"
        );
        assert_eq!(
            store
                .get_events("run-guard", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_interrupted")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn stale_launch_interruption_rolls_back_when_event_append_fails() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-stale-launch",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let launch = store.allocate_worker_attempt(
            "run-stale-launch",
            "slice-001",
            1,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        store.mark_worker_attempt_launched(launch.launch_id)?;
        store.open_active_worker_question_with_launch_id_and_recommendation(
            "q-stale-launch",
            "run-stale-launch",
            "slice-001",
            1,
            Some(launch.launch_id),
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation::default(),
            "worker_question_asked",
            |question| Ok(serde_json::json!({ "question_id": question.id })),
            "awaiting operator answer",
        )?;
        store.finish_worker_attempt(launch.launch_id, "interrupted", "worker exited")?;

        inject_decision_transaction_fault(DecisionTransactionFaultStage::BeforeEventAppend);
        assert!(
            store
                .decide_worker_question_command(
                    "run-stale-launch",
                    "q-stale-launch",
                    WorkerQuestionDecisionCommand::resolve_timeout(
                        Some(launch.launch_id),
                        "worker_question_cancelled",
                        "operator question closed",
                        "worker remains blocked",
                    ),
                )
                .is_err()
        );
        assert_eq!(
            store
                .get_worker_question("q-stale-launch")?
                .expect("rolled back stale question")
                .state,
            "pending"
        );
        assert_eq!(
            store
                .get_events("run-stale-launch", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_interrupted")
                .count(),
            0
        );

        let applied = store.decide_worker_question_command(
            "run-stale-launch",
            "q-stale-launch",
            WorkerQuestionDecisionCommand::resolve_timeout(
                Some(launch.launch_id),
                "worker_question_cancelled",
                "operator question closed",
                "worker remains blocked",
            ),
        )?;
        assert_eq!(applied.outcome, DecisionCommandOutcome::StaleToken);
        assert_eq!(
            applied.question.expect("stale question").state,
            "interrupted"
        );
        assert_eq!(
            store
                .get_events("run-stale-launch", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_interrupted")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn worker_question_command_returns_typed_answer_outcomes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-question-command",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-question-command".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        store.update_progress(
            "run-question-command",
            "awaiting_operator",
            "slice-001",
            1,
            "ask_operator",
            "awaiting answer",
            "",
        )?;
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is reversible".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };
        store.insert_worker_question_with_recommendation(
            "q-command",
            "run-question-command",
            "slice-001",
            1,
            "Which path?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
        )?;

        let applied = store.decide_worker_question_command(
            "run-question-command",
            "q-command",
            WorkerQuestionDecisionCommand::answer(
                "B",
                WorkerQuestionAnswerSource::Operator,
                "operator answered; worker resuming",
            ),
        )?;
        assert_eq!(applied.outcome, DecisionCommandOutcome::Applied);
        assert_eq!(applied.question.as_ref().expect("answer").answer, "B");

        let duplicate = store.decide_worker_question_command(
            "run-question-command",
            "q-command",
            WorkerQuestionDecisionCommand::answer(
                "B",
                WorkerQuestionAnswerSource::Operator,
                "operator answered; worker resuming",
            ),
        )?;
        assert_eq!(
            duplicate.outcome,
            DecisionCommandOutcome::AlreadyAppliedIdempotently
        );
        let changed_progress = store.decide_worker_question_command(
            "run-question-command",
            "q-command",
            WorkerQuestionDecisionCommand::answer(
                "B",
                WorkerQuestionAnswerSource::Operator,
                "different durable progress evidence",
            ),
        )?;
        assert_eq!(changed_progress.outcome, DecisionCommandOutcome::Conflict);

        let conflict = store.decide_worker_question_command(
            "run-question-command",
            "q-command",
            WorkerQuestionDecisionCommand::answer(
                "A",
                WorkerQuestionAnswerSource::LlmRecommendationTimeout,
                "recommendation applied; worker resuming",
            ),
        )?;
        assert_eq!(conflict.outcome, DecisionCommandOutcome::Conflict);
        let events = store.get_events("run-question-command", 100)?;
        let answered = events
            .iter()
            .filter(|event| event.typ == "worker_question_answered")
            .collect::<Vec<_>>();
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].payload["answer"], "B");
        assert_eq!(answered[0].payload["answer_source"], "operator");

        store.update_progress(
            "run-question-command",
            "awaiting_operator",
            "slice-001",
            1,
            "ask_operator",
            "awaiting rollback answer",
            "",
        )?;
        store.insert_worker_question_with_recommendation(
            "q-command-rollback",
            "run-question-command",
            "slice-001",
            1,
            "Rollback this answer?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
        )?;
        inject_decision_transaction_fault(DecisionTransactionFaultStage::BeforeEventAppend);
        assert!(
            store
                .decide_worker_question_command(
                    "run-question-command",
                    "q-command-rollback",
                    WorkerQuestionDecisionCommand::answer(
                        "B",
                        WorkerQuestionAnswerSource::Operator,
                        "operator answered; worker resuming",
                    ),
                )
                .is_err()
        );
        assert_eq!(
            store
                .get_worker_question("q-command-rollback")?
                .expect("rolled back question")
                .state,
            "pending"
        );
        assert_eq!(
            store
                .get_events("run-question-command", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_answered")
                .count(),
            1,
            "a failed event append must roll back the answer"
        );
        assert_eq!(
            store
                .get_progress("run-question-command")?
                .expect("rolled back progress")
                .phase,
            "awaiting_operator"
        );
        Ok(())
    }

    #[test]
    fn simultaneous_operator_fallback_and_legacy_timeout_commands_each_commit_one_outcome()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-question-race",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-question-race".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        store.update_progress(
            "run-question-race",
            "awaiting_operator",
            "slice-001",
            1,
            "ask_operator",
            "awaiting answer",
            "",
        )?;
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is reversible".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };
        store.insert_worker_question_with_recommendation(
            "q-race-command",
            "run-question-race",
            "slice-001",
            1,
            "Which path?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
        )?;
        store.conn()?.execute(
            "UPDATE worker_questions SET deadline_at=?1 WHERE id='q-race-command'",
            params![(Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()],
        )?;

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let operator_store = store.clone();
        let operator_barrier = barrier.clone();
        let operator = std::thread::spawn(move || {
            operator_barrier.wait();
            operator_store.decide_worker_question_command(
                "run-question-race",
                "q-race-command",
                WorkerQuestionDecisionCommand::answer(
                    "B",
                    WorkerQuestionAnswerSource::Operator,
                    "operator answered; worker resuming",
                ),
            )
        });
        let fallback_store = store.clone();
        let fallback_barrier = barrier.clone();
        let fallback = std::thread::spawn(move || {
            fallback_barrier.wait();
            fallback_store.decide_worker_question_command(
                "run-question-race",
                "q-race-command",
                WorkerQuestionDecisionCommand::resolve_timeout(
                    None,
                    "worker_question_timed_out",
                    "operator question timed out",
                    "worker applying blocked contract",
                ),
            )
        });
        barrier.wait();
        let outcomes = [
            operator.join().expect("operator command")?.outcome,
            fallback.join().expect("fallback command")?.outcome,
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Applied)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Conflict)
                .count(),
            1
        );

        let question = store
            .get_worker_question("q-race-command")?
            .expect("durable winner");
        let answered_events = store
            .get_events("run-question-race", 100)?
            .into_iter()
            .filter(|event| event.typ == "worker_question_answered")
            .collect::<Vec<_>>();
        assert_eq!(answered_events.len(), 1);
        assert_eq!(answered_events[0].payload["answer"], question.answer);
        assert_eq!(
            answered_events[0].payload["answer_source"],
            question.answer_source.expect("winner source").as_str()
        );

        store.update_progress(
            "run-question-race",
            "awaiting_operator",
            "slice-001",
            1,
            "ask_operator",
            "awaiting legacy answer",
            "",
        )?;
        store.insert_worker_question(
            "q-race-legacy-timeout",
            "run-question-race",
            "slice-001",
            1,
            "Legacy question?",
            &[],
            60,
        )?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let operator_store = store.clone();
        let operator_barrier = barrier.clone();
        let operator = std::thread::spawn(move || {
            operator_barrier.wait();
            operator_store.decide_worker_question_command(
                "run-question-race",
                "q-race-legacy-timeout",
                WorkerQuestionDecisionCommand::answer(
                    "legacy answer",
                    WorkerQuestionAnswerSource::Operator,
                    "operator answered; worker resuming",
                ),
            )
        });
        let timeout_store = store.clone();
        let timeout_barrier = barrier.clone();
        let timeout = std::thread::spawn(move || {
            timeout_barrier.wait();
            timeout_store.decide_worker_question_command(
                "run-question-race",
                "q-race-legacy-timeout",
                WorkerQuestionDecisionCommand::resolve_timeout(
                    None,
                    "worker_question_timed_out",
                    "operator question timed out",
                    "worker applying blocked contract",
                ),
            )
        });
        barrier.wait();
        let outcomes = [
            operator.join().expect("operator command")?.outcome,
            timeout.join().expect("timeout command")?.outcome,
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Applied)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DecisionCommandOutcome::Conflict)
                .count(),
            1
        );
        let question = store
            .get_worker_question("q-race-legacy-timeout")?
            .expect("legacy timeout winner");
        assert!(matches!(question.state.as_str(), "answered" | "timed_out"));
        let authoritative_events = store
            .get_events("run-question-race", 100)?
            .into_iter()
            .filter(|event| {
                event.payload["question_id"] == "q-race-legacy-timeout"
                    && matches!(
                        event.typ.as_str(),
                        "worker_question_answered" | "run_incident"
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(authoritative_events.len(), 1);
        assert_eq!(
            authoritative_events[0].typ,
            if question.state == "answered" {
                "worker_question_answered"
            } else {
                "run_incident"
            }
        );
        assert_eq!(
            store
                .get_progress("run-question-race")?
                .expect("legacy resolution progress")
                .phase,
            "worker_running"
        );
        Ok(())
    }

    #[test]
    fn activating_next_attempt_interrupts_stale_pending_question() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-stale", "/tmp/repo", RunStatus::Running, now))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-stale".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        open_active_test_question(
            &store,
            "q-stale",
            "run-stale",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation::default(),
        )?;

        store.update_slice_status(
            "run-stale",
            "slice-001",
            SliceStatus::RepairNeeded,
            "retrying",
        )?;
        store.activate_slice_attempt("run-stale", "slice-001", 2)?;

        let stale = store
            .get_worker_question("q-stale")?
            .expect("stale question");
        assert_eq!(stale.state, "interrupted");
        assert!(stale.answer.contains("superseded by worker attempt 2"));
        assert!(!store.has_pending_worker_question("run-stale", "slice-001", 1)?);
        let interrupted_events = store
            .get_events("run-stale", 100)?
            .into_iter()
            .filter(|event| event.typ == "worker_question_interrupted")
            .collect::<Vec<_>>();
        assert_eq!(interrupted_events.len(), 1);
        assert_eq!(interrupted_events[0].payload["question_id"], "q-stale");
        assert_eq!(interrupted_events[0].payload["active_attempt"], 2);
        let current = open_active_test_question(
            &store,
            "q-current",
            "run-stale",
            "slice-001",
            2,
            "Current retry choice?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation::default(),
        )?;
        assert_eq!(current.state, "pending");
        assert!(store.worker_attempt_is_active("run-stale", "slice-001", 2)?);
        Ok(())
    }

    #[test]
    fn worker_question_open_is_atomic_with_event_and_progress() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-open", "/tmp/repo", RunStatus::Running, now))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-open".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        store.update_progress(
            "run-open",
            "worker_running",
            "slice-001",
            1,
            "pi",
            "worker running",
            "",
        )?;
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is reversible".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };

        let failed = store.open_active_worker_question_with_recommendation(
            "q-failed",
            "run-open",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
            "worker_question_asked",
            |_| -> Result<serde_json::Value> { anyhow::bail!("injected event failure") },
            "awaiting operator answer",
        );
        assert!(failed.is_err());
        assert!(store.get_worker_question("q-failed")?.is_none());
        assert!(store.get_events("run-open", 100)?.is_empty());
        assert_eq!(
            store.get_progress("run-open")?.expect("progress").phase,
            "worker_running"
        );

        let opened = store.open_active_worker_question_with_recommendation(
            "q-opened",
            "run-open",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
            "worker_question_asked",
            |question| {
                Ok(serde_json::json!({
                    "question_id": question.id,
                    "deadline_at": question.deadline_at
                }))
            },
            "awaiting operator answer",
        )?;
        assert_eq!(opened.state, "pending");
        let events = store.get_events("run-open", 100)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].typ, "worker_question_asked");
        assert_eq!(events[0].payload["question_id"], "q-opened");
        let progress = store.get_progress("run-open")?.expect("progress");
        assert_eq!(progress.phase, "awaiting_operator");
        assert_eq!(progress.slice_id, "slice-001");
        assert_eq!(progress.attempt, 1);
        Ok(())
    }

    #[test]
    fn worker_question_timeout_cas_is_idempotent_and_parallel_attempts_stay_independent()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-parallel", "/tmp/repo", RunStatus::Running, now))?;
        for slice_id in ["slice-a", "slice-b"] {
            store.upsert_slice_run(&SliceRun {
                run_id: "run-parallel".to_string(),
                slice_id: slice_id.to_string(),
                status: SliceStatus::Running,
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 1,
                last_error: String::new(),
            })?;
        }
        let legacy = WorkerQuestionRecommendation::default();
        let question_a = open_active_test_question(
            &store,
            "q-a",
            "run-parallel",
            "slice-a",
            1,
            "Choose A?",
            &["yes".to_string(), "no".to_string()],
            60,
            &legacy,
        )?;
        let question_b = open_active_test_question(
            &store,
            "q-b",
            "run-parallel",
            "slice-b",
            1,
            "Choose B?",
            &["yes".to_string(), "no".to_string()],
            60,
            &legacy,
        )?;
        assert_eq!(question_a.state, "pending");
        assert_eq!(question_b.state, "pending");

        let timeout = WorkerQuestionDecisionCommand::timeout(
            "worker_question_timed_out",
            "operator question timed out",
            "worker applying blocked contract",
        );
        let first = store.decide_worker_question_command("run-parallel", "q-b", timeout.clone())?;
        let duplicate = store.decide_worker_question_command("run-parallel", "q-b", timeout)?;
        assert_eq!(first.outcome, DecisionCommandOutcome::Applied);
        assert_eq!(
            duplicate.outcome,
            DecisionCommandOutcome::AlreadyAppliedIdempotently
        );
        assert_eq!(
            duplicate.question.expect("timed out question").state,
            "timed_out"
        );
        let conflicting_timeout = store.decide_worker_question_command(
            "run-parallel",
            "q-b",
            WorkerQuestionDecisionCommand::timeout(
                "different_incident_code",
                "different timeout attribution",
                "different progress evidence",
            ),
        )?;
        assert_eq!(
            conflicting_timeout.outcome,
            DecisionCommandOutcome::Conflict
        );
        assert_eq!(
            store
                .get_events("run-parallel", 100)?
                .iter()
                .filter(|event| event.typ == "run_incident")
                .count(),
            1
        );
        assert_eq!(
            store
                .get_worker_question("q-a")?
                .expect("parallel question")
                .state,
            "pending"
        );
        let progress = store.get_progress("run-parallel")?.expect("progress");
        assert_eq!(progress.phase, "awaiting_operator");
        assert_eq!(progress.slice_id, "slice-a");
        assert_eq!(progress.attempt, 1);
        Ok(())
    }

    #[test]
    fn parallel_worker_lifecycle_progress_cannot_hide_pending_question() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-progress-race",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        for slice_id in ["slice-a", "slice-b"] {
            store.upsert_slice_run(&SliceRun {
                run_id: "run-progress-race".to_string(),
                slice_id: slice_id.to_string(),
                status: SliceStatus::Running,
                branch: String::new(),
                commit_sha: String::new(),
                attempts: 1,
                last_error: String::new(),
            })?;
        }
        open_active_test_question(
            &store,
            "q-a",
            "run-progress-race",
            "slice-a",
            1,
            "Choose A?",
            &["yes".to_string(), "no".to_string()],
            60,
            &WorkerQuestionRecommendation::default(),
        )?;

        store.update_progress(
            "run-progress-race",
            "ready_to_merge",
            "slice-b",
            1,
            "",
            "slice B is ready to merge",
            "",
        )?;

        let awaiting = store.get_progress("run-progress-race")?.expect("progress");
        assert_eq!(awaiting.phase, "awaiting_operator");
        assert_eq!(awaiting.slice_id, "slice-a");
        assert_eq!(awaiting.attempt, 1);

        let transition = store.decide_worker_question_command(
            "run-progress-race",
            "q-a",
            WorkerQuestionDecisionCommand::timeout(
                "worker_question_timed_out",
                "operator question timed out",
                "worker resuming after blocked contract",
            ),
        )?;
        assert_eq!(transition.outcome, DecisionCommandOutcome::Applied);
        let resumed = store.get_progress("run-progress-race")?.expect("progress");
        assert_eq!(resumed.phase, "worker_running");
        assert_eq!(resumed.slice_id, "slice-a");
        assert_eq!(resumed.attempt, 1);
        Ok(())
    }

    #[test]
    fn worker_question_terminal_transition_prevents_same_attempt_revival() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-terminal-question",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-terminal-question".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-terminal-question".to_string(),
            slice_id: "slice-002".to_string(),
            status: SliceStatus::Pending,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 0,
            last_error: String::new(),
        })?;
        open_active_test_question(
            &store,
            "q-terminal-question",
            "run-terminal-question",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation {
                recommended_answer: "A".to_string(),
                rationale: "A is reversible".to_string(),
                bounded_within_current_slice_or_mission_authority: true,
                reversible: true,
            },
        )?;

        assert_eq!(
            store.prepare_run_terminal_transition(
                "run-terminal-question",
                RunStatus::Cancelled,
                "cancelled by operator",
                "cancelled by operator",
                "run cancelled before the question was answered",
            )?,
            1
        );
        let preparing_run = store
            .get_run("run-terminal-question")?
            .expect("run preparing its terminal transition");
        assert_eq!(preparing_run.status, RunStatus::Running);
        let question = store
            .get_worker_question("q-terminal-question")?
            .expect("interrupted question");
        assert_eq!(question.state, "interrupted");
        assert_eq!(question.answer_source, None);
        let progress = store
            .get_progress("run-terminal-question")?
            .expect("terminal progress");
        assert_eq!(progress.phase, "cancelled");
        let slice_runs = store.get_slice_runs("run-terminal-question")?;
        assert_eq!(
            slice_runs
                .iter()
                .find(|slice_run| slice_run.slice_id == "slice-001")
                .expect("active slice")
                .status,
            SliceStatus::Cancelled
        );
        assert_eq!(
            slice_runs
                .iter()
                .find(|slice_run| slice_run.slice_id == "slice-002")
                .expect("unstarted slice")
                .status,
            SliceStatus::Pending,
            "terminalization must not erase resumable planned work"
        );

        store.update_progress(
            "run-terminal-question",
            "ready_to_merge",
            "slice-001",
            1,
            "",
            "late lifecycle write",
            "",
        )?;
        assert_eq!(
            store
                .get_progress("run-terminal-question")?
                .expect("guarded terminal progress")
                .phase,
            "cancelled"
        );

        store.update_run(
            "run-terminal-question",
            RunStatus::Cancelled,
            "cancelled by operator",
        )?;
        assert_eq!(
            store
                .get_run("run-terminal-question")?
                .expect("published terminal run")
                .status,
            RunStatus::Cancelled
        );
        store.update_run("run-terminal-question", RunStatus::Running, "")?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-terminal-question".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 0,
            last_error: String::new(),
        })?;
        store.activate_slice_attempt("run-terminal-question", "slice-001", 1)?;
        let stale = store.decide_worker_question_command(
            "run-terminal-question",
            "q-terminal-question",
            WorkerQuestionDecisionCommand::answer(
                "A",
                WorkerQuestionAnswerSource::LlmRecommendationTimeout,
                "recommendation applied",
            ),
        )?;
        assert_eq!(stale.outcome, DecisionCommandOutcome::Conflict);
        let stale = stale.question.expect("interrupted question");
        assert_eq!(stale.state, "interrupted");
        assert_eq!(stale.answer_source, None);
        assert_eq!(
            store
                .get_events("run-terminal-question", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_interrupted")
                .count(),
            1
        );
        assert!(
            store
                .get_events("run-terminal-question", 100)?
                .iter()
                .all(|event| event.typ != "worker_question_answered")
        );
        Ok(())
    }

    #[test]
    fn terminalization_intent_is_first_commit_wins_and_idempotent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-terminal-first-commit-wins",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        let allocated = store.allocate_worker_attempt(
            "run-terminal-first-commit-wins",
            "slice-allocated",
            1,
            1,
            0,
            0,
            "slice-worker",
            dir.path(),
        )?;
        let running = store.allocate_worker_attempt(
            "run-terminal-first-commit-wins",
            "slice-running",
            1,
            1,
            0,
            0,
            "slice-worker",
            dir.path(),
        )?;
        store.mark_worker_attempt_launched(running.launch_id)?;

        assert_eq!(
            store.prepare_run_terminal_transition(
                "run-terminal-first-commit-wins",
                RunStatus::Cancelled,
                "operator cancelled",
                "operator cancelled",
                "run cancelled before question answer",
            )?,
            0
        );
        for launch in [allocated, running] {
            let ledger = store
                .list_worker_attempt_ledger("run-terminal-first-commit-wins", &launch.slice_id)?;
            assert_eq!(ledger[0].state, "interrupted");
            assert!(ledger[0].finished_at.is_some());
            assert_eq!(
                ledger[0].failure_cause,
                "run cancelled before question answer"
            );
        }
        assert_eq!(
            store.prepare_run_terminal_transition(
                "run-terminal-first-commit-wins",
                RunStatus::Cancelled,
                "operator cancelled",
                "operator cancelled",
                "run cancelled before question answer",
            )?,
            0,
            "an exact retry must not duplicate terminal intent side effects"
        );
        let conflict = store.prepare_run_terminal_transition(
            "run-terminal-first-commit-wins",
            RunStatus::Failed,
            "different failure",
            "different failure",
            "different interruption reason",
        );
        assert!(conflict.is_err());
        assert_eq!(
            store
                .get_events("run-terminal-first-commit-wins", 100)?
                .iter()
                .filter(|event| event.typ == "terminal_transition_intended")
                .count(),
            1
        );
        assert_eq!(
            store
                .get_events("run-terminal-first-commit-wins", 100)?
                .iter()
                .filter(|event| event.typ == "worker_attempt_interrupted")
                .count(),
            2,
            "retrying the same terminal intent must not duplicate launch transitions"
        );
        assert_eq!(
            store
                .get_run("run-terminal-first-commit-wins")?
                .expect("run exists")
                .status,
            RunStatus::Running
        );
        Ok(())
    }

    #[test]
    fn terminalization_state_and_event_commit_roll_back_together_on_event_failure() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-terminal-transaction",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        store.prepare_run_terminal_transition(
            "run-terminal-transaction",
            RunStatus::Failed,
            "integration gate failed",
            "integration gate failed",
            "run failed before question answer",
        )?;
        store.mark_terminal_summary_written(
            "run-terminal-transaction",
            "terminal_summary_written",
            &serde_json::json!({"path": "/tmp/run-summary.json"}),
        )?;

        inject_terminal_transition_fault(TerminalTransitionFaultStage::BeforeTerminalEvent);
        let failed = store.commit_terminal_transition(
            "run-terminal-transaction",
            "run_error",
            &serde_json::json!({"error": "integration gate failed"}),
        );
        assert!(failed.is_err());
        assert_eq!(
            store
                .get_run("run-terminal-transaction")?
                .expect("run remains present")
                .status,
            RunStatus::Running
        );
        let transition = store
            .terminal_transition("run-terminal-transaction")?
            .expect("durable retry intent");
        assert_eq!(transition.status, RunStatus::Failed);
        assert_eq!(transition.error, "integration gate failed");
        assert!(transition.summary_written);
        assert!(!transition.committed);
        assert!(
            store
                .get_events("run-terminal-transaction", 100)?
                .iter()
                .all(|event| event.typ != "run_error")
        );

        assert!(store.commit_terminal_transition(
            "run-terminal-transaction",
            "run_error",
            &serde_json::json!({"error": "integration gate failed"}),
        )?);
        assert_eq!(
            store
                .get_run("run-terminal-transaction")?
                .expect("terminal run")
                .status,
            RunStatus::Failed
        );
        assert_eq!(
            store
                .get_events("run-terminal-transaction", 100)?
                .iter()
                .filter(|event| event.typ == "run_error")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn fallback_timeout_duplicates_require_exact_command_evidence() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-fallback-exact",
            "/tmp/repo",
            RunStatus::Running,
            now,
        ))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-fallback-exact".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        open_active_test_question(
            &store,
            "q-fallback-exact",
            "run-fallback-exact",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &WorkerQuestionRecommendation {
                recommended_answer: "A".to_string(),
                rationale: "A is reversible".to_string(),
                bounded_within_current_slice_or_mission_authority: true,
                reversible: true,
            },
        )?;
        store.conn()?.execute(
            "UPDATE worker_questions SET deadline_at='2020-01-01T00:00:00Z' WHERE id='q-fallback-exact'",
            [],
        )?;

        let command = || {
            WorkerQuestionDecisionCommand::resolve_timeout(
                None,
                "worker_question_timed_out",
                "operator question timed out",
                "worker applying durable fallback",
            )
        };
        assert_eq!(
            store
                .decide_worker_question_command(
                    "run-fallback-exact",
                    "q-fallback-exact",
                    command(),
                )?
                .outcome,
            DecisionCommandOutcome::Applied
        );
        assert_eq!(
            store
                .decide_worker_question_command(
                    "run-fallback-exact",
                    "q-fallback-exact",
                    command(),
                )?
                .outcome,
            DecisionCommandOutcome::AlreadyAppliedIdempotently
        );
        assert_eq!(
            store
                .decide_worker_question_command(
                    "run-fallback-exact",
                    "q-fallback-exact",
                    WorkerQuestionDecisionCommand::resolve_timeout(
                        None,
                        "different_incident_attribution",
                        "operator question timed out",
                        "worker applying durable fallback",
                    ),
                )?
                .outcome,
            DecisionCommandOutcome::Conflict
        );
        assert_eq!(
            store
                .get_events("run-fallback-exact", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_answered")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn worker_question_fallback_cannot_commit_before_durable_deadline() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-early", "/tmp/repo", RunStatus::Running, now))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-early".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: "A".to_string(),
            rationale: "A is reversible".to_string(),
            bounded_within_current_slice_or_mission_authority: true,
            reversible: true,
        };
        open_active_test_question(
            &store,
            "q-early",
            "run-early",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
            &recommendation,
        )?;
        assert!(
            store
                .decide_worker_question_command(
                    "run-early",
                    "q-early",
                    WorkerQuestionDecisionCommand::answer(
                        "A",
                        WorkerQuestionAnswerSource::LlmRecommendationTimeout,
                        "worker resuming",
                    ),
                )
                .is_err()
        );
        assert_eq!(
            store
                .get_worker_question("q-early")?
                .expect("question")
                .state,
            "pending"
        );
        assert_eq!(
            store
                .get_events("run-early", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_answered")
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn legacy_worker_questions_migrate_without_fallback_authority() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let legacy = Connection::open(&path)?;
        legacy.execute_batch(
            r#"
            CREATE TABLE worker_questions (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                slice_id TEXT NOT NULL,
                question TEXT NOT NULL,
                options_json TEXT NOT NULL,
                state TEXT NOT NULL,
                asked_at TEXT NOT NULL,
                answered_at TEXT NOT NULL DEFAULT '',
                answer TEXT NOT NULL DEFAULT ''
            );
            INSERT INTO worker_questions
                (id, run_id, slice_id, question, options_json, state, asked_at)
            VALUES
                ('q-legacy', 'run-legacy', 'slice-001', 'Choose?', '["A","B"]',
                 'pending', '2026-07-01T00:00:00Z');
            "#,
        )?;
        drop(legacy);

        let store = Store::open(&path)?;
        let question = store
            .get_worker_question("q-legacy")?
            .expect("migrated legacy question");
        assert_eq!(question.attempt, 0);
        assert_eq!(question.timeout_seconds, 0);
        assert_eq!(question.recommended_answer, "");
        assert_eq!(question.recommendation_rationale, "");
        assert!(!question.bounded_within_current_slice_or_mission_authority);
        assert!(!question.reversible);
        assert!(!question.fallback_eligible);
        assert_eq!(question.deadline_at, None);
        assert_eq!(question.answer_source, None);
        assert_eq!(question.state, "pending");
        Ok(())
    }

    #[test]
    fn worker_attempt_ledger_allocates_monotonic_immutable_launches_across_epochs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-ledger", "/tmp/repo", RunStatus::Running, now))?;

        let first = store.allocate_worker_attempt(
            "run-ledger",
            "slice-001",
            1,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        let second = store.allocate_worker_attempt(
            "run-ledger",
            "slice-001",
            2,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;

        assert!(second.launch_id > first.launch_id);
        assert_eq!(first.launch_ordinal, 1);
        assert_eq!(second.launch_ordinal, 2);
        assert_eq!(first.execution_epoch, 1);
        assert_eq!(second.execution_epoch, 2);
        assert_eq!(
            first.output_stem,
            format!("slice-001.worker.launch-{}", first.launch_id)
        );
        assert_eq!(
            first.branch,
            format!("khazad/run-ledger/slice-001/launch-{}", first.launch_id)
        );
        assert_eq!(
            first.worktree,
            format!("/tmp/worktrees/slice-001/launch-{}", first.launch_id)
        );
        let projection = store.get_slice_runs("run-ledger")?;
        assert_eq!(projection[0].status, SliceStatus::Running);
        assert_eq!(projection[0].branch, second.branch);
        let history = store.list_worker_attempt_ledger("run-ledger", "slice-001")?;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], first);
        assert_eq!(history[1], second);
        Ok(())
    }

    #[test]
    fn worker_attempt_ledger_preserves_allocated_but_not_launched_evidence_for_reconciliation()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.sqlite");
        let store = Store::open(&path)?;
        let now = Utc::now();
        store.insert_run(&run("run-unlaunched", "/tmp/repo", RunStatus::Running, now))?;
        let allocated = store.allocate_worker_attempt(
            "run-unlaunched",
            "slice-001",
            1,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        drop(store);

        let reopened = Store::open(&path)?;
        let before = reopened.list_worker_attempt_ledger("run-unlaunched", "slice-001")?;
        assert_eq!(before, vec![allocated.clone()]);
        assert_eq!(before[0].state, "allocated");
        assert_eq!(
            reopened.reconcile_unlaunched_worker_attempts(
                "run-unlaunched",
                "daemon restarted before worker launch",
            )?,
            1
        );
        let after = reopened.list_worker_attempt_ledger("run-unlaunched", "slice-001")?;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].launch_id, allocated.launch_id);
        assert_eq!(after[0].state, "interrupted");
        assert_eq!(
            after[0].failure_cause,
            "daemon restarted before worker launch"
        );
        assert!(after[0].finished_at.is_some());
        Ok(())
    }

    #[test]
    fn worker_attempt_ledger_transitions_are_legal_and_append_events() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        store.insert_run(&run(
            "run-transitions",
            "/tmp/repo",
            RunStatus::Running,
            Utc::now(),
        ))?;
        let attempt = store.allocate_worker_attempt(
            "run-transitions",
            "slice-001",
            1,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        assert!(
            store
                .finish_worker_attempt(attempt.launch_id, "succeeded", "")
                .is_err()
        );
        store.mark_worker_attempt_launched(attempt.launch_id)?;
        assert!(
            store
                .mark_worker_attempt_launched(attempt.launch_id)
                .is_err()
        );
        store.finish_worker_attempt(attempt.launch_id, "succeeded", "")?;
        assert!(
            store
                .finish_worker_attempt(attempt.launch_id, "failed", "late")
                .is_err()
        );
        let history = store.list_worker_attempt_ledger("run-transitions", "slice-001")?;
        assert_eq!(history[0].state, "succeeded");
        assert!(history[0].launched_at.is_some());
        assert!(history[0].finished_at.is_some());
        let events = store.get_events("run-transitions", 20)?;
        assert!(
            events
                .iter()
                .any(|event| event.typ == "worker_attempt_launched")
        );
        assert!(
            events
                .iter()
                .any(|event| event.typ == "worker_attempt_finished")
        );
        Ok(())
    }

    #[test]
    fn reopening_a_run_transactionally_advances_execution_epoch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        store.insert_run(&run(
            "run-epoch",
            "/tmp/repo",
            RunStatus::Failed,
            Utc::now(),
        ))?;

        assert_eq!(store.current_run_execution_epoch("run-epoch")?, 1);
        assert_eq!(store.reopen_run_for_resume("run-epoch")?, 2);
        assert_eq!(store.current_run_execution_epoch("run-epoch")?, 2);
        store.activate_run_launch("run-epoch", 2)?;
        store.fail_run_launch("run-epoch", 2, "resume attempt failed", "")?;
        assert_eq!(
            store.get_run("run-epoch")?.expect("restored run").status,
            RunStatus::Failed
        );
        assert_eq!(store.reopen_run_for_resume("run-epoch")?, 3);
        assert_eq!(store.current_run_execution_epoch("run-epoch")?, 3);
        Ok(())
    }

    #[test]
    fn worker_launch_tokens_are_bound_to_the_exact_active_launch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        store.insert_run(&run(
            "run-launch-token",
            "/tmp/repo",
            RunStatus::Running,
            Utc::now(),
        ))?;

        let first = store.allocate_worker_attempt(
            "run-launch-token",
            "slice-001",
            1,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        store.store_worker_launch_token("run-launch-token", first.launch_id, "first-token")?;
        assert!(
            store
                .store_worker_launch_token("run-launch-token", first.launch_id, "replacement")
                .is_err()
        );
        store.mark_worker_attempt_launched(first.launch_id)?;
        assert!(store.validate_worker_launch_token(
            "run-launch-token",
            first.launch_id,
            "first-token"
        )?);
        store.finish_worker_attempt(first.launch_id, "interrupted", "resume")?;
        assert!(!store.validate_worker_launch_token(
            "run-launch-token",
            first.launch_id,
            "first-token"
        )?);

        let second = store.allocate_worker_attempt(
            "run-launch-token",
            "slice-001",
            2,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        store.store_worker_launch_token("run-launch-token", second.launch_id, "second-token")?;
        store.mark_worker_attempt_launched(second.launch_id)?;
        assert!(store.validate_worker_launch_token(
            "run-launch-token",
            second.launch_id,
            "second-token"
        )?);
        assert!(!store.validate_worker_launch_token(
            "run-launch-token",
            second.launch_id,
            "first-token"
        )?);
        assert!(!store.validate_worker_launch_token(
            "run-launch-token",
            first.launch_id,
            "second-token"
        )?);
        Ok(())
    }

    #[test]
    fn integration_repair_authority_uses_its_repair_ordinal_not_a_retry_ordinal() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        store.insert_run(&run(
            "run-integration-repair-launch",
            "/tmp/repo",
            RunStatus::Running,
            Utc::now(),
        ))?;
        let launch = store.allocate_run_worker_attempt(
            "run-integration-repair-launch",
            "integration-repair",
            1,
            0,
            1,
            0,
            "integration-repair",
            Path::new("/tmp/worktrees"),
        )?;
        store.store_worker_launch_token(
            "run-integration-repair-launch",
            launch.launch_id,
            "repair-token",
        )?;
        store.mark_worker_attempt_launched(launch.launch_id)?;

        assert!(store.validate_worker_launch_token(
            "run-integration-repair-launch",
            launch.launch_id,
            "repair-token"
        )?);
        assert!(store.worker_attempt_is_active_with_launch_id(
            "run-integration-repair-launch",
            "integration-repair",
            1,
            Some(launch.launch_id),
        )?);
        assert!(!store.worker_attempt_is_active_with_launch_id(
            "run-integration-repair-launch",
            "integration-repair",
            0,
            Some(launch.launch_id),
        )?);
        store.update_progress(
            "run-integration-repair-launch",
            "integration_repair",
            "integration-repair",
            1,
            "pi",
            "repair running",
            "",
        )?;
        store.observe_worker_attempt(
            "run-integration-repair-launch",
            "integration_repair",
            "integration-repair",
            1,
            Some(launch.launch_id),
            Some(42),
            "tool",
            "repair progress",
            30,
            10,
        )?;
        assert_eq!(
            store
                .get_progress("run-integration-repair-launch")?
                .and_then(|progress| progress.worker)
                .and_then(|worker| worker.launch_id),
            Some(launch.launch_id)
        );
        Ok(())
    }

    #[test]
    fn worker_activity_is_bound_to_the_exact_running_launch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        store.insert_run(&run(
            "run-launch-activity",
            "/tmp/repo",
            RunStatus::Running,
            Utc::now(),
        ))?;
        let first = store.allocate_worker_attempt(
            "run-launch-activity",
            "slice-001",
            1,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        store.mark_worker_attempt_launched(first.launch_id)?;
        store.finish_worker_attempt(first.launch_id, "interrupted", "superseded")?;
        let second = store.allocate_worker_attempt(
            "run-launch-activity",
            "slice-001",
            2,
            1,
            0,
            0,
            "worker",
            Path::new("/tmp/worktrees"),
        )?;
        store.mark_worker_attempt_launched(second.launch_id)?;
        store.update_progress(
            "run-launch-activity",
            "worker_running",
            "slice-001",
            1,
            "pi",
            "worker running",
            "",
        )?;

        store.observe_worker_attempt(
            "run-launch-activity",
            "worker_running",
            "slice-001",
            1,
            Some(first.launch_id),
            Some(111),
            "stderr",
            "stale launch output",
            30,
            10,
        )?;
        assert!(
            store
                .get_progress("run-launch-activity")?
                .expect("progress")
                .worker
                .is_none(),
            "a finished launch must not mutate current worker activity"
        );

        store.observe_worker_attempt(
            "run-launch-activity",
            "worker_running",
            "slice-001",
            1,
            Some(second.launch_id),
            Some(222),
            "stderr",
            "current launch output",
            30,
            10,
        )?;
        let worker = store
            .get_progress("run-launch-activity")?
            .expect("progress")
            .worker
            .expect("current worker activity");
        assert_eq!(worker.launch_id, Some(second.launch_id));
        assert_eq!(worker.pid, Some(222));
        assert_eq!(worker.last_event_kind, "stderr");
        let history = store.list_worker_attempt_ledger("run-launch-activity", "slice-001")?;
        assert!(history[0].activity.is_none());
        let activity = history[1].activity.as_ref().expect("launch activity");
        assert_eq!(activity.launch_id, Some(second.launch_id));
        assert_eq!(activity.pid, Some(222));
        assert_eq!(activity.last_event_kind, "stderr");
        assert_eq!(activity.attempt_timeout_seconds, 30);
        assert_eq!(activity.no_output_warning_seconds, 10);
        Ok(())
    }

    #[test]
    fn status_snapshot_reads_one_sqlite_revision() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("state.sqlite");
        let store = Store::open(&db_path)?;
        let writer = Store::open(&db_path)?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-status-snapshot",
            "/tmp/status-snapshot",
            RunStatus::Running,
            now,
        ))?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-status-snapshot".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;

        let snapshot = store
            .status_snapshot_with_hook("run-status-snapshot", 20, || {
                writer.update_run("run-status-snapshot", RunStatus::Completed, "")?;
                writer.upsert_slice_run(&SliceRun {
                    run_id: "run-status-snapshot".to_string(),
                    slice_id: "slice-001".to_string(),
                    status: SliceStatus::Merged,
                    branch: "worker/slice-001".to_string(),
                    commit_sha: "commit".to_string(),
                    attempts: 1,
                    last_error: String::new(),
                })?;
                writer.record_status_source_snapshot(
                    "run-status-snapshot",
                    "economics",
                    || serde_json::json!({"repair_policy": "after-hook"}),
                )?;
                Ok(())
            })?
            .expect("status snapshot");

        assert_eq!(snapshot.run.status, RunStatus::Running);
        assert_eq!(snapshot.slice_runs[0].status, SliceStatus::Running);
        assert!(snapshot.status_sources.is_empty());
        assert!(snapshot.revision.max_event_id >= 0);
        assert_eq!(
            store
                .get_run("run-status-snapshot")?
                .expect("updated run")
                .status,
            RunStatus::Completed
        );
        assert_eq!(
            store.get_slice_runs("run-status-snapshot")?[0].status,
            SliceStatus::Merged
        );
        Ok(())
    }

    #[test]
    fn latest_status_lookup_and_components_share_one_sqlite_revision() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("state.sqlite");
        let store = Store::open(&db_path)?;
        let writer = Store::open(&db_path)?;
        let now = Utc::now();
        let repo_path = "/tmp/latest-status-revision";
        store.insert_run(&run(
            "run-selected-before-hook",
            repo_path,
            RunStatus::Completed,
            now,
        ))?;

        let snapshot = store
            .latest_status_snapshot_with_hook(repo_path, false, 20, || {
                writer.insert_run(&run(
                    "run-inserted-during-hook",
                    repo_path,
                    RunStatus::Completed,
                    now + ChronoDuration::seconds(1),
                ))?;
                Ok(())
            })?
            .expect("latest snapshot");
        assert_eq!(snapshot.run.id, "run-selected-before-hook");

        let next = store
            .latest_status_snapshot(repo_path, false, 20)?
            .expect("new latest snapshot");
        assert_eq!(next.run.id, "run-inserted-during-hook");
        Ok(())
    }

    #[test]
    fn status_source_payload_and_event_frontier_are_captured_atomically() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("state.sqlite");
        let store = Store::open(&db_path)?;
        let writer = Store::open(&db_path)?;
        let run = run(
            "run-source-race",
            "/tmp/source-race",
            RunStatus::Running,
            Utc::now(),
        );
        store.insert_run(&run)?;
        store.record_event(&run.id, "before_capture", &serde_json::json!({}))?;

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let mut writer_thread = None;
        store.record_status_source_snapshot_with_hook(
            &run.id,
            "economics",
            || serde_json::json!({"repair_policy": "captured"}),
            || {
                let run_id = run.id.clone();
                writer_thread = Some(std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    writer
                        .record_event(&run_id, "after_capture", &serde_json::json!({}))
                        .unwrap();
                }));
                started_rx.recv().unwrap();
            },
        )?;
        writer_thread.expect("writer thread").join().unwrap();

        let snapshot = store
            .status_snapshot(&run.id, 20)?
            .expect("status snapshot");
        let source = snapshot
            .status_sources
            .iter()
            .find(|source| source.source == "economics")
            .expect("economics source");
        assert_eq!(source.indexed_event_id, 1);
        assert_eq!(snapshot.revision.max_event_id, 2);
        Ok(())
    }

    #[test]
    fn malformed_required_status_rows_fail_instead_of_becoming_empty_state() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("state.sqlite");
        let store = Store::open(&db_path)?;
        let now = Utc::now();
        store.insert_run(&run(
            "run-malformed-status",
            "/tmp/malformed-status",
            RunStatus::Running,
            now,
        ))?;
        store.store_worker_token("run-malformed-status", "legacy-token")?;
        store.insert_worker_question(
            "q-malformed",
            "run-malformed-status",
            "slice-001",
            1,
            "Choose?",
            &["A".to_string(), "B".to_string()],
            60,
        )?;
        let conn = Connection::open(&db_path)?;
        conn.execute(
            "UPDATE worker_questions SET options_json = '{not-json' WHERE id = 'q-malformed'",
            [],
        )?;

        let error = store
            .status_snapshot("run-malformed-status", 20)
            .expect_err("required question decode must fail");
        assert!(
            format!("{error:#}").contains("worker question options"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn status_snapshot_keeps_complete_semantic_history_and_bounds_only_the_wire_tail() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let run = run(
            "run-bounded-tail",
            "/tmp/bounded-tail",
            RunStatus::Running,
            Utc::now(),
        );
        store.insert_run(&run)?;
        for index in 0..5 {
            store.record_event(
                &run.id,
                if index == 0 { "run_error" } else { "noise" },
                &serde_json::json!({"index": index, "error": "first failure"}),
            )?;
        }

        let snapshot = store.status_snapshot(&run.id, 2)?.expect("status snapshot");
        assert_eq!(snapshot.events.len(), 5);
        assert_eq!(snapshot.event_tail.len(), 2);
        assert_eq!(snapshot.event_tail[0].payload["index"], 3);
        assert_eq!(snapshot.event_tail[1].payload["index"], 4);
        assert_eq!(snapshot.revision.max_event_id, snapshot.events[4].id);
        Ok(())
    }

    #[test]
    fn malformed_indexed_status_source_fails_the_snapshot() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("state.sqlite");
        let store = Store::open(&db_path)?;
        let run = run(
            "run-malformed-source",
            "/tmp/malformed-source",
            RunStatus::Running,
            Utc::now(),
        );
        store.insert_run(&run)?;
        store.record_status_source_snapshot(
            &run.id,
            "economics",
            || serde_json::json!({"repair_policy": "never"}),
        )?;
        let malformed = "{not-json";
        let malformed_sha = format!("{:x}", Sha256::digest(malformed.as_bytes()));
        Connection::open(&db_path)?.execute(
            "UPDATE status_source_snapshots SET payload_json = ?1, content_sha256 = ?2 WHERE run_id = ?3",
            params![malformed, malformed_sha, run.id],
        )?;

        let error = store
            .status_snapshot(&run.id, 20)
            .expect_err("malformed indexed source must fail");
        assert!(
            format!("{error:#}").contains("status source payload"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn indexed_status_source_checksum_mismatch_fails_the_snapshot() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("state.sqlite");
        let store = Store::open(&db_path)?;
        let run = run(
            "run-source-checksum",
            "/tmp/source-checksum",
            RunStatus::Running,
            Utc::now(),
        );
        store.insert_run(&run)?;
        store.record_status_source_snapshot(
            &run.id,
            "economics",
            || serde_json::json!({"repair_policy": "never"}),
        )?;
        Connection::open(&db_path)?.execute(
            "UPDATE status_source_snapshots SET payload_json = ?1 WHERE run_id = ?2",
            params![r#"{"repair_policy":"always"}"#, run.id],
        )?;

        let error = store
            .status_snapshot(&run.id, 20)
            .expect_err("checksum mismatch must fail");
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn worker_questions_are_token_scoped_and_single_answered() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-1", "/tmp/repo", RunStatus::Running, now))?;
        store.store_worker_token("run-1", "secret-token")?;
        store.upsert_slice_run(&SliceRun {
            run_id: "run-1".to_string(),
            slice_id: "slice-001".to_string(),
            status: SliceStatus::Running,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 2,
            last_error: String::new(),
        })?;
        store.update_progress(
            "run-1",
            "awaiting_operator",
            "slice-001",
            2,
            "ask_operator",
            "awaiting answer",
            "",
        )?;

        assert!(store.validate_worker_token("run-1", "secret-token")?);
        assert!(!store.validate_worker_token("run-1", "wrong-token")?);
        let question = store.insert_worker_question(
            "q-1",
            "run-1",
            "slice-001",
            2,
            "Which path?",
            &["A".to_string(), "B".to_string()],
            3600,
        )?;
        assert_eq!(question.attempt, 2);
        assert_eq!(question.timeout_seconds, 3600);
        assert_eq!(question.state, "pending");
        assert_eq!(store.list_worker_questions("run-1")?.len(), 1);

        let answered = store.decide_worker_question_command(
            "run-1",
            "q-1",
            WorkerQuestionDecisionCommand::answer(
                "A",
                WorkerQuestionAnswerSource::Operator,
                "operator answered; worker resuming",
            ),
        )?;
        assert_eq!(answered.outcome, DecisionCommandOutcome::Applied);
        let answered = answered.question.expect("answered question");
        assert_eq!(answered.state, "answered");
        assert_eq!(answered.answer, "A");
        let conflict = store.decide_worker_question_command(
            "run-1",
            "q-1",
            WorkerQuestionDecisionCommand::answer(
                "B",
                WorkerQuestionAnswerSource::Operator,
                "operator answered; worker resuming",
            ),
        )?;
        assert_eq!(conflict.outcome, DecisionCommandOutcome::Conflict);
        Ok(())
    }
}
