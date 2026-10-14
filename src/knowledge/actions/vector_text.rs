//! The text a node is embedded as (`KnowledgeVectorService.buildEmbeddingText`).

use serde_json::Value;

use crate::knowledge::engine::pure::GraphNode;

fn text<'a>(node: &'a GraphNode, key: &str) -> Option<&'a str> {
    node.properties.get(key).and_then(Value::as_str)
}

pub fn build_embedding_text(node: &GraphNode) -> String {
    let mut parts: Vec<String> = Vec::new();
    let name = text(node, "name")
        .or_else(|| text(node, "title"))
        .unwrap_or_default();
    if !name.is_empty() {
        parts.push(name.to_string());
    }
    for key in ["description", "narrative"] {
        if let Some(t) = text(node, key).filter(|t| !t.is_empty()) {
            parts.push(t.to_string());
        }
    }
    if let Some(t) = text(node, "goal").filter(|t| !t.is_empty()) {
        parts.push(format!("Goal: {t}"));
    }
    if let Some(t) = text(node, "focus").filter(|t| !t.is_empty()) {
        parts.push(format!("Focus: {t}"));
    }
    if let Some(n) = node.properties.get("layerNumber").and_then(Value::as_f64) {
        parts.push(format!("Layer {}", crate::jsnumber::js_number(n)));
    }
    if let Some(n) = node.properties.get("sequence").and_then(Value::as_f64) {
        parts.push(format!("Sequence {}", crate::jsnumber::js_number(n)));
    }
    let tags: Vec<&str> = node
        .properties
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !tags.is_empty() {
        parts.push(format!("Tags: {}", tags.join(", ")));
    }
    let keywords: Vec<&str> = node
        .properties
        .get("keywords")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|k| !k.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !keywords.is_empty() {
        parts.push(format!("Keywords: {}", keywords.join(", ")));
    }
    let joined = parts.join("\n\n").trim().to_string();
    if joined.is_empty() {
        if name.is_empty() {
            node.id.clone()
        } else {
            name.to_string()
        }
    } else {
        joined
    }
}
