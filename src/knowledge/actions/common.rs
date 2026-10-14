//! What the action ports share: the graph loaders (`lesson.ts` `loadGraph` / `conceptOf`), the route-key
//! mappers (`mappers.ts`), property readers and the JS comparisons the TS ordering relies on.

use std::cmp::Ordering;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Map, Number, Value};

use crate::json::number;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use crate::knowledge::store::{KnowledgeDeps, KnowledgeError, StoreResult};

/// `loadGraph`: the learner's graph, or the refusal that names the path.
pub fn load_graph(
    deps: &KnowledgeDeps,
    uid: &str,
    graph_id: &str,
) -> StoreResult<NodeBasedKnowledgeGraph> {
    if !deps.graphs.graph_exists(uid, graph_id) {
        return Err(KnowledgeError::not_found(format!(
            "Path {graph_id} not found"
        )));
    }
    deps.graphs.get_graph(uid, graph_id)
}

/// `conceptOf`.
pub fn concept_of<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    concept_id: &str,
) -> StoreResult<&'a GraphNode> {
    match graph.nodes.get(concept_id) {
        Some(node) if node.node_type == "Concept" => Ok(node),
        _ => Err(KnowledgeError::not_found(format!(
            "Concept {concept_id} not found in path {}",
            graph.id
        ))),
    }
}

/// A concept id as an opaque URL-safe key (base64url of its UTF-8).
pub fn to_route_key(concept_id: &str) -> String {
    URL_SAFE_NO_PAD.encode(concept_id.as_bytes())
}

/// The concept id a route key stands for; `None` when the text is not a route key.
pub fn from_route_key(key: &str) -> Option<String> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(key).ok()?;
    let id = String::from_utf8_lossy(&bytes).into_owned();
    (to_route_key(&id) == key).then_some(id)
}

pub fn str_prop(node: &GraphNode, key: &str) -> String {
    node.properties
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn opt_str_prop(node: &GraphNode, key: &str) -> Option<String> {
    node.properties
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn num_prop(node: &GraphNode, key: &str) -> Option<f64> {
    node.properties.get(key).and_then(Value::as_f64)
}

pub fn stamp(ms: f64) -> Number {
    match number(ms) {
        Value::Number(n) => n,
        _ => Number::from(0),
    }
}

/// The learner's mastery of one concept in a graph they own: their Progress node, else 0.
pub fn mastery_of(graph: &NodeBasedKnowledgeGraph, uid: &str, concept_id: &str) -> f64 {
    graph
        .nodes
        .get(&format!("progress-{uid}-{concept_id}"))
        .filter(|n| n.node_type == "Progress")
        .and_then(|n| num_prop(n, "masteryLevel"))
        .unwrap_or(0.0)
}

pub fn concept_ref(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name, "routeKey": to_route_key(id) })
}

/// `createGraphNode(id, type, properties, createdAt, updatedAt)`.
pub fn make_node(id: &str, node_type: &str, properties: Value, now: f64) -> GraphNode {
    GraphNode {
        id: id.to_string(),
        node_type: node_type.to_string(),
        properties: properties.as_object().cloned().unwrap_or_default(),
        created_at: Some(stamp(now)),
        updated_at: Some(stamp(now)),
    }
}

/// A stand-in for ICU `localeCompare` on the labels KFlow stores: letters ignore case first (lowercase sorts
/// before uppercase on a tie), digits sort before letters, punctuation before digits.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    fn class(c: char) -> u8 {
        if c.is_whitespace() {
            0
        } else if c.is_alphabetic() {
            3
        } else if c.is_numeric() {
            2
        } else {
            1
        }
    }
    let primary = |s: &str| -> Vec<(u8, String)> {
        s.chars()
            .map(|c| (class(c), c.to_lowercase().collect::<String>()))
            .collect()
    };
    primary(a).cmp(&primary(b)).then_with(|| {
        let tertiary = |s: &str| -> Vec<bool> { s.chars().map(char::is_uppercase).collect() };
        tertiary(a).cmp(&tertiary(b))
    })
}

/// A JS `a - b` comparator over numbers (NaN compares equal, as `sort` treats it).
pub fn num_order(a: f64, b: f64) -> Ordering {
    (a - b).partial_cmp(&0.0).unwrap_or(Ordering::Equal)
}

pub fn as_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_key_round_trips_and_a_stranger_is_refused() {
        let key = to_route_key("Light & Shadow");
        assert_eq!(from_route_key(&key).as_deref(), Some("Light & Shadow"));
        assert_eq!(from_route_key("not a key!"), None);
        assert_eq!(from_route_key(""), None);
    }

    #[test]
    fn locale_compare_folds_case_before_breaking_ties_lowercase_first() {
        assert_eq!(locale_compare("apple", "Banana"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("1", "a"), Ordering::Less);
    }
}
