use std::fs;
use std::path::{Path, PathBuf};

const PI_CONTRACT_OWNER: &str = "src/pi_contract.rs";
const PAINTER_ENTRYPOINTS: &[&str] = &["src/cli.rs", "src/workflow/cockpit.rs"];

const PI_EVENT_FIELD_NAMES: &[&str] = &[
    "assistantMessageEvent",
    "contentIndex",
    "contractVersion",
    "contract_version",
    "durationMs",
    "elapsedMs",
    "toolCallId",
    "toolName",
];

const PI_LAUNCH_WIRE_STRINGS: &[&str] = &["--mode", "--no-session"];

const DAEMON_ACTIVITY_EVENT_NAMES: &[&str] = &[
    "checkpoint_written",
    "cockpit_ready",
    "cockpit_worker_ready",
    "implementation_summary",
    "integration_repair_completed",
    "progress",
    "run_completed",
    "run_started",
    "slice_merged",
    "slice_started",
    "terminal_notification_sent",
    "terminal_notification_skipped",
    "terminal_summary_written",
    "worker_question_answered",
    "worker_question_asked",
    "worktrees_cleaned",
];

#[test]
fn confinement_pi_wire_vocabulary_is_owned_by_pi_contract_layer() {
    let root = repo_root();
    let mut guarded_strings = pi_event_vocabulary(&root);
    guarded_strings.extend(
        PI_EVENT_FIELD_NAMES
            .iter()
            .map(|value| (*value).to_string()),
    );
    guarded_strings.extend(
        PI_LAUNCH_WIRE_STRINGS
            .iter()
            .map(|value| (*value).to_string()),
    );
    guarded_strings.sort();
    guarded_strings.dedup();

    let mut violations = Vec::new();
    for file in rust_source_files(&root.join("src")) {
        let relative = relative_path(&root, &file);
        if relative == PI_CONTRACT_OWNER {
            continue;
        }
        let source = production_source(
            &fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", file.display())),
        );
        collect_literal_matches(
            &mut violations,
            "Pi wire-string ownership",
            &relative,
            &source,
            &guarded_strings,
        );
    }

    assert!(
        violations.is_empty(),
        "Pi wire-string confinement violated: {PI_CONTRACT_OWNER} owns Pi event vocabulary, event field names, and launch wire strings; renderers, painters, and workflow code must call the Pi contract layer instead of matching raw Pi wire values. Unexpected matches:\n{}",
        violations.join("\n")
    );
}

#[test]
fn confinement_cockpit_painters_do_not_interpret_daemon_event_names() {
    let root = repo_root();
    let guarded_strings = DAEMON_ACTIVITY_EVENT_NAMES
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let mut violations = Vec::new();

    for relative in PAINTER_ENTRYPOINTS {
        let file = root.join(relative);
        let source = production_source(
            &fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", file.display())),
        );
        collect_literal_matches(
            &mut violations,
            "painter event-name boundary",
            relative,
            &source,
            &guarded_strings,
        );
    }

    assert!(
        violations.is_empty(),
        "Painter event-name confinement violated: cockpit painter entrypoints must paint StatusFeed/PiActivityFormatter output and must not grow ad-hoc raw daemon event-name interpretation. Keep event-name interpretation in projection/read-model owners. Unexpected matches:\n{}",
        violations.join("\n")
    );
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn pi_event_vocabulary(root: &Path) -> Vec<String> {
    let source = fs::read_to_string(root.join(PI_CONTRACT_OWNER))
        .unwrap_or_else(|err| panic!("failed to read {PI_CONTRACT_OWNER}: {err}"));
    let mut vocabulary = Vec::new();
    vocabulary.extend(extract_const_string_array(&source, "KNOWN_EVENT_TYPES"));
    vocabulary.extend(extract_const_string_array(
        &source,
        "KNOWN_ASSISTANT_EVENT_TYPES",
    ));
    vocabulary.sort();
    vocabulary.dedup();

    assert!(
        vocabulary.iter().any(|value| value == "message_update")
            && vocabulary.iter().any(|value| value == "tool_execution_end")
            && vocabulary.iter().any(|value| value == "text_delta"),
        "confinement test could not read the expected Pi event vocabulary from {PI_CONTRACT_OWNER}"
    );

    vocabulary
}

fn extract_const_string_array(source: &str, const_name: &str) -> Vec<String> {
    let marker = format!("{const_name}:");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("missing {const_name} in {PI_CONTRACT_OWNER}"));
    let rest = &source[start..];
    let open = rest
        .find("&[")
        .unwrap_or_else(|| panic!("missing array start for {const_name}"));
    let rest = &rest[(open + 2)..];
    let close = rest
        .find("];")
        .unwrap_or_else(|| panic!("missing array end for {const_name}"));
    rust_string_literals(&rest[..close])
}

fn rust_string_literals(source: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut value = String::new();
        let mut escaped = false;
        for ch in chars.by_ref() {
            if escaped {
                value.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => break,
                _ => value.push(ch),
            }
        }
        values.push(value);
    }
    values
}

fn rust_source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_source_files(root, &mut files);
    files.sort();
    files
}

fn collect_rust_source_files(path: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(path)
        .unwrap_or_else(|err| panic!("failed to read directory {}: {err}", path.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read directory entry: {err}"));
        let path = entry.path();
        if path.is_dir() {
            collect_rust_source_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn production_source(source: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut pending_test_attr = false;
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if pending_test_attr && trimmed.starts_with("mod tests") {
            break;
        }
        if trimmed == "#[cfg(test)]" {
            pending_test_attr = true;
            lines.push((index + 1, line.to_string()));
            continue;
        }
        if pending_test_attr && !trimmed.is_empty() && !trimmed.starts_with("#[") {
            pending_test_attr = false;
        }
        lines.push((index + 1, line.to_string()));
    }
    lines
}

fn collect_literal_matches(
    violations: &mut Vec<String>,
    boundary: &str,
    relative: &str,
    source: &[(usize, String)],
    needles: &[String],
) {
    for (line_number, line) in source {
        for needle in needles {
            if contains_wire_literal(line, needle) {
                violations.push(format!(
                    "{boundary}: {relative}:{line_number} unexpectedly matched {needle:?}: {}",
                    line.trim()
                ));
            }
        }
    }
}

fn contains_wire_literal(line: &str, needle: &str) -> bool {
    let quoted = format!("\"{needle}\"");
    let escaped = format!("\\\"{needle}\\\"");
    line.contains(&quoted) || line.contains(&escaped)
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
