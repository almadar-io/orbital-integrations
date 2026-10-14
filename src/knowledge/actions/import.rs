//! Importing markdown or pasted documents into a path (`import.ts`).

use std::collections::HashMap;

use serde_json::{json, Value};

use super::import_graph::{commit_into_graph, ImportOutcome};
use super::import_sink::KnowledgeImportSink;
use super::migration::{
    extract, field_for_slot, hierarchy_relationships, map_units, topo_order, validate_units,
    AdapterKind, ImportUnit, RawDocument, StagedRelationship,
};
use super::params::{self, validate, Ty};
use super::{action, Env};
use crate::knowledge::store::StoreResult;
use crate::knowledge::ActionTable;

fn documents_ty() -> Ty {
    Ty::Object(vec![
        params::id("ref"),
        params::text("title"),
        params::text("raw"),
    ])
}

fn documents_of(p: &serde_json::Map<String, Value>) -> Vec<(RawDocument, String)> {
    p.get("documents")
        .and_then(Value::as_array)
        .map(|docs| {
            docs.iter()
                .map(|d| {
                    let text = |k: &str| {
                        d.get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    (
                        RawDocument {
                            reference: text("ref"),
                            raw: text("raw"),
                        },
                        text("title"),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn adapter_of(p: &serde_json::Map<String, Value>) -> AdapterKind {
    if params::str_of(p, "adapter") == "markdown" {
        AdapterKind::Markdown
    } else {
        AdapterKind::Paste
    }
}

fn new_graph_name(documents: &[(RawDocument, String)]) -> String {
    match documents.first() {
        None => "Import".to_string(),
        Some((doc, title)) => {
            if title.trim().is_empty() {
                doc.reference.clone()
            } else {
                title.clone()
            }
        }
    }
}

fn unit_title(unit: &ImportUnit) -> String {
    field_for_slot(&unit.target_entity, "title")
        .and_then(|field| unit.fields.get(field))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `commitStagedUnits`: parents first, relationships after, failures per unit.
fn commit_staged_units(
    units: &[ImportUnit],
    relationships: &[StagedRelationship],
    error_refs: &[String],
    sink: &mut KnowledgeImportSink<'_>,
) -> StoreResult<ImportOutcome> {
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for unit in topo_order(units) {
        if error_refs.contains(&unit.reference) {
            continue;
        }
        if let Some(parent) = &unit.parent_ref {
            if units.iter().any(|u| &u.reference == parent) && !ids.contains_key(parent) {
                failed.push((
                    unit.reference.clone(),
                    format!("parent '{parent}' was not committed"),
                ));
                continue;
            }
        }
        match sink.create_entity_row(&unit.target_entity, &unit.fields) {
            Ok(id) => {
                ids.insert(unit.reference.clone(), id);
            }
            Err(e) => failed.push((unit.reference.clone(), e.message)),
        }
    }
    for rel in relationships {
        let (Some(from), Some(to)) = (ids.get(&rel.from_ref), ids.get(&rel.to_ref)) else {
            continue;
        };
        if let Err(e) = sink.create_relationship(rel.rel_type, from, to) {
            failed.push((format!("{}->{}", rel.from_ref, rel.to_ref), e.message));
        }
    }
    let result = sink.commit()?;
    failed.extend(result.failed);
    Ok(ImportOutcome {
        committed: result.committed,
        failed,
    })
}

pub fn register(table: &mut ActionTable, env: &Env) {
    table.insert(
        "importPreview",
        action(|_call, p, _deps| {
            validate(p, &[params::pick("adapter", &["markdown", "paste"]), params::list("documents", documents_ty(), true)])?;
            let documents: Vec<RawDocument> = documents_of(p).into_iter().map(|(d, _)| d).collect();
            let units = map_units(&extract(adapter_of(p), &documents).units);
            let errors = validate_units(&units);
            Ok(json!({
                "units": units.iter().map(|u| json!({
                    "ref": u.reference, "targetEntity": u.target_entity, "title": unit_title(u),
                    "parentRef": u.parent_ref.clone().unwrap_or_default(),
                })).collect::<Vec<_>>(),
                "errors": errors.iter().map(|e| json!({ "ref": e.reference, "message": e.message })).collect::<Vec<_>>(),
            }))
        }),
    );

    let uuid = env.uuid.clone();
    table.insert(
        "importCommit",
        action(move |call, p, deps| {
            validate(
                p,
                &[params::pick("adapter", &["markdown", "paste"]), params::list("documents", documents_ty(), true), params::opt_id("graphId")],
            )?;
            let documents = documents_of(p);
            let name = new_graph_name(&documents);
            let raw: Vec<RawDocument> = documents.into_iter().map(|(d, _)| d).collect();
            let adapter = adapter_of(p);
            let (graph_id, outcome) = commit_into_graph(deps, &uuid, call.uid, params::opt_str_of(p, "graphId"), &name, |sink| {
                let units = map_units(&extract(adapter, &raw).units);
                let relationships = hierarchy_relationships(&units);
                let errors: Vec<String> = validate_units(&units).into_iter().map(|e| e.reference).collect();
                commit_staged_units(&units, &relationships, &errors, sink)
            })?;
            Ok(json!({
                "graphId": graph_id,
                "committed": outcome.committed,
                "failed": outcome.failed.iter().map(|(r, why)| json!({ "ref": r, "reason": why })).collect::<Vec<_>>(),
            }))
        }),
    );
}
