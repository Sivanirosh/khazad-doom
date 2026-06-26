use crate::domain::{Handoff, Slice};
use serde::Serialize;

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
    prompt.push_str("- If the slice gives enough authority, proceed. If you must invent intent, return status=blocked with an ask-user finding.\n");
    prompt.push_str("- Preserve unrelated changes.\n");
    prompt.push_str("- Run the requested verification commands when possible.\n");
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
        "\n\nFinal JSON must summarize what you did and include commit_sha if available.\n",
    );
    prompt
}

pub fn integration_repair_prompt(
    run_id: &str,
    integration_worktree: &str,
    slices: &[Slice],
    check_summary: &str,
) -> String {
    format!(
        r#"You are a Khazad-Doom Integration Repair Worker.

Run ID: {run_id}
Worktree: {integration_worktree}

Task:
- Inspect the already-merged integration branch.
- Always perform a quick integration repair pass, even if the expected result is no-op.
- Fix only cross-slice/integration breakage or verification breakage for this run.
- Do not add new product scope.
- Do not rewrite completed slice work for preference.
- If no issue exists, return status "no-op".
- If you fix anything, commit the repair on the current branch and leave the worktree clean.
- If fixing would require inventing product intent, return status "blocked" with an ask-user finding.
- Return only JSON.

Slices now integrated:
{}

Recent check summary:
{check_summary}
"#,
        must_json(&slices)
    )
}

fn must_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unserializable>".to_string())
}
