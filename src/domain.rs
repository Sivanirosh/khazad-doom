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
    pub worker_question_timeout_seconds: u64,
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
            worker_question_timeout_seconds: 1800,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FindingDisposition {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finding_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub finding_index: usize,
    pub disposition: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub replan_proposal_id: String,
    pub rationale: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OriginNotificationTarget {
    pub schema_version: u64,
    pub target: String,
    pub target_kind: String,
    pub delivery_adapter: String,
    pub delivery_surface: String,
    pub source: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalNotificationRecord {
    pub schema_version: u64,
    pub run_id: String,
    pub terminal_status: String,
    pub transition_key: String,
    pub delivery_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin_target: String,
    pub delivery_adapter: String,
    pub delivery_surface: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct WorkerProfileEvidence {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_evidence_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_evidence_label: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_attribution: BTreeMap<String, String>,
}

impl WorkerProfileEvidence {
    pub fn is_empty(&self) -> bool {
        self.agent.is_empty()
            && self.agent_profile.is_empty()
            && self.agent_provider.is_empty()
            && self.agent_model.is_empty()
            && self.agent_reasoning.is_empty()
            && self.agent_mode.is_empty()
            && self.profile_summary.is_empty()
            && self.launch_summary.is_empty()
            && self.worker_evidence_kind.is_empty()
            && self.worker_evidence_label.is_empty()
            && self.source_attribution.is_empty()
    }

    pub fn from_json_surface(value: &serde_json::Value) -> Option<Self> {
        if let Some(worker_profile) = value.get("worker_profile")
            && let Ok(profile) = serde_json::from_value::<Self>(worker_profile.clone())
            && !profile.is_empty()
        {
            return Some(profile);
        }
        let mut profile = Self {
            agent: json_string(value, "agent"),
            agent_profile: json_string(value, "agent_profile"),
            agent_provider: json_string(value, "agent_provider"),
            agent_model: json_string(value, "agent_model"),
            agent_reasoning: json_string(value, "agent_reasoning"),
            agent_mode: json_string(value, "agent_mode"),
            profile_summary: json_string(value, "profile_summary"),
            launch_summary: json_string(value, "launch_summary"),
            worker_evidence_kind: json_string(value, "worker_evidence_kind"),
            worker_evidence_label: json_string(value, "worker_evidence_label"),
            source_attribution: BTreeMap::new(),
        };
        if let Some(source) = value.get("profile_source_attribution")
            && let Ok(map) = serde_json::from_value::<BTreeMap<String, String>>(source.clone())
        {
            profile.source_attribution = map;
        }
        (!profile.is_empty()).then_some(profile)
    }
}

fn json_string(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
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
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
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
    pub finding_dispositions: Vec<FindingDisposition>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finding_dispositions: Vec<FindingDisposition>,
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

#[derive(Debug, Clone, Deserialize, Default)]
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

impl AgentCallEconomics {
    pub fn worker_evidence_kind(&self) -> &'static str {
        agent_call_worker_evidence_kind(&self.runner)
    }

    pub fn worker_evidence_label(&self) -> &'static str {
        agent_call_worker_evidence_label(&self.runner)
    }
}

impl Serialize for AgentCallEconomics {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut field_count = 8;
        for value in [
            &self.slice_id,
            &self.agent_profile,
            &self.agent_provider,
            &self.agent_model,
            &self.agent_reasoning,
            &self.agent_mode,
            &self.profile_summary,
            &self.launch_summary,
            &self.error,
        ] {
            if !value.is_empty() {
                field_count += 1;
            }
        }
        if self.operator_pause_ms != 0 {
            field_count += 1;
        }
        if self.input_tokens != 0 {
            field_count += 1;
        }
        if self.output_tokens != 0 {
            field_count += 1;
        }

        let mut state = serializer.serialize_struct("AgentCallEconomics", field_count)?;
        state.serialize_field("phase", &self.phase)?;
        if !self.slice_id.is_empty() {
            state.serialize_field("slice_id", &self.slice_id)?;
        }
        state.serialize_field("attempt", &self.attempt)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("runner", &self.runner)?;
        state.serialize_field("worker_evidence_kind", self.worker_evidence_kind())?;
        state.serialize_field("worker_evidence_label", self.worker_evidence_label())?;
        if !self.agent_profile.is_empty() {
            state.serialize_field("agent_profile", &self.agent_profile)?;
        }
        if !self.agent_provider.is_empty() {
            state.serialize_field("agent_provider", &self.agent_provider)?;
        }
        if !self.agent_model.is_empty() {
            state.serialize_field("agent_model", &self.agent_model)?;
        }
        if !self.agent_reasoning.is_empty() {
            state.serialize_field("agent_reasoning", &self.agent_reasoning)?;
        }
        if !self.agent_mode.is_empty() {
            state.serialize_field("agent_mode", &self.agent_mode)?;
        }
        if !self.profile_summary.is_empty() {
            state.serialize_field("profile_summary", &self.profile_summary)?;
        }
        if !self.launch_summary.is_empty() {
            state.serialize_field("launch_summary", &self.launch_summary)?;
        }
        state.serialize_field("status", &self.status)?;
        state.serialize_field("duration_ms", &self.duration_ms)?;
        if self.operator_pause_ms != 0 {
            state.serialize_field("operator_pause_ms", &self.operator_pause_ms)?;
        }
        if !self.error.is_empty() {
            state.serialize_field("error", &self.error)?;
        }
        if self.input_tokens != 0 {
            state.serialize_field("input_tokens", &self.input_tokens)?;
        }
        if self.output_tokens != 0 {
            state.serialize_field("output_tokens", &self.output_tokens)?;
        }
        state.end()
    }
}

fn agent_call_worker_evidence_kind(runner: &str) -> &'static str {
    if runner.eq_ignore_ascii_case("fake") {
        "deterministic_test_double_not_real_pi_worker_evidence"
    } else {
        "real_pi_worker"
    }
}

fn agent_call_worker_evidence_label(runner: &str) -> &'static str {
    if runner.eq_ignore_ascii_case("fake") {
        "deterministic test-double evidence; not real Pi worker implementation evidence"
    } else {
        "real Pi worker implementation evidence"
    }
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
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
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
    #[serde(default)]
    pub plan_revisions: PlanRevisions,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_semantic_progress_summary: String,
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

const GATE_PANE_LINE_WIDTH: usize = 180;
const GATE_PANE_TAIL_LINES: usize = 6;

pub(crate) struct GatePaneProjection {
    pub active: bool,
    pub feed: StatusFeed,
}

pub(crate) fn project_gate_pane(details: &RunDetails, now: DateTime<Utc>) -> GatePaneProjection {
    if let Some(progress) = gate_pane_active_progress(details) {
        return GatePaneProjection {
            active: true,
            feed: active_gate_pane_feed(details, progress, now),
        };
    }
    GatePaneProjection {
        active: false,
        feed: idle_gate_pane_feed(details),
    }
}

fn active_gate_pane_feed(
    details: &RunDetails,
    progress: &RunProgress,
    now: DateTime<Utc>,
) -> StatusFeed {
    let terminal_reason = gate_pane_terminal_reason(details);
    let operator_commands = gate_pane_operator_commands(details, terminal_reason.as_ref());
    let mut blocks = vec![gate_pane_block(
        "Run",
        format!(
            "{} {} • {}",
            gate_pane_status_icon(details.run.status),
            details.run.status,
            gate_pane_short_run_id(&details.run.id)
        ),
        vec![gate_pane_line(
            "Source: daemon status feed / shell progress; gate results and artifacts remain authoritative",
            StatusFeedRole::Dim,
        )],
    )];

    let mut activity_lines = vec![
        gate_pane_line(
            format!(
                "command: {}",
                gate_pane_truncate(&progress.command, GATE_PANE_LINE_WIDTH)
            ),
            StatusFeedRole::Info,
        ),
        gate_pane_line("state: running", StatusFeedRole::Info),
    ];
    if !progress.message.trim().is_empty() {
        activity_lines.push(gate_pane_line(
            format!(
                "message: {}",
                gate_pane_truncate(&progress.message, GATE_PANE_LINE_WIDTH)
            ),
            StatusFeedRole::Info,
        ));
    }
    if !progress.slice_id.trim().is_empty() {
        activity_lines.push(gate_pane_line(
            format!("slice: {}", progress.slice_id),
            StatusFeedRole::Dim,
        ));
    }
    if progress.attempt > 0 {
        activity_lines.push(gate_pane_line(
            format!("attempt: {}", progress.attempt),
            StatusFeedRole::Dim,
        ));
    }
    activity_lines.push(gate_pane_line(
        format!("updated {} ago", gate_pane_since(progress.updated_at, now)),
        StatusFeedRole::Dim,
    ));
    if let Some(worker) = &progress.worker {
        activity_lines.push(gate_pane_line(
            format!(
                "supervisor: {}",
                match worker.process_observed_at {
                    Some(observed_at) => format!(
                        "alive, observed child {} ago",
                        gate_pane_since(observed_at, now)
                    ),
                    None => "starting, no child observation yet".to_string(),
                }
            ),
            StatusFeedRole::Dim,
        ));
    }
    blocks.push(gate_pane_block(
        gate_pane_activity_label(&progress.phase),
        format!(
            "(running • elapsed {})",
            gate_pane_since(progress.phase_started_at, now)
        ),
        activity_lines,
    ));

    let tail = gate_pane_compact_tail(&progress.output_tail);
    if tail.is_empty() {
        blocks.push(gate_pane_block(
            "Tail",
            "",
            vec![gate_pane_line(
                "waiting for daemon-owned command output",
                StatusFeedRole::Dim,
            )],
        ));
    } else {
        blocks.push(gate_pane_block(
            "Tail",
            format!("(last {} compact lines)", tail.len()),
            tail.into_iter()
                .map(|text| gate_pane_line(text, StatusFeedRole::Dim))
                .collect(),
        ));
    }

    StatusFeed {
        feed_version: 1,
        summary_line: "Khazad-Doom gate/repair activity painter (read-only)".to_string(),
        terminal_reason,
        operator_commands,
        attention: Vec::new(),
        blocks,
    }
}

fn idle_gate_pane_feed(details: &RunDetails) -> StatusFeed {
    let summary = gate_pane_latest_implementation_summary(details);
    let latest_gate = summary
        .and_then(|summary| gate_pane_gate_result_field(summary, "integration_gate"))
        .or_else(|| gate_pane_gate_result_from_economics(details));
    let pre_repair_gate = summary
        .and_then(|summary| gate_pane_gate_result_field(summary, "pre_repair_integration_gate"));
    let repair = summary.and_then(gate_pane_repair_result_field);
    let exit_states = summary.and_then(gate_pane_exit_states_field);
    let terminal_reason = gate_pane_terminal_reason(details);
    let operator_commands = gate_pane_operator_commands(details, terminal_reason.as_ref());

    let blocks = vec![
        gate_pane_gate_block(
            details,
            latest_gate.as_ref(),
            pre_repair_gate.as_ref(),
            summary,
        ),
        gate_pane_repair_block(details, latest_gate.as_ref(), repair.as_ref()),
        gate_pane_handoff_block(
            details,
            latest_gate.as_ref(),
            exit_states.as_ref(),
            terminal_reason.as_ref(),
        ),
        gate_pane_next_block(details, &operator_commands),
    ];

    StatusFeed {
        feed_version: 1,
        summary_line: "Khazad-Doom gate/repair status (idle)".to_string(),
        terminal_reason,
        operator_commands,
        attention: Vec::new(),
        blocks,
    }
}

fn gate_pane_gate_block(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    pre_repair_gate: Option<&GateResult>,
    summary: Option<&serde_json::Map<String, serde_json::Value>>,
) -> StatusFeedBlock {
    let mut lines = vec![gate_pane_line(
        format!(
            "Verification profile: {}",
            gate_pane_verification_profile(details, summary)
        ),
        StatusFeedRole::Info,
    )];
    match latest_gate {
        Some(gate) => lines.push(gate_pane_line(
            format!(
                "Latest gate: {}{}",
                gate_pane_display_or_dash(&gate.status),
                gate_pane_summary_suffix(&gate.summary)
            ),
            gate_pane_role_for_gate_status(&gate.status),
        )),
        None => lines.push(gate_pane_line(
            "Latest gate: not run yet",
            StatusFeedRole::Dim,
        )),
    }
    let last_failure = gate_pane_last_failure(pre_repair_gate, latest_gate);
    lines.push(gate_pane_line(
        format!(
            "Last failure: {}",
            last_failure.clone().unwrap_or_else(|| "none".to_string())
        ),
        if last_failure.is_some() {
            StatusFeedRole::Warning
        } else {
            StatusFeedRole::Dim
        },
    ));
    gate_pane_block(
        "Gate",
        format!(
            "({})",
            latest_gate
                .map(|gate| gate_pane_display_or_dash(&gate.status))
                .unwrap_or("not run")
        ),
        lines,
    )
}

fn gate_pane_repair_block(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    repair: Option<&RepairResult>,
) -> StatusFeedBlock {
    let policy = gate_pane_repair_policy(details);
    let (state, role) = gate_pane_repair_state(policy.as_str(), latest_gate, repair);
    let mut lines = vec![gate_pane_line(state, role)];
    if let Some(attempts) = gate_pane_repair_attempts(details, repair) {
        lines.push(gate_pane_line(attempts, StatusFeedRole::Dim));
    }
    gate_pane_block("Repair", format!("({policy})"), lines)
}

fn gate_pane_handoff_block(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    exit_states: Option<&WorkflowExitStates>,
    terminal_reason: Option<&TerminalReason>,
) -> StatusFeedBlock {
    let (meta, mut lines) = gate_pane_handoff_lines(details, latest_gate, exit_states);
    if let Some(reason) = terminal_reason {
        lines.push(gate_pane_line(
            format!(
                "Terminal reason: {}{}",
                gate_pane_display_or_dash(&reason.kind),
                gate_pane_summary_suffix(&reason.summary)
            ),
            if reason.operator_action_required {
                StatusFeedRole::Attention
            } else {
                StatusFeedRole::Warning
            },
        ));
    }
    lines.push(gate_pane_line(
        format!("Run: {}", details.run.status),
        StatusFeedRole::Dim,
    ));
    gate_pane_block("Handoff", format!("({meta})"), lines)
}

fn gate_pane_next_block(details: &RunDetails, operator_commands: &[String]) -> StatusFeedBlock {
    let mut commands = operator_commands.to_vec();
    match details.run.status {
        RunStatus::Completed => gate_pane_push_unique(
            &mut commands,
            format!("khazad-doom handoff --run {}", details.run.id),
        ),
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => {
            gate_pane_push_unique(
                &mut commands,
                format!("khazad-doom inspect --run {}", details.run.id),
            );
            gate_pane_push_unique(
                &mut commands,
                format!("khazad-doom resume --run {}", details.run.id),
            );
        }
        RunStatus::Pending | RunStatus::Running => {}
    }
    let lines = if commands.is_empty() {
        vec![gate_pane_line(
            "No operator gate/repair command is currently needed.",
            StatusFeedRole::Dim,
        )]
    } else {
        commands
            .into_iter()
            .map(|command| gate_pane_line(command, StatusFeedRole::Attention))
            .collect()
    };
    gate_pane_block("Next", "", lines)
}

fn gate_pane_active_progress(details: &RunDetails) -> Option<&RunProgress> {
    let progress = details.progress.as_ref()?;
    if gate_pane_terminal_status(details.run.status) || progress.command.trim().is_empty() {
        return None;
    }
    gate_pane_is_gate_or_repair_phase(&progress.phase).then_some(progress)
}

fn gate_pane_is_gate_or_repair_phase(phase: &str) -> bool {
    let normalized = phase.to_ascii_lowercase();
    normalized.contains("integration_gate") || normalized.contains("integration_repair")
}

fn gate_pane_activity_label(phase: &str) -> &'static str {
    if phase.to_ascii_lowercase().contains("repair") {
        "Repair"
    } else {
        "Integration Gate"
    }
}

fn gate_pane_compact_tail(output_tail: &str) -> Vec<String> {
    output_tail
        .trim_end()
        .lines()
        .rev()
        .take(GATE_PANE_TAIL_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| gate_pane_truncate(line, GATE_PANE_LINE_WIDTH))
        .collect()
}

fn gate_pane_terminal_reason(details: &RunDetails) -> Option<TerminalReason> {
    details
        .feed
        .as_ref()
        .and_then(|feed| feed.terminal_reason.clone())
        .or_else(|| details.primary_terminal_reason.clone())
}

fn gate_pane_operator_commands(
    details: &RunDetails,
    terminal_reason: Option<&TerminalReason>,
) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(feed) = &details.feed {
        for command in &feed.operator_commands {
            gate_pane_push_unique(&mut commands, command.clone());
        }
    }
    if let Some(reason) = terminal_reason {
        for command in &reason.operator_commands {
            gate_pane_push_unique(&mut commands, command.clone());
        }
    }
    commands
}

fn gate_pane_latest_implementation_summary(
    details: &RunDetails,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    details
        .events
        .iter()
        .rev()
        .find(|event| event.typ == "implementation_summary")
        .and_then(|event| event.payload.as_object())
}

fn gate_pane_gate_result_field(
    summary: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<GateResult> {
    let value = summary.get(key)?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value(value.clone()).ok()
}

fn gate_pane_repair_result_field(
    summary: &serde_json::Map<String, serde_json::Value>,
) -> Option<RepairResult> {
    summary
        .get("integration_repair")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn gate_pane_exit_states_field(
    summary: &serde_json::Map<String, serde_json::Value>,
) -> Option<WorkflowExitStates> {
    summary
        .get("exit_states")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn gate_pane_gate_result_from_economics(details: &RunDetails) -> Option<GateResult> {
    let economics = details.economics.as_ref()?;
    let commands = economics
        .command_executions
        .iter()
        .filter(|command| command.phase == "integration_gate")
        .map(|command| GateCommandResult {
            command: command.command.clone(),
            status: command.status.clone(),
            exit_code: command.exit_code,
            output: String::new(),
            cwd: command.cwd.clone(),
            dedupe_key: command.dedupe_key.clone(),
            duration_ms: command.duration_ms,
            cache_hit: command.cache_hit,
            skip_reason: command.skip_reason.clone(),
            failure_kind: String::new(),
        })
        .collect::<Vec<_>>();
    if commands.is_empty() {
        return None;
    }
    let status = if commands.iter().any(|command| command.status == "failed") {
        "failed"
    } else if commands.iter().all(|command| command.status == "skipped") {
        "skipped"
    } else if commands
        .iter()
        .all(|command| command.status == "passed" || command.status == "skipped")
    {
        "passed"
    } else {
        "unknown"
    };
    let summary = match status {
        "passed" => "integration gate passed",
        "failed" => "one or more integration gate commands failed",
        "skipped" => "integration gate commands skipped",
        _ => "integration gate status is unknown",
    };
    Some(GateResult {
        status: status.to_string(),
        summary: summary.to_string(),
        commands,
        findings: Vec::new(),
    })
}

fn gate_pane_verification_profile(
    details: &RunDetails,
    summary: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    if let Some(profile) = summary
        .and_then(|summary| summary.get("verify_profile"))
        .and_then(serde_json::Value::as_str)
        .filter(|profile| !profile.trim().is_empty())
    {
        return profile.to_string();
    }
    for event in details.events.iter().rev() {
        if event.typ != "run_started" {
            continue;
        }
        let Some(payload) = event.payload.as_object() else {
            continue;
        };
        if let Some(profile) = payload
            .get("verify_profile")
            .and_then(serde_json::Value::as_str)
            .filter(|profile| !profile.trim().is_empty())
        {
            return profile.to_string();
        }
        if let Some(profiles) = payload
            .get("verify_profiles")
            .and_then(serde_json::Value::as_array)
        {
            let joined = profiles
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|profile| !profile.trim().is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            if !joined.trim().is_empty() {
                return joined;
            }
        }
    }
    "unknown".to_string()
}

fn gate_pane_repair_policy(details: &RunDetails) -> String {
    details
        .economics
        .as_ref()
        .map(|economics| economics.repair_policy.trim())
        .filter(|policy| !policy.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn gate_pane_repair_attempts(
    details: &RunDetails,
    repair: Option<&RepairResult>,
) -> Option<String> {
    if let Some(economics) = &details.economics {
        return Some(format!(
            "Attempts: {}/{}",
            economics.repair_attempts, economics.repair_max_attempts
        ));
    }
    repair
        .filter(|repair| repair.attempts > 0)
        .map(|repair| format!("Attempts: {}", repair.attempts))
}

fn gate_pane_repair_state(
    policy: &str,
    latest_gate: Option<&GateResult>,
    repair: Option<&RepairResult>,
) -> (String, StatusFeedRole) {
    if let Some(repair) = repair {
        let mut text = format!("State: {}", gate_pane_display_or_dash(&repair.status));
        if !repair.summary.trim().is_empty() {
            text.push_str(&gate_pane_summary_suffix(&repair.summary));
        }
        if !repair.trigger.trim().is_empty() {
            text.push_str(&format!(" ({})", repair.trigger));
        }
        return (text, gate_pane_role_for_repair_status(&repair.status));
    }
    match latest_gate.map(|gate| gate.status.as_str()) {
        None => (
            "State: waiting for gate result".to_string(),
            StatusFeedRole::Dim,
        ),
        Some("passed") => (
            "State: not needed; latest gate passed".to_string(),
            StatusFeedRole::Success,
        ),
        Some("failed") if matches!(policy, "auto" | "always") => (
            "State: repairable: daemon policy can run integration repair".to_string(),
            StatusFeedRole::Warning,
        ),
        Some("failed") if policy == "never" => (
            "State: disabled by policy after failed gate".to_string(),
            StatusFeedRole::Warning,
        ),
        Some("failed") => (
            "State: unresolved after failed gate".to_string(),
            StatusFeedRole::Warning,
        ),
        Some(status) => (
            format!("State: waiting after gate {status}"),
            StatusFeedRole::Dim,
        ),
    }
}

fn gate_pane_handoff_lines(
    details: &RunDetails,
    latest_gate: Option<&GateResult>,
    exit_states: Option<&WorkflowExitStates>,
) -> (String, Vec<StatusFeedLine>) {
    if let Some(exit_states) = exit_states {
        let meta = gate_pane_display_or_dash(&exit_states.handoff).to_string();
        let mut lines = vec![gate_pane_line(
            format!("Handoff: {meta}"),
            StatusFeedRole::Info,
        )];
        if !exit_states.evidence.trim().is_empty() {
            lines.push(gate_pane_line(
                format!("Evidence: {}", exit_states.evidence),
                StatusFeedRole::Dim,
            ));
        }
        return (meta, lines);
    }
    match details.run.status {
        RunStatus::Completed => (
            "ready".to_string(),
            vec![gate_pane_line("Handoff: ready", StatusFeedRole::Success)],
        ),
        RunStatus::Blocked | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => (
            "not_ready".to_string(),
            vec![gate_pane_line(
                format!("Handoff: not ready — run is {}", details.run.status),
                StatusFeedRole::Warning,
            )],
        ),
        RunStatus::Pending | RunStatus::Running => {
            if latest_gate.is_some_and(|gate| gate.status == "failed") {
                (
                    "not_ready".to_string(),
                    vec![gate_pane_line(
                        "Handoff: not ready — latest gate failed",
                        StatusFeedRole::Warning,
                    )],
                )
            } else {
                (
                    "unknown".to_string(),
                    vec![gate_pane_line(
                        "Handoff: unknown until integration gate finishes",
                        StatusFeedRole::Dim,
                    )],
                )
            }
        }
    }
}

fn gate_pane_last_failure(
    pre_repair_gate: Option<&GateResult>,
    latest_gate: Option<&GateResult>,
) -> Option<String> {
    pre_repair_gate
        .and_then(gate_pane_failure_line)
        .map(|failure| format!("{failure} (pre-repair)"))
        .or_else(|| latest_gate.and_then(gate_pane_failure_line))
}

fn gate_pane_failure_line(gate: &GateResult) -> Option<String> {
    gate.commands
        .iter()
        .find(|command| command.status == "failed")
        .map(|command| {
            let output = command
                .output
                .trim()
                .lines()
                .last()
                .unwrap_or_default()
                .trim();
            if !output.is_empty() {
                format!(
                    "{} — {}",
                    gate_pane_truncate(&command.command, 80),
                    gate_pane_truncate(output, 90)
                )
            } else if let Some(exit_code) = command.exit_code {
                format!(
                    "{} (exit {exit_code})",
                    gate_pane_truncate(&command.command, 120)
                )
            } else {
                gate_pane_truncate(&command.command, 120)
            }
        })
}

fn gate_pane_role_for_gate_status(status: &str) -> StatusFeedRole {
    match status {
        "passed" => StatusFeedRole::Success,
        "failed" => StatusFeedRole::Error,
        "skipped" => StatusFeedRole::Dim,
        _ => StatusFeedRole::Info,
    }
}

fn gate_pane_role_for_repair_status(status: &str) -> StatusFeedRole {
    match status {
        "completed" | "fixed" | "no-op" => StatusFeedRole::Success,
        "failed" | "blocked" => StatusFeedRole::Error,
        "skipped" => StatusFeedRole::Dim,
        _ => StatusFeedRole::Info,
    }
}

fn gate_pane_summary_suffix(summary: &str) -> String {
    if summary.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", gate_pane_truncate(summary, GATE_PANE_LINE_WIDTH))
    }
}

fn gate_pane_status_icon(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "✓",
        RunStatus::Running => "●",
        RunStatus::Blocked => "!",
        RunStatus::Failed => "✗",
        RunStatus::Cancelled | RunStatus::Interrupted => "×",
        RunStatus::Pending => "○",
    }
}

fn gate_pane_terminal_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed
            | RunStatus::Failed
            | RunStatus::Blocked
            | RunStatus::Cancelled
            | RunStatus::Interrupted
    )
}

fn gate_pane_since(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(time).num_seconds().max(0) as u64;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn gate_pane_short_run_id(value: &str) -> String {
    if value.chars().count() <= 30 {
        return gate_pane_display_or_dash(value).to_string();
    }
    let prefix = value.chars().take(11).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn gate_pane_truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut truncated = value.chars().take(keep).collect::<String>();
    truncated.push('…');
    truncated
}

fn gate_pane_display_or_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

fn gate_pane_push_unique(values: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn gate_pane_block(
    label: impl Into<String>,
    meta: impl Into<String>,
    lines: Vec<StatusFeedLine>,
) -> StatusFeedBlock {
    StatusFeedBlock {
        label: label.into(),
        meta: meta.into(),
        lines,
    }
}

fn gate_pane_line(text: impl Into<String>, role: StatusFeedRole) -> StatusFeedLine {
    StatusFeedLine {
        text: text.into(),
        role,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerQuestion {
    pub id: String,
    pub run_id: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PlanRevisions {
    pub source_of_truth: String,
    pub queue_summary: String,
    pub unresolved_pending_blocks_handoff: bool,
    pub pending: Vec<PlanRevisionRecord>,
    pub accepted: Vec<PlanRevisionRecord>,
    pub rejected: Vec<PlanRevisionRecord>,
    pub deferred: Vec<PlanRevisionRecord>,
    pub superseded: Vec<PlanRevisionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRevisionRecord {
    pub proposal_id: String,
    pub state: String,
    pub source: ReplanProposalSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_finding_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ReplanEvidenceLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_changes: Vec<ReplanProposedChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorized_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action_class: String,
    pub risk: String,
    pub before_queue_or_slice_summary: String,
    pub after_queue_or_slice_summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PlanRevisionDecisionSummary>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRevisionDecisionSummary {
    pub decision: String,
    pub rationale: String,
    pub authorizer: String,
    pub source: String,
    pub decided_at: DateTime<Utc>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    pub applied_at_checkpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub replacement_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revisit_condition: String,
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
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
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
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_slices: Vec<String>,
    #[serde(default)]
    pub exit_states: WorkflowExitStates,
    #[serde(default)]
    pub evidence_attestation: EvidenceAttestation,
    #[serde(default)]
    pub plan_revisions: PlanRevisions,
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
