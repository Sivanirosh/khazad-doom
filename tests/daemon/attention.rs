use serde_json::Value;

pub fn assert_answer_command_is_advertised(status: &Value, run_id: &str, question_id: &str) {
    let expected = format!("answer {run_id} {question_id}");
    assert!(
        status["feed"]["operator_commands"]
            .as_array()
            .expect("operator commands")
            .iter()
            .any(|command| command.as_str().unwrap_or_default().contains(&expected)),
        "feed should advertise answer command containing {expected}"
    );
}
