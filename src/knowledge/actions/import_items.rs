//! Commit an approved, already-structured proposal into a path (`importItems.ts`).

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::import_graph::{commit_into_graph, ImportOutcome};
use super::import_sink::{KnowledgeImportSink, MIGRATION_RELATIONSHIPS};
use super::params::{self, validate, Ty};
use super::{action, Env};
use crate::knowledge::store::StoreResult;
use crate::knowledge::ActionTable;

const TARGET_ENTITIES: [&str; 4] = ["Concept", "Lesson", "FlashCard", "LearningGoal"];

#[derive(Clone)]
struct ImportItem {
    reference: String,
    target_entity: String,
    title: String,
    parent_ref: String,
    content: String,
    front: Option<String>,
    back: Option<String>,
}

fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

fn side_of(explicit: Option<&str>, fallback: &str) -> String {
    if is_blank(explicit) {
        fallback.to_string()
    } else {
        explicit.unwrap_or(fallback).to_string()
    }
}

fn relationship_between(parent: &str, child: &str) -> Option<&'static str> {
    MIGRATION_RELATIONSHIPS
        .iter()
        .find(|(_, from, to)| *from == parent && *to == child)
        .map(|(t, _, _)| *t)
}

/// The entity row fields for an item, or the reason it cannot be written.
fn fields_for(item: &ImportItem) -> Result<Map<String, Value>, String> {
    let mut fields = Map::new();
    match item.target_entity.as_str() {
        "Concept" => {
            fields.insert("name".into(), json!(item.title));
            fields.insert("lesson".into(), json!(item.content));
        }
        "LearningGoal" => {
            fields.insert("name".into(), json!(item.title));
            fields.insert("description".into(), json!(item.content));
        }
        "Lesson" => {
            if is_blank(Some(&item.content)) {
                return Err("a Lesson needs content".to_string());
            }
            fields.insert("content".into(), json!(item.content));
        }
        _ => {
            let front = side_of(item.front.as_deref(), &item.title);
            let back = side_of(item.back.as_deref(), &item.content);
            if is_blank(Some(&front)) || is_blank(Some(&back)) {
                return Err("a FlashCard needs a front and a back".to_string());
            }
            fields.insert("front".into(), json!(front));
            fields.insert("back".into(), json!(back));
        }
    }
    Ok(fields)
}

struct Committer<'a, 'b> {
    sink: &'a mut KnowledgeImportSink<'b>,
    by_ref: Vec<ImportItem>,
    ids: Vec<(String, String)>,
    failed: Vec<(String, String)>,
    in_progress: Vec<String>,
}

impl Committer<'_, '_> {
    fn item(&self, reference: &str) -> Option<ImportItem> {
        self.by_ref
            .iter()
            .find(|i| i.reference == reference)
            .cloned()
    }

    fn id_of(&self, reference: &str) -> Option<String> {
        self.ids
            .iter()
            .find(|(r, _)| r == reference)
            .map(|(_, id)| id.clone())
    }

    fn place(&mut self, item: &ImportItem) -> Result<String, String> {
        if let Some(id) = self.id_of(&item.reference) {
            return Ok(id);
        }
        if let Some((_, prior)) = self.failed.iter().find(|(r, _)| *r == item.reference) {
            return Err(prior.clone());
        }
        if self.in_progress.contains(&item.reference) {
            return Err("parentRef forms a cycle".to_string());
        }
        self.in_progress.push(item.reference.clone());
        let outcome = self.create(item);
        self.in_progress.retain(|r| *r != item.reference);
        match outcome {
            Ok(id) => {
                self.ids.push((item.reference.clone(), id.clone()));
                Ok(id)
            }
            Err(reason) => {
                match self.failed.iter_mut().find(|(r, _)| *r == item.reference) {
                    Some(entry) => entry.1 = reason.clone(),
                    None => self.failed.push((item.reference.clone(), reason.clone())),
                }
                Err(reason)
            }
        }
    }

    fn create(&mut self, item: &ImportItem) -> Result<String, String> {
        if is_blank(Some(&item.title)) {
            return Err("title is blank".to_string());
        }
        let mapped = fields_for(item)?;
        let mut parent: Option<(String, &'static str)> = None;
        if !item.parent_ref.is_empty() {
            let Some(parent_item) = self.item(&item.parent_ref) else {
                return Err(format!(
                    "parentRef '{}' is not in the proposal",
                    item.parent_ref
                ));
            };
            let Some(rel) = relationship_between(&parent_item.target_entity, &item.target_entity)
            else {
                return Err(format!(
                    "a {} cannot sit under a {}",
                    item.target_entity, parent_item.target_entity
                ));
            };
            let parent_id = self
                .place(&parent_item)
                .map_err(|e| format!("parent '{}' failed: {e}", item.parent_ref))?;
            parent = Some((parent_id, rel));
        } else if item.target_entity == "Lesson" || item.target_entity == "FlashCard" {
            return Err(format!("a {} needs a parent Concept", item.target_entity));
        }
        let id = self
            .sink
            .create_entity_row(&item.target_entity, &mapped)
            .map_err(|e| e.message)?;
        if let Some((parent_id, rel)) = parent {
            self.sink
                .create_relationship(rel, &parent_id, &id)
                .map_err(|e| e.message)?;
        }
        Ok(id)
    }
}

fn commit_items(
    sink: &mut KnowledgeImportSink<'_>,
    items: &[ImportItem],
) -> StoreResult<ImportOutcome> {
    let mut by_ref: Vec<ImportItem> = Vec::new();
    let mut duplicates: Vec<(String, String)> = Vec::new();
    for item in items {
        if by_ref.iter().any(|i| i.reference == item.reference) {
            duplicates.push((item.reference.clone(), "duplicate ref".to_string()));
        } else {
            by_ref.push(item.clone());
        }
    }
    let mut committer = Committer {
        sink,
        by_ref: by_ref.clone(),
        ids: Vec::new(),
        failed: Vec::new(),
        in_progress: Vec::new(),
    };
    for item in &by_ref {
        let _ = committer.place(item);
    }
    let Committer {
        ids, failed, sink, ..
    } = committer;

    let sunk = sink.commit()?;
    let ref_by_id: HashMap<&str, &str> = ids
        .iter()
        .map(|(r, id)| (id.as_str(), r.as_str()))
        .collect();
    let commit_failures: Vec<(String, String)> = sunk
        .failed
        .iter()
        .map(|(r, reason)| {
            (
                ref_by_id
                    .get(r.as_str())
                    .map_or_else(|| r.clone(), |x| (*x).to_string()),
                reason.clone(),
            )
        })
        .collect();
    let lost: Vec<&String> = commit_failures.iter().map(|(r, _)| r).collect();
    let mut order: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        order.insert(item.reference.as_str(), i);
    }
    let mut all: Vec<(String, String)> = failed;
    all.extend(duplicates);
    all.extend(commit_failures.clone());
    all.sort_by_key(|(r, _)| order.get(r.as_str()).copied().unwrap_or(0));
    let committed = ids.iter().filter(|(r, _)| !lost.contains(&r)).count();
    Ok(ImportOutcome {
        committed,
        failed: all,
    })
}

fn items_of(params: &Map<String, Value>) -> Vec<ImportItem> {
    params
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|v| {
                    let text = |k: &str| {
                        v.get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    ImportItem {
                        reference: text("ref"),
                        target_entity: text("targetEntity"),
                        title: text("title"),
                        parent_ref: text("parentRef"),
                        content: text("content"),
                        front: v.get("front").and_then(Value::as_str).map(str::to_string),
                        back: v.get("back").and_then(Value::as_str).map(str::to_string),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let uuid = env.uuid.clone();
    table.insert(
        "importItems",
        action(move |call, p, deps| {
            let item = Ty::Object(vec![
                params::id("ref"),
                params::pick("targetEntity", &TARGET_ENTITIES),
                params::text("title"),
                params::text("parentRef"),
                params::text("content"),
                params::opt_text("front"),
                params::opt_text("back"),
            ]);
            validate(p, &[params::list("items", item, true), params::opt_id("graphId"), params::opt_text("title")])?;
            let items = items_of(p);
            let title = params::opt_str_of(p, "title");
            let name = if !is_blank(title) {
                title.unwrap_or_default().to_string()
            } else {
                match items.first() {
                    Some(first) if !is_blank(Some(&first.title)) => first.title.clone(),
                    _ => "Import".to_string(),
                }
            };
            let (graph_id, outcome) = commit_into_graph(deps, &uuid, call.uid, params::opt_str_of(p, "graphId"), &name, |sink| commit_items(sink, &items))?;
            Ok(json!({
                "graphId": graph_id,
                "committed": outcome.committed,
                "failed": outcome.failed.iter().map(|(r, why)| json!({ "ref": r, "reason": why })).collect::<Vec<_>>(),
            }))
        }),
    );
}
