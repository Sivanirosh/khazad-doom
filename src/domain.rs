use chrono::{DateTime, Utc};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SliceProvenance {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub parent_slice_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub origin_proposal_id: String,
    pub generation: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub created_by: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub created_at: String,
}

impl SliceProvenance {
    pub fn is_empty(&self) -> bool {
        self.parent_slice_id.is_empty()
            && self.origin_proposal_id.is_empty()
            && self.generation == 0
            && self.created_by.is_empty()
            && self.created_at.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Slice {
    pub id: String,
    pub title: String,
    pub goal: String,
    pub github_issue: String,
    pub status: String,
    pub closed_by_run: String,
    pub closed_at: String,
    pub depends_on: Vec<String>,
    pub areas: Vec<String>,
    pub acceptance: Vec<String>,
    pub must_ask_if: Vec<String>,
    pub verify_profile: String,
    pub verify: Vec<String>,
    pub verify_timeout_seconds: u64,
}

const SLICE_PROVENANCE_MARKER: &str = "\u{001e}khazad_slice_provenance:";

impl Slice {
    pub fn provenance(&self) -> Option<SliceProvenance> {
        split_slice_github_issue(&self.github_issue).1
    }

    #[allow(dead_code)]
    pub fn set_provenance(&mut self, provenance: SliceProvenance) {
        let github_issue = self.github_issue_text();
        self.github_issue = encode_slice_github_issue(&github_issue, &provenance);
    }

    #[allow(dead_code)]
    pub fn github_issue_text(&self) -> String {
        split_slice_github_issue(&self.github_issue).0.to_string()
    }
}

impl Serialize for Slice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (github_issue, provenance) = split_slice_github_issue(&self.github_issue);
        let mut state = serializer.serialize_struct("Slice", 16)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("title", &self.title)?;
        state.serialize_field("goal", &self.goal)?;
        if !github_issue.is_empty() {
            state.serialize_field("github_issue", github_issue)?;
        }
        if !is_open_status(&self.status) {
            state.serialize_field("status", &self.status)?;
        }
        if !self.closed_by_run.is_empty() {
            state.serialize_field("closed_by_run", &self.closed_by_run)?;
        }
        if !self.closed_at.is_empty() {
            state.serialize_field("closed_at", &self.closed_at)?;
        }
        if let Some(provenance) = provenance.filter(|provenance| !provenance.is_empty()) {
            state.serialize_field("provenance", &provenance)?;
        }
        if !self.depends_on.is_empty() {
            state.serialize_field("depends_on", &self.depends_on)?;
        }
        if !self.areas.is_empty() {
            state.serialize_field("areas", &self.areas)?;
        }
        state.serialize_field("acceptance", &self.acceptance)?;
        if !self.must_ask_if.is_empty() {
            state.serialize_field("must_ask_if", &self.must_ask_if)?;
        }
        if !self.verify_profile.is_empty() {
            state.serialize_field("verify_profile", &self.verify_profile)?;
        }
        if !self.verify.is_empty() {
            state.serialize_field("verify", &self.verify)?;
        }
        if !is_zero_u64(&self.verify_timeout_seconds) {
            state.serialize_field("verify_timeout_seconds", &self.verify_timeout_seconds)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for Slice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SliceWire {
            id: String,
            title: String,
            goal: String,
            #[serde(default)]
            github_issue: String,
            #[serde(default = "default_slice_status")]
            status: String,
            #[serde(default)]
            closed_by_run: String,
            #[serde(default)]
            closed_at: String,
            #[serde(default)]
            provenance: Option<SliceProvenance>,
            #[serde(default)]
            depends_on: Vec<String>,
            #[serde(default)]
            areas: Vec<String>,
            acceptance: Vec<String>,
            #[serde(default)]
            must_ask_if: Vec<String>,
            #[serde(default)]
            verify_profile: String,
            #[serde(default)]
            verify: Vec<String>,
            #[serde(default)]
            verify_timeout_seconds: u64,
        }

        let wire = SliceWire::deserialize(deserializer)?;
        let github_issue = wire
            .provenance
            .as_ref()
            .map(|provenance| encode_slice_github_issue(&wire.github_issue, provenance))
            .unwrap_or(wire.github_issue);
        Ok(Self {
            id: wire.id,
            title: wire.title,
            goal: wire.goal,
            github_issue,
            status: wire.status,
            closed_by_run: wire.closed_by_run,
            closed_at: wire.closed_at,
            depends_on: wire.depends_on,
            areas: wire.areas,
            acceptance: wire.acceptance,
            must_ask_if: wire.must_ask_if,
            verify_profile: wire.verify_profile,
            verify: wire.verify,
            verify_timeout_seconds: wire.verify_timeout_seconds,
        })
    }
}

fn encode_slice_github_issue(github_issue: &str, provenance: &SliceProvenance) -> String {
    if provenance.is_empty() {
        github_issue.to_string()
    } else {
        let payload = serde_json::to_string(provenance).unwrap_or_else(|_| "{}".to_string());
        format!("{github_issue}{SLICE_PROVENANCE_MARKER}{payload}")
    }
}

fn split_slice_github_issue(value: &str) -> (&str, Option<SliceProvenance>) {
    if let Some((github_issue, payload)) = value.split_once(SLICE_PROVENANCE_MARKER) {
        (github_issue, serde_json::from_str(payload).ok())
    } else {
        (value, None)
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    #[default]
    Off,
    Shadow,
    Promote,
    Run,
}

impl AutonomyLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Promote => "promote",
            Self::Run => "run",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim() {
            "" | "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "promote" => Ok(Self::Promote),
            "run" => Ok(Self::Run),
            other => anyhow::bail!(
                "mission envelope autonomy_level {other:?} is invalid; expected off, shadow, promote, or run"
            ),
        }
    }
}

impl std::fmt::Display for AutonomyLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MissionEnvelope {
    pub goal: String,
    pub allowed_areas: Vec<String>,
    pub non_goals: Vec<String>,
    #[serde(default = "default_mission_verify_profile")]
    pub verify_profile: String,
    #[serde(default)]
    pub max_auto_promotions: i64,
    #[serde(default)]
    pub max_depth: i64,
    #[serde(default)]
    pub max_generated_slices: i64,
    #[serde(default)]
    pub autonomy_level: AutonomyLevel,
    pub must_ask_if: Vec<String>,
}

impl Default for MissionEnvelope {
    fn default() -> Self {
        Self {
            goal: String::new(),
            allowed_areas: Vec::new(),
            non_goals: Vec::new(),
            verify_profile: default_mission_verify_profile(),
            max_auto_promotions: 0,
            max_depth: 0,
            max_generated_slices: 0,
            autonomy_level: AutonomyLevel::Off,
            must_ask_if: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FrontierBudgetState {
    #[serde(default)]
    pub auto_promotions_used: i64,
    #[serde(default)]
    pub generated_slices: i64,
    #[serde(default)]
    pub max_generation_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrontierClassification {
    pub tier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    pub classified_at: DateTime<Utc>,
    pub envelope_hash: String,
    pub budget_snapshot: FrontierBudgetState,
    pub autonomy_level: AutonomyLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct FrontierSummary {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary_line: String,
    pub candidates_seen: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tier_distribution: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub would_have_promoted: Vec<FrontierProposalOutcome>,
    #[serde(default, skip_serializing_if = "FrontierAgreementMetric::is_empty")]
    pub agreement: FrontierAgreementMetric,
}

impl FrontierSummary {
    pub fn is_empty(&self) -> bool {
        self.candidates_seen == 0
            && self.summary_line.is_empty()
            && self.tier_distribution.is_empty()
            && self.would_have_promoted.is_empty()
            && self.agreement.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct FrontierAgreementMetric {
    pub tier1_total: usize,
    pub accepted_unchanged: usize,
    pub accepted_modified: usize,
    pub rejected: usize,
    pub deferred: usize,
    pub pending: usize,
    pub agreement_numerator: usize,
    pub agreement_denominator: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agreement_ratio: String,
    pub agreement_percent: f64,
}

impl FrontierAgreementMetric {
    pub fn is_empty(&self) -> bool {
        self.tier1_total == 0
            && self.accepted_unchanged == 0
            && self.accepted_modified == 0
            && self.rejected == 0
            && self.deferred == 0
            && self.pending == 0
            && self.agreement_numerator == 0
            && self.agreement_denominator == 0
            && self.agreement_ratio.is_empty()
            && self.agreement_percent == 0.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FrontierProposalOutcome {
    pub proposal_id: String,
    pub tier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    pub operator_outcome: String,
    pub classified_at: String,
    pub envelope_hash: String,
    pub annotation: String,
}

pub fn frontier_classification_annotation(classification: &FrontierClassification) -> String {
    let tier = frontier_tier_label(&classification.tier);
    let reasons = if classification.reason_codes.is_empty() {
        "no_reason_codes".to_string()
    } else {
        classification.reason_codes.join(",")
    };
    let outcome = if frontier_classification_would_auto_promote(classification) {
        "would auto-promote"
    } else {
        match classification.tier.as_str() {
            "tier_0" => "would attest inline",
            "tier_1" => "would auto-promote",
            "tier_2" => "would queue pending",
            "tier_3" => "would ask operator",
            "stop" => "would stop frontier",
            _ => "would require review",
        }
    };
    let prefix = match classification.autonomy_level {
        AutonomyLevel::Shadow => "shadow".to_string(),
        AutonomyLevel::Promote => "promote".to_string(),
        AutonomyLevel::Run => "run".to_string(),
        AutonomyLevel::Off => "off".to_string(),
    };
    format!("{prefix}: {outcome} ({tier}: {reasons})")
}

pub fn frontier_classification_would_auto_promote(classification: &FrontierClassification) -> bool {
    if classification.tier == "tier_1" {
        return true;
    }
    let reasons = &classification.reason_codes;
    let has = |code: &str| reasons.iter().any(|reason| reason == code);
    let required = [
        "inside_allowed_areas",
        "acceptance_present",
        "verify_present",
        "within_budget",
        "within_depth",
        "not_duplicate",
        "add_followup_slice_only",
    ];
    let disqualifying = [
        "frontier_disabled",
        "area_outside_envelope",
        "area_ambiguous",
        "non_goal_overlap",
        "candidate_changes_dependencies",
        "candidate_changes_acceptance",
        "candidate_changes_verify_profile",
        "candidate_changes_policy_or_schema",
        "candidate_hits_must_ask_if",
        "envelope_must_ask_hit",
        "operator_only_change_kind",
        "duplicate_rejected_or_deferred_proposal",
        "classification_ambiguous",
        "frontier_budget_exhausted",
        "frontier_depth_exhausted",
        "no_frontier",
        "cancel_requested",
        "replan_apply_incomplete",
        "candidate_missing_acceptance",
        "candidate_missing_verify",
        "duplicate_open_slice",
        "duplicate_closed_slice",
        "duplicate_pending_proposal",
        "proposal_needs_operator_context",
    ];
    has("shadow_observation_only")
        && required.iter().all(|code| has(code))
        && !disqualifying.iter().any(|code| has(code))
}

fn frontier_tier_label(tier: &str) -> &'static str {
    match tier {
        "tier_0" => "Tier 0",
        "tier_1" => "Tier 1",
        "tier_2" => "Tier 2",
        "tier_3" => "Tier 3",
        "stop" => "Stop",
        _ => "Unknown tier",
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AttentionNotificationRecord {
    pub schema_version: u64,
    pub run_id: String,
    pub attention_key: String,
    pub attention_kind: String,
    pub delivery_status: String,
    pub send_status: String,
    pub focus_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposal_id: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_envelope: Option<MissionEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget: Option<FrontierBudgetState>,
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
#[serde(default)]
pub struct FollowupSliceDraft {
    pub id: String,
    pub title: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_ask_if: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
}

impl FollowupSliceDraft {
    pub fn to_slice(&self) -> Slice {
        Slice {
            id: self.id.clone(),
            title: self.title.clone(),
            goal: self.goal.clone(),
            github_issue: String::new(),
            status: default_slice_status(),
            closed_by_run: String::new(),
            closed_at: String::new(),
            depends_on: self.depends_on.clone(),
            areas: self.areas.clone(),
            acceptance: self.acceptance.clone(),
            must_ask_if: self.must_ask_if.clone(),
            verify_profile: self.verify_profile.clone(),
            verify: self.verify.clone(),
            verify_timeout_seconds: 0,
        }
    }
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
    pub candidate_followup_slices: Vec<FollowupSliceDraft>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_followup_slices: Vec<FollowupSliceDraft>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_envelope: Option<MissionEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget: Option<FrontierBudgetState>,
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

#[derive(Debug, Clone, Default)]
pub struct ReplanProposedChange {
    pub kind: String,
    pub target: String,
    pub summary: String,
}

const FOLLOWUP_DRAFT_MARKER: &str = "\u{001e}khazad_followup_slice_draft:";

impl ReplanProposedChange {
    pub fn with_followup_slice_draft(
        kind: String,
        target: String,
        summary: String,
        draft: FollowupSliceDraft,
    ) -> Self {
        Self {
            kind,
            target,
            summary: encode_replan_change_summary(&summary, &draft),
        }
    }

    pub fn summary_text(&self) -> String {
        split_replan_change_summary(&self.summary).0.to_string()
    }

    pub fn followup_slice_draft(&self) -> Option<FollowupSliceDraft> {
        split_replan_change_summary(&self.summary).1
    }
}

impl Serialize for ReplanProposedChange {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (summary, followup_slice_draft) = split_replan_change_summary(&self.summary);
        let mut state = serializer.serialize_struct("ReplanProposedChange", 4)?;
        state.serialize_field("kind", &self.kind)?;
        if !self.target.is_empty() {
            state.serialize_field("target", &self.target)?;
        }
        state.serialize_field("summary", summary)?;
        if let Some(followup_slice_draft) = followup_slice_draft {
            state.serialize_field("followup_slice_draft", &followup_slice_draft)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for ReplanProposedChange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ReplanProposedChangeWire {
            kind: String,
            #[serde(default)]
            target: String,
            summary: String,
            #[serde(default)]
            followup_slice_draft: Option<FollowupSliceDraft>,
        }

        let wire = ReplanProposedChangeWire::deserialize(deserializer)?;
        let summary = wire
            .followup_slice_draft
            .as_ref()
            .map(|draft| encode_replan_change_summary(&wire.summary, draft))
            .unwrap_or(wire.summary);
        Ok(Self {
            kind: wire.kind,
            target: wire.target,
            summary,
        })
    }
}

fn encode_replan_change_summary(summary: &str, draft: &FollowupSliceDraft) -> String {
    let payload = serde_json::to_string(draft).unwrap_or_else(|_| "{}".to_string());
    format!("{summary}{FOLLOWUP_DRAFT_MARKER}{payload}")
}

fn split_replan_change_summary(value: &str) -> (&str, Option<FollowupSliceDraft>) {
    if let Some((summary, payload)) = value.split_once(FOLLOWUP_DRAFT_MARKER) {
        (summary, serde_json::from_str(payload).ok())
    } else {
        (value, None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplanDecision {
    pub decision: String,
    pub rationale: String,
    pub authorizer: String,
    pub source: String,
    pub decided_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub frontier_tier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontier_reason_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget_before: Option<FrontierBudgetState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget_after: Option<FrontierBudgetState>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub generated_slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub generated_slice_commit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_before_checkpoint_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_after_checkpoint_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_after: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub queue_before_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub queue_after_hash: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_classification: Option<FrontierClassification>,
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
    #[serde(default, skip_serializing_if = "FrontierSummary::is_empty")]
    pub frontier: FrontierSummary,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_classification: Option<FrontierClassification>,
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub frontier_tier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontier_reason_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget_before: Option<FrontierBudgetState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget_after: Option<FrontierBudgetState>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    pub applied_at_checkpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub generated_slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub generated_slice_commit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_before_checkpoint_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apply_after_checkpoint_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_after: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub queue_before_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub queue_after_hash: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GeneratedSliceRecord {
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin_proposal_id: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub generation: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetails {
    pub run: Run,
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
    pub slice_runs: Vec<SliceRun>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_slices: Vec<GeneratedSliceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<RunProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incidents: Vec<RunIncident>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<WorkerQuestion>,
    #[serde(default)]
    pub replan: ReplanStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_envelope: Option<MissionEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget: Option<FrontierBudgetState>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_envelope: Option<MissionEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_budget: Option<FrontierBudgetState>,
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

fn default_mission_verify_profile() -> String {
    "default".to_string()
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
