use crate::artifact;
use crate::domain::{
    BranchHandoff, ReplanProposalState, Run, RunDetails, RunInspection, RunStatus,
    SliceWriteResult, WorkerQuestion,
};
use crate::ipc::{
    AnswerQuestionParams, AnswerQuestionResult, CancelRunParams, CancelRunResult,
    CreateReplanProposalParams, CreateReplanProposalResult, DecideReplanProposalParams,
    DecideReplanProposalResult, HandoffParams, InitRepoParams, InitRepoResult, InspectRunParams,
    ListQuestionsParams, ListQuestionsResult, ListReplanProposalsParams, ListReplanProposalsResult,
    ListSlicesResult, Request, Response, ResumeRunParams, SliceImportGithubParams, SliceNewParams,
    SlicesParams, StartRunParams, StartRunResult, StatusParams, WorkerAskParams, WorkerAskResult,
    WorkerQuestionTimeoutParams,
};
use crate::paths::Paths;
use crate::state::Store as StateStore;
use crate::workflow::attention::worker_question_deadline;
use crate::workflow::events as workflow_events;
use crate::workflow::read_model::{enrich_replan_proposal, replan_status_from_proposals};
use crate::workflow::{
    GithubImportOptions, Manager, ResumeOptions, RunReadModelBuilder, RunReadModelOptions,
    SliceDraft, StartOptions,
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
        Ok(RunReadModelBuilder::new(&self.store)
            .snapshot(&run, RunReadModelOptions::status(events_limit))?
            .details)
    }

    fn handle_worker_ask_open(&self, params: WorkerAskParams) -> Result<WorkerAskResult> {
        let (question, timeout_seconds) = self.open_worker_question(&params)?;
        self.schedule_worker_question_timeout(question.clone());
        Ok(WorkerAskResult {
            question_id: question.id,
            state: "pending".to_string(),
            answer: String::new(),
            timed_out: false,
            timeout_seconds,
        })
    }

    fn handle_worker_ask(&self, params: WorkerAskParams) -> Result<WorkerAskResult> {
        let (question, timeout_seconds) = self.open_worker_question(&params)?;
        let question_id = question.id.clone();
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
                    timeout_seconds,
                });
            }
            if current.state == "timed_out" {
                return Ok(WorkerAskResult {
                    question_id,
                    state: "timed_out".to_string(),
                    answer: String::new(),
                    timed_out: true,
                    timeout_seconds,
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
        let question = self.timeout_worker_question(
            &params.run_id,
            &question_id,
            "worker_question_timed_out",
            "operator question timed out",
        )?;
        if question.state == "answered" {
            return Ok(WorkerAskResult {
                question_id,
                state: "answered".to_string(),
                answer: question.answer,
                timed_out: false,
                timeout_seconds: question.timeout_seconds,
            });
        }
        Ok(WorkerAskResult {
            question_id,
            state: question.state,
            answer: String::new(),
            timed_out: true,
            timeout_seconds: question.timeout_seconds,
        })
    }

    fn open_worker_question(&self, params: &WorkerAskParams) -> Result<(WorkerQuestion, u64)> {
        if !self
            .store
            .validate_worker_token(&params.run_id, &params.token)?
        {
            self.store.record_event(
                &params.run_id,
                workflow_events::RUN_INCIDENT,
                &workflow_events::RunIncidentPayload::error(
                    "worker_question_token_rejected",
                    "workerAsk rejected because the worker token did not match the run",
                )
                .with_extra("slice_id", &params.slice_id),
            )?;
            bail!("worker token rejected for run {}", params.run_id);
        }
        let timeout_seconds = self.worker_question_timeout_seconds(params);
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
            workflow_events::WORKER_QUESTION_ASKED,
            &workflow_events::WorkerQuestionAskedPayload::from_question(&question, deadline_at),
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
        Ok((question, timeout_seconds))
    }

    fn handle_worker_question_timeout(
        &self,
        params: WorkerQuestionTimeoutParams,
    ) -> Result<WorkerAskResult> {
        if !self
            .store
            .validate_worker_token(&params.run_id, &params.token)?
        {
            self.store.record_event(
                &params.run_id,
                workflow_events::RUN_INCIDENT,
                &workflow_events::RunIncidentPayload::error(
                    "worker_question_token_rejected",
                    "workerQuestionTimeout rejected because the worker token did not match the run",
                ),
            )?;
            bail!("worker token rejected for run {}", params.run_id);
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
                "question {} is not attached to the active worker attempt",
                pending.id
            );
        }
        let question = self.timeout_worker_question(
            &params.run_id,
            &params.question_id,
            "worker_question_cancelled",
            "operator question closed without an answer",
        )?;
        let timed_out = question.state == "timed_out";
        Ok(WorkerAskResult {
            question_id: question.id,
            state: question.state,
            answer: if timed_out {
                String::new()
            } else {
                question.answer
            },
            timed_out,
            timeout_seconds: question.timeout_seconds,
        })
    }

    fn timeout_worker_question(
        &self,
        run_id: &str,
        question_id: &str,
        incident_code: &str,
        message_prefix: &str,
    ) -> Result<WorkerQuestion> {
        let question = self
            .store
            .timeout_worker_question(question_id)?
            .ok_or_else(|| anyhow!("question {question_id:?} disappeared"))?;
        if question.state != "timed_out" {
            return Ok(question);
        }
        self.store.record_event(
            run_id,
            workflow_events::RUN_INCIDENT,
            &workflow_events::RunIncidentPayload::warning(
                incident_code,
                format!("{message_prefix}: {}", question.question),
            )
            .with_extra("question_id", &question.id)
            .with_extra("slice_id", &question.slice_id),
        )?;
        let _ = self.store.update_progress(
            run_id,
            "worker_running",
            &question.slice_id,
            question.attempt,
            "ask_operator",
            &format!(
                "operator answer unavailable for {}; worker applying blocked contract",
                question.id
            ),
            "",
        );
        Ok(question)
    }

    fn schedule_worker_question_timeout(&self, question: WorkerQuestion) {
        if question.timeout_seconds == 0 {
            return;
        }
        let server = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(question.timeout_seconds));
            let Ok(Some(current)) = server.store.get_worker_question(&question.id) else {
                return;
            };
            if current.state != "pending" {
                return;
            }
            let _ = server.timeout_worker_question(
                &question.run_id,
                &question.id,
                "worker_question_timed_out",
                "operator question timed out",
            );
        });
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
        self.manager.notify_worker_question_attention(question);
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
                    native_pi_tui_worker: params.native_pi_tui_worker,
                    parallelism: params.parallelism,
                    allow_dirty: params.allow_dirty,
                    origin_notification_target: params.origin_notification_target,
                    mission_envelope: params.mission_envelope,
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
                    native_pi_tui_worker: params.native_pi_tui_worker,
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
            "workerAskOpen" => {
                let params: WorkerAskParams = decode_params(raw)?;
                let result = self.handle_worker_ask_open(params)?;
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
                    workflow_events::WORKER_QUESTION_ANSWERED,
                    &workflow_events::WorkerQuestionAnsweredPayload::new(
                        &question.id,
                        &question.slice_id,
                        &question.answer,
                    ),
                )?;
                let _ = self.store.update_progress(
                    &params.run_id,
                    "worker_running",
                    &question.slice_id,
                    question.attempt,
                    "ask_operator",
                    &format!("operator answered {}; worker resuming", question.id),
                    "",
                );
                Ok(HandleOutcome::result(AnswerQuestionResult { question })?)
            }
            "workerQuestionTimeout" => {
                let params: WorkerQuestionTimeoutParams = decode_params(raw)?;
                let result = self.handle_worker_question_timeout(params)?;
                Ok(HandleOutcome::result(result)?)
            }
            "listReplanProposals" => {
                let params: ListReplanProposalsParams = decode_params(raw)?;
                if params.run_id.trim().is_empty() {
                    bail!("listReplanProposals requires run_id");
                }
                let status = replan_status_from_proposals(
                    &params.run_id,
                    self.store.list_replan_proposals(&params.run_id)?,
                );
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
                self.manager
                    .notify_replan_attention(&params.run_id, &proposal);
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

fn replan_decision_state(value: &str) -> Result<ReplanProposalState> {
    match value {
        "accepted" | "accept" => Ok(ReplanProposalState::Accepted),
        "rejected" | "reject" => Ok(ReplanProposalState::Rejected),
        "deferred" | "defer" => Ok(ReplanProposalState::Deferred),
        "superseded" | "supersede" => Ok(ReplanProposalState::Superseded),
        other => bail!("unknown replan decision {other:?}"),
    }
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
        "workerAsk"
            | "workerAskOpen"
            | "workerQuestionTimeout"
            | "answerQuestion"
            | "listQuestions"
            | "listReplanProposals"
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
    use crate::domain::Event;
    use crate::workflow::read_model::primary_terminal_reason;
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
