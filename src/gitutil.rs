use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(dir: impl AsRef<Path>, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir.as_ref())
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let msg = if stderr.is_empty() { stdout } else { stderr };
        if msg.is_empty() {
            bail!("git {} failed with {}", args.join(" "), output.status);
        }
        bail!(
            "git {} failed with {}: {}",
            args.join(" "),
            output.status,
            msg
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn repo_root(dir: impl AsRef<Path>) -> Result<PathBuf> {
    let root = run(dir.as_ref(), &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root);
    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(dir.as_ref().join(root).canonicalize()?)
    }
}

pub fn head_sha(dir: impl AsRef<Path>) -> Result<String> {
    run(dir, &["rev-parse", "HEAD"])
}

pub fn current_branch(dir: impl AsRef<Path>) -> Result<String> {
    let branch = run(dir, &["branch", "--show-current"])?;
    if branch.is_empty() {
        Ok("HEAD".to_string())
    } else {
        Ok(branch)
    }
}

pub fn status_porcelain(dir: impl AsRef<Path>) -> Result<String> {
    run(dir, &["status", "--porcelain"])
}

pub fn worktree_add(
    repo_path: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
    branch: &str,
    start_point: &str,
) -> Result<()> {
    let worktree = worktree_path.as_ref().to_string_lossy().to_string();
    run(
        repo_path,
        &["worktree", "add", "-B", branch, &worktree, start_point],
    )?;
    Ok(())
}

pub fn worktree_add_existing(
    repo_path: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
    branch: &str,
) -> Result<()> {
    let worktree = worktree_path.as_ref().to_string_lossy().to_string();
    run(repo_path, &["worktree", "add", worktree.as_str(), branch])?;
    Ok(())
}

#[allow(dead_code)]
pub fn worktree_remove(repo_path: impl AsRef<Path>, worktree_path: impl AsRef<Path>) -> Result<()> {
    let worktree = worktree_path.as_ref().to_string_lossy().to_string();
    run(repo_path, &["worktree", "remove", "--force", &worktree])?;
    Ok(())
}

pub fn merge(worktree_path: impl AsRef<Path>, branch: &str, message: &str) -> Result<()> {
    run(worktree_path, &["merge", "--no-ff", branch, "-m", message])?;
    Ok(())
}

pub fn commit_all(dir: impl AsRef<Path>, message: &str) -> Result<()> {
    if status_porcelain(dir.as_ref())?.trim().is_empty() {
        return Ok(());
    }
    run(dir.as_ref(), &["add", "-A"])?;
    run(dir, &["commit", "-m", message])?;
    Ok(())
}
