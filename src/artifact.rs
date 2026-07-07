use crate::domain::{
    AgentProfilesConfig, ArtifactEntry, Handoff, ImplementationSummary, OriginNotificationTarget,
    RunCheckpoint, Slice, SliceSummary, SliceValidationIssue, SliceValidationReport,
    SliceWriteResult, TerminalNotificationRecord, WorkflowConfig,
};
use crate::gitutil;
use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = ".workflow";

#[derive(Debug, Clone)]
pub struct Store {
    repo_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct SliceClosureIncident {
    pub severity: String,
    pub kind: String,
    pub slice_id: String,
    pub path: String,
    pub message: String,
    pub policy: String,
}

#[derive(Debug, Clone, Default)]
pub struct SliceClosureReport {
    pub incidents: Vec<SliceClosureIncident>,
}

impl SliceClosureReport {
    pub fn blocks_handoff(&self) -> bool {
        self.incidents
            .iter()
            .any(|incident| incident.severity == "error")
    }
}

impl Store {
    pub fn new(repo_path: impl AsRef<Path>) -> Self {
        Self {
            repo_path: repo_path.as_ref().to_path_buf(),
        }
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.slices_dir(),
            self.plans_dir(),
            self.reports_dir(),
            self.runs_dir(),
            self.schema_dir(),
        ] {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        self.ensure_default_config()?;
        self.write_slice_schema()?;
        ensure_gitignore(&self.repo_path)
    }

    pub fn workflow_dir(&self) -> PathBuf {
        self.repo_path.join(DIR_NAME)
    }

    pub fn slices_dir(&self) -> PathBuf {
        self.workflow_dir().join("slices")
    }

    pub fn plans_dir(&self) -> PathBuf {
        self.workflow_dir().join("plans")
    }

    pub fn reports_dir(&self) -> PathBuf {
        self.workflow_dir().join("reports")
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.workflow_dir().join("runs")
    }

    pub fn schema_dir(&self) -> PathBuf {
        self.workflow_dir().join("schema")
    }

    pub fn config_path(&self) -> PathBuf {
        self.workflow_dir().join("khazad.json")
    }

    pub fn slice_schema_path(&self) -> PathBuf {
        self.schema_dir().join("slice.schema.json")
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }

    pub fn handoff_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("handoffs")
    }

    pub fn output_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("outputs")
    }

    pub fn origin_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("origin.json")
    }

    pub fn notifications_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("notifications")
    }

    pub fn terminal_notification_path(&self, run_id: &str, transition_key: &str) -> PathBuf {
        self.notifications_dir(run_id).join(format!(
            "terminal-{}.json",
            safe_filename_segment(transition_key)
        ))
    }

    pub fn ensure_run_dirs(&self, run_id: &str) -> Result<()> {
        for dir in [self.handoff_dir(run_id), self.output_dir(run_id)] {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn load_slices(&self) -> Result<Vec<Slice>> {
        let report = self.validate_slices_report()?;
        if !report.valid {
            let messages: Vec<_> = report
                .issues
                .iter()
                .filter(|issue| issue.severity == "error")
                .map(|issue| {
                    if issue.file.is_empty() {
                        issue.message.clone()
                    } else {
                        format!("{}: {}", issue.file, issue.message)
                    }
                })
                .collect();
            bail!("slice validation failed: {}", messages.join("; "));
        }
        let mut slices = self.load_parseable_slices()?;
        slices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(slices)
    }

    pub fn validate_slices_report(&self) -> Result<SliceValidationReport> {
        let mut issues = Vec::new();
        let mut slices = Vec::new();
        let dir = self.slices_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SliceValidationReport {
                    valid: true,
                    slices: Vec::new(),
                    issues: Vec::new(),
                });
            }
            Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir()
                || path.extension().and_then(|ext| ext.to_str()) != Some("json")
            {
                continue;
            }
            paths.push(path);
        }
        paths.sort();
        for path in paths {
            match read_json::<Slice>(&path) {
                Ok(slice) => match validate_slice(&slice) {
                    Ok(()) => slices.push(slice),
                    Err(err) => issues.push(issue_for_path(
                        &path,
                        &slice.id,
                        format!("invalid slice: {err}"),
                    )),
                },
                Err(err) => issues.push(issue_for_path(
                    &path,
                    "",
                    format!("invalid JSON slice: {err}"),
                )),
            }
        }
        issues.extend(validate_slice_set(&slices));
        slices.sort_by(|a, b| a.id.cmp(&b.id));
        let summaries = slices.iter().map(SliceSummary::from).collect();
        Ok(SliceValidationReport {
            valid: !issues.iter().any(|issue| issue.severity == "error"),
            slices: summaries,
            issues,
        })
    }

    fn load_parseable_slices(&self) -> Result<Vec<Slice>> {
        let dir = self.slices_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
        };
        let mut slices = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir()
                || path.extension().and_then(|ext| ext.to_str()) != Some("json")
            {
                continue;
            }
            let slice: Slice = read_json(&path)?;
            validate_slice(&slice)?;
            slices.push(slice);
        }
        Ok(slices)
    }

    pub fn write_handoff(&self, run_id: &str, handoff: &Handoff) -> Result<PathBuf> {
        self.ensure_run_dirs(run_id)?;
        let path = self
            .handoff_dir(run_id)
            .join(format!("{}.json", handoff.slice.id));
        write_json(&path, handoff)?;
        Ok(path)
    }

    pub fn write_origin_notification_target(
        &self,
        run_id: &str,
        origin: &OriginNotificationTarget,
    ) -> Result<PathBuf> {
        self.ensure_run_dirs(run_id)?;
        let path = self.origin_path(run_id);
        write_json(&path, origin)?;
        Ok(path)
    }

    pub fn read_origin_notification_target(
        &self,
        run_id: &str,
    ) -> Result<Option<OriginNotificationTarget>> {
        let path = self.origin_path(run_id);
        if !path.exists() {
            return Ok(None);
        }
        read_json(path).map(Some)
    }

    pub fn terminal_notification_exists(&self, run_id: &str, transition_key: &str) -> bool {
        self.terminal_notification_path(run_id, transition_key)
            .exists()
    }

    pub fn write_terminal_notification_record(
        &self,
        run_id: &str,
        transition_key: &str,
        record: &TerminalNotificationRecord,
    ) -> Result<PathBuf> {
        self.ensure_run_dirs(run_id)?;
        let dir = self.notifications_dir(run_id);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let path = self.terminal_notification_path(run_id, transition_key);
        write_json(&path, record)?;
        Ok(path)
    }

    pub fn output_path(&self, run_id: &str, name: &str) -> PathBuf {
        self.output_dir(run_id).join(name)
    }

    pub fn implementation_summary_report_path(&self, run_id: &str) -> PathBuf {
        self.reports_dir()
            .join(format!("{run_id}-implementation-summary.json"))
    }

    pub fn final_report_artifact_path(&self, run_id: &str) -> PathBuf {
        self.reports_dir()
            .join(format!("{run_id}-final-report.json"))
    }

    pub fn publication_reports_exist(&self, run_id: &str) -> bool {
        self.implementation_summary_report_path(run_id).exists()
            && self.final_report_artifact_path(run_id).exists()
    }

    pub fn slice_path(&self, slice_id: &str) -> PathBuf {
        self.slices_dir().join(format!("{slice_id}.json"))
    }

    pub fn read_config(&self) -> Result<WorkflowConfig> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(WorkflowConfig::default());
        }
        read_json(path)
    }

    pub fn ensure_default_config(&self) -> Result<()> {
        let path = self.config_path();
        if !path.exists() {
            write_json(path, &WorkflowConfig::default())?;
        }
        Ok(())
    }

    pub fn write_slice_schema(&self) -> Result<PathBuf> {
        let path = self.slice_schema_path();
        write_json(&path, &slice_schema())?;
        Ok(path)
    }

    pub fn write_slice(&self, slice: &Slice, overwrite: bool) -> Result<SliceWriteResult> {
        validate_slice(slice)?;
        self.ensure_layout()?;
        let path = self.slice_path(&slice.id);
        if path.exists() && !overwrite {
            bail!(
                "slice {:?} already exists at {}; use --overwrite or choose a different --id",
                slice.id,
                path.display()
            );
        }
        write_json(&path, slice)?;
        Ok(SliceWriteResult {
            slice: slice.clone(),
            path: path.to_string_lossy().to_string(),
            written: true,
        })
    }

    pub fn close_slices_if_present(
        &self,
        slice_ids: &[String],
        run_id: &str,
        closed_at: &str,
    ) -> SliceClosureReport {
        let mut report = SliceClosureReport::default();
        for slice_id in slice_ids {
            let path = self.slice_path(slice_id);
            if !path.exists() {
                report.incidents.push(SliceClosureIncident {
                    severity: "warning".to_string(),
                    kind: "slice_close_skipped".to_string(),
                    slice_id: slice_id.clone(),
                    path: path.to_string_lossy().to_string(),
                    message: format!(
                        "slice metadata for {slice_id} was not present at {}; closure skipped",
                        path.display()
                    ),
                    policy: "preserve_handoff_ready_missing_metadata".to_string(),
                });
                continue;
            }
            if let Err(err) = self.close_slice_file(slice_id, run_id, closed_at) {
                report.incidents.push(SliceClosureIncident {
                    severity: "error".to_string(),
                    kind: "slice_close_failed".to_string(),
                    slice_id: slice_id.clone(),
                    path: path.to_string_lossy().to_string(),
                    message: format!(
                        "failed to close slice {slice_id} at {}: {err:#}",
                        path.display()
                    ),
                    policy: "block_handoff_on_close_failure".to_string(),
                });
            }
        }
        report
    }

    fn close_slice_file(&self, slice_id: &str, run_id: &str, closed_at: &str) -> Result<()> {
        let path = self.slice_path(slice_id);
        let mut slice: Slice = read_json(&path)
            .with_context(|| format!("read slice {} for closing", path.display()))?;
        if slice.status == crate::domain::SLICE_STATUS_CLOSED {
            if slice.closed_by_run == run_id && !slice.closed_at.trim().is_empty() {
                return Ok(());
            }
            bail!(
                "slice {slice_id} is already closed by run {:?}; refusing to overwrite close metadata for run {run_id:?}",
                slice.closed_by_run
            );
        }
        slice.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
        slice.closed_by_run = run_id.to_string();
        slice.closed_at = closed_at.to_string();
        validate_slice(&slice)?;
        write_json(&path, &slice)
            .with_context(|| format!("write closed slice {}", path.display()))?;
        Ok(())
    }

    pub fn write_checkpoint(&self, checkpoint: &RunCheckpoint) -> Result<PathBuf> {
        let path = self.output_path(&checkpoint.run_id, "checkpoint.json");
        write_json(&path, checkpoint)?;
        Ok(path)
    }

    pub fn read_checkpoint(&self, run_id: &str) -> Result<RunCheckpoint> {
        read_json(self.output_path(run_id, "checkpoint.json"))
    }

    pub fn write_implementation_summary(&self, summary: &ImplementationSummary) -> Result<PathBuf> {
        let path = self.implementation_summary_report_path(&summary.run_id);
        write_json(&path, summary)?;
        Ok(path)
    }

    pub fn write_final_report(&self, summary: &ImplementationSummary) -> Result<PathBuf> {
        let path = self.final_report_artifact_path(&summary.run_id);
        write_json(&path, summary)
            .with_context(|| format!("write final report {}", path.display()))?;
        Ok(path)
    }

    pub fn commit_completion_publication(&self, run_id: &str) -> Result<()> {
        gitutil::commit_all(
            &self.repo_path,
            &format!("khazad(run): publish completion {run_id}"),
        )
    }

    pub fn list_run_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactEntry>> {
        let mut entries = Vec::new();
        collect_dir_entries(&mut entries, "handoff", &self.handoff_dir(run_id))?;
        collect_dir_entries(&mut entries, "output", &self.output_dir(run_id))?;
        collect_report_entries(&mut entries, run_id, &self.reports_dir())?;
        entries.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));
        Ok(entries)
    }
}

fn safe_filename_segment(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

pub fn slice_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://khazad-doom.local/slice.schema.json",
        "title": "Khazad-Doom JSON Issue Slice",
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "title", "goal", "acceptance"],
        "properties": {
            "id": { "type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]*$" },
            "title": { "type": "string", "minLength": 1 },
            "goal": { "type": "string", "minLength": 1 },
            "github_issue": { "type": "string" },
            "status": { "type": "string", "enum": ["open", "closed"] },
            "closed_by_run": { "type": "string" },
            "closed_at": { "type": "string" },
            "depends_on": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
            "areas": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
            "acceptance": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
            "must_ask_if": { "type": "array", "items": { "type": "string" } },
            "verify_profile": { "type": "string" },
            "verify": { "type": "array", "items": { "type": "string" } },
            "verify_timeout_seconds": { "type": "integer", "minimum": 0, "maximum": 86400 }
        }
    })
}

pub fn validate_slice(slice: &Slice) -> Result<()> {
    if slice.id.trim().is_empty() {
        bail!("id is required");
    }
    if !is_safe_slice_id(&slice.id) {
        bail!(
            "id must be path/ref safe: use ASCII letters, digits, '.', '_' or '-', start with a letter or digit, do not end with '.', and do not contain '..' or '.lock'"
        );
    }
    if slice.title.trim().is_empty() {
        bail!("title is required");
    }
    if slice.goal.trim().is_empty() {
        bail!("goal is required");
    }
    if slice.acceptance.is_empty() {
        bail!("acceptance must contain at least one criterion");
    }
    if !matches!(slice.status.as_str(), "open" | "closed") {
        bail!("status must be either 'open' or 'closed'");
    }
    if slice.status == "open" && (!slice.closed_by_run.is_empty() || !slice.closed_at.is_empty()) {
        bail!("open slices must not set closed_by_run or closed_at");
    }
    if slice.verify_timeout_seconds > 86_400 {
        bail!("verify_timeout_seconds must be <= 86400");
    }
    for dep in &slice.depends_on {
        if !is_safe_slice_id(dep) {
            bail!("dependency id {dep:?} is not path/ref safe");
        }
        if dep == &slice.id {
            bail!("slice cannot depend on itself");
        }
    }
    Ok(())
}

pub fn validate_slice_set(slices: &[Slice]) -> Vec<SliceValidationIssue> {
    let mut issues = Vec::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for slice in slices {
        *seen.entry(&slice.id).or_default() += 1;
    }
    for (id, count) in seen.iter().filter(|(_, count)| **count > 1) {
        issues.push(issue(
            "",
            id,
            format!("duplicate slice id {id:?} appears {count} times"),
        ));
    }
    let ids: BTreeSet<_> = slices.iter().map(|slice| slice.id.as_str()).collect();
    for slice in slices {
        for dep in &slice.depends_on {
            if !ids.contains(dep.as_str()) {
                issues.push(issue(
                    "",
                    &slice.id,
                    format!("missing dependency {dep:?} for slice {:?}", slice.id),
                ));
            }
        }
    }
    issues.extend(cycle_issues(slices));
    issues
}

pub fn topological_order(slices: &[Slice], requested: &[String]) -> Result<Vec<Slice>> {
    let by_id: BTreeMap<_, _> = slices
        .iter()
        .map(|slice| (slice.id.as_str(), slice))
        .collect();
    let wanted = wanted_slice_ids(slices, requested, &by_id)?;

    let mut ordered = Vec::new();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for id in wanted.clone() {
        visit_ordered(
            &id,
            &by_id,
            &wanted,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }
    Ok(ordered.into_iter().cloned().collect())
}

pub fn dependency_layers(slices: &[Slice]) -> Result<Vec<Vec<Slice>>> {
    let mut pending: BTreeMap<String, Slice> = slices
        .iter()
        .map(|slice| (slice.id.clone(), slice.clone()))
        .collect();
    let selected: BTreeSet<_> = pending.keys().cloned().collect();
    let mut completed = BTreeSet::new();
    let mut layers = Vec::new();
    while !pending.is_empty() {
        let ready_ids: Vec<_> = pending
            .values()
            .filter(|slice| {
                slice
                    .depends_on
                    .iter()
                    .filter(|dep| selected.contains(*dep))
                    .all(|dep| completed.contains(dep))
            })
            .map(|slice| slice.id.clone())
            .collect();
        if ready_ids.is_empty() {
            bail!("slice dependency cycle or missing dependency in selected set");
        }
        let mut layer = Vec::new();
        for id in ready_ids {
            if let Some(slice) = pending.remove(&id) {
                completed.insert(id);
                layer.push(slice);
            }
        }
        layer.sort_by(|a, b| a.id.cmp(&b.id));
        layers.push(layer);
    }
    Ok(layers)
}

fn wanted_slice_ids<'a>(
    slices: &[Slice],
    requested: &[String],
    by_id: &BTreeMap<&'a str, &'a Slice>,
) -> Result<BTreeSet<String>> {
    let mut wanted = BTreeSet::new();
    if requested.is_empty() {
        wanted.extend(slices.iter().map(|slice| slice.id.clone()));
    } else {
        for id in requested {
            collect_with_dependencies(id, by_id, &mut wanted)?;
        }
    }
    Ok(wanted)
}

fn collect_with_dependencies<'a>(
    id: &str,
    by_id: &BTreeMap<&'a str, &'a Slice>,
    wanted: &mut BTreeSet<String>,
) -> Result<()> {
    let slice = by_id
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("slice {id:?} not found"))?;
    if !wanted.insert(slice.id.clone()) {
        return Ok(());
    }
    for dep in &slice.depends_on {
        collect_with_dependencies(dep, by_id, wanted)?;
    }
    Ok(())
}

fn visit_ordered<'a>(
    id: &str,
    by_id: &BTreeMap<&'a str, &'a Slice>,
    wanted: &BTreeSet<String>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    ordered: &mut Vec<&'a Slice>,
) -> Result<()> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id.to_string()) {
        bail!("slice dependency cycle involving {id:?}");
    }
    let slice = by_id
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("slice {id:?} not found"))?;
    for dep in &slice.depends_on {
        if wanted.contains(dep) {
            visit_ordered(dep, by_id, wanted, visiting, visited, ordered)?;
        }
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    ordered.push(slice);
    Ok(())
}

fn cycle_issues(slices: &[Slice]) -> Vec<SliceValidationIssue> {
    let by_id: BTreeMap<_, _> = slices
        .iter()
        .map(|slice| (slice.id.as_str(), slice))
        .collect();
    let mut issues = Vec::new();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for slice in slices {
        if let Err(err) = visit_cycle(&slice.id, &by_id, &mut visiting, &mut visited) {
            issues.push(issue("", &slice.id, err.to_string()));
        }
    }
    issues
}

fn visit_cycle<'a>(
    id: &str,
    by_id: &BTreeMap<&'a str, &'a Slice>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> Result<()> {
    if visited.contains(id) || !by_id.contains_key(id) {
        return Ok(());
    }
    if !visiting.insert(id.to_string()) {
        bail!("slice dependency cycle involving {id:?}");
    }
    let slice = by_id[id];
    for dep in &slice.depends_on {
        visit_cycle(dep, by_id, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    Ok(())
}

pub fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T> {
    let data = fs::read_to_string(path.as_ref())
        .with_context(|| format!("read {}", path.as_ref().display()))?;
    Ok(serde_json::from_str(&data)?)
}

pub fn write_json<T: Serialize>(path: impl AsRef<Path>, value: &T) -> Result<()> {
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    fs::write(path.as_ref(), data).with_context(|| format!("write {}", path.as_ref().display()))?;
    Ok(())
}

pub fn default_agent_profiles_toml() -> &'static str {
    r#"# Khazad-Doom agent launch profiles.
# Code-writing workers must use the implementer profile. The Pi adapter
# enforces provider/model/reasoning before launching real workers; fake is
# exempt for deterministic smoke tests.

[profiles.implementer]
provider = "openai-codex"
model = "gpt-5.5"
reasoning = "xhigh"
mode = "fast"
required = true

[profiles.planner]
provider = "openai-codex"
model = "gpt-5.5"
reasoning = "high"
mode = "normal"
read_only = true

[profiles.verifier]
provider = "openai-codex"
model = "gpt-5.5"
reasoning = "high"
mode = "fast"
read_only = true
"#
}

pub(crate) fn read_agent_profiles_file(path: &Path) -> Result<AgentProfilesConfig> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_agent_profiles_toml(&text).with_context(|| format!("parse {}", path.display()))
}

fn parse_agent_profiles_toml(text: &str) -> Result<AgentProfilesConfig> {
    let mut config = AgentProfilesConfig {
        profiles: BTreeMap::new(),
    };
    let mut current_profile: Option<String> = None;
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_toml_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !(line.starts_with("[profiles.") && line.ends_with(']')) {
                bail!("line {line_number}: expected [profiles.<name>] section");
            }
            let name = &line[10..line.len() - 1];
            if name.trim().is_empty() || name.contains(char::is_whitespace) {
                bail!("line {line_number}: invalid profile name {name:?}");
            }
            current_profile = Some(name.to_string());
            config.profiles.entry(name.to_string()).or_default();
            continue;
        }
        let profile_name = current_profile
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("line {line_number}: key outside profile section"))?
            .clone();
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("line {line_number}: expected key = value"))?;
        let key = key.trim();
        let value = value.trim();
        let profile = config
            .profiles
            .get_mut(&profile_name)
            .expect("profile section created before keys");
        match key {
            "provider" => profile.provider = parse_toml_string(value, line_number)?,
            "model" => profile.model = parse_toml_string(value, line_number)?,
            "reasoning" => profile.reasoning = parse_toml_string(value, line_number)?,
            "mode" => profile.mode = parse_toml_string(value, line_number)?,
            "args" => profile.args = parse_toml_string_array(value, line_number)?,
            "required" => profile.required = parse_toml_bool(value, line_number)?,
            "read_only" => profile.read_only = parse_toml_bool(value, line_number)?,
            other => bail!("line {line_number}: unknown agent profile key {other:?}"),
        }
    }
    Ok(config)
}

fn strip_toml_comment(line: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;
    let mut out = String::new();
    for ch in line.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if in_string && ch == '\\' {
            out.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if !in_string && ch == '#' {
            break;
        }
        out.push(ch);
    }
    out
}

fn parse_toml_string(value: &str, line_number: usize) -> Result<String> {
    if !(value.starts_with('"') && value.ends_with('"')) || value.len() < 2 {
        bail!("line {line_number}: expected quoted string");
    }
    unescape_toml_string(&value[1..value.len() - 1], line_number)
}

fn parse_toml_string_array(value: &str, line_number: usize) -> Result<Vec<String>> {
    let trimmed = value.trim();
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        bail!("line {line_number}: expected string array");
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if in_string && ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            current.push(ch);
            in_string = !in_string;
            continue;
        }
        if !in_string && ch == ',' {
            items.push(parse_toml_string(current.trim(), line_number)?);
            current.clear();
            continue;
        }
        current.push(ch);
    }
    if in_string {
        bail!("line {line_number}: unterminated string in array");
    }
    if !current.trim().is_empty() {
        items.push(parse_toml_string(current.trim(), line_number)?);
    }
    Ok(items)
}

fn parse_toml_bool(value: &str, line_number: usize) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("line {line_number}: expected boolean"),
    }
}

fn unescape_toml_string(value: &str, line_number: usize) -> Result<String> {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            bail!("line {line_number}: dangling escape in string");
        };
        match escaped {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            other => bail!("line {line_number}: unsupported escape \\{other}"),
        }
    }
    Ok(out)
}

fn collect_dir_entries(entries: &mut Vec<ArtifactEntry>, kind: &str, dir: &Path) -> Result<()> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    };
    for entry in read_dir {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        push_artifact_entry(entries, kind, entry.path())?;
    }
    Ok(())
}

fn collect_report_entries(
    entries: &mut Vec<ArtifactEntry>,
    run_id: &str,
    dir: &Path,
) -> Result<()> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    };
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.contains(run_id) {
            push_artifact_entry(entries, "report", path)?;
        }
    }
    Ok(())
}

fn push_artifact_entry(entries: &mut Vec<ArtifactEntry>, kind: &str, path: PathBuf) -> Result<()> {
    let metadata = fs::metadata(&path)?;
    entries.push(ArtifactEntry {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string(),
        kind: kind.to_string(),
        path: path.to_string_lossy().to_string(),
        size_bytes: metadata.len(),
        exists: true,
    });
    Ok(())
}

fn ensure_gitignore(repo_path: &Path) -> Result<()> {
    let path = repo_path.join(".gitignore");
    let wanted = [".workflow/runs/", ".workflow/worktrees/"];
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let missing: Vec<_> = wanted
        .iter()
        .filter(|line| !contains_line(&current, line))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut updated = current.clone();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("\n# Khazad-Doom runtime artifacts\n");
    for line in missing {
        updated.push_str(line);
        updated.push('\n');
    }
    fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn contains_line(text: &str, wanted: &str) -> bool {
    text.lines().any(|line| line.trim() == wanted)
}

fn is_safe_slice_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    if id.ends_with('.') || id.contains("..") || id.ends_with(".lock") {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn issue(file: &str, slice_id: &str, message: String) -> SliceValidationIssue {
    SliceValidationIssue {
        severity: "error".to_string(),
        file: file.to_string(),
        slice_id: slice_id.to_string(),
        message,
    }
}

fn issue_for_path(path: &Path, slice_id: &str, message: String) -> SliceValidationIssue {
    issue(&path.to_string_lossy(), slice_id, message)
}

#[cfg(test)]
mod tests {
    use super::{
        Slice, Store, dependency_layers, parse_agent_profiles_toml, topological_order,
        validate_slice, validate_slice_set,
    };

    fn valid_slice(id: &str) -> Slice {
        Slice {
            id: id.to_string(),
            title: "Title".to_string(),
            goal: "Goal".to_string(),
            github_issue: String::new(),
            status: crate::domain::SLICE_STATUS_OPEN.to_string(),
            closed_by_run: String::new(),
            closed_at: String::new(),
            depends_on: Vec::new(),
            areas: Vec::new(),
            acceptance: vec!["done".to_string()],
            must_ask_if: Vec::new(),
            verify_profile: String::new(),
            verify: Vec::new(),
            verify_timeout_seconds: 0,
        }
    }

    #[test]
    fn rejects_path_or_ref_unsafe_slice_ids() {
        for id in ["../x", "a/b", ".hidden", "a..b", "a.lock", "a b", "a:"] {
            assert!(
                validate_slice(&valid_slice(id)).is_err(),
                "id {id:?} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_safe_slice_id() {
        validate_slice(&valid_slice("slice-001.alpha_beta")).unwrap();
    }

    #[test]
    fn ensure_default_config_uses_parallelism_three() {
        let repo = tempfile::tempdir().unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();

        let config = store.read_config().unwrap();
        assert_eq!(config.parallelism, 3);
        assert!(!store.workflow_dir().join("agents.toml").exists());
    }

    #[test]
    fn parses_agent_profile_args() {
        let profiles = parse_agent_profiles_toml(
            r#"
            [profiles.implementer]
            provider = "openai"
            model = "gpt-5.5"
            reasoning = "xhigh"
            mode = "fast"
            args = ["--flag", "value"]
            required = true
            "#,
        )
        .unwrap();
        let implementer = profiles.profiles.get("implementer").unwrap();
        assert_eq!(implementer.args, ["--flag", "value"]);
        assert!(implementer.required);
    }

    #[test]
    fn reports_missing_dependencies() {
        let mut slice = valid_slice("slice-002");
        slice.depends_on = vec!["slice-001".to_string()];
        let issues = validate_slice_set(&[slice]);
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("missing dependency"))
        );
    }

    #[test]
    fn topological_order_includes_requested_dependencies_first() {
        let first = valid_slice("slice-001");
        let mut second = valid_slice("slice-002");
        second.depends_on = vec!["slice-001".to_string()];
        let ordered = topological_order(&[second, first], &["slice-002".to_string()]).unwrap();
        let ids: Vec<_> = ordered.iter().map(|slice| slice.id.as_str()).collect();
        assert_eq!(ids, ["slice-001", "slice-002"]);
    }

    #[test]
    fn dependency_layers_group_independent_slices() {
        let first = valid_slice("slice-001");
        let second = valid_slice("slice-002");
        let mut third = valid_slice("slice-003");
        third.depends_on = vec!["slice-001".to_string(), "slice-002".to_string()];
        let layers = dependency_layers(&[third, second, first]).unwrap();
        let ids: Vec<Vec<_>> = layers
            .iter()
            .map(|layer| layer.iter().map(|slice| slice.id.as_str()).collect())
            .collect();
        assert_eq!(ids, vec![vec!["slice-001", "slice-002"], vec!["slice-003"]]);
    }
}
