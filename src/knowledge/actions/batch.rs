//! Applying a mutation batch to a graph value (`GraphMutationService.ts`): each mutation is validated, applied in
//! order, and a rejected one is reported without stopping the rest.

use serde_json::{Map, Value};

use super::common::stamp;
use crate::knowledge::engine::mutations::{
    GraphMutation, IndexOperation, MutationBatch, MutationError,
};
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};

const INDEXABLE_TYPES: [&str; 10] = [
    "Graph",
    "Concept",
    "Layer",
    "LearningGoal",
    "Milestone",
    "PracticeExercise",
    "Lesson",
    "ConceptMetadata",
    "GraphMetadata",
    "FlashCard",
];

fn validate(graph: &NodeBasedKnowledgeGraph, mutation: &GraphMutation) -> bool {
    match mutation {
        GraphMutation::CreateNode { node, .. } => {
            !graph.nodes.contains_key(&node.id) && !node.id.is_empty() && !node.node_type.is_empty()
        }
        GraphMutation::UpdateNode { node_id, .. } | GraphMutation::DeleteNode { node_id, .. } => {
            graph.nodes.contains_key(node_id)
        }
        GraphMutation::CreateRelationship { relationship } => {
            graph.nodes.contains_key(&relationship.source)
                && graph.nodes.contains_key(&relationship.target)
                && !graph.relationships.iter().any(|r| r.id == relationship.id)
        }
        GraphMutation::DeleteRelationship { relationship_id } => {
            graph.relationships.iter().any(|r| &r.id == relationship_id)
        }
        GraphMutation::UpdateNodeTypeIndex {
            node_type, node_id, ..
        } => INDEXABLE_TYPES.contains(&node_type.as_str()) && graph.nodes.contains_key(node_id),
    }
}

fn add_to_index(graph: &mut NodeBasedKnowledgeGraph, node_type: &str, node_id: &str) {
    let list = graph.node_types.get_or_insert_with(node_type, Vec::new);
    if !list.iter().any(|id| id == node_id) {
        list.push(node_id.to_string());
    }
}

fn remove_from_index(graph: &mut NodeBasedKnowledgeGraph, node_type: &str, node_id: &str) {
    let list = graph.node_types.get_or_insert_with(node_type, Vec::new);
    list.retain(|id| id != node_id);
}

fn apply(
    graph: &mut NodeBasedKnowledgeGraph,
    mutation: &GraphMutation,
    now: f64,
) -> Result<(), String> {
    match mutation {
        GraphMutation::CreateNode { node, update_index } => {
            if graph.nodes.contains_key(&node.id) {
                return Err(format!("Node with ID {} already exists", node.id));
            }
            graph.nodes.insert(node.id.clone(), node.clone());
            if update_index.unwrap_or(true) {
                add_to_index(graph, &node.node_type, &node.id);
            }
        }
        GraphMutation::UpdateNode {
            node_id,
            properties,
            update_timestamp,
        } => {
            let existing = graph
                .nodes
                .get(node_id)
                .ok_or_else(|| format!("Node {node_id} not found"))?;
            let mut merged: Map<String, Value> = existing.properties.clone();
            for (k, v) in properties {
                merged.insert(k.clone(), v.clone());
            }
            let updated = GraphNode {
                properties: merged,
                updated_at: if update_timestamp.unwrap_or(true) {
                    Some(stamp(now))
                } else {
                    existing.updated_at.clone()
                },
                ..existing.clone()
            };
            graph.nodes.insert(node_id.clone(), updated);
        }
        GraphMutation::DeleteNode { node_id, cascade } => {
            let node = graph
                .nodes
                .remove(node_id)
                .ok_or_else(|| format!("Node {node_id} not found"))?;
            remove_from_index(graph, &node.node_type, node_id);
            if cascade.unwrap_or(true) {
                graph
                    .relationships
                    .retain(|r| &r.source != node_id && &r.target != node_id);
            }
        }
        GraphMutation::CreateRelationship { relationship } => {
            if !graph.nodes.contains_key(&relationship.source) {
                return Err(format!("Source node {} not found", relationship.source));
            }
            if !graph.nodes.contains_key(&relationship.target) {
                return Err(format!("Target node {} not found", relationship.target));
            }
            if graph.relationships.iter().any(|r| r.id == relationship.id) {
                return Err(format!(
                    "Relationship with ID {} already exists",
                    relationship.id
                ));
            }
            graph.relationships.push(relationship.clone());
        }
        GraphMutation::DeleteRelationship { relationship_id } => {
            if !graph.relationships.iter().any(|r| &r.id == relationship_id) {
                return Err(format!("Relationship {relationship_id} not found"));
            }
            graph.relationships.retain(|r| &r.id != relationship_id);
        }
        GraphMutation::UpdateNodeTypeIndex {
            node_type,
            node_id,
            operation,
        } => match operation {
            IndexOperation::Add => add_to_index(graph, node_type, node_id),
            IndexOperation::Remove => remove_from_index(graph, node_type, node_id),
        },
    }
    graph.updated_at = now;
    Ok(())
}

/// `applyMutationBatchSafe`: the graph after every mutation that applied, and the ones that did not.
pub fn apply_mutation_batch_safe(
    graph: &NodeBasedKnowledgeGraph,
    batch: &MutationBatch,
    now: f64,
) -> (NodeBasedKnowledgeGraph, Vec<MutationError>) {
    let mut updated = graph.clone();
    let mut errors = Vec::new();
    for mutation in &batch.mutations {
        if !validate(&updated, mutation) {
            errors.push(MutationError {
                mutation: mutation.clone(),
                error: "Validation failed".to_string(),
            });
            continue;
        }
        if let Err(error) = apply(&mut updated, mutation, now) {
            errors.push(MutationError {
                mutation: mutation.clone(),
                error,
            });
        }
    }
    (updated, errors)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn graph() -> NodeBasedKnowledgeGraph {
        serde_json::from_value(json!({
            "id": "g", "createdAt": 1, "updatedAt": 1,
            "nodes": { "a": { "id": "a", "type": "Concept", "properties": { "name": "A" } } },
            "nodeTypes": { "Concept": ["a"] }, "relationships": []
        }))
        .unwrap()
    }

    fn batch(value: Value) -> MutationBatch {
        serde_json::from_value(json!({ "mutations": value })).unwrap()
    }

    #[test]
    fn a_rejected_mutation_is_reported_and_the_rest_still_apply() {
        let b = batch(json!([
            { "type": "update_node", "nodeId": "missing", "properties": { "x": 1 } },
            { "type": "update_node", "nodeId": "a", "properties": { "name": "B" } }
        ]));
        let (g, errors) = apply_mutation_batch_safe(&graph(), &b, 9.0);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error, "Validation failed");
        assert_eq!(g.nodes.get("a").unwrap().properties["name"], json!("B"));
        assert_eq!(g.updated_at, 9.0);
    }

    #[test]
    fn a_created_node_is_indexed_unless_the_mutation_says_otherwise() {
        let node = json!({ "id": "n", "type": "Layer", "properties": {} });
        let (g, _) = apply_mutation_batch_safe(
            &graph(),
            &batch(json!([{ "type": "create_node", "node": node }])),
            2.0,
        );
        assert_eq!(g.ids_of("Layer"), ["n"]);
        let (g, _) = apply_mutation_batch_safe(
            &graph(),
            &batch(json!([{ "type": "create_node", "node": node, "updateIndex": false }])),
            2.0,
        );
        assert!(g.ids_of("Layer").is_empty());
    }

    #[test]
    fn deleting_a_node_drops_its_edges_and_index_entry() {
        let mut g = graph();
        g.nodes.insert(
            "b".into(),
            serde_json::from_value(json!({ "id": "b", "type": "Concept", "properties": {} }))
                .unwrap(),
        );
        g.relationships.push(
            serde_json::from_value(
                json!({ "id": "r", "source": "a", "target": "b", "type": "hasChild" }),
            )
            .unwrap(),
        );
        let (g, errors) = apply_mutation_batch_safe(
            &g,
            &batch(json!([{ "type": "delete_node", "nodeId": "a" }])),
            3.0,
        );
        assert!(errors.is_empty());
        assert!(g.relationships.is_empty());
        assert!(g.ids_of("Concept").is_empty());
    }
}
