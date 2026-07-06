use crate::domain::{CockpitMode, Run};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const RUN_STATUS_FEED_PANE: &str = "Run Status / Event Feed";
pub(crate) const INTEGRATION_GATE_REPAIR_PANE: &str = "Integration Gate / Repair";
const COCKPIT_MODE_TRANSPORT_PREFIX: &str = "__khazad_cockpit_mode=";
const HERDR_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub(crate) struct CockpitRunRequest {
    pub repo_path: PathBuf,
    pub khazad_home: PathBuf,
    pub workspace_label: String,
    pub feed_command: String,
    pub phase_command: String,
}

impl CockpitRunRequest {
    pub fn for_run(run: &Run, khazad_home: &Path) -> Self {
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("khazad-doom"));
        let binary = shell_quote(&binary.to_string_lossy());
        let run_id = shell_quote(&run.id);
        Self {
            repo_path: PathBuf::from(&run.repo_path),
            khazad_home: khazad_home.to_path_buf(),
            workspace_label: workspace_label_for_run(&run.id),
            feed_command: format!("{binary} monitor --run {run_id} --interval-ms 1000"),
            phase_command: format!("{binary} status --run {run_id} --follow --interval-ms 1000"),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CockpitPaneRequest {
    pub label: String,
    pub command: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct CockpitWorkspaceRef {
    id: String,
    anchor_pane: CockpitPaneRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitPaneRef {
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitOpened {
    pub adapter: String,
    pub mode: CockpitMode,
    pub workspace_label: String,
    pub pane_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CockpitLaunch {
    Opened(CockpitOpened),
    SkippedDirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitUnavailable {
    pub mode: CockpitMode,
    pub adapter: String,
    pub message: String,
    pub remediation: String,
}

impl CockpitUnavailable {
    fn new(mode: CockpitMode, adapter: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            mode,
            adapter: adapter.into(),
            message: message.into(),
            remediation: "Install a usable herdr binary, or run with --cockpit direct to suppress cockpit attempts. Khazad-Doom continues to use daemon state, status, watch, monitor, verification, merge, and handoff as the source of truth.".to_string(),
        }
    }
}

pub(crate) trait CockpitAdapter {
    fn name(&self) -> &'static str;
    fn open_or_focus_run_workspace(
        &self,
        request: &CockpitRunRequest,
    ) -> Result<CockpitWorkspaceRef>;
    fn create_read_only_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef>;
}

#[derive(Debug)]
pub(crate) struct Cockpit<A> {
    mode: CockpitMode,
    adapter: A,
}

impl<A: CockpitAdapter> Cockpit<A> {
    pub fn new(mode: CockpitMode, adapter: A) -> Self {
        Self { mode, adapter }
    }

    pub fn open_run(&self, request: &CockpitRunRequest) -> Result<CockpitLaunch> {
        if self.mode == CockpitMode::Direct {
            return Ok(CockpitLaunch::SkippedDirect);
        }
        let workspace = self.adapter.open_or_focus_run_workspace(request)?;
        let pane_env = vec![
            (
                "KHAZAD_HOME".to_string(),
                request.khazad_home.to_string_lossy().to_string(),
            ),
            ("KHAZAD_COCKPIT_READ_ONLY".to_string(), "1".to_string()),
        ];
        let feed = CockpitPaneRequest {
            label: RUN_STATUS_FEED_PANE.to_string(),
            command: request.feed_command.clone(),
            cwd: request.repo_path.clone(),
            env: pane_env.clone(),
        };
        self.adapter.create_read_only_pane(&workspace, &feed)?;
        let phase = CockpitPaneRequest {
            label: INTEGRATION_GATE_REPAIR_PANE.to_string(),
            command: request.phase_command.clone(),
            cwd: request.repo_path.clone(),
            env: pane_env,
        };
        self.adapter.create_read_only_pane(&workspace, &phase)?;
        Ok(CockpitLaunch::Opened(CockpitOpened {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            workspace_label: request.workspace_label.clone(),
            pane_labels: vec![feed.label, phase.label],
        }))
    }
}

pub(crate) fn open_default_run_cockpit(
    run: &Run,
    mode: CockpitMode,
    khazad_home: &Path,
) -> std::result::Result<CockpitLaunch, CockpitUnavailable> {
    #[cfg(test)]
    // Unit tests should not open external Herdr workspaces; the integration smoke covers the real adapter.
    if std::env::var("KHAZAD_UNIT_TEST_COCKPIT").ok().as_deref() != Some("1") {
        return Ok(CockpitLaunch::SkippedDirect);
    }
    if mode == CockpitMode::Direct {
        return Ok(CockpitLaunch::SkippedDirect);
    }
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    let request = CockpitRunRequest::for_run(run, khazad_home);
    Cockpit::new(mode, adapter)
        .open_run(&request)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn cockpit_mode_transport_arg(value: &str) -> Result<String> {
    let mode = CockpitMode::parse(value)?;
    Ok(format!("{COCKPIT_MODE_TRANSPORT_PREFIX}{}", mode.as_str()))
}

pub(crate) fn take_cockpit_mode_transport_arg(
    args: &mut Vec<String>,
) -> Result<Option<CockpitMode>> {
    let mut mode = None;
    let mut kept = Vec::with_capacity(args.len());
    for arg in args.drain(..) {
        if let Some(value) = arg.strip_prefix(COCKPIT_MODE_TRANSPORT_PREFIX) {
            let parsed = CockpitMode::parse(value)?;
            if mode.replace(parsed).is_some() {
                bail!("multiple cockpit mode overrides were provided");
            }
        } else {
            kept.push(arg);
        }
    }
    *args = kept;
    Ok(mode)
}

pub(crate) fn workspace_label_for_run(run_id: &str) -> String {
    format!("Khazad-Doom {run_id}")
}

#[derive(Debug, Clone)]
struct HerdrCockpitAdapter {
    bin: PathBuf,
}

impl HerdrCockpitAdapter {
    fn discover(mode: CockpitMode) -> std::result::Result<Self, CockpitUnavailable> {
        let Some(bin) = find_executable_in_path("herdr") else {
            return Err(CockpitUnavailable::new(
                mode,
                "herdr",
                "herdr binary was not found on PATH",
            ));
        };
        let adapter = Self { bin };
        adapter
            .run_command(&["--version".to_string()])
            .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))?;
        Ok(adapter)
    }

    fn run_json(&self, args: &[String]) -> Result<Value> {
        let output = self.run_command(args)?;
        serde_json::from_str(&output.stdout).with_context(|| {
            format!(
                "herdr {} did not return JSON: {}{}{}",
                display_args(args),
                bounded(&output.stdout),
                if output.stdout.is_empty() || output.stderr.is_empty() {
                    ""
                } else {
                    " | "
                },
                bounded(&output.stderr)
            )
        })
    }

    fn run_command(&self, args: &[String]) -> Result<CommandOutput> {
        run_command_with_timeout(&self.bin, args, HERDR_COMMAND_TIMEOUT)
    }

    fn first_pane_in_workspace(&self, workspace_id: &str) -> Result<CockpitPaneRef> {
        let value = self.run_json(&[
            "pane".to_string(),
            "list".to_string(),
            "--workspace".to_string(),
            workspace_id.to_string(),
        ])?;
        let pane_id = value
            .pointer("/result/panes")
            .and_then(Value::as_array)
            .and_then(|panes| panes.first())
            .and_then(|pane| pane.get("pane_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("herdr workspace {workspace_id} has no pane to anchor cockpit panes")
            })?;
        Ok(CockpitPaneRef {
            id: pane_id.to_string(),
        })
    }
}

impl CockpitAdapter for HerdrCockpitAdapter {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn open_or_focus_run_workspace(
        &self,
        request: &CockpitRunRequest,
    ) -> Result<CockpitWorkspaceRef> {
        let list = self.run_json(&["workspace".to_string(), "list".to_string()])?;
        if let Some(existing) = list
            .pointer("/result/workspaces")
            .and_then(Value::as_array)
            .and_then(|workspaces| {
                workspaces.iter().find(|workspace| {
                    workspace.get("label").and_then(Value::as_str)
                        == Some(request.workspace_label.as_str())
                })
            })
        {
            let workspace_id = existing
                .get("workspace_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("herdr workspace list item omitted workspace_id"))?;
            self.run_json(&[
                "workspace".to_string(),
                "focus".to_string(),
                workspace_id.to_string(),
            ])?;
            return Ok(CockpitWorkspaceRef {
                id: workspace_id.to_string(),
                anchor_pane: self.first_pane_in_workspace(workspace_id)?,
            });
        }

        let env_arg = format!("KHAZAD_HOME={}", request.khazad_home.to_string_lossy());
        let created = self.run_json(&[
            "workspace".to_string(),
            "create".to_string(),
            "--cwd".to_string(),
            request.repo_path.to_string_lossy().to_string(),
            "--label".to_string(),
            request.workspace_label.clone(),
            "--env".to_string(),
            env_arg,
            "--focus".to_string(),
        ])?;
        let workspace_id = created
            .pointer("/result/workspace/workspace_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr workspace create omitted workspace_id"))?;
        let root_pane = created
            .pointer("/result/root_pane/pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr workspace create omitted root_pane.pane_id"))?;
        Ok(CockpitWorkspaceRef {
            id: workspace_id.to_string(),
            anchor_pane: CockpitPaneRef {
                id: root_pane.to_string(),
            },
        })
    }

    fn create_read_only_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef> {
        let direction = if request.label == INTEGRATION_GATE_REPAIR_PANE {
            "down"
        } else {
            "right"
        };
        let mut args = vec![
            "pane".to_string(),
            "split".to_string(),
            workspace.anchor_pane.id.clone(),
            "--direction".to_string(),
            direction.to_string(),
            "--ratio".to_string(),
            "0.5".to_string(),
            "--cwd".to_string(),
            request.cwd.to_string_lossy().to_string(),
        ];
        for (key, value) in &request.env {
            args.push("--env".to_string());
            args.push(format!("{key}={value}"));
        }
        args.push("--no-focus".to_string());
        let split = self.run_json(&args)?;
        let pane_id = split
            .pointer("/result/pane/pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr pane split omitted pane_id"))?
            .to_string();
        self.run_json(&[
            "pane".to_string(),
            "rename".to_string(),
            pane_id.clone(),
            request.label.clone(),
        ])?;
        self.run_command(&[
            "pane".to_string(),
            "run".to_string(),
            pane_id.clone(),
            request.command.clone(),
        ])?;
        let _ = workspace.id.as_str();
        Ok(CockpitPaneRef { id: pane_id })
    }
}

#[derive(Debug)]
struct CommandOutput {
    stdout: String,
    stderr: String,
}

fn run_command_with_timeout(
    bin: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<CommandOutput> {
    let mut child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn herdr {}", display_args(args)))?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if output.status.success() {
                return Ok(CommandOutput { stdout, stderr });
            }
            bail!(
                "herdr {} exited with {}: {}{}{}",
                display_args(args),
                output.status,
                bounded(&stdout),
                if stdout.is_empty() || stderr.is_empty() {
                    ""
                } else {
                    " | "
                },
                bounded(&stderr)
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "herdr {} timed out after {}s: {}{}{}",
                display_args(args),
                timeout.as_secs(),
                bounded(&stdout),
                if stdout.is_empty() || stderr.is_empty() {
                    ""
                } else {
                    " | "
                },
                bounded(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let name = OsStr::new(name);
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn display_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn bounded(value: &str) -> String {
    const LIMIT: usize = 500;
    if value.len() <= LIMIT {
        value.to_string()
    } else {
        let prefix: String = value.chars().take(LIMIT).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Run, RunStatus, WorkflowConfig};
    use chrono::Utc;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeCockpitAdapter {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeCockpitAdapter {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CockpitAdapter for FakeCockpitAdapter {
        fn name(&self) -> &'static str {
            "fake-herdr"
        }

        fn open_or_focus_run_workspace(
            &self,
            request: &CockpitRunRequest,
        ) -> Result<CockpitWorkspaceRef> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("workspace:{}", request.workspace_label));
            Ok(CockpitWorkspaceRef {
                id: "workspace-1".to_string(),
                anchor_pane: CockpitPaneRef {
                    id: "pane-1".to_string(),
                },
            })
        }

        fn create_read_only_pane(
            &self,
            _workspace: &CockpitWorkspaceRef,
            request: &CockpitPaneRequest,
        ) -> Result<CockpitPaneRef> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("pane:{}:{}", request.label, request.command));
            Ok(CockpitPaneRef {
                id: format!("pane-{}", request.label.len()),
            })
        }
    }

    #[test]
    fn cockpit_opens_default_workspace_and_read_only_panes_without_planner() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-test"),
            feed_command: "khazad-doom monitor --run kd-test".to_string(),
            phase_command: "khazad-doom status --run kd-test --follow".to_string(),
        };

        let opened = Cockpit::new(CockpitMode::Auto, adapter.clone())
            .open_run(&request)
            .unwrap();

        assert_eq!(
            opened,
            CockpitLaunch::Opened(CockpitOpened {
                adapter: "fake-herdr".to_string(),
                mode: CockpitMode::Auto,
                workspace_label: "Khazad-Doom kd-test".to_string(),
                pane_labels: vec![
                    RUN_STATUS_FEED_PANE.to_string(),
                    INTEGRATION_GATE_REPAIR_PANE.to_string(),
                ],
            })
        );
        let calls = adapter.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "workspace:Khazad-Doom kd-test");
        assert!(calls[1].starts_with("pane:Run Status / Event Feed:"));
        assert!(calls[2].starts_with("pane:Integration Gate / Repair:"));
        assert!(
            calls
                .iter()
                .all(|call| !call.to_lowercase().contains("planner"))
        );
    }

    #[test]
    fn cockpit_direct_mode_skips_adapter() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-test"),
            feed_command: "feed".to_string(),
            phase_command: "phase".to_string(),
        };

        let launched = Cockpit::new(CockpitMode::Direct, adapter.clone())
            .open_run(&request)
            .unwrap();

        assert_eq!(launched, CockpitLaunch::SkippedDirect);
        assert!(adapter.calls().is_empty());
    }

    #[test]
    fn cockpit_config_defaults_auto_and_deserializes_durable_overrides() {
        assert_eq!(WorkflowConfig::default().cockpit, CockpitMode::Auto);

        let config: WorkflowConfig = serde_json::from_value(serde_json::json!({
            "cockpit": "direct"
        }))
        .unwrap();

        assert_eq!(config.cockpit, CockpitMode::Direct);
    }

    #[test]
    fn cockpit_mode_transport_round_trips_and_filters_pi_args() {
        let mut args = vec![
            "--foo".to_string(),
            cockpit_mode_transport_arg("herdr").unwrap(),
            "bar".to_string(),
        ];

        let mode = take_cockpit_mode_transport_arg(&mut args).unwrap();

        assert_eq!(mode, Some(CockpitMode::Herdr));
        assert_eq!(args, vec!["--foo".to_string(), "bar".to_string()]);
    }

    #[test]
    fn cockpit_request_uses_run_named_workspace_and_no_planner_command() {
        let now = Utc::now();
        let run = Run {
            id: "kd-123".to_string(),
            repo_id: "repo".to_string(),
            repo_path: "/tmp/repo".to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base".to_string(),
            integration_branch: "khazad/kd-123/integration".to_string(),
            selected_slice_id: "slice-1".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        };

        let request = CockpitRunRequest::for_run(&run, Path::new("/tmp/khazad-home"));

        assert_eq!(request.workspace_label, "Khazad-Doom kd-123");
        assert!(request.feed_command.contains("monitor --run kd-123"));
        assert!(
            request
                .phase_command
                .contains("status --run kd-123 --follow")
        );
        assert!(!request.feed_command.to_lowercase().contains("planner"));
        assert!(!request.phase_command.to_lowercase().contains("planner"));
    }
}
