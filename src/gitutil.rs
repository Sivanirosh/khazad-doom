use crate::agent::CancellationToken;
use crate::domain::{
    GitPathChangeEvidence, GitWorktreeSnapshotEvidence, VerificationWorkspaceEvidence,
};
use crate::workflow::shell::{ShellCommand, ShellCommandError, ShellFailureKind};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::Mutex;

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

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

pub fn has_retained_completion_publication_journal(dir: impl AsRef<Path>) -> Result<bool> {
    let dir = dir.as_ref();
    match fs::symlink_metadata(dir.join(".git")) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to inspect a symlinked Git administration marker")
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).context("inspect retained-journal Git administration marker"),
    }
    let repository = PinnedGitRepository::open(dir)
        .context("pin worktree while inspecting its retained publication journal")?;
    let (_, lock_path) = repository.index_paths();
    let lock_leaf = lock_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("completion publication journal omitted its leaf"))?;
    repository.ensure_attached()?;
    #[cfg(unix)]
    let file = match open_admin_leaf(&repository.git_dir, lock_leaf, libc::O_RDONLY, 0) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            repository.ensure_attached()?;
            return Ok(false);
        }
        Err(err) => return Err(err.into()),
    };
    #[cfg(not(unix))]
    let file = match File::open(&lock_path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            repository.ensure_attached()?;
            return Ok(false);
        }
        Err(err) => return Err(err.into()),
    };
    let bytes = read_open_file(&file)?;
    #[cfg(unix)]
    ensure_open_file_matches_admin_leaf(&file, &repository.git_dir, lock_leaf)
        .context("retained publication journal changed while it was inspected")?;
    repository.ensure_attached()?;
    if !bytes.starts_with(INDEX_TRANSACTION_MAGIC) {
        bail!("integration worktree has an unrecognized Git index lock; refusing cleanup");
    }
    parse_index_transaction(&bytes).context("validate retained completion publication journal")?;
    claim_stale_index_transaction(&file)
        .context("claim retained completion publication journal for cleanup inspection")?;
    Ok(true)
}

fn error_chain_has_io_kind(error: &anyhow::Error, kind: std::io::ErrorKind) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == kind)
    })
}

pub fn pinned_path_identity_digest(file: &File, logical_path: &Path) -> Result<String> {
    let mut identity = path_identity_bytes(logical_path);
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        identity.extend_from_slice(&metadata.dev().to_be_bytes());
        identity.extend_from_slice(&metadata.ino().to_be_bytes());
        identity.extend_from_slice(&metadata.mode().to_be_bytes());
    }
    #[cfg(not(unix))]
    {
        identity.extend_from_slice(&metadata.len().to_be_bytes());
        identity.push(u8::from(metadata.is_dir()));
        identity.push(u8::from(metadata.is_file()));
    }
    Ok(hex::encode(Sha256::digest(identity)))
}

pub fn path_identity_digest(path: impl AsRef<Path>) -> String {
    let path = path
        .as_ref()
        .canonicalize()
        .unwrap_or_else(|_| path.as_ref().to_path_buf());
    hex::encode(Sha256::digest(path_location_identity_bytes(&path)))
}

#[derive(Debug, Clone)]
struct WorktreeSnapshot {
    git_context: std::sync::Arc<VerificationGitContext>,
    root: PathBuf,
    root_identity: Vec<u8>,
    repository_identity: Vec<u8>,
    git_configuration: GitConfigurationSnapshot,
    head: Vec<u8>,
    head_attachment: Vec<u8>,
    index_entries: Vec<u8>,
    index_file: Vec<u8>,
    index_semantics: Vec<u8>,
    tracked_filesystem: BTreeMap<Vec<u8>, TrackedFilesystemEntry>,
    staged: Vec<u8>,
    unstaged: Vec<u8>,
    untracked: Vec<u8>,
    nonignored_empty_directories: Vec<u8>,
}

struct PublicationSideEffectSnapshot {
    workspace: WorktreeSnapshot,
    ambient_filesystem: BTreeMap<Vec<u8>, PublicationFilesystemEntry>,
    #[cfg(target_os = "linux")]
    lease_monitor: PublicationLeaseMonitor,
}

struct PublicationSideEffectState {
    workspace: WorktreeSnapshot,
    ambient_filesystem: BTreeMap<Vec<u8>, PublicationFilesystemEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicationFilesystemEntry {
    kind: PublicationFilesystemKind,
    mode: u32,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    ctime_seconds: i64,
    ctime_nanoseconds: i64,
    device: u64,
    inode: u64,
    link_count: u64,
    hardlink_group: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicationFilesystemKind {
    File { size: u64 },
    Symlink(Vec<u8>),
    Directory,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitConfigurationSnapshot {
    common: Option<Vec<u8>>,
    worktree: Option<Vec<u8>>,
}

fn git_configuration_digest(configuration: &GitConfigurationSnapshot) -> Vec<u8> {
    let mut digest = Sha256::new();
    for value in [&configuration.common, &configuration.worktree] {
        match value {
            Some(bytes) => {
                digest.update([1]);
                digest.update((bytes.len() as u64).to_be_bytes());
                digest.update(bytes);
            }
            None => digest.update([0]),
        }
    }
    digest.finalize().to_vec()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedFilesystemEntry {
    kind: TrackedFilesystemKind,
    mode: u32,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackedFilesystemKind {
    File(Vec<u8>),
    Symlink(Vec<u8>),
    Directory,
    Missing,
}

impl PartialEq for WorktreeSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.root_identity == other.root_identity
            && self.repository_identity == other.repository_identity
            && self.git_configuration == other.git_configuration
            && self.head == other.head
            && self.head_attachment == other.head_attachment
            && self.index_entries == other.index_entries
            && self.index_file == other.index_file
            && self.index_semantics == other.index_semantics
            && self.tracked_filesystem == other.tracked_filesystem
            && self.staged == other.staged
            && self.unstaged == other.unstaged
            && self.untracked == other.untracked
            && self.nonignored_empty_directories == other.nonignored_empty_directories
    }
}

impl Eq for WorktreeSnapshot {}

impl PublicationSideEffectState {
    fn capture(repository: &PinnedGitRepository) -> Result<Self> {
        repository.ensure_attached()?;
        let workspace = WorktreeSnapshot::capture_from_root(repository.root.operation_path())?;
        let ambient_filesystem = capture_publication_ambient_filesystem(repository)?.entries;
        let workspace_after =
            WorktreeSnapshot::capture_from_root(repository.root.operation_path())?;
        let ambient_after = capture_publication_ambient_filesystem(repository)?.entries;
        repository.ensure_attached()?;
        if workspace != workspace_after || ambient_filesystem != ambient_after {
            bail!("publication worktree state changed while hook-side-effect state was captured");
        }
        Ok(Self {
            workspace,
            ambient_filesystem,
        })
    }
}

impl PublicationSideEffectSnapshot {
    fn capture(repository: &PinnedGitRepository) -> Result<Self> {
        repository.ensure_attached()?;
        let workspace = WorktreeSnapshot::capture_from_root(repository.root.operation_path())?;
        let ambient = capture_publication_ambient_filesystem(repository)?;
        #[cfg(target_os = "linux")]
        let lease_monitor = PublicationLeaseMonitor::start(
            repository.root.operation_path(),
            &ambient.regular_file_groups,
        )?;
        let workspace_after =
            WorktreeSnapshot::capture_from_root(repository.root.operation_path())?;
        let ambient_after = capture_publication_ambient_filesystem(repository)?.entries;
        repository.ensure_attached()?;
        if workspace != workspace_after || ambient.entries != ambient_after {
            bail!("publication worktree state changed while hook-side-effect state was captured");
        }
        Ok(Self {
            workspace,
            ambient_filesystem: ambient.entries,
            #[cfg(target_os = "linux")]
            lease_monitor,
        })
    }

    fn run_supervised_ref_transaction<F>(
        &mut self,
        cancellation: &CancellationToken,
        transaction: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        #[cfg(target_os = "linux")]
        let _ = self.lease_monitor.begin_transaction(cancellation, false)?;
        let transaction =
            crate::workflow::check_cancelled(cancellation).and_then(|()| transaction());
        #[cfg(target_os = "linux")]
        return self.lease_monitor.finish_transaction(transaction);
        #[cfg(not(target_os = "linux"))]
        transaction
    }

    fn run_supervised_ref_rollback<F>(&mut self, rollback: F) -> Result<()>
    where
        F: FnOnce(&CancellationToken) -> Result<()>,
    {
        let cancellation = CancellationToken::new();
        #[cfg(target_os = "linux")]
        let monitor_failed = self.lease_monitor.begin_transaction(&cancellation, true)?;
        #[cfg(not(target_os = "linux"))]
        let monitor_failed = false;
        let transaction = (|| -> Result<()> {
            let deadline_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let deadline = if monitor_failed {
                // Linux does not send a second signal while a failed lease break is
                // pending. Bound the fresh-token rollback instead of waiting for the
                // kernel's forced lease-break timeout.
                let stop = deadline_stop.clone();
                let cancellation = cancellation.clone();
                Some(
                    std::thread::Builder::new()
                        .name("khazad-publication-rollback-deadline".to_string())
                        .spawn(move || {
                            for _ in 0..200 {
                                if stop.load(std::sync::atomic::Ordering::Acquire) {
                                    return;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            cancellation.cancel();
                        })
                        .context("start bounded publication rollback cancellation")?,
                )
            } else {
                None
            };
            let rollback = rollback(&cancellation);
            deadline_stop.store(true, std::sync::atomic::Ordering::Release);
            let deadline = deadline
                .map(|thread| {
                    thread.join().map_err(|_| {
                        anyhow::anyhow!("bounded publication rollback cancellation thread panicked")
                    })
                })
                .transpose()
                .map(|_| ());
            combine_ref_transaction_and_lease_monitor(rollback, deadline)
        })();
        #[cfg(target_os = "linux")]
        return self.lease_monitor.finish_transaction(transaction);
        #[cfg(not(target_os = "linux"))]
        transaction
    }

    fn release_failed_leases_after_all_transactions(
        &mut self,
        final_transaction: &Result<()>,
    ) -> Result<()> {
        if final_transaction
            .as_ref()
            .err()
            .is_some_and(ref_transaction_supervision_failed)
        {
            #[cfg(target_os = "linux")]
            {
                self.lease_monitor.retain_until_process_exit = true;
            }
            bail!(
                "publication process supervision failed; ignored-file leases were retained until daemon exit"
            );
        }
        #[cfg(target_os = "linux")]
        self.lease_monitor.release_if_failed()?;
        Ok(())
    }

    fn retain_leases_after_supervision_failure(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.lease_monitor.retain_until_process_exit = true;
        }
    }

    fn matches(&self, other: &PublicationSideEffectState) -> bool {
        self.workspace
            .matches_publication_side_effect_state(&other.workspace)
            && self.ambient_filesystem == other.ambient_filesystem
            && {
                #[cfg(target_os = "linux")]
                {
                    self.lease_monitor.ensure_healthy().is_ok()
                        && self.lease_monitor.changed_paths().is_empty()
                }
                #[cfg(not(target_os = "linux"))]
                {
                    true
                }
            }
    }

    fn restore(
        &self,
        repository: &PinnedGitRepository,
        entries: &[CapturedPublicationEntry],
        restore_head: bool,
        index_lock: &GitIndexLock,
    ) -> Result<()> {
        self.workspace
            .restore_publication_side_effects(None, entries, restore_head, index_lock)?;
        restore_publication_ambient_filesystem(repository, self, entries)?;
        let restored = PublicationSideEffectState::capture(repository)?;
        if !publication_ambient_semantically_matches(
            &self.ambient_filesystem,
            &restored.ambient_filesystem,
        ) || !self
            .workspace
            .matches_publication_side_effect_state(&restored.workspace)
        {
            bail!("publication hook-side-effect restoration did not reproduce its exact prestate");
        }
        #[cfg(target_os = "linux")]
        self.lease_monitor.ensure_healthy()?;
        Ok(())
    }
}

impl WorktreeSnapshot {
    fn matches_raw_state_except_unstaged_stat_cache(&self, other: &Self) -> bool {
        let mut normalized = other.clone();
        normalized.unstaged.clone_from(&self.unstaged);
        &normalized == self
    }

    fn matches_publication_side_effect_state(&self, other: &Self) -> bool {
        self.root_identity == other.root_identity
            && self.repository_identity == other.repository_identity
            && self.git_configuration == other.git_configuration
            && self.head_attachment == other.head_attachment
            && self.index_entries == other.index_entries
            && self.index_file == other.index_file
            && self.index_semantics == other.index_semantics
            && self.tracked_filesystem == other.tracked_filesystem
            && self.unstaged == other.unstaged
            && self.untracked == other.untracked
            && self.nonignored_empty_directories == other.nonignored_empty_directories
    }
}

pub struct VerificationWorktreeGuard {
    before: WorktreeSnapshot,
    pinned_root: Option<File>,
    pinned_root_path: Option<PathBuf>,
}

pub enum VerificationGuardOutcome {
    Unchanged,
    Mutation(Box<VerificationMutationOutcome>),
}

pub struct VerificationMutationOutcome {
    pub evidence: VerificationWorkspaceEvidence,
    pub restoration_succeeded: bool,
}

impl VerificationWorktreeGuard {
    #[cfg(test)]
    pub fn capture(dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            before: WorktreeSnapshot::capture(dir)?,
            pinned_root: None,
            pinned_root_path: None,
        })
    }

    pub fn capture_pinned(dir: impl AsRef<Path>, root_directory: &File) -> Result<Self> {
        let dir = dir.as_ref();
        ensure_open_file_matches_path(root_directory, dir)
            .context("verification worktree root changed before snapshot capture")?;
        let pinned_root = root_directory.try_clone()?;
        make_pinned_directory_inheritable(&pinned_root)?;
        let operation_root = pinned_directory_path(&pinned_root, dir);
        let before = WorktreeSnapshot::capture_from_root(&operation_root)?;
        ensure_open_file_matches_path(&pinned_root, dir)
            .context("verification worktree root changed during snapshot capture")?;
        Ok(Self {
            before,
            pinned_root: Some(pinned_root),
            pinned_root_path: Some(dir.to_path_buf()),
        })
    }

    pub fn is_clean(&self) -> bool {
        self.before.is_clean()
    }

    pub fn snapshot_digest(&self) -> String {
        self.before.digest()
    }

    pub fn precommand_evidence(&self) -> VerificationWorkspaceEvidence {
        verification_workspace_evidence(
            Some(&self.before),
            None,
            None,
            String::new(),
            String::new(),
        )
    }

    pub fn precommand_change_evidence(&self) -> Option<VerificationWorkspaceEvidence> {
        if let (Some(root), Some(root_path)) = (&self.pinned_root, &self.pinned_root_path)
            && let Err(err) = ensure_open_file_matches_path(root, root_path)
        {
            return Some(verification_workspace_evidence(
                Some(&self.before),
                None,
                None,
                format!("verification worktree root changed: {err:#}"),
                "pre-command state could not be revalidated; no restoration attempted".to_string(),
            ));
        }
        match self.before.recapture() {
            Ok(current) if current == self.before => None,
            Ok(current) => Some(verification_workspace_evidence(
                Some(&self.before),
                Some(&current),
                None,
                String::new(),
                "pre-command state changed concurrently; no restoration attempted".to_string(),
            )),
            Err(err) => Some(verification_workspace_evidence(
                Some(&self.before),
                None,
                None,
                format!("{err:#}"),
                "pre-command state could not be revalidated; no restoration attempted".to_string(),
            )),
        }
    }

    pub fn finish(&self) -> VerificationGuardOutcome {
        let root_attachment_error = match (&self.pinned_root, &self.pinned_root_path) {
            (Some(root), Some(root_path)) => ensure_open_file_matches_path(root, root_path)
                .context("verification worktree root changed before post-command snapshot")
                .err(),
            _ => None,
        };
        let after = self.before.recapture();
        match after {
            Ok(after) if after == self.before && root_attachment_error.is_none() => {
                VerificationGuardOutcome::Unchanged
            }
            Ok(after) => {
                let restoration = if after == self.before {
                    Ok(after.clone())
                } else {
                    self.before.restore(Some(&after))
                };
                let restoration_succeeded = restoration.is_ok() && root_attachment_error.is_none();
                let restoration_error = [
                    restoration.as_ref().err().map(|err| format!("{err:#}")),
                    root_attachment_error.as_ref().map(|err| format!("{err:#}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
                VerificationGuardOutcome::Mutation(Box::new(VerificationMutationOutcome {
                    evidence: verification_workspace_evidence(
                        Some(&self.before),
                        Some(&after),
                        restoration.as_ref().ok(),
                        String::new(),
                        restoration_error,
                    ),
                    restoration_succeeded,
                }))
            }
            Err(after_err) => {
                let restoration = self.before.restore(None);
                let restoration_error = [
                    restoration.as_ref().err().map(|err| format!("{err:#}")),
                    root_attachment_error.as_ref().map(|err| format!("{err:#}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
                VerificationGuardOutcome::Mutation(Box::new(VerificationMutationOutcome {
                    evidence: verification_workspace_evidence(
                        Some(&self.before),
                        None,
                        restoration.as_ref().ok(),
                        format!("{after_err:#}"),
                        restoration_error,
                    ),
                    restoration_succeeded: restoration.is_ok() && root_attachment_error.is_none(),
                }))
            }
        }
    }

    pub fn cache_worktree_digest(&self) -> Result<String> {
        cache_worktree_digest(&self.before.root)
    }
}

pub(crate) fn cache_worktree_digest(dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    #[cfg(target_os = "linux")]
    {
        let root = open_verification_root(dir)?;
        digest_confined_directory(&root, Path::new(""), true, &mut hasher)?;
    }
    #[cfg(not(target_os = "linux"))]
    digest_worktree_directory(dir, dir, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(not(target_os = "linux"))]
fn digest_worktree_directory(root: &Path, directory: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read verification cache directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| path_identity_bytes(&PathBuf::from(entry.file_name())));
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        if relative.components().next() == Some(Component::Normal(std::ffi::OsStr::new(".git"))) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect verification cache path {}", path.display()))?;
        digest_field(hasher, &path_identity_bytes(relative));
        #[cfg(unix)]
        {
            digest_field(hasher, &metadata.mode().to_be_bytes());
            digest_field(hasher, &metadata.len().to_be_bytes());
            digest_field(hasher, &metadata.mtime().to_be_bytes());
            digest_field(hasher, &metadata.mtime_nsec().to_be_bytes());
        }
        if metadata.file_type().is_symlink() {
            digest_field(hasher, b"symlink");
            #[cfg(unix)]
            digest_field(hasher, fs::read_link(&path)?.as_os_str().as_bytes());
            #[cfg(not(unix))]
            digest_field(hasher, fs::read_link(&path)?.to_string_lossy().as_bytes());
        } else if metadata.is_dir() {
            digest_field(hasher, b"directory");
            digest_worktree_directory(root, &path, hasher)?;
        } else if metadata.is_file() {
            digest_field(hasher, b"file");
            digest_field(hasher, &fs::read(&path)?);
        } else {
            digest_field(hasher, b"other");
        }
    }
    Ok(())
}

fn digest_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(target_os = "linux")]
fn open_verification_root(root: &Path) -> Result<File> {
    let root = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open pinned verification root");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_verification_entry(parent: &File, name: &OsStr) -> std::io::Result<File> {
    let name = std::ffi::CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn open_verification_directory(parent: &File, name: &OsStr) -> std::io::Result<File> {
    let name = std::ffi::CString::new(name.as_bytes()).expect("filesystem name omitted NUL");
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn verification_directory_names(directory: &File) -> Result<Vec<OsString>> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut names = fs::read_dir(&path)
        .with_context(|| format!("read pinned verification directory {}", path.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort_by_key(|name| name.as_bytes().to_vec());
    Ok(names)
}

#[cfg(target_os = "linux")]
fn read_verification_file(file: &File) -> Result<Vec<u8>> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let mut file = File::open(&path)
        .with_context(|| format!("open pinned verification file {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn read_verification_symlink(file: &File) -> Result<Vec<u8>> {
    let empty = c"";
    let mut bytes = vec![0_u8; 256];
    loop {
        let length = unsafe {
            libc::readlinkat(
                file.as_raw_fd(),
                empty.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if length < 0 {
            return Err(std::io::Error::last_os_error())
                .context("read pinned verification symlink");
        }
        let length = length as usize;
        if length < bytes.len() {
            bytes.truncate(length);
            return Ok(bytes);
        }
        bytes.resize(bytes.len() * 2, 0);
    }
}

#[cfg(all(test, target_os = "linux"))]
struct VerificationParentSubstitution {
    expected_path: Vec<u8>,
    expected_parent_identity: Vec<u8>,
    parent: PathBuf,
    parked: PathBuf,
    outside: PathBuf,
}

#[cfg(all(test, target_os = "linux"))]
static SUBSTITUTE_VERIFICATION_PARENT_DURING_CACHE_DIGEST: std::sync::Mutex<
    Option<VerificationParentSubstitution>,
> = std::sync::Mutex::new(None);

#[cfg(all(test, target_os = "linux"))]
pub fn substitute_next_verification_parent_during_cache_digest(
    relative: &Path,
    parent: &Path,
    parked: &Path,
    outside: &Path,
) {
    *SUBSTITUTE_VERIFICATION_PARENT_DURING_CACHE_DIGEST
        .lock()
        .unwrap() = Some(VerificationParentSubstitution {
        expected_path: path_identity_bytes(relative),
        expected_parent_identity: filesystem_object_identity_bytes(parent)
            .expect("verification cache substitution parent must exist"),
        parent: parent.to_path_buf(),
        parked: parked.to_path_buf(),
        outside: outside.to_path_buf(),
    });
}

#[cfg(target_os = "linux")]
fn digest_confined_directory(
    directory: &File,
    relative_directory: &Path,
    root: bool,
    hasher: &mut Sha256,
) -> Result<()> {
    for name in verification_directory_names(directory)? {
        if root && name == OsStr::new(".git") {
            continue;
        }
        let entry = open_verification_entry(directory, &name).with_context(|| {
            format!(
                "open verification cache path {}",
                relative_directory.join(&name).display()
            )
        })?;
        let metadata = entry.metadata()?;
        let relative = relative_directory.join(&name);
        #[cfg(test)]
        {
            let current_parent_identity = open_filesystem_object_identity_bytes(&entry)?;
            let substitution = {
                let mut substitution = SUBSTITUTE_VERIFICATION_PARENT_DURING_CACHE_DIGEST
                    .lock()
                    .unwrap();
                if substitution.as_ref().is_some_and(|substitution| {
                    substitution.expected_path == path_identity_bytes(&relative)
                        && substitution.expected_parent_identity == current_parent_identity
                }) {
                    substitution.take()
                } else {
                    None
                }
            };
            if let Some(substitution) = substitution {
                fs::rename(&substitution.parent, &substitution.parked)?;
                std::os::unix::fs::symlink(&substitution.outside, &substitution.parent)?;
            }
        }
        digest_field(hasher, &path_identity_bytes(&relative));
        digest_field(hasher, &metadata.mode().to_be_bytes());
        digest_field(hasher, &metadata.len().to_be_bytes());
        digest_field(hasher, &metadata.mtime().to_be_bytes());
        digest_field(hasher, &metadata.mtime_nsec().to_be_bytes());
        if metadata.file_type().is_symlink() {
            digest_field(hasher, b"symlink");
            digest_field(hasher, &read_verification_symlink(&entry)?);
        } else if metadata.is_dir() {
            digest_field(hasher, b"directory");
            digest_confined_directory(&entry, &relative, false, hasher)?;
        } else if metadata.is_file() {
            digest_field(hasher, b"file");
            digest_field(hasher, &read_verification_file(&entry)?);
        } else {
            digest_field(hasher, b"other");
        }
    }
    Ok(())
}

fn worktree_identity(
    context: &VerificationGitContext,
    dir: &Path,
) -> Result<(PathBuf, Vec<u8>, Vec<u8>, GitConfigurationSnapshot)> {
    let root_bytes = context.run_bytes(&["rev-parse", "--show-toplevel"])?;
    let root = path_from_git_bytes(strip_command_line_ending(&root_bytes))?
        .canonicalize()
        .with_context(|| format!("canonicalize verification worktree {}", dir.display()))?;
    let (root_identity, repository_identity) = context.identities()?;
    let git_configuration = context.configuration_snapshot()?;
    Ok((root, root_identity, repository_identity, git_configuration))
}

impl WorktreeSnapshot {
    #[cfg(test)]
    fn capture(dir: impl AsRef<Path>) -> Result<Self> {
        let root = dir.as_ref().canonicalize().with_context(|| {
            format!("canonicalize verification root {}", dir.as_ref().display())
        })?;
        Self::capture_from_root(&root)
    }

    fn capture_from_root(root: &Path) -> Result<Self> {
        let git_context = std::sync::Arc::new(VerificationGitContext::open(root)?);
        Self::capture_from_context(root, git_context, true)
    }

    fn capture_from_context(
        _root: &Path,
        git_context: std::sync::Arc<VerificationGitContext>,
        initial_capture: bool,
    ) -> Result<Self> {
        let root = git_context.operation_root_path()?;
        let (discovered_root, root_identity, repository_identity, current_git_configuration) =
            worktree_identity(&git_context, &root)?;
        let git_configuration = if initial_capture {
            let baseline = git_context.baseline_configuration();
            if current_git_configuration != baseline {
                bail!("Git configuration changed while verification administration was pinned");
            }
            baseline
        } else {
            current_git_configuration
        };
        if filesystem_object_identity_bytes(&root)?
            != filesystem_object_identity_bytes(&discovered_root)?
        {
            bail!("verification descriptor does not identify the Git worktree root");
        }
        let index_snapshot_directory =
            PublicationTemporaryDirectory::create(&std::env::temp_dir())?;
        let snapshot_index_file_path = index_snapshot_directory.path.join("snapshot-index");
        let current_index_before = git_context.current_index_bytes()?;
        let index_file_before = if initial_capture {
            let baseline = git_context.baseline_index_bytes();
            if current_index_before != baseline {
                bail!("Git index changed while verification administration was pinned");
            }
            baseline
        } else {
            current_index_before
        };
        fs::write(&snapshot_index_file_path, &index_file_before).with_context(|| {
            format!(
                "write private verification index {}",
                snapshot_index_file_path.display()
            )
        })?;
        let snapshot_index_file = File::open(&snapshot_index_file_path)?;
        make_pinned_directory_inheritable(&snapshot_index_file)?;
        #[cfg(unix)]
        let snapshot_index_path =
            PathBuf::from(format!("/proc/self/fd/{}", snapshot_index_file.as_raw_fd()));
        #[cfg(not(unix))]
        let snapshot_index_path = snapshot_index_file_path;
        let object_format = trim_ascii(
            &git_context
                .run_snapshot_bytes(&snapshot_index_path, &["rev-parse", "--show-object-format"])?,
        )
        .to_vec();
        let object_id_len = match object_format.as_slice() {
            b"sha1" => 20,
            b"sha256" => 32,
            _ => bail!(
                "unsupported Git object format {}",
                String::from_utf8_lossy(&object_format)
            ),
        };
        let index_semantics_before = semantic_index_bytes(&index_file_before, object_id_len)?;
        let head = trim_ascii(
            &git_context
                .run_snapshot_bytes(&snapshot_index_path, &["rev-parse", "--verify", "HEAD"])?,
        )
        .to_vec();
        let head_attachment = trim_ascii(&git_context.run_snapshot_bytes(
            &snapshot_index_path,
            &["rev-parse", "--symbolic-full-name", "HEAD"],
        )?)
        .to_vec();
        let index_entries = git_context
            .run_snapshot_bytes(&snapshot_index_path, &["ls-files", "--stage", "-v", "-z"])?;
        let tracked_filesystem = capture_tracked_filesystem(&root, &index_entries)?;
        let staged = git_context.run_snapshot_bytes(
            &snapshot_index_path,
            &[
                "diff",
                "--cached",
                "--raw",
                "-z",
                "--no-abbrev",
                "--find-renames",
            ],
        )?;
        let unstaged = capture_unstaged_state(
            &git_context,
            &root,
            &snapshot_index_path,
            &index_file_before,
            object_id_len,
        )?;
        let untracked = git_context.run_snapshot_bytes(
            &snapshot_index_path,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let (_, nonignored_empty_directories) =
            capture_nonignored_empty_directories(&git_context, &root, &snapshot_index_path)?;
        let head_after = trim_ascii(
            &git_context
                .run_snapshot_bytes(&snapshot_index_path, &["rev-parse", "--verify", "HEAD"])?,
        )
        .to_vec();
        let head_attachment_after = trim_ascii(&git_context.run_snapshot_bytes(
            &snapshot_index_path,
            &["rev-parse", "--symbolic-full-name", "HEAD"],
        )?)
        .to_vec();
        let index_entries_after = git_context
            .run_snapshot_bytes(&snapshot_index_path, &["ls-files", "--stage", "-v", "-z"])?;
        let tracked_filesystem_after = capture_tracked_filesystem(&root, &index_entries_after)?;
        let staged_after = git_context.run_snapshot_bytes(
            &snapshot_index_path,
            &[
                "diff",
                "--cached",
                "--raw",
                "-z",
                "--no-abbrev",
                "--find-renames",
            ],
        )?;
        let unstaged_after = capture_unstaged_state(
            &git_context,
            &root,
            &snapshot_index_path,
            &index_file_before,
            object_id_len,
        )?;
        let untracked_after = git_context.run_snapshot_bytes(
            &snapshot_index_path,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let (_, nonignored_empty_directories_after) =
            capture_nonignored_empty_directories(&git_context, &root, &snapshot_index_path)?;
        let index_file = git_context.current_index_bytes()?;
        let (
            _discovered_root_after,
            root_identity_after,
            repository_identity_after,
            git_configuration_after,
        ) = worktree_identity(&git_context, &root)?;
        let index_semantics = semantic_index_bytes(&index_file, object_id_len)?;
        if root_identity != root_identity_after
            || repository_identity != repository_identity_after
            || git_configuration != git_configuration_after
            || index_file_before != index_file
            || index_semantics_before != index_semantics
            || head != head_after
            || head_attachment != head_attachment_after
            || index_entries != index_entries_after
            || tracked_filesystem != tracked_filesystem_after
            || staged != staged_after
            || unstaged != unstaged_after
            || untracked != untracked_after
            || nonignored_empty_directories != nonignored_empty_directories_after
        {
            bail!(
                "verification Git or tracked-filesystem state changed while its snapshot was being captured (root={}, repository={}, config={}, raw_index={}, index={} at {:?} lengths {}/{}, head={}, attachment={}, entries={}, filesystem={}, staged={}, unstaged={}, untracked={}, empty_directories={})",
                root_identity != root_identity_after,
                repository_identity != repository_identity_after,
                git_configuration != git_configuration_after,
                index_file_before != index_file,
                index_semantics_before != index_semantics,
                index_semantics_before
                    .iter()
                    .zip(&index_semantics)
                    .position(|(before, after)| before != after),
                index_semantics_before.len(),
                index_semantics.len(),
                head != head_after,
                head_attachment != head_attachment_after,
                index_entries != index_entries_after,
                tracked_filesystem != tracked_filesystem_after,
                staged != staged_after,
                unstaged != unstaged_after,
                untracked != untracked_after,
                nonignored_empty_directories != nonignored_empty_directories_after
            );
        }
        Ok(Self {
            git_context,
            root,
            root_identity,
            repository_identity,
            git_configuration,
            head,
            head_attachment,
            index_entries,
            index_file: index_file_before,
            index_semantics: index_semantics_before,
            tracked_filesystem,
            staged,
            unstaged,
            untracked,
            nonignored_empty_directories,
        })
    }

    fn recapture(&self) -> Result<Self> {
        Self::capture_from_context(&self.root, self.git_context.clone(), false)
    }

    fn is_clean(&self) -> bool {
        self.staged.is_empty()
            && self.unstaged.is_empty()
            && self.untracked.is_empty()
            && self.nonignored_empty_directories.is_empty()
            && !index_has_hidden_worktree_flags(&self.index_entries)
    }

    fn digest(&self) -> String {
        let mut digest = Sha256::new();
        for bytes in [
            self.root_identity.clone(),
            self.repository_identity.clone(),
            git_configuration_digest(&self.git_configuration),
            self.head.clone(),
            self.head_attachment.clone(),
            self.index_entries.clone(),
            self.index_file.clone(),
            self.index_semantics.clone(),
            tracked_filesystem_digest(&self.tracked_filesystem),
            self.staged.clone(),
            self.unstaged.clone(),
            self.untracked.clone(),
            self.nonignored_empty_directories.clone(),
        ] {
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
        hex::encode(digest.finalize())
    }

    fn evidence(&self) -> Result<GitWorktreeSnapshotEvidence> {
        Ok(GitWorktreeSnapshotEvidence {
            digest: self.digest(),
            repository_id: hex::encode(Sha256::digest(&self.repository_identity)),
            worktree_id: hex::encode(Sha256::digest(&self.root_identity)),
            head_sha: ascii_sha(&self.head)?.to_string(),
            head_attachment: ascii_git_value(&self.head_attachment, "HEAD attachment")?.to_string(),
            index_digest: hex::encode(Sha256::digest(&self.index_file)),
            tracked_filesystem_digest: hex::encode(tracked_filesystem_digest(
                &self.tracked_filesystem,
            )),
            staged: raw_diff_changes(&self.staged)?
                .into_iter()
                .map(|change| change.evidence())
                .collect(),
            unstaged: raw_diff_changes(&self.unstaged)?
                .into_iter()
                .map(|change| change.evidence())
                .collect(),
            untracked_path_bytes_hex: nul_paths(&self.untracked)
                .into_iter()
                .map(hex::encode)
                .collect(),
            nonignored_empty_directory_path_bytes_hex: nul_paths(
                &self.nonignored_empty_directories,
            )
            .into_iter()
            .map(hex::encode)
            .collect(),
        })
    }

    fn restore(&self, after: Option<&Self>) -> Result<Self> {
        self.restore_inner(after, None, true, None)
    }

    fn restore_publication_side_effects(
        &self,
        after: Option<&Self>,
        entries: &[CapturedPublicationEntry],
        restore_head: bool,
        index_lock: &GitIndexLock,
    ) -> Result<Self> {
        let authorized_paths = entries
            .iter()
            .map(|entry| entry.path_bytes.clone())
            .collect::<BTreeSet<_>>();
        let unauthorized_unstaged = raw_diff_paths(&self.unstaged)?
            .into_iter()
            .filter(|path| !authorized_paths.contains(path))
            .map(hex::encode)
            .collect::<Vec<_>>();
        let unauthorized_untracked = nul_paths(&self.untracked)
            .into_iter()
            .filter(|path| !authorized_paths.contains(path))
            .map(hex::encode)
            .collect::<Vec<_>>();
        if !self.staged.is_empty()
            || !unauthorized_unstaged.is_empty()
            || !unauthorized_untracked.is_empty()
            || index_has_hidden_worktree_flags(&self.index_entries)
        {
            bail!(
                "refusing publication restoration over unauthorized pre-publication state (staged={}, unstaged={}, untracked={}, hidden_index_flags={})",
                !self.staged.is_empty(),
                unauthorized_unstaged.join(","),
                unauthorized_untracked.join(","),
                index_has_hidden_worktree_flags(&self.index_entries)
            );
        }
        self.restore_inner(after, Some(entries), restore_head, Some(index_lock))
    }

    fn restore_inner(
        &self,
        after: Option<&Self>,
        publication_entries: Option<&[CapturedPublicationEntry]>,
        restore_head: bool,
        held_index_lock: Option<&GitIndexLock>,
    ) -> Result<Self> {
        if publication_entries.is_none() && !self.is_clean() {
            bail!("refusing to restore verification over a dirty pre-command snapshot");
        }
        let (
            _current_root,
            current_root_identity,
            current_repository_identity,
            current_git_configuration,
        ) = match worktree_identity(&self.git_context, &self.root) {
            Ok(identity) => identity,
            Err(identity_err) => {
                let tracked_restoration =
                    restore_tracked_filesystem(&self.root, &self.tracked_filesystem);
                return match tracked_restoration {
                    Ok(()) => Err(identity_err.context(
                        "verification administration detached; descriptor-confined tracked files were restored before refusing Git-state restoration",
                    )),
                    Err(restoration_err) => Err(identity_err.context(format!(
                        "verification administration detached and descriptor-confined tracked-file restoration also failed: {restoration_err:#}"
                    ))),
                };
            }
        };
        if self.root_identity != current_root_identity
            || self.repository_identity != current_repository_identity
        {
            bail!("verification worktree identity changed; refusing restoration");
        }
        if let Some(after) = after
            && (self.root_identity != after.root_identity
                || self.repository_identity != after.repository_identity)
        {
            bail!("verification worktree identity changed; refusing restoration");
        }

        let observed_git_configuration = after
            .map(|snapshot| &snapshot.git_configuration)
            .unwrap_or(&current_git_configuration);
        if self.git_configuration != *observed_git_configuration {
            self.git_context
                .restore_configuration(&self.git_configuration, observed_git_configuration)?;
        }

        if let Some(index_lock) = held_index_lock {
            index_lock
                .replace_index(&self.index_file)
                .context("restore exact pre-publication git index")?;
        } else {
            let index_lock = self
                .git_context
                .acquire_index_lock()
                .context("lock git index for exact verification restoration")?;
            index_lock
                .replace_index(&self.index_file)
                .context("restore exact pre-verification git index")?;
            drop(index_lock);
        }

        let current_head =
            trim_ascii(
                &self
                    .git_context
                    .run_bytes(&["rev-parse", "--verify", "HEAD"])?,
            )
            .to_vec();
        let current_attachment = trim_ascii(&self.git_context.run_bytes(&[
            "rev-parse",
            "--symbolic-full-name",
            "HEAD",
        ])?)
        .to_vec();
        if restore_head && (self.head != current_head || self.head_attachment != current_attachment)
        {
            restore_head_attachment(self)?;
        }

        restore_tracked_filesystem(&self.root, &self.tracked_filesystem)?;
        let baseline_untracked = nul_paths(&self.untracked)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let baseline_empty_directories = nul_paths(&self.nonignored_empty_directories)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let index_path = self.git_context.current_index_path()?;
        let (directory_count, _) =
            capture_nonignored_empty_directories(&self.git_context, &self.root, &index_path)?;
        let cleanup_pass_limit = nul_paths(
            &self
                .git_context
                .run_bytes(&["ls-files", "--others", "-z"])?,
        )
        .len()
        .saturating_add(directory_count)
        .saturating_add(1);
        let mut cleanup_complete = false;
        for _ in 0..cleanup_pass_limit {
            let mut removable_paths = nul_paths(&self.git_context.run_bytes(&[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
            ])?)
            .into_iter()
            .filter(|path| !baseline_untracked.contains(path))
            .collect::<Vec<_>>();
            let (_, current_empty_directories) =
                capture_nonignored_empty_directories(&self.git_context, &self.root, &index_path)?;
            removable_paths.extend(
                nul_paths(&current_empty_directories)
                    .into_iter()
                    .filter(|path| !baseline_empty_directories.contains(path)),
            );
            if removable_paths.is_empty() {
                cleanup_complete = true;
                break;
            }
            removable_paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
            removable_paths.dedup();
            for raw_path in removable_paths {
                remove_untracked_path(&self.root, &raw_path)?;
            }
        }
        if !cleanup_complete {
            bail!("verification untracked cleanup did not reach a stable original-ignore state");
        }
        if let Some(entries) = publication_entries {
            restore_untracked_publication_entries(&self.root, &self.tracked_filesystem, entries)?;
        }
        let mut restored = self.recapture()?;
        if self.matches_raw_state_except_unstaged_stat_cache(&restored) {
            // Replacing a mutated inode necessarily changes ctime/inode stat-cache fields that
            // cannot be restored. Exact index bytes plus exact descriptor-captured worktree bytes,
            // types, modes, and timestamps prove the same semantic state as the clean baseline
            // without invoking a configured clean filter to re-hash it.
            restored.unstaged.clone_from(&self.unstaged);
        }
        let restoration_matches = if restore_head {
            &restored == self
        } else {
            self.matches_publication_side_effect_state(&restored)
        };
        if !restoration_matches {
            bail!(
                "verification worktree restoration did not reproduce snapshot {} (restored {})",
                self.digest(),
                restored.digest()
            );
        }
        Ok(restored)
    }
}

#[cfg(target_os = "linux")]
fn verification_parent_and_name(root: &File, relative: &Path) -> Result<Option<(File, OsString)>> {
    let components = relative.components().collect::<Vec<_>>();
    let Some(Component::Normal(leaf)) = components.last() else {
        bail!("git returned an empty or unsafe tracked path");
    };
    let mut directory = root.try_clone()?;
    for component in &components[..components.len() - 1] {
        let Component::Normal(name) = component else {
            bail!("git returned unsafe tracked path component");
        };
        let next = match open_verification_entry(&directory, name) {
            Ok(next) => next,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("open tracked verification parent {}", relative.display())
                });
            }
        };
        if !next.metadata()?.is_dir() {
            bail!(
                "tracked verification path has a non-directory parent: {}",
                relative.display()
            );
        }
        directory = next;
    }
    Ok(Some((directory, leaf.to_os_string())))
}

#[cfg(all(test, target_os = "linux"))]
static SUBSTITUTE_VERIFICATION_PARENT_DURING_CAPTURE: std::sync::Mutex<
    Option<VerificationParentSubstitution>,
> = std::sync::Mutex::new(None);

#[cfg(all(test, target_os = "linux"))]
pub fn substitute_next_verification_parent_during_capture(
    tracked_path: &[u8],
    parent: &Path,
    parked: &Path,
    outside: &Path,
) {
    *SUBSTITUTE_VERIFICATION_PARENT_DURING_CAPTURE
        .lock()
        .unwrap() = Some(VerificationParentSubstitution {
        expected_path: tracked_path.to_vec(),
        expected_parent_identity: filesystem_object_identity_bytes(parent)
            .expect("verification capture substitution parent must exist"),
        parent: parent.to_path_buf(),
        parked: parked.to_path_buf(),
        outside: outside.to_path_buf(),
    });
}

#[cfg(target_os = "linux")]
fn capture_tracked_filesystem(
    root: &Path,
    index_entries: &[u8],
) -> Result<BTreeMap<Vec<u8>, TrackedFilesystemEntry>> {
    let root_directory = open_verification_root(root)?;
    let mut captured = BTreeMap::new();
    for (path_bytes, index_records) in index_entries_by_path(index_entries)? {
        let relative = safe_relative_git_path(&path_bytes)?;
        let is_gitlink = index_records_are_gitlink(&index_records)?;
        let Some((parent, name)) = verification_parent_and_name(&root_directory, &relative)? else {
            captured.insert(
                path_bytes,
                TrackedFilesystemEntry {
                    kind: TrackedFilesystemKind::Missing,
                    mode: 0,
                    mtime_seconds: 0,
                    mtime_nanoseconds: 0,
                },
            );
            continue;
        };
        #[cfg(test)]
        {
            let current_parent_identity = open_filesystem_object_identity_bytes(&parent)?;
            let substitution = {
                let mut substitution = SUBSTITUTE_VERIFICATION_PARENT_DURING_CAPTURE
                    .lock()
                    .unwrap();
                if substitution.as_ref().is_some_and(|substitution| {
                    substitution.expected_path.as_slice() == path_bytes.as_slice()
                        && substitution.expected_parent_identity == current_parent_identity
                }) {
                    substitution.take()
                } else {
                    None
                }
            };
            if let Some(substitution) = substitution {
                fs::rename(&substitution.parent, &substitution.parked)?;
                std::os::unix::fs::symlink(&substitution.outside, &substitution.parent)?;
            }
        }
        let entry = match open_verification_entry(&parent, &name) {
            Ok(file) => {
                let metadata = file.metadata()?;
                if is_gitlink {
                    bail!(
                        "initialized Git submodule cannot be restored in isolation during verification: {}",
                        relative.display()
                    );
                }
                if metadata.is_file() && metadata.nlink() > 1 {
                    bail!(
                        "hard-linked tracked verification path cannot be restored in isolation: {}",
                        relative.display()
                    );
                }
                let kind = if metadata.file_type().is_symlink() {
                    TrackedFilesystemKind::Symlink(read_verification_symlink(&file)?)
                } else if metadata.is_file() {
                    TrackedFilesystemKind::File(read_verification_file(&file)?)
                } else if metadata.is_dir() {
                    TrackedFilesystemKind::Directory
                } else {
                    bail!(
                        "unsupported tracked verification path type: {}",
                        relative.display()
                    );
                };
                TrackedFilesystemEntry {
                    kind,
                    mode: metadata.mode(),
                    mtime_seconds: metadata.mtime(),
                    mtime_nanoseconds: metadata.mtime_nsec(),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => TrackedFilesystemEntry {
                kind: TrackedFilesystemKind::Missing,
                mode: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
            },
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("inspect tracked verification path {}", relative.display())
                });
            }
        };
        captured.insert(path_bytes, entry);
    }
    Ok(captured)
}

#[cfg(not(target_os = "linux"))]
fn capture_tracked_filesystem(
    root: &Path,
    index_entries: &[u8],
) -> Result<BTreeMap<Vec<u8>, TrackedFilesystemEntry>> {
    let mut captured = BTreeMap::new();
    for (path_bytes, index_records) in index_entries_by_path(index_entries)? {
        let relative = safe_relative_git_path(&path_bytes)?;
        let is_gitlink = index_records_are_gitlink(&index_records)?;
        verify_tracked_path_parents(root, &relative)?;
        let absolute = root.join(&relative);
        let entry = match fs::symlink_metadata(&absolute) {
            Ok(metadata) => {
                if is_gitlink {
                    bail!(
                        "initialized Git submodule cannot be restored in isolation during verification: {}",
                        absolute.display()
                    );
                }
                #[cfg(unix)]
                if metadata.is_file() && metadata.nlink() > 1 {
                    bail!(
                        "hard-linked tracked verification path cannot be restored in isolation: {}",
                        absolute.display()
                    );
                }
                let kind = if metadata.file_type().is_symlink() {
                    #[cfg(unix)]
                    let target = fs::read_link(&absolute)?.as_os_str().as_bytes().to_vec();
                    #[cfg(not(unix))]
                    let target = fs::read_link(&absolute)?
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec();
                    TrackedFilesystemKind::Symlink(target)
                } else if metadata.is_file() {
                    TrackedFilesystemKind::File(fs::read(&absolute).with_context(|| {
                        format!("read tracked verification path {}", absolute.display())
                    })?)
                } else if metadata.is_dir() {
                    TrackedFilesystemKind::Directory
                } else {
                    bail!(
                        "unsupported tracked verification path type: {}",
                        absolute.display()
                    );
                };
                TrackedFilesystemEntry {
                    kind,
                    mode: filesystem_mode(&metadata),
                    mtime_seconds: filesystem_mtime_seconds(&metadata),
                    mtime_nanoseconds: filesystem_mtime_nanoseconds(&metadata),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => TrackedFilesystemEntry {
                kind: TrackedFilesystemKind::Missing,
                mode: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
            },
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("inspect tracked verification path {}", absolute.display())
                });
            }
        };
        captured.insert(path_bytes, entry);
    }
    Ok(captured)
}

fn publication_ambient_paths(repository: &PinnedGitRepository) -> Result<Vec<Vec<u8>>> {
    repository.ensure_attached()?;
    let mut paths = nul_paths(&repository.run_bytes(&["ls-files", "--others", "-z"])?);
    let mut directories = Vec::new();
    let mut empty = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let root = open_verification_root(repository.root.operation_path())?;
        collect_verification_directories(&root, Path::new(""), true, &mut directories, &mut empty)?;
    }
    #[cfg(not(target_os = "linux"))]
    collect_verification_directories(
        repository.root.operation_path(),
        Path::new(""),
        true,
        &mut directories,
        &mut empty,
    )?;
    paths.extend(directories);
    paths.sort();
    paths.dedup();
    repository.ensure_attached()?;
    Ok(paths)
}

struct CapturedPublicationAmbient {
    entries: BTreeMap<Vec<u8>, PublicationFilesystemEntry>,
    regular_file_groups: BTreeMap<Vec<u8>, PublicationFilesystemEntry>,
}

#[cfg(target_os = "linux")]
fn capture_publication_ambient_filesystem(
    repository: &PinnedGitRepository,
) -> Result<CapturedPublicationAmbient> {
    let root = open_verification_root(repository.root.operation_path())?;
    let mut entries = BTreeMap::new();
    for path_bytes in publication_ambient_paths(repository)? {
        let relative = safe_relative_git_path(&path_bytes)?;
        let Some((parent, name)) = verification_parent_and_name(&root, &relative)? else {
            entries.insert(path_bytes, missing_publication_filesystem_entry());
            continue;
        };
        let entry = match open_verification_entry(&parent, &name) {
            Ok(file) => {
                let metadata = file.metadata()?;
                let kind = if metadata.file_type().is_symlink() {
                    PublicationFilesystemKind::Symlink(read_verification_symlink(&file)?)
                } else if metadata.is_file() {
                    PublicationFilesystemKind::File {
                        size: metadata.len(),
                    }
                } else if metadata.is_dir() {
                    PublicationFilesystemKind::Directory
                } else {
                    bail!(
                        "unsupported ignored publication path type: {}",
                        relative.display()
                    );
                };
                PublicationFilesystemEntry {
                    kind,
                    mode: metadata.mode(),
                    mtime_seconds: metadata.mtime(),
                    mtime_nanoseconds: metadata.mtime_nsec(),
                    ctime_seconds: metadata.ctime(),
                    ctime_nanoseconds: metadata.ctime_nsec(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    link_count: metadata.nlink(),
                    hardlink_group: Vec::new(),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                missing_publication_filesystem_entry()
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("inspect ignored publication path {}", relative.display())
                });
            }
        };
        entries.insert(path_bytes, entry);
    }

    let mut links = BTreeMap::<(u64, u64), Vec<Vec<u8>>>::new();
    for (path, entry) in &entries {
        if matches!(
            entry.kind,
            PublicationFilesystemKind::File { .. } | PublicationFilesystemKind::Symlink(_)
        ) {
            links
                .entry((entry.device, entry.inode))
                .or_default()
                .push(path.clone());
        }
    }
    let mut regular_file_groups = BTreeMap::new();
    for paths in links.values_mut() {
        paths.sort();
        let canonical = paths
            .first()
            .expect("publication hard-link group cannot be empty")
            .clone();
        let expected_links = entries
            .get(&canonical)
            .expect("publication hard-link canonical path exists")
            .link_count;
        if expected_links != paths.len() as u64 {
            bail!(
                "ignored publication hard-link group escapes the captured worktree at {}",
                safe_relative_git_path(&canonical)?.display()
            );
        }
        for path in paths {
            let entry = entries
                .get_mut(path)
                .expect("publication hard-link path exists");
            entry.hardlink_group.clone_from(&canonical);
        }
        let entry = entries
            .get(&canonical)
            .expect("publication hard-link canonical entry exists");
        if matches!(entry.kind, PublicationFilesystemKind::File { .. }) {
            regular_file_groups.insert(canonical, entry.clone());
        }
    }
    repository.ensure_attached()?;
    Ok(CapturedPublicationAmbient {
        entries,
        regular_file_groups,
    })
}

#[cfg(not(target_os = "linux"))]
fn capture_publication_ambient_filesystem(
    _repository: &PinnedGitRepository,
) -> Result<CapturedPublicationAmbient> {
    bail!("completion publication ambient hook supervision requires Linux")
}

fn missing_publication_filesystem_entry() -> PublicationFilesystemEntry {
    PublicationFilesystemEntry {
        kind: PublicationFilesystemKind::Missing,
        mode: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        device: 0,
        inode: 0,
        link_count: 0,
        hardlink_group: Vec::new(),
    }
}

fn publication_ambient_semantically_matches(
    expected: &BTreeMap<Vec<u8>, PublicationFilesystemEntry>,
    actual: &BTreeMap<Vec<u8>, PublicationFilesystemEntry>,
) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected.iter().all(|(path, expected)| {
        actual.get(path).is_some_and(|actual| {
            expected.kind == actual.kind
                && expected.mode == actual.mode
                && expected.mtime_seconds == actual.mtime_seconds
                && expected.mtime_nanoseconds == actual.mtime_nanoseconds
                && expected.hardlink_group == actual.hardlink_group
                && expected.link_count == actual.link_count
        })
    })
}

#[cfg(all(test, target_os = "linux"))]
static FAIL_PUBLICATION_LAZY_BACKUP: std::sync::Mutex<Vec<(u64, u64)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(all(test, target_os = "linux"))]
static FAIL_PUBLICATION_MEMORY_BACKUP: std::sync::Mutex<Vec<(u64, u64)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fail_next_publication_lazy_backup(path: &Path) {
    let metadata = fs::metadata(path).expect("lazy-backup fault target must exist");
    FAIL_PUBLICATION_LAZY_BACKUP
        .lock()
        .unwrap()
        .push((metadata.dev(), metadata.ino()));
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fail_next_publication_memory_backup(path: &Path) {
    let metadata = fs::metadata(path).expect("memory-backup fault target must exist");
    FAIL_PUBLICATION_MEMORY_BACKUP
        .lock()
        .unwrap()
        .push((metadata.dev(), metadata.ino()));
}

#[cfg(all(test, target_os = "linux"))]
fn take_publication_backup_failure(
    failures: &std::sync::Mutex<Vec<(u64, u64)>>,
    file: &File,
) -> bool {
    let metadata = file.metadata().expect("lazy-backup fault target metadata");
    let identity = (metadata.dev(), metadata.ino());
    let mut failures = failures.lock().unwrap();
    let Some(index) = failures.iter().position(|candidate| *candidate == identity) else {
        return false;
    };
    failures.remove(index);
    true
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct PublicationLeaseShared {
    sources: BTreeMap<Vec<u8>, File>,
    backups: BTreeMap<Vec<u8>, File>,
    errors: BTreeMap<Vec<u8>, String>,
    monitor_error: Option<String>,
    monitor_unavailable: bool,
    active_cancellation: Option<CancellationToken>,
}

#[cfg(target_os = "linux")]
struct PublicationLeaseMonitor {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shared: std::sync::Arc<Mutex<PublicationLeaseShared>>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
    failure_reported: bool,
    retain_until_process_exit: bool,
    _backup_temporary_directory: PublicationTemporaryDirectory,
}

#[cfg(target_os = "linux")]
impl PublicationLeaseMonitor {
    fn start(root: &Path, groups: &BTreeMap<Vec<u8>, PublicationFilesystemEntry>) -> Result<Self> {
        let backup_temporary_directory =
            PublicationTemporaryDirectory::create(&std::env::temp_dir())?;
        let backup_directory = open_publication_root(&backup_temporary_directory.path)
            .context("pin ignored publication lease backup directory")?;
        let thread_backup_directory = backup_directory.try_clone()?;
        let root_directory = open_verification_root(root)?;
        let mut files = Vec::new();
        for (path, expected) in groups {
            let relative = safe_relative_git_path(path)?;
            let Some((parent, name)) = verification_parent_and_name(&root_directory, &relative)?
            else {
                bail!(
                    "ignored publication file disappeared before lease acquisition: {}",
                    relative.display()
                );
            };
            let name_c = verification_c_name(&name)?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "open ignored publication file for lease: {}",
                        relative.display()
                    )
                });
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.dev() != expected.device
                || metadata.ino() != expected.inode
                || metadata.nlink() != expected.link_count
                || metadata.len()
                    != match expected.kind {
                        PublicationFilesystemKind::File { size } => size,
                        _ => 0,
                    }
            {
                bail!(
                    "ignored publication file changed before lease acquisition: {}",
                    relative.display()
                );
            }
            files.push((path.clone(), file));
        }

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shared = std::sync::Arc::new(Mutex::new(PublicationLeaseShared::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread_stop = stop.clone();
        let thread_shared = shared.clone();
        let runtime_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("khazad-publication-leases".to_string())
            .spawn(move || {
                let result = run_publication_lease_monitor(
                    files,
                    thread_backup_directory,
                    thread_stop,
                    thread_shared,
                    ready_tx,
                );
                if let Err(err) = &result {
                    runtime_shared
                        .lock()
                        .expect("publication lease state lock")
                        .monitor_error = Some(format!("{err:#}"));
                }
                result
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                shared,
                thread: Some(thread),
                failure_reported: false,
                retain_until_process_exit: false,
                _backup_temporary_directory: backup_temporary_directory,
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                bail!("acquire ignored publication write leases: {err}");
            }
            Err(err) => {
                let joined = thread.join();
                bail!("ignored publication lease monitor exited during startup: {err}; {joined:?}");
            }
        }
    }

    fn changed_paths(&self) -> BTreeSet<Vec<u8>> {
        self.shared
            .lock()
            .expect("publication lease state lock")
            .backups
            .keys()
            .cloned()
            .collect()
    }

    fn backup_failed(&self, path: &[u8]) -> bool {
        self.shared
            .lock()
            .expect("publication lease state lock")
            .errors
            .contains_key(path)
    }

    fn ensure_healthy(&self) -> Result<()> {
        let shared = self.shared.lock().expect("publication lease state lock");
        if let Some(err) = &shared.monitor_error {
            bail!("ignored publication lease monitor failed: {err}");
        }
        Ok(())
    }

    fn source_file(&self, path: &[u8]) -> Result<File> {
        let shared = self.shared.lock().expect("publication lease state lock");
        if let Some(backup) = shared.backups.get(path) {
            let mut backup = backup
                .try_clone()
                .context("duplicate descriptor-confined ignored publication lease backup")?;
            backup.seek(SeekFrom::Start(0))?;
            return Ok(backup);
        }
        if let Some(source) = shared.sources.get(path) {
            let mut source = source
                .try_clone()
                .context("duplicate descriptor-pinned ignored publication file for restoration")?;
            source.seek(SeekFrom::Start(0))?;
            return Ok(source);
        }
        if let Some(err) = &shared.monitor_error {
            bail!("ignored publication lease monitor failed: {err}");
        }
        if let Some(err) = shared.errors.get(path) {
            bail!("could not preserve ignored publication file before hook mutation: {err}");
        }
        bail!("ignored publication restoration source was missing")
    }

    fn begin_transaction(
        &self,
        cancellation: &CancellationToken,
        allow_failed_lease: bool,
    ) -> Result<bool> {
        let mut shared = self
            .shared
            .lock()
            .map_err(|_| anyhow::anyhow!("publication lease state lock was poisoned"))?;
        if shared.active_cancellation.is_some() {
            bail!("ignored publication lease monitor already had an active ref transaction");
        }
        shared.active_cancellation = Some(cancellation.clone());
        let failed_lease = shared.monitor_error.is_some() || !shared.errors.is_empty();
        if shared.monitor_unavailable || (!allow_failed_lease && failed_lease) {
            cancellation.cancel();
        }
        Ok(failed_lease)
    }

    fn finish_transaction(&mut self, transaction: Result<()>) -> Result<()> {
        let monitor = {
            let mut shared = self
                .shared
                .lock()
                .map_err(|_| anyhow::anyhow!("publication lease state lock was poisoned"))?;
            if shared.active_cancellation.take().is_none() {
                Err(anyhow::anyhow!(
                    "ignored publication lease monitor had no active ref transaction"
                ))
            } else if self.failure_reported {
                Ok(())
            } else if let Some(err) = &shared.monitor_error {
                Err(anyhow::anyhow!(
                    "ignored publication lease monitor failed: {err}"
                ))
            } else {
                Ok(())
            }
        };
        if monitor.is_err() {
            self.failure_reported = true;
        }
        combine_ref_transaction_and_lease_monitor(transaction, monitor)
    }

    fn release_if_failed(&mut self) -> Result<()> {
        let failed = {
            let shared = self
                .shared
                .lock()
                .map_err(|_| anyhow::anyhow!("publication lease state lock was poisoned"))?;
            if shared.active_cancellation.is_some() {
                bail!(
                    "cannot release ignored publication leases while a ref transaction is active"
                );
            }
            shared.monitor_error.is_some() || !shared.errors.is_empty()
        };
        if !failed {
            return Ok(());
        }
        self.stop_and_join()?;
        if self.failure_reported {
            return Ok(());
        }
        self.failure_reported = true;
        self.ensure_healthy()
    }

    fn stop_and_join(&mut self) -> Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(result) => result?,
                Err(_) => bail!("ignored publication lease monitor thread panicked"),
            }
        }
        Ok(())
    }
}

fn ref_transaction_supervision_failed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ShellCommandError>()
            .is_some_and(|error| error.kind() == ShellFailureKind::Supervision)
    })
}

fn combine_ref_transaction_and_lease_monitor(
    transaction: Result<()>,
    monitor: Result<()>,
) -> Result<()> {
    match (transaction, monitor) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(transaction_err), Ok(())) => Err(transaction_err),
        (Ok(()), Err(monitor_err)) => Err(monitor_err),
        (Err(transaction_err), Err(monitor_err)) => Err(transaction_err).context(format!(
            "ignored publication lease monitor also failed: {monitor_err:#}"
        )),
    }
}

#[cfg(target_os = "linux")]
impl Drop for PublicationLeaseMonitor {
    fn drop(&mut self) {
        if !self.retain_until_process_exit {
            let _ = self.stop_and_join();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod verification_git_context_tests {
    use super::*;

    fn wait_for_marker(marker: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for verification test marker"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn verification_restoration_disables_reference_transaction_hooks() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "-b", "main"]).unwrap();
        run(repo.path(), &["config", "user.name", "Test"]).unwrap();
        run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "first\n").unwrap();
        commit_all(repo.path(), "first").unwrap();
        let previous_head = run(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        commit_all(repo.path(), "baseline").unwrap();
        let baseline_head = run(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        let invoked = outside.path().join("hook-invoked");
        let delayed = repo.path().join("delayed-hook-write");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\n(sleep 0.2; printf escaped > '{}') </dev/null >/dev/null 2>&1 &\nexit 0\n",
                invoked.display(),
                delayed.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let snapshot = WorktreeSnapshot::capture(repo.path()).unwrap();

        fs::write(
            repo.path().join(".git/refs/heads/main"),
            format!("{previous_head}\n"),
        )
        .unwrap();
        let restored = snapshot.restore(None).unwrap();
        assert_eq!(restored.head, baseline_head.as_bytes());
        std::thread::sleep(std::time::Duration::from_millis(400));

        assert!(
            !invoked.exists(),
            "verification restoration invoked a Git hook"
        );
        assert!(
            !delayed.exists(),
            "a verification-restoration hook descendant survived restoration"
        );
    }

    #[test]
    fn verification_restoration_never_writes_substituted_git_administration() {
        let repo = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "-b", "main"]).unwrap();
        run(repo.path(), &["config", "user.name", "Test"]).unwrap();
        run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        commit_all(repo.path(), "base").unwrap();
        let snapshot = WorktreeSnapshot::capture(repo.path()).unwrap();
        let original_admin = repo.path().join(".git-original");
        fs::rename(repo.path().join(".git"), &original_admin).unwrap();
        run(repo.path(), &["init", "-b", "replacement"]).unwrap();
        run(repo.path(), &["config", "user.name", "Replacement"]).unwrap();
        run(
            repo.path(),
            &["config", "user.email", "replacement@example.com"],
        )
        .unwrap();
        commit_all(repo.path(), "replacement").unwrap();
        let replacement_index = fs::read(repo.path().join(".git/index")).unwrap();
        fs::write(repo.path().join("tracked.txt"), "mutated\n").unwrap();

        let err = snapshot.restore(None).unwrap_err();

        assert!(
            format!("{err:#}").contains("administrative directory changed"),
            "{err:#}"
        );
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            replacement_index
        );
        fs::remove_dir_all(repo.path().join(".git")).unwrap();
        fs::rename(original_admin, repo.path().join(".git")).unwrap();
    }

    #[test]
    fn verification_index_lock_race_never_writes_replacement_administration() {
        let repo = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "-b", "main"]).unwrap();
        run(repo.path(), &["config", "user.name", "Test"]).unwrap();
        run(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        fs::write(repo.path().join("tracked.txt"), "baseline\n").unwrap();
        commit_all(repo.path(), "base").unwrap();
        let snapshot = WorktreeSnapshot::capture(repo.path()).unwrap();
        fs::write(repo.path().join("tracked.txt"), "mutated\n").unwrap();
        let rendezvous = tempfile::tempdir().unwrap();
        let marker = rendezvous.path().join("paused");
        let release = rendezvous.path().join("release");
        pause_next_verification_before_index_lock(repo.path(), &marker, &release);
        let restore = std::thread::spawn(move || snapshot.restore(None));
        wait_for_marker(&marker);

        let original_admin = repo.path().join(".git-original");
        fs::rename(repo.path().join(".git"), &original_admin).unwrap();
        run(repo.path(), &["init", "-b", "replacement"]).unwrap();
        run(repo.path(), &["config", "user.name", "Replacement"]).unwrap();
        run(
            repo.path(),
            &["config", "user.email", "replacement@example.com"],
        )
        .unwrap();
        commit_all(repo.path(), "replacement").unwrap();
        let replacement_index = fs::read(repo.path().join(".git/index")).unwrap();
        fs::write(&release, b"release\n").unwrap();

        let err = restore.join().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("worktree root changed")
                || format!("{err:#}").contains("administrative directory changed"),
            "{err:#}"
        );
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            replacement_index
        );
        fs::remove_dir_all(repo.path().join(".git")).unwrap();
        fs::rename(original_admin, repo.path().join(".git")).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod publication_lease_monitor_tests {
    use super::*;

    #[test]
    fn reflog_cleanup_context_preserves_typed_supervision_failure() {
        let transaction_err = anyhow::Error::new(ShellCommandError::new(
            ShellFailureKind::Supervision,
            "authenticated supervisor cleanup failed",
        ));
        let combined = transaction_err.context(
            "confined completion publication ref transaction failed; operation-created reflog cleanup also failed: injected cleanup failure",
        );

        assert!(ref_transaction_supervision_failed(&combined));
    }

    #[test]
    fn backup_failure_before_forward_transaction_cancels_before_launch() {
        let repo = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "-b", "main"]).unwrap();
        run(
            repo.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        )
        .unwrap();
        let repository = PinnedGitRepository::open(repo.path()).unwrap();
        let mut snapshot = PublicationSideEffectSnapshot::capture(&repository).unwrap();
        snapshot.lease_monitor.shared.lock().unwrap().monitor_error =
            Some("injected pre-transaction backup failure".to_string());

        let forward = CancellationToken::new();
        let forward_launched = std::cell::Cell::new(false);
        assert!(
            snapshot
                .run_supervised_ref_transaction(&forward, || {
                    forward_launched.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(forward.is_cancelled());
        assert!(!forward_launched.get());

        let rollback_launched = std::cell::Cell::new(false);
        let rollback = snapshot.run_supervised_ref_rollback(|rollback| {
            assert!(!rollback.is_cancelled());
            rollback_launched.set(true);
            Ok(())
        });
        assert!(rollback.is_ok());
        assert!(rollback_launched.get());
        snapshot
            .release_failed_leases_after_all_transactions(&rollback)
            .unwrap();
    }
}

#[cfg(target_os = "linux")]
const LINUX_F_SETSIG: libc::c_int = 10;
#[cfg(target_os = "linux")]
const LINUX_F_SETOWN_EX: libc::c_int = 15;
#[cfg(target_os = "linux")]
const LINUX_F_OWNER_TID: libc::c_int = 0;

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxFileOwner {
    kind: libc::c_int,
    pid: libc::pid_t,
}

#[cfg(target_os = "linux")]
fn run_publication_lease_monitor(
    mut files: Vec<(Vec<u8>, File)>,
    backup_directory: File,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shared: std::sync::Arc<Mutex<PublicationLeaseShared>>,
    ready: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<()> {
    let signal = libc::SIGRTMIN() + 5;
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    if unsafe { libc::sigemptyset(&mut mask) } != 0
        || unsafe { libc::sigaddset(&mut mask, signal) } != 0
        || unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) } != 0
    {
        let err = std::io::Error::last_os_error();
        let _ = ready.send(Err(format!("block publication lease signal: {err}")));
        return Err(err.into());
    }
    let signal_fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if signal_fd < 0 {
        let err = std::io::Error::last_os_error();
        let _ = ready.send(Err(format!(
            "create publication lease signal descriptor: {err}"
        )));
        return Err(err.into());
    }
    let signal_file = unsafe { File::from_raw_fd(signal_fd) };
    let owner = LinuxFileOwner {
        kind: LINUX_F_OWNER_TID,
        pid: unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t },
    };
    let mut leased = BTreeSet::new();
    let mut setup_error = None;
    for (path, file) in &files {
        let fd = file.as_raw_fd();
        let configured = unsafe { libc::fcntl(fd, LINUX_F_SETOWN_EX, &owner) } == 0
            && unsafe { libc::fcntl(fd, LINUX_F_SETSIG, signal) } == 0
            && unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) } == 0;
        if !configured {
            setup_error = Some(format!(
                "lease {}: {}",
                safe_relative_git_path(path)?.display(),
                std::io::Error::last_os_error()
            ));
            break;
        }
        leased.insert(fd);
        shared
            .lock()
            .expect("publication lease state lock")
            .sources
            .insert(path.clone(), file.try_clone()?);
    }
    if let Some(err) = setup_error {
        for (_, file) in &files {
            if leased.contains(&file.as_raw_fd()) {
                unsafe {
                    libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK);
                }
            }
        }
        let _ = ready.send(Err(err.clone()));
        bail!(err);
    }
    ready
        .send(Ok(()))
        .map_err(|_| anyhow::anyhow!("publication lease startup receiver disappeared"))?;

    let runtime_result = (|| -> Result<()> {
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            let mut poll_fd = libc::pollfd {
                fd: signal_file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let poll_result = unsafe { libc::poll(&mut poll_fd, 1, 25) };
            if poll_result < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                bail!("poll publication lease signals: {err}");
            }
            if poll_result == 0 || poll_fd.revents & libc::POLLIN == 0 {
                continue;
            }
            loop {
                let mut info = unsafe { std::mem::zeroed::<libc::signalfd_siginfo>() };
                let read = unsafe {
                    libc::read(
                        signal_file.as_raw_fd(),
                        (&mut info as *mut libc::signalfd_siginfo).cast(),
                        std::mem::size_of::<libc::signalfd_siginfo>(),
                    )
                };
                if read < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        break;
                    }
                    bail!("read publication lease signal: {err}");
                }
                if read as usize != std::mem::size_of::<libc::signalfd_siginfo>() {
                    bail!("publication lease signal record was truncated");
                }
                let fd = info.ssi_fd;
                let Some((path, file)) = files.iter().find(|(_, file)| file.as_raw_fd() == fd)
                else {
                    continue;
                };
                if leased.contains(&fd) {
                    let (backup_already_failed, active_cancellation) = {
                        let state = shared.lock().map_err(|_| {
                            anyhow::anyhow!("publication lease state lock was poisoned")
                        })?;
                        (
                            state.errors.contains_key(path),
                            state.active_cancellation.clone(),
                        )
                    };
                    if backup_already_failed {
                        if let Some(cancellation) = active_cancellation {
                            cancellation.cancel();
                        }
                        continue;
                    }
                    let backup_name = OsString::from(hex::encode(Sha256::digest(path)));
                    #[cfg(test)]
                    let disk_backup =
                        if take_publication_backup_failure(&FAIL_PUBLICATION_LAZY_BACKUP, file) {
                            Err(anyhow::anyhow!("injected ignored-file disk backup failure"))
                        } else {
                            copy_open_publication_file(file, &backup_directory, &backup_name)
                        };
                    #[cfg(not(test))]
                    let disk_backup =
                        copy_open_publication_file(file, &backup_directory, &backup_name);
                    let backup_result = disk_backup.or_else(|disk_err| {
                        #[cfg(test)]
                        if take_publication_backup_failure(
                            &FAIL_PUBLICATION_MEMORY_BACKUP,
                            file,
                        ) {
                            return Err(anyhow::anyhow!(
                                "injected ignored-file anonymous-memory backup failure"
                            ))
                            .with_context(|| {
                                format!(
                                    "descriptor-confined disk backup failed ({disk_err:#}); anonymous-memory fallback also failed"
                                )
                            });
                        }
                        copy_open_publication_file_to_memfd(file).with_context(|| {
                            format!(
                                "descriptor-confined disk backup failed ({disk_err:#}); anonymous-memory fallback also failed"
                            )
                        })
                    });
                    match backup_result {
                        Ok(backup) => {
                            shared
                                .lock()
                                .expect("publication lease state lock")
                                .backups
                                .insert(path.clone(), backup);
                            if unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_UNLCK) } != 0 {
                                bail!(
                                    "release ignored publication write lease after durable lazy backup: {}",
                                    std::io::Error::last_os_error()
                                );
                            }
                            leased.remove(&fd);
                        }
                        Err(err) => {
                            let cancellation = {
                                let mut state = shared.lock().map_err(|_| {
                                    anyhow::anyhow!("publication lease state lock was poisoned")
                                })?;
                                let message = format!(
                                    "preserve ignored publication file before write-open: {err:#}"
                                );
                                state.errors.insert(path.clone(), message.clone());
                                state.monitor_error = Some(message);
                                state.active_cancellation.clone()
                            };
                            if let Some(cancellation) = cancellation {
                                cancellation.cancel();
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(err) = &runtime_result {
        if let Ok(mut state) = shared.lock() {
            state.monitor_error = Some(format!("{err:#}"));
            state.monitor_unavailable = true;
            if let Some(cancellation) = &state.active_cancellation {
                cancellation.cancel();
            }
        }
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let mut unlock_error = None;
    for (_, file) in &files {
        if leased.contains(&file.as_raw_fd())
            && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } != 0
            && unlock_error.is_none()
        {
            unlock_error = Some(std::io::Error::last_os_error());
        }
    }
    files.clear();
    if let Some(unlock_error) = unlock_error {
        return match runtime_result {
            Ok(()) => Err(unlock_error)
                .context("release ignored publication write leases during monitor shutdown"),
            Err(runtime_error) => Err(runtime_error).context(format!(
                "release ignored publication write leases during monitor shutdown also failed: {unlock_error}"
            )),
        };
    }
    runtime_result
}

#[cfg(target_os = "linux")]
fn copy_open_publication_file(
    source: &File,
    backup_directory: &File,
    backup_name: &OsStr,
) -> Result<File> {
    let mut source = source.try_clone()?;
    source.seek(SeekFrom::Start(0))?;
    let backup_name = std::ffi::CString::new(backup_name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            backup_directory.as_raw_fd(),
            backup_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("create descriptor-confined ignored publication lease backup");
    }
    let mut destination = unsafe { File::from_raw_fd(fd) };
    if unsafe { libc::unlinkat(backup_directory.as_raw_fd(), backup_name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("unlink descriptor-confined ignored publication lease backup");
    }
    backup_directory.sync_all()?;
    std::io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    destination.seek(SeekFrom::Start(0))?;
    Ok(destination)
}

#[cfg(target_os = "linux")]
fn copy_open_publication_file_to_memfd(source: &File) -> Result<File> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"khazad-publication-backup".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as libc::c_int
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("create anonymous-memory ignored publication lease backup");
    }
    let mut destination = unsafe { File::from_raw_fd(fd) };
    let mut source = source.try_clone()?;
    source.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut source, &mut destination)?;
    if unsafe {
        libc::fcntl(
            destination.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("seal anonymous-memory ignored publication lease backup");
    }
    destination.seek(SeekFrom::Start(0))?;
    Ok(destination)
}

#[cfg(target_os = "linux")]
fn restore_publication_ambient_filesystem(
    repository: &PinnedGitRepository,
    baseline: &PublicationSideEffectSnapshot,
    _entries: &[CapturedPublicationEntry],
) -> Result<()> {
    repository.ensure_attached()?;
    let current = capture_publication_ambient_filesystem(repository)?.entries;
    for (path, expected) in &baseline.ambient_filesystem {
        if current.get(path) != Some(expected)
            && !expected.hardlink_group.is_empty()
            && baseline
                .lease_monitor
                .backup_failed(&expected.hardlink_group)
        {
            bail!(
                "ignored publication file changed after both lazy backups failed; original names were preserved for operator recovery: {}",
                safe_relative_git_path(path)?.display()
            );
        }
    }
    let baseline_paths = baseline
        .ambient_filesystem
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut removable = current
        .keys()
        .filter(|path| !baseline_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    removable.sort_by_key(|path| std::cmp::Reverse(path.len()));
    removable.dedup();
    for path in removable {
        remove_untracked_path(repository.root.operation_path(), &path)?;
    }

    let mut changed_groups = baseline.lease_monitor.changed_paths();
    for (path, expected) in &baseline.ambient_filesystem {
        if current.get(path) != Some(expected) && !expected.hardlink_group.is_empty() {
            changed_groups.insert(expected.hardlink_group.clone());
        }
    }

    let mut directories = BTreeMap::new();
    for (path, entry) in &baseline.ambient_filesystem {
        if matches!(entry.kind, PublicationFilesystemKind::Directory) {
            directories.insert(path.clone(), publication_entry_for_metadata(entry));
        }
    }
    restore_tracked_filesystem(repository.root.operation_path(), &directories)?;

    let mut groups = BTreeMap::<Vec<u8>, Vec<Vec<u8>>>::new();
    for (path, entry) in &baseline.ambient_filesystem {
        if !entry.hardlink_group.is_empty() {
            groups
                .entry(entry.hardlink_group.clone())
                .or_default()
                .push(path.clone());
        }
    }
    for (group, paths) in groups {
        if !changed_groups.contains(&group) {
            continue;
        }
        let canonical_entry = baseline
            .ambient_filesystem
            .get(&group)
            .ok_or_else(|| anyhow::anyhow!("publication hard-link canonical entry was missing"))?;
        match &canonical_entry.kind {
            PublicationFilesystemKind::File { .. } => {
                let source = baseline.lease_monitor.source_file(&group)?;
                restore_publication_regular_file_group(
                    repository.root.operation_path(),
                    &group,
                    &paths,
                    canonical_entry,
                    source,
                )?;
            }
            PublicationFilesystemKind::Symlink(target) => {
                restore_publication_symlink_group(
                    repository.root.operation_path(),
                    &group,
                    &paths,
                    canonical_entry,
                    target,
                )?;
            }
            PublicationFilesystemKind::Directory | PublicationFilesystemKind::Missing => {
                bail!("publication hard-link group had an unsupported path type");
            }
        }
    }
    restore_tracked_filesystem(repository.root.operation_path(), &directories)?;
    repository.ensure_attached()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn restore_publication_ambient_filesystem(
    _repository: &PinnedGitRepository,
    _baseline: &PublicationSideEffectSnapshot,
    _entries: &[CapturedPublicationEntry],
) -> Result<()> {
    bail!("completion publication ambient hook restoration requires Linux")
}

#[cfg(target_os = "linux")]
fn publication_entry_for_metadata(entry: &PublicationFilesystemEntry) -> TrackedFilesystemEntry {
    TrackedFilesystemEntry {
        kind: match &entry.kind {
            PublicationFilesystemKind::Directory => TrackedFilesystemKind::Directory,
            PublicationFilesystemKind::Symlink(target) => {
                TrackedFilesystemKind::Symlink(target.clone())
            }
            PublicationFilesystemKind::File { .. } => TrackedFilesystemKind::Missing,
            PublicationFilesystemKind::Missing => TrackedFilesystemKind::Missing,
        },
        mode: entry.mode,
        mtime_seconds: entry.mtime_seconds,
        mtime_nanoseconds: entry.mtime_nanoseconds,
    }
}

#[cfg(target_os = "linux")]
fn remove_publication_group_paths(root: &File, paths: &[Vec<u8>]) -> Result<()> {
    for path in paths.iter().rev() {
        let relative = safe_relative_git_path(path)?;
        let (parent, name) = verification_parent_for_restore(root, &relative)?;
        remove_confined_verification_entry(&parent, &name)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_publication_regular_file_group(
    root: &Path,
    canonical: &[u8],
    paths: &[Vec<u8>],
    entry: &PublicationFilesystemEntry,
    mut source: File,
) -> Result<()> {
    let root = open_verification_root(root)?;
    remove_publication_group_paths(&root, paths)?;
    let canonical_relative = safe_relative_git_path(canonical)?;
    let (canonical_parent, canonical_name) =
        verification_parent_for_restore(&root, &canonical_relative)?;
    let mut destination =
        open_confined_verification_file_for_restore(&canonical_parent, &canonical_name)?;
    source.seek(SeekFrom::Start(0))?;
    let copied = std::io::copy(&mut source, &mut destination)?;
    let expected_size = match entry.kind {
        PublicationFilesystemKind::File { size } => size,
        _ => 0,
    };
    if copied != expected_size {
        bail!(
            "ignored publication restoration source changed length: expected {expected_size}, copied {copied}"
        );
    }
    destination.set_len(expected_size)?;
    destination.sync_all()?;
    for alias in paths.iter().filter(|path| path.as_slice() != canonical) {
        let alias_relative = safe_relative_git_path(alias)?;
        let (alias_parent, alias_name) = verification_parent_for_restore(&root, &alias_relative)?;
        let canonical_name_c = verification_c_name(&canonical_name)?;
        let alias_name_c = verification_c_name(&alias_name)?;
        if unsafe {
            libc::linkat(
                canonical_parent.as_raw_fd(),
                canonical_name_c.as_ptr(),
                alias_parent.as_raw_fd(),
                alias_name_c.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("restore ignored publication hard link");
        }
    }
    restore_open_file_metadata(&destination, &publication_entry_for_metadata(entry))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_publication_symlink_group(
    root: &Path,
    canonical: &[u8],
    paths: &[Vec<u8>],
    entry: &PublicationFilesystemEntry,
    target: &[u8],
) -> Result<()> {
    let root = open_verification_root(root)?;
    remove_publication_group_paths(&root, paths)?;
    let canonical_relative = safe_relative_git_path(canonical)?;
    let (canonical_parent, canonical_name) =
        verification_parent_for_restore(&root, &canonical_relative)?;
    let canonical_name_c = verification_c_name(&canonical_name)?;
    let target_c = std::ffi::CString::new(target)?;
    if unsafe {
        libc::symlinkat(
            target_c.as_ptr(),
            canonical_parent.as_raw_fd(),
            canonical_name_c.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("restore ignored publication symlink");
    }
    for alias in paths.iter().filter(|path| path.as_slice() != canonical) {
        let alias_relative = safe_relative_git_path(alias)?;
        let (alias_parent, alias_name) = verification_parent_for_restore(&root, &alias_relative)?;
        let alias_name_c = verification_c_name(&alias_name)?;
        if unsafe {
            libc::linkat(
                canonical_parent.as_raw_fd(),
                canonical_name_c.as_ptr(),
                alias_parent.as_raw_fd(),
                alias_name_c.as_ptr(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("restore ignored publication hard-linked symlink");
        }
    }
    restore_confined_symlink_mtime(
        &canonical_parent,
        &canonical_name,
        &publication_entry_for_metadata(entry),
    )?;
    Ok(())
}

fn tracked_filesystem_digest(entries: &BTreeMap<Vec<u8>, TrackedFilesystemEntry>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for (path, entry) in entries {
        digest_field(&mut hasher, path);
        digest_field(&mut hasher, &entry.mode.to_be_bytes());
        digest_field(&mut hasher, &entry.mtime_seconds.to_be_bytes());
        digest_field(&mut hasher, &entry.mtime_nanoseconds.to_be_bytes());
        match &entry.kind {
            TrackedFilesystemKind::File(bytes) => {
                digest_field(&mut hasher, b"file");
                digest_field(&mut hasher, bytes);
            }
            TrackedFilesystemKind::Symlink(target) => {
                digest_field(&mut hasher, b"symlink");
                digest_field(&mut hasher, target);
            }
            TrackedFilesystemKind::Directory => digest_field(&mut hasher, b"directory"),
            TrackedFilesystemKind::Missing => digest_field(&mut hasher, b"missing"),
        }
    }
    hasher.finalize().to_vec()
}

#[cfg(target_os = "linux")]
fn verification_c_name(name: &OsStr) -> Result<std::ffi::CString> {
    Ok(std::ffi::CString::new(name.as_bytes())?)
}

#[cfg(target_os = "linux")]
fn remove_confined_verification_entry(parent: &File, name: &OsStr) -> Result<()> {
    let entry = match open_verification_entry(parent, name) {
        Ok(entry) => entry,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("open verification path for confined removal"),
    };
    let metadata = entry.metadata()?;
    if metadata.is_dir() {
        for child in verification_directory_names(&entry)? {
            remove_confined_verification_entry(&entry, &child)?;
        }
    }
    let current = open_verification_entry(parent, name)
        .context("revalidate verification path before confined removal")?;
    let current_metadata = current.metadata()?;
    if current_metadata.dev() != metadata.dev() || current_metadata.ino() != metadata.ino() {
        bail!("verification path changed concurrently before confined removal");
    }
    let name = verification_c_name(name)?;
    let flags = if metadata.is_dir() {
        libc::AT_REMOVEDIR
    } else {
        0
    };
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error()).context("remove confined verification path");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_confined_verification_directory(parent: &File, name: &OsStr) -> Result<File> {
    let name_c = verification_c_name(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(err).context("create confined verification directory");
        }
    }
    let directory = open_verification_directory(parent, name)
        .context("open confined verification directory after creation")?;
    if !directory.metadata()?.is_dir() {
        bail!("verification parent changed to a non-directory");
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn verification_parent_for_restore(root: &File, relative: &Path) -> Result<(File, OsString)> {
    let components = relative.components().collect::<Vec<_>>();
    let Some(Component::Normal(leaf)) = components.last() else {
        bail!("git returned an empty or unsafe tracked path");
    };
    let mut directory = root.try_clone()?;
    for component in &components[..components.len() - 1] {
        let Component::Normal(name) = component else {
            bail!("git returned unsafe tracked path component");
        };
        let next = match open_verification_entry(&directory, name) {
            Ok(next) if next.metadata()?.is_dir() => next,
            Ok(_) => {
                remove_confined_verification_entry(&directory, name)?;
                create_confined_verification_directory(&directory, name)?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                create_confined_verification_directory(&directory, name)?
            }
            Err(err) => return Err(err).context("open verification restoration parent"),
        };
        directory = next;
    }
    Ok((directory, leaf.to_os_string()))
}

#[cfg(target_os = "linux")]
fn open_confined_verification_file_for_restore(parent: &File, name: &OsStr) -> Result<File> {
    remove_confined_verification_entry(parent, name)?;
    let name = verification_c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open confined tracked file for restoration");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn restore_open_file_metadata(file: &File, entry: &TrackedFilesystemEntry) -> Result<()> {
    if unsafe { libc::fchmod(file.as_raw_fd(), entry.mode as libc::mode_t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("restore confined verification mode");
    }
    let times = [
        libc::timespec {
            tv_sec: entry.mtime_seconds,
            tv_nsec: entry.mtime_nanoseconds,
        },
        libc::timespec {
            tv_sec: entry.mtime_seconds,
            tv_nsec: entry.mtime_nanoseconds,
        },
    ];
    if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("restore confined verification timestamp");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_confined_symlink_mtime(
    parent: &File,
    name: &OsStr,
    entry: &TrackedFilesystemEntry,
) -> Result<()> {
    let symlink = open_verification_entry(parent, name)
        .context("pin restored verification symlink metadata")?;
    if !symlink.metadata()?.file_type().is_symlink() {
        bail!("restored verification symlink changed before metadata restoration");
    }
    let times = [
        libc::timespec {
            tv_sec: entry.mtime_seconds,
            tv_nsec: entry.mtime_nanoseconds,
        },
        libc::timespec {
            tv_sec: entry.mtime_seconds,
            tv_nsec: entry.mtime_nanoseconds,
        },
    ];
    if unsafe {
        libc::utimensat(
            symlink.as_raw_fd(),
            c"".as_ptr(),
            times.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("restore confined verification symlink timestamp");
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
static SUBSTITUTE_VERIFICATION_PARENT_DURING_RESTORE: std::sync::Mutex<
    Vec<VerificationParentSubstitution>,
> = std::sync::Mutex::new(Vec::new());

#[cfg(all(test, target_os = "linux"))]
pub fn substitute_next_verification_parent_during_restore(
    tracked_path: &[u8],
    parent: &Path,
    parked: &Path,
    outside: &Path,
) {
    SUBSTITUTE_VERIFICATION_PARENT_DURING_RESTORE
        .lock()
        .unwrap()
        .push(VerificationParentSubstitution {
            expected_path: tracked_path.to_vec(),
            expected_parent_identity: filesystem_object_identity_bytes(parent)
                .expect("verification restoration substitution parent must exist"),
            parent: parent.to_path_buf(),
            parked: parked.to_path_buf(),
            outside: outside.to_path_buf(),
        });
}

#[cfg(target_os = "linux")]
fn restore_tracked_filesystem(
    root: &Path,
    entries: &BTreeMap<Vec<u8>, TrackedFilesystemEntry>,
) -> Result<()> {
    let root_directory = open_verification_root(root)?;
    let mut directories = Vec::new();
    for (path_bytes, entry) in entries {
        let relative = safe_relative_git_path(path_bytes)?;
        let (parent, name) = verification_parent_for_restore(&root_directory, &relative)?;
        #[cfg(test)]
        {
            let current_parent_identity = open_filesystem_object_identity_bytes(&parent)?;
            let substitution = {
                let mut substitution = SUBSTITUTE_VERIFICATION_PARENT_DURING_RESTORE
                    .lock()
                    .unwrap();
                substitution
                    .iter()
                    .position(|substitution| {
                        substitution.expected_path.as_slice() == path_bytes.as_slice()
                            && substitution.expected_parent_identity == current_parent_identity
                    })
                    .map(|position| substitution.remove(position))
            };
            if let Some(substitution) = substitution {
                fs::rename(&substitution.parent, &substitution.parked)?;
                std::os::unix::fs::symlink(&substitution.outside, &substitution.parent)?;
            }
        }
        match &entry.kind {
            TrackedFilesystemKind::Missing => {
                remove_confined_verification_entry(&parent, &name)?;
            }
            TrackedFilesystemKind::File(bytes) => {
                let mut file = open_confined_verification_file_for_restore(&parent, &name)?;
                file.write_all(bytes)?;
                file.set_len(bytes.len() as u64)?;
                restore_open_file_metadata(&file, entry)?;
            }
            TrackedFilesystemKind::Symlink(target) => {
                remove_confined_verification_entry(&parent, &name)?;
                let name_c = verification_c_name(&name)?;
                let target = std::ffi::CString::new(target.as_slice())?;
                if unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name_c.as_ptr()) }
                    != 0
                {
                    return Err(std::io::Error::last_os_error())
                        .context("restore confined tracked symlink");
                }
                restore_confined_symlink_mtime(&parent, &name, entry)?;
            }
            TrackedFilesystemKind::Directory => {
                let directory = match open_verification_directory(&parent, &name) {
                    Ok(current) => current,
                    Err(err)
                        if matches!(
                            err.raw_os_error(),
                            Some(libc::ENOTDIR) | Some(libc::ELOOP)
                        ) =>
                    {
                        remove_confined_verification_entry(&parent, &name)?;
                        create_confined_verification_directory(&parent, &name)?
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        create_confined_verification_directory(&parent, &name)?
                    }
                    Err(err) => return Err(err).context("restore tracked verification directory"),
                };
                directories.push((directory, entry));
            }
        }
    }
    for (directory, entry) in directories.into_iter().rev() {
        restore_open_file_metadata(&directory, entry)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn restore_tracked_filesystem(
    root: &Path,
    entries: &BTreeMap<Vec<u8>, TrackedFilesystemEntry>,
) -> Result<()> {
    let mut directories = Vec::new();
    for (path_bytes, entry) in entries {
        let relative = safe_relative_git_path(path_bytes)?;
        let absolute = root.join(relative);
        match &entry.kind {
            TrackedFilesystemKind::Missing => remove_existing_path(&absolute)?,
            TrackedFilesystemKind::File(bytes) => {
                if fs::symlink_metadata(&absolute)
                    .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
                {
                    remove_existing_path(&absolute)?;
                }
                if let Some(parent) = absolute.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&absolute, bytes).with_context(|| {
                    format!("restore tracked verification bytes {}", absolute.display())
                })?;
                restore_filesystem_mode(&absolute, entry.mode)?;
                restore_filesystem_mtime(
                    &absolute,
                    entry.mtime_seconds,
                    entry.mtime_nanoseconds,
                    false,
                )?;
            }
            TrackedFilesystemKind::Symlink(target) => {
                remove_existing_path(&absolute)?;
                if let Some(parent) = absolute.parent() {
                    fs::create_dir_all(parent)?;
                }
                restore_symlink(target, &absolute)?;
                restore_filesystem_mtime(
                    &absolute,
                    entry.mtime_seconds,
                    entry.mtime_nanoseconds,
                    true,
                )?;
            }
            TrackedFilesystemKind::Directory => {
                if fs::symlink_metadata(&absolute)
                    .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
                {
                    remove_existing_path(&absolute)?;
                }
                fs::create_dir_all(&absolute)?;
                directories.push((absolute, entry));
            }
        }
    }
    for (path, entry) in directories.into_iter().rev() {
        restore_filesystem_mode(&path, entry.mode)?;
        restore_filesystem_mtime(&path, entry.mtime_seconds, entry.mtime_nanoseconds, false)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_untracked_publication_entries(
    root: &Path,
    tracked: &BTreeMap<Vec<u8>, TrackedFilesystemEntry>,
    entries: &[CapturedPublicationEntry],
) -> Result<()> {
    let root_directory = open_verification_root(root)?;
    for entry in entries {
        if tracked.contains_key(&entry.path_bytes) {
            continue;
        }
        let relative = safe_relative_git_path(&entry.path_bytes)?;
        let (parent, name) = verification_parent_for_restore(&root_directory, &relative)?;
        let mut file = open_confined_verification_file_for_restore(&parent, &name)?;
        file.write_all(&entry.bytes)?;
        file.set_len(entry.bytes.len() as u64)?;
        let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("restore completion publication manifest mode");
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn restore_untracked_publication_entries(
    root: &Path,
    tracked: &BTreeMap<Vec<u8>, TrackedFilesystemEntry>,
    entries: &[CapturedPublicationEntry],
) -> Result<()> {
    for entry in entries {
        if tracked.contains_key(&entry.path_bytes) {
            continue;
        }
        let relative = safe_relative_git_path(&entry.path_bytes)?;
        let absolute = root.join(relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&absolute, &entry.bytes)?;
        restore_filesystem_mode(
            &absolute,
            if entry.mode == "100755" { 0o755 } else { 0o644 },
        )?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn remove_existing_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn restore_symlink(target: &[u8], path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(OsString::from_vec(target.to_vec()), path)?;
    Ok(())
}

#[cfg(not(unix))]
fn restore_symlink(target: &[u8], path: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(String::from_utf8_lossy(target).as_ref(), path)?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn filesystem_mode(metadata: &fs::Metadata) -> u32 {
    metadata.mode()
}

#[cfg(not(unix))]
fn filesystem_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[cfg(all(unix, not(target_os = "linux")))]
fn filesystem_mtime_seconds(metadata: &fs::Metadata) -> i64 {
    metadata.mtime()
}

#[cfg(not(unix))]
fn filesystem_mtime_seconds(_metadata: &fs::Metadata) -> i64 {
    0
}

#[cfg(all(unix, not(target_os = "linux")))]
fn filesystem_mtime_nanoseconds(metadata: &fs::Metadata) -> i64 {
    metadata.mtime_nsec()
}

#[cfg(not(unix))]
fn filesystem_mtime_nanoseconds(_metadata: &fs::Metadata) -> i64 {
    0
}

#[cfg(all(unix, not(target_os = "linux")))]
fn restore_filesystem_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restore_filesystem_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn restore_filesystem_mtime(
    path: &Path,
    seconds: i64,
    nanoseconds: i64,
    no_follow: bool,
) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: seconds,
            tv_nsec: nanoseconds,
        },
    ];
    let flags = if no_follow {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    let status = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) };
    if status != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("restore verification mtime {}", path.to_string_lossy()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_filesystem_mtime(
    _path: &Path,
    _seconds: i64,
    _nanoseconds: i64,
    _no_follow: bool,
) -> Result<()> {
    Ok(())
}

fn restore_head_attachment(snapshot: &WorktreeSnapshot) -> Result<()> {
    let head = ascii_sha(&snapshot.head)?;
    let attachment = ascii_git_value(&snapshot.head_attachment, "HEAD attachment")?;
    if attachment == "HEAD" {
        snapshot
            .git_context
            .run(&["update-ref", "--no-deref", "HEAD", head])?;
    } else {
        snapshot
            .git_context
            .run(&["update-ref", attachment, head])?;
        snapshot
            .git_context
            .run(&["symbolic-ref", "HEAD", attachment])?;
    }
    Ok(())
}

fn index_has_hidden_worktree_flags(entries: &[u8]) -> bool {
    entries
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry[0] == b'S' || entry[0].is_ascii_lowercase())
}

fn index_records_are_gitlink(records: &[Vec<u8>]) -> Result<bool> {
    records.iter().try_fold(false, |found, record| {
        let mode_end = record
            .get(2..)
            .and_then(|metadata| metadata.iter().position(|byte| *byte == b' '))
            .map(|offset| offset + 2)
            .context("Git index entry omitted its mode")?;
        let mode = std::str::from_utf8(&record[2..mode_end])?;
        Ok(found || u32::from_str_radix(mode, 8)? == 0o160000)
    })
}

fn index_entries_by_path(entries: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<Vec<u8>>>> {
    let mut by_path = BTreeMap::<Vec<u8>, Vec<Vec<u8>>>::new();
    for entry in entries
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| anyhow::anyhow!("invalid NUL-delimited git index entry"))?;
        if tab < 3 || entry.get(1) != Some(&b' ') || tab + 1 >= entry.len() {
            bail!("invalid NUL-delimited git index entry metadata");
        }
        by_path
            .entry(entry[tab + 1..].to_vec())
            .or_default()
            .push(entry[..tab].to_vec());
    }
    for entries in by_path.values_mut() {
        entries.sort();
    }
    Ok(by_path)
}

fn verification_workspace_evidence(
    before: Option<&WorktreeSnapshot>,
    after: Option<&WorktreeSnapshot>,
    restored: Option<&WorktreeSnapshot>,
    after_capture_error: String,
    restoration_error: String,
) -> VerificationWorkspaceEvidence {
    let mut evidence_error = Vec::new();
    let mut capture = |label: &str, snapshot: Option<&WorktreeSnapshot>| {
        snapshot.and_then(|snapshot| match snapshot.evidence() {
            Ok(evidence) => Some(evidence),
            Err(err) => {
                evidence_error.push(format!("{label}: {err:#}"));
                None
            }
        })
    };
    VerificationWorkspaceEvidence {
        before: capture("before", before),
        after: capture("after", after),
        restored: capture("restored", restored),
        after_capture_error,
        restoration_error,
        evidence_error: evidence_error.join("; "),
    }
}

fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(dir)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let message = if stderr.is_empty() { stdout } else { stderr };
        if message.is_empty() {
            bail!("git {} failed with {}", args.join(" "), output.status);
        }
        bail!(
            "git {} failed with {}: {}",
            args.join(" "),
            output.status,
            message
        );
    }
    Ok(output.stdout)
}

fn semantic_index_bytes(index: &[u8], object_id_len: usize) -> Result<Vec<u8>> {
    let checksum_start = index
        .len()
        .checked_sub(object_id_len)
        .context("Git index is shorter than its checksum")?;
    if checksum_start < 12 || &index[..4] != b"DIRC" {
        bail!("invalid Git index header");
    }
    let version = u32::from_be_bytes(index[4..8].try_into().expect("fixed index header"));
    if !(2..=4).contains(&version) {
        bail!("unsupported Git index version {version}");
    }
    let entry_count =
        u32::from_be_bytes(index[8..12].try_into().expect("fixed index header")) as usize;
    let mut normalized = index.to_vec();
    let mut offset = 12usize;
    for _ in 0..entry_count {
        let entry_start = offset;
        let fixed_len = 40usize
            .checked_add(object_id_len)
            .and_then(|len| len.checked_add(2))
            .context("Git index entry length overflow")?;
        let fixed_end = offset
            .checked_add(fixed_len)
            .context("Git index entry offset overflow")?;
        if fixed_end > checksum_start {
            bail!("truncated Git index entry");
        }
        normalized[offset..offset + 24].fill(0);
        normalized[offset + 28..offset + 40].fill(0);
        let flags_offset = offset + 40 + object_id_len;
        let flags = u16::from_be_bytes(
            index[flags_offset..flags_offset + 2]
                .try_into()
                .expect("checked index flags"),
        );
        offset = fixed_end;
        if flags & 0x4000 != 0 {
            offset = offset
                .checked_add(2)
                .context("Git extended index entry offset overflow")?;
            if offset > checksum_start {
                bail!("truncated extended Git index entry");
            }
        }
        if version == 4 {
            loop {
                let byte = *index
                    .get(offset)
                    .filter(|_| offset < checksum_start)
                    .context("truncated Git v4 path prefix")?;
                offset += 1;
                if byte & 0x80 == 0 {
                    break;
                }
            }
        }
        let suffix_len = index[offset..checksum_start]
            .iter()
            .position(|byte| *byte == 0)
            .context("unterminated Git index path")?;
        offset = offset
            .checked_add(suffix_len + 1)
            .context("Git index path offset overflow")?;
        if version < 4 {
            let entry_len = offset - entry_start;
            let padding = (8 - (entry_len % 8)) % 8;
            offset = offset
                .checked_add(padding)
                .context("Git index padding overflow")?;
            if offset > checksum_start {
                bail!("truncated Git index padding");
            }
        }
    }
    if offset > checksum_start {
        bail!("Git index entries overlap its checksum");
    }
    normalized[checksum_start..].fill(0);
    Ok(normalized)
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct IndexStatEntry {
    path: Vec<u8>,
    object_id: Vec<u8>,
    mode: u32,
    ctime_seconds: u32,
    ctime_nanoseconds: u32,
    mtime_seconds: u32,
    mtime_nanoseconds: u32,
    device: u32,
    inode: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

#[cfg(target_os = "linux")]
fn index_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated Git index stat field")?
            .try_into()?,
    ))
}

#[cfg(target_os = "linux")]
fn decode_index_v4_strip_count(bytes: &[u8], offset: &mut usize, end: usize) -> Result<usize> {
    let first = *bytes
        .get(*offset)
        .filter(|_| *offset < end)
        .context("truncated Git v4 path prefix")?;
    *offset += 1;
    let mut value = usize::from(first & 0x7f);
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = *bytes
            .get(*offset)
            .filter(|_| *offset < end)
            .context("truncated Git v4 path prefix")?;
        *offset += 1;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
            .context("Git v4 path prefix overflow")?;
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
fn index_stat_entries(index: &[u8], object_id_len: usize) -> Result<Vec<IndexStatEntry>> {
    let checksum_start = index
        .len()
        .checked_sub(object_id_len)
        .context("Git index is shorter than its checksum")?;
    if checksum_start < 12 || &index[..4] != b"DIRC" {
        bail!("invalid Git index header");
    }
    let version = index_u32(index, 4)?;
    if !(2..=4).contains(&version) {
        bail!("unsupported Git index version {version}");
    }
    let entry_count = index_u32(index, 8)? as usize;
    let mut entries = Vec::with_capacity(entry_count);
    let mut previous_path = Vec::new();
    let mut offset = 12usize;
    for _ in 0..entry_count {
        let entry_start = offset;
        let fixed_end = offset
            .checked_add(40 + object_id_len + 2)
            .context("Git index entry offset overflow")?;
        if fixed_end > checksum_start {
            bail!("truncated Git index entry");
        }
        let flags_offset = offset + 40 + object_id_len;
        let flags = u16::from_be_bytes(index[flags_offset..flags_offset + 2].try_into()?);
        let stage = (flags >> 12) & 0x3;
        let object_id = index[offset + 40..offset + 40 + object_id_len].to_vec();
        let stat = IndexStatEntry {
            path: Vec::new(),
            object_id,
            mode: index_u32(index, offset + 24)?,
            ctime_seconds: index_u32(index, offset)?,
            ctime_nanoseconds: index_u32(index, offset + 4)?,
            mtime_seconds: index_u32(index, offset + 8)?,
            mtime_nanoseconds: index_u32(index, offset + 12)?,
            device: index_u32(index, offset + 16)?,
            inode: index_u32(index, offset + 20)?,
            uid: index_u32(index, offset + 28)?,
            gid: index_u32(index, offset + 32)?,
            size: index_u32(index, offset + 36)?,
        };
        offset = fixed_end;
        if flags & 0x4000 != 0 {
            offset = offset
                .checked_add(2)
                .context("Git extended index entry offset overflow")?;
            if offset > checksum_start {
                bail!("truncated extended Git index entry");
            }
        }
        let path = if version == 4 {
            let strip = decode_index_v4_strip_count(index, &mut offset, checksum_start)?;
            if strip > previous_path.len() {
                bail!("Git v4 path prefix exceeds previous path");
            }
            let suffix_len = index[offset..checksum_start]
                .iter()
                .position(|byte| *byte == 0)
                .context("unterminated Git v4 index path")?;
            let mut path = previous_path[..previous_path.len() - strip].to_vec();
            path.extend_from_slice(&index[offset..offset + suffix_len]);
            offset += suffix_len + 1;
            path
        } else {
            let path_len = index[offset..checksum_start]
                .iter()
                .position(|byte| *byte == 0)
                .context("unterminated Git index path")?;
            let path = index[offset..offset + path_len].to_vec();
            offset += path_len + 1;
            let entry_len = offset - entry_start;
            offset = offset
                .checked_add((8 - (entry_len % 8)) % 8)
                .context("Git index padding overflow")?;
            if offset > checksum_start {
                bail!("truncated Git index padding");
            }
            path
        };
        previous_path = path.clone();
        if stage == 0 {
            entries.push(IndexStatEntry { path, ..stat });
        }
    }
    Ok(entries)
}

#[cfg(target_os = "linux")]
fn git_filesystem_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o120000
    } else if metadata.is_dir() {
        0o160000
    } else if metadata.mode() & 0o111 == 0 {
        0o100644
    } else {
        0o100755
    }
}

#[cfg(target_os = "linux")]
fn index_stat_matches(entry: &IndexStatEntry, metadata: &fs::Metadata) -> bool {
    entry.mode == git_filesystem_mode(metadata)
        && entry.ctime_seconds == metadata.ctime() as u32
        && entry.ctime_nanoseconds == metadata.ctime_nsec() as u32
        && entry.mtime_seconds == metadata.mtime() as u32
        && entry.mtime_nanoseconds == metadata.mtime_nsec() as u32
        && entry.device == metadata.dev() as u32
        && entry.inode == metadata.ino() as u32
        && entry.uid == metadata.uid()
        && entry.gid == metadata.gid()
        && entry.size == metadata.size() as u32
}

#[cfg(target_os = "linux")]
fn tracked_path_metadata(root: &File, raw_path: &[u8]) -> Result<Option<fs::Metadata>> {
    let relative = safe_relative_git_path(raw_path)?;
    let Some((parent, name)) = verification_parent_and_name(root, &relative)? else {
        return Ok(None);
    };
    match open_verification_entry(&parent, &name) {
        Ok(entry) => Ok(Some(entry.metadata()?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("inspect descriptor-confined tracked stat state"),
    }
}

#[cfg(target_os = "linux")]
fn raw_unstaged_stat_diff(root: &Path, index: &[u8], object_id_len: usize) -> Result<Vec<u8>> {
    let root = open_verification_root(root)?;
    let zero_object = vec![b'0'; object_id_len * 2];
    let mut raw = Vec::new();
    for entry in index_stat_entries(index, object_id_len)? {
        let metadata = tracked_path_metadata(&root, &entry.path)?;
        if metadata
            .as_ref()
            .is_some_and(|metadata| index_stat_matches(&entry, metadata))
        {
            continue;
        }
        let current_mode = metadata
            .as_ref()
            .map(git_filesystem_mode)
            .unwrap_or_default();
        let status = if metadata.is_some() { b'M' } else { b'D' };
        raw.extend_from_slice(
            format!(
                ":{:06o} {:06o} {} ",
                entry.mode,
                current_mode,
                hex::encode(&entry.object_id)
            )
            .as_bytes(),
        );
        raw.extend_from_slice(&zero_object);
        raw.push(b' ');
        raw.push(status);
        raw.push(0);
        raw.extend_from_slice(&entry.path);
        raw.push(0);
    }
    Ok(raw)
}

#[cfg(target_os = "linux")]
fn snapshot_has_executable_filters(context: &VerificationGitContext) -> Result<bool> {
    if std::env::var_os("GIT_EXTERNAL_DIFF").is_some() {
        return Ok(true);
    }
    let names = context.run_snapshot_bytes(
        Path::new("/dev/null"),
        &["config", "--null", "--name-only", "--list"],
    )?;
    Ok(names.split(|byte| *byte == 0).any(|name| {
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        (name.starts_with("filter.")
            && (name.ends_with(".clean")
                || name.ends_with(".smudge")
                || name.ends_with(".process")))
            || (name.starts_with("diff.")
                && (name.ends_with(".textconv") || name.ends_with(".command")))
            || name == "diff.external"
    }))
}

#[cfg(target_os = "linux")]
fn capture_unstaged_state(
    context: &VerificationGitContext,
    root: &Path,
    private_index: &Path,
    raw_index: &[u8],
    object_id_len: usize,
) -> Result<Vec<u8>> {
    if snapshot_has_executable_filters(context)? {
        // Git's worktree-to-index comparison may execute clean/process filters. When any such
        // executable configuration is in scope, rely on the exact index stat cache instead;
        // later mutation detection still compares descriptor-captured raw bytes and metadata.
        raw_unstaged_stat_diff(root, raw_index, object_id_len)
    } else {
        context.run_snapshot_bytes(
            private_index,
            &["diff", "--raw", "-z", "--no-abbrev", "--find-renames"],
        )
    }
}

#[cfg(not(target_os = "linux"))]
fn capture_unstaged_state(
    context: &VerificationGitContext,
    _root: &Path,
    private_index: &Path,
    _raw_index: &[u8],
    _object_id_len: usize,
) -> Result<Vec<u8>> {
    context.run_snapshot_bytes(
        private_index,
        &["diff", "--raw", "-z", "--no-abbrev", "--find-renames"],
    )
}

#[cfg(target_os = "linux")]
fn collect_verification_directories(
    directory: &File,
    relative: &Path,
    root: bool,
    all: &mut Vec<Vec<u8>>,
    empty: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let names = verification_directory_names(directory)?;
    if !relative.as_os_str().is_empty() && names.is_empty() {
        empty.push(path_identity_bytes(relative));
    }
    for name in names {
        if root && name == OsStr::new(".git") {
            continue;
        }
        let entry = open_verification_entry(directory, &name)
            .with_context(|| format!("open verification directory entry {:?}", name))?;
        if !entry.metadata()?.is_dir() {
            continue;
        }
        let child = relative.join(&name);
        all.push(path_identity_bytes(&child));
        collect_verification_directories(&entry, &child, false, all, empty)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn collect_verification_directories(
    directory: &Path,
    relative: &Path,
    root: bool,
    all: &mut Vec<Vec<u8>>,
    empty: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| path_identity_bytes(&PathBuf::from(entry.file_name())));
    if !relative.as_os_str().is_empty() && entries.is_empty() {
        empty.push(path_identity_bytes(relative));
    }
    for entry in entries {
        if root && entry.file_name() == OsStr::new(".git") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let child = relative.join(entry.file_name());
        all.push(path_identity_bytes(&child));
        collect_verification_directories(&entry.path(), &child, false, all, empty)?;
    }
    Ok(())
}

fn ignored_directory_paths(
    context: &VerificationGitContext,
    index_path: &Path,
    directories: &[Vec<u8>],
) -> Result<BTreeSet<Vec<u8>>> {
    if directories.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut input = Vec::new();
    for path in directories {
        input.extend_from_slice(path);
        input.push(b'/');
        input.push(0);
    }
    context.validate_directories()?;
    let child = context
        .command()
        .args([
            "-c",
            "core.fsmonitor=false",
            "check-ignore",
            "--no-index",
            "--stdin",
            "-z",
        ])
        .env("GIT_INDEX_FILE", index_path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    context.validate_directories()?;
    let mut child = child.context("spawn snapshot git check-ignore for empty directories")?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("snapshot git check-ignore stdin unavailable"))
        .and_then(|mut stdin| stdin.write_all(&input).map_err(Into::into));
    let output = child.wait_with_output();
    context.validate_directories()?;
    write_result?;
    let output = output?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "snapshot git check-ignore failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut ignored = BTreeSet::new();
    for path in nul_paths(&output.stdout) {
        let path = path.strip_suffix(b"/").unwrap_or(&path);
        ignored.insert(path.to_vec());
    }
    Ok(ignored)
}

fn capture_nonignored_empty_directories(
    context: &VerificationGitContext,
    root: &Path,
    index_path: &Path,
) -> Result<(usize, Vec<u8>)> {
    let mut all = Vec::new();
    let mut empty = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let root_directory = open_verification_root(root)?;
        collect_verification_directories(
            &root_directory,
            Path::new(""),
            true,
            &mut all,
            &mut empty,
        )?;
    }
    #[cfg(not(target_os = "linux"))]
    collect_verification_directories(root, Path::new(""), true, &mut all, &mut empty)?;
    all.sort();
    all.dedup();
    empty.sort();
    empty.dedup();
    let ignored = ignored_directory_paths(context, index_path, &empty)?;
    let mut nonignored = Vec::new();
    for path in empty {
        if !ignored.contains(&path) {
            nonignored.extend_from_slice(&path);
            nonignored.push(0);
        }
    }
    Ok((all.len(), nonignored))
}

fn strip_command_line_ending(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn ascii_sha(bytes: &[u8]) -> Result<&str> {
    ascii_git_value(bytes, "git object id")
}

fn ascii_git_value<'a>(bytes: &'a [u8], label: &str) -> Result<&'a str> {
    std::str::from_utf8(bytes).with_context(|| format!("{label} was not ASCII"))
}

#[derive(Debug)]
struct RawDiffChange {
    status: Vec<u8>,
    paths: Vec<Vec<u8>>,
}

impl RawDiffChange {
    fn evidence(self) -> GitPathChangeEvidence {
        GitPathChangeEvidence {
            status: String::from_utf8_lossy(&self.status).into_owned(),
            path_bytes_hex: self.paths.into_iter().map(hex::encode).collect(),
        }
    }
}

fn raw_diff_changes(raw: &[u8]) -> Result<Vec<RawDiffChange>> {
    let mut changes = Vec::new();
    let mut cursor = 0;
    while cursor < raw.len() {
        let header_end = raw[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| cursor + offset)
            .ok_or_else(|| anyhow::anyhow!("malformed NUL-delimited git raw diff header"))?;
        let header = &raw[cursor..header_end];
        let status = header
            .rsplit(|byte| *byte == b' ')
            .next()
            .filter(|status| !status.is_empty())
            .ok_or_else(|| anyhow::anyhow!("git raw diff header omitted status"))?;
        cursor = header_end + 1;
        let (path, next) = next_nul_field(raw, cursor)?;
        let mut paths = vec![path.to_vec()];
        cursor = next;
        if matches!(status[0], b'R' | b'C') {
            let (path, next) = next_nul_field(raw, cursor)?;
            paths.push(path.to_vec());
            cursor = next;
        }
        changes.push(RawDiffChange {
            status: status.to_vec(),
            paths,
        });
    }
    Ok(changes)
}

fn raw_diff_paths(raw: &[u8]) -> Result<Vec<Vec<u8>>> {
    Ok(raw_diff_changes(raw)?
        .into_iter()
        .flat_map(|change| change.paths)
        .collect())
}

fn nul_paths(raw: &[u8]) -> Vec<Vec<u8>> {
    raw.split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

fn next_nul_field(raw: &[u8], cursor: usize) -> Result<(&[u8], usize)> {
    let end = raw[cursor..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|offset| cursor + offset)
        .ok_or_else(|| anyhow::anyhow!("malformed NUL-delimited git path"))?;
    Ok((&raw[cursor..end], end + 1))
}

#[cfg(not(target_os = "linux"))]
fn verify_tracked_path_parents(root: &Path, relative: &Path) -> Result<()> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            bail!("git returned unsafe tracked path component");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "tracked verification path has a non-directory parent: {}",
                current.display()
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("inspect tracked verification parent {}", current.display())
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_untracked_path(root: &Path, raw_path: &[u8]) -> Result<()> {
    let relative = safe_relative_git_path(raw_path)?;
    let components = relative.components().collect::<Vec<_>>();
    let root = open_verification_root(root)?;
    let mut directory = root;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            bail!("git returned unsafe untracked path component");
        };
        let entry = match open_verification_entry(&directory, name) {
            Ok(entry) => entry,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).context("open confined untracked verification path"),
        };
        if index + 1 == components.len() {
            remove_confined_verification_entry(&directory, name)?;
            return Ok(());
        }
        if !entry.metadata()?.is_dir() {
            remove_confined_verification_entry(&directory, name)?;
            return Ok(());
        }
        directory = entry;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn remove_untracked_path(root: &Path, raw_path: &[u8]) -> Result<()> {
    let relative = safe_relative_git_path(raw_path)?;
    let mut path = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            bail!("git returned unsafe untracked path component");
        };
        path.push(component);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("inspect {}", path.display()));
            }
        };
        if index + 1 != components.len() {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                continue;
            }
            remove_existing_path(&path)?;
            return Ok(());
        }
        remove_existing_path(&path)?;
    }
    let mut parent = path.parent();
    while let Some(candidate) = parent {
        if candidate == root {
            break;
        }
        if fs::remove_dir(candidate).is_err() {
            break;
        }
        parent = candidate.parent();
    }
    Ok(())
}

fn safe_relative_git_path(raw_path: &[u8]) -> Result<PathBuf> {
    let path = path_from_git_bytes(raw_path)?;
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        bail!("git returned unsafe repository-relative path");
    }
    Ok(path)
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes)?))
}

#[cfg(target_os = "linux")]
fn make_pinned_directory_inheritable(directory: &File) -> Result<()> {
    let flags = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                directory.as_raw_fd(),
                libc::F_SETFD,
                flags & !libc::FD_CLOEXEC,
            )
        } < 0
    {
        return Err(std::io::Error::last_os_error())
            .context("make pinned verification root available to Git subprocesses");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn make_pinned_directory_inheritable(_directory: &File) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn pinned_directory_path(directory: &File, _fallback: &Path) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

#[cfg(not(target_os = "linux"))]
fn pinned_directory_path(_directory: &File, fallback: &Path) -> PathBuf {
    fallback.to_path_buf()
}

fn open_filesystem_object_identity_bytes(file: &File) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    let mut identity = Vec::new();
    #[cfg(unix)]
    {
        identity.extend_from_slice(&metadata.dev().to_be_bytes());
        identity.extend_from_slice(&metadata.ino().to_be_bytes());
        identity.extend_from_slice(&metadata.mode().to_be_bytes());
    }
    #[cfg(not(unix))]
    {
        identity.extend_from_slice(&metadata.len().to_be_bytes());
        identity.push(u8::from(metadata.is_dir()));
        identity.push(u8::from(metadata.is_file()));
    }
    Ok(identity)
}

fn filesystem_object_identity_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("inspect filesystem identity {}", path.display()))?;
    let mut identity = Vec::new();
    #[cfg(unix)]
    {
        identity.extend_from_slice(&metadata.dev().to_be_bytes());
        identity.extend_from_slice(&metadata.ino().to_be_bytes());
        identity.extend_from_slice(&metadata.mode().to_be_bytes());
    }
    #[cfg(not(unix))]
    {
        identity.extend_from_slice(&metadata.len().to_be_bytes());
        identity.push(u8::from(metadata.is_dir()));
        identity.push(u8::from(metadata.is_file()));
    }
    Ok(identity)
}

fn path_location_identity_bytes(path: &Path) -> Vec<u8> {
    let mut identity = path_identity_bytes(path);
    if let Ok(metadata) = fs::metadata(path) {
        #[cfg(unix)]
        {
            identity.extend_from_slice(&metadata.dev().to_be_bytes());
            identity.extend_from_slice(&metadata.ino().to_be_bytes());
            identity.extend_from_slice(&metadata.mode().to_be_bytes());
        }
        #[cfg(not(unix))]
        {
            identity.extend_from_slice(&metadata.len().to_be_bytes());
            identity.push(u8::from(metadata.is_dir()));
            identity.push(u8::from(metadata.is_file()));
        }
    }
    identity
}

#[cfg(unix)]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
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

pub fn worktree_prune(repo_path: impl AsRef<Path>) -> Result<()> {
    run(repo_path, &["worktree", "prune"])?;
    Ok(())
}

pub fn merge(worktree_path: impl AsRef<Path>, branch: &str, message: &str) -> Result<()> {
    run(worktree_path, &["merge", "--no-ff", branch, "-m", message])?;
    Ok(())
}

pub fn merge_abort(worktree_path: impl AsRef<Path>) -> Result<()> {
    run(worktree_path, &["merge", "--abort"])?;
    Ok(())
}

pub fn conflicted_files(worktree_path: impl AsRef<Path>) -> Result<Vec<String>> {
    let output = run(worktree_path, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
pub fn commit_all(dir: impl AsRef<Path>, message: &str) -> Result<()> {
    if status_porcelain(dir.as_ref())?.trim().is_empty() {
        return Ok(());
    }
    run(dir.as_ref(), &["add", "-A"])?;
    run(dir, &["commit", "-m", message])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactPathManifestEntry {
    pub path: PathBuf,
    pub expected_bytes: Vec<u8>,
    pub expected_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactPathManifest {
    pub root_identity: Vec<u8>,
    pub entries: Vec<ExactPathManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactPathCommitEntry {
    pub path_bytes: Vec<u8>,
    pub mode: String,
    pub object_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactPathCommitReceipt {
    pub committed: bool,
    pub commit_sha: String,
    pub parent_sha: String,
    pub tree_sha: String,
    pub staged_path_bytes: Vec<Vec<u8>>,
    pub manifest_entries: Vec<ExactPathCommitEntry>,
}

#[cfg(test)]
static ABANDON_PUBLICATION_AFTER_REF_CAS: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub fn abandon_next_publication_after_ref_cas(repo: &Path) {
    *ABANDON_PUBLICATION_AFTER_REF_CAS.lock().unwrap() = Some(repo.to_path_buf());
}

#[cfg(test)]
static MUTATE_PUBLICATION_AFTER_INDEX_INSTALL: std::sync::Mutex<
    Option<(PathBuf, PathBuf, Vec<u8>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
pub fn mutate_next_publication_after_index_install(repo: &Path, path: &Path, bytes: &[u8]) {
    *MUTATE_PUBLICATION_AFTER_INDEX_INSTALL.lock().unwrap() =
        Some((repo.to_path_buf(), path.to_path_buf(), bytes.to_vec()));
}

#[cfg(test)]
static REWIND_PUBLICATION_AFTER_INDEX_INSTALL: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static PAUSE_PUBLICATION_AFTER_CAPTURE: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<PathBuf, (PathBuf, PathBuf)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(test)]
static PAUSE_PUBLICATION_AFTER_PACKED_REF_COPY: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<PathBuf, (PathBuf, PathBuf)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(test)]
pub fn pause_next_publication_after_packed_ref_copy(repo: &Path, marker: &Path, release: &Path) {
    PAUSE_PUBLICATION_AFTER_PACKED_REF_COPY
        .lock()
        .unwrap()
        .insert(
            repo.to_path_buf(),
            (marker.to_path_buf(), release.to_path_buf()),
        );
}

#[cfg(test)]
pub fn pause_next_publication_after_capture(repo: &Path, marker: &Path, release: &Path) {
    PAUSE_PUBLICATION_AFTER_CAPTURE.lock().unwrap().insert(
        repo.to_path_buf(),
        (marker.to_path_buf(), release.to_path_buf()),
    );
}

#[cfg(test)]
fn pause_publication_after_packed_ref_copy(repository: &PinnedGitRepository) -> Result<()> {
    let Some((marker, release)) = PAUSE_PUBLICATION_AFTER_PACKED_REF_COPY
        .lock()
        .unwrap()
        .remove(repository.root.attached_path())
    else {
        return Ok(());
    };
    fs::write(&marker, b"packed-ref-copied\n")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !release.is_file() {
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting to release packed-ref publication test pause");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(all(test, unix))]
static SUBSTITUTE_PUBLICATION_PARENT_DURING_OPEN: std::sync::Mutex<
    Option<(PathBuf, PathBuf, PathBuf)>,
> = std::sync::Mutex::new(None);

#[cfg(all(test, unix))]
pub fn substitute_next_publication_parent_during_open(
    repo: &Path,
    manifest_path: &Path,
    outside: &Path,
) {
    *SUBSTITUTE_PUBLICATION_PARENT_DURING_OPEN.lock().unwrap() = Some((
        repo.to_path_buf(),
        manifest_path.to_path_buf(),
        outside.to_path_buf(),
    ));
}

#[cfg(test)]
pub fn rewind_next_publication_after_index_install(repo: &Path) {
    *REWIND_PUBLICATION_AFTER_INDEX_INSTALL.lock().unwrap() = Some(repo.to_path_buf());
}

#[derive(Debug)]
struct CapturedPublicationEntry {
    path: PathBuf,
    path_bytes: Vec<u8>,
    bytes: Vec<u8>,
    mode: String,
    object_id: String,
}

#[cfg(test)]
fn pause_publication_after_capture(dir: &Path, entries: &[CapturedPublicationEntry]) -> Result<()> {
    let Some((marker, release)) = PAUSE_PUBLICATION_AFTER_CAPTURE.lock().unwrap().remove(dir)
    else {
        return Ok(());
    };
    fs::write(
        &marker,
        entries
            .first()
            .map(|entry| entry.path_bytes.as_slice())
            .unwrap_or_default(),
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !release.is_file() {
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting to release completion publication test pause");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

fn paths_are_clean_from_root(
    repository: &PinnedGitRepository,
    manifest: &[ExactPathManifestEntry],
) -> Result<bool> {
    let captured = capture_publication_entries_from_root(repository, manifest)?;
    let index =
        index_entries_by_path(&repository.run_bytes(&["ls-files", "--stage", "-v", "-z"])?)?;
    for entry in captured {
        let Some(index_entries) = index.get(&entry.path_bytes) else {
            return Ok(false);
        };
        if index_entries.len() != 1 {
            return Ok(false);
        }
        let metadata = std::str::from_utf8(
            index_entries[0]
                .get(2..)
                .ok_or_else(|| anyhow::anyhow!("publication index entry omitted metadata"))?,
        )?;
        let mut fields = metadata.split_whitespace();
        let mode = fields.next();
        let object_id = fields.next();
        let stage = fields.next();
        if mode != Some(entry.mode.as_str())
            || object_id != Some(entry.object_id.as_str())
            || stage != Some("0")
            || fields.next().is_some()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
pub fn commit_exact_paths(
    dir: impl AsRef<Path>,
    manifest: &ExactPathManifest,
    message: &str,
    expected_head_ref: &str,
) -> Result<ExactPathCommitReceipt> {
    let gate_approval = completion_publication_approval(dir.as_ref())?;
    let gate_publication_identity = completion_publication_root_identity(dir.as_ref())?;
    commit_exact_paths_with_approval(
        dir,
        manifest,
        &gate_approval,
        &gate_publication_identity,
        message,
        expected_head_ref,
    )
}

pub fn commit_exact_paths_with_approval(
    dir: impl AsRef<Path>,
    manifest: &ExactPathManifest,
    gate_approval: &GitWorktreeSnapshotEvidence,
    gate_publication_identity: &[u8],
    message: &str,
    expected_head_ref: &str,
) -> Result<ExactPathCommitReceipt> {
    let requested_dir = dir.as_ref();
    validate_publication_ref(expected_head_ref)?;
    let repository = PinnedGitRepository::open(requested_dir)?;
    repository.require_identity(&manifest.root_identity)?;
    let publication_ref = PinnedGitRef::open(&repository, expected_head_ref)?;
    let manifest = &manifest.entries;
    let mut index_lock = GitIndexLock::acquire_pinned(&repository)?;
    repository
        .ensure_attached()
        .context("completion publication worktree or administrative directory changed while acquiring its index lock")?;
    if repository.identity()? != gate_publication_identity {
        bail!("completion publication root/repository identity diverged from the passed gate");
    }
    let original_index = index_lock.index_bytes()?;
    let head_before = publication_ref.sha(&repository)?;
    if head_before != gate_approval.head_sha {
        bail!(
            "completion publication parent {} diverged from the passed gate-approved parent {}",
            head_before,
            gate_approval.head_sha
        );
    }
    ensure_head_ref(&repository, &publication_ref, &head_before)?;
    let index_entries_before = repository.run_bytes(&["ls-files", "--stage", "-v", "-z"])?;
    let preexisting_index = repository.run_bytes(&[
        "diff-index",
        "--cached",
        "--raw",
        "-z",
        "--no-abbrev",
        &head_before,
        "--",
    ])?;
    if !preexisting_index.is_empty() {
        bail!(
            "completion publication refused a pre-staged index: {}",
            raw_diff_paths(&preexisting_index)?
                .iter()
                .map(hex::encode)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    repository.require_approval(gate_approval, &original_index)?;

    let entries = capture_publication_entries_from_root(&repository, manifest)?;
    #[cfg(test)]
    pause_publication_after_capture(requested_dir, &entries)?;
    index_lock.ensure_attached()?;
    repository
        .ensure_attached()
        .context("completion publication worktree root changed or Git administrative directory changed after manifest capture")?;
    if entries.is_empty() {
        let tree_sha = repository.run(&["rev-parse", &format!("{head_before}^{{tree}}")])?;
        return Ok(ExactPathCommitReceipt {
            committed: false,
            commit_sha: head_before.clone(),
            parent_sha: head_before,
            tree_sha,
            staged_path_bytes: Vec::new(),
            manifest_entries: Vec::new(),
        });
    }
    let allowed = entries
        .iter()
        .map(|entry| entry.path_bytes.clone())
        .collect::<BTreeSet<_>>();

    let temporary = PublicationTemporaryDirectory::create(index_lock.parent())?;
    let temporary_index = temporary.path.join("index");
    index_lock.copy_index_to(&temporary_index)?;
    let mut index_info = Vec::new();
    for entry in &entries {
        index_info.extend_from_slice(entry.mode.as_bytes());
        index_info.push(b' ');
        index_info.extend_from_slice(entry.object_id.as_bytes());
        index_info.push(b'\t');
        index_info.extend_from_slice(&entry.path_bytes);
        index_info.push(0);
    }
    repository.run_with_index_input(
        &temporary_index,
        &["update-index", "--add", "-z", "--index-info"],
        &index_info,
    )?;
    let tree_sha = String::from_utf8(
        strip_command_line_ending(&repository.run_with_index_input(
            &temporary_index,
            &["write-tree"],
            &[],
        )?)
        .to_vec(),
    )
    .context("publication tree object id was not UTF-8")?;
    let mut staged_path_bytes = nul_paths(&repository.run_bytes(&[
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        &head_before,
        &tree_sha,
    ])?);
    staged_path_bytes.sort();
    if !staged_path_bytes.iter().all(|path| allowed.contains(path)) {
        bail!(
            "completion publication tree contains paths outside its manifest: {}",
            staged_path_bytes
                .iter()
                .filter(|path| !allowed.contains(*path))
                .map(hex::encode)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let manifest_entries = publication_receipt_entries(&entries);
    if staged_path_bytes.is_empty() {
        ensure_publication_inputs_unchanged(
            &repository,
            &publication_ref,
            &index_lock,
            &head_before,
            &index_entries_before,
            &entries,
        )?;
        return Ok(ExactPathCommitReceipt {
            committed: false,
            commit_sha: head_before.clone(),
            parent_sha: head_before,
            tree_sha,
            staged_path_bytes,
            manifest_entries,
        });
    }

    let commit_sha = String::from_utf8(
        strip_command_line_ending(
            &repository
                .run_with_input(
                    &["commit-tree", &tree_sha, "-p", &head_before, "-m", message],
                    &[],
                )
                .context("completion publication commit failed")?,
        )
        .to_vec(),
    )
    .context("publication commit object id was not UTF-8")?;
    ensure_publication_inputs_unchanged(
        &repository,
        &publication_ref,
        &index_lock,
        &head_before,
        &index_entries_before,
        &entries,
    )?;
    repository
        .install_quarantined_objects()
        .context("install confined completion publication objects")?;
    repository
        .validate_installed_publication_objects(&commit_sha, &tree_sha, &manifest_entries)
        .context("validate installed completion publication object graph")?;
    let final_index_entries = repository.run_with_index_input(
        &temporary_index,
        &["ls-files", "--stage", "-v", "-z"],
        &[],
    )?;
    let final_index = fs::read(&temporary_index).with_context(|| {
        format!(
            "read prepared completion publication index {}",
            temporary_index.display()
        )
    })?;
    index_lock.prepare(&final_index)?;
    ensure_publication_inputs_unchanged(
        &repository,
        &publication_ref,
        &index_lock,
        &head_before,
        &index_entries_before,
        &entries,
    )?;
    let ref_transaction_cancellation = CancellationToken::new();
    let mut publication_side_effects_before =
        PublicationSideEffectSnapshot::capture(&repository)
            .context("capture pre-ref-transaction publication state")?;
    let update_result = publication_side_effects_before.run_supervised_ref_transaction(
        &ref_transaction_cancellation,
        || {
            publication_ref.update_cas(
                &repository,
                &commit_sha,
                &head_before,
                &ref_transaction_cancellation,
            )
        },
    );
    if let Err(update_err) = update_result {
        if ref_transaction_supervision_failed(&update_err) {
            publication_side_effects_before.retain_leases_after_supervision_failure();
            let restoration =
                publication_side_effects_before.restore(&repository, &entries, false, &index_lock);
            let failure = match restoration {
                Ok(()) => anyhow::anyhow!(
                    "completion publication process supervision failed: {update_err:#}; hook side effects were restored without ref rollback; ignored-file leases remain held until daemon exit"
                ),
                Err(restoration_err) => anyhow::anyhow!(
                    "verification_restoration_failed after completion publication process supervision failure: {restoration_err:#}; supervision failure: {update_err:#}; ignored-file leases remain held until daemon exit"
                ),
            };
            return Err(index_lock.retain_journal_after_failure(failure));
        }
        let rollback = publication_side_effects_before.run_supervised_ref_rollback(
            |rollback_cancellation| {
                publication_ref
                    .refresh_loose_ref()
                    .and_then(|()| publication_ref.sha(&repository))
                    .and_then(|current| {
                        if current == head_before {
                            Ok(())
                        } else if current == commit_sha {
                            publication_ref.restore_original_cas(
                                &repository,
                                &head_before,
                                &commit_sha,
                                rollback_cancellation,
                            )
                        } else {
                            bail!(
                                "completion publication ref entered divergent state {current} after failed compare-and-swap"
                            )
                        }
                    })
            },
        );
        let lease_release =
            publication_side_effects_before.release_failed_leases_after_all_transactions(&rollback);
        let rollback = combine_ref_transaction_and_lease_monitor(rollback, lease_release);
        let restoration = publication_side_effects_before.restore(
            &repository,
            &entries,
            rollback.is_ok(),
            &index_lock,
        );
        if let Err(restoration_err) = restoration {
            let failure = anyhow::anyhow!(
                "verification_restoration_failed after failed completion publication ref transaction: {restoration_err:#}; ref transaction failure: {update_err:#}; ref rollback: {}",
                rollback
                    .as_ref()
                    .err()
                    .map(|err| format!("failed: {err:#}"))
                    .unwrap_or_else(|| "succeeded".to_string())
            );
            return Err(index_lock.retain_journal_after_failure(failure));
        }
        if let Err(rollback_err) = rollback {
            let failure = anyhow::anyhow!(
                "completion publication ref compare-and-swap failed: {update_err:#}; hook side effects were restored but ref rollback failed: {rollback_err:#}"
            );
            return Err(index_lock.retain_journal_after_failure(failure));
        }
        return Err(update_err).context(
            "completion publication ref compare-and-swap failed; ref and hook side effects were restored",
        );
    }
    let post_cas_inputs = ensure_publication_inputs_unchanged(
        &repository,
        &publication_ref,
        &index_lock,
        &commit_sha,
        &index_entries_before,
        &entries,
    );
    let post_cas_snapshot = PublicationSideEffectState::capture(&repository);
    let post_cas_result = post_cas_inputs.and_then(|()| {
        let after = post_cas_snapshot.as_ref().map_err(|err| {
            anyhow::anyhow!("capture post-ref-transaction publication state: {err:#}")
        })?;
        if !publication_side_effects_before.matches(after) {
            bail!("completion publication ref hook changed worktree or local configuration state");
        }
        Ok(())
    });
    if let Err(post_cas_err) = post_cas_result {
        let rollback =
            publication_side_effects_before.run_supervised_ref_rollback(|rollback_cancellation| {
                publication_ref.restore_original_cas(
                    &repository,
                    &head_before,
                    &commit_sha,
                    rollback_cancellation,
                )
            });
        let lease_release =
            publication_side_effects_before.release_failed_leases_after_all_transactions(&rollback);
        let rollback = combine_ref_transaction_and_lease_monitor(rollback, lease_release);
        let restoration = publication_side_effects_before.restore(
            &repository,
            &entries,
            rollback.is_ok(),
            &index_lock,
        );
        if let Err(restoration_err) = restoration {
            let failure = anyhow::anyhow!(
                "verification_restoration_failed after completion publication ref transaction: {restoration_err:#}; original publication failure: {post_cas_err:#}; ref rollback: {}",
                rollback
                    .as_ref()
                    .err()
                    .map(|err| format!("failed: {err:#}"))
                    .unwrap_or_else(|| "succeeded".to_string())
            );
            return Err(index_lock.retain_journal_after_failure(failure));
        }
        match rollback {
            Ok(_) => {
                return Err(post_cas_err).context(
                    "completion publication inputs changed during ref compare-and-swap; ref and hook side effects were rolled back",
                );
            }
            Err(rollback_err) => {
                let failure = anyhow::anyhow!(
                    "completion publication inputs changed after ref update: {post_cas_err:#}; hook side effects were restored but ref rollback failed: {rollback_err:#}"
                );
                return Err(index_lock.retain_journal_after_failure(failure));
            }
        }
    }
    #[cfg(test)]
    let abandon_after_ref_cas = {
        let mut target = ABANDON_PUBLICATION_AFTER_REF_CAS.lock().unwrap();
        if target.as_ref().is_some_and(|repo| repo == requested_dir) {
            target.take();
            true
        } else {
            false
        }
    };
    #[cfg(test)]
    if abandon_after_ref_cas {
        index_lock.abandon_after_ref_update_for_test();
        bail!("simulated process loss after completion publication ref update");
    }
    if let Err(install_err) = index_lock.install() {
        let rollback =
            publication_side_effects_before.run_supervised_ref_rollback(|rollback_cancellation| {
                publication_ref.restore_original_cas(
                    &repository,
                    &head_before,
                    &commit_sha,
                    rollback_cancellation,
                )
            });
        let lease_release =
            publication_side_effects_before.release_failed_leases_after_all_transactions(&rollback);
        let rollback = combine_ref_transaction_and_lease_monitor(rollback, lease_release);
        match rollback {
            Ok(_) => match index_lock.replace_index(&original_index) {
                Ok(()) => {
                    return Err(install_err).context(
                        "completion publication index install failed; ref and index were rolled back",
                    );
                }
                Err(index_err) => {
                    let failure = anyhow::anyhow!(
                        "completion publication index install failed: {install_err:#}; ref rollback succeeded but index rollback failed: {index_err:#}"
                    );
                    return Err(index_lock.retain_journal_after_failure(failure));
                }
            },
            Err(rollback_err) => {
                let failure = anyhow::anyhow!(
                    "completion publication index install failed after ref update: {install_err:#}; ref rollback also failed: {rollback_err:#}"
                );
                return Err(index_lock.retain_journal_after_failure(failure));
            }
        }
    }
    #[cfg(test)]
    {
        let mutation = {
            let mut mutation = MUTATE_PUBLICATION_AFTER_INDEX_INSTALL.lock().unwrap();
            if mutation
                .as_ref()
                .is_some_and(|(repo, _, _)| repo == requested_dir)
            {
                mutation.take()
            } else {
                None
            }
        };
        if let Some((_, path, bytes)) = mutation {
            fs::write(requested_dir.join(path), bytes)?;
        }
        let rewind = {
            let mut target = REWIND_PUBLICATION_AFTER_INDEX_INSTALL.lock().unwrap();
            if target.as_ref().is_some_and(|repo| repo == requested_dir) {
                target.take();
                true
            } else {
                false
            }
        };
        if rewind {
            publication_side_effects_before.run_supervised_ref_rollback(
                |rollback_cancellation| {
                    publication_ref.restore_original_cas(
                        &repository,
                        &head_before,
                        &commit_sha,
                        rollback_cancellation,
                    )
                },
            )?;
        }
    }
    if let Err(final_state_err) = ensure_publication_inputs_unchanged(
        &repository,
        &publication_ref,
        &index_lock,
        &commit_sha,
        &final_index_entries,
        &entries,
    ) {
        let ref_rollback =
            publication_side_effects_before.run_supervised_ref_rollback(|rollback_cancellation| {
                publication_ref.restore_original_cas(
                    &repository,
                    &head_before,
                    &commit_sha,
                    rollback_cancellation,
                )
            });
        let lease_release = publication_side_effects_before
            .release_failed_leases_after_all_transactions(&ref_rollback);
        let ref_rollback = combine_ref_transaction_and_lease_monitor(ref_rollback, lease_release);
        match ref_rollback {
            Ok(_) => match index_lock.replace_index(&original_index) {
                Ok(()) => {
                    return Err(final_state_err).context(
                        "completion publication state changed during index installation; ref and index were rolled back",
                    );
                }
                Err(index_err) => {
                    let failure = anyhow::anyhow!(
                        "completion publication final validation failed: {final_state_err:#}; ref rollback succeeded but index rollback failed: {index_err:#}"
                    );
                    return Err(index_lock.retain_journal_after_failure(failure));
                }
            },
            Err(ref_err) => {
                let failure = anyhow::anyhow!(
                    "completion publication final validation failed: {final_state_err:#}; ref rollback failed: {ref_err:#}; installed index was preserved"
                );
                return Err(index_lock.retain_journal_after_failure(failure));
            }
        }
    }
    Ok(ExactPathCommitReceipt {
        committed: true,
        commit_sha,
        parent_sha: head_before,
        tree_sha,
        staged_path_bytes,
        manifest_entries,
    })
}

pub fn validate_exact_path_receipt_at_ref(
    dir: impl AsRef<Path>,
    expected_head_ref: &str,
    receipt: &ExactPathCommitReceipt,
) -> Result<()> {
    let repository = PinnedGitRepository::open(dir.as_ref())?;
    let publication_ref = PinnedGitRef::open(&repository, expected_head_ref)?;
    validate_exact_path_receipt_at_ref_from_root(&repository, &publication_ref, receipt)
}

fn validate_exact_path_receipt_at_ref_from_root(
    repository: &PinnedGitRepository,
    publication_ref: &PinnedGitRef,
    receipt: &ExactPathCommitReceipt,
) -> Result<()> {
    if !receipt.committed {
        bail!("completion publication receipt did not record a commit");
    }
    let current = publication_ref.sha(repository)?;
    if current != receipt.commit_sha {
        bail!(
            "integration ref advanced beyond completion publication {} to {}; operator reconciliation is required",
            receipt.commit_sha,
            current
        );
    }
    let parents = repository.run(&["rev-list", "--parents", "-n", "1", &receipt.commit_sha])?;
    let parent_fields = parents.split_whitespace().collect::<Vec<_>>();
    if parent_fields.len() != 2
        || parent_fields[0] != receipt.commit_sha
        || parent_fields[1] != receipt.parent_sha
    {
        bail!("completion publication receipt parent does not match its commit");
    }
    if repository.run(&["rev-parse", &format!("{}^{{tree}}", receipt.commit_sha)])?
        != receipt.tree_sha
    {
        bail!("completion publication receipt tree no longer matches its commit");
    }
    let mut changed = nul_paths(&repository.run_bytes(&[
        "diff",
        "--name-only",
        "-z",
        &receipt.parent_sha,
        &receipt.commit_sha,
        "--",
    ])?);
    changed.sort();
    if changed != receipt.staged_path_bytes {
        bail!("completion publication changed-path receipt does not match its commit");
    }
    let mut manifest_paths = BTreeSet::new();
    for entry in &receipt.manifest_entries {
        let path = safe_relative_git_path(&entry.path_bytes)?;
        let (mode, object_id) = commit_path_entry(repository, &receipt.commit_sha, &path)?;
        if mode != entry.mode || object_id != entry.object_id {
            bail!(
                "completion publication manifest receipt does not match commit path {}",
                path.display()
            );
        }
        if !manifest_paths.insert(entry.path_bytes.clone()) {
            bail!("completion publication receipt repeats a manifest path");
        }
    }
    if !receipt
        .staged_path_bytes
        .iter()
        .all(|path| manifest_paths.contains(path))
    {
        bail!("completion publication changed paths escape its manifest receipt");
    }
    Ok(())
}

pub fn exact_path_receipt_blobs(
    dir: impl AsRef<Path>,
    receipt: &ExactPathCommitReceipt,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let repository = PinnedGitRepository::open(dir.as_ref())?;
    let mut blobs = BTreeMap::new();
    for entry in &receipt.manifest_entries {
        let path = safe_relative_git_path(&entry.path_bytes)?;
        let (mode, object_id) = commit_path_entry(&repository, &receipt.commit_sha, &path)?;
        if mode != entry.mode || object_id != entry.object_id {
            bail!(
                "completion publication receipt blob does not match commit path {}",
                path.display()
            );
        }
        let bytes = repository.run_bytes(&["cat-file", "blob", &object_id])?;
        if blobs.insert(entry.path_bytes.clone(), bytes).is_some() {
            bail!("completion publication receipt repeats a blob path");
        }
    }
    Ok(blobs)
}

pub fn validate_exact_path_receipt(
    dir: impl AsRef<Path>,
    manifest: &ExactPathManifest,
    expected_head_ref: &str,
    receipt: &ExactPathCommitReceipt,
) -> Result<()> {
    let requested_dir = dir.as_ref();
    let repository = PinnedGitRepository::open(requested_dir)?;
    repository.require_identity(&manifest.root_identity)?;
    let publication_ref = PinnedGitRef::open(&repository, expected_head_ref)?;
    let manifest = &manifest.entries;
    let index_lock = GitIndexLock::acquire_pinned(&repository)?;
    repository
        .ensure_attached()
        .context("completion publication worktree or administrative directory changed while validating its receipt")?;
    validate_exact_path_receipt_at_ref_from_root(&repository, &publication_ref, receipt)?;
    ensure_head_ref(&repository, &publication_ref, &receipt.commit_sha)?;
    if repository.run(&["rev-parse", &format!("{}^{{tree}}", receipt.commit_sha)])?
        != receipt.tree_sha
    {
        bail!("completion publication receipt tree no longer matches its commit");
    }
    let entries = capture_publication_entries_from_root(&repository, manifest)?;
    if publication_receipt_entries(&entries) != receipt.manifest_entries {
        bail!("completion publication manifest diverged from its receipt");
    }
    let mut changed = nul_paths(&repository.run_bytes(&[
        "diff",
        "--name-only",
        "-z",
        &receipt.parent_sha,
        &receipt.commit_sha,
        "--",
    ])?);
    changed.sort();
    if changed != receipt.staged_path_bytes {
        bail!("completion publication changed-path receipt does not match its commit");
    }
    if !repository
        .run_bytes(&[
            "diff-index",
            "--cached",
            "--name-only",
            "-z",
            &receipt.commit_sha,
            "--",
        ])?
        .is_empty()
    {
        bail!("completion publication index diverged from its receipt");
    }
    if !paths_are_clean_from_root(&repository, manifest)? {
        bail!("completion publication manifest paths diverged from its receipt");
    }
    index_lock.ensure_attached()?;
    Ok(())
}

pub fn find_exact_path_commit(
    dir: impl AsRef<Path>,
    manifest: &ExactPathManifest,
    message: &str,
    expected_head_ref: &str,
) -> Result<Option<ExactPathCommitReceipt>> {
    let requested_dir = dir.as_ref();
    validate_publication_ref(expected_head_ref)?;
    let repository = PinnedGitRepository::open(requested_dir)?;
    repository.require_identity(&manifest.root_identity)?;
    let publication_ref = PinnedGitRef::open(&repository, expected_head_ref)?;
    let manifest = &manifest.entries;
    let mut index_lock = GitIndexLock::acquire_for_recovery(&repository)?;
    repository
        .ensure_attached()
        .context("completion publication worktree or administrative directory changed during recovery lock acquisition")?;
    let current_head = publication_ref.sha(&repository)?;
    ensure_head_ref(&repository, &publication_ref, &current_head)?;
    let index_entries = repository.run_bytes(&["ls-files", "--stage", "-v", "-z"])?;
    let staged = repository.run_bytes(&[
        "diff-index",
        "--cached",
        "--name-only",
        "-z",
        &current_head,
        "--",
    ])?;
    if index_lock.recovered_stale_transaction()
        && !staged.is_empty()
        && index_lock.index_bytes()? == index_lock.prepared_bytes().unwrap_or_default()
    {
        let temporary = PublicationTemporaryDirectory::create(index_lock.parent())?;
        let original_index = temporary.path.join("journaled-original-index");
        fs::write(&original_index, index_lock.original_bytes())?;
        let original_tree = String::from_utf8(
            strip_command_line_ending(&repository.run_with_index_input(
                &original_index,
                &["write-tree"],
                &[],
            )?)
            .to_vec(),
        )?;
        let current_tree = repository.run(&["rev-parse", &format!("{current_head}^{{tree}}")])?;
        if original_tree == current_tree {
            index_lock.replace_index(index_lock.original_bytes())?;
            ensure_head_ref(&repository, &publication_ref, &current_head)?;
            if !repository
                .run_bytes(&[
                    "diff-index",
                    "--cached",
                    "--name-only",
                    "-z",
                    &current_head,
                    "--",
                ])?
                .is_empty()
            {
                bail!("completion publication inverse recovery did not restore a clean index");
            }
            index_lock.complete_recovery();
            return Ok(None);
        }
    }
    let mut allowed = BTreeSet::new();
    let mut expected_paths = BTreeMap::new();
    for entry in manifest {
        validate_manifest_path(&entry.path)?;
        let path_bytes = path_identity_bytes(&entry.path);
        if !allowed.insert(path_bytes.clone())
            || expected_paths.insert(path_bytes, entry.clone()).is_some()
        {
            bail!("completion publication manifest repeats a path");
        }
    }
    let grep = format!("--grep={message}");
    let candidates = repository.run(&[
        "log",
        "--format=%H",
        "--fixed-strings",
        &grep,
        &current_head,
    ])?;
    for commit_sha in candidates.lines().filter(|line| !line.is_empty()) {
        let commit_message =
            String::from_utf8(repository.run_bytes(&["show", "-s", "--format=%B", commit_sha])?)
                .context("publication commit message was not UTF-8")?;
        if commit_message.trim_end() != message {
            continue;
        }
        let parent_line = repository.run(&["rev-list", "--parents", "-n", "1", commit_sha])?;
        let fields = parent_line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 {
            continue;
        }
        let parent_sha = fields[1].to_string();
        let mut staged_path_bytes = nul_paths(&repository.run_bytes(&[
            "diff",
            "--name-only",
            "-z",
            &parent_sha,
            commit_sha,
            "--",
        ])?);
        staged_path_bytes.sort();
        if staged_path_bytes.is_empty()
            || !staged_path_bytes.iter().all(|path| allowed.contains(path))
        {
            continue;
        }
        if current_head != commit_sha {
            bail!(
                "integration ref advanced beyond completion publication {commit_sha} to {current_head}"
            );
        }

        let mut candidate_manifest = Vec::new();
        let mut expected_objects = BTreeMap::new();
        for (path_bytes, entry) in &expected_paths {
            let (mode, object_id) = commit_path_entry(&repository, commit_sha, &entry.path).with_context(
                || {
                    format!(
                        "completion publication commit {commit_sha} was found but expected path {} was absent",
                        entry.path.display()
                    )
                },
            )?;
            candidate_manifest.push(entry.clone());
            expected_objects.insert(path_bytes.clone(), (mode, object_id));
        }
        if !staged_path_bytes
            .iter()
            .all(|path| expected_objects.contains_key(path))
        {
            continue;
        }
        let entries =
            capture_publication_entries_from_root(&repository, &candidate_manifest)
        .with_context(|| {
            format!(
                "completion publication commit {commit_sha} was found but its current manifest could not be captured"
            )
        })?;
        if !entries.iter().all(|entry| {
            expected_objects.get(&entry.path_bytes)
                == Some(&(entry.mode.clone(), entry.object_id.clone()))
        }) {
            bail!(
                "completion publication commit {commit_sha} was found but current manifest bytes or modes diverged"
            );
        }
        if staged.is_empty() && !paths_are_clean_from_root(&repository, &candidate_manifest)? {
            bail!(
                "completion publication commit {commit_sha} was found but its manifest paths are not clean"
            );
        }
        let tree_sha = repository.run(&["rev-parse", &format!("{commit_sha}^{{tree}}")])?;
        if !staged.is_empty() {
            let mut staged_now = nul_paths(&staged);
            staged_now.sort();
            if current_head != commit_sha || staged_now != staged_path_bytes {
                bail!(
                    "completion publication commit {commit_sha} was found but the index contains unrelated staged state"
                );
            }
            repair_publication_index(
                &repository,
                &publication_ref,
                &mut index_lock,
                &parent_sha,
                &tree_sha,
                commit_sha,
            )?;
        } else {
            if index_lock.recovered_stale_transaction()
                && index_lock.index_bytes()? != index_lock.prepared_bytes().unwrap_or_default()
            {
                bail!(
                    "completion publication stale transaction did not leave the exact prepared index"
                );
            }
            ensure_publication_inputs_unchanged(
                &repository,
                &publication_ref,
                &index_lock,
                &current_head,
                &index_entries,
                &entries,
            )?;
        }
        index_lock.complete_recovery();
        return Ok(Some(ExactPathCommitReceipt {
            committed: true,
            commit_sha: commit_sha.to_string(),
            parent_sha,
            tree_sha,
            staged_path_bytes,
            manifest_entries: publication_receipt_entries(&entries),
        }));
    }
    if !staged.is_empty() {
        bail!("completion publication recovery refused a pre-staged index");
    }
    if index_lock.recovered_stale_transaction()
        && index_lock.index_bytes()? != index_lock.original_bytes()
    {
        bail!("completion publication stale transaction left an unknown index state");
    }
    repository
        .ensure_attached()
        .context("completion publication worktree or administrative directory changed during recovery inspection")?;
    ensure_head_ref(&repository, &publication_ref, &current_head)?;
    if repository.run_bytes(&["ls-files", "--stage", "-v", "-z"])? != index_entries {
        bail!("completion publication index changed during recovery inspection");
    }
    index_lock.complete_recovery();
    Ok(None)
}

fn repair_publication_index(
    repository: &PinnedGitRepository,
    publication_ref: &PinnedGitRef,
    index_lock: &mut GitIndexLock,
    expected_parent: &str,
    expected_tree: &str,
    expected_head: &str,
) -> Result<()> {
    if !index_lock.recovered_stale_transaction() {
        bail!(
            "completion publication recovery refused staged state without an exact durable transaction journal"
        );
    }
    let current_index = index_lock.index_bytes()?;
    if current_index != index_lock.original_bytes() {
        bail!("completion publication recovery index did not match the journaled original");
    }
    let temporary = PublicationTemporaryDirectory::create(index_lock.parent())?;
    let original_index = temporary.path.join("original-index");
    fs::write(&original_index, &current_index)?;
    let original_tree = String::from_utf8(
        strip_command_line_ending(&repository.run_with_index_input(
            &original_index,
            &["write-tree"],
            &[],
        )?)
        .to_vec(),
    )?;
    let parent_tree = repository.run(&["rev-parse", &format!("{expected_parent}^{{tree}}")])?;
    if original_tree != parent_tree {
        bail!(
            "completion publication journaled original index did not match parent tree {parent_tree}"
        );
    }
    let prepared = index_lock
        .prepared_bytes()
        .ok_or_else(|| anyhow::anyhow!("publication recovery journal omitted prepared index"))?;
    let prepared_index = temporary.path.join("prepared-index");
    fs::write(&prepared_index, prepared)?;
    let prepared_tree = String::from_utf8(
        strip_command_line_ending(&repository.run_with_index_input(
            &prepared_index,
            &["write-tree"],
            &[],
        )?)
        .to_vec(),
    )?;
    if prepared_tree != expected_tree {
        bail!(
            "completion publication journaled prepared index did not match expected tree {expected_tree}"
        );
    }
    ensure_head_ref(repository, publication_ref, expected_head)?;
    index_lock.install()?;
    if let Err(err) = ensure_head_ref(repository, publication_ref, expected_head) {
        return Err(err).context(
            "completion publication branch changed during recovery; durable index journal was retained",
        );
    }
    if !repository
        .run_bytes(&[
            "diff-index",
            "--cached",
            "--name-only",
            "-z",
            expected_head,
            "--",
        ])?
        .is_empty()
    {
        bail!(
            "completion publication recovery left staged index state; durable index journal was retained"
        );
    }
    Ok(())
}

fn commit_path_entry(
    repository: &PinnedGitRepository,
    commit_sha: &str,
    path: &Path,
) -> Result<(String, String)> {
    let output = repository
        .command()
        .args(["ls-tree", "-z", commit_sha, "--"])
        .arg(path)
        .output()
        .with_context(|| {
            format!(
                "inspect publication path {} at {commit_sha}",
                path.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git ls-tree for publication path {} failed with {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let record = output
        .stdout
        .strip_suffix(&[0])
        .filter(|record| !record.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "publication path {} is absent at {commit_sha}",
                path.display()
            )
        })?;
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| anyhow::anyhow!("malformed git ls-tree publication record"))?;
    if record[tab + 1..] != path_identity_bytes(path) {
        bail!("git ls-tree returned a different publication path");
    }
    let header = std::str::from_utf8(&record[..tab])?;
    let mut fields = header.split_whitespace();
    let mode = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("git ls-tree omitted publication mode"))?;
    let kind = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("git ls-tree omitted publication object type"))?;
    let object_id = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("git ls-tree omitted publication object id"))?;
    if kind != "blob" || fields.next().is_some() {
        bail!("publication path did not resolve to one blob object");
    }
    Ok((mode.to_string(), object_id.to_string()))
}

#[cfg(unix)]
fn open_confined_component(
    parent_fd: libc::c_int,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> Result<File> {
    let name = std::ffi::CString::new(name.as_bytes())?;
    let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open confined publication path");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn publication_directory_names(directory: &File, _attached_path: &Path) -> Result<Vec<OsString>> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let mut names = fs::read_dir(&path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort_by_key(|name| name.as_bytes().to_vec());
    Ok(names)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn publication_directory_names(directory: &File, attached_path: &Path) -> Result<Vec<OsString>> {
    ensure_open_file_matches_path(directory, attached_path)?;
    let mut names = fs::read_dir(attached_path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort_by_key(|name| name.as_bytes().to_vec());
    ensure_open_file_matches_path(directory, attached_path)?;
    Ok(names)
}

struct PinnedPublicationRoot {
    attached_path: PathBuf,
    operation_path: PathBuf,
    directory: File,
}

impl PinnedPublicationRoot {
    fn open(path: &Path) -> Result<Self> {
        let directory = open_publication_root(path)?;
        make_pinned_directory_inheritable(&directory)?;
        let operation_path = pinned_directory_path(&directory, path);
        let root = Self {
            attached_path: path.to_path_buf(),
            operation_path,
            directory,
        };
        root.ensure_attached()?;
        Ok(root)
    }

    fn ensure_attached(&self) -> Result<()> {
        ensure_open_file_matches_path(&self.directory, &self.attached_path)
    }

    fn attached_path(&self) -> &Path {
        &self.attached_path
    }

    fn operation_path(&self) -> &Path {
        &self.operation_path
    }

    fn directory(&self) -> &File {
        &self.directory
    }
}

struct PinnedLooseObject {
    file: File,
    expected_bytes: Vec<u8>,
}

struct PinnedGitRepository {
    root: PinnedPublicationRoot,
    git_dir_path: PathBuf,
    git_dir_operation_path: PathBuf,
    git_dir: File,
    head: File,
    common_dir_path: PathBuf,
    common_dir_operation_path: PathBuf,
    common_dir: File,
    objects_dir_path: PathBuf,
    objects_dir_operation_path: PathBuf,
    objects_dir: File,
    object_fanouts: RefCell<BTreeMap<OsString, File>>,
    installed_objects: RefCell<BTreeMap<(OsString, OsString), PinnedLooseObject>>,
    _object_quarantine: PublicationTemporaryDirectory,
    object_quarantine_path: PathBuf,
    object_quarantine_operation_path: PathBuf,
    object_quarantine_dir: File,
}

impl PinnedGitRepository {
    fn open(path: &Path) -> Result<Self> {
        let root = PinnedPublicationRoot::open(path)?;
        let git_dir_path = absolute_git_directory(root.operation_path(), "--absolute-git-dir")?;
        let common_dir_path = absolute_git_directory(root.operation_path(), "--git-common-dir")?;
        let git_dir = open_publication_root(&git_dir_path).with_context(|| {
            format!(
                "pin Git administrative directory {}",
                git_dir_path.display()
            )
        })?;
        let head = open_admin_leaf(&git_dir, OsStr::new("HEAD"), libc::O_RDONLY, 0)
            .context("pin Git HEAD attachment")?;
        let common_dir = open_publication_root(&common_dir_path).with_context(|| {
            format!(
                "pin Git common administrative directory {}",
                common_dir_path.display()
            )
        })?;
        let objects_dir_path = common_dir_path.join("objects");
        let objects_dir = open_publication_root(&objects_dir_path)
            .with_context(|| format!("pin Git object directory {}", objects_dir_path.display()))?;
        let object_fanouts = pin_existing_object_fanouts(&objects_dir, &objects_dir_path)?;
        make_pinned_directory_inheritable(&git_dir)?;
        make_pinned_directory_inheritable(&common_dir)?;
        make_pinned_directory_inheritable(&objects_dir)?;
        let object_quarantine = PublicationTemporaryDirectory::create(&std::env::temp_dir())?;
        let object_quarantine_path = object_quarantine.path.join("objects");
        fs::create_dir(&object_quarantine_path)?;
        let object_quarantine_dir = open_publication_root(&object_quarantine_path)
            .context("pin completion publication object quarantine")?;
        make_pinned_directory_inheritable(&object_quarantine_dir)?;
        let object_quarantine_operation_path =
            pinned_directory_path(&object_quarantine_dir, &object_quarantine_path);
        let repository = Self {
            git_dir_operation_path: pinned_directory_path(&git_dir, &git_dir_path),
            common_dir_operation_path: pinned_directory_path(&common_dir, &common_dir_path),
            objects_dir_operation_path: pinned_directory_path(&objects_dir, &objects_dir_path),
            root,
            git_dir_path,
            git_dir,
            head,
            common_dir_path,
            common_dir,
            objects_dir_path,
            objects_dir,
            object_fanouts: RefCell::new(object_fanouts),
            installed_objects: RefCell::new(BTreeMap::new()),
            _object_quarantine: object_quarantine,
            object_quarantine_path,
            object_quarantine_operation_path,
            object_quarantine_dir,
        };
        repository.ensure_attached()?;
        let discovered_root = path_from_git_bytes(strip_command_line_ending(
            &repository.run_bytes(&["rev-parse", "--show-toplevel"])?,
        ))?
        .canonicalize()?;
        if filesystem_object_identity_bytes(&discovered_root)?
            != open_filesystem_object_identity_bytes(repository.root.directory())?
        {
            bail!("pinned Git context does not identify the requested worktree root");
        }
        repository.ensure_attached()?;
        Ok(repository)
    }

    fn root(&self) -> &PinnedPublicationRoot {
        &self.root
    }

    #[cfg(unix)]
    fn configured_hooks_root(&self) -> Result<Option<PinnedPublicationRoot>> {
        self.ensure_attached()?;
        let configured = path_from_git_bytes(strip_command_line_ending(&self.run_bytes(&[
            "rev-parse",
            "--git-path",
            "hooks",
        ])?))?;
        if configured.as_os_str().is_empty() {
            bail!("Git resolved an empty hooks path");
        }
        let attached_path = if configured.is_absolute() {
            configured
        } else {
            normalize_absolute_path(self.root.attached_path(), &configured)?
        };
        match fs::symlink_metadata(&attached_path) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.ensure_attached()?;
                return Ok(None);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect configured Git hooks path {}",
                        attached_path.display()
                    )
                });
            }
        }
        let hooks = PinnedPublicationRoot::open(&attached_path).with_context(|| {
            format!("pin configured Git hooks path {}", attached_path.display())
        })?;
        self.ensure_attached()?;
        Ok(Some(hooks))
    }

    fn index_paths(&self) -> (PathBuf, PathBuf) {
        let index_path = self.git_dir_operation_path.join("index");
        let mut lock_name = index_path.as_os_str().to_os_string();
        lock_name.push(".lock");
        (index_path, PathBuf::from(lock_name))
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .env("GIT_DIR", &self.git_dir_operation_path)
            .env("GIT_COMMON_DIR", &self.common_dir_operation_path)
            .env("GIT_WORK_TREE", self.root.operation_path())
            .env(
                "GIT_OBJECT_DIRECTORY",
                &self.object_quarantine_operation_path,
            )
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE")
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                &self.objects_dir_operation_path,
            )
            .current_dir(self.root.operation_path());
        command
    }

    fn run_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self
            .command()
            .args(args)
            .output()
            .with_context(|| format!("run pinned git {}", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let message = if stderr.is_empty() { stdout } else { stderr };
            bail!(
                "pinned git {} failed with {}: {}",
                args.join(" "),
                output.status,
                message
            );
        }
        Ok(output.stdout)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run_bytes(args)?)
            .trim()
            .to_string())
    }

    fn optional_config_value(&self, key: &str) -> Result<Option<String>> {
        self.ensure_attached()?;
        let output = self
            .command()
            .args(["config", "--get", key])
            .output()
            .with_context(|| format!("read pinned Git configuration value {key}"))?;
        self.ensure_attached()?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            ));
        }
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        bail!(
            "read pinned Git configuration value {key} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn run_real_objects(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut command = self.command();
        command
            .env("GIT_OBJECT_DIRECTORY", &self.objects_dir_operation_path)
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
        let output = command
            .args(args)
            .output()
            .with_context(|| format!("run pinned real-object git {}", args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "pinned real-object git {} failed with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    fn validate_installed_publication_objects(
        &self,
        commit_sha: &str,
        tree_sha: &str,
        manifest_entries: &[ExactPathCommitEntry],
    ) -> Result<()> {
        self.ensure_attached()?;
        for object in std::iter::once((commit_sha, "commit"))
            .chain(std::iter::once((tree_sha, "tree")))
            .chain(
                manifest_entries
                    .iter()
                    .map(|entry| (entry.object_id.as_str(), "blob")),
            )
        {
            let expression = format!("{}^{{{}}}", object.0, object.1);
            self.run_real_objects(&["cat-file", "-e", &expression])?;
        }
        self.ensure_attached()
    }

    fn run_with_input(&self, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn pinned git {}", args.join(" ")))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("pinned git stdin unavailable"))?
            .write_all(input)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "pinned git {} failed with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    fn run_with_index_input(&self, index: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
        let mut command = self.command();
        command.env("GIT_INDEX_FILE", index);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn pinned git {} with private index", args.join(" ")))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("pinned git private-index stdin unavailable"))?
            .write_all(input)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "pinned git {} with private index failed with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    fn head_attachment(&self) -> Result<String> {
        self.ensure_attached()?;
        let bytes = read_open_file(&self.head)?;
        let head = std::str::from_utf8(trim_ascii(&bytes))
            .context("publication HEAD attachment was not UTF-8")?;
        let attachment = head
            .strip_prefix("ref: ")
            .ok_or_else(|| anyhow::anyhow!("completion publication requires symbolic HEAD"))?;
        validate_publication_ref(attachment)?;
        Ok(attachment.to_string())
    }

    fn install_quarantined_objects(&self) -> Result<()> {
        #[cfg(unix)]
        {
            self.ensure_attached()?;
            for name in publication_directory_names(
                &self.object_quarantine_dir,
                &self.object_quarantine_path,
            )? {
                let name_bytes = name.as_bytes();
                if name_bytes.len() != 2 || !name_bytes.iter().all(u8::is_ascii_hexdigit) {
                    bail!("publication object quarantine contained an unexpected entry");
                }
                let source = open_confined_component(
                    self.object_quarantine_dir.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
                .context("open pinned publication object quarantine fanout")?;
                let pinned = self
                    .object_fanouts
                    .borrow()
                    .get(&name)
                    .map(File::try_clone)
                    .transpose()?;
                let destination = if let Some(pinned) = pinned {
                    let current = open_confined_component(
                        self.objects_dir.as_raw_fd(),
                        &name,
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    )
                    .context("revalidate pinned Git object fanout before installation")?;
                    if open_filesystem_object_identity_bytes(&pinned)?
                        != open_filesystem_object_identity_bytes(&current)?
                    {
                        bail!("Git object fanout changed after it was pinned");
                    }
                    pinned
                } else {
                    open_or_create_object_fanout(&self.objects_dir, &name)?
                };
                let destination_identity = open_filesystem_object_identity_bytes(&destination)?;
                let mut installed = Vec::new();
                for object_name in
                    publication_directory_names(&source, &self.object_quarantine_path.join(&name))?
                {
                    let object_name_bytes = object_name.as_bytes();
                    if !matches!(object_name_bytes.len(), 38 | 62)
                        || !object_name_bytes.iter().all(u8::is_ascii_hexdigit)
                    {
                        bail!("publication object quarantine contained an invalid loose object");
                    }
                    let source_file = open_admin_leaf(&source, &object_name, libc::O_RDONLY, 0)
                        .context("open pinned quarantined loose object")?;
                    let bytes = read_open_file(&source_file)?;
                    let (file, created) = install_loose_object(&destination, &object_name, &bytes)?;
                    if created {
                        installed.push(object_name.clone());
                    }
                    self.installed_objects.borrow_mut().insert(
                        (name.clone(), object_name),
                        PinnedLooseObject {
                            file,
                            expected_bytes: bytes,
                        },
                    );
                }
                let current = open_confined_component(
                    self.objects_dir.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
                .context("revalidate Git object fanout after installation")?;
                if open_filesystem_object_identity_bytes(&current)? != destination_identity {
                    for object in installed {
                        let _ = unlink_admin_leaf(&destination, &object);
                    }
                    bail!("Git object fanout changed during publication installation");
                }
                destination.sync_all()?;
                self.object_fanouts
                    .borrow_mut()
                    .entry(name)
                    .or_insert(destination);
            }
            self.objects_dir.sync_all()?;
            self.ensure_attached()
        }
        #[cfg(not(unix))]
        {
            bail!("confined Git object installation requires Unix")
        }
    }

    fn ensure_attached(&self) -> Result<()> {
        self.root.ensure_attached()?;
        ensure_open_file_matches_path(&self.git_dir, &self.git_dir_path)
            .context("Git administrative directory changed after it was pinned")?;
        ensure_open_file_matches_admin_leaf(&self.head, &self.git_dir, OsStr::new("HEAD"))
            .context("Git HEAD attachment changed after it was pinned")?;
        ensure_open_file_matches_path(&self.common_dir, &self.common_dir_path)
            .context("Git common administrative directory changed after it was pinned")?;
        ensure_open_file_matches_path(&self.objects_dir, &self.objects_dir_path)
            .context("Git object directory changed after it was pinned")?;
        ensure_open_file_matches_path(&self.object_quarantine_dir, &self.object_quarantine_path)
            .context("Git object quarantine changed after it was pinned")?;
        for (name, fanout) in &*self.object_fanouts.borrow() {
            let current = open_confined_component(
                self.objects_dir.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
            .context("revalidate pinned Git object fanout")?;
            if open_filesystem_object_identity_bytes(fanout)?
                != open_filesystem_object_identity_bytes(&current)?
            {
                bail!("Git object fanout changed after it was pinned");
            }
        }
        let fanouts = self.object_fanouts.borrow();
        for ((fanout_name, object_name), object) in &*self.installed_objects.borrow() {
            let fanout = fanouts
                .get(fanout_name)
                .context("installed Git object fanout was not pinned")?;
            let current = open_admin_leaf(fanout, object_name, libc::O_RDONLY, 0)
                .context("revalidate installed loose Git object")?;
            if open_filesystem_object_identity_bytes(&object.file)?
                != open_filesystem_object_identity_bytes(&current)?
                || read_open_file(&object.file)? != object.expected_bytes
            {
                bail!("installed loose Git object changed after it was validated");
            }
        }
        Ok(())
    }

    fn approval(&self, index_bytes: &[u8]) -> Result<GitWorktreeSnapshotEvidence> {
        let (root_identity, repository_identity) = self.identities()?;
        let head_sha = self.run(&["rev-parse", "--verify", "HEAD"])?;
        let head_attachment = self.head_attachment()?;
        Ok(GitWorktreeSnapshotEvidence {
            digest: String::new(),
            repository_id: hex::encode(Sha256::digest(repository_identity)),
            worktree_id: hex::encode(Sha256::digest(root_identity)),
            head_sha,
            head_attachment,
            index_digest: hex::encode(Sha256::digest(index_bytes)),
            tracked_filesystem_digest: String::new(),
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked_path_bytes_hex: Vec::new(),
            nonignored_empty_directory_path_bytes_hex: Vec::new(),
        })
    }

    fn require_approval(
        &self,
        expected: &GitWorktreeSnapshotEvidence,
        index_bytes: &[u8],
    ) -> Result<()> {
        let current = self.approval(index_bytes)?;
        if current.worktree_id != expected.worktree_id
            || current.repository_id != expected.repository_id
            || current.head_sha != expected.head_sha
            || current.head_attachment != expected.head_attachment
            || current.index_digest != expected.index_digest
        {
            bail!(
                "completion publication worktree, repository, HEAD, or index diverged from the passed gate (worktree={}, repository={}, head={}, attachment={}, index={}, expected_index={}, current_index={})",
                current.worktree_id != expected.worktree_id,
                current.repository_id != expected.repository_id,
                current.head_sha != expected.head_sha,
                current.head_attachment != expected.head_attachment,
                current.index_digest != expected.index_digest,
                expected.index_digest,
                current.index_digest
            );
        }
        Ok(())
    }

    fn identities(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        self.ensure_attached()?;
        let root_identity = open_filesystem_object_identity_bytes(self.root.directory())?;
        let mut repository_identity = Vec::new();
        for identity in [
            open_filesystem_object_identity_bytes(&self.git_dir)?,
            open_filesystem_object_identity_bytes(&self.common_dir)?,
            open_filesystem_object_identity_bytes(&self.objects_dir)?,
        ] {
            repository_identity.extend_from_slice(&(identity.len() as u64).to_be_bytes());
            repository_identity.extend_from_slice(&identity);
        }
        self.ensure_attached()?;
        Ok((root_identity, repository_identity))
    }

    fn identity(&self) -> Result<Vec<u8>> {
        let (root_identity, repository_identity) = self.identities()?;
        let mut identity = Vec::new();
        identity.extend_from_slice(&(root_identity.len() as u64).to_be_bytes());
        identity.extend_from_slice(&root_identity);
        identity.extend_from_slice(&(repository_identity.len() as u64).to_be_bytes());
        identity.extend_from_slice(&repository_identity);
        let head_identity = open_filesystem_object_identity_bytes(&self.head)?;
        identity.extend_from_slice(&(head_identity.len() as u64).to_be_bytes());
        identity.extend_from_slice(&head_identity);
        Ok(identity)
    }

    fn require_identity(&self, expected: &[u8]) -> Result<()> {
        if self.identity()? != expected {
            bail!(
                "completion publication root/repository identity diverged from its pinned manifest"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
static PAUSE_VERIFICATION_BEFORE_INDEX_LOCK: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<PathBuf, (PathBuf, PathBuf)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(test)]
fn pause_next_verification_before_index_lock(repo: &Path, marker: &Path, release: &Path) {
    PAUSE_VERIFICATION_BEFORE_INDEX_LOCK.lock().unwrap().insert(
        repo.to_path_buf(),
        (marker.to_path_buf(), release.to_path_buf()),
    );
}

struct VerificationGitContext {
    repository: std::sync::Mutex<PinnedGitRepository>,
    _baseline_index_file: File,
    baseline_index_bytes: Vec<u8>,
    _baseline_common_config_file: Option<File>,
    _baseline_worktree_config_file: Option<File>,
    baseline_configuration: GitConfigurationSnapshot,
}

impl std::fmt::Debug for VerificationGitContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerificationGitContext")
            .finish_non_exhaustive()
    }
}

fn pin_verification_admin_leaf(
    parent: &File,
    leaf: &OsStr,
) -> Result<(Option<File>, Option<Vec<u8>>)> {
    match open_admin_leaf(parent, leaf, libc::O_RDONLY, 0) {
        Ok(file) => {
            let bytes = read_open_file(&file)?;
            ensure_open_file_matches_admin_leaf(&file, parent, leaf)
                .context("verification Git administrative leaf changed while it was pinned")?;
            Ok((Some(file), Some(bytes)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
        Err(err) => Err(err).context("pin verification Git administrative leaf"),
    }
}

impl VerificationGitContext {
    fn open(root: &Path) -> Result<Self> {
        let root_attachment_path = fs::canonicalize(root)
            .with_context(|| format!("resolve verification root {}", root.display()))?;
        let mut repository = PinnedGitRepository::open(root)?;
        repository.root.attached_path = root_attachment_path;
        let (index_path, _) = repository.index_paths();
        let index_leaf = index_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("verification Git index omitted its leaf"))?;
        let baseline_index_file =
            open_admin_leaf(&repository.git_dir, index_leaf, libc::O_RDONLY, 0)
                .with_context(|| format!("pin verification Git index {}", index_path.display()))?;
        let baseline_index_bytes = read_open_file(&baseline_index_file)?;
        ensure_open_file_matches_admin_leaf(&baseline_index_file, &repository.git_dir, index_leaf)?;
        let (baseline_common_config_file, common) =
            pin_verification_admin_leaf(&repository.common_dir, OsStr::new("config"))?;
        let (baseline_worktree_config_file, worktree) =
            pin_verification_admin_leaf(&repository.git_dir, OsStr::new("config.worktree"))?;
        Ok(Self {
            repository: std::sync::Mutex::new(repository),
            _baseline_index_file: baseline_index_file,
            baseline_index_bytes,
            _baseline_common_config_file: baseline_common_config_file,
            _baseline_worktree_config_file: baseline_worktree_config_file,
            baseline_configuration: GitConfigurationSnapshot { common, worktree },
        })
    }

    fn baseline_index_bytes(&self) -> Vec<u8> {
        self.baseline_index_bytes.clone()
    }

    fn baseline_configuration(&self) -> GitConfigurationSnapshot {
        self.baseline_configuration.clone()
    }

    fn operation_root_path(&self) -> Result<PathBuf> {
        let repository = self.repository()?;
        Ok(repository.root.operation_path().to_path_buf())
    }

    fn repository(&self) -> Result<std::sync::MutexGuard<'_, PinnedGitRepository>> {
        self.repository
            .lock()
            .map_err(|_| anyhow::anyhow!("verification Git context lock was poisoned"))
    }

    fn validate_directories(&self) -> Result<()> {
        let repository = self.repository()?;
        repository.root.ensure_attached()?;
        ensure_open_file_matches_path(&repository.git_dir, &repository.git_dir_path)
            .context("verification Git administrative directory changed after it was pinned")?;
        ensure_open_file_matches_path(&repository.common_dir, &repository.common_dir_path)
            .context("verification Git common directory changed after it was pinned")?;
        ensure_open_file_matches_path(&repository.objects_dir, &repository.objects_dir_path)
            .context("verification Git object directory changed after it was pinned")?;
        ensure_open_file_matches_path(
            &repository.object_quarantine_dir,
            &repository.object_quarantine_path,
        )
        .context("verification Git quarantine changed after it was pinned")
    }

    fn identities(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        self.validate_directories()?;
        let repository = self.repository()?;
        let root_identity = open_filesystem_object_identity_bytes(repository.root.directory())?;
        let mut repository_identity = Vec::new();
        for identity in [
            open_filesystem_object_identity_bytes(&repository.git_dir)?,
            open_filesystem_object_identity_bytes(&repository.common_dir)?,
            open_filesystem_object_identity_bytes(&repository.objects_dir)?,
        ] {
            repository_identity.extend_from_slice(&(identity.len() as u64).to_be_bytes());
            repository_identity.extend_from_slice(&identity);
        }
        drop(repository);
        self.validate_directories()?;
        Ok((root_identity, repository_identity))
    }

    fn configuration_snapshot(&self) -> Result<GitConfigurationSnapshot> {
        self.validate_directories()?;
        let repository = self.repository()?;
        let (_, common) =
            pin_verification_admin_leaf(&repository.common_dir, OsStr::new("config"))?;
        let (_, worktree) =
            pin_verification_admin_leaf(&repository.git_dir, OsStr::new("config.worktree"))?;
        drop(repository);
        self.validate_directories()?;
        Ok(GitConfigurationSnapshot { common, worktree })
    }

    fn restore_configuration(
        &self,
        before: &GitConfigurationSnapshot,
        observed_after: &GitConfigurationSnapshot,
    ) -> Result<()> {
        self.validate_directories()?;
        if self.configuration_snapshot()? != *observed_after {
            bail!("Git configuration changed concurrently; refusing verification restoration");
        }
        let repository = self.repository()?;
        if before.common != observed_after.common {
            restore_admin_leaf_exact(
                &repository.common_dir,
                OsStr::new("config"),
                before.common.as_deref(),
                observed_after.common.as_deref(),
            )
            .context("restore repository-local Git configuration")?;
        }
        if before.worktree != observed_after.worktree {
            restore_admin_leaf_exact(
                &repository.git_dir,
                OsStr::new("config.worktree"),
                before.worktree.as_deref(),
                observed_after.worktree.as_deref(),
            )
            .context("restore worktree-local Git configuration")?;
        }
        drop(repository);
        if self.configuration_snapshot()? != *before {
            bail!("Git configuration restoration did not reproduce the pre-command bytes");
        }
        self.validate_directories()
    }

    fn command(&self) -> Command {
        let mut command = self
            .repository
            .lock()
            .expect("verification Git context lock")
            .command();
        command.args(["-c", "core.hooksPath=/dev/null"]);
        command
    }

    fn run_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.validate_directories()?;
        let output = self.command().args(args).output();
        self.validate_directories()?;
        let output = output.with_context(|| {
            format!("run descriptor-pinned verification git {}", args.join(" "))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let message = if stderr.is_empty() { stdout } else { stderr };
            bail!(
                "descriptor-pinned verification git {} failed with {}: {}",
                args.join(" "),
                output.status,
                message
            );
        }
        Ok(output.stdout)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        String::from_utf8(trim_ascii(&self.run_bytes(args)?).to_vec())
            .context("descriptor-pinned verification Git output was not UTF-8")
    }

    fn run_snapshot_bytes(&self, index_path: &Path, args: &[&str]) -> Result<Vec<u8>> {
        self.validate_directories()?;
        let output = self
            .command()
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .env("GIT_INDEX_FILE", index_path)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output();
        self.validate_directories()?;
        let output = output
            .with_context(|| format!("run descriptor-pinned snapshot git {}", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let message = if stderr.is_empty() { stdout } else { stderr };
            if message.is_empty() {
                bail!(
                    "descriptor-pinned snapshot git {} failed with {}",
                    args.join(" "),
                    output.status
                );
            }
            bail!(
                "descriptor-pinned snapshot git {} failed with {}: {}",
                args.join(" "),
                output.status,
                message
            );
        }
        Ok(output.stdout)
    }

    fn current_index_path(&self) -> Result<PathBuf> {
        self.validate_directories()?;
        let repository = self.repository()?;
        let path = repository.index_paths().0;
        drop(repository);
        self.validate_directories()?;
        Ok(path)
    }

    fn current_index_bytes(&self) -> Result<Vec<u8>> {
        self.validate_directories()?;
        let repository = self.repository()?;
        let (index_path, _) = repository.index_paths();
        let index_leaf = index_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("verification Git index omitted its leaf"))?;
        let index_file = open_admin_leaf(&repository.git_dir, index_leaf, libc::O_RDONLY, 0)
            .with_context(|| {
                format!(
                    "open pinned verification Git index {}",
                    index_path.display()
                )
            })?;
        let bytes = read_open_file(&index_file)?;
        ensure_open_file_matches_admin_leaf(&index_file, &repository.git_dir, index_leaf)
            .context("verification Git index changed while it was read")?;
        drop(repository);
        self.validate_directories()?;
        Ok(bytes)
    }

    #[cfg(test)]
    fn pause_before_index_lock(&self) -> Result<()> {
        let attached_root = self.repository()?.root.attached_path().to_path_buf();
        let Some((marker, release)) = PAUSE_VERIFICATION_BEFORE_INDEX_LOCK
            .lock()
            .unwrap()
            .remove(&attached_root)
        else {
            return Ok(());
        };
        fs::write(&marker, b"before-index-lock\n")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !release.is_file() {
            if std::time::Instant::now() >= deadline {
                bail!("timed out waiting to release verification index-lock test pause");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }

    fn acquire_index_lock(&self) -> Result<GitIndexLock> {
        #[cfg(test)]
        self.pause_before_index_lock()?;
        self.validate_directories()?;
        let repository = self.repository()?;
        let index_lock = GitIndexLock::acquire_pinned(&repository);
        drop(repository);
        self.validate_directories()?;
        index_lock
    }
}

fn absolute_git_directory(root: &Path, argument: &str) -> Result<PathBuf> {
    let bytes = run_bytes(root, &["rev-parse", argument])?;
    let path = path_from_git_bytes(strip_command_line_ending(&bytes))?;
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

struct PinnedGitRef {
    reference: String,
    parent: File,
    leaf: OsString,
    leaf_file: RefCell<Option<File>>,
    initially_loose: bool,
    packed_refs: Option<File>,
    packed_refs_bytes: Option<Vec<u8>>,
}

impl PinnedGitRef {
    fn open(repository: &PinnedGitRepository, reference: &str) -> Result<Self> {
        validate_publication_ref(reference)?;
        let reference_path = Path::new(reference);
        let (parent, leaf) =
            open_pinned_admin_parent(&repository.common_dir, reference_path, "publication ref")?;
        let leaf_file = match open_admin_leaf(&parent, &leaf, libc::O_RDONLY, 0) {
            Ok(file) => Some(file),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("pin loose Git ref"),
        };
        let packed_refs = match open_admin_leaf(
            &repository.common_dir,
            OsStr::new("packed-refs"),
            libc::O_RDONLY,
            0,
        ) {
            Ok(file) => Some(file),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).context("pin packed Git refs"),
        };
        let initially_loose = leaf_file.is_some();
        let packed_refs_bytes = packed_refs.as_ref().map(read_open_file).transpose()?;
        let pinned = Self {
            reference: reference.to_string(),
            parent,
            leaf,
            leaf_file: RefCell::new(leaf_file),
            initially_loose,
            packed_refs,
            packed_refs_bytes,
        };
        pinned.ensure_attached(repository)?;
        Ok(pinned)
    }

    fn ensure_attached(&self, repository: &PinnedGitRepository) -> Result<()> {
        repository.ensure_attached()?;
        let current = open_pinned_admin_parent(
            &repository.common_dir,
            Path::new(&self.reference),
            "publication ref",
        )?
        .0;
        if open_filesystem_object_identity_bytes(&self.parent)?
            != open_filesystem_object_identity_bytes(&current)?
        {
            bail!("Git ref parent directory changed after it was pinned");
        }
        match &*self.leaf_file.borrow() {
            Some(leaf) => ensure_open_file_matches_admin_leaf(leaf, &self.parent, &self.leaf)
                .context("loose Git ref changed after it was pinned")?,
            None => match open_admin_leaf(&self.parent, &self.leaf, libc::O_RDONLY, 0) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => bail!("loose Git ref appeared after ref state was pinned"),
                Err(err) => return Err(err).context("revalidate absent loose Git ref"),
            },
        }
        if let Some(packed_refs) = &self.packed_refs {
            ensure_open_file_matches_admin_leaf(
                packed_refs,
                &repository.common_dir,
                OsStr::new("packed-refs"),
            )
            .context("packed Git refs changed after they were pinned")?;
            if self.packed_refs_bytes.as_deref() != Some(read_open_file(packed_refs)?.as_slice()) {
                bail!("packed Git refs content changed after it was pinned");
            }
        }
        Ok(())
    }

    fn sha(&self, repository: &PinnedGitRepository) -> Result<String> {
        self.ensure_attached(repository)?;
        if let Some(leaf) = &*self.leaf_file.borrow() {
            return validated_ref_sha(trim_ascii(&read_open_file(leaf)?), &self.reference);
        }
        if let Some(packed_refs) = &self.packed_refs {
            let packed = read_open_file(packed_refs)?;
            for line in packed.split(|byte| *byte == b'\n') {
                if line.is_empty() || matches!(line.first(), Some(b'#' | b'^')) {
                    continue;
                }
                let Some(space) = line.iter().position(|byte| *byte == b' ') else {
                    bail!("malformed packed publication ref record");
                };
                if &line[space + 1..] == self.reference.as_bytes() {
                    return validated_ref_sha(&line[..space], &self.reference);
                }
            }
        }
        bail!("publication ref {} is absent", self.reference)
    }

    fn refresh_loose_ref(&self) -> Result<()> {
        match open_admin_leaf(&self.parent, &self.leaf, libc::O_RDONLY, 0) {
            Ok(leaf) => *self.leaf_file.borrow_mut() = Some(leaf),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                *self.leaf_file.borrow_mut() = None
            }
            Err(err) => return Err(err).context("refresh updated loose Git ref"),
        }
        Ok(())
    }

    fn repin_loose_ref(&self) -> Result<()> {
        self.refresh_loose_ref()?;
        if self.leaf_file.borrow().is_none() {
            bail!("updated loose Git ref disappeared");
        }
        Ok(())
    }

    fn restore_original_cas(
        &self,
        repository: &PinnedGitRepository,
        original_sha: &str,
        current_sha: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.initially_loose {
            return self.update_cas(repository, original_sha, current_sha, cancellation);
        }
        validated_ref_sha(original_sha.as_bytes(), &self.reference)?;
        validated_ref_sha(current_sha.as_bytes(), &self.reference)?;
        self.ensure_attached(repository)?;
        #[cfg(unix)]
        {
            run_confined_delete_ref(
                repository,
                &self.parent,
                self.packed_refs_bytes.as_deref(),
                &self.reference,
                current_sha,
                cancellation,
            )?;
            *self.leaf_file.borrow_mut() = None;
            self.ensure_attached(repository)?;
            if self.sha(repository)? != original_sha {
                bail!("packed publication ref was not exposed after loose-ref rollback");
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            repository.run(&["update-ref", "-d", &self.reference, current_sha])?;
            *self.leaf_file.borrow_mut() = None;
            self.ensure_attached(repository)?;
            Ok(())
        }
    }

    fn update_cas(
        &self,
        repository: &PinnedGitRepository,
        new_sha: &str,
        old_sha: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        validated_ref_sha(new_sha.as_bytes(), &self.reference)?;
        validated_ref_sha(old_sha.as_bytes(), &self.reference)?;
        self.ensure_attached(repository)?;
        #[cfg(unix)]
        {
            run_confined_update_ref(
                repository,
                &self.parent,
                self.packed_refs_bytes.as_deref(),
                &self.reference,
                new_sha,
                old_sha,
                cancellation,
            )?;
            self.repin_loose_ref()?;
            if let Err(err) = self.ensure_attached(repository) {
                let rollback = if self.initially_loose {
                    run_confined_update_ref(
                        repository,
                        &self.parent,
                        self.packed_refs_bytes.as_deref(),
                        &self.reference,
                        old_sha,
                        new_sha,
                        cancellation,
                    )
                    .and_then(|()| self.repin_loose_ref())
                } else {
                    run_confined_delete_ref(
                        repository,
                        &self.parent,
                        self.packed_refs_bytes.as_deref(),
                        &self.reference,
                        new_sha,
                        cancellation,
                    )
                    .map(|()| *self.leaf_file.borrow_mut() = None)
                };
                return match rollback {
                    Ok(()) => Err(err).context(
                        "Git ref administrative directory changed during compare-and-swap; ref was rolled back",
                    ),
                    Err(rollback_err) => Err(err).context(format!(
                        "Git ref administrative directory changed during compare-and-swap and detached ref rollback failed: {rollback_err:#}"
                    )),
                };
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            repository.run(&["update-ref", &self.reference, new_sha, old_sha])?;
            self.ensure_attached(repository)?;
            Ok(())
        }
    }
}

fn validated_ref_sha(bytes: &[u8], reference: &str) -> Result<String> {
    if !matches!(bytes.len(), 40 | 64) || !bytes.iter().all(u8::is_ascii_hexdigit) {
        bail!("publication ref {reference} did not contain one object id");
    }
    Ok(std::str::from_utf8(bytes)?.to_ascii_lowercase())
}

#[cfg(unix)]
fn open_pinned_admin_parent(root: &File, relative: &Path, label: &str) -> Result<(File, OsString)> {
    let components = relative.components().collect::<Vec<_>>();
    let Some(Component::Normal(leaf)) = components.last() else {
        bail!("{label} path was empty or unsafe");
    };
    let mut directory = root.try_clone()?;
    for component in &components[..components.len() - 1] {
        let Component::Normal(name) = component else {
            bail!("{label} path contained an unsafe component");
        };
        directory = open_confined_component(
            directory.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .with_context(|| format!("open {label} parent component"))?;
    }
    Ok((directory, leaf.to_os_string()))
}

#[cfg(unix)]
struct CreatedAdminDirectory {
    parent: File,
    name: OsString,
    directory: File,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
struct PinnedAdminParent {
    directory: File,
    created: Vec<CreatedAdminDirectory>,
    committed: bool,
}

#[cfg(unix)]
impl PinnedAdminParent {
    fn rollback_created(&mut self) -> Result<()> {
        while let Some(created) = self.created.last() {
            let current = match open_confined_component(
                created.parent.as_raw_fd(),
                &created.name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            ) {
                Ok(current) => current,
                Err(err)
                    if err
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
                {
                    self.created.pop();
                    continue;
                }
                Err(err) => {
                    return Err(err)
                        .context("reopen operation-created Git administrative directory");
                }
            };
            let metadata = current.metadata()?;
            let held_metadata = created.directory.metadata()?;
            if metadata.dev() != created.device
                || metadata.ino() != created.inode
                || held_metadata.dev() != created.device
                || held_metadata.ino() != created.inode
            {
                bail!("operation-created Git administrative directory changed before cleanup");
            }
            if !publication_directory_names(&current, Path::new("."))?.is_empty() {
                bail!("operation-created Git administrative directory was not empty at cleanup");
            }
            let name = std::ffi::CString::new(created.name.as_bytes())?;
            if unsafe {
                libc::unlinkat(
                    created.parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("remove operation-created Git administrative directory");
            }
            created.parent.sync_all()?;
            self.created.pop();
        }
        Ok(())
    }

    fn commit(mut self) {
        self.committed = true;
        self.created.clear();
    }
}

#[cfg(unix)]
impl Drop for PinnedAdminParent {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.rollback_created();
        }
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn install_owned_admin_directory(parent: &File, name: &OsStr, label: &str) -> Result<(File, bool)> {
    for _ in 0..16 {
        let temporary_name = OsString::from(format!(
            ".khazad-admin-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let temporary_c = std::ffi::CString::new(temporary_name.as_bytes())?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), temporary_c.as_ptr(), 0o777) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(err).with_context(|| format!("create temporary {label} parent component"));
        }
        let temporary = open_confined_component(
            parent.as_raw_fd(),
            &temporary_name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .with_context(|| format!("pin temporary {label} parent component"))?;
        let temporary_metadata = temporary.metadata()?;
        ensure_open_file_matches_admin_leaf(&temporary, parent, &temporary_name)
            .with_context(|| format!("revalidate temporary {label} parent component"))?;
        let name_c = std::ffi::CString::new(name.as_bytes())?;
        let renamed = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.as_raw_fd(),
                temporary_c.as_ptr(),
                parent.as_raw_fd(),
                name_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if renamed != 0 {
            let rename_err = std::io::Error::last_os_error();
            let cleanup = (|| -> Result<()> {
                ensure_open_file_matches_admin_leaf(&temporary, parent, &temporary_name)?;
                if unsafe {
                    libc::unlinkat(parent.as_raw_fd(), temporary_c.as_ptr(), libc::AT_REMOVEDIR)
                } != 0
                {
                    return Err(std::io::Error::last_os_error())
                        .context("remove operation-owned temporary Git administrative directory");
                }
                Ok(())
            })();
            if let Err(cleanup_err) = cleanup {
                return Err(rename_err).context(format!(
                    "install {label} parent component; temporary cleanup also failed: {cleanup_err:#}"
                ));
            }
            if rename_err.kind() == std::io::ErrorKind::AlreadyExists {
                return open_confined_component(
                    parent.as_raw_fd(),
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
                .map(|directory| (directory, false))
                .with_context(|| format!("open concurrently created {label} parent component"));
            }
            return Err(rename_err).with_context(|| format!("install {label} parent component"));
        }
        let installed = open_confined_component(
            parent.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .with_context(|| format!("pin installed {label} parent component"))?;
        let installed_metadata = installed.metadata()?;
        if installed_metadata.dev() != temporary_metadata.dev()
            || installed_metadata.ino() != temporary_metadata.ino()
        {
            bail!("installed {label} parent component changed before ownership was recorded");
        }
        parent.sync_all()?;
        return Ok((installed, true));
    }
    bail!("could not allocate an operation-owned temporary {label} parent component")
}

#[cfg(all(unix, not(target_os = "linux")))]
fn install_owned_admin_directory(
    _parent: &File,
    _name: &OsStr,
    label: &str,
) -> Result<(File, bool)> {
    bail!("atomic creation of {label} parent components requires Linux renameat2")
}

#[cfg(unix)]
fn open_or_create_pinned_admin_parent(
    root: &File,
    relative: &Path,
    label: &str,
) -> Result<PinnedAdminParent> {
    let components = relative.components().collect::<Vec<_>>();
    let Some(Component::Normal(_leaf)) = components.last() else {
        bail!("{label} path was empty or unsafe");
    };
    let mut pinned = PinnedAdminParent {
        directory: root.try_clone()?,
        created: Vec::new(),
        committed: false,
    };
    let creation_result = (|| -> Result<()> {
        for component in &components[..components.len() - 1] {
            let Component::Normal(name) = component else {
                bail!("{label} path contained an unsafe component");
            };
            let parent = pinned.directory.try_clone()?;
            pinned.directory = match open_confined_component(
                parent.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            ) {
                Ok(directory) => directory,
                Err(err)
                    if err
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
                {
                    let (directory, created) = install_owned_admin_directory(&parent, name, label)?;
                    if !created {
                        directory
                    } else {
                        let metadata = directory.metadata()?;
                        pinned.created.push(CreatedAdminDirectory {
                            parent,
                            name: name.to_os_string(),
                            directory,
                            device: metadata.dev(),
                            inode: metadata.ino(),
                        });
                        pinned
                            .created
                            .last()
                            .expect("operation-created administrative directory was recorded")
                            .directory
                            .try_clone()?
                    }
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("open {label} parent component"));
                }
            };
        }
        Ok(())
    })();
    if let Err(creation_err) = creation_result {
        if let Err(cleanup_err) = pinned.rollback_created() {
            bail!(
                "create descriptor-confined {label} hierarchy failed: {creation_err:#}; operation-created directory cleanup also failed: {cleanup_err:#}"
            );
        }
        return Err(creation_err);
    }
    Ok(pinned)
}

#[cfg(unix)]
fn read_pinned_admin_file(
    root: &File,
    _root_path: &Path,
    relative: &Path,
) -> Result<Option<Vec<u8>>> {
    let (directory, leaf) = open_pinned_admin_parent(root, relative, "Git administrative file")?;
    let leaf = std::ffi::CString::new(leaf.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(err).context("open pinned Git administrative file");
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        bail!("pinned Git administrative path was not a regular file");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(not(unix))]
fn read_pinned_admin_file(
    _root: &File,
    root_path: &Path,
    relative: &Path,
) -> Result<Option<Vec<u8>>> {
    let path = root_path.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("inspect {}", path.display())),
    };
    if !metadata.file_type().is_file() {
        bail!("pinned Git administrative path was not a regular file");
    }
    Ok(Some(fs::read(path)?))
}

#[cfg(unix)]
fn run_confined_update_ref(
    repository: &PinnedGitRepository,
    ref_parent: &File,
    packed_refs: Option<&[u8]>,
    reference: &str,
    new_sha: &str,
    old_sha: &str,
    cancellation: &CancellationToken,
) -> Result<()> {
    run_confined_ref_transaction(
        repository,
        ref_parent,
        packed_refs,
        reference,
        Some(new_sha),
        old_sha,
        cancellation,
    )
}

#[cfg(unix)]
fn run_confined_delete_ref(
    repository: &PinnedGitRepository,
    ref_parent: &File,
    packed_refs: Option<&[u8]>,
    reference: &str,
    old_sha: &str,
    cancellation: &CancellationToken,
) -> Result<()> {
    run_confined_ref_transaction(
        repository,
        ref_parent,
        packed_refs,
        reference,
        None,
        old_sha,
        cancellation,
    )
}

#[cfg(unix)]
fn effective_ref_updates_are_logged(
    repository: &PinnedGitRepository,
    reference: &str,
) -> Result<bool> {
    let log_reference = Path::new("logs").join(reference);
    match read_pinned_admin_file(
        &repository.common_dir,
        &repository.common_dir_path,
        &log_reference,
    ) {
        Ok(Some(_)) => return Ok(true),
        Ok(None) => {}
        Err(err) if error_chain_has_io_kind(&err, std::io::ErrorKind::NotFound) => {}
        Err(err) => return Err(err).context("inspect existing publication reflog"),
    }
    let configured = repository.optional_config_value("core.logAllRefUpdates")?;
    match configured.as_deref().map(str::trim) {
        Some(value)
            if value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
                || value.eq_ignore_ascii_case("off")
                || value == "0" =>
        {
            Ok(false)
        }
        Some(value) if value.eq_ignore_ascii_case("always") => Ok(true),
        Some(value)
            if value.is_empty()
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
                || value == "1" =>
        {
            Ok(reference.starts_with("refs/heads/")
                || reference.starts_with("refs/remotes/")
                || reference.starts_with("refs/notes/")
                || reference == "HEAD")
        }
        Some(value) => bail!("unsupported effective core.logAllRefUpdates value: {value}"),
        None => Ok(
            repository.run(&["rev-parse", "--is-bare-repository"])? == "false"
                && (reference.starts_with("refs/heads/")
                    || reference.starts_with("refs/remotes/")
                    || reference.starts_with("refs/notes/")
                    || reference == "HEAD"),
        ),
    }
}

#[cfg(unix)]
fn run_confined_ref_transaction(
    repository: &PinnedGitRepository,
    ref_parent: &File,
    packed_refs: Option<&[u8]>,
    reference: &str,
    new_sha: Option<&str>,
    old_sha: &str,
    cancellation: &CancellationToken,
) -> Result<()> {
    make_pinned_directory_inheritable(ref_parent)?;
    let temporary = PublicationTemporaryDirectory::create(&std::env::temp_dir())?;
    let common = temporary.path.join("common");
    let git_dir = temporary.path.join("git");
    fs::create_dir(&common)?;
    fs::create_dir(&git_dir)?;
    let head = read_open_file(&repository.head)?;
    fs::write(git_dir.join("HEAD"), head)?;
    fs::write(git_dir.join("commondir"), b"../common\n")?;
    if let Some(config) = read_pinned_admin_file(
        &repository.common_dir,
        &repository.common_dir_path,
        Path::new("config"),
    )? {
        fs::write(common.join("config"), config)?;
    }
    if let Some(config) = read_pinned_admin_file(
        &repository.git_dir,
        &repository.git_dir_path,
        Path::new("config.worktree"),
    )? {
        fs::write(git_dir.join("config.worktree"), config)?;
    }
    if let Some(packed_refs) = packed_refs {
        fs::write(common.join("packed-refs"), packed_refs)?;
    }
    #[cfg(test)]
    pause_publication_after_packed_ref_copy(repository)?;

    let reference_path = Path::new(reference);
    let reference_parent = reference_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("publication ref omitted its parent"))?;
    link_pinned_admin_parent(&common, reference_parent, ref_parent)?;

    let log_reference = Path::new("logs").join(reference_path);
    let mut log_parent = if effective_ref_updates_are_logged(repository, reference)? {
        Some(open_or_create_pinned_admin_parent(
            &repository.common_dir,
            &log_reference,
            "publication reflog",
        )?)
    } else {
        None
    };

    let transaction_result = (|| -> Result<()> {
        if let Some(parent) = &log_parent {
            make_pinned_directory_inheritable(&parent.directory)?;
            link_pinned_admin_parent(
                &common,
                log_reference
                    .parent()
                    .expect("publication reflog has a parent"),
                &parent.directory,
            )?;
        }

        let hooks = repository.configured_hooks_root()?;
        let empty_hooks_path = common.join("hooks");
        let hooks_path = if let Some(hooks) = &hooks {
            make_pinned_directory_inheritable(hooks.directory())?;
            hooks.operation_path().to_path_buf()
        } else {
            fs::create_dir(&empty_hooks_path)?;
            empty_hooks_path
        };

        let mut environment = BTreeMap::new();
        environment.insert(
            OsString::from("GIT_DIR"),
            git_dir.as_os_str().to_os_string(),
        );
        environment.insert(
            OsString::from("GIT_COMMON_DIR"),
            common.as_os_str().to_os_string(),
        );
        environment.insert(
            OsString::from("GIT_WORK_TREE"),
            repository.root.operation_path().as_os_str().to_os_string(),
        );
        environment.insert(
            OsString::from("GIT_OBJECT_DIRECTORY"),
            repository
                .object_quarantine_operation_path
                .as_os_str()
                .to_os_string(),
        );
        environment.insert(
            OsString::from("GIT_ALTERNATE_OBJECT_DIRECTORIES"),
            repository
                .objects_dir_operation_path
                .as_os_str()
                .to_os_string(),
        );
        environment.insert(
            OsString::from("GIT_NO_REPLACE_OBJECTS"),
            OsString::from("1"),
        );
        environment.insert(
            OsString::from("KHAZAD_HOOKS_PATH"),
            hooks_path.as_os_str().to_os_string(),
        );
        environment.insert(
            OsString::from("KHAZAD_PUBLICATION_REF_TRANSACTION"),
            OsString::from("1"),
        );
        let mut command = "git -c core.hooksPath=\"$KHAZAD_HOOKS_PATH\"".to_string();
        command.push_str(" update-ref");
        if new_sha.is_none() {
            command.push_str(" -d");
        }
        command.push(' ');
        command.push_str(&shell_single_quote(reference));
        if let Some(new_sha) = new_sha {
            command.push(' ');
            command.push_str(&shell_single_quote(new_sha));
        }
        command.push(' ');
        command.push_str(&shell_single_quote(old_sha));
        let output = ShellCommand::new(repository.root.operation_path(), command)
            .pinned_cwd(repository.root.directory())?
            .envs_os(environment)
            .env_remove(&[OsStr::new("GIT_INDEX_FILE"), OsStr::new("GIT_NAMESPACE")])
            .timeout(std::time::Duration::from_secs(600))
            .run(cancellation)
            .context("supervise confined completion publication ref compare-and-swap")?;
        if !output.success() {
            bail!(
                "confined completion publication update-ref failed with {:?}: {}",
                output.exit_code(),
                output.trimmed_combined_output()
            );
        }
        if let Some(hooks) = &hooks {
            hooks
                .ensure_attached()
                .context("configured Git hooks directory changed during ref transaction")?;
        }
        let current = read_pinned_admin_file(
            ref_parent,
            reference_parent,
            Path::new(
                reference_path
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("publication ref omitted its leaf"))?,
            ),
        )?;
        match (new_sha, current) {
            (Some(new_sha), Some(current))
                if validated_ref_sha(trim_ascii(&current), reference)? == new_sha => {}
            (Some(_), _) => {
                bail!("confined completion publication ref did not install the requested object id")
            }
            (None, None) => {}
            (None, Some(_)) => {
                bail!("confined completion publication ref deletion did not complete")
            }
        }
        Ok(())
    })();

    match transaction_result {
        Ok(()) => {
            if let Some(parent) = log_parent.take() {
                parent.commit();
            }
            Ok(())
        }
        Err(transaction_err) => {
            if let Some(parent) = &mut log_parent
                && let Err(cleanup_err) = parent.rollback_created()
            {
                return Err(transaction_err).context(format!(
                    "confined completion publication ref transaction failed; operation-created reflog cleanup also failed: {cleanup_err:#}"
                ));
            }
            Err(transaction_err)
        }
    }
}

#[cfg(unix)]
fn normalize_absolute_path(base: &Path, relative: &Path) -> Result<PathBuf> {
    if !base.is_absolute() {
        bail!(
            "configured Git path base was not absolute: {}",
            base.display()
        );
    }
    let mut normalized = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!(
                        "configured Git path escapes the filesystem root: {}",
                        relative.display()
                    );
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "configured Git path was not relative: {}",
                    relative.display()
                );
            }
        }
    }
    Ok(normalized)
}

#[cfg(unix)]
fn link_pinned_admin_parent(overlay: &Path, relative: &Path, directory: &File) -> Result<()> {
    let parent = relative
        .parent()
        .ok_or_else(|| anyhow::anyhow!("pinned administrative path omitted its parent"))?;
    fs::create_dir_all(overlay.join(parent))?;
    std::os::unix::fs::symlink(
        pinned_directory_path(directory, relative),
        overlay.join(relative),
    )?;
    Ok(())
}

pub fn completion_publication_root_identity(dir: impl AsRef<Path>) -> Result<Vec<u8>> {
    PinnedGitRepository::open(dir.as_ref())?.identity()
}

#[cfg(test)]
pub fn completion_publication_approval(
    dir: impl AsRef<Path>,
) -> Result<GitWorktreeSnapshotEvidence> {
    let repository = PinnedGitRepository::open(dir.as_ref())?;
    let index_lock = GitIndexLock::acquire_pinned(&repository)?;
    let index_bytes = index_lock.index_bytes()?;
    repository.approval(&index_bytes)
}

#[cfg(unix)]
pub(crate) fn open_pinned_directory_nofollow(root: &Path) -> Result<File> {
    open_publication_root(root)
}

#[cfg(unix)]
fn open_publication_root(root: &Path) -> Result<File> {
    let components = root.components().collect::<Vec<_>>();
    let mut next_component = 0usize;
    #[cfg(target_os = "linux")]
    let descriptor_prefix = {
        let bytes = root.as_os_str().as_bytes();
        bytes.strip_prefix(b"/proc/self/fd/").and_then(|suffix| {
            let length = suffix
                .iter()
                .position(|byte| *byte == b'/')
                .unwrap_or(suffix.len());
            let descriptor = &suffix[..length];
            (!descriptor.is_empty() && descriptor.iter().all(u8::is_ascii_digit))
                .then(|| {
                    std::str::from_utf8(descriptor)
                        .ok()?
                        .parse::<libc::c_int>()
                        .ok()
                        .map(|fd| (fd, length))
                })
                .flatten()
        })
    };
    #[cfg(not(target_os = "linux"))]
    let descriptor_prefix: Option<(libc::c_int, usize)> = None;

    let mut directory = if let Some((fd, descriptor_length)) = descriptor_prefix {
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicated < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("duplicate pinned directory {}", root.display()));
        }
        // / proc self fd N are the first five components. Any remaining suffix is
        // traversed from the duplicated descriptor rather than through procfs.
        next_component = 5;
        if root.as_os_str().as_bytes().len() > b"/proc/self/fd/".len() + descriptor_length {
            next_component = 5;
        }
        unsafe { File::from_raw_fd(duplicated) }
    } else {
        let start = if root.is_absolute() { c"/" } else { c"." };
        let fd = unsafe {
            libc::open(
                start.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open publication path anchor for {}", root.display()));
        }
        unsafe { File::from_raw_fd(fd) }
    };

    for component in components.into_iter().skip(next_component) {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                directory = open_confined_component(
                    directory.as_raw_fd(),
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
                .with_context(|| {
                    format!(
                        "open publication directory component {:?} in {}",
                        name,
                        root.display()
                    )
                })?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                bail!(
                    "publication directory path contains an unsafe component: {}",
                    root.display()
                );
            }
        }
    }
    if !directory.metadata()?.is_dir() {
        bail!("publication path is not a directory: {}", root.display());
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_publication_root(root: &Path) -> Result<File> {
    File::open(root).with_context(|| format!("open publication worktree {}", root.display()))
}

#[cfg(unix)]
fn read_confined_regular_file(
    _root: &Path,
    root_directory: &File,
    path: &Path,
) -> Result<(Vec<u8>, fs::Metadata)> {
    validate_manifest_path(path)?;
    let mut directory = root_directory
        .try_clone()
        .context("duplicate pinned publication worktree")?;
    let components = path.components().collect::<Vec<_>>();
    for component in &components[..components.len() - 1] {
        let Component::Normal(name) = component else {
            bail!("completion publication path contains an unsafe component");
        };
        directory = open_confined_component(
            directory.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .with_context(|| {
            format!(
                "completion publication path has a non-directory parent: {}",
                path.display()
            )
        })?;
    }
    #[cfg(test)]
    {
        let substitution = {
            let mut substitution = SUBSTITUTE_PUBLICATION_PARENT_DURING_OPEN.lock().unwrap();
            if substitution
                .as_ref()
                .is_some_and(|(repo, manifest, _)| repo == _root && manifest == path)
            {
                substitution.take()
            } else {
                None
            }
        };
        if let Some((_, _, outside)) = substitution {
            let parent = _root.join(path.parent().expect("validated publication parent"));
            let file_name = parent
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("publication parent omitted its name"))?;
            let mut parked_name = file_name.to_os_string();
            parked_name.push(format!(".khazad-swap-{}", std::process::id()));
            let parked = parent.with_file_name(parked_name);
            fs::rename(&parent, &parked)?;
            std::os::unix::fs::symlink(outside, &parent)?;
        }
    }
    let Component::Normal(name) = components.last().expect("validated nonempty path") else {
        bail!("completion publication path contains an unsafe component");
    };
    let mut file = open_confined_component(
        directory.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
    .with_context(|| format!("open completion publication path {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect publication path {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "completion publication manifest entry is not a regular file: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read publication path {}", path.display()))?;
    Ok((bytes, metadata))
}

#[cfg(not(unix))]
fn read_confined_regular_file(
    root: &Path,
    _root_directory: &File,
    path: &Path,
) -> Result<(Vec<u8>, fs::Metadata)> {
    validate_manifest_path(path)?;
    verify_tracked_path_parents(root, path)
        .with_context(|| format!("confine completion publication path {}", path.display()))?;
    let absolute = root.join(path);
    let metadata = fs::symlink_metadata(&absolute)
        .with_context(|| format!("inspect publication path {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "completion publication manifest entry is not a regular file: {}",
            path.display()
        );
    }
    let bytes =
        fs::read(&absolute).with_context(|| format!("read publication path {}", path.display()))?;
    Ok((bytes, metadata))
}

fn capture_publication_entries_from_root(
    repository: &PinnedGitRepository,
    manifest: &[ExactPathManifestEntry],
) -> Result<Vec<CapturedPublicationEntry>> {
    repository
        .ensure_attached()
        .context("completion publication worktree or administrative directory changed during manifest capture")?;
    let publication_root = repository.root();
    let mut unique = BTreeMap::new();
    for expected in manifest {
        validate_manifest_path(&expected.path)?;
        let path_bytes = path_identity_bytes(&expected.path);
        if unique.insert(path_bytes, expected).is_some() {
            bail!("completion publication manifest repeats a path");
        }
    }
    let mut entries = Vec::with_capacity(unique.len());
    for (path_bytes, expected) in unique {
        let path = &expected.path;
        let (bytes, metadata) = read_confined_regular_file(
            publication_root.attached_path(),
            publication_root.directory(),
            path,
        )?;
        let mode = publication_file_mode(&metadata);
        if bytes != expected.expected_bytes || mode != expected.expected_mode {
            bail!(
                "completion publication path diverged from its pinned semantic manifest bytes or mode: {}",
                path.display()
            );
        }
        let object_id = hash_publication_bytes(repository, path, &bytes)?;
        entries.push(CapturedPublicationEntry {
            path: path.clone(),
            path_bytes,
            bytes,
            mode,
            object_id,
        });
    }
    Ok(entries)
}

fn publication_receipt_entries(entries: &[CapturedPublicationEntry]) -> Vec<ExactPathCommitEntry> {
    entries
        .iter()
        .map(|entry| ExactPathCommitEntry {
            path_bytes: entry.path_bytes.clone(),
            mode: entry.mode.clone(),
            object_id: entry.object_id.clone(),
        })
        .collect()
}

fn ensure_publication_inputs_unchanged(
    repository: &PinnedGitRepository,
    publication_ref: &PinnedGitRef,
    index_lock: &GitIndexLock,
    expected_head: &str,
    expected_index_entries: &[u8],
    entries: &[CapturedPublicationEntry],
) -> Result<()> {
    repository.ensure_attached().context(
        "completion publication worktree or administrative directory changed during publication",
    )?;
    index_lock.ensure_attached()?;
    ensure_head_ref(repository, publication_ref, expected_head)?;
    let index_entries = repository.run_bytes(&["ls-files", "--stage", "-v", "-z"])?;
    if index_entries != expected_index_entries {
        bail!("completion publication index changed while preparing the commit");
    }
    for entry in entries {
        let (bytes, metadata) = read_confined_regular_file(
            repository.root().attached_path(),
            repository.root().directory(),
            &entry.path,
        )
        .with_context(|| format!("reconfine publication path {}", entry.path.display()))?;
        if publication_file_mode(&metadata) != entry.mode || bytes != entry.bytes {
            bail!(
                "completion publication manifest path changed while preparing the commit: {}",
                entry.path.display()
            );
        }
    }
    index_lock.ensure_attached()?;
    Ok(())
}

fn ensure_head_ref(
    repository: &PinnedGitRepository,
    publication_ref: &PinnedGitRef,
    expected_head: &str,
) -> Result<()> {
    let attachment = repository.head_attachment()?;
    if attachment != publication_ref.reference {
        bail!(
            "completion publication HEAD attachment changed: expected {}, found {attachment}",
            publication_ref.reference
        );
    }
    let current_head = publication_ref
        .sha(repository)
        .context("completion publication HEAD changed")?;
    if current_head != expected_head {
        bail!(
            "completion publication HEAD changed: expected {expected_head}, found {current_head}"
        );
    }
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn validate_publication_ref(expected_head_ref: &str) -> Result<()> {
    if !expected_head_ref.starts_with("refs/heads/") || expected_head_ref == "refs/heads/" {
        bail!(
            "completion publication requires an explicit branch ref, found {expected_head_ref:?}"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn publication_file_mode(metadata: &fs::Metadata) -> String {
    if metadata.permissions().mode() & 0o111 == 0 {
        "100644".to_string()
    } else {
        "100755".to_string()
    }
}

#[cfg(not(unix))]
fn publication_file_mode(_metadata: &fs::Metadata) -> String {
    "100644".to_string()
}

fn hash_publication_bytes(
    repository: &PinnedGitRepository,
    path: &Path,
    bytes: &[u8],
) -> Result<String> {
    let mut command = repository.command();
    command.env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    let mut child = command
        .arg("hash-object")
        .arg("-w")
        .arg("--no-filters")
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn git hash-object for {}", path.display()))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("git hash-object stdin unavailable"))?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git hash-object for {} failed with {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(strip_command_line_ending(&output.stdout).to_vec())
        .context("publication blob object id was not UTF-8")
}

struct PublicationTemporaryDirectory {
    path: PathBuf,
}

impl PublicationTemporaryDirectory {
    fn create(parent: &Path) -> Result<Self> {
        for _ in 0..16 {
            let path = parent.join(format!(
                ".khazad-publication-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            #[cfg(unix)]
            let create = {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&path)
            };
            #[cfg(not(unix))]
            let create = fs::create_dir(&path);
            match create {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "create completion publication temporary directory {}",
                            path.display()
                        )
                    });
                }
            }
        }
        bail!("could not allocate a unique completion publication temporary directory")
    }
}

impl Drop for PublicationTemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const INDEX_TRANSACTION_MAGIC: &[u8] = b"KHAZAD-INDEX-TRANSACTION-V1\0";

#[cfg(test)]
pub(crate) fn retain_test_completion_publication_journal(dir: &Path) -> Result<()> {
    let mut lock = GitIndexLock::acquire(dir)?;
    let current = lock.index_bytes()?;
    lock.prepare(&current)?;
    lock.retain_journal()
}

#[cfg(test)]
pub(crate) fn retain_test_process_loss_completion_publication_journal(
    dir: &Path,
    dead_owner_pid: u32,
) -> Result<()> {
    if dead_owner_pid == 0 || dead_owner_pid == u32::MAX || process_is_alive(dead_owner_pid) {
        bail!("process-loss publication test owner must be a dead real PID");
    }
    let mut lock = GitIndexLock::acquire(dir)?;
    let current = lock.index_bytes()?;
    lock.prepare(&current)?;
    lock.file
        .seek(SeekFrom::Start(INDEX_TRANSACTION_MAGIC.len() as u64))?;
    lock.file.write_all(&dead_owner_pid.to_be_bytes())?;
    lock.file.sync_all()?;
    lock.parent.sync_all()?;
    lock.remove_on_drop = false;
    Ok(())
}

struct GitIndexLock {
    parent: File,
    index_leaf: OsString,
    lock_leaf: OsString,
    index_path: PathBuf,
    index_file: RefCell<File>,
    file: File,
    original: Vec<u8>,
    prepared: Option<Vec<u8>>,
    recovered_stale_transaction: bool,
    remove_on_drop: bool,
}

impl GitIndexLock {
    #[cfg(test)]
    fn acquire(dir: &Path) -> Result<Self> {
        let (index_path, lock_path) = git_index_paths(dir)?;
        let parent_path = index_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Git index omitted its parent"))?;
        let parent = open_publication_root(parent_path)?;
        Self::acquire_parent(parent, index_path, lock_path)
    }

    fn acquire_pinned(repository: &PinnedGitRepository) -> Result<Self> {
        let (index_path, lock_path) = repository.index_paths();
        Self::acquire_parent(repository.git_dir.try_clone()?, index_path, lock_path)
    }

    fn acquire_parent(parent: File, index_path: PathBuf, lock_path: PathBuf) -> Result<Self> {
        let index_leaf = index_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Git index omitted its leaf"))?
            .to_os_string();
        let lock_leaf = lock_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Git index lock omitted its leaf"))?
            .to_os_string();
        let file = open_admin_leaf(
            &parent,
            &lock_leaf,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o666,
        )
        .with_context(|| {
            format!(
                "acquire completion publication index lock {}",
                lock_path.display()
            )
        })?;
        let index_file = open_admin_leaf(&parent, &index_leaf, libc::O_RDONLY, 0)
            .with_context(|| format!("open locked git index {}", index_path.display()))?;
        let original = read_open_file(&index_file)
            .with_context(|| format!("read locked git index {}", index_path.display()))?;
        Ok(Self {
            parent,
            index_leaf,
            lock_leaf,
            index_path,
            index_file: RefCell::new(index_file),
            file,
            original,
            prepared: None,
            recovered_stale_transaction: false,
            remove_on_drop: true,
        })
    }

    fn acquire_for_recovery(repository: &PinnedGitRepository) -> Result<Self> {
        let (index_path, lock_path) = repository.index_paths();
        let parent = repository.git_dir.try_clone()?;
        let index_leaf = index_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Git index omitted its leaf"))?
            .to_os_string();
        let lock_leaf = lock_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Git index lock omitted its leaf"))?
            .to_os_string();
        match open_admin_leaf(
            &parent,
            &lock_leaf,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o666,
        ) {
            Ok(file) => {
                let index_file = open_admin_leaf(&parent, &index_leaf, libc::O_RDONLY, 0)
                    .with_context(|| format!("open locked git index {}", index_path.display()))?;
                let original = read_open_file(&index_file)
                    .with_context(|| format!("read locked git index {}", index_path.display()))?;
                Ok(Self {
                    parent,
                    index_leaf,
                    lock_leaf,
                    index_path,
                    index_file: RefCell::new(index_file),
                    file,
                    original,
                    prepared: None,
                    recovered_stale_transaction: false,
                    remove_on_drop: true,
                })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut file = open_admin_leaf(&parent, &lock_leaf, libc::O_RDWR, 0)?;
                claim_stale_index_transaction(&file).with_context(|| {
                    format!(
                        "claim completion publication recovery journal {}",
                        lock_path.display()
                    )
                })?;
                ensure_open_file_matches_admin_leaf(&file, &parent, &lock_leaf).with_context(|| {
                    format!(
                        "completion publication recovery journal changed while it was claimed: {}",
                        lock_path.display()
                    )
                })?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).with_context(|| {
                    format!(
                        "read completion publication journal {}",
                        lock_path.display()
                    )
                })?;
                let (owner_pid, original, prepared) = parse_index_transaction(&bytes)
                    .with_context(|| {
                        format!(
                            "existing index lock {} is not a recoverable Khazad-Doom publication transaction",
                            lock_path.display()
                        )
                    })?;
                if process_is_alive(owner_pid) {
                    bail!(
                        "completion publication index transaction owned by live process {owner_pid}"
                    );
                }
                let index_file = open_admin_leaf(&parent, &index_leaf, libc::O_RDONLY, 0)
                    .with_context(|| format!("open locked git index {}", index_path.display()))?;
                Ok(Self {
                    parent,
                    index_leaf,
                    lock_leaf,
                    index_path,
                    index_file: RefCell::new(index_file),
                    file,
                    original,
                    prepared: Some(prepared),
                    recovered_stale_transaction: true,
                    remove_on_drop: false,
                })
            }
            Err(err) => Err(err).with_context(|| {
                format!(
                    "acquire completion publication recovery index lock {}",
                    lock_path.display()
                )
            }),
        }
    }

    fn parent(&self) -> &Path {
        self.index_path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn copy_index_to(&self, destination: &Path) -> Result<()> {
        fs::write(destination, self.index_bytes()?).with_context(|| {
            format!(
                "copy locked completion publication index {} to {}",
                self.index_path.display(),
                destination.display()
            )
        })
    }

    fn index_bytes(&self) -> Result<Vec<u8>> {
        self.ensure_attached()?;
        read_open_file(&self.index_file.borrow())
            .with_context(|| format!("read locked git index {}", self.index_path.display()))
    }

    fn ensure_attached(&self) -> Result<()> {
        ensure_open_file_matches_admin_leaf(
            &self.index_file.borrow(),
            &self.parent,
            &self.index_leaf,
        )
        .context("Git index changed after its publication lock was acquired")?;
        ensure_open_file_matches_admin_leaf(&self.file, &self.parent, &self.lock_leaf)
            .context("Git index lock changed after it was acquired")
    }

    fn prepare(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        self.file.write_all(INDEX_TRANSACTION_MAGIC)?;
        self.file.write_all(&std::process::id().to_be_bytes())?;
        self.file
            .write_all(&(self.original.len() as u64).to_be_bytes())?;
        self.file.write_all(&(bytes.len() as u64).to_be_bytes())?;
        self.file.write_all(&self.original)?;
        self.file.write_all(bytes)?;
        self.file.sync_all()?;
        self.parent.sync_all()?;
        self.prepared = Some(bytes.to_vec());
        Ok(())
    }

    fn recovered_stale_transaction(&self) -> bool {
        self.recovered_stale_transaction
    }

    fn complete_recovery(&mut self) {
        self.remove_on_drop = true;
    }

    fn retain_journal(&mut self) -> Result<()> {
        // Once mandatory publication state fails, never remove the recovery evidence,
        // even if marking it abandoned cannot itself be made durable.
        self.remove_on_drop = false;
        self.file
            .seek(SeekFrom::Start(INDEX_TRANSACTION_MAGIC.len() as u64))
            .context("seek durable publication index journal owner")?;
        self.file
            .write_all(&u32::MAX.to_be_bytes())
            .context("mark durable publication index journal abandoned")?;
        self.file
            .sync_all()
            .context("sync abandoned publication index journal")
    }

    fn retain_journal_after_failure(&mut self, failure: anyhow::Error) -> anyhow::Error {
        match self.retain_journal() {
            Ok(()) => failure.context("durable journal retained"),
            Err(retention_err) => retention_err.context(format!(
                "durable index journal retention failed after publication failure: {failure:#}"
            )),
        }
    }

    fn original_bytes(&self) -> &[u8] {
        &self.original
    }

    fn prepared_bytes(&self) -> Option<&[u8]> {
        self.prepared.as_deref()
    }

    #[cfg(test)]
    fn abandon_after_ref_update_for_test(mut self) {
        let _ = self
            .file
            .seek(SeekFrom::Start(INDEX_TRANSACTION_MAGIC.len() as u64));
        let _ = self.file.write_all(&u32::MAX.to_be_bytes());
        let _ = self.file.sync_all();
        std::mem::forget(self);
    }

    fn replace_index(&self, bytes: &[u8]) -> Result<()> {
        replace_admin_leaf(&self.parent, &self.index_leaf, bytes).with_context(|| {
            format!(
                "install completion publication index {}",
                self.index_path.display()
            )
        })?;
        let index_file = open_admin_leaf(&self.parent, &self.index_leaf, libc::O_RDONLY, 0)
            .context("repin installed completion publication index")?;
        *self.index_file.borrow_mut() = index_file;
        self.ensure_attached()
    }

    fn install(&mut self) -> Result<()> {
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("completion publication index was not prepared"))?;
        self.replace_index(prepared)
    }
}

impl Drop for GitIndexLock {
    fn drop(&mut self) {
        if self.remove_on_drop
            && ensure_open_file_matches_admin_leaf(&self.file, &self.parent, &self.lock_leaf)
                .is_ok()
        {
            let _ = unlink_admin_leaf(&self.parent, &self.lock_leaf);
            let _ = self.parent.sync_all();
        }
    }
}

#[cfg(unix)]
fn pin_existing_object_fanouts(
    objects: &File,
    objects_path: &Path,
) -> Result<BTreeMap<OsString, File>> {
    let mut fanouts = BTreeMap::new();
    for name in publication_directory_names(objects, objects_path)? {
        let bytes = name.as_bytes();
        if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        let directory = open_confined_component(
            objects.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .context("pin existing Git object fanout")?;
        fanouts.insert(name, directory);
    }
    Ok(fanouts)
}

#[cfg(not(unix))]
fn pin_existing_object_fanouts(
    _objects: &File,
    _objects_path: &Path,
) -> Result<BTreeMap<OsString, File>> {
    bail!("pinning Git object fanouts requires Unix")
}

#[cfg(unix)]
fn open_or_create_object_fanout(objects: &File, name: &std::ffi::OsStr) -> Result<File> {
    match open_confined_component(
        objects.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    ) {
        Ok(_) => bail!("Git object fanout appeared after object storage was pinned"),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
        {
            let name = std::ffi::CString::new(name.as_bytes())?;
            if unsafe { libc::mkdirat(objects.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("create new confined Git object fanout");
            }
            open_confined_component(
                objects.as_raw_fd(),
                std::ffi::OsStr::from_bytes(name.as_bytes()),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
            .context("open confined Git object fanout")
        }
        Err(err) => Err(err).context("open confined Git object fanout"),
    }
}

#[cfg(unix)]
fn install_loose_object(
    fanout: &File,
    name: &std::ffi::OsStr,
    bytes: &[u8],
) -> Result<(File, bool)> {
    match open_admin_leaf(fanout, name, libc::O_RDONLY, 0) {
        Ok(file) => {
            if !file.metadata()?.file_type().is_file() {
                bail!("existing Git object leaf was not a regular file");
            }
            if read_open_file(&file)? != bytes {
                bail!("existing loose Git object content does not match the publication object id");
            }
            return Ok((file, false));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("inspect confined Git object leaf"),
    }
    let destination = std::ffi::CString::new(name.as_bytes())?;
    for _ in 0..16 {
        let temporary = std::ffi::CString::new(format!(
            ".khazad-object-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ))?;
        let fd = unsafe {
            libc::openat(
                fanout.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o444,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(err).context("create confined loose Git object");
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes)?;
        file.sync_all()?;
        let linked = unsafe {
            libc::linkat(
                fanout.as_raw_fd(),
                temporary.as_ptr(),
                fanout.as_raw_fd(),
                destination.as_ptr(),
                0,
            )
        } == 0;
        let link_error = if linked {
            None
        } else {
            Some(std::io::Error::last_os_error())
        };
        unsafe {
            libc::unlinkat(fanout.as_raw_fd(), temporary.as_ptr(), 0);
        }
        if !linked {
            let err = link_error.expect("failed link retained its error");
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                let current = open_admin_leaf(fanout, name, libc::O_RDONLY, 0)?;
                if !current.metadata()?.file_type().is_file() {
                    bail!("concurrent Git object leaf was not a regular file");
                }
                if read_open_file(&current)? != bytes {
                    bail!(
                        "concurrent loose Git object content does not match the publication object id"
                    );
                }
                return Ok((current, false));
            }
            return Err(err).context("install confined loose Git object");
        }
        fanout.sync_all()?;
        let installed = open_admin_leaf(fanout, name, libc::O_RDONLY, 0)
            .context("pin installed loose Git object")?;
        if read_open_file(&installed)? != bytes {
            bail!("installed loose Git object bytes changed during installation");
        }
        return Ok((installed, true));
    }
    bail!("could not allocate a confined loose Git object")
}

#[cfg(unix)]
fn open_admin_leaf(
    parent: &File,
    leaf: &std::ffi::OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<File> {
    let leaf = std::ffi::CString::new(leaf.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in admin leaf"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(not(unix))]
fn open_admin_leaf(
    _parent: &File,
    _leaf: &std::ffi::OsStr,
    _flags: libc::c_int,
    _mode: libc::mode_t,
) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "descriptor-relative Git administration requires Unix",
    ))
}

fn read_open_file(file: &File) -> Result<Vec<u8>> {
    if !file.metadata()?.file_type().is_file() {
        bail!("Git administrative leaf was not a regular file");
    }
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
fn optional_admin_leaf_bytes(parent: &File, leaf: &OsStr) -> Result<Option<Vec<u8>>> {
    match open_admin_leaf(parent, leaf, libc::O_RDONLY, 0) {
        Ok(file) => Ok(Some(read_open_file(&file)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("open Git administrative leaf"),
    }
}

#[cfg(unix)]
fn restore_admin_leaf_exact(
    parent: &File,
    leaf: &OsStr,
    before: Option<&[u8]>,
    observed_after: Option<&[u8]>,
) -> Result<()> {
    let mut lock_name = leaf.to_os_string();
    lock_name.push(".lock");
    let mut lock = open_admin_leaf(
        parent,
        &lock_name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o666,
    )
    .context("lock Git administrative leaf for exact restoration")?;
    let result = (|| -> Result<bool> {
        if optional_admin_leaf_bytes(parent, leaf)?.as_deref() != observed_after {
            bail!("Git administrative leaf changed concurrently during restoration");
        }
        if let Some(bytes) = before {
            lock.write_all(bytes)?;
            lock.sync_all()?;
            rename_admin_leaf(parent, &lock_name, leaf)?;
            parent.sync_all()?;
            Ok(true)
        } else {
            if observed_after.is_some() {
                unlink_admin_leaf(parent, leaf)?;
            }
            parent.sync_all()?;
            Ok(false)
        }
    })();
    let installed_lock = result.as_ref().is_ok_and(|renamed| *renamed);
    if !installed_lock {
        let _ = unlink_admin_leaf(parent, &lock_name);
    }
    result.map(|_| ())
}

#[cfg(unix)]
fn rename_admin_leaf(parent: &File, from: &OsStr, to: &OsStr) -> Result<()> {
    let from = std::ffi::CString::new(from.as_bytes())?;
    let to = std::ffi::CString::new(to.as_bytes())?;
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("rename Git administrative leaf");
    }
    Ok(())
}

#[cfg(unix)]
fn replace_admin_leaf(parent: &File, leaf: &std::ffi::OsStr, bytes: &[u8]) -> Result<()> {
    let leaf = std::ffi::CString::new(leaf.as_bytes())?;
    for _ in 0..16 {
        let temporary = std::ffi::CString::new(format!(
            ".khazad-index-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(err).context("create confined Git index replacement");
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    leaf.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("install confined Git index replacement");
            }
            parent.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        return result;
    }
    bail!("could not allocate a confined Git index replacement")
}

#[cfg(not(unix))]
fn replace_admin_leaf(_parent: &File, _leaf: &std::ffi::OsStr, _bytes: &[u8]) -> Result<()> {
    bail!("descriptor-relative Git index replacement requires Unix")
}

#[cfg(unix)]
fn unlink_admin_leaf(parent: &File, leaf: &std::ffi::OsStr) -> Result<()> {
    let leaf = std::ffi::CString::new(leaf.as_bytes())?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("unlink Git administrative leaf");
    }
    Ok(())
}

#[cfg(not(unix))]
fn unlink_admin_leaf(_parent: &File, _leaf: &std::ffi::OsStr) -> Result<()> {
    bail!("descriptor-relative Git administration requires Unix")
}

fn ensure_open_file_matches_admin_leaf(
    file: &File,
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<()> {
    let current = open_admin_leaf(parent, leaf, libc::O_RDONLY, 0)?;
    if open_filesystem_object_identity_bytes(file)?
        != open_filesystem_object_identity_bytes(&current)?
    {
        bail!("Git administrative leaf changed after it was opened");
    }
    Ok(())
}

#[cfg(unix)]
fn claim_stale_index_transaction(file: &File) -> Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("another completion publication recovery owns the journal");
    }
    Ok(())
}

#[cfg(not(unix))]
fn claim_stale_index_transaction(_file: &File) -> Result<()> {
    bail!("stale completion publication recovery requires an exclusive journal claim")
}

#[cfg(unix)]
fn ensure_open_file_matches_path(file: &File, path: &Path) -> Result<()> {
    let current = open_publication_root(path)?;
    if open_filesystem_object_identity_bytes(file)?
        != open_filesystem_object_identity_bytes(&current)?
    {
        bail!("open directory no longer matches its descriptor-confined pathname");
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_open_file_matches_path(_file: &File, path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("index lock path is no longer a file");
    }
    Ok(())
}

#[cfg(test)]
fn git_index_paths(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let raw_index_path =
        trim_ascii(&run_bytes(dir, &["rev-parse", "--git-path", "index"])?).to_vec();
    let index_path = path_from_git_bytes(&raw_index_path)?;
    let index_path = if index_path.is_absolute() {
        index_path
    } else {
        dir.join(index_path)
    };
    let mut lock_name = index_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    Ok((index_path, PathBuf::from(lock_name)))
}

fn parse_index_transaction(bytes: &[u8]) -> Result<(u32, Vec<u8>, Vec<u8>)> {
    let mut cursor = INDEX_TRANSACTION_MAGIC.len();
    if !bytes.starts_with(INDEX_TRANSACTION_MAGIC) || bytes.len() < cursor + 20 {
        bail!("invalid completion publication index transaction header");
    }
    let owner_pid = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?);
    cursor += 4;
    let original_len = u64::from_be_bytes(bytes[cursor..cursor + 8].try_into()?) as usize;
    cursor += 8;
    let prepared_len = u64::from_be_bytes(bytes[cursor..cursor + 8].try_into()?) as usize;
    cursor += 8;
    let expected_len = cursor
        .checked_add(original_len)
        .and_then(|value| value.checked_add(prepared_len))
        .ok_or_else(|| anyhow::anyhow!("publication index transaction length overflow"))?;
    if bytes.len() != expected_len {
        bail!("truncated completion publication index transaction");
    }
    let original = bytes[cursor..cursor + original_len].to_vec();
    cursor += original_len;
    let prepared = bytes[cursor..].to_vec();
    Ok((owner_pid, original, prepared))
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }
    let status = unsafe { libc::kill(pid as i32, 0) };
    status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn validate_manifest_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "completion publication path must be a non-empty repository-relative literal path: {}",
            path.display()
        );
    }
    Ok(())
}

pub fn commit_paths(dir: impl AsRef<Path>, paths: &[&str], message: &str) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let dir = dir.as_ref();
    let mut add_args = vec!["add", "--"];
    add_args.extend(paths.iter().copied());
    run(dir, &add_args)?;
    let diff_args = ["diff", "--cached", "--quiet", "--"];
    let status = Command::new("git")
        .args(diff_args)
        .args(paths)
        .current_dir(dir)
        .status()
        .with_context(|| format!("run git diff --cached --quiet -- {}", paths.join(" ")))?;
    if status.success() {
        return Ok(false);
    }
    run(dir, &["commit", "-m", message])?;
    Ok(true)
}
