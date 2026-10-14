//! Proposals that complete a path, and writing the approved ones (`enrich.ts`).

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::common::{load_graph, make_node, num_order};
use super::enrichment::{
    cross_layer_request, edge_key, goal_aware_request, missing_concepts_request, parse_cross_layer,
    parse_goal_aware, parse_missing_concepts, parse_prerequisites, prerequisites_request,
    reply_shape, request_json, Draft, EnrichConcept, EnrichEdge, EnrichGoal, EnrichLayer,
    GraphIndex, ModelRequest, Resolved,
};
use super::model::{check_shape, schema as z};
use super::params::{self, validate};
use super::queries::extract_learning_path_summary;
use super::{action, Env};
use crate::json::number;
use crate::knowledge::engine::pure::NodeBasedKnowledgeGraph;
use crate::knowledge::firestore::forward_relationship;
use crate::knowledge::store::{ErrorKind, KnowledgeDeps, KnowledgeError, StoreResult};
use crate::knowledge::{ActionTable, KnowledgeCall, KnowledgeProgress};

const MODEL: &str = "deepseek-v4-flash";
const ALL_STRATEGIES: [&str; 4] = [
    "missingConcepts",
    "prerequisites",
    "crossLayer",
    "goalAware",
];

fn concept_nodes(
    graph: &NodeBasedKnowledgeGraph,
) -> Vec<&crate::knowledge::engine::pure::GraphNode> {
    graph
        .ids_of("Concept")
        .iter()
        .filter_map(|id| graph.nodes.get(id))
        .filter(|n| n.node_type == "Concept")
        .collect()
}

fn edge_of(rel: &crate::knowledge::engine::pure::Relationship) -> Option<EnrichEdge> {
    matches!(
        rel.rel_type.as_str(),
        "hasPrerequisite" | "hasParent" | "referencesConcept"
    )
    .then(|| EnrichEdge {
        from: rel.source.clone(),
        to: rel.target.clone(),
        edge_type: rel.rel_type.clone(),
    })
}

fn goal_of(graph: &NodeBasedKnowledgeGraph) -> EnrichGoal {
    let goal = graph
        .ids_of("LearningGoal")
        .first()
        .and_then(|id| graph.nodes.get(id))
        .filter(|n| n.node_type == "LearningGoal");
    match goal {
        Some(node) => {
            let text = |k: &str| {
                node.properties
                    .get(k)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            EnrichGoal {
                title: text("name").unwrap_or_default(),
                description: text("description").unwrap_or_default(),
                goal_type: text("type"),
                target: text("target"),
                assessed_level: text("assessedLevel"),
            }
        }
        None => EnrichGoal {
            title: extract_learning_path_summary(graph).title,
            description: String::new(),
            goal_type: None,
            target: None,
            assessed_level: None,
        },
    }
}

struct PathView {
    goal: EnrichGoal,
    concepts: Vec<EnrichConcept>,
    edges: Vec<EnrichEdge>,
    layers: Vec<EnrichLayer>,
    index: GraphIndex,
}

fn path_view_of(graph: &NodeBasedKnowledgeGraph) -> PathView {
    let concepts: Vec<EnrichConcept> = concept_nodes(graph)
        .iter()
        .map(|c| EnrichConcept {
            id: c.id.clone(),
            name: c
                .properties
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            description: c
                .properties
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            layer: c
                .properties
                .get("layer")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        })
        .collect();
    let edges: Vec<EnrichEdge> = graph.relationships.iter().filter_map(edge_of).collect();
    let layer_nodes: Vec<&crate::knowledge::engine::pure::GraphNode> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Layer")
        .collect();
    let mut numbers: Vec<f64> = Vec::new();
    for n in concepts.iter().map(|c| c.layer).chain(
        layer_nodes
            .iter()
            .filter_map(|l| l.properties.get("layerNumber").and_then(Value::as_f64)),
    ) {
        if !numbers.contains(&n) {
            numbers.push(n);
        }
    }
    numbers.sort_by(|a, b| num_order(*a, *b));
    let layers: Vec<EnrichLayer> = numbers
        .iter()
        .map(|&layer_number| {
            let node = layer_nodes.iter().find(|l| {
                l.properties.get("layerNumber").and_then(Value::as_f64) == Some(layer_number)
            });
            EnrichLayer {
                layer_number,
                goal: node.and_then(|n| {
                    n.properties
                        .get("goal")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
                milestone: node.and_then(|n| {
                    n.properties
                        .get("targetMilestoneTitle")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
                concepts: concepts
                    .iter()
                    .filter(|c| c.layer == layer_number)
                    .cloned()
                    .collect(),
            }
        })
        .collect();
    PathView {
        goal: goal_of(graph),
        index: GraphIndex {
            concept_ids: concepts.iter().map(|c| c.id.clone()).collect(),
            edge_keys: edges
                .iter()
                .map(|e| edge_key(&e.edge_type, &e.from, &e.to))
                .collect(),
            layer_numbers: numbers,
        },
        concepts,
        edges,
        layers,
    }
}

struct Job {
    request: ModelRequest,
    resolve: Box<dyn Fn(&Value) -> Resolved>,
}

fn jobs_for(strategy: &str, view: &PathView, layer_number: Option<f64>) -> Vec<Job> {
    let scoped: Vec<&EnrichLayer> = view
        .layers
        .iter()
        .filter(|l| layer_number.is_none_or(|n| l.layer_number == n))
        .collect();
    let index = |view: &PathView| GraphIndex {
        concept_ids: view.index.concept_ids.clone(),
        edge_keys: view.index.edge_keys.clone(),
        layer_numbers: view.index.layer_numbers.clone(),
    };
    match strategy {
        "missingConcepts" => scoped
            .into_iter()
            .map(|layer| {
                let (idx, layer_owned) = (index(view), layer.clone());
                Job {
                    request: missing_concepts_request(&view.goal, layer),
                    resolve: Box::new(move |reply| {
                        parse_missing_concepts(reply, &idx, &layer_owned)
                    }),
                }
            })
            .collect(),
        "prerequisites" => view
            .concepts
            .iter()
            .filter(|c| layer_number.is_none_or(|n| c.layer == n))
            .map(|concept| {
                let others: Vec<EnrichConcept> = view
                    .concepts
                    .iter()
                    .filter(|c| c.id != concept.id)
                    .cloned()
                    .collect();
                let (idx, concept_owned) = (index(view), concept.clone());
                Job {
                    request: prerequisites_request(&view.goal, concept, &others, &view.edges),
                    resolve: Box::new(move |reply| {
                        parse_prerequisites(reply, &idx, &concept_owned)
                    }),
                }
            })
            .collect(),
        "crossLayer" => view
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, upper)| {
                let lower = if i == 0 { None } else { view.layers.get(i - 1) }?;
                if layer_number.is_some_and(|n| upper.layer_number != n) {
                    return None;
                }
                let (idx, upper_owned) = (index(view), upper.clone());
                Some(Job {
                    request: cross_layer_request(&view.goal, lower, upper),
                    resolve: Box::new(move |reply| parse_cross_layer(reply, &idx, &upper_owned)),
                })
            })
            .collect(),
        _ => {
            let concepts: Vec<EnrichConcept> = scoped
                .iter()
                .flat_map(|l| l.concepts.iter().cloned())
                .collect();
            let ids: HashSet<&str> = concepts.iter().map(|c| c.id.as_str()).collect();
            let edges: Vec<EnrichEdge> = view
                .edges
                .iter()
                .filter(|e| ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()))
                .cloned()
                .collect();
            let levels: Vec<f64> = scoped.iter().map(|l| l.layer_number).collect();
            let idx = index(view);
            vec![Job {
                request: goal_aware_request(&view.goal, &concepts, &edges),
                resolve: Box::new(move |reply| parse_goal_aware(reply, &idx, &levels)),
            }]
        }
    }
}

fn draft_edge_key(draft: &Draft) -> Option<String> {
    match draft {
        Draft::Prerequisite { from, to, .. } => Some(edge_key("hasPrerequisite", from, to)),
        Draft::Relation {
            from, to, relation, ..
        } => Some(edge_key(relation, from, to)),
        Draft::Concept { .. } => None,
    }
}

fn via_model<T>(what: &str, call: impl FnOnce() -> StoreResult<T>) -> StoreResult<T> {
    call().map_err(|e| {
        if e.kind == ErrorKind::Other {
            KnowledgeError::service(format!("{what} failed: {}", e.message))
        } else {
            e
        }
    })
}

fn item_json(draft: &Draft, reference: String) -> Value {
    match draft {
        Draft::Concept {
            layer,
            name,
            description,
            reason,
        } => {
            json!({ "kind": "concept", "layer": number(*layer), "name": name, "description": description, "reason": reason, "ref": reference })
        }
        Draft::Prerequisite { from, to, reason } => {
            json!({ "kind": "prerequisite", "from": from, "to": to, "reason": reason, "ref": reference })
        }
        Draft::Relation {
            from,
            to,
            relation,
            reason,
        } => {
            json!({ "kind": "relation", "from": from, "to": to, "relation": relation, "reason": reason, "ref": reference })
        }
    }
}

fn run_preview(
    call: &KnowledgeCall<'_>,
    deps: &KnowledgeDeps,
    graph: &NodeBasedKnowledgeGraph,
    strategies: &[String],
    layer_number: Option<f64>,
) -> StoreResult<Vec<Value>> {
    let view = path_view_of(graph);
    if let Some(n) = layer_number {
        if !view.index.layer_numbers.contains(&n) {
            return Err(KnowledgeError::not_found(format!(
                "Path {} has no level {}",
                graph.id,
                crate::jsnumber::js_number(n)
            )));
        }
    }
    call.progress(KnowledgeProgress::new("start", "", MODEL));
    let mut items: Vec<Value> = Vec::new();
    let mut seen_concepts: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<String> = HashSet::new();
    let mut done: Vec<&str> = Vec::new();
    for strategy in strategies {
        if done.contains(&strategy.as_str()) {
            continue;
        }
        done.push(strategy);
        let jobs = jobs_for(strategy, &view, layer_number);
        call.progress(KnowledgeProgress::new(
            "step",
            format!(
                "{strategy}: {} {}",
                jobs.len(),
                if jobs.len() == 1 {
                    "request"
                } else {
                    "requests"
                }
            ),
            MODEL,
        ));
        let mut n = 0;
        for job in jobs {
            let raw = via_model(&format!("Enrichment ({strategy})"), || {
                deps.models
                    .generate_structured(&request_json(&job.request, MODEL))
            })?;
            call.throw_if_cancelled()?;
            let reply = check_shape(&raw, &reply_shape()).map_err(|issues| {
                KnowledgeError::service(format!(
                    "The model's {strategy} reply is invalid: {}",
                    issues.join("; ")
                ))
            })?;
            let resolved = (job.resolve)(&reply);
            for draft in resolved.drafts {
                let edge = draft_edge_key(&draft);
                let concept_id = if let Draft::Concept { name, .. } = &draft {
                    Some(name.clone())
                } else {
                    None
                };
                let duplicate = match (&concept_id, &edge) {
                    (Some(id), _) => seen_concepts.contains(id),
                    (None, Some(key)) => seen_edges.contains(key),
                    (None, None) => false,
                };
                if duplicate {
                    continue;
                }
                if let Some(id) = concept_id {
                    seen_concepts.insert(id);
                }
                if let Some(key) = edge {
                    seen_edges.insert(key);
                }
                n += 1;
                items.push(item_json(&draft, format!("{strategy}-{n}")));
            }
        }
    }
    Ok(items)
}

fn apply_items(
    call: &KnowledgeCall<'_>,
    graph: &NodeBasedKnowledgeGraph,
    items: &[Value],
    deps: &KnowledgeDeps,
) -> StoreResult<(usize, Vec<Value>)> {
    let view = path_view_of(graph);
    let mut ids = view.index.concept_ids.clone();
    let mut edge_keys = view.index.edge_keys.clone();
    let mut per_layer: HashMap<String, usize> = HashMap::new();
    for c in &view.concepts {
        *per_layer
            .entry(crate::jsnumber::js_number(c.layer))
            .or_insert(0) += 1;
    }
    let mut failed: Vec<Value> = Vec::new();
    let mut applied = 0usize;
    let kind = |i: &Value| {
        i.get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let ordered: Vec<&Value> = items
        .iter()
        .filter(|i| kind(i) == "concept")
        .chain(items.iter().filter(|i| kind(i) != "concept"))
        .collect();
    for item in ordered {
        call.throw_if_cancelled()?;
        let text = |k: &str| {
            item.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let reference = text("ref");
        let outcome: StoreResult<Option<String>> = (|| {
            if kind(item) == "concept" {
                let name = text("name");
                if ids.contains(&name) {
                    return Ok(Some(format!(
                        "Path {} already has a concept \"{name}\"",
                        graph.id
                    )));
                }
                let layer = item.get("layer").and_then(Value::as_f64).unwrap_or(0.0);
                let layer_key = crate::jsnumber::js_number(layer);
                let sequence = per_layer.get(&layer_key).copied().unwrap_or(0) + 1;
                let now = deps.graphs.now_ms();
                deps.graphs.create_node(
                    call.uid,
                    &graph.id,
                    make_node(
                        &name,
                        "Concept",
                        json!({ "id": name, "name": name, "description": text("description"), "layer": number(layer), "sequence": sequence, "isAutoGenerated": true }),
                        now,
                    ),
                )?;
                deps.graphs.create_relationship(
                    call.uid,
                    &graph.id,
                    forward_relationship(&graph.id, &name, "containsConcept", deps.graphs.now_ms()),
                )?;
                ids.insert(name);
                per_layer.insert(layer_key, sequence);
                applied += 1;
                return Ok(None);
            }
            let rel_type = if kind(item) == "prerequisite" {
                "hasPrerequisite".to_string()
            } else {
                text("relation")
            };
            let (from, to) = (text("from"), text("to"));
            let key = edge_key(&rel_type, &from, &to);
            let missing: Vec<String> = [&from, &to]
                .into_iter()
                .filter(|id| !ids.contains(*id))
                .map(|id| format!("\"{id}\""))
                .collect();
            if !missing.is_empty() {
                return Ok(Some(format!(
                    "Path {} has no concept {}",
                    graph.id,
                    missing.join(" or ")
                )));
            }
            if from == to {
                return Ok(Some(format!("\"{from}\" cannot relate to itself")));
            }
            if edge_keys.contains(&key) {
                return Ok(Some(format!(
                    "Path {} already has {rel_type} from \"{from}\" to \"{to}\"",
                    graph.id
                )));
            }
            deps.graphs.create_relationship(
                call.uid,
                &graph.id,
                forward_relationship(&from, &to, &rel_type, deps.graphs.now_ms()),
            )?;
            edge_keys.insert(key);
            applied += 1;
            Ok(None)
        })();
        match outcome {
            Ok(None) => {}
            Ok(Some(reason)) => failed.push(json!({ "ref": reference, "reason": reason })),
            Err(e) if e.kind == ErrorKind::Other => {
                failed.push(json!({ "ref": reference, "reason": e.message }))
            }
            Err(e) => return Err(e),
        }
    }
    Ok((applied, failed))
}

fn item_shape() -> Value {
    z::union(
        "kind",
        vec![
            (
                "concept",
                z::object(
                    vec![
                        ("ref", z::string(true, false)),
                        ("kind", z::string(false, false)),
                        ("layer", z::integer()),
                        ("name", z::string(true, false)),
                        ("description", z::string(false, false)),
                        ("reason", z::string(false, false)),
                    ],
                    &[],
                ),
            ),
            (
                "prerequisite",
                z::object(
                    vec![
                        ("ref", z::string(true, false)),
                        ("kind", z::string(false, false)),
                        ("from", z::string(true, false)),
                        ("to", z::string(true, false)),
                        ("reason", z::string(false, false)),
                    ],
                    &[],
                ),
            ),
            (
                "relation",
                z::object(
                    vec![
                        ("ref", z::string(true, false)),
                        ("kind", z::string(false, false)),
                        ("from", z::string(true, false)),
                        ("to", z::string(true, false)),
                        ("relation", z::one_of(&["hasParent", "referencesConcept"])),
                        ("reason", z::string(false, false)),
                    ],
                    &[],
                ),
            ),
        ],
    )
}

pub fn register(table: &mut ActionTable, _env: &Env) {
    table.insert(
        "enrichPreview",
        action(|call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::opt_int("layerNumber", None, None),
                    params::opt_list(
                        "strategies",
                        super::params::Ty::Enum(&ALL_STRATEGIES),
                        false,
                    ),
                ],
            )?;
            let graph = load_graph(deps, call.uid, params::str_of(p, "graphId"))?;
            let strategies: Vec<String> = match p.get("strategies").and_then(Value::as_array) {
                Some(list) => list
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect(),
                None => ALL_STRATEGIES.iter().map(|s| (*s).to_string()).collect(),
            };
            let items = run_preview(
                call,
                deps,
                &graph,
                &strategies,
                params::opt_f64_of(p, "layerNumber"),
            )?;
            Ok(json!({ "items": items }))
        }),
    );

    table.insert(
        "enrichApply",
        action(|call, p, deps| {
            let shape = z::object(
                vec![
                    ("graphId", z::string(true, false)),
                    ("items", z::array(item_shape(), None, None)),
                ],
                &[],
            );
            let checked = check_shape(&Value::Object(p.clone()), &shape)
                .map_err(|issues| KnowledgeError::validation(issues.join("; ")))?;
            let graph_id = checked
                .get("graphId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let items = checked
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let graph = load_graph(deps, call.uid, &graph_id)?;
            let (applied, failed) = apply_items(call, &graph, &items, deps)?;
            Ok(json!({ "applied": applied, "failed": failed }))
        }),
    );
}
