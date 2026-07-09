use crate::artifact;
use crate::daemon::{Client, DaemonHealth, Server};
use crate::domain::{
    AutonomyLevel, BranchHandoff, MissionEnvelope, ReplanEvidenceLink, ReplanProposalSource,
    ReplanProposalState, ReplanProposedChange, RunDetails, RunInspection, RunStatus,
    SliceValidationReport, SliceWriteResult, StatusFeed, StatusFeedRole,
};
use crate::ipc::{
    AnswerQuestionParams, AnswerQuestionResult, CancelRunParams, CancelRunResult,
    CreateReplanProposalParams, CreateReplanProposalResult, DecideReplanProposalParams,
    DecideReplanProposalResult, HandoffParams, InitRepoParams, InitRepoResult, InspectRunParams,
    ListQuestionsParams, ListQuestionsResult, ListReplanProposalsParams, ListReplanProposalsResult,
    ListSlicesResult, ResumeRunParams, SliceImportGithubParams, SliceNewParams, SlicesParams,
    StartRunParams, StartRunResult, StatusParams,
};
use crate::paths::Paths;
use crate::pi_contract::PiActivityFormatter;
use crate::state::Store as StateStore;
use crate::workflow::{
    CockpitOpenFocus, cockpit_mode_transport_arg, cockpit_workspace_label_for_run,
    open_default_run_cockpit_for_operator, project_gate_pane,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Serialize)]
struct CockpitOpenOutput {
    pub run_id: String,
    pub repo_path: String,
    pub workspace_label: String,
    pub adapter: String,
    pub opened: bool,
    pub action: String,
    pub pane_labels: Vec<String>,
    pub fallback: String,
    pub remediation: String,
    pub message: String,
    pub operator_commands: Vec<String>,
}

#[derive(Debug, Clone)]
struct WorkerActivityPainterOptions {
    stdout_path: PathBuf,
    status_path: PathBuf,
    exit_path: PathBuf,
    poll_interval: Duration,
    startup_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct WorkerActivityWrapperStatus {
    state: String,
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
        /// Deprecated compatibility flag; native Herdr-hosted Pi TUI workers are now the default.
        #[arg(long = "experimental-pi-tui-worker")]
        deprecated_experimental_pi_tui_worker: bool,
        /// Force the legacy JSON wrapper worker even when Herdr cockpit is available.
        #[arg(long = "json-wrapper-worker")]
        json_wrapper_worker: bool,
        /// Run independent slice workers concurrently, then merge serially.
        #[arg(long, default_value_t = 1)]
        parallel: usize,
        /// Allow starting from a dirty source repo; recorded in preflight artifacts.
        #[arg(long)]
        allow_dirty: bool,
        /// Optional opaque Herdr/Pi target for inert terminal-run feedback.
        #[arg(long = "origin-notification-target", default_value = "")]
        origin_notification_target: String,
        /// Record a per-run mission envelope JSON file. AF-02 records this only; it grants no authority.
        #[arg(long = "envelope")]
        envelope: Option<PathBuf>,
        /// Override the envelope autonomy level: off, shadow, promote, or run. Levels above off are recorded only in AF-02.
        #[arg(long = "autonomy", value_parser = ["off", "shadow", "promote", "run"])]
        autonomy: Option<String>,
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
        /// Deprecated compatibility flag; native Herdr-hosted Pi TUI workers are now the default.
        #[arg(long = "experimental-pi-tui-worker")]
        deprecated_experimental_pi_tui_worker: bool,
        /// Force the legacy JSON wrapper worker even when Herdr cockpit is available.
        #[arg(long = "json-wrapper-worker")]
        json_wrapper_worker: bool,
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
    /// Interactive operator attention surface over daemon-owned commands.
    Attend {
        /// Specific run id to attend.
        #[arg(long, default_value = "")]
        run: String,
        /// Repository path used with --latest. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Attach to the latest run for the repository.
        #[arg(long)]
        latest: bool,
        /// Poll interval in milliseconds after empty input.
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
    /// Open or focus optional cockpit surfaces for daemon-owned runs.
    Cockpit {
        #[command(subcommand)]
        command: CockpitCommand,
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
    /// Record and decide daemon-owned replan proposals.
    Replan {
        #[command(subcommand)]
        command: ReplanCommand,
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
enum CockpitCommand {
    /// Open or focus the Herdr workspace for a run without changing daemon workflow state.
    Open {
        /// Specific run id to open/focus.
        #[arg(long, default_value = "")]
        run: String,
        /// Repository path used with --latest. Defaults to the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Open/focus the latest daemon-owned run for the repository, including terminal runs.
        #[arg(long)]
        latest: bool,
    },
    /// Internal read-only Herdr worker-pane painter over daemon-owned wrapper artifacts.
    #[command(name = "paint-worker-activity", hide = true)]
    PaintWorkerActivity {
        #[arg(long)]
        stdout: PathBuf,
        #[arg(long)]
        status: PathBuf,
        #[arg(long)]
        exit: PathBuf,
        #[arg(long, default_value_t = 250)]
        interval_ms: u64,
    },
    /// Internal read-only Herdr gate/repair-pane painter over daemon status feed data.
    #[command(name = "paint-gate-activity", hide = true)]
    PaintGateActivity {
        #[arg(long)]
        run: String,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
}

#[derive(Debug, Subcommand)]
enum ReplanCommand {
    /// List replan proposals for a run.
    List { run: String },
    /// Create a durable pending replan proposal without applying it.
    Propose {
        run: String,
        #[arg(long, default_value = "")]
        id: String,
        #[arg(long, default_value = "operator")]
        source_kind: String,
        #[arg(long, default_value = "")]
        source_slice: String,
        #[arg(long, default_value = "")]
        source_phase: String,
        #[arg(long, default_value_t = 0)]
        source_attempt: usize,
        #[arg(long, default_value = "")]
        source_summary: String,
        #[arg(long = "finding")]
        findings: Vec<String>,
        /// Evidence as kind:path[:summary]. Repeat for multiple links.
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        /// Proposed change as kind:target:summary. Repeat for multiple changes.
        #[arg(long = "change")]
        changes: Vec<String>,
        #[arg(long, default_value = "operator_review")]
        risk: String,
    },
    /// Mark a pending proposal accepted. V1 records applied=false and does not mutate the queue.
    Accept {
        run: String,
        proposal: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "")]
        authorizer: String,
    },
    /// Mark a pending proposal rejected.
    Reject {
        run: String,
        proposal: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "")]
        authorizer: String,
    },
    /// Mark a pending proposal deferred with a revisit condition.
    Defer {
        run: String,
        proposal: String,
        #[arg(long = "until")]
        revisit_condition: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "")]
        authorizer: String,
    },
    /// Mark a pending proposal superseded by another proposal id.
    Supersede {
        run: String,
        proposal: String,
        replacement: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "")]
        authorizer: String,
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
            deprecated_experimental_pi_tui_worker,
            json_wrapper_worker,
            parallel,
            allow_dirty,
            origin_notification_target,
            envelope,
            autonomy,
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
                deprecated_experimental_pi_tui_worker,
                json_wrapper_worker,
                parallel,
                allow_dirty,
                origin_notification_target,
                envelope,
                autonomy,
                wait,
            },
        ),
        CommandArgs::Resume {
            run,
            agent,
            pi_bin,
            pi_args,
            cockpit,
            deprecated_experimental_pi_tui_worker,
            json_wrapper_worker,
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
                deprecated_experimental_pi_tui_worker,
                json_wrapper_worker,
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
        CommandArgs::Attend {
            run,
            repo,
            latest,
            interval_ms,
        } => run_attend(paths, run, repo, latest, interval_ms),
        CommandArgs::Watch { run, interval_ms } => run_watch(paths, run, interval_ms),
        CommandArgs::Cockpit { command } => run_cockpit(paths, command),
        CommandArgs::Questions { run, repo } => run_questions(paths, run, repo),
        CommandArgs::Answer {
            run,
            question,
            answer,
        } => run_answer(paths, run, question, answer),
        CommandArgs::Replan { command } => run_replan(paths, command),
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
    deprecated_experimental_pi_tui_worker: bool,
    json_wrapper_worker: bool,
    parallel: usize,
    allow_dirty: bool,
    origin_notification_target: String,
    envelope: Option<PathBuf>,
    autonomy: Option<String>,
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
    let mission_envelope =
        read_mission_envelope(opts.envelope.as_deref(), opts.autonomy.as_deref())?;
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
            native_pi_tui_worker: native_pi_tui_worker_requested(
                opts.deprecated_experimental_pi_tui_worker,
                opts.json_wrapper_worker,
            ),
            parallelism: parallel,
            allow_dirty: opts.allow_dirty,
            origin_notification_target: effective_request_text(
                opts.origin_notification_target,
                "KHAZAD_ORIGIN_NOTIFICATION_TARGET",
            ),
            mission_envelope,
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
    deprecated_experimental_pi_tui_worker: bool,
    json_wrapper_worker: bool,
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
            native_pi_tui_worker: native_pi_tui_worker_requested(
                opts.deprecated_experimental_pi_tui_worker,
                opts.json_wrapper_worker,
            ),
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

fn run_attend(
    paths: Paths,
    run_id: String,
    repo: Option<PathBuf>,
    latest: bool,
    interval_ms: u64,
) -> Result<()> {
    if latest && !run_id.trim().is_empty() {
        bail!("attend --latest cannot be combined with --run <run-id>");
    }
    if !latest && run_id.trim().is_empty() {
        bail!("attend requires --run <run-id> or --latest");
    }
    if !latest && repo.is_some() {
        bail!("attend --repo can only be used with --latest");
    }
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    let repo_path = if latest {
        Some(resolve_repo_path(
            repo.unwrap_or_else(|| PathBuf::from(".")),
        )?)
    } else {
        None
    };
    let interval = Duration::from_millis(interval_ms.max(100));
    let stdin = io::stdin();
    loop {
        let details = if latest {
            let repo_path = repo_path.as_ref().expect("repo path for latest");
            match fetch_latest_run(&client, &repo_path.to_string_lossy(), 50, false)? {
                Some(details) => details,
                None => {
                    print!("\x1b[2J\x1b[H");
                    println!("Khazad-Doom Attend");
                    println!("No run found for {}", repo_path.display());
                    println!("Press Enter to refresh or q to quit.");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    stdin.read_line(&mut input)?;
                    if matches!(input.trim(), "q" | "quit" | "exit") {
                        return Ok(());
                    }
                    thread::sleep(interval);
                    continue;
                }
            }
        } else {
            fetch_run_details(&client, &run_id, 50)?
        };
        print!("\x1b[2J\x1b[H");
        println!("Khazad-Doom Attend — {}", details.run.id);
        println!();
        render_run_monitor(&mut io::stdout(), &details)?;
        println!();
        println!(
            "Commands: a <n> <answer> | answer <question-id> <answer> | accept <proposal-id> <reason> | reject <proposal-id> <reason> | defer <proposal-id> <condition> --reason <reason> | resume | q"
        );
        print!("attend> ");
        io::stdout().flush()?;
        let mut input = String::new();
        stdin.read_line(&mut input)?;
        let input = input.trim();
        if input.is_empty() {
            thread::sleep(interval);
            continue;
        }
        if matches!(input, "q" | "quit" | "exit") {
            return Ok(());
        }
        handle_attend_command(&client, &details, input)?;
    }
}

fn handle_attend_command(client: &Client, details: &RunDetails, input: &str) -> Result<()> {
    let mut parts = input.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(());
    };
    match command {
        "a" => {
            let index: usize = parts
                .next()
                .context("usage: a <question-number> <answer>")?
                .parse()
                .context("question number must be an integer")?;
            let answer = parts.collect::<Vec<_>>().join(" ");
            if answer.trim().is_empty() {
                bail!("usage: a <question-number> <answer>");
            }
            let pending = details
                .questions
                .iter()
                .filter(|question| question.state == "pending")
                .collect::<Vec<_>>();
            let question = pending
                .get(index.saturating_sub(1))
                .ok_or_else(|| anyhow::anyhow!("pending question {index} not found"))?;
            let _: AnswerQuestionResult = client.call(
                "answerQuestion",
                &AnswerQuestionParams {
                    run_id: details.run.id.clone(),
                    question_id: question.id.clone(),
                    answer,
                },
            )?;
        }
        "answer" => {
            let question_id = parts
                .next()
                .context("usage: answer <question-id> <answer>")?;
            let answer = parts.collect::<Vec<_>>().join(" ");
            if answer.trim().is_empty() {
                bail!("usage: answer <question-id> <answer>");
            }
            let _: AnswerQuestionResult = client.call(
                "answerQuestion",
                &AnswerQuestionParams {
                    run_id: details.run.id.clone(),
                    question_id: question_id.to_string(),
                    answer,
                },
            )?;
        }
        "accept" | "reject" => {
            let proposal_id = parts
                .next()
                .context("usage: accept|reject <proposal-id> <reason>")?;
            let reason = strip_reason_flag(&parts.collect::<Vec<_>>().join(" "));
            if reason.trim().is_empty() {
                bail!("usage: accept|reject <proposal-id> <reason>");
            }
            let decision = if command == "accept" {
                "accepted"
            } else {
                "rejected"
            };
            let _: DecideReplanProposalResult = client.call(
                "decideReplanProposal",
                &DecideReplanProposalParams {
                    run_id: details.run.id.clone(),
                    proposal_id: proposal_id.to_string(),
                    decision: decision.to_string(),
                    rationale: reason,
                    authorizer: String::new(),
                    source: "attend".to_string(),
                    replacement_id: String::new(),
                    revisit_condition: String::new(),
                },
            )?;
        }
        "defer" => {
            let proposal_id = parts
                .next()
                .context("usage: defer <proposal-id> <condition> --reason <reason>")?;
            let rest = parts.collect::<Vec<_>>().join(" ");
            let (condition, reason) = split_defer_condition_reason(&rest)?;
            let _: DecideReplanProposalResult = client.call(
                "decideReplanProposal",
                &DecideReplanProposalParams {
                    run_id: details.run.id.clone(),
                    proposal_id: proposal_id.to_string(),
                    decision: "deferred".to_string(),
                    rationale: reason,
                    authorizer: String::new(),
                    source: "attend".to_string(),
                    replacement_id: String::new(),
                    revisit_condition: condition,
                },
            )?;
        }
        "resume" => {
            let _: StartRunResult = client.call(
                "resumeRun",
                &ResumeRunParams {
                    run_id: details.run.id.clone(),
                    agent: String::new(),
                    pi_bin: String::new(),
                    pi_args: Vec::new(),
                    native_pi_tui_worker: false,
                    parallelism: 1,
                },
            )?;
        }
        other => bail!("unknown attend command {other:?}"),
    }
    Ok(())
}

fn strip_reason_flag(value: &str) -> String {
    value
        .trim()
        .strip_prefix("--reason ")
        .unwrap_or(value.trim())
        .trim_matches('"')
        .to_string()
}

fn split_defer_condition_reason(value: &str) -> Result<(String, String)> {
    let Some((condition, reason)) = value.split_once(" --reason ") else {
        bail!("usage: defer <proposal-id> <condition> --reason <reason>");
    };
    let condition = condition.trim().trim_matches('"').to_string();
    let reason = reason.trim().trim_matches('"').to_string();
    if condition.is_empty() || reason.is_empty() {
        bail!("usage: defer <proposal-id> <condition> --reason <reason>");
    }
    Ok((condition, reason))
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

fn open_or_focus_cockpit(paths: &Paths, details: &RunDetails) -> Result<CockpitOpenOutput> {
    let run = &details.run;
    let workspace_label = cockpit_workspace_label_for_run(&run.id);
    let operator_commands = cockpit_operator_commands(&run.id);
    match open_default_run_cockpit_for_operator(run, &paths.root) {
        Ok(opened) => Ok(cockpit_open_success_output(run, opened, operator_commands)),
        Err(unavailable) => Ok(cockpit_unavailable_output(
            run,
            workspace_label,
            unavailable.message,
            operator_commands,
        )),
    }
}

fn cockpit_open_success_output(
    run: &crate::domain::Run,
    opened: CockpitOpenFocus,
    operator_commands: Vec<String>,
) -> CockpitOpenOutput {
    CockpitOpenOutput {
        run_id: run.id.clone(),
        repo_path: run.repo_path.clone(),
        workspace_label: opened.workspace_label,
        adapter: opened.adapter,
        opened: true,
        action: opened.action,
        pane_labels: opened.pane_labels,
        fallback: String::new(),
        remediation: String::new(),
        message: opened.message,
        operator_commands,
    }
}

fn cockpit_operator_commands(run_id: &str) -> Vec<String> {
    vec![
        format!("khazad-doom monitor --run {}", shell_quote_arg(run_id)),
        format!("khazad-doom watch --run {}", shell_quote_arg(run_id)),
        format!("khazad-doom status --run {}", shell_quote_arg(run_id)),
    ]
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

fn run_cockpit(paths: Paths, command: CockpitCommand) -> Result<()> {
    match command {
        CockpitCommand::Open { run, repo, latest } => run_cockpit_open(paths, run, repo, latest),
        CockpitCommand::PaintWorkerActivity {
            stdout,
            status,
            exit,
            interval_ms,
        } => run_cockpit_paint_worker_activity(stdout, status, exit, interval_ms),
        CockpitCommand::PaintGateActivity { run, interval_ms } => {
            run_cockpit_paint_gate_activity(paths, run, interval_ms)
        }
    }
}

fn run_cockpit_paint_worker_activity(
    stdout_path: PathBuf,
    status_path: PathBuf,
    exit_path: PathBuf,
    interval_ms: u64,
) -> Result<()> {
    let options = WorkerActivityPainterOptions {
        stdout_path,
        status_path,
        exit_path,
        poll_interval: Duration::from_millis(interval_ms.max(50)),
        startup_timeout: Duration::from_secs(10),
    };
    let mut out = io::stdout();
    paint_worker_activity(options, &mut out)
}

fn run_cockpit_paint_gate_activity(paths: Paths, run_id: String, interval_ms: u64) -> Result<()> {
    if run_id.trim().is_empty() {
        bail!("cockpit paint-gate-activity requires --run <run-id>");
    }
    let client = Client::new(paths);
    let interval = Duration::from_millis(interval_ms.max(250));
    let mut out = io::stdout();
    let mut first = true;
    loop {
        let details = fetch_run_details(&client, &run_id, 10)?;
        if !first {
            writeln!(out, "---")?;
        }
        first = false;
        paint_gate_activity_snapshot(&details, &mut out)?;
        out.flush()?;
        if is_terminal_status(details.run.status) {
            return Ok(());
        }
        thread::sleep(interval);
    }
}

fn paint_gate_activity_snapshot(details: &RunDetails, out: &mut impl Write) -> Result<bool> {
    paint_gate_activity_snapshot_at(details, chrono::Utc::now(), out)
}

fn paint_gate_activity_snapshot_at(
    details: &RunDetails,
    now: chrono::DateTime<chrono::Utc>,
    out: &mut impl Write,
) -> Result<bool> {
    let projection = project_gate_pane(details, now);
    render_feed_plain(out, &projection.feed)?;
    Ok(projection.active)
}

fn paint_worker_activity(
    options: WorkerActivityPainterOptions,
    out: &mut impl Write,
) -> Result<()> {
    writeln!(out, "Khazad-Doom worker activity painter (read-only)")?;
    writeln!(out, "source: {}", options.stdout_path.display())?;
    writeln!(
        out,
        "operator input: use daemon commands such as answer or cancel; this pane is display-only"
    )?;
    out.flush()?;

    let mut formatter = PiActivityFormatter::default();
    let mut offset = 0_u64;
    let started_at = Instant::now();
    loop {
        for line in read_new_activity_lines(&options.stdout_path, &mut offset)? {
            for rendered in formatter.render_line(&line) {
                writeln!(out, "{rendered}")?;
            }
            out.flush()?;
        }
        if worker_activity_terminal(&options.status_path, &options.exit_path) {
            for rendered in formatter.flush() {
                writeln!(out, "{rendered}")?;
            }
            writeln!(
                out,
                "[khazad] wrapper terminal artifacts observed; painter exiting"
            )?;
            out.flush()?;
            return Ok(());
        }
        if !options.stdout_path.exists()
            && !options.status_path.exists()
            && started_at.elapsed() >= options.startup_timeout
        {
            bail!(
                "worker activity painter timed out waiting for wrapper artifacts at {}",
                options.stdout_path.display()
            );
        }
        thread::sleep(options.poll_interval);
    }
}

fn read_new_activity_lines(path: &Path, offset: &mut u64) -> Result<Vec<String>> {
    let Ok(mut file) = File::open(path) else {
        return Ok(Vec::new());
    };
    let len = file.metadata()?.len();
    if len < *offset {
        *offset = 0;
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut consumed = 0_u64;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        consumed += bytes as u64;
        if line.ends_with('\n') {
            while matches!(line.chars().last(), Some('\n' | '\r')) {
                line.pop();
            }
            lines.push(line);
        } else {
            consumed = consumed.saturating_sub(bytes as u64);
            break;
        }
    }
    *offset += consumed;
    Ok(lines)
}

fn worker_activity_terminal(status_path: &Path, exit_path: &Path) -> bool {
    if exit_path.exists() {
        return true;
    }
    let Ok(text) = fs::read_to_string(status_path) else {
        return false;
    };
    let Ok(status) = serde_json::from_str::<WorkerActivityWrapperStatus>(&text) else {
        return false;
    };
    matches!(
        status.state.as_str(),
        "finished" | "handoff_failed" | "setup_failed"
    )
}

fn run_cockpit_open(
    paths: Paths,
    run_id: String,
    repo: Option<PathBuf>,
    latest: bool,
) -> Result<()> {
    if !run_id.is_empty() && latest {
        bail!("cockpit open --latest cannot be combined with --run <run-id>");
    }
    if run_id.is_empty() && !latest {
        bail!("cockpit open requires --run <run-id> or --latest");
    }
    if !run_id.is_empty() && repo.is_some() {
        bail!("cockpit open --repo can only be used with --latest");
    }

    ensure_daemon(&paths)?;
    let client = Client::new(paths.clone());
    let details = if latest {
        let repo = resolve_repo_path(repo.unwrap_or_else(|| PathBuf::from(".")))?;
        let repo_path = repo.to_string_lossy().to_string();
        fetch_latest_run(&client, &repo_path, 20, false)?
            .ok_or_else(|| anyhow::anyhow!("no runs found for repo {repo_path}"))?
    } else {
        fetch_run_details(&client, &run_id, 20)?
    };
    let output = open_or_focus_cockpit(&paths, &details)?;
    print_json(&output)
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

fn run_replan(paths: Paths, command: ReplanCommand) -> Result<()> {
    ensure_daemon(&paths)?;
    let client = Client::new(paths);
    match command {
        ReplanCommand::List { run } => {
            let result: ListReplanProposalsResult = client.call(
                "listReplanProposals",
                &ListReplanProposalsParams { run_id: run },
            )?;
            print_json(&result)
        }
        ReplanCommand::Propose {
            run,
            id,
            source_kind,
            source_slice,
            source_phase,
            source_attempt,
            source_summary,
            findings,
            evidence,
            changes,
            risk,
        } => {
            let proposed_changes = parse_replan_changes(&changes)?;
            let evidence = parse_replan_evidence(&evidence)?;
            let result: CreateReplanProposalResult = client.call(
                "createReplanProposal",
                &CreateReplanProposalParams {
                    run_id: run,
                    id,
                    source: ReplanProposalSource {
                        kind: source_kind,
                        slice_id: source_slice,
                        phase: source_phase,
                        attempt: source_attempt,
                        summary: source_summary,
                    },
                    trigger_finding_ids: findings,
                    evidence,
                    proposed_changes,
                    risk,
                },
            )?;
            print_json(&result)
        }
        ReplanCommand::Accept {
            run,
            proposal,
            reason,
            authorizer,
        } => decide_replan(
            &client,
            ReplanDecisionRequest {
                run_id: run,
                proposal_id: proposal,
                state: ReplanProposalState::Accepted,
                rationale: reason,
                authorizer,
                replacement_id: String::new(),
                revisit_condition: String::new(),
            },
        ),
        ReplanCommand::Reject {
            run,
            proposal,
            reason,
            authorizer,
        } => decide_replan(
            &client,
            ReplanDecisionRequest {
                run_id: run,
                proposal_id: proposal,
                state: ReplanProposalState::Rejected,
                rationale: reason,
                authorizer,
                replacement_id: String::new(),
                revisit_condition: String::new(),
            },
        ),
        ReplanCommand::Defer {
            run,
            proposal,
            revisit_condition,
            reason,
            authorizer,
        } => decide_replan(
            &client,
            ReplanDecisionRequest {
                run_id: run,
                proposal_id: proposal,
                state: ReplanProposalState::Deferred,
                rationale: reason,
                authorizer,
                replacement_id: String::new(),
                revisit_condition,
            },
        ),
        ReplanCommand::Supersede {
            run,
            proposal,
            replacement,
            reason,
            authorizer,
        } => decide_replan(
            &client,
            ReplanDecisionRequest {
                run_id: run,
                proposal_id: proposal,
                state: ReplanProposalState::Superseded,
                rationale: reason,
                authorizer,
                replacement_id: replacement,
                revisit_condition: String::new(),
            },
        ),
    }
}

struct ReplanDecisionRequest {
    run_id: String,
    proposal_id: String,
    state: ReplanProposalState,
    rationale: String,
    authorizer: String,
    replacement_id: String,
    revisit_condition: String,
}

fn decide_replan(client: &Client, request: ReplanDecisionRequest) -> Result<()> {
    let result: DecideReplanProposalResult = client.call(
        "decideReplanProposal",
        &DecideReplanProposalParams {
            run_id: request.run_id,
            proposal_id: request.proposal_id,
            decision: request.state.as_str().to_string(),
            rationale: request.rationale,
            authorizer: default_authorizer(request.authorizer),
            source: "cli".to_string(),
            replacement_id: request.replacement_id,
            revisit_condition: request.revisit_condition,
        },
    )?;
    print_json(&result)
}

fn parse_replan_evidence(values: &[String]) -> Result<Vec<ReplanEvidenceLink>> {
    values
        .iter()
        .map(|value| {
            let mut parts = value.splitn(3, ':');
            let kind = parts.next().unwrap_or_default().trim();
            let path = parts.next().unwrap_or_default().trim();
            let summary = parts.next().unwrap_or_default().trim();
            if kind.is_empty() || path.is_empty() {
                bail!("replan evidence must be kind:path[:summary], got {value:?}");
            }
            Ok(ReplanEvidenceLink {
                kind: kind.to_string(),
                path: path.to_string(),
                event_id: 0,
                summary: summary.to_string(),
            })
        })
        .collect()
}

fn parse_replan_changes(values: &[String]) -> Result<Vec<ReplanProposedChange>> {
    if values.is_empty() {
        bail!("replan propose requires at least one --change kind:target:summary");
    }
    values
        .iter()
        .map(|value| {
            let mut parts = value.splitn(3, ':');
            let kind = parts.next().unwrap_or_default().trim();
            let target = parts.next().unwrap_or_default().trim();
            let summary = parts.next().unwrap_or_default().trim();
            if kind.is_empty() || target.is_empty() || summary.is_empty() {
                bail!("replan change must be kind:target:summary, got {value:?}");
            }
            Ok(ReplanProposedChange {
                kind: kind.to_string(),
                target: target.to_string(),
                summary: summary.to_string(),
            })
        })
        .collect()
}

fn default_authorizer(value: String) -> String {
    if !value.trim().is_empty() {
        return value;
    }
    std::env::var("USER").unwrap_or_else(|_| "operator".to_string())
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
                StatusFeedRole::Attention => monitor_tree_unbounded(out, &line.text)?,
                StatusFeedRole::Dim => monitor_tree_dim(out, &line.text)?,
                _ => monitor_tree(out, &line.text)?,
            }
        }
    }
    Ok(())
}

const MONITOR_LINE_WIDTH: usize = 96;

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

fn monitor_tree_unbounded(out: &mut impl Write, text: &str) -> Result<()> {
    writeln!(out, "└ {text}")?;
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

fn cockpit_unavailable_output(
    run: &crate::domain::Run,
    workspace_label: String,
    message: String,
    operator_commands: Vec<String>,
) -> CockpitOpenOutput {
    CockpitOpenOutput {
        run_id: run.id.clone(),
        repo_path: run.repo_path.clone(),
        workspace_label,
        adapter: "herdr".to_string(),
        opened: false,
        action: "fallback".to_string(),
        pane_labels: Vec::new(),
        fallback: "Herdr cockpit was not opened; continue with daemon-owned status/watch/monitor, handoff, and answer commands.".to_string(),
        remediation: "Install a usable herdr binary on PATH, fix the Herdr command failure, or keep using khazad-doom monitor/watch/status for headless operation.".to_string(),
        message,
        operator_commands,
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

fn read_mission_envelope(
    path: Option<&Path>,
    autonomy_override: Option<&str>,
) -> Result<Option<MissionEnvelope>> {
    let mut envelope = if let Some(path) = path {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read mission envelope {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parse mission envelope JSON {}", path.display()))?;
        warn_unknown_mission_envelope_fields(path, &value);
        Some(
            serde_json::from_value::<MissionEnvelope>(value)
                .with_context(|| format!("decode mission envelope {}", path.display()))?,
        )
    } else {
        None
    };

    if let Some(level) = autonomy_override {
        let level = AutonomyLevel::parse(level)?;
        match envelope.as_mut() {
            Some(envelope) => envelope.autonomy_level = level,
            None if level == AutonomyLevel::Off => {}
            None => bail!(
                "--autonomy {level} requires --envelope; frontier classification is bounded by a per-run mission envelope"
            ),
        }
    }
    Ok(envelope)
}

fn warn_unknown_mission_envelope_fields(path: &Path, value: &serde_json::Value) {
    const KNOWN: &[&str] = &[
        "goal",
        "allowed_areas",
        "non_goals",
        "verify_profile",
        "max_auto_promotions",
        "max_depth",
        "max_generated_slices",
        "autonomy_level",
        "must_ask_if",
    ];
    let Some(object) = value.as_object() else {
        return;
    };
    for key in object.keys() {
        if !KNOWN.contains(&key.as_str()) {
            eprintln!(
                "warning: mission envelope {} contains unknown field {key:?}; field is ignored in AF-02",
                path.display()
            );
        }
    }
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

fn native_pi_tui_worker_requested(cli_flag: bool, json_wrapper_flag: bool) -> bool {
    if json_wrapper_flag
        || env_flag_enabled("KHAZAD_JSON_WRAPPER_WORKER")
        || env_flag_enabled("KHAZAD_DISABLE_PI_TUI_WORKER")
    {
        return false;
    }
    if cli_flag {
        return true;
    }
    if let Ok(value) = std::env::var("KHAZAD_EXPERIMENTAL_PI_TUI_WORKER") {
        return !matches!(value.trim(), "0" | "false" | "no" | "off");
    }
    true
}

fn env_flag_enabled(key: &str) -> bool {
    std::env::var(key)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_activity_painter_follows_file_growth_until_exit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let stdout_path = temp.path().join("worker.stdout.ndjson");
        let status_path = temp.path().join("worker.status.json");
        let exit_path = temp.path().join("worker.exit.json");
        let fixture = include_str!("../tests/fixtures/rpl_worker_activity.ndjson");
        let lines: Vec<_> = fixture.lines().collect();
        fs::write(&status_path, r#"{"state":"launched","pid":123}"#)?;
        fs::write(&stdout_path, format!("{}\n", lines[0]))?;

        let writer_stdout = stdout_path.clone();
        let writer_status = status_path.clone();
        let writer_exit = exit_path.clone();
        let remaining = lines[1..].join("\n") + "\n";
        let writer = thread::spawn(move || -> std::io::Result<()> {
            thread::sleep(Duration::from_millis(50));
            let mut file = fs::OpenOptions::new().append(true).open(&writer_stdout)?;
            file.write_all(remaining.as_bytes())?;
            file.flush()?;
            fs::write(&writer_exit, r#"{"exit_code":0}"#)?;
            fs::write(
                &writer_status,
                r#"{"state":"finished","pid":123,"exit_code":0}"#,
            )?;
            Ok(())
        });

        let mut out = Vec::new();
        paint_worker_activity(
            WorkerActivityPainterOptions {
                stdout_path,
                status_path,
                exit_path,
                poll_interval: Duration::from_millis(10),
                startup_timeout: Duration::from_secs(1),
            },
            &mut out,
        )?;
        writer.join().expect("writer thread")?;
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("read-only"));
        assert!(rendered.contains("tool read path="));
        assert!(rendered.contains("assistant: hello world"));
        assert!(rendered.contains("unknown event ignored"));
        assert!(rendered.contains("wrapper terminal artifacts observed"));
        Ok(())
    }

    #[test]
    fn render_monitor_dashboard_v2_uses_feed_only_and_keeps_attention_unbounded() -> Result<()> {
        let long_attention = "Attention: this operator-facing line must remain complete even when it is wider than the compact right dashboard column";
        let long_dim = "profile implementer: provider=openai-codex model=gpt-5.5 reasoning=xhigh mode=fast with extra wrapping noise";
        let mut details = test_run_details(TestRunDetailsOptions {
            status: RunStatus::Running,
            feed: Some(StatusFeed {
                feed_version: 1,
                summary_line: "daemon feed summary".to_string(),
                terminal_reason: None,
                operator_commands: vec!["khazad-doom answer kd-test q-1 <answer>".to_string()],
                attention: vec![crate::domain::StatusFeedLine {
                    text: long_attention.to_string(),
                    role: StatusFeedRole::Attention,
                }],
                blocks: vec![
                    crate::domain::StatusFeedBlock {
                        label: "Run".to_string(),
                        meta: "● running • kd-test".to_string(),
                        lines: vec![crate::domain::StatusFeedLine {
                            text: long_dim.to_string(),
                            role: StatusFeedRole::Dim,
                        }],
                    },
                    crate::domain::StatusFeedBlock {
                        label: "Attention".to_string(),
                        meta: String::new(),
                        lines: vec![crate::domain::StatusFeedLine {
                            text: long_attention.to_string(),
                            role: StatusFeedRole::Attention,
                        }],
                    },
                    crate::domain::StatusFeedBlock {
                        label: "Commands".to_string(),
                        meta: String::new(),
                        lines: vec![crate::domain::StatusFeedLine {
                            text: "khazad-doom answer kd-test q-1 <answer>".to_string(),
                            role: StatusFeedRole::Attention,
                        }],
                    },
                ],
            }),
            ..Default::default()
        });
        details.run.error = "from run error not feed".to_string();

        let mut out = Vec::new();
        render_run_monitor(&mut out, &details)?;
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains(long_attention));
        assert!(rendered.contains("khazad-doom answer kd-test q-1 <answer>"));
        assert!(!rendered.contains(long_dim));
        assert!(rendered.contains('…'));
        assert!(!rendered.contains("from run error not feed"));
        Ok(())
    }

    #[test]
    fn gate_activity_painter_renders_active_command_from_daemon_progress() -> Result<()> {
        let now = fixed_time();
        let details = test_run_details(TestRunDetailsOptions {
            status: RunStatus::Running,
            progress: Some(crate::domain::RunProgress {
                run_id: "kd-test".to_string(),
                phase: "integration_gate".to_string(),
                slice_id: String::new(),
                attempt: 0,
                command: "cargo test gate --quiet".to_string(),
                message: "running integration gate command".to_string(),
                output_tail: "gate line 1\ngate line 2\n".to_string(),
                phase_started_at: now - chrono::Duration::seconds(185),
                updated_at: now - chrono::Duration::seconds(5),
                worker: None,
                parallel_layer: false,
                parallel_slices: Vec::new(),
            }),
            ..Default::default()
        });

        assert_gate_pane_golden(
            &details,
            now,
            true,
            include_str!("../tests/fixtures/gate_activity_active_command.golden.txt"),
        )?;
        Ok(())
    }

    #[test]
    fn gate_pane_summary_idle_passed_uses_gate_scoped_golden() -> Result<()> {
        let now = fixed_time();
        let details = test_run_details(TestRunDetailsOptions {
            status: RunStatus::Completed,
            events: vec![
                run_started_event(now),
                implementation_summary_event(
                    now,
                    serde_json::json!({
                        "integration_gate": {
                            "status": "passed",
                            "summary": "integration gate passed",
                            "commands": [
                                { "command": "cargo test gate --quiet", "status": "passed", "exit_code": 0, "output": "ok" }
                            ]
                        },
                        "integration_repair": {
                            "status": "skipped",
                            "summary": "integration gate passed; integration_repair=auto skipped repair",
                            "trigger": "gate_passed"
                        },
                        "exit_states": {
                            "run": "completed",
                            "handoff": "ready",
                            "evidence": "attested",
                            "slices": []
                        }
                    }),
                ),
            ],
            economics: Some(test_economics()),
            feed: Some(generic_monitor_feed()),
            ..Default::default()
        });

        let rendered = assert_gate_pane_golden(
            &details,
            now,
            false,
            include_str!("../tests/fixtures/gate_activity_idle_passed.golden.txt"),
        )?;
        assert_absent_generic_monitor_sections(&rendered);
        Ok(())
    }

    #[test]
    fn gate_pane_summary_idle_failed_repairable_uses_daemon_gate_economics() -> Result<()> {
        let now = fixed_time();
        let mut economics = test_economics();
        economics
            .command_executions
            .push(crate::domain::CommandExecutionEconomics {
                phase: "integration_gate".to_string(),
                slice_id: String::new(),
                attempt: 0,
                command: "cargo test gate --quiet".to_string(),
                cwd: ".".to_string(),
                status: "failed".to_string(),
                exit_code: Some(1),
                duration_ms: 1200,
                dedupe_key: "cargo test gate --quiet".to_string(),
                tree_sha: "tree".to_string(),
                cache_key: "cache".to_string(),
                cache_hit: false,
                skip_reason: String::new(),
            });
        economics.command_execution_count = 1;
        let details = test_run_details(TestRunDetailsOptions {
            status: RunStatus::Running,
            events: vec![run_started_event(now)],
            economics: Some(economics),
            feed: Some(generic_monitor_feed()),
            ..Default::default()
        });

        let rendered = assert_gate_pane_golden(
            &details,
            now,
            false,
            include_str!("../tests/fixtures/gate_activity_idle_failed_repairable.golden.txt"),
        )?;
        assert_absent_generic_monitor_sections(&rendered);
        Ok(())
    }

    #[test]
    fn gate_pane_summary_idle_no_gate_yet_uses_gate_scoped_golden() -> Result<()> {
        let now = fixed_time();
        let details = test_run_details(TestRunDetailsOptions {
            status: RunStatus::Running,
            events: vec![run_started_event(now)],
            economics: Some(test_economics()),
            feed: Some(generic_monitor_feed()),
            ..Default::default()
        });

        let rendered = assert_gate_pane_golden(
            &details,
            now,
            false,
            include_str!("../tests/fixtures/gate_activity_idle_no_gate.golden.txt"),
        )?;
        assert_absent_generic_monitor_sections(&rendered);
        Ok(())
    }

    fn assert_gate_pane_golden(
        details: &RunDetails,
        now: chrono::DateTime<chrono::Utc>,
        expected_active: bool,
        expected: &str,
    ) -> Result<String> {
        let mut out = Vec::new();
        let active = paint_gate_activity_snapshot_at(details, now, &mut out)?;
        let rendered = String::from_utf8(out).unwrap();
        assert_eq!(active, expected_active);
        assert_eq!(rendered, expected);
        Ok(rendered)
    }

    fn assert_absent_generic_monitor_sections(rendered: &str) {
        assert!(!rendered.contains("Activity"), "{rendered}");
        assert!(!rendered.contains("Todos"), "{rendered}");
        assert!(
            !rendered.contains("unrelated worker feed entry"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("existing daemon feed/status summary"),
            "{rendered}"
        );
    }

    struct TestRunDetailsOptions {
        status: RunStatus,
        progress: Option<crate::domain::RunProgress>,
        events: Vec<crate::domain::Event>,
        economics: Option<crate::domain::RunEconomics>,
        feed: Option<StatusFeed>,
    }

    impl Default for TestRunDetailsOptions {
        fn default() -> Self {
            Self {
                status: RunStatus::Running,
                progress: None,
                events: Vec::new(),
                economics: None,
                feed: None,
            }
        }
    }

    fn test_run_details(options: TestRunDetailsOptions) -> RunDetails {
        let now = fixed_time();
        RunDetails {
            run: crate::domain::Run {
                id: "kd-test".to_string(),
                repo_id: "repo".to_string(),
                repo_path: "/tmp/repo".to_string(),
                status: options.status,
                base_branch: "main".to_string(),
                base_sha: "base".to_string(),
                integration_branch: "khazad/kd-test/integration".to_string(),
                selected_slice_id: "slice-1".to_string(),
                error: String::new(),
                started_at: now,
                updated_at: now,
            },
            worker_profile: Default::default(),
            slice_runs: Vec::new(),
            generated_slices: Vec::new(),
            progress: options.progress,
            incidents: Vec::new(),
            questions: Vec::new(),
            replan: Default::default(),
            mission_envelope: None,
            frontier_budget: None,
            events: options.events,
            economics: options.economics,
            primary_terminal_reason: None,
            feed: options.feed,
        }
    }

    fn test_economics() -> crate::domain::RunEconomics {
        crate::domain::RunEconomics {
            repair_policy: "auto".to_string(),
            repair_max_attempts: 1,
            ..Default::default()
        }
    }

    fn run_started_event(now: chrono::DateTime<chrono::Utc>) -> crate::domain::Event {
        crate::domain::Event {
            id: 1,
            run_id: "kd-test".to_string(),
            typ: "run_started".to_string(),
            payload: serde_json::json!({
                "verify_profile": "full",
                "verify_profiles": ["full"]
            }),
            created_at: now,
        }
    }

    fn implementation_summary_event(
        now: chrono::DateTime<chrono::Utc>,
        payload: serde_json::Value,
    ) -> crate::domain::Event {
        crate::domain::Event {
            id: 2,
            run_id: "kd-test".to_string(),
            typ: "implementation_summary".to_string(),
            payload,
            created_at: now,
        }
    }

    fn generic_monitor_feed() -> StatusFeed {
        StatusFeed {
            feed_version: 1,
            summary_line: "existing daemon feed/status summary".to_string(),
            terminal_reason: None,
            operator_commands: Vec::new(),
            attention: Vec::new(),
            blocks: vec![crate::domain::StatusFeedBlock {
                label: "Activity".to_string(),
                meta: "(1 recent)".to_string(),
                lines: vec![crate::domain::StatusFeedLine {
                    text: "unrelated worker feed entry".to_string(),
                    role: StatusFeedRole::Dim,
                }],
            }],
        }
    }

    fn fixed_time() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }
}
