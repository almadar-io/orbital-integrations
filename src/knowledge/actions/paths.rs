//! The learner's paths (`paths.ts`): listing, the map, one path by level, and removal. Generating the next
//! level and a level's practice stream a model reply and live with the model ports.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::batch::apply_mutation_batch_safe;
use super::common::{locale_compare, mastery_of, num_order, num_prop, str_prop};
use super::concept_stream::ConceptScanner;
use super::expansion::{
    build_expansion_mutations, build_expansion_prompt, ExpansionPromptContext, TargetMilestone,
};
use super::goal::{
    focus_of, infer_difficulty, infer_focus, infer_learning_goal, resolve_difficulty,
};
use super::mappers::to_knowledge_concept;
use super::params::{self, validate};
use super::prompts::{build_layer_practice_prompt, LayerPracticeContext};
use super::queries::{
    assign_clusters, canonical_concept_key, compute_shared_concept_edges,
    extract_learning_path_summary, get_concepts_by_layer, LearningPathSummary, PathCluster,
    PathInput, PATH_LINK_JACCARD,
};
use super::{action, Embedder, Env};
use crate::json::number;
use crate::knowledge::engine::mutations::{GraphMutation, MutationBatch, MutationMetadata};
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use crate::knowledge::store::{KnowledgeDeps, KnowledgeError, ModelStream, StoreResult};
use crate::knowledge::{ActionTable, KnowledgeCall, KnowledgeProgress};
use serde_json::Map;

const DEFAULT_PAGE_SIZE: f64 = 9.0;
const MIN_PAGE_SIZE: f64 = 1.0;
const MAX_PAGE_SIZE: f64 = 48.0;
const SEARCH_NODE_TYPES: [&str; 3] = ["Concept", "LearningGoal", "Lesson"];
const SEMANTIC_SEARCH_K: usize = 40;
const RELEVANCE_FLOOR: f64 = 0.2;
const SIMILARITY_FLOOR: f64 = 0.2;

struct LoadedPath {
    summary: LearningPathSummary,
    level_count: usize,
    learned_count: usize,
    concept_keys: Vec<String>,
}

/// Levels of a graph: its distinct layers above the seed layer 0.
fn level_count_of(graph: &NodeBasedKnowledgeGraph) -> usize {
    get_concepts_by_layer(graph, false)
        .grouped
        .iter()
        .filter(|(layer, _)| *layer > 0.0)
        .count()
}

fn load_paths(uid: &str, deps: &KnowledgeDeps) -> StoreResult<Vec<LoadedPath>> {
    let mut loaded = Vec::new();
    for graph_id in deps.graphs.list_graph_ids(uid)? {
        let Ok(graph) = deps.graphs.get_graph(uid, &graph_id) else {
            continue;
        };
        let concept_ids = graph.ids_of("Concept");
        let learned_count = concept_ids
            .iter()
            .filter(|id| mastery_of(&graph, uid, id) == 3.0)
            .count();
        let concept_keys = concept_ids
            .iter()
            .map(|id| {
                graph
                    .nodes
                    .get(id)
                    .map_or_else(|| id.clone(), canonical_concept_key)
            })
            .collect();
        loaded.push(LoadedPath {
            summary: extract_learning_path_summary(&graph),
            level_count: level_count_of(&graph),
            learned_count,
            concept_keys,
        });
    }
    loaded.sort_by(|a, b| num_order(b.summary.updated_at, a.summary.updated_at));
    Ok(loaded)
}

struct Clustering {
    cluster_of: Vec<(String, PathCluster)>,
    clusters: Vec<PathCluster>,
    shared: Vec<super::queries::SharedConceptEdge>,
}

fn cluster_paths(paths: &[LoadedPath]) -> Clustering {
    let inputs: Vec<PathInput> = paths
        .iter()
        .map(|p| PathInput {
            id: p.summary.id.clone(),
            concept_ids: p.concept_keys.clone(),
        })
        .collect();
    let shared = compute_shared_concept_edges(&inputs);
    let cluster_of = assign_clusters(&inputs, &shared, PATH_LINK_JACCARD);
    let mut distinct: Vec<PathCluster> = Vec::new();
    for (_, c) in &cluster_of {
        match distinct.iter_mut().find(|d| d.id == c.id) {
            Some(slot) => *slot = c.clone(),
            None => distinct.push(c.clone()),
        }
    }
    distinct.sort_by(|a, b| b.size.cmp(&a.size));
    Clustering {
        cluster_of,
        clusters: distinct,
        shared,
    }
}

fn cluster_id_of<'a>(
    cluster_of: &'a [(String, PathCluster)],
    path_id: &str,
) -> Option<&'a PathCluster> {
    cluster_of
        .iter()
        .find(|(id, _)| id == path_id)
        .map(|(_, c)| c)
}

fn to_knowledge_path(path: &LoadedPath, cluster_of: &[(String, PathCluster)]) -> Value {
    let s = &path.summary;
    json!({
        "id": s.id,
        "title": s.title,
        "description": s.description,
        "conceptCount": s.concept_count,
        "levelCount": path.level_count,
        "learnedCount": path.learned_count,
        "seedConcept": s.seed_concept.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
        "clusterId": cluster_id_of(cluster_of, &s.id).map(|c| c.id.clone()).unwrap_or_default(),
        "createdAt": number(s.created_at),
        "updatedAt": number(s.updated_at),
    })
}

fn clusters_json(clusters: &[PathCluster]) -> Vec<Value> {
    clusters
        .iter()
        .map(|c| json!({ "id": c.id, "name": c.name, "size": c.size }))
        .collect()
}

/// Rank graphs by the best node a query matches in each; graphs under the floor are dropped.
fn rank_paths_by_query(
    deps: &KnowledgeDeps,
    graph_ids: &[String],
    query: &str,
) -> StoreResult<Vec<String>> {
    if graph_ids.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let types: Vec<String> = SEARCH_NODE_TYPES.iter().map(|t| (*t).to_string()).collect();
    let hits =
        deps.vectors
            .search_by_text_cross(graph_ids, query, SEMANTIC_SEARCH_K, Some(&types))?;
    let mut best: Vec<(String, f64)> = Vec::new();
    for hit in hits {
        match best.iter_mut().find(|(id, _)| *id == hit.graph_id) {
            Some(entry) => {
                if hit.score > entry.1 {
                    entry.1 = hit.score;
                }
            }
            None => best.push((hit.graph_id, hit.score)),
        }
    }
    let mut ranked: Vec<(String, f64)> = best
        .into_iter()
        .filter(|(_, s)| *s >= RELEVANCE_FLOOR)
        .collect();
    ranked.sort_by(|a, b| num_order(b.1, a.1));
    Ok(ranked.into_iter().map(|(id, _)| id).collect())
}

fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// All-pairs cosine similarity of each path's `title description`, above the floor. Empty without embeddings.
fn compute_path_similarity(embedder: &dyn Embedder, paths: &[&LearningPathSummary]) -> Vec<Value> {
    if paths.len() < 2 {
        return Vec::new();
    }
    let texts: Vec<String> = paths
        .iter()
        .map(|p| format!("{} {}", p.title, p.description).trim().to_string())
        .collect();
    let Some(vectors) = embedder
        .embed_batch(&texts)
        .filter(|v| v.len() == paths.len())
    else {
        return Vec::new();
    };
    let mut edges = Vec::new();
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            let weight = cosine_similarity(&vectors[i], &vectors[j]).max(0.0);
            if weight >= SIMILARITY_FLOOR {
                edges.push(json!({ "source": paths[i].id, "target": paths[j].id, "weight": number(weight) }));
            }
        }
    }
    edges
}

fn load_owned_graph(
    uid: &str,
    graph_id: &str,
    deps: &KnowledgeDeps,
) -> StoreResult<NodeBasedKnowledgeGraph> {
    if !deps.graphs.graph_exists(uid, graph_id) {
        return Err(KnowledgeError::not_found(format!(
            "Path {graph_id} not found"
        )));
    }
    deps.graphs.get_graph(uid, graph_id)
}

fn review_of(graph: &NodeBasedKnowledgeGraph, layer_number: f64) -> String {
    graph
        .nodes
        .values()
        .find(|n| {
            n.node_type == "Layer"
                && n.properties.get("layerNumber").and_then(Value::as_f64) == Some(layer_number)
        })
        .and_then(|n| n.properties.get("review").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

const DEFAULT_EXPAND_CONCEPTS: usize = 10;

fn require_seed(graph: &NodeBasedKnowledgeGraph) -> StoreResult<()> {
    let seed = graph
        .seed_concept_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|id| graph.nodes.get(id));
    if seed.is_none_or(|n| n.node_type != "Concept") {
        return Err(KnowledgeError::validation(format!(
            "Path {} has no seed concept to expand from",
            graph.id
        )));
    }
    Ok(())
}

/// The graph with its goal's chosen level set (the level every later generation reads).
fn with_assessed_level(
    graph: &NodeBasedKnowledgeGraph,
    level: &str,
) -> StoreResult<NodeBasedKnowledgeGraph> {
    let goal_id = graph.ids_of("LearningGoal").first().cloned();
    let goal = goal_id
        .as_ref()
        .and_then(|id| graph.nodes.get(id))
        .filter(|n| n.node_type == "LearningGoal");
    let (Some(goal_id), Some(goal)) = (goal_id.as_ref(), goal) else {
        return Err(KnowledgeError::validation(format!(
            "Path {} has no goal to set a level on",
            graph.id
        )));
    };
    let mut leveled = goal.clone();
    leveled
        .properties
        .insert("assessedLevel".into(), json!(level));
    let mut next = graph.clone();
    next.nodes.insert(goal_id.clone(), leveled);
    Ok(next)
}

fn open_expansion_stream(
    deps: &KnowledgeDeps,
    graph: &NodeBasedKnowledgeGraph,
    num_concepts: usize,
) -> StoreResult<ModelStream> {
    let seed_id = graph
        .seed_concept_id
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            KnowledgeError::other("Seed concept not found — graph does not have a seedConceptId")
        })?;
    let seed = match graph.nodes.get(&seed_id) {
        Some(n) if n.node_type == "Concept" => n,
        _ => {
            return Err(KnowledgeError::other(format!(
                "Seed concept {seed_id} not found in graph nodes"
            )))
        }
    };
    let previous: Vec<&GraphNode> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Concept")
        .collect();
    let layers: Vec<&GraphNode> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Layer")
        .collect();
    let next_layer_number = if layers.is_empty() {
        1.0
    } else {
        layers
            .iter()
            .map(|n| num_prop(n, "layerNumber").unwrap_or(0.0))
            .fold(f64::MIN, f64::max)
            + 1.0
    };
    let goal = infer_learning_goal(graph);
    let difficulty = resolve_difficulty(goal.as_ref(), graph);
    let focus = focus_of(goal.as_ref(), graph);
    let total_milestones = goal
        .as_ref()
        .and_then(|g| g.milestones.as_ref())
        .map_or(0, Vec::len);

    struct Ms {
        sequence: f64,
        title: String,
        description: Option<String>,
        completed: bool,
    }
    let mut sorted: Vec<Ms> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Milestone")
        .map(|n| Ms {
            sequence: num_prop(n, "sequence").unwrap_or(0.0),
            title: n
                .properties
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| n.properties.get("title").and_then(Value::as_str))
                .unwrap_or("Milestone")
                .to_string(),
            description: n
                .properties
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            completed: n
                .properties
                .get("completed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect();
    sorted.sort_by(|a, b| num_order(a.sequence, b.sequence));
    let milestones: Vec<Ms> = if !sorted.is_empty() {
        sorted
    } else {
        goal.as_ref()
            .and_then(|g| g.milestones.as_ref())
            .map(|ms| {
                ms.iter()
                    .enumerate()
                    .map(|(i, m)| Ms {
                        sequence: i as f64,
                        title: m.title.clone(),
                        description: m.description.clone(),
                        completed: m.completed,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let target = if milestones.is_empty() {
        None
    } else {
        let clamped = (next_layer_number - 1.0)
            .min(milestones.len() as f64 - 1.0)
            .max(0.0) as usize;
        milestones.get(clamped).map(|m| TargetMilestone {
            index: m.sequence,
            title: m.title.clone(),
            description: m.description.clone(),
            node_id: None,
        })
    };
    let completed = milestones.iter().filter(|m| m.completed).count();
    let (system, user) = build_expansion_prompt(&ExpansionPromptContext {
        seed_concept: seed,
        previous_layers: previous,
        existing_layers: layers.clone(),
        num_concepts,
        learning_goal: goal.as_ref(),
        difficulty,
        focus,
        is_first_layer: layers.is_empty(),
        target_milestone: target.as_ref(),
        completed_milestones_count: completed,
        total_milestones,
    });
    deps.models.stream(&json!({
        "systemPrompt": system, "userPrompt": user, "provider": "openrouter", "model": "qwen/qwen3-30b-a3b-instruct-2507",
        "stream": true,
    }))
}

/// Relay a model stream like `relayModelStream`, naming each concept as the model closes its tag.
fn relay_with_concept_steps(call: &KnowledgeCall<'_>, stream: ModelStream) -> StoreResult<String> {
    let model = stream.model.clone();
    call.progress(KnowledgeProgress::new("start", "", &model));
    let mut scanner = ConceptScanner::new();
    let mut content = String::new();
    for chunk in stream.chunks {
        call.throw_if_cancelled()?;
        let chunk = chunk?;
        if !chunk.content.is_empty() {
            content.push_str(&chunk.content);
            call.progress(KnowledgeProgress::new(
                "delta",
                chunk.content.clone(),
                &model,
            ));
            for scanned in scanner.feed(&chunk.content) {
                call.progress(KnowledgeProgress::new("step", scanned.name, &model));
            }
        }
        if chunk.done {
            break;
        }
    }
    call.throw_if_cancelled()?;
    Ok(content)
}

pub struct AppliedExpansion {
    pub graph: NodeBasedKnowledgeGraph,
    pub level_name: String,
    pub concept_ids: Vec<String>,
}

/// Turn the model's expansion narrative into the next level of the graph; `before_apply` may adjust the batch.
pub fn apply_expansion(
    graph: &NodeBasedKnowledgeGraph,
    content: &str,
    now: f64,
    before_apply: impl FnOnce(&mut MutationBatch) -> StoreResult<()>,
) -> StoreResult<AppliedExpansion> {
    require_seed(graph)?;
    let goal = infer_learning_goal(graph);
    let mut built = build_expansion_mutations(content, graph, goal.as_ref(), None, now)?;
    if built.parsed.concepts.is_empty() {
        return Err(KnowledgeError::service(
            "The model returned no concepts for the next level",
        ));
    }
    before_apply(&mut built.mutations)?;
    let (updated, _errors) = apply_mutation_batch_safe(graph, &built.mutations, now);
    let layer_id = built.mutations.mutations.iter().find_map(|m| match m {
        GraphMutation::CreateNode { node, .. } if node.node_type == "Layer" => {
            Some(node.id.clone())
        }
        _ => None,
    });
    let concept_ids = built
        .mutations
        .mutations
        .iter()
        .filter_map(|m| match m {
            GraphMutation::CreateRelationship { relationship }
                if relationship.rel_type == "containsConcept"
                    && Some(&relationship.source) == layer_id.as_ref() =>
            {
                Some(relationship.target.clone())
            }
            _ => None,
        })
        .collect();
    Ok(AppliedExpansion {
        graph: updated,
        level_name: built.parsed.level_name.unwrap_or_default(),
        concept_ids,
    })
}

/// Give every new concept its cross-graph identity: embed the batch, then mint a self-identity for each (adopting
/// an existing path's identity needs a vector search by embedding, which the vector port does not carry).
fn canonicalize_batch(embedder: &dyn super::Embedder, batch: &mut MutationBatch) {
    let mut texts = Vec::new();
    for m in &batch.mutations {
        if let GraphMutation::CreateNode { node, .. } = m {
            let canonical = node
                .properties
                .get("canonicalName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if node.node_type == "Concept" && canonical.is_empty() {
                texts.push(super::vector_text::build_embedding_text(node));
            }
        }
    }
    if texts.is_empty() {
        return;
    }
    let _ = embedder.embed_batch(&texts);
    for m in &mut batch.mutations {
        if let GraphMutation::CreateNode { node, .. } = m {
            let canonical = node
                .properties
                .get("canonicalName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if node.node_type == "Concept" && canonical.is_empty() {
                let name = str_prop(node, "name");
                node.properties.insert("canonicalName".into(), json!(name));
            }
        }
    }
}

fn layer_nodes(graph: &NodeBasedKnowledgeGraph) -> Vec<&GraphNode> {
    graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Layer")
        .collect()
}

fn concepts_of_layer<'a>(graph: &'a NodeBasedKnowledgeGraph, layer_id: &str) -> Vec<&'a GraphNode> {
    graph
        .relationships
        .iter()
        .filter(|r| r.source == layer_id && r.rel_type == "containsConcept")
        .filter_map(|r| graph.nodes.get(&r.target))
        .filter(|n| n.node_type == "Concept")
        .collect()
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let canonical_embedder = env.embedder.clone();
    table.insert(
        "expand",
        action(move |call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::opt_int("numConcepts", Some(1.0), None),
                    params::opt_pick("difficulty", &["beginner", "intermediate", "advanced"]),
                ],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let num_concepts = params::opt_f64_of(p, "numConcepts")
                .map_or(DEFAULT_EXPAND_CONCEPTS, |n| n as usize);
            let loaded = load_owned_graph(call.uid, graph_id, deps)?;
            let graph = match params::opt_str_of(p, "difficulty") {
                Some(level) => with_assessed_level(&loaded, level)?,
                None => loaded,
            };
            require_seed(&graph)?;
            let stream = open_expansion_stream(deps, &graph, num_concepts)?;
            let content = relay_with_concept_steps(call, stream)?;
            call.throw_if_cancelled()?;
            let applied = apply_expansion(&graph, &content, deps.graphs.now_ms(), |batch| {
                deps.graphs.list_graph_ids(call.uid)?;
                canonicalize_batch(canonical_embedder.as_ref(), batch);
                Ok(())
            })?;
            call.throw_if_cancelled()?;
            let saved = deps
                .graphs
                .save_graph(call.uid, &applied.graph, graph.version)?;
            let added: Vec<Value> = applied
                .concept_ids
                .iter()
                .filter_map(|id| {
                    saved
                        .nodes
                        .get(id)
                        .map(|node| to_knowledge_concept(&saved, node, call.uid))
                })
                .collect();
            Ok(json!({ "levelName": applied.level_name, "added": added }))
        }),
    );

    table.insert(
        "layerPractice",
        action(|call, p, deps| {
            validate(p, &[params::id("graphId"), params::int("layerNumber")])?;
            let graph_id = params::str_of(p, "graphId");
            let layer_number = params::opt_f64_of(p, "layerNumber").unwrap_or(0.0);
            let graph = load_owned_graph(call.uid, graph_id, deps)?;
            let layer = layer_nodes(&graph).into_iter().find(|n| num_prop(n, "layerNumber") == Some(layer_number));
            let concepts = layer.map(|l| concepts_of_layer(&graph, &l.id)).unwrap_or_default();
            let Some(layer) = layer.filter(|_| !concepts.is_empty()) else {
                return Err(KnowledgeError::not_found(format!("No concepts found for level {}", crate::jsnumber::js_number(layer_number))));
            };
            let seed = graph.seed_concept_id.as_deref().and_then(|id| graph.nodes.get(id)).filter(|n| n.node_type == "Concept");
            let (system, user) = build_layer_practice_prompt(&LayerPracticeContext {
                concepts,
                layer_goal: layer.properties.get("goal").and_then(Value::as_str).unwrap_or_default().to_string(),
                layer_number,
                seed_concept: seed,
                difficulty: Some(infer_difficulty(&graph)),
                focus: infer_focus(&graph),
            });
            let stream = deps.models.stream(&json!({
                "systemPrompt": system, "userPrompt": user, "provider": "deepseek", "model": "deepseek-v4-flash", "stream": true,
            }))?;
            let content = call.relay_model_stream(stream)?;
            call.throw_if_cancelled()?;
            let review = content.trim().to_string();
            if review.is_empty() {
                return Err(KnowledgeError::service("The model returned an empty review"));
            }
            let layer_id = layer_nodes(&graph)
                .into_iter()
                .find(|n| num_prop(n, "layerNumber") == Some(layer_number))
                .map(|n| n.id.clone())
                .ok_or_else(|| KnowledgeError::not_found(format!("Path {} has no level {}", graph.id, crate::jsnumber::js_number(layer_number))))?;
            let now = deps.graphs.now_ms();
            let mut properties = Map::new();
            properties.insert("review".into(), json!(review));
            properties.insert("reviewGeneratedAt".into(), number(now));
            let batch = MutationBatch {
                mutations: vec![GraphMutation::UpdateNode { node_id: layer_id, properties, update_timestamp: Some(true) }],
                metadata: Some(MutationMetadata { operation: "generateLayerPractice".into(), timestamp: now, model: None }),
            };
            let (updated, errors) = apply_mutation_batch_safe(&graph, &batch, now);
            if !errors.is_empty() {
                return Err(KnowledgeError::service(format!(
                    "Could not store the review: {}",
                    errors.iter().map(|e| e.error.clone()).collect::<Vec<_>>().join("; ")
                )));
            }
            deps.graphs.save_graph(call.uid, &updated, graph.version)?;
            Ok(json!({ "review": review }))
        }),
    );

    table.insert(
        "listPaths",
        action(|call, p, deps| {
            validate(
                p,
                &[
                    params::opt_text("search"),
                    params::opt_pick("sort", &["recent", "oldest", "az", "za"]),
                    params::opt_text("clusterId"),
                    params::opt_int("page", None, None),
                    params::opt_int("limit", None, None),
                ],
            )?;
            let search = params::opt_str_of(p, "search")
                .map(str::trim)
                .unwrap_or_default();
            let sort = params::opt_str_of(p, "sort").unwrap_or("recent");
            let page = params::opt_f64_of(p, "page").unwrap_or(1.0).max(1.0);
            let limit = params::opt_f64_of(p, "limit")
                .unwrap_or(DEFAULT_PAGE_SIZE)
                .clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE);

            let paths = load_paths(call.uid, deps)?;
            let clustering = cluster_paths(&paths);
            let mut items: Vec<&LoadedPath> = paths.iter().collect();
            if let Some(cluster_id) = params::opt_str_of(p, "clusterId").filter(|c| !c.is_empty()) {
                items.retain(|path| {
                    cluster_id_of(&clustering.cluster_of, &path.summary.id)
                        .is_some_and(|c| c.id == cluster_id)
                });
            }
            if !search.is_empty() {
                let ids: Vec<String> = items.iter().map(|p| p.summary.id.clone()).collect();
                let ranked = rank_paths_by_query(deps, &ids, search)?;
                let by_id: HashMap<&str, &LoadedPath> =
                    items.iter().map(|p| (p.summary.id.as_str(), *p)).collect();
                items = ranked
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()).copied())
                    .collect();
            } else if sort == "oldest" {
                items.sort_by(|a, b| num_order(a.summary.updated_at, b.summary.updated_at));
            } else if sort == "az" {
                items.sort_by(|a, b| locale_compare(&a.summary.title, &b.summary.title));
            } else if sort == "za" {
                items.sort_by(|a, b| locale_compare(&b.summary.title, &a.summary.title));
            }

            let total = items.len();
            let start = ((page - 1.0) * limit) as usize;
            let end = (start + limit as usize).min(total);
            let shown: Vec<Value> = if start >= total {
                Vec::new()
            } else {
                items[start..end]
                    .iter()
                    .map(|path| to_knowledge_path(path, &clustering.cluster_of))
                    .collect()
            };
            Ok(json!({
                "items": shown,
                "total": total,
                "page": number(page),
                "totalPages": ((total as f64 / limit).ceil() as usize).max(1),
                "clusters": clusters_json(&clustering.clusters),
            }))
        }),
    );

    let embedder = env.embedder.clone();
    table.insert(
        "pathMap",
        action(move |call, p, deps| {
            let issues: Vec<String> = p
                .iter()
                .map(|(k, v)| {
                    let seen = match v {
                        Value::Null => "null",
                        Value::Bool(_) => "boolean",
                        Value::Number(_) => "number",
                        Value::String(_) => "string",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                    };
                    format!("{k}: Expected never, received {seen}")
                })
                .collect();
            if !issues.is_empty() {
                return Err(KnowledgeError::validation(issues.join("; ")));
            }
            let paths = load_paths(call.uid, deps)?;
            let summaries: Vec<&LearningPathSummary> = paths.iter().map(|p| &p.summary).collect();
            let similarity = compute_path_similarity(embedder.as_ref(), &summaries);
            let clustering = cluster_paths(&paths);
            Ok(json!({
                "paths": paths.iter().map(|path| to_knowledge_path(path, &clustering.cluster_of)).collect::<Vec<_>>(),
                "similarity": similarity,
                "sharedConcepts": clustering.shared.iter().map(|e| json!({
                    "source": e.source,
                    "target": e.target,
                    "weight": number(e.weight),
                    "shared": e.shared_concepts,
                    "linked": e.weight >= PATH_LINK_JACCARD,
                })).collect::<Vec<_>>(),
                "clusters": clusters_json(&clustering.clusters),
            }))
        }),
    );

    table.insert(
        "conceptsByLayer",
        action(|call, p, deps| {
            validate(p, &[params::id("graphId")])?;
            let graph_id = params::str_of(p, "graphId");
            let graph = load_owned_graph(call.uid, graph_id, deps)?;
            let summary = extract_learning_path_summary(&graph);
            let by_layer = get_concepts_by_layer(&graph, true);
            let mut numbers: Vec<f64> = by_layer.grouped.iter().map(|(l, _)| *l).collect();
            for info in &by_layer.layer_info {
                if !numbers.contains(&info.layer_number) {
                    numbers.push(info.layer_number);
                }
            }
            numbers.sort_by(|a, b| num_order(*a, *b));
            let layers: Vec<Value> = numbers
                .iter()
                .map(|&layer_number| {
                    let concepts: Vec<Value> = by_layer
                        .grouped
                        .iter()
                        .find(|(l, _)| *l == layer_number)
                        .map(|(_, list)| list.as_slice())
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|c| graph.nodes.get(&c.id).map(|node| to_knowledge_concept(&graph, node, call.uid)))
                        .collect();
                    json!({
                        "layerNumber": number(layer_number),
                        "goal": by_layer.layer_info.iter().find(|i| i.layer_number == layer_number).and_then(|i| i.goal.clone()).unwrap_or_default(),
                        "review": review_of(&graph, layer_number),
                        "concepts": concepts,
                    })
                })
                .collect();
            Ok(json!({
                "graphId": graph_id,
                "title": summary.title,
                "goal": summary.description,
                "seedConcept": summary.seed_concept.map(|c| c.name).unwrap_or_default(),
                "layers": layers,
            }))
        }),
    );

    table.insert(
        "deletePath",
        action(|call, p, deps| {
            validate(p, &[params::id("graphId")])?;
            let graph_id = params::str_of(p, "graphId");
            if !deps.graphs.graph_exists(call.uid, graph_id) {
                return Err(KnowledgeError::not_found(format!(
                    "Path {graph_id} not found"
                )));
            }
            deps.graphs.delete_graph(call.uid, graph_id)?;
            deps.vectors.delete_graph(graph_id)?;
            deps.students
                .remove_enrolled_graph(call.uid, call.uid, graph_id)?;
            Ok(json!({ "deleted": true }))
        }),
    );
}
