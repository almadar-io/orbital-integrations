//! The prompts of the model calls that write long text: a level's practice review (`layerPracticePromptBuilder.ts`)
//! and an answer to a learner's question (`answerQuestion.ts`).

use serde_json::Value;

use super::prompt_builder::{Channel, PromptBuilder, MERMAID_DIAGRAM_RULES, MERMAID_SECTION_TITLE};
use crate::knowledge::engine::pure::GraphNode;

pub struct LayerPracticeContext<'a> {
    pub concepts: Vec<&'a GraphNode>,
    pub layer_goal: String,
    pub layer_number: f64,
    pub seed_concept: Option<&'a GraphNode>,
    pub difficulty: Option<String>,
    pub focus: Option<String>,
}

pub fn build_layer_practice_system_prompt() -> String {
    PromptBuilder::new()
        .section("Role", "You are an expert educational content creator. Your task is to create a solution-focused review that helps learners understand how concepts apply to real-world goals.", Channel::System)
        .section("Content Format", r####"Create content in Markdown format that:
1. First presents a complete solution (project, implementation, or approach) that addresses the learning goal
2. Then explains how each concept learned relates to and is used in that solution
3. Uses clear, engaging language appropriate for the learner's level"####, Channel::System)
        .section("Structure", r####"Structure the content as follows:
- Start with a section presenting the complete solution to the goal (this is the "answer" or "solution")
- Follow with sections for each concept, explaining how it relates to the solution (these are like "questions" about each concept's role, followed by "answers" explaining their application)

Format it naturally without explicit question/answer tags - just present it as flowing text with clear sections. Use headings to separate the solution from concept explanations."####, Channel::System)
        .section("Markdown Formatting", r####"Use Markdown formatting for:
- Code blocks with syntax highlighting (triple backticks with language)
- Lists (bulleted or numbered)
- Emphasis (asterisks for italic or bold)
- Headings (hash symbols for sections)
- Inline code (single backticks)
- Links and other Markdown features"####, Channel::System)
        .section(MERMAID_SECTION_TITLE, MERMAID_DIAGRAM_RULES, Channel::System)
        .build_system()
}

fn prop(node: &GraphNode, key: &str) -> Option<String> {
    node.properties
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn build_layer_practice_prompt(c: &LayerPracticeContext<'_>) -> (String, String) {
    let concept_list = c
        .concepts
        .iter()
        .map(|node| {
            let name = if node.node_type == "Concept" {
                prop(node, "name").unwrap_or_default()
            } else {
                node.id.clone()
            };
            let desc = match prop(node, "description").filter(|d| !d.is_empty()) {
                Some(d) if node.node_type == "Concept" => format!(" - {d}"),
                _ => String::new(),
            };
            format!("- {name}{desc}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let difficulty_text = match c.difficulty.as_deref() {
        Some("beginner") => "beginner-level",
        Some("intermediate") => "intermediate-level",
        Some(_) => "advanced-level",
        None => "",
    };
    let seed_name = c
        .seed_concept
        .and_then(|s| prop(s, "name"))
        .unwrap_or_default();
    let seed_desc = c
        .seed_concept
        .and_then(|s| prop(s, "description"))
        .unwrap_or_default();
    let seed_info = if c.seed_concept.is_some() {
        format!(
            "\n\n**Overall Learning Context:**\nThe learner is studying the broader topic \"{seed_name}\".{}{}{}",
            if seed_desc.is_empty() { String::new() } else { format!(" This topic is about: {seed_desc}") },
            if c.difficulty.is_some() { format!(" The learner is at a {difficulty_text} level.") } else { String::new() },
            match c.focus.as_deref().filter(|f| !f.is_empty()) {
                Some(f) => format!(" The learning focus for this path is: \"{f}\"."),
                None => String::new(),
            }
        )
    } else {
        String::new()
    };
    let context_guidance = if c.seed_concept.is_some() {
        format!("\n\nEnsure the content connects back to \"{seed_name}\" and shows how this layer's concepts fit into the broader learning journey.")
    } else {
        String::new()
    };
    let builder = PromptBuilder::new()
        .with_context(&[
            ("layerNumber", crate::jsnumber::js_number(c.layer_number)),
            ("layerGoal", c.layer_goal.clone()),
            ("conceptList", concept_list),
            ("seedInfo", seed_info),
            ("contextGuidance", context_guidance),
        ])
        .user_section("Task", r####"Create a solution-focused review for Level {{layerNumber}} with the following learning goal:

**Learning Goal:** {{layerGoal}}

**Concepts Learned:**
{{conceptList}}{{seedInfo}}

Generate content that:
1. First presents a complete solution that addresses the learning goal (this could be a project, implementation, approach, or method)
2. Then for each concept, explain how it relates to and is used in that solution
3. Connect everything back to the overall learning topic and ensure the content is appropriate for the learner's level{{contextGuidance}}

Structure it as:
- A main section with the solution/implementation
- Followed by sections for each concept showing how it applies to the solution
- Make connections to how this relates to the broader learning topic

Write it naturally without explicit question/answer tags - just flowing text with clear headings. Focus on showing how each concept contributes to solving the goal and how it fits into the overall learning journey.

Return only the Markdown text, no additional formatting or JSON."####);
    (build_layer_practice_system_prompt(), builder.build_user())
}

/// The prompt that answers a learner's question about a concept (`buildAnswerQuestionPrompt`).
pub fn build_answer_question_prompt(
    concept: &GraphNode,
    question: &str,
    selected_text: Option<&str>,
    difficulty: &str,
) -> (String, String) {
    let concept_name = prop(concept, "name").unwrap_or_default();
    let concept_description = prop(concept, "description").unwrap_or_default();
    let question_text = match selected_text.filter(|s| !s.is_empty()) {
        Some(sel) => format!("{question}\n\nSelected text: \"{sel}\""),
        None => question.to_string(),
    };
    let guidelines = [
        "Answer the question directly and clearly",
        "Tailor your response to the learner's level",
        "Use examples when helpful",
        "Be concise but thorough",
        "Format your response in clear, readable Markdown",
    ]
    .iter()
    .map(|g| format!("- {g}"))
    .collect::<Vec<_>>()
    .join("\n");
    let system = PromptBuilder::new()
        .section("Role", &format!("You are an expert educational assistant helping a learner understand concepts at the {difficulty} level."), Channel::System)
        .section("Guidelines", &guidelines, Channel::System)
        .section(MERMAID_SECTION_TITLE, MERMAID_DIAGRAM_RULES, Channel::System)
        .build_system();
    let user = PromptBuilder::new()
        .user_section("Context", &format!("**Concept**: {concept_name}\n\n{concept_description}"))
        .user_section("Question", &question_text)
        .user_section("Task", &format!("Provide a clear, helpful answer to this question, tailored for a {difficulty} level learner."))
        .build_user();
    (system, user)
}
