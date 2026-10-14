//! Graph mutation types (twin of `engine/mutations.ts`) and the pure graph edits the TS
//! `NodeMutationService` / `RelationshipMutationService` apply between a load and a save. A store
//! composes these with its own get/save; timestamps are passed in so the edits stay deterministic.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use super::pure::{GraphNode, NodeBasedKnowledgeGraph, Relationship};
use crate::json::number;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GraphMutation {
    CreateNode {
        node: GraphNode,
        #[serde(
            rename = "updateIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        update_index: Option<bool>,
    },
    UpdateNode {
        #[serde(rename = "nodeId")]
        node_id: String,
        properties: Map<String, Value>,
        #[serde(
            rename = "updateTimestamp",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        update_timestamp: Option<bool>,
    },
    DeleteNode {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cascade: Option<bool>,
    },
    CreateRelationship {
        relationship: Relationship,
    },
    DeleteRelationship {
        #[serde(rename = "relationshipId")]
        relationship_id: String,
    },
    UpdateNodeTypeIndex {
        #[serde(rename = "nodeType")]
        node_type: String,
        #[serde(rename = "nodeId")]
        node_id: String,
        operation: IndexOperation,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexOperation {
    Add,
    Remove,
}

impl GraphMutation {
    pub fn is_create_node(&self) -> bool {
        matches!(self, Self::CreateNode { .. })
    }
    pub fn is_update_node(&self) -> bool {
        matches!(self, Self::UpdateNode { .. })
    }
    pub fn is_delete_node(&self) -> bool {
        matches!(self, Self::DeleteNode { .. })
    }
    pub fn is_create_relationship(&self) -> bool {
        matches!(self, Self::CreateRelationship { .. })
    }
    pub fn is_delete_relationship(&self) -> bool {
        matches!(self, Self::DeleteRelationship { .. })
    }
    pub fn is_update_node_type_index(&self) -> bool {
        matches!(self, Self::UpdateNodeTypeIndex { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationMetadata {
    pub operation: String,
    pub timestamp: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationBatch {
    pub mutations: Vec<GraphMutation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MutationMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationContext {
    #[serde(rename = "graphId")]
    pub graph_id: String,
    #[serde(
        rename = "seedConceptId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub seed_concept_id: Option<String>,
    #[serde(
        rename = "targetNodeId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub target_node_id: Option<String>,
    #[serde(rename = "existingNodes")]
    pub existing_nodes: Map<String, Value>,
    #[serde(rename = "existingRelationships")]
    pub existing_relationships: Vec<Relationship>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationError {
    pub mutation: GraphMutation,
    pub error: String,
}

fn stamp(now: f64) -> Number {
    match number(now) {
        Value::Number(n) => n,
        _ => Number::from(0),
    }
}

fn unindex(graph: &mut NodeBasedKnowledgeGraph, node_type: &str, node_id: &str) {
    if let Some(list) = graph.node_types.get_mut(node_type) {
        if let Some(at) = list.iter().position(|x| x == node_id) {
            list.remove(at);
        }
    }
}

/// `NodeMutationService.createNode` minus the save.
pub fn create_node(
    graph: &mut NodeBasedKnowledgeGraph,
    node: GraphNode,
    now: f64,
) -> Result<GraphNode, String> {
    if graph.nodes.contains_key(&node.id) {
        return Err(format!("Node {} already exists", node.id));
    }
    graph
        .node_types
        .get_or_insert_with(&node.node_type, Vec::new)
        .push(node.id.clone());
    graph.nodes.insert(node.id.clone(), node.clone());
    graph.updated_at = now;
    Ok(node)
}

/// `NodeMutationService.updateNode`: top-level fields of `updates` overlay the node, `properties` merge
/// key-wise, and `updatedAt` is stamped.
pub fn update_node(
    graph: &mut NodeBasedKnowledgeGraph,
    node_id: &str,
    updates: &Map<String, Value>,
    now: f64,
) -> Result<GraphNode, String> {
    let node = graph
        .nodes
        .get(node_id)
        .ok_or_else(|| format!("Node {node_id} not found"))?;
    let mut merged = match serde_json::to_value(node) {
        Ok(Value::Object(map)) => map,
        _ => return Err(format!("Node {node_id} is unreadable")),
    };
    for (key, value) in updates {
        if key == "properties" {
            if let (Some(Value::Object(base)), Value::Object(patch)) =
                (merged.get_mut("properties"), value)
            {
                for (k, v) in patch {
                    base.insert(k.clone(), v.clone());
                }
            }
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged.insert("updatedAt".to_string(), Value::Number(stamp(now)));
    let updated: GraphNode = serde_json::from_value(Value::Object(merged))
        .map_err(|e| format!("Node {node_id} update is invalid: {e}"))?;
    graph.nodes.insert(node_id.to_string(), updated.clone());
    graph.updated_at = now;
    Ok(updated)
}

/// `NodeMutationService.deleteNode`: drops the node, its index entry and every relationship touching it.
pub fn delete_node(
    graph: &mut NodeBasedKnowledgeGraph,
    node_id: &str,
    now: f64,
) -> Result<GraphNode, String> {
    let node = graph
        .nodes
        .remove(node_id)
        .ok_or_else(|| format!("Node {node_id} not found"))?;
    unindex(graph, &node.node_type, node_id);
    graph
        .relationships
        .retain(|r| r.source != node_id && r.target != node_id);
    graph.updated_at = now;
    Ok(node)
}

/// `NodeMutationService.deleteNodes`: unknown ids are skipped. Returns the removed nodes.
pub fn delete_nodes(
    graph: &mut NodeBasedKnowledgeGraph,
    node_ids: &[String],
    now: f64,
) -> Vec<GraphNode> {
    let mut removed = Vec::new();
    for id in node_ids {
        if let Some(node) = graph.nodes.remove(id) {
            unindex(graph, &node.node_type, id);
            removed.push(node);
        }
    }
    graph
        .relationships
        .retain(|r| !node_ids.contains(&r.source) && !node_ids.contains(&r.target));
    graph.updated_at = now;
    removed
}

/// `RelationshipMutationService.createRelationship`: an existing edge with the same source, target and
/// type is returned unchanged (and `changed` is false, so the caller skips the save).
pub fn create_relationship(
    graph: &mut NodeBasedKnowledgeGraph,
    relationship: Relationship,
    now: f64,
) -> Result<(Relationship, bool), String> {
    if !graph.nodes.contains_key(&relationship.source)
        || !graph.nodes.contains_key(&relationship.target)
    {
        return Err("Source or target node does not exist".to_string());
    }
    if let Some(existing) = graph.relationships.iter().find(|r| {
        r.source == relationship.source
            && r.target == relationship.target
            && r.rel_type == relationship.rel_type
    }) {
        return Ok((existing.clone(), false));
    }
    graph.relationships.push(relationship.clone());
    graph.updated_at = now;
    Ok((relationship, true))
}

/// `RelationshipMutationService.updateRelationship`: `id`, `source`, `target` and `type` are not updatable.
pub fn update_relationship(
    graph: &mut NodeBasedKnowledgeGraph,
    rel_id: &str,
    updates: &Map<String, Value>,
    now: f64,
) -> Result<Relationship, String> {
    let at = graph
        .relationships
        .iter()
        .position(|r| r.id == rel_id)
        .ok_or_else(|| format!("Relationship {rel_id} not found"))?;
    let mut merged = match serde_json::to_value(&graph.relationships[at]) {
        Ok(Value::Object(map)) => map,
        _ => return Err(format!("Relationship {rel_id} is unreadable")),
    };
    for (key, value) in updates {
        if !matches!(key.as_str(), "id" | "source" | "target" | "type") {
            merged.insert(key.clone(), value.clone());
        }
    }
    let updated: Relationship = serde_json::from_value(Value::Object(merged))
        .map_err(|e| format!("Relationship {rel_id} update is invalid: {e}"))?;
    graph.relationships[at] = updated.clone();
    graph.updated_at = now;
    Ok(updated)
}

pub fn delete_relationship(
    graph: &mut NodeBasedKnowledgeGraph,
    rel_id: &str,
    now: f64,
) -> Result<(), String> {
    let at = graph
        .relationships
        .iter()
        .position(|r| r.id == rel_id)
        .ok_or_else(|| format!("Relationship {rel_id} not found"))?;
    graph.relationships.remove(at);
    graph.updated_at = now;
    Ok(())
}

/// `deleteRelationshipsBetween`: returns how many edges went; the caller saves only when it is non-zero.
pub fn delete_relationships_between(
    graph: &mut NodeBasedKnowledgeGraph,
    source: &str,
    target: &str,
    rel_type: Option<&str>,
    now: f64,
) -> usize {
    let before = graph.relationships.len();
    graph.relationships.retain(|r| {
        !(r.source == source && r.target == target && rel_type.is_none_or(|t| r.rel_type == t))
    });
    let deleted = before - graph.relationships.len();
    if deleted > 0 {
        graph.updated_at = now;
    }
    deleted
}

/// `deleteRelationshipsByType`.
pub fn delete_relationships_by_type(
    graph: &mut NodeBasedKnowledgeGraph,
    rel_type: &str,
    now: f64,
) -> usize {
    let before = graph.relationships.len();
    graph.relationships.retain(|r| r.rel_type != rel_type);
    let deleted = before - graph.relationships.len();
    if deleted > 0 {
        graph.updated_at = now;
    }
    deleted
}

/// `deleteRelationships`: always stamps, even when no id matched.
pub fn delete_relationships(graph: &mut NodeBasedKnowledgeGraph, rel_ids: &[String], now: f64) {
    graph.relationships.retain(|r| !rel_ids.contains(&r.id));
    graph.updated_at = now;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, node_type: &str) -> GraphNode {
        serde_json::from_value(json!({
            "id": id, "type": node_type, "properties": { "name": id }, "createdAt": 1, "updatedAt": 1
        }))
        .unwrap()
    }

    fn rel(id: &str, source: &str, target: &str, rel_type: &str) -> Relationship {
        serde_json::from_value(json!({
            "id": id, "source": source, "target": target, "type": rel_type, "direction": "forward"
        }))
        .unwrap()
    }

    fn graph() -> NodeBasedKnowledgeGraph {
        let mut g = NodeBasedKnowledgeGraph::new("g", 10.0);
        create_node(&mut g, node("a", "Concept"), 11.0).unwrap();
        create_node(&mut g, node("b", "Concept"), 12.0).unwrap();
        g
    }

    #[test]
    fn create_node_indexes_and_rejects_a_duplicate() {
        let mut g = graph();
        assert_eq!(g.ids_of("Concept"), ["a", "b"]);
        assert_eq!(g.updated_at, 12.0);
        assert_eq!(
            create_node(&mut g, node("a", "Concept"), 13.0).unwrap_err(),
            "Node a already exists"
        );
        assert_eq!(g.updated_at, 12.0);
    }

    #[test]
    fn update_node_merges_properties_and_stamps() {
        let mut g = graph();
        let updates = json!({ "properties": { "description": "d" } });
        let out = update_node(&mut g, "a", updates.as_object().unwrap(), 20.0).unwrap();
        assert_eq!(out.properties["name"], json!("a"));
        assert_eq!(out.properties["description"], json!("d"));
        assert_eq!(out.updated_at, Some(Number::from(20)));
        assert_eq!(out.created_at, Some(Number::from(1)));
        assert_eq!(
            update_node(&mut g, "zz", &Map::new(), 21.0).unwrap_err(),
            "Node zz not found"
        );
    }

    #[test]
    fn delete_node_cascades_to_edges_and_index() {
        let mut g = graph();
        create_relationship(&mut g, rel("r", "a", "b", "hasChild"), 13.0).unwrap();
        delete_node(&mut g, "a", 14.0).unwrap();
        assert!(g.relationships.is_empty());
        assert_eq!(g.ids_of("Concept"), ["b"]);
        assert!(delete_node(&mut g, "a", 15.0).is_err());
    }

    #[test]
    fn delete_nodes_skips_unknown_ids() {
        let mut g = graph();
        let removed = delete_nodes(&mut g, &["a".into(), "nope".into()], 14.0);
        assert_eq!(removed.len(), 1);
        assert_eq!(g.nodes.len(), 1);
    }

    #[test]
    fn create_relationship_is_idempotent_on_source_target_type() {
        let mut g = graph();
        let (_, changed) =
            create_relationship(&mut g, rel("r1", "a", "b", "hasChild"), 13.0).unwrap();
        assert!(changed);
        let (existing, changed) =
            create_relationship(&mut g, rel("r2", "a", "b", "hasChild"), 14.0).unwrap();
        assert!(!changed);
        assert_eq!(existing.id, "r1");
        assert_eq!(g.relationships.len(), 1);
        assert_eq!(g.updated_at, 13.0);
        // control: a different type is a new edge
        let (_, changed) =
            create_relationship(&mut g, rel("r3", "a", "b", "hasParent"), 15.0).unwrap();
        assert!(changed);
        assert_eq!(
            create_relationship(&mut g, rel("r4", "a", "ghost", "hasChild"), 16.0).unwrap_err(),
            "Source or target node does not exist"
        );
    }

    #[test]
    fn update_relationship_cannot_rewrite_identity() {
        let mut g = graph();
        create_relationship(&mut g, rel("r", "a", "b", "hasChild"), 13.0).unwrap();
        let updates = json!({ "strength": 0.5, "source": "b", "type": "hasParent" });
        let out = update_relationship(&mut g, "r", updates.as_object().unwrap(), 14.0).unwrap();
        assert_eq!(out.source, "a");
        assert_eq!(out.rel_type, "hasChild");
        assert_eq!(out.strength.value().and_then(|n| n.as_f64()), Some(0.5));
        assert!(update_relationship(&mut g, "nope", &Map::new(), 15.0).is_err());
    }

    #[test]
    fn bulk_relationship_deletes_count_and_only_stamp_on_change() {
        let mut g = graph();
        create_relationship(&mut g, rel("r1", "a", "b", "hasChild"), 13.0).unwrap();
        create_relationship(&mut g, rel("r2", "a", "b", "hasParent"), 13.0).unwrap();
        assert_eq!(
            delete_relationships_between(&mut g, "a", "b", Some("hasChild"), 20.0),
            1
        );
        assert_eq!(g.updated_at, 20.0);
        assert_eq!(
            delete_relationships_between(&mut g, "a", "b", Some("hasChild"), 30.0),
            0
        );
        assert_eq!(g.updated_at, 20.0);
        assert_eq!(delete_relationships_by_type(&mut g, "hasParent", 40.0), 1);
        delete_relationships(&mut g, &["x".into()], 50.0);
        assert_eq!(g.updated_at, 50.0);
        assert_eq!(
            delete_relationship(&mut g, "r1", 60.0),
            Err("Relationship r1 not found".to_string())
        );
    }

    #[test]
    fn mutations_round_trip_their_tagged_wire_form() {
        let wire = json!({ "type": "update_node_type_index", "nodeType": "Concept", "nodeId": "a", "operation": "add" });
        let m: GraphMutation = serde_json::from_value(wire.clone()).unwrap();
        assert!(m.is_update_node_type_index());
        assert!(!m.is_create_node());
        assert_eq!(serde_json::to_value(&m).unwrap(), wire);
        let del: GraphMutation =
            serde_json::from_value(json!({ "type": "delete_node", "nodeId": "a" })).unwrap();
        assert!(del.is_delete_node());
    }
}
