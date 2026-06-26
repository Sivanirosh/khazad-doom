use crate::agent::RunnerSpec;
use crate::daemon::{Client, Server};
use crate::domain::{
    BranchHandoff, RunDetails, RunInspection, RunStatus, SliceValidationReport, SliceWriteResult,
};
use crate::ipc::{
    CancelRunParams, CancelRunResult, HandoffParams, InitRepoParams, InitRepoResult,
    InspectRunParams, ListSlicesResult, ResumeRunParams, SliceImportGithubParams, SliceNewParams,
    SlicesParams, StartRunParams, StartRunResult, StatusParams,
};
use crate::paths::Paths;
use crate::state::Store as StateStore;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(name = "khazad-doom", about = "Khazad-Doom — You shall not slop.")]
struct Cli {
    #[command(subcommand)]
    command: CommandArgs,
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Initialize a repository-local .workflow contract area.
    Init {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Start a Khazad-Doom run for selected JSON Issue Slices.
    Run {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Slice id to run. Repeat to select multiple. Dependencies are included automatically.
        #[arg(long = "slice")]
        slices: Vec<String>,
        /// Run all slices in dependency order. This is also the default when no --slice is given.
        #[arg(long)]
        all: bool,
        /// Agent adapter to use: pi or fake. Defaults to KHAZAD_AGENT or pi.
        #[arg(long, default_value = "")]
        agent: String,
        /// Run independent slice workers concurrently, then merge serially.
        #[arg(long, default_value_t = 1)]
        parallel: usize,
        #[arg(long)]
        wait: bool,
    },
    /// Resume an interrupted/failed/cancelled run from its durable checkpoint.
    Resume {
        #[arg(long)]
        run: String,
        /// Agent adapter to use: pi or fake. Defaults to KHAZAD_AGENT or pi.
        #[arg(long, default_value = "")]
        agent: String,
        /// Run independent slice workers concurrently, then merge serially.
        #[arg(long, default_value_t = 1)]
        parallel: usize,
        #[arg(long)]
        wait: bool,
    },
    /// Request cancellation for a run.
    Cancel {
        #[arg(long)]
        run: String,
        #[arg(long, default_value = "cancel requested")]
        reason: String,
    },
    /// Build a JSON branch/PR handoff for a completed run.
    Handoff {
        #[arg(long)]
        run: String,
        /// Execute `git push -u origin <integration-branch>`.
        #[arg(long)]
        push: bool,
        /// Execute `gh pr create` after the branch is available remotely.
        #[arg(long)]
        create_pr: bool,
    },
    /// Inspect run artifacts and daemon log tail.
    Inspect {
        #[arg(long)]
        run: String,
        #[arg(long, default_value_t = 50)]
        log_tail: usize,
    },
    /// Show daemon status or run status.
    Status {
        #[arg(long, default_value = "")]
        run: String,
        #[arg(long, default_value_t = 50)]
        events_limit: usize,
    },
    /// Inspect repo-local JSON Issue Slices.
    Slices {
        #[command(subcommand)]
        command: SlicesCommand,
    },
    /// Manage the per-user daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SlicesCommand {
    List {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    Validate {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Generate a new JSON Issue Slice template.
    New {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        goal: String,
        #[arg(long, default_value = "")]
        github_issue: String,
        #[arg(long = "acceptance")]
        acceptance: Vec<String>,
        #[arg(long = "verify")]
        verify: Vec<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Import a GitHub issue into a JSON Issue Slice using `gh issue view`.
    ImportGithub {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        issue: String,
        #[arg(long, default_value = "")]
        id: String,
        #[arg(long = "verify")]
        verify: Vec<String>,
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Start,
    Stop,
    Status,
    Serve,
}

pub fn run(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> Result<()> {
    let cli = Cli::parse_from(
        std::iter::once(OsString::from("khazad-doom")).chain(args.into_iter().map(Into::into)),
    );
    let paths = Paths::resolve()?;
    match cli.command {
        CommandArgs::Init { repo } => run_init(paths, repo),
        CommandArgs::Run {
            repo,
            slices,
            all,
            agent,
            parallel,
            wait,
        } => run_start(paths, repo, slices, all, agent, parallel, wait),
        CommandArgs::Resume {
            run,
            agent,
            parallel,
            wait,
        } => run_resume(paths, run, agent, parallel, wait),
        CommandArgs::Cancel { run, reason } => run_cancel(paths, run, reason),
        CommandArgs::Handoff {
            run,
            push,
            create_pr,
        } => run_handoff(paths, run, push, create_pr),
        CommandArgs::Inspect { run, log_tail } => run_inspect(paths, run, log_tail),
        CommandArgs::Status { run, events_limit } => run_status(paths, run, events_limit),
        CommandArgs::Slices { command } => run_slices(paths, command),
        CommandArgs::Daemon { command } => run_daemon(paths, command),
    }
}

fn run_init(paths: Paths, repo: PathBuf) -> Result<()> {
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let repo = resolve_repo_path(repo)?;
    let result: InitRepoResult = client.call(
        "initRepo",
        &InitRepoParams {
            repo_path: repo.to_string_lossy().to_string(),
        },
    )?;
    print_json(&result)
}

fn run_start(
    paths: Paths,
    repo: PathBuf,
    slices: Vec<String>,
    all: bool,
    agent: String,
    parallel: usize,
    wait: bool,
) -> Result<()> {
    let runner = RunnerSpec::from_agent_and_env(&agent)?;
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let repo = resolve_repo_path(repo)?;
    let result: StartRunResult = client.call(
        "startRun",
        &StartRunParams {
            repo_path: repo.to_string_lossy().to_string(),
            slice_id: String::new(),
            slice_ids: slices,
            all,
            agent: runner.kind,
            pi_bin: runner.pi_bin,
            pi_args: runner.pi_args,
            parallelism: parallel,
        },
    )?;
    if !wait {
        return print_json(&result);
    }
    wait_run(&client, &result.run_id)
}

fn run_resume(
    paths: Paths,
    run_id: String,
    agent: String,
    parallel: usize,
    wait: bool,
) -> Result<()> {
    let runner = RunnerSpec::from_agent_and_env(&agent)?;
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let result: StartRunResult = client.call(
        "resumeRun",
        &ResumeRunParams {
            run_id,
            agent: runner.kind,
            pi_bin: runner.pi_bin,
            pi_args: runner.pi_args,
            parallelism: parallel,
        },
    )?;
    if !wait {
        return print_json(&result);
    }
    wait_run(&client, &result.run_id)
}

fn run_cancel(paths: Paths, run_id: String, reason: String) -> Result<()> {
    let client = Client::new(paths);
    let result: CancelRunResult = client.call("cancelRun", &CancelRunParams { run_id, reason })?;
    print_json(&result)
}

fn run_handoff(paths: Paths, run_id: String, push: bool, create_pr: bool) -> Result<()> {
    let client = Client::new(paths);
    let handoff: BranchHandoff = client.call(
        "handoffRun",
        &HandoffParams {
            run_id,
            push,
            create_pr,
        },
    )?;
    print_json(&handoff)
}

fn run_inspect(paths: Paths, run_id: String, log_tail_lines: usize) -> Result<()> {
    let client = Client::new(paths);
    let inspection: RunInspection = client.call(
        "inspectRun",
        &InspectRunParams {
            run_id,
            log_tail_lines,
        },
    )?;
    print_json(&inspection)
}

fn run_status(paths: Paths, run_id: String, events_limit: usize) -> Result<()> {
    let client = Client::new(paths);
    if !run_id.is_empty() {
        let details: RunDetails = client.call(
            "status",
            &StatusParams {
                run_id,
                limit: 0,
                events_limit,
            },
        )?;
        return print_json(&details);
    }
    let out: serde_json::Value = client.call(
        "status",
        &StatusParams {
            run_id: String::new(),
            limit: 10,
            events_limit: 0,
        },
    )?;
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn run_slices(paths: Paths, command: SlicesCommand) -> Result<()> {
    match command {
        SlicesCommand::List { repo } => {
            ensure_daemon(&paths)?;
            let client = Client::new(paths);
            let repo = resolve_repo_path(repo)?;
            let result: ListSlicesResult = client.call(
                "listSlices",
                &SlicesParams {
                    repo_path: repo.to_string_lossy().to_string(),
                },
            )?;
            print_json(&result)
        }
        SlicesCommand::Validate { repo } => {
            ensure_daemon(&paths)?;
            let client = Client::new(paths);
            let repo = resolve_repo_path(repo)?;
            let report: SliceValidationReport = client.call(
                "validateSlices",
                &SlicesParams {
                    repo_path: repo.to_string_lossy().to_string(),
                },
            )?;
            print_json(&report)?;
            if report.valid {
                Ok(())
            } else {
                bail!("slice validation failed")
            }
        }
        SlicesCommand::New {
            repo,
            id,
            title,
            goal,
            github_issue,
            acceptance,
            verify,
            overwrite,
        } => {
            ensure_daemon(&paths)?;
            let client = Client::new(paths);
            let repo = resolve_repo_path(repo)?;
            let result: SliceWriteResult = client.call(
                "createSlice",
                &SliceNewParams {
                    repo_path: repo.to_string_lossy().to_string(),
                    id,
                    title,
                    goal,
                    github_issue,
                    acceptance,
                    verify,
                    overwrite,
                },
            )?;
            print_json(&result)
        }
        SlicesCommand::ImportGithub {
            repo,
            issue,
            id,
            verify,
            overwrite,
        } => {
            ensure_daemon(&paths)?;
            let client = Client::new(paths);
            let repo = resolve_repo_path(repo)?;
            let result: SliceWriteResult = client.call(
                "importGithubIssue",
                &SliceImportGithubParams {
                    repo_path: repo.to_string_lossy().to_string(),
                    issue,
                    id,
                    verify,
                    overwrite,
                },
            )?;
            print_json(&result)
        }
    }
}

fn run_daemon(paths: Paths, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start => start_daemon(&paths),
        DaemonCommand::Stop => {
            let client = Client::new(paths);
            let result: serde_json::Value = client.call("shutdown", &serde_json::json!({}))?;
            print_json(&result)
        }
        DaemonCommand::Status => {
            let client = Client::new(paths);
            client.ping()?;
            println!("running");
            Ok(())
        }
        DaemonCommand::Serve => serve_daemon(paths),
    }
}

fn ensure_daemon(paths: &Paths) -> Result<()> {
    let client = Client::new(paths.clone());
    if client.ping().is_ok() {
        return Ok(());
    }
    start_daemon(paths)
}

fn start_daemon(paths: &Paths) -> Result<()> {
    let client = Client::new(paths.clone());
    if client.ping().is_ok() {
        println!("daemon already running");
        return Ok(());
    }
    paths.ensure()?;
    let exe = std::env::current_exe().context("resolve current executable")?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.daemon_log())
        .with_context(|| format!("open {}", paths.daemon_log().display()))?;
    let stderr = log_file.try_clone()?;
    let mut child = Command::new(exe)
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("start daemon")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if client.ping().is_ok() {
            println!("daemon started pid={}", child.id());
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "daemon exited during startup with {status}; see {}",
                paths.daemon_log().display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "daemon did not become ready; see {}",
        paths.daemon_log().display()
    )
}

fn serve_daemon(paths: Paths) -> Result<()> {
    paths.ensure()?;
    let state = StateStore::open(paths.db_file())?;
    let server = Server::new(paths, state);
    server.serve()
}

fn wait_run(client: &Client, run_id: &str) -> Result<()> {
    loop {
        let details: RunDetails = client.call(
            "status",
            &StatusParams {
                run_id: run_id.to_string(),
                limit: 0,
                events_limit: 50,
            },
        )?;
        print_json(&details)?;
        match details.run.status {
            RunStatus::Completed => return Ok(()),
            RunStatus::Failed
            | RunStatus::Blocked
            | RunStatus::Cancelled
            | RunStatus::Interrupted => {
                bail!(
                    "run ended with status {}: {}",
                    details.run.status,
                    details.run.error
                );
            }
            _ => thread::sleep(Duration::from_secs(2)),
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn resolve_repo_path(repo: PathBuf) -> Result<PathBuf> {
    repo.canonicalize()
        .with_context(|| format!("resolve repository path {}", repo.display()))
}
