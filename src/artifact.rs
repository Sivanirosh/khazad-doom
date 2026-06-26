use crate::domain::{
    ArtifactEntry, Handoff, ImplementationSummary, Slice, SliceSummary, SliceValidationIssue,
    SliceValidationReport,
};
use crate::gitutil;
use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = ".workflow";

#[derive(Debug, Clone)]
pub struct Store {
    repo_path: PathBuf,
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
        ] {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
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

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }

    pub fn handoff_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("handoffs")
    }

    pub fn output_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("outputs")
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

    pub fn output_path(&self, run_id: &str, name: &str) -> PathBuf {
        self.output_dir(run_id).join(name)
    }

    pub fn write_implementation_summary(&self, summary: &ImplementationSummary) -> Result<PathBuf> {
        let path = self
            .reports_dir()
            .join(format!("{}-implementation-summary.json", summary.run_id));
        write_json(&path, summary)?;
        gitutil::commit_all(
            &self.repo_path,
            &format!("khazad(run): summarize {}", summary.run_id),
        )?;
        Ok(path)
    }

    pub fn write_final_report(&self, summary: &ImplementationSummary) -> Result<PathBuf> {
        let path = self
            .reports_dir()
            .join(format!("{}-final-report.json", summary.run_id));
        write_json(&path, summary)
            .with_context(|| format!("write final report {}", path.display()))?;
        gitutil::commit_all(
            &self.repo_path,
            &format!("khazad(run): final report {}", summary.run_id),
        )?;
        Ok(path)
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
    let mut wanted = BTreeSet::new();
    if requested.is_empty() {
        wanted.extend(slices.iter().map(|slice| slice.id.clone()));
    } else {
        for id in requested {
            collect_with_dependencies(id, &by_id, &mut wanted)?;
        }
    }

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
    use super::{Slice, topological_order, validate_slice, validate_slice_set};

    fn valid_slice(id: &str) -> Slice {
        Slice {
            id: id.to_string(),
            title: "Title".to_string(),
            goal: "Goal".to_string(),
            github_issue: String::new(),
            depends_on: Vec::new(),
            areas: Vec::new(),
            acceptance: vec!["done".to_string()],
            must_ask_if: Vec::new(),
            verify: Vec::new(),
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
}
