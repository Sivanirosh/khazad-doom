use crate::artifact;
use crate::daemon::{Client, DaemonHealth, Server};
use crate::domain::{
    BranchHandoff, RunDetails, RunInspection, RunStatus, SliceValidationReport, SliceWriteResult,
    StatusFeed, StatusFeedRole,
};
use crate::ipc::{
    AnswerQuestionParams, AnswerQuestionResult, CancelRunParams, CancelRunResult, HandoffParams,
    InitRepoParams, InitRepoResult, InspectRunParams, ListQuestionsParams, ListQuestionsResult,
    ListSlicesResult, ResumeRunParams, SliceImportGithubParams, SliceNewParams, SlicesParams,
    StartRunParams, StartRunResult, StatusParams,
};
use crate::paths::Paths;
use crate::state::Store as StateStore;
use crate::workflow::cockpit_mode_transport_arg;
use anyhow::{Context, Result, bail};
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
        /// Live cockpit mode for this run: auto, herdr, or direct. Defaults to repo config.
        #[arg(long, value_parser = ["auto", "herdr", "direct"])]
        cockpit: Option<String>,
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
        /// Live cockpit mode for resumed execution: auto, herdr, or direct. Defaults to repo config.
        #[arg(long, value_parser = ["auto", "herdr", "direct"])]
        cockpit: Option<String>,
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
            cockpit,
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
                cockpit,
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
            cockpit,
            parallel,
            wait,
        } => run_resume(
            paths,
            ResumeCliOptions {
                run_id: run,
                agent,
                pi_bin,
                pi_args,
                cockpit,
                parallel,
                wait,
            },
        ),
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
    cockpit: Option<String>,
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
    let mut pi_args = effective_request_args(opts.pi_args, "KHAZAD_PI_ARGS");
    if let Some(cockpit) = &opts.cockpit {
        pi_args.push(cockpit_mode_transport_arg(cockpit)?);
    }
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

struct ResumeCliOptions {
    run_id: String,
    agent: String,
    pi_bin: String,
    pi_args: Vec<String>,
    cockpit: Option<String>,
    parallel: usize,
    wait: bool,
}

fn run_resume(paths: Paths, opts: ResumeCliOptions) -> Result<()> {
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let result: StartRunResult = client.call(
        "resumeRun",
        &ResumeRunParams {
            run_id: opts.run_id,
            agent: effective_request_text(opts.agent, "KHAZAD_AGENT"),
            pi_bin: effective_request_text(opts.pi_bin, "KHAZAD_PI_BIN"),
            pi_args: effective_request_args_with_cockpit(opts.pi_args, opts.cockpit.as_deref())?,
            parallelism: opts.parallel,
        },
    )?;
    if !opts.wait {
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
    let mut out = io::stdout();
    println!("Run: {}", details.run.id);
    println!("Status: {}", details.run.status);
    if let Some(progress) = &details.progress {
        println!("Phase: {}", progress_phase_label(progress));
    }
    match &details.feed {
        Some(feed) => {
            let _ = render_feed_plain(&mut out, feed);
        }
        None => println!("daemon status feed unavailable"),
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
    match &details.feed {
        Some(feed) => render_feed_monitor(out, feed),
        None => {
            monitor_heading(out, "Feed", "unavailable")?;
            monitor_tree(out, "daemon status feed unavailable")
        }
    }
}

fn progress_phase_label(progress: &crate::domain::RunProgress) -> String {
    if progress.phase.trim().is_empty() {
        "unknown".to_string()
    } else {
        progress.phase.clone()
    }
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
    if let Some(reason) = &details.primary_terminal_reason {
        bail!(
            "run ended with status {}: {}",
            details.run.status,
            reason.summary
        );
    }
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

fn effective_request_args_with_cockpit(
    values: Vec<String>,
    cockpit: Option<&str>,
) -> Result<Vec<String>> {
    let mut args = effective_request_args(values, "KHAZAD_PI_ARGS");
    if let Some(cockpit) = cockpit {
        args.push(cockpit_mode_transport_arg(cockpit)?);
    }
    Ok(args)
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
