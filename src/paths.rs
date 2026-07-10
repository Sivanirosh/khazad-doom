use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        if let Ok(home) = std::env::var("KHAZAD_HOME") {
            return Ok(Self {
                root: PathBuf::from(home),
            });
        }
        let home = std::env::var_os("HOME").context("resolve home directory: HOME is not set")?;
        Ok(Self {
            root: PathBuf::from(home).join(".khazad-doom"),
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [self.root.clone(), self.log_dir(), self.worktrees_dir()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let profiles = self.agent_profiles_file();
        if !profiles.exists() {
            std::fs::write(&profiles, crate::artifact::default_agent_profiles_toml())
                .with_context(|| format!("write {}", profiles.display()))?;
        }
        Ok(())
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("socket")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }

    pub fn db_file(&self) -> PathBuf {
        self.root.join("state.sqlite")
    }

    pub fn agent_profiles_file(&self) -> PathBuf {
        self.root.join("agents.toml")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.log_dir().join("daemon.log")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    pub fn repo_worktree_dir(&self, repo_id: &str, run_id: &str) -> PathBuf {
        self.worktrees_dir().join(repo_id).join(run_id)
    }
}

/// Finds a reusable Khazad-Doom executable for commands spawned by a long-lived
/// daemon. Linux reports a replaced running executable as `… (deleted)`;
/// prefer the replacement at the stripped path, then PATH, instead of handing
/// that non-executable procfs display path to a child process.
pub(crate) fn khazad_child_binary() -> PathBuf {
    reusable_khazad_binary(std::env::current_exe().ok().as_deref())
        .unwrap_or_else(|| PathBuf::from("khazad-doom"))
}

pub(crate) fn reusable_khazad_binary(current_exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = current_exe {
        if is_executable(path) {
            return Some(path.to_path_buf());
        }
        if let Some(stripped) = strip_linux_deleted_exe_suffix(path)
            && is_executable(&stripped)
        {
            return Some(stripped);
        }
    }
    find_executable_in_path("khazad-doom")
}

fn strip_linux_deleted_exe_suffix(path: &Path) -> Option<PathBuf> {
    path.to_string_lossy()
        .strip_suffix(" (deleted)")
        .map(PathBuf::from)
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn repo_id(abs_path: impl AsRef<Path>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(abs_path.as_ref().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn reusable_binary_strips_linux_deleted_current_exe_suffix() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let installed = temp.path().join("khazad-doom");
        std::fs::write(&installed, b"fake khazad")?;
        let mut permissions = std::fs::metadata(&installed)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&installed, permissions)?;
        let deleted = PathBuf::from(format!("{} (deleted)", installed.display()));

        assert_eq!(reusable_khazad_binary(Some(&deleted)), Some(installed));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn reusable_binary_rejects_non_executable_candidates() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let candidate = temp.path().join("khazad-doom");
        std::fs::write(&candidate, b"not executable")?;
        let mut permissions = std::fs::metadata(&candidate)?.permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&candidate, permissions)?;

        assert!(!is_executable(&candidate));
        Ok(())
    }

    #[test]
    fn ensure_creates_operator_agent_profiles_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("khazad-home"),
        };

        paths.ensure().unwrap();

        let text = std::fs::read_to_string(paths.agent_profiles_file()).unwrap();
        assert!(text.contains("provider = \"openai-codex\""));
    }
}
