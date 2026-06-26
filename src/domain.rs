use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slice {
    pub id: String,
    pub title: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub github_issue: String,
    #[serde(
        default = "default_slice_status",
        skip_serializing_if = "is_open_status"
    )]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub closed_by_run: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub closed_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_ask_if: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub verify_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowConfig {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub parallelism: usize,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub verify_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub worker_attempt_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub worker_no_output_warning_seconds: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub worker_termination_grace_seconds: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "HandoffDefaults::is_empty")]
    pub handoff: HandoffDefaults,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub verify_profiles: BTreeMap<String, VerifyProfile>,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            agent: "pi".to_string(),
            parallelism: 1,
            verify_timeout_seconds: 600,
            worker_attempt_timeout_seconds: 0,
            worker_no_output_warning_seconds: 900,
            worker_termination_grace_seconds: 30,
            base_branch: String::new(),
            handoff: HandoffDefaults::default(),
            verify_profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffDefaults {
    #[serde(default, skip_serializing_if = "is_false")]
    pub push: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub create_pr: bool,
}

impl HandoffDefaults {
    pub fn is_empty(&self) -> bool {
        !self.push && !self.create_pr
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyProfile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<VerifyCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyCommand {
    pub command: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub severity: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub line: i64,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "blocked" => Ok(Self::Blocked),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            _ => anyhow::bail!("unknown run status {value:?}"),
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SliceStatus {
    Pending,
    Running,
    RepairNeeded,
    ReadyToMerge,
    Merged,
    Blocked,
    Failed,
    Cancelled,
    Interrupted,
}

impl SliceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::RepairNeeded => "repair_needed",
            Self::ReadyToMerge => "ready_to_merge",
            Self::Merged => "merged",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

impl SliceStatus {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "repair_needed" => Ok(Self::RepairNeeded),
            "ready_to_merge" => Ok(Self::ReadyToMerge),
            "merged" => Ok(Self::Merged),
            "blocked" => Ok(Self::Blocked),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            _ => anyhow::bail!("unknown slice status {value:?}"),
        }
    }
}

impl std::fmt::Display for SliceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub repo_id: String,
    pub repo_path: String,
    pub status: RunStatus,
    pub base_branch: String,
    pub base_sha: String,
    pub integration_branch: String,
    pub selected_slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub run_id: String,
    pub role: String,
    pub repo_path: String,
    pub worktree_path: String,
    pub branch: String,
    pub slice: Slice,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub dependency_summary: std::collections::BTreeMap<String, String>,
    pub output_path: String,
    pub contract: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerResult {
    pub slice_id: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_interfaces_changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests_run: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_status: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub slice_id: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests_run: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_head: String,
    #[serde(rename = "worktree_clean")]
    pub worktree_ok: bool,
    pub commit_found: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepairResult {
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_sha: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests_run: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCommandResult {
    pub command: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GateResult {
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<GateCommandResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationSummary {
    pub run_id: String,
    pub repo_path: String,
    pub integration_branch: String,
    pub base_sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_sha: String,
    pub completed_slices: Vec<WorkerResult>,
    pub checks: Vec<CheckResult>,
    pub integration_repair: RepairResult,
    pub integration_gate: GateResult,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceRun {
    pub run_id: String,
    pub slice_id: String,
    pub status: SliceStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_sha: String,
    pub attempts: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub run_id: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProgress {
    pub run_id: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_tail: String,
    pub phase_started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerAttemptProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAttemptProgress {
    pub attempt_started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_observed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_event_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_semantic_progress_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub attempt_timeout_seconds: u64,
    #[serde(default)]
    pub no_output_warning_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetails {
    pub run: Run,
    pub slice_runs: Vec<SliceRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<RunProgress>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceSummary {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "is_open_status")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    pub acceptance_count: usize,
    pub verify_count: usize,
}

impl From<&Slice> for SliceSummary {
    fn from(slice: &Slice) -> Self {
        Self {
            id: slice.id.clone(),
            title: slice.title.clone(),
            status: slice.status.clone(),
            depends_on: slice.depends_on.clone(),
            areas: slice.areas.clone(),
            acceptance_count: slice.acceptance.len(),
            verify_count: slice.verify.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceValidationIssue {
    pub severity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceValidationReport {
    pub valid: bool,
    pub slices: Vec<SliceSummary>,
    pub issues: Vec<SliceValidationIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchHandoff {
    pub run_id: String,
    pub repo_path: String,
    pub status: RunStatus,
    pub integration_branch: String,
    pub base_branch: String,
    pub base_sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_sha: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_slices: Vec<String>,
    pub summary_path: String,
    pub final_report_path: String,
    pub push_command: String,
    pub pr_command: String,
    pub pr_title: String,
    pub pr_body: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
    pub diagnostics: HandoffDiagnostics,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<HandoffActionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HandoffDiagnostics {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gh_version: String,
    pub gh_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffActionResult {
    pub action: String,
    pub command: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub size_bytes: u64,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInspection {
    pub run: Run,
    pub artifacts: Vec<ArtifactEntry>,
    pub daemon_log: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log_tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceWriteResult {
    pub slice: Slice,
    pub path: String,
    pub written: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCheckpoint {
    pub run_id: String,
    pub integration_branch: String,
    pub base_sha: String,
    pub current_sha: String,
    pub completed_slices: Vec<String>,
    pub remaining_slices: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflictReport {
    pub run_id: String,
    pub slice_id: String,
    pub branch: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicted_files: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

pub const SLICE_STATUS_OPEN: &str = "open";
pub const SLICE_STATUS_CLOSED: &str = "closed";

fn default_slice_status() -> String {
    SLICE_STATUS_OPEN.to_string()
}

pub fn is_open_status(value: &str) -> bool {
    value.is_empty() || value == SLICE_STATUS_OPEN
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}
