use serde_json::Value;

pub fn assert_pending_replan(status: &Value, proposal_id: &str) {
    assert_eq!(
        status["replan"]["pending"][0]["id"].as_str(),
        Some(proposal_id)
    );
    assert!(
        status["feed"]["operator_commands"]
            .as_array()
            .expect("operator commands")
            .iter()
            .any(|command| command.as_str().unwrap_or_default().contains(proposal_id)),
        "feed should advertise a command for pending replan {proposal_id}"
    );
}
