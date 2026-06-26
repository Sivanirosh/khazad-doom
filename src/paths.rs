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

pub fn repo_id(abs_path: impl AsRef<Path>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(abs_path.as_ref().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..6])
}
