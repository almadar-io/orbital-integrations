//! The graph reads the actions lean on: the twin of `engine/server/queries/graphQueryService.ts`,
//! `vector/convergence.ts`, `vector/conceptCanonicalizer.ts` (`canonicalConceptKey`) and
//! `operations/graphOperations/assembleCourse.ts`.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use super::common::{locale_compare, make_node, num_order, num_prop, str_prop};
use crate::json::number;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph, Relationship};

pub const PATH_LINK_JACCARD: f64 = 0.1;

/// The ids indexed under `node_type`, resolved to nodes (ids with no node are skipped).
pub fn nodes_by_type<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    node_type: &str,
) -> Vec<&'a GraphNode> {
    graph
        .ids_of(node_type)
        .iter()
        .filter_map(|id| graph.nodes.get(id))
        .collect()
}

#[derive(Debug, Clone)]
pub struct SeedConcept {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct LearningPathSummary {
    pub id: String,
    pub title: String,
    pub description: String,
    pub concept_count: usize,
    pub seed_concept: Option<SeedConcept>,
    pub updated_at: f64,
    pub created_at: f64,
}

pub fn extract_learning_path_summary(graph: &NodeBasedKnowledgeGraph) -> LearningPathSummary {
    const UNTITLED: &str = "Untitled Learning Path";
    let mut title = UNTITLED.to_string();
    let mut description = String::new();
    if let Some(goal) = graph
        .ids_of("LearningGoal")
        .first()
        .and_then(|id| graph.nodes.get(id))
    {
        if goal.node_type == "LearningGoal" {
            let name = str_prop(goal, "name");
            if !name.is_empty() {
                title = name;
            }
            description = str_prop(goal, "description");
        }
    }
    let seed_id = graph.seed_concept_id.as_deref().filter(|s| !s.is_empty());
    if title == UNTITLED {
        if let Some(seed) = seed_id.and_then(|id| graph.nodes.get(id)) {
            if seed.node_type == "Concept" {
                let name = str_prop(seed, "name");
                if !name.is_empty() {
                    title = name;
                }
            }
        }
    }
    let mut seed_concept = None;
    if let Some(seed_id) = seed_id {
        let mut seed_node = graph.nodes.get(seed_id);
        if seed_node.is_none() {
            for id in graph.ids_of("Concept") {
                if let Some(n) = graph.nodes.get(id) {
                    if n.node_type == "Concept"
                        && (n.properties.get("isSeed") == Some(&Value::Bool(true))
                            || n.id == seed_id
                            || str_prop(n, "name") == seed_id)
                    {
                        seed_node = Some(n);
                        break;
                    }
                }
            }
        }
        if let Some(n) = seed_node.filter(|n| n.node_type == "Concept") {
            seed_concept = Some(SeedConcept {
                id: n.id.clone(),
                name: str_prop(n, "name"),
                description: str_prop(n, "description"),
            });
        }
    }
    LearningPathSummary {
        id: graph.id.clone(),
        title,
        description,
        concept_count: graph.ids_of("Concept").len(),
        seed_concept,
        updated_at: graph.updated_at,
        created_at: graph.created_at,
    }
}

#[derive(Debug, Clone)]
pub struct SharedConceptEdge {
    pub source: String,
    pub target: String,
    pub weight: f64,
    pub shared: usize,
    pub shared_concepts: Vec<String>,
}

pub struct PathInput {
    pub id: String,
    pub concept_ids: Vec<String>,
}

pub fn compute_shared_concept_edges(paths: &[PathInput]) -> Vec<SharedConceptEdge> {
    let sets: Vec<HashSet<&String>> = paths
        .iter()
        .map(|p| p.concept_ids.iter().collect())
        .collect();
    let mut edges = Vec::new();
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            let (a, b) = (&sets[i], &sets[j]);
            let mut intersection: Vec<String> = a.intersection(b).map(|s| (*s).clone()).collect();
            if intersection.is_empty() {
                continue;
            }
            let union = a.len() + b.len() - intersection.len();
            intersection.sort_by(|x, y| locale_compare(x, y));
            edges.push(SharedConceptEdge {
                source: paths[i].id.clone(),
                target: paths[j].id.clone(),
                weight: if union > 0 {
                    intersection.len() as f64 / union as f64
                } else {
                    0.0
                },
                shared: intersection.len(),
                shared_concepts: intersection,
            });
        }
    }
    edges
}

#[derive(Debug, Clone)]
pub struct PathCluster {
    pub id: String,
    pub name: String,
    pub size: usize,
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    let mut root = i;
    while parent[root] != root {
        root = parent[root];
    }
    while parent[i] != root {
        let next = parent[i];
        parent[i] = root;
        i = next;
    }
    root
}

/// Every path's cluster, in the order the TS `Map` holds them.
pub fn assign_clusters(
    paths: &[PathInput],
    shared: &[SharedConceptEdge],
    threshold: f64,
) -> Vec<(String, PathCluster)> {
    let n = paths.len();
    if n == 0 {
        return Vec::new();
    }
    let index_of: HashMap<&str, usize> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.as_str(), i))
        .collect();
    let mut parent: Vec<usize> = (0..n).collect();
    for edge in shared {
        if edge.weight < threshold {
            continue;
        }
        let (Some(&si), Some(&ti)) = (
            index_of.get(edge.source.as_str()),
            index_of.get(edge.target.as_str()),
        ) else {
            continue;
        };
        let ra = find(&mut parent, si);
        let rb = find(&mut parent, ti);
        if ra != rb {
            parent[rb] = ra;
        }
    }
    fn unique(p: &PathInput) -> Vec<&String> {
        let mut seen = HashSet::new();
        p.concept_ids.iter().filter(|c| seen.insert(*c)).collect()
    }
    let concept_sets: Vec<Vec<&String>> = paths.iter().map(unique).collect();
    let mut global_occ: HashMap<&String, usize> = HashMap::new();
    for set in &concept_sets {
        for c in set {
            *global_occ.entry(c).or_insert(0) += 1;
        }
    }
    let mut roots: Vec<usize> = Vec::new();
    let mut members_by_root: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        if !members_by_root.contains_key(&r) {
            roots.push(r);
        }
        members_by_root.entry(r).or_default().push(i);
    }
    let mut result = Vec::new();
    let mut next_id = 0;
    for root in roots {
        let members = &members_by_root[&root];
        if members.len() < 2 {
            continue;
        }
        let mut coverage: Vec<(&String, usize)> = Vec::new();
        for &mi in members {
            for c in &concept_sets[mi] {
                match coverage.iter_mut().find(|(k, _)| k == c) {
                    Some(entry) => entry.1 += 1,
                    None => coverage.push((c, 1)),
                }
            }
        }
        let mut best: Option<&String> = None;
        let mut best_cov = 0usize;
        let mut have_best = false;
        for (concept, cov) in &coverage {
            let better = !have_best
                || *cov > best_cov
                || (*cov == best_cov
                    && best.is_some_and(|b| {
                        global_occ[concept] < global_occ[b]
                            || (global_occ[concept] == global_occ[b]
                                && locale_compare(concept, b).is_lt())
                    }));
            if better {
                best = Some(concept);
                best_cov = *cov;
                have_best = true;
            }
        }
        let cluster = PathCluster {
            id: format!("cluster-{next_id}"),
            name: best
                .cloned()
                .unwrap_or_else(|| paths[members[0]].id.clone()),
            size: members.len(),
        };
        next_id += 1;
        for &mi in members {
            result.push((paths[mi].id.clone(), cluster.clone()));
        }
    }
    result
}

#[derive(Debug, Clone)]
pub struct ConceptDisplay {
    pub id: String,
    pub name: String,
    pub description: String,
    pub layer: f64,
    pub is_seed: bool,
    pub sequence: Option<f64>,
    pub parents: Vec<String>,
    pub children: Vec<String>,
    pub prerequisites: Vec<String>,
}

fn extract_layer(graph: &NodeBasedKnowledgeGraph, node_id: &str) -> f64 {
    let Some(node) = graph
        .nodes
        .get(node_id)
        .filter(|n| n.node_type == "Concept")
    else {
        return 0.0;
    };
    match node.properties.get("layer") {
        Some(v) if !v.is_null() => {
            let n = crate::knowledge::engine::coerce::num(Some(v));
            if v.is_number() && n >= 0.0 {
                n
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

/// The nodes `node_id` relates to by `rel_type`; a parent/child link counts whichever side stored it.
fn related_nodes<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    node_id: &str,
    rel_type: &str,
    source_side: bool,
) -> Vec<&'a GraphNode> {
    let mut seen: Vec<&GraphNode> = Vec::new();
    for r in &graph.relationships {
        let related_id: Option<&str> = match rel_type {
            "hasParent" => {
                if r.source == node_id && r.rel_type == "hasParent" {
                    Some(&r.target)
                } else if r.target == node_id && r.rel_type == "hasChild" {
                    Some(&r.source)
                } else {
                    None
                }
            }
            "hasChild" => {
                if r.source == node_id && r.rel_type == "hasChild" {
                    Some(&r.target)
                } else if r.target == node_id && r.rel_type == "hasParent" {
                    Some(&r.source)
                } else {
                    None
                }
            }
            other if r.rel_type == other => {
                if source_side && r.source == node_id {
                    Some(&r.target)
                } else if !source_side && r.target == node_id {
                    Some(&r.source)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(related) = related_id.and_then(|id| graph.nodes.get(id)) {
            if !seen.iter().any(|s| s.id == related.id) {
                seen.push(related);
            }
        }
    }
    seen
}

fn relationship_names(
    graph: &NodeBasedKnowledgeGraph,
    node_id: &str,
    rel_type: &str,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for n in related_nodes(graph, node_id, rel_type, true) {
        let name = str_prop(n, "name");
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

pub fn convert_node_to_concept_display(
    node: &GraphNode,
    graph: &NodeBasedKnowledgeGraph,
    include_relationships: bool,
) -> ConceptDisplay {
    let (parents, children, prerequisites) = if include_relationships {
        (
            relationship_names(graph, &node.id, "hasParent"),
            relationship_names(graph, &node.id, "hasChild"),
            relationship_names(graph, &node.id, "hasPrerequisite"),
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    ConceptDisplay {
        id: node.id.clone(),
        name: str_prop(node, "name"),
        description: str_prop(node, "description"),
        layer: extract_layer(graph, &node.id),
        is_seed: graph.seed_concept_id.as_deref() == Some(node.id.as_str()),
        sequence: num_prop(node, "sequence"),
        parents,
        children,
        prerequisites,
    }
}

#[derive(Debug, Clone)]
pub struct LayerInfo {
    pub layer_number: f64,
    pub concept_count: usize,
    pub goal: Option<String>,
}

fn extract_layer_info(graph: &NodeBasedKnowledgeGraph) -> Vec<LayerInfo> {
    let mut info: Vec<LayerInfo> = graph
        .ids_of("Layer")
        .iter()
        .filter_map(|id| {
            let n = graph.nodes.get(id).filter(|n| n.node_type == "Layer")?;
            let layer_num = num_prop(n, "layerNumber").unwrap_or(0.0);
            let by_belongs = graph
                .relationships
                .iter()
                .filter(|r| r.rel_type == "belongsToLayer" && &r.target == id)
                .count();
            let by_contains = graph
                .relationships
                .iter()
                .filter(|r| r.rel_type == "containsConcept" && &r.source == id)
                .count();
            let by_prop = graph
                .nodes
                .values()
                .filter(|node| {
                    node.node_type == "Concept"
                        && node.properties.get("layer").is_some_and(|v| {
                            crate::knowledge::engine::coerce::num(Some(v)) == layer_num
                                && !v.is_null()
                        })
                })
                .count();
            Some(LayerInfo {
                layer_number: layer_num,
                concept_count: by_belongs.max(by_contains).max(by_prop),
                goal: n
                    .properties
                    .get("goal")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    info.sort_by(|a, b| num_order(a.layer_number, b.layer_number));
    info
}

pub struct ConceptsByLayer {
    pub concepts: Vec<ConceptDisplay>,
    /// Layers ascending, each with its concepts in display order.
    pub grouped: Vec<(f64, Vec<ConceptDisplay>)>,
    pub layer_info: Vec<LayerInfo>,
}

pub fn get_concepts_by_layer(
    graph: &NodeBasedKnowledgeGraph,
    include_relationships: bool,
) -> ConceptsByLayer {
    let mut concepts: Vec<ConceptDisplay> = graph
        .ids_of("Concept")
        .iter()
        .filter_map(|id| graph.nodes.get(id).filter(|n| n.node_type == "Concept"))
        .map(|n| convert_node_to_concept_display(n, graph, include_relationships))
        .collect();
    concepts.sort_by(|a, b| {
        if a.layer != b.layer {
            num_order(a.layer, b.layer)
        } else {
            num_order(a.sequence.unwrap_or(0.0), b.sequence.unwrap_or(0.0))
        }
    });
    let mut grouped: Vec<(f64, Vec<ConceptDisplay>)> = Vec::new();
    for c in &concepts {
        match grouped.iter_mut().find(|(layer, _)| *layer == c.layer) {
            Some((_, list)) => list.push(c.clone()),
            None => grouped.push((c.layer, vec![c.clone()])),
        }
    }
    ConceptsByLayer {
        concepts,
        grouped,
        layer_info: extract_layer_info(graph),
    }
}

pub struct ConceptDetail {
    pub concept: ConceptDisplay,
    pub lesson_id: Option<String>,
    pub parents: Vec<(String, String)>,
    pub prerequisites: Vec<(String, String)>,
}

fn related_concepts(
    graph: &NodeBasedKnowledgeGraph,
    concept_id: &str,
    rel_type: &str,
) -> Vec<(String, String)> {
    related_nodes(graph, concept_id, rel_type, true)
        .into_iter()
        .map(|n| (n.id.clone(), str_prop(n, "name")))
        .collect()
}

/// The slice of `getConceptDetail` the actions read: the concept, its lesson id, parents and prerequisites.
pub fn get_concept_detail(
    graph: &NodeBasedKnowledgeGraph,
    concept_id: &str,
) -> Result<ConceptDetail, String> {
    let concept_node = match graph.nodes.get(concept_id) {
        Some(n) if n.node_type == "Concept" => n,
        _ => return Err(format!("Concept {concept_id} not found")),
    };
    let concept = convert_node_to_concept_display(concept_node, graph, true);
    let lesson_id = graph
        .relationships
        .iter()
        .find(|r| r.source == concept_id && r.rel_type == "hasLesson")
        .and_then(|r| graph.nodes.get(&r.target))
        .filter(|n| n.node_type == "Lesson")
        .map(|n| n.id.clone());
    Ok(ConceptDetail {
        concept,
        lesson_id,
        parents: related_concepts(graph, concept_id, "hasParent"),
        prerequisites: related_concepts(graph, concept_id, "hasPrerequisite"),
    })
}

/// A concept's cross-graph identity: `canonicalName` when set, else its id.
pub fn canonical_concept_key(node: &GraphNode) -> String {
    if node.node_type != "Concept" {
        return node.id.clone();
    }
    match node.properties.get("canonicalName").and_then(Value::as_str) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => node.id.clone(),
    }
}

/// Jaccard overlap of two canonical-name sets, in [0,1]; an empty set gives 0.
pub fn convergence_score(a_keys: &[String], b_keys: &[String]) -> f64 {
    if a_keys.is_empty() || b_keys.is_empty() {
        return 0.0;
    }
    let a: HashSet<&String> = a_keys.iter().collect();
    let b: HashSet<&String> = b_keys.iter().collect();
    let intersection = a.intersection(&b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

pub struct AssembledCourse {
    pub id: String,
    pub node: GraphNode,
    pub relationship: Relationship,
}

/// `assembleCourse`: a Course pointer node over a concept graph, one module per layer.
pub fn assemble_course(
    graph: &NodeBasedKnowledgeGraph,
    course_title: &str,
    description: Option<&str>,
    now: f64,
) -> AssembledCourse {
    let nodes: Vec<&GraphNode> = graph.nodes.values().collect();
    let mut layer_names: Vec<(f64, String)> = Vec::new();
    for n in &nodes {
        if n.node_type == "Layer" {
            let number_ = num_prop(n, "layerNumber").unwrap_or(f64::NAN);
            let name = n
                .properties
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("Layer {}", crate::jsnumber::js_number(number_)));
            match layer_names.iter_mut().find(|(k, _)| *k == number_) {
                Some(entry) => entry.1 = name,
                None => layer_names.push((number_, name)),
            }
        }
    }
    let mut concept_nodes: Vec<(usize, &GraphNode)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.node_type == "Concept")
        .map(|(i, n)| (i, *n))
        .collect();
    concept_nodes.sort_by(|(ia, a), (ib, b)| {
        let sa = num_prop(a, "sequence").unwrap_or(f64::INFINITY);
        let sb = num_prop(b, "sequence").unwrap_or(f64::INFINITY);
        if sa != sb {
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            ia.cmp(ib)
        }
    });
    let mut by_layer: Vec<(f64, Vec<Value>)> = Vec::new();
    for (_, c) in &concept_nodes {
        let layer = num_prop(c, "layer").unwrap_or(0.0);
        let reference = json!({ "nodeId": c.id, "sourceGraphId": graph.id });
        match by_layer.iter_mut().find(|(k, _)| *k == layer) {
            Some((_, refs)) => refs.push(reference),
            None => by_layer.push((layer, vec![reference])),
        }
    }
    by_layer.sort_by(|(a, _), (b, _)| num_order(*a, *b));
    let modules: Vec<Value> = by_layer
        .into_iter()
        .map(|(layer, refs)| {
            let title = layer_names
                .iter()
                .find(|(k, _)| *k == layer)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| format!("Layer {}", crate::jsnumber::js_number(layer)));
            json!({ "title": title, "conceptRefs": refs })
        })
        .collect();
    let resolved_description = description
        .map(str::to_string)
        .or_else(|| {
            concept_nodes.first().and_then(|(_, c)| {
                c.properties
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| course_title.to_string());
    let course_id = format!("course-{course_title}");
    let mut props = Map::new();
    props.insert("id".into(), json!(course_id));
    props.insert("title".into(), json!(course_title));
    props.insert("description".into(), json!(resolved_description));
    props.insert("modules".into(), Value::Array(modules));
    props.insert("publishStatus".into(), json!("draft"));
    props.insert("createdAt".into(), number(now));
    props.insert("updatedAt".into(), number(now));
    let node = make_node(&course_id, "Course", Value::Object(props), now);
    let relationship =
        crate::knowledge::firestore::forward_relationship(&graph.id, &course_id, "hasCourse", now);
    AssembledCourse {
        id: course_id,
        node,
        relationship,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::engine::pure::NodeBasedKnowledgeGraph;

    fn graph(value: Value) -> NodeBasedKnowledgeGraph {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn convergence_is_jaccard_and_zero_for_an_empty_side() {
        let a = vec!["x".to_string(), "y".to_string()];
        let b = vec!["y".to_string(), "z".to_string()];
        assert!((convergence_score(&a, &b) - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(convergence_score(&a, &[]), 0.0);
    }

    #[test]
    fn clusters_name_the_concept_present_in_most_member_paths() {
        let paths = vec![
            PathInput {
                id: "p1".into(),
                concept_ids: vec!["a".into(), "b".into()],
            },
            PathInput {
                id: "p2".into(),
                concept_ids: vec!["b".into(), "c".into()],
            },
            PathInput {
                id: "p3".into(),
                concept_ids: vec!["z".into()],
            },
        ];
        let edges = compute_shared_concept_edges(&paths);
        assert_eq!(edges.len(), 1);
        let clusters = assign_clusters(&paths, &edges, PATH_LINK_JACCARD);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].1.name, "b");
        assert!(clusters.iter().all(|(id, _)| id != "p3"));
    }

    #[test]
    fn a_path_without_a_goal_is_titled_by_its_seed_concept() {
        let g = graph(json!({
            "id": "g", "seedConceptId": "Light", "createdAt": 1, "updatedAt": 2,
            "nodes": { "Light": { "id": "Light", "type": "Concept", "properties": { "name": "Light" } } },
            "nodeTypes": { "Concept": ["Light"] }, "relationships": []
        }));
        let summary = extract_learning_path_summary(&g);
        assert_eq!(summary.title, "Light");
        assert_eq!(summary.concept_count, 1);
    }
}
