use crate::domain::{
    MissionEnvelope, ReplanEvidenceLink, ReplanProposal, ReplanProposalSource,
    ReplanProposedChange, SliceSummary, SliceValidationIssue, WorkerQuestion,
    WorkerQuestionAnswerSource,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitRepoParams {
    pub repo_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitRepoResult {
    pub repo_id: String,
    pub repo_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRunParams {
    pub repo_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slice_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pi_bin: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pi_args: Vec<String>,
    #[serde(
        default,
        alias = "experimental_pi_tui_worker",
        skip_serializing_if = "is_false"
    )]
    pub native_pi_tui_worker: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub parallelism: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_dirty: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin_notification_target: String,
    #[serde(default, alias = "envelope", skip_serializing_if = "Option::is_none")]
    pub mission_envelope: Option<MissionEnvelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeRunParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pi_bin: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pi_args: Vec<String>,
    #[serde(
        default,
        alias = "experimental_pi_tui_worker",
        skip_serializing_if = "is_false"
    )]
    pub native_pi_tui_worker: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub parallelism: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRunResult {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAskParams {
    pub run_id: String,
    pub slice_id: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempt: usize,
    /// Immutable daemon launch identity. Absent for legacy workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_recommendation_string",
        skip_serializing_if = "String::is_empty"
    )]
    pub recommended_answer: String,
    #[serde(
        default,
        deserialize_with = "deserialize_recommendation_string",
        skip_serializing_if = "String::is_empty"
    )]
    pub rationale: String,
    #[serde(
        default,
        deserialize_with = "deserialize_recommendation_bool",
        skip_serializing_if = "is_false"
    )]
    pub bounded_within_current_slice_or_mission_authority: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_recommendation_bool",
        skip_serializing_if = "is_false"
    )]
    pub reversible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAskResult {
    pub question_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_source: Option<WorkerQuestionAnswerSource>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommended_answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommendation_rationale: String,
    #[serde(default)]
    pub fallback_eligible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListQuestionsParams {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListQuestionsResult {
    pub questions: Vec<WorkerQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerQuestionParams {
    pub run_id: String,
    pub question_id: String,
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerQuestionResult {
    pub question: WorkerQuestion,
    #[serde(default)]
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerQuestionTimeoutParams {
    pub run_id: String,
    pub question_id: String,
    pub token: String,
    /// Immutable daemon launch identity. Absent for legacy workers/questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListReplanProposalsParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListReplanProposalsResult {
    pub proposals: Vec<ReplanProposal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateReplanProposalParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub source: ReplanProposalSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_finding_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ReplanEvidenceLink>,
    pub proposed_changes: Vec<ReplanProposedChange>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub risk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReplanProposalResult {
    pub proposal: ReplanProposal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DecideReplanProposalParams {
    pub run_id: String,
    pub proposal_id: String,
    pub decision: String,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub authorizer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub replacement_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revisit_condition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecideReplanProposalResult {
    pub proposal: ReplanProposal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusParams {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub latest: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub active_only: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub limit: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub events_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRunParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRunResult {
    pub run_id: String,
    pub status: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlicesParams {
    pub repo_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceNewParams {
    pub repo_path: String,
    pub id: String,
    pub title: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub github_issue: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceImportGithubParams {
    pub repo_path: String,
    pub issue: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub overwrite: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub push: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub create_pr: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectRunParams {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub log_tail_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSlicesResult {
    pub slices: Vec<SliceSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<SliceValidationIssue>,
}

fn deserialize_recommendation_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::String(value) => value,
        _ => String::new(),
    })
}

fn deserialize_recommendation_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(matches!(
        Value::deserialize(deserializer)?,
        Value::Bool(true)
    ))
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_pi_tui_worker_accepts_legacy_ipc_field() {
        let start: StartRunParams = serde_json::from_value(json!({
            "repo_path": "/tmp/repo",
            "experimental_pi_tui_worker": true
        }))
        .expect("start params decode");
        assert!(start.native_pi_tui_worker);

        let resume: ResumeRunParams = serde_json::from_value(json!({
            "run_id": "kd-test",
            "experimental_pi_tui_worker": true
        }))
        .expect("resume params decode");
        assert!(resume.native_pi_tui_worker);
    }
}
