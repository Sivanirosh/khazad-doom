use crate::artifact;
use crate::domain::{
    BranchHandoff, Event, ImplementationSummary, Run, RunDetails, RunEconomics, RunIncident,
    RunInspection, RunProgress, SliceRun, SliceStatus, SliceWriteResult,
};
use crate::ipc::{
    CancelRunParams, CancelRunResult, HandoffParams, InitRepoParams, InitRepoResult,
    InspectRunParams, ListSlicesResult, Request, Response, ResumeRunParams,
    SliceImportGithubParams, SliceNewParams, SlicesParams, StartRunParams, StartRunResult,
    StatusParams,
};
use crate::paths::Paths;
use crate::state::Store as StateStore;
use crate::workflow::{GithubImportOptions, Manager, ResumeOptions, SliceDraft, StartOptions};
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
        let handled = {
            let _request_guard = self
                .request_lock
                .lock()
                .expect("daemon request mutex poisoned");
            self.handle(request.method.as_str(), request.params.clone())
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
        let incidents = run_incidents_from_events(&self.store.get_incident_events(&run_id)?);
        Ok(RunDetails {
            slice_runs,
            progress,
            incidents,
            events,
            economics,
            run,
        })
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
