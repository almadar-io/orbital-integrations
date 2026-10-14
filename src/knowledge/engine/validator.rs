//! Graph integrity validator: the 15 invariants of `engine/validator.ts`, reported in the same order with the same messages.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::coerce::{js_number, opt_num, str};
use super::pure::NodeBasedKnowledgeGraph;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub code: String,
    pub severity: Severity,
    #[serde(rename = "nodeId", default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub message: String,
}

const VALID_RELATIONSHIP_TYPES: [&str; 50] = [
    "hasParent",
    "hasChild",
    "hasPrerequisite",
    "isPrerequisiteOf",
    "hasTopLevelConcept",
    "topLevelConceptOf",
    "containsConcept",
    "belongsToGraph",
    "belongsToLayer",
    "hasLayer",
    "hasLearningGoal",
    "hasMilestone",
    "belongsToGoal",
    "hasPracticeExercise",
    "hasLesson",
    "belongsToConcept",
    "hasMetadata",
    "hasFlashCard",
    "precedesLayer",
    "followsLayer",
    "precedesMilestone",
    "followsMilestone",
    "precedesConcept",
    "followsConcept",
    "referencesConcept",
    "referencesLayer",
    "hasShortTermGoal",
    "generatedBy",
    "hasSeedConcept",
    "hasAssessment",
    "hasQuestion",
    "hasTranslation",
    "translationOf",
    "hasLanguageConfig",
    "hasEnrolledStudent",
    "belongsToStudent",
    "assignedToCourse",
    "hasStory",
    "hasProgress",
    "forConcept",
    "hasEnrollment",
    "forCourse",
    "submitted",
    "forAssessment",
    "achieved",
    "hasPreferences",
    "belongsToCategory",
    "instantiatesInto",
    "explainsConcept",
    "derivedFrom",
];

const REQUIRED_PROPS: [(&str, &[&str]); 8] = [
    ("Concept", &["name", "description"]),
    ("Layer", &["layerNumber"]),
    ("LearningGoal", &["name", "description", "type"]),
    ("Milestone", &["name", "completed"]),
    ("Lesson", &["content", "generatedAt"]),
    ("Course", &["title", "description", "modules"]),
    ("Assessment", &["title", "conceptRefs"]),
    (
        "Translation",
        &["sourceNodeId", "sourceNodeType", "language", "status"],
    ),
];

struct Collector(Vec<Violation>);

impl Collector {
    fn add(&mut self, code: &str, severity: Severity, node_id: Option<&str>, message: String) {
        self.0.push(Violation {
            code: code.to_string(),
            severity,
            node_id: node_id.map(str::to_string),
            message,
        });
    }
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|v| !v.is_empty())
}

pub fn validate_graph(graph: &NodeBasedKnowledgeGraph) -> Vec<Violation> {
    use Severity::{Error, Warning};
    let mut out = Collector(Vec::new());

    // Invariant 1: seedConceptId exists and is a Concept
    if let Some(seed) = non_empty(&graph.seed_concept_id) {
        match graph.nodes.get(seed) {
            None => out.add(
                "KG_INV_01_SEED_MISSING",
                Error,
                None,
                format!("seedConceptId \"{seed}\" does not reference an existing node."),
            ),
            Some(node) if node.node_type != "Concept" => out.add(
                "KG_INV_01_SEED_NOT_CONCEPT",
                Error,
                Some(seed),
                format!(
                    "seedConceptId \"{seed}\" references a node of type \"{}\", expected \"Concept\".",
                    node.node_type
                ),
            ),
            Some(_) => {}
        }
    }

    // Invariant 2: nodeTypes <-> nodes mutual consistency
    let mut indexed: HashSet<&str> = HashSet::new();
    for (type_name, ids) in graph.node_types.iter() {
        for id in ids {
            indexed.insert(id);
            match graph.nodes.get(id) {
                None => out.add(
                    "KG_INV_02_INDEX_GHOST",
                    Error,
                    Some(id),
                    format!("nodeTypes[\"{type_name}\"] references \"{id}\" but no such node exists in graph.nodes."),
                ),
                Some(node) if node.node_type != *type_name => out.add(
                    "KG_INV_02_INDEX_TYPE_MISMATCH",
                    Error,
                    Some(id),
                    format!("nodeTypes[\"{type_name}\"] indexes \"{id}\" but node.type is \"{}\".", node.node_type),
                ),
                Some(_) => {}
            }
        }
    }
    for (node_id, node) in graph.nodes.iter() {
        if !indexed.contains(node_id.as_str()) {
            out.add(
                "KG_INV_02_NODE_NOT_INDEXED",
                Warning,
                Some(node_id),
                format!(
                    "Node \"{node_id}\" (type \"{}\") exists but is not listed in nodeTypes.",
                    node.node_type
                ),
            );
        }
    }

    // Invariant 3: relationship endpoints exist; type is valid
    for rel in &graph.relationships {
        if non_empty(&rel.source_graph_id).is_none() && !graph.nodes.contains_key(&rel.source) {
            out.add(
                "KG_INV_03_REL_SRC_MISSING",
                Error,
                None,
                format!(
                    "Relationship \"{}\" source \"{}\" does not exist.",
                    rel.id, rel.source
                ),
            );
        }
        if non_empty(&rel.target_graph_id).is_none() && !graph.nodes.contains_key(&rel.target) {
            out.add(
                "KG_INV_03_REL_TGT_MISSING",
                Error,
                None,
                format!(
                    "Relationship \"{}\" target \"{}\" does not exist.",
                    rel.id, rel.target
                ),
            );
        }
        if !VALID_RELATIONSHIP_TYPES.contains(&rel.rel_type.as_str()) {
            out.add(
                "KG_INV_03_REL_UNKNOWN_TYPE",
                Error,
                None,
                format!(
                    "Relationship \"{}\" has unknown type \"{}\".",
                    rel.id, rel.rel_type
                ),
            );
        }
    }

    // Invariant 4: Concept->Layer hierarchy coherent
    for rel in &graph.relationships {
        if rel.rel_type == "containsConcept" {
            if let (Some(layer), Some(concept)) =
                (graph.nodes.get(&rel.source), graph.nodes.get(&rel.target))
            {
                if layer.node_type != "Layer" {
                    out.add(
                        "KG_INV_04_CONTAINS_CONCEPT_SRC",
                        Error,
                        Some(&rel.source),
                        format!("containsConcept source \"{}\" is not a Layer.", rel.source),
                    );
                }
                if concept.node_type != "Concept" {
                    out.add(
                        "KG_INV_04_CONTAINS_CONCEPT_TGT",
                        Error,
                        Some(&rel.target),
                        format!(
                            "containsConcept target \"{}\" is not a Concept.",
                            rel.target
                        ),
                    );
                }
            }
        }
    }

    // Invariant 5: hasLesson targets are Lesson nodes with non-empty content
    for rel in &graph.relationships {
        if rel.rel_type == "hasLesson" {
            if let Some(lesson) = graph.nodes.get(&rel.target) {
                if lesson.node_type != "Lesson" {
                    out.add(
                        "KG_INV_05_LESSON_TYPE",
                        Error,
                        Some(&rel.target),
                        format!(
                            "hasLesson target \"{}\" is type \"{}\", expected \"Lesson\".",
                            rel.target, lesson.node_type
                        ),
                    );
                } else if str(lesson.properties.get("content")).is_empty() {
                    out.add(
                        "KG_INV_05_LESSON_EMPTY",
                        Warning,
                        Some(&rel.target),
                        format!("Lesson \"{}\" has empty content.", rel.target),
                    );
                }
            }
        }
    }

    // Invariant 6: Layer precedence is linear and acyclic
    {
        let mut precedes: HashMap<&str, &str> = HashMap::new();
        for rel in &graph.relationships {
            if rel.rel_type == "precedesLayer" {
                if precedes.contains_key(rel.source.as_str()) {
                    out.add(
                        "KG_INV_06_LAYER_MULTI_PRECEDES",
                        Error,
                        Some(&rel.source),
                        format!("Layer \"{}\" has more than one precedesLayer relationship (non-linear).", rel.source),
                    );
                }
                precedes.insert(&rel.source, &rel.target);
            }
        }
        for start in graph.ids_of("Layer") {
            let mut visited: HashSet<&str> = HashSet::new();
            let mut cur: Option<&str> = Some(start.as_str());
            while let Some(c) = cur.filter(|c| !c.is_empty() && !visited.contains(c)) {
                visited.insert(c);
                cur = precedes.get(c).copied();
            }
            if let Some(c) = cur.filter(|c| !c.is_empty() && visited.contains(c)) {
                out.add(
                    "KG_INV_06_LAYER_CYCLE",
                    Error,
                    Some(c),
                    format!("Layer precedence graph contains a cycle involving \"{c}\"."),
                );
                break;
            }
        }
    }

    // Invariant 7: LearningGoal/Milestone wiring
    for goal in graph.ids_of("LearningGoal") {
        let has_milestone = graph
            .relationships
            .iter()
            .any(|r| r.source == *goal && r.rel_type == "hasMilestone");
        let linked = has_milestone
            || graph.relationships.iter().any(|r| {
                (r.source == *goal || r.target == *goal)
                    && (r.rel_type == "belongsToGoal" || r.rel_type == "hasLearningGoal")
            });
        if !linked && !graph.ids_of("Milestone").is_empty() {
            out.add(
                "KG_INV_07_GOAL_ISOLATED",
                Warning,
                Some(goal),
                format!("LearningGoal \"{goal}\" has no hasMilestone, hasLearningGoal, or belongsToGoal relationships."),
            );
        }
    }

    // Invariant 8: publishing consistency
    for id in graph.ids_of("Course") {
        let Some(node) = graph.nodes.get(id).filter(|n| n.node_type == "Course") else {
            continue;
        };
        let published = str(node.properties.get("publishStatus")) == "published";
        let empty_modules =
            matches!(node.properties.get("modules"), Some(Value::Array(m)) if m.is_empty());
        if published && empty_modules {
            out.add(
                "KG_INV_08_COURSE_EMPTY_MODULES",
                Warning,
                Some(id),
                format!("Course \"{id}\" publishStatus=published but has no modules."),
            );
        }
    }

    // Invariant 9: assessment completeness
    for id in graph.ids_of("Assessment") {
        let Some(node) = graph.nodes.get(id).filter(|n| n.node_type == "Assessment") else {
            continue;
        };
        let has_refs =
            matches!(node.properties.get("conceptRefs"), Some(Value::Array(r)) if !r.is_empty());
        if !has_refs {
            out.add(
                "KG_INV_09_ASSESSMENT_NO_CONCEPTS",
                Warning,
                Some(id),
                format!("Assessment \"{id}\" has no conceptRefs."),
            );
        }
    }

    // Invariant 10: translation pairing
    for id in graph.ids_of("Translation") {
        let Some(node) = graph.nodes.get(id).filter(|n| n.node_type == "Translation") else {
            continue;
        };
        let source = str(node.properties.get("sourceNodeId"));
        let source_exists = graph.nodes.contains_key(&source);
        if !source_exists {
            out.add(
                "KG_INV_10_TRANSLATION_SRC_MISSING",
                Error,
                Some(id),
                format!("Translation \"{id}\" references sourceNodeId \"{source}\" which does not exist."),
            );
        }
        let back_edge = graph
            .relationships
            .iter()
            .any(|r| r.source == *id && r.rel_type == "translationOf");
        if !back_edge && !source.is_empty() && source_exists {
            out.add(
                "KG_INV_10_TRANSLATION_NO_BACK_EDGE",
                Warning,
                Some(id),
                format!("Translation \"{id}\" has no translationOf back-edge to \"{source}\"."),
            );
        }
    }

    // Invariant 11: enrollment consistency
    for id in graph.ids_of("Enrollment") {
        let Some(node) = graph.nodes.get(id).filter(|n| n.node_type == "Enrollment") else {
            continue;
        };
        let course = str(node.properties.get("courseId"));
        if !course.is_empty() && !graph.nodes.contains_key(&course) {
            out.add(
                "KG_INV_11_ENROLLMENT_COURSE_MISSING",
                Warning,
                Some(id),
                format!(
                    "Enrollment \"{id}\" references courseId \"{course}\" but no such node exists."
                ),
            );
        }
    }

    // Invariant 12: updatedAt >= createdAt; version >= 1
    for (node_id, node) in graph.nodes.iter() {
        if let (Some(created), Some(updated)) = (
            node.created_at.as_ref().and_then(|n| n.as_f64()),
            node.updated_at.as_ref().and_then(|n| n.as_f64()),
        ) {
            if updated < created {
                out.add(
                    "KG_INV_12_TIMESTAMPS",
                    Warning,
                    Some(node_id),
                    format!(
                        "Node \"{node_id}\" has updatedAt ({}) < createdAt ({}).",
                        js_number(updated),
                        js_number(created)
                    ),
                );
            }
        }
    }
    if let Some(version) = graph.version.filter(|v| *v < 1.0) {
        out.add(
            "KG_INV_12_VERSION_NONPOSITIVE",
            Warning,
            None,
            format!(
                "Graph \"{}\" has version {} which is < 1.",
                graph.id,
                js_number(version)
            ),
        );
    }

    // Invariant 13: required properties per node type
    for (node_id, node) in graph.nodes.iter() {
        let Some((_, required)) = REQUIRED_PROPS.iter().find(|(t, _)| *t == node.node_type) else {
            continue;
        };
        for key in *required {
            if matches!(node.properties.get(*key), None | Some(Value::Null)) {
                out.add(
                    "KG_INV_13_MISSING_PROP",
                    Warning,
                    Some(node_id),
                    format!(
                        "Node \"{node_id}\" ({}) is missing required property \"{key}\".",
                        node.node_type
                    ),
                );
            }
        }
    }

    // Invariant 14: hasParent/hasPrerequisite graph is acyclic
    {
        let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
        for rel in &graph.relationships {
            if rel.rel_type == "hasParent" || rel.rel_type == "hasPrerequisite" {
                edges.entry(&rel.source).or_default().push(&rel.target);
            }
        }
        fn dfs<'a>(
            node: &'a str,
            edges: &HashMap<&'a str, Vec<&'a str>>,
            visited: &mut HashSet<&'a str>,
            in_stack: &mut HashSet<&'a str>,
        ) -> bool {
            if in_stack.contains(node) {
                return true;
            }
            if visited.contains(node) {
                return false;
            }
            visited.insert(node);
            in_stack.insert(node);
            for next in edges.get(node).map(Vec::as_slice).unwrap_or(&[]) {
                if dfs(next, edges, visited, in_stack) {
                    return true;
                }
            }
            in_stack.remove(node);
            false
        }
        let mut visited: HashSet<&str> = HashSet::new();
        let mut in_stack: HashSet<&str> = HashSet::new();
        for id in graph.nodes.keys() {
            if !visited.contains(id.as_str()) && dfs(id, &edges, &mut visited, &mut in_stack) {
                out.add(
                    "KG_INV_14_CYCLE",
                    Error,
                    None,
                    "The hasParent/hasPrerequisite graph contains a cycle.".to_string(),
                );
                break;
            }
        }
    }

    // Invariant 15: GraphMetadata counts match
    for id in graph.ids_of("GraphMetadata") {
        let Some(node) = graph
            .nodes
            .get(id)
            .filter(|n| n.node_type == "GraphMetadata")
        else {
            continue;
        };
        let concepts = graph.ids_of("Concept").len() as f64;
        if let Some(claimed) =
            opt_num(node.properties.get("totalConcepts")).filter(|c| *c != concepts)
        {
            out.add(
                "KG_INV_15_METADATA_CONCEPT_COUNT",
                Warning,
                Some(id),
                format!(
                    "GraphMetadata \"{id}\" claims totalConcepts={} but actual count is {}.",
                    js_number(claimed),
                    js_number(concepts)
                ),
            );
        }
        let rels = graph.relationships.len() as f64;
        if let Some(claimed) =
            opt_num(node.properties.get("totalRelationships")).filter(|c| *c != rels)
        {
            out.add(
                "KG_INV_15_METADATA_REL_COUNT",
                Warning,
                Some(id),
                format!(
                    "GraphMetadata \"{id}\" claims totalRelationships={} but actual count is {}.",
                    js_number(claimed),
                    js_number(rels)
                ),
            );
        }
    }

    out.0
}
