//! Proposals that complete a path (`operations/enrichment/*`): the four strategies' model requests, and what each
//! reply is allowed to become.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::model::schema as z;

pub const RESPONSE_FORMAT: &str = "Reply ONLY with JSON {\"missingConcepts\":[{\"name\": string, \"description\": string, \"reason\": string, \"layer\": integer}],\
\"missingRelationships\":[{\"from\": string, \"to\": string, \"type\": \"prerequisite\"|\"hasParent\"|\"referencesConcept\", \"reason\": string}]}. \
Both arrays are always present, empty when nothing is missing. \
A relationship is read \"from <type> to\": \"prerequisite\" means from requires to first; \"hasParent\" means from is part of to; \"referencesConcept\" means from builds on or is used together with to. \
from and to are concept ids exactly as listed, or the name of a concept you propose in missingConcepts (a new concept's id is its name). \
Every reason says why the addition helps the learner.";

/// The reply shape every strategy asks for (`enrichReplySchema`).
pub fn reply_shape() -> Value {
    z::object(
        vec![
            (
                "missingConcepts",
                z::array(
                    z::object(
                        vec![
                            ("name", z::string(true, false)),
                            ("description", z::string(false, false)),
                            ("reason", z::string(true, false)),
                            ("layer", z::integer()),
                        ],
                        &["layer"],
                    ),
                    None,
                    None,
                ),
            ),
            (
                "missingRelationships",
                z::array(
                    z::object(
                        vec![
                            ("from", z::string(true, false)),
                            ("to", z::string(true, false)),
                            (
                                "type",
                                z::one_of(&["prerequisite", "hasParent", "referencesConcept"]),
                            ),
                            ("reason", z::string(true, false)),
                        ],
                        &[],
                    ),
                    None,
                    None,
                ),
            ),
        ],
        &[],
    )
}

#[derive(Debug, Clone)]
pub struct EnrichConcept {
    pub id: String,
    pub name: String,
    pub description: String,
    pub layer: f64,
}

#[derive(Debug, Clone)]
pub struct EnrichEdge {
    pub from: String,
    pub to: String,
    /// `hasPrerequisite` | `hasParent` | `referencesConcept`
    pub edge_type: String,
}

#[derive(Debug, Clone)]
pub struct EnrichGoal {
    pub title: String,
    pub description: String,
    pub goal_type: Option<String>,
    pub target: Option<String>,
    pub assessed_level: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnrichLayer {
    pub layer_number: f64,
    pub goal: Option<String>,
    pub milestone: Option<String>,
    pub concepts: Vec<EnrichConcept>,
}

/// What the whole path already holds; proposals are checked against it.
pub struct GraphIndex {
    pub concept_ids: HashSet<String>,
    pub edge_keys: HashSet<String>,
    pub layer_numbers: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Draft {
    Concept {
        layer: f64,
        name: String,
        description: String,
        reason: String,
    },
    Prerequisite {
        from: String,
        to: String,
        reason: String,
    },
    Relation {
        from: String,
        to: String,
        relation: String,
        reason: String,
    },
}

pub struct Resolved {
    pub drafts: Vec<Draft>,
    pub dropped: Vec<String>,
}

pub struct ModelRequest {
    pub system_prompt: String,
    pub user_prompt: String,
}

pub fn edge_key(edge_type: &str, from: &str, to: &str) -> String {
    format!("{edge_type}|{from}|{to}")
}

fn js_number(n: f64) -> String {
    crate::jsnumber::js_number(n)
}

pub fn goal_lines(goal: &EnrichGoal) -> Vec<String> {
    let mut lines = vec![format!("Learning Goal: \"{}\"", goal.title)];
    if !goal.description.is_empty() {
        lines.push(format!("Goal Description: \"{}\"", goal.description));
    }
    if let Some(t) = &goal.goal_type {
        lines.push(format!("Goal Type: {t}"));
    }
    if let Some(t) = &goal.target {
        lines.push(format!("Target: {t}"));
    }
    if let Some(l) = &goal.assessed_level {
        lines.push(format!("Assessed Level: {l}"));
    }
    lines
}

pub fn concept_lines(concepts: &[EnrichConcept]) -> Vec<String> {
    if concepts.is_empty() {
        return vec!["None".to_string()];
    }
    concepts
        .iter()
        .map(|c| {
            let desc = if c.description.is_empty() {
                String::new()
            } else {
                format!(": {}", c.description)
            };
            format!(
                "- id: {} | {}{} | layer {}",
                c.id,
                c.name,
                desc,
                js_number(c.layer)
            )
        })
        .collect()
}

pub fn edge_lines(edges: &[EnrichEdge]) -> Vec<String> {
    if edges.is_empty() {
        return vec!["None".to_string()];
    }
    edges
        .iter()
        .map(|e| format!("{} -> {} ({})", e.from, e.to, e.edge_type))
        .collect()
}

fn edge_of(relation_type: &str) -> &'static str {
    match relation_type {
        "prerequisite" => "hasPrerequisite",
        "hasParent" => "hasParent",
        _ => "referencesConcept",
    }
}

struct ResolveRules<'a> {
    layer_of: &'a dyn Fn(&Value) -> Option<f64>,
    allowed_relations: &'a [&'a str],
}

/// Keeps the proposals the path can take; every other one is returned in `dropped` with the reason.
fn resolve_reply(reply: &Value, index: &GraphIndex, rules: &ResolveRules<'_>) -> Resolved {
    let mut drafts = Vec::new();
    let mut dropped = Vec::new();
    let mut proposed: HashSet<String> = HashSet::new();
    for c in reply
        .get("missingConcepts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = c
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let layer = (rules.layer_of)(c);
        if index.concept_ids.contains(&name) {
            dropped.push(format!("concept \"{name}\" already exists"));
        } else if proposed.contains(&name) {
            dropped.push(format!("concept \"{name}\" proposed twice"));
        } else if let Some(layer) = layer {
            proposed.insert(name.clone());
            drafts.push(Draft::Concept {
                layer,
                name,
                description: c
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                reason: c
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        } else {
            let shown = c
                .get("layer")
                .and_then(Value::as_f64)
                .map_or("none".to_string(), js_number);
            dropped.push(format!(
                "concept \"{name}\" names level {shown}, which the path does not have"
            ));
        }
    }
    let mut taken: HashSet<String> = HashSet::new();
    for r in reply
        .get("missingRelationships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |k: &str| {
            r.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (from, to, kind, reason) = (text("from"), text("to"), text("type"), text("reason"));
        let label = format!("{kind} {from} -> {to}");
        let key = edge_key(edge_of(&kind), &from, &to);
        let known = |id: &str| index.concept_ids.contains(id) || proposed.contains(id);
        if !rules.allowed_relations.contains(&kind.as_str()) {
            dropped.push(format!("{label}: this strategy does not propose {kind}"));
        } else if from == to {
            dropped.push(format!("{label}: a concept cannot relate to itself"));
        } else if !known(&from) || !known(&to) {
            dropped.push(format!("{label}: the path has no such concept"));
        } else if index.edge_keys.contains(&key) || taken.contains(&key) {
            dropped.push(format!("{label}: already in the path"));
        } else {
            taken.insert(key);
            drafts.push(if kind == "prerequisite" {
                Draft::Prerequisite { from, to, reason }
            } else {
                Draft::Relation {
                    from,
                    to,
                    relation: kind,
                    reason,
                }
            });
        }
    }
    Resolved { drafts, dropped }
}

const ALL_RELATIONS: [&str; 3] = ["prerequisite", "hasParent", "referencesConcept"];

pub fn missing_concepts_request(goal: &EnrichGoal, layer: &EnrichLayer) -> ModelRequest {
    let system = format!(
        "You are an expert learning path analyzer. Your task is to identify missing concepts that are essential for achieving a specific learning milestone.\n\n\
Key principles:\n\
- Consider what a learner would actually need to build/achieve the milestone\n\
- Identify missing concepts that are essential (not just nice-to-have)\n\
- Focus ONLY on missing concepts, NOT on relationships\n\
- Consider the learning goal context and target level\n\n\
{RESPONSE_FORMAT}\n\n\
IMPORTANT:\n\
- Return ONLY missingConcepts (missingRelationships must be empty)\n\
- All missing concepts belong to the same level as the milestone\n\
- Provide clear reasons why each concept is needed\n\
- Never repeat a concept already listed"
    );
    let mut user: Vec<String> = vec![
        "Identify missing concepts needed for this level to achieve the milestone.".into(),
        String::new(),
    ];
    if let Some(m) = &layer.milestone {
        user.push(format!("Milestone: \"{m}\""));
    }
    if let Some(g) = &layer.goal {
        user.push(format!("Level goal: \"{g}\""));
    }
    user.extend(goal_lines(goal));
    user.push(String::new());
    user.push(format!(
        "Concepts in level {}:",
        js_number(layer.layer_number)
    ));
    user.extend(concept_lines(&layer.concepts));
    user.extend(
        [
            "",
            "Task:",
            "1. Evaluate what concepts are needed to achieve this milestone",
            "2. Identify missing concepts that are essential",
            "3. Consider what a learner would actually need to build/achieve this milestone",
            "4. For each missing concept, explain why it is needed",
        ]
        .map(String::from),
    );
    ModelRequest {
        system_prompt: system,
        user_prompt: user.join("\n"),
    }
}

pub fn parse_missing_concepts(reply: &Value, index: &GraphIndex, layer: &EnrichLayer) -> Resolved {
    let number = layer.layer_number;
    resolve_reply(
        reply,
        index,
        &ResolveRules {
            layer_of: &move |_| Some(number),
            allowed_relations: &[],
        },
    )
}

pub fn prerequisites_request(
    goal: &EnrichGoal,
    concept: &EnrichConcept,
    others: &[EnrichConcept],
    edges: &[EnrichEdge],
) -> ModelRequest {
    let own: Vec<EnrichEdge> = edges
        .iter()
        .filter(|e| e.from == concept.id || e.to == concept.id)
        .cloned()
        .collect();
    let required: Vec<&str> = edges
        .iter()
        .filter(|e| e.edge_type == "hasPrerequisite" && e.from == concept.id)
        .map(|e| e.to.as_str())
        .collect();
    let system = format!(
        "You are an expert learning path analyzer. Your task is to analyze the complete prerequisite chain for a concept.\n\n\
Key principles:\n\
- Identify ALL prerequisites needed (both explicit and implicit)\n\
- Consider the learning goal context\n\
- Think about what a learner MUST know before attempting this concept\n\
- Include foundational concepts, tools, and related skills\n\
- Check if prerequisites exist in the graph\n\
- For prerequisites that do not exist, add them to missingConcepts\n\
- For prerequisites that exist but are not linked yet, add a \"prerequisite\" relationship from the concept to them\n\n\
{RESPONSE_FORMAT}\n\n\
IMPORTANT:\n\
- Only \"prerequisite\" relationships are accepted\n\
- Each concept you add to missingConcepts is linked as a prerequisite of the analyzed concept for you; do not repeat that link\n\
- Never repeat a concept or a relationship already listed"
    );
    let mut user: Vec<String> = vec![
        "Analyze the prerequisite chain for this concept:".into(),
        String::new(),
        format!(
            "Concept: id: {} | {} | layer {}",
            concept.id,
            concept.name,
            js_number(concept.layer)
        ),
    ];
    if !concept.description.is_empty() {
        user.push(format!("Description: \"{}\"", concept.description));
    }
    user.push(format!(
        "Current Prerequisites: {}",
        if required.is_empty() {
            "None".to_string()
        } else {
            required.join(", ")
        }
    ));
    user.push(String::new());
    user.extend(goal_lines(goal));
    user.push(String::new());
    user.push("Related Concepts in Graph:".into());
    user.extend(concept_lines(others));
    user.push(String::new());
    user.push("Existing Relationships:".into());
    user.extend(edge_lines(&own));
    user.extend(
        [
            "",
            "Task:",
            "1. Identify ALL prerequisites needed (both explicit and implicit)",
            "2. Consider the learning goal context",
            "3. Think about what a learner MUST know before attempting this concept",
            "4. Include foundational concepts, tools, and related skills",
            "5. Check if each prerequisite exists in the graph (use the related concepts list)",
            "6. Add missing prerequisites to missingConcepts",
            "7. Add prerequisite relationships for the ones that exist",
        ]
        .map(String::from),
    );
    ModelRequest {
        system_prompt: system,
        user_prompt: user.join("\n"),
    }
}

pub fn parse_prerequisites(reply: &Value, index: &GraphIndex, concept: &EnrichConcept) -> Resolved {
    let layer = concept.layer;
    let resolved = resolve_reply(
        reply,
        index,
        &ResolveRules {
            layer_of: &move |_| Some(layer),
            allowed_relations: &["prerequisite"],
        },
    );
    let linked: HashSet<String> = resolved
        .drafts
        .iter()
        .filter_map(|d| match d {
            Draft::Prerequisite { from, to, .. } => Some(edge_key("hasPrerequisite", from, to)),
            _ => None,
        })
        .collect();
    let links: Vec<Draft> = resolved
        .drafts
        .iter()
        .filter_map(|d| match d {
            Draft::Concept { name, .. }
                if !linked.contains(&edge_key("hasPrerequisite", &concept.id, name)) =>
            {
                Some(Draft::Prerequisite {
                    from: concept.id.clone(),
                    to: name.clone(),
                    reason: format!("Prerequisite for {}", concept.name),
                })
            }
            _ => None,
        })
        .collect();
    let concepts = resolved
        .drafts
        .iter()
        .filter(|d| matches!(d, Draft::Concept { .. }))
        .cloned();
    let rest = resolved
        .drafts
        .iter()
        .filter(|d| !matches!(d, Draft::Concept { .. }))
        .cloned();
    Resolved {
        drafts: concepts.chain(links).chain(rest).collect(),
        dropped: resolved.dropped,
    }
}

pub fn cross_layer_request(
    goal: &EnrichGoal,
    lower: &EnrichLayer,
    upper: &EnrichLayer,
) -> ModelRequest {
    let system = format!(
        "You are an expert learning path analyzer. Your task is to analyze concept progression across levels to identify gaps and bridging concepts.\n\n\
Key principles:\n\
- Analyze the progression from the lower level to the upper level\n\
- Identify concepts that should bridge these levels\n\
- Find missing foundational concepts\n\
- Suggest concepts that would improve the learning path\n\
- Consider the learning goal and milestone context\n\
- Identify missing relationships between the levels\n\n\
{RESPONSE_FORMAT}\n\n\
IMPORTANT:\n\
- Bridging concepts and missing foundations go in missingConcepts and join the upper level\n\
- Relationships between concepts go in missingRelationships\n\
- Never repeat a concept already listed"
    );
    let mut user: Vec<String> = vec![
        "Analyze concept progression across levels to identify gaps.".into(),
        String::new(),
    ];
    user.extend(goal_lines(goal));
    if let Some(m) = &upper.milestone {
        user.push(format!("Target Milestone: \"{m}\""));
    }
    if let Some(g) = &upper.goal {
        user.push(format!("Upper level goal: \"{g}\""));
    }
    user.push(String::new());
    user.push(format!("Level {} concepts:", js_number(lower.layer_number)));
    user.extend(concept_lines(&lower.concepts));
    user.push(String::new());
    user.push(format!("Level {} concepts:", js_number(upper.layer_number)));
    user.extend(concept_lines(&upper.concepts));
    user.push(String::new());
    user.push("Task:".into());
    user.push(format!(
        "1. Analyze the progression from level {} to level {}",
        js_number(lower.layer_number),
        js_number(upper.layer_number)
    ));
    user.extend(
        [
            "2. Identify concepts that should bridge these levels",
            "3. Find missing foundational concepts",
            "4. Suggest concepts that would improve the learning path",
            "5. Consider what learners need to transition smoothly between levels",
        ]
        .map(String::from),
    );
    ModelRequest {
        system_prompt: system,
        user_prompt: user.join("\n"),
    }
}

pub fn parse_cross_layer(reply: &Value, index: &GraphIndex, upper: &EnrichLayer) -> Resolved {
    let number = upper.layer_number;
    resolve_reply(
        reply,
        index,
        &ResolveRules {
            layer_of: &move |_| Some(number),
            allowed_relations: &ALL_RELATIONS,
        },
    )
}

pub fn goal_aware_request(
    goal: &EnrichGoal,
    concepts: &[EnrichConcept],
    edges: &[EnrichEdge],
) -> ModelRequest {
    let system = format!(
        "You are an expert learning path analyzer. Your task is to analyze relationships between concepts in the context of a learning goal.\n\n\
Key principles:\n\
- Identify relationships between concepts that are relevant to the goal\n\
- Consider how concepts work together in real-world scenarios\n\
- Identify complementary concepts (used together): \"referencesConcept\"\n\
- Identify sequential relationships (one must come first): \"prerequisite\"\n\
- Identify hierarchical relationships (one is part of another): \"hasParent\"\n\
- Focus on relationships that help achieve the learning goal\n\
- Identify any missing concepts that should connect existing concepts\n\n\
{RESPONSE_FORMAT}\n\n\
IMPORTANT:\n\
- Each missing concept names the level it joins in \"layer\", one of the levels listed\n\
- Never repeat a concept or a relationship already listed"
    );
    let mut user: Vec<String> = vec![
        "Analyze relationships between concepts in the context of a learning goal.".into(),
        String::new(),
    ];
    user.extend(goal_lines(goal));
    user.push(String::new());
    user.push("Current Concepts:".into());
    user.extend(concept_lines(concepts));
    user.push(String::new());
    user.push("Existing Relationships:".into());
    user.extend(edge_lines(edges));
    user.extend(
        [
            "",
            "Task:",
            "1. Identify relationships between concepts that are relevant to the goal",
            "2. Consider how concepts work together in real-world scenarios",
            "3. Identify complementary, sequential and hierarchical relationships",
            "4. Focus on relationships that help achieve the learning goal",
        ]
        .map(String::from),
    );
    ModelRequest {
        system_prompt: system,
        user_prompt: user.join("\n"),
    }
}

pub fn parse_goal_aware(reply: &Value, index: &GraphIndex, levels: &[f64]) -> Resolved {
    let levels: Vec<f64> = levels.to_vec();
    resolve_reply(
        reply,
        index,
        &ResolveRules {
            layer_of: &move |c| {
                c.get("layer")
                    .and_then(Value::as_f64)
                    .filter(|l| levels.contains(l))
            },
            allowed_relations: &ALL_RELATIONS,
        },
    )
}

pub fn request_json(request: &ModelRequest, model: &str) -> Value {
    json!({
        "systemPrompt": request.system_prompt, "userPrompt": request.user_prompt, "provider": "deepseek",
        "model": model, "temperature": 0.3, "schema": reply_shape(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> GraphIndex {
        GraphIndex {
            concept_ids: ["A".to_string(), "B".to_string()].into_iter().collect(),
            edge_keys: [edge_key("hasPrerequisite", "A", "B")]
                .into_iter()
                .collect(),
            layer_numbers: vec![1.0],
        }
    }

    #[test]
    fn existing_duplicate_and_unplaceable_proposals_are_dropped_with_a_reason() {
        let reply = json!({
            "missingConcepts": [
                { "name": "A", "description": "", "reason": "r" },
                { "name": "C", "description": "d", "reason": "r" },
                { "name": "C", "description": "d", "reason": "r" },
                { "name": "D", "description": "d", "reason": "r", "layer": 7 }
            ],
            "missingRelationships": [
                { "from": "A", "to": "B", "type": "prerequisite", "reason": "r" },
                { "from": "A", "to": "C", "type": "hasParent", "reason": "r" },
                { "from": "A", "to": "A", "type": "hasParent", "reason": "r" },
                { "from": "A", "to": "Z", "type": "hasParent", "reason": "r" }
            ]
        });
        let layers = [1.0];
        let resolved = parse_goal_aware(&reply, &index(), &layers);
        assert!(resolved.drafts.is_empty() || resolved.drafts.len() == 2);
        assert_eq!(resolved.dropped[0], "concept \"A\" already exists");
        assert!(resolved
            .dropped
            .contains(&"concept \"D\" names level 7, which the path does not have".to_string()));
        assert!(resolved
            .dropped
            .contains(&"prerequisite A -> B: already in the path".to_string()));
        assert!(resolved
            .dropped
            .contains(&"hasParent A -> A: a concept cannot relate to itself".to_string()));
        assert!(resolved
            .dropped
            .contains(&"hasParent A -> Z: the path has no such concept".to_string()));
    }

    #[test]
    fn a_proposed_prerequisite_concept_is_linked_to_the_analysed_one() {
        let concept = EnrichConcept {
            id: "A".into(),
            name: "A".into(),
            description: String::new(),
            layer: 1.0,
        };
        let reply = json!({ "missingConcepts": [{ "name": "N", "description": "d", "reason": "r" }], "missingRelationships": [] });
        let resolved = parse_prerequisites(&reply, &index(), &concept);
        assert_eq!(resolved.drafts.len(), 2);
        assert!(
            matches!(&resolved.drafts[1], Draft::Prerequisite { from, to, .. } if from == "A" && to == "N")
        );
    }
}
