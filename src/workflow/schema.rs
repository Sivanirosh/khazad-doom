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
    }
  },
  "required": ["status", "summary"]
}"#;
