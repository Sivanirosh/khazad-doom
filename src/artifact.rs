use crate::domain::{
    AgentProfilesConfig, ArtifactEntry, GateResult, GitWorktreeSnapshotEvidence, Handoff,
    ImplementationSummary, OriginNotificationTarget, RunCheckpoint, Slice, SliceSummary,
    SliceValidationIssue, SliceValidationReport, SliceWriteResult, TerminalNotificationRecord,
    WorkflowConfig,
};
use crate::gitutil;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const DIR_NAME: &str = ".workflow";
pub const AREA_CONTRACT_FILE: &str = "AREA_CONTRACT.md";

const AREA_CONTRACT: &str = r#"# Khazad-Doom Area Contract

`areas` in `.workflow/slices/*.json` are repo-relative literal path prefixes, not globs.

Use directory prefixes with a trailing slash and exact file paths:

```text
src/normia/       ✅ directory prefix
tests/            ✅ directory prefix
roadmap/          ✅ directory prefix
legacy/           ✅ directory prefix
README.md         ✅ file path
pyproject.toml    ✅ file path

src/normia/**     ❌ glob
tests/*           ❌ glob
./src/normia/     ❌ leading ./
../foo            ❌ parent traversal
```

A valid area must be non-empty and must not:

- contain leading or trailing whitespace
- contain `*`, `?`, `[`, or `]`
- contain `..`
- start with `/`
- start with `./`

Khazad-Doom core is authoritative: `khazad-doom slices validate` rejects invalid areas before a worker can hit the path guard. Slice generators, PRDs, issues, and skills must generate areas that follow this contract.
"#;

const SLICE_AREA_SCHEMA_PATTERN: &str =
    r"^(?!\s)(?!/)(?!\.\/)(?!.*\.\.)(?!.*[\*\?\[\]])(?!.*\s$).+$";

#[derive(Debug, Clone)]
pub struct Store {
    repo_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct PiWrapperArtifacts {
    pub prompt_path: PathBuf,
    pub env_path: PathBuf,
    pub wrapper_path: PathBuf,
    pub command_path: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub exit_path: PathBuf,
    pub status_path: PathBuf,
    pub result_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct PiTuiWorkerArtifacts {
    pub prompt_path: PathBuf,
    pub command_path: PathBuf,
    pub result_path: PathBuf,
    pub extension_dir: PathBuf,
    pub extension_index_path: PathBuf,
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
    pub closed_slice_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CompletionPublicationManifest {
    run_id: String,
    closed_slice_ids: Vec<String>,
    gate_approval: GitWorktreeSnapshotEvidence,
    gate_publication_identity: Vec<u8>,
    exact: gitutil::ExactPathManifest,
}

#[derive(Debug, Clone)]
enum CompletionPublicationArtifact {
    ClosedSlice(String),
    ImplementationSummary,
    FinalReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionPublicationPathReceipt {
    pub path_bytes_hex: String,
    pub mode: String,
    pub object_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionPublicationReceipt {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub closed_slice_ids: Vec<String>,
    pub committed: bool,
    pub commit_sha: String,
    pub parent_sha: String,
    pub tree_sha: String,
    pub staged_path_bytes_hex: Vec<String>,
    pub manifest_entries: Vec<CompletionPublicationPathReceipt>,
}

fn exact_completion_publication_receipt(
    receipt: &CompletionPublicationReceipt,
) -> Result<gitutil::ExactPathCommitReceipt> {
    Ok(gitutil::ExactPathCommitReceipt {
        committed: receipt.committed,
        commit_sha: receipt.commit_sha.clone(),
        parent_sha: receipt.parent_sha.clone(),
        tree_sha: receipt.tree_sha.clone(),
        staged_path_bytes: receipt
            .staged_path_bytes_hex
            .iter()
            .map(|path| hex::decode(path).context("decode publication receipt path"))
            .collect::<Result<Vec<_>>>()?,
        manifest_entries: receipt
            .manifest_entries
            .iter()
            .map(|entry| {
                Ok(gitutil::ExactPathCommitEntry {
                    path_bytes: hex::decode(&entry.path_bytes_hex)
                        .context("decode publication manifest receipt path")?,
                    mode: entry.mode.clone(),
                    object_id: entry.object_id.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn completion_publication_receipt(
    receipt: gitutil::ExactPathCommitReceipt,
    manifest: &CompletionPublicationManifest,
) -> CompletionPublicationReceipt {
    CompletionPublicationReceipt {
        run_id: manifest.run_id.clone(),
        closed_slice_ids: manifest.closed_slice_ids.clone(),
        committed: receipt.committed,
        commit_sha: receipt.commit_sha,
        parent_sha: receipt.parent_sha,
        tree_sha: receipt.tree_sha,
        staged_path_bytes_hex: receipt
            .staged_path_bytes
            .into_iter()
            .map(hex::encode)
            .collect(),
        manifest_entries: receipt
            .manifest_entries
            .into_iter()
            .map(|entry| CompletionPublicationPathReceipt {
                path_bytes_hex: hex::encode(entry.path_bytes),
                mode: entry.mode,
                object_id: entry.object_id,
            })
            .collect(),
    }
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

    pub(crate) fn repo_path(&self) -> &Path {
        &self.repo_path
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
        self.write_mission_envelope_schema()?;
        self.ensure_area_contract()?;
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

    pub fn mission_envelope_schema_path(&self) -> PathBuf {
        self.schema_dir().join("mission-envelope.schema.json")
    }

    pub fn area_contract_path(&self) -> PathBuf {
        self.workflow_dir().join(AREA_CONTRACT_FILE)
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

    pub(crate) fn pi_wrapper_artifacts_for_output_path(
        &self,
        output_path: &Path,
    ) -> Result<PiWrapperArtifacts> {
        let parent = output_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("worker output path has no parent"))?;
        let file_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("worker output path is not UTF-8"))?;
        let prefix = file_name.strip_suffix(".json").unwrap_or(file_name);
        let path = |suffix: &str| parent.join(format!("{prefix}.herdr.{suffix}"));
        Ok(PiWrapperArtifacts {
            prompt_path: path("prompt.txt"),
            env_path: path("env.sh"),
            wrapper_path: path("wrapper.sh"),
            command_path: path("command.json"),
            stdout_path: path("stdout.ndjson"),
            stderr_path: path("stderr.log"),
            exit_path: path("exit.json"),
            status_path: path("status.json"),
            result_path: path("result.json"),
        })
    }

    pub(crate) fn pi_tui_worker_artifacts_for_output_path(
        &self,
        output_path: &Path,
    ) -> Result<PiTuiWorkerArtifacts> {
        let parent = output_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("worker output path has no parent"))?;
        let file_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("worker output path is not UTF-8"))?;
        let prefix = file_name.strip_suffix(".json").unwrap_or(file_name);
        let path = |suffix: &str| parent.join(format!("{prefix}.herdr-tui.{suffix}"));
        let extension_dir = path("extension");
        Ok(PiTuiWorkerArtifacts {
            prompt_path: path("prompt.md"),
            command_path: path("command.json"),
            result_path: path("result.json"),
            extension_index_path: extension_dir.join("index.js"),
            extension_dir,
        })
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

    #[allow(dead_code)] // retained for legacy handoff producers/readers.
    pub fn write_handoff(&self, run_id: &str, handoff: &Handoff) -> Result<PathBuf> {
        self.write_handoff_named(run_id, handoff, &handoff.slice.id)
    }

    /// Writes a handoff under a daemon-owned immutable attempt name. The legacy
    /// slice-id-only API remains for reading and producing old run artifacts.
    pub fn write_handoff_named(
        &self,
        run_id: &str,
        handoff: &Handoff,
        name: &str,
    ) -> Result<PathBuf> {
        self.ensure_run_dirs(run_id)?;
        let path = self.handoff_dir(run_id).join(format!("{name}.json"));
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
        let config = if path.exists() {
            read_json(path)?
        } else {
            WorkflowConfig::default()
        };
        config.validate()?;
        Ok(config)
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

    pub fn write_mission_envelope_schema(&self) -> Result<PathBuf> {
        let path = self.mission_envelope_schema_path();
        write_json(&path, &mission_envelope_schema())?;
        Ok(path)
    }

    fn ensure_area_contract(&self) -> Result<()> {
        let path = self.area_contract_path();
        if path.exists() {
            return Ok(());
        }
        fs::write(&path, AREA_CONTRACT).with_context(|| format!("write {}", path.display()))
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
                    severity: "error".to_string(),
                    kind: "slice_close_missing".to_string(),
                    slice_id: slice_id.clone(),
                    path: path.to_string_lossy().to_string(),
                    message: format!(
                        "required slice close record for {slice_id} was not present at {}",
                        path.display()
                    ),
                    policy: "block_completion_publication_on_missing_close_record".to_string(),
                });
                continue;
            }
            match self.close_slice_file(slice_id, run_id, closed_at) {
                Ok(()) => report.closed_slice_ids.push(slice_id.clone()),
                Err(err) => report.incidents.push(SliceClosureIncident {
                    severity: "error".to_string(),
                    kind: "slice_close_failed".to_string(),
                    slice_id: slice_id.clone(),
                    path: path.to_string_lossy().to_string(),
                    message: format!(
                        "failed to close slice {slice_id} at {}: {err:#}",
                        path.display()
                    ),
                    policy: "block_handoff_on_close_failure".to_string(),
                }),
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

    #[cfg(test)]
    pub fn completion_publication_manifest(
        &self,
        run_id: &str,
        closed_slice_ids: &[String],
    ) -> Result<CompletionPublicationManifest> {
        let gate_approval = gitutil::completion_publication_approval(&self.repo_path)?;
        let gate_publication_identity =
            gitutil::completion_publication_root_identity(&self.repo_path)?;
        self.pin_completion_publication_manifest(
            run_id,
            closed_slice_ids,
            gate_approval,
            gate_publication_identity,
        )
    }

    pub fn completion_publication_manifest_for_gate(
        &self,
        run_id: &str,
        closed_slice_ids: &[String],
        gate: &GateResult,
    ) -> Result<CompletionPublicationManifest> {
        if gate.status != "passed" {
            bail!("completion publication requires a passed integration gate");
        }
        let gate_approval = gate
            .approved_workspace
            .clone()
            .context("passed integration gate omitted its approved workspace identity")?;
        if gate.publication_identity.is_empty() {
            bail!("passed integration gate omitted its publication identity");
        }
        self.pin_completion_publication_manifest(
            run_id,
            closed_slice_ids,
            gate_approval,
            gate.publication_identity.clone(),
        )
    }

    fn completion_publication_manifest_for_recovery(
        &self,
        run_id: &str,
        closed_slice_ids: &[String],
    ) -> Result<CompletionPublicationManifest> {
        // Recovery validates immutable commit and journal identities; it must not try to
        // acquire a second real-index lock merely to reconstruct semantic path bytes.
        let root_identity = gitutil::completion_publication_root_identity(&self.repo_path)?;
        self.pin_completion_publication_manifest(
            run_id,
            closed_slice_ids,
            GitWorktreeSnapshotEvidence::default(),
            root_identity,
        )
    }

    fn completion_publication_artifacts(
        &self,
        run_id: &str,
        closed_slice_ids: &[String],
    ) -> Result<Vec<(PathBuf, CompletionPublicationArtifact)>> {
        if !is_safe_slice_id(run_id) {
            bail!("unsafe completion publication run id {run_id:?}");
        }
        let mut canonical_slice_ids = closed_slice_ids.to_vec();
        for slice_id in &canonical_slice_ids {
            if !is_safe_slice_id(slice_id) {
                bail!("unsafe completion publication slice id {slice_id:?}");
            }
        }
        canonical_slice_ids.sort();
        if canonical_slice_ids.windows(2).any(|ids| ids[0] == ids[1]) {
            bail!("completion publication repeats a closed slice id");
        }
        let mut artifacts = Vec::new();
        for slice_id in canonical_slice_ids {
            artifacts.push((
                self.repo_relative_publication_path(&self.slice_path(&slice_id))?,
                CompletionPublicationArtifact::ClosedSlice(slice_id),
            ));
        }
        artifacts.push((
            self.repo_relative_publication_path(&self.implementation_summary_report_path(run_id))?,
            CompletionPublicationArtifact::ImplementationSummary,
        ));
        artifacts.push((
            self.repo_relative_publication_path(&self.final_report_artifact_path(run_id))?,
            CompletionPublicationArtifact::FinalReport,
        ));
        artifacts.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(artifacts)
    }

    fn pin_completion_publication_manifest(
        &self,
        run_id: &str,
        closed_slice_ids: &[String],
        gate_approval: GitWorktreeSnapshotEvidence,
        gate_publication_identity: Vec<u8>,
    ) -> Result<CompletionPublicationManifest> {
        let artifacts = self.completion_publication_artifacts(run_id, closed_slice_ids)?;
        let root_identity = gitutil::completion_publication_root_identity(&self.repo_path)?;
        let mut entries = Vec::with_capacity(artifacts.len());
        for (path, _) in &artifacts {
            let absolute = self.repo_path.join(path);
            let expected_bytes = fs::read(&absolute).with_context(|| {
                format!(
                    "completion publication artifact is missing or unreadable: {}",
                    absolute.display()
                )
            })?;
            entries.push(gitutil::ExactPathManifestEntry {
                path: path.clone(),
                expected_bytes,
                expected_mode: "100644".to_string(),
            });
        }
        let mut canonical_slice_ids = closed_slice_ids.to_vec();
        canonical_slice_ids.sort();
        if gitutil::completion_publication_root_identity(&self.repo_path)? != root_identity {
            bail!(
                "completion publication root/repository identity changed while pinning semantics"
            );
        }
        if root_identity != gate_publication_identity {
            bail!("completion publication root/repository identity diverged from the passed gate");
        }
        let manifest = CompletionPublicationManifest {
            run_id: run_id.to_string(),
            closed_slice_ids: canonical_slice_ids,
            gate_approval,
            gate_publication_identity,
            exact: gitutil::ExactPathManifest {
                root_identity,
                entries,
            },
        };
        self.validate_completion_publication_manifest_semantics(&manifest)?;
        Ok(manifest)
    }

    fn validate_completion_publication_manifest_semantics(
        &self,
        manifest: &CompletionPublicationManifest,
    ) -> Result<()> {
        let artifacts =
            self.completion_publication_artifacts(&manifest.run_id, &manifest.closed_slice_ids)?;
        let mut entries = BTreeMap::new();
        for entry in &manifest.exact.entries {
            if entry.expected_mode != "100644" {
                bail!(
                    "completion publication semantic artifact {} must have mode 100644",
                    entry.path.display()
                );
            }
            if entries
                .insert(entry.path.clone(), entry.expected_bytes.as_slice())
                .is_some()
            {
                bail!("completion publication semantic manifest repeats a path");
            }
        }
        let expected_paths = artifacts
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        if entries.keys().cloned().collect::<BTreeSet<_>>() != expected_paths {
            bail!("completion publication semantic manifest path set is not exact");
        }

        let mut implementation = None;
        let mut final_report = None;
        for (path, artifact) in artifacts {
            let bytes = entries
                .get(&path)
                .copied()
                .context("completion publication semantic manifest omitted an artifact")?;
            match artifact {
                CompletionPublicationArtifact::ClosedSlice(expected_slice_id) => {
                    let slice: Slice = serde_json::from_slice(bytes).with_context(|| {
                        format!(
                            "completion publication slice {} is not valid JSON",
                            path.display()
                        )
                    })?;
                    validate_slice(&slice).with_context(|| {
                        format!(
                            "completion publication slice {} failed validation",
                            path.display()
                        )
                    })?;
                    if slice.id != expected_slice_id
                        || slice.status != crate::domain::SLICE_STATUS_CLOSED
                        || slice.closed_by_run != manifest.run_id
                        || slice.closed_at.trim().is_empty()
                    {
                        bail!(
                            "completion publication slice {} is not closed by run {} as slice {}",
                            path.display(),
                            manifest.run_id,
                            expected_slice_id
                        );
                    }
                }
                CompletionPublicationArtifact::ImplementationSummary => {
                    implementation = Some(bytes);
                }
                CompletionPublicationArtifact::FinalReport => {
                    final_report = Some(bytes);
                }
            }
        }
        let implementation = implementation.context("implementation summary was not pinned")?;
        let final_report = final_report.context("final report was not pinned")?;
        if implementation != final_report {
            bail!("completion publication reports do not contain the same pinned summary bytes");
        }
        let summary: ImplementationSummary = serde_json::from_slice(implementation)
            .context("completion publication report is not a valid implementation summary")?;
        if summary.run_id != manifest.run_id
            || summary.integration_gate.status != "passed"
            || !summary.final_sha.is_empty()
        {
            bail!(
                "completion publication report does not describe a passed, pre-publication summary for run {}",
                manifest.run_id
            );
        }
        let completed_ids = summary
            .completed_slices
            .iter()
            .map(|result| result.slice_id.clone())
            .collect::<Vec<_>>();
        let completed_set = completed_ids.iter().cloned().collect::<BTreeSet<_>>();
        let expected_set = manifest
            .closed_slice_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if completed_ids.len() != completed_set.len() || !expected_set.is_subset(&completed_set) {
            bail!(
                "completion publication report does not include every closed-slice manifest identity"
            );
        }
        Ok(())
    }

    pub fn find_completion_publication(
        &self,
        run_id: &str,
        expected_branch: &str,
        closed_slice_ids: &[String],
    ) -> Result<Option<CompletionPublicationReceipt>> {
        let artifacts = self.completion_publication_artifacts(run_id, closed_slice_ids)?;
        if artifacts
            .iter()
            .any(|(path, _)| !self.repo_path.join(path).is_file())
        {
            let head_message =
                gitutil::run(&self.repo_path, &["show", "-s", "--format=%B", "HEAD"])?;
            if head_message.trim_end() == format!("khazad(run): publish completion {run_id}") {
                bail!(
                    "completion publication commit at HEAD was found but its current manifest could not be captured"
                );
            }
            return Ok(None);
        }
        let manifest =
            self.completion_publication_manifest_for_recovery(run_id, closed_slice_ids)?;
        gitutil::find_exact_path_commit(
            &self.repo_path,
            &manifest.exact,
            &format!("khazad(run): publish completion {run_id}"),
            &format!("refs/heads/{expected_branch}"),
        )
        .map(|receipt| receipt.map(|receipt| completion_publication_receipt(receipt, &manifest)))
    }

    pub fn commit_completion_publication(
        &self,
        run_id: &str,
        expected_branch: &str,
        manifest: &CompletionPublicationManifest,
    ) -> Result<CompletionPublicationReceipt> {
        if run_id != manifest.run_id {
            bail!(
                "completion publication run {} does not match pinned manifest run {}",
                run_id,
                manifest.run_id
            );
        }
        self.validate_completion_publication_manifest_semantics(manifest)?;
        let receipt = gitutil::commit_exact_paths_with_approval(
            &self.repo_path,
            &manifest.exact,
            &manifest.gate_approval,
            &manifest.gate_publication_identity,
            &format!("khazad(run): publish completion {run_id}"),
            &format!("refs/heads/{expected_branch}"),
        )?;
        Ok(completion_publication_receipt(receipt, manifest))
    }

    pub fn validate_completion_publication(
        &self,
        expected_branch: &str,
        manifest: &CompletionPublicationManifest,
        receipt: &CompletionPublicationReceipt,
    ) -> Result<()> {
        self.validate_completion_publication_manifest_semantics(manifest)?;
        if receipt.run_id != manifest.run_id
            || receipt.closed_slice_ids != manifest.closed_slice_ids
        {
            bail!("completion publication receipt semantics do not match its pinned manifest");
        }
        gitutil::validate_exact_path_receipt(
            &self.repo_path,
            &manifest.exact,
            &format!("refs/heads/{expected_branch}"),
            &exact_completion_publication_receipt(receipt)?,
        )
    }

    pub fn validate_completion_publication_receipt_at_ref(
        &self,
        expected_branch: &str,
        receipt: &CompletionPublicationReceipt,
    ) -> Result<()> {
        let exact = exact_completion_publication_receipt(receipt)?;
        gitutil::validate_exact_path_receipt_at_ref(
            &self.repo_path,
            &format!("refs/heads/{expected_branch}"),
            &exact,
        )?;
        let mut canonical_slice_ids = receipt.closed_slice_ids.clone();
        canonical_slice_ids.sort();
        if receipt.run_id.is_empty() || canonical_slice_ids != receipt.closed_slice_ids {
            bail!("completion publication receipt omitted canonical run/slice semantics");
        }
        let artifacts =
            self.completion_publication_artifacts(&receipt.run_id, &receipt.closed_slice_ids)?;
        if exact
            .manifest_entries
            .iter()
            .any(|entry| entry.mode != "100644")
        {
            bail!("completion publication receipt contains a non-regular JSON artifact mode");
        }
        let blobs = gitutil::exact_path_receipt_blobs(&self.repo_path, &exact)?;
        let mut entries = Vec::with_capacity(artifacts.len());
        for (path, _) in artifacts {
            let raw_path = path
                .to_str()
                .context("completion publication semantic path was not UTF-8")?
                .as_bytes()
                .to_vec();
            let expected_bytes = blobs.get(&raw_path).cloned().with_context(|| {
                format!(
                    "completion publication receipt omitted semantic path {}",
                    path.display()
                )
            })?;
            entries.push(gitutil::ExactPathManifestEntry {
                path,
                expected_bytes,
                expected_mode: "100644".to_string(),
            });
        }
        if entries.len() != blobs.len() {
            bail!("completion publication receipt contains paths outside its run/slice semantics");
        }
        self.validate_completion_publication_manifest_semantics(&CompletionPublicationManifest {
            run_id: receipt.run_id.clone(),
            closed_slice_ids: receipt.closed_slice_ids.clone(),
            gate_approval: GitWorktreeSnapshotEvidence::default(),
            gate_publication_identity: Vec::new(),
            exact: gitutil::ExactPathManifest {
                root_identity: gitutil::completion_publication_root_identity(&self.repo_path)?,
                entries,
            },
        })
    }

    fn repo_relative_publication_path(&self, path: &Path) -> Result<PathBuf> {
        path.strip_prefix(&self.repo_path)
            .map(Path::to_path_buf)
            .with_context(|| {
                format!(
                    "completion publication path {} is outside repository {}",
                    path.display(),
                    self.repo_path.display()
                )
            })
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
            "provenance": {
                "type": "object",
                "additionalProperties": false,
                "required": ["parent_slice_id", "origin_proposal_id", "generation", "created_by", "created_at"],
                "properties": {
                    "parent_slice_id": { "type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]*$" },
                    "origin_proposal_id": { "type": "string", "minLength": 1 },
                    "generation": { "type": "integer", "minimum": 0 },
                    "created_by": { "type": "string", "enum": ["operator", "worker+daemon"] },
                    "created_at": { "type": "string", "format": "date-time" }
                }
            },
            "depends_on": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
            "areas": {
                "type": "array",
                "items": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": SLICE_AREA_SCHEMA_PATTERN,
                    "description": "Repo-relative literal path prefix; globs, parent traversal, absolute paths, leading/trailing whitespace, and leading ./ are rejected."
                },
                "uniqueItems": true
            },
            "acceptance": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
            "must_ask_if": { "type": "array", "items": { "type": "string" } },
            "verify_profile": {
                "type": "string",
                "description": "Integration-gate profile name. Profile commands run after merges, not as worker-local slice checks."
            },
            "verify": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Worker-local verification commands; keep them inside the slice's fix authority."
            },
            "verify_timeout_seconds": { "type": "integer", "minimum": 0, "maximum": 86400 }
        }
    })
}

pub fn mission_envelope_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://khazad-doom.local/mission-envelope.schema.json",
        "title": "Khazad-Doom Mission Envelope",
        "type": "object",
        "additionalProperties": true,
        "required": ["goal", "allowed_areas"],
        "properties": {
            "goal": { "type": "string", "minLength": 1 },
            "allowed_areas": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": SLICE_AREA_SCHEMA_PATTERN,
                    "description": "Repo-relative literal path prefix using the same area contract as slice areas."
                },
                "uniqueItems": true
            },
            "non_goals": { "type": "array", "items": { "type": "string", "minLength": 1 } },
            "verify_profile": { "type": "string", "minLength": 1, "default": "default" },
            "max_auto_promotions": { "type": "integer", "minimum": 0 },
            "max_depth": { "type": "integer", "minimum": 0 },
            "max_generated_slices": { "type": "integer", "minimum": 0 },
            "autonomy_level": { "type": "string", "enum": ["off", "shadow", "promote", "run"], "default": "off" },
            "must_ask_if": { "type": "array", "items": { "type": "string", "minLength": 1 } }
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
    if let Some(provenance) = slice.provenance() {
        validate_slice_provenance(&provenance)?;
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
    for area in &slice.areas {
        validate_slice_area(area)?;
    }
    Ok(())
}

fn validate_slice_provenance(provenance: &crate::domain::SliceProvenance) -> Result<()> {
    if provenance.parent_slice_id.trim().is_empty() {
        bail!("provenance.parent_slice_id is required");
    }
    if !is_safe_slice_id(&provenance.parent_slice_id) {
        bail!("provenance.parent_slice_id is not path/ref safe");
    }
    if provenance.origin_proposal_id.trim().is_empty() {
        bail!("provenance.origin_proposal_id is required");
    }
    if provenance.origin_proposal_id.contains('/') || provenance.origin_proposal_id.contains("..") {
        bail!("provenance.origin_proposal_id must be a safe proposal id");
    }
    if !matches!(provenance.created_by.as_str(), "operator" | "worker+daemon") {
        bail!("provenance.created_by must be either 'operator' or 'worker+daemon'");
    }
    if provenance.created_at.trim().is_empty() {
        bail!("provenance.created_at is required");
    }
    chrono::DateTime::parse_from_rfc3339(&provenance.created_at)
        .map(|_| ())
        .context("provenance.created_at must be RFC3339 date-time")
}

pub fn validate_slice_area(area: &str) -> Result<()> {
    if area.trim().is_empty() {
        bail!("area is invalid: areas must be non-empty repo-relative literal path prefixes");
    }
    if area.trim() != area {
        bail!("area {area:?} is invalid: remove leading/trailing whitespace");
    }
    if area.starts_with('/') {
        bail!("area {area:?} is invalid: areas must be repo-relative, not absolute paths");
    }
    if area.starts_with("./") {
        bail!(
            "area {area:?} is invalid: omit leading ./ and use a repo-relative literal path prefix"
        );
    }
    if area.contains("..") {
        bail!("area {area:?} is invalid: parent traversal '..' is not allowed");
    }
    if let Some(ch) = area.chars().find(|ch| matches!(ch, '*' | '?' | '[' | ']')) {
        bail!(
            "area {area:?} is invalid: glob character {ch:?} is not allowed; use a repo-relative literal path prefix such as 'src/normia/' or 'README.md'"
        );
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonWriteFaultStage {
    BeforeTempWrite,
    BeforeRename,
    AfterRenameBeforeParentSync,
}

#[cfg(test)]
thread_local! {
    static JSON_WRITE_FAULT: std::cell::RefCell<Option<JsonWriteFaultStage>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn inject_json_write_fault(stage: JsonWriteFaultStage) {
    JSON_WRITE_FAULT.with(|fault| *fault.borrow_mut() = Some(stage));
}

#[cfg(test)]
fn take_json_write_fault(stage: JsonWriteFaultStage) -> bool {
    JSON_WRITE_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if *fault == Some(stage) {
            *fault = None;
            true
        } else {
            false
        }
    })
}

static JSON_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn create_json_temp(parent: &Path, file_name: &OsStr) -> Result<(PathBuf, fs::File)> {
    for _ in 0..128 {
        let sequence = JSON_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsStr::new(".").to_os_string();
        temporary_name.push(file_name);
        temporary_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("create JSON temporary file {}", temporary_path.display())
                });
            }
        }
    }
    bail!(
        "could not allocate a unique same-directory temporary JSON file under {}",
        parent.display()
    )
}

#[cfg(unix)]
fn ensure_json_write_durability_supported() -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn ensure_json_write_durability_supported() -> Result<()> {
    bail!(
        "crash-durable authoritative JSON replacement requires Unix directory synchronization; this runtime is unsupported"
    )
}

#[cfg(unix)]
fn sync_json_parent(parent: &Path) -> Result<()> {
    fs::File::open(parent)
        .with_context(|| format!("open JSON parent directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync JSON parent directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_json_parent(_parent: &Path) -> Result<()> {
    bail!(
        "crash-durable authoritative JSON replacement requires Unix directory synchronization; this runtime is unsupported"
    )
}

pub(crate) const ATOMIC_JSON_WRITER_ARG: &str = "__khazad_atomic_json_write_v1";

pub(crate) fn write_json_from_stdin(path: impl AsRef<Path>) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("read JSON replacement input")?;
    let value: Value = serde_json::from_slice(&bytes).context("parse JSON replacement input")?;
    write_json(path, &value)
}

pub fn write_json<T: Serialize>(path: impl AsRef<Path>, value: &T) -> Result<()> {
    ensure_json_write_durability_supported()?;
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path.file_name().ok_or_else(|| {
        anyhow::anyhow!("JSON artifact path has no file name: {}", path.display())
    })?;
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');

    let (temporary_path, temporary_file) = create_json_temp(parent, file_name)?;
    let mut temporary = Some(temporary_file);
    let mut installed = false;
    let result = (|| -> Result<()> {
        #[cfg(test)]
        if take_json_write_fault(JsonWriteFaultStage::BeforeTempWrite) {
            bail!("injected JSON write fault before temporary-file write");
        }
        {
            let temporary = temporary
                .as_mut()
                .expect("temporary JSON file remains open before replacement");
            temporary.write_all(&data).with_context(|| {
                format!("write JSON temporary file {}", temporary_path.display())
            })?;
            temporary.flush().with_context(|| {
                format!("flush JSON temporary file {}", temporary_path.display())
            })?;
            match fs::metadata(path) {
                Ok(metadata) => temporary
                    .set_permissions(metadata.permissions())
                    .with_context(|| {
                        format!("preserve JSON artifact permissions for {}", path.display())
                    })?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("inspect JSON artifact permissions for {}", path.display())
                    });
                }
            }
            temporary.sync_all().with_context(|| {
                format!("sync JSON temporary file {}", temporary_path.display())
            })?;
        }
        drop(temporary.take());
        #[cfg(test)]
        if take_json_write_fault(JsonWriteFaultStage::BeforeRename) {
            bail!("injected JSON write fault before rename");
        }
        fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "atomically replace JSON artifact {} from {}",
                path.display(),
                temporary_path.display()
            )
        })?;
        installed = true;
        #[cfg(test)]
        if take_json_write_fault(JsonWriteFaultStage::AfterRenameBeforeParentSync) {
            bail!("injected JSON write fault after rename before parent sync");
        }
        sync_json_parent(parent)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            if installed {
                return Err(err.context(format!(
                    "JSON artifact {} was atomically replaced with complete new bytes, but its crash durability is uncertain because post-rename parent synchronization did not complete",
                    path.display()
                )));
            }
            drop(temporary.take());
            if let Err(cleanup_err) = fs::remove_file(&temporary_path) {
                return Err(err.context(format!(
                    "remove failed JSON temporary file {}: {cleanup_err}",
                    temporary_path.display()
                )));
            }
            Err(err)
        }
    }
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

pub(crate) fn is_safe_slice_id(id: &str) -> bool {
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
    use std::fs;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    fn install_blocking_publication_pause(repo: &std::path::Path) {
        let marker = repo.join("publication-filter.marker");
        let release = repo.join("publication-filter.release");
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&release);
        crate::gitutil::pause_next_publication_after_capture(repo, &marker, &release);
    }

    fn wait_for_publication_pause(repo: &std::path::Path) -> String {
        let marker = repo.join("publication-filter.marker");
        let release = repo.join("publication-filter.release");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(path) = fs::read_to_string(&marker)
                && !path.is_empty()
            {
                return path;
            }
            if Instant::now() >= deadline {
                let _ = fs::write(&release, "release\n");
                panic!("timed out waiting for publication capture pause");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn exact_manifest_entry(
        repo: &std::path::Path,
        path: impl Into<std::path::PathBuf>,
    ) -> crate::gitutil::ExactPathManifestEntry {
        let path = path.into();
        #[cfg(unix)]
        let expected_mode = {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(repo.join(&path)).unwrap().permissions().mode() & 0o111 == 0 {
                "100644"
            } else {
                "100755"
            }
            .to_string()
        };
        #[cfg(not(unix))]
        let expected_mode = "100644".to_string();
        crate::gitutil::ExactPathManifestEntry {
            expected_bytes: fs::read(repo.join(&path)).unwrap(),
            expected_mode,
            path,
        }
    }

    fn exact_manifest(
        repo: &std::path::Path,
        entries: Vec<crate::gitutil::ExactPathManifestEntry>,
    ) -> crate::gitutil::ExactPathManifest {
        crate::gitutil::ExactPathManifest {
            root_identity: crate::gitutil::completion_publication_root_identity(repo).unwrap(),
            entries,
        }
    }

    fn valid_slice(id: &str) -> Slice {
        Slice {
            id: id.to_string(),
            title: "Title".to_string(),
            goal: "Goal".to_string(),
            github_issue: String::new(),
            provenance: None,
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

    fn write_completion_publication_fixture(store: &Store, run_id: &str, slice_ids: &[&str]) {
        store.ensure_layout().unwrap();
        for slice_id in slice_ids {
            let mut slice = valid_slice(slice_id);
            slice.status = crate::domain::SLICE_STATUS_CLOSED.to_string();
            slice.closed_by_run = run_id.to_string();
            slice.closed_at = "2026-07-10T00:00:00Z".to_string();
            super::write_json(store.slice_path(slice_id), &slice).unwrap();
        }
        let gate = crate::domain::GateResult {
            status: "passed".to_string(),
            ..Default::default()
        };
        let summary = crate::domain::ImplementationSummary {
            run_id: run_id.to_string(),
            repo_path: store.repo_path.to_string_lossy().into_owned(),
            integration_branch: "main".to_string(),
            base_sha: "base".to_string(),
            final_sha: String::new(),
            worker_profile: crate::domain::WorkerProfileEvidence::default(),
            mission_envelope: None,
            frontier_budget: None,
            completed_slices: slice_ids
                .iter()
                .map(|slice_id| crate::domain::WorkerResult {
                    slice_id: (*slice_id).to_string(),
                    status: "completed".to_string(),
                    ..crate::domain::WorkerResult::default()
                })
                .collect(),
            checks: Vec::new(),
            integration_repair: crate::domain::RepairResult::default(),
            pre_repair_integration_gate: None,
            integration_gate: gate,
            exit_states: crate::domain::WorkflowExitStates::default(),
            evidence_attestation: crate::domain::EvidenceAttestation::default(),
            economics: crate::domain::RunEconomics::default(),
            plan_revisions: crate::domain::PlanRevisions::default(),
            worker_questions: Vec::new(),
            worker_attempts: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        store.write_implementation_summary(&summary).unwrap();
        store.write_final_report(&summary).unwrap();
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
    fn validates_slice_areas_as_repo_relative_literal_prefixes() {
        let mut slice = valid_slice("slice-001");
        slice.areas = vec![
            "src/normia/".to_string(),
            "tests/".to_string(),
            "README.md".to_string(),
            ".workflow/slices".to_string(),
        ];
        validate_slice(&slice).unwrap();

        for area in [
            "src/normia/**",
            "tests/*",
            "docs/[draft]",
            "docs/?",
            "./src",
            "../foo",
            "/tmp",
            "",
            " docs/",
            "docs/ ",
        ] {
            let mut invalid = valid_slice("slice-001");
            invalid.areas = vec![area.to_string()];
            let err = validate_slice(&invalid).unwrap_err().to_string();
            assert!(
                err.contains("area") || err.contains("glob") || err.contains("parent"),
                "area {area:?} should be rejected with area-specific message, got {err:?}"
            );
        }
    }

    #[test]
    fn validates_optional_slice_provenance_and_schema_property() {
        let mut slice = valid_slice("slice-generated");
        let provenance = crate::domain::SliceProvenance {
            parent_slice_id: "slice-parent".to_string(),
            origin_proposal_id: "rp-test-001".to_string(),
            generation: 1,
            created_by: "worker+daemon".to_string(),
            created_at: "2026-07-09T15:00:00Z".to_string(),
        };
        slice.set_provenance(provenance.clone());
        validate_slice(&slice).unwrap();
        assert_eq!(slice.provenance(), Some(provenance));

        let serialized = serde_json::to_value(&slice).unwrap();
        assert_eq!(serialized["provenance"]["created_by"], "worker+daemon");

        let schema = super::slice_schema();
        assert_eq!(
            schema["properties"]["provenance"]["properties"]["created_by"]["enum"][1],
            "worker+daemon"
        );

        let mut invalid_provenance = crate::domain::SliceProvenance {
            parent_slice_id: "slice-parent".to_string(),
            origin_proposal_id: "rp-test-001".to_string(),
            generation: 1,
            created_by: "envelope".to_string(),
            created_at: "2026-07-09T15:00:00Z".to_string(),
        };
        invalid_provenance.created_by = "envelope".to_string();
        slice.set_provenance(invalid_provenance);
        let err = validate_slice(&slice).unwrap_err().to_string();
        assert!(err.contains("provenance.created_by"));
    }

    #[test]
    fn ensure_default_config_uses_runtime_defaults() {
        let repo = tempfile::tempdir().unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();

        let config = store.read_config().unwrap();
        assert_eq!(config.parallelism, 3);
        assert_eq!(config.worker_question_timeout_seconds, 60);
        assert!(store.area_contract_path().exists());
        assert!(!store.workflow_dir().join("agents.toml").exists());
    }

    #[test]
    fn bounded_runtime_config_has_safe_defaults_and_rejects_values_above_hard_maximums() {
        let repo = tempfile::tempdir().unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();

        let defaults = store.read_config().unwrap().runtime;
        assert_eq!(defaults.retained_output_bytes, 64 * 1024);
        assert_eq!(defaults.observation_flush_bytes, 16 * 1024);
        assert_eq!(defaults.observation_flush_millis, 250);
        assert_eq!(defaults.poll_initial_millis, 25);
        assert_eq!(defaults.poll_max_millis, 500);
        assert_eq!(defaults.economics_checkpoint_millis, 500);
        assert!(defaults.raw_output_spill);

        super::write_json(
            store.config_path(),
            &serde_json::json!({
                "runtime": {
                    "retained_output_bytes": crate::domain::MAX_RETAINED_OUTPUT_BYTES + 1
                }
            }),
        )
        .unwrap();
        let error = store
            .read_config()
            .expect_err("hard maximum must reject accidental unbounded retention");
        assert!(format!("{error:#}").contains("retained_output_bytes"));
    }

    #[test]
    fn bounded_runtime_zero_values_have_deliberate_validation_semantics() {
        let repo = tempfile::tempdir().unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        super::write_json(
            store.config_path(),
            &serde_json::json!({
                "runtime": {
                    "retained_output_bytes": 0,
                    "retained_output_lines": 0,
                    "observation_flush_millis": 0,
                    "raw_output_spill": true
                }
            }),
        )
        .unwrap();
        let runtime = store.read_config().unwrap().runtime;
        assert_eq!(runtime.retained_output_bytes, 0);
        assert_eq!(runtime.observation_flush_millis, 0);

        super::write_json(
            store.config_path(),
            &serde_json::json!({
                "runtime": {
                    "retained_output_bytes": 0,
                    "retained_output_lines": 0,
                    "raw_output_spill": false
                }
            }),
        )
        .unwrap();
        let error = store
            .read_config()
            .expect_err("zero retention without spill would silently drop evidence");
        assert!(format!("{error:#}").contains("raw_output_spill"));
    }

    #[test]
    fn partial_config_keeps_worker_question_timeout_default() {
        let config: crate::domain::WorkflowConfig = serde_json::from_value(serde_json::json!({
            "parallelism": 1
        }))
        .unwrap();

        assert_eq!(config.worker_question_timeout_seconds, 60);
    }

    #[test]
    fn atomic_json_replacement_preserves_previous_artifact_when_rename_is_not_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/run-summary.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = b"{\"version\":\"old\"}\n";
        fs::write(&path, old).unwrap();

        super::inject_json_write_fault(super::JsonWriteFaultStage::BeforeRename);
        let err = super::write_json(&path, &serde_json::json!({ "version": "new" }))
            .expect_err("injected pre-rename failure must abort replacement");

        assert!(format!("{err:#}").contains("injected"), "{err:#}");
        assert_eq!(fs::read(&path).unwrap(), old);
        let restored: serde_json::Value = super::read_json(&path).unwrap();
        assert_eq!(restored["version"], "old");
    }

    #[test]
    fn atomic_json_replacement_preserves_previous_artifact_when_temp_write_is_not_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/run-summary.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = b"{\"version\":\"old\"}\n";
        fs::write(&path, old).unwrap();

        super::inject_json_write_fault(super::JsonWriteFaultStage::BeforeTempWrite);
        let err = super::write_json(&path, &serde_json::json!({ "version": "new" }))
            .expect_err("injected pre-write failure must abort replacement");

        assert!(format!("{err:#}").contains("injected"), "{err:#}");
        assert_eq!(fs::read(&path).unwrap(), old);
        assert_eq!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
                .count(),
            0,
            "failed replacement must clean up its temporary sibling"
        );
    }

    #[test]
    fn atomic_json_replacement_keeps_new_complete_artifact_when_parent_sync_is_not_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/run-summary.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"version\":\"old\"}\n").unwrap();

        super::inject_json_write_fault(super::JsonWriteFaultStage::AfterRenameBeforeParentSync);
        let err = super::write_json(&path, &serde_json::json!({ "version": "new" }))
            .expect_err("injected post-rename failure must report uncertain durability");

        let rendered = format!("{err:#}");
        assert!(rendered.contains("injected"), "{rendered}");
        assert!(
            rendered.contains("was atomically replaced with complete new bytes")
                && rendered.contains("crash durability is uncertain"),
            "post-rename failure must describe the installed-but-not-durably-linked outcome: {rendered}"
        );
        let replaced: serde_json::Value = super::read_json(&path).unwrap();
        assert_eq!(replaced["version"], "new");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_json_replacement_writes_before_preserving_read_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-summary.json");
        fs::write(&path, b"{\"version\":\"old\"}\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o444);
        fs::set_permissions(&path, permissions).unwrap();

        super::write_json(&path, &serde_json::json!({ "version": "new" })).unwrap();

        let replaced: serde_json::Value = super::read_json(&path).unwrap();
        assert_eq!(replaced["version"], "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
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

    #[test]
    fn completion_publication_does_not_stage_unrelated_changes() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        std::fs::write(repo.path().join("unrelated-tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        std::fs::write(repo.path().join("unrelated-tracked.txt"), "operator edit\n").unwrap();
        std::fs::write(
            repo.path().join("unrelated-untracked.txt"),
            "operator scratch\n",
        )
        .unwrap();

        let closed_slice_ids = ["slice-001".to_string()];
        assert!(
            store
                .find_completion_publication("run-1", "main", &closed_slice_ids)
                .unwrap()
                .is_none()
        );
        let manifest = store
            .completion_publication_manifest("run-1", &closed_slice_ids)
            .unwrap();
        let receipt = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap();
        assert!(receipt.committed);
        assert_eq!(
            receipt.commit_sha,
            crate::gitutil::head_sha(repo.path()).unwrap()
        );
        assert_eq!(
            receipt.parent_sha,
            crate::gitutil::run(repo.path(), &["rev-parse", "HEAD^"]).unwrap()
        );
        assert_eq!(
            receipt.tree_sha,
            crate::gitutil::run(repo.path(), &["rev-parse", "HEAD^{tree}"]).unwrap()
        );
        assert_eq!(receipt.staged_path_bytes_hex.len(), 3);
        assert_eq!(receipt.manifest_entries.len(), 3);
        assert!(
            receipt
                .manifest_entries
                .iter()
                .all(|entry| entry.mode == "100644" && !entry.object_id.is_empty())
        );
        assert!(
            store
                .find_completion_publication("run-1", "main", &closed_slice_ids)
                .unwrap()
                .is_some()
        );

        assert_eq!(
            crate::gitutil::run(repo.path(), &["show", "HEAD:unrelated-tracked.txt"]).unwrap(),
            "baseline"
        );
        assert!(
            crate::gitutil::run(
                repo.path(),
                &["ls-tree", "--name-only", "HEAD", "unrelated-untracked.txt"]
            )
            .unwrap()
            .is_empty()
        );
        let status = crate::gitutil::status_porcelain(repo.path()).unwrap();
        assert!(status.contains("unrelated-tracked.txt"));
        assert!(status.contains("unrelated-untracked.txt"));
    }

    #[test]
    fn completion_publication_rejects_head_advance_after_gate_approval() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let gate = crate::domain::GateResult {
            status: "passed".to_string(),
            approved_workspace: Some(
                crate::gitutil::completion_publication_approval(repo.path()).unwrap(),
            ),
            publication_identity: crate::gitutil::completion_publication_root_identity(repo.path())
                .unwrap(),
            ..crate::domain::GateResult::default()
        };
        let store = Store::new(repo.path());
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        crate::gitutil::run(
            repo.path(),
            &["commit", "--allow-empty", "-m", "concurrent"],
        )
        .unwrap();

        let manifest = store
            .completion_publication_manifest_for_gate("run-1", &["slice-001".to_string()], &gate)
            .unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(format!("{err:#}").contains("passed gate"), "{err:#}");
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
    }

    #[test]
    fn completion_publication_rejects_index_change_after_gate_approval() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let gate = crate::domain::GateResult {
            status: "passed".to_string(),
            approved_workspace: Some(
                crate::gitutil::completion_publication_approval(repo.path()).unwrap(),
            ),
            publication_identity: crate::gitutil::completion_publication_root_identity(repo.path())
                .unwrap(),
            ..crate::domain::GateResult::default()
        };
        let store = Store::new(repo.path());
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        crate::gitutil::run(repo.path(), &["update-index", "--index-version", "4"]).unwrap();
        let manifest = store
            .completion_publication_manifest_for_gate("run-1", &["slice-001".to_string()], &gate)
            .unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(format!("{err:#}").contains("passed gate"), "{err:#}");
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
    }

    #[test]
    fn completion_publication_rejects_semantically_valid_replacement_after_pin() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let mut replacement: Slice = super::read_json(store.slice_path("slice-001")).unwrap();
        replacement.title = "concurrent but still semantically valid".to_string();
        super::write_json(store.slice_path("slice-001"), &replacement).unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("pinned semantic manifest"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rejects_mode_change_after_semantic_pin() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let report = store.final_report_artifact_path("run-1");
        let mut permissions = fs::metadata(&report).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&report, permissions).unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("pinned semantic manifest"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rejects_symlinked_repository_ancestor_before_pinning() {
        let parent = tempfile::tempdir().unwrap();
        let actual_parent = parent.path().join("actual");
        let alias_parent = parent.path().join("alias");
        let repo_path = actual_parent.join("integration");
        fs::create_dir_all(&repo_path).unwrap();
        crate::gitutil::run(&repo_path, &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(&repo_path, &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(&repo_path, &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo_path.join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(&repo_path, "initial").unwrap();
        std::os::unix::fs::symlink(&actual_parent, &alias_parent).unwrap();
        let aliased_repo = alias_parent.join("integration");

        let err = crate::gitutil::completion_publication_root_identity(&aliased_repo).unwrap_err();

        assert!(
            format!("{err:#}").contains("publication directory component"),
            "{err:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_root_replacement_after_semantic_pin() {
        let parent = tempfile::tempdir().unwrap();
        let repo_path = parent.path().join("integration");
        let parked_path = parent.path().join("parked-integration");
        fs::create_dir(&repo_path).unwrap();
        crate::gitutil::run(&repo_path, &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(&repo_path, &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(&repo_path, &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo_path.join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(&repo_path, "initial").unwrap();
        let store = Store::new(&repo_path);
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let original_head = crate::gitutil::head_sha(&repo_path).unwrap();

        fs::rename(&repo_path, &parked_path).unwrap();
        fs::create_dir(&repo_path).unwrap();
        crate::gitutil::run(&repo_path, &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(&repo_path, &["config", "user.email", "outside@example.com"]).unwrap();
        crate::gitutil::run(&repo_path, &["config", "user.name", "Outside"]).unwrap();
        fs::write(repo_path.join("outside.txt"), "outside\n").unwrap();
        crate::gitutil::commit_all(&repo_path, "outside initial").unwrap();
        for entry in &manifest.exact.entries {
            let path = repo_path.join(&entry.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &entry.expected_bytes).unwrap();
        }
        let replacement_head = crate::gitutil::head_sha(&repo_path).unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(format!("{err:#}").contains("identity diverged"), "{err:#}");
        assert_eq!(
            crate::gitutil::head_sha(&parked_path).unwrap(),
            original_head
        );
        assert_eq!(
            crate::gitutil::head_sha(&repo_path).unwrap(),
            replacement_head
        );
    }

    #[test]
    fn normal_and_recovery_publication_manifests_have_identical_semantics() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let store = Store::new(repo.path());
        let slice_ids = vec!["slice-001".to_string()];
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);

        let normal = store
            .completion_publication_manifest("run-1", &slice_ids)
            .unwrap();
        let recovery = store
            .completion_publication_manifest_for_recovery("run-1", &slice_ids)
            .unwrap();

        assert_eq!(normal.run_id, recovery.run_id);
        assert_eq!(normal.closed_slice_ids, recovery.closed_slice_ids);
        assert_eq!(
            normal.gate_publication_identity,
            recovery.gate_publication_identity
        );
        assert_eq!(normal.exact, recovery.exact);
        store
            .validate_completion_publication_manifest_semantics(&normal)
            .unwrap();
        store
            .validate_completion_publication_manifest_semantics(&recovery)
            .unwrap();
    }

    #[test]
    fn completion_receipt_rejects_commit_resident_wrong_run_semantics() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let semantic_manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        for path in [
            store.implementation_summary_report_path("run-1"),
            store.final_report_artifact_path("run-1"),
        ] {
            let mut summary: serde_json::Value = super::read_json(&path).unwrap();
            summary["run_id"] = serde_json::Value::String("run-other".to_string());
            super::write_json(path, &summary).unwrap();
        }
        let malicious_manifest = exact_manifest(
            repo.path(),
            semantic_manifest
                .exact
                .entries
                .iter()
                .map(|entry| exact_manifest_entry(repo.path(), entry.path.clone()))
                .collect(),
        );
        let exact = crate::gitutil::commit_exact_paths(
            repo.path(),
            &malicious_manifest,
            "malicious publication fixture",
            "refs/heads/main",
        )
        .unwrap();
        let receipt = super::completion_publication_receipt(exact, &semantic_manifest);

        let err = store
            .validate_completion_publication_receipt_at_ref("main", &receipt)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("does not describe a passed, pre-publication summary"),
            "{err:#}"
        );
    }

    #[test]
    fn completion_publication_commit_failure_unstages_only_manifest_paths() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        crate::gitutil::run(repo.path(), &["config", "user.name", ""]).unwrap();
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(err.to_string().contains("publication commit failed"));
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert!(store.slice_path("slice-001").is_file());
        assert!(store.implementation_summary_report_path("run-1").is_file());
        assert!(store.final_report_artifact_path("run-1").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rejects_a_symlinked_manifest_parent() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::write(outside.path().join("report.json"), "outside\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join("publication")).unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        let manifest = exact_manifest(
            repo.path(),
            vec![crate::gitutil::ExactPathManifestEntry {
                path: std::path::PathBuf::from("publication/report.json"),
                expected_bytes: b"outside\n".to_vec(),
                expected_mode: "100644".to_string(),
            }],
        );
        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("non-directory parent"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert_eq!(
            fs::read(outside.path().join("report.json")).unwrap(),
            b"outside\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_parent_substitution_cannot_redirect_capture() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        fs::write(outside.path().join("report.json"), "outside\n").unwrap();
        let path = std::path::PathBuf::from("publication/report.json");
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), path.clone())],
        );
        crate::gitutil::substitute_next_publication_parent_during_open(
            repo.path(),
            &path,
            outside.path(),
        );
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("non-directory parent"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert_eq!(
            fs::read(outside.path().join("report.json")).unwrap(),
            b"outside\n"
        );
        assert!(
            crate::gitutil::run(
                repo.path(),
                &["ls-tree", "--name-only", "HEAD", "publication/report.json"]
            )
            .unwrap()
            .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rejects_whole_worktree_substitution() {
        let main = tempfile::tempdir().unwrap();
        let worktrees = tempfile::tempdir().unwrap();
        let integration = worktrees.path().join("integration");
        let parked = worktrees.path().join("parked-integration");
        let marker = worktrees.path().join("publication-captured");
        let release = worktrees.path().join("publication-release");
        crate::gitutil::run(main.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(main.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(main.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(main.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(main.path(), "initial").unwrap();
        crate::gitutil::run(
            main.path(),
            &[
                "worktree",
                "add",
                "-b",
                "integration",
                integration.to_str().unwrap(),
            ],
        )
        .unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "inside\n").unwrap();
        let before = crate::gitutil::head_sha(&integration).unwrap();
        let manifest = exact_manifest(
            &integration,
            vec![exact_manifest_entry(
                &integration,
                "publication/report.json",
            )],
        );
        crate::gitutil::pause_next_publication_after_capture(&integration, &marker, &release);
        let publication_root = integration.clone();
        let publication_manifest = manifest.clone();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &publication_manifest,
                "publication",
                "refs/heads/integration",
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.is_file() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for whole-root substitution pause"
            );
            thread::sleep(Duration::from_millis(10));
        }

        fs::rename(&integration, &parked).unwrap();
        fs::create_dir(&integration).unwrap();
        fs::copy(parked.join(".git"), integration.join(".git")).unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "outside\n").unwrap();
        fs::write(&release, "release\n").unwrap();

        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("worktree root changed"),
            "{err:#}"
        );
        assert_eq!(
            crate::gitutil::run(main.path(), &["rev-parse", "integration"]).unwrap(),
            before
        );
        assert_eq!(
            fs::read(integration.join("publication/report.json")).unwrap(),
            b"outside\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_git_subprocesses_stay_on_pinned_admin_directories() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main");
        let integration = parent.path().join("integration");
        let parked_common = parent.path().join("parked-common.git");
        let marker = parent.path().join("publication-captured");
        let release = parent.path().join("publication-release");
        fs::create_dir(&main).unwrap();
        crate::gitutil::run(&main, &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(&main, &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(&main, &["config", "user.name", "Test User"]).unwrap();
        fs::write(main.join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(&main, "initial").unwrap();
        crate::gitutil::run(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "integration",
                integration.to_str().unwrap(),
            ],
        )
        .unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            &integration,
            vec![exact_manifest_entry(
                &integration,
                "publication/report.json",
            )],
        );
        let original_ref = crate::gitutil::run(&main, &["rev-parse", "integration"]).unwrap();
        crate::gitutil::pause_next_publication_after_capture(&integration, &marker, &release);
        let publication_root = integration.clone();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/integration",
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.is_file() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for admin-directory substitution pause"
            );
            thread::sleep(Duration::from_millis(10));
        }

        fs::rename(main.join(".git"), &parked_common).unwrap();
        let copy = Command::new("cp")
            .args(["-a"])
            .arg(&parked_common)
            .arg(main.join(".git"))
            .status()
            .unwrap();
        assert!(copy.success());
        let replacement_ref = crate::gitutil::run(&main, &["rev-parse", "integration"]).unwrap();
        let replacement_index = fs::read(main.join(".git/worktrees/integration/index")).unwrap();
        let replacement_objects = crate::gitutil::run(&main, &["count-objects", "-v"]).unwrap();
        fs::write(&release, "release\n").unwrap();

        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("administrative directory changed"),
            "{err:#}"
        );
        assert_eq!(
            crate::gitutil::run(&main, &["rev-parse", "integration"]).unwrap(),
            replacement_ref
        );
        assert_eq!(
            fs::read(main.join(".git/worktrees/integration/index")).unwrap(),
            replacement_index
        );
        assert_eq!(
            crate::gitutil::run(&main, &["count-objects", "-v"]).unwrap(),
            replacement_objects
        );
        assert_eq!(
            crate::gitutil::run(
                parent.path(),
                &[
                    &format!("--git-dir={}", parked_common.display()),
                    "rev-parse",
                    "refs/heads/integration",
                ],
            )
            .unwrap(),
            original_ref
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_nested_ref_parent_substitution() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main");
        let integration = parent.path().join("integration");
        fs::create_dir(&main).unwrap();
        crate::gitutil::run(&main, &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(&main, &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(&main, &["config", "user.name", "Test User"]).unwrap();
        fs::write(main.join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(&main, "initial").unwrap();
        crate::gitutil::run(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "integration/nested",
                integration.to_str().unwrap(),
            ],
        )
        .unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            &integration,
            vec![exact_manifest_entry(
                &integration,
                "publication/report.json",
            )],
        );
        let before = crate::gitutil::run(&main, &["rev-parse", "integration/nested"]).unwrap();
        install_blocking_publication_pause(&integration);
        let publication_root = integration.clone();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/integration/nested",
            )
        });
        wait_for_publication_pause(&integration);
        let ref_parent = main.join(".git/refs/heads/integration");
        let parked = main.join(".git/refs/heads/integration.parked");
        fs::rename(&ref_parent, &parked).unwrap();
        assert!(
            Command::new("cp")
                .args(["-a"])
                .arg(&parked)
                .arg(&ref_parent)
                .status()
                .unwrap()
                .success()
        );
        fs::write(integration.join("publication-filter.release"), "release\n").unwrap();
        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("ref parent directory changed"),
            "{err:#}"
        );
        assert_eq!(
            crate::gitutil::run(&main, &["rev-parse", "integration/nested"]).unwrap(),
            before
        );
        assert_eq!(
            fs::read_to_string(parked.join("nested")).unwrap().trim(),
            before
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_loose_ref_leaf_substitution() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        install_blocking_publication_pause(repo.path());
        let publication_root = repo.path().to_path_buf();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/main",
            )
        });
        wait_for_publication_pause(repo.path());
        let reference = repo.path().join(".git/refs/heads/main");
        let parked = repo.path().join(".git/refs/heads/main.parked");
        fs::rename(&reference, &parked).unwrap();
        fs::copy(&parked, &reference).unwrap();
        fs::write(repo.path().join("publication-filter.release"), "release\n").unwrap();
        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("loose Git ref changed"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert_eq!(fs::read_to_string(parked).unwrap().trim(), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_index_leaf_substitution() {
        let parent = tempfile::tempdir().unwrap();
        let main = parent.path().join("main");
        let integration = parent.path().join("integration");
        fs::create_dir(&main).unwrap();
        crate::gitutil::run(&main, &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(&main, &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(&main, &["config", "user.name", "Test User"]).unwrap();
        fs::write(main.join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(&main, "initial").unwrap();
        crate::gitutil::run(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "integration",
                integration.to_str().unwrap(),
            ],
        )
        .unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            &integration,
            vec![exact_manifest_entry(
                &integration,
                "publication/report.json",
            )],
        );
        let before = crate::gitutil::run(&main, &["rev-parse", "integration"]).unwrap();
        install_blocking_publication_pause(&integration);
        let publication_root = integration.clone();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/integration",
            )
        });
        wait_for_publication_pause(&integration);
        let index = main.join(".git/worktrees/integration/index");
        let parked = main.join(".git/worktrees/integration/index.parked");
        fs::rename(&index, &parked).unwrap();
        fs::copy(&parked, &index).unwrap();
        let replacement = fs::read(&index).unwrap();
        fs::write(integration.join("publication-filter.release"), "release\n").unwrap();
        let err = publisher.join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("Git index changed"), "{err:#}");
        assert_eq!(
            crate::gitutil::run(&main, &["rev-parse", "integration"]).unwrap(),
            before
        );
        assert_eq!(fs::read(index).unwrap(), replacement);
        assert_eq!(fs::read(parked).unwrap(), replacement);
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rejects_invalid_existing_loose_object_content() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        let report = repo.path().join("publication/report.json");
        let report_bytes = format!("collision-fixture-{:016x}\n", rand::random::<u64>());
        fs::write(&report, &report_bytes).unwrap();
        let object_id = crate::gitutil::run(
            repo.path(),
            &["hash-object", "--no-filters", report.to_str().unwrap()],
        )
        .unwrap();
        let fanout = repo.path().join(".git/objects").join(&object_id[..2]);
        fs::create_dir_all(&fanout).unwrap();
        let object = fanout.join(&object_id[2..]);
        fs::write(&object, b"not a valid loose object").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("existing loose Git object content"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert_eq!(fs::read(object).unwrap(), b"not a valid loose object");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_object_fanout_substitution() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        let report = repo.path().join("publication/report.json");
        fs::write(&report, "inside-object-fanout-test\n").unwrap();
        let object_id = crate::gitutil::run(
            repo.path(),
            &["hash-object", "--no-filters", report.to_str().unwrap()],
        )
        .unwrap();
        let fanout = repo.path().join(".git/objects").join(&object_id[..2]);
        fs::create_dir_all(&fanout).unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        install_blocking_publication_pause(repo.path());
        let publication_root = repo.path().to_path_buf();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/main",
            )
        });
        wait_for_publication_pause(repo.path());
        let parked = repo.path().join(".git/objects/parked-fanout");
        fs::rename(&fanout, &parked).unwrap();
        fs::create_dir(&fanout).unwrap();
        fs::write(repo.path().join("publication-filter.release"), "release\n").unwrap();
        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("object fanout changed"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert!(fs::read_dir(&fanout).unwrap().next().is_none());
        assert!(fs::read_dir(&parked).unwrap().next().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_object_directory_substitution() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        install_blocking_publication_pause(repo.path());
        let publication_root = repo.path().to_path_buf();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/main",
            )
        });
        wait_for_publication_pause(repo.path());
        let objects = repo.path().join(".git/objects");
        let parked = repo.path().join(".git/objects.parked");
        fs::rename(&objects, &parked).unwrap();
        assert!(
            Command::new("cp")
                .args(["-a"])
                .arg(&parked)
                .arg(&objects)
                .status()
                .unwrap()
                .success()
        );
        let replacement_entries = fs::read_dir(&objects).unwrap().count();
        fs::write(repo.path().join("publication-filter.release"), "release\n").unwrap();
        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("object directory changed"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert_eq!(fs::read_dir(objects).unwrap().count(), replacement_entries);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_packed_ref_leaf_substitution() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        crate::gitutil::run(repo.path(), &["pack-refs", "--all", "--prune"]).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        install_blocking_publication_pause(repo.path());
        let publication_root = repo.path().to_path_buf();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/main",
            )
        });
        wait_for_publication_pause(repo.path());
        let packed = repo.path().join(".git/packed-refs");
        let parked = repo.path().join(".git/packed-refs.parked");
        fs::rename(&packed, &parked).unwrap();
        fs::copy(&parked, &packed).unwrap();
        fs::write(repo.path().join("publication-filter.release"), "release\n").unwrap();
        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("packed Git refs changed"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert!(!repo.path().join(".git/refs/heads/main").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_restores_ref_hook_worktree_and_config_mutations() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        let config = repo.path().join(".git/config");
        fs::write(
            &hook,
            format!(
                r#"#!/bin/sh
if [ "${{KHAZAD_PUBLICATION_REF_TRANSACTION:-}}" = 1 ] && [ "$1" = committed ]; then
    printf hook-side-effect > "$GIT_WORK_TREE/hook-side-effect"
    printf '\n[hook-side-effect]\n\tvalue = changed\n' >> '{}'
fi
exit 0
"#,
                config.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let config_before = fs::read(&config).unwrap();
        let index_before = fs::read(repo.path().join(".git/index")).unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("ref hook changed worktree or local configuration"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        assert_eq!(fs::read(&config).unwrap(), config_before);
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            index_before
        );
        assert!(!repo.path().join("hook-side-effect").exists());
        assert_eq!(
            fs::read(repo.path().join("publication/report.json")).unwrap(),
            b"inside\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_runs_configured_reference_hook() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("configured-hook-ran");
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        fs::create_dir(repo.path().join(".githooks")).unwrap();
        let hook = repo.path().join(".githooks/reference-transaction");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif [ \"${{KHAZAD_PUBLICATION_REF_TRANSACTION:-}}\" = 1 ]; then\n  printf configured > '{}'\n  printf 'configured hook refused publication\\n' >&2\n  exit 1\nfi\nexit 0\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        crate::gitutil::run(repo.path(), &["config", "core.hooksPath", ".githooks"]).unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("configured hook refused publication"),
            "{err:#}"
        );
        assert!(marker.is_file(), "configured reference hook did not run");
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_creates_missing_effective_reflog_hierarchy() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "core.logAllRefUpdates", "true"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::remove_dir_all(repo.path().join(".git/logs")).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let receipt = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap();

        let log = fs::read_to_string(repo.path().join(".git/logs/refs/heads/main")).unwrap();
        assert!(
            log.lines()
                .any(|line| line
                    .starts_with(&format!("{} {} ", receipt.parent_sha, receipt.commit_sha))),
            "missing publication ref update in effective reflog: {log}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_publication_removes_only_its_created_reflog_directories() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "core.logAllRefUpdates", "true"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::remove_dir_all(repo.path().join(".git/logs/refs")).unwrap();
        fs::create_dir(repo.path().join(".git/logs/refs")).unwrap();
        fs::create_dir(repo.path().join(".git/logs/operator-state")).unwrap();
        fs::write(
            repo.path().join(".git/logs/operator-state/marker"),
            "preserve\n",
        )
        .unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            "#!/bin/sh\n[ \"$1\" != prepared ] || exit 1\nexit 0\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("ref compare-and-swap failed"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        let preexisting_refs = repo.path().join(".git/logs/refs");
        assert!(
            preexisting_refs.is_dir(),
            "failed transaction removed a pre-existing reflog directory"
        );
        assert!(
            fs::read_dir(&preexisting_refs).unwrap().next().is_none(),
            "failed transaction left its operation-created reflog child"
        );
        assert_eq!(
            fs::read(repo.path().join(".git/logs/operator-state/marker")).unwrap(),
            b"preserve\n"
        );
        assert!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_preserves_disabled_reflog_behavior() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        crate::gitutil::run(repo.path(), &["config", "core.logAllRefUpdates", "false"]).unwrap();
        fs::remove_dir_all(repo.path().join(".git/logs")).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let receipt = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap();

        assert!(receipt.committed);
        assert!(!repo.path().join(".git/logs").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_updates_existing_reflog_when_auto_creation_is_disabled() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let reflog = repo.path().join(".git/logs/refs/heads/main");
        let reflog_before = fs::read(&reflog).unwrap();
        crate::gitutil::run(repo.path(), &["config", "core.logAllRefUpdates", "false"]).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let receipt = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap();

        let reflog_after = fs::read(&reflog).unwrap();
        assert!(receipt.committed);
        assert_eq!(
            reflog_after.iter().filter(|byte| **byte == b'\n').count(),
            reflog_before.iter().filter(|byte| **byte == b'\n').count() + 1
        );
        assert!(
            String::from_utf8_lossy(&reflog_after).contains(&receipt.commit_sha),
            "publication reflog omitted {}",
            receipt.commit_sha
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_restores_ignored_ref_hook_mutation() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored-hook-state\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = repo.path().join("ignored-hook-state");
        fs::write(&ignored, "baseline ignored bytes\n").unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            r#"#!/bin/sh
if [ "${KHAZAD_PUBLICATION_REF_TRANSACTION:-}" = 1 ] && [ "$1" = committed ]; then
    printf changed-by-hook > "$GIT_WORK_TREE/ignored-hook-state"
fi
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("ref hook changed worktree or local configuration"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        assert_eq!(fs::read(&ignored).unwrap(), b"baseline ignored bytes\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_blocks_before_hooks_when_ignored_lease_is_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored-hook-state\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = repo.path().join("ignored-hook-state");
        fs::write(&ignored, "baseline ignored bytes\n").unwrap();
        let _writer = fs::OpenOptions::new().write(true).open(&ignored).unwrap();
        let marker = repo.path().join("hook-ran");
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf hook-ran > '{}'\nexit 0\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("acquire ignored publication write leases"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        assert_eq!(fs::read(&ignored).unwrap(), b"baseline ignored bytes\n");
        assert!(
            !marker.exists(),
            "publication hook ran without a safe lease"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_restores_replaced_ignored_file_and_metadata() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored-hook-state\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = repo.path().join("ignored-hook-state");
        fs::write(&ignored, "baseline ignored bytes\n").unwrap();
        let mut original_permissions = fs::metadata(&ignored).unwrap().permissions();
        original_permissions.set_mode(0o640);
        fs::set_permissions(&ignored, original_permissions).unwrap();
        let original_metadata = fs::metadata(&ignored).unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            r#"#!/bin/sh
if [ "${KHAZAD_PUBLICATION_REF_TRANSACTION:-}" = 1 ] && [ "$1" = committed ]; then
    rm -f "$GIT_WORK_TREE/ignored-hook-state"
    printf replacement-by-hook > "$GIT_WORK_TREE/ignored-hook-state"
    chmod 0600 "$GIT_WORK_TREE/ignored-hook-state"
fi
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("ref hook changed worktree or local configuration"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        assert_eq!(fs::read(&ignored).unwrap(), b"baseline ignored bytes\n");
        let restored_metadata = fs::metadata(&ignored).unwrap();
        assert_eq!(restored_metadata.mode(), original_metadata.mode());
        assert_eq!(restored_metadata.mtime(), original_metadata.mtime());
        assert_eq!(
            restored_metadata.mtime_nsec(),
            original_metadata.mtime_nsec()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disk_backup_failure_uses_memory_fallback_before_ignored_file_mutation() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored-hook-state\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = repo.path().join("ignored-hook-state");
        fs::write(&ignored, "baseline ignored bytes\n").unwrap();
        crate::gitutil::fail_next_publication_lazy_backup(&ignored);
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            r#"#!/bin/sh
if [ "${KHAZAD_PUBLICATION_REF_TRANSACTION:-}" = 1 ] && [ "$1" = committed ]; then
    printf mutation-must-not-land > "$GIT_WORK_TREE/ignored-hook-state"
fi
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("ref hook changed worktree or local configuration"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        assert_eq!(fs::read(&ignored).unwrap(), b"baseline ignored bytes\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn both_lazy_backup_failures_cancel_and_reap_writer_before_lease_release() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::time::{Duration, Instant};

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(
            repo.path().join(".gitignore"),
            "ignored-hook-state\nwriter-survived\n",
        )
        .unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = repo.path().join("ignored-hook-state");
        fs::write(&ignored, "baseline ignored bytes\n").unwrap();
        let ignored_metadata_before = fs::metadata(&ignored).unwrap();
        let index_before = fs::read(repo.path().join(".git/index")).unwrap();
        crate::gitutil::fail_next_publication_lazy_backup(&ignored);
        crate::gitutil::fail_next_publication_memory_backup(&ignored);
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            r#"#!/bin/sh
if [ "${KHAZAD_PUBLICATION_REF_TRANSACTION:-}" = 1 ] && [ "$1" = committed ]; then
    target="$GIT_WORK_TREE/ignored-hook-state"
    marker="$GIT_WORK_TREE/writer-survived"
    setsid -w env -i PATH="$PATH" sh -c '
        exec </dev/null >/dev/null 2>&1
        trap "" TERM
        printf mutation-must-not-land > "$1"
        printf survived > "$2"
    ' sh "$target" "$marker" &
    wait
fi
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let head_before = crate::gitutil::head_sha(repo.path()).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let started = Instant::now();
        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "backup failure waited for the kernel lease-break timeout: {:?}",
            started.elapsed()
        );
        let error = format!("{err:#}");
        assert!(
            error.contains("verification_restoration_failed")
                && error.contains("ignored publication lease monitor failed"),
            "{error}"
        );
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), head_before);
        assert_eq!(fs::read(&ignored).unwrap(), b"baseline ignored bytes\n");
        let ignored_metadata_after = fs::metadata(&ignored).unwrap();
        assert_eq!(
            (
                ignored_metadata_after.dev(),
                ignored_metadata_after.ino(),
                ignored_metadata_after.mode(),
                ignored_metadata_after.uid(),
                ignored_metadata_after.gid(),
                ignored_metadata_after.nlink(),
                ignored_metadata_after.len(),
                ignored_metadata_after.mtime(),
                ignored_metadata_after.mtime_nsec(),
            ),
            (
                ignored_metadata_before.dev(),
                ignored_metadata_before.ino(),
                ignored_metadata_before.mode(),
                ignored_metadata_before.uid(),
                ignored_metadata_before.gid(),
                ignored_metadata_before.nlink(),
                ignored_metadata_before.len(),
                ignored_metadata_before.mtime(),
                ignored_metadata_before.mtime_nsec(),
            )
        );
        assert!(!repo.path().join("writer-survived").exists());
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            index_before
        );
        let journal = fs::read(repo.path().join(".git/index.lock")).unwrap();
        let magic = b"KHAZAD-INDEX-TRANSACTION-V1\0";
        assert!(
            journal.starts_with(magic),
            "backup failure did not retain its durable recovery journal"
        );
        assert_eq!(
            u32::from_be_bytes(journal[magic.len()..magic.len() + 4].try_into().unwrap()),
            u32::MAX,
            "retained recovery journal was not durably marked abandoned"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_restores_mutated_ignored_hardlink_group() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::create_dir(repo.path().join("ignored")).unwrap();
        let first = repo.path().join("ignored/first");
        let second = repo.path().join("ignored/second");
        fs::write(&first, "shared baseline bytes\n").unwrap();
        fs::hard_link(&first, &second).unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        fs::write(
            &hook,
            r#"#!/bin/sh
if [ "${KHAZAD_PUBLICATION_REF_TRANSACTION:-}" = 1 ] && [ "$1" = committed ]; then
    printf changed-by-hook > "$GIT_WORK_TREE/ignored/second"
fi
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("ref hook changed worktree or local configuration"),
            "{err:#}"
        );
        assert_eq!(fs::read(&first).unwrap(), b"shared baseline bytes\n");
        assert_eq!(fs::read(&second).unwrap(), b"shared baseline bytes\n");
        let first_metadata = fs::metadata(&first).unwrap();
        let second_metadata = fs::metadata(&second).unwrap();
        assert_eq!(first_metadata.ino(), second_metadata.ino());
        assert_eq!(first_metadata.nlink(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_ignored_hardlink_outside_worktree() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored-state\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = repo.path().join("ignored-state");
        fs::write(&ignored, "shared bytes\n").unwrap();
        fs::hard_link(&ignored, outside.path().join("outside-link")).unwrap();
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );

        let err = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("hard-link group escapes the captured worktree"),
            "{err:#}"
        );
        assert_eq!(
            fs::read(outside.path().join("outside-link")).unwrap(),
            b"shared bytes\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_does_not_read_unchanged_large_ignored_file() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join(".gitignore"), "ignored-cache\n").unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let ignored = fs::File::create(repo.path().join("ignored-cache")).unwrap();
        ignored.set_len(8 * 1024 * 1024 * 1024).unwrap();
        drop(ignored);
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let started = Instant::now();

        let receipt = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap();

        assert!(receipt.committed);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "unchanged sparse ignored file was copied or read: {:?}",
            started.elapsed()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_rejects_packed_ref_change_after_overlay_copy() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let approved = crate::gitutil::head_sha(repo.path()).unwrap();
        crate::gitutil::run(
            repo.path(),
            &["commit", "--allow-empty", "-m", "concurrent packed target"],
        )
        .unwrap();
        let concurrent = crate::gitutil::head_sha(repo.path()).unwrap();
        crate::gitutil::run(repo.path(), &["reset", "--hard", &approved]).unwrap();
        crate::gitutil::run(repo.path(), &["pack-refs", "--all", "--prune"]).unwrap();
        assert!(!repo.path().join(".git/refs/heads/main").exists());
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let marker = repo.path().join("packed-overlay-copied");
        let release = repo.path().join("packed-overlay-release");
        crate::gitutil::pause_next_publication_after_packed_ref_copy(
            repo.path(),
            &marker,
            &release,
        );
        let publication_root = repo.path().to_path_buf();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/main",
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.is_file() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for packed-ref overlay copy"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let packed = repo.path().join(".git/packed-refs");
        let bytes = fs::read(&packed).unwrap();
        let changed = String::from_utf8(bytes)
            .unwrap()
            .replace(&approved, &concurrent);
        fs::write(&packed, changed).unwrap();
        fs::write(&release, "release\n").unwrap();

        let err = publisher.join().unwrap().unwrap_err();

        assert!(
            format!("{err:#}").contains("packed Git refs content changed"),
            "{err:#}"
        );
        assert!(!repo.path().join(".git/refs/heads/main").exists());
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), concurrent);
    }

    #[test]
    fn completion_publication_supports_a_packed_integration_ref() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        crate::gitutil::run(repo.path(), &["pack-refs", "--all", "--prune"]).unwrap();
        assert!(!repo.path().join(".git/refs/heads/main").exists());
        fs::create_dir(repo.path().join("publication")).unwrap();
        fs::write(repo.path().join("publication/report.json"), "inside\n").unwrap();
        let manifest = exact_manifest(
            repo.path(),
            vec![exact_manifest_entry(repo.path(), "publication/report.json")],
        );
        let receipt = crate::gitutil::commit_exact_paths(
            repo.path(),
            &manifest,
            "publication",
            "refs/heads/main",
        )
        .unwrap();
        assert!(receipt.committed);
        assert_eq!(
            crate::gitutil::head_sha(repo.path()).unwrap(),
            receipt.commit_sha
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_publication_git_subprocesses_stay_on_pinned_root() {
        let parent = tempfile::tempdir().unwrap();
        let integration = parent.path().join("integration");
        let parked = parent.path().join("parked-integration");
        let marker = parent.path().join("publication-captured");
        let release = parent.path().join("publication-release");
        fs::create_dir(&integration).unwrap();
        crate::gitutil::run(&integration, &["init", "-b", "integration"]).unwrap();
        crate::gitutil::run(&integration, &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(&integration, &["config", "user.name", "Test User"]).unwrap();
        fs::write(integration.join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(&integration, "initial").unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "inside\n").unwrap();
        let before = crate::gitutil::head_sha(&integration).unwrap();
        let manifest = exact_manifest(
            &integration,
            vec![exact_manifest_entry(
                &integration,
                "publication/report.json",
            )],
        );
        crate::gitutil::pause_next_publication_after_capture(&integration, &marker, &release);
        let publication_root = integration.clone();
        let publisher = thread::spawn(move || {
            crate::gitutil::commit_exact_paths(
                &publication_root,
                &manifest,
                "publication",
                "refs/heads/integration",
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.is_file() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pinned-root publication pause"
            );
            thread::sleep(Duration::from_millis(10));
        }

        fs::rename(&integration, &parked).unwrap();
        fs::create_dir(&integration).unwrap();
        crate::gitutil::run(&integration, &["init", "-b", "integration"]).unwrap();
        crate::gitutil::run(
            &integration,
            &["config", "user.email", "outside@example.com"],
        )
        .unwrap();
        crate::gitutil::run(&integration, &["config", "user.name", "Outside"]).unwrap();
        fs::write(integration.join("outside.txt"), "outside\n").unwrap();
        crate::gitutil::commit_all(&integration, "outside initial").unwrap();
        fs::create_dir(integration.join("publication")).unwrap();
        fs::write(integration.join("publication/report.json"), "outside\n").unwrap();
        let outside_head = crate::gitutil::head_sha(&integration).unwrap();
        let outside_index = fs::read(integration.join(".git/index")).unwrap();
        let outside_objects = crate::gitutil::run(&integration, &["count-objects", "-v"]).unwrap();
        fs::write(&release, "release\n").unwrap();

        let err = publisher.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("worktree root changed"),
            "{err:#}"
        );
        assert_eq!(crate::gitutil::head_sha(&parked).unwrap(), before);
        assert_eq!(
            crate::gitutil::head_sha(&integration).unwrap(),
            outside_head
        );
        assert_eq!(
            fs::read(integration.join(".git/index")).unwrap(),
            outside_index
        );
        assert_eq!(
            crate::gitutil::run(&integration, &["count-objects", "-v"]).unwrap(),
            outside_objects,
            "a publication Git subprocess wrote to the replacement repository"
        );
        assert_eq!(
            fs::read(integration.join("publication/report.json")).unwrap(),
            b"outside\n"
        );
    }

    #[test]
    fn completion_publication_refuses_a_pre_staged_index() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        std::fs::write(repo.path().join("unrelated.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        std::fs::write(repo.path().join("unrelated.txt"), "staged operator edit\n").unwrap();
        crate::gitutil::run(repo.path(), &["add", "unrelated.txt"]).unwrap();
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(err.to_string().contains("pre-staged index"), "{err:#}");
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert_eq!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"]).unwrap(),
            "unrelated.txt"
        );

        crate::gitutil::run(repo.path(), &["reset", "--mixed", "HEAD"]).unwrap();
        std::fs::write(repo.path().join("intent-to-add.txt"), "operator scratch\n").unwrap();
        crate::gitutil::run(
            repo.path(),
            &["add", "--intent-to-add", "intent-to-add.txt"],
        )
        .unwrap();
        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();
        assert!(err.to_string().contains("pre-staged index"), "{err:#}");
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert!(
            !crate::gitutil::run(repo.path(), &["ls-files", "intent-to-add.txt"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn completion_publication_preserves_unrelated_index_flags_and_hidden_bytes() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        fs::write(repo.path().join("skip.txt"), "baseline skip\n").unwrap();
        fs::write(repo.path().join("assume.txt"), "baseline assume\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        crate::gitutil::run(
            repo.path(),
            &["update-index", "--skip-worktree", "skip.txt"],
        )
        .unwrap();
        crate::gitutil::run(
            repo.path(),
            &["update-index", "--assume-unchanged", "assume.txt"],
        )
        .unwrap();
        fs::write(repo.path().join("skip.txt"), "hidden skip bytes\n").unwrap();
        fs::write(repo.path().join("assume.txt"), "hidden assume bytes\n").unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();

        let receipt = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap();

        assert!(receipt.committed);
        assert!(
            crate::gitutil::run(repo.path(), &["ls-files", "-v", "skip.txt"])
                .unwrap()
                .starts_with("S ")
        );
        assert!(
            crate::gitutil::run(repo.path(), &["ls-files", "-v", "assume.txt"])
                .unwrap()
                .starts_with("h ")
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("skip.txt")).unwrap(),
            "hidden skip bytes\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("assume.txt")).unwrap(),
            "hidden assume bytes\n"
        );
        assert_eq!(
            crate::gitutil::run(repo.path(), &["show", "HEAD:skip.txt"]).unwrap(),
            "baseline skip"
        );
        assert_eq!(
            crate::gitutil::run(repo.path(), &["show", "HEAD:assume.txt"]).unwrap(),
            "baseline assume"
        );
    }

    #[test]
    fn completion_publication_recovery_repairs_ref_before_index_crash_state() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let ids = vec!["slice-001".to_string()];
        let manifest = store
            .completion_publication_manifest("run-1", &ids)
            .unwrap();
        let parent_sha = crate::gitutil::head_sha(repo.path()).unwrap();
        crate::gitutil::abandon_next_publication_after_ref_cas(repo.path());
        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("simulated process loss after completion publication ref update")
        );
        let publication_sha = crate::gitutil::head_sha(repo.path()).unwrap();
        assert_ne!(publication_sha, parent_sha);
        assert!(
            !crate::gitutil::status_porcelain(repo.path())
                .unwrap()
                .is_empty()
        );

        #[cfg(unix)]
        {
            let index_path = crate::gitutil::run(
                repo.path(),
                &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            )
            .unwrap();
            let mut lock_path = std::ffi::OsString::from(index_path.trim());
            lock_path.push(".lock");
            let recovery_owner = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(std::path::PathBuf::from(lock_path))
                .unwrap();
            assert_eq!(
                unsafe { libc::flock(recovery_owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
                0
            );
            let concurrent = store
                .find_completion_publication("run-1", "main", &ids)
                .unwrap_err();
            assert!(
                format!("{concurrent:#}").contains("another completion publication recovery"),
                "{concurrent:#}"
            );
        }

        let recovered = store
            .find_completion_publication("run-1", "main", &ids)
            .unwrap()
            .expect("recover exact publication");

        assert_eq!(recovered.commit_sha, publication_sha);
        assert!(
            crate::gitutil::status_porcelain(repo.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn completion_publication_recovery_does_not_overwrite_same_path_staging() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let ids = vec!["slice-001".to_string()];
        let manifest = store
            .completion_publication_manifest("run-1", &ids)
            .unwrap();
        let pinned_final_report = fs::read(store.final_report_artifact_path("run-1")).unwrap();
        let receipt = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap();
        crate::gitutil::run(repo.path(), &["read-tree", &receipt.parent_sha]).unwrap();
        fs::write(
            store.final_report_artifact_path("run-1"),
            "operator staged\n",
        )
        .unwrap();
        crate::gitutil::run(
            repo.path(),
            &["add", ".workflow/reports/run-1-final-report.json"],
        )
        .unwrap();
        fs::write(
            store.final_report_artifact_path("run-1"),
            &pinned_final_report,
        )
        .unwrap();
        let index_path = crate::gitutil::run(
            repo.path(),
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .unwrap();
        let before = fs::read(index_path.trim()).unwrap();

        let err = store
            .find_completion_publication("run-1", "main", &ids)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("without an exact durable transaction journal"),
            "{err:#}"
        );
        assert_eq!(fs::read(index_path.trim()).unwrap(), before);
    }

    #[test]
    fn completion_publication_refuses_the_wrong_head_attachment() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let main_before = crate::gitutil::head_sha(repo.path()).unwrap();
        crate::gitutil::run(repo.path(), &["switch", "-c", "wrong-publication-branch"]).unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(err.to_string().contains("HEAD attachment changed"));
        assert_eq!(
            crate::gitutil::run(repo.path(), &["rev-parse", "refs/heads/main"]).unwrap(),
            main_before
        );
        assert_eq!(
            crate::gitutil::run(repo.path(), &["branch", "--show-current"]).unwrap(),
            "wrong-publication-branch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rejects_concurrent_manifest_content_change() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        install_blocking_publication_pause(repo.path());
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let repo_path = repo.path().to_path_buf();
        let race = thread::spawn(move || {
            let filtered_path = wait_for_publication_pause(&repo_path);
            fs::write(
                repo_path.join(filtered_path),
                "{\"version\":\"concurrent\"}\n",
            )
            .unwrap();
            fs::write(repo_path.join("publication-filter.release"), "release\n").unwrap();
        });

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        race.join().unwrap();
        assert!(err.to_string().contains("manifest path changed"));
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), before);
        assert!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert!(
            fs::read_to_string(repo.path().join("publication-filter.marker"))
                .unwrap()
                .starts_with(".workflow/")
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_rolls_back_final_window_manifest_change() {
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let original_head = crate::gitutil::head_sha(repo.path()).unwrap();
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        let marker = repo.path().join(".git/final-window-once");
        let target = store.final_report_artifact_path("run-1");
        let target_before = fs::read(&target).unwrap();
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif [ \"$1\" = prepared ] && [ ! -e '{}' ]; then\n  : > '{}'\n  printf concurrent > '{}'\nfi\nexit 0\n",
                marker.display(),
                marker.display(),
                target.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("inputs changed during ref compare-and-swap"),
            "{err:#}"
        );
        assert_eq!(
            crate::gitutil::head_sha(repo.path()).unwrap(),
            original_head
        );
        assert_eq!(fs::read(target).unwrap(), target_before);
    }

    #[test]
    fn completion_publication_rolls_back_manifest_change_after_index_install() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let original_head = crate::gitutil::head_sha(repo.path()).unwrap();
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let final_report = store.final_report_artifact_path("run-1");
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        crate::gitutil::mutate_next_publication_after_index_install(
            repo.path(),
            final_report.strip_prefix(repo.path()).unwrap(),
            b"concurrent\n",
        );

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("state changed during index installation"),
            "{err:#}"
        );
        assert_eq!(
            crate::gitutil::head_sha(repo.path()).unwrap(),
            original_head
        );
        assert!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&final_report).unwrap(), b"concurrent\n");
    }

    #[test]
    fn completion_publication_inverse_recovers_post_install_ref_movement() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let original_head = crate::gitutil::head_sha(repo.path()).unwrap();
        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let ids = vec!["slice-001".to_string()];
        let manifest = store
            .completion_publication_manifest("run-1", &ids)
            .unwrap();
        crate::gitutil::rewind_next_publication_after_index_install(repo.path());

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("durable journal retained"),
            "{err:#}"
        );
        assert_eq!(
            crate::gitutil::head_sha(repo.path()).unwrap(),
            original_head
        );
        let index_lock = crate::gitutil::run(
            repo.path(),
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index.lock",
            ],
        )
        .unwrap();
        assert!(std::path::Path::new(index_lock.trim()).is_file());

        assert!(
            store
                .find_completion_publication("run-1", "main", &ids)
                .unwrap()
                .is_none()
        );
        assert!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert!(!std::path::Path::new(index_lock.trim()).exists());
    }

    #[test]
    fn completion_publication_cas_does_not_overwrite_concurrent_head() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        install_blocking_publication_pause(repo.path());
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        let before = crate::gitutil::head_sha(repo.path()).unwrap();
        let tree = crate::gitutil::run(repo.path(), &["rev-parse", "HEAD^{tree}"]).unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let repo_path = repo.path().to_path_buf();
        let before_for_race = before.clone();
        let race = thread::spawn(move || {
            wait_for_publication_pause(&repo_path);
            let concurrent = crate::gitutil::run(
                &repo_path,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &before_for_race,
                    "-m",
                    "concurrent head",
                ],
            )
            .unwrap();
            crate::gitutil::run(
                &repo_path,
                &[
                    "update-ref",
                    "refs/heads/main",
                    &concurrent,
                    &before_for_race,
                ],
            )
            .unwrap();
            fs::write(repo_path.join("publication-filter.release"), "release\n").unwrap();
            concurrent
        });

        let err = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap_err();

        let concurrent = race.join().unwrap();
        assert!(err.to_string().contains("HEAD changed"));
        assert_eq!(crate::gitutil::head_sha(repo.path()).unwrap(), concurrent);
        assert_eq!(
            crate::gitutil::run(repo.path(), &["show", "-s", "--format=%s", "HEAD"]).unwrap(),
            "concurrent head"
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_publication_holds_index_lock_across_ref_transaction() {
        let repo = tempfile::tempdir().unwrap();
        crate::gitutil::run(repo.path(), &["init", "-b", "main"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        crate::gitutil::run(repo.path(), &["config", "user.name", "Test User"]).unwrap();
        let store = Store::new(repo.path());
        store.ensure_layout().unwrap();
        install_blocking_publication_pause(repo.path());
        fs::write(repo.path().join("unrelated.txt"), "baseline\n").unwrap();
        crate::gitutil::commit_all(repo.path(), "initial").unwrap();
        fs::write(repo.path().join("unrelated.txt"), "concurrent edit\n").unwrap();

        write_completion_publication_fixture(&store, "run-1", &["slice-001"]);
        let manifest = store
            .completion_publication_manifest("run-1", &["slice-001".to_string()])
            .unwrap();
        let repo_path = repo.path().to_path_buf();
        let race = thread::spawn(move || {
            wait_for_publication_pause(&repo_path);
            let output = Command::new("git")
                .args(["add", "unrelated.txt"])
                .current_dir(&repo_path)
                .output()
                .unwrap();
            fs::write(repo_path.join("publication-filter.release"), "release\n").unwrap();
            output
        });

        let receipt = store
            .commit_completion_publication("run-1", "main", &manifest)
            .unwrap();

        let concurrent_add = race.join().unwrap();
        assert!(receipt.committed);
        assert!(!concurrent_add.status.success());
        assert!(String::from_utf8_lossy(&concurrent_add.stderr).contains("index.lock"));
        assert!(
            crate::gitutil::run(repo.path(), &["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            crate::gitutil::run(repo.path(), &["show", "HEAD:unrelated.txt"]).unwrap(),
            "baseline"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("unrelated.txt")).unwrap(),
            "concurrent edit\n"
        );
    }
}
