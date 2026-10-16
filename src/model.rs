use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub remotes: Vec<String>,
    pub base: Option<String>,
    pub include_unmerged: bool,
    pub keep_ignored: bool,
    pub fetch: bool,
    /// Also search patch-equivalent squashes and verify their exact result.
    pub deep: bool,
    /// Let Git persist refreshed file stat metadata while checking status.
    pub refresh_index: bool,
    pub current_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RepositorySeed {
    pub path: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteReport {
    pub name: String,
    pub eligible: bool,
    pub default_branch: Option<String>,
    pub issue: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryReport {
    pub path: PathBuf,
    pub common_dir: PathBuf,
    pub bare: bool,
    pub stashes: bool,
    pub unpushed_branches: Vec<String>,
    pub unknown_branches: Vec<String>,
    pub remotes: Vec<RemoteReport>,
    pub fetched: bool,
    pub worktrees: Vec<WorktreeReport>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeReport {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub primary: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
    pub modified: bool,
    pub untracked: bool,
    pub ignored: bool,
    pub conflicts: bool,
    pub operation: Option<String>,
    pub integrated: Option<bool>,
    pub backed_up: Option<bool>,
    pub removable: bool,
    pub reason: Option<String>,
    pub errors: Vec<String>,
    pub deletion: Option<DeletionResult>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionStatus {
    Deleted,
    WouldDelete,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeletionResult {
    pub status: DeletionStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub repositories: Vec<RepositoryReport>,
    pub errors: Vec<String>,
    pub elapsed_ms: u128,
}

impl WorktreeReport {
    pub fn needs_attention(&self) -> bool {
        self.modified
            || self.untracked
            || self.conflicts
            || self.backed_up == Some(false)
            || self.integrated == Some(false)
            || self.removable
            || !self.errors.is_empty()
            || self.operation.is_some()
            || (!self.primary && (self.locked || self.prunable || self.head.is_none()))
            || (self.head.is_some() && self.integrated.is_none())
            || self.deletion.is_some()
    }
}

impl RepositoryReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
            || self.worktrees.iter().any(|worktree| {
                !worktree.errors.is_empty()
                    || matches!(
                        worktree.deletion.as_ref().map(|result| &result.status),
                        Some(DeletionStatus::Failed)
                    )
            })
    }
}
