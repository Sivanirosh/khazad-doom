use crate::agent::RunnerSpec;
use crate::artifact;
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
use chrono::Utc;
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
        /// Print handoff only; suppress configured push/PR defaults.
        #[arg(long)]
        dry_run: bool,
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
        /// Repository path used with --latest. Defaults to the current directory for latest lookup.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Return the latest active run details for a repository, or null if none exists.
        #[arg(long)]
        latest: bool,
        #[arg(long, default_value_t = 50)]
        events_limit: usize,
        /// Follow a run with compact human-readable progress until it reaches a terminal state.
        #[arg(long)]
        follow: bool,
        /// Poll interval for --follow, in milliseconds.
        #[arg(long, default_value_t = 2000)]
        interval_ms: u64,
    },
    /// Watch a run with compact human-readable progress until it reaches a terminal state.
    Watch {
        #[arg(long)]
        run: String,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 2000)]
        interval_ms: u64,
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
        #[arg(long)]
        dry_run: bool,
    },
    /// Print or write the JSON Schema for Issue Slices.
    Schema {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        write: bool,
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
            dry_run,
        } => run_handoff(paths, run, push, create_pr, dry_run),
        CommandArgs::Inspect { run, log_tail } => run_inspect(paths, run, log_tail),
        CommandArgs::Status {
            run,
            repo,
            latest,
            events_limit,
            follow,
            interval_ms,
        } => run_status(paths, run, repo, latest, events_limit, follow, interval_ms),
        CommandArgs::Watch { run, interval_ms } => run_watch(paths, run, interval_ms),
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
    let repo = resolve_repo_path(repo)?;
    let config = artifact::Store::new(&repo)
        .read_config()
        .unwrap_or_default();
    let effective_agent = if agent.trim().is_empty() && std::env::var("KHAZAD_AGENT").is_err() {
        config.agent.clone()
    } else {
        agent
    };
    let runner = RunnerSpec::from_agent_and_env(&effective_agent)?;
    let parallel = effective_cli_parallelism(parallel, config.parallelism);
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
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
    let runner = if agent.trim().is_empty() && std::env::var("KHAZAD_AGENT").is_err() {
        None
    } else {
        Some(RunnerSpec::from_agent_and_env(&agent)?)
    };
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let result: StartRunResult = client.call(
        "resumeRun",
        &ResumeRunParams {
            run_id,
            agent: runner
                .as_ref()
                .map(|runner| runner.kind.clone())
                .unwrap_or_default(),
            pi_bin: runner
                .as_ref()
                .map(|runner| runner.pi_bin.clone())
                .unwrap_or_else(|| std::env::var("KHAZAD_PI_BIN").unwrap_or_default()),
            pi_args: runner
                .as_ref()
                .map(|runner| runner.pi_args.clone())
                .unwrap_or_else(|| {
                    std::env::var("KHAZAD_PI_ARGS")
                        .unwrap_or_default()
                        .split_whitespace()
                        .map(str::to_string)
                        .collect()
                }),
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

fn run_handoff(
    paths: Paths,
    run_id: String,
    push: bool,
    create_pr: bool,
    dry_run: bool,
) -> Result<()> {
    let client = Client::new(paths);
    let handoff: BranchHandoff = client.call(
        "handoffRun",
        &HandoffParams {
            run_id,
            push,
            create_pr,
            dry_run,
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

fn run_status(
    paths: Paths,
    run_id: String,
    repo: Option<PathBuf>,
    latest: bool,
    events_limit: usize,
    follow: bool,
    interval_ms: u64,
) -> Result<()> {
    if follow {
        if run_id.is_empty() {
            bail!("status --follow requires --run <run-id>");
        }
        if latest {
            bail!("status --follow cannot be combined with --latest");
        }
        return run_watch(paths, run_id, interval_ms);
    }
    let client = Client::new(paths);
    if !run_id.is_empty() {
        if latest {
            bail!("status --latest cannot be combined with --run <run-id>");
        }
        let details: RunDetails = client.call(
            "status",
            &StatusParams {
                run_id,
                events_limit,
                ..StatusParams::default()
            },
        )?;
        return print_json(&details);
    }
    if latest {
        let repo = resolve_repo_path(repo.unwrap_or_else(|| PathBuf::from(".")))?;
        let details: Option<RunDetails> = client.call(
            "status",
            &StatusParams {
                repo_path: repo.to_string_lossy().to_string(),
                latest: true,
                active_only: true,
                events_limit,
                ..StatusParams::default()
            },
        )?;
        return print_json(&details);
    }
    if repo.is_some() {
        bail!("status --repo requires --latest");
    }
    let out: serde_json::Value = client.call(
        "status",
        &StatusParams {
            limit: 10,
            ..StatusParams::default()
        },
    )?;
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn run_watch(paths: Paths, run_id: String, interval_ms: u64) -> Result<()> {
    let client = Client::new(paths);
    let interval = Duration::from_millis(interval_ms.max(100));
    loop {
        let details: RunDetails = client.call(
            "status",
            &StatusParams {
                run_id: run_id.clone(),
                events_limit: 5,
                ..StatusParams::default()
            },
        )?;
        print_watch_snapshot(&details);
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
            _ => thread::sleep(interval),
        }
    }
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
            dry_run,
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
                    dry_run,
                },
            )?;
            print_json(&result)
        }
        SlicesCommand::Schema { repo, write } => {
            let schema = artifact::slice_schema();
            if write {
                let repo = resolve_repo_path(repo)?;
                artifact::Store::new(repo).write_slice_schema()?;
            }
            print_json(&schema)
        }
    }
}

fn run_daemon(paths: Paths, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start => start_daemon(&paths, true),
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
    start_daemon(paths, false)
}

fn start_daemon(paths: &Paths, announce: bool) -> Result<()> {
    let client = Client::new(paths.clone());
    if client.ping().is_ok() {
        if announce {
            println!("daemon already running");
        }
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
            if announce {
                println!("daemon started pid={}", child.id());
            }
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

fn print_watch_snapshot(details: &RunDetails) {
    println!("Run: {}", details.run.id);
    println!("Status: {}", details.run.status);
    if let Some(progress) = &details.progress {
        let elapsed = Utc::now()
            .signed_duration_since(progress.phase_started_at)
            .to_std()
            .unwrap_or_default();
        println!("Phase: {}", progress.phase);
        if !progress.slice_id.is_empty() {
            println!("Slice: {}", progress.slice_id);
        }
        if !progress.command.is_empty() {
            println!("Command: {}", progress.command);
        }
        println!("Elapsed: {}", format_duration(elapsed));
        println!("Updated: {}", progress.updated_at);
        println!("Message: {}", progress.message);
        if !progress.output_tail.trim().is_empty() {
            println!("Last output:");
            for line in progress
                .output_tail
                .trim_end()
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                println!("  {line}");
            }
        }
    } else {
        let elapsed = Utc::now()
            .signed_duration_since(details.run.started_at)
            .to_std()
            .unwrap_or_default();
        println!("Phase: unknown");
        println!("Elapsed: {}", format_duration(elapsed));
    }
    println!();
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn wait_run(client: &Client, run_id: &str) -> Result<()> {
    loop {
        let details: RunDetails = client.call(
            "status",
            &StatusParams {
                run_id: run_id.to_string(),
                events_limit: 50,
                ..StatusParams::default()
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

fn effective_cli_parallelism(requested: usize, configured: usize) -> usize {
    if requested > 1 {
        requested
    } else if configured > 0 {
        configured
    } else {
        requested.max(1)
    }
}

fn resolve_repo_path(repo: PathBuf) -> Result<PathBuf> {
    repo.canonicalize()
        .with_context(|| format!("resolve repository path {}", repo.display()))
}
