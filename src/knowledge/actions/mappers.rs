//! Graph nodes as the contract's concepts (`mappers.ts`).

use serde_json::{json, Value};

use super::common::{mastery_of, to_route_key};
use super::queries::{canonical_concept_key, convert_node_to_concept_display};
use crate::json::number;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};

/// A Concept node as the contract's concept, with the learner's mastery.
pub fn to_knowledge_concept(graph: &NodeBasedKnowledgeGraph, node: &GraphNode, uid: &str) -> Value {
    let display = convert_node_to_concept_display(node, graph, true);
    let has_lesson = graph.relationships.iter().any(|rel| {
        rel.source == display.id
            && rel.rel_type == "hasLesson"
            && graph
                .nodes
                .get(&rel.target)
                .is_some_and(|n| n.node_type == "Lesson")
    });
    json!({
        "id": display.id,
        "name": display.name,
        "description": display.description,
        "layer": number(display.layer),
        "isSeed": display.is_seed,
        "sequence": number(display.sequence.unwrap_or(0.0)),
        "prerequisites": display.prerequisites,
        "parents": display.parents,
        "masteryLevel": number(mastery_of(graph, uid, &display.id)),
        "hasLesson": has_lesson,
        "canonicalKey": canonical_concept_key(node),
        "routeKey": to_route_key(&display.id),
    })
}
