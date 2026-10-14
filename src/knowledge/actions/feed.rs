//! The Rawi feed (`feed.ts`): ranked cards over every path the learner owns, kept card content, and the
//! signals a learner gives a card. The two model calls (a card, a batch of explore concepts) go through `FeedSources`.

use serde_json::{json, Map, Value};

use super::common::{concept_of, load_graph, make_node, mastery_of, to_route_key};
use super::feed_ranking::{
    decode_cursor, rank_feed, ExploreCandidate, FeedConcept, FeedMix, DEFAULT_FEED_LIMIT,
    DEFAULT_FEED_MIX,
};
use super::params::{self, validate};
use super::queries::extract_learning_path_summary;
use super::{action, Env};
use crate::json::number;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use crate::knowledge::firestore::forward_relationship;
use crate::knowledge::store::{ErrorKind, KnowledgeDeps, KnowledgeError, StoreResult};
use crate::knowledge::{ActionTable, KnowledgeCall, KnowledgeProgress};

/// How many concepts the explore bucket proposes next to a path at a time.
pub const EXPLORE_BATCH: usize = 5;

pub struct CardRequest {
    pub name: String,
    pub description: String,
    pub path_title: String,
}

pub struct ExploreRequest {
    pub path_title: String,
    pub goal: String,
    pub level_number: f64,
    /// Every concept name already in the path; a proposal must not repeat one.
    pub existing: Vec<String>,
}

/// The model calls the feed makes (`FeedSources`).
pub trait FeedSources: Send + Sync {
    /// The model's card, as JSON; the action validates it.
    fn generate_card(&self, request: &CardRequest) -> StoreResult<Value>;
    /// The model's explore concepts, each `{ name, description }`.
    fn propose_explore(&self, request: &ExploreRequest) -> StoreResult<Vec<Value>>;
    fn model(&self) -> String;
}

pub fn card_prompt(request: &CardRequest) -> (String, String) {
    (
        "You write one short-form learning card, the kind a learner swipes through on a phone. \
Reply ONLY with JSON {\"hook\": string, \"beat\": string, \"check\": null | {\"kind\":\"reveal\",\"prompt\":string,\"answer\":string} | {\"kind\":\"trueFalse\",\"statement\":string,\"correct\":boolean,\"explanation\":string}}. \
hook: one line (under 90 characters) that makes the reader stop scrolling — a surprising fact, a question or a stake. \
beat: 2 to 4 plain sentences that teach the core idea accurately. \
check: a question answerable in five seconds from the beat (a true/false statement or a prompt with a one-line answer), or null if none fits. \
Never invent facts; write in the same language as the concept."
            .to_string(),
        format!("Path: {}\nConcept: {}\nDescription: {}", request.path_title, request.name, request.description),
    )
}

pub fn explore_prompt(request: &ExploreRequest) -> (String, String) {
    let mut user = vec![
        format!("Path: {}", request.path_title),
        format!("Goal: {}", request.goal),
        format!(
            "Level: {}",
            crate::jsnumber::js_number(request.level_number)
        ),
        "Already in the path:".to_string(),
    ];
    user.extend(request.existing.iter().map(|n| format!("- {n}")));
    (
        format!(
            "You suggest {EXPLORE_BATCH} concepts a learner could explore next to a learning path: closely related ideas that are NOT already in it. \
Reply ONLY with JSON {{\"concepts\":[{{\"name\": string, \"description\": string}}]}}. Names are short concept names; descriptions are one sentence. \
Never repeat an existing concept, and write in the same language as the path."
        ),
        user.join("\n"),
    )
}

/// Run a model call; a refusal of the action's own passes through, any other failure becomes `SERVICE_ERROR`.
fn via_model<T>(what: &str, call: impl FnOnce() -> StoreResult<T>) -> StoreResult<T> {
    call().map_err(|e| {
        if e.kind == ErrorKind::Other {
            KnowledgeError::service(format!("{what} failed: {}", e.message))
        } else {
            e
        }
    })
}

/// The card shape the model must return (`cardSchema`).
fn card_shape() -> Value {
    use super::model::schema as z;
    z::object(
        vec![
            ("hook", z::string(true, false)),
            ("beat", z::string(true, false)),
            (
                "check",
                z::nullable(z::union(
                    "kind",
                    vec![
                        (
                            "reveal",
                            z::object(
                                vec![
                                    ("kind", z::string(false, false)),
                                    ("prompt", z::string(true, false)),
                                    ("answer", z::string(true, false)),
                                ],
                                &[],
                            ),
                        ),
                        (
                            "trueFalse",
                            z::object(
                                vec![
                                    ("kind", z::string(false, false)),
                                    ("statement", z::string(true, false)),
                                    ("correct", z::boolean()),
                                    ("explanation", z::string(true, false)),
                                ],
                                &[],
                            ),
                        ),
                    ],
                )),
            ),
        ],
        &[],
    )
}

fn explore_shape() -> Value {
    use super::model::schema as z;
    z::object(
        vec![(
            "concepts",
            z::array(
                z::object(
                    vec![
                        ("name", z::string(true, false)),
                        ("description", z::string(true, false)),
                    ],
                    &[],
                ),
                None,
                None,
            ),
        )],
        &[],
    )
}

const FEED_MODEL: &str = "deepseek-v4-flash";

/// The production feed sources: both calls are structured requests on the model port.
pub struct ModelFeedSources {
    pub models: std::sync::Arc<dyn crate::knowledge::store::ModelPort>,
}

impl FeedSources for ModelFeedSources {
    fn generate_card(&self, request: &CardRequest) -> StoreResult<Value> {
        let (system, user) = card_prompt(request);
        self.models.generate_structured(&json!({
            "systemPrompt": system, "userPrompt": user, "provider": "deepseek", "model": FEED_MODEL,
            "temperature": 0.6, "schema": card_shape(),
        }))
    }

    fn propose_explore(&self, request: &ExploreRequest) -> StoreResult<Vec<Value>> {
        let (system, user) = explore_prompt(request);
        let out = self.models.generate_structured(&json!({
            "systemPrompt": system, "userPrompt": user, "provider": "deepseek", "model": FEED_MODEL,
            "temperature": 0.7, "schema": explore_shape(),
        }))?;
        Ok(out
            .get("concepts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn model(&self) -> String {
        FEED_MODEL.to_string()
    }
}

fn nonblank(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

/// `cardSchema.safeParse`: the card the model returned, or the issues zod would list.
fn check_card(card: &Value) -> Result<Value, String> {
    let mut issues = Vec::new();
    let Some(map) = card.as_object() else {
        return Err(": Expected object, received other".to_string());
    };
    for key in ["hook", "beat"] {
        match map.get(key) {
            None => issues.push(format!("{key}: Required")),
            Some(Value::String(s)) if !s.is_empty() => {}
            Some(Value::String(_)) => issues.push(format!(
                "{key}: String must contain at least 1 character(s)"
            )),
            Some(_) => issues.push(format!("{key}: Expected string")),
        }
    }
    match map.get("check") {
        None => issues.push("check: Required".to_string()),
        Some(Value::Null) => {}
        Some(Value::Object(check)) => match check.get("kind").and_then(Value::as_str) {
            Some("reveal") => {
                for key in ["prompt", "answer"] {
                    if !nonblank(check.get(key)) {
                        issues.push(format!(
                            "check.{key}: String must contain at least 1 character(s)"
                        ));
                    }
                }
            }
            Some("trueFalse") => {
                for key in ["statement", "explanation"] {
                    if !nonblank(check.get(key)) {
                        issues.push(format!(
                            "check.{key}: String must contain at least 1 character(s)"
                        ));
                    }
                }
                if !check.get("correct").is_some_and(Value::is_boolean) {
                    issues.push("check.correct: Expected boolean".to_string());
                }
            }
            _ => issues.push(
                "check.kind: Invalid discriminator value. Expected 'reveal' | 'trueFalse'"
                    .to_string(),
            ),
        },
        Some(_) => issues.push("check: Expected object".to_string()),
    }
    if issues.is_empty() {
        Ok(card.clone())
    } else {
        Err(issues.join("; "))
    }
}

fn concept_nodes(graph: &NodeBasedKnowledgeGraph) -> Vec<&GraphNode> {
    graph
        .ids_of("Concept")
        .iter()
        .filter_map(|id| graph.nodes.get(id))
        .filter(|n| n.node_type == "Concept")
        .collect()
}

fn layer_nodes(graph: &NodeBasedKnowledgeGraph) -> Vec<&GraphNode> {
    graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Layer")
        .collect()
}

fn last_studied_of(graph: &NodeBasedKnowledgeGraph, uid: &str, concept_id: &str) -> Option<f64> {
    graph
        .nodes
        .get(&format!("progress-{uid}-{concept_id}"))
        .filter(|n| n.node_type == "Progress")
        .and_then(|n| n.properties.get("lastStudied"))
        .and_then(Value::as_f64)
}

/// Every concept of a path, as ranking reads it. Prerequisite edges run from the concept to what it requires.
pub fn feed_concepts_of(graph: &NodeBasedKnowledgeGraph, uid: &str) -> Vec<FeedConcept> {
    let path_title = extract_learning_path_summary(graph).title;
    let concepts = concept_nodes(graph);
    let ids: std::collections::HashSet<&str> = concepts.iter().map(|c| c.id.as_str()).collect();
    let mut requires: std::collections::HashMap<&str, Vec<String>> = Default::default();
    let mut neighbours: std::collections::HashMap<&str, Vec<String>> = Default::default();
    let mut link = |a: &str, b: &str| {
        if !ids.contains(a) || !ids.contains(b) {
            return;
        }
        let ab = neighbours
            .entry(ids.get(a).copied().unwrap_or_default())
            .or_default();
        if !ab.iter().any(|x| x == b) {
            ab.push(b.to_string());
        }
        let ba = neighbours
            .entry(ids.get(b).copied().unwrap_or_default())
            .or_default();
        if !ba.iter().any(|x| x == a) {
            ba.push(a.to_string());
        }
    };
    for rel in &graph.relationships {
        if rel.rel_type == "hasPrerequisite"
            && ids.contains(rel.source.as_str())
            && ids.contains(rel.target.as_str())
        {
            requires
                .entry(ids.get(rel.source.as_str()).copied().unwrap_or_default())
                .or_default()
                .push(rel.target.clone());
            link(&rel.source, &rel.target);
        }
        if rel.rel_type == "hasParent" {
            link(&rel.source, &rel.target);
        }
    }
    concepts
        .iter()
        .map(|c| {
            let feed = c.properties.get("feed").filter(|v| !v.is_null());
            let signal = |k: &str| feed.and_then(|f| f.get(k));
            FeedConcept {
                graph_id: graph.id.clone(),
                path_title: path_title.clone(),
                concept_id: c.id.clone(),
                route_key: to_route_key(&c.id),
                name: c
                    .properties
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                layer: c
                    .properties
                    .get("layer")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                sequence: c
                    .properties
                    .get("sequence")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                mastery: mastery_of(graph, uid, &c.id),
                last_studied: last_studied_of(graph, uid, &c.id),
                prerequisites: requires.get(c.id.as_str()).cloned().unwrap_or_default(),
                neighbours: neighbours.get(c.id.as_str()).cloned().unwrap_or_default(),
                liked: signal("liked").and_then(Value::as_bool).unwrap_or(false),
                hidden: signal("hidden").and_then(Value::as_bool).unwrap_or(false),
                last_seen: signal("lastSeen").and_then(Value::as_f64),
                content: c.properties.get("card").filter(|v| !v.is_null()).cloned(),
            }
        })
        .collect()
}

/// The explore candidates of a path: those kept on its levels, minus any the learner has since added.
pub fn explore_of(graph: &NodeBasedKnowledgeGraph) -> Vec<ExploreCandidate> {
    let path_title = extract_learning_path_summary(graph).title;
    layer_nodes(graph)
        .into_iter()
        .flat_map(|layer| {
            layer
                .properties
                .get("exploreCandidates")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| {
                    !graph
                        .nodes
                        .contains_key(e.get("name").and_then(Value::as_str).unwrap_or_default())
                })
                .map(|e| ExploreCandidate {
                    graph_id: graph.id.clone(),
                    path_title: path_title.clone(),
                    name: e
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: e
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn newest_layer(graph: &NodeBasedKnowledgeGraph) -> Option<&GraphNode> {
    let mut layers = layer_nodes(graph);
    layers.sort_by(|a, b| {
        let na = a
            .properties
            .get("layerNumber")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let nb = b
            .properties
            .get("layerNumber")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        super::common::num_order(nb, na)
    });
    layers.into_iter().next()
}

fn load_owned(
    deps: &KnowledgeDeps,
    uid: &str,
    only: Option<Vec<String>>,
) -> StoreResult<Vec<NodeBasedKnowledgeGraph>> {
    let ids = match only {
        Some(ids) => ids,
        None => deps.graphs.list_graph_ids(uid)?,
    };
    let mut graphs = Vec::new();
    for id in &ids {
        graphs.push(load_graph(deps, uid, id)?);
    }
    graphs.sort_by(|a, b| {
        super::common::num_order(
            extract_learning_path_summary(b).updated_at,
            extract_learning_path_summary(a).updated_at,
        )
    });
    Ok(graphs)
}

fn start(call: &KnowledgeCall<'_>, model: &str) {
    call.progress(KnowledgeProgress::new("start", "", model));
}

fn validated_card(generated: &Value) -> StoreResult<Value> {
    check_card(generated)
        .map_err(|issues| KnowledgeError::service(format!("The model's card is invalid: {issues}")))
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let sources = env.feed.clone();
    table.insert("feed", {
        let sources = sources.clone();
        action(move |call, p, deps| {
            let mix_ty = vec![
                params::opt_int("review", Some(0.0), None),
                params::opt_int("frontier", Some(0.0), None),
                params::opt_int("explore", Some(0.0), None),
            ];
            let mut mix_required = mix_ty.clone();
            for f in &mut mix_required {
                f.optional = false;
            }
            validate(
                p,
                &[
                    params::opt_id("cursor"),
                    params::opt_int("limit", Some(1.0), Some(20.0)),
                    params::opt_list("graphIds", params::id_ty(), false),
                    params::object("mix", mix_required, true),
                ],
            )?;
            let cursor = match params::opt_str_of(p, "cursor") {
                None => None,
                Some(text) => match decode_cursor(text).map_err(KnowledgeError::other)? {
                    Some(c) => Some(c),
                    None => {
                        return Err(KnowledgeError::validation(
                            "The feed cursor is not one this feed returned",
                        ))
                    }
                },
            };
            let weight = |m: &Map<String, Value>, k: &str| {
                m.get(k).and_then(Value::as_f64).unwrap_or(0.0) as usize
            };
            let mix = p
                .get("mix")
                .and_then(Value::as_object)
                .map_or(DEFAULT_FEED_MIX, |m| FeedMix {
                    review: weight(m, "review"),
                    frontier: weight(m, "frontier"),
                    explore: weight(m, "explore"),
                });
            let only = p.get("graphIds").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });
            let mut graphs = load_owned(deps, call.uid, only)?;
            if mix.explore > 0 {
                if let Some(newest) = graphs.first() {
                    if explore_of(newest).is_empty() {
                        if let Some(layer) = newest_layer(newest) {
                            start(call, &sources.model());
                            let request = ExploreRequest {
                                path_title: extract_learning_path_summary(newest).title,
                                goal: layer
                                    .properties
                                    .get("goal")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                level_number: layer
                                    .properties
                                    .get("layerNumber")
                                    .and_then(Value::as_f64)
                                    .unwrap_or(0.0),
                                existing: concept_nodes(newest)
                                    .iter()
                                    .map(|c| {
                                        c.properties
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_string()
                                    })
                                    .collect(),
                            };
                            let proposed = via_model("Proposing concepts to explore", || {
                                sources.propose_explore(&request)
                            })?;
                            call.throw_if_cancelled()?;
                            let mut properties = layer.properties.clone();
                            properties.insert("exploreCandidates".into(), Value::Array(proposed));
                            properties
                                .insert("exploreGeneratedAt".into(), number(deps.graphs.now_ms()));
                            let mut updates = Map::new();
                            updates.insert("properties".into(), Value::Object(properties));
                            let (newest_id, layer_id) = (newest.id.clone(), layer.id.clone());
                            deps.graphs
                                .update_node(call.uid, &newest_id, &layer_id, &updates)?;
                            graphs[0] = deps.graphs.get_graph(call.uid, &newest_id)?;
                        }
                    }
                }
            }
            let concepts: Vec<FeedConcept> = graphs
                .iter()
                .flat_map(|g| feed_concepts_of(g, call.uid))
                .collect();
            let explore: Vec<ExploreCandidate> = graphs.iter().flat_map(explore_of).collect();
            let limit = params::opt_f64_of(p, "limit").map_or(DEFAULT_FEED_LIMIT, |l| l as usize);
            let page = rank_feed(
                &concepts,
                &explore,
                deps.graphs.now_ms(),
                limit,
                mix,
                cursor.as_ref(),
            );
            Ok(json!({ "cards": page.cards, "nextCursor": page.next_cursor }))
        })
    });

    table.insert("cardContent", {
        let sources = sources.clone();
        action(move |call, p, deps| {
            validate(p, &[params::id("graphId"), params::id("conceptId")])?;
            let graph_id = params::str_of(p, "graphId");
            let concept_id = params::str_of(p, "conceptId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let concept = concept_of(&graph, concept_id)?;
            if let Some(card) = concept.properties.get("card").filter(|v| !v.is_null()) {
                return Ok(json!({ "content": card }));
            }
            start(call, &sources.model());
            let request = CardRequest {
                name: concept
                    .properties
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                description: concept
                    .properties
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                path_title: extract_learning_path_summary(&graph).title,
            };
            let generated = via_model("Writing the card", || sources.generate_card(&request))?;
            let content = validated_card(&generated)?;
            call.throw_if_cancelled()?;
            let mut properties = concept.properties.clone();
            properties.insert("card".into(), content.clone());
            let mut updates = Map::new();
            updates.insert("properties".into(), Value::Object(properties));
            deps.graphs
                .update_node(call.uid, graph_id, concept_id, &updates)?;
            Ok(json!({ "content": content }))
        })
    });

    table.insert("exploreContent", {
        let sources = sources.clone();
        action(move |call, p, deps| {
            validate(p, &[params::id("graphId"), params::id("name")])?;
            let graph_id = params::str_of(p, "graphId");
            let name = params::str_of(p, "name");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let Some(candidate) = explore_of(&graph).into_iter().find(|e| e.name == name) else {
                return Err(KnowledgeError::not_found(format!(
                    "\"{name}\" is not an explore card of path {graph_id}"
                )));
            };
            start(call, &sources.model());
            let request = CardRequest {
                name: name.to_string(),
                description: candidate.description,
                path_title: candidate.path_title,
            };
            let generated = via_model("Writing the card", || sources.generate_card(&request))?;
            Ok(json!({ "content": validated_card(&generated)? }))
        })
    });

    table.insert(
        "feedSignal",
        action(|call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::id("conceptId"),
                    params::pick("signal", &["like", "unlike", "notInterested", "seen"]),
                ],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let concept_id = params::str_of(p, "conceptId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let concept = concept_of(&graph, concept_id)?;
            let mut next = concept
                .properties
                .get("feed")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(|| {
                    Map::from_iter([
                        ("liked".into(), json!(false)),
                        ("hidden".into(), json!(false)),
                        ("lastSeen".into(), Value::Null),
                    ])
                });
            match params::str_of(p, "signal") {
                "like" => next.insert("liked".into(), json!(true)),
                "unlike" => next.insert("liked".into(), json!(false)),
                "notInterested" => next.insert("hidden".into(), json!(true)),
                _ => next.insert("lastSeen".into(), number(deps.graphs.now_ms())),
            };
            let mut properties = concept.properties.clone();
            properties.insert("feed".into(), Value::Object(next));
            let mut updates = Map::new();
            updates.insert("properties".into(), Value::Object(properties));
            deps.graphs
                .update_node(call.uid, graph_id, concept_id, &updates)?;
            Ok(json!({ "saved": true }))
        }),
    );

    table.insert(
        "addExplore",
        action(|call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::id("name"),
                    params::text("description"),
                ],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let name = params::str_of(p, "name");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let candidate_layer = layer_nodes(&graph).into_iter().find(|l| {
                l.properties
                    .get("exploreCandidates")
                    .and_then(Value::as_array)
                    .is_some_and(|list| {
                        list.iter()
                            .any(|e| e.get("name").and_then(Value::as_str) == Some(name))
                    })
            });
            let Some(layer) = candidate_layer else {
                return Err(KnowledgeError::not_found(format!(
                    "\"{name}\" is not an explore card of path {graph_id}"
                )));
            };
            if graph.nodes.contains_key(name) {
                return Err(KnowledgeError::conflict(format!(
                    "Path {graph_id} already has a concept \"{name}\""
                )));
            }
            let layer_number = layer
                .properties
                .get("layerNumber")
                .cloned()
                .unwrap_or(Value::Null);
            let sequence = concept_nodes(&graph)
                .iter()
                .filter(|c| c.properties.get("layer") == Some(&layer_number))
                .count()
                + 1;
            let now = deps.graphs.now_ms();
            let node = make_node(
                name,
                "Concept",
                json!({
                    "id": name, "name": name, "description": params::str_of(p, "description"),
                    "layer": layer_number, "sequence": sequence, "isAutoGenerated": true,
                }),
                now,
            );
            deps.graphs.create_node(call.uid, graph_id, node)?;
            deps.graphs.create_relationship(
                call.uid,
                graph_id,
                forward_relationship(graph_id, name, "containsConcept", now),
            )?;
            Ok(json!({ "conceptId": name, "routeKey": to_route_key(name) }))
        }),
    );
}
