mod daemon;

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn atomic_json_writer_child_replaces_complete_artifact() -> TestResult {
    let bin = binary_path();
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("run-summary.json");
    fs::write(&path, b"{\"version\":\"old\"}\n")?;
    let mut child = Command::new(&bin)
        .arg("__khazad_atomic_json_write_v1")
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("atomic JSON writer stdin")
        .write_all(b"{\"version\":\"new\"}\n")?;
    let status = child.wait()?;
    assert!(status.success());
    let replaced: Value = serde_json::from_slice(&fs::read(&path)?)?;
    assert_eq!(replaced["version"], "new");
    Ok(())
}

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
fn roadmap_truth_lint_rejects_generated_slice_without_accepted_proposal_decision() -> TestResult {
    let repo = tempfile::tempdir()?;
    write_roadmap_fixture(
        repo.path(),
        "| Product Decision | Required Feature | Slice ID | Status |\n|---|---|---|---|\n| D1 | Fixture | FIX-01 | `planned` |\n",
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "FIX-02",
            "title": "Generated fixture",
            "goal": "Generated without accepted proposal evidence.",
            "acceptance": ["done"],
            "provenance": {
                "parent_slice_id": "FIX-01",
                "origin_proposal_id": "rp-fixture",
                "generation": 1,
                "created_by": "frontier",
                "created_at": "2026-07-07T00:00:00Z"
            }
        }),
    )?;

    let output = roadmap_truth_check(repo.path())?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rp-fixture"), "{stderr}");
    assert!(
        stderr.contains("has no accepted daemon report decision"),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn roadmap_truth_lint_accepts_generated_slice_with_accepted_proposal_decision() -> TestResult {
    let repo = tempfile::tempdir()?;
    write_roadmap_fixture(
        repo.path(),
        "| Product Decision | Required Feature | Slice ID | Status |\n|---|---|---|---|\n| D1 | Fixture | FIX-01 | `planned` |\n",
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "FIX-02",
            "title": "Generated fixture",
            "goal": "Generated with accepted proposal evidence.",
            "acceptance": ["done"],
            "provenance": {
                "parent_slice_id": "FIX-01",
                "origin_proposal_id": "rp-fixture",
                "generation": 1,
                "created_by": "frontier",
                "created_at": "2026-07-07T00:00:00Z"
            }
        }),
    )?;
    fs::create_dir_all(repo.path().join(".workflow/reports"))?;
    fs::write(
        repo.path()
            .join(".workflow/reports/kd-fixture-final-report.json"),
        serde_json::to_string_pretty(&json!({
            "plan_revisions": {
                "accepted": [{
                    "proposal_id": "rp-fixture",
                    "state": "accepted",
                    "decision": {
                        "decision": "accepted",
                        "generated_slice_id": "FIX-02"
                    }
                }]
            }
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
    let slice_one_stem = worker_attempt_output_stem(&status, "slice-001", "slice-worker", 1, 0)?;
    assert!(artifacts.iter().any(|artifact| {
        artifact["kind"] == "handoff"
            && artifact["name"].as_str() == Some(&format!("{slice_one_stem}.json"))
    }));

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
fn profile_and_fake_evidence_are_consistent_everywhere_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-fake-proof",
            "title": "Fake proof slice",
            "goal": "Create deterministic fake output with clear attestation.",
            "acceptance": ["fake output exists"],
            "verify": ["test -f slice-fake-proof.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add fake proof slice"])?;

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
    let run_dir = repo.path().join(".workflow/runs").join(&run_id);
    let preflight: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("outputs/preflight.json"))?)?;
    let worker_stem =
        worker_attempt_output_stem(&status, "slice-fake-proof", "slice-worker", 1, 0)?;
    let handoff_slice: Value = serde_json::from_str(&fs::read_to_string(
        run_dir.join("handoffs").join(format!("{worker_stem}.json")),
    )?)?;
    let final_report: Value = serde_json::from_str(&fs::read_to_string(
        run_dir.join("outputs/final-report.json"),
    )?)?;
    let implementation_summary: Value = serde_json::from_str(&fs::read_to_string(
        run_dir.join("outputs/implementation-summary.json"),
    )?)?;
    let branch_handoff = json_stdout(&kd_ok(&bin, home.path(), &["handoff", "--run", &run_id])?)?;

    let expected_summary =
        "fake: deterministic test-double evidence (not real Pi worker implementation evidence)";
    let expected_kind = "deterministic_test_double_not_real_pi_worker_evidence";
    let expected_label =
        "deterministic test-double evidence; not real Pi worker implementation evidence";
    for surface in [
        &status["worker_profile"],
        &preflight["worker_profile"],
        &handoff_slice["worker_profile"],
        &final_report["worker_profile"],
        &implementation_summary["worker_profile"],
        &branch_handoff["worker_profile"],
    ] {
        assert_eq!(surface["profile_summary"], expected_summary);
        assert_eq!(surface["launch_summary"], expected_summary);
        assert_eq!(surface["worker_evidence_kind"], expected_kind);
        assert_eq!(surface["worker_evidence_label"], expected_label);
    }
    assert_eq!(preflight["worker_evidence_kind"], expected_kind);
    assert_eq!(preflight["worker_evidence_label"], expected_label);
    assert_eq!(
        status["economics"]["agent_calls"][0]["worker_evidence_kind"],
        expected_kind
    );
    assert_eq!(
        status["economics"]["agent_calls"][0]["worker_evidence_label"],
        expected_label
    );
    assert!(
        final_report["evidence_attestation"]["basis"]
            .as_array()
            .unwrap()
            .iter()
            .any(|basis| basis.as_str().unwrap_or_default().contains(expected_kind))
    );

    guard.stop();
    Ok(())
}

#[test]
fn pi_contract_preflight_records_profile_launch_and_contract_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_quiet_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-pi-contract",
            "title": "Pi contract slice",
            "goal": "Exercise Pi launch contract recording.",
            "acceptance": ["pi contract preflight is recorded"],
            "verify": ["test -f slice-pi-contract.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add pi contract slice"])?;

    let fake_pi_string = path(&fake_pi).to_string();
    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[
            ("KHAZAD_PI_BIN", fake_pi_string.as_str()),
            ("FAKE_PI_SLEEP", "0"),
        ],
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
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
    let preflight: Value = serde_json::from_str(&fs::read_to_string(
        repo.path()
            .join(".workflow/runs")
            .join(&run_id)
            .join("outputs/preflight.json"),
    )?)?;
    assert_eq!(preflight["pi_contract"]["binary"], fake_pi_string);
    assert_eq!(preflight["pi_contract"]["supported_contract_version"], 1);
    let flags = preflight["pi_contract"]["launch_flags"]
        .as_array()
        .expect("launch flags")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    for flag in [
        "--provider",
        "--model",
        "--thinking",
        "--mode",
        "json",
        "--no-session",
    ] {
        assert!(
            flags.contains(&flag),
            "missing Pi launch flag {flag}: {flags:?}"
        );
    }
    let profile_summary = preflight["worker_profile"]["profile_summary"]
        .as_str()
        .expect("profile summary");
    assert_eq!(status["worker_profile"]["profile_summary"], profile_summary);
    assert_eq!(preflight["worker_profile"]["agent"], "pi");
    assert_eq!(
        preflight["worker_profile"]["worker_evidence_kind"],
        "real_pi_worker"
    );
    assert_eq!(
        status["economics"]["agent_calls"][0]["worker_evidence_kind"],
        "real_pi_worker"
    );
    assert!(
        status["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["type"].as_str() == Some("run_started"))
            .and_then(|event| event["payload"]["worker_profile"]["profile_summary"].as_str())
            == Some(profile_summary)
    );

    guard.stop();
    Ok(())
}

#[test]
fn ask_operator_answer_timeout_unavailable_and_restart_black_box() -> TestResult {
    let bin = binary_path();
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_operator_question_fake_pi(fake_bin.path())?;
    let fake_pi_string = path(&fake_pi).to_string();

    let home_answer = tempfile::tempdir()?;
    let repo_answer = tempfile::tempdir()?;
    init_git_repo(repo_answer.path())?;
    let guard_answer = DaemonGuard::new(bin.clone(), home_answer.path().to_path_buf());
    kd_ok(
        &bin,
        home_answer.path(),
        &["init", "--repo", path(repo_answer.path())],
    )?;
    write_slice(
        repo_answer.path(),
        json!({
            "id": "slice-ask-answer",
            "title": "Ask answer slice",
            "goal": "Wait for an operator answer and use it.",
            "acceptance": ["operator answer is recorded"],
            "verify": ["grep -q 'operator answer: alpha' slice-ask-answer.txt"]
        }),
    )?;
    git(repo_answer.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo_answer.path(),
        &["commit", "-m", "add ask answer slice"],
    )?;
    let started = kd_ok_with_env(
        &bin,
        home_answer.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &[
            "run",
            "--repo",
            path(repo_answer.path()),
            "--agent",
            "pi",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let answer_run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let awaiting = wait_for_pending_question(&bin, home_answer.path(), &answer_run_id)?;
    let question = awaiting["questions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|question| question["state"].as_str() == Some("pending"))
        .expect("pending question");
    assert_eq!(question["attempt"], 1);
    let question_id = question["id"].as_str().unwrap().to_string();
    daemon::attention::assert_answer_command_is_advertised(&awaiting, &answer_run_id, &question_id);
    let answered = json_stdout(&kd_ok(
        &bin,
        home_answer.path(),
        &["answer", &answer_run_id, &question_id, "alpha"],
    )?)?;
    assert_eq!(answered["question"]["state"], "answered");
    let resumed = json_stdout(&kd_ok(
        &bin,
        home_answer.path(),
        &["status", "--run", &answer_run_id],
    )?)?;
    assert_eq!(resumed["progress"]["phase"], "worker_running");
    assert!(
        resumed["progress"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("worker resuming")
    );
    let completed = wait_for_status(&bin, home_answer.path(), &answer_run_id, "completed")?;
    assert_eq!(completed["questions"][0]["answer"], "alpha");
    guard_answer.stop();

    let home_timeout = tempfile::tempdir()?;
    let repo_timeout = tempfile::tempdir()?;
    init_git_repo(repo_timeout.path())?;
    let guard_timeout = DaemonGuard::new(bin.clone(), home_timeout.path().to_path_buf());
    kd_ok(
        &bin,
        home_timeout.path(),
        &["init", "--repo", path(repo_timeout.path())],
    )?;
    fs::write(
        repo_timeout.path().join(".workflow/khazad.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "agent": "pi",
                "parallelism": 1,
                "worker_question_timeout_seconds": 1
            }))?
        ),
    )?;
    write_slice(
        repo_timeout.path(),
        json!({
            "id": "slice-ask-timeout",
            "title": "Ask timeout slice",
            "goal": "Block when the operator does not answer.",
            "acceptance": ["timeout becomes a blocked ask-user finding"]
        }),
    )?;
    git(repo_timeout.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo_timeout.path(),
        &["commit", "-m", "add ask timeout slice"],
    )?;
    let started = kd_ok_with_env(
        &bin,
        home_timeout.path(),
        &[
            ("KHAZAD_PI_BIN", fake_pi_string.as_str()),
            ("KHAZAD_FAKE_PI_OPERATOR_TIMEOUT", "1"),
        ],
        &[
            "run",
            "--repo",
            path(repo_timeout.path()),
            "--agent",
            "pi",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let timeout_run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let blocked = wait_for_terminal_status(&bin, home_timeout.path(), &timeout_run_id, "blocked")?;
    assert_eq!(blocked["questions"][0]["state"], "timed_out");
    assert!(
        blocked["run"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ask_operator timed out")
    );
    guard_timeout.stop();

    let home_unavailable = tempfile::tempdir()?;
    let repo_unavailable = tempfile::tempdir()?;
    init_git_repo(repo_unavailable.path())?;
    let guard_unavailable = DaemonGuard::new(bin.clone(), home_unavailable.path().to_path_buf());
    kd_ok(
        &bin,
        home_unavailable.path(),
        &["init", "--repo", path(repo_unavailable.path())],
    )?;
    write_slice(
        repo_unavailable.path(),
        json!({
            "id": "slice-ask-unavailable",
            "title": "Ask unavailable slice",
            "goal": "Block instead of inventing intent when ask_operator is unavailable.",
            "acceptance": ["unavailable ask_operator becomes a blocked ask-user finding"]
        }),
    )?;
    git(repo_unavailable.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo_unavailable.path(),
        &["commit", "-m", "add ask unavailable slice"],
    )?;
    let started = kd_ok_with_env(
        &bin,
        home_unavailable.path(),
        &[
            ("KHAZAD_PI_BIN", fake_pi_string.as_str()),
            ("KHAZAD_FAKE_PI_OPERATOR_MODE", "unavailable"),
        ],
        &[
            "run",
            "--repo",
            path(repo_unavailable.path()),
            "--agent",
            "pi",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let unavailable_run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let unavailable = wait_for_terminal_status(
        &bin,
        home_unavailable.path(),
        &unavailable_run_id,
        "blocked",
    )?;
    assert!(
        unavailable["questions"]
            .as_array()
            .map(|questions| questions.is_empty())
            .unwrap_or(true)
    );
    assert!(
        unavailable["run"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ask_operator unavailable")
    );
    guard_unavailable.stop();

    let home_restart = tempfile::tempdir()?;
    let repo_restart = tempfile::tempdir()?;
    init_git_repo(repo_restart.path())?;
    let guard_restart = DaemonGuard::new(bin.clone(), home_restart.path().to_path_buf());
    kd_ok(
        &bin,
        home_restart.path(),
        &["init", "--repo", path(repo_restart.path())],
    )?;
    write_slice(
        repo_restart.path(),
        json!({
            "id": "slice-ask-restart",
            "title": "Ask restart slice",
            "goal": "Recover an interrupted pending operator question safely.",
            "acceptance": ["fresh operator answer completes after restart"],
            "verify": ["grep -q 'operator answer: bravo' slice-ask-restart.txt"]
        }),
    )?;
    git(repo_restart.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo_restart.path(),
        &["commit", "-m", "add ask restart slice"],
    )?;
    let started = kd_ok_with_env(
        &bin,
        home_restart.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &[
            "run",
            "--repo",
            path(repo_restart.path()),
            "--agent",
            "pi",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let restart_run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let awaiting = wait_for_pending_question(&bin, home_restart.path(), &restart_run_id)?;
    let old_question_id = awaiting["questions"][0]["id"].as_str().unwrap().to_string();
    kill_daemon(home_restart.path())?;
    kd_ok(&bin, home_restart.path(), &["daemon", "start"])?;
    let interrupted =
        wait_for_terminal_status(&bin, home_restart.path(), &restart_run_id, "interrupted")?;
    assert_eq!(interrupted["progress"]["phase"], "interrupted");
    assert_eq!(interrupted["questions"][0]["state"], "interrupted");
    let stale_answer = kd(
        &bin,
        home_restart.path(),
        &["answer", &restart_run_id, &old_question_id, "alpha"],
    )?;
    assert!(!stale_answer.status.success());
    assert_eq!(json_stdout(&stale_answer)?["outcome"], "conflict");
    assert!(
        String::from_utf8(stale_answer.stderr)?
            .contains("answer question command returned conflict")
    );
    kd_ok_with_env(
        &bin,
        home_restart.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &[
            "resume",
            "--run",
            &restart_run_id,
            "--agent",
            "pi",
            "--pi-bin",
            fake_pi_string.as_str(),
        ],
    )?;
    let awaiting_fresh = wait_for_pending_question(&bin, home_restart.path(), &restart_run_id)?;
    let fresh_question = awaiting_fresh["questions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|question| question["state"].as_str() == Some("pending"))
        .expect("fresh pending question");
    let fresh_question_id = fresh_question["id"].as_str().unwrap().to_string();
    assert_ne!(fresh_question_id, old_question_id);
    kd_ok(
        &bin,
        home_restart.path(),
        &["answer", &restart_run_id, &fresh_question_id, "bravo"],
    )?;
    wait_for_status(&bin, home_restart.path(), &restart_run_id, "completed")?;
    guard_restart.stop();

    Ok(())
}

#[test]
fn concurrent_question_socket_rpcs_commit_one_fallback_or_legacy_timeout_winner() -> TestResult {
    let bin = binary_path();
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_operator_question_fake_pi(fake_bin.path())?;
    let fake_pi_string = path(&fake_pi).to_string();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());
    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-ask-same-pane-rpc-race",
            "title": "Question RPC race",
            "goal": "Exercise concurrent answer and timeout RPCs.",
            "acceptance": ["one durable question resolution wins"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add question RPC race slice"],
    )?;

    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[
            ("KHAZAD_PI_BIN", fake_pi_string.as_str()),
            ("KHAZAD_FAKE_PI_OPERATOR_MODE", "same_pane"),
        ],
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run id")
        .to_string();
    let awaiting = wait_for_pending_question(&bin, home.path(), &run_id)?;
    let first_question = awaiting["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["state"] == "pending")
        })
        .expect("eligible pending question");
    let first_question_id = first_question["id"]
        .as_str()
        .expect("question id")
        .to_string();
    let first_launch_id = first_question["launch_id"]
        .as_i64()
        .expect("launch-scoped question");
    let first_token = live_worker_token(home.path(), &awaiting, first_launch_id)?;
    force_question_deadline(home.path(), &first_question_id, true)?;
    let first_outcomes = race_question_socket_rpcs(
        home.path(),
        &run_id,
        &first_question_id,
        first_launch_id,
        &first_token,
    )?;
    assert_one_applied_one_conflict(&first_outcomes);
    let first_status = json_stdout(&kd_ok(&bin, home.path(), &["status", "--run", &run_id])?)?;
    let first_winner = first_status["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["id"] == first_question_id)
        })
        .expect("eligible durable winner");
    assert_eq!(first_winner["state"], "answered");
    assert!(matches!(
        first_winner["answer_source"].as_str(),
        Some("operator" | "llm_recommendation_timeout")
    ));
    assert_eq!(
        question_resolution_event_count(&first_status, &first_question_id),
        1
    );

    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &run_id,
            "--reason",
            "prepare legacy timeout RPC race",
        ],
    )?;
    wait_for_terminal_status(&bin, home.path(), &run_id, "cancelled")?;
    kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_FAKE_PI_OPERATOR_MODE", "same_pane")],
        &[
            "resume",
            "--run",
            &run_id,
            "--agent",
            "pi",
            "--pi-bin",
            fake_pi_string.as_str(),
        ],
    )?;
    let awaiting = wait_for_pending_question(&bin, home.path(), &run_id)?;
    let second_question = awaiting["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["state"] == "pending")
        })
        .expect("legacy pending question");
    let second_question_id = second_question["id"]
        .as_str()
        .expect("question id")
        .to_string();
    assert_ne!(second_question_id, first_question_id);
    let second_launch_id = second_question["launch_id"]
        .as_i64()
        .expect("launch-scoped question");
    let second_token = live_worker_token(home.path(), &awaiting, second_launch_id)?;
    force_question_deadline(home.path(), &second_question_id, false)?;
    let second_outcomes = race_question_socket_rpcs(
        home.path(),
        &run_id,
        &second_question_id,
        second_launch_id,
        &second_token,
    )?;
    assert_one_applied_one_conflict(&second_outcomes);
    let second_status = json_stdout(&kd_ok(&bin, home.path(), &["status", "--run", &run_id])?)?;
    let second_winner = second_status["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["id"] == second_question_id)
        })
        .expect("legacy durable winner");
    assert!(matches!(
        second_winner["state"].as_str(),
        Some("answered" | "timed_out")
    ));
    assert_eq!(
        question_resolution_event_count(&second_status, &second_question_id),
        1
    );

    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &run_id,
            "--reason",
            "question RPC races complete",
        ],
    )?;
    wait_for_terminal_status(&bin, home.path(), &run_id, "cancelled")?;
    guard.stop();
    Ok(())
}

#[test]
fn ask_operator_terminal_same_pane_question_cannot_revive_on_resume() -> TestResult {
    let bin = binary_path();
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_operator_question_fake_pi(fake_bin.path())?;
    let fake_pi_string = path(&fake_pi).to_string();
    let resumed_fake_bin = tempfile::tempdir()?;
    let resumed_fake_pi = write_quiet_fake_pi(resumed_fake_bin.path())?;
    let resumed_fake_pi_string = path(&resumed_fake_pi).to_string();
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
                "agent": "pi",
                "parallelism": 1,
                "worker_attempt_timeout_seconds": 0,
                "worker_question_timeout_seconds": 3
            }))?
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-ask-terminal-resume",
            "title": "Terminal same-pane question",
            "goal": "Never revive a question from a terminal worker attempt.",
            "acceptance": ["old question stays interrupted across resume"],
            "verify": ["test -f slice-ask-terminal-resume.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add terminal same-pane question slice"],
    )?;

    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[
            ("KHAZAD_PI_BIN", fake_pi_string.as_str()),
            ("KHAZAD_FAKE_PI_OPERATOR_MODE", "same_pane"),
        ],
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--cockpit",
            "direct",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run id")
        .to_string();
    let awaiting = wait_for_pending_question(&bin, home.path(), &run_id)?;
    let old_question = awaiting["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["state"] == "pending")
        })
        .expect("same-pane pending question");
    let old_question_id = old_question["id"]
        .as_str()
        .expect("old question id")
        .to_string();
    assert_eq!(old_question["attempt"], 1);

    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &run_id,
            "--reason",
            "cancel same-pane question",
        ],
    )?;
    let cancelled = wait_for_terminal_status(&bin, home.path(), &run_id, "cancelled")?;
    assert_eq!(cancelled["progress"]["phase"], "cancelled");
    let old_question = cancelled["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["id"] == old_question_id)
        })
        .expect("durable old question");
    assert_eq!(old_question["state"], "interrupted");
    assert!(old_question["answer_source"].is_null());

    let summary_path = repo
        .path()
        .join(".workflow/runs")
        .join(&run_id)
        .join("outputs/run-summary.json");
    let summary = wait_for_json_file(&summary_path)?;
    assert_eq!(summary["progress"]["phase"], "cancelled");
    assert_eq!(summary["questions"][0]["state"], "interrupted");

    // A first resume creates a distinct, still-interrupted launch; the second
    // resume must retain both abandoned launch identities and their handoffs.
    kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_FAKE_PI_OPERATOR_MODE", "same_pane")],
        &[
            "resume",
            "--run",
            &run_id,
            "--agent",
            "pi",
            "--pi-bin",
            fake_pi_string.as_str(),
        ],
    )?;
    let second_awaiting = wait_for_pending_question(&bin, home.path(), &run_id)?;
    let second_question_id = second_awaiting["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["state"] == "pending")
        })
        .and_then(|question| question["id"].as_str())
        .expect("second resume pending question")
        .to_string();
    assert_ne!(second_question_id, old_question_id);
    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &run_id,
            "--reason",
            "cancel second same-pane question",
        ],
    )?;
    wait_for_terminal_status(&bin, home.path(), &run_id, "cancelled")?;

    kd_ok(
        &bin,
        home.path(),
        &[
            "resume",
            "--run",
            &run_id,
            "--agent",
            "pi",
            "--pi-bin",
            resumed_fake_pi_string.as_str(),
        ],
    )?;
    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    let old_question = completed["questions"]
        .as_array()
        .and_then(|questions| {
            questions
                .iter()
                .find(|question| question["id"] == old_question_id)
        })
        .expect("old question after resume");
    assert_eq!(old_question["attempt"], 1);
    assert_eq!(old_question["state"], "interrupted");
    assert!(old_question["answer_source"].is_null());
    assert!(completed["events"].as_array().is_some_and(|events| {
        events.iter().all(|event| {
            event["type"] != "worker_question_answered"
                || event["payload"]["question_id"] != old_question_id
        })
    }));
    let attempts = completed["worker_attempts"]
        .as_array()
        .expect("public immutable worker attempt history");
    assert!(attempts.len() >= 3, "initial launch plus two resumes");
    let launch_ids: std::collections::BTreeSet<_> = attempts
        .iter()
        .filter_map(|attempt| attempt["launch_id"].as_i64())
        .collect();
    let stems: std::collections::BTreeSet<_> = attempts
        .iter()
        .filter_map(|attempt| attempt["output_stem"].as_str())
        .collect();
    let epochs: std::collections::BTreeSet<_> = attempts
        .iter()
        .filter_map(|attempt| attempt["execution_epoch"].as_u64())
        .collect();
    assert_eq!(launch_ids.len(), attempts.len());
    assert_eq!(stems.len(), attempts.len());
    assert!(epochs.is_superset(&std::collections::BTreeSet::from([1, 2, 3])));
    let run_dir = repo.path().join(".workflow/runs").join(&run_id);
    for stem in stems {
        assert!(
            run_dir
                .join("handoffs")
                .join(format!("{stem}.json"))
                .exists(),
            "preserved handoff for {stem}"
        );
    }

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
    daemon::cockpit::assert_herdr_failure_falls_back_to_direct(&completed, "COCKPIT-FALLBACK");

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
    let launch_deadline = Instant::now() + Duration::from_secs(10);
    let launch_id = loop {
        let status = json_stdout(&kd_ok(&bin, home.path(), &["status", "--run", &run_id])?)?;
        if let Some(launch_id) = status["worker_attempts"].as_array().and_then(|attempts| {
            attempts.iter().find_map(|attempt| {
                (attempt["slice_id"].as_str() == Some("HERDR-WRAP"))
                    .then(|| attempt["launch_id"].as_i64())
                    .flatten()
            })
        }) {
            break launch_id;
        }
        assert!(
            Instant::now() < launch_deadline,
            "worker launch identity was not published"
        );
        thread::sleep(Duration::from_millis(50));
    };
    let worker_label = format!("Worker {run_id}/HERDR-WRAP attempt 1 launch {launch_id}");
    wait_for_herdr_pane_label(&workspace_id, &worker_label)?;

    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    assert!(completed["events"].as_array().unwrap().iter().any(|event| {
        event["type"].as_str() == Some("cockpit_worker_ready")
            && event["payload"]["pane"].as_str() == Some(worker_label.as_str())
            && event["payload"]["launch_id"].as_i64() == Some(launch_id)
            && event["payload"]["source_of_truth"].as_str() == Some("kd_artifact_files")
    }));

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let worker_stem = worker_attempt_output_stem(&completed, "HERDR-WRAP", "slice-worker", 1, 0)?;
    for suffix in [
        ".herdr.stdout.ndjson",
        ".herdr.stderr.log",
        ".herdr.exit.json",
        ".herdr.result.json",
        ".json",
    ] {
        artifact_path(&inspected, &format!("{worker_stem}{suffix}"))?;
    }

    guard.stop();
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn publication_ref_hook_descendants_are_reaped_before_completion() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());
    let marker = repo.path().join("escaped-publication-hook");
    let hook = repo.path().join(".git/hooks/reference-transaction");
    fs::write(
        &hook,
        format!(
            r#"#!/bin/sh
if [ "${{KHAZAD_PUBLICATION_REF_TRANSACTION:-}}" != 1 ] || [ "$1" != committed ]; then
    exit 0
fi
target=
while read old new ref; do
    target=$new
done
setsid sh -c '
    ( sleep 1
      printf escaped > "$1"
      env -u GIT_DIR -u GIT_COMMON_DIR -u GIT_WORK_TREE -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES \
        git -C "$2" update-ref refs/heads/hook-late "$3"
    ) </dev/null >/dev/null 2>&1 &
' sh '{}' '{}' "$target" </dev/null >/dev/null 2>&1 &
exit 0
"#,
            marker.display(),
            repo.path().display()
        ),
    )?;
    let mut mode = fs::metadata(&hook)?.permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&hook, mode)?;

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "HOOK-PUB-01",
            "title": "Publication hook supervision",
            "goal": "Publish without escaped hook descendants.",
            "acceptance": ["fake output exists"],
            "verify": ["test -f HOOK-PUB-01.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add hook publication slice"])?;

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
        .expect("run id")
        .to_string();
    wait_for_status(&bin, home.path(), &run_id, "completed")?;
    thread::sleep(Duration::from_millis(1_500));

    assert!(
        !marker.exists(),
        "detached publication hook descendant survived supervision"
    );
    let hook_ref = Command::new("git")
        .args(["show-ref", "--verify", "refs/heads/hook-late"])
        .current_dir(repo.path())
        .output()?;
    assert!(
        !hook_ref.status.success(),
        "detached publication hook changed a ref after publication"
    );
    guard.stop();
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn publication_ref_hook_worktree_and_config_side_effects_are_restored() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());
    let config_path = repo.path().join(".git/config");
    let hook = repo.path().join(".git/hooks/reference-transaction");
    fs::write(
        &hook,
        format!(
            r#"#!/bin/sh
if [ "${{KHAZAD_PUBLICATION_REF_TRANSACTION:-}}" = 1 ] && [ "$1" = committed ]; then
    printf hook-side-effect > "$GIT_WORK_TREE/hook-side-effect"
    printf '\n[hook-side-effect]\n\tvalue = changed\n' >> '{}'
fi
exit 0
"#,
            config_path.display()
        ),
    )?;
    let mut mode = fs::metadata(&hook)?.permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&hook, mode)?;

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "HOOK-PUB-02",
            "title": "Publication hook restoration",
            "goal": "Reject and restore publication hook mutations.",
            "acceptance": ["fake output exists"],
            "verify": ["test -f HOOK-PUB-02.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add hook restoration slice"])?;
    let config_before_run = fs::read(&config_path)?;

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
        .expect("run id")
        .to_string();
    let failed = wait_for_status(&bin, home.path(), &run_id, "failed")?;

    assert_eq!(fs::read(&config_path)?, config_before_run);
    assert!(
        failed["events"].as_array().unwrap().iter().any(|event| {
            event
                .to_string()
                .contains("ref hook changed worktree or local configuration")
        }),
        "failed run omitted publication-hook mutation evidence: {failed:#}"
    );
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
    daemon::publication::assert_handoff_targets_integration_branch(&handoff, integration_branch);

    fs::write(
        repo.path().join("operator-after-publication.txt"),
        "advanced\n",
    )?;
    git(repo.path(), &["add", "operator-after-publication.txt"])?;
    let advanced_tree = git(repo.path(), &["write-tree"])?;
    let advanced = git(
        repo.path(),
        &[
            "commit-tree",
            &advanced_tree,
            "-p",
            final_sha,
            "-m",
            "advance after publication",
        ],
    )?;
    git(repo.path(), &["reset", "--hard", "HEAD"])?;
    git(
        repo.path(),
        &[
            "update-ref",
            &format!("refs/heads/{integration_branch}"),
            &advanced,
            final_sha,
        ],
    )?;
    let rejected = kd(&bin, home.path(), &["handoff", "--run", &run_id])?;
    assert!(
        !rejected.status.success(),
        "advanced handoff unexpectedly succeeded"
    );
    let rejection = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        rejection.contains("moved from recorded completion publication")
            || rejection.contains("advanced beyond completion publication"),
        "unexpected advanced-handoff rejection: {rejection}"
    );

    guard.stop();
    Ok(())
}

#[test]
fn completion_publication_manifest_excludes_filter_side_effects() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "PUB-MANIFEST-01",
            "title": "Explicit publication manifest regression",
            "goal": "Publish only daemon-owned completion artifacts.",
            "depends_on": [],
            "acceptance": ["PUB-MANIFEST-01.txt exists"],
            "verify": ["test -f PUB-MANIFEST-01.txt"]
        }),
    )?;
    fs::write(repo.path().join("unrelated-tracked.txt"), "baseline\n")?;
    fs::write(
        repo.path().join(".gitattributes"),
        ".workflow/reports/*.json filter=publication-inject\n",
    )?;
    let filter = repo.path().join("publication-filter.sh");
    fs::write(
        &filter,
        concat!(
            "#!/bin/sh\n",
            "cat\n",
            "printf 'operator edit\\n' > unrelated-tracked.txt\n",
            "printf 'operator scratch\\n' > unrelated-untracked.txt\n"
        ),
    )?;
    git(
        repo.path(),
        &[
            "config",
            "filter.publication-inject.clean",
            &format!("sh {}", path(&filter)),
        ],
    )?;
    git(
        repo.path(),
        &["config", "filter.publication-inject.smudge", "cat"],
    )?;
    git(repo.path(), &["add", "."])?;
    git(
        repo.path(),
        &["commit", "-m", "add publication manifest regression"],
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
            "PUB-MANIFEST-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let completed = wait_for_status(&bin, home.path(), &run_id, "completed")?;
    assert_eq!(
        fs::read(repo.path().join("unrelated-tracked.txt"))?,
        b"baseline\n",
        "publication clean filters must not mutate unrelated tracked bytes"
    );
    assert!(
        !repo.path().join("unrelated-untracked.txt").exists(),
        "publication clean filters must not create unrelated files"
    );
    let handoff = json_stdout(&kd_ok(&bin, home.path(), &["handoff", "--run", &run_id])?)?;
    let final_sha = handoff["final_sha"].as_str().expect("final sha");
    assert_eq!(
        git(
            repo.path(),
            &["show", &format!("{final_sha}:unrelated-tracked.txt")],
        )?,
        "baseline"
    );
    assert!(
        git(
            repo.path(),
            &[
                "ls-tree",
                "--name-only",
                final_sha,
                "unrelated-untracked.txt",
            ],
        )?
        .is_empty()
    );
    let receipt = completed["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "completion_publication_committed")
        .expect("publication receipt event");
    let staged = receipt["payload"]["staged_path_bytes_hex"]
        .as_array()
        .expect("staged path receipt");
    for expected in [
        ".workflow/slices/PUB-MANIFEST-01.json".to_string(),
        format!(".workflow/reports/{run_id}-implementation-summary.json"),
        format!(".workflow/reports/{run_id}-final-report.json"),
    ] {
        assert!(
            staged.contains(&Value::String(hex::encode(expected.as_bytes()))),
            "publication receipt omitted {expected}: {receipt:#}"
        );
    }
    assert_eq!(staged.len(), 3);

    guard.stop();
    Ok(())
}

#[test]
fn verification_mutation_blocks_without_repair_publication_or_final_sha() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-01",
            "title": "Verification purity regression",
            "goal": "Block verification side effects before merge or publication.",
            "depends_on": [],
            "acceptance": ["PURITY-01.txt exists"],
            "verify": ["printf 'verification side effect\\n' > verification-side-effect.txt && test -f PURITY-01.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add purity regression slice"],
    )?;
    let base_sha = git(repo.path(), &["rev-parse", "HEAD"])?;

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
            "PURITY-01",
        ],
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
            .contains("verification command changed the worktree"),
        "unexpected blocked reason: {blocked:#}"
    );
    assert_eq!(blocked["slice_runs"][0]["attempts"], 1);
    assert_eq!(blocked["economics"]["agent_call_count"], 1);
    let integration_branch = blocked["run"]["integration_branch"]
        .as_str()
        .expect("integration branch");
    assert_eq!(
        git(repo.path(), &["rev-parse", integration_branch])?,
        base_sha
    );
    assert!(
        git(
            repo.path(),
            &[
                "ls-tree",
                "--name-only",
                integration_branch,
                &format!(".workflow/reports/{run_id}-final-report.json"),
            ],
        )?
        .is_empty()
    );
    assert!(
        blocked["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| { event["type"].as_str() != Some("completion_publication_committed") })
    );

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let worker_stem = worker_attempt_output_stem(&blocked, "PURITY-01", "slice-worker", 1, 0)?;
    let check_path = artifact_path(&inspected, &format!("{worker_stem}.check.json"))?;
    let check: Value = serde_json::from_str(&fs::read_to_string(check_path)?)?;
    assert_eq!(check["failure_kind"], "verification_mutated_worktree");
    assert_eq!(
        check["verification_commands"][0]["verification_workspace"]["restored"]["digest"],
        check["verification_commands"][0]["verification_workspace"]["before"]["digest"]
    );
    assert!(
        check["verification_commands"][0]["verification_workspace"]["after"]
            ["untracked_path_bytes_hex"]
            .as_array()
            .unwrap()
            .contains(&Value::String(hex::encode("verification-side-effect.txt")))
    );

    guard.stop();
    Ok(())
}

#[test]
fn detached_environment_scrubbed_verification_descendant_is_reaped() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let marker_dir = tempfile::tempdir()?;
    let marker = marker_dir.path().join("escaped-descendant");
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    let verify = format!(
        "env -i PATH=\"$PATH\" setsid sh -c '(for fd in /proc/$$/fd/*; do n=${{fd##*/}}; [ \"$n\" -le 2 ] || eval \"exec $n>&-\"; done; sleep 0.3; printf escaped > {}) &' >/dev/null 2>&1 & test -f PURITY-SUPERVISOR-01.txt",
        marker.display()
    );
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-SUPERVISOR-01",
            "title": "Detached verification descendant containment",
            "goal": "Contain verification descendants that scrub inherited markers and sessions.",
            "depends_on": [],
            "acceptance": ["PURITY-SUPERVISOR-01.txt exists"],
            "verify": [verify]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add descendant containment regression"],
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
            "PURITY-SUPERVISOR-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_terminal_status(&bin, home.path(), &run_id, "completed")?;
    thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "detached environment-scrubbed verification descendant survived supervision"
    );

    guard.stop();
    Ok(())
}

#[test]
fn verification_descendant_cannot_kill_its_subreaper_and_escape() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let marker_dir = tempfile::tempdir()?;
    let marker = marker_dir.path().join("killed-sub-reaper");
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    let verify = format!(
        "setsid sh -c 'for fd in /proc/$$/fd/*; do n=${{fd##*/}}; [ \"$n\" -le 2 ] || eval \"exec $n>&-\"; done; sleep 0.3; printf escaped > {}' >/dev/null 2>&1 & kill -KILL \"$PPID\" 2>/dev/null || true; test -f PURITY-SUPERVISOR-KILL-01.txt",
        marker.display()
    );
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-SUPERVISOR-KILL-01",
            "title": "Verification subreaper signal confinement",
            "goal": "Prevent a supervised command from killing its own containment boundary.",
            "depends_on": [],
            "acceptance": ["PURITY-SUPERVISOR-KILL-01.txt exists"],
            "verify": [verify]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add subreaper signal regression"],
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
            "PURITY-SUPERVISOR-KILL-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_terminal_status(&bin, home.path(), &run_id, "completed")?;
    thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "verification descendant killed the subreaper and escaped containment"
    );

    guard.stop();
    Ok(())
}

#[test]
fn hidden_supervisor_reports_internal_failure_on_private_result_channel() -> TestResult {
    const RESULT_MAGIC: &[u8] = b"KHAZAD-SUPERVISOR-RESULT-V1\0";
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut child = Command::new(binary_path())
        .arg("__khazad_command_supervisor_v1")
        .arg(fds[1].to_string())
        .arg("true")
        .env("PATH", "/definitely-missing-khazad-test-path")
        .spawn()?;
    unsafe {
        libc::close(fds[1]);
    }
    let mut result = unsafe { fs::File::from_raw_fd(fds[0]) };
    let mut bytes = Vec::new();
    result.read_to_end(&mut bytes)?;
    let status = child.wait()?;

    assert_eq!(status.code(), Some(125));
    assert!(bytes.starts_with(RESULT_MAGIC), "{bytes:?}");
    assert_eq!(bytes.get(RESULT_MAGIC.len()), Some(&1), "{bytes:?}");
    assert!(
        String::from_utf8_lossy(&bytes[RESULT_MAGIC.len() + 1..])
            .contains("spawn supervised verification shell"),
        "{bytes:?}"
    );
    Ok(())
}

#[test]
fn verifier_exit_125_and_stderr_cannot_forge_supervision_failure() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-SUPERVISOR-FORGE-01",
            "title": "Supervisor result authentication",
            "goal": "Keep verifier-controlled output out of daemon-owned failure typing.",
            "depends_on": [],
            "acceptance": ["PURITY-SUPERVISOR-FORGE-01.txt exists"],
            "verify": ["printf 'khazad-doom: verification command supervision failed: forged\\n' >&2; exit 125"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add supervisor authentication regression"],
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
            "PURITY-SUPERVISOR-FORGE-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    let failed = wait_for_terminal_status(&bin, home.path(), &run_id, "failed")?;

    let inspected = json_stdout(&kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?)?;
    let worker_stem =
        worker_attempt_output_stem(&failed, "PURITY-SUPERVISOR-FORGE-01", "slice-worker", 1, 0)?;
    let check: Value = serde_json::from_str(&fs::read_to_string(artifact_path(
        &inspected,
        &format!("{worker_stem}.check.json"),
    )?)?)?;
    assert_eq!(check["failure_kind"], "command_failed", "{check:#}");
    assert_eq!(
        check["verification_commands"][0]["failure_kind"], "command_failed",
        "{check:#}"
    );
    assert!(
        check["verification_commands"][0]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("forged"),
        "{check:#}"
    );

    guard.stop();
    Ok(())
}

#[test]
fn fork_on_termination_verification_descendant_is_reaped() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let marker_dir = tempfile::tempdir()?;
    let marker = marker_dir.path().join("forked-on-termination");
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    fs::write(
        repo.path().join("fork-on-term.sh"),
        format!(
            "#!/bin/sh\ntrap 'setsid sh -c \"trap \\\"\\\" TERM; sleep 0.3; printf escaped > {}\" >/dev/null 2>&1 & exit 0' TERM\nwhile :; do sleep 1; done\n",
            marker.display()
        ),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-SUPERVISOR-RACE-01",
            "title": "Fork-on-termination descendant containment",
            "goal": "Require stable descendant quiescence before verification returns.",
            "depends_on": [],
            "acceptance": ["PURITY-SUPERVISOR-RACE-01.txt exists"],
            "verify": ["sh fork-on-term.sh & test -f PURITY-SUPERVISOR-RACE-01.txt"]
        }),
    )?;
    git(
        repo.path(),
        &["add", ".gitignore", ".workflow", "fork-on-term.sh"],
    )?;
    git(
        repo.path(),
        &[
            "commit",
            "-m",
            "add fork-on-termination containment regression",
        ],
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
            "PURITY-SUPERVISOR-RACE-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_terminal_status(&bin, home.path(), &run_id, "completed")?;
    thread::sleep(Duration::from_millis(500));
    assert!(
        !marker.exists(),
        "descendant forked while handling termination escaped supervision"
    );

    guard.stop();
    Ok(())
}

#[test]
fn filter_equivalent_raw_verification_mutation_blocks_and_restores() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    git(
        repo.path(),
        &["config", "filter.normalize.clean", "sed 's/|raw-[^|]*$//'"],
    )?;
    git(repo.path(), &["config", "filter.normalize.smudge", "cat"])?;
    fs::write(
        repo.path().join(".gitattributes"),
        "RAW-01.txt filter=normalize\n",
    )?;
    fs::write(repo.path().join("RAW-01.txt"), "canonical|raw-before\n")?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-FILTER-01",
            "title": "Filter-equivalent verification purity regression",
            "goal": "Reject raw-byte mutation hidden by a clean filter.",
            "depends_on": [],
            "acceptance": ["PURITY-FILTER-01.txt exists"],
            "verify": ["printf 'canonical|raw-after\\n' > RAW-01.txt && test -f PURITY-FILTER-01.txt"]
        }),
    )?;
    git(
        repo.path(),
        &[
            "add",
            ".gitignore",
            ".gitattributes",
            "RAW-01.txt",
            ".workflow",
        ],
    )?;
    git(
        repo.path(),
        &["commit", "-m", "add filter purity regression"],
    )?;
    let base_sha = git(repo.path(), &["rev-parse", "HEAD"])?;

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
            "PURITY-FILTER-01",
        ],
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
            .contains("verification command changed the worktree"),
        "unexpected blocked reason: {blocked:#}"
    );
    let integration_branch = blocked["run"]["integration_branch"]
        .as_str()
        .expect("integration branch");
    assert_eq!(
        git(repo.path(), &["rev-parse", integration_branch])?,
        base_sha
    );
    assert!(
        blocked["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["type"].as_str() != Some("completion_publication_committed"))
    );

    let inspected = json_stdout(&kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?)?;
    let worker_stem =
        worker_attempt_output_stem(&blocked, "PURITY-FILTER-01", "slice-worker", 1, 0)?;
    let check: Value = serde_json::from_str(&fs::read_to_string(artifact_path(
        &inspected,
        &format!("{worker_stem}.check.json"),
    )?)?)?;
    assert_eq!(check["failure_kind"], "verification_mutated_worktree");
    assert_eq!(
        check["verification_commands"][0]["verification_workspace"]["before"]["tracked_filesystem_digest"],
        check["verification_commands"][0]["verification_workspace"]["restored"]["tracked_filesystem_digest"]
    );
    assert_ne!(
        check["verification_commands"][0]["verification_workspace"]["before"]["tracked_filesystem_digest"],
        check["verification_commands"][0]["verification_workspace"]["after"]["tracked_filesystem_digest"]
    );

    guard.stop();
    Ok(())
}

#[test]
fn cancelled_slice_verification_persists_restored_mutation_evidence() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let marker = tempfile::NamedTempFile::new()?;
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-CANCEL-01",
            "title": "Cancelled verification purity regression",
            "goal": "Persist restoration evidence before cancellation wins.",
            "depends_on": [],
            "acceptance": ["PURITY-CANCEL-01.txt exists"],
            "verify": [format!(
                "printf '{{}}' > '{}'; printf changed > PURITY-CANCEL-01.txt; sleep 30",
                marker_path.display()
            )]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add cancelled purity regression slice"],
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
            "PURITY-CANCEL-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_json_file(&marker_path)?;
    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &run_id,
            "--reason",
            "cancel purity regression",
        ],
    )?;
    let cancelled = wait_for_terminal_status(&bin, home.path(), &run_id, "cancelled")?;
    assert!(
        cancelled["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| { event["type"].as_str() != Some("completion_publication_committed") })
    );

    let inspected = json_stdout(&kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?)?;
    let worker_stem =
        worker_attempt_output_stem(&cancelled, "PURITY-CANCEL-01", "slice-worker", 1, 0)?;
    let check_path = artifact_path(&inspected, &format!("{worker_stem}.check.json"))?;
    let check: Value = serde_json::from_str(&fs::read_to_string(check_path)?)?;
    assert_eq!(check["verification_cancelled"], true);
    assert_eq!(check["failure_kind"], "verification_mutated_worktree");
    let workspace = &check["verification_commands"][0]["verification_workspace"];
    assert_eq!(
        workspace["restored"]["digest"],
        workspace["before"]["digest"]
    );
    assert!(
        workspace["after"]["unstaged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["status"] == "M")
    );

    guard.stop();
    Ok(())
}

#[test]
fn cancelled_integration_verification_persists_restored_mutation_evidence() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let marker = tempfile::NamedTempFile::new()?;
    let marker_path = marker.path().to_path_buf();
    drop(marker);
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    let config_path = repo.path().join(".workflow/khazad.json");
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&config_path)?)?;
    config["verify_profiles"] = json!({
        "cancel-purity": {
            "commands": [{
                "command": format!(
                    "printf '{{}}' > '{}'; printf changed > PURITY-INT-CANCEL-01.txt; sleep 30",
                    marker_path.display()
                ),
                "timeout_seconds": 30
            }]
        }
    });
    fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-INT-CANCEL-01",
            "title": "Cancelled integration verification purity regression",
            "goal": "Persist integration restoration evidence before cancellation wins.",
            "depends_on": [],
            "acceptance": ["PURITY-INT-CANCEL-01.txt exists"],
            "verify_profile": "cancel-purity",
            "verify": ["test -f PURITY-INT-CANCEL-01.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &[
            "commit",
            "-m",
            "add cancelled integration purity regression slice",
        ],
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
            "PURITY-INT-CANCEL-01",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    wait_for_json_file(&marker_path)?;
    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &run_id,
            "--reason",
            "cancel integration purity regression",
        ],
    )?;
    let cancelled = wait_for_terminal_status(&bin, home.path(), &run_id, "cancelled")?;
    assert!(
        cancelled["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| { event["type"].as_str() != Some("completion_publication_committed") })
    );

    let gate_path = repo
        .path()
        .join(".workflow/runs")
        .join(&run_id)
        .join("outputs/integration-gate.cancelled.json");
    let gate = wait_for_json_file(&gate_path)?;
    assert_eq!(gate["verification_cancelled"], true);
    assert_eq!(
        gate["commands"][0]["failure_kind"],
        "verification_mutated_worktree"
    );
    let workspace = &gate["commands"][0]["verification_workspace"];
    assert_eq!(
        workspace["restored"]["digest"],
        workspace["before"]["digest"]
    );
    assert!(
        workspace["after"]["unstaged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["status"] == "M")
    );

    guard.stop();
    Ok(())
}

#[test]
fn integration_verification_mutation_has_no_repair_publication_or_final_sha() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    let config_path = repo.path().join(".workflow/khazad.json");
    let mut config: Value = serde_json::from_str(&fs::read_to_string(&config_path)?)?;
    config["verify_profiles"] = json!({
        "purity": {
            "commands": [{
                "command": "printf 'integration verification side effect\\n' > integration-verification-side-effect.txt",
                "timeout_seconds": 30
            }]
        }
    });
    fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    write_slice(
        repo.path(),
        json!({
            "id": "PURITY-02",
            "title": "Integration verification purity regression",
            "goal": "Block integration verification side effects before publication.",
            "depends_on": [],
            "acceptance": ["PURITY-02.txt exists"],
            "verify_profile": "purity",
            "verify": ["test -f PURITY-02.txt"]
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add integration purity regression slice"],
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
            "PURITY-02",
        ],
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
            .contains("integration gate needs operator environment fix"),
        "unexpected blocked reason: {blocked:#}"
    );
    assert_eq!(blocked["economics"]["agent_call_count"], 1);
    let integration_branch = blocked["run"]["integration_branch"]
        .as_str()
        .expect("integration branch");
    git(
        repo.path(),
        &["show", &format!("{integration_branch}:PURITY-02.txt")],
    )?;
    for path_in_tree in [
        "integration-verification-side-effect.txt".to_string(),
        format!(".workflow/reports/{run_id}-final-report.json"),
        format!(".workflow/reports/{run_id}-implementation-summary.json"),
    ] {
        assert!(
            git(
                repo.path(),
                &["ls-tree", "--name-only", integration_branch, &path_in_tree],
            )?
            .is_empty(),
            "unexpected publication path {path_in_tree}"
        );
    }
    let open_slice: Value = serde_json::from_str(&git(
        repo.path(),
        &[
            "show",
            &format!("{integration_branch}:.workflow/slices/PURITY-02.json"),
        ],
    )?)?;
    assert_eq!(open_slice["status"].as_str().unwrap_or("open"), "open");

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let summary_path = artifact_path(&inspected, "implementation-summary.json")?;
    let summary: Value = serde_json::from_str(&fs::read_to_string(summary_path)?)?;
    assert!(summary["final_sha"].as_str().unwrap_or_default().is_empty());
    assert_eq!(
        summary["integration_gate"]["commands"][0]["failure_kind"],
        "verification_mutated_worktree"
    );
    assert_eq!(
        summary["integration_gate"]["commands"][0]["verification_workspace"]["restored"]["digest"],
        summary["integration_gate"]["commands"][0]["verification_workspace"]["before"]["digest"]
    );
    assert!(
        blocked["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| { event["type"].as_str() != Some("completion_publication_committed") })
    );

    guard.stop();
    Ok(())
}

#[test]
fn concurrent_daemon_admission_rpcs_have_one_durable_winner_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_cancellable_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-admission",
            "title": "Admission race fixture",
            "goal": "Keep one admitted worker active while a competing RPC is rejected.",
            "acceptance": ["admission race is serialized"],
            "verify": []
        }),
    )?;
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(repo.path(), &["commit", "-m", "add admission race slice"])?;
    kd_ok(&bin, home.path(), &["daemon", "start"])?;

    // Raw RPCs bypass the CLI test helper that forces direct cockpit mode. Carry
    // the same transport argument so this test cannot open real Herdr panes.
    let start_params = || {
        json!({
            "repo_path": path(repo.path()),
            "slice_id": "",
            "slice_ids": ["slice-admission"],
            "all": false,
            "agent": "pi",
            "pi_bin": path(&fake_pi),
            "pi_args": ["__khazad_cockpit_mode=direct"],
            "native_pi_tui_worker": false,
            "parallelism": 1,
            "allow_dirty": true,
            "origin_notification_target": "",
            "mission_envelope": null
        })
    };
    let assert_one_winner = |outcomes: &[Result<Value, String>; 2]| {
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
            1,
            "admission outcomes: {outcomes:?}"
        );
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1,
            "admission outcomes: {outcomes:?}"
        );
        let error = outcomes
            .iter()
            .find_map(|outcome| outcome.as_ref().err())
            .expect("conflicting admission error");
        assert!(
            error.contains("already has active run") || error.contains("cannot be resumed"),
            "unexpected admission error: {error}"
        );
    };
    let winner_id = |outcomes: &[Result<Value, String>; 2]| -> String {
        outcomes
            .iter()
            .find_map(|outcome| outcome.as_ref().ok())
            .and_then(|value| value["run_id"].as_str())
            .expect("successful admission run id")
            .to_string()
    };
    let assert_db = |expected_runs: i64, expected_intents: i64, expected_epoch: i64| {
        let conn = rusqlite::Connection::open(home.path().join("state.sqlite"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let run_count =
            conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get::<_, i64>(0))?;
        let active_count = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE status IN ('pending','running')",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let intent_count =
            conn.query_row("SELECT COUNT(*) FROM run_launch_intents", [], |row| {
                row.get::<_, i64>(0)
            })?;
        let max_epoch = conn.query_row("SELECT MAX(execution_epoch) FROM runs", [], |row| {
            row.get::<_, i64>(0)
        })?;
        if run_count != expected_runs
            || active_count != 1
            || intent_count != expected_intents
            || max_epoch != expected_epoch
        {
            return Err(format!(
                "unexpected admission state: runs={run_count} active={active_count} intents={intent_count} epoch={max_epoch}"
            )
            .into());
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    };

    let start_start = race_daemon_socket_rpcs(
        home.path(),
        "startRun",
        start_params(),
        "startRun",
        start_params(),
    );
    assert_one_winner(&start_start);
    let original_run_id = winner_id(&start_start);
    assert_db(1, 1, 1)?;
    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &original_run_id,
            "--reason",
            "prepare resume race",
        ],
    )?;
    wait_for_terminal_status(&bin, home.path(), &original_run_id, "cancelled")?;

    let resume_params = || {
        json!({
            "run_id": original_run_id,
            "agent": "pi",
            "pi_bin": path(&fake_pi),
            "pi_args": ["__khazad_cockpit_mode=direct"],
            "native_pi_tui_worker": false,
            "parallelism": 1
        })
    };
    let resume_resume = race_daemon_socket_rpcs(
        home.path(),
        "resumeRun",
        resume_params(),
        "resumeRun",
        resume_params(),
    );
    assert_one_winner(&resume_resume);
    assert_eq!(winner_id(&resume_resume), original_run_id);
    assert_db(1, 2, 2)?;
    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &original_run_id,
            "--reason",
            "prepare mixed race",
        ],
    )?;
    wait_for_terminal_status(&bin, home.path(), &original_run_id, "cancelled")?;

    let start_resume = race_daemon_socket_rpcs(
        home.path(),
        "startRun",
        start_params(),
        "resumeRun",
        resume_params(),
    );
    assert_one_winner(&start_resume);
    let mixed_winner = winner_id(&start_resume);
    let expected_runs = if mixed_winner == original_run_id {
        1
    } else {
        2
    };
    assert_db(expected_runs, 3, if expected_runs == 1 { 3 } else { 2 })?;
    kd_ok(
        &bin,
        home.path(),
        &[
            "cancel",
            "--run",
            &mixed_winner,
            "--reason",
            "race complete",
        ],
    )?;
    wait_for_terminal_status(&bin, home.path(), &mixed_winner, "cancelled")?;
    let conn = rusqlite::Connection::open(home.path().join("state.sqlite"))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE status IN ('pending','running')",
            [],
            |row| row.get::<_, i64>(0),
        )?,
        0
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
fn untracked_slice_metadata_blocks_completion_with_integrity_incident_black_box() -> TestResult {
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
    let status = wait_for_terminal_status(&bin, home.path(), &run_id, "blocked")?;
    let incidents = status["incidents"].as_array().expect("incidents array");
    assert!(incidents.iter().any(|incident| {
        incident["kind"].as_str() == Some("slice_close_missing")
            && incident["severity"].as_str() == Some("error")
    }));
    let close_event = status["events"]
        .as_array()
        .expect("events array")
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("run_incident")
                && event["payload"]["kind"].as_str() == Some("slice_close_missing")
        })
        .expect("slice close missing event");
    assert_eq!(close_event["payload"]["slice_id"], "slice-001");
    assert!(
        close_event["payload"]["path"]
            .as_str()
            .unwrap()
            .ends_with(".workflow/slices/slice-001.json")
    );
    assert_eq!(
        close_event["payload"]["policy"],
        "block_completion_publication_on_missing_close_record"
    );
    assert!(
        !status["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["type"].as_str() == Some("completion_publication_committed") })
    );
    let handoff = kd(&bin, home.path(), &["handoff", "--run", &run_id])?;
    assert!(!handoff.status.success());

    let monitor = kd(
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
    assert!(!monitor.status.success());
    let monitor = String::from_utf8(monitor.stdout)?;
    assert!(monitor.contains("Incidents"));
    assert!(monitor.contains("slice_close_missing"));
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
    let latest_monitor = kd(
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
    assert!(!latest_monitor.status.success());
    let latest_monitor = String::from_utf8(latest_monitor.stdout)?;
    assert!(latest_monitor.contains(&run_id));
    assert!(latest_monitor.contains("blocked"));

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

    let unpushed_pr = kd_with_env(
        &bin,
        home.path(),
        &env,
        &["handoff", "--run", &run_id, "--create-pr"],
    )?;
    assert!(
        !unpushed_pr.status.success(),
        "PR creation without an exact receipt push unexpectedly succeeded"
    );
    assert!(
        String::from_utf8_lossy(&unpushed_pr.stderr)
            .contains("requires pushing and validating the exact publication receipt SHA")
    );

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
    assert!(monitored_once.contains("Workers"));
    assert!(monitored_once.contains("Run ● running"));
    assert!(monitored_once.contains("phase worker_verify"));
    assert!(monitored_once.contains("slice-001"));
    assert!(monitored_once.contains("Checks"));
    assert!(monitored_once.contains("elapsed"));
    assert!(monitored_once.contains("tail started-progress"));

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
    assert!(watched.contains("completed"), "{watched}");
    assert!(watched.contains("Run ✓ completed"), "{watched}");
    assert!(!watched.contains("Status:"));
    assert!(!watched.contains("Phase:"));

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
            "supervisor alive, observed child",
            "pid=",
            "last event none",
            "semantic unknown",
            "timeout disabled",
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
    let accepted_decision = kd_ok(
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
    assert_eq!(json_stdout(&accepted_decision)?["outcome"], "applied");
    let duplicate_decision = kd_ok(
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
    assert_eq!(
        json_stdout(&duplicate_decision)?["outcome"],
        "already_applied_idempotently"
    );

    kd_ok(
        &bin,
        home.path(),
        &[
            "replan",
            "propose",
            &run_id,
            "--id",
            "rp-race",
            "--source-kind",
            "worker",
            "--source-slice",
            "slice-001",
            "--change",
            "mark_duplicate:slice-race:exercise concurrent decisions",
            "--risk",
            "operator_review",
        ],
    )?;
    let (accepted_race, rejected_race) = thread::scope(|scope| {
        let accept = scope.spawn(|| {
            kd(
                &bin,
                home.path(),
                &[
                    "replan",
                    "accept",
                    &run_id,
                    "rp-race",
                    "--reason",
                    "accept concurrently",
                ],
            )
            .expect("run concurrent accept command")
        });
        let reject = scope.spawn(|| {
            kd(
                &bin,
                home.path(),
                &[
                    "replan",
                    "reject",
                    &run_id,
                    "rp-race",
                    "--reason",
                    "reject concurrently",
                ],
            )
            .expect("run concurrent reject command")
        });
        (
            accept.join().expect("accept command thread"),
            reject.join().expect("reject command thread"),
        )
    });
    let race_outputs = [accepted_race, rejected_race];
    assert_eq!(
        race_outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1,
        "exactly one conflicting daemon command may succeed"
    );
    let mut race_outcomes = race_outputs
        .iter()
        .map(json_stdout)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|result| result["outcome"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    race_outcomes.sort();
    assert_eq!(race_outcomes, vec!["applied", "conflict"]);

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
    daemon::replan::assert_pending_replan(&status, "rp-pending");
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
    assert!(monitored.contains("Pending replan rp-pending"));
    assert!(monitored.contains("Decision command:"));
    assert!(monitored.contains("Commands"));

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
    assert_eq!(slice_run["attempts"], 1);

    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let worker_stem = worker_attempt_output_stem(&completed, "slice-001", "slice-worker", 1, 0)?;
    let invalid_path = artifact_path(&inspected, &format!("{worker_stem}.invalid-output.json"))?;
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
    let first_envelope_stem =
        worker_attempt_output_stem(&completed, "slice-001", "slice-envelope-retry", 1, 1)?;
    let invalid_schema_path = artifact_path(
        &inspected,
        &format!("{first_envelope_stem}.envelope-1.invalid-output.json"),
    )?;
    let invalid_schema: Value = serde_json::from_str(&fs::read_to_string(invalid_schema_path)?)?;
    let invalid_schema_error = invalid_schema["parse_error"].as_str().unwrap_or_default();
    assert!(
        invalid_schema_error.contains("summary") || invalid_schema_error.contains("validation"),
        "unexpected invalid schema error: {invalid_schema_error}"
    );
    assert_eq!(invalid_schema["attempt"], 1);
    assert_eq!(invalid_schema["envelope_retry"], 1);
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
fn af_worker_envelope_failures_are_corrected_in_same_attempt_black_box() -> TestResult {
    let bin = binary_path();
    let home = tempfile::tempdir()?;
    let repo = tempfile::tempdir()?;
    let fake_bin = tempfile::tempdir()?;
    let fake_pi = write_af_envelope_corrections_fake_pi(fake_bin.path())?;
    init_git_repo(repo.path())?;
    let guard = DaemonGuard::new(bin.clone(), home.path().to_path_buf());

    kd_ok(&bin, home.path(), &["init", "--repo", path(repo.path())])?;
    for (id, title, acceptance) in [
        (
            "missing-acceptance",
            "Correct missing acceptance evidence",
            "exact acceptance evidence is required",
        ),
        (
            "missing-action",
            "Correct missing finding action",
            "every finding includes its action",
        ),
    ] {
        write_slice(
            repo.path(),
            json!({
                "id": id,
                "title": title,
                "goal": "Correct an invalid result envelope without repeating implementation.",
                "acceptance": [acceptance]
            }),
        )?;
    }
    git(repo.path(), &["add", ".gitignore", ".workflow"])?;
    git(
        repo.path(),
        &["commit", "-m", "add AF envelope correction slices"],
    )?;

    let fake_pi_string = path(&fake_pi).to_string();
    let started = kd_ok_with_env(
        &bin,
        home.path(),
        &[("KHAZAD_PI_BIN", fake_pi_string.as_str())],
        &[
            "run",
            "--repo",
            path(repo.path()),
            "--agent",
            "pi",
            "--parallel",
            "1",
            "--all",
        ],
    )?;
    let run_id = json_stdout(&started)?["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let completed = wait_for_terminal_status(&bin, home.path(), &run_id, "completed")?;
    let inspected = kd_ok(&bin, home.path(), &["inspect", "--run", &run_id])?;
    let inspected = json_stdout(&inspected)?;
    let events = completed["events"].as_array().expect("events");
    for (slice_id, expected_error, expected_payload) in [
        (
            "missing-acceptance",
            "missing acceptance evidence",
            "wrong acceptance criterion",
        ),
        (
            "missing-action",
            "missing field `action`",
            "finding without required action",
        ),
    ] {
        let slice_run = completed["slice_runs"]
            .as_array()
            .expect("slice runs")
            .iter()
            .find(|slice_run| slice_run["slice_id"].as_str() == Some(slice_id))
            .expect("slice run");
        assert_eq!(slice_run["attempts"], 1, "{slice_id}");

        let worker_stem = worker_attempt_output_stem(&completed, slice_id, "slice-worker", 1, 0)?;
        let invalid_path =
            artifact_path(&inspected, &format!("{worker_stem}.invalid-output.json"))?;
        let invalid: Value = serde_json::from_str(&fs::read_to_string(invalid_path)?)?;
        assert_eq!(invalid["attempt"], 1);
        assert_eq!(invalid["envelope_retry"], 0);
        assert!(
            invalid["parse_error"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_error),
            "unexpected {slice_id} error: {invalid:#}"
        );
        assert!(
            invalid["raw_invalid_payload"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_payload),
            "missing preserved {slice_id} payload: {invalid:#}"
        );
        assert!(
            invalid["committed_diff_name_only"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("{slice_id}.txt")),
            "initial implementation commit should be preserved: {invalid:#}"
        );
        let envelope_stem =
            worker_attempt_output_stem(&completed, slice_id, "slice-envelope-retry", 1, 1)?;
        let envelope_path = artifact_path(&inspected, &format!("{envelope_stem}.json"))?;
        assert!(Path::new(envelope_path).exists());
        assert!(events.iter().any(|event| {
            event["type"].as_str() == Some("worker_envelope_retry_succeeded")
                && event["payload"]["slice_id"].as_str() == Some(slice_id)
                && event["payload"]["attempt"].as_u64() == Some(1)
                && event["payload"]["envelope_retry"].as_u64() == Some(1)
        }));
        assert_eq!(
            fs::read_to_string(fake_bin.path().join(format!("{slice_id}.attempt")))?,
            "2",
            "{slice_id} should use one implementation call and one envelope correction"
        );
    }

    let run_summary_path = artifact_path(&inspected, "run-summary.json")?;
    let run_summary: Value = serde_json::from_str(&fs::read_to_string(run_summary_path)?)?;
    assert_eq!(run_summary["economics"]["agent_call_count"], 4);

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
                block["label"].as_str() == Some("Attention")
                    && block["lines"].as_array().unwrap().iter().any(|line| {
                        line["text"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("kind=agent_auth_required")
                    })
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

    let worker_stem = worker_attempt_output_stem(&cancelled, "slice-001", "slice-worker", 1, 0)?;
    let failure_path = artifact_path(&inspected, &format!("{worker_stem}.failure.json"))?;
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
            "areas": ["slice-001.txt"],
            "acceptance": ["parallel layer failure is recorded"]
        }),
    )?;
    write_slice(
        repo.path(),
        json!({
            "id": "slice-002",
            "title": "Long parallel sibling",
            "goal": "Stay active until the layer cancellation reaches this worker.",
            "areas": ["slice-002.txt"],
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
        monitored.contains("active parallel slice-001, slice-002"),
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
    assert!(stdout.contains("Terminal reason: kind=failed"));
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
    assert!(latest_monitor.contains("Workers"));

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

fn wait_for_pending_question(bin: &Path, home: &Path, run_id: &str) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = kd_ok(bin, home, &["status", "--run", run_id])?;
        let value = json_stdout(&output)?;
        let progress = &value["progress"];
        let progress_slice = progress["slice_id"].as_str().unwrap_or_default();
        let progress_attempt = progress["attempt"].as_u64().unwrap_or(0);
        let matching = value["questions"].as_array().is_some_and(|questions| {
            questions.iter().any(|question| {
                question["state"].as_str() == Some("pending")
                    && question["slice_id"].as_str() == Some(progress_slice)
                    && question["attempt"].as_u64().unwrap_or(0) == progress_attempt
            })
        });
        if value["run"]["status"].as_str() == Some("running")
            && progress["phase"].as_str() == Some("awaiting_operator")
            && matching
        {
            return Ok(value);
        }
        if matches!(
            value["run"]["status"].as_str(),
            Some("failed" | "blocked" | "cancelled" | "interrupted" | "completed")
        ) {
            panic!("run reached terminal state before pending question was visible: {value:#}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pending question"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_json_file(path: &Path) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match fs::read_to_string(path) {
            Ok(contents) => return Ok(serde_json::from_str(&contents)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for JSON file {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(50));
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
    with open(f"{slice_id}.txt", "w", encoding="utf-8") as fh:
        fh.write(f"implementation preserved across invalid output for {slice_id}\n")
    subprocess.run(["git", "add", "."], check=True)
    subprocess.run(["git", "commit", "-m", f"fake pi implement {slice_id}"], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
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

fn write_af_envelope_corrections_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json
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
    with open(f"{slice_id}.txt", "w", encoding="utf-8") as fh:
        fh.write(f"implemented once for {slice_id}\n")
    subprocess.run(["git", "add", "."], check=True)
    subprocess.run(
        ["git", "commit", "-m", f"fake pi implement {slice_id}"],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
criterion = handoff["slice"]["acceptance"][0]

if attempt == 1 and slice_id == "missing-acceptance":
    result = {
        "slice_id": slice_id,
        "status": "complete",
        "summary": "implementation complete with mismatched evidence",
        "commit_sha": sha,
        "changed_files": [f"{slice_id}.txt"],
        "acceptance_status": [{
            "criterion": "wrong acceptance criterion",
            "status": "satisfied",
            "evidence": "the implementation commit exists",
        }],
    }
elif attempt == 1 and slice_id == "missing-action":
    result = {
        "slice_id": slice_id,
        "status": "complete",
        "summary": "implementation complete with malformed finding",
        "commit_sha": sha,
        "changed_files": [f"{slice_id}.txt"],
        "acceptance_status": [{
            "criterion": criterion,
            "status": "satisfied",
            "evidence": "the implementation commit exists",
        }],
        "findings": [{
            "severity": "warning",
            "description": "finding without required action",
        }],
    }
else:
    result = {
        "slice_id": slice_id,
        "status": "complete",
        "summary": "valid envelope for the existing implementation commit",
        "commit_sha": sha,
        "changed_files": [f"{slice_id}.txt"],
        "acceptance_status": [{
            "criterion": criterion,
            "status": "satisfied",
            "evidence": "re-emitted from the unchanged implementation HEAD",
        }],
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

fn write_operator_question_fake_pi(dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pi");
    fs::write(
        &path,
        r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import socket
import subprocess
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


def handoff_from_prompt(prompt):
    lines = prompt.splitlines()
    for index, line in enumerate(lines):
        if line.strip() == "Read this handoff JSON first:" and index + 1 < len(lines):
            return lines[index + 1].strip()
    return ""


def daemon_call(method, params):
    sock_path = os.environ.get("KHAZAD_DAEMON_SOCKET", "")
    if not sock_path:
        raise RuntimeError("KHAZAD_DAEMON_SOCKET is not available")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.connect(sock_path)
        request = {
            "id": f"fake-pi-{time.time_ns()}",
            "method": method,
            "params": params,
        }
        sock.sendall((json.dumps(request) + "\n").encode("utf-8"))
        data = b""
        while b"\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                raise RuntimeError("daemon socket closed before workerAsk response")
            data += chunk
    response = json.loads(data.split(b"\n", 1)[0].decode("utf-8"))
    if response.get("error"):
        raise RuntimeError(response["error"])
    return response.get("result") or {}


prompt = sys.stdin.read()
handoff_path = handoff_from_prompt(prompt)
if not handoff_path:
    emit({"status": "no-op", "summary": "operator fake pi: no repair needed"})
    sys.exit(0)

with open(handoff_path, encoding="utf-8") as fh:
    handoff = json.load(fh)

mode = os.environ.get("KHAZAD_FAKE_PI_OPERATOR_MODE", "answer")
slice_id = handoff["slice"]["id"]
run_id = handoff["run_id"]
if mode == "answer" and "timeout" in slice_id:
    mode = "timeout"
if mode == "answer" and "unavailable" in slice_id:
    mode = "unavailable"
if mode == "answer" and "same-pane" in slice_id:
    mode = "same_pane"
attempt = int(os.environ.get("KHAZAD_ATTEMPT", "0") or "0")
question_text = f"Which operator answer should {slice_id} use on attempt {attempt}?"

if mode == "unavailable":
    emit({
        "slice_id": slice_id,
        "status": "blocked",
        "summary": "ask_operator unavailable fallback blocked the worker",
        "findings": [{
            "severity": "blocker",
            "action": "ask-user",
            "description": "ask_operator channel unavailable; worker produced blocked JSON instead of inventing operator intent",
        }],
    })
    sys.exit(0)

params = {
    "run_id": run_id,
    "slice_id": slice_id,
    "attempt": attempt,
    "token": os.environ.get("KHAZAD_WORKER_TOKEN", ""),
    "question": question_text,
    "options": ["alpha", "bravo"],
    "timeout_seconds": 1 if mode == "timeout" else int(os.environ.get("KHAZAD_FAKE_PI_OPERATOR_TIMEOUT", "30")),
}
launch_id = int(os.environ.get("KHAZAD_LAUNCH_ID", "0") or "0")
if launch_id > 0:
    params["launch_id"] = launch_id
if mode == "same_pane":
    params.update({
        "recommended_answer": "alpha",
        "rationale": "alpha is the bounded reversible fixture choice",
        "bounded_within_current_slice_or_mission_authority": True,
        "reversible": True,
    })
try:
    result = daemon_call("workerAskOpen" if mode == "same_pane" else "workerAsk", params)
except Exception as exc:
    emit({
        "slice_id": slice_id,
        "status": "blocked",
        "summary": "ask_operator unavailable fallback blocked the worker",
        "findings": [{
            "severity": "blocker",
            "action": "ask-user",
            "description": f"ask_operator channel unavailable: {exc}; worker produced blocked JSON instead of inventing operator intent",
        }],
    })
    sys.exit(0)

if mode == "same_pane":
    while True:
        time.sleep(1)

if result.get("timed_out"):
    emit({
        "slice_id": slice_id,
        "status": "blocked",
        "summary": "ask_operator timed out and worker blocked for operator intent",
        "findings": [{
            "severity": "blocker",
            "action": "ask-user",
            "description": f"operator answer timed out; workerAsk question {result.get('question_id')} timed out",
        }],
    })
    sys.exit(0)

answer = result.get("answer", "")
Path(f"{slice_id}.txt").write_text(f"operator answer: {answer}\n", encoding="utf-8")
subprocess.run(["git", "add", "."], check=True)
subprocess.run(
    ["git", "commit", "-m", f"fake pi operator answer {slice_id}"],
    check=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
)
sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
emit({
    "slice_id": slice_id,
    "status": "complete",
    "summary": f"operator answered {answer}",
    "commit_sha": sha,
    "changed_files": [f"{slice_id}.txt"],
    "tests_run": handoff["slice"].get("verify", []),
    "acceptance_status": [
        {
            "criterion": criterion,
            "status": "satisfied",
            "evidence": f"operator answer {answer} recorded",
        }
        for criterion in handoff["slice"].get("acceptance", [])
    ],
})
"#,
    )?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

fn live_worker_token(home: &Path, status: &Value, launch_id: i64) -> TestResult<String> {
    let pid = if let Some(pid) = status["progress"]["worker"]["pid"].as_i64() {
        pid
    } else {
        let conn = rusqlite::Connection::open(home.join("state.sqlite"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row(
            "SELECT worker_pid FROM worker_attempt_ledger WHERE launch_id=?1",
            rusqlite::params![launch_id],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .ok_or("pending question launch has no live worker pid")?
    };
    let environ = fs::read(format!("/proc/{pid}/environ"))?;
    let token = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| {
            entry
                .strip_prefix(b"KHAZAD_WORKER_TOKEN=")
                .map(|value| String::from_utf8_lossy(value).into_owned())
        })
        .filter(|value| !value.is_empty())
        .ok_or("live worker environment has no token")?;
    Ok(token)
}

fn force_question_deadline(home: &Path, question_id: &str, fallback_eligible: bool) -> TestResult {
    let conn = rusqlite::Connection::open(home.join("state.sqlite"))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let updated = conn.execute(
        "UPDATE worker_questions SET deadline_at='2000-01-01T00:00:00+00:00', fallback_eligible=?1 WHERE id=?2",
        rusqlite::params![fallback_eligible, question_id],
    )?;
    if updated != 1 {
        return Err(format!("question {question_id:?} was not updated").into());
    }
    Ok(())
}

fn race_daemon_socket_rpcs(
    home: &Path,
    first_method: &str,
    first_params: Value,
    second_method: &str,
    second_params: Value,
) -> [Result<Value, String>; 2] {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let first_home = home.to_path_buf();
    let first_method = first_method.to_string();
    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        daemon_rpc(&first_home, &first_method, first_params).map_err(|error| error.to_string())
    });
    let second_home = home.to_path_buf();
    let second_method = second_method.to_string();
    let second_barrier = barrier.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        daemon_rpc(&second_home, &second_method, second_params).map_err(|error| error.to_string())
    });
    barrier.wait();
    [
        first.join().expect("first admission RPC thread"),
        second.join().expect("second admission RPC thread"),
    ]
}

fn race_question_socket_rpcs(
    home: &Path,
    run_id: &str,
    question_id: &str,
    launch_id: i64,
    token: &str,
) -> TestResult<[Value; 2]> {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let answer_home = home.to_path_buf();
    let answer_run_id = run_id.to_string();
    let answer_question_id = question_id.to_string();
    let answer_barrier = barrier.clone();
    let answer = thread::spawn(move || {
        answer_barrier.wait();
        daemon_rpc(
            &answer_home,
            "answerQuestion",
            json!({
                "run_id": answer_run_id,
                "question_id": answer_question_id,
                "answer": "bravo"
            }),
        )
        .map_err(|error| error.to_string())
    });
    let timeout_home = home.to_path_buf();
    let timeout_run_id = run_id.to_string();
    let timeout_question_id = question_id.to_string();
    let timeout_token = token.to_string();
    let timeout_barrier = barrier.clone();
    let timeout = thread::spawn(move || {
        timeout_barrier.wait();
        daemon_rpc(
            &timeout_home,
            "workerQuestionTimeout",
            json!({
                "run_id": timeout_run_id,
                "question_id": timeout_question_id,
                "token": timeout_token,
                "launch_id": launch_id
            }),
        )
        .map_err(|error| error.to_string())
    });
    barrier.wait();
    let answer = answer
        .join()
        .map_err(|_| "answer RPC thread panicked")?
        .map_err(|error| format!("answer RPC failed: {error}"))?;
    let timeout = timeout
        .join()
        .map_err(|_| "timeout RPC thread panicked")?
        .map_err(|error| format!("timeout RPC failed: {error}"))?;
    Ok([answer, timeout])
}

fn daemon_rpc(home: &Path, method: &str, params: Value) -> TestResult<Value> {
    let mut stream = UnixStream::connect(home.join("socket"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = json!({
        "id": format!("integration-{method}"),
        "method": method,
        "params": params,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let response: Value = serde_json::from_str(&response)?;
    if let Some(error) = response["error"].as_str() {
        return Err(format!("daemon {method} error: {error}").into());
    }
    Ok(response["result"].clone())
}

fn assert_one_applied_one_conflict(outcomes: &[Value; 2]) {
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome["outcome"] == "applied")
            .count(),
        1,
        "RPC outcomes: {outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome["outcome"] == "conflict")
            .count(),
        1,
        "RPC outcomes: {outcomes:?}"
    );
}

fn question_resolution_event_count(status: &Value, question_id: &str) -> usize {
    status["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("worker_question_answered" | "run_incident")
            ) && event["payload"]["question_id"] == question_id
        })
        .count()
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

fn worker_attempt_output_stem<'a>(
    status: &'a Value,
    slice_id: &str,
    kind: &str,
    worker_retry_ordinal: u64,
    envelope_retry_ordinal: u64,
) -> TestResult<&'a str> {
    status["worker_attempts"]
        .as_array()
        .and_then(|attempts| {
            attempts.iter().find_map(|attempt| {
                (attempt["slice_id"].as_str() == Some(slice_id)
                    && attempt["kind"].as_str() == Some(kind)
                    && attempt["worker_retry_ordinal"].as_u64() == Some(worker_retry_ordinal)
                    && attempt["envelope_retry_ordinal"].as_u64()
                        == Some(envelope_retry_ordinal))
                .then(|| attempt["output_stem"].as_str())
                .flatten()
            })
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "worker attempt {slice_id}/{kind}/retry-{worker_retry_ordinal}/envelope-{envelope_retry_ordinal} not found in status: {status:#}"
                ),
            )
            .into()
        })
}

fn path(path: &Path) -> &str {
    path.to_str().expect("utf-8 test path")
}
