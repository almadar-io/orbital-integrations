//! Commit into the learner's graph, or a fresh one (`importGraph.ts`).

use super::import_sink::{create_import_graph, CommitResult, KnowledgeImportSink};
use super::IdSource;
use crate::knowledge::store::{ErrorKind, KnowledgeDeps, KnowledgeError, StoreResult};

pub struct ImportOutcome {
    pub committed: usize,
    pub failed: Vec<(String, String)>,
}

impl From<CommitResult> for ImportOutcome {
    fn from(r: CommitResult) -> Self {
        Self {
            committed: r.committed,
            failed: r.failed,
        }
    }
}

/// Commit into the caller's graph (or a fresh one named `new_graph_name`); a fresh graph that ends with nothing
/// committed is deleted and reported as `SERVICE_ERROR`.
pub fn commit_into_graph(
    deps: &KnowledgeDeps,
    uuid: &IdSource,
    uid: &str,
    graph_id: Option<&str>,
    new_graph_name: &str,
    run: impl FnOnce(&mut KnowledgeImportSink<'_>) -> StoreResult<ImportOutcome>,
) -> StoreResult<(String, ImportOutcome)> {
    let (resolved, created) = match graph_id {
        Some(id) => {
            if !deps.graphs.graph_exists(uid, id) {
                return Err(KnowledgeError::not_found(format!("Path {id} not found")));
            }
            (id.to_string(), false)
        }
        None => (create_import_graph(deps, uuid, uid, new_graph_name)?, true),
    };
    let rollback = || {
        if created {
            let _ = deps.graphs.delete_graph(uid, &resolved);
        }
    };
    let mut sink = KnowledgeImportSink::new(deps, uuid, uid, &resolved);
    match run(&mut sink) {
        Ok(outcome) => {
            if created && outcome.committed == 0 {
                rollback();
                let reasons = outcome
                    .failed
                    .iter()
                    .map(|(r, why)| format!("{r}: {why}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(KnowledgeError::service(if reasons.is_empty() {
                    "Nothing was imported".to_string()
                } else {
                    format!("Nothing was imported: {reasons}")
                }));
            }
            Ok((resolved.clone(), outcome))
        }
        Err(e) => {
            if e.kind == ErrorKind::Other {
                rollback();
            }
            Err(e)
        }
    }
}
