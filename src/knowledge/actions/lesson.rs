//! Lessons, notes, questions and progress (`lesson.ts`). Generating a lesson and answering a question stream a
//! model reply; those two actions live with the model ports.

use serde_json::{json, Map, Value};

use regex::Regex;

use super::common::{
    concept_of, concept_ref, from_route_key, load_graph, make_node, stamp, to_route_key,
};
use super::explain_prompt::{build_explain_prompt, CrossGraphPrior, ExplainPromptContext};
use super::goal::{focus_of, infer_difficulty, infer_learning_goal, resolve_difficulty};
use super::mappers::to_knowledge_concept;
use super::params::{self, validate, Field};
use super::prompts::build_answer_question_prompt;
use super::queries::{extract_learning_path_summary, get_concept_detail, get_concepts_by_layer};
use super::{action, Env};
use crate::json::number;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use crate::knowledge::firestore::literal_relationship;
use crate::knowledge::store::{KnowledgeDeps, KnowledgeError, StoreResult, VectorHit};
use crate::knowledge::{ActionTable, KnowledgeCall};

fn lesson_of<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    lesson_id: &str,
) -> StoreResult<&'a GraphNode> {
    match graph.nodes.get(lesson_id) {
        Some(n) if n.node_type == "Lesson" => Ok(n),
        _ => Err(KnowledgeError::not_found(format!(
            "Lesson {lesson_id} not found in path {}",
            graph.id
        ))),
    }
}

pub fn lesson_of_concept<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    concept_id: &str,
) -> Option<&'a GraphNode> {
    graph
        .relationships
        .iter()
        .filter(|rel| rel.source == concept_id && rel.rel_type == "hasLesson")
        .find_map(|rel| {
            graph
                .nodes
                .get(&rel.target)
                .filter(|n| n.node_type == "Lesson")
        })
}

fn to_note(item: &Value) -> Value {
    json!({
        "id": item.get("id").cloned().unwrap_or(Value::Null),
        "text": item.get("text").cloned().unwrap_or(Value::Null),
        "selectedText": item.get("selectedText").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("")),
        "timestamp": item.get("timestamp").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(0)),
    })
}

fn to_question(item: &Value) -> Value {
    json!({
        "id": item.get("id").cloned().unwrap_or(Value::Null),
        "question": item.get("question").cloned().unwrap_or(Value::Null),
        "answer": item.get("answer").cloned().unwrap_or(Value::Null),
        "selectedText": item.get("selectedText").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("")),
        "timestamp": item.get("timestamp").cloned().unwrap_or(Value::Null),
    })
}

fn items_of(lesson: &GraphNode, key: &str) -> Vec<Value> {
    lesson
        .properties
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Change one Lesson's properties on a fresh read of the graph and save with its version, so a concurrent writer conflicts instead of being overwritten.
fn mutate_lesson(
    deps: &KnowledgeDeps,
    uid: &str,
    graph_id: &str,
    lesson_id: &str,
    change: impl FnOnce(&GraphNode) -> StoreResult<Map<String, Value>>,
) -> StoreResult<()> {
    let mut graph = load_graph(deps, uid, graph_id)?;
    let next = change(lesson_of(&graph, lesson_id)?)?;
    let now = deps.graphs.now_ms();
    let version = graph.version;
    if let Some(lesson) = graph.nodes.get_mut(lesson_id) {
        for (k, v) in next {
            lesson.properties.insert(k, v);
        }
        lesson.updated_at = Some(stamp(now));
    }
    graph.updated_at = now;
    deps.graphs.save_graph(uid, &graph, version)?;
    Ok(())
}

/// One point each for an activation answer, a reflection, any Bloom answer, three or more Bloom answers.
fn calculate_mastery_level(
    existing: f64,
    activation: Option<&str>,
    reflection: Option<&str>,
    bloom_answered: usize,
) -> f64 {
    if existing >= 2.0 {
        return existing;
    }
    let score = u32::from(activation.is_some_and(|s| !s.is_empty()))
        + u32::from(reflection.is_some_and(|s| !s.is_empty()))
        + u32::from(bloom_answered > 0)
        + u32::from(bloom_answered >= 3);
    match score {
        0 => 0.0,
        1 => 1.0,
        2 | 3 => 2.0,
        _ => 3.0,
    }
}

fn split_top_level(list: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for ch in list.chars() {
        if ch == '(' {
            depth += 1;
        }
        if ch == ')' {
            depth = (depth - 1).max(0);
        }
        if ch == ',' && depth == 0 {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    parts.push(current);
    parts
}

/// Every `<prq>` tag's comma-separated names, de-duplicated; a trailing parenthetical is a description, not part of the name.
pub fn parse_prerequisites_from_lesson(lesson: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let Ok(re) = Regex::new(r"(?is)<prq>(.*?)</prq>") else {
        return names;
    };
    for caps in re.captures_iter(lesson) {
        for raw in split_top_level(&caps[1]) {
            let trimmed = raw.trim();
            let name = match trimmed.find('(') {
                Some(at) if at > 0 => trimmed[..at].trim(),
                _ => trimmed,
            };
            if !name.is_empty() && name.to_lowercase() != "none" && !names.iter().any(|n| n == name)
            {
                names.push(name.to_string());
            }
        }
    }
    names
}

pub struct AppliedLesson {
    pub graph: NodeBasedKnowledgeGraph,
    pub lesson: GraphNode,
}

/// Put generated lesson text on a concept: the existing Lesson node is regenerated in place (keeping the learner's
/// notes and questions), else a new one is linked with `hasLesson`.
pub fn apply_generated_lesson(
    graph: &NodeBasedKnowledgeGraph,
    concept_id: &str,
    lesson_text: &str,
    model: &str,
    now: f64,
) -> StoreResult<AppliedLesson> {
    concept_of(graph, concept_id)?;
    let content = lesson_text.trim();
    if content.is_empty() {
        return Err(KnowledgeError::service(
            "The model returned an empty lesson",
        ));
    }
    let prerequisites = parse_prerequisites_from_lesson(content);
    let mut next = graph.clone();
    if let Some(existing) = lesson_of_concept(graph, concept_id) {
        let mut lesson = existing.clone();
        lesson.properties.insert("content".into(), json!(content));
        lesson.properties.insert("generatedAt".into(), number(now));
        lesson.properties.insert("model".into(), json!(model));
        lesson
            .properties
            .insert("prerequisites".into(), json!(prerequisites));
        lesson.updated_at = Some(stamp(now));
        next.nodes.insert(lesson.id.clone(), lesson.clone());
        next.updated_at = now;
        return Ok(AppliedLesson {
            graph: next,
            lesson,
        });
    }
    let lesson_id = format!("lesson-{concept_id}-{}", crate::jsnumber::js_number(now));
    let lesson = make_node(
        &lesson_id,
        "Lesson",
        json!({ "id": lesson_id, "content": content, "generatedAt": number(now), "model": model, "prerequisites": prerequisites }),
        now,
    );
    next.nodes.insert(lesson_id.clone(), lesson.clone());
    next.node_types
        .get_or_insert_with("Lesson", Vec::new)
        .push(lesson_id.clone());
    next.relationships.push(literal_relationship(
        concept_id,
        &lesson_id,
        "hasLesson",
        now,
        None,
    ));
    next.updated_at = now;
    Ok(AppliedLesson {
        graph: next,
        lesson,
    })
}

/// The persistence half of `explain`: apply the generated text to the concept's lesson and save it.
pub fn store_explained_lesson(
    deps: &KnowledgeDeps,
    uid: &str,
    graph_id: &str,
    concept_id: &str,
    lesson_text: &str,
    model: &str,
) -> StoreResult<Value> {
    let graph = load_graph(deps, uid, graph_id)?;
    let applied =
        apply_generated_lesson(&graph, concept_id, lesson_text, model, deps.graphs.now_ms())?;
    deps.graphs.save_graph(uid, &applied.graph, graph.version)?;
    let _ = deps
        .vectors
        .upsert_nodes(graph_id, std::slice::from_ref(&applied.lesson), uid);
    Ok(json!({
        "lessonId": applied.lesson.id,
        "lesson": applied.lesson.properties.get("content").cloned().unwrap_or(Value::Null),
    }))
}

fn name_and_description(node: &GraphNode) -> String {
    let name = node
        .properties
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = node
        .properties
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("{name} {description}").trim().to_string()
}

/// Related concepts the learner already has on their OTHER paths; degrades to none when search is unavailable.
pub fn find_related_concepts(
    deps: &KnowledgeDeps,
    uid: &str,
    graph_id: &str,
    concept: &GraphNode,
) -> StoreResult<Vec<Value>> {
    let others: Vec<String> = deps
        .graphs
        .list_graph_ids(uid)?
        .into_iter()
        .filter(|id| id != graph_id)
        .collect();
    if others.is_empty() {
        return Ok(Vec::new());
    }
    let query = name_and_description(concept);
    let hits =
        deps.vectors
            .search_by_text_cross(&others, &query, 3, Some(&["Concept".to_string()]))?;
    Ok(related_from_hits(&hits, graph_id))
}

pub fn related_from_hits(hits: &[VectorHit], graph_id: &str) -> Vec<Value> {
    let mut related = Vec::new();
    for hit in hits {
        let Some(name) = hit.name.as_ref().filter(|n| !n.is_empty()) else {
            continue;
        };
        if hit.graph_id == graph_id {
            continue;
        }
        related.push(json!({
            "graphId": hit.graph_id, "conceptId": hit.node_id, "routeKey": to_route_key(&hit.node_id),
            "name": name, "text": hit.text,
        }));
        if related.len() == 3 {
            break;
        }
    }
    related
}

fn first_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// The explain model call: build the lesson prompt and open the stream.
fn open_explain_stream(
    deps: &KnowledgeDeps,
    visualizations: &[super::explain_prompt::VisualizationDomain],
    uid: &str,
    graph: &NodeBasedKnowledgeGraph,
    concept: &GraphNode,
    simple: bool,
    all_user_graph_ids: &[String],
) -> StoreResult<crate::knowledge::store::ModelStream> {
    let seed = graph
        .seed_concept_id
        .as_deref()
        .and_then(|id| graph.nodes.get(id))
        .filter(|n| n.node_type == "Concept");
    let prerequisites: Vec<&GraphNode> = graph
        .relationships
        .iter()
        .filter(|r| r.source == concept.id && r.rel_type == "hasPrerequisite")
        .filter_map(|r| graph.nodes.get(&r.target))
        .collect();
    let mut priors: Vec<CrossGraphPrior> = Vec::new();
    if !all_user_graph_ids.is_empty() {
        let others: Vec<String> = all_user_graph_ids
            .iter()
            .filter(|id| **id != graph.id)
            .cloned()
            .collect();
        if !others.is_empty() {
            let query = first_chars(&name_and_description(concept), 400);
            if let Ok(hits) = deps.vectors.search_by_text_cross(
                &others,
                &query,
                3,
                Some(&["Concept".to_string()]),
            ) {
                priors = hits
                    .iter()
                    .filter(|h| {
                        !h.graph_id.is_empty()
                            && h.graph_id != graph.id
                            && h.name.as_ref().is_some_and(|n| !n.is_empty())
                    })
                    .take(3)
                    .map(|h| CrossGraphPrior {
                        name: h.name.clone().unwrap_or_default(),
                        text: first_chars(&h.text, 180),
                    })
                    .collect();
            }
        }
    }
    let goal = infer_learning_goal(graph);
    let difficulty = resolve_difficulty(goal.as_ref(), graph);
    let focus = focus_of(goal.as_ref(), graph);
    let (system, user) = build_explain_prompt(&ExplainPromptContext {
        concept,
        seed_concept: seed,
        learning_goal: goal.as_ref(),
        difficulty,
        focus,
        simple,
        minimal: false,
        prerequisites,
        cross_graph_priors: priors,
        visualizations,
    });
    let _ = uid;
    deps.models.stream(&json!({
        "systemPrompt": system, "userPrompt": user, "provider": "deepseek", "model": "deepseek-v4-flash",
        "stream": true, "disableReasoning": true,
    }))
}

fn relay(
    call: &KnowledgeCall<'_>,
    stream: crate::knowledge::store::ModelStream,
) -> StoreResult<(String, String)> {
    let model = stream.model.clone();
    let text = call.relay_model_stream(stream)?;
    Ok((text, model))
}

fn graph_and_lesson() -> Vec<Field> {
    vec![params::id("graphId"), params::id("lessonId")]
}

/// The persistence half of `answerQuestion`: keep the Q&A on the concept's lesson with a server-generated id and time.
#[allow(clippy::too_many_arguments)]
fn record_answer(
    deps: &KnowledgeDeps,
    uuid: &super::IdSource,
    uid: &str,
    graph_id: &str,
    concept_id: &str,
    question: &str,
    answer: &str,
    selected_text: &str,
) -> StoreResult<Value> {
    let graph = load_graph(deps, uid, graph_id)?;
    concept_of(&graph, concept_id)?;
    let Some(lesson) = lesson_of_concept(&graph, concept_id) else {
        return Err(KnowledgeError::not_found(format!(
            "Concept {concept_id} has no lesson to keep the answer with; generate the lesson first"
        )));
    };
    let lesson_id = lesson.id.clone();
    let item = json!({
        "id": uuid(), "question": question, "answer": answer, "selectedText": selected_text,
        "timestamp": number(deps.graphs.now_ms()),
    });
    mutate_lesson(deps, uid, graph_id, &lesson_id, |l| {
        let mut questions = items_of(l, "questions");
        questions.push(item.clone());
        Ok(Map::from_iter([(
            "questions".to_string(),
            Value::Array(questions),
        )]))
    })?;
    Ok(to_question(&item))
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let uuid = env.uuid.clone();
    table.insert(
        "conceptDetail",
        action(|call, p, deps| {
            validate(p, &[params::id("graphId"), params::id("routeKey")])?;
            let route_key = params::str_of(p, "routeKey");
            let Some(concept_id) = from_route_key(route_key) else {
                return Err(KnowledgeError::not_found(format!("Concept {route_key} not found")));
            };
            let graph = load_graph(deps, call.uid, params::str_of(p, "graphId"))?;
            let node = concept_of(&graph, &concept_id)?;
            let detail = get_concept_detail(&graph, &concept_id).map_err(KnowledgeError::other)?;
            let lesson_node = match &detail.lesson_id {
                Some(id) => Some(lesson_of(&graph, id)?),
                None => None,
            };
            let concept = to_knowledge_concept(&graph, node, call.uid);
            let layer = detail.concept.layer;
            let same_layer: Vec<_> = get_concepts_by_layer(&graph, false)
                .concepts
                .into_iter()
                .filter(|c| c.layer == layer)
                .collect();
            let at = same_layer.iter().position(|c| c.id == concept_id);
            let reference = |c: Option<&super::queries::ConceptDisplay>| -> Vec<Value> {
                c.map(|c| vec![concept_ref(&c.id, &c.name)]).unwrap_or_default()
            };
            let pairs = |list: &[(String, String)]| -> Vec<Value> { list.iter().map(|(id, name)| concept_ref(id, name)).collect() };
            Ok(json!({
                "concept": concept,
                "pathTitle": extract_learning_path_summary(&graph).title,
                "lessonId": lesson_node.map(|l| l.id.clone()).unwrap_or_default(),
                "lesson": lesson_node.and_then(|l| l.properties.get("content").and_then(Value::as_str)).unwrap_or_default(),
                "prerequisites": pairs(&detail.prerequisites),
                "parents": pairs(&detail.parents),
                "previous": match at { Some(i) if i > 0 => reference(same_layer.get(i - 1)), _ => Vec::new() },
                "next": match at { Some(i) => reference(same_layer.get(i + 1)), None => Vec::new() },
                "notes": lesson_node.map(|l| items_of(l, "notes")).unwrap_or_default().iter().map(to_note).collect::<Vec<_>>(),
                "questions": lesson_node.map(|l| items_of(l, "questions")).unwrap_or_default().iter().map(to_question).collect::<Vec<_>>(),
            }))
        }),
    );

    let visualizations = env.visualizations.clone();
    table.insert(
        "explain",
        action(move |call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::id("conceptId"),
                    params::opt_flag("simple"),
                ],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let concept_id = params::str_of(p, "conceptId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let concept = concept_of(&graph, concept_id)?;
            let all_ids = deps.graphs.list_graph_ids(call.uid)?;
            let stream = open_explain_stream(
                deps,
                &visualizations,
                call.uid,
                &graph,
                concept,
                params::opt_bool_of(p, "simple") == Some(true),
                &all_ids,
            )?;
            let (text, model) = relay(call, stream)?;
            store_explained_lesson(deps, call.uid, graph_id, concept_id, &text, &model)
        }),
    );

    let answer_uuid = uuid.clone();
    table.insert(
        "answerQuestion",
        action(move |call, p, deps| {
            validate(
                p,
                &[params::id("graphId"), params::id("conceptId"), params::id("question"), params::opt_text("selectedText")],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let concept_id = params::str_of(p, "conceptId");
            let question = params::str_of(p, "question");
            let selected = params::opt_str_of(p, "selectedText");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let concept = concept_of(&graph, concept_id)?;
            if lesson_of_concept(&graph, concept_id).is_none() {
                return Err(KnowledgeError::not_found(format!(
                    "Concept {concept_id} has no lesson to keep the answer with; generate the lesson first"
                )));
            }
            let _all_ids = deps.graphs.list_graph_ids(call.uid)?;
            let difficulty = infer_difficulty(&graph);
            let query = first_chars(&name_and_description(concept), 400);
            let mut rag = String::new();
            if let Ok(hits) = deps.vectors.search_by_text_cross(std::slice::from_ref(&graph.id), &query, 3, Some(&["Concept".to_string()])) {
                let lines: Vec<String> = hits
                    .iter()
                    .filter(|h| h.node_id != concept_id)
                    .take(2)
                    .map(|h| format!("- {}: {}", h.name.clone().unwrap_or_else(|| h.node_id.clone()), first_chars(&h.text, 200)))
                    .collect();
                if !lines.is_empty() {
                    rag = format!("\n\nRelated concepts from this graph (for extra context):\n{}", lines.join("\n"));
                }
            }
            let (system, user) = build_answer_question_prompt(concept, question, selected, &difficulty);
            let stream = deps.models.stream(&json!({
                "systemPrompt": system, "userPrompt": format!("{user}{rag}"), "provider": "deepseek", "model": "deepseek-v4-flash",
                "stream": true,
            }))?;
            let (text, _) = relay(call, stream)?;
            let answer = text.trim().to_string();
            if answer.is_empty() {
                return Err(KnowledgeError::service("The model returned an empty answer"));
            }
            let stored = record_answer(deps, &answer_uuid, call.uid, graph_id, concept_id, question, &answer, selected.unwrap_or_default())?;
            let related = find_related_concepts(deps, call.uid, graph_id, concept)?;
            Ok(json!({ "question": stored, "relatedConcepts": related }))
        }),
    );

    table.insert(
        "saveLesson",
        action(|call, p, deps| {
            let mut fields = graph_and_lesson();
            fields.push(params::id("content"));
            validate(p, &fields)?;
            let graph_id = params::str_of(p, "graphId");
            let lesson_id = params::str_of(p, "lessonId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            let lesson = lesson_of(&graph, lesson_id)?;
            let mut properties = lesson.properties.clone();
            properties.insert("content".into(), json!(params::str_of(p, "content")));
            let mut updates = Map::new();
            updates.insert("properties".into(), Value::Object(properties));
            deps.graphs
                .update_node(call.uid, graph_id, lesson_id, &updates)?;
            Ok(json!({ "saved": true }))
        }),
    );

    table.insert(
        "addNote",
        action(move |call, p, deps| {
            let mut fields = graph_and_lesson();
            fields.extend([params::id("text"), params::opt_text("selectedText")]);
            validate(p, &fields)?;
            let item = json!({
                "id": uuid(),
                "text": params::str_of(p, "text"),
                "selectedText": params::opt_str_of(p, "selectedText").unwrap_or_default(),
                "timestamp": number(deps.graphs.now_ms()),
            });
            mutate_lesson(
                deps,
                call.uid,
                params::str_of(p, "graphId"),
                params::str_of(p, "lessonId"),
                |lesson| {
                    let mut notes = items_of(lesson, "notes");
                    notes.push(item.clone());
                    Ok(Map::from_iter([("notes".to_string(), Value::Array(notes))]))
                },
            )?;
            Ok(json!({ "note": to_note(&item) }))
        }),
    );

    table.insert(
        "deleteNote",
        action(|call, p, deps| {
            let mut fields = graph_and_lesson();
            fields.push(params::id("noteId"));
            validate(p, &fields)?;
            let (lesson_id, note_id) = (params::str_of(p, "lessonId"), params::str_of(p, "noteId"));
            mutate_lesson(
                deps,
                call.uid,
                params::str_of(p, "graphId"),
                lesson_id,
                |lesson| {
                    let notes = items_of(lesson, "notes");
                    if !notes
                        .iter()
                        .any(|n| n.get("id").and_then(Value::as_str) == Some(note_id))
                    {
                        return Err(KnowledgeError::not_found(format!(
                            "Note {note_id} not found on lesson {lesson_id}"
                        )));
                    }
                    let kept: Vec<Value> = notes
                        .into_iter()
                        .filter(|n| n.get("id").and_then(Value::as_str) != Some(note_id))
                        .collect();
                    Ok(Map::from_iter([("notes".to_string(), Value::Array(kept))]))
                },
            )?;
            Ok(json!({ "deleted": true }))
        }),
    );

    table.insert(
        "deleteQuestion",
        action(|call, p, deps| {
            let mut fields = graph_and_lesson();
            fields.push(params::id("questionId"));
            validate(p, &fields)?;
            let (lesson_id, question_id) = (
                params::str_of(p, "lessonId"),
                params::str_of(p, "questionId"),
            );
            mutate_lesson(
                deps,
                call.uid,
                params::str_of(p, "graphId"),
                lesson_id,
                |lesson| {
                    let questions = items_of(lesson, "questions");
                    if !questions
                        .iter()
                        .any(|q| q.get("id").and_then(Value::as_str) == Some(question_id))
                    {
                        return Err(KnowledgeError::not_found(format!(
                            "Question {question_id} not found on lesson {lesson_id}"
                        )));
                    }
                    let kept: Vec<Value> = questions
                        .into_iter()
                        .filter(|q| q.get("id").and_then(Value::as_str) != Some(question_id))
                        .collect();
                    Ok(Map::from_iter([(
                        "questions".to_string(),
                        Value::Array(kept),
                    )]))
                },
            )?;
            Ok(json!({ "deleted": true }))
        }),
    );

    table.insert(
        "recordLessonProgress",
        action(|call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::id("conceptId"),
                    params::opt_text("activationResponse"),
                    params::opt_text("reflection"),
                    params::opt_id("bloomLevel"),
                    params::opt_flag("correct"),
                ],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let concept_id = params::str_of(p, "conceptId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            concept_of(&graph, concept_id)?;
            let existing = deps
                .students
                .get_progress_for_concept(call.uid, graph_id, call.uid, concept_id)?;
            let existing_str = |k: &str| {
                existing
                    .as_ref()
                    .and_then(|e| e.get(k))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let bloom_level = params::opt_str_of(p, "bloomLevel");
            let correct = params::opt_bool_of(p, "correct");

            let mut bloom_answered = existing
                .as_ref()
                .and_then(|e| e.get("bloomAnswered"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(level) = bloom_level {
                bloom_answered.insert(level.to_string(), Value::Bool(correct.unwrap_or(false)));
            }
            let mut completed: Vec<Value> = existing
                .as_ref()
                .and_then(|e| e.get("bloomLevelsCompleted"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let (Some(level), Some(true)) = (bloom_level, correct) {
                if !completed.iter().any(|c| c.as_str() == Some(level)) {
                    completed.push(json!(level));
                }
            }
            let reflection = params::opt_str_of(p, "reflection").filter(|r| !r.is_empty());
            let reflection_notes: Option<String> = match reflection {
                Some(r) => {
                    let parts: Vec<String> = [existing_str("reflectionNotes"), Some(r.to_string())]
                        .into_iter()
                        .flatten()
                        .filter(|s| !s.is_empty())
                        .collect();
                    Some(parts.join("\n"))
                }
                None => existing_str("reflectionNotes"),
            };
            let activation: Option<String> =
                match params::opt_str_of(p, "activationResponse").filter(|a| !a.is_empty()) {
                    Some(a) => Some(a.to_string()),
                    None => existing_str("activationResponse"),
                };
            let existing_level = existing
                .as_ref()
                .and_then(|e| e.get("masteryLevel"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let mastery = calculate_mastery_level(
                existing_level,
                activation.as_deref(),
                reflection_notes.as_deref(),
                bloom_answered.len(),
            );
            let mut data = Map::new();
            data.insert("masteryLevel".into(), number(mastery));
            data.insert("lastStudied".into(), number(deps.graphs.now_ms()));
            data.insert("bloomAnswered".into(), Value::Object(bloom_answered));
            data.insert("bloomLevelsCompleted".into(), Value::Array(completed));
            if let Some(a) = activation {
                data.insert("activationResponse".into(), json!(a));
            }
            if let Some(r) = reflection_notes {
                data.insert("reflectionNotes".into(), json!(r));
            }
            deps.students
                .upsert_progress(call.uid, graph_id, call.uid, concept_id, &data)?;
            Ok(json!({ "masteryLevel": number(mastery) }))
        }),
    );

    table.insert(
        "markLearned",
        action(|call, p, deps| {
            validate(
                p,
                &[
                    params::id("graphId"),
                    params::id("conceptId"),
                    params::flag("learned"),
                ],
            )?;
            let graph_id = params::str_of(p, "graphId");
            let concept_id = params::str_of(p, "conceptId");
            let graph = load_graph(deps, call.uid, graph_id)?;
            concept_of(&graph, concept_id)?;
            let mastery = if params::opt_bool_of(p, "learned") == Some(true) {
                3
            } else {
                0
            };
            let mut data = Map::new();
            data.insert("masteryLevel".into(), json!(mastery));
            data.insert("lastStudied".into(), number(deps.graphs.now_ms()));
            deps.students
                .upsert_progress(call.uid, graph_id, call.uid, concept_id, &data)?;
            Ok(json!({ "masteryLevel": mastery }))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::calculate_mastery_level;

    #[test]
    fn mastery_counts_signals_and_never_falls_from_two() {
        assert_eq!(calculate_mastery_level(0.0, None, None, 0), 0.0);
        assert_eq!(calculate_mastery_level(0.0, Some("x"), None, 0), 1.0);
        assert_eq!(calculate_mastery_level(0.0, Some("x"), Some("y"), 0), 2.0);
        assert_eq!(calculate_mastery_level(0.0, Some("x"), Some("y"), 3), 3.0);
        assert_eq!(calculate_mastery_level(2.0, None, None, 0), 2.0);
        assert_eq!(calculate_mastery_level(1.0, Some(""), None, 0), 0.0);
    }
}
