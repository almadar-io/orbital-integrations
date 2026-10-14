//! The `workspace` service: the Rust twin of `WorkspaceIntegration` (`@almadar/integrations`
//! `integrations/workspace`) over a Rust twin of `@almadar/workspace`'s file and history surface. Actions:
//! `tree`, `readFile`, `writeFile`, `removeFile`, `snapshot`, `history`, `changes`, `restore`, `undo`, `redo`.
//!
//! History is a git repository driven through the `git` CLI (as the TS twin does), and the storage-backed host
//! keeps the same bucket layout as `storageWorkspaceHost`: `workspaces/<owner>/<workspace>/files/<path>` and
//! `.../history/{base.bundle,inc-NNNNNN.bundle}`, so a TS host and a Rust host can share a bucket.

pub mod bucket;
pub mod clock;
mod disk;
mod git;
pub mod host;
pub mod layout;
pub mod port;

use std::sync::Arc;

use serde_json::{json, Map, Value};

pub use self::bucket::{MemoryBucket, WorkspaceBucket};
pub use self::clock::Clock;
pub use self::disk::{ArchiveBackend, DiskWorkspace, OpenOptions, RestoreSource};
pub use self::host::{LocalWorkspaceHost, StorageWorkspaceHost, WorkspaceHost, AUTH_ERROR};
pub use self::layout::workspace_owner_segment;
pub use self::port::{HistoryFileChange, HistoryState, SnapshotMeta, TreeNode, WorkspacePort};
use crate::error::{IntegrationError, NOT_CONFIGURED};
use crate::provider::{CallContext, Integrations, Provider};

pub const SERVICE: &str = "workspace";

pub struct WorkspaceProvider {
    host: Option<Arc<dyn WorkspaceHost>>,
}

/// Register `workspace` on `integrations`. A workspace has no credentials and no mock: it is always the
/// host's own store, so a missing host reports `NOT_CONFIGURED` from the call itself, as in TS.
pub fn register(integrations: &mut Integrations, host: Option<Arc<dyn WorkspaceHost>>) {
    integrations.register(SERVICE, Arc::new(WorkspaceProvider::new(host)));
}

impl WorkspaceProvider {
    pub fn new(host: Option<Arc<dyn WorkspaceHost>>) -> Self {
        Self { host }
    }
}

fn text(params: &Map<String, Value>, key: &str) -> String {
    match params.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn step_result(step: Option<String>) -> Value {
    match step {
        None => json!({ "changed": false, "oid": "" }),
        Some(oid) => json!({ "changed": true, "oid": oid }),
    }
}

fn snapshot_row(meta: &SnapshotMeta) -> Value {
    json!({
        "id": meta.id,
        "timestamp": meta.timestamp_ms,
        "reason": meta.reason,
        "kind": meta.kind.clone().unwrap_or_default(),
        "target": meta
            .restores
            .clone()
            .or_else(|| meta.undoes.clone())
            .or_else(|| meta.redoes.clone())
            .unwrap_or_default(),
    })
}

impl Provider for WorkspaceProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        let Some(host) = &self.host else {
            return Err(IntegrationError::coded(
                NOT_CONFIGURED,
                "No workspace host is installed on this host",
            ));
        };
        let ws = host.open(&text(params, "workspace"), ctx.principal())?;
        let path = match params.get("path") {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        };
        match action {
            "tree" => {
                let nodes = ws.list_tree(path)?;
                Ok(json!({
                    "nodes": nodes
                        .iter()
                        .map(|n| json!({
                            "id": n.path,
                            "path": n.path,
                            "type": if n.is_directory { "directory" } else { "file" },
                            "size": n.size,
                        }))
                        .collect::<Vec<_>>()
                }))
            }
            "readFile" => {
                let path = text(params, "path");
                let content = ws.read_file(&path)?;
                Ok(json!({
                    "path": path,
                    "exists": content.is_some(),
                    "content": content.unwrap_or_default(),
                }))
            }
            "writeFile" => {
                let path = text(params, "path");
                ws.write_file(&path, &text(params, "content"))?;
                Ok(json!({ "path": path, "written": true }))
            }
            "removeFile" => {
                let path = text(params, "path");
                let existed = ws.exists(&path)?;
                ws.remove_file(&path)?;
                Ok(json!({ "path": path, "removed": existed }))
            }
            "snapshot" => {
                let commit = ws.snapshot(&text(params, "reason"))?;
                Ok(json!({
                    "committed": commit.is_some(),
                    "oid": commit.unwrap_or_default(),
                }))
            }
            "history" => {
                let limit = match params.get("limit") {
                    Some(Value::Number(n)) => n.as_f64().map(crate::jsnumber::js_number),
                    _ => None,
                };
                let snapshots = ws.list_snapshots(limit.as_deref())?;
                let state = ws.history_state()?;
                Ok(json!({
                    "snapshots": snapshots.iter().map(snapshot_row).collect::<Vec<_>>(),
                    "canUndo": state.can_undo,
                    "canRedo": state.can_redo,
                }))
            }
            "changes" => {
                let changes = ws.changes_at(&text(params, "oid"))?;
                Ok(json!({
                    "changes": changes
                        .iter()
                        .map(|c| json!({
                            "id": c.path,
                            "path": c.path,
                            "before": c.before.clone().unwrap_or_default(),
                            "after": c.after.clone().unwrap_or_default(),
                            "change": if c.before.is_none() {
                                "added"
                            } else if c.after.is_none() {
                                "removed"
                            } else {
                                "modified"
                            },
                        }))
                        .collect::<Vec<_>>()
                }))
            }
            "restore" => Ok(step_result(ws.restore(&text(params, "oid"))?)),
            "undo" => Ok(step_result(ws.undo()?)),
            "redo" => Ok(step_result(ws.redo()?)),
            other => Err(IntegrationError::plain(format!(
                "Unknown workspace action: {other}"
            ))),
        }
    }
}
