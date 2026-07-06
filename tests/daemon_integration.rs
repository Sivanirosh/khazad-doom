use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn daemon_fake_run_handoff_and_inspect_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "First fake slice",
            "goal": "Create first fake output.",
            "depends_on": [],
            "acceptance": ["slice-001.txt exists"],
            "verify": ["test -f slice-001.txt"]
        }),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-002",
            "title": "Second fake slice",
            "goal": "Create second fake output after the first.",
            "depends_on": ["slice-001"],
            "acceptance": ["slice-002.txt exists"],
            "verify": ["test -f slice-002.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add slices"])?;

    let validate = kd_ok(
        &bin,
        home.path(),
        &["slices", "validate", "--repo", path(repo.path())],
    )?;
    assert!(json_stdout(&validate)?["valid"].as_bool().unwrap_or(false));

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let status = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    assert_eq!(status["run"]["selected_slice_id"], "slice-001,slice-002");
    assert!(
        status["economics"]["agent_call_count"]
            .as_u64()
            .unwrap_or(0)
            >= 2
    );
    assert!(
        status["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["type"].as_str() == Some("run_completed") })
    );

    let handoff = kd_ok(&bin, home.path(), &["handoff", "--run", &run_id])?;
    let handoff = json_stdout(&handoff)?;
    assert_eq!(handoff["run_id"], run_id);
    assert_eq!(handoff["exit_states"]["run"], "completed");
    assert_eq!(handoff["exit_states"]["handoff"], "ready_for_handoff");
    assert_eq!(handoff["exit_states"]["evidence"], "daemon_attested");
    assert_eq!(handoff["evidence_attestation"]["status"], "daemon_attested");
    assert_eq!(
        handoff["evidence_attestation"]["worker_self_approved"],
        false
    );
    assert!(handoff["push_command"].as_str().unwrap().contains("git -C"));
    assert!(
        handoff["pr_command"]
            .as_str()
            .unwrap()
            .contains("gh pr create")
    );
    let final_sha = handoff["final_sha"].as_str().expect("final sha");
    let integration_branch = handoff["integration_branch"]
        .as_str()
        .expect("integration branch");
    assert_eq!(
        final_sha,
        git(repo.path(), &["rev-parse", integration_branch])?
    );
    let final_report_path = PathBuf::from(
        handoff["final_report_path"]
            .as_str()
            .expect("final report path"),
    );
    let final_report: Value = serde_json::from_str(&fs::read_to_string(final_report_path)?)?;
    assert_eq!(final_report["final_sha"].as_str(), Some(final_sha));
    let closed_slice_ref = format!("{final_sha}:.workflow/slices/slice-001.json");
    let closed_slice = git(repo.path(), &["show", &closed_slice_ref])?;
    assert!(closed_slice.contains("\"status\": \"closed\""));
    assert!(closed_slice.contains(&format!("\"closed_by_run\": \"{}\"", run_id)));
    let report_ref = format!("{final_sha}:.workflow/reports/{run_id}-final-report.json");
    git(repo.path(), &["show", &report_ref])?;

    let inspection = kd_ok(
        &bin,
        home.path(),
        &["inspect", "--run", &run_id, "--log-tail", "5"],
    )?;
    let inspection = json_stdout(&inspection)?;
    let artifacts = inspection["artifacts"].as_array().expect("artifacts");
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact["name"] == "final-report.json")
    );
    assert!(
        artifacts.iter().any(|artifact| {
            artifact["kind"] == "output" && artifact["name"] == "economics.json"
        })
    );
    assert!(
        artifacts.iter().any(|artifact| {
            artifact["kind"] == "handoff" && artifact["name"] == "slice-001.json"
        })
    );

    let env_started = kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_AGENT", "fake")],
        &["run", "--repo", path(repo.path()), "--all"],
    )?;
    let env_run_id = json_stdout(&env_started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_status(&bin, home.path(), &env_run_id, "completed")?;

    let missing_cancel = kd(&bin, home.path(), &["cancel", "--run", "does-not-exist"])?;
    assert!(!missing_cancel.status.success());

    guard.stop();
    Ok(())
}

#[test]
fn daemon_status_responds_while_raw_socket_client_is_idle_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["daemon", "start"])?;
    let _idle_client = UnixStream::connect(home.path().join("socket"))?;

    let status = kd_with_timeout(
        &bin,
        home.path(),
        &["daemon", "status"],
        Duration::from_secs(2),
    )?;
    assert!(
        status.status.success(),
        "daemon status failed while an idle socket client was connected\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "running");

    let start = kd_with_timeout(
        &bin,
        home.path(),
        &["daemon", "start"],
        Duration::from_secs(2),
    )?;
    assert!(
        start.status.success(),
        "daemon start failed while an idle socket client was connected\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(
        String::from_utf8_lossy(&start.stdout).contains("daemon already running"),
        "daemon start should classify the existing healthy daemon as running: {}",
        String::from_utf8_lossy(&start.stdout)
    );

    guard.stop();
    Ok(())
}

#[test]
fn daemon_start_detaches_from_parent_process_group_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["daemon", "start"])?;
    let pid = read_daemon_pid(home.path())?;
    let daemon_pgrp = unsafe { libc::getpgid(pid) };
    assert_ne!(daemon_pgrp, -1, "daemon pid should have a process group");
    let parent_pgrp = unsafe { libc::getpgrp() };
    assert_ne!(
        daemon_pgrp, parent_pgrp,
        "daemon should be detached from the CLI process group so terminal Ctrl-C/Ctrl-Z cannot stop it"
    );
    assert_eq!(
        daemon_pgrp, pid,
        "daemon should become its own session/process-group leader"
    );

    guard.stop();
    Ok(())
}

#[test]
fn daemon_status_reports_stopped_daemon_without_hanging_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["daemon", "start"])?;
    let pid = read_daemon_pid(home.path())?;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let status_result = kd_with_timeout(
        &bin,
        home.path(),
        &["daemon", "status"],
        Duration::from_secs(2),
    );
    let _ = unsafe { libc::kill(pid, libc::SIGCONT) };
    let status = status_result?;
    assert!(
        !status.status.success(),
        "stopped daemon should be unhealthy, not reported running"
    );
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("daemon unhealthy"),
        "stopped daemon status should explain unhealthy daemon\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    guard.stop();
    Ok(())
}

#[test]
fn untracked_slice_metadata_completes_with_incident_flag_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Untracked workflow slice",
            "goal": "Create fake output while workflow metadata is untracked.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["test -f slice-001.txt"]
        }),
    )?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--all",
            "--allow-dirty",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let status = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let incidents = status["incidents"].as_array().expect("incidents array");
    assert!(incidents.iter().any(|incident| {
        incident["kind"].as_str() == Some("slice_close_skipped")
            && incident["severity"].as_str() == Some("warning")
    }));
    let close_event = status["events"]
        .as_array()
        .expect("events array")
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("run_incident")
                && event["payload"]["kind"].as_str() == Some("slice_close_skipped")
        })
        .expect("slice close skipped event");
    assert_eq!(close_event["payload"]["slice_id"], "slice-001");
    assert!(
        close_event["payload"]["path"]
            .as_str()
            .unwrap()
            .ends_with(".workflow/slices/slice-001.json")
    );
    assert_eq!(
        close_event["payload"]["policy"],
        "preserve_handoff_ready_missing_metadata"
    );
    let monitor = kd_ok(
        &bin,
        home.path(),
        &[
            "monitor",
            "--run",
            &run_id,
            "--once",
            "--interval-ms",
            "100",
        ],
    )?;
    let monitor = String::from_utf8(monitor.stdout)?;
    assert!(monitor.contains("Incidents"));
    assert!(monitor.contains("slice_close_skipped"));
    let latest_status = kd_ok(
        &bin,
        home.path(),
        &[
            "status",
            "--repo",
            path(repo.path()),
            "--latest",
            "--include-terminal",
        ],
    )?;
    assert_eq!(json_stdout(&latest_status)?["run"]["id"], run_id);
    let latest_monitor = kd_ok(
        &bin,
        home.path(),
        &[
            "monitor",
            "--repo",
            path(repo.path()),
            "--latest",
            "--once",
            "--interval-ms",
            "100",
        ],
    )?;
    let latest_monitor = String::from_utf8(latest_monitor.stdout)?;
    assert!(latest_monitor.contains(&run_id));
    assert!(latest_monitor.contains("Run ✓ completed"));

    guard.stop();
    Ok(())
}

#[test]
fn schema_import_and_handoff_v2_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let remote = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let gh_log = home.path().join("fake-gh.log");
    write_fake_gh(fake_bin.path())?;
    let path_env = prepend_path(fake_bin.path());
    let gh_log_string = path(&gh_log).to_string();
    let env = [
        ("PATH", path_env.as_str()),
        ("FAKE_GH_LOG", gh_log_string.as_str()),
    ];

    init_git_repo(repo.path())?;
    git(remote.path(), &["init", "--bare"])?;
    git(
        repo.path(),
        &["remote", "add", "origin", path(remote.path())],
    )?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok_with_env(
        &bin,
        home.path(),
        &env,
        &["init", "--repo", path(repo.path())],
    )?;
    let schema = kd_ok(
        &bin,
        home.path(),
        &["slices", "schema", "--repo", path(repo.path()), "--write"],
    )?;
    assert_eq!(
        json_stdout(&schema)?["title"],
        "Khazad-Doom JSON Issue Slice"
    );
    assert!(
        repo.path()
            .join(".workflow/schema/slice.schema.json")
            .exists()
    );

    let imported = kd_ok_with_env(
        &bin,
        home.path(),
        &env,
        &[
            "slices",
            "import-github",
            "--repo",
            path(repo.path()),
            "--issue",
            "https://github.com/acme/widgets/issues/42",
            "--dry-run",
        ],
    )?;
    let imported = json_stdout(&imported)?;
    assert_eq!(imported["written"], false);
    assert_eq!(imported["slice"]["areas"][0], "backend");
    assert!(imported["slice"]["acceptance"].as_array().unwrap().len() >= 2);
    assert!(fs::read_to_string(&gh_log)?.contains("--repo acme/widgets"));

    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Fake handoff slice",
            "goal": "Create fake output.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["test -f slice-001.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add workflow"])?;
    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_status(&bin, home.path(), &run_id, "completed")?;

    let handoff = kd_ok_with_env(
        &bin,
        home.path(),
        &env,
        &["handoff", "--run", &run_id, "--push", "--create-pr"],
    )?;
    let handoff = json_stdout(&handoff)?;
    assert_eq!(handoff["diagnostics"]["gh_available"], true);
    assert!(
        handoff["diagnostics"]["origin_url"]
            .as_str()
            .unwrap()
            .contains(path(remote.path()))
    );
    let actions = handoff["actions"].as_array().expect("actions");
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|action| action["status"] == "passed"));
    assert!(fs::read_to_string(&gh_log)?.contains("pr create"));

    let dry = kd_ok(
        &bin,
        home.path(),
        &["handoff", "--run", &run_id, "--dry-run"],
    )?;
    let dry = json_stdout(&dry)?;
    assert_eq!(dry["dry_run"], true);
    assert!(
        dry["actions"]
            .as_array()
            .is_none_or(|actions| actions.is_empty())
    );

    guard.stop();
    Ok(())
}

#[test]
fn status_and_watch_expose_live_progress_for_long_verification() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Long verification slice",
            "goal": "Create fake output and run a visibly long verification command.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["printf 'started-progress\\n'; sleep 4; printf 'finished-progress\\n'; test -f slice-001.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add long verification slice"],
    )?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let live = wait_for_progress_output(
        &bin,
        home.path(),
        &run_id,
        "worker_verify",
        "started-progress",
    )?;
    assert_eq!(live["progress"]["slice_id"], "slice-001");
    assert_eq!(
        live["progress"]["command"],
        "printf 'started-progress\\n'; sleep 4; printf 'finished-progress\\n'; test -f slice-001.txt"
    );
    assert!(
        live["progress"]["output_tail"]
            .as_str()
            .unwrap_or_default()
            .contains("started-progress"),
        "progress should include streamed command output: {live:#}"
    );

    let monitored_once = kd_ok(
        &bin,
        home.path(),
        &[
            "monitor",
            "--run",
            &run_id,
            "--once",
            "--interval-ms",
            "100",
        ],
    )?;
    let monitored_once = String::from_utf8(monitored_once.stdout)?;
    assert!(monitored_once.contains("Khazad-Doom Monitor"));
    assert!(monitored_once.contains(&run_id));
    assert!(monitored_once.contains("Todos"));
    assert!(monitored_once.contains("Run ● running"));
    assert!(monitored_once.contains("phase worker_verify"));
    assert!(monitored_once.contains("slice slice-001"));
    assert!(monitored_once.contains("Shell"));
    assert!(monitored_once.contains("elapsed"));
    assert!(monitored_once.contains("Activity"));
    assert!(monitored_once.contains("Tail"));
    assert!(monitored_once.contains("started-progress"));

    wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let monitored_completed = kd_ok(
        &bin,
        home.path(),
        &["monitor", "--run", &run_id, "--interval-ms", "100"],
    )?;
    let monitored_completed = String::from_utf8(monitored_completed.stdout)?;
    assert!(monitored_completed.contains(&run_id));
    assert!(monitored_completed.contains("Run ✓ completed"));

    let watched = kd_ok(
        &bin,
        home.path(),
        &["watch", "--run", &run_id, "--interval-ms", "100"],
    )?;
    let watched = String::from_utf8(watched.stdout)?;
    assert!(watched.contains(&format!("Run: {run_id}")));
    assert!(watched.contains("Status: completed"));
    assert!(watched.contains("Phase: completed"));

    guard.stop();
    Ok(())
}

#[test]
fn monitor_exposes_quiet_pi_worker_supervision_without_default_timeout() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_quiet_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    fs::write(
        repo.path().join(".workflow/khazad.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "agent": "pi",
                "parallelism": 1,
                "verify_timeout_seconds": 600,
                "worker_attempt_timeout_seconds": 0,
                "worker_no_output_warning_seconds": 1,
                "worker_termination_grace_seconds": 1
            }))?
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Quiet pi worker slice",
            "goal": "Use a quiet pi-compatible worker long enough for supervisor status to be visible.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["test -f slice-001.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add quiet worker slice"])?;

    let fake_pi_string = path(&fake_pi).to_string();
    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &["run", "--repo", path(repo.path()), "--agent", "pi", "--all"],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let status = wait_for_worker_supervision(&bin, home.path(), &run_id)?;
    assert_eq!(status["progress"]["phase"], "worker_running");
    assert_eq!(status["progress"]["worker"]["attempt_timeout_seconds"], 0);
    assert!(status["progress"]["worker"]["process_observed_at"].is_string());
    assert!(status["progress"]["worker"]["pid"].is_number());

    let monitored = wait_for_monitor_text(
        &bin,
        home.path(),
        &run_id,
        &[
            "Supervisor: alive, observed child",
            "Process: running pid=",
            "Last worker event: none",
            "Last semantic progress: unknown",
            "Timeout: disabled",
            "worker is quiet",
            "wait, inspect, or cancel explicitly",
        ],
    )?;
    assert!(monitored.contains(&run_id));

    wait_for_status(&bin, home.path(), &run_id, "completed")?;

    guard.stop();
    Ok(())
}

#[test]
fn pi_auth_launch_failure_blocks_without_retries_or_later_layers() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_auth_failure_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Auth blocked slice",
            "goal": "Try to launch a pi worker without auth.",
            "acceptance": ["auth failure blocks without retries"]
        }),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-002",
            "title": "Dependent slice should not launch",
            "goal": "Remain pending because slice-001 blocked.",
            "depends_on": ["slice-001"],
            "acceptance": ["later dependency layer does not launch"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add auth blocked slices"])?;

    let fake_pi_string = path(&fake_pi).to_string();
    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &["run", "--repo", path(repo.path()), "--agent", "pi", "--all"],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let blocked = wait_for_terminal_status(&bin, home.path(), &run_id, "blocked")?;
    assert!(
        blocked["run"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Pi is not authenticated for provider openai-codex"),
        "blocked run should explain auth failure: {blocked:#}"
    );

    let slice_runs = blocked["slice_runs"].as_array().expect("slice_runs");
    let slice_run = |slice_id: &str| {
        slice_runs
            .iter()
            .find(|slice_run| slice_run["slice_id"].as_str() == Some(slice_id))
            .unwrap_or_else(|| panic!("missing slice run {slice_id}: {blocked:#}"))
    };
    assert_eq!(slice_run("slice-001")["status"], "blocked");
    assert_eq!(slice_run("slice-001")["attempts"], 1);
    assert_eq!(slice_run("slice-002")["status"], "pending");
    assert_eq!(slice_run("slice-002")["attempts"], 0);

    let incident = blocked["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("run_incident")
                && event["payload"]["failure_kind"].as_str() == Some("agent_auth_required")
        })
        .expect("agent auth incident");
    assert_eq!(incident["payload"]["operator_action_required"], true);
    assert_eq!(incident["payload"]["retryable"], false);
    assert_eq!(incident["payload"]["agent_provider"], "openai-codex");
    assert_eq!(incident["payload"]["agent_model"], "gpt-5.5");
    assert_eq!(incident["payload"]["agent_profile"], "implementer");
    assert!(
        incident["payload"]["fix_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str() == Some("pi /login"))
    );

    guard.stop();
    Ok(())
}

#[test]
fn cancelled_pi_worker_retains_terminal_and_attempt_artifacts_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_cancellable_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    fs::write(
        repo.path().join(".workflow/khazad.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "agent": "pi",
                "worker_termination_grace_seconds": 1
            }))?
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Cancellable pi worker slice",
            "goal": "Stay running until cancellation so terminal artifacts can be captured.",
            "acceptance": ["cancellation is retained"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add cancellable worker slice"],
    )?;

    let fake_pi_string = path(&fake_pi).to_string();
    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &["run", "--repo", path(repo.path()), "--agent", "pi", "--all"],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_worker_supervision(&bin, home.path(), &run_id)?;

    let reason = "test cancellation keeps forensic evidence";
    kd_ok(
        &bin,
        home.path(),
        &["cancel", "--run", &run_id, "--reason", reason],
    )?;
    let cancelled = wait_for_status(&bin, home.path(), &run_id, "cancelled")?;
    assert_eq!(cancelled["run"]["error"], reason);

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let run_summary_path = artifact_path(&inspected, "run-summary.json")?;
    let run_summary: Value = serde_json::from_str(&fs::read_to_string(run_summary_path)?)?;
    assert_eq!(run_summary["cancel_reason"], reason);
    assert_eq!(run_summary["primary_failure"], reason);

    let failure_path = artifact_path(&inspected, "slice-001.worker.attempt-1.failure.json")?;
    let failure: Value = serde_json::from_str(&fs::read_to_string(failure_path)?)?;
    assert!(
        failure["stdout_tail"]
            .as_str()
            .unwrap()
            .contains("partial stdout")
    );
    assert!(
        failure["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("partial stderr")
    );

    guard.stop();
    Ok(())
}

#[test]
fn parallel_layer_failure_joins_records_and_cancels_siblings_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let marker = tempfile::tempdir()?;
    let fake_pi = write_parallel_fail_fake_pi(fake_bin.path())?;
    let marker_string = path(marker.path()).to_string();
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_PARALLEL_MARKER", marker_string.as_str())],
        &["init", "--repo", path(repo.path())],
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Failing parallel slice",
            "goal": "Fail while a sibling worker is still active.",
            "acceptance": ["parallel layer failure is recorded"]
        }),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-002",
            "title": "Long parallel sibling",
            "goal": "Stay active until the layer cancellation reaches this worker.",
            "acceptance": ["parallel sibling receives cancellation"],
            "verify": ["test -f slice-002.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add parallel failure slices"],
    )?;

    let fake_pi_string = path(&fake_pi).to_string();
    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[
            ("KHAZAD_PI_BIN", fake_pi_string.as_str()),
            ("KHAZAD_PARALLEL_MARKER", marker_string.as_str()),
        ],
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--all",
            "--parallel",
            "2",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let live = wait_for_parallel_progress(&bin, home.path(), &run_id, &["slice-001", "slice-002"])?;
    assert_eq!(live["progress"]["parallel_layer"], true);
    assert_eq!(
        live["progress"]["parallel_slices"],
        json!(["slice-001", "slice-002"])
    );
    let monitored = kd_ok(
        &bin,
        home.path(),
        &[
            "monitor",
            "--run",
            &run_id,
            "--once",
            "--interval-ms",
            "100",
        ],
    )?;
    let monitored = String::from_utf8(monitored.stdout)?;
    assert!(
        monitored.contains("parallel_worker_layer"),
        "monitor should show parallel phase; output:\n{monitored}"
    );
    assert!(
        monitored.contains("Parallel layer: slice-001, slice-002"),
        "monitor should show active parallel layer; output:\n{monitored}"
    );

    let failed = wait_for_terminal_status(&bin, home.path(), &run_id, "failed")?;
    assert!(
        failed["run"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("parallel worker layer failed"),
        "run error should summarize the parallel layer: {failed:#}"
    );
    assert!(marker.path().join("slice-002.terminated").exists());

    let slice_runs = failed["slice_runs"].as_array().expect("slice_runs");
    let slice_status = |slice_id: &str| {
        slice_runs
            .iter()
            .find(|slice_run| slice_run["slice_id"].as_str() == Some(slice_id))
            .and_then(|slice_run| slice_run["status"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(slice_status("slice-001"), "failed");
    assert_eq!(slice_status("slice-002"), "cancelled");

    let failed_event = failed["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"].as_str() == Some("parallel_layer_failed"))
        .expect("parallel layer failure event");
    let outcomes = failed_event["payload"]["outcomes"]
        .as_array()
        .expect("parallel outcomes");
    assert_eq!(outcomes[0]["slice_id"], "slice-001");
    assert_eq!(outcomes[0]["status"], "failed");
    assert_eq!(outcomes[1]["slice_id"], "slice-002");
    assert_eq!(outcomes[1]["status"], "cancelled");

    let branch = failed["run"]["integration_branch"].as_str().unwrap();
    let subjects = git(repo.path(), &["log", "--format=%s", branch])?;
    assert!(!subjects.contains("khazad(slice:slice-001): merge"));
    assert!(!subjects.contains("khazad(slice:slice-002): merge"));

    guard.stop();
    Ok(())
}

#[test]
fn monitor_specific_run_returns_error_for_failed_terminal_status() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Failing monitor slice",
            "goal": "Fail verification so monitor reports a terminal error.",
            "acceptance": ["monitor sees failed run"],
            "verify": ["printf 'monitor-fail\\n'; false"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add failing monitor slice"])?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let monitored = kd(
        &bin,
        home.path(),
        &["monitor", "--run", &run_id, "--interval-ms", "100"],
    )?;
    assert!(!monitored.status.success());
    let stdout = String::from_utf8_lossy(&monitored.stdout);
    let stderr = String::from_utf8_lossy(&monitored.stderr);
    assert!(stdout.contains(&run_id));
    assert!(stdout.contains("Run ✗ failed"));
    assert!(stderr.contains("run ended with status failed"));

    guard.stop();
    Ok(())
}

#[test]
fn status_latest_returns_active_run_for_repo_or_null() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo_a = tempfile::tempdir()?;
    let repo_b = tempfile::tempdir()?;
    let empty_repo = tempfile::tempdir()?;
    init_git_repo(repo_a.path())?;
    init_git_repo(repo_b.path())?;
    init_git_repo(empty_repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(
        &bin,
        home.path(),
        &["init", "--repo", path(empty_repo.path())],
    )?;
    let empty = kd_ok(
        &bin,
        home.path(),
        &["status", "--repo", path(empty_repo.path()), "--latest"],
    )?;
    assert!(json_stdout(&empty)?.is_null());
    let empty_monitor = kd_ok(
        &bin,
        home.path(),
        &[
            "monitor",
            "--repo",
            path(empty_repo.path()),
            "--latest",
            "--once",
        ],
    )?;
    let empty_monitor = String::from_utf8(empty_monitor.stdout)?;
    assert!(empty_monitor.contains("Run waiting"));
    assert!(empty_monitor.contains("waiting for the latest active daemon-owned run"));

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo_a.path())])?;
    kd_ok(&bin, home.path(), &["init", "--repo", path(repo_b.path())])?;
    write_slice(
        repo_a.path(),
        json!({
            "id": "slice-001",
            "title": "Long active slice A",
            "goal": "Keep repo A active long enough for latest lookup.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["printf 'latest-a\\n'; sleep 6; test -f slice-001.txt"]
        }),
    )?;
    write_slice(
        repo_b.path(),
        json!({
            "id": "slice-001",
            "title": "Long active slice B",
            "goal": "Keep repo B active long enough for latest lookup.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["printf 'latest-b\\n'; sleep 6; test -f slice-001.txt"]
        }),
    )?;
    git(repo_a.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo_a.path(), &["commit", "-m", "add long slice a"])?;
    let repo_a_subdir = repo_a.path().join("nested");
    fs::create_dir_all(&repo_a_subdir)?;
    git(repo_b.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo_b.path(), &["commit", "-m", "add long slice b"])?;

    let started_a = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(&repo_a_subdir),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let started_a = json_stdout(&started_a)?;
    let run_a = started_a["run_id"].as_str().expect("run_id").to_string();
    assert_eq!(started_a["repo_path"], path(repo_a.path()));
    assert_eq!(
        started_a["monitor_command"],
        format!(
            "khazad-doom monitor --repo {} --latest",
            path(repo_a.path())
        )
    );
    let latest_a = wait_for_latest_run(&bin, home.path(), repo_a.path(), &run_a)?;
    assert_eq!(latest_a["run"]["repo_path"], path(repo_a.path()));
    let latest_a_from_subdir = wait_for_latest_run(&bin, home.path(), &repo_a_subdir, &run_a)?;
    assert_eq!(latest_a_from_subdir["run"]["id"], run_a);
    assert_eq!(latest_a["run"]["status"], "running");
    assert!(latest_a["progress"].is_object());
    assert!(!latest_a["slice_runs"].as_array().unwrap().is_empty());
    assert!(!latest_a["events"].as_array().unwrap().is_empty());
    let latest_monitor = kd_ok(
        &bin,
        home.path(),
        &[
            "monitor",
            "--repo",
            path(repo_a.path()),
            "--latest",
            "--once",
        ],
    )?;
    let latest_monitor = String::from_utf8(latest_monitor.stdout)?;
    assert!(latest_monitor.contains("Khazad-Doom Monitor"));
    assert!(latest_monitor.contains(&run_a));
    assert!(latest_monitor.contains("Run ● running"));
    assert!(latest_monitor.contains("Activity"));

    let started_b = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo_b.path()),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let run_b = json_stdout(&started_b)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let latest_b = wait_for_latest_run(&bin, home.path(), repo_b.path(), &run_b)?;
    assert_eq!(latest_b["run"]["repo_path"], path(repo_b.path()));

    let scoped_a = wait_for_latest_run(&bin, home.path(), repo_a.path(), &run_a)?;
    assert_eq!(scoped_a["run"]["id"], run_a);

    wait_for_status(&bin, home.path(), &run_a, "completed")?;
    wait_for_status(&bin, home.path(), &run_b, "completed")?;
    let no_active = kd_ok(
        &bin,
        home.path(),
        &["status", "--repo", path(repo_a.path()), "--latest"],
    )?;
    assert!(json_stdout(&no_active)?.is_null());
    let latest_inspect = kd_ok(
        &bin,
        home.path(),
        &["inspect", "--repo", path(repo_a.path()), "--latest"],
    )?;
    let latest_inspect = json_stdout(&latest_inspect)?;
    assert_eq!(latest_inspect["run"]["id"], run_a.as_str());
    assert!(artifact_path(&latest_inspect, "run-summary.json").is_ok());

    guard.stop();
    Ok(())
}

#[test]
fn interrupted_run_resumes_without_duplicate_merges_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let hold = home.path().join("hold-second-slice");
    fs::write(&hold, "hold\n")?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "First resumable slice",
            "goal": "Create first fake output.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["test -f slice-001.txt"]
        }),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-002",
            "title": "Second resumable slice",
            "goal": "Create second fake output after restart.",
            "depends_on": ["slice-001"],
            "acceptance": ["slice-002.txt exists"],
            "verify": [format!("test -f slice-002.txt && if test -f '{}'; then sleep 30; fi", path(&hold))]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add resumable slices"])?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_event(&bin, home.path(), &run_id, "checkpoint_written")?;
    kill_daemon(home.path())?;
    fs::remove_file(&hold)?;

    kd_ok(&bin, home.path(), &["daemon", "start"])?;
    let interrupted = kd_ok(&bin, home.path(), &["status", "--run", &run_id])?;
    assert_eq!(json_stdout(&interrupted)?["run"]["status"], "interrupted");
    kd_ok(
        &bin,
        home.path(),
        &["resume", "--run", &run_id, "--agent", "fake"],
    )?;
    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let branch = completed["run"]["integration_branch"].as_str().unwrap();
    let subjects = git(repo.path(), &["log", "--format=%s", branch])?;
    assert_eq!(
        subjects.matches("khazad(slice:slice-001): merge").count(),
        1
    );
    assert_eq!(
        subjects.matches("khazad(slice:slice-002): merge").count(),
        1
    );

    guard.stop();
    Ok(())
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_khazad-doom"))
}

fn read_daemon_pid(home: &Path) -> TestResult<libc::pid_t> {
    Ok(fs::read_to_string(home.join("daemon.pid"))?
        .trim()
        .parse::<libc::pid_t>()?)
}

struct DaemonGuard {
    bin: PathBuf,
    home: PathBuf,
}

impl DaemonGuard {
    fn new(bin: PathBuf, home: PathBuf) -> Self {
        Self { bin, home }
    }

    fn stop(&self) {
        let _ = kd(&self.bin, &self.home, &["daemon", "stop"]);
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_for_event(bin: &Path, home: &Path, run_id: &str, wanted: &str) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        if value["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"].as_str() == Some(wanted))
        {
            return Ok(value);
        }
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted" | "completed")
        ) {
            panic!("run reached terminal state before event {wanted}: {value:#}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for event {wanted}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_status(bin: &Path, home: &Path, run_id: &str, wanted: &str) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        if value["run"]["status"].as_str() == Some(wanted) {
            return Ok(value);
        }
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted")
        ) {
            panic!("run reached terminal non-success state: {value:#}");
        }
        assert!(Instant::now() < deadline, "timed out waiting for {wanted}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_terminal_status(
    bin: &Path,
    home: &Path,
    run_id: &str,
    wanted: &str,
) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        let status = value["run"]["status"].as_str();
        if status == Some(wanted) {
            return Ok(value);
        }
        if matches!(
            status,
            Some("completed" | "failed" | "blocked" | "cancelled" | "interrupted")
        ) {
            panic!("run reached unexpected terminal state while waiting for {wanted}: {value:#}");
        }
        assert!(Instant::now() < deadline, "timed out waiting for {wanted}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_parallel_progress(
    bin: &Path,
    home: &Path,
    run_id: &str,
    expected: &[&str],
) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        let parallel_slices = value["progress"]["parallel_slices"]
            .as_array()
            .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        if parallel_slices == expected {
            return Ok(value);
        }
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted" | "completed")
        ) {
            panic!("run reached terminal state before parallel progress was visible: {value:#}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for parallel progress {expected:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_latest_run(bin: &Path, home: &Path, repo: &Path, run_id: &str) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--repo", path(repo), "--latest"])?;
        let value = json_stdout(&output)?;
        if value["run"]["id"].as_str() == Some(run_id) {
            return Ok(value);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for latest active run {run_id}: {value:#}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_progress_output(
    bin: &Path,
    home: &Path,
    run_id: &str,
    wanted_phase: &str,
    wanted_output: &str,
) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        if value["progress"]["phase"].as_str() == Some(wanted_phase)
            && value["progress"]["output_tail"]
                .as_str()
                .unwrap_or_default()
                .contains(wanted_output)
        {
            return Ok(value);
        }
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted" | "completed")
        ) {
            panic!(
                "run reached terminal state before progress phase {wanted_phase} with output {wanted_output}: {value:#}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for progress phase {wanted_phase} with output {wanted_output}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_worker_supervision(bin: &Path, home: &Path, run_id: &str) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        if value["progress"]["phase"].as_str() == Some("worker_running")
            && value["progress"]["worker"]["process_observed_at"].is_string()
        {
            return Ok(value);
        }
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted" | "completed")
        ) {
            panic!("run reached terminal state before worker supervision was visible: {value:#}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for worker supervision"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_monitor_text(
    bin: &Path,
    home: &Path,
    run_id: &str,
    wanted: &[&str],
) -> TestResult<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(
            bin,
            home,
            &["monitor", "--run", run_id, "--once", "--interval-ms", "100"],
        )?;
        let text = String::from_utf8(output.stdout)?;
        if wanted.iter().all(|needle| text.contains(needle)) {
            return Ok(text);
        }
        let status = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&status)?;
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted" | "completed")
        ) {
            panic!("run reached terminal state before monitor text was visible: {text}\n{value:#}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for monitor text; last output:\n{text}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn kill_daemon(home: &Path) -> TestResult {
    let pid = fs::read_to_string(home.join("daemon.pid"))?
        .trim()
        .to_string();
    let output = Command::new("kill").arg("-TERM").arg(pid).output()?;
    if !output.status.success() {
        panic!(
            "kill daemon failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    thread::sleep(Duration::from_millis(300));
    Ok(())
}

fn write_fake_gh(dir: &Path) -> TestResult {
    fs::create_dir_all(dir)?;
    let path = dir.join("gh");
    fs::write(
        &path,
        r#"#!/usr/bin/env sh
set -eu
if [ "${1:-}" = "--version" ]; then
  echo "gh fake 0.0"
  exit 0
fi
if [ -n "${FAKE_GH_LOG:-}" ]; then
  echo "$*" >> "$FAKE_GH_LOG"
fi
cat <<'JSON'
{"title":"Add Better Import","body":"Intro paragraph.\n\n- [ ] Do the thing\n- [x] Keep proof","url":"https://github.com/acme/widgets/issues/42","labels":[{"name":"backend"},{"name":"workflow"}]}
JSON
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
    Ok(())
}

fn write_parallel_fail_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import signal
import sys
import time

def emit(result):
    event = {
        "type": "agent_end",
        "messages": [
            {
                "role": "assistant",
                "content": [{"type": "text", "text": json.dumps(result)}],
            }
        ],
    }
    print(json.dumps(event), flush=True)

prompt = sys.stdin.read()
handoff_path = ""
lines = prompt.splitlines()
for index, line in enumerate(lines):
    if line.strip() == "Read this handoff JSON first:" and index + 1 < len(lines):
        handoff_path = lines[index + 1].strip()
        break
if not handoff_path:
    emit({"status": "no-op", "summary": "parallel fake pi: no repair needed"})
    sys.exit(0)
else:
    with open(handoff_path, encoding="utf-8") as fh:
        handoff = json.load(fh)
    slice_id = handoff["slice"]["id"]
    marker_dir = Path(os.environ["KHAZAD_PARALLEL_MARKER"])
    marker_dir.mkdir(parents=True, exist_ok=True)
    (marker_dir / f"{slice_id}.started").write_text("started\n", encoding="utf-8")

    if slice_id == "slice-001":
        time.sleep(4)
        emit({
            "slice_id": slice_id,
            "status": "failed",
            "summary": "intentional parallel worker failure",
        })
        sys.exit(0)

    def terminate(signum, frame):
        (marker_dir / f"{slice_id}.terminated").write_text("terminated\n", encoding="utf-8")
        sys.exit(143)

    signal.signal(signal.SIGTERM, terminate)
    while True:
        time.sleep(0.1)
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

fn write_auth_failure_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env sh
cat >/dev/null
printf 'No API key found for openai.\n' >&2
printf 'Use /login to log into a provider via OAuth or API key.\n' >&2
exit 1
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

fn write_cancellable_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import signal
import sys
import time

_ = sys.stdin.read()
print("partial stdout from cancellable fake pi", flush=True)
print("partial stderr from cancellable fake pi", file=sys.stderr, flush=True)

def terminate(signum, frame):
    print("terminating cancellable fake pi", file=sys.stderr, flush=True)
    sys.exit(143)

signal.signal(signal.SIGTERM, terminate)
while True:
    time.sleep(0.1)
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

fn write_quiet_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time

prompt = sys.stdin.read()
handoff_path = ""
lines = prompt.splitlines()
for index, line in enumerate(lines):
    if line.strip() == "Read this handoff JSON first:" and index + 1 < len(lines):
        handoff_path = lines[index + 1].strip()
        break
if not handoff_path:
    result = {"status": "no-op", "summary": "quiet fake pi: no repair needed"}
    event = {
        "type": "agent_end",
        "messages": [
            {
                "role": "assistant",
                "content": [{"type": "text", "text": json.dumps(result)}],
            }
        ],
    }
    print(json.dumps(event), flush=True)
    sys.exit(0)
with open(handoff_path, encoding="utf-8") as fh:
    handoff = json.load(fh)
slice_id = handoff["slice"]["id"]
time.sleep(float(os.environ.get("FAKE_PI_SLEEP", "4")))
with open(f"{slice_id}.txt", "w", encoding="utf-8") as fh:
    fh.write(f"quiet fake pi implementation for {slice_id}\n")
subprocess.run(["git", "add", "."], check=True)
subprocess.run(
    ["git", "commit", "-m", f"fake pi implement {slice_id}"],
    check=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
)
sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
result = {
    "slice_id": slice_id,
    "status": "complete",
    "summary": "quiet fake pi completed deterministic slice implementation",
    "commit_sha": sha,
    "changed_files": [f"{slice_id}.txt"],
    "tests_run": handoff["slice"].get("verify", []),
    "acceptance_status": [
        {
            "criterion": criterion,
            "status": "satisfied",
            "evidence": f"{slice_id} implemented by quiet fake pi",
        }
        for criterion in handoff["slice"].get("acceptance", [])
    ],
}
event = {
    "type": "agent_end",
    "messages": [
        {
            "role": "assistant",
            "content": [{"type": "text", "text": json.dumps(result)}],
        }
    ],
}
print(json.dumps(event), flush=True)
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

fn prepend_path(dir: &Path) -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    format!("{}:{current}", path(dir))
}

fn write_slice(repo: &Path, value: Value) -> TestResult {
    let id = value["id"].as_str().expect("slice id");
    let dir = repo.join(".workflow/slices");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{id}.json")),
        format!("{}\n", serde_json::to_string_pretty(&value)?),
    )?;
    Ok(())
}

fn init_git_repo(path: &Path) -> TestResult {
    git(path, &["init"])?;
    git(path, &["config", "user.email", "test@example.com"])?;
    git(path, &["config", "user.name", "Test User"])?;
    std::fs::write(path.join("README.md"), "fixture\n")?;
    git(path, &["add", "README.md"])?;
    git(path, &["commit", "-m", "initial"])?;
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if !output.status.success() {
        panic!(
            "git {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn kd_ok(bin: &Path, home: &Path, args: &[&str]) -> TestResult<Output> {
    let output = kd(bin, home, args)?;
    if !output.status.success() {
        panic!(
            "khazad-doom {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

fn kd_ok_with_env(
    bin: &Path,
    home: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> TestResult<Output> {
    let output = kd_with_env(bin, home, extra_env, args)?;
    if !output.status.success() {
        panic!(
            "khazad-doom {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

fn kd(bin: &Path, home: &Path, args: &[&str]) -> TestResult<Output> {
    kd_with_env(bin, home, &[], args)
}

fn kd_with_timeout(
    bin: &Path,
    home: &Path,
    args: &[&str],
    timeout: Duration,
) -> TestResult<Output> {
    let mut child = Command::new(bin)
        .args(args)
        .env("KHAZAD_HOME", home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let output = child.wait_with_output()?;
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "khazad-doom {} did not finish within {:?}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            timeout,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
    .into())
}

fn kd_with_env(
    bin: &Path,
    home: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> TestResult<Output> {
    let mut command = Command::new(bin);
    command.args(args).env("KHAZAD_HOME", home);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    Ok(command.output()?)
}

fn json_stdout(output: &Output) -> TestResult<Value> {
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn artifact_path<'a>(inspection: &'a Value, name: &str) -> TestResult<&'a str> {
    inspection["artifacts"]
        .as_array()
        .and_then(|artifacts| {
            artifacts.iter().find_map(|artifact| {
                (artifact["name"].as_str() == Some(name))
                    .then(|| artifact["path"].as_str())
                    .flatten()
            })
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("artifact {name:?} not found in inspection: {inspection:#}"),
            )
            .into()
        })
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf-8 test path")
}
