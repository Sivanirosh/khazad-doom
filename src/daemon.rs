use crate::domain::{BranchHandoff, Run, RunDetails, RunInspection, SliceWriteResult};
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        let mut stream = UnixStream::connect(self.paths.socket())?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
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
        let response: Response = serde_json::from_str(&line)?;
        if let Some(error) = response.error {
            bail!(error);
        }
        let result = response.result.context("daemon response missing result")?;
        Ok(serde_json::from_value(result)?)
    }

    pub fn ping(&self) -> Result<()> {
        let _: serde_json::Value = self.call(
            "status",
            &StatusParams {
                run_id: String::new(),
                limit: 1,
                events_limit: 0,
            },
        )?;
        Ok(())
    }
}

pub struct Server {
    paths: Paths,
    store: StateStore,
    manager: Manager,
}

impl Server {
    pub fn new(paths: Paths, store: StateStore) -> Self {
        let manager = Manager::new(paths.clone(), store.clone());
        Self {
            paths,
            store,
            manager,
        }
    }

    pub fn serve(&self) -> Result<()> {
        self.paths.ensure()?;
        let socket = self.paths.socket();
        if socket.exists() {
            if UnixStream::connect(&socket).is_ok() {
                bail!("daemon already running at {}", socket.display());
            }
            let _ = fs::remove_file(&socket);
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
        for stream in listener.incoming() {
            let stream = stream?;
            let shutdown = self.handle_conn(stream);
            match shutdown {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(err) => eprintln!("khazad-doom daemon: {err:#}"),
            }
        }
        Ok(())
    }

    fn handle_conn(&self, mut stream: UnixStream) -> Result<bool> {
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
        let handled = self.handle(request.method.as_str(), request.params.clone());
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
                    let details = RunDetails {
                        slice_runs: self.store.get_slice_runs(&params.run_id)?,
                        progress: self.store.get_progress(&params.run_id)?,
                        events: self.store.get_events(&params.run_id, params.events_limit)?,
                        run,
                    };
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

fn request_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}
