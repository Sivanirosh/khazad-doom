use crate::domain::{Handoff, Slice};
use serde::Serialize;

const IMPLEMENTER_STYLE_GUIDANCE: &str = "- Follow YAGNI: implement only what the slice/evidence requires.\n- Prefer one-line or surgical fixes when correct and readable; otherwise use the smallest clear change.\n- Do not add abstractions, frameworks, broad refactors, or speculative extensibility.\n";

pub fn worker_prompt(handoff_path: &str, handoff: &Handoff, previous_failure: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are a Khazad-Doom Slice Worker. Implement exactly one JSON Issue Slice.\n\n",
    );
    prompt.push_str("Read this handoff JSON first:\n");
    prompt.push_str(handoff_path);
    prompt.push_str("\n\n");
    prompt.push_str("Contract:\n");
    prompt.push_str("\n- Work only in the provided worktree.\n");
    prompt.push_str("- Implement only the slice described in the handoff.\n");
    prompt.push_str("- The JSON slice is authoritative. GitHub/PRD text is extra context only.\n");
    prompt.push_str("- If the slice gives enough authority, proceed. If you must invent intent, call the ask_operator tool with the question/options when available; return status=blocked with an ask-user finding only if that channel is unavailable or times out.\n");
    prompt.push_str("- Treat acceptance as minimum evidence, not an exhaustive spec: learning is allowed inside the fence; moving the fence requires approval. If TDD or code inspection reveals an additional case directly implied by the slice goal/acceptance and inside declared areas, implement the smallest clear fix and report it in summary/tests/assumptions. If it changes product intent, public API semantics, dependencies, verification policy, or required paths outside areas, return status=blocked with an ask-user finding.\n");
    prompt.push_str("- Preserve unrelated changes.\n");
    prompt.push_str(IMPLEMENTER_STYLE_GUIDANCE);
    prompt.push_str("- Do not run daemon-owned verification commands unless needed for your own confidence; the daemon will run required checks.\n");
    prompt.push_str("- Do not approve your own evidence; acceptance_status and finding dispositions are claims plus evidence only, and the daemon/operator will attest or reject them.\n");
    prompt.push_str("- If a complete result includes findings with action=auto-fix or action=ask-user, include finding_dispositions entries matching each finding by finding_id or 1-based finding_index. Use disposition=fixed/not_applicable/documented for terminal dispositions, or disposition=proposed when a daemon replan proposal/operator decision is required.\n");
    prompt.push_str("- Do not mark actionable findings resolved without a disposition/proposal.\n");
    prompt.push_str("- Commit all intended changes on the current branch before finishing.\n");
    prompt.push_str("- Leave the worktree clean.\n");
    prompt.push_str("- Do not create markdown reports; return only JSON.\n");
    if !previous_failure.is_empty() {
        prompt.push_str("\nPrevious lightweight check failure to repair:\n\n");
        prompt.push_str(previous_failure);
        prompt.push('\n');
    }
    prompt.push_str("\nSlice summary:\n");
    prompt.push_str(&must_json(&handoff.slice));
    prompt.push_str(
        "\n\nFinal JSON must summarize what you did, include commit_sha if available, and include acceptance_status objects for every acceptance criterion with criterion, status, and evidence.\n",
    );
    prompt
}

pub fn integration_repair_prompt(
    run_id: &str,
    integration_worktree: &str,
    slices: &[Slice],
    check_summary: &str,
    gate_summary: &str,
    trigger: &str,
) -> String {
    format!(
        r#"You are a Khazad-Doom Integration Repair Worker.

Run ID: {run_id}
Worktree: {integration_worktree}
Repair trigger: {trigger}

Task:
- Inspect the already-merged integration branch.
{IMPLEMENTER_STYLE_GUIDANCE}- Repair only the integration breakage evidenced below.
- Do not add new product scope.
- Do not rewrite completed slice work for preference.
- Do not rerun the full daemon verification suite; the daemon will rerun the integration gate.
- If no issue exists, return status "no-op".
- If you fix anything, commit the repair on the current branch and leave the worktree clean.
- Repair only files covered by the integrated slices' areas; do not mutate .workflow policy/slices/profiles/verification. If repair needs out-of-area or workflow-policy changes, return finding_dispositions with disposition="proposed" so the daemon can record an operator-approved replan proposal instead of applying it silently.
- If a successful repair output includes findings with action=auto-fix or action=ask-user, include finding_dispositions entries matching each finding by finding_id or 1-based finding_index.
- If fixing would require inventing product intent, return status "blocked" with an ask-user finding.
- Return only JSON.

Slices now integrated:
{}

Worker check summary:
{check_summary}

Integration gate evidence:
{gate_summary}
"#,
        must_json(&slices)
    )
}

fn must_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unserializable>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn worker_prompt_authorizes_within_intent_tdd_discoveries() {
        let slice = Slice {
            id: "slice-test".to_string(),
            title: "Test anti-waterfall prompt".to_string(),
            goal: "Support the intended behavior".to_string(),
            github_issue: String::new(),
            status: "open".to_string(),
            closed_by_run: String::new(),
            closed_at: String::new(),
            depends_on: Vec::new(),
            areas: vec!["src/".to_string(), "tests/".to_string()],
            acceptance: vec!["Behavior is covered by tests".to_string()],
            must_ask_if: Vec::new(),
            verify_profile: String::new(),
            verify: Vec::new(),
            verify_timeout_seconds: 0,
        };
        let handoff = Handoff {
            run_id: "kd-test".to_string(),
            role: "implementer".to_string(),
            repo_path: "/repo".to_string(),
            worktree_path: "/repo/.worktree".to_string(),
            branch: "khazad/slice-test".to_string(),
            slice,
            dependency_summary: BTreeMap::new(),
            worker_profile: Default::default(),
            agent_profile: String::new(),
            agent_provider: String::new(),
            agent_model: String::new(),
            agent_reasoning: String::new(),
            agent_mode: String::new(),
            profile_summary: String::new(),
            launch_summary: String::new(),
            output_path: "/tmp/output.json".to_string(),
            contract: "Return JSON".to_string(),
        };

        let prompt = worker_prompt("/tmp/handoff.json", &handoff, "");

        assert!(prompt.contains("acceptance as minimum evidence"));
        assert!(prompt.contains("learning is allowed inside the fence"));
        assert!(prompt.contains("directly implied by the slice goal/acceptance"));
        assert!(prompt.contains("status=blocked with an ask-user finding"));
    }
}
