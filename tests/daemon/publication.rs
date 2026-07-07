use serde_json::Value;

pub fn assert_handoff_targets_integration_branch(handoff: &Value, integration_branch: &str) {
    for field in ["push_command", "pr_command"] {
        assert!(
            handoff[field]
                .as_str()
                .unwrap_or_default()
                .contains(integration_branch),
            "{field} should target integration branch {integration_branch}"
        );
    }
}
