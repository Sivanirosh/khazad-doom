use crate::artifact;
use crate::domain::{
    BranchHandoff, Event, ImplementationSummary, ReplanProposal, ReplanProposalState, ReplanStatus,
    Run, RunDetails, RunEconomics, RunIncident, RunInspection, RunProgress, RunStatus, SliceRun,
    SliceStatus, SliceWriteResult, TerminalReason, WorkerProfileEvidence, WorkerQuestion,
    replan_decision_commands,
};
use crate::ipc::{
    AnswerQuestionParams, AnswerQuestionResult, CancelRunParams, CancelRunResult,
    CreateReplanProposalParams, CreateReplanProposalResult, DecideReplanProposalParams,
    DecideReplanProposalResult, HandoffParams, InitRepoParams, InitRepoResult, InspectRunParams,
    ListQuestionsParams, ListQuestionsResult, ListReplanProposalsParams, ListReplanProposalsResult,
    ListSlicesResult, Request, Response, ResumeRunParams, SliceImportGithubParams, SliceNewParams,
    SlicesParams, StartRunParams, StartRunResult, StatusParams, WorkerAskParams, WorkerAskResult,
};
use crate::paths::Paths;
use crate::state::Store as StateStore;
use crate::workflow::{
    GithubImportOptions, Manager, ResumeOptions, SliceDraft, StartOptions,
    focus_default_agent_target, project_run, send_default_agent_message,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIENT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const DAEMON_HEALTH_TIMEOUT: Duration = Duration::from_millis(500);
const DAEMON_REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(1);
const ACCEPT_LOOP_IDLE_SLEEP: Duration = Duration::from_millis(50);

fn worker_question_answer_command(question: &WorkerQuestion) -> String {
    format!(
        "khazad-doom answer {} {} <answer>",
        question.run_id, question.id
    )
}

fn worker_question_deadline(question: &WorkerQuestion) -> Option<String> {
    if question.timeout_seconds == 0 {
        return None;
    }
    Some(
        (question.asked_at + chrono::Duration::seconds(question.timeout_seconds as i64))
            .to_rfc3339(),
    )
}

#[derive(Debug, Clone)]
pub struct Client {
    paths: Paths,
}

impl Client {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    pub fn call<P, O>(&self, method: &str, params: &P) -> Result<O>
    where
        P: Serialize,
        O: DeserializeOwned,
    {
        self.call_with_timeout(method, params, CLIENT_RPC_TIMEOUT)
    }

    pub fn call_with_timeout<P, O>(&self, method: &str, params: &P, timeout: Duration) -> Result<O>
    where
        P: Serialize,
        O: DeserializeOwned,
    {
        let mut stream = UnixStream::connect(self.paths.socket())?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let id = request_id();
        let request = Request {
            id,
            method: method.to_string(),
            params: Some(serde_json::to_value(params)?),
        };
        write_json_line(&mut stream, &request)?;

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.trim().is_empty() {
            bail!("daemon returned an empty response");
        }
        let response: serde_json::Value = serde_json::from_str(&line)?;
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            if let Some(error) = error.as_str() {
                bail!(error.to_string());
            }
            bail!(error.to_string());
        }
        let result = response
            .get("result")
            .cloned()
            .context("daemon response missing result")?;
        Ok(serde_json::from_value(result)?)
    }

    pub fn ping_with_timeout(&self, timeout: Duration) -> Result<()> {
        let _: serde_json::Value = self.call_with_timeout(
            "status",
            &StatusParams {
                limit: 1,
                ..StatusParams::default()
            },
            timeout,
        )?;
        Ok(())
    }

    pub fn health_check(&self) -> DaemonHealth {
        if !self.paths.socket().exists() {
            return DaemonHealth::Missing;
        }
        match self.ping_with_timeout(DAEMON_HEALTH_TIMEOUT) {
            Ok(()) => DaemonHealth::Running,
            Err(err) if is_missing_socket_error(err.as_ref()) => DaemonHealth::Missing,
            Err(err) => DaemonHealth::Unhealthy(err.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonHealth {
    Running,
    Missing,
    Unhealthy(String),
}

#[derive(Clone)]
pub struct Server {
    paths: Paths,
    store: StateStore,
    manager: Manager,
    request_lock: Arc<Mutex<()>>,
}

impl Server {
    pub fn new(paths: Paths, store: StateStore) -> Self {
        let manager = Manager::new(paths.clone(), store.clone());
        Self {
            paths,
            store,
            manager,
            request_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn serve(&self) -> Result<()> {
        self.paths.ensure()?;
        let socket = self.paths.socket();
        if socket.exists() {
            match Client::new(self.paths.clone()).health_check() {
                DaemonHealth::Running => bail!("daemon already running at {}", socket.display()),
                DaemonHealth::Missing => {
                    let _ = fs::remove_file(&socket);
                }
                DaemonHealth::Unhealthy(reason) => {
                    if let Some(pid) = live_daemon_pid(&self.paths) {
                        bail!(
                            "daemon socket at {} is unhealthy but daemon pid {pid} is still running: {reason}",
                            socket.display()
                        );
                    }
                    let _ = fs::remove_file(&socket);
                    let _ = fs::remove_file(self.paths.pid_file());
                }
            }
        }
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("listen on {}", socket.display()))?;
        self.manager.recover_interrupted_runs()?;
        fs::write(self.paths.pid_file(), format!("{}\n", std::process::id()))?;

        let result = self.accept_loop(listener);

        let _ = fs::remove_file(self.paths.socket());
        let _ = fs::remove_file(self.paths.pid_file());
        result
    }

    fn accept_loop(&self, listener: UnixListener) -> Result<()> {
        listener.set_nonblocking(true)?;
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        loop {
            if shutdown_rx.try_recv().unwrap_or(false) {
                return Ok(());
            }
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let server = self.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    thread::spawn(move || {
                        let shutdown = server.handle_conn(stream);
                        match shutdown {
                            Ok(true) => {
                                let _ = shutdown_tx.send(true);
                            }
                            Ok(false) => {}
                            Err(err) => eprintln!("khazad-doom daemon: {err:#}"),
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if shutdown_rx
                        .recv_timeout(ACCEPT_LOOP_IDLE_SLEEP)
                        .unwrap_or(false)
                    {
                        return Ok(());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn handle_conn(&self, mut stream: UnixStream) -> Result<bool> {
        stream.set_read_timeout(Some(DAEMON_REQUEST_READ_TIMEOUT))?;
        stream.set_write_timeout(Some(CLIENT_RPC_TIMEOUT))?;
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        if line.trim().is_empty() {
            let response = Response {
                id: String::new(),
                result: None,
                error: Some("empty request".to_string()),
            };
            write_json_line(&mut stream, &response)?;
            return Ok(false);
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let response = Response {
                    id: String::new(),
                    result: None,
                    error: Some(err.to_string()),
                };
                write_json_line(&mut stream, &response)?;
                return Ok(false);
            }
        };
        let id = request.id.clone();
        let method = request.method.clone();
        let params = request.params.clone();
        let handled = if method_allows_concurrent_handling(&method) {
            self.handle(method.as_str(), params)
        } else {
            let _request_guard = self
                .request_lock
                .lock()
                .expect("daemon request mutex poisoned");
            self.handle(method.as_str(), params)
        };
        let mut shutdown = false;
        let response = match handled {
            Ok(HandleOutcome {
                result,
                should_shutdown,
            }) => {
                shutdown = should_shutdown;
                Response {
                    id,
                    result: Some(result),
                    error: None,
                }
            }
            Err(err) => Response {
                id,
                result: None,
                error: Some(err.to_string()),
            },
        };
        write_json_line(&mut stream, &response)?;
        Ok(shutdown)
    }

    fn run_details(&self, run: Run, events_limit: usize) -> Result<RunDetails> {
        let run_id = run.id.clone();
        let slice_runs = self.store.get_slice_runs(&run_id)?;
        let mut progress = self.store.get_progress(&run_id)?;
        let events = self.store.get_events(&run_id, events_limit)?;
        if let Some(progress) = progress.as_mut() {
            annotate_parallel_progress(progress, &slice_runs, &events);
        }
        let economics = read_run_economics(&run).ok();
        let worker_profile = read_worker_profile(&run, economics.as_ref()).unwrap_or_default();
        let incident_events = self.store.get_incident_events(&run_id)?;
        let incidents = run_incidents_from_events(&incident_events);
        let questions = self
            .store
            .list_worker_questions(&run_id)
            .unwrap_or_default();
        let replan = self.replan_status(&run_id).unwrap_or_default();
        let primary_terminal_reason = primary_terminal_reason(
            &run,
            &slice_runs,
            progress.as_ref(),
            &events,
            &incident_events,
            &questions,
        );
        let mut details = RunDetails {
            worker_profile,
            slice_runs,
            progress,
            incidents,
            questions,
            replan,
            events,
            economics,
            primary_terminal_reason,
            feed: None,
            run,
        };
        details.feed = Some(project_run(&details));
        Ok(details)
    }

    fn replan_status(&self, run_id: &str) -> Result<ReplanStatus> {
        let proposals = self.store.list_replan_proposals(run_id)?;
        Ok(replan_status_from_proposals(run_id, proposals))
    }

    fn handle_worker_ask(&self, params: WorkerAskParams) -> Result<WorkerAskResult> {
        if !self
            .store
            .validate_worker_token(&params.run_id, &params.token)?
        {
            self.store.record_event(
                &params.run_id,
                "run_incident",
                &json!({
                    "severity": "error",
                    "kind": "worker_question_token_rejected",
                    "message": "workerAsk rejected because the worker token did not match the run",
                    "slice_id": params.slice_id,
                }),
            )?;
            bail!("worker token rejected for run {}", params.run_id);
        }
        let timeout_seconds = self.worker_question_timeout_seconds(&params);
        let question_id = format!("q-{}", request_id());
        let question = self.store.insert_worker_question(
            &question_id,
            &params.run_id,
            &params.slice_id,
            params.attempt,
            &params.question,
            &params.options,
            timeout_seconds,
        )?;
        let deadline_at = worker_question_deadline(&question);
        self.store.record_event(
            &params.run_id,
            "worker_question_asked",
            &json!({
                "question_id": question.id,
                "slice_id": question.slice_id,
                "attempt": question.attempt,
                "question": question.question,
                "options": question.options,
                "timeout_seconds": question.timeout_seconds,
                "deadline_at": deadline_at,
                "answer_command": worker_question_answer_command(&question),
            }),
        )?;
        let _ = self.store.update_progress(
            &params.run_id,
            "awaiting_operator",
            &params.slice_id,
            params.attempt,
            "ask_operator",
            &format!("awaiting operator answer: {}", params.question),
            "",
        );

        self.notify_attention_for_worker_question(&question);

        let deadline = if timeout_seconds == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(timeout_seconds))
        };
        while deadline.is_none_or(|deadline| Instant::now() < deadline) {
            let Some(current) = self.store.get_worker_question(&question_id)? else {
                bail!("question {question_id:?} disappeared");
            };
            if current.state == "answered" {
                return Ok(WorkerAskResult {
                    question_id,
                    state: "answered".to_string(),
                    answer: current.answer,
                    timed_out: false,
                });
            }
            let run = self.store.get_run(&params.run_id)?;
            if run.as_ref().is_some_and(|run| {
                matches!(
                    run.status,
                    RunStatus::Interrupted
                        | RunStatus::Cancelled
                        | RunStatus::Failed
                        | RunStatus::Blocked
                        | RunStatus::Completed
                )
            }) {
                bail!(
                    "run {} reached a terminal state before the question was answered",
                    params.run_id
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
        let question = self
            .store
            .timeout_worker_question(&question_id)?
            .ok_or_else(|| anyhow!("question {question_id:?} disappeared"))?;
        self.store.record_event(
            &params.run_id,
            "run_incident",
            &json!({
                "severity": "warning",
                "kind": "worker_question_timed_out",
                "message": format!("operator question timed out: {}", question.question),
                "question_id": question.id,
                "slice_id": question.slice_id,
            }),
        )?;
        Ok(WorkerAskResult {
            question_id,
            state: "timed_out".to_string(),
            answer: String::new(),
            timed_out: true,
        })
    }

    fn worker_question_timeout_seconds(&self, params: &WorkerAskParams) -> u64 {
        self.store
            .get_run(&params.run_id)
            .ok()
            .flatten()
            .and_then(|run| artifact::Store::new(&run.repo_path).read_config().ok())
            .map(|config| config.worker_question_timeout_seconds)
            .unwrap_or_else(|| {
                if params.timeout_seconds == 0 {
                    1800
                } else {
                    params.timeout_seconds
                }
            })
    }

    fn notify_attention_for_worker_question(&self, question: &WorkerQuestion) {
        let Ok(Some(run)) = self.store.get_run(&question.run_id) else {
            return;
        };
        let store = artifact::Store::new(&run.repo_path);
        let Ok(Some(origin)) = store.read_origin_notification_target(&run.id) else {
            return;
        };
        if origin.target.trim().is_empty() {
            return;
        }
        let payload = json!({
            "schema_version": 1,
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
        let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
        match send_default_agent_message(&origin.target, &text) {
            Ok(sent) => {
                let _ = self.store.record_event(
                    &run.id,
                    "attention_notification_sent",
                    &json!({
                        "kind": "worker_question_pending",
                        "question_id": question.id,
                        "slice_id": question.slice_id,
                        "adapter": sent.adapter,
                        "surface": sent.surface,
                        "target_kind": origin.target_kind,
                    }),
                );
            }
            Err(err) => {
                let _ = self.store.record_event(
                    &run.id,
                    "run_incident",
                    &json!({
                        "severity": "warning",
                        "kind": "attention_notification_failed",
                        "visibility_kind": "delivery_failed",
                        "message": format!("worker question notification was not delivered: {}", err.message),
                        "question_id": question.id,
                        "slice_id": question.slice_id,
                        "source_of_truth": "daemon_worker_questions",
                    }),
                );
            }
        }
        match focus_default_agent_target(&origin.target) {
            Ok(focused) => {
                let _ = self.store.record_event(
                    &run.id,
                    "attention_focus_sent",
                    &json!({
                        "kind": "worker_question_pending",
                        "question_id": question.id,
                        "slice_id": question.slice_id,
                        "adapter": focused.adapter,
                        "surface": focused.surface,
                        "target_kind": origin.target_kind,
                    }),
                );
            }
            Err(err) => {
                let _ = self.store.record_event(
                    &run.id,
                    "run_incident",
                    &json!({
                        "severity": "warning",
                        "kind": "attention_focus_failed",
                        "visibility_kind": "focus_failed",
                        "message": format!("worker question focus was not delivered: {}", err.message),
                        "question_id": question.id,
                        "slice_id": question.slice_id,
                        "source_of_truth": "daemon_worker_questions",
                    }),
                );
            }
        }
    }

    fn worker_question_is_currently_awaited(&self, question: &WorkerQuestion) -> Result<bool> {
        let Some(progress) = self.store.get_progress(&question.run_id)? else {
            return Ok(false);
        };
        Ok(progress.phase == "awaiting_operator"
            && progress.slice_id == question.slice_id
            && progress.attempt == question.attempt)
    }

    fn handle(&self, method: &str, raw: Option<serde_json::Value>) -> Result<HandleOutcome> {
        match method {
            "initRepo" => {
                let params: InitRepoParams = decode_params(raw)?;
                let repo = self.manager.init_repo(PathBuf::from(params.repo_path))?;
                Ok(HandleOutcome::result(InitRepoResult {
                    repo_id: repo.id,
                    repo_path: repo.path,
                })?)
            }
            "startRun" => {
                let params: StartRunParams = decode_params(raw)?;
                let mut slice_ids = params.slice_ids;
                if !params.slice_id.is_empty() {
                    slice_ids.push(params.slice_id);
                }
                let run = self.manager.start_run(StartOptions {
                    repo_path: PathBuf::from(params.repo_path),
                    slice_ids,
                    all: params.all,
                    agent: params.agent,
                    pi_bin: params.pi_bin,
                    pi_args: params.pi_args,
                    parallelism: params.parallelism,
                    allow_dirty: params.allow_dirty,
                    origin_notification_target: params.origin_notification_target,
                })?;
                Ok(HandleOutcome::result(StartRunResult { run_id: run.id })?)
            }
            "status" => {
                let params: StatusParams = raw
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                if !params.run_id.is_empty() {
                    let run: Run = self
                        .store
                        .get_run(&params.run_id)?
                        .ok_or_else(|| anyhow!("run {:?} not found", params.run_id))?;
                    let details = self.run_details(run, params.events_limit)?;
                    Ok(HandleOutcome::result(details)?)
                } else if params.latest {
                    if params.repo_path.trim().is_empty() {
                        bail!("status latest requires repo_path");
                    }
                    let details = self
                        .store
                        .latest_run_for_repo(&params.repo_path, params.active_only)?
                        .map(|run| self.run_details(run, params.events_limit))
                        .transpose()?;
                    Ok(HandleOutcome::result(details)?)
                } else {
                    let runs = self.store.latest_runs(params.limit)?;
                    Ok(HandleOutcome::value(json!({ "runs": runs })))
                }
            }
            "cancelRun" => {
                let params: CancelRunParams = decode_params(raw)?;
                let active = self.manager.cancel_run(&params.run_id, &params.reason)?;
                Ok(HandleOutcome::result(CancelRunResult {
                    run_id: params.run_id,
                    status: "cancel_requested".to_string(),
                    active,
                })?)
            }
            "resumeRun" => {
                let params: ResumeRunParams = decode_params(raw)?;
                let run = self.manager.resume_run(ResumeOptions {
                    run_id: params.run_id,
                    agent: params.agent,
                    pi_bin: params.pi_bin,
                    pi_args: params.pi_args,
                    parallelism: params.parallelism,
                })?;
                Ok(HandleOutcome::result(StartRunResult { run_id: run.id })?)
            }
            "listSlices" => {
                let params: SlicesParams = decode_params(raw)?;
                let report = self
                    .manager
                    .validate_slices(PathBuf::from(params.repo_path))?;
                Ok(HandleOutcome::result(ListSlicesResult {
                    slices: report.slices,
                    issues: report.issues,
                })?)
            }
            "createSlice" => {
                let params: SliceNewParams = decode_params(raw)?;
                let result: SliceWriteResult = self.manager.create_slice(SliceDraft {
                    repo_path: PathBuf::from(params.repo_path),
                    id: params.id,
                    title: params.title,
                    goal: params.goal,
                    github_issue: params.github_issue,
                    acceptance: params.acceptance,
                    verify: params.verify,
                    overwrite: params.overwrite,
                })?;
                Ok(HandleOutcome::result(result)?)
            }
            "importGithubIssue" => {
                let params: SliceImportGithubParams = decode_params(raw)?;
                let result: SliceWriteResult =
                    self.manager.import_github_issue(GithubImportOptions {
                        repo_path: PathBuf::from(params.repo_path),
                        issue: params.issue,
                        id: params.id,
                        verify: params.verify,
                        overwrite: params.overwrite,
                        dry_run: params.dry_run,
                    })?;
                Ok(HandleOutcome::result(result)?)
            }
            "handoffRun" => {
                let params: HandoffParams = decode_params(raw)?;
                let handoff: BranchHandoff = self.manager.branch_handoff(
                    &params.run_id,
                    params.push,
                    params.create_pr,
                    params.dry_run,
                )?;
                Ok(HandleOutcome::result(handoff)?)
            }
            "workerAsk" => {
                let params: WorkerAskParams = decode_params(raw)?;
                let result = self.handle_worker_ask(params)?;
                Ok(HandleOutcome::result(result)?)
            }
            "listQuestions" => {
                let params: ListQuestionsParams = raw
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let questions = if !params.run_id.trim().is_empty() {
                    self.store.list_worker_questions(&params.run_id)?
                } else if !params.repo_path.trim().is_empty() {
                    self.store
                        .list_worker_questions_for_repo(&params.repo_path)?
                } else {
                    bail!("listQuestions requires run_id or repo_path");
                };
                Ok(HandleOutcome::result(ListQuestionsResult { questions })?)
            }
            "answerQuestion" => {
                let params: AnswerQuestionParams = decode_params(raw)?;
                let run = self
                    .store
                    .get_run(&params.run_id)?
                    .ok_or_else(|| anyhow!("run {:?} not found", params.run_id))?;
                if matches!(run.status, RunStatus::Interrupted | RunStatus::Cancelled) {
                    bail!(
                        "run {} is {}; resume first before answering",
                        run.id,
                        run.status
                    );
                }
                let pending = self
                    .store
                    .get_worker_question(&params.question_id)?
                    .ok_or_else(|| anyhow!("question {:?} not found", params.question_id))?;
                if pending.run_id != params.run_id {
                    bail!(
                        "question {:?} belongs to run {}, not {}",
                        params.question_id,
                        pending.run_id,
                        params.run_id
                    );
                }
                if pending.state != "pending" {
                    bail!("question {:?} is already {}", pending.id, pending.state);
                }
                if !self.worker_question_is_currently_awaited(&pending)? {
                    bail!(
                        "question {} is not attached to the active worker attempt; resume the run and answer the fresh pending question shown by status/watch/monitor",
                        pending.id
                    );
                }
                let question = self.store.answer_worker_question(
                    &params.run_id,
                    &params.question_id,
                    &params.answer,
                )?;
                self.store.record_event(
                    &params.run_id,
                    "worker_question_answered",
                    &json!({
                        "question_id": question.id,
                        "slice_id": question.slice_id,
                        "answer": question.answer,
                    }),
                )?;
                Ok(HandleOutcome::result(AnswerQuestionResult { question })?)
            }
            "listReplanProposals" => {
                let params: ListReplanProposalsParams = decode_params(raw)?;
                if params.run_id.trim().is_empty() {
                    bail!("listReplanProposals requires run_id");
                }
                let status = self.replan_status(&params.run_id)?;
                let proposals = status.pending.into_iter().chain(status.history).collect();
                Ok(HandleOutcome::result(ListReplanProposalsResult {
                    proposals,
                })?)
            }
            "createReplanProposal" => {
                let params: CreateReplanProposalParams = decode_params(raw)?;
                let proposal = self.store.create_replan_proposal(
                    &params.run_id,
                    &params.id,
                    params.source,
                    params.trigger_finding_ids,
                    params.evidence,
                    params.proposed_changes,
                    &params.risk,
                )?;
                let proposal = enrich_replan_proposal(&params.run_id, proposal);
                self.store.record_event(
                    &params.run_id,
                    "replan_proposal_created",
                    &json!({
                        "proposal_id": proposal.id,
                        "state": proposal.state,
                        "risk": proposal.risk,
                        "source": proposal.source,
                        "proposed_changes": proposal.proposed_changes,
                        "decision_commands": proposal.decision_commands,
                    }),
                )?;
                Ok(HandleOutcome::result(CreateReplanProposalResult {
                    proposal,
                })?)
            }
            "decideReplanProposal" => {
                let params: DecideReplanProposalParams = decode_params(raw)?;
                let state = replan_decision_state(&params.decision)?;
                let proposal = self.store.decide_replan_proposal(
                    &params.run_id,
                    &params.proposal_id,
                    state,
                    &params.rationale,
                    &params.authorizer,
                    &params.source,
                    &params.replacement_id,
                    &params.revisit_condition,
                )?;
                self.store.record_event(
                    &params.run_id,
                    "replan_proposal_decided",
                    &json!({
                        "proposal_id": proposal.id,
                        "state": proposal.state,
                        "decision": proposal.operator_decision,
                    }),
                )?;
                Ok(HandleOutcome::result(DecideReplanProposalResult {
                    proposal,
                })?)
            }
            "inspectRun" => {
                let params: InspectRunParams = decode_params(raw)?;
                let inspection: RunInspection = self
                    .manager
                    .inspect_run(&params.run_id, params.log_tail_lines)?;
                Ok(HandleOutcome::result(inspection)?)
            }
            "validateSlices" => {
                let params: SlicesParams = decode_params(raw)?;
                let report = self
                    .manager
                    .validate_slices(PathBuf::from(params.repo_path))?;
                Ok(HandleOutcome::result(report)?)
            }
            "shutdown" => {
                let active = self.manager.active_run_count();
                if active > 0 {
                    bail!("cannot stop daemon while {active} run(s) are active");
                }
                self.store.cancel_running_runs("daemon stopped")?;
                Ok(HandleOutcome {
                    result: json!({ "status": "stopping" }),
                    should_shutdown: true,
                })
            }
            _ => bail!("unknown method {method:?}"),
        }
    }
}

fn replan_status_from_proposals(run_id: &str, proposals: Vec<ReplanProposal>) -> ReplanStatus {
    let mut pending = Vec::new();
    let mut history = Vec::new();
    for proposal in proposals {
        let proposal = enrich_replan_proposal(run_id, proposal);
        if proposal.state == ReplanProposalState::Pending {
            pending.push(proposal);
        } else {
            history.push(proposal);
        }
    }
    let pending_attention_reason = if pending.is_empty() {
        String::new()
    } else {
        format!(
            "awaiting replan decision for {}",
            pending
                .iter()
                .map(|proposal| proposal.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    ReplanStatus {
        pending_attention_reason,
        pending,
        history,
        auto_approvable: Vec::new(),
    }
}

fn enrich_replan_proposal(run_id: &str, mut proposal: ReplanProposal) -> ReplanProposal {
    proposal.decision_commands = if proposal.state == ReplanProposalState::Pending {
        replan_decision_commands(run_id, &proposal.id)
    } else {
        Vec::new()
    };
    proposal
}

fn replan_decision_state(value: &str) -> Result<ReplanProposalState> {
    match value {
        "accepted" | "accept" => Ok(ReplanProposalState::Accepted),
        "rejected" | "reject" => Ok(ReplanProposalState::Rejected),
        "deferred" | "defer" => Ok(ReplanProposalState::Deferred),
        "superseded" | "supersede" => Ok(ReplanProposalState::Superseded),
        other => bail!("unknown replan decision {other:?}"),
    }
}

fn primary_terminal_reason(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
    recent_events: &[Event],
    incident_events: &[Event],
    questions: &[WorkerQuestion],
) -> Option<TerminalReason> {
    if !matches!(
        run.status,
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted
    ) {
        return None;
    }

    let source = terminal_reason_source(run, slice_runs, progress, recent_events, incident_events);
    let mut commands = Vec::new();
    for question in questions
        .iter()
        .filter(|question| question.state == "pending")
    {
        push_unique_command(
            &mut commands,
            format!(
                "khazad-doom answer {} {} <answer>",
                question.run_id, question.id
            ),
        );
    }
    for command in source.fix_commands {
        push_unique_command(&mut commands, command);
    }
    for command in terminal_inspection_commands(run) {
        push_unique_command(&mut commands, command);
    }

    Some(TerminalReason {
        kind: source.kind,
        resolution_owner: source.resolution_owner,
        retryable: source.retryable,
        operator_action_required: source.operator_action_required,
        summary: source.summary,
        evidence_links: source.evidence_links,
        remediation: source.remediation,
        disposition: source.disposition,
        operator_commands: commands,
    })
}

struct TerminalReasonSource {
    kind: String,
    resolution_owner: String,
    retryable: bool,
    operator_action_required: bool,
    summary: String,
    evidence_links: Vec<String>,
    remediation: String,
    disposition: String,
    fix_commands: Vec<String>,
}

fn terminal_reason_source(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
    recent_events: &[Event],
    incident_events: &[Event],
) -> TerminalReasonSource {
    if let Some(event) = terminal_incident_event(incident_events) {
        return terminal_reason_from_event(run, event);
    }
    if let Some(event) = terminal_run_error_event(incident_events)
        .or_else(|| terminal_run_error_event(recent_events))
    {
        return terminal_reason_from_event(run, event);
    }

    let summary = terminal_summary_text(run, slice_runs, progress);
    let kind = match run.status {
        RunStatus::Blocked => "blocked",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => "terminal",
    }
    .to_string();
    TerminalReasonSource {
        kind,
        resolution_owner: default_resolution_owner(run.status),
        retryable: default_retryable(run.status),
        operator_action_required: default_operator_action_required(run.status),
        summary,
        evidence_links: default_evidence_links(run),
        remediation: default_remediation(run.status),
        disposition: default_disposition(run.status),
        fix_commands: Vec::new(),
    }
}

fn terminal_incident_event(events: &[Event]) -> Option<&Event> {
    events.iter().rev().find(|event| {
        event.typ == "run_incident"
            && (event.payload.get("failure_kind").is_some()
                || event.payload.get("operator_action_required").is_some()
                || payload_string(&event.payload, "severity") == Some("error".to_string()))
    })
}

fn terminal_run_error_event(events: &[Event]) -> Option<&Event> {
    events.iter().rev().find(|event| event.typ == "run_error")
}

fn terminal_reason_from_event(run: &Run, event: &Event) -> TerminalReasonSource {
    let payload = &event.payload;
    let kind = payload_string(payload, "failure_kind")
        .or_else(|| payload_string(payload, "kind"))
        .unwrap_or_else(|| match event.typ.as_str() {
            "run_error" => run.status.as_str().to_string(),
            other => other.to_string(),
        });
    let operator_action_required = payload
        .get("operator_action_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| default_operator_action_required(run.status));
    let retryable = payload
        .get("retryable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| default_retryable(run.status));
    let summary = payload_string(payload, "message")
        .or_else(|| payload_string(payload, "error"))
        .or_else(|| payload_string(payload, "summary"))
        .unwrap_or_else(|| fallback_run_error(run));
    let resolution_owner = payload_string(payload, "resolution_owner").unwrap_or_else(|| {
        if operator_action_required {
            "operator".to_string()
        } else {
            default_resolution_owner(run.status)
        }
    });
    let mut evidence_links = default_evidence_links(run);
    push_unique_command(
        &mut evidence_links,
        format!("event:{}:{}", event.id, event.typ),
    );
    TerminalReasonSource {
        kind,
        resolution_owner,
        retryable,
        operator_action_required,
        summary,
        evidence_links,
        remediation: remediation_for(run.status, operator_action_required, retryable),
        disposition: default_disposition(run.status),
        fix_commands: string_array(payload, "fix_commands"),
    }
}

fn terminal_summary_text(
    run: &Run,
    slice_runs: &[SliceRun],
    progress: Option<&RunProgress>,
) -> String {
    if !run.error.trim().is_empty() {
        return run.error.clone();
    }
    if let Some(slice_run) = slice_runs
        .iter()
        .find(|slice_run| !slice_run.last_error.trim().is_empty())
    {
        return slice_run.last_error.clone();
    }
    if let Some(progress) = progress
        && !progress.message.trim().is_empty()
    {
        return progress.message.clone();
    }
    fallback_run_error(run)
}

fn fallback_run_error(run: &Run) -> String {
    format!("run ended with status {}", run.status)
}

fn default_resolution_owner(status: RunStatus) -> String {
    match status {
        RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted => "operator",
        RunStatus::Failed => "daemon",
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => "daemon",
    }
    .to_string()
}

fn default_retryable(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted
    )
}

fn default_operator_action_required(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted
    )
}

fn default_remediation(status: RunStatus) -> String {
    remediation_for(
        status,
        default_operator_action_required(status),
        default_retryable(status),
    )
}

fn remediation_for(status: RunStatus, operator_action_required: bool, retryable: bool) -> String {
    if operator_action_required {
        return "complete the listed operator action, then resume the run".to_string();
    }
    if retryable {
        return "inspect artifacts, fix the underlying failure, then resume the run".to_string();
    }
    match status {
        RunStatus::Failed => "inspect artifacts and create a follow-up slice if needed".to_string(),
        _ => "inspect artifacts before taking further action".to_string(),
    }
}

fn default_disposition(status: RunStatus) -> String {
    match status {
        RunStatus::Blocked => "blocked; handoff is not ready until the operator action is resolved",
        RunStatus::Failed => "failed; handoff is not ready until the failure is resolved",
        RunStatus::Cancelled => "cancelled by request; handoff is not ready",
        RunStatus::Interrupted => "interrupted; resume from checkpoint before handoff",
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => {
            "terminal disposition unavailable"
        }
    }
    .to_string()
}

fn default_evidence_links(run: &Run) -> Vec<String> {
    let store = artifact::Store::new(&run.repo_path);
    let summary_path = store.output_path(&run.id, "run-summary.json");
    if summary_path.exists() {
        vec![summary_path.to_string_lossy().to_string()]
    } else {
        Vec::new()
    }
}

fn terminal_inspection_commands(run: &Run) -> Vec<String> {
    match run.status {
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => {
            vec![
                format!("khazad-doom inspect --run {}", run.id),
                format!("khazad-doom resume --run {}", run.id),
            ]
        }
        RunStatus::Pending | RunStatus::Running | RunStatus::Completed => Vec::new(),
    }
}

fn payload_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn push_unique_command(commands: &mut Vec<String>, command: String) {
    if !command.trim().is_empty() && !commands.iter().any(|existing| existing == &command) {
        commands.push(command);
    }
}

fn run_incidents_from_events(events: &[Event]) -> Vec<RunIncident> {
    events
        .iter()
        .filter_map(|event| {
            let payload = &event.payload;
            let (severity, kind, message) = match event.typ.as_str() {
                "run_incident" => (
                    payload_text(payload, "severity", "warning"),
                    payload_text(payload, "kind", "run_incident"),
                    payload_text(payload, "message", "incident recorded"),
                ),
                "run_error" => (
                    "error".to_string(),
                    "run_error".to_string(),
                    payload_text(payload, "error", "run failed"),
                ),
                "run_resumed" => (
                    "warning".to_string(),
                    "run_resumed".to_string(),
                    "run resumed after a terminal/interrupted state".to_string(),
                ),
                "worktree_cleanup_error" | "daemon_recovery_cleanup_error" => (
                    "warning".to_string(),
                    event.typ.clone(),
                    payload_text(payload, "error", "worktree cleanup reported an error"),
                ),
                "integration_repair_completed" => (
                    "warning".to_string(),
                    "integration_repair_completed".to_string(),
                    [
                        payload_text(payload, "status", ""),
                        payload_text(payload, "summary", "integration repair completed"),
                    ]
                    .into_iter()
                    .filter(|part| !part.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
                ),
                _ => return None,
            };
            Some(RunIncident {
                severity,
                kind,
                message,
                event_id: event.id,
                created_at: event.created_at,
            })
        })
        .collect()
}

fn payload_text(payload: &serde_json::Value, field: &str, fallback: &str) -> String {
    payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn read_run_economics(run: &Run) -> Result<RunEconomics> {
    let store = artifact::Store::new(&run.repo_path);
    let live_path = store.output_path(&run.id, "economics.json");
    if live_path.exists()
        && let Ok(economics) = artifact::read_json(live_path)
    {
        return Ok(economics);
    }
    let summary: ImplementationSummary =
        artifact::read_json(store.output_path(&run.id, "final-report.json"))?;
    Ok(summary.economics)
}

fn read_worker_profile(
    run: &Run,
    economics: Option<&RunEconomics>,
) -> Option<WorkerProfileEvidence> {
    let store = artifact::Store::new(&run.repo_path);
    if let Ok(summary) = artifact::read_json::<ImplementationSummary>(
        store.output_path(&run.id, "final-report.json"),
    ) && !summary.worker_profile.is_empty()
    {
        return Some(summary.worker_profile);
    }
    if let Ok(value) =
        artifact::read_json::<serde_json::Value>(store.output_path(&run.id, "preflight.json"))
        && let Some(profile) = WorkerProfileEvidence::from_json_surface(&value)
    {
        return Some(profile);
    }
    economics.and_then(|economics| {
        economics.agent_calls.iter().find_map(|call| {
            let value = json!({
                "agent": call.runner,
                "agent_profile": call.agent_profile,
                "agent_provider": call.agent_provider,
                "agent_model": call.agent_model,
                "agent_reasoning": call.agent_reasoning,
                "agent_mode": call.agent_mode,
                "profile_summary": call.profile_summary,
                "launch_summary": call.launch_summary,
                "worker_evidence_kind": call.worker_evidence_kind(),
                "worker_evidence_label": call.worker_evidence_label(),
            });
            WorkerProfileEvidence::from_json_surface(&value)
        })
    })
}

fn annotate_parallel_progress(
    progress: &mut RunProgress,
    slice_runs: &[SliceRun],
    events: &[Event],
) {
    if progress.phase == "parallel_worker_layer" && !progress.slice_id.trim().is_empty() {
        progress.parallel_layer = true;
        progress.parallel_slices = split_parallel_slice_ids(&progress.slice_id);
        return;
    }
    if !is_worker_layer_phase(&progress.phase) {
        return;
    }
    let active: Vec<_> = slice_runs
        .iter()
        .filter(|slice_run| is_parallel_layer_slice_status(slice_run.status))
        .map(|slice_run| slice_run.slice_id.clone())
        .collect();
    if active.len() > 1 {
        progress.parallel_layer = true;
        progress.parallel_slices = active;
        return;
    }
    if let Some(layer) = current_parallel_layer_from_events(events) {
        progress.parallel_layer = true;
        progress.parallel_slices = layer;
    }
}

fn is_worker_layer_phase(phase: &str) -> bool {
    matches!(
        phase,
        "worker_started" | "worker_running" | "worker_verify" | "ready_to_merge"
    )
}

fn current_parallel_layer_from_events(events: &[Event]) -> Option<Vec<String>> {
    for event in events.iter().rev() {
        match event.typ.as_str() {
            "parallel_layer_completed" | "parallel_layer_failed" => return None,
            "parallel_layer_started" => {
                let slices = event
                    .payload
                    .get("slices")
                    .and_then(serde_json::Value::as_array)?
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                return (slices.len() > 1).then_some(slices);
            }
            _ => {}
        }
    }
    None
}

fn split_parallel_slice_ids(slice_ids: &str) -> Vec<String> {
    slice_ids
        .split(',')
        .map(str::trim)
        .filter(|slice_id| !slice_id.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_parallel_layer_slice_status(status: SliceStatus) -> bool {
    !matches!(status, SliceStatus::Pending | SliceStatus::Merged)
}

struct HandleOutcome {
    result: serde_json::Value,
    should_shutdown: bool,
}

impl HandleOutcome {
    fn value(result: serde_json::Value) -> Self {
        Self {
            result,
            should_shutdown: false,
        }
    }

    fn result<T: Serialize>(result: T) -> Result<Self> {
        Ok(Self::value(serde_json::to_value(result)?))
    }
}

fn method_allows_concurrent_handling(method: &str) -> bool {
    matches!(
        method,
        "workerAsk" | "answerQuestion" | "listQuestions" | "listReplanProposals"
    )
}

fn decode_params<T: DeserializeOwned>(raw: Option<serde_json::Value>) -> Result<T> {
    let raw = raw.unwrap_or_else(|| json!({}));
    Ok(serde_json::from_value(raw)?)
}

fn write_json_line<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn is_missing_socket_error(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    let Some(io) = err.downcast_ref::<std::io::Error>() else {
        return false;
    };
    matches!(
        io.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn live_daemon_pid(paths: &Paths) -> Option<u32> {
    let pid = fs::read_to_string(paths.pid_file())
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    process_is_alive(pid).then_some(pid)
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn request_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn terminal_reason_prefers_structured_incident_payload() {
        let now = Utc::now();
        let run = run_with_status(RunStatus::Blocked, "Pi is not authenticated");
        let event = Event {
            id: 7,
            run_id: run.id.clone(),
            typ: "run_incident".to_string(),
            payload: json!({
                "severity": "error",
                "kind": "agent_auth_required",
                "failure_kind": "agent_auth_required",
                "message": "Pi is not authenticated",
                "operator_action_required": true,
                "retryable": false,
                "fix_commands": ["pi /login"]
            }),
            created_at: now,
        };

        let reason = primary_terminal_reason(&run, &[], None, &[], &[event], &[]).expect("reason");

        assert_eq!(reason.kind, "agent_auth_required");
        assert_eq!(reason.resolution_owner, "operator");
        assert!(!reason.retryable);
        assert!(reason.operator_action_required);
        assert!(
            reason
                .evidence_links
                .iter()
                .any(|link| link == "event:7:run_incident")
        );
        assert!(
            reason
                .operator_commands
                .iter()
                .any(|command| command == "pi /login")
        );
        assert!(
            reason
                .operator_commands
                .iter()
                .any(|command| command == "khazad-doom resume --run kd-test")
        );
    }

    #[test]
    fn terminal_reason_failed_run_falls_back_to_run_error() {
        let run = run_with_status(RunStatus::Failed, "integration gate failed");

        let reason = primary_terminal_reason(&run, &[], None, &[], &[], &[]).expect("reason");

        assert_eq!(reason.kind, "failed");
        assert_eq!(reason.resolution_owner, "daemon");
        assert!(reason.retryable);
        assert!(!reason.operator_action_required);
        assert_eq!(reason.summary, "integration gate failed");
        assert!(
            reason
                .operator_commands
                .iter()
                .any(|command| command == "khazad-doom inspect --run kd-test")
        );
    }

    fn run_with_status(status: RunStatus, error: &str) -> Run {
        let now = Utc::now();
        Run {
            id: "kd-test".to_string(),
            repo_id: "repo".to_string(),
            repo_path: "/tmp/repo".to_string(),
            status,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "khazad/kd-test/integration".to_string(),
            selected_slice_id: "slice-1".to_string(),
            error: error.to_string(),
            started_at: now,
            updated_at: now,
        }
    }
}
