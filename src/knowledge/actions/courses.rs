//! The learner's courses: pointer records over their paths (`courses.ts`).

use serde_json::{json, Map, Value};

use super::action;
use super::common::{concept_of, load_graph, num_order, str_prop};
use super::params::{self, validate, Field, Ty};
use super::queries::assemble_course;
use crate::json::number;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use crate::knowledge::store::{KnowledgeError, StoreResult};
use crate::knowledge::ActionTable;

fn course_of<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    course_id: &str,
) -> StoreResult<&'a GraphNode> {
    match graph.nodes.get(course_id) {
        Some(n) if n.node_type == "Course" => Ok(n),
        _ => Err(KnowledgeError::not_found(format!(
            "Course {course_id} not found in path {}",
            graph.id
        ))),
    }
}

fn updated_at(node: &GraphNode) -> f64 {
    node.properties
        .get("updatedAt")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

pub fn to_knowledge_course(graph_id: &str, node: &GraphNode) -> Value {
    let modules: Vec<Value> = node
        .properties
        .get("modules")
        .and_then(Value::as_array)
        .map(|modules| {
            modules
                .iter()
                .map(|m| {
                    let refs: Vec<Value> = m
                        .get("conceptRefs")
                        .and_then(Value::as_array)
                        .map(|refs| {
                            refs.iter()
                                .map(|r| {
                                    json!({
                                        "nodeId": r.get("nodeId").cloned().unwrap_or(Value::Null),
                                        "sourceGraphId": r.get("sourceGraphId").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(graph_id)),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({ "title": m.get("title").cloned().unwrap_or(Value::Null), "conceptRefs": refs })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": node.id,
        "graphId": graph_id,
        "title": node.properties.get("title").cloned().unwrap_or(Value::Null),
        "description": node.properties.get("description").cloned().unwrap_or(Value::Null),
        "modules": modules,
        "publishStatus": node.properties.get("publishStatus").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("draft")),
        "updatedAt": node.properties.get("updatedAt").cloned().unwrap_or(Value::Null),
    })
}

fn courses_of(graph: &NodeBasedKnowledgeGraph) -> Vec<&GraphNode> {
    let mut courses: Vec<&GraphNode> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Course")
        .collect();
    courses.sort_by(|a, b| num_order(updated_at(b), updated_at(a)));
    courses
}

fn lesson_text_of(graph: &NodeBasedKnowledgeGraph, concept_id: &str) -> String {
    graph
        .relationships
        .iter()
        .find(|r| {
            r.source == concept_id
                && r.rel_type == "hasLesson"
                && graph
                    .nodes
                    .get(&r.target)
                    .is_some_and(|n| n.node_type == "Lesson")
        })
        .and_then(|r| graph.nodes.get(&r.target))
        .map(|lesson| str_prop(lesson, "content"))
        .unwrap_or_default()
}

pub fn package_of(graph: &NodeBasedKnowledgeGraph, course: &GraphNode) -> StoreResult<Value> {
    let mut modules = Vec::new();
    for m in course
        .properties
        .get("modules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let mut concepts = Vec::new();
        for r in m
            .get("conceptRefs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let node_id = r.get("nodeId").and_then(Value::as_str).unwrap_or_default();
            let concept = concept_of(graph, node_id)?;
            concepts.push(json!({
                "name": concept.properties.get("name").cloned().unwrap_or(Value::Null),
                "description": concept.properties.get("description").cloned().unwrap_or(Value::Null),
                "lesson": lesson_text_of(graph, &concept.id),
                "layer": concept.properties.get("layer").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(0)),
            }));
        }
        modules.push(json!({ "title": m.get("title").cloned().unwrap_or(Value::Null), "concepts": concepts }));
    }
    Ok(json!({
        "title": course.properties.get("title").cloned().unwrap_or(Value::Null),
        "description": course.properties.get("description").cloned().unwrap_or(Value::Null),
        "modules": modules,
    }))
}

fn graph_and_course() -> Vec<Field> {
    vec![params::id("graphId"), params::id("courseId")]
}

pub fn register(table: &mut ActionTable) {
    table.insert(
        "listCourses",
        action(|call, p, deps| {
            validate(p, &[params::opt_id("graphId")])?;
            let ids = match params::opt_str_of(p, "graphId") {
                Some(id) => vec![id.to_string()],
                None => deps.graphs.list_graph_ids(call.uid)?,
            };
            let mut graphs = Vec::new();
            for id in &ids {
                graphs.push(load_graph(deps, call.uid, id)?);
            }
            let mut courses: Vec<(f64, Value)> = graphs
                .iter()
                .flat_map(|g| {
                    courses_of(g)
                        .into_iter()
                        .map(|c| (updated_at(c), to_knowledge_course(&g.id, c)))
                })
                .collect();
            courses.sort_by(|a, b| num_order(b.0, a.0));
            Ok(json!({ "courses": courses.into_iter().map(|(_, c)| c).collect::<Vec<_>>() }))
        }),
    );

    table.insert(
        "courseDetail",
        action(|call, p, deps| {
            validate(p, &graph_and_course())?;
            let graph_id = params::str_of(p, "graphId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let course = course_of(&graph, params::str_of(p, "courseId"))?;
            Ok(json!({ "course": to_knowledge_course(graph_id, course) }))
        }),
    );

    table.insert(
        "createCourse",
        action(|call, p, deps| {
            validate(p, &[params::id("graphId"), params::id("title"), params::opt_text("description")])?;
            let graph_id = params::str_of(p, "graphId");
            let title = params::str_of(p, "title");
            let graph = load_graph(deps, call.uid, graph_id)?;
            if graph.nodes.contains_key(&format!("course-{title}")) {
                return Err(KnowledgeError::conflict(format!("Path {graph_id} already has a course \"{title}\"")));
            }
            let assembled = assemble_course(&graph, title, params::opt_str_of(p, "description"), deps.graphs.now_ms());
            deps.graphs.create_node(call.uid, graph_id, assembled.node.clone())?;
            deps.graphs.create_relationship(call.uid, graph_id, assembled.relationship.clone())?;
            let saved = deps.graphs.get_graph(call.uid, graph_id)?;
            Ok(json!({ "course": to_knowledge_course(graph_id, course_of(&saved, &assembled.id)?) }))
        }),
    );

    table.insert(
        "updateCourse",
        action(|call, p, deps| {
            let module_item = Ty::Object(vec![params::id("title"), params::list("conceptIds", params::id_ty(), false)]);
            let mut fields = graph_and_course();
            fields.extend([
                params::opt_id("title"),
                params::opt_text("description"),
                params::opt_list("modules", module_item, false),
                params::opt_pick("publishStatus", &["draft", "published"]),
            ]);
            validate(p, &fields)?;
            let graph_id = params::str_of(p, "graphId");
            let course_id = params::str_of(p, "courseId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let course = course_of(&graph, course_id)?;
            let modules = p.get("modules").and_then(Value::as_array);
            for m in modules.into_iter().flatten() {
                for id in m.get("conceptIds").and_then(Value::as_array).into_iter().flatten() {
                    concept_of(&graph, id.as_str().unwrap_or_default())?;
                }
            }
            let mut properties = course.properties.clone();
            for key in ["title", "description", "publishStatus"] {
                if let Some(v) = p.get(key) {
                    properties.insert(key.to_string(), v.clone());
                }
            }
            if let Some(modules) = modules {
                let mapped: Vec<Value> = modules
                    .iter()
                    .map(|m| {
                        let refs: Vec<Value> = m
                            .get("conceptIds")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .map(|id| json!({ "nodeId": id, "sourceGraphId": graph_id }))
                            .collect();
                        json!({ "title": m.get("title").cloned().unwrap_or(Value::Null), "conceptRefs": refs })
                    })
                    .collect();
                properties.insert("modules".into(), Value::Array(mapped));
            }
            properties.insert("updatedAt".into(), number(deps.graphs.now_ms()));
            let mut updates = Map::new();
            updates.insert("properties".into(), Value::Object(properties));
            deps.graphs.update_node(call.uid, graph_id, course_id, &updates)?;
            let saved = deps.graphs.get_graph(call.uid, graph_id)?;
            Ok(json!({ "course": to_knowledge_course(graph_id, course_of(&saved, course_id)?) }))
        }),
    );

    table.insert(
        "deleteCourse",
        action(|call, p, deps| {
            validate(p, &graph_and_course())?;
            let graph_id = params::str_of(p, "graphId");
            let course_id = params::str_of(p, "courseId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            course_of(&graph, course_id)?;
            deps.graphs.delete_node(call.uid, graph_id, course_id)?;
            Ok(json!({ "deleted": true }))
        }),
    );

    table.insert(
        "coursePackage",
        action(|call, p, deps| {
            validate(p, &graph_and_course())?;
            let graph = load_graph(deps, call.uid, params::str_of(p, "graphId"))?;
            let course = course_of(&graph, params::str_of(p, "courseId"))?;
            Ok(json!({ "package": package_of(&graph, course)? }))
        }),
    );
}
