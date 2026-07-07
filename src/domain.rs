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
    #[serde(default, skip_serializing_if = "is_default_cockpit_mode")]
    pub cockpit: CockpitMode,
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
    #[serde(
        default = "default_integration_repair_policy",
        skip_serializing_if = "is_default_integration_repair_policy"
    )]
    pub integration_repair: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub gate_fail_fast: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_setup: Vec<VerifyCommand>,
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
            cockpit: CockpitMode::Auto,
            parallelism: 3,
            verify_timeout_seconds: 600,
            worker_attempt_timeout_seconds: 0,
            worker_no_output_warning_seconds: 900,
            worker_termination_grace_seconds: 30,
            integration_repair: default_integration_repair_policy(),
            gate_fail_fast: true,
            worktree_setup: Vec::new(),
            base_branch: String::new(),
            handoff: HandoffDefaults::default(),
            verify_profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CockpitMode {
    #[default]
    Auto,
    Herdr,
    Direct,
}

impl CockpitMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Herdr => "herdr",
            Self::Direct => "direct",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim() {
            "" | "auto" => Ok(Self::Auto),
            "herdr" => Ok(Self::Herdr),
            "direct" => Ok(Self::Direct),
            other => {
                anyhow::bail!("unknown cockpit mode {other:?}; expected auto, herdr, or direct")
            }
        }
    }
}

pub const IMPLEMENTER_PROFILE: &str = "implementer";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentProfilesConfig {
    pub profiles: BTreeMap<String, AgentProfile>,
}

impl Default for AgentProfilesConfig {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(IMPLEMENTER_PROFILE.to_string(), AgentProfile::implementer());
        profiles.insert(
            "planner".to_string(),
            AgentProfile {
                provider: "openai-codex".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning: "high".to_string(),
                mode: "normal".to_string(),
                read_only: true,
                ..AgentProfile::default()
            },
        );
        profiles.insert(
            "verifier".to_string(),
            AgentProfile {
                provider: "openai-codex".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning: "high".to_string(),
                mode: "fast".to_string(),
                read_only: true,
                ..AgentProfile::default()
            },
        );
        Self { profiles }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentProfile {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
}

impl AgentProfile {
    pub fn implementer() -> Self {
        Self {
            provider: "openai-codex".to_string(),
            model: "gpt-5.5".to_string(),
            reasoning: "xhigh".to_string(),
            mode: "fast".to_string(),
            args: Vec::new(),
            required: true,
            read_only: false,
        }
    }

    pub fn validate_required(&self, name: &str) -> anyhow::Result<()> {
        let mut missing = Vec::new();
        if self.provider.trim().is_empty() {
            missing.push("provider");
        }
        if self.model.trim().is_empty() {
            missing.push("model");
        }
        if self.reasoning.trim().is_empty() {
            missing.push("reasoning");
        }
        if self.mode.trim().is_empty() {
            missing.push("mode");
        }
        if missing.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "agent profile {name:?} is missing required settings: {}",
                missing.join(", ")
            )
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_summary: String,
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
    pub acceptance_status: Vec<AcceptanceEvidence>,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_kind: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AcceptanceEvidence {
    pub criterion: String,
    pub status: String,
    pub evidence: String,
}

impl<'de> Deserialize<'de> for AcceptanceEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(text) = value.as_str() {
            return Ok(Self {
                criterion: text.to_string(),
                status: "satisfied".to_string(),
                evidence: text.to_string(),
            });
        }
        #[derive(Deserialize)]
        struct StructuredAcceptanceEvidence {
            criterion: String,
            status: String,
            evidence: String,
        }
        let structured =
            StructuredAcceptanceEvidence::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            criterion: structured.criterion,
            status: structured.status,
            evidence: structured.evidence,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepairResult {
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trigger: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempts: usize,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "is_zero_u128")]
    pub duration_ms: u128,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cache_hit: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skip_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_kind: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhaseDuration {
    pub phase: String,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentCallEconomics {
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    pub attempt: usize,
    pub kind: String,
    pub runner: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_summary: String,
    pub status: String,
    pub duration_ms: u128,
    #[serde(default, skip_serializing_if = "is_zero_u128")]
    pub operator_pause_ms: u128,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub input_tokens: usize,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub output_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandExecutionEconomics {
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    pub attempt: usize,
    pub command: String,
    pub cwd: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub dedupe_key: String,
    pub tree_sha: String,
    pub cache_key: String,
    pub cache_hit: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skip_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DuplicateCommandEconomics {
    pub dedupe_key: String,
    pub command: String,
    pub executions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunEconomics {
    pub repair_policy: String,
    pub gate_fail_fast: bool,
    pub worker_max_attempts: usize,
    pub repair_max_attempts: usize,
    pub repair_attempts: usize,
    pub agent_call_count: usize,
    pub command_execution_count: usize,
    pub duplicate_command_count: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_calls: Vec<AgentCallEconomics>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub phase_durations: BTreeMap<String, PhaseDuration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_executions: Vec<CommandExecutionEconomics>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duplicate_commands: Vec<DuplicateCommandEconomics>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sla_violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowExitStates {
    pub run: String,
    pub handoff: String,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slices: Vec<SliceExitState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SliceExitState {
    pub slice_id: String,
    pub worker: String,
    pub daemon: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvidenceAttestation {
    pub status: String,
    pub attester: String,
    pub worker_self_approved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub basis: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_repair_integration_gate: Option<GateResult>,
    pub integration_gate: GateResult,
    #[serde(default)]
    pub exit_states: WorkflowExitStates,
    #[serde(default)]
    pub evidence_attestation: EvidenceAttestation,
    #[serde(default)]
    pub economics: RunEconomics,
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub parallel_layer: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parallel_slices: Vec<String>,
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
pub struct RunIncident {
    pub severity: String,
    pub kind: String,
    pub message: String,
    pub event_id: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TerminalReason {
    pub kind: String,
    pub resolution_owner: String,
    pub retryable: bool,
    pub operator_action_required: bool,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_links: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remediation: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub disposition: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFeed {
    pub feed_version: u64,
    pub summary_line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TerminalReason>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<StatusFeedLine>,
    pub blocks: Vec<StatusFeedBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFeedBlock {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub meta: String,
    pub lines: Vec<StatusFeedLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFeedLine {
    pub text: String,
    #[serde(default = "default_status_feed_role")]
    pub role: StatusFeedRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFeedRole {
    Heading,
    Info,
    Dim,
    Success,
    Warning,
    Error,
    Attention,
}

fn default_status_feed_role() -> StatusFeedRole {
    StatusFeedRole::Info
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerQuestion {
    pub id: String,
    pub run_id: String,
    pub slice_id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    pub state: String,
    pub asked_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplanProposalState {
    Pending,
    Accepted,
    Rejected,
    Deferred,
    Superseded,
}

impl ReplanProposalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Deferred => "deferred",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            "deferred" => Ok(Self::Deferred),
            "superseded" => Ok(Self::Superseded),
            _ => anyhow::bail!("unknown replan proposal state {value:?}"),
        }
    }
}

impl std::fmt::Display for ReplanProposalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplanProposalSource {
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplanEvidenceLink {
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub event_id: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplanProposedChange {
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplanDecision {
    pub decision: String,
    pub rationale: String,
    pub authorizer: String,
    pub source: String,
    pub decided_at: DateTime<Utc>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub replacement_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revisit_condition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplanProposal {
    pub id: String,
    pub run_id: String,
    pub state: ReplanProposalState,
    pub source: ReplanProposalSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_finding_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ReplanEvidenceLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_changes: Vec<ReplanProposedChange>,
    pub risk: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_decision: Option<ReplanDecision>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplanStatus {
    pub pending_attention_reason: String,
    pub pending: Vec<ReplanProposal>,
    pub history: Vec<ReplanProposal>,
    pub auto_approvable: Vec<ReplanProposal>,
}

pub fn replan_decision_commands(run_id: &str, proposal_id: &str) -> Vec<String> {
    vec![
        format!("khazad-doom replan accept {run_id} {proposal_id} --reason <reason>"),
        format!("khazad-doom replan reject {run_id} {proposal_id} --reason <reason>"),
        format!(
            "khazad-doom replan defer {run_id} {proposal_id} --until <condition> --reason <reason>"
        ),
        format!(
            "khazad-doom replan supersede {run_id} {proposal_id} <replacement-proposal> --reason <reason>"
        ),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetails {
    pub run: Run,
    pub slice_runs: Vec<SliceRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<RunProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incidents: Vec<RunIncident>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<WorkerQuestion>,
    #[serde(default)]
    pub replan: ReplanStatus,
    pub events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub economics: Option<RunEconomics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_terminal_reason: Option<TerminalReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<StatusFeed>,
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
    #[serde(default)]
    pub exit_states: WorkflowExitStates,
    #[serde(default)]
    pub evidence_attestation: EvidenceAttestation,
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

pub fn default_integration_repair_policy() -> String {
    "auto".to_string()
}

fn is_default_integration_repair_policy(value: &str) -> bool {
    value.is_empty() || value == "auto"
}

fn is_default_cockpit_mode(value: &CockpitMode) -> bool {
    *value == CockpitMode::Auto
}

fn default_true() -> bool {
    true
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

fn is_zero_u128(value: &u128) -> bool {
    *value == 0
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}
