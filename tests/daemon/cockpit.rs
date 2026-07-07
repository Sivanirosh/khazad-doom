use serde_json::Value;

pub fn assert_herdr_failure_falls_back_to_direct(status: &Value, slice_id: &str) {
    let events = status["events"].as_array().expect("events");
    assert!(events.iter().any(|event| {
        event["type"].as_str() == Some("run_incident")
            && event["payload"]["kind"].as_str() == Some("cockpit_unavailable")
            && event["payload"]["fallback"].as_str() == Some("direct")
            && event["payload"]["remediation"].as_str().is_some()
    }));
    assert!(events.iter().any(|event| {
        event["type"].as_str() == Some("run_incident")
            && event["payload"]["kind"].as_str() == Some("cockpit_worker_fallback")
            && event["payload"]["fallback"].as_str() == Some("direct")
            && event["payload"]["slice_id"].as_str() == Some(slice_id)
    }));
}
