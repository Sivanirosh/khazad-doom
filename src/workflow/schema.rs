pub const WORKER_RESULT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "slice_id": {"type": "string"},
    "status": {"type": "string", "enum": ["complete", "blocked", "failed"]},
    "summary": {"type": "string"},
    "commit_sha": {"type": "string"},
    "commit_message": {"type": "string"},
    "changed_files": {"type": "array", "items": {"type": "string"}},
    "public_interfaces_changed": {"type": "array", "items": {"type": "string"}},
    "tests_run": {"type": "array", "items": {"type": "string"}},
    "acceptance_status": {
      "description": "Worker evidence claims only; not approval. Khazad-Doom attests or rejects evidence separately.",
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "criterion": {"type": "string"},
          "status": {"type": "string", "enum": ["satisfied", "blocked", "failed"]},
          "evidence": {"type": "string"}
        },
        "required": ["criterion", "status", "evidence"]
      }
    },
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "id": {"type": "string"},
          "severity": {"type": "string", "enum": ["error", "warning", "info"]},
          "action": {"type": "string", "enum": ["auto-fix", "ask-user", "no-op"]},
          "file": {"type": "string"},
          "line": {"type": "integer"},
          "description": {"type": "string"}
        },
        "required": ["severity", "action", "description"]
      }
    },
    "finding_dispositions": {
      "description": "Required for each actionable finding in a successful output. Workers claim disposition only; the daemon/operator attests or rejects it.",
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "finding_id": {"type": "string"},
          "finding_index": {"type": "integer", "minimum": 1},
          "disposition": {"type": "string", "enum": ["fixed", "not_applicable", "documented", "proposed"]},
          "replan_proposal_id": {"type": "string"},
          "rationale": {"type": "string"}
        },
        "required": ["disposition", "rationale"]
      }
    },
    "candidate_followup_slices": {
      "description": "Optional complete follow-up slice drafts proposed by the worker. The daemon validates each draft against slice and area-contract rules before creating any replan proposal.",
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "id": {"type": "string"},
          "title": {"type": "string"},
          "goal": {"type": "string"},
          "areas": {"type": "array", "items": {"type": "string"}},
          "acceptance": {"type": "array", "items": {"type": "string"}},
          "verify": {"type": "array", "items": {"type": "string"}},
          "verify_profile": {"type": "string"},
          "depends_on": {"type": "array", "items": {"type": "string"}},
          "must_ask_if": {"type": "array", "items": {"type": "string"}},
          "rationale": {"type": "string"}
        },
        "required": ["id", "title", "goal", "areas", "acceptance", "verify", "verify_profile", "depends_on", "must_ask_if", "rationale"]
      }
    },
    "assumptions": {"type": "array", "items": {"type": "string"}}
  },
  "required": ["slice_id", "status", "summary", "acceptance_status"]
}"#;

pub const REPAIR_RESULT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "status": {"type": "string", "enum": ["no-op", "fixed", "blocked", "failed"]},
    "summary": {"type": "string"},
    "commit_sha": {"type": "string"},
    "changed_files": {"type": "array", "items": {"type": "string"}},
    "tests_run": {"type": "array", "items": {"type": "string"}},
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "id": {"type": "string"},
          "severity": {"type": "string", "enum": ["error", "warning", "info"]},
          "action": {"type": "string", "enum": ["auto-fix", "ask-user", "no-op"]},
          "file": {"type": "string"},
          "line": {"type": "integer"},
          "description": {"type": "string"}
        },
        "required": ["severity", "action", "description"]
      }
    },
    "finding_dispositions": {
      "description": "Required for each actionable finding in a successful repair output. Use disposition=proposed when operator approval/replan is required.",
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "finding_id": {"type": "string"},
          "finding_index": {"type": "integer", "minimum": 1},
          "disposition": {"type": "string", "enum": ["fixed", "not_applicable", "documented", "proposed"]},
          "replan_proposal_id": {"type": "string"},
          "rationale": {"type": "string"}
        },
        "required": ["disposition", "rationale"]
      }
    },
    "candidate_followup_slices": {
      "description": "Optional complete follow-up slice drafts proposed by the repair worker. The daemon validates each draft against slice and area-contract rules before creating any replan proposal.",
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "id": {"type": "string"},
          "title": {"type": "string"},
          "goal": {"type": "string"},
          "areas": {"type": "array", "items": {"type": "string"}},
          "acceptance": {"type": "array", "items": {"type": "string"}},
          "verify": {"type": "array", "items": {"type": "string"}},
          "verify_profile": {"type": "string"},
          "depends_on": {"type": "array", "items": {"type": "string"}},
          "must_ask_if": {"type": "array", "items": {"type": "string"}},
          "rationale": {"type": "string"}
        },
        "required": ["id", "title", "goal", "areas", "acceptance", "verify", "verify_profile", "depends_on", "must_ask_if", "rationale"]
      }
    }
  },
  "required": ["status", "summary"]
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RepairResult, ReplanProposedChange, WorkerResult};
    use serde_json::{Value, json};

    #[test]
    fn worker_output_schema_accepts_candidate_followup_slices_without_breaking_legacy_outputs() {
        let worker_schema: Value = serde_json::from_str(WORKER_RESULT_SCHEMA).unwrap();
        assert!(worker_schema["properties"]["candidate_followup_slices"].is_object());
        let repair_schema: Value = serde_json::from_str(REPAIR_RESULT_SCHEMA).unwrap();
        assert!(repair_schema["properties"]["candidate_followup_slices"].is_object());

        let legacy_worker: WorkerResult = serde_json::from_value(json!({
            "slice_id": "slice-001",
            "status": "complete",
            "summary": "done",
            "acceptance_status": [{
                "criterion": "done",
                "status": "satisfied",
                "evidence": "legacy output still decodes"
            }]
        }))
        .unwrap();
        assert!(legacy_worker.candidate_followup_slices.is_empty());

        let worker_with_candidate: WorkerResult = serde_json::from_value(json!({
            "slice_id": "slice-001",
            "status": "complete",
            "summary": "done with candidate",
            "acceptance_status": [{
                "criterion": "done",
                "status": "satisfied",
                "evidence": "candidate output decodes"
            }],
            "candidate_followup_slices": [{
                "id": "slice-001-followup",
                "title": "Follow-up",
                "goal": "Do the follow-up",
                "areas": ["src/"],
                "acceptance": ["follow-up works"],
                "verify": ["cargo test followup"],
                "verify_profile": "quick",
                "depends_on": ["slice-001"],
                "must_ask_if": ["intent changes"],
                "rationale": "Worker found a bounded follow-up."
            }]
        }))
        .unwrap();
        assert_eq!(
            worker_with_candidate.candidate_followup_slices[0].id,
            "slice-001-followup"
        );

        let legacy_repair: RepairResult = serde_json::from_value(json!({
            "status": "no-op",
            "summary": "legacy repair output still decodes"
        }))
        .unwrap();
        assert!(legacy_repair.candidate_followup_slices.is_empty());
    }

    #[test]
    fn legacy_replan_proposed_change_json_decodes_without_followup_slice_draft() {
        let change: ReplanProposedChange = serde_json::from_value(json!({
            "kind": "follow_up_or_revision",
            "target": "slice-001",
            "summary": "legacy prose-only proposal"
        }))
        .unwrap();
        assert!(change.followup_slice_draft().is_none());
    }
}
