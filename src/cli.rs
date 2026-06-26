use crate::agent::RunnerSpec;
use crate::artifact;
use crate::daemon::{Client, Server};
use crate::domain::{
    BranchHandoff, Event, RunDetails, RunInspection, RunStatus, SliceStatus, SliceValidationReport,
    SliceWriteResult,
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
use std::io::{self, Write};
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
    /// Monitor a run or latest active repo run with a compact terminal dashboard.
    Monitor {
        /// Specific run id to follow.
        #[arg(long, default_value = "")]
        run: String,
        /// Repository path used with --latest. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Attach to the latest active run for the repository and wait for future runs.
        #[arg(long)]
        latest: bool,
        /// Render one dashboard snapshot and exit.
        #[arg(long)]
        once: bool,
        /// Number of events to fetch before displaying the compact recent event tail.
        #[arg(long, default_value_t = 20)]
        events_limit: usize,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
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
        CommandArgs::Monitor {
            run,
            repo,
            latest,
            once,
            events_limit,
            interval_ms,
        } => run_monitor(paths, run, repo, latest, once, events_limit, interval_ms),
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

fn run_monitor(
    paths: Paths,
    run_id: String,
    repo: Option<PathBuf>,
    latest: bool,
    once: bool,
    events_limit: usize,
    interval_ms: u64,
) -> Result<()> {
    if !run_id.is_empty() && latest {
        bail!("monitor --latest cannot be combined with --run <run-id>");
    }
    if run_id.is_empty() && !latest {
        bail!("monitor requires --run <run-id> or --latest");
    }
    if !run_id.is_empty() && repo.is_some() {
        bail!("monitor --repo can only be used with --latest");
    }

    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let interval = Duration::from_millis(interval_ms.max(100));
    let clear_screen = stdout_is_terminal();
    if !run_id.is_empty() {
        return monitor_run(&client, run_id, once, events_limit, interval, clear_screen);
    }

    let repo = resolve_repo_path(repo.unwrap_or_else(|| PathBuf::from(".")))?;
    monitor_latest(
        &client,
        repo.to_string_lossy().to_string(),
        once,
        events_limit,
        interval,
        clear_screen,
    )
}

fn monitor_run(
    client: &Client,
    run_id: String,
    once: bool,
    events_limit: usize,
    interval: Duration,
    clear_screen: bool,
) -> Result<()> {
    let mut first = true;
    loop {
        let details = fetch_run_details(client, &run_id, events_limit)?;
        render_monitor_snapshot(Some(&details), None, clear_screen, !clear_screen && !first)?;
        first = false;
        if once {
            return monitor_once_result(&details);
        }
        match details.run.status {
            RunStatus::Completed => return Ok(()),
            RunStatus::Failed
            | RunStatus::Blocked
            | RunStatus::Cancelled
            | RunStatus::Interrupted => return terminal_run_error(&details),
            _ => thread::sleep(interval),
        }
    }
}

fn monitor_latest(
    client: &Client,
    repo_path: String,
    once: bool,
    events_limit: usize,
    interval: Duration,
    clear_screen: bool,
) -> Result<()> {
    let mut attached_run_id: Option<String> = None;
    let mut first = true;
    loop {
        let details = if let Some(run_id) = attached_run_id.clone() {
            Some(fetch_run_details(client, &run_id, events_limit)?)
        } else {
            let latest = fetch_latest_active_run(client, &repo_path, events_limit)?;
            if let Some(details) = &latest {
                attached_run_id = Some(details.run.id.clone());
            }
            latest
        };
        render_monitor_snapshot(
            details.as_ref(),
            Some(&repo_path),
            clear_screen,
            !clear_screen && !first,
        )?;
        first = false;

        if once {
            if let Some(details) = &details {
                return monitor_once_result(details);
            }
            return Ok(());
        }
        if details
            .as_ref()
            .is_some_and(|details| is_terminal_status(details.run.status))
        {
            attached_run_id = None;
        }
        thread::sleep(interval);
    }
}

fn fetch_run_details(client: &Client, run_id: &str, events_limit: usize) -> Result<RunDetails> {
    client.call(
        "status",
        &StatusParams {
            run_id: run_id.to_string(),
            events_limit,
            ..StatusParams::default()
        },
    )
}

fn fetch_latest_active_run(
    client: &Client,
    repo_path: &str,
    events_limit: usize,
) -> Result<Option<RunDetails>> {
    client.call(
        "status",
        &StatusParams {
            repo_path: repo_path.to_string(),
            latest: true,
            active_only: true,
            events_limit,
            ..StatusParams::default()
        },
    )
}

fn monitor_once_result(details: &RunDetails) -> Result<()> {
    match details.run.status {
        RunStatus::Failed | RunStatus::Blocked | RunStatus::Cancelled | RunStatus::Interrupted => {
            terminal_run_error(details)
        }
        _ => Ok(()),
    }
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
            | RunStatus::Interrupted => return terminal_run_error(&details),
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

const MONITOR_EVENT_LINES: usize = 5;
const MONITOR_OUTPUT_LINES: usize = 12;
const MONITOR_LINE_WIDTH: usize = 180;

fn render_monitor_snapshot(
    details: Option<&RunDetails>,
    waiting_repo: Option<&str>,
    clear_screen: bool,
    separator: bool,
) -> Result<()> {
    let mut out = io::stdout();
    if clear_screen {
        write!(out, "\x1b[2J\x1b[H")?;
    } else if separator {
        writeln!(out, "---")?;
    }

    writeln!(out, "Khazad-Doom Monitor")?;
    match details {
        Some(details) => render_run_monitor(&mut out, details)?,
        None => render_waiting_monitor(&mut out, waiting_repo.unwrap_or("-"))?,
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn render_waiting_monitor(out: &mut impl Write, repo: &str) -> Result<()> {
    writeln!(out, "Run: -")?;
    writeln!(out, "Repo: {repo}")?;
    writeln!(out, "Status: waiting")?;
    writeln!(out, "Phase: waiting")?;
    writeln!(out, "Slice: -")?;
    writeln!(out, "Command: -")?;
    writeln!(out, "Elapsed: 0s")?;
    writeln!(out, "Updated: -")?;
    writeln!(out, "Message: waiting for latest active run")?;
    writeln!(out, "Recent events:")?;
    writeln!(out, "  -")?;
    writeln!(out, "Output tail:")?;
    writeln!(out, "  -")?;
    Ok(())
}

fn render_run_monitor(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    let progress = details.progress.as_ref();
    let phase = match progress {
        Some(progress) if !progress.phase.trim().is_empty() => progress.phase.as_str(),
        _ if is_terminal_status(details.run.status) => details.run.status.as_str(),
        _ => "unknown",
    };
    let command = progress
        .map(|progress| progress.command.as_str())
        .unwrap_or_default();
    let message = monitor_message(details);
    let elapsed_start = progress
        .map(|progress| progress.phase_started_at)
        .unwrap_or(details.run.started_at);
    let elapsed = Utc::now()
        .signed_duration_since(elapsed_start)
        .to_std()
        .unwrap_or_default();
    let updated = progress
        .map(|progress| progress.updated_at.to_rfc3339())
        .unwrap_or_else(|| details.run.updated_at.to_rfc3339());

    writeln!(out, "Run: {}", details.run.id)?;
    writeln!(out, "Repo: {}", details.run.repo_path)?;
    writeln!(out, "Status: {}", details.run.status)?;
    writeln!(out, "Phase: {phase}")?;
    writeln!(out, "Slice: {}", monitor_slice_label(details))?;
    writeln!(out, "Command: {}", display_or_dash(command))?;
    writeln!(out, "Elapsed: {}", format_duration(elapsed))?;
    writeln!(out, "Updated: {updated}")?;
    writeln!(
        out,
        "Message: {}",
        truncate_display(&message, MONITOR_LINE_WIDTH)
    )?;
    writeln!(out, "Recent events:")?;
    render_event_tail(out, &details.events)?;
    writeln!(out, "Output tail:")?;
    render_output_tail(
        out,
        progress
            .map(|progress| progress.output_tail.as_str())
            .unwrap_or_default(),
    )?;
    Ok(())
}

fn render_event_tail(out: &mut impl Write, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        writeln!(out, "  -")?;
        return Ok(());
    }
    let recent = events
        .iter()
        .rev()
        .take(MONITOR_EVENT_LINES)
        .collect::<Vec<_>>();
    for event in recent.into_iter().rev() {
        let summary = event_summary(event);
        if summary.is_empty() {
            writeln!(
                out,
                "  {} {}",
                event.created_at.format("%H:%M:%S"),
                event.typ
            )?;
        } else {
            writeln!(
                out,
                "  {} {} {}",
                event.created_at.format("%H:%M:%S"),
                event.typ,
                summary
            )?;
        }
    }
    Ok(())
}

fn render_output_tail(out: &mut impl Write, output_tail: &str) -> Result<()> {
    let trimmed = output_tail.trim_end();
    if trimmed.is_empty() {
        writeln!(out, "  -")?;
        return Ok(());
    }
    let lines = trimmed
        .lines()
        .rev()
        .take(MONITOR_OUTPUT_LINES)
        .collect::<Vec<_>>();
    for line in lines.into_iter().rev() {
        writeln!(out, "  {}", truncate_display(line, MONITOR_LINE_WIDTH))?;
    }
    Ok(())
}

fn event_summary(event: &Event) -> String {
    let Some(map) = event.payload.as_object() else {
        return truncate_display(&event.payload.to_string(), 120);
    };
    let mut parts = Vec::new();
    for key in [
        "slice_id", "phase", "status", "message", "summary", "error", "command",
    ] {
        let Some(value) = map.get(key) else {
            continue;
        };
        let text = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        if !text.trim().is_empty() {
            parts.push(format!("{key}={}", truncate_display(&text, 80)));
        }
    }
    if parts.is_empty() {
        truncate_display(&event.payload.to_string(), 120)
    } else {
        truncate_display(&parts.join(" "), 160)
    }
}

fn monitor_message(details: &RunDetails) -> String {
    if let Some(progress) = &details.progress
        && !progress.message.trim().is_empty()
    {
        return progress.message.clone();
    }
    if !details.run.error.trim().is_empty() {
        return details.run.error.clone();
    }
    format!("run is {}", details.run.status)
}

fn monitor_slice_label(details: &RunDetails) -> String {
    if let Some(progress) = &details.progress
        && !progress.slice_id.trim().is_empty()
    {
        return progress.slice_id.clone();
    }
    for status in [
        SliceStatus::Running,
        SliceStatus::RepairNeeded,
        SliceStatus::ReadyToMerge,
        SliceStatus::Pending,
    ] {
        if let Some(slice_run) = details
            .slice_runs
            .iter()
            .find(|slice_run| slice_run.status == status)
        {
            return format!("{} ({})", slice_run.slice_id, slice_run.status);
        }
    }
    if details.slice_runs.len() == 1 {
        let slice_run = &details.slice_runs[0];
        return format!("{} ({})", slice_run.slice_id, slice_run.status);
    }
    display_or_dash(&details.run.selected_slice_id).to_string()
}

fn display_or_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn truncate_display(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push('…');
    truncated
}

fn is_terminal_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed
            | RunStatus::Failed
            | RunStatus::Blocked
            | RunStatus::Cancelled
            | RunStatus::Interrupted
    )
}

fn terminal_run_error(details: &RunDetails) -> Result<()> {
    if details.run.error.trim().is_empty() {
        bail!("run ended with status {}", details.run.status);
    }
    bail!(
        "run ended with status {}: {}",
        details.run.status,
        details.run.error
    )
}

fn stdout_is_terminal() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
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
            | RunStatus::Interrupted => return terminal_run_error(&details),
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
