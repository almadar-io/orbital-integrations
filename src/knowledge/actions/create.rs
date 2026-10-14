//! Creating a learning goal and its path from what the learner wants to learn (`create.ts`).

use regex::Regex;
use serde_json::{json, Map, Value};

use super::common::make_node;
use super::goal::infer_difficulty;
use super::model::{check_shape, parse_json_response, schema as z};
use super::params::{self, validate};
use super::{action, Env};
use crate::json::number;
use crate::knowledge::engine::pure::NodeBasedKnowledgeGraph;
use crate::knowledge::firestore::forward_relationship;
use crate::knowledge::store::{KnowledgeError, StoreResult};
use crate::knowledge::{ActionTable, KnowledgeProgress};

pub const GOAL_PROVIDER: &str = "deepseek";
pub const GOAL_MODEL: &str = "deepseek-v4-flash";

const GOAL_SYSTEM_PROMPT: &str = r####"You are an expert learning advisor that helps users create structured, actionable learning goals.

Your task is to analyze a user's learning aspiration and generate a comprehensive learning goal.

Key principles:
- Create a clear, specific, and achievable learning goal
- The "title" MUST be the bare canonical subject name only (e.g. "Haskell", "Linear Algebra", "Organic Chemistry", "React"). NEVER add ANY prefix or suffix: not "Master", "Mastering", "Core", "Essential", "Fundamentals", "Learning", "Understanding", "Introduction to", "A guide to", "The Complete", or any similar word. Output the subject name and nothing else.
- Infer the most appropriate goal type (common types: "certification", "skill_mastery", "language_level", "project_completion", or suggest a custom type if it fits better)
- Make the goal actionable and measurable
- "customMetadata" is flat: each value is a string, number, boolean or null (e.g. {"difficulty": "beginner", "focus": "conversation"})
- "keywords" are domain search terms a learner might use that are not the subject name itself (e.g. ["JVM","runtime","bytecode"])

Output format:
Return a JSON object with this structure:
{
  "title": "Bare canonical subject name only",
  "description": "Detailed description of the learning goal",
  "type": "goal_type_here",
  "target": "Specific target (e.g., 'AWS Certified Solutions Architect', 'B1 Spanish', 'Publish research paper')",
  "estimatedTime": 120,
  "keywords": ["term"],
  "milestones": [
    { "id": "m1", "title": "Milestone title", "description": "Milestone description", "targetDate": null, "completed": false }
  ],
  "customMetadata": {}
}

Return ONLY the JSON object, no additional text."####;

pub fn goal_user_prompt(anchor_answer: &str) -> String {
    format!(
        "The user answered the anchor question \"What's something you've always wanted to learn?\" with:\n\n\"{anchor_answer}\"\n\nGenerate a comprehensive learning goal based on this information.\n\nReturn the JSON object with the goal structure."
    )
}

fn milestone_shape() -> Value {
    z::object(
        vec![
            ("id", z::string(false, false)),
            ("title", z::string(false, false)),
            ("description", z::string(false, false)),
            ("targetDate", z::nullable(z::number())),
            ("completed", z::boolean()),
        ],
        &["id", "title", "description", "targetDate", "completed"],
    )
}

fn goal_draft_shape() -> Value {
    z::object(
        vec![
            ("title", z::string(true, false)),
            ("description", z::string(true, false)),
            ("type", z::string(true, false)),
            ("target", z::string(true, false)),
            ("estimatedTime", z::nullable(z::number())),
            ("milestones", z::array(milestone_shape(), None, None)),
            ("keywords", z::array(z::string(false, false), None, None)),
            ("customMetadata", json!({ "t": "record" })),
        ],
        &["estimatedTime", "milestones", "keywords", "customMetadata"],
    )
}

fn partial_goal_shape() -> Value {
    z::object(
        vec![
            ("title", z::string(true, false)),
            ("description", z::string(true, false)),
            ("milestones", z::array(milestone_shape(), None, None)),
        ],
        &["title", "description", "milestones"],
    )
}

/// The model's goal JSON as a draft: objects inside `customMetadata` are kept as their JSON text.
pub fn parse_goal_draft(content: &str) -> StoreResult<Value> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(KnowledgeError::service(
            "Failed to generate goal: empty response from the model",
        ));
    }
    let parsed: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            let start = trimmed.find('{');
            let end = trimmed.rfind('}');
            match (start, end) {
                (Some(s), Some(e)) if e > s => {
                    serde_json::from_str(&trimmed[s..=e]).map_err(|err| {
                        KnowledgeError::service(format!(
                            "Failed to parse the model response as JSON: {err}"
                        ))
                    })?
                }
                _ => {
                    return Err(KnowledgeError::service(
                        "Failed to parse the model response as JSON",
                    ))
                }
            }
        }
    };
    let mut checked = check_shape(&parsed, &goal_draft_shape()).map_err(|issues| {
        KnowledgeError::service(format!(
            "Model response is not a valid goal: {}",
            issues.join("; ")
        ))
    })?;
    if let Some(map) = checked.as_object_mut() {
        map.entry("milestones".to_string())
            .or_insert_with(|| json!([]));
        if let Some(meta) = parsed.get("customMetadata").and_then(Value::as_object) {
            let flat: Map<String, Value> = meta
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if v.is_object() || v.is_array() {
                            Value::String(v.to_string())
                        } else {
                            v.clone()
                        },
                    )
                })
                .collect();
            map.insert("customMetadata".into(), Value::Object(flat));
        }
    }
    Ok(checked)
}

/// `JSON.stringify` of the draft's milestones with the defaults a stored goal carries, keys in the TS order.
fn milestones_json(draft: &Value) -> String {
    let mut texts = Vec::new();
    for (index, m) in draft
        .get("milestones")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let scalar = |v: &Value| serde_json::to_string(v).unwrap_or_else(|_| "null".to_string());
        let id = m
            .get("id")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!(format!("m{}", index + 1)));
        let title = m
            .get("title")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!("Milestone"));
        let target = m
            .get("targetDate")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null);
        let completed = m
            .get("completed")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(json!(false));
        let mut parts = vec![
            format!("\"id\":{}", scalar(&id)),
            format!("\"title\":{}", scalar(&title)),
        ];
        if let Some(d) = m.get("description").filter(|v| !v.is_null()) {
            parts.push(format!("\"description\":{}", scalar(d)));
        }
        parts.push(format!("\"targetDate\":{}", scalar(&target)));
        parts.push(format!("\"completed\":{}", scalar(&completed)));
        texts.push(format!("{{{}}}", parts.join(",")));
    }
    format!("[{}]", texts.join(","))
}

/// The node-based graph for a new goal: Graph, seed Concept named after the anchor, and the LearningGoal.
pub fn build_goal_graph(
    graph_id: &str,
    goal_id: &str,
    anchor_answer: &str,
    draft: &Value,
    now: f64,
) -> StoreResult<NodeBasedKnowledgeGraph> {
    let seed_name = anchor_answer.trim().to_string();
    let seed_id = seed_name.clone();
    let text = |k: &str| {
        draft
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let goal_text = format!("{}: {}", text("title"), text("description"));
    let milestones = milestones_json(draft);

    let mut custom = Map::new();
    custom.insert("graphId".into(), json!(graph_id));
    custom.insert("milestones".into(), json!(milestones));
    custom.insert("shortTermGoals".into(), json!("[]"));
    if let Some(extra) = draft.get("customMetadata").and_then(Value::as_object) {
        for (k, v) in extra {
            custom.insert(k.clone(), v.clone());
        }
    }
    let mut goal = Map::new();
    goal.insert("id".into(), json!(goal_id));
    goal.insert("name".into(), json!(text("title")));
    goal.insert("description".into(), json!(text("description")));
    goal.insert("type".into(), json!(text("type")));
    goal.insert("target".into(), json!(text("target")));
    if let Some(t) = draft
        .get("estimatedTime")
        .and_then(Value::as_f64)
        .filter(|t| *t != 0.0)
    {
        goal.insert("estimatedTime".into(), number(t));
    }
    if let Some(k) = draft.get("keywords").filter(|v| !v.is_null()) {
        goal.insert("keywords".into(), k.clone());
    }
    goal.insert("customMetadata".into(), Value::Object(custom));
    goal.insert("createdAt".into(), number(now));
    goal.insert("updatedAt".into(), number(now));

    let mut node_types = Map::new();
    for kind in super::import_sink::NODE_TYPE_INDEX {
        node_types.insert(kind.to_string(), json!([]));
    }
    node_types.insert("Graph".into(), json!([graph_id]));
    node_types.insert("Concept".into(), json!([seed_id]));
    node_types.insert("LearningGoal".into(), json!([goal_id]));
    let nodes = [
        make_node(
            graph_id,
            "Graph",
            json!({ "id": graph_id, "seedConceptId": seed_id, "createdAt": number(now), "updatedAt": number(now) }),
            now,
        ),
        make_node(
            &seed_id,
            "Concept",
            json!({ "id": seed_id, "name": seed_name, "description": goal_text, "goal": goal_text, "isSeed": true }),
            now,
        ),
        make_node(goal_id, "LearningGoal", Value::Object(goal), now),
    ];
    let mut node_map = Map::new();
    for n in nodes {
        node_map.insert(
            n.id.clone(),
            serde_json::to_value(&n).unwrap_or(Value::Null),
        );
    }
    serde_json::from_value(json!({
        "id": graph_id, "seedConceptId": seed_id, "createdAt": number(now), "updatedAt": number(now), "version": 1,
        "name": seed_name, "nodes": node_map, "nodeTypes": node_types,
        "relationships": [
            forward_relationship(graph_id, &seed_id, "containsConcept", now),
            forward_relationship(graph_id, &seed_id, "hasSeedConcept", now),
            forward_relationship(graph_id, goal_id, "hasLearningGoal", now),
        ],
    }))
    .map_err(|e| KnowledgeError::other(format!("the goal graph is invalid: {e}")))
}

/// `autoCloseJson`: close a string and the brackets a streamed JSON text left open.
pub fn auto_close_json(json: &str) -> String {
    let mut result = json.trim().to_string();
    let (mut in_string, mut escaped) = (false, false);
    for c in result.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_string = !in_string;
        }
    }
    if in_string {
        result.push('"');
    }
    if let Ok(re) = Regex::new(r",\s*$") {
        result = re.replace(&result, "").into_owned();
    }
    if let Ok(re) = Regex::new(r":\s*$") {
        result = re.replace(&result, ": null").into_owned();
    }
    let mut stack: Vec<char> = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    for c in result.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && in_string {
            escaped = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match c {
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            ']' | '}' => {
                if stack.last() == Some(&c) {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
    result.extend(stack.iter().rev());
    result
}

/// Reads the goal out of its streamed JSON as it grows: `title` and `description` whenever they change, each
/// milestone once the next one has started (`finish` reports the rest).
type Reporter<'a> = Box<dyn FnMut(&str, &str) + 'a>;

pub struct GoalReader<'a> {
    report: Reporter<'a>,
    title: String,
    description: String,
    milestones: usize,
}

impl<'a> GoalReader<'a> {
    pub fn new(report: impl FnMut(&str, &str) + 'a) -> Self {
        Self {
            report: Box::new(report),
            title: String::new(),
            description: String::new(),
            milestones: 0,
        }
    }

    fn fields(&mut self, title: Option<&str>, description: Option<&str>) {
        if let Some(t) = title.filter(|t| *t != self.title) {
            self.title = t.to_string();
            (self.report)("title", t);
        }
        if let Some(d) = description.filter(|d| *d != self.description) {
            self.description = d.to_string();
            (self.report)("description", d);
        }
    }

    fn milestone(&mut self, titles: &[Option<String>], up_to: usize) {
        while self.milestones < up_to {
            if let Some(Some(text)) = titles.get(self.milestones) {
                if !text.is_empty() {
                    (self.report)("milestone", text);
                }
            }
            self.milestones += 1;
        }
    }

    pub fn feed(&mut self, content: &str) {
        let Some(start) = content.find('{') else {
            return;
        };
        let closed = auto_close_json(&content[start..]);
        let Ok(parsed) = parse_json_response(&closed, Some(&partial_goal_shape())) else {
            return;
        };
        self.fields(
            parsed.get("title").and_then(Value::as_str),
            parsed.get("description").and_then(Value::as_str),
        );
        let titles: Vec<Option<String>> = parsed
            .get("milestones")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|m| m.get("title").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.milestone(&titles, titles.len().saturating_sub(1));
    }

    pub fn finish(&mut self, draft: &Value) {
        self.fields(
            draft.get("title").and_then(Value::as_str),
            draft.get("description").and_then(Value::as_str),
        );
        let titles: Vec<Option<String>> = draft
            .get("milestones")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|m| m.get("title").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.milestone(&titles, titles.len());
    }
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let uuid = env.uuid.clone();
    table.insert(
        "createGoalWithGraph",
        action(move |call, p, deps| {
            validate(p, &[params::text("anchorAnswer")])?;
            let anchor = params::str_of(p, "anchorAnswer").trim().to_string();
            if anchor.is_empty() {
                return Err(KnowledgeError::validation(
                    "anchorAnswer is required and must be a non-empty string",
                ));
            }
            let step_model = std::cell::RefCell::new(GOAL_MODEL.to_string());
            let mut reader = GoalReader::new(|field, text| {
                let mut message =
                    KnowledgeProgress::new("step", text, step_model.borrow().as_str());
                message.field = Some(field.to_string());
                call.progress(message);
            });
            let content = {
                let streamed = deps.models.stream(&json!({
                    "systemPrompt": GOAL_SYSTEM_PROMPT,
                    "userPrompt": goal_user_prompt(&anchor),
                    "provider": GOAL_PROVIDER,
                    "model": GOAL_MODEL,
                    "stream": true,
                }))?;
                let model = streamed.model.clone();
                *step_model.borrow_mut() = model.clone();
                call.progress(KnowledgeProgress::new("start", "", &model));
                let mut content = String::new();
                let mut seen = String::new();
                for chunk in streamed.chunks {
                    call.throw_if_cancelled()?;
                    let chunk = chunk?;
                    if !chunk.content.is_empty() {
                        content.push_str(&chunk.content);
                        call.progress(KnowledgeProgress::new(
                            "delta",
                            chunk.content.clone(),
                            &model,
                        ));
                        seen.push_str(&chunk.content);
                        reader.feed(&seen);
                    }
                    if chunk.done {
                        break;
                    }
                }
                call.throw_if_cancelled()?;
                content
            };
            let draft = parse_goal_draft(&content)?;
            reader.finish(&draft);
            let graph_id = uuid();
            let goal_id = uuid();
            let graph =
                build_goal_graph(&graph_id, &goal_id, &anchor, &draft, deps.graphs.now_ms())?;
            call.throw_if_cancelled()?;
            deps.graphs.save_graph(call.uid, &graph, None)?;
            let milestone_titles: Vec<Value> = draft
                .get("milestones")
                .and_then(Value::as_array)
                .map(|ms| {
                    ms.iter()
                        .filter_map(|m| {
                            m.get("title")
                                .and_then(Value::as_str)
                                .filter(|t| !t.is_empty())
                                .map(|t| json!(t))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({
                "graphId": graph_id,
                "goalTitle": draft.get("title").cloned().unwrap_or(Value::Null),
                "description": draft.get("description").cloned().unwrap_or(Value::Null),
                "milestones": milestone_titles,
                "difficulty": infer_difficulty(&graph),
            }))
        }),
    );
}
