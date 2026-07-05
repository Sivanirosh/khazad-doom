use crate::artifact;
use crate::daemon::{Client, DaemonHealth, Server};
use crate::domain::{
    BranchHandoff, Event, RunDetails, RunEconomics, RunIncident, RunInspection, RunStatus,
    SliceStatus, SliceValidationReport, SliceWriteResult, StatusFeed, StatusFeedRole,
    WorkerAttemptProgress,
};
use crate::ipc::{
    AnswerQuestionParams, AnswerQuestionResult, CancelRunParams, CancelRunResult, HandoffParams,
    InitRepoParams, InitRepoResult, InspectRunParams, ListQuestionsParams, ListQuestionsResult,
    ListSlicesResult, ResumeRunParams, SliceImportGithubParams, SliceNewParams, SlicesParams,
    StartRunParams, StartRunResult, StatusParams,
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
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
struct RunStartOutput {
    pub run_id: String,
    pub repo_path: String,
    pub monitor_command: String,
    pub run_monitor_command: String,
}

impl RunStartOutput {
    fn new(run_id: String, repo_path: String) -> Self {
        let monitor_command = format!(
            "khazad-doom monitor --repo {} --latest",
            shell_quote_arg(&repo_path)
        );
        let run_monitor_command = format!("khazad-doom monitor --run {}", shell_quote_arg(&run_id));
        Self {
            run_id,
            repo_path,
            monitor_command,
            run_monitor_command,
        }
    }
}

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
        /// Open slice id to run. Repeat to select multiple. Open dependencies are included automatically; closed dependencies are skipped.
        #[arg(long = "slice")]
        slices: Vec<String>,
        /// Run all open slices in dependency order. This is also the default when no --slice is given.
        #[arg(long)]
        all: bool,
        /// Agent adapter to use: pi or fake. Defaults to KHAZAD_AGENT or repo config.
        #[arg(long, default_value = "")]
        agent: String,
        /// Pi binary for worker launches. Defaults to KHAZAD_PI_BIN or pi.
        #[arg(long, default_value = "")]
        pi_bin: String,
        /// Extra Pi launch args. Repeat or pass a quoted string; overrides KHAZAD_PI_ARGS.
        #[arg(long = "pi-args")]
        pi_args: Vec<String>,
        /// Run independent slice workers concurrently, then merge serially.
        #[arg(long, default_value_t = 1)]
        parallel: usize,
        /// Allow starting from a dirty source repo; recorded in preflight artifacts.
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long)]
        wait: bool,
    },
    /// Resume an interrupted/failed/cancelled run from its durable checkpoint.
    Resume {
        #[arg(long)]
        run: String,
        /// Agent adapter to use: pi or fake. Defaults to KHAZAD_AGENT or repo config.
        #[arg(long, default_value = "")]
        agent: String,
        /// Pi binary for worker launches. Defaults to KHAZAD_PI_BIN or pi.
        #[arg(long, default_value = "")]
        pi_bin: String,
        /// Extra Pi launch args. Repeat or pass a quoted string; overrides KHAZAD_PI_ARGS.
        #[arg(long = "pi-args")]
        pi_args: Vec<String>,
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
        #[arg(long, default_value = "")]
        run: String,
        /// Repository path used with --latest. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Inspect the latest run for a repository, including terminal runs.
        #[arg(long)]
        latest: bool,
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
        /// With --latest, fall back to the latest terminal run when no active run exists.
        #[arg(long)]
        include_terminal: bool,
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
    /// List pending/answered worker questions.
    Questions {
        #[arg(long, default_value = "")]
        run: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Answer a worker operator-escalation question.
    Answer {
        run: String,
        question: String,
        answer: String,
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
            pi_bin,
            pi_args,
            parallel,
            allow_dirty,
            wait,
        } => run_start(
            paths,
            RunStartOptions {
                repo,
                slices,
                all,
                agent,
                pi_bin,
                pi_args,
                parallel,
                allow_dirty,
                wait,
            },
        ),
        CommandArgs::Resume {
            run,
            agent,
            pi_bin,
            pi_args,
            parallel,
            wait,
        } => run_resume(paths, run, agent, pi_bin, pi_args, parallel, wait),
        CommandArgs::Cancel { run, reason } => run_cancel(paths, run, reason),
        CommandArgs::Handoff {
            run,
            push,
            create_pr,
            dry_run,
        } => run_handoff(paths, run, push, create_pr, dry_run),
        CommandArgs::Inspect {
            run,
            repo,
            latest,
            log_tail,
        } => run_inspect(paths, run, repo, latest, log_tail),
        CommandArgs::Status {
            run,
            repo,
            latest,
            include_terminal,
            events_limit,
            follow,
            interval_ms,
        } => run_status(
            paths,
            RunStatusOptions {
                run_id: run,
                repo,
                latest,
                include_terminal,
                events_limit,
                follow,
                interval_ms,
            },
        ),
        CommandArgs::Monitor {
            run,
            repo,
            latest,
            once,
            events_limit,
            interval_ms,
        } => run_monitor(paths, run, repo, latest, once, events_limit, interval_ms),
        CommandArgs::Watch { run, interval_ms } => run_watch(paths, run, interval_ms),
        CommandArgs::Questions { run, repo } => run_questions(paths, run, repo),
        CommandArgs::Answer {
            run,
            question,
            answer,
        } => run_answer(paths, run, question, answer),
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

struct RunStartOptions {
    repo: PathBuf,
    slices: Vec<String>,
    all: bool,
    agent: String,
    pi_bin: String,
    pi_args: Vec<String>,
    parallel: usize,
    allow_dirty: bool,
    wait: bool,
}

fn run_start(paths: Paths, opts: RunStartOptions) -> Result<()> {
    let repo = resolve_repo_path(opts.repo)?;
    let config = artifact::Store::new(&repo)
        .read_config()
        .unwrap_or_default();
    let agent = effective_request_text(opts.agent, "KHAZAD_AGENT");
    let pi_bin = effective_request_text(opts.pi_bin, "KHAZAD_PI_BIN");
    let pi_args = effective_request_args(opts.pi_args, "KHAZAD_PI_ARGS");
    let parallel = effective_cli_parallelism(opts.parallel, config.parallelism);
    let repo_path = repo.to_string_lossy().to_string();
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let result: StartRunResult = client.call(
        "startRun",
        &StartRunParams {
            repo_path: repo_path.clone(),
            slice_id: String::new(),
            slice_ids: opts.slices,
            all: opts.all,
            agent,
            pi_bin,
            pi_args,
            parallelism: parallel,
            allow_dirty: opts.allow_dirty,
        },
    )?;
    let output = RunStartOutput::new(result.run_id, repo_path);
    if !opts.wait {
        return print_json(&output);
    }
    wait_run(&client, &output.run_id)
}

fn run_resume(
    paths: Paths,
    run_id: String,
    agent: String,
    pi_bin: String,
    pi_args: Vec<String>,
    parallel: usize,
    wait: bool,
) -> Result<()> {
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let result: StartRunResult = client.call(
        "resumeRun",
        &ResumeRunParams {
            run_id,
            agent: effective_request_text(agent, "KHAZAD_AGENT"),
            pi_bin: effective_request_text(pi_bin, "KHAZAD_PI_BIN"),
            pi_args: effective_request_args(pi_args, "KHAZAD_PI_ARGS"),
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

fn run_inspect(
    paths: Paths,
    run_id: String,
    repo: Option<PathBuf>,
    latest: bool,
    log_tail_lines: usize,
) -> Result<()> {
    if latest && !run_id.is_empty() {
        bail!("inspect --latest cannot be combined with --run <run-id>");
    }
    if !latest && repo.is_some() {
        bail!("inspect --repo requires --latest");
    }
    let client = Client::new(paths);
    let run_id = if latest {
        let repo = resolve_repo_path(repo.unwrap_or_else(|| PathBuf::from(".")))?;
        let repo_path = repo.to_string_lossy().to_string();
        let details = fetch_latest_run(&client, &repo_path, 1, false)?
            .ok_or_else(|| anyhow::anyhow!("no runs found for repo {repo_path}"))?;
        details.run.id
    } else if run_id.is_empty() {
        bail!("inspect requires --run <run-id> or --latest");
    } else {
        run_id
    };
    let inspection: RunInspection = client.call(
        "inspectRun",
        &InspectRunParams {
            run_id,
            log_tail_lines,
        },
    )?;
    print_json(&inspection)
}

struct RunStatusOptions {
    run_id: String,
    repo: Option<PathBuf>,
    latest: bool,
    include_terminal: bool,
    events_limit: usize,
    follow: bool,
    interval_ms: u64,
}

fn run_status(paths: Paths, opts: RunStatusOptions) -> Result<()> {
    if opts.follow {
        if opts.run_id.is_empty() {
            bail!("status --follow requires --run <run-id>");
        }
        if opts.latest {
            bail!("status --follow cannot be combined with --latest");
        }
        return run_watch(paths, opts.run_id, opts.interval_ms);
    }
    let client = Client::new(paths);
    if opts.include_terminal && !opts.latest {
        bail!("status --include-terminal requires --latest");
    }
    if !opts.run_id.is_empty() {
        if opts.latest {
            bail!("status --latest cannot be combined with --run <run-id>");
        }
        let details: RunDetails = client.call(
            "status",
            &StatusParams {
                run_id: opts.run_id,
                events_limit: opts.events_limit,
                ..StatusParams::default()
            },
        )?;
        return print_json(&details);
    }
    if opts.latest {
        let repo = resolve_repo_path(opts.repo.unwrap_or_else(|| PathBuf::from(".")))?;
        let repo_path = repo.to_string_lossy().to_string();
        let active = fetch_latest_run(&client, &repo_path, opts.events_limit, true)?;
        let details = if opts.include_terminal && active.is_none() {
            fetch_latest_run(&client, &repo_path, opts.events_limit, false)?
        } else {
            active
        };
        return print_json(&details);
    }
    if opts.repo.is_some() {
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
            let active = fetch_latest_run(client, &repo_path, events_limit, true)?;
            if let Some(details) = &active {
                attached_run_id = Some(details.run.id.clone());
            }
            if active.is_some() {
                active
            } else {
                fetch_latest_run(client, &repo_path, events_limit, false)?
            }
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

fn fetch_latest_run(
    client: &Client,
    repo_path: &str,
    events_limit: usize,
    active_only: bool,
) -> Result<Option<RunDetails>> {
    client.call(
        "status",
        &StatusParams {
            repo_path: repo_path.to_string(),
            latest: true,
            active_only,
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

fn run_questions(paths: Paths, run_id: String, repo: Option<PathBuf>) -> Result<()> {
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let repo_path = repo
        .map(resolve_repo_path)
        .transpose()?
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    if run_id.trim().is_empty() && repo_path.trim().is_empty() {
        bail!("questions requires --run <run-id> or --repo <path>");
    }
    let result: ListQuestionsResult =
        client.call("listQuestions", &ListQuestionsParams { run_id, repo_path })?;
    print_json(&result)
}

fn run_answer(paths: Paths, run_id: String, question_id: String, answer: String) -> Result<()> {
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let result: AnswerQuestionResult = client.call(
        "answerQuestion",
        &AnswerQuestionParams {
            run_id,
            question_id,
            answer,
        },
    )?;
    print_json(&result)
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
            match client.health_check() {
                DaemonHealth::Running => {
                    println!("running");
                    Ok(())
                }
                DaemonHealth::Missing => bail!("daemon not running"),
                DaemonHealth::Unhealthy(reason) => bail!("daemon unhealthy: {reason}"),
            }
        }
        DaemonCommand::Serve => serve_daemon(paths),
    }
}

fn ensure_daemon(paths: &Paths) -> Result<()> {
    let client = Client::new(paths.clone());
    match client.health_check() {
        DaemonHealth::Running => Ok(()),
        DaemonHealth::Missing => start_daemon(paths, false),
        DaemonHealth::Unhealthy(reason) => bail!("daemon unhealthy: {reason}"),
    }
}

fn start_daemon(paths: &Paths, announce: bool) -> Result<()> {
    let client = Client::new(paths.clone());
    if matches!(client.health_check(), DaemonHealth::Running) {
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
    let mut command = Command::new(exe);
    command
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("start daemon")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(client.health_check(), DaemonHealth::Running) {
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
    if let Some(feed) = &details.feed {
        let mut out = io::stdout();
        println!("Run: {}", details.run.id);
        println!("Status: {}", details.run.status);
        if let Some(progress) = &details.progress {
            println!("Phase: {}", progress_phase_label(progress));
        }
        let _ = render_feed_plain(&mut out, feed);
        println!();
        return;
    }
    println!("Run: {}", details.run.id);
    println!("Status: {}", details.run.status);
    if let Some(progress) = &details.progress {
        let elapsed = Utc::now()
            .signed_duration_since(progress.phase_started_at)
            .to_std()
            .unwrap_or_default();
        println!("Phase: {}", progress_phase_label(progress));
        if !progress.slice_id.is_empty() {
            println!("Slice: {}", progress.slice_id);
        }
        if !progress.parallel_slices.is_empty() {
            println!("Parallel layer: {}", progress.parallel_slices.join(", "));
        }
        if !progress.command.is_empty() {
            println!("Command: {}", progress.command);
        }
        println!("Elapsed: {}", format_duration(elapsed));
        println!("Updated: {}", progress.updated_at);
        println!("Message: {}", progress.message);
        if let Some(worker) = &progress.worker {
            let mut out = io::stdout();
            let _ = render_worker_attempt(&mut out, worker);
        }
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
    if let Some(economics) = &details.economics {
        print_economics(economics);
    }
    if !details.incidents.is_empty() {
        println!("Incidents: {}", details.incidents.len());
        for incident in details.incidents.iter().rev().take(3).rev() {
            println!("  - {}: {}", incident.kind, incident.message);
        }
    }
    println!();
}

fn render_feed_plain(out: &mut impl Write, feed: &StatusFeed) -> Result<()> {
    writeln!(out, "{}", feed.summary_line)?;
    for block in &feed.blocks {
        if !block.meta.trim().is_empty() {
            writeln!(out, "{} {}", block.label, block.meta)?;
        } else {
            writeln!(out, "{}", block.label)?;
        }
        for line in &block.lines {
            writeln!(out, "  - {}", line.text)?;
        }
    }
    Ok(())
}

fn render_feed_monitor(out: &mut impl Write, feed: &StatusFeed) -> Result<()> {
    for (index, block) in feed.blocks.iter().enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        monitor_heading(out, &block.label, &block.meta)?;
        if block.lines.is_empty() {
            monitor_tree_dim(out, "-")?;
            continue;
        }
        for line in &block.lines {
            match line.role {
                StatusFeedRole::Dim => monitor_tree_dim(out, &line.text)?,
                _ => monitor_tree(out, &line.text)?,
            }
        }
    }
    Ok(())
}

const MONITOR_ACTIVITY_LIMIT: usize = 7;
const MONITOR_OUTPUT_LINES: usize = 4;
const MONITOR_TODO_ITEMS: usize = 8;
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
    writeln!(out)?;
    match details {
        Some(details) => render_run_monitor(&mut out, details)?,
        None => render_waiting_monitor(&mut out, waiting_repo.unwrap_or("-"))?,
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn render_waiting_monitor(out: &mut impl Write, repo: &str) -> Result<()> {
    monitor_heading(out, "Run", "waiting")?;
    monitor_tree(out, &format!("repo {repo}"))?;
    monitor_tree_dim(out, "waiting for the latest active daemon-owned run")?;
    writeln!(out)?;
    monitor_heading(out, "Hint", "")?;
    monitor_tree_dim(
        out,
        "start a run normally; this dashboard will attach when status --latest returns one",
    )?;
    Ok(())
}

fn render_run_monitor(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    if let Some(feed) = &details.feed {
        return render_feed_monitor(out, feed);
    }
    render_todos(out, details)?;
    writeln!(out)?;
    render_run_summary(out, details)?;
    render_current_progress(out, details)?;
    if let Some(economics) = &details.economics {
        writeln!(out)?;
        render_economics(out, economics)?;
    }
    render_incidents(out, &details.incidents)?;
    render_activity(out, details)?;
    render_tail(out, details)?;
    render_monitor_footer(out, details)?;
    Ok(())
}

fn render_todos(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    let items = selected_slice_items(details);
    let item_label = if items.len() == 1 { "item" } else { "items" };
    monitor_heading(out, "Todos", &format!("({} {item_label})", items.len()))?;
    if items.is_empty() {
        monitor_tree_dim(out, "no selected slices recorded")?;
        return Ok(());
    }
    for slice in items.iter().take(MONITOR_TODO_ITEMS) {
        writeln!(out, "{}", todo_line(slice))?;
    }
    if items.len() > MONITOR_TODO_ITEMS {
        monitor_tree_dim(out, &format!("… {} more", items.len() - MONITOR_TODO_ITEMS))?;
    }
    Ok(())
}

fn selected_slice_items(details: &RunDetails) -> Vec<crate::domain::SliceRun> {
    if !details.slice_runs.is_empty() {
        return details.slice_runs.clone();
    }
    details
        .run
        .selected_slice_id
        .split(',')
        .map(str::trim)
        .filter(|slice_id| !slice_id.is_empty())
        .map(|slice_id| crate::domain::SliceRun {
            run_id: details.run.id.clone(),
            slice_id: slice_id.to_string(),
            status: SliceStatus::Pending,
            branch: String::new(),
            commit_sha: String::new(),
            attempts: 0,
            last_error: String::new(),
        })
        .collect()
}

fn todo_line(slice: &crate::domain::SliceRun) -> String {
    let mut meta = Vec::new();
    meta.push(slice.status.to_string());
    if slice.attempts > 0 {
        meta.push(format!(
            "{} {}",
            slice.attempts,
            if slice.attempts == 1 {
                "attempt"
            } else {
                "attempts"
            }
        ));
    }
    if !slice.commit_sha.trim().is_empty() {
        meta.push(short_sha(&slice.commit_sha));
    }
    format!(
        "{} {}{}",
        slice_checkbox(slice.status),
        slice.slice_id,
        if meta.is_empty() {
            String::new()
        } else {
            format!("  {}", meta.join(" • "))
        }
    )
}

fn render_run_summary(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    let progress = details.progress.as_ref();
    let phase = progress
        .map(progress_phase_label)
        .filter(|phase| !phase.trim().is_empty())
        .unwrap_or_else(|| {
            if is_terminal_status(details.run.status) {
                details.run.status.as_str().to_string()
            } else {
                "unknown".to_string()
            }
        });
    let elapsed_start = progress
        .map(|progress| progress.phase_started_at)
        .unwrap_or(details.run.started_at);
    monitor_heading(
        out,
        "Run",
        &format!(
            "{} {} • {}",
            status_icon(details.run.status),
            details.run.status,
            short_run_id(&details.run.id)
        ),
    )?;
    monitor_tree(
        out,
        &format!("phase {phase} • elapsed {}", since_time(elapsed_start)),
    )?;
    monitor_tree_dim(out, &format!("repo {}", short_path(&details.run.repo_path)))?;
    let message = monitor_message(details);
    if !message.trim().is_empty() {
        monitor_tree(out, &truncate_display(&message, MONITOR_LINE_WIDTH))?;
    }
    Ok(())
}

fn render_current_progress(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    let Some(progress) = details.progress.as_ref() else {
        return Ok(());
    };
    if is_terminal_status(details.run.status) && is_terminal_phase(&progress.phase) {
        return Ok(());
    }
    writeln!(out)?;
    if let Some(worker) = &progress.worker {
        render_worker_progress_block(out, details, progress, worker)?;
    } else if !progress.command.trim().is_empty() {
        render_command_progress_block(out, details, progress)?;
    } else {
        render_generic_progress_block(out, details, progress)?;
    }
    if let Some(worker) = &progress.worker
        && let Some(warning) = worker_quiet_warning(worker)
    {
        writeln!(out)?;
        monitor_heading(out, "Warn", "")?;
        monitor_tree(out, &warning)?;
        monitor_tree_dim(out, "wait, inspect, or cancel explicitly")?;
    }
    Ok(())
}

fn render_worker_progress_block(
    out: &mut impl Write,
    details: &RunDetails,
    progress: &crate::domain::RunProgress,
    worker: &WorkerAttemptProgress,
) -> Result<()> {
    let mut meta = Vec::new();
    let slice = monitor_slice_label(details);
    if slice != "-" {
        meta.push(slice);
    }
    if progress.attempt > 0 {
        meta.push(format!("attempt {}", progress.attempt));
    }
    meta.push("now".to_string());
    monitor_heading(out, "Worker", &format!("({})", meta.join(" • ")))?;
    if progress.parallel_layer && !progress.parallel_slices.is_empty() {
        monitor_tree(
            out,
            &format!("Parallel layer: {}", progress.parallel_slices.join(", ")),
        )?;
    }
    monitor_tree(out, &format!("Supervisor: {}", supervisor_label(worker)))?;
    monitor_tree(out, &format!("Process: {}", worker_process_label(worker)))?;
    monitor_tree(
        out,
        &format!("Runtime: {}", since_time(worker.attempt_started_at)),
    )?;
    monitor_tree(
        out,
        &format!("Last worker event: {}", last_worker_event_label(worker)),
    )?;
    monitor_tree(
        out,
        &format!(
            "Last semantic progress: {}",
            worker
                .last_semantic_progress_at
                .map(since_time)
                .unwrap_or_else(|| "unknown".to_string())
        ),
    )?;
    monitor_tree(out, &format!("Timeout: {}", timeout_label(worker)))?;
    Ok(())
}

fn render_command_progress_block(
    out: &mut impl Write,
    details: &RunDetails,
    progress: &crate::domain::RunProgress,
) -> Result<()> {
    let label = command_block_label(&progress.phase, &progress.command);
    let mut meta = Vec::new();
    if label == "Worker" && !progress.slice_id.trim().is_empty() {
        meta.push(progress.slice_id.clone());
    }
    if label == "Worker" && progress.attempt > 0 {
        meta.push(format!("attempt {}", progress.attempt));
    }
    if label != "Worker" {
        meta.push(command_meta(&progress.command));
    }
    meta.push("now".to_string());
    monitor_heading(out, label, &format!("({})", meta.join(" • ")))?;
    if label != "Worker" || progress.command.trim() != "pi" {
        monitor_tree_dim(
            out,
            &truncate_display(&progress.command, MONITOR_LINE_WIDTH),
        )?;
    }
    render_progress_scope(out, details, progress)?;
    if !progress.message.trim().is_empty() {
        monitor_tree(
            out,
            &truncate_display(&progress.message, MONITOR_LINE_WIDTH),
        )?;
    }
    monitor_tree_dim(
        out,
        &format!("updated {} ago", since_time(progress.updated_at)),
    )?;
    Ok(())
}

fn render_generic_progress_block(
    out: &mut impl Write,
    details: &RunDetails,
    progress: &crate::domain::RunProgress,
) -> Result<()> {
    monitor_heading(out, phase_label(&progress.phase), "(now)")?;
    render_progress_scope(out, details, progress)?;
    if !progress.message.trim().is_empty() {
        monitor_tree(
            out,
            &truncate_display(&progress.message, MONITOR_LINE_WIDTH),
        )?;
    }
    monitor_tree_dim(
        out,
        &format!("updated {} ago", since_time(progress.updated_at)),
    )?;
    Ok(())
}

fn render_progress_scope(
    out: &mut impl Write,
    details: &RunDetails,
    progress: &crate::domain::RunProgress,
) -> Result<()> {
    if progress.parallel_layer && !progress.parallel_slices.is_empty() {
        monitor_tree(
            out,
            &format!("Parallel layer: {}", progress.parallel_slices.join(", ")),
        )?;
    } else if !progress.slice_id.trim().is_empty() {
        monitor_tree(out, &format!("slice {}", progress.slice_id))?;
    } else {
        monitor_tree(out, &monitor_slice_label(details))?;
    }
    if progress.phase_started_at != progress.updated_at {
        monitor_tree_dim(
            out,
            &format!("elapsed {}", since_time(progress.phase_started_at)),
        )?;
    }
    Ok(())
}

fn render_incidents(out: &mut impl Write, incidents: &[RunIncident]) -> Result<()> {
    if incidents.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    monitor_heading(out, "Incidents", &format!("({})", incidents.len()))?;
    for incident in incidents.iter().rev().take(8).rev() {
        monitor_tree(
            out,
            &format!(
                "{}: {}",
                incident.kind,
                truncate_display(&incident.message, MONITOR_LINE_WIDTH)
            ),
        )?;
    }
    Ok(())
}

fn render_activity(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    let lines = details
        .events
        .iter()
        .filter_map(|event| activity_line(event, details))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Ok(());
    }
    let visible = lines
        .iter()
        .rev()
        .take(MONITOR_ACTIVITY_LIMIT)
        .collect::<Vec<_>>();
    writeln!(out)?;
    monitor_heading(out, "Activity", &format!("({} recent)", visible.len()))?;
    for line in visible.into_iter().rev() {
        monitor_tree(out, line)?;
    }
    Ok(())
}

fn render_tail(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    let output_tail = details
        .progress
        .as_ref()
        .map(|progress| progress.output_tail.as_str())
        .unwrap_or_default();
    if output_tail.trim().is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    monitor_heading(out, "Tail", "")?;
    render_output_tail(out, output_tail)?;
    Ok(())
}

fn render_monitor_footer(out: &mut impl Write, details: &RunDetails) -> Result<()> {
    writeln!(out)?;
    monitor_tree_dim(
        out,
        &format!(
            "Ctrl-C detach • updated {} • run {}",
            details.run.updated_at.format("%H:%M:%S"),
            short_run_id(&details.run.id)
        ),
    )?;
    Ok(())
}

fn progress_phase_label(progress: &crate::domain::RunProgress) -> String {
    if progress.parallel_layer && progress.phase != "parallel_worker_layer" {
        format!("parallel_worker_layer ({})", progress.phase)
    } else {
        progress.phase.clone()
    }
}

fn print_economics(economics: &RunEconomics) {
    let mut stdout = io::stdout().lock();
    let _ = write_economics(&mut stdout, economics, false);
}

fn render_economics(out: &mut impl Write, economics: &RunEconomics) -> Result<()> {
    monitor_heading(out, "Economics", "")?;
    write_economics(out, economics, true)
}

fn write_economics(out: &mut impl Write, economics: &RunEconomics, tree: bool) -> Result<()> {
    let lines = [
        format!(
            "Agent calls: {} | Commands: {} | Duplicates: {} | Cache: {}/{} hit/miss",
            economics.agent_call_count,
            economics.command_execution_count,
            economics.duplicate_command_count,
            economics.cache_hits,
            economics.cache_misses
        ),
        format!(
            "Repair: policy={} attempts={}/{} | Fail-fast: {}",
            economics.repair_policy,
            economics.repair_attempts,
            economics.repair_max_attempts,
            economics.gate_fail_fast
        ),
    ];
    for line in lines {
        if tree {
            monitor_tree(out, &line)?;
        } else {
            writeln!(out, "{line}")?;
        }
    }
    if !economics.sla_violations.is_empty() {
        let line = format!("SLA violations: {}", economics.sla_violations.join("; "));
        if tree {
            monitor_tree(out, &line)?;
        } else {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

fn render_worker_attempt(out: &mut impl Write, worker: &WorkerAttemptProgress) -> Result<()> {
    writeln!(out, "Supervisor: {}", supervisor_label(worker))?;
    writeln!(out, "Worker process: {}", worker_process_label(worker))?;
    writeln!(
        out,
        "Worker runtime: {}",
        since_time(worker.attempt_started_at)
    )?;
    writeln!(
        out,
        "Last worker event: {}",
        last_worker_event_label(worker)
    )?;
    writeln!(
        out,
        "Last semantic progress: {}",
        worker
            .last_semantic_progress_at
            .map(since_time)
            .unwrap_or_else(|| "unknown".to_string())
    )?;
    writeln!(out, "Timeout: {}", timeout_label(worker))?;
    if let Some(warning) = worker_quiet_warning(worker) {
        writeln!(out, "Warning: {warning}")?;
        writeln!(out, "Hint: wait, inspect, or cancel")?;
    }
    Ok(())
}

fn supervisor_label(worker: &WorkerAttemptProgress) -> String {
    match worker.process_observed_at {
        Some(observed_at) => format!("alive, observed child {} ago", since_time(observed_at)),
        None => "starting, no child observation yet".to_string(),
    }
}

fn worker_process_label(worker: &WorkerAttemptProgress) -> String {
    match worker.pid {
        Some(pid) => format!("running pid={pid}"),
        None => "running".to_string(),
    }
}

fn last_worker_event_label(worker: &WorkerAttemptProgress) -> String {
    match worker.last_event_at {
        Some(last_event_at) if worker.last_event_kind.trim().is_empty() => {
            format!("{} ago", since_time(last_event_at))
        }
        Some(last_event_at) => format!(
            "{} ago ({})",
            since_time(last_event_at),
            worker.last_event_kind
        ),
        None => "none".to_string(),
    }
}

fn timeout_label(worker: &WorkerAttemptProgress) -> String {
    if worker.attempt_timeout_seconds == 0 {
        return "disabled".to_string();
    }
    let elapsed = Utc::now()
        .signed_duration_since(worker.attempt_started_at)
        .to_std()
        .unwrap_or_default();
    let timeout = Duration::from_secs(worker.attempt_timeout_seconds);
    if elapsed >= timeout {
        return format!(
            "{}s, exceeded by {}",
            worker.attempt_timeout_seconds,
            format_duration(elapsed.saturating_sub(timeout))
        );
    }
    format!(
        "{}s, remaining {}",
        worker.attempt_timeout_seconds,
        format_duration(timeout.saturating_sub(elapsed))
    )
}

fn worker_quiet_warning(worker: &WorkerAttemptProgress) -> Option<String> {
    if worker.no_output_warning_seconds == 0 {
        return None;
    }
    let reference = worker.last_event_at.unwrap_or(worker.attempt_started_at);
    let quiet_for = Utc::now()
        .signed_duration_since(reference)
        .to_std()
        .unwrap_or_default();
    if quiet_for < Duration::from_secs(worker.no_output_warning_seconds) {
        return None;
    }
    let timeout_suffix = if worker.attempt_timeout_seconds == 0 {
        "; no timeout configured"
    } else {
        ""
    };
    Some(format!(
        "worker is quiet for {}; this may be normal{}",
        format_duration(quiet_for),
        timeout_suffix
    ))
}

fn since_time(time: chrono::DateTime<Utc>) -> String {
    let duration = Utc::now()
        .signed_duration_since(time)
        .to_std()
        .unwrap_or_default();
    format_duration(duration)
}

fn render_output_tail(out: &mut impl Write, output_tail: &str) -> Result<()> {
    let trimmed = output_tail.trim_end();
    if trimmed.is_empty() {
        monitor_tree_dim(out, "-")?;
        return Ok(());
    }
    let lines = trimmed
        .lines()
        .rev()
        .take(MONITOR_OUTPUT_LINES)
        .collect::<Vec<_>>();
    for line in lines.into_iter().rev() {
        monitor_tree_dim(out, &truncate_display(line, MONITOR_LINE_WIDTH))?;
    }
    Ok(())
}

fn activity_line(event: &Event, details: &RunDetails) -> Option<String> {
    let payload = event.payload.as_object();
    match event.typ.as_str() {
        "run_started" => {
            let selected = payload
                .and_then(|payload| payload.get("selected_slices"))
                .and_then(serde_json::Value::as_array)
                .map(|items| items.len())
                .unwrap_or_else(|| selected_slice_items(details).len());
            Some(format!(
                "Run (started): {selected} selected {}",
                if selected == 1 { "slice" } else { "slices" }
            ))
        }
        "slice_started" => Some(format!(
            "Worker ({}): slice worker started",
            payload_text(payload, "slice_id").unwrap_or_else(|| "-".to_string())
        )),
        "slice_merged" => {
            let slice_id = payload_text(payload, "slice_id").unwrap_or_else(|| "slice".to_string());
            let sha = payload_text(payload, "commit_sha")
                .filter(|sha| !sha.trim().is_empty())
                .map(|sha| format!(" • {}", short_sha(&sha)))
                .unwrap_or_default();
            Some(format!("Todos ({slice_id}): ☒ {slice_id}  merged{sha}"))
        }
        "integration_repair_completed" => {
            let status = payload_text(payload, "status").unwrap_or_else(|| "-".to_string());
            let summary = payload_text(payload, "summary")
                .unwrap_or_else(|| "integration repair completed".to_string());
            Some(format!("Repair ({status}): {summary}"))
        }
        "implementation_summary" => implementation_summary_line(payload),
        "run_completed" => Some("Run (completed): handoff artifacts are ready".to_string()),
        "worktrees_cleaned" => Some("Cleanup: worker worktrees cleaned".to_string()),
        "checkpoint_written" => checkpoint_line(payload),
        "progress" => progress_activity_line(event, payload),
        _ => {
            let summary = event_summary(event);
            if summary.is_empty() {
                Some(event_label(&event.typ))
            } else {
                Some(format!("{}: {summary}", event_label(&event.typ)))
            }
        }
    }
}

fn implementation_summary_line(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let mut parts = Vec::new();
    if let Some(completed) = payload
        .get("completed_slices")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
    {
        parts.push(format!(
            "{completed} completed {}",
            if completed == 1 { "slice" } else { "slices" }
        ));
    }
    if let Some(gate) = payload
        .get("integration_gate")
        .and_then(serde_json::Value::as_object)
    {
        if let Some(summary) = gate.get("summary").and_then(serde_json::Value::as_str) {
            if !summary.trim().is_empty() {
                parts.push(summary.to_string());
            }
        } else if let Some(status) = gate.get("status").and_then(serde_json::Value::as_str) {
            parts.push(format!("integration gate {status}"));
        }
    }
    if let Some(final_sha) = payload.get("final_sha").and_then(serde_json::Value::as_str)
        && !final_sha.trim().is_empty()
    {
        parts.push(format!("final {}", short_sha(final_sha)));
    }
    (!parts.is_empty()).then(|| format!("Summary: {}", parts.join(" • ")))
}

fn checkpoint_line(payload: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let payload = payload?;
    let completed = payload
        .get("completed_slices")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let remaining = payload
        .get("remaining_slices")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    Some(format!(
        "State: checkpoint written • {completed} done • {remaining} remaining"
    ))
}

fn progress_activity_line(
    event: &Event,
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let payload = payload?;
    let phase = payload_text(Some(payload), "phase").unwrap_or_else(|| "activity".to_string());
    if phase == "completed" {
        return None;
    }
    let label = if let Some(command) = payload_text(Some(payload), "command") {
        command_block_label(&phase, &command).to_string()
    } else {
        phase_label(&phase).to_string()
    };
    let mut meta = Vec::new();
    if let Some(slice_id) = payload_text(Some(payload), "slice_id")
        && !slice_id.trim().is_empty()
    {
        meta.push(slice_id);
    }
    if let Some(attempt) = payload.get("attempt").and_then(serde_json::Value::as_u64)
        && attempt > 0
    {
        meta.push(format!("attempt {attempt}"));
    }
    if label != "Worker"
        && let Some(command) = payload_text(Some(payload), "command")
    {
        meta.push(command_meta(&command));
    }
    let message = payload_text(Some(payload), "message")
        .unwrap_or_else(|| event_summary(event))
        .trim()
        .to_string();
    let summary = if message.is_empty() {
        phase.replace('_', " ")
    } else {
        message
    };
    Some(format!(
        "{}{}: {}",
        label,
        if meta.is_empty() {
            String::new()
        } else {
            format!(" ({})", meta.join(" • "))
        },
        truncate_display(&summary, MONITOR_LINE_WIDTH)
    ))
}

fn payload_text(
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    payload
        .and_then(|payload| payload.get(key))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| (!value.is_null()).then(|| value.to_string()))
        })
        .filter(|value| !value.trim().is_empty())
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
        && progress.parallel_layer
        && !progress.parallel_slices.is_empty()
    {
        return format!("parallel layer: {}", progress.parallel_slices.join(", "));
    }
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

fn monitor_heading(out: &mut impl Write, label: &str, meta: &str) -> Result<()> {
    if meta.trim().is_empty() {
        writeln!(out, "{label}")?;
    } else {
        writeln!(out, "{label} {meta}")?;
    }
    Ok(())
}

fn monitor_tree(out: &mut impl Write, text: &str) -> Result<()> {
    writeln!(out, "└ {}", truncate_display(text, MONITOR_LINE_WIDTH))?;
    Ok(())
}

fn monitor_tree_dim(out: &mut impl Write, text: &str) -> Result<()> {
    monitor_tree(out, text)
}

fn status_icon(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "✓",
        RunStatus::Running => "●",
        RunStatus::Blocked => "!",
        RunStatus::Failed => "✗",
        RunStatus::Cancelled | RunStatus::Interrupted => "×",
        RunStatus::Pending => "○",
    }
}

fn slice_checkbox(status: SliceStatus) -> &'static str {
    match status {
        SliceStatus::Merged => "☒",
        SliceStatus::Running | SliceStatus::ReadyToMerge | SliceStatus::RepairNeeded => "◐",
        SliceStatus::Failed
        | SliceStatus::Blocked
        | SliceStatus::Cancelled
        | SliceStatus::Interrupted => "✗",
        SliceStatus::Pending => "☐",
    }
}

fn short_sha(value: &str) -> String {
    value.chars().take(8).collect()
}

fn short_run_id(value: &str) -> String {
    if value.chars().count() <= 30 {
        return display_or_dash(value).to_string();
    }
    let prefix = value.chars().take(11).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn short_path(value: &str) -> String {
    let text = value.trim();
    if text.is_empty() {
        return "-".to_string();
    }
    let parts = text
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() <= 2 {
        return text.to_string();
    }
    format!("…/{}", parts[parts.len().saturating_sub(2)..].join("/"))
}

fn phase_label(phase: &str) -> &'static str {
    let normalized = phase.to_ascii_lowercase();
    if normalized.starts_with("worker") {
        if normalized == "worker_verify" {
            "Shell"
        } else {
            "Worker"
        }
    } else if normalized.contains("gate") || normalized.contains("setup") {
        "Shell"
    } else if normalized.contains("merge") {
        "Merge"
    } else if normalized.contains("repair") {
        "Repair"
    } else if normalized == "ready_to_merge" {
        "Todos"
    } else if matches!(
        normalized.as_str(),
        "completed" | "started" | "integration_setup"
    ) {
        "Run"
    } else {
        "Activity"
    }
}

fn command_block_label(phase: &str, command: &str) -> &'static str {
    let normalized = phase.to_ascii_lowercase();
    let text = command.to_ascii_lowercase();
    if normalized == "worker_running" || text == "pi" {
        "Worker"
    } else if normalized.contains("merge") || text.starts_with("git merge") {
        "Merge"
    } else if normalized.contains("repair") {
        "Repair"
    } else {
        "Shell"
    }
}

fn command_meta(command: &str) -> String {
    let mut text = command.trim().to_string();
    while let Some((prefix, rest)) = text.split_once(' ') {
        if is_env_assignment(prefix) {
            text = rest.trim_start().to_string();
        } else {
            break;
        }
    }
    truncate_display(if text.is_empty() { command } else { &text }, 34)
}

fn is_env_assignment(value: &str) -> bool {
    let Some((key, _value)) = value.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn event_label(value: &str) -> String {
    value
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_terminal_phase(phase: &str) -> bool {
    matches!(
        phase,
        "completed" | "failed" | "blocked" | "cancelled" | "interrupted"
    )
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

fn shell_quote_arg(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        matches!(
            byte,
            b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'/'
                | b'.'
                | b'_'
                | b'-'
                | b':'
                | b','
                | b'='
        )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn effective_request_text(value: String, env_key: &str) -> String {
    if !value.trim().is_empty() {
        value
    } else {
        std::env::var(env_key).unwrap_or_default()
    }
}

fn effective_request_args(values: Vec<String>, env_key: &str) -> Vec<String> {
    if !values.is_empty() {
        return split_arg_values(values);
    }
    split_arg_values(vec![std::env::var(env_key).unwrap_or_default()])
}

fn split_arg_values(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .flat_map(|value| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|value| !value.trim().is_empty())
        .collect()
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
    let canonical = repo
        .canonicalize()
        .with_context(|| format!("resolve repository path {}", repo.display()))?;
    crate::gitutil::repo_root(&canonical)
        .with_context(|| format!("resolve git repository root for {}", canonical.display()))
}
