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
fn roadmap_truth_lint_rejects_done_status_without_closed_slice_evidence() -> TestResult {
    let repo = tempfile::tempdir()?;
    write_roadmap_fixture(
        repo.path(),
        "| Product Decision | Required Feature | Slice ID | Status |\n|---|---|---|---|\n| D1 | Fixture | FIX-01 | `done` |\n",
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "FIX-01",
            "title": "Fixture",
            "goal": "Stay open.",
            "acceptance": ["done"]
        }),
    )?;

    let output = roadmap_truth_check(repo.path())?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FIX-01"));
    assert!(stderr.contains("not closed"));
    Ok(())
}

#[test]
fn roadmap_truth_lint_accepts_done_status_with_slice_and_run_evidence() -> TestResult {
    let repo = tempfile::tempdir()?;
    write_roadmap_fixture(
        repo.path(),
        "| Product Decision | Required Feature | Slice ID | Status |\n|---|---|---|---|\n| D1 | Fixture | FIX-01 | `done` |\n",
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "FIX-01",
            "title": "Fixture",
            "goal": "Closed with evidence.",
            "status": "closed",
            "closed_by_run": "kd-fixture",
            "closed_at": "2026-07-07T00:00:00Z",
            "acceptance": ["done"]
        }),
    )?;
    fs::create_dir_all(repo.path().join(".workflow/reports"))?;
    fs::write(
        repo.path()
            .join(".workflow/reports/kd-fixture-final-report.json"),
        serde_json::to_string_pretty(&json!({
            "run_id": "kd-fixture",
            "completed_slices": [{"slice_id": "FIX-01", "status": "complete", "summary": "done"}],
            "exit_states": {"run": "completed"}
        }))?,
    )?;

    let output = roadmap_truth_check(repo.path())?;
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

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
            "--cockpit",
            "direct",
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
    assert_eq!(
        handoff["plan_revisions"]["source_of_truth"].as_str(),
        Some("daemon_replan_proposals")
    );
    assert!(handoff["plan_revisions"]["pending"].is_array());
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
    assert_eq!(
        final_report["plan_revisions"]["source_of_truth"].as_str(),
        Some("daemon_replan_proposals")
    );
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
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--cockpit",
            "direct",
            "--all",
        ],
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
fn cockpit_direct_cli_override_beats_durable_herdr_config() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    fs::write(
        repo.path().join(".workflow/khazad.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "agent": "fake",
                "cockpit": "herdr",
                "parallelism": 1
            }))?
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "COCKPIT-DIRECT",
            "title": "Cockpit direct override",
            "goal": "Prove direct cockpit override does not affect fake execution.",
            "acceptance": ["COCKPIT-DIRECT.txt exists"],
            "verify": ["test -f COCKPIT-DIRECT.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add cockpit override slice"])?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let events = completed["events"].as_array().expect("events");
    assert!(events.iter().all(|event| {
        event["type"].as_str() != Some("cockpit_ready")
            && event["payload"]["kind"].as_str() != Some("cockpit_unavailable")
    }));

    guard.stop();
    Ok(())
}

#[test]
fn cockpit_herdr_failure_is_nonfatal_incident() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    write_failing_herdr(fake_bin.path())?;
    let fake_pi = write_quiet_fake_pi(fake_bin.path())?;
    let fake_path = prepend_path(fake_bin.path());
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok_with_env(
        &bin,
        home.path(),
        &[("PATH", fake_path.as_str())],
        &["init", "--repo", path(repo.path())],
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "COCKPIT-FALLBACK",
            "title": "Cockpit fallback",
            "goal": "Complete Pi work even when Herdr cockpit launch fails.",
            "acceptance": ["COCKPIT-FALLBACK.txt exists"],
            "verify": ["test -f COCKPIT-FALLBACK.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add cockpit fallback slice"])?;

    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[("PATH", fake_path.as_str())],
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--pi-bin",
            path(&fake_pi),
            "--cockpit",
            "herdr",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let completed = kd_ok(
        &bin,
        home.path(),
        &["status", "--run", &run_id, "--events-limit", "200"],
    )?;
    let completed = json_stdout(&completed)?;
    assert_eq!(completed["run"]["status"].as_str(), Some("completed"));
    let events = completed["events"].as_array().unwrap();
    assert!(events.iter().any(|event| {
        event["type"].as_str() == Some("run_incident")
            && event["payload"]["kind"].as_str() == Some("cockpit_unavailable")
            && event["payload"]["fallback"].as_str() == Some("direct")
            && event["payload"]["remediation"].as_str().is_some()
    }));
    assert!(events.iter().any(|event| {
        event["type"].as_str() == Some("run_incident")
            && event["payload"]["kind"].as_str() == Some("cockpit_worker_fallback")
            && event["payload"]["fallback"].as_str() == Some("direct")
            && event["payload"]["slice_id"].as_str() == Some("COCKPIT-FALLBACK")
    }));

    guard.stop();
    Ok(())
}

#[test]
fn herdr_cockpit_workspace_real() -> TestResult {
    if std::env::var("HERDR_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping real Herdr cockpit smoke; set HERDR_E2E=1 to enable");
        return Ok(());
    }
    if !herdr_available() {
        eprintln!("skipping real Herdr cockpit smoke; herdr binary is not on PATH");
        return Ok(());
    }

    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "HERDR-SMOKE",
            "title": "Herdr cockpit smoke",
            "goal": "Create fake output while Herdr cockpit panes are opened.",
            "acceptance": ["HERDR-SMOKE.txt exists"],
            "verify": ["test -f HERDR-SMOKE.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add Herdr cockpit smoke slice"],
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
    let workspace_label = format!("Khazad-Doom {run_id}");
    let workspace_id = wait_for_herdr_workspace(&workspace_label)?;
    let _workspace_guard = HerdrWorkspaceGuard::new(workspace_id.clone());
    let panes = herdr_json(&["pane", "list", "--workspace", &workspace_id])?;
    let empty_panes = Vec::new();
    let labels = panes["result"]["panes"]
        .as_array()
        .unwrap_or(&empty_panes)
        .iter()
        .filter_map(|pane| pane["label"].as_str())
        .collect::<Vec<_>>();
    assert!(
        labels.contains(&"Run Status / Event Feed"),
        "Herdr panes should include Run Status / Event Feed, got {labels:?}"
    );
    assert!(
        labels.contains(&"Integration Gate / Repair"),
        "Herdr panes should include Integration Gate / Repair, got {labels:?}"
    );

    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    assert!(completed["events"].as_array().unwrap().iter().any(|event| {
        event["type"].as_str() == Some("cockpit_ready")
            && event["payload"]["workspace"].as_str() == Some(workspace_label.as_str())
    }));

    guard.stop();
    Ok(())
}

#[test]
fn cockpit_cli_does_not_reimplement_herdr_protocol_helpers() -> TestResult {
    let cli = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs"))?;
    for forbidden in [
        "run_herdr",
        "create_herdr",
        "find_herdr_workspace_id",
        "root_pane",
        "pane_id",
    ] {
        assert!(
            !cli.contains(forbidden),
            "src/cli.rs must delegate Herdr protocol details through workflow::cockpit; found {forbidden}"
        );
    }
    Ok(())
}

#[test]
fn herdr_open_focus_reports_fallback_when_unavailable() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    write_failing_herdr(fake_bin.path())?;
    let fake_path = prepend_path(fake_bin.path());
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "HERDR-OPEN-FALLBACK",
            "title": "Herdr open fallback",
            "goal": "Keep daemon status useful when Herdr cannot open.",
            "acceptance": ["HERDR-OPEN-FALLBACK.txt exists"],
            "verify": ["test -f HERDR-OPEN-FALLBACK.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add Herdr open fallback slice"],
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
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let opened = kd_ok_with_env(
        &bin,
        home.path(),
        &[("PATH", fake_path.as_str())],
        &["cockpit", "open", "--run", &run_id],
    )?;
    let opened = json_stdout(&opened)?;
    assert_eq!(opened["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(opened["opened"].as_bool(), Some(false));
    assert_eq!(opened["adapter"].as_str(), Some("herdr"));
    assert!(
        opened["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fake herdr failure")
    );
    assert!(
        opened["fallback"]
            .as_str()
            .unwrap_or_default()
            .contains("status/watch/monitor")
    );
    assert!(
        opened["remediation"]
            .as_str()
            .unwrap_or_default()
            .contains("herdr")
    );
    assert!(
        opened["operator_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                command
                    .as_str()
                    .unwrap_or_default()
                    .contains("khazad-doom monitor --run")
            })
    );

    wait_for_status(&bin, home.path(), &run_id, "completed")?;
    guard.stop();
    Ok(())
}

#[test]
fn herdr_open_focus_real() -> TestResult {
    if std::env::var("HERDR_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping real Herdr open/focus smoke; set HERDR_E2E=1 to enable");
        return Ok(());
    }
    if !herdr_available() {
        eprintln!("skipping real Herdr open/focus smoke; herdr binary is not on PATH");
        return Ok(());
    }

    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "HERDR-OPEN",
            "title": "Herdr open focus",
            "goal": "Open or focus Herdr explicitly after a run exists.",
            "acceptance": ["HERDR-OPEN.txt exists"],
            "verify": ["test -f HERDR-OPEN.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add Herdr open focus slice"])?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "fake",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let opened = kd_ok(&bin, home.path(), &["cockpit", "open", "--run", &run_id])?;
    let opened = json_stdout(&opened)?;
    assert_eq!(opened["opened"].as_bool(), Some(true));
    assert_eq!(opened["action"].as_str(), Some("opened"));
    assert_eq!(
        opened["workspace_label"].as_str(),
        Some(format!("Khazad-Doom {run_id}").as_str())
    );
    let workspace_id = wait_for_herdr_workspace(opened["workspace_label"].as_str().unwrap())?;
    let _workspace_guard = HerdrWorkspaceGuard::new(workspace_id.clone());
    wait_for_herdr_pane_label(&workspace_id, "Run Status / Event Feed")?;
    wait_for_herdr_pane_label(&workspace_id, "Integration Gate / Repair")?;

    wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let focused = kd_ok(
        &bin,
        home.path(),
        &["cockpit", "open", "--latest", "--repo", path(repo.path())],
    )?;
    let focused = json_stdout(&focused)?;
    assert_eq!(focused["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(focused["opened"].as_bool(), Some(true));
    assert_eq!(focused["action"].as_str(), Some("focused_existing"));

    guard.stop();
    Ok(())
}

#[test]
fn herdr_worker_wrapper_real() -> TestResult {
    if std::env::var("HERDR_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping real Herdr worker wrapper smoke; set HERDR_E2E=1 to enable");
        return Ok(());
    }
    if !herdr_available() {
        eprintln!("skipping real Herdr worker wrapper smoke; herdr binary is not on PATH");
        return Ok(());
    }

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
                "worker_termination_grace_seconds": 1
            }))?
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "HERDR-WRAP",
            "title": "Herdr wrapper worker smoke",
            "goal": "Run a Pi-compatible worker through the KD-owned Herdr wrapper.",
            "acceptance": ["HERDR-WRAP.txt exists"],
            "verify": ["test -f HERDR-WRAP.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add Herdr wrapper smoke slice"],
    )?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--pi-bin",
            path(&fake_pi),
            "--cockpit",
            "herdr",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let workspace_label = format!("Khazad-Doom {run_id}");
    let workspace_id = wait_for_herdr_workspace(&workspace_label)?;
    let _workspace_guard = HerdrWorkspaceGuard::new(workspace_id.clone());
    let worker_label = format!("Worker {run_id}/HERDR-WRAP attempt 1");
    wait_for_herdr_pane_label(&workspace_id, &worker_label)?;

    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    assert!(completed["events"].as_array().unwrap().iter().any(|event| {
        event["type"].as_str() == Some("cockpit_worker_ready")
            && event["payload"]["pane"].as_str() == Some(worker_label.as_str())
            && event["payload"]["source_of_truth"].as_str() == Some("kd_artifact_files")
    }));

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    for artifact in [
        "HERDR-WRAP.worker.attempt-1.herdr.stdout.ndjson",
        "HERDR-WRAP.worker.attempt-1.herdr.stderr.log",
        "HERDR-WRAP.worker.attempt-1.herdr.exit.json",
        "HERDR-WRAP.worker.attempt-1.herdr.result.json",
        "HERDR-WRAP.worker.attempt-1.json",
    ] {
        artifact_path(&inspected, artifact)?;
    }

    guard.stop();
    Ok(())
}

#[test]
fn final_sha_advertises_publication_commit_with_close_records() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "PUB-01A",
            "title": "Publication SHA regression",
            "goal": "Create deterministic publication evidence.",
            "depends_on": [],
            "acceptance": ["PUB-01A.txt exists"],
            "verify": ["test -f PUB-01A.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add publication regression slice"],
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
            "--slice",
            "PUB-01A",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_status(&bin, home.path(), &run_id, "completed")?;

    let handoff = kd_ok(&bin, home.path(), &["handoff", "--run", &run_id])?;
    let handoff = json_stdout(&handoff)?;
    let final_sha = handoff["final_sha"].as_str().expect("final sha");
    let integration_branch = handoff["integration_branch"]
        .as_str()
        .expect("integration branch");
    let integration_tip = git(repo.path(), &["rev-parse", integration_branch])?;
    assert_eq!(
        final_sha, integration_tip,
        "regression kd-20260706-215228-0f3bba96 advertised 9a0eb84594c7c26edbcd648c8f807c249bf4ce08 while the integration tip was 1d3f90c544a783e627faa83b25efce7333f967dc"
    );

    let final_report_path = PathBuf::from(
        handoff["final_report_path"]
            .as_str()
            .expect("final report path"),
    );
    let final_report: Value = serde_json::from_str(&fs::read_to_string(final_report_path)?)?;
    assert_eq!(final_report["final_sha"].as_str(), Some(final_sha));
    let summary_path = PathBuf::from(handoff["summary_path"].as_str().expect("summary path"));
    let implementation_summary: Value = serde_json::from_str(&fs::read_to_string(summary_path)?)?;
    assert_eq!(
        implementation_summary["final_sha"].as_str(),
        Some(final_sha)
    );

    let closed_slice_ref = format!("{final_sha}:.workflow/slices/PUB-01A.json");
    let closed_slice: Value =
        serde_json::from_str(&git(repo.path(), &["show", &closed_slice_ref])?)?;
    assert_eq!(closed_slice["status"].as_str(), Some("closed"));
    assert_eq!(
        closed_slice["closed_by_run"].as_str(),
        Some(run_id.as_str())
    );
    git(repo.path(), &["show", &format!("{final_sha}:PUB-01A.txt")])?;
    git(
        repo.path(),
        &[
            "show",
            &format!("{final_sha}:.workflow/reports/{run_id}-final-report.json"),
        ],
    )?;
    git(
        repo.path(),
        &[
            "show",
            &format!("{final_sha}:.workflow/reports/{run_id}-implementation-summary.json"),
        ],
    )?;
    assert!(
        handoff["push_command"]
            .as_str()
            .unwrap()
            .contains(integration_branch)
    );
    assert!(
        handoff["pr_command"]
            .as_str()
            .unwrap()
            .contains(integration_branch)
    );

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
fn replan_status_projection_and_restart_preserve_pending_proposal_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_quiet_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());
    let fake_pi_string = path(&fake_pi).to_string();

    kd_ok_with_env(
        &bin,
        home.path(),
        &[("FAKE_PI_SLEEP", "20")],
        &["init", "--repo", path(repo.path())],
    )?;
    fs::write(
        repo.path().join(".workflow/khazad.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "agent": "pi",
                "parallelism": 1,
                "worker_attempt_timeout_seconds": 0,
                "worker_no_output_warning_seconds": 60,
                "worker_termination_grace_seconds": 1
            }))?
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Replan pending slice",
            "goal": "Stay active long enough to record a replan proposal.",
            "acceptance": ["slice-001.txt exists"],
            "verify": ["test -f slice-001.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add replan fixture slice"])?;

    let started = kd_ok(
        &bin,
        home.path(),
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--pi-bin",
            fake_pi_string.as_str(),
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_worker_supervision(&bin, home.path(), &run_id)?;

    let accepted = kd_ok(
        &bin,
        home.path(),
        &[
            "replan",
            "propose",
            &run_id,
            "--id",
            "rp-accepted",
            "--source-kind",
            "worker",
            "--source-slice",
            "slice-001",
            "--source-phase",
            "worker_running",
            "--source-summary",
            "worker identified an already-covered follow-up",
            "--evidence",
            "worker_output:.workflow/runs/example/outputs/slice-001.worker.json:worker evidence",
            "--change",
            "mark_duplicate:slice-001-followup:proposal is a duplicate",
            "--risk",
            "operator_review",
        ],
    )?;
    assert_eq!(json_stdout(&accepted)?["proposal"]["state"], "pending");
    kd_ok(
        &bin,
        home.path(),
        &[
            "replan",
            "accept",
            &run_id,
            "rp-accepted",
            "--reason",
            "recorded but not applied in v1",
        ],
    )?;

    let pending = kd_ok(
        &bin,
        home.path(),
        &[
            "replan",
            "propose",
            &run_id,
            "--id",
            "rp-pending",
            "--source-kind",
            "worker",
            "--source-slice",
            "slice-001",
            "--source-phase",
            "worker_running",
            "--source-summary",
            "worker needs operator intent before queue mutation",
            "--finding",
            "finding-queue",
            "--evidence",
            "worker_output:.workflow/runs/example/outputs/slice-001.worker.json:worker evidence",
            "--change",
            "add_followup_slice:slice-001-followup:needs an explicit follow-up slice",
            "--risk",
            "intent_affecting",
        ],
    )?;
    assert_eq!(json_stdout(&pending)?["proposal"]["state"], "pending");

    let status = kd_ok(&bin, home.path(), &["status", "--run", &run_id])?;
    let status = json_stdout(&status)?;
    assert_eq!(status["replan"]["pending"][0]["id"], "rp-pending");
    assert_eq!(status["replan"]["history"][0]["id"], "rp-accepted");
    assert_eq!(
        status["replan"]["history"][0]["operator_decision"]["applied"],
        false
    );
    assert!(
        status["replan"]["auto_approvable"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        status["replan"]["pending_attention_reason"]
            .as_str()
            .unwrap()
            .contains("rp-pending")
    );
    let accept_command = format!("khazad-doom replan accept {run_id} rp-pending --reason <reason>");
    assert!(
        status["feed"]["operator_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str() == Some(accept_command.as_str()))
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
    assert!(monitored.contains("Awaiting replan decision rp-pending"));
    assert!(monitored.contains("Replan (1 pending, 1 decided)"));

    kill_daemon(home.path())?;
    kd_ok(&bin, home.path(), &["daemon", "start"])?;
    let restarted = wait_for_terminal_status(&bin, home.path(), &run_id, "interrupted")?;
    assert_eq!(restarted["replan"]["pending"][0]["id"], "rp-pending");

    let resume = kd(&bin, home.path(), &["resume", "--run", &run_id])?;
    assert!(!resume.status.success());
    let stderr = String::from_utf8(resume.stderr)?;
    assert!(stderr.contains("awaiting replan decision for rp-pending before resume"));
    let blocked_resume = kd_ok(&bin, home.path(), &["status", "--run", &run_id])?;
    let blocked_resume = json_stdout(&blocked_resume)?;
    assert_eq!(blocked_resume["run"]["status"], "interrupted");
    assert_eq!(blocked_resume["progress"]["phase"], "awaiting_replan");
    assert_eq!(blocked_resume["replan"]["pending"][0]["id"], "rp-pending");

    guard.stop();
    Ok(())
}

#[test]
fn invalid_worker_output_pi_attempt_is_preserved_and_counted_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_invalid_then_valid_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-001",
            "title": "Retry invalid output",
            "goal": "Retry after an invalid worker JSON attempt.",
            "acceptance": ["invalid output is preserved before retry"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add invalid-output retry slice"],
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

    let completed = wait_for_terminal_status(&bin, home.path(), &run_id, "completed")?;
    let slice_run = completed["slice_runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slice_run| slice_run["slice_id"].as_str() == Some("slice-001"))
        .expect("slice run");
    assert_eq!(slice_run["attempts"], 3);

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let invalid_path = artifact_path(&inspected, "slice-001.worker.attempt-1.invalid-output.json")?;
    let invalid: Value = serde_json::from_str(&fs::read_to_string(invalid_path)?)?;
    assert_eq!(invalid["slice_id"], "slice-001");
    assert_eq!(invalid["attempt"], 1);
    assert!(
        invalid["parse_error"]
            .as_str()
            .unwrap_or_default()
            .contains("parse pi JSON output failed")
    );
    assert!(
        invalid["raw_invalid_payload"]
            .as_str()
            .unwrap_or_default()
            .contains("this is not json from worker")
    );
    assert!(
        invalid["stderr_tail"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid worker stderr tail")
    );
    let invalid_schema_path =
        artifact_path(&inspected, "slice-001.worker.attempt-2.invalid-output.json")?;
    let invalid_schema: Value = serde_json::from_str(&fs::read_to_string(invalid_schema_path)?)?;
    assert!(
        invalid_schema["parse_error"]
            .as_str()
            .unwrap_or_default()
            .contains("worker JSON did not match result model")
    );
    assert!(
        invalid_schema["raw_invalid_payload"]
            .to_string()
            .contains("complete")
    );

    let invalid_event = completed["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"].as_str() == Some("invalid_worker_output"))
        .expect("invalid output event");
    assert_eq!(invalid_event["payload"]["attempt"], 1);
    assert!(
        invalid_event["payload"]["raw_invalid_payload"]
            .as_str()
            .unwrap_or_default()
            .contains("this is not json from worker")
    );

    let run_summary_path = artifact_path(&inspected, "run-summary.json")?;
    let run_summary: Value = serde_json::from_str(&fs::read_to_string(run_summary_path)?)?;
    assert_eq!(run_summary["economics"]["agent_call_count"], 3);

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

    let reason = &blocked["primary_terminal_reason"];
    assert_eq!(reason["kind"], "agent_auth_required");
    assert_eq!(reason["resolution_owner"], "operator");
    assert_eq!(reason["operator_action_required"], true);
    assert_eq!(reason["retryable"], false);
    assert!(
        reason["evidence_links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| { link.as_str().unwrap_or_default().contains("run_incident") })
    );
    assert!(
        reason["operator_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| { command.as_str() == Some("pi /login") })
    );
    assert_eq!(
        blocked["feed"]["terminal_reason"]["kind"],
        "agent_auth_required"
    );
    assert!(
        blocked["feed"]["operator_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| { command.as_str() == Some("pi /login") })
    );
    assert!(
        blocked["feed"]["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| {
                block["label"].as_str() == Some("Terminal")
                    && block["meta"].as_str() == Some("agent_auth_required")
            })
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
    assert!(stdout.contains("Terminal failed"));
    assert!(stderr.contains("run ended with status failed"));

    let failed = kd_ok(&bin, home.path(), &["status", "--run", &run_id])?;
    let failed = json_stdout(&failed)?;
    assert_eq!(failed["primary_terminal_reason"]["kind"], "failed");
    assert_eq!(
        failed["primary_terminal_reason"]["resolution_owner"],
        "daemon"
    );
    assert_eq!(failed["primary_terminal_reason"]["retryable"], true);
    assert_eq!(
        failed["primary_terminal_reason"]["operator_action_required"],
        false
    );
    assert_eq!(failed["feed"]["terminal_reason"]["kind"], "failed");

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

fn herdr_available() -> bool {
    herdr_output(&["--version"], Duration::from_secs(2))
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn herdr_json(args: &[&str]) -> TestResult<Value> {
    let output = herdr_output(args, Duration::from_secs(3))?;
    if !output.status.success() {
        panic!(
            "herdr {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn wait_for_herdr_workspace(label: &str) -> TestResult<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let workspaces = herdr_json(&["workspace", "list"])?;
        if let Some(workspace_id) =
            workspaces["result"]["workspaces"]
                .as_array()
                .and_then(|items| {
                    items.iter().find_map(|workspace| {
                        (workspace["label"].as_str() == Some(label))
                            .then(|| workspace["workspace_id"].as_str())
                            .flatten()
                    })
                })
        {
            return Ok(workspace_id.to_string());
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Herdr workspace {label:?}; workspaces: {workspaces:#}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_herdr_pane_label(workspace_id: &str, label: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let panes = herdr_json(&["pane", "list", "--workspace", workspace_id])?;
        if panes["result"]["panes"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|pane| pane["label"].as_str() == Some(label))
        }) {
            return Ok(());
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Herdr pane {label:?}; panes: {panes:#}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn herdr_output(args: &[&str], timeout: Duration) -> TestResult<Output> {
    let mut child = Command::new("herdr")
        .args(args)
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
            "herdr {} did not finish within {:?}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            timeout,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
    .into())
}

struct HerdrWorkspaceGuard {
    workspace_id: Option<String>,
}

impl HerdrWorkspaceGuard {
    fn new(workspace_id: String) -> Self {
        Self {
            workspace_id: Some(workspace_id),
        }
    }
}

impl Drop for HerdrWorkspaceGuard {
    fn drop(&mut self) {
        if let Some(workspace_id) = self.workspace_id.take() {
            let _ = herdr_output(
                &["workspace", "close", &workspace_id],
                Duration::from_secs(3),
            );
        }
    }
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

fn write_failing_herdr(dir: &Path) -> TestResult {
    fs::create_dir_all(dir)?;
    let path = dir.join("herdr");
    fs::write(
        &path,
        r#"#!/usr/bin/env sh
printf 'fake herdr failure for %s\n' "$*" >&2
exit 42
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
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

fn write_invalid_then_valid_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys

prompt = sys.stdin.read()
handoff_path = ""
lines = prompt.splitlines()
for index, line in enumerate(lines):
    if line.strip() == "Read this handoff JSON first:" and index + 1 < len(lines):
        handoff_path = lines[index + 1].strip()
        break
if not handoff_path:
    event = {
        "type": "agent_end",
        "messages": [{"role": "assistant", "content": [{"type": "text", "text": json.dumps({"status": "no-op", "summary": "no repair"})}]}],
    }
    print(json.dumps(event), flush=True)
    sys.exit(0)
with open(handoff_path, encoding="utf-8") as fh:
    handoff = json.load(fh)
slice_id = handoff["slice"]["id"]
state_path = Path(__file__).with_name(f"{slice_id}.attempt")
attempt = int(state_path.read_text(encoding="utf-8")) + 1 if state_path.exists() else 1
state_path.write_text(str(attempt), encoding="utf-8")
if attempt == 1:
    print("invalid worker stdout tail", flush=True)
    print("invalid worker stderr tail", file=sys.stderr, flush=True)
    event = {
        "type": "agent_end",
        "messages": [{"role": "assistant", "content": [{"type": "text", "text": "this is not json from worker"}]}],
    }
    print(json.dumps(event), flush=True)
    sys.exit(0)
if attempt == 2:
    bad_result = {"slice_id": slice_id, "status": "complete"}
    event = {
        "type": "agent_end",
        "messages": [{"role": "assistant", "content": [{"type": "text", "text": json.dumps(bad_result)}]}],
    }
    print(json.dumps(event), flush=True)
    sys.exit(0)
with open(f"{slice_id}.txt", "w", encoding="utf-8") as fh:
    fh.write(f"valid implementation after invalid output for {slice_id}\n")
subprocess.run(["git", "add", "."], check=True)
subprocess.run(["git", "commit", "-m", f"fake pi implement {slice_id}"], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
result = {
    "slice_id": slice_id,
    "status": "complete",
    "summary": "valid retry after invalid worker output",
    "commit_sha": sha,
    "changed_files": [f"{slice_id}.txt"],
    "tests_run": handoff["slice"].get("verify", []),
    "acceptance_status": [
        {"criterion": criterion, "status": "satisfied", "evidence": "valid retry completed"}
        for criterion in handoff["slice"].get("acceptance", [])
    ],
}
event = {
    "type": "agent_end",
    "messages": [{"role": "assistant", "content": [{"type": "text", "text": json.dumps(result)}]}],
}
print(json.dumps(event), flush=True)
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

fn write_roadmap_fixture(repo: &Path, markdown: &str) -> TestResult {
    let path = repo.join("docs/roadmap/pi-native/00-matrix.md");
    let parent = path.parent().expect("matrix parent");
    fs::create_dir_all(parent)?;
    fs::write(path, markdown)?;
    Ok(())
}

fn roadmap_truth_check(repo: &Path) -> TestResult<Output> {
    Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/roadmap-truth-check"))
        .arg(repo)
        .output()
        .map_err(Into::into)
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
    let effective_args = test_command_args(args);
    let mut child = Command::new(bin)
        .args(&effective_args)
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
    let effective_args = test_command_args(args);
    let mut command = Command::new(bin);
    command.args(&effective_args).env("KHAZAD_HOME", home);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    Ok(command.output()?)
}

fn test_command_args<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    if should_force_direct_cockpit(args) {
        let mut effective = Vec::with_capacity(args.len() + 2);
        effective.push(args[0]);
        effective.push("--cockpit");
        effective.push("direct");
        effective.extend_from_slice(&args[1..]);
        effective
    } else {
        args.to_vec()
    }
}

fn should_force_direct_cockpit(args: &[&str]) -> bool {
    matches!(args.first().copied(), Some("run" | "resume"))
        && !args.contains(&"--cockpit")
        && std::env::var("HERDR_E2E").ok().as_deref() != Some("1")
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
