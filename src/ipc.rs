use crate::domain::{SliceSummary, SliceValidationIssue};
use serde::{Deserialize, Serialize};
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub parallelism: usize,
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub parallelism: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRunResult {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusParams {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
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

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}
