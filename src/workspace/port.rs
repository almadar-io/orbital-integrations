//! The slice of a workspace the `workspace` service drives (`WorkspacePort` in the TS twin).

use crate::error::IntegrationError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub path: String,
    pub is_directory: bool,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub id: String,
    pub timestamp_ms: u64,
    pub reason: String,
    pub kind: Option<String>,
    pub restores: Option<String>,
    pub undoes: Option<String>,
    pub redoes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryState {
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFileChange {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

pub trait WorkspacePort: Send + Sync {
    fn list_tree(&self, rel_path: Option<&str>) -> Result<Vec<TreeNode>, IntegrationError>;
    fn read_file(&self, rel_path: &str) -> Result<Option<String>, IntegrationError>;
    fn write_file(&self, rel_path: &str, content: &str) -> Result<(), IntegrationError>;
    fn remove_file(&self, rel_path: &str) -> Result<(), IntegrationError>;
    fn exists(&self, rel_path: &str) -> Result<bool, IntegrationError>;
    /// The new commit, or `None` when nothing changed.
    fn snapshot(&self, reason: &str) -> Result<Option<String>, IntegrationError>;
    /// Newest first. `limit` is the text `git log -n` receives.
    fn list_snapshots(&self, limit: Option<&str>) -> Result<Vec<SnapshotMeta>, IntegrationError>;
    fn history_state(&self) -> Result<HistoryState, IntegrationError>;
    fn changes_at(&self, oid: &str) -> Result<Vec<HistoryFileChange>, IntegrationError>;
    fn restore(&self, oid: &str) -> Result<Option<String>, IntegrationError>;
    fn undo(&self) -> Result<Option<String>, IntegrationError>;
    fn redo(&self) -> Result<Option<String>, IntegrationError>;
}
