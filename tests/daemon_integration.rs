use serde_json::{Value, json};
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
    git(repo.path(), &["add", ".gitignore", ".workflow/slices"])?;
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
        status["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["type"].as_str() == Some("run_completed") })
    );

    let handoff = kd_ok(&bin, home.path(), &["handoff", "--run", &run_id])?;
    let handoff = json_stdout(&handoff)?;
    assert_eq!(handoff["run_id"], run_id);
    assert!(handoff["push_command"].as_str().unwrap().contains("git -C"));
    assert!(
        handoff["pr_command"]
            .as_str()
            .unwrap()
            .contains("gh pr create")
    );

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

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_khazad-doom"))
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

fn path(path: &Path) -> &str {
    path.to_str().expect("utf-8 test path")
}
