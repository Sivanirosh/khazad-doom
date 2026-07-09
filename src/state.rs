use crate::domain::{
    Event, FrontierBudgetState, FrontierClassification, MissionEnvelope, ReplanDecision,
    ReplanEvidenceLink, ReplanProposal, ReplanProposalSource, ReplanProposalState,
    ReplanProposedChange, Run, RunProgress, RunStatus, SliceRun, SliceStatus,
    WorkerAttemptProgress, WorkerQuestion,
};
use crate::pi_contract;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

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
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
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
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
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
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL
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
                question TEXT NOT NULL,
                options_json TEXT NOT NULL,
                timeout_seconds INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                asked_at TEXT NOT NULL,
                answered_at TEXT NOT NULL DEFAULT '',
                answer TEXT NOT NULL DEFAULT ''
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
        ensure_column(
            &conn,
            "worker_questions",
            "timeout_seconds",
            "timeout_seconds INTEGER NOT NULL DEFAULT 0",
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

    pub fn insert_run(&self, run: &Run) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            r#"INSERT INTO runs
               (id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                selected_slice_id, error, started_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
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
                run.started_at.to_rfc3339(),
                run.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

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

    pub fn update_run(&self, run_id: &str, status: RunStatus, error: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE runs SET status=?1, error=?2, updated_at=?3 WHERE id=?4",
            params![status.as_str(), error, Utc::now().to_rfc3339(), run_id],
        )?;
        Ok(())
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

    pub fn cancel_active_slice_runs(&self, run_id: &str, reason: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            r#"UPDATE slice_runs SET status=?1, last_error=?2
               WHERE run_id=?3 AND status IN ('pending', 'running', 'repair_needed')"#,
            params![SliceStatus::Cancelled.as_str(), reason, run_id],
        )?;
        Ok(())
    }

    pub fn interrupt_active_slice_runs(&self, run_id: &str, reason: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            r#"UPDATE slice_runs SET status=?1, last_error=?2
               WHERE run_id=?3 AND status IN ('pending', 'running', 'repair_needed', 'ready_to_merge')"#,
            params![SliceStatus::Interrupted.as_str(), reason, run_id],
        )?;
        Ok(())
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

    #[allow(clippy::too_many_arguments)]
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
        let conn = self.conn()?;
        let now = Utc::now();
        conn.execute(
            r#"INSERT INTO worker_questions
               (id, run_id, slice_id, attempt, question, options_json, timeout_seconds, state, asked_at, answered_at, answer)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, '', '')"#,
            params![
                id,
                run_id,
                slice_id,
                attempt as i64,
                question,
                serde_json::to_string(options)?,
                timeout_seconds as i64,
                now.to_rfc3339()
            ],
        )?;
        Ok(WorkerQuestion {
            id: id.to_string(),
            run_id: run_id.to_string(),
            slice_id: slice_id.to_string(),
            attempt,
            question: question.to_string(),
            options: options.to_vec(),
            timeout_seconds,
            state: "pending".to_string(),
            asked_at: now,
            answered_at: None,
            answer: String::new(),
        })
    }

    pub fn get_worker_question(&self, id: &str) -> Result<Option<WorkerQuestion>> {
        let conn = self.conn()?;
        Ok(conn
            .query_row(
                r#"SELECT id, run_id, slice_id, attempt, question, options_json, timeout_seconds, state, asked_at, answered_at, answer
               FROM worker_questions WHERE id=?1"#,
                params![id],
                worker_question_from_row,
            )
            .optional()?)
    }

    pub fn list_worker_questions(&self, run_id: &str) -> Result<Vec<WorkerQuestion>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, run_id, slice_id, attempt, question, options_json, timeout_seconds, state, asked_at, answered_at, answer
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
            r#"SELECT q.id, q.run_id, q.slice_id, q.attempt, q.question, q.options_json, q.timeout_seconds, q.state, q.asked_at, q.answered_at, q.answer
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

    pub fn interrupt_pending_worker_questions(&self, run_id: &str, reason: &str) -> Result<usize> {
        let conn = self.conn()?;
        let updated = conn.execute(
            r#"UPDATE worker_questions SET state='interrupted', answered_at=?1, answer=?2
               WHERE run_id=?3 AND state='pending'"#,
            params![Utc::now().to_rfc3339(), reason, run_id],
        )?;
        Ok(updated)
    }

    pub fn answer_worker_question(
        &self,
        run_id: &str,
        question_id: &str,
        answer: &str,
    ) -> Result<WorkerQuestion> {
        let conn = self.conn()?;
        let existing = conn
            .query_row(
                r#"SELECT id, run_id, slice_id, attempt, question, options_json, timeout_seconds, state, asked_at, answered_at, answer
                   FROM worker_questions WHERE id=?1 AND run_id=?2"#,
                params![question_id, run_id],
                worker_question_from_row,
            )
            .optional()?;
        let Some(existing) = existing else {
            anyhow::bail!("question {question_id:?} for run {run_id:?} not found");
        };
        if existing.state != "pending" {
            anyhow::bail!("question {question_id:?} is already {}", existing.state);
        }
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE worker_questions SET state='answered', answered_at=?1, answer=?2 WHERE id=?3",
            params![now, answer, question_id],
        )?;
        self.get_worker_question(question_id)?
            .ok_or_else(|| anyhow::anyhow!("question disappeared after answer"))
    }

    pub fn timeout_worker_question(&self, question_id: &str) -> Result<Option<WorkerQuestion>> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE worker_questions SET state='timed_out', answered_at=?1 WHERE id=?2 AND state='pending'",
            params![Utc::now().to_rfc3339(), question_id],
        )?;
        self.get_worker_question(question_id)
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
        let row = conn
            .query_row(
                r#"SELECT id, run_id, state, source_json, trigger_finding_ids_json,
                          evidence_json, proposed_changes_json, risk, decision_json,
                          frontier_classification_json, created_at, updated_at
                   FROM replan_proposals WHERE run_id=?1 AND id=?2"#,
                params![run_id, proposal_id],
                replan_proposal_tuple_from_row,
            )
            .optional()?;
        row.map(replan_proposal_from_tuple).transpose()
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
        if state == ReplanProposalState::Pending {
            anyhow::bail!("replan decision cannot leave proposal pending");
        }
        if rationale.trim().is_empty() {
            anyhow::bail!("replan decision rationale is required");
        }
        if state == ReplanProposalState::Deferred && revisit_condition.trim().is_empty() {
            anyhow::bail!("replan defer requires --until <condition>");
        }
        if state == ReplanProposalState::Superseded && replacement_id.trim().is_empty() {
            anyhow::bail!("replan supersede requires a replacement proposal id");
        }
        let existing = self
            .get_replan_proposal(run_id, proposal_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("replan proposal {proposal_id:?} for run {run_id:?} not found")
            })?;
        if existing.state != ReplanProposalState::Pending {
            anyhow::bail!(
                "replan proposal {proposal_id:?} is already {}",
                existing.state
            );
        }
        let now = Utc::now();
        let run_for_apply = self.get_run(run_id)?;
        let decision = ReplanDecision {
            decision: state.as_str().to_string(),
            rationale: rationale.trim().to_string(),
            authorizer: if authorizer.trim().is_empty() {
                "operator".to_string()
            } else {
                authorizer.trim().to_string()
            },
            source: if source.trim().is_empty() {
                "daemon_ipc".to_string()
            } else {
                source.trim().to_string()
            },
            decided_at: now,
            applied: false,
            applied_at: None,
            apply_status: initial_replan_apply_status(&existing, state, run_for_apply.as_ref()),
            apply_reason: initial_replan_apply_reason(&existing, state, run_for_apply.as_ref()),
            generated_slice_id: initial_replan_generated_slice_id(&existing, state),
            generated_slice_commit: String::new(),
            apply_before_checkpoint_id: String::new(),
            apply_after_checkpoint_id: String::new(),
            queue_before: Vec::new(),
            queue_after: Vec::new(),
            queue_before_hash: String::new(),
            queue_after_hash: String::new(),
            replacement_id: replacement_id.trim().to_string(),
            revisit_condition: revisit_condition.trim().to_string(),
        };
        let conn = self.conn()?;
        conn.execute(
            r#"UPDATE replan_proposals
               SET state=?1, decision_json=?2, updated_at=?3
               WHERE run_id=?4 AND id=?5"#,
            params![
                state.as_str(),
                serde_json::to_string(&decision)?,
                now.to_rfc3339(),
                run_id,
                proposal_id,
            ],
        )?;
        self.get_replan_proposal(run_id, proposal_id)?
            .ok_or_else(|| anyhow::anyhow!("replan proposal disappeared after decision"))
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
        let conn = self.conn()?;
        let previous: Option<(String, String, i64, String, String)> = conn
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
        conn.execute(
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
        pid: Option<u32>,
        event_kind: &str,
        event_text: &str,
        attempt_timeout_seconds: u64,
        no_output_warning_seconds: u64,
    ) -> Result<Option<RunProgress>> {
        let conn = self.conn()?;
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
        row.map(run_progress_from_tuple).transpose()
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                          selected_slice_id, error, started_at, updated_at
                   FROM runs WHERE id=?1"#,
                params![id],
                run_tuple_from_row,
            )
            .optional()?;
        row.map(run_from_tuple).transpose()
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

    pub fn get_incident_events(&self, run_id: &str) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, run_id, type, payload_json, created_at
               FROM events
               WHERE run_id=?1
                 AND type IN (
                   'run_incident',
                   'run_error',
                   'run_resumed',
                   'worktree_cleanup_error',
                   'daemon_recovery_cleanup_error',
                   'integration_repair_completed'
                 )
               ORDER BY id ASC"#,
        )?;
        let rows = stmt.query_map(params![run_id], event_tuple_from_row)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(event_from_tuple(row?)?);
        }
        Ok(events)
    }

    pub fn cancel_running_runs(&self, reason: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE runs SET status=?1, error=?2, updated_at=?3 WHERE status IN (?4, ?5)",
            params![
                RunStatus::Cancelled.as_str(),
                reason,
                Utc::now().to_rfc3339(),
                RunStatus::Running.as_str(),
                RunStatus::Pending.as_str()
            ],
        )?;
        Ok(())
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

    pub fn active_run_for_repo(
        &self,
        repo_id: &str,
        allowed_run_id: Option<&str>,
    ) -> Result<Option<Run>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            r#"SELECT id, repo_id, repo_path, status, base_branch, base_sha, integration_branch,
                      selected_slice_id, error, started_at, updated_at
               FROM runs
               WHERE repo_id=?1 AND status IN (?2, ?3)
               ORDER BY started_at ASC"#,
        )?;
        let rows = stmt.query_map(
            params![
                repo_id,
                RunStatus::Pending.as_str(),
                RunStatus::Running.as_str()
            ],
            run_tuple_from_row,
        )?;
        for row in rows {
            let run = run_from_tuple(row?)?;
            if Some(run.id.as_str()) != allowed_run_id {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }

    pub fn mark_run_interrupted(&self, run_id: &str, reason: &str) -> Result<()> {
        self.update_run(run_id, RunStatus::Interrupted, reason)
    }

    pub fn latest_run_for_repo(&self, repo_path: &str, active_only: bool) -> Result<Option<Run>> {
        let conn = self.conn()?;
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

fn worker_question_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerQuestion> {
    let options_json: String = row.get(5)?;
    let timeout_seconds: i64 = row.get(6)?;
    let asked_at: String = row.get(8)?;
    let answered_at: String = row.get(9)?;
    let options = serde_json::from_str::<Vec<String>>(&options_json).unwrap_or_default();
    let attempt: i64 = row.get(3)?;
    Ok(WorkerQuestion {
        id: row.get(0)?,
        run_id: row.get(1)?,
        slice_id: row.get(2)?,
        attempt: attempt.max(0) as usize,
        question: row.get(4)?,
        options,
        timeout_seconds: timeout_seconds.max(0) as u64,
        state: row.get(7)?,
        asked_at: DateTime::parse_from_rfc3339(&asked_at)
            .map(|time| time.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        answered_at: if answered_at.trim().is_empty() {
            None
        } else {
            DateTime::parse_from_rfc3339(&answered_at)
                .map(|time| time.with_timezone(&Utc))
                .ok()
        },
        answer: row.get(10)?,
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
    fn latest_run_for_repo_is_deterministic_and_active_scoped() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        let repo_a = "/tmp/repo-a";
        let repo_b = "/tmp/repo-b";
        let repo_tie = "/tmp/repo-tie";

        store.insert_run(&run("run-active-a", repo_a, RunStatus::Running, now))?;
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
        store.insert_run(&run("run-tie-a", repo_tie, RunStatus::Running, now))?;
        store.insert_run(&run("run-tie-b", repo_tie, RunStatus::Running, now))?;

        let active = store
            .latest_run_for_repo(repo_a, true)?
            .expect("active run for repo_a");
        assert_eq!(active.id, "run-active-b");

        let latest = store
            .latest_run_for_repo(repo_a, false)?
            .expect("latest run for repo_a");
        assert_eq!(latest.id, "run-completed-newer");

        let scoped = store
            .latest_run_for_repo(repo_b, true)?
            .expect("active run for repo_b");
        assert_eq!(scoped.id, "run-other-repo");

        let tied = store
            .latest_run_for_repo(repo_tie, true)?
            .expect("tie-broken active run");
        assert_eq!(tied.id, "run-tie-b");

        assert!(store.latest_run_for_repo("/tmp/missing", true)?.is_none());
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
    fn worker_questions_are_token_scoped_and_single_answered() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("state.sqlite"))?;
        let now = Utc::now();
        store.insert_run(&run("run-1", "/tmp/repo", RunStatus::Running, now))?;
        store.store_worker_token("run-1", "secret-token")?;

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

        let answered = store.answer_worker_question("run-1", "q-1", "A")?;
        assert_eq!(answered.state, "answered");
        assert_eq!(answered.answer, "A");
        assert!(store.answer_worker_question("run-1", "q-1", "B").is_err());
        Ok(())
    }
}
