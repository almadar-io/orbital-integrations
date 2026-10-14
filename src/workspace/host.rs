//! Where a host keeps its workspaces and how it opens the one a call names for the calling principal
//! (`WorkspaceHost`, `localWorkspaceHost`, `storageWorkspaceHost` in the TS twin).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::bucket::WorkspaceBucket;
use super::clock::Clock;
use super::disk::{ArchiveBackend, DiskWorkspace, OpenOptions, RestoreSource};
use super::layout::workspace_owner_segment;
use super::port::{HistoryFileChange, HistoryState, SnapshotMeta, TreeNode, WorkspacePort};
use crate::error::IntegrationError;

pub const AUTH_ERROR: &str = "AUTH_ERROR";
const PROJECT_NAME: &str = "Untitled";

pub trait WorkspaceHost: Send + Sync {
    /// Where the host keeps its workspaces on disk.
    fn root(&self) -> &Path;
    /// The workspace `workspace` names, as `principal` (who owns access).
    fn open(
        &self,
        workspace: &str,
        principal: Option<&str>,
    ) -> Result<Arc<dyn WorkspacePort>, IntegrationError>;
}

fn signed_in(principal: Option<&str>) -> Result<&str, IntegrationError> {
    match principal {
        Some(p) if !p.is_empty() => Ok(p),
        _ => Err(IntegrationError::coded(
            AUTH_ERROR,
            "A workspace needs a signed-in caller",
        )),
    }
}

type Opened = Arc<DiskWorkspace>;

/// The host's own disk workspaces under `<project_root>/workspaces`, one per (signed-in user, workspace id).
pub struct LocalWorkspaceHost {
    root: PathBuf,
    clock: Clock,
    opened: Mutex<HashMap<String, Opened>>,
}

impl LocalWorkspaceHost {
    pub fn new(project_root: &Path) -> Self {
        Self::with_clock(project_root, Clock::system())
    }

    pub fn with_clock(project_root: &Path, clock: Clock) -> Self {
        Self {
            root: project_root.join("workspaces"),
            clock,
            opened: Mutex::new(HashMap::new()),
        }
    }
}

impl WorkspaceHost for LocalWorkspaceHost {
    fn root(&self) -> &Path {
        &self.root
    }

    fn open(
        &self,
        workspace: &str,
        principal: Option<&str>,
    ) -> Result<Arc<dyn WorkspacePort>, IntegrationError> {
        let owner = workspace_owner_segment(signed_in(principal)?);
        let key = format!("{owner}/{workspace}");
        let mut opened = self.opened.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ws) = opened.get(&key) {
            return Ok(ws.clone());
        }
        let ws = Arc::new(DiskWorkspace::open(OpenOptions {
            root: &self.root,
            user_id: &owner,
            app_id: workspace,
            project_name: PROJECT_NAME,
            restore: None,
            archive: None,
            clock: self.clock.clone(),
        })?);
        opened.insert(key, ws.clone());
        Ok(ws)
    }
}

struct BucketArchive {
    bucket: Arc<dyn WorkspaceBucket>,
    history_prefix: String,
}

impl BucketArchive {
    fn base(&self) -> String {
        format!("{}base.bundle", self.history_prefix)
    }

    fn increments(&self) -> Result<Vec<String>, IntegrationError> {
        let mut names = self.bucket.list(&format!("{}inc-", self.history_prefix))?;
        names.sort();
        Ok(names)
    }
}

impl ArchiveBackend for BucketArchive {
    fn save_bundle(&self, bytes: &[u8]) -> Result<(), IntegrationError> {
        self.bucket.write(&self.base(), bytes)?;
        for name in self.increments()? {
            self.bucket.remove(&name)?;
        }
        Ok(())
    }

    fn append_increment(&self, bytes: &[u8]) -> Result<(), IntegrationError> {
        let seq = self.increments()?.len();
        self.bucket.write(
            &format!("{}inc-{seq:06}.bundle", self.history_prefix),
            bytes,
        )
    }

    fn load_bundles(&self) -> Result<Option<Vec<Vec<u8>>>, IntegrationError> {
        let Some(whole) = self.bucket.read(&self.base())? else {
            return Ok(None);
        };
        let mut all = vec![whole];
        for name in self.increments()? {
            if let Some(bytes) = self.bucket.read(&name)? {
                all.push(bytes);
            }
        }
        Ok(Some(all))
    }
}

struct BucketRestore {
    bucket: Arc<dyn WorkspaceBucket>,
    files_prefix: String,
}

impl RestoreSource for BucketRestore {
    fn list_files(&self) -> Result<Vec<String>, IntegrationError> {
        Ok(self
            .bucket
            .list(&self.files_prefix)?
            .iter()
            .map(|name| name[self.files_prefix.len()..].to_string())
            .collect())
    }

    fn read_file(&self, path: &str) -> Result<Option<String>, IntegrationError> {
        Ok(self
            .bucket
            .read(&format!("{}{path}", self.files_prefix))?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// A workspace whose files and history are mirrored into the bucket after every write and history step.
struct StoragePort {
    ws: Arc<DiskWorkspace>,
    bucket: Arc<dyn WorkspaceBucket>,
    files_prefix: String,
}

impl StoragePort {
    /// Make the bucket's files mirror the workspace tree (git internals stay in the history archive).
    fn sync_tree(&self) -> Result<(), IntegrationError> {
        let mut kept = HashSet::new();
        for node in self.ws.list_tree(None)? {
            if node.is_directory || node.path == ".git" || node.path.starts_with(".git/") {
                continue;
            }
            let Some(content) = self.ws.read_file(&node.path)? else {
                continue;
            };
            let name = format!("{}{}", self.files_prefix, node.path);
            self.bucket.write(&name, content.as_bytes())?;
            kept.insert(name);
        }
        for name in self.bucket.list(&self.files_prefix)? {
            if !kept.contains(&name) {
                self.bucket.remove(&name)?;
            }
        }
        Ok(())
    }

    fn after_history_step(
        &self,
        step: Result<Option<String>, IntegrationError>,
    ) -> Result<Option<String>, IntegrationError> {
        let step = step?;
        self.sync_tree()?;
        self.ws.archive()?;
        Ok(step)
    }
}

impl WorkspacePort for StoragePort {
    fn list_tree(&self, rel_path: Option<&str>) -> Result<Vec<TreeNode>, IntegrationError> {
        self.ws.list_tree(rel_path)
    }
    fn read_file(&self, rel_path: &str) -> Result<Option<String>, IntegrationError> {
        self.ws.read_file(rel_path)
    }
    fn write_file(&self, rel_path: &str, content: &str) -> Result<(), IntegrationError> {
        self.ws.write_file(rel_path, content)?;
        self.bucket.write(
            &format!("{}{rel_path}", self.files_prefix),
            content.as_bytes(),
        )
    }
    fn remove_file(&self, rel_path: &str) -> Result<(), IntegrationError> {
        self.ws.remove_file(rel_path)?;
        self.bucket
            .remove(&format!("{}{rel_path}", self.files_prefix))
    }
    fn exists(&self, rel_path: &str) -> Result<bool, IntegrationError> {
        self.ws.exists(rel_path)
    }
    fn snapshot(&self, reason: &str) -> Result<Option<String>, IntegrationError> {
        self.after_history_step(self.ws.snapshot(reason))
    }
    fn list_snapshots(&self, limit: Option<&str>) -> Result<Vec<SnapshotMeta>, IntegrationError> {
        self.ws.list_snapshots(limit)
    }
    fn history_state(&self) -> Result<HistoryState, IntegrationError> {
        self.ws.history_state()
    }
    fn changes_at(&self, oid: &str) -> Result<Vec<HistoryFileChange>, IntegrationError> {
        self.ws.changes_at(oid)
    }
    fn restore(&self, oid: &str) -> Result<Option<String>, IntegrationError> {
        self.after_history_step(self.ws.restore(oid))
    }
    fn undo(&self) -> Result<Option<String>, IntegrationError> {
        self.after_history_step(self.ws.undo())
    }
    fn redo(&self) -> Result<Option<String>, IntegrationError> {
        self.after_history_step(self.ws.redo())
    }
}

/// Workspaces kept in a bucket, for a host whose disk does not last: each is cached under `cache_root`,
/// restored from `workspaces/<owner>/<workspace>/files/` on a cold cache with its git history from `history/`,
/// and every write, snapshot and history step is written back.
pub struct StorageWorkspaceHost {
    bucket: Arc<dyn WorkspaceBucket>,
    cache_root: PathBuf,
    clock: Clock,
    hydrated: Mutex<HashSet<String>>,
    opened: Mutex<HashMap<String, Arc<StoragePort>>>,
}

impl StorageWorkspaceHost {
    pub fn new(bucket: Arc<dyn WorkspaceBucket>, cache_root: &Path) -> Self {
        Self::with_clock(bucket, cache_root, Clock::system())
    }

    pub fn with_clock(bucket: Arc<dyn WorkspaceBucket>, cache_root: &Path, clock: Clock) -> Self {
        Self {
            bucket,
            cache_root: cache_root.to_path_buf(),
            clock,
            hydrated: Mutex::new(HashSet::new()),
            opened: Mutex::new(HashMap::new()),
        }
    }
}

impl WorkspaceHost for StorageWorkspaceHost {
    fn root(&self) -> &Path {
        &self.cache_root
    }

    fn open(
        &self,
        workspace: &str,
        principal: Option<&str>,
    ) -> Result<Arc<dyn WorkspacePort>, IntegrationError> {
        let owner = workspace_owner_segment(signed_in(principal)?);
        let key = format!("{owner}/{workspace}");
        let mut opened = self.opened.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(port) = opened.get(&key) {
            return Ok(port.clone());
        }
        let prefix = format!("workspaces/{owner}/{}/", workspace_owner_segment(workspace));
        let files_prefix = format!("{prefix}files/");
        let restore = BucketRestore {
            bucket: self.bucket.clone(),
            files_prefix: files_prefix.clone(),
        };
        let ws = Arc::new(DiskWorkspace::open(OpenOptions {
            root: &self.cache_root,
            user_id: &owner,
            app_id: workspace,
            project_name: PROJECT_NAME,
            restore: Some(&restore),
            archive: Some(Arc::new(BucketArchive {
                bucket: self.bucket.clone(),
                history_prefix: format!("{prefix}history/"),
            })),
            clock: self.clock.clone(),
        })?);
        let port = Arc::new(StoragePort {
            ws,
            bucket: self.bucket.clone(),
            files_prefix: files_prefix.clone(),
        });
        let first = self
            .hydrated
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone());
        if first {
            if port.ws.list_snapshots(Some("1"))?.is_empty() {
                port.ws.hydrate_from_archive()?;
            }
            if self.bucket.list(&files_prefix)?.is_empty() {
                port.sync_tree()?;
            }
        }
        opened.insert(key, port.clone());
        Ok(port)
    }
}
