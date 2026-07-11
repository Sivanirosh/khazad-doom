use crate::artifact;
use crate::domain::{
    BranchHandoff, ReplanProposalState, Run, RunDetails, RunInspection, RunStatus,
    SliceWriteResult, WorkerQuestion, WorkerQuestionAnswerSource, WorkerQuestionRecommendation,
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
use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        let question = self.open_worker_question(&params)?;
        self.schedule_worker_question_timeout(question.clone());
        Ok(worker_ask_result(&question))
    }

    fn handle_worker_ask(&self, params: WorkerAskParams) -> Result<WorkerAskResult> {
        let question = self.open_worker_question(&params)?;
        let question_id = question.id.clone();
        loop {
            let Some(current) = self.store.get_worker_question(&question_id)? else {
                bail!("question {question_id:?} disappeared");
            };
            if current.state != "pending" {
                return completed_worker_ask_result(&current);
            }
            let run = self.store.get_run(&params.run_id)?;
            if run
                .as_ref()
                .is_some_and(|run| run.status != RunStatus::Running)
            {
                let transition = self.store.interrupt_worker_question_if_inactive_cas(
                    &params.run_id,
                    &question_id,
                    "run reached a terminal state before the question was answered",
                )?;
                return completed_worker_ask_result(&transition.question);
            }
            if current
                .deadline_at
                .is_some_and(|deadline| Utc::now() >= deadline)
            {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let question = self.resolve_worker_question_deadline(
            &params.run_id,
            &question_id,
            "worker_question_timed_out",
            "operator question timed out",
        )?;
        completed_worker_ask_result(&question)
    }

    fn open_worker_question(&self, params: &WorkerAskParams) -> Result<WorkerQuestion> {
        if params.launch_id.is_some_and(|launch_id| launch_id <= 0) {
            bail!("worker question launch_id must be positive when supplied");
        }
        if !self.worker_question_token_is_authorized(
            &params.run_id,
            params.launch_id,
            &params.token,
        )? {
            self.store.record_event(
                &params.run_id,
                workflow_events::RUN_INCIDENT,
                &workflow_events::RunIncidentPayload::error(
                    "worker_question_token_rejected",
                    "workerAsk rejected because the worker token did not match the launch",
                )
                .with_extra("slice_id", &params.slice_id)
                .with_extra("launch_id", params.launch_id),
            )?;
            bail!(
                "worker token rejected for run {} launch {:?}",
                params.run_id,
                params.launch_id
            );
        }
        let timeout_seconds = self.worker_question_timeout_seconds(params);
        let question_id = format!("q-{}", request_id());
        let recommendation = WorkerQuestionRecommendation {
            recommended_answer: params.recommended_answer.clone(),
            rationale: params.rationale.clone(),
            bounded_within_current_slice_or_mission_authority: params
                .bounded_within_current_slice_or_mission_authority,
            reversible: params.reversible,
        };
        let question = self
            .store
            .open_active_worker_question_with_launch_id_and_recommendation(
                &question_id,
                &params.run_id,
                &params.slice_id,
                params.attempt,
                params.launch_id,
                &params.question,
                &params.options,
                timeout_seconds,
                &recommendation,
                workflow_events::WORKER_QUESTION_ASKED,
                |question| {
                    Ok(workflow_events::WorkerQuestionAskedPayload::from_question(
                        question,
                        worker_question_deadline(question),
                    ))
                },
                &format!("awaiting operator answer: {}", params.question),
            )?;

        self.notify_attention_for_worker_question(&question);
        Ok(question)
    }

    fn handle_worker_question_timeout(
        &self,
        params: WorkerQuestionTimeoutParams,
    ) -> Result<WorkerAskResult> {
        if params.launch_id.is_some_and(|launch_id| launch_id <= 0) {
            bail!("worker question launch_id must be positive when supplied");
        }
        if !self.worker_question_token_is_authorized(
            &params.run_id,
            params.launch_id,
            &params.token,
        )? {
            self.store.record_event(
                &params.run_id,
                workflow_events::RUN_INCIDENT,
                &workflow_events::RunIncidentPayload::error(
                    "worker_question_token_rejected",
                    "workerQuestionTimeout rejected because the worker token did not match the launch",
                )
                .with_extra("launch_id", params.launch_id),
            )?;
            bail!(
                "worker token rejected for run {} launch {:?}",
                params.run_id,
                params.launch_id
            );
        }
        let current = self
            .store
            .get_worker_question(&params.question_id)?
            .ok_or_else(|| anyhow!("question {:?} not found", params.question_id))?;
        if current.run_id != params.run_id {
            bail!(
                "question {:?} belongs to run {}, not {}",
                params.question_id,
                current.run_id,
                params.run_id
            );
        }
        if current.launch_id != params.launch_id {
            bail!(
                "question {:?} belongs to launch {:?}, not {:?}",
                params.question_id,
                current.launch_id,
                params.launch_id
            );
        }
        if current.state != "pending" {
            return Ok(worker_ask_result(&current));
        }
        if !self.worker_question_is_currently_awaited(&current)? {
            bail!(
                "question {} is not attached to the active worker attempt",
                current.id
            );
        }
        let question = if current
            .deadline_at
            .is_some_and(|deadline| Utc::now() >= deadline)
        {
            self.resolve_worker_question_deadline(
                &params.run_id,
                &params.question_id,
                "worker_question_timed_out",
                "operator question timed out",
            )?
        } else {
            self.timeout_worker_question(
                &params.run_id,
                &params.question_id,
                "worker_question_cancelled",
                "operator question closed without an answer",
            )?
        };
        Ok(worker_ask_result(&question))
    }

    fn resolve_worker_question_deadline(
        &self,
        run_id: &str,
        question_id: &str,
        incident_code: &str,
        message_prefix: &str,
    ) -> Result<WorkerQuestion> {
        let question = self
            .store
            .get_worker_question(question_id)?
            .ok_or_else(|| anyhow!("question {question_id:?} disappeared"))?;
        if question.run_id != run_id {
            bail!(
                "question {question_id:?} belongs to run {}, not {run_id}",
                question.run_id
            );
        }
        if question.state != "pending" {
            return Ok(question);
        }
        if !self.worker_question_is_currently_awaited(&question)? {
            return Ok(self
                .store
                .interrupt_worker_question_if_inactive_cas(
                    run_id,
                    question_id,
                    "worker attempt became inactive before question resolution",
                )?
                .question);
        }
        let recommendation = question.recommendation();
        if question.fallback_eligible && recommendation.is_eligible(&question.options) {
            let answer = question.recommended_answer.clone();
            let source = WorkerQuestionAnswerSource::LlmRecommendationTimeout;
            let payload = workflow_events::WorkerQuestionAnsweredPayload::from_question(
                &question, &answer, source,
            );
            let transition = self.store.answer_worker_question_cas(
                run_id,
                question_id,
                &answer,
                source,
                workflow_events::WORKER_QUESTION_ANSWERED,
                &payload,
                &format!(
                    "LLM recommendation applied at deadline for {}; worker resuming",
                    question.id
                ),
            )?;
            return Ok(transition.question);
        }
        self.timeout_worker_question(run_id, question_id, incident_code, message_prefix)
    }

    fn timeout_worker_question(
        &self,
        run_id: &str,
        question_id: &str,
        incident_code: &str,
        message_prefix: &str,
    ) -> Result<WorkerQuestion> {
        let current = self
            .store
            .get_worker_question(question_id)?
            .ok_or_else(|| anyhow!("question {question_id:?} disappeared"))?;
        if current.state != "pending" {
            return Ok(current);
        }
        let incident = workflow_events::RunIncidentPayload::warning(
            incident_code,
            format!("{message_prefix}: {}", current.question),
        )
        .with_extra("question_id", &current.id)
        .with_extra("slice_id", &current.slice_id);
        let transition = self.store.timeout_worker_question_cas(
            run_id,
            question_id,
            workflow_events::RUN_INCIDENT,
            &incident,
            &format!(
                "operator answer unavailable for {}; worker applying blocked contract",
                current.id
            ),
        )?;
        Ok(transition.question)
    }

    fn schedule_worker_question_timeout(&self, question: WorkerQuestion) {
        let Some(deadline_at) = question.deadline_at else {
            return;
        };
        let server = self.clone();
        thread::spawn(move || {
            let delay = (deadline_at - Utc::now()).to_std().unwrap_or_default();
            thread::sleep(delay);
            if let Err(error) = server.resolve_worker_question_deadline(
                &question.run_id,
                &question.id,
                "worker_question_timed_out",
                "operator question timed out",
            ) {
                eprintln!(
                    "khazad-doom: failed to resolve worker question {} at its deadline: {error:#}",
                    question.id
                );
                let _ = server.store.record_event(
                    &question.run_id,
                    workflow_events::RUN_INCIDENT,
                    &workflow_events::RunIncidentPayload::error(
                        "worker_question_deadline_resolution_failed",
                        format!(
                            "failed to resolve worker question {} at its deadline: {error:#}",
                            question.id
                        ),
                    )
                    .with_extra("question_id", &question.id)
                    .with_extra("slice_id", &question.slice_id),
                );
            }
        });
    }

    fn worker_question_token_is_authorized(
        &self,
        run_id: &str,
        launch_id: Option<i64>,
        token: &str,
    ) -> Result<bool> {
        match launch_id {
            Some(launch_id) => self
                .store
                .validate_worker_launch_token(run_id, launch_id, token),
            None => self.store.validate_worker_token(run_id, token),
        }
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
                    60
                } else {
                    params.timeout_seconds
                }
            })
    }

    fn notify_attention_for_worker_question(&self, question: &WorkerQuestion) {
        self.manager.notify_worker_question_attention(question);
    }

    fn worker_question_is_currently_awaited(&self, question: &WorkerQuestion) -> Result<bool> {
        self.store.worker_attempt_is_active_with_launch_id(
            &question.run_id,
            &question.slice_id,
            question.attempt,
            question.launch_id,
        )
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
                let current = self
                    .store
                    .get_worker_question(&params.question_id)?
                    .ok_or_else(|| anyhow!("question {:?} not found", params.question_id))?;
                if current.run_id != params.run_id {
                    bail!(
                        "question {:?} belongs to run {}, not {}",
                        params.question_id,
                        current.run_id,
                        params.run_id
                    );
                }
                if current.state == "answered" {
                    return HandleOutcome::result(AnswerQuestionResult {
                        question: current,
                        applied: false,
                    });
                }
                let run = self
                    .store
                    .get_run(&params.run_id)?
                    .ok_or_else(|| anyhow!("run {:?} not found", params.run_id))?;
                if run.status != RunStatus::Running {
                    bail!(
                        "run {} is {}; resume first before answering",
                        run.id,
                        run.status
                    );
                }
                if current.state != "pending" {
                    bail!(
                        "question {:?} is {}; it has no durable answer to return",
                        params.question_id,
                        current.state
                    );
                }
                if !self.worker_question_is_currently_awaited(&current)? {
                    bail!(
                        "question {} is not attached to the active worker attempt; resume the run and answer the fresh pending question shown by status/watch/monitor",
                        current.id
                    );
                }
                let source = WorkerQuestionAnswerSource::Operator;
                let payload = workflow_events::WorkerQuestionAnsweredPayload::from_question(
                    &current,
                    &params.answer,
                    source,
                );
                let transition = self.store.answer_worker_question_cas(
                    &params.run_id,
                    &params.question_id,
                    &params.answer,
                    source,
                    workflow_events::WORKER_QUESTION_ANSWERED,
                    &payload,
                    &format!("operator answered {}; worker resuming", current.id),
                )?;
                Ok(HandleOutcome::result(AnswerQuestionResult {
                    question: transition.question,
                    applied: transition.applied,
                })?)
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
                self.manager
                    .terminalize_inactive_runs_for_shutdown("daemon stopped")?;
                Ok(HandleOutcome {
                    result: json!({ "status": "stopping" }),
                    should_shutdown: true,
                })
            }
            _ => bail!("unknown method {method:?}"),
        }
    }
}

fn completed_worker_ask_result(question: &WorkerQuestion) -> Result<WorkerAskResult> {
    match question.state.as_str() {
        "answered" | "timed_out" => Ok(worker_ask_result(question)),
        "pending" => bail!(
            "question {} is no longer attached to an active worker attempt",
            question.id
        ),
        state => bail!(
            "question {} ended as {state} before it was answered",
            question.id
        ),
    }
}

fn worker_ask_result(question: &WorkerQuestion) -> WorkerAskResult {
    WorkerAskResult {
        question_id: question.id.clone(),
        state: question.state.clone(),
        answer: question.answer.clone(),
        answer_source: question.answer_source,
        timed_out: question.state == "timed_out",
        timeout_seconds: question.timeout_seconds,
        deadline_at: worker_question_deadline(question),
        recommended_answer: question.recommended_answer.clone(),
        recommendation_rationale: question.recommendation_rationale.clone(),
        fallback_eligible: question.fallback_eligible,
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
    use crate::domain::{Event, SliceRun, SliceStatus};
    use crate::workflow::read_model::primary_terminal_reason;
    use chrono::Utc;

    #[test]
    fn worker_question_rpc_timeout_fallback_wins_before_a_late_operator_answer() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(1)?;
        let opened = server.handle(
            "workerAskOpen",
            Some(eligible_worker_ask_params("run-fallback", "q choice")),
        )?;
        let question_id = opened.result["question_id"]
            .as_str()
            .expect("question id")
            .to_string();
        assert_eq!(opened.result["fallback_eligible"], true);
        assert!(opened.result["deadline_at"].as_str().is_some());

        wait_for_worker_question_state(&store, &question_id, "answered")?;
        let late_answer = server.handle(
            "answerQuestion",
            Some(json!({
                "run_id": "run-fallback",
                "question_id": question_id,
                "answer": "B"
            })),
        )?;

        assert_eq!(late_answer.result["applied"], false);
        assert_eq!(late_answer.result["question"]["state"], "answered");
        assert_eq!(late_answer.result["question"]["answer"], "A");
        assert_eq!(
            late_answer.result["question"]["answer_source"],
            "llm_recommendation_timeout"
        );
        let cancel_after_fallback = server.handle(
            "workerQuestionTimeout",
            Some(json!({
                "run_id": "run-fallback",
                "question_id": question_id,
                "token": "secret-token"
            })),
        )?;
        assert_eq!(cancel_after_fallback.result["timed_out"], false);
        assert_eq!(cancel_after_fallback.result["answer"], "A");
        assert_eq!(
            cancel_after_fallback.result["answer_source"],
            "llm_recommendation_timeout"
        );
        let answered_events = store
            .get_events("run-fallback", 100)?
            .into_iter()
            .filter(|event| event.typ == workflow_events::WORKER_QUESTION_ANSWERED)
            .collect::<Vec<_>>();
        assert_eq!(answered_events.len(), 1);
        assert_eq!(
            answered_events[0].payload["answer_source"],
            "llm_recommendation_timeout"
        );
        assert_eq!(answered_events[0].payload["recommended_answer"], "A");
        assert_eq!(
            answered_events[0].payload["recommendation_rationale"],
            "A is the smallest reversible option"
        );
        assert_eq!(
            store.get_progress("run-fallback")?.expect("progress").phase,
            "worker_running"
        );
        Ok(())
    }

    #[test]
    fn fallback_loser_receives_durable_winner_after_run_terminalizes() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(1)?;
        let opened = server.handle(
            "workerAskOpen",
            Some(eligible_worker_ask_params("run-fallback", "terminal race")),
        )?;
        let question_id = opened.result["question_id"]
            .as_str()
            .expect("question id")
            .to_string();
        wait_for_worker_question_state(&store, &question_id, "answered")?;
        store.update_run("run-fallback", RunStatus::Completed, "")?;

        let loser = server.handle(
            "answerQuestion",
            Some(json!({
                "run_id": "run-fallback",
                "question_id": question_id,
                "answer": "B"
            })),
        )?;

        assert_eq!(loser.result["applied"], false);
        assert_eq!(loser.result["question"]["answer"], "A");
        assert_eq!(
            loser.result["question"]["answer_source"],
            "llm_recommendation_timeout"
        );
        assert_eq!(
            store
                .get_events("run-fallback", 100)?
                .iter()
                .filter(|event| event.typ == workflow_events::WORKER_QUESTION_ANSWERED)
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn worker_question_open_persists_optional_positive_launch_id() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(1)?;
        let launch_id = store.list_worker_attempt_ledger("run-fallback", "slice-1")?[0].launch_id;
        let mut payload = eligible_worker_ask_params("run-fallback", "launch-scoped question");
        payload["launch_id"] = json!(launch_id);
        let opened = server.handle("workerAskOpen", Some(payload))?;
        let question_id = opened.result["question_id"].as_str().expect("question id");
        assert_eq!(
            store
                .get_worker_question(question_id)?
                .expect("durable question")
                .launch_id,
            Some(launch_id)
        );
        assert_eq!(
            store.list_worker_questions_for_repo(
                &store.get_run("run-fallback")?.expect("run").repo_path,
            )?[0]
                .launch_id,
            Some(launch_id)
        );

        let legacy = server.handle(
            "workerAskOpen",
            Some(eligible_worker_ask_params(
                "run-fallback",
                "legacy question",
            )),
        );
        assert!(
            legacy.is_err(),
            "active attempt already has a pending question"
        );
        Ok(())
    }

    #[test]
    fn resumed_launch_rejects_the_prior_launch_token_and_question() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(60)?;
        let first = store.list_worker_attempt_ledger("run-fallback", "slice-1")?[0].clone();
        let mut first_ask = eligible_worker_ask_params("run-fallback", "first launch question");
        first_ask["launch_id"] = json!(first.launch_id);
        let opened = server.handle("workerAskOpen", Some(first_ask))?;
        let old_question_id = opened.result["question_id"]
            .as_str()
            .expect("old question id")
            .to_string();

        store.finish_worker_attempt(first.launch_id, "interrupted", "resume")?;
        let interrupted = store.interrupt_worker_question_if_inactive_cas(
            "run-fallback",
            &old_question_id,
            "superseded by resume",
        )?;
        assert!(interrupted.applied);
        assert_eq!(interrupted.question.state, "interrupted");

        let second = store.allocate_worker_attempt(
            "run-fallback",
            "slice-1",
            2,
            1,
            0,
            0,
            "worker",
            &server.paths.root.join("worktrees"),
        )?;
        store.store_worker_launch_token("run-fallback", second.launch_id, "fresh-token")?;
        store.mark_worker_attempt_launched(second.launch_id)?;

        let mut stale_launch = eligible_worker_ask_params("run-fallback", "stale launch");
        stale_launch["launch_id"] = json!(first.launch_id);
        assert!(server.handle("workerAskOpen", Some(stale_launch)).is_err());

        let mut stale_token = eligible_worker_ask_params("run-fallback", "stale token");
        stale_token["launch_id"] = json!(second.launch_id);
        assert!(server.handle("workerAskOpen", Some(stale_token)).is_err());

        let mismatched_close = server.handle(
            "workerQuestionTimeout",
            Some(json!({
                "run_id": "run-fallback",
                "question_id": old_question_id,
                "token": "fresh-token",
                "launch_id": second.launch_id
            })),
        );
        assert!(mismatched_close.is_err());

        let mut fresh = eligible_worker_ask_params("run-fallback", "fresh launch");
        fresh["launch_id"] = json!(second.launch_id);
        fresh["token"] = json!("fresh-token");
        let fresh_opened = server.handle("workerAskOpen", Some(fresh))?;
        assert_ne!(fresh_opened.result["question_id"], old_question_id);
        assert_eq!(
            store
                .get_worker_question(&old_question_id)?
                .expect("old durable question")
                .state,
            "interrupted"
        );
        Ok(())
    }

    #[test]
    fn worker_question_rpc_operator_answer_committed_before_deadline_wins() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(1)?;
        let opened = server.handle(
            "workerAskOpen",
            Some(eligible_worker_ask_params("run-fallback", "q choice")),
        )?;
        let question_id = opened.result["question_id"]
            .as_str()
            .expect("question id")
            .to_string();
        let operator = server.handle(
            "answerQuestion",
            Some(json!({
                "run_id": "run-fallback",
                "question_id": question_id,
                "answer": "B"
            })),
        )?;
        assert_eq!(operator.result["applied"], true);

        thread::sleep(Duration::from_millis(1_250));
        let question = store
            .get_worker_question(&question_id)?
            .expect("durable question");
        assert_eq!(question.answer, "B");
        assert_eq!(
            question.answer_source,
            Some(WorkerQuestionAnswerSource::Operator)
        );
        assert_eq!(
            store
                .get_events("run-fallback", 100)?
                .iter()
                .filter(|event| event.typ == workflow_events::WORKER_QUESTION_ANSWERED)
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn operator_answer_after_absolute_deadline_wins_before_fallback_commits() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(1)?;
        let params: WorkerAskParams =
            serde_json::from_value(eligible_worker_ask_params("run-fallback", "q deadline"))?;
        let opened = server.open_worker_question(&params)?;
        thread::sleep(Duration::from_millis(1_100));

        let answered = server.handle(
            "answerQuestion",
            Some(json!({
                "run_id": "run-fallback",
                "question_id": opened.id,
                "answer": "B"
            })),
        )?;
        assert_eq!(answered.result["question"]["answer_source"], "operator");
        assert_eq!(answered.result["question"]["answer"], "B");
        assert_eq!(answered.result["applied"], true);
        assert_eq!(
            store
                .get_events("run-fallback", 100)?
                .iter()
                .filter(|event| event.typ == workflow_events::WORKER_QUESTION_ANSWERED)
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn worker_question_rpc_invalid_recommendation_keeps_legacy_timeout_behavior() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(1)?;
        let mut malformed = eligible_worker_ask_params("run-fallback", "q malformed");
        malformed["reversible"] = json!("yes");
        let malformed_opened = server.handle("workerAskOpen", Some(malformed))?;
        let malformed_id = malformed_opened.result["question_id"]
            .as_str()
            .expect("malformed question id")
            .to_string();
        assert_eq!(malformed_opened.result["fallback_eligible"], false);
        wait_for_worker_question_state(&store, &malformed_id, "timed_out")?;

        let mut params = eligible_worker_ask_params("run-fallback", "q invalid");
        params["recommended_answer"] = json!("not-an-option");
        let opened = server.handle("workerAskOpen", Some(params))?;
        let question_id = opened.result["question_id"]
            .as_str()
            .expect("question id")
            .to_string();
        assert_eq!(opened.result["fallback_eligible"], false);

        wait_for_worker_question_state(&store, &question_id, "timed_out")?;
        let question = store
            .get_worker_question(&question_id)?
            .expect("durable question");
        assert_eq!(question.state, "timed_out");
        assert_eq!(question.answer_source, None);
        assert!(
            store
                .get_events("run-fallback", 100)?
                .iter()
                .all(|event| event.typ != workflow_events::WORKER_QUESTION_ANSWERED)
        );
        Ok(())
    }

    #[test]
    fn stale_worker_question_attempt_cannot_open_or_replace_progress() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(60)?;
        let mut stale = eligible_worker_ask_params("run-fallback", "stale question");
        stale["attempt"] = json!(2);

        assert!(server.handle("workerAskOpen", Some(stale)).is_err());
        assert!(store.list_worker_questions("run-fallback")?.is_empty());
        let progress = store.get_progress("run-fallback")?.expect("progress");
        assert_eq!(progress.slice_id, "slice-1");
        assert_eq!(progress.attempt, 1);
        assert_eq!(progress.phase, "worker_running");
        Ok(())
    }

    #[test]
    fn headless_worker_ask_never_returns_an_interruption_reason_as_an_answer() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(0)?;
        let params = serde_json::from_value(eligible_worker_ask_params(
            "run-fallback",
            "question interrupted by retry",
        ))?;
        let waiter = {
            let server = server.clone();
            thread::spawn(move || server.handle_worker_ask(params))
        };

        let question = wait_for_pending_worker_question(&store, "run-fallback")?;
        store.activate_slice_attempt("run-fallback", "slice-1", 2)?;

        let error = waiter
            .join()
            .expect("headless workerAsk thread")
            .expect_err("interrupted question must not become a successful worker answer");
        assert!(error.to_string().contains("ended as interrupted"));
        let question = store
            .get_worker_question(&question.id)?
            .expect("durable interrupted question");
        assert_eq!(question.state, "interrupted");
        assert_eq!(question.answer, "superseded by worker attempt 2");
        assert_eq!(question.answer_source, None);
        Ok(())
    }

    #[test]
    fn headless_worker_ask_interrupts_pending_question_when_run_terminalizes() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(0)?;
        let params = serde_json::from_value(eligible_worker_ask_params(
            "run-fallback",
            "question interrupted by terminal run",
        ))?;
        let waiter = {
            let server = server.clone();
            thread::spawn(move || server.handle_worker_ask(params))
        };

        let question = wait_for_pending_worker_question(&store, "run-fallback")?;
        store.update_run("run-fallback", RunStatus::Completed, "")?;

        let error = waiter
            .join()
            .expect("headless workerAsk thread")
            .expect_err("terminal run must interrupt a pending worker question");
        assert!(error.to_string().contains("ended as interrupted"));
        let question = store
            .get_worker_question(&question.id)?
            .expect("durable interrupted question");
        assert_eq!(question.state, "interrupted");
        assert_eq!(
            question.answer,
            "run reached a terminal state before the question was answered"
        );
        assert_eq!(question.answer_source, None);
        Ok(())
    }

    #[test]
    fn deadline_resolution_interrupts_question_for_inactive_attempt() -> Result<()> {
        let (_dir, server, store) = worker_question_test_server(0)?;
        let opened = server.handle(
            "workerAskOpen",
            Some(eligible_worker_ask_params(
                "run-fallback",
                "inactive attempt",
            )),
        )?;
        let question_id = opened.result["question_id"]
            .as_str()
            .expect("question id")
            .to_string();
        store.update_run("run-fallback", RunStatus::Completed, "")?;

        let resolved = server.resolve_worker_question_deadline(
            "run-fallback",
            &question_id,
            "worker_question_timed_out",
            "operator question timed out",
        )?;

        assert_eq!(resolved.state, "interrupted");
        assert_eq!(resolved.answer_source, None);
        assert_eq!(
            store
                .get_events("run-fallback", 100)?
                .iter()
                .filter(|event| event.typ == "worker_question_interrupted")
                .count(),
            1
        );
        Ok(())
    }

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

    fn wait_for_pending_worker_question(
        store: &StateStore,
        run_id: &str,
    ) -> Result<WorkerQuestion> {
        for _ in 0..200 {
            if let Some(question) = store
                .list_worker_questions(run_id)?
                .into_iter()
                .find(|question| question.state == "pending")
            {
                return Ok(question);
            }
            thread::sleep(Duration::from_millis(25));
        }
        bail!("run {run_id:?} did not publish a pending worker question")
    }

    fn wait_for_worker_question_state(
        store: &StateStore,
        question_id: &str,
        expected: &str,
    ) -> Result<()> {
        for _ in 0..200 {
            let question = store
                .get_worker_question(question_id)?
                .ok_or_else(|| anyhow!("question {question_id:?} disappeared"))?;
            if question.state == expected {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        let question = store
            .get_worker_question(question_id)?
            .ok_or_else(|| anyhow!("question {question_id:?} disappeared"))?;
        bail!(
            "question {question_id:?} stayed {:?}; expected {expected:?}",
            question.state
        )
    }

    fn eligible_worker_ask_params(run_id: &str, question: &str) -> serde_json::Value {
        json!({
            "run_id": run_id,
            "slice_id": "slice-1",
            "token": "secret-token",
            "attempt": 1,
            "question": question,
            "options": ["A", "B"],
            "recommended_answer": "A",
            "rationale": "A is the smallest reversible option",
            "bounded_within_current_slice_or_mission_authority": true,
            "reversible": true
        })
    }

    fn worker_question_test_server(
        timeout_seconds: u64,
    ) -> Result<(tempfile::TempDir, Server, StateStore)> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().join("repo");
        fs::create_dir_all(repo_path.join(".workflow"))?;
        fs::write(
            repo_path.join(".workflow/khazad.json"),
            serde_json::to_vec_pretty(&json!({
                "worker_question_timeout_seconds": timeout_seconds
            }))?,
        )?;
        let paths = Paths {
            root: dir.path().join("khazad-home"),
        };
        let store = StateStore::open(paths.db_file())?;
        let mut run = run_with_status(RunStatus::Running, "");
        run.id = "run-fallback".to_string();
        run.repo_path = repo_path.to_string_lossy().into_owned();
        store.insert_run(&run)?;
        store.store_worker_token(&run.id, "secret-token")?;
        let launch = store.allocate_worker_attempt(
            &run.id,
            "slice-1",
            1,
            1,
            0,
            0,
            "worker",
            &paths.root.join("worktrees"),
        )?;
        store.store_worker_launch_token(&run.id, launch.launch_id, "secret-token")?;
        store.mark_worker_attempt_launched(launch.launch_id)?;
        store.upsert_slice_run(&SliceRun {
            run_id: run.id.clone(),
            slice_id: "slice-1".to_string(),
            status: SliceStatus::Running,
            branch: "khazad/test/slice-1".to_string(),
            commit_sha: String::new(),
            attempts: 1,
            last_error: String::new(),
        })?;
        store.update_progress(
            &run.id,
            "worker_running",
            "slice-1",
            1,
            "pi",
            "worker running",
            "",
        )?;
        let server = Server::new(paths, store.clone());
        Ok((dir, server, store))
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
