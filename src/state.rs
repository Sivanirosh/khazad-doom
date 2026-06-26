use crate::domain::{
    Event, Run, RunProgress, RunStatus, SliceRun, SliceStatus, WorkerAttemptProgress,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
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
            "worker_attempt_timeout_seconds",
            "worker_attempt_timeout_seconds INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "run_progress",
            "worker_no_output_warning_seconds",
            "worker_no_output_warning_seconds INTEGER NOT NULL DEFAULT 0",
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
                worker_attempt_timeout_seconds, worker_no_output_warning_seconds)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '', NULL, '', '', '', '', 0, 0)
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
        attempt_timeout_seconds: u64,
        no_output_warning_seconds: u64,
    ) -> Result<Option<RunProgress>> {
        let conn = self.conn()?;
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let is_process_observation = matches!(event_kind, "started" | "process_observed");
        let is_worker_event = matches!(event_kind, "stdout" | "stderr");
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
                 worker_attempt_timeout_seconds=?6,
                 worker_no_output_warning_seconds=?7
               WHERE run_id=?8 AND phase=?9 AND slice_id=?10 AND attempt=?11"#,
            params![
                now_text,
                pid.map(|pid| pid as i64),
                is_process_observation,
                is_worker_event,
                event_kind,
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
                          worker_last_semantic_progress_at, worker_attempt_timeout_seconds,
                          worker_no_output_warning_seconds
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
}
