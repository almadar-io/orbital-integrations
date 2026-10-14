//! The next level of a path: its prompt (`expansionPromptBuilder.ts`), the mutations a model's narrative becomes
//! (`expansionMutations.ts`) and what the stream reads out of it as it is written.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{json, Map, Value};

use super::common::{make_node, num_order, num_prop, str_prop};
use super::concept_stream::ConceptScanner;
use super::goal::LearningGoal;
use super::prompt_builder::{Channel, PromptBuilder};
use crate::json::number;
use crate::knowledge::engine::mutations::{GraphMutation, MutationBatch, MutationMetadata};
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use crate::knowledge::firestore::literal_relationship;
use crate::knowledge::store::{KnowledgeError, StoreResult};

pub struct TargetMilestone {
    pub index: f64,
    pub title: String,
    pub description: Option<String>,
    pub node_id: Option<String>,
}

pub struct ExpansionPromptContext<'a> {
    pub seed_concept: &'a GraphNode,
    pub previous_layers: Vec<&'a GraphNode>,
    pub existing_layers: Vec<&'a GraphNode>,
    pub num_concepts: usize,
    pub learning_goal: Option<&'a LearningGoal>,
    pub difficulty: String,
    pub focus: Option<String>,
    pub is_first_layer: bool,
    pub target_milestone: Option<&'a TargetMilestone>,
    pub completed_milestones_count: usize,
    pub total_milestones: usize,
}

pub fn build_expansion_system_prompt() -> String {
    PromptBuilder::new()
        .section("Role", "You are a learning architect who designs progressive learning paths. You create structured, goal-oriented educational content that builds knowledge incrementally.", Channel::System)
        .section("Output Format", "You must return your response in a specific format. Every field is MANDATORY — empty tags are not allowed.\n\nFor each concept, output this exact sequence of tags:\n<concept>CONCEPT NAME</concept>\n<description>A full sentence explaining what this concept is and why it matters. Must be at least 15 characters — never empty.</description>\n<parents>PARENT1, PARENT2</parents>\n\nAlso include:\n- <level-name>NAME</level-name> for the layer name\n- <goal>GOAL TEXT</goal> for the layer goal\n\nExample:\n<concept>Heat Transfer</concept>\n<description>The movement of thermal energy from a hotter object to a cooler one via conduction, convection, or radiation.</description>\n<parents>Thermodynamics, Temperature</parents>", Channel::System)
        .build_system()
}

fn js_number(n: f64) -> String {
    crate::jsnumber::js_number(n)
}

fn milestone_list(goal: &LearningGoal) -> String {
    goal.milestones
        .iter()
        .flatten()
        .enumerate()
        .map(|(i, m)| {
            format!(
                "{}. {}{}",
                i + 1,
                m.title,
                if m.completed { " ✓" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn difficulty_section(builder: PromptBuilder, difficulty: &str) -> PromptBuilder {
    match difficulty {
        "beginner" => builder.user_section("Difficulty Level", "Focus on absolute basics and fundamental principles. Use simple language and avoid advanced terminology. Prioritize concepts that build a strong foundation before moving to more complex topics."),
        "intermediate" => builder.user_section("Difficulty Level", "Assume the learner has some basic knowledge. Focus on concepts that bridge basic understanding to more advanced topics."),
        "advanced" => builder.user_section("Difficulty Level", "Assume substantial prior knowledge. Include sophisticated concepts that prepare for expert-level understanding."),
        _ => builder,
    }
}

fn milestone_goal_requirement(m: &TargetMilestone) -> String {
    let n = js_number(m.index + 1.0);
    let mut text = format!(
        "\n\n**CRITICAL MILESTONE GOAL REQUIREMENT:**\n- This layer's goal **MUST be prefixed** with \"Milestone {n}: \" in the <goal> tag\n- **Example format**: `<goal>Milestone {n}: [your project goal that advances toward \"{}\"]</goal>`\n- The goal should directly contribute to achieving \"{}\"",
        m.title, m.title
    );
    if let Some(d) = m.description.as_deref().filter(|d| !d.is_empty()) {
        text.push_str(&format!("\n- **Milestone Context**: {d}"));
    }
    text
}

pub fn build_first_layer_prompt(c: &ExpansionPromptContext<'_>) -> String {
    let concept_name = str_prop(c.seed_concept, "name");
    let concept_description = str_prop(c.seed_concept, "description");
    let mut builder = PromptBuilder::new().with_context(&[
        ("conceptName", concept_name.clone()),
        ("conceptDescription", concept_description),
        ("numConcepts", c.num_concepts.to_string()),
        ("difficulty", c.difficulty.clone()),
        ("focus", c.focus.clone().unwrap_or_default()),
    ]);
    builder = builder.user_section("Topic", "You are introducing a new learning path for: **{{conceptName}}**\n\n{{conceptDescription}}");

    if let Some(goal) = c.learning_goal {
        let mut parts: Vec<String> = vec![
            format!("**Goal Title**: {}", goal.title),
            format!("**Goal Description**: {}", goal.description),
            format!("**Goal Type**: {}", goal.goal_type),
        ];
        if !goal.target.is_empty() {
            parts.push(format!("**Target**: {}", goal.target));
        }
        if let Some(t) = goal.estimated_time.filter(|t| *t != 0.0) {
            parts.push(format!("**Estimated Time**: {} hours", js_number(t)));
        }
        if goal.milestones.as_ref().is_some_and(|m| !m.is_empty()) {
            let total = goal.milestones.as_ref().map_or(0, Vec::len);
            let total_count = if c.total_milestones > 0 || total == 0 {
                c.total_milestones
            } else {
                total
            };
            parts.push(format!("\n**Total Milestones**: {total_count}"));
            parts.push(format!(
                "**Completed Milestones**: {}/{total_count}",
                c.completed_milestones_count
            ));
            if let Some(m) = c.target_milestone {
                parts.push(format!(
                    "\n**Current Target Milestone**: Milestone {}: {}",
                    js_number(m.index + 1.0),
                    m.title
                ));
                if let Some(d) = m.description.as_deref().filter(|d| !d.is_empty()) {
                    parts.push(format!("**Milestone Description**: {d}"));
                }
            }
            parts.push(format!("**All Milestones**: {}", milestone_list(goal)));
        }
        builder = builder.user_section("Learning Goal", &parts.join("\n"));
    }

    let mut task = String::from("Create a simple project appropriate for {{conceptName}} at the {{difficulty}} level.\n\n**Requirements:**\n1. Suggest a name for this first layer (e.g., \"Level 1\", \"Foundation\", \"Module 1\") in a <level-name> tag\n2. Define a project goal in a <goal> tag that advances toward the learning goal");
    if let Some(m) = c.target_milestone {
        task.push_str(&milestone_goal_requirement(m));
    }
    task.push_str(&format!(
        "\n\n3. Generate a narrative introducing {{{{conceptName}}}} and describing approximately {{{{numConcepts}}}} foundational concepts\n4. Tag each concept with <concept>CONCEPT NAME</concept>\n5. Include <description>...</description> for each concept\n6. Include <parents>...</parents> listing prerequisites for each concept\n\n**IMPORTANT**: This project should be the first step toward achieving the learning goal: \"{}\"",
        c.learning_goal.map_or("the stated learning objective", |g| g.title.as_str())
    ));
    builder = builder.user_section("Task", &task);
    builder = difficulty_section(builder, &c.difficulty);
    if let Some(focus) = c.focus.as_deref().filter(|f| !f.is_empty()) {
        builder = builder.user_section("Learning Focus", &format!("**CRITICAL**: All concepts must align with and support the learning focus: \"{focus}\". Do not introduce concepts that stray from this focus."));
    }
    let mut rules: Vec<String> = vec![
        "Each concept must be tagged exactly once with <concept>CONCEPT NAME</concept>".into(),
        "CRITICAL: Every <description> tag MUST contain a meaningful sentence (at least 15 characters). Empty descriptions are not allowed.".into(),
        "Each concept MUST be immediately followed by its <description> and <parents> tags in that order".into(),
        "Use simple, single-concept names (e.g., \"arrays\", \"functions\")".into(),
        "Avoid long descriptive names, parentheses, or special characters".into(),
        "Parents must reference either the seed concept (\"{{conceptName}}\") or concepts introduced earlier in THIS layer. Copy the name EXACTLY.".into(),
        "Use Title Case for concept names".into(),
        "Do NOT repeat concepts from previous layers".into(),
    ];
    if let Some(focus) = c.focus.as_deref().filter(|f| !f.is_empty()) {
        rules.push(format!(
            "All concepts must align with the learning focus \"{focus}\""
        ));
    }
    rules.push("Return only the narrative text; no JSON".into());
    builder.rules(&rules).build_user()
}

pub fn build_subsequent_layer_prompt(c: &ExpansionPromptContext<'_>) -> String {
    let concept_name = str_prop(c.seed_concept, "name");
    let concept_nodes: Vec<&GraphNode> = c
        .previous_layers
        .iter()
        .copied()
        .filter(|n| n.node_type == "Concept")
        .collect();
    let total_previous = concept_nodes.len();
    let mut by_layer: Vec<(f64, Vec<String>)> = Vec::new();
    for n in &concept_nodes {
        let layer = num_prop(n, "layer").unwrap_or(0.0);
        let name = str_prop(n, "name");
        match by_layer.iter_mut().find(|(l, _)| *l == layer) {
            Some((_, names)) => names.push(name),
            None => by_layer.push((layer, vec![name])),
        }
    }
    let mut layers_desc: Vec<f64> = by_layer.iter().map(|(l, _)| *l).collect();
    layers_desc.sort_by(|a, b| num_order(*b, *a));
    let line = |l: f64| {
        let names = by_layer
            .iter()
            .find(|(k, _)| *k == l)
            .map(|(_, n)| n.join(", "))
            .unwrap_or_default();
        format!("- Level {}: {names}", js_number(l))
    };
    const RECENT_FULL_LAYERS: usize = 3;
    let recent: Vec<String> = layers_desc
        .iter()
        .take(RECENT_FULL_LAYERS)
        .map(|l| line(*l))
        .collect();
    let older: Vec<String> = layers_desc
        .iter()
        .skip(RECENT_FULL_LAYERS)
        .map(|l| line(*l))
        .collect();
    let previous_concepts_block = if total_previous > 0 {
        recent
            .into_iter()
            .chain(older)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        "- (none yet)".to_string()
    };
    let mut valid_parent_names: Vec<String> =
        concept_nodes.iter().map(|n| str_prop(n, "name")).collect();
    valid_parent_names.sort();

    let layer_nodes: Vec<&GraphNode> = if c.existing_layers.is_empty() {
        c.previous_layers.clone()
    } else {
        c.existing_layers.clone()
    };
    let previous_layer_goals: Vec<(Option<String>, String)> = layer_nodes
        .iter()
        .filter(|n| n.node_type == "Layer")
        .filter_map(|n| {
            let goal = n
                .properties
                .get("goal")
                .and_then(Value::as_str)
                .filter(|g| !g.is_empty())?;
            Some((
                n.properties
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                goal.to_string(),
            ))
        })
        .collect();
    let last_layer_goal = previous_layer_goals.last().map(|(_, g)| g.clone());

    let mut builder = PromptBuilder::new().with_context(&[
        ("conceptName", concept_name),
        ("numConcepts", c.num_concepts.to_string()),
        ("difficulty", c.difficulty.clone()),
    ]);

    let mut context = format!("You are expanding the learning path for **{{{{conceptName}}}}**.\n\n**Previous Concepts by level (most recent first):**\n{previous_concepts_block}");
    if !valid_parent_names.is_empty() {
        context.push_str(&format!("\n\n**Valid Parent Names** (copy these EXACTLY into <parents> tags — do NOT invent, paraphrase, or modify any name):\n{}", valid_parent_names.join(", ")));
    }
    if !previous_layer_goals.is_empty() {
        context.push_str("\n\n**Previous Layers:**");
        for (idx, (name, goal)) in previous_layer_goals.iter().enumerate() {
            context.push_str(&format!(
                "\n- {}: {goal}",
                name.clone().unwrap_or_else(|| format!("Layer {}", idx + 1))
            ));
        }
    }
    builder = builder.user_section("Context", &context);

    if let Some(goal) = c.learning_goal {
        let target = if goal.target.is_empty() {
            goal.description.clone()
        } else {
            goal.target.clone()
        };
        let mut parts: Vec<String> = vec![
            format!("**Long-term Goal**: {} - {target}", goal.title),
            format!("**Goal Type**: {}", goal.goal_type),
        ];
        if let Some(milestones) = goal.milestones.as_ref().filter(|m| !m.is_empty()) {
            let completed = if c.completed_milestones_count > 0 || c.total_milestones > 0 {
                c.completed_milestones_count
            } else {
                milestones.iter().filter(|m| m.completed).count()
            };
            let total = if c.total_milestones > 0 {
                c.total_milestones
            } else {
                milestones.len()
            };
            if let Some(m) = c.target_milestone {
                parts.push(format!(
                    "\n**Current Target Milestone**: Milestone {}: {}",
                    js_number(m.index + 1.0),
                    m.title
                ));
                if let Some(d) = m.description.as_deref().filter(|d| !d.is_empty()) {
                    parts.push(format!("**Milestone Description**: {d}"));
                }
            }
            parts.push(format!(
                "**Progress**: {completed}/{total} milestones completed"
            ));
            parts.push(format!("**All Milestones**: {}", milestone_list(goal)));
        }
        if let Some(t) = goal.estimated_time.filter(|t| *t != 0.0) {
            parts.push(format!("**Total Estimated Time**: {} hours", js_number(t)));
        }
        builder = builder.user_section("Learning Goal Progress", &parts.join("\n"));
    }

    let mut task = String::from("Generate the next layer of concepts that build upon the previous layers.\n\n**Requirements:**\n1. Suggest a layer name in <level-name> tag that follows the naming pattern of previous layers");
    task.push_str("\n2. Define a goal in <goal> tag that builds on previous layers");
    if let Some(m) = c.target_milestone {
        task.push_str(&milestone_goal_requirement(m));
        task.push_str(&format!(
            "\n- This layer should advance the learner closer to completing Milestone {}",
            js_number(m.index + 1.0)
        ));
    }
    if let Some(last) = last_layer_goal {
        task.push_str(&format!("\n\n**Previous Layer Goal**: {last}\n\n**IMPORTANT**: Build upon the previous goal. Your new goal should demonstrate a different use case or build on the skills from the previous layer, creating a natural learning progression."));
    }
    task.push_str("\n\n3. Generate a narrative describing approximately {{numConcepts}} new concepts\n4. Tag each concept with <concept>CONCEPT NAME</concept>\n5. Include <description> and <parents> for each concept\n\nOnly introduce concepts that are direct next steps beyond the current knowledge. Concepts should progress naturally from the current knowledge and build on previous concepts.");
    builder = builder.user_section("Task", &task);

    if let Some(focus) = c.focus.as_deref().filter(|f| !f.is_empty()) {
        builder = builder.user_section("Learning Focus", &format!("**CRITICAL**: All concepts must align with and support the learning focus: \"{focus}\". Do not introduce concepts that stray from this focus."));
    }
    let mut rules: Vec<String> = vec![
        "Each new concept must have at least one parent from previous layers".into(),
        "CRITICAL: Parent names in <parents> tags MUST be exact copies from the Valid Parent Names list above. Do not invent, abbreviate, or paraphrase parent names.".into(),
        "Do NOT repeat concepts from previous layers".into(),
        "CRITICAL: Every <description> tag MUST contain a meaningful sentence (at least 15 characters). Empty descriptions are not allowed.".into(),
        "Each concept MUST be immediately followed by its <description> and <parents> tags in that order".into(),
        "Build progressively on existing knowledge".into(),
        "Use simple, single-concept names (e.g., \"arrays\", \"functions\")".into(),
        "Avoid long descriptive names, parentheses, or special characters".into(),
        "Use Title Case for concept names".into(),
    ];
    if let Some(focus) = c.focus.as_deref().filter(|f| !f.is_empty()) {
        rules.push(format!(
            "All concepts must align with the learning focus \"{focus}\""
        ));
    }
    rules.push("Return only the narrative text; no JSON".into());
    builder.rules(&rules).build_user()
}

pub fn build_expansion_prompt(c: &ExpansionPromptContext<'_>) -> (String, String) {
    (
        build_expansion_system_prompt(),
        if c.is_first_layer {
            build_first_layer_prompt(c)
        } else {
            build_subsequent_layer_prompt(c)
        },
    )
}

// ---- what a narrative becomes ----

#[derive(Debug, Clone)]
pub struct ExpansionConcept {
    pub name: String,
    pub description: String,
    pub parents: Vec<String>,
}

pub struct ParsedExpansion {
    pub goal: Option<String>,
    pub level_name: Option<String>,
    pub concepts: Vec<ExpansionConcept>,
}

/// Every concept of an expansion's output, with its description and parents.
pub fn read_expansion_concepts(content: &str) -> Vec<ExpansionConcept> {
    let chars: Vec<char> = content.chars().collect();
    let next_tag_re = Regex::new(r"(?i)<(?:concept|level-name|goal|/)").ok();
    let tag_re = Regex::new(r"<[^>]+>").ok();
    let sentence_re = Regex::new(r"(?s)^[^.!?]*[.!?]").ok();
    let mut concepts = Vec::new();
    for scanned in ConceptScanner::new().feed(content) {
        let escaped = regex::escape(&scanned.name);
        let desc_re = Regex::new(&format!(
            r"(?is)<concept>{escaped}</concept>\s*<description>(.*?)</description>"
        ))
        .ok();
        let section_re = Regex::new(&format!(r"(?is)<concept>{escaped}</concept>\s*(?:<description>.*?</description>\s*)?<parents>(.*?)</parents>")).ok();
        let parents: Vec<String> = section_re
            .and_then(|re| re.captures(content).map(|c| c[1].to_string()))
            .map(|s| {
                s.split([';', ','])
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let mut description = desc_re
            .and_then(|re| re.captures(content).map(|c| c[1].trim().to_string()))
            .unwrap_or_default();
        if description.chars().count() < 10 {
            let after: String = chars[scanned.end.min(chars.len())..].iter().collect();
            let next_tag = next_tag_re
                .as_ref()
                .and_then(|re| re.find(&after).map(|m| after[..m.start()].chars().count()));
            let slice: String = after.chars().take(next_tag.unwrap_or(200)).collect();
            let clean = tag_re
                .as_ref()
                .map(|re| re.replace_all(&slice, "").trim().to_string())
                .unwrap_or_default();
            if clean.chars().count() >= 10 {
                let first = sentence_re
                    .as_ref()
                    .and_then(|re| re.find(&clean).map(|m| m.as_str().to_string()));
                description = first
                    .unwrap_or_else(|| clean.chars().take(200).collect())
                    .trim()
                    .to_string();
            }
        }
        concepts.push(ExpansionConcept {
            name: scanned.name,
            description,
            parents,
        });
    }
    concepts
}

fn normalize_name(name: &str) -> String {
    name.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct BuiltExpansion {
    pub mutations: MutationBatch,
    pub parsed: ParsedExpansion,
}

fn tag_text(content: &str, tag: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r"(?i)<{tag}>([^\n\r\u{{2028}}\u{{2029}}]*?)</{tag}>"
    ))
    .ok()?;
    re.captures(content).map(|c| c[1].trim().to_string())
}

/// `buildExpansionMutations`: the layer, its concepts and their edges, as a batch to apply.
pub fn build_expansion_mutations(
    content: &str,
    graph: &NodeBasedKnowledgeGraph,
    learning_goal: Option<&LearningGoal>,
    prompt: Option<&str>,
    now: f64,
) -> StoreResult<BuiltExpansion> {
    let existing_layers: Vec<&GraphNode> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Layer")
        .collect();
    let next_layer_number = if existing_layers.is_empty() {
        1.0
    } else {
        existing_layers
            .iter()
            .map(|n| num_prop(n, "layerNumber").unwrap_or(0.0))
            .fold(f64::MIN, f64::max)
            + 1.0
    };
    let is_first_layer = existing_layers.is_empty();

    let milestone_nodes: Vec<&GraphNode> = graph
        .nodes
        .values()
        .filter(|n| n.node_type == "Milestone")
        .collect();
    struct Ms {
        id: String,
        sequence: f64,
        title: String,
    }
    let mut sorted: Vec<Ms> = milestone_nodes
        .iter()
        .map(|n| Ms {
            id: n.id.clone(),
            sequence: num_prop(n, "sequence").unwrap_or(0.0),
            title: str_prop(n, "name"),
        })
        .collect();
    sorted.sort_by(|a, b| num_order(a.sequence, b.sequence));
    let with_sequence: Vec<Ms> = if !sorted.is_empty() {
        sorted
    } else {
        learning_goal
            .and_then(|g| g.milestones.as_ref())
            .map(|ms| {
                ms.iter()
                    .enumerate()
                    .map(|(i, m)| Ms {
                        id: m.id.clone(),
                        sequence: i as f64,
                        title: m.title.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut target: Option<&Ms> = None;
    let mut target_node: Option<&GraphNode> = None;
    if !with_sequence.is_empty() {
        let clamped =
            ((next_layer_number - 1.0).min(with_sequence.len() as f64 - 1.0)).max(0.0) as usize;
        if let Some(m) = with_sequence.get(clamped) {
            target = Some(m);
            target_node = milestone_nodes.iter().copied().find(|n| n.id == m.id);
        }
    }

    let level_name = tag_text(content, "level-name")
        .unwrap_or_else(|| format!("Level {}", js_number(next_layer_number)));
    let goal = tag_text(content, "goal");
    let concepts = read_expansion_concepts(content);

    let layer_id = format!("layer-{}-{}", graph.id, js_number(next_layer_number));
    let seed_id = graph
        .seed_concept_id
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            KnowledgeError::other("Seed concept not found — graph does not have a seedConceptId.")
        })?;
    match graph.nodes.get(&seed_id) {
        Some(n) if n.node_type == "Concept" => {}
        Some(_) => {
            return Err(KnowledgeError::other(format!(
                "Seed concept {seed_id} not found in graph nodes or is not a Concept node"
            )))
        }
        None => {
            return Err(KnowledgeError::other(format!(
                "Seed concept {seed_id} not found in graph nodes or is not a Concept node"
            )))
        }
    }

    let mut mutations: Vec<GraphMutation> = Vec::new();
    let layer_node = make_node(
        &layer_id,
        "Layer",
        json!({
            "id": layer_id,
            "layerNumber": number(next_layer_number),
            "name": level_name,
            "goal": goal.clone().unwrap_or_else(|| level_name.clone()),
            "prompt": prompt.unwrap_or_default(),
            "response": content,
            "createdAt": number(now),
            "targetMilestoneId": target.map_or(Value::Null, |m| json!(m.id)),
            "targetMilestoneSequence": target.map_or(Value::Null, |m| number(m.sequence)),
            "targetMilestoneTitle": target.map_or(Value::Null, |m| json!(m.title)),
        }),
        now,
    );
    mutations.push(GraphMutation::CreateNode {
        node: layer_node,
        update_index: Some(true),
    });
    mutations.push(GraphMutation::CreateRelationship {
        relationship: literal_relationship(&seed_id, &layer_id, "hasLayer", now, None),
    });

    if let Some(node) = target_node {
        mutations.push(GraphMutation::CreateRelationship {
            relationship: literal_relationship(
                &layer_id,
                &node.id,
                "hasMilestone",
                now,
                Some(Map::from_iter([
                    ("layerNumber".to_string(), number(next_layer_number)),
                    (
                        "milestoneSequence".to_string(),
                        number(target.map_or(0.0, |m| m.sequence)),
                    ),
                ])),
            ),
        });
    }

    if !is_first_layer {
        if let Some(milestones) = learning_goal
            .and_then(|g| g.milestones.as_ref())
            .filter(|m| !m.is_empty())
        {
            let to_complete = existing_layers.len() as isize - 1;
            if to_complete >= 0 && (to_complete as usize) < milestones.len() {
                let m = &milestones[to_complete as usize];
                if !m.completed {
                    let found = milestone_nodes.iter().find(|n| {
                        n.properties.get("id").and_then(Value::as_str) == Some(m.id.as_str())
                            || n.properties.get("name").and_then(Value::as_str)
                                == Some(m.title.as_str())
                            || n.id == m.id
                    });
                    if let Some(node) = found {
                        let mut props = Map::new();
                        props.insert("completed".into(), json!(true));
                        props.insert("completedAt".into(), number(now));
                        mutations.push(GraphMutation::UpdateNode {
                            node_id: node.id.clone(),
                            properties: props,
                            update_timestamp: Some(true),
                        });
                    }
                }
            }
        }
    }

    let graph_by_normalized: HashMap<String, &GraphNode> = {
        let mut map = HashMap::new();
        for n in graph.nodes.values() {
            if n.node_type == "Concept" {
                if let Some(name) = n.properties.get("name").and_then(Value::as_str) {
                    map.entry(normalize_name(name)).or_insert(n);
                }
            }
        }
        map
    };
    let mut batch_by_name: HashMap<String, GraphNode> = HashMap::new();
    let mut batch_by_normalized: HashMap<String, GraphNode> = HashMap::new();
    let mut batch_by_id: HashMap<String, GraphNode> = HashMap::new();

    for concept in &concepts {
        let concept_id = concept.name.clone();
        let existing = graph.nodes.get(&concept_id);
        let concept_node: GraphNode;
        if let Some(existing) = existing {
            let existing_desc = if existing.node_type == "Concept" {
                str_prop(existing, "description")
            } else {
                String::new()
            };
            let mut props = Map::new();
            props.insert(
                "description".into(),
                json!(if concept.description.is_empty() {
                    existing_desc
                } else {
                    concept.description.clone()
                }),
            );
            props.insert("layer".into(), number(next_layer_number));
            props.insert("updatedAt".into(), number(now));
            mutations.push(GraphMutation::UpdateNode {
                node_id: concept_id.clone(),
                properties: props,
                update_timestamp: Some(true),
            });
            concept_node = existing.clone();
        } else {
            concept_node = make_node(
                &concept_id,
                "Concept",
                json!({ "id": concept_id, "name": concept.name, "description": concept.description, "layer": number(next_layer_number), "createdAt": number(now) }),
                now,
            );
            mutations.push(GraphMutation::CreateNode {
                node: concept_node.clone(),
                update_index: Some(true),
            });
        }
        batch_by_id.insert(concept_id.clone(), concept_node.clone());
        batch_by_name.insert(concept.name.clone(), concept_node.clone());
        batch_by_normalized.insert(normalize_name(&concept.name), concept_node);

        mutations.push(GraphMutation::CreateRelationship {
            relationship: literal_relationship(
                &layer_id,
                &concept_id,
                "containsConcept",
                now,
                None,
            ),
        });
        mutations.push(GraphMutation::CreateRelationship {
            relationship: literal_relationship(&concept_id, &layer_id, "belongsToLayer", now, None),
        });

        for parent_name in &concept.parents {
            let name = parent_name.trim();
            let resolved: Option<String> = if name.is_empty() {
                None
            } else {
                let exact = graph
                    .nodes
                    .values()
                    .find(|n| {
                        n.node_type == "Concept"
                            && (n.id == name
                                || n.properties.get("name").and_then(Value::as_str) == Some(name))
                    })
                    .map(|n| n.id.clone())
                    .or_else(|| batch_by_name.get(name).map(|n| n.id.clone()))
                    .or_else(|| batch_by_id.get(name).map(|n| n.id.clone()));
                exact.or_else(|| {
                    let norm = normalize_name(name);
                    graph_by_normalized
                        .get(&norm)
                        .map(|n| n.id.clone())
                        .or_else(|| batch_by_normalized.get(&norm).map(|n| n.id.clone()))
                })
            };
            let Some(parent_id) = resolved else { continue };
            mutations.push(GraphMutation::CreateRelationship {
                relationship: literal_relationship(&parent_id, &concept_id, "hasChild", now, None),
            });
            mutations.push(GraphMutation::CreateRelationship {
                relationship: literal_relationship(&concept_id, &parent_id, "hasParent", now, None),
            });
        }
    }

    Ok(BuiltExpansion {
        mutations: MutationBatch {
            mutations,
            metadata: Some(MutationMetadata {
                operation: "progressiveExpandMultipleFromText".into(),
                timestamp: now,
                model: None,
            }),
        },
        parsed: ParsedExpansion {
            goal,
            level_name: Some(level_name),
            concepts,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concepts_carry_their_description_and_parents() {
        let text = "<level-name>L</level-name><concept>Heat</concept><description>Energy moving between bodies.</description><parents>Thermo; Temp</parents>";
        let concepts = read_expansion_concepts(text);
        assert_eq!(concepts.len(), 1);
        assert_eq!(concepts[0].description, "Energy moving between bodies.");
        assert_eq!(concepts[0].parents, ["Thermo", "Temp"]);
    }

    #[test]
    fn a_missing_description_falls_back_to_the_text_after_the_tag() {
        let text = "<concept>Entropy</concept> A measure of disorder in a system. More words <concept>Next</concept>";
        let concepts = read_expansion_concepts(text);
        assert_eq!(
            concepts[0].description,
            "A measure of disorder in a system."
        );
    }

    #[test]
    fn names_normalise_for_parent_matching() {
        assert_eq!(normalize_name("  Heat   Transfer "), "heat transfer");
    }
}
