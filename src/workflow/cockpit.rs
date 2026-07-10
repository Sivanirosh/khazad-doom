use super::events as workflow_events;
use crate::artifact;
use crate::domain::{
    AttentionNotificationRecord, CockpitMode, ReplanProposal, Run, WorkerQuestion,
    replan_decision_commands,
};
use crate::state::Store as StateStore;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const RUN_STATUS_FEED_PANE: &str = "Dashboard";
const WORKER_REGION_PLACEHOLDER_PANE: &str = "Worker region (pending)";
const COCKPIT_LAYOUT_MAX_WORKERS: usize = 4;
const COCKPIT_LAYOUT_WORKER_REGION_RATIO: &str = "0.68";
const COCKPIT_LAYOUT_WORKER_SPLIT_RATIO: &str = "0.50";
const COCKPIT_MODE_TRANSPORT_PREFIX: &str = "__khazad_cockpit_mode=";
const HERDR_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const HERDR_SPAWN_RETRY_ATTEMPTS: usize = 5;
const HERDR_SPAWN_RETRY_DELAY: Duration = Duration::from_millis(10);
const ATTENTION_PAYLOAD_SCHEMA_VERSION: u64 = 1;
const ATTENTION_DELIVERY_ADAPTER: &str = "herdr";
const ATTENTION_DELIVERY_SURFACE: &str = "agent_send";
static HERDR_LAYOUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct CockpitRunRequest {
    pub repo_path: PathBuf,
    pub khazad_home: PathBuf,
    pub workspace_label: String,
    pub feed_command: String,
}

impl CockpitRunRequest {
    pub fn for_run(run: &Run, khazad_home: &Path) -> Self {
        let binary = khazad_child_binary();
        let binary = shell_quote(&binary.to_string_lossy());
        let run_id = shell_quote(&run.id);
        Self {
            repo_path: PathBuf::from(&run.repo_path),
            khazad_home: khazad_home.to_path_buf(),
            workspace_label: workspace_label_for_run(&run.id),
            feed_command: format!("{binary} monitor --run {run_id} --interval-ms 1000"),
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
pub(crate) struct CockpitWorkerPaneRequest {
    pub run_id: String,
    pub slice_id: String,
    pub attempt: usize,
    pub command: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct CockpitTuiWorkerRequest {
    pub run_id: String,
    pub slice_id: String,
    pub attempt: usize,
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct CockpitWorkspaceRef {
    id: String,
    anchor_pane: Option<CockpitPaneRef>,
    existed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitPaneRef {
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CockpitLayoutDirection {
    Right,
    Down,
}

impl CockpitLayoutDirection {
    fn as_herdr_arg(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitDashboardLayout {
    pub name: String,
    pub region: String,
    pub split_from_slot: String,
    pub direction: CockpitLayoutDirection,
    pub ratio: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitWorkerSplit {
    pub anchor_slot: String,
    pub direction: CockpitLayoutDirection,
    pub ratio: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitWorkerSlot {
    pub index: usize,
    pub name: String,
    pub region: String,
    pub split: Option<CockpitWorkerSplit>,
}

impl CockpitWorkerSlot {
    fn pane_label(&self, worker_label: &str) -> String {
        format!("{}: {worker_label}", self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitLayoutPlan {
    pub worker_count: usize,
    pub dashboard: CockpitDashboardLayout,
    pub worker_slots: Vec<CockpitWorkerSlot>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CockpitLayoutPlanner;

impl CockpitLayoutPlanner {
    pub fn plan(&self, worker_count: usize) -> Result<CockpitLayoutPlan> {
        if !(1..=COCKPIT_LAYOUT_MAX_WORKERS).contains(&worker_count) {
            bail!(
                "cockpit layout v2 supports 1-{COCKPIT_LAYOUT_MAX_WORKERS} workers, got {worker_count}"
            );
        }

        let mut worker_slots = vec![CockpitWorkerSlot {
            index: 1,
            name: "worker-1".to_string(),
            region: "left-worker-region".to_string(),
            split: None,
        }];
        if worker_count >= 2 {
            worker_slots.push(CockpitWorkerSlot {
                index: 2,
                name: "worker-2".to_string(),
                region: "left-worker-region".to_string(),
                split: Some(CockpitWorkerSplit {
                    anchor_slot: "worker-1".to_string(),
                    direction: CockpitLayoutDirection::Right,
                    ratio: COCKPIT_LAYOUT_WORKER_SPLIT_RATIO.to_string(),
                }),
            });
        }
        if worker_count >= 3 {
            worker_slots.push(CockpitWorkerSlot {
                index: 3,
                name: "worker-3".to_string(),
                region: "left-worker-region".to_string(),
                split: Some(CockpitWorkerSplit {
                    anchor_slot: "worker-1".to_string(),
                    direction: CockpitLayoutDirection::Down,
                    ratio: COCKPIT_LAYOUT_WORKER_SPLIT_RATIO.to_string(),
                }),
            });
        }
        if worker_count >= 4 {
            worker_slots.push(CockpitWorkerSlot {
                index: 4,
                name: "worker-4".to_string(),
                region: "left-worker-region".to_string(),
                split: Some(CockpitWorkerSplit {
                    anchor_slot: "worker-2".to_string(),
                    direction: CockpitLayoutDirection::Down,
                    ratio: COCKPIT_LAYOUT_WORKER_SPLIT_RATIO.to_string(),
                }),
            });
        }

        Ok(CockpitLayoutPlan {
            worker_count,
            dashboard: CockpitDashboardLayout {
                name: RUN_STATUS_FEED_PANE.to_string(),
                region: "right-dashboard".to_string(),
                split_from_slot: "worker-1".to_string(),
                direction: CockpitLayoutDirection::Right,
                ratio: COCKPIT_LAYOUT_WORKER_REGION_RATIO.to_string(),
            },
            worker_slots,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CockpitLayoutPane {
    pub id: String,
    pub label: String,
    pub tab_id: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CockpitLayoutInspection {
    pub root_pane_id: Option<String>,
    pub panes: Vec<CockpitLayoutPane>,
}

impl CockpitLayoutInspection {
    fn dashboard_pane_id(&self) -> Option<&str> {
        self.panes
            .iter()
            .find(|pane| pane.label == RUN_STATUS_FEED_PANE)
            .map(|pane| pane.id.as_str())
    }

    fn worker_region_placeholder_pane_id(&self) -> Option<&str> {
        self.panes
            .iter()
            .find(|pane| pane.label == WORKER_REGION_PLACEHOLDER_PANE)
            .map(|pane| pane.id.as_str())
    }

    fn unlabeled_pane_id(&self) -> Option<&str> {
        self.panes
            .iter()
            .find(|pane| pane.label.trim().is_empty())
            .map(|pane| pane.id.as_str())
    }

    fn worker_slot_count(&self) -> usize {
        self.panes
            .iter()
            .filter_map(|pane| worker_slot_index_from_label(&pane.label))
            .max()
            .unwrap_or(0)
    }

    fn worker_slot_pane_id(&self, slot_name: &str) -> Option<&str> {
        let prefix = format!("{slot_name}: ");
        self.panes
            .iter()
            .find(|pane| pane.label.starts_with(&prefix))
            .map(|pane| pane.id.as_str())
    }

    fn label_for_pane(&self, pane_id: &str) -> Option<&str> {
        self.panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .map(|pane| pane.label.as_str())
    }

    fn tab_id_for_pane(&self, pane_id: &str) -> Option<&str> {
        self.panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .map(|pane| pane.tab_id.as_str())
            .filter(|tab_id| !tab_id.is_empty())
    }

    fn live_layout_anchor_pane_id(&self, preferred_anchor_id: Option<&str>) -> Option<&str> {
        self.worker_region_placeholder_pane_id()
            .or_else(|| self.worker_slot_pane_id("worker-1"))
            .or_else(|| {
                preferred_anchor_id.and_then(|pane_id| {
                    self.panes
                        .iter()
                        .find(|pane| pane.id == pane_id && pane.label.trim().is_empty())
                        .map(|pane| pane.id.as_str())
                })
            })
            .or_else(|| self.unlabeled_pane_id())
            .or_else(|| self.dashboard_pane_id())
            .or_else(|| {
                preferred_anchor_id.and_then(|pane_id| {
                    self.panes
                        .iter()
                        .find(|pane| pane.id == pane_id)
                        .map(|pane| pane.id.as_str())
                })
            })
            .or_else(|| self.panes.first().map(|pane| pane.id.as_str()))
    }

    fn slot_one_reusable_pane_id(&self, preferred_anchor_id: Option<&str>) -> Option<&str> {
        self.worker_region_placeholder_pane_id()
            .or_else(|| self.worker_slot_pane_id("worker-1"))
            .or_else(|| {
                preferred_anchor_id.and_then(|pane_id| {
                    self.panes
                        .iter()
                        .find(|pane| pane.id == pane_id && pane.label.trim().is_empty())
                        .map(|pane| pane.id.as_str())
                })
            })
            .or_else(|| self.unlabeled_pane_id())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CockpitSlotOneTarget {
    pane_id: String,
    tab_id: String,
    close_after_move: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitWorkerPlacement {
    pub pane: CockpitPaneRef,
    pub slot_name: String,
    pub slot_index: usize,
    pub slot_region: String,
    pub pane_label: String,
}

fn worker_slot_index_from_label(label: &str) -> Option<usize> {
    let rest = label.strip_prefix("worker-")?;
    let (index, _) = rest.split_once(':')?;
    index.parse().ok()
}

fn pane_command_with_env(request: &CockpitPaneRequest) -> String {
    if request.env.is_empty() {
        return request.command.clone();
    }
    let assignments = request
        .env
        .iter()
        .map(|(key, value)| shell_quote(&format!("{key}={value}")))
        .collect::<Vec<_>>()
        .join(" ");
    format!("env {assignments} {}", request.command)
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
pub(crate) struct CockpitOpenFocus {
    pub adapter: String,
    pub mode: CockpitMode,
    pub workspace_label: String,
    pub action: String,
    pub pane_labels: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitWorkerOpened {
    pub adapter: String,
    pub mode: CockpitMode,
    pub workspace_label: String,
    pub pane_label: String,
    pub pane_id: String,
    pub slot_name: String,
    pub slot_index: usize,
    pub slot_region: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CockpitWorkerLaunch {
    Opened(CockpitWorkerOpened),
    SkippedDirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitTuiWorkerOpened {
    pub adapter: String,
    pub mode: CockpitMode,
    pub workspace_label: String,
    pub agent_name: String,
    pub pane_label: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub slot_name: String,
    pub slot_index: usize,
    pub slot_region: String,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CockpitTuiWorkerLaunch {
    Opened(CockpitTuiWorkerOpened),
    SkippedDirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitAgentMessageSent {
    pub adapter: String,
    pub mode: CockpitMode,
    pub target: String,
    pub surface: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitAgentFocused {
    pub adapter: String,
    pub mode: CockpitMode,
    pub target: String,
    pub surface: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitAgentRenamed {
    pub adapter: String,
    pub mode: CockpitMode,
    pub target: String,
    pub surface: String,
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

#[allow(dead_code)]
pub(crate) fn gate_activity_pane_command(run_id: &str) -> String {
    let binary = khazad_child_binary();
    let painter_command = format!(
        "{} cockpit paint-gate-activity --run {} --interval-ms 1000",
        shell_quote(&binary.to_string_lossy()),
        shell_quote(run_id),
    );
    let script = format!(
        "{painter_command}; khazad_painter_status=$?; if [ \"$khazad_painter_status\" -ne 0 ]; then printf '%s\\n' '[khazad] gate/repair activity painter exited non-fatally; daemon gate artifacts remain authoritative' >&2; fi; exit 0"
    );
    format!("/bin/sh -c {}", shell_quote(&script))
}

pub(crate) fn worker_activity_pane_command(
    wrapper_command: &str,
    stdout_path: &Path,
    status_path: &Path,
    exit_path: &Path,
) -> String {
    let binary = khazad_child_binary();
    let painter_command = format!(
        "{} cockpit paint-worker-activity --stdout {} --status {} --exit {}",
        shell_quote(&binary.to_string_lossy()),
        shell_quote(&stdout_path.to_string_lossy()),
        shell_quote(&status_path.to_string_lossy()),
        shell_quote(&exit_path.to_string_lossy()),
    );
    let script = format!(
        "({wrapper_command}) & khazad_wrapper_pid=$!; {painter_command}; khazad_painter_status=$?; if [ \"$khazad_painter_status\" -ne 0 ]; then printf '%s\\n' '[khazad] worker activity painter exited non-fatally; wrapper artifacts remain authoritative' >&2; fi; wait \"$khazad_wrapper_pid\"; exit $?"
    );
    format!("/bin/sh -c {}", shell_quote(&script))
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
    fn create_worker_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef> {
        self.create_read_only_pane(workspace, request)
    }

    fn inspect_layout(&self, _workspace: &CockpitWorkspaceRef) -> Result<CockpitLayoutInspection> {
        bail!(
            "{} adapter does not support cockpit layout inspection",
            self.name()
        )
    }

    fn ensure_dashboard_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        _dashboard: &CockpitDashboardLayout,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef> {
        self.create_read_only_pane(workspace, request)
    }

    fn cleanup_placeholder_root_pane(
        &self,
        _workspace: &CockpitWorkspaceRef,
        _slot: &CockpitWorkerSlot,
        _replacement_label: &str,
    ) -> Result<()> {
        Ok(())
    }

    fn place_worker_slot_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        _plan: &CockpitLayoutPlan,
        slot: &CockpitWorkerSlot,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitWorkerPlacement> {
        let pane = self.create_worker_pane(workspace, request)?;
        Ok(CockpitWorkerPlacement {
            pane,
            slot_name: slot.name.clone(),
            slot_index: slot.index,
            slot_region: slot.region.clone(),
            pane_label: request.label.clone(),
        })
    }

    fn start_tui_worker_agent_in_slot(
        &self,
        workspace: &CockpitWorkspaceRef,
        _plan: &CockpitLayoutPlan,
        slot: &CockpitWorkerSlot,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        let mut opened = self.start_tui_worker_agent(workspace, request)?;
        opened.pane_label = slot.pane_label(&worker_pane_label(
            &request.run_id,
            &request.slice_id,
            request.attempt,
        ));
        opened.slot_name = slot.name.clone();
        opened.slot_index = slot.index;
        opened.slot_region = slot.region.clone();
        Ok(opened)
    }

    fn start_tui_worker_agent(
        &self,
        _workspace: &CockpitWorkspaceRef,
        _request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        bail!("{} adapter does not support TUI worker agents", self.name())
    }

    fn close_pane(&self, _pane_id: &str) -> Result<()> {
        bail!("{} adapter does not support closing panes", self.name())
    }

    fn send_agent_message(&self, _target: &str, _text: &str) -> Result<()> {
        bail!("{} adapter does not support agent messages", self.name())
    }

    fn focus_agent(&self, _target: &str) -> Result<()> {
        bail!("{} adapter does not support agent focus", self.name())
    }

    fn rename_agent(&self, _target: &str, _name: &str) -> Result<()> {
        bail!("{} adapter does not support agent rename", self.name())
    }
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

    fn dashboard_pane_request(&self, request: &CockpitRunRequest) -> CockpitPaneRequest {
        CockpitPaneRequest {
            label: RUN_STATUS_FEED_PANE.to_string(),
            command: request.feed_command.clone(),
            cwd: request.repo_path.clone(),
            env: vec![
                (
                    "KHAZAD_HOME".to_string(),
                    request.khazad_home.to_string_lossy().to_string(),
                ),
                ("KHAZAD_COCKPIT_READ_ONLY".to_string(), "1".to_string()),
                (
                    "KHAZAD_COCKPIT_SOURCE_OF_TRUTH".to_string(),
                    "daemon_state".to_string(),
                ),
            ],
        }
    }

    fn create_run_panes(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitRunRequest,
    ) -> Result<Vec<String>> {
        let plan = CockpitLayoutPlanner.plan(1)?;
        let dashboard = self.dashboard_pane_request(request);
        self.adapter.inspect_layout(workspace)?;
        self.adapter
            .ensure_dashboard_pane(workspace, &plan.dashboard, &dashboard)?;
        Ok(vec![dashboard.label])
    }

    pub fn open_run(&self, request: &CockpitRunRequest) -> Result<CockpitLaunch> {
        if self.mode == CockpitMode::Direct {
            return Ok(CockpitLaunch::SkippedDirect);
        }
        let workspace = self.adapter.open_or_focus_run_workspace(request)?;
        let pane_labels = self.create_run_panes(&workspace, request)?;
        Ok(CockpitLaunch::Opened(CockpitOpened {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            workspace_label: request.workspace_label.clone(),
            pane_labels,
        }))
    }

    pub fn open_or_focus_run(&self, request: &CockpitRunRequest) -> Result<CockpitOpenFocus> {
        if self.mode == CockpitMode::Direct {
            bail!("cockpit direct mode does not open a Herdr workspace");
        }
        let workspace = self.adapter.open_or_focus_run_workspace(request)?;
        if workspace.existed {
            return Ok(CockpitOpenFocus {
                adapter: self.adapter.name().to_string(),
                mode: self.mode,
                workspace_label: request.workspace_label.clone(),
                action: "focused_existing".to_string(),
                pane_labels: Vec::new(),
                message: "focused existing Herdr cockpit workspace".to_string(),
            });
        }
        let pane_labels = self.create_run_panes(&workspace, request)?;
        Ok(CockpitOpenFocus {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            workspace_label: request.workspace_label.clone(),
            action: "opened".to_string(),
            pane_labels,
            message: "opened Herdr cockpit workspace backed by daemon monitor/status commands"
                .to_string(),
        })
    }

    pub fn open_worker_pane(
        &self,
        run_request: &CockpitRunRequest,
        worker_request: &CockpitWorkerPaneRequest,
    ) -> Result<CockpitWorkerLaunch> {
        if self.mode == CockpitMode::Direct {
            return Ok(CockpitWorkerLaunch::SkippedDirect);
        }
        let _layout_guard = herdr_layout_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let workspace = self.adapter.open_or_focus_run_workspace(run_request)?;
        let inspection = self.adapter.inspect_layout(&workspace)?;
        let plan = CockpitLayoutPlanner.plan(inspection.worker_slot_count() + 1)?;
        let dashboard = self.dashboard_pane_request(run_request);
        self.adapter
            .ensure_dashboard_pane(&workspace, &plan.dashboard, &dashboard)?;
        let slot = plan
            .worker_slots
            .last()
            .ok_or_else(|| anyhow!("cockpit layout plan omitted worker slot"))?;
        let pane = CockpitPaneRequest {
            label: worker_pane_label(
                &worker_request.run_id,
                &worker_request.slice_id,
                worker_request.attempt,
            ),
            command: worker_request.command.clone(),
            cwd: worker_request.cwd.clone(),
            env: worker_request.env.clone(),
        };
        let stable_label = slot.pane_label(&pane.label);
        self.adapter
            .cleanup_placeholder_root_pane(&workspace, slot, &stable_label)?;
        let placement = self
            .adapter
            .place_worker_slot_pane(&workspace, &plan, slot, &pane)?;
        Ok(CockpitWorkerLaunch::Opened(CockpitWorkerOpened {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            workspace_label: run_request.workspace_label.clone(),
            pane_label: placement.pane_label,
            pane_id: placement.pane.id,
            slot_name: placement.slot_name,
            slot_index: placement.slot_index,
            slot_region: placement.slot_region,
        }))
    }

    pub fn open_tui_worker_agent(
        &self,
        run_request: &CockpitRunRequest,
        worker_request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerLaunch> {
        if self.mode == CockpitMode::Direct {
            return Ok(CockpitTuiWorkerLaunch::SkippedDirect);
        }
        let _layout_guard = herdr_layout_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let workspace = self.adapter.open_or_focus_run_workspace(run_request)?;
        let inspection = self.adapter.inspect_layout(&workspace)?;
        let plan = CockpitLayoutPlanner.plan(inspection.worker_slot_count() + 1)?;
        let dashboard = self.dashboard_pane_request(run_request);
        self.adapter
            .ensure_dashboard_pane(&workspace, &plan.dashboard, &dashboard)?;
        let slot = plan
            .worker_slots
            .last()
            .ok_or_else(|| anyhow!("cockpit layout plan omitted TUI worker slot"))?;
        let mut opened =
            self.adapter
                .start_tui_worker_agent_in_slot(&workspace, &plan, slot, worker_request)?;
        opened.mode = self.mode;
        opened.workspace_label = run_request.workspace_label.clone();
        Ok(CockpitTuiWorkerLaunch::Opened(opened))
    }

    pub fn close_pane(&self, pane_id: &str) -> Result<()> {
        if self.mode == CockpitMode::Direct {
            bail!("cockpit direct mode does not close Herdr panes");
        }
        self.adapter.close_pane(pane_id)
    }

    pub fn send_agent_message(&self, target: &str, text: &str) -> Result<CockpitAgentMessageSent> {
        if self.mode == CockpitMode::Direct {
            bail!("cockpit direct mode does not send Herdr agent messages");
        }
        self.adapter.send_agent_message(target, text)?;
        Ok(CockpitAgentMessageSent {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            target: target.to_string(),
            surface: "herdr agent send".to_string(),
        })
    }

    pub fn focus_agent(&self, target: &str) -> Result<CockpitAgentFocused> {
        if self.mode == CockpitMode::Direct {
            bail!("cockpit direct mode does not focus Herdr agents");
        }
        self.adapter.focus_agent(target)?;
        Ok(CockpitAgentFocused {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            target: target.to_string(),
            surface: "herdr agent focus".to_string(),
        })
    }

    pub fn rename_agent(&self, target: &str, name: &str) -> Result<CockpitAgentRenamed> {
        if self.mode == CockpitMode::Direct {
            bail!("cockpit direct mode does not rename Herdr agents");
        }
        self.adapter.rename_agent(target, name)?;
        Ok(CockpitAgentRenamed {
            adapter: self.adapter.name().to_string(),
            mode: self.mode,
            target: target.to_string(),
            surface: "herdr agent rename".to_string(),
        })
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

pub(crate) fn open_default_run_cockpit_for_operator(
    run: &Run,
    khazad_home: &Path,
) -> std::result::Result<CockpitOpenFocus, CockpitUnavailable> {
    let mode = CockpitMode::Herdr;
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    let request = CockpitRunRequest::for_run(run, khazad_home);
    Cockpit::new(mode, adapter)
        .open_or_focus_run(&request)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn open_default_worker_pane(
    run: &Run,
    mode: CockpitMode,
    khazad_home: &Path,
    worker_request: &CockpitWorkerPaneRequest,
) -> std::result::Result<CockpitWorkerLaunch, CockpitUnavailable> {
    #[cfg(test)]
    if std::env::var("KHAZAD_UNIT_TEST_COCKPIT").ok().as_deref() != Some("1") {
        return Ok(CockpitWorkerLaunch::SkippedDirect);
    }
    if mode == CockpitMode::Direct {
        return Ok(CockpitWorkerLaunch::SkippedDirect);
    }
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    let request = CockpitRunRequest::for_run(run, khazad_home);
    Cockpit::new(mode, adapter)
        .open_worker_pane(&request, worker_request)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn open_default_tui_worker_agent(
    run: &Run,
    mode: CockpitMode,
    khazad_home: &Path,
    worker_request: &CockpitTuiWorkerRequest,
) -> std::result::Result<CockpitTuiWorkerLaunch, CockpitUnavailable> {
    #[cfg(test)]
    if std::env::var("KHAZAD_UNIT_TEST_COCKPIT").ok().as_deref() != Some("1") {
        return Ok(CockpitTuiWorkerLaunch::SkippedDirect);
    }
    if mode == CockpitMode::Direct {
        return Ok(CockpitTuiWorkerLaunch::SkippedDirect);
    }
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    let request = CockpitRunRequest::for_run(run, khazad_home);
    Cockpit::new(mode, adapter)
        .open_tui_worker_agent(&request, worker_request)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn close_default_pane(pane_id: &str) -> std::result::Result<(), CockpitUnavailable> {
    let mode = CockpitMode::Herdr;
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    Cockpit::new(mode, adapter)
        .close_pane(pane_id)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn send_default_agent_message(
    target: &str,
    text: &str,
) -> std::result::Result<CockpitAgentMessageSent, CockpitUnavailable> {
    let mode = CockpitMode::Herdr;
    #[cfg(test)]
    if std::env::var("KHAZAD_UNIT_TEST_TERMINAL_FEEDBACK")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Err(CockpitUnavailable::new(
            mode,
            "herdr",
            "Herdr agent message delivery is disabled in unit tests",
        ));
    }
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    Cockpit::new(mode, adapter)
        .send_agent_message(target, text)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn focus_default_agent_target(
    target: &str,
) -> std::result::Result<CockpitAgentFocused, CockpitUnavailable> {
    let mode = CockpitMode::Herdr;
    #[cfg(test)]
    if std::env::var("KHAZAD_UNIT_TEST_TERMINAL_FEEDBACK")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Err(CockpitUnavailable::new(
            mode,
            "herdr",
            "Herdr agent focus is disabled in unit tests",
        ));
    }
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    Cockpit::new(mode, adapter)
        .focus_agent(target)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn notify_origin_worker_question_attention(
    state: &StateStore,
    question: &WorkerQuestion,
) {
    let Ok(Some(run)) = state.get_run(&question.run_id) else {
        return;
    };
    let attention_key = format!("worker-question:{}", question.id);
    let answer_commands = vec![format!(
        "khazad-doom answer {} {} <answer>",
        question.run_id, question.id
    )];
    let status_commands = origin_attention_status_commands(&question.run_id);
    let operator_commands = answer_commands
        .iter()
        .chain(status_commands.iter())
        .cloned()
        .collect::<Vec<_>>();
    let payload = json!({
        "schema_version": ATTENTION_PAYLOAD_SCHEMA_VERSION,
        "kind": "worker_question_pending",
        "attention_key": attention_key,
        "run_id": question.run_id,
        "slice_id": question.slice_id,
        "attempt": question.attempt,
        "question_id": question.id,
        "reason": format!("Worker question {} is awaiting an operator answer for slice {}.", question.id, question.slice_id),
        "question": question.question,
        "options": question.options,
        "timeout_seconds": question.timeout_seconds,
        "deadline_at": origin_attention_worker_question_deadline(question),
        "recommended_answer": question.recommended_answer,
        "recommendation_rationale": question.recommendation_rationale,
        "bounded_within_current_slice_or_mission_authority": question.bounded_within_current_slice_or_mission_authority,
        "reversible": question.reversible,
        "fallback_eligible": question.fallback_eligible,
        "answer_command": answer_commands[0],
        "answer_commands": answer_commands,
        "status_commands": status_commands,
        "operator_commands": operator_commands,
        "source_of_truth": "daemon_worker_questions",
        "delivery_semantics": "visibility_only_no_auto_decision",
    });
    send_origin_attention(
        state,
        OriginAttentionRequest {
            run: &run,
            attention_key: &attention_key,
            attention_kind: "worker_question_pending",
            payload,
            source_of_truth: "daemon_worker_questions",
            question_id: &question.id,
            slice_id: &question.slice_id,
            proposal_id: "",
            delivery_message: "worker question notification was not delivered",
            focus_message: "worker question focus was not delivered",
        },
    );
}

pub(crate) fn notify_origin_replan_attention(
    state: &StateStore,
    run: &Run,
    proposal: &ReplanProposal,
) {
    let attention_key = format!("replan-proposal:{}", proposal.id);
    let decision_commands = replan_decision_commands(&run.id, &proposal.id);
    let status_commands = origin_attention_status_commands(&run.id);
    let operator_commands = decision_commands
        .iter()
        .chain(status_commands.iter())
        .cloned()
        .collect::<Vec<_>>();
    let mut payload = serde_json::to_value(workflow_events::ReplanNotificationPayload::new(
        &run.id,
        &proposal.id,
        proposal.source.clone(),
        &proposal.risk,
        proposal.proposed_changes.clone(),
        decision_commands,
    ))
    .unwrap_or(Value::Null);
    if let Value::Object(fields) = &mut payload {
        fields.insert("attention_key".to_string(), json!(attention_key));
        fields.insert(
            "reason".to_string(),
            json!(format!(
                "Replan proposal {} is pending an operator decision.",
                proposal.id
            )),
        );
        fields.insert("status_commands".to_string(), json!(status_commands));
        fields.insert("operator_commands".to_string(), json!(operator_commands));
        fields.insert(
            "delivery_semantics".to_string(),
            json!("visibility_only_no_auto_decision"),
        );
    }
    send_origin_attention(
        state,
        OriginAttentionRequest {
            run,
            attention_key: &attention_key,
            attention_kind: "replan_decision_pending",
            payload,
            source_of_truth: "daemon_replan_proposals",
            question_id: "",
            slice_id: "",
            proposal_id: &proposal.id,
            delivery_message: "replan proposal notification was not delivered",
            focus_message: "replan proposal focus was not delivered",
        },
    );
}

struct OriginAttentionRequest<'a> {
    run: &'a Run,
    attention_key: &'a str,
    attention_kind: &'static str,
    payload: Value,
    source_of_truth: &'static str,
    question_id: &'a str,
    slice_id: &'a str,
    proposal_id: &'a str,
    delivery_message: &'static str,
    focus_message: &'static str,
}

fn send_origin_attention(state: &StateStore, request: OriginAttentionRequest<'_>) {
    let store = artifact::Store::new(&request.run.repo_path);
    if origin_attention_record_exists(&store, &request.run.id, request.attention_key) {
        return;
    }
    let origin = match store.read_origin_notification_target(&request.run.id) {
        Ok(Some(origin)) if !origin.target.trim().is_empty() => origin,
        Ok(_) => return,
        Err(err) => {
            let _ = state.record_event(
                &request.run.id,
                workflow_events::RUN_INCIDENT,
                &origin_attention_failure_payload(
                    &request,
                    "attention_notification_failed",
                    "origin_target_read_failed",
                    format!(
                        "{}: origin target read failed: {err}",
                        request.delivery_message
                    ),
                ),
            );
            return;
        }
    };
    let created_at = Utc::now().to_rfc3339();
    if !write_origin_attention_record(
        state,
        &store,
        &origin,
        &request,
        "pending",
        "pending",
        "pending",
        "",
        request.payload.clone(),
        created_at.clone(),
    ) {
        return;
    }

    let text = serde_json::to_string_pretty(&request.payload)
        .unwrap_or_else(|_| request.payload.to_string());
    let mut send_status = "failed";
    let mut focus_status = "failed";
    let mut errors = Vec::new();
    match send_default_agent_message(&origin.target, &text) {
        Ok(sent) => {
            send_status = "sent";
            let _ = state.record_event(
                &request.run.id,
                workflow_events::ATTENTION_NOTIFICATION_SENT,
                &workflow_events::AttentionDeliveryPayload {
                    kind: request.attention_kind.to_string(),
                    question_id: request.question_id.to_string(),
                    slice_id: request.slice_id.to_string(),
                    proposal_id: request.proposal_id.to_string(),
                    adapter: sent.adapter,
                    surface: sent.surface,
                    target_kind: origin.target_kind.clone(),
                },
            );
        }
        Err(err) => {
            errors.push(format!("send: {}", err.message));
            let _ = state.record_event(
                &request.run.id,
                workflow_events::RUN_INCIDENT,
                &origin_attention_failure_payload(
                    &request,
                    "attention_notification_failed",
                    "delivery_failed",
                    format!("{}: {}", request.delivery_message, err.message),
                ),
            );
        }
    }
    match focus_default_agent_target(&origin.target) {
        Ok(focused) => {
            focus_status = "sent";
            let _ = state.record_event(
                &request.run.id,
                workflow_events::ATTENTION_FOCUS_SENT,
                &workflow_events::AttentionDeliveryPayload {
                    kind: request.attention_kind.to_string(),
                    question_id: request.question_id.to_string(),
                    slice_id: request.slice_id.to_string(),
                    proposal_id: request.proposal_id.to_string(),
                    adapter: focused.adapter,
                    surface: focused.surface,
                    target_kind: origin.target_kind.clone(),
                },
            );
        }
        Err(err) => {
            errors.push(format!("focus: {}", err.message));
            let _ = state.record_event(
                &request.run.id,
                workflow_events::RUN_INCIDENT,
                &origin_attention_failure_payload(
                    &request,
                    "attention_focus_failed",
                    "focus_failed",
                    format!("{}: {}", request.focus_message, err.message),
                ),
            );
        }
    }
    let delivery_status = match (send_status, focus_status) {
        ("sent", "sent") => "sent",
        ("failed", "failed") => "failed",
        _ => "partial",
    };
    write_origin_attention_record(
        state,
        &store,
        &origin,
        &request,
        delivery_status,
        send_status,
        focus_status,
        &errors.join("; "),
        request.payload.clone(),
        created_at,
    );
}

#[allow(clippy::too_many_arguments)]
fn write_origin_attention_record(
    state: &StateStore,
    store: &artifact::Store,
    origin: &crate::domain::OriginNotificationTarget,
    request: &OriginAttentionRequest<'_>,
    delivery_status: &str,
    send_status: &str,
    focus_status: &str,
    error: &str,
    payload: Value,
    created_at: String,
) -> bool {
    let record = AttentionNotificationRecord {
        schema_version: ATTENTION_PAYLOAD_SCHEMA_VERSION,
        run_id: request.run.id.clone(),
        attention_key: request.attention_key.to_string(),
        attention_kind: request.attention_kind.to_string(),
        delivery_status: delivery_status.to_string(),
        send_status: send_status.to_string(),
        focus_status: focus_status.to_string(),
        question_id: request.question_id.to_string(),
        slice_id: request.slice_id.to_string(),
        proposal_id: request.proposal_id.to_string(),
        origin_target: origin.target.clone(),
        delivery_adapter: ATTENTION_DELIVERY_ADAPTER.to_string(),
        delivery_surface: ATTENTION_DELIVERY_SURFACE.to_string(),
        error: error.to_string(),
        payload,
        created_at,
    };
    let path = origin_attention_record_path(store, &request.run.id, request.attention_key);
    if let Err(err) = artifact::write_json(path, &record) {
        let _ = state.record_event(
            &request.run.id,
            workflow_events::RUN_INCIDENT,
            &origin_attention_failure_payload(
                request,
                "attention_notification_record_failed",
                "record_write_failed",
                format!(
                    "attention notification record for {} was not written: {err}",
                    request.attention_key
                ),
            ),
        );
        return false;
    }
    true
}

fn origin_attention_failure_payload(
    request: &OriginAttentionRequest<'_>,
    kind: &str,
    visibility_kind: &str,
    message: String,
) -> workflow_events::RunIncidentPayload {
    let mut payload = workflow_events::RunIncidentPayload::warning(kind, message)
        .with_extra("visibility_kind", visibility_kind)
        .with_extra("attention_key", request.attention_key)
        .with_extra("attention_kind", request.attention_kind)
        .with_extra("source_of_truth", request.source_of_truth);
    if !request.question_id.trim().is_empty() {
        payload = payload.with_extra("question_id", request.question_id);
    }
    if !request.slice_id.trim().is_empty() {
        payload = payload.with_extra("slice_id", request.slice_id);
    }
    if !request.proposal_id.trim().is_empty() {
        payload = payload.with_extra("proposal_id", request.proposal_id);
    }
    payload
}

fn origin_attention_record_exists(
    store: &artifact::Store,
    run_id: &str,
    attention_key: &str,
) -> bool {
    origin_attention_record_path(store, run_id, attention_key).exists()
}

fn origin_attention_record_path(
    store: &artifact::Store,
    run_id: &str,
    attention_key: &str,
) -> PathBuf {
    store.notifications_dir(run_id).join(format!(
        "attention-{}.json",
        origin_attention_safe_segment(attention_key)
    ))
}

fn origin_attention_safe_segment(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

fn origin_attention_status_commands(run_id: &str) -> Vec<String> {
    vec![
        format!("khazad-doom status --run {run_id}"),
        format!("khazad-doom monitor --run {run_id}"),
        format!("khazad-doom watch --run {run_id}"),
    ]
}

fn origin_attention_worker_question_deadline(question: &WorkerQuestion) -> Option<String> {
    question.deadline_at.map(|deadline| deadline.to_rfc3339())
}

pub(crate) fn rename_default_agent_target(
    target: &str,
    name: &str,
) -> std::result::Result<CockpitAgentRenamed, CockpitUnavailable> {
    let mode = CockpitMode::Herdr;
    #[cfg(test)]
    if std::env::var("KHAZAD_UNIT_TEST_TERMINAL_FEEDBACK")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Err(CockpitUnavailable::new(
            mode,
            "herdr",
            "Herdr agent rename is disabled in unit tests",
        ));
    }
    let adapter = HerdrCockpitAdapter::discover(mode)?;
    Cockpit::new(mode, adapter)
        .rename_agent(target, name)
        .map_err(|err| CockpitUnavailable::new(mode, "herdr", err.to_string()))
}

pub(crate) fn worker_pane_label(run_id: &str, slice_id: &str, attempt: usize) -> String {
    format!("Worker {run_id}/{slice_id} attempt {attempt}")
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

#[derive(Debug, Clone)]
struct HerdrTempTab {
    tab_id: String,
    root_pane_id: String,
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

    fn pane_list(&self, workspace_id: &str) -> Result<Vec<CockpitLayoutPane>> {
        let value = self.run_json(&[
            "pane".to_string(),
            "list".to_string(),
            "--workspace".to_string(),
            workspace_id.to_string(),
        ])?;
        Ok(value
            .pointer("/result/panes")
            .and_then(Value::as_array)
            .map(|panes| {
                panes
                    .iter()
                    .filter_map(|pane| {
                        let id = pane.get("pane_id").and_then(Value::as_str)?;
                        let label = pane
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let tab_id = pane
                            .get("tab_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        Some(CockpitLayoutPane {
                            id: id.to_string(),
                            label: label.to_string(),
                            tab_id: tab_id.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn preferred_anchor_id<'a>(&self, workspace: &'a CockpitWorkspaceRef) -> Option<&'a str> {
        workspace.anchor_pane.as_ref().map(|pane| pane.id.as_str())
    }

    fn select_live_layout_anchor(
        &self,
        workspace: &CockpitWorkspaceRef,
        panes: Vec<CockpitLayoutPane>,
    ) -> Result<(String, Vec<CockpitLayoutPane>)> {
        let inspection = CockpitLayoutInspection {
            root_pane_id: None,
            panes,
        };
        let anchor_id = inspection
            .live_layout_anchor_pane_id(self.preferred_anchor_id(workspace))
            .ok_or_else(|| {
                anyhow!(
                    "herdr workspace {} has no live pane to anchor cockpit panes",
                    workspace.id
                )
            })?
            .to_string();
        Ok((anchor_id, inspection.panes))
    }

    fn live_layout_anchor_pane_id(&self, workspace: &CockpitWorkspaceRef) -> Result<String> {
        let panes = self.pane_list(&workspace.id)?;
        self.select_live_layout_anchor(workspace, panes)
            .map(|(anchor_id, _)| anchor_id)
    }

    fn slot_one_target(
        &self,
        workspace: &CockpitWorkspaceRef,
        inspection: &CockpitLayoutInspection,
    ) -> Result<CockpitSlotOneTarget> {
        let pane_id = if let Some(pane_id) =
            inspection.slot_one_reusable_pane_id(self.preferred_anchor_id(workspace))
        {
            pane_id
        } else {
            inspection.dashboard_pane_id().ok_or_else(|| {
                anyhow!("cockpit layout has no live pane available for TUI worker slot 1")
            })?
        };
        let tab_id = inspection
            .tab_id_for_pane(pane_id)
            .ok_or_else(|| anyhow!("cockpit layout slot 1 target pane omitted tab id"))?;
        Ok(CockpitSlotOneTarget {
            pane_id: pane_id.to_string(),
            tab_id: tab_id.to_string(),
            close_after_move: inspection
                .slot_one_reusable_pane_id(self.preferred_anchor_id(workspace))
                == Some(pane_id),
        })
    }

    fn split_pane(
        &self,
        anchor_pane_id: &str,
        direction: CockpitLayoutDirection,
        ratio: &str,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef> {
        let mut args = vec![
            "pane".to_string(),
            "split".to_string(),
            anchor_pane_id.to_string(),
            "--direction".to_string(),
            direction.as_herdr_arg().to_string(),
            "--ratio".to_string(),
            ratio.to_string(),
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
        Ok(CockpitPaneRef { id: pane_id })
    }

    fn rename_pane(&self, pane_id: &str, label: &str) -> Result<()> {
        self.run_json(&[
            "pane".to_string(),
            "rename".to_string(),
            pane_id.to_string(),
            label.to_string(),
        ])?;
        Ok(())
    }

    fn run_pane(&self, pane_id: &str, command: &str) -> Result<()> {
        self.run_command(&[
            "pane".to_string(),
            "run".to_string(),
            pane_id.to_string(),
            command.to_string(),
        ])?;
        Ok(())
    }

    fn run_pane_request(&self, pane_id: &str, request: &CockpitPaneRequest) -> Result<()> {
        self.run_pane_request_with_label(pane_id, &request.label, request)
    }

    fn run_pane_request_with_label(
        &self,
        pane_id: &str,
        label: &str,
        request: &CockpitPaneRequest,
    ) -> Result<()> {
        self.rename_pane(pane_id, label)?;
        self.run_pane(pane_id, &pane_command_with_env(request))
    }

    fn create_temp_tui_worker_tab(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<HerdrTempTab> {
        let created = self.run_json(&[
            "tab".to_string(),
            "create".to_string(),
            "--workspace".to_string(),
            workspace.id.clone(),
            "--cwd".to_string(),
            request.cwd.to_string_lossy().to_string(),
            "--label".to_string(),
            format!("{} staging", request.name),
            "--no-focus".to_string(),
        ])?;
        let tab_id = created
            .pointer("/result/tab/tab_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr tab create omitted tab_id"))?
            .to_string();
        let root_pane_id = created
            .pointer("/result/root_pane/pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr tab create omitted root_pane.pane_id"))?
            .to_string();
        Ok(HerdrTempTab {
            tab_id,
            root_pane_id,
        })
    }

    fn start_tui_worker_agent_in_tab(
        &self,
        tab_id: &str,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        let mut args = vec![
            "agent".to_string(),
            "start".to_string(),
            request.name.clone(),
            "--cwd".to_string(),
            request.cwd.to_string_lossy().to_string(),
            "--tab".to_string(),
            tab_id.to_string(),
            "--split".to_string(),
            "down".to_string(),
        ];
        self.push_tui_worker_agent_env(&mut args, request);
        args.push("--no-focus".to_string());
        args.push("--".to_string());
        args.extend(request.argv.iter().cloned());
        let started = self.run_json(&args)?;
        self.tui_worker_opened_from_agent(started, request)
    }

    fn push_tui_worker_agent_env(&self, args: &mut Vec<String>, request: &CockpitTuiWorkerRequest) {
        for (key, value) in &request.env {
            args.push("--env".to_string());
            args.push(format!("{key}={value}"));
        }
        args.push("--env".to_string());
        args.push(format!(
            "KHAZAD_TUI_WORKER_ID={}:{}:{}",
            request.run_id, request.slice_id, request.attempt
        ));
    }

    fn tui_worker_opened_from_agent(
        &self,
        started: Value,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        let agent = started
            .pointer("/result/agent")
            .ok_or_else(|| anyhow!("herdr agent start omitted result.agent"))?;
        let pane_id = agent
            .get("pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("herdr agent start omitted agent.pane_id"))?;
        let terminal_id = agent
            .get("terminal_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(CockpitTuiWorkerOpened {
            adapter: self.name().to_string(),
            mode: CockpitMode::Herdr,
            workspace_label: String::new(),
            agent_name: request.name.clone(),
            pane_label: String::new(),
            pane_id: pane_id.to_string(),
            terminal_id: terminal_id.to_string(),
            slot_name: String::new(),
            slot_index: 0,
            slot_region: String::new(),
        })
    }

    fn move_pane_to_slot(
        &self,
        pane_id: &str,
        target_tab_id: &str,
        target_pane_id: &str,
        direction: CockpitLayoutDirection,
        ratio: &str,
    ) -> Result<()> {
        self.run_json(&[
            "pane".to_string(),
            "move".to_string(),
            pane_id.to_string(),
            "--tab".to_string(),
            target_tab_id.to_string(),
            "--split".to_string(),
            direction.as_herdr_arg().to_string(),
            "--target-pane".to_string(),
            target_pane_id.to_string(),
            "--ratio".to_string(),
            ratio.to_string(),
            "--no-focus".to_string(),
        ])?;
        Ok(())
    }

    fn start_tui_worker_agent_in_slot_once(
        &self,
        workspace: &CockpitWorkspaceRef,
        slot: &CockpitWorkerSlot,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        let pane_label = slot.pane_label(&worker_pane_label(
            &request.run_id,
            &request.slice_id,
            request.attempt,
        ));
        let inspection = self.inspect_layout(workspace)?;
        let (target_pane_id, target_tab_id, direction, ratio, replaced_root) = if slot.index == 1 {
            let target = self.slot_one_target(workspace, &inspection)?;
            (
                target.pane_id.clone(),
                target.tab_id,
                CockpitLayoutDirection::Down,
                COCKPIT_LAYOUT_WORKER_SPLIT_RATIO.to_string(),
                target.close_after_move.then_some(target.pane_id),
            )
        } else {
            let split = slot
                .split
                .as_ref()
                .ok_or_else(|| anyhow!("cockpit layout slot {} omitted split", slot.name))?;
            let anchor_pane_id = inspection
                .worker_slot_pane_id(&split.anchor_slot)
                .ok_or_else(|| {
                    anyhow!(
                        "cockpit layout slot {} could not find anchor {}",
                        slot.name,
                        split.anchor_slot
                    )
                })?;
            let target_tab_id = inspection
                .tab_id_for_pane(anchor_pane_id)
                .ok_or_else(|| anyhow!("cockpit layout anchor pane omitted tab id"))?;
            (
                anchor_pane_id.to_string(),
                target_tab_id.to_string(),
                split.direction,
                split.ratio.clone(),
                None,
            )
        };

        let temp_tab = self.create_temp_tui_worker_tab(workspace, request)?;
        let mut opened = match self.start_tui_worker_agent_in_tab(&temp_tab.tab_id, request) {
            Ok(opened) => opened,
            Err(err) => {
                self.close_pane_best_effort(&temp_tab.root_pane_id);
                return Err(err);
            }
        };
        let post_start_result = (|| -> Result<()> {
            self.move_pane_to_slot(
                &opened.pane_id,
                &target_tab_id,
                &target_pane_id,
                direction,
                &ratio,
            )?;
            self.rename_pane(&opened.pane_id, &pane_label)?;
            if let Some(root_pane_id) = &replaced_root {
                self.close_pane(root_pane_id)?;
            }
            self.close_pane(&temp_tab.root_pane_id)?;
            Ok(())
        })();
        if let Err(err) = post_start_result {
            self.close_pane_best_effort(&opened.pane_id);
            self.close_pane_best_effort(&temp_tab.root_pane_id);
            return Err(err);
        }
        opened.pane_label = pane_label;
        opened.slot_name = slot.name.clone();
        opened.slot_index = slot.index;
        opened.slot_region = slot.region.clone();
        Ok(opened)
    }

    fn close_pane_best_effort(&self, pane_id: &str) {
        let _ = self.run_command(&["pane".to_string(), "close".to_string(), pane_id.to_string()]);
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
                anchor_pane: None,
                existed: true,
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
            anchor_pane: Some(CockpitPaneRef {
                id: root_pane.to_string(),
            }),
            existed: false,
        })
    }

    fn create_read_only_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef> {
        let direction = if request.label.starts_with("Worker ") {
            CockpitLayoutDirection::Down
        } else {
            CockpitLayoutDirection::Right
        };
        let anchor_pane_id = self.live_layout_anchor_pane_id(workspace)?;
        let pane = self.split_pane(&anchor_pane_id, direction, "0.5", request)?;
        self.run_pane_request(&pane.id, request)?;
        Ok(pane)
    }

    fn inspect_layout(&self, workspace: &CockpitWorkspaceRef) -> Result<CockpitLayoutInspection> {
        let mut last_pane_not_found = None;
        for _ in 0..2 {
            let panes = self.pane_list(&workspace.id)?;
            let (root_pane_id, panes) = self.select_live_layout_anchor(workspace, panes)?;
            match self.run_json(&[
                "pane".to_string(),
                "layout".to_string(),
                "--pane".to_string(),
                root_pane_id.clone(),
            ]) {
                Ok(_) => {
                    return Ok(CockpitLayoutInspection {
                        root_pane_id: Some(root_pane_id),
                        panes,
                    });
                }
                Err(err) if is_herdr_pane_not_found_error(&err) => {
                    last_pane_not_found = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_pane_not_found
            .unwrap_or_else(|| anyhow!("cockpit layout inspection could not resolve a live pane")))
    }

    fn ensure_dashboard_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        dashboard: &CockpitDashboardLayout,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitPaneRef> {
        let inspection = self.inspect_layout(workspace)?;
        if let Some(pane_id) = inspection.dashboard_pane_id() {
            return Ok(CockpitPaneRef {
                id: pane_id.to_string(),
            });
        }
        let root_pane_id = inspection
            .root_pane_id
            .as_deref()
            .ok_or_else(|| anyhow!("cockpit layout inspection omitted root pane"))?;
        let pane = self.split_pane(root_pane_id, dashboard.direction, &dashboard.ratio, request)?;
        self.run_pane_request(&pane.id, request)?;
        let root_label = inspection.label_for_pane(root_pane_id).unwrap_or_default();
        if root_label.trim().is_empty() {
            self.rename_pane(root_pane_id, WORKER_REGION_PLACEHOLDER_PANE)?;
        }
        Ok(pane)
    }

    fn cleanup_placeholder_root_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        slot: &CockpitWorkerSlot,
        replacement_label: &str,
    ) -> Result<()> {
        if slot.index != 1 {
            return Ok(());
        }
        let _ = (workspace, replacement_label);
        Ok(())
    }

    fn place_worker_slot_pane(
        &self,
        workspace: &CockpitWorkspaceRef,
        _plan: &CockpitLayoutPlan,
        slot: &CockpitWorkerSlot,
        request: &CockpitPaneRequest,
    ) -> Result<CockpitWorkerPlacement> {
        let pane_label = slot.pane_label(&request.label);
        if slot.index == 1 {
            let inspection = self.inspect_layout(workspace)?;
            if let Some(root_pane_id) =
                inspection.slot_one_reusable_pane_id(self.preferred_anchor_id(workspace))
            {
                let root_pane_id = root_pane_id.to_string();
                self.run_pane_request_with_label(&root_pane_id, &pane_label, request)?;
                return Ok(CockpitWorkerPlacement {
                    pane: CockpitPaneRef { id: root_pane_id },
                    slot_name: slot.name.clone(),
                    slot_index: slot.index,
                    slot_region: slot.region.clone(),
                    pane_label,
                });
            }
            let dashboard_pane_id = inspection.dashboard_pane_id().ok_or_else(|| {
                anyhow!("cockpit layout has no live pane available for worker slot 1")
            })?;
            let pane = self.split_pane(
                dashboard_pane_id,
                CockpitLayoutDirection::Down,
                COCKPIT_LAYOUT_WORKER_SPLIT_RATIO,
                request,
            )?;
            self.run_pane_request_with_label(&pane.id, &pane_label, request)?;
            return Ok(CockpitWorkerPlacement {
                pane,
                slot_name: slot.name.clone(),
                slot_index: slot.index,
                slot_region: slot.region.clone(),
                pane_label,
            });
        }

        let split = slot
            .split
            .as_ref()
            .ok_or_else(|| anyhow!("cockpit layout slot {} omitted split", slot.name))?;
        let inspection = self.inspect_layout(workspace)?;
        let anchor_pane_id = inspection
            .worker_slot_pane_id(&split.anchor_slot)
            .ok_or_else(|| {
                anyhow!(
                    "cockpit layout slot {} could not find anchor {}",
                    slot.name,
                    split.anchor_slot
                )
            })?;
        let pane = self.split_pane(anchor_pane_id, split.direction, &split.ratio, request)?;
        self.run_pane_request_with_label(&pane.id, &pane_label, request)?;
        Ok(CockpitWorkerPlacement {
            pane,
            slot_name: slot.name.clone(),
            slot_index: slot.index,
            slot_region: slot.region.clone(),
            pane_label,
        })
    }

    fn start_tui_worker_agent_in_slot(
        &self,
        workspace: &CockpitWorkspaceRef,
        _plan: &CockpitLayoutPlan,
        slot: &CockpitWorkerSlot,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        let mut last_pane_not_found = None;
        for _ in 0..2 {
            match self.start_tui_worker_agent_in_slot_once(workspace, slot, request) {
                Ok(opened) => return Ok(opened),
                Err(err) if is_herdr_pane_not_found_error(&err) => {
                    last_pane_not_found = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_pane_not_found.unwrap_or_else(|| {
            anyhow!("cockpit TUI worker placement could not resolve a live pane")
        }))
    }

    fn start_tui_worker_agent(
        &self,
        workspace: &CockpitWorkspaceRef,
        request: &CockpitTuiWorkerRequest,
    ) -> Result<CockpitTuiWorkerOpened> {
        let mut args = vec![
            "agent".to_string(),
            "start".to_string(),
            request.name.clone(),
            "--cwd".to_string(),
            request.cwd.to_string_lossy().to_string(),
            "--workspace".to_string(),
            workspace.id.clone(),
            "--split".to_string(),
            "down".to_string(),
        ];
        self.push_tui_worker_agent_env(&mut args, request);
        args.push("--no-focus".to_string());
        args.push("--".to_string());
        args.extend(request.argv.iter().cloned());
        let started = self.run_json(&args)?;
        self.tui_worker_opened_from_agent(started, request)
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.run_command(&["pane".to_string(), "close".to_string(), pane_id.to_string()])?;
        Ok(())
    }

    fn send_agent_message(&self, target: &str, text: &str) -> Result<()> {
        self.run_command(&[
            "agent".to_string(),
            "send".to_string(),
            target.to_string(),
            text.to_string(),
        ])?;
        Ok(())
    }

    fn focus_agent(&self, target: &str) -> Result<()> {
        self.run_command(&["agent".to_string(), "focus".to_string(), target.to_string()])?;
        Ok(())
    }

    fn rename_agent(&self, target: &str, name: &str) -> Result<()> {
        self.run_command(&[
            "agent".to_string(),
            "rename".to_string(),
            target.to_string(),
            name.to_string(),
        ])?;
        Ok(())
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
    let mut child = spawn_command_with_retry(bin, args)
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

fn spawn_command_with_retry(bin: &Path, args: &[String]) -> std::io::Result<Child> {
    for attempt in 0..HERDR_SPAWN_RETRY_ATTEMPTS {
        match Command::new(bin)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(err) if is_text_file_busy(&err) && attempt + 1 < HERDR_SPAWN_RETRY_ATTEMPTS => {
                thread::sleep(HERDR_SPAWN_RETRY_DELAY);
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("spawn retry loop always returns on its final attempt")
}

fn is_text_file_busy(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(26)
    }
    #[cfg(not(unix))]
    {
        let _ = err;
        false
    }
}

fn khazad_child_binary() -> PathBuf {
    reusable_khazad_binary(std::env::current_exe().ok().as_deref())
        .unwrap_or_else(|| PathBuf::from("khazad-doom"))
}

fn reusable_khazad_binary(current_exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = current_exe {
        if is_executable(path) {
            return Some(path.to_path_buf());
        }
        if let Some(stripped) = strip_linux_deleted_exe_suffix(path)
            && is_executable(&stripped)
        {
            return Some(stripped);
        }
    }
    find_executable_in_path("khazad-doom")
}

fn strip_linux_deleted_exe_suffix(path: &Path) -> Option<PathBuf> {
    path.to_string_lossy()
        .strip_suffix(" (deleted)")
        .map(PathBuf::from)
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

fn is_herdr_pane_not_found_error(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("pane_not_found")
        || message.contains("target_pane_not_found")
        || message.contains("pane not found")
}

fn herdr_layout_lock() -> &'static Mutex<()> {
    HERDR_LAYOUT_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Store as ArtifactStore;
    use crate::domain::{
        AttentionNotificationRecord, OriginNotificationTarget, ReplanEvidenceLink, ReplanProposal,
        ReplanProposalSource, ReplanProposedChange, Run, RunStatus, WorkflowConfig,
    };
    use crate::state::Store as StateStore;
    use chrono::Utc;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeCockpitAdapter {
        calls: Arc<Mutex<Vec<String>>>,
        layout: Arc<Mutex<FakeLayoutState>>,
        workspace_existed: bool,
    }

    #[derive(Default)]
    struct FakeLayoutState {
        dashboard: bool,
        worker_slots: Vec<String>,
    }

    impl Default for FakeCockpitAdapter {
        fn default() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                layout: Arc::new(Mutex::new(FakeLayoutState::default())),
                workspace_existed: false,
            }
        }
    }

    impl FakeCockpitAdapter {
        fn existing_workspace() -> Self {
            Self {
                workspace_existed: true,
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn attention_state_store() -> Result<(tempfile::TempDir, StateStore)> {
        let home = tempfile::tempdir()?;
        let state = StateStore::open(home.path().join("state.sqlite"))?;
        Ok((home, state))
    }

    fn attention_run_fixture(repo_path: &Path, run_id: &str) -> Run {
        let now = Utc::now();
        Run {
            id: run_id.to_string(),
            repo_id: "repo-fixture".to_string(),
            repo_path: repo_path.to_string_lossy().to_string(),
            status: RunStatus::Running,
            base_branch: "main".to_string(),
            base_sha: "base-sha".to_string(),
            integration_branch: format!("khazad/{run_id}/integration"),
            selected_slice_id: "slice-001".to_string(),
            error: String::new(),
            started_at: now,
            updated_at: now,
        }
    }

    fn attention_origin() -> OriginNotificationTarget {
        OriginNotificationTarget {
            schema_version: ATTENTION_PAYLOAD_SCHEMA_VERSION,
            target: "agent-1".to_string(),
            target_kind: "opaque".to_string(),
            delivery_adapter: ATTENTION_DELIVERY_ADAPTER.to_string(),
            delivery_surface: ATTENTION_DELIVERY_SURFACE.to_string(),
            source: "test".to_string(),
            created_at: Utc::now().to_rfc3339(),
        }
    }

    fn attention_records(
        store: &ArtifactStore,
        run_id: &str,
    ) -> Result<Vec<AttentionNotificationRecord>> {
        let dir = store.notifications_dir(run_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records: Vec<AttentionNotificationRecord> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("attention-") && name.ends_with(".json") {
                records.push(artifact::read_json(entry.path())?);
            }
        }
        records.sort_by(|left, right| left.attention_key.cmp(&right.attention_key));
        Ok(records)
    }

    fn attention_replan_proposal(
        state: &StateStore,
        run_id: &str,
        id: &str,
    ) -> Result<ReplanProposal> {
        state.create_replan_proposal(
            run_id,
            id,
            ReplanProposalSource {
                kind: "worker_finding".to_string(),
                slice_id: "slice-001".to_string(),
                phase: "slice_worker".to_string(),
                attempt: 1,
                summary: "needs operator review".to_string(),
            },
            vec![format!("finding-{id}")],
            vec![ReplanEvidenceLink {
                kind: "worker_output".to_string(),
                path: "worker-output.json".to_string(),
                event_id: 0,
                summary: "evidence".to_string(),
            }],
            vec![ReplanProposedChange {
                kind: "follow_up_or_revision".to_string(),
                target: "slice-001".to_string(),
                summary: "revise scope".to_string(),
            }],
            "operator_review_required",
        )
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
                anchor_pane: Some(CockpitPaneRef {
                    id: "pane-1".to_string(),
                }),
                existed: self.workspace_existed,
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

        fn inspect_layout(
            &self,
            _workspace: &CockpitWorkspaceRef,
        ) -> Result<CockpitLayoutInspection> {
            self.calls
                .lock()
                .unwrap()
                .push("layout:inspect".to_string());
            let layout = self.layout.lock().unwrap();
            let mut panes = vec![CockpitLayoutPane {
                id: "pane-1".to_string(),
                label: WORKER_REGION_PLACEHOLDER_PANE.to_string(),
                tab_id: "tab-1".to_string(),
            }];
            if layout.dashboard {
                panes.push(CockpitLayoutPane {
                    id: "pane-dashboard".to_string(),
                    label: RUN_STATUS_FEED_PANE.to_string(),
                    tab_id: "tab-1".to_string(),
                });
            }
            for slot in &layout.worker_slots {
                panes.push(CockpitLayoutPane {
                    id: format!("pane-{slot}"),
                    label: format!("{slot}: fake worker"),
                    tab_id: "tab-1".to_string(),
                });
            }
            Ok(CockpitLayoutInspection {
                root_pane_id: Some("pane-1".to_string()),
                panes,
            })
        }

        fn ensure_dashboard_pane(
            &self,
            _workspace: &CockpitWorkspaceRef,
            dashboard: &CockpitDashboardLayout,
            request: &CockpitPaneRequest,
        ) -> Result<CockpitPaneRef> {
            self.calls.lock().unwrap().push(format!(
                "layout:dashboard:{}:{}:{}",
                request.label,
                dashboard.direction.as_herdr_arg(),
                dashboard.ratio
            ));
            self.layout.lock().unwrap().dashboard = true;
            Ok(CockpitPaneRef {
                id: "pane-dashboard".to_string(),
            })
        }

        fn cleanup_placeholder_root_pane(
            &self,
            _workspace: &CockpitWorkspaceRef,
            slot: &CockpitWorkerSlot,
            replacement_label: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(format!(
                "layout:cleanup-root:{}:{}",
                slot.name, replacement_label
            ));
            Ok(())
        }

        fn place_worker_slot_pane(
            &self,
            _workspace: &CockpitWorkspaceRef,
            _plan: &CockpitLayoutPlan,
            slot: &CockpitWorkerSlot,
            request: &CockpitPaneRequest,
        ) -> Result<CockpitWorkerPlacement> {
            let pane_label = slot.pane_label(&request.label);
            self.calls.lock().unwrap().push(format!(
                "layout:worker-slot:{}:{}:{}",
                slot.name, pane_label, request.command
            ));
            self.layout
                .lock()
                .unwrap()
                .worker_slots
                .push(slot.name.clone());
            Ok(CockpitWorkerPlacement {
                pane: CockpitPaneRef {
                    id: format!("pane-{}", slot.name),
                },
                slot_name: slot.name.clone(),
                slot_index: slot.index,
                slot_region: slot.region.clone(),
                pane_label,
            })
        }

        fn start_tui_worker_agent(
            &self,
            workspace: &CockpitWorkspaceRef,
            request: &CockpitTuiWorkerRequest,
        ) -> Result<CockpitTuiWorkerOpened> {
            self.calls.lock().unwrap().push(format!(
                "agent_start:{}:{}:{}:{}:{}:{}",
                workspace.id,
                request.run_id,
                request.slice_id,
                request.attempt,
                request.name,
                request.argv.join(" ")
            ));
            Ok(CockpitTuiWorkerOpened {
                adapter: self.name().to_string(),
                mode: CockpitMode::Herdr,
                workspace_label: String::new(),
                agent_name: request.name.clone(),
                pane_label: String::new(),
                pane_id: "pane-tui".to_string(),
                terminal_id: "terminal-tui".to_string(),
                slot_name: String::new(),
                slot_index: 0,
                slot_region: String::new(),
            })
        }

        fn start_tui_worker_agent_in_slot(
            &self,
            workspace: &CockpitWorkspaceRef,
            _plan: &CockpitLayoutPlan,
            slot: &CockpitWorkerSlot,
            request: &CockpitTuiWorkerRequest,
        ) -> Result<CockpitTuiWorkerOpened> {
            let pane_label = slot.pane_label(&worker_pane_label(
                &request.run_id,
                &request.slice_id,
                request.attempt,
            ));
            self.calls.lock().unwrap().push(format!(
                "layout:tui-worker-slot:{}:{}:{}:{}:{}:{}",
                slot.name,
                pane_label,
                workspace.id,
                request.run_id,
                request.slice_id,
                request.argv.join(" ")
            ));
            self.layout
                .lock()
                .unwrap()
                .worker_slots
                .push(slot.name.clone());
            Ok(CockpitTuiWorkerOpened {
                adapter: self.name().to_string(),
                mode: CockpitMode::Herdr,
                workspace_label: String::new(),
                agent_name: request.name.clone(),
                pane_label,
                pane_id: format!("pane-{}", slot.name),
                terminal_id: format!("terminal-{}", slot.name),
                slot_name: slot.name.clone(),
                slot_index: slot.index,
                slot_region: slot.region.clone(),
            })
        }

        fn close_pane(&self, pane_id: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("pane_close:{pane_id}"));
            Ok(())
        }

        fn send_agent_message(&self, target: &str, text: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("agent_send:{target}:{text}"));
            Ok(())
        }
    }

    #[test]
    fn attention_notification_no_origin_noops_for_worker_question() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let artifact_store = ArtifactStore::new(repo.path());
        let (_home, state) = attention_state_store()?;
        let run = attention_run_fixture(repo.path(), "kd-attention-no-origin");
        state.insert_run(&run)?;
        let question = state.insert_worker_question(
            "q-no-origin",
            &run.id,
            "slice-001",
            1,
            "choose?",
            &["a".to_string(), "b".to_string()],
            0,
        )?;

        notify_origin_worker_question_attention(&state, &question);

        assert!(attention_records(&artifact_store, &run.id)?.is_empty());
        assert!(state.get_events(&run.id, 100)?.is_empty());
        Ok(())
    }

    #[test]
    fn attention_notification_worker_question_dedupes_and_notifies_new_question() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let artifact_store = ArtifactStore::new(repo.path());
        let (_home, state) = attention_state_store()?;
        let run = attention_run_fixture(repo.path(), "kd-attention-worker-question");
        state.insert_run(&run)?;
        artifact_store.write_origin_notification_target(&run.id, &attention_origin())?;
        let first = state.insert_worker_question_with_recommendation(
            "q-1",
            &run.id,
            "slice-001",
            1,
            "choose?",
            &["a".to_string()],
            30,
            &crate::domain::WorkerQuestionRecommendation {
                recommended_answer: "a".to_string(),
                rationale: "a is bounded and reversible".to_string(),
                bounded_within_current_slice_or_mission_authority: true,
                reversible: true,
            },
        )?;
        let second = state.insert_worker_question(
            "q-2",
            &run.id,
            "slice-001",
            1,
            "choose again?",
            &["b".to_string()],
            30,
        )?;

        notify_origin_worker_question_attention(&state, &first);
        let event_count_after_first = state.get_events(&run.id, 100)?.len();
        notify_origin_worker_question_attention(&state, &first);
        assert_eq!(
            state.get_events(&run.id, 100)?.len(),
            event_count_after_first
        );
        notify_origin_worker_question_attention(&state, &second);

        let records = attention_records(&artifact_store, &run.id)?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].attention_key, "worker-question:q-1");
        assert_eq!(records[1].attention_key, "worker-question:q-2");
        assert_eq!(records[0].origin_target, "agent-1");
        assert_eq!(
            records[0].payload["answer_commands"][0],
            "khazad-doom answer kd-attention-worker-question q-1 <answer>"
        );
        assert_eq!(
            records[0].payload["status_commands"][0],
            "khazad-doom status --run kd-attention-worker-question"
        );
        assert_eq!(
            records[0].payload["delivery_semantics"],
            "visibility_only_no_auto_decision"
        );
        assert_eq!(records[0].payload["recommended_answer"], "a");
        assert_eq!(
            records[0].payload["recommendation_rationale"],
            "a is bounded and reversible"
        );
        assert_eq!(records[0].payload["fallback_eligible"], true);
        assert_eq!(
            records[0].payload["deadline_at"],
            first.deadline_at.expect("durable deadline").to_rfc3339()
        );
        Ok(())
    }

    #[test]
    fn attention_notification_replan_dedupes_and_notifies_new_proposal() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let artifact_store = ArtifactStore::new(repo.path());
        let (_home, state) = attention_state_store()?;
        let run = attention_run_fixture(repo.path(), "kd-attention-replan");
        state.insert_run(&run)?;
        artifact_store.write_origin_notification_target(&run.id, &attention_origin())?;
        let first = attention_replan_proposal(&state, &run.id, "rp-1")?;
        let second = attention_replan_proposal(&state, &run.id, "rp-2")?;

        notify_origin_replan_attention(&state, &run, &first);
        let event_count_after_first = state.get_events(&run.id, 100)?.len();
        notify_origin_replan_attention(&state, &run, &first);
        assert_eq!(
            state.get_events(&run.id, 100)?.len(),
            event_count_after_first
        );
        notify_origin_replan_attention(&state, &run, &second);

        let records = attention_records(&artifact_store, &run.id)?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].attention_key, "replan-proposal:rp-1");
        assert_eq!(records[1].attention_key, "replan-proposal:rp-2");
        assert_eq!(
            records[0].payload["decision_commands"][0],
            "khazad-doom replan accept kd-attention-replan rp-1 --reason <reason>"
        );
        assert_eq!(
            records[0].payload["status_commands"][1],
            "khazad-doom monitor --run kd-attention-replan"
        );
        assert_eq!(
            records[0].payload["delivery_semantics"],
            "visibility_only_no_auto_decision"
        );
        Ok(())
    }

    #[test]
    fn terminal_notification_cockpit_uses_inert_agent_send_surface() {
        let adapter = FakeCockpitAdapter::default();

        let sent = Cockpit::new(CockpitMode::Herdr, adapter.clone())
            .send_agent_message("agent-1", "{\"run_id\":\"kd-test\"}")
            .unwrap();

        assert_eq!(sent.adapter, "fake-herdr");
        assert_eq!(sent.target, "agent-1");
        assert_eq!(sent.surface, "herdr agent send");
        assert_eq!(
            adapter.calls(),
            vec!["agent_send:agent-1:{\"run_id\":\"kd-test\"}".to_string()]
        );
    }

    #[test]
    fn cockpit_layout_planner_places_dashboard_right_workers_left_for_one_to_four_workers() {
        let planner = CockpitLayoutPlanner;

        for workers in 1..=4 {
            let plan = planner.plan(workers).unwrap();

            assert_eq!(plan.worker_count, workers);
            assert_eq!(plan.dashboard.region, "right-dashboard");
            assert_eq!(plan.dashboard.split_from_slot, "worker-1");
            assert_eq!(plan.dashboard.direction, CockpitLayoutDirection::Right);
            assert_eq!(plan.dashboard.ratio, "0.68");
            assert!(
                plan.worker_slots
                    .iter()
                    .all(|slot| slot.region == "left-worker-region")
            );
            assert!(
                plan.worker_slots
                    .iter()
                    .all(|slot| !slot.name.to_lowercase().contains("operator"))
            );
        }
    }

    #[test]
    fn cockpit_layout_planner_uses_stable_worker_slots_and_v2_three_worker_fallback() {
        let planner = CockpitLayoutPlanner;
        let plan = planner.plan(4).unwrap();
        let names = plan
            .worker_slots
            .iter()
            .map(|slot| slot.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["worker-1", "worker-2", "worker-3", "worker-4"]);
        assert_eq!(
            plan.worker_slots[1].split.as_ref().unwrap().anchor_slot,
            "worker-1"
        );
        assert_eq!(
            plan.worker_slots[1].split.as_ref().unwrap().direction,
            CockpitLayoutDirection::Right
        );
        assert_eq!(
            plan.worker_slots[2].split.as_ref().unwrap().anchor_slot,
            "worker-1"
        );
        assert_eq!(
            plan.worker_slots[2].split.as_ref().unwrap().direction,
            CockpitLayoutDirection::Down
        );
        assert_eq!(
            plan.worker_slots[3].split.as_ref().unwrap().anchor_slot,
            "worker-2"
        );
        assert_eq!(
            plan.worker_slots[3].split.as_ref().unwrap().direction,
            CockpitLayoutDirection::Down
        );
    }

    fn fake_herdr_fixture(
        panes: serde_json::Value,
        next_pane: usize,
    ) -> (tempfile::TempDir, HerdrCockpitAdapter) {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("fake-herdr-state.json");
        let script_path = dir.path().join("herdr");
        fs::write(
            &state_path,
            serde_json::json!({
                "panes": panes,
                "next_pane": next_pane,
                "next_tab": 2
            })
            .to_string(),
        )
        .unwrap();
        let state_path_json = serde_json::to_string(&state_path.to_string_lossy()).unwrap();
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import pathlib
import sys

STATE = pathlib.Path({state_path_json})

def load():
    return json.loads(STATE.read_text())

def save(state):
    STATE.write_text(json.dumps(state))

def ok(result):
    print(json.dumps({{"result": result}}))
    sys.exit(0)

def fail(code, message):
    print(json.dumps({{"error": {{"code": code, "message": message}}, "id": "fake-herdr"}}))
    sys.exit(1)

def pane(state, pane_id):
    for item in state["panes"]:
        if item["pane_id"] == pane_id:
            return item
    return None

def next_pane(state, workspace_id):
    pane_id = f"{{workspace_id}}:p{{state['next_pane']}}"
    state["next_pane"] += 1
    return pane_id

def next_tab(state, workspace_id):
    tab_id = f"{{workspace_id}}:t{{state['next_tab']}}"
    state["next_tab"] += 1
    return tab_id

args = sys.argv[1:]
state = load()

if args[:2] == ["pane", "list"]:
    workspace_id = args[args.index("--workspace") + 1]
    ok({{"panes": [p for p in state["panes"] if p["workspace_id"] == workspace_id]}})

if args[:2] == ["pane", "layout"]:
    pane_id = args[args.index("--pane") + 1]
    if pane(state, pane_id) is None:
        fail("pane_not_found", "pane not found")
    ok({{"pane": {{"pane_id": pane_id}}}})

if args[:2] == ["tab", "create"]:
    workspace_id = args[args.index("--workspace") + 1]
    tab_id = next_tab(state, workspace_id)
    root_pane_id = next_pane(state, workspace_id)
    state["panes"].append({{"pane_id": root_pane_id, "workspace_id": workspace_id, "tab_id": tab_id, "label": ""}})
    save(state)
    ok({{"tab": {{"tab_id": tab_id}}, "root_pane": {{"pane_id": root_pane_id}}}})

if args[:2] == ["agent", "start"]:
    tab_id = args[args.index("--tab") + 1]
    workspace_id = tab_id.split(":t", 1)[0]
    pane_id = next_pane(state, workspace_id)
    state["panes"].append({{"pane_id": pane_id, "workspace_id": workspace_id, "tab_id": tab_id, "label": ""}})
    save(state)
    ok({{"agent": {{"pane_id": pane_id, "terminal_id": f"term-{{pane_id}}"}}}})

if args[:2] == ["pane", "move"]:
    pane_id = args[2]
    moved = pane(state, pane_id)
    target = pane(state, args[args.index("--target-pane") + 1])
    if moved is None:
        fail("pane_not_found", "pane not found")
    if target is None:
        fail("target_pane_not_found", "target pane not found")
    moved["tab_id"] = args[args.index("--tab") + 1]
    save(state)
    ok({{"pane": {{"pane_id": pane_id}}}})

if args[:2] == ["pane", "rename"]:
    item = pane(state, args[2])
    if item is None:
        fail("pane_not_found", "pane not found")
    item["label"] = " ".join(args[3:])
    save(state)
    ok({{"pane": {{"pane_id": args[2]}}}})

if args[:2] == ["pane", "close"]:
    pane_id = args[2]
    if pane(state, pane_id) is None:
        fail("pane_not_found", "pane not found")
    state["panes"] = [p for p in state["panes"] if p["pane_id"] != pane_id]
    save(state)
    ok({{"closed": pane_id}})

fail("unsupported", "unsupported fake herdr command: " + " ".join(args))
"#
        );
        fs::write(&script_path, script).unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        (dir, HerdrCockpitAdapter { bin: script_path })
    }

    fn test_workspace(anchor_pane: Option<&str>) -> CockpitWorkspaceRef {
        CockpitWorkspaceRef {
            id: "w1".to_string(),
            anchor_pane: anchor_pane.map(|id| CockpitPaneRef { id: id.to_string() }),
            existed: anchor_pane.is_none(),
        }
    }

    fn test_tui_worker(slice_id: &str) -> CockpitTuiWorkerRequest {
        CockpitTuiWorkerRequest {
            run_id: "kd-run".to_string(),
            slice_id: slice_id.to_string(),
            attempt: 1,
            name: format!("kd-run-{slice_id}-1"),
            argv: vec!["pi".to_string(), format!("@{slice_id}.md")],
            cwd: PathBuf::from("/repo/worker"),
            env: Vec::new(),
        }
    }

    fn fake_herdr_state(dir: &tempfile::TempDir) -> serde_json::Value {
        let state_path = dir.path().join("fake-herdr-state.json");
        serde_json::from_str(&fs::read_to_string(state_path).unwrap()).unwrap()
    }

    #[test]
    fn tui_slot1_replacement_then_second_slot_placement_succeeds() {
        let (_dir, adapter) = fake_herdr_fixture(
            serde_json::json!([
                {"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","label":WORKER_REGION_PLACEHOLDER_PANE},
                {"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","label":RUN_STATUS_FEED_PANE}
            ]),
            3,
        );
        let workspace = test_workspace(Some("w1:p1"));
        let plan = CockpitLayoutPlanner.plan(2).unwrap();

        adapter
            .start_tui_worker_agent_in_slot_once(
                &workspace,
                &plan.worker_slots[0],
                &test_tui_worker("SLICE-1"),
            )
            .unwrap();
        let second = adapter
            .start_tui_worker_agent_in_slot_once(
                &workspace,
                &plan.worker_slots[1],
                &test_tui_worker("SLICE-2"),
            )
            .unwrap();

        assert_eq!(second.slot_name, "worker-2");
        assert_eq!(second.slot_index, 2);
        assert_eq!(
            second.pane_label,
            "worker-2: Worker kd-run/SLICE-2 attempt 1"
        );
    }

    #[test]
    fn tui_slot1_retry_after_close_recreates_placeholder() {
        let (dir, adapter) = fake_herdr_fixture(
            serde_json::json!([
                {"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","label":RUN_STATUS_FEED_PANE}
            ]),
            3,
        );
        let workspace = test_workspace(None);
        let plan = CockpitLayoutPlanner.plan(1).unwrap();

        let opened = adapter
            .start_tui_worker_agent_in_slot_once(
                &workspace,
                &plan.worker_slots[0],
                &test_tui_worker("SLICE-RETRY"),
            )
            .unwrap();

        assert_eq!(opened.slot_name, "worker-1");
        assert_eq!(
            opened.pane_label,
            "worker-1: Worker kd-run/SLICE-RETRY attempt 1"
        );
        let state = fake_herdr_state(&dir);
        let panes = state["panes"].as_array().unwrap();
        assert!(
            panes
                .iter()
                .any(|pane| pane["label"].as_str() == Some(RUN_STATUS_FEED_PANE))
        );
        assert!(
            panes.iter().any(|pane| pane["label"].as_str()
                == Some("worker-1: Worker kd-run/SLICE-RETRY attempt 1"))
        );
    }

    #[test]
    fn tui_slot1_retry_replaces_existing_worker_before_dashboard() {
        let (dir, adapter) = fake_herdr_fixture(
            serde_json::json!([
                {"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","label":RUN_STATUS_FEED_PANE},
                {"pane_id":"w1:p4","workspace_id":"w1","tab_id":"w1:t1","label":"worker-1: Worker kd-run/OLD attempt 1"}
            ]),
            5,
        );
        let workspace = test_workspace(None);
        let plan = CockpitLayoutPlanner.plan(1).unwrap();

        adapter
            .start_tui_worker_agent_in_slot_once(
                &workspace,
                &plan.worker_slots[0],
                &test_tui_worker("SLICE-RETRY"),
            )
            .unwrap();

        let state = fake_herdr_state(&dir);
        let panes = state["panes"].as_array().unwrap();
        assert!(
            panes
                .iter()
                .any(|pane| pane["label"].as_str() == Some(RUN_STATUS_FEED_PANE))
        );
        assert_eq!(
            panes
                .iter()
                .filter(|pane| pane["label"]
                    .as_str()
                    .is_some_and(|label| label.starts_with("worker-1: ")))
                .count(),
            1
        );
        assert!(
            panes.iter().any(|pane| pane["label"].as_str()
                == Some("worker-1: Worker kd-run/SLICE-RETRY attempt 1"))
        );
    }

    #[test]
    fn focused_existing_workspace_resolves_anchor_by_label_not_first_pane() {
        let (_dir, adapter) = fake_herdr_fixture(
            serde_json::json!([
                {"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","label":RUN_STATUS_FEED_PANE},
                {"pane_id":"w1:p4","workspace_id":"w1","tab_id":"w1:t1","label":"worker-1: Worker kd-run/SLICE-1 attempt 1"}
            ]),
            5,
        );
        let workspace = test_workspace(None);

        let inspection = adapter.inspect_layout(&workspace).unwrap();

        assert_eq!(inspection.root_pane_id.as_deref(), Some("w1:p4"));
    }

    #[test]
    fn cockpit_layout_opens_dashboard_without_default_operator_column() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-test"),
            feed_command: "khazad-doom monitor --run kd-test".to_string(),
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
                pane_labels: vec![RUN_STATUS_FEED_PANE.to_string()],
            })
        );
        let calls = adapter.calls();
        assert_eq!(
            calls,
            vec![
                "workspace:Khazad-Doom kd-test".to_string(),
                "layout:inspect".to_string(),
                "layout:dashboard:Dashboard:right:0.68".to_string(),
            ]
        );
        assert!(calls.iter().all(|call| !call.contains("Operator")));
    }

    #[test]
    fn cockpit_open_or_focus_existing_workspace_does_not_create_duplicate_panes() {
        let adapter = FakeCockpitAdapter::existing_workspace();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-test"),
            feed_command: "khazad-doom monitor --run kd-test".to_string(),
        };

        let opened = Cockpit::new(CockpitMode::Herdr, adapter.clone())
            .open_or_focus_run(&request)
            .unwrap();

        assert_eq!(opened.action, "focused_existing");
        assert!(opened.pane_labels.is_empty());
        assert_eq!(adapter.calls(), vec!["workspace:Khazad-Doom kd-test"]);
    }

    #[test]
    fn cockpit_direct_mode_skips_adapter() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-test"),
            feed_command: "feed".to_string(),
        };

        let launched = Cockpit::new(CockpitMode::Direct, adapter.clone())
            .open_run(&request)
            .unwrap();

        assert_eq!(launched, CockpitLaunch::SkippedDirect);
        assert!(adapter.calls().is_empty());
    }

    #[test]
    fn cockpit_layout_worker_pane_uses_deterministic_slot_and_run_slice_label() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-run"),
            feed_command: "feed".to_string(),
        };
        let worker = CockpitWorkerPaneRequest {
            run_id: "kd-run".to_string(),
            slice_id: "SLICE-1".to_string(),
            attempt: 2,
            command: "/bin/sh wrapper.sh".to_string(),
            cwd: PathBuf::from("/repo/worker"),
            env: vec![("KHAZAD_COCKPIT_WORKER".to_string(), "1".to_string())],
        };

        let opened = Cockpit::new(CockpitMode::Herdr, adapter.clone())
            .open_worker_pane(&request, &worker)
            .unwrap();

        assert_eq!(
            opened,
            CockpitWorkerLaunch::Opened(CockpitWorkerOpened {
                adapter: "fake-herdr".to_string(),
                mode: CockpitMode::Herdr,
                workspace_label: "Khazad-Doom kd-run".to_string(),
                pane_label: "worker-1: Worker kd-run/SLICE-1 attempt 2".to_string(),
                pane_id: "pane-worker-1".to_string(),
                slot_name: "worker-1".to_string(),
                slot_index: 1,
                slot_region: "left-worker-region".to_string(),
            })
        );
        let calls = adapter.calls();
        assert_eq!(calls[0], "workspace:Khazad-Doom kd-run");
        assert!(calls.contains(&"layout:dashboard:Dashboard:right:0.68".to_string()));
        assert!(calls.iter().any(|call| call.starts_with(
            "layout:worker-slot:worker-1:worker-1: Worker kd-run/SLICE-1 attempt 2:/bin/sh wrapper.sh"
        )));
    }

    #[test]
    fn cockpit_layout_worker_panes_advance_to_next_stable_slot() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-run"),
            feed_command: "feed".to_string(),
        };
        let first = CockpitWorkerPaneRequest {
            run_id: "kd-run".to_string(),
            slice_id: "SLICE-1".to_string(),
            attempt: 1,
            command: "first".to_string(),
            cwd: PathBuf::from("/repo/worker-1"),
            env: Vec::new(),
        };
        let second = CockpitWorkerPaneRequest {
            run_id: "kd-run".to_string(),
            slice_id: "SLICE-2".to_string(),
            attempt: 1,
            command: "second".to_string(),
            cwd: PathBuf::from("/repo/worker-2"),
            env: Vec::new(),
        };
        let cockpit = Cockpit::new(CockpitMode::Herdr, adapter.clone());

        cockpit.open_worker_pane(&request, &first).unwrap();
        let opened = cockpit.open_worker_pane(&request, &second).unwrap();

        assert_eq!(
            opened,
            CockpitWorkerLaunch::Opened(CockpitWorkerOpened {
                adapter: "fake-herdr".to_string(),
                mode: CockpitMode::Herdr,
                workspace_label: "Khazad-Doom kd-run".to_string(),
                pane_label: "worker-2: Worker kd-run/SLICE-2 attempt 1".to_string(),
                pane_id: "pane-worker-2".to_string(),
                slot_name: "worker-2".to_string(),
                slot_index: 2,
                slot_region: "left-worker-region".to_string(),
            })
        );
    }

    #[test]
    fn cockpit_tui_worker_uses_layout_v2_worker_slot() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-run"),
            feed_command: "feed".to_string(),
        };
        let worker = CockpitTuiWorkerRequest {
            run_id: "kd-run".to_string(),
            slice_id: "SLICE-1".to_string(),
            attempt: 2,
            name: "kd-run-SLICE-1-2".to_string(),
            argv: vec!["pi".to_string(), "@prompt.md".to_string()],
            cwd: PathBuf::from("/repo/worker"),
            env: vec![(
                "KHAZAD_WORKER_RESULT_PATH".to_string(),
                "/repo/.workflow/runs/kd-run/outputs/result.json".to_string(),
            )],
        };

        let opened = Cockpit::new(CockpitMode::Herdr, adapter.clone())
            .open_tui_worker_agent(&request, &worker)
            .unwrap();

        assert_eq!(
            opened,
            CockpitTuiWorkerLaunch::Opened(CockpitTuiWorkerOpened {
                adapter: "fake-herdr".to_string(),
                mode: CockpitMode::Herdr,
                workspace_label: "Khazad-Doom kd-run".to_string(),
                agent_name: "kd-run-SLICE-1-2".to_string(),
                pane_label: "worker-1: Worker kd-run/SLICE-1 attempt 2".to_string(),
                pane_id: "pane-worker-1".to_string(),
                terminal_id: "terminal-worker-1".to_string(),
                slot_name: "worker-1".to_string(),
                slot_index: 1,
                slot_region: "left-worker-region".to_string(),
            })
        );
        let calls = adapter.calls();
        assert_eq!(calls[0], "workspace:Khazad-Doom kd-run");
        assert!(calls.contains(&"layout:dashboard:Dashboard:right:0.68".to_string()));
        assert!(calls.iter().any(|call| call.starts_with(
            "layout:tui-worker-slot:worker-1:worker-1: Worker kd-run/SLICE-1 attempt 2"
        )));
        assert!(calls.iter().all(|call| !call.contains("Operator")));
    }

    #[test]
    fn cockpit_tui_worker_grid_advances_stable_left_worker_slots_for_four_workers() {
        let adapter = FakeCockpitAdapter::default();
        let request = CockpitRunRequest {
            repo_path: PathBuf::from("/repo"),
            khazad_home: PathBuf::from("/khazad-home"),
            workspace_label: workspace_label_for_run("kd-run"),
            feed_command: "feed".to_string(),
        };
        let cockpit = Cockpit::new(CockpitMode::Herdr, adapter.clone());
        let mut opened_slots = Vec::new();

        for index in 1..=4 {
            let worker = CockpitTuiWorkerRequest {
                run_id: "kd-run".to_string(),
                slice_id: format!("SLICE-{index}"),
                attempt: 1,
                name: format!("kd-run-SLICE-{index}-1"),
                argv: vec!["pi".to_string(), format!("@prompt-{index}.md")],
                cwd: PathBuf::from(format!("/repo/worker-{index}")),
                env: Vec::new(),
            };
            match cockpit.open_tui_worker_agent(&request, &worker).unwrap() {
                CockpitTuiWorkerLaunch::Opened(opened) => {
                    assert_eq!(opened.slot_region, "left-worker-region");
                    assert_eq!(opened.slot_index, index);
                    opened_slots.push(opened.slot_name);
                }
                CockpitTuiWorkerLaunch::SkippedDirect => panic!("unexpected direct skip"),
            }
        }

        assert_eq!(
            opened_slots,
            vec!["worker-1", "worker-2", "worker-3", "worker-4"]
        );
        let calls = adapter.calls();
        assert!(calls.iter().any(|call| call.starts_with(
            "layout:tui-worker-slot:worker-4:worker-4: Worker kd-run/SLICE-4 attempt 1"
        )));
        assert!(calls.iter().all(|call| !call.contains("Operator")));
    }

    #[test]
    fn cockpit_can_close_tui_worker_pane_through_adapter() {
        let adapter = FakeCockpitAdapter::default();

        Cockpit::new(CockpitMode::Herdr, adapter.clone())
            .close_pane("pane-tui")
            .unwrap();

        assert_eq!(adapter.calls(), vec!["pane_close:pane-tui".to_string()]);
    }

    #[test]
    fn worker_activity_painter_command_waits_for_wrapper_after_painter_exit() {
        let command = worker_activity_pane_command(
            "/bin/sh /tmp/kd-wrapper.sh",
            Path::new("/tmp/kd.stdout.ndjson"),
            Path::new("/tmp/kd.status.json"),
            Path::new("/tmp/kd.exit.json"),
        );

        assert!(command.contains("paint-worker-activity"));
        assert!(command.contains("/tmp/kd-wrapper.sh"));
        assert!(command.contains("/tmp/kd.stdout.ndjson"));
        assert!(command.contains("/tmp/kd.status.json"));
        assert!(command.contains("/tmp/kd.exit.json"));
        assert!(command.contains("wait \"$khazad_wrapper_pid\""));
        assert!(command.contains("wrapper artifacts remain authoritative"));
    }

    #[test]
    fn reusable_binary_strips_linux_deleted_current_exe_suffix() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let installed = temp.path().join("khazad-doom");
        fs::write(&installed, b"fake khazad")?;
        let deleted = PathBuf::from(format!("{} (deleted)", installed.display()));

        assert_eq!(reusable_khazad_binary(Some(&deleted)), Some(installed));
        Ok(())
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
        assert!(!request.feed_command.to_lowercase().contains("planner"));
    }
}
