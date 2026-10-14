//! The prompt for a concept's lesson (`explainPromptBuilder.ts`).

use super::goal::LearningGoal;
use super::prompt_builder::{Channel, PromptBuilder, MERMAID_DIAGRAM_RULES, MERMAID_SECTION_TITLE};
use crate::knowledge::engine::pure::GraphNode;

/// A visualization domain the lesson may point at (`getVisualizationCapabilities()`): the host reads these from
/// the installed learning organisms.
#[derive(Debug, Clone)]
pub struct VisualizationDomain {
    pub domain_type: String,
    pub description: String,
}

/// A concept the learner met on another path that resembles this one.
pub struct CrossGraphPrior {
    pub name: String,
    pub text: String,
}

pub struct ExplainPromptContext<'a> {
    pub concept: &'a GraphNode,
    pub seed_concept: Option<&'a GraphNode>,
    pub learning_goal: Option<&'a LearningGoal>,
    pub difficulty: String,
    pub focus: Option<String>,
    pub simple: bool,
    pub minimal: bool,
    pub prerequisites: Vec<&'a GraphNode>,
    pub cross_graph_priors: Vec<CrossGraphPrior>,
    pub visualizations: &'a [VisualizationDomain],
}

pub fn build_explain_system_prompt() -> String {
    PromptBuilder::new()
        .section("Role", "You are an expert instructional designer who creates engaging, comprehensive learning content.", Channel::System)
        .section("Output Format", "Return your explanation in Markdown format with proper formatting:\n- Use headings, lists, and code blocks\n- Include examples when helpful\n- Use clear, structured sections", Channel::System)
        .section(MERMAID_SECTION_TITLE, MERMAID_DIAGRAM_RULES, Channel::System)
        .build_system()
}

fn name_of(node: &GraphNode) -> String {
    if node.node_type == "Concept" {
        node.properties
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        node.id.clone()
    }
}

pub fn build_explain_prompt(c: &ExplainPromptContext<'_>) -> (String, String) {
    let prop = |node: &GraphNode, key: &str| {
        node.properties
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut builder = PromptBuilder::new().with_context(&[
        ("conceptName", prop(c.concept, "name")),
        ("conceptDescription", prop(c.concept, "description")),
        ("difficulty", c.difficulty.clone()),
        ("focus", c.focus.clone().unwrap_or_default()),
    ]);

    builder = builder.user_section(
        "Concept to Explain",
        "**Name**: {{conceptName}}\n\n**Description**: {{conceptDescription}}",
    );
    if let Some(seed) = c.seed_concept {
        builder = builder.user_section(
            "Learning Context",
            &format!("The learner is studying: **{}**", prop(seed, "name")),
        );
    }
    if let Some(goal) = c.learning_goal {
        builder = builder.user_section(
            "Learning Goal",
            &format!("**Goal**: {}\n\n{}", goal.title, goal.description),
        );
    }
    if c.difficulty == "beginner" {
        builder = builder.user_section("Difficulty Level", "Use simple language, avoid jargon, and provide clear explanations with examples. Assume minimal prior knowledge.");
    } else if c.difficulty == "advanced" {
        builder = builder.user_section("Difficulty Level", "Provide detailed, nuanced explanations suitable for someone with substantial prior knowledge.");
    }
    let prerequisite_names: Vec<String> = c.prerequisites.iter().map(|p| name_of(p)).collect();
    if !prerequisite_names.is_empty() {
        builder = builder.list("Prerequisites", &prerequisite_names);
    }
    if !c.cross_graph_priors.is_empty() {
        let cross_list = c
            .cross_graph_priors
            .iter()
            .map(|p| {
                format!(
                    "- **{}** (from one of the learner's other learning paths): {}",
                    p.name, p.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        builder = builder.user_section(
            "Related Concepts from Other Learning Paths (semantic matches via embeddings)",
            &cross_list,
        );
    }

    let mut task_parts: Vec<&str> = vec![
        "Create a comprehensive explanation for {{conceptName}}",
        "Use Markdown formatting",
        "Include examples, analogies, and real-world applications when helpful",
    ];
    if !c.minimal {
        task_parts.push("Include practice questions with Bloom's taxonomy levels");
        task_parts.push("Use learning science tags: <activate>, <connect>, <reflect>, <bloom>");
        task_parts.push(
            "Include interactive visualization markers and runnable code blocks where helpful",
        );
    }
    let task = task_parts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {t}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    builder = builder.user_section("Task", &task);

    if !c.minimal && !c.simple {
        let parents_list = if prerequisite_names.is_empty() {
            "- No prior concepts (this is foundational)".to_string()
        } else {
            prerequisite_names
                .iter()
                .map(|p| format!("- **{p}**: [one-sentence reminder of what this concept means]"))
                .collect::<Vec<_>>()
                .join("\n   ")
        };
        let cross_priors_note = if c.cross_graph_priors.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nAlso consider these semantically related concepts the learner has seen in *other* learning paths (use natural language reminders, do not invent details):\n{}",
                c.cross_graph_priors.iter().map(|p| format!("- **{}**: {}", p.name, p.text)).collect::<Vec<_>>().join("\n")
            )
        };
        let skip_note = if prerequisite_names.is_empty() && c.cross_graph_priors.is_empty() {
            "(You may skip <connect> tag if no parent concepts)"
        } else {
            ""
        };
        builder = builder.user_section(
            "Learning Science Tags (REQUIRED)",
            &format!(
                r####"
**CRITICAL: All tags MUST have proper opening AND closing tags. Never leave tags unclosed.**

1. **Activation Tag** - MUST be the very first element in the lesson:
   <activate>
   A thought-provoking question about prior knowledge (1-2 sentences)
   </activate>

2. **Connection Tag** - Immediately after activation:
   <connect>
   This builds on:
   {parents_list}{cross_priors_note}
   </connect>
   {skip_note}

3. **Reflection Tags** - Insert 1-2 reflection prompts throughout the lesson (after key concepts):
   <reflect>
   One sentence question encouraging elaboration or real-world connections
   </reflect>

4. **Bloom's Taxonomy Tags** - Wrap ALL practice questions with cognitive level:
   <bloom level="[remember|understand|apply|analyze|evaluate|create]">
   <question>Your question here</question>
   <answer>Your answer here</answer>
   </bloom>
   - Use at least 2-3 different Bloom levels
   - Progress from lower to higher cognitive levels

**Tag Rules:**
- <activate> MUST be the very first element
- <connect> immediately after <activate>
- <reflect> tags inline within lesson content (1-2 total)
- <bloom> wraps existing <question>/<answer> pairs
- ALL tags MUST be properly closed (</activate>, </connect>, </reflect>, </bloom>)
- Do NOT nest tags incorrectly or leave any tags unclosed"####,
                parents_list = parents_list,
                cross_priors_note = cross_priors_note,
                skip_note = skip_note
            ),
        );

        builder = builder.user_section("Runnable Code Blocks", r####"When the lesson includes a programming or executable example that the learner should edit and run, use a fenced code block with the language identifier followed by "run" (e.g. ```python run).
- Only mark code blocks as runnable when interactive execution adds learning value.
- Keep runnable blocks self-contained and deterministic."####);

        let domain_list = c
            .visualizations
            .iter()
            .map(|d| format!("  * \"{}\" — {}", d.domain_type, d.description))
            .collect::<Vec<_>>()
            .join("\n");
        let allowed_types = c
            .visualizations
            .iter()
            .map(|d| d.domain_type.as_str())
            .collect::<Vec<_>>()
            .join("|");
        builder = builder.user_section(
            "Interactive Visualizations",
            &format!(
                r####"The SDK produces INTERACTIVE SIMULATIONS, not static visualizations. Every one of the domains below is a live canvas the learner operates — not a picture they look at. When a concept genuinely benefits from that, insert an inline visualization marker.

- Use EXACTLY this self-closing tag format: <visualize type="{allowed_types}" description="short, specific description of what the learner DOES" />
- Available domains (each backed by a real interactive canvas organism):
{domain_list}
- The type MUST match one of the domains above — no other types are allowed.
- The description MUST describe what the learner DOES on the canvas — the variable they change, the action they trigger, the result they watch update. Do NOT describe a static picture, and do NOT use metaphors or analogies.
- TYPE SELECTION GUIDE — what the learner does in each:
  * "algorithms" — steps a classic CS algorithm (sorting, searching, graph traversal) forward/backward over live data and watches the state change at each step. ONLY for classic CS algorithms — not general step-throughs, type conversions, or code traces.
  * "math" — manipulates geometric objects, drags points on a coordinate plane, or varies a function's parameters and watches the graph update.
  * "physics" — varies a quantity (force, angle, velocity) and launches/runs a simulation to watch the resulting motion, wave, or field.
  * "chemistry" — adds a reactant/titrant or changes a condition and watches the reaction, concentration curve, or diffusion respond.
  * "biology" — manipulates an input (light, nutrients, a cell's state) and watches a biological process play out.
  * "probability" — runs trials (rolls dice, flips coins, samples) and watches the resulting distribution build up live.
  * "logic" — toggles truth assignments of propositions and watches compound formulas evaluate live, or applies inference rules (modus ponens, modus tollens) step by step to derive a proof line.
  * "astronomy" — adjusts orbital eccentricity/axis and steps a planet along its orbit watching equal-area sweeps, or sets a star's initial mass and steps it through its life cycle.
  * "earth" — picks a plate-boundary type and drives the motion watching landforms and quakes accumulate, or varies solar input and watches water-cycle fluxes rebalance.
  * "economics" — shifts supply/demand curves and sets a price to see shortage/surplus emerge against the computed equilibrium, or moves an allocation slider along a production-possibilities frontier and reads the marginal opportunity cost.
  * "psychology" — runs conditioning trials (pairing/extinction) and watches the Rescorla-Wagner associative-strength curve update, or classifies everyday scenarios onto Maslow's hierarchy with per-pick feedback.
  * "sociology" — sets a mobility rate and steps generations watching stratification shares and a Gini index respond, or links personal troubles to structural public issues (Mills) on a drawn trouble→issue map.
  * "linguistics" — builds a constituent syntax tree bottom-up by attaching NP/VP/PP/S in derivation order, or applies ordered sound-change rules and watches every matching word shift at once.
  * "history" — toggles causes on a causal-chain map to test counterfactuals against a war threshold (with cascades through dependent causes), or steps a filtered timeline reading per-category running tallies.
  * "civics" — advances a bill through House/Senate votes, veto, and the two-thirds override arithmetic, or names which branch checks a given action and sees the check arrow drawn.
  * "geography" — sets birth/death/migration rates and steps decades watching a cohort population pyramid and dependency ratio evolve, or drives temperature/precipitation sliders and watches the Köppen classification flip at declared thresholds.
- Choose the MOST INTERACTIVE type that can genuinely model the concept — never the type that merely illustrates it.
- If NO listed type can genuinely model the concept as something the learner operates, DO NOT emit a <visualize> tag at all. A missing visualization is better than a static or force-fit one.
- Good: type="physics" description="Drag the launch angle and initial speed sliders, then fire the projectile and watch its parabolic trajectory trace across the canvas"
- Good: type="chemistry" description="Add titrant drop by drop and watch the pH curve plot live as the solution crosses the equivalence point"
- Bad: type="math" description="Two people speaking different languages with a translator matching phrases" (not a real visual scene)
- Bad: type="physics" description="A diagram of the solar system" (static picture, not something the learner operates)
- Place visualizations right after the explanatory paragraph that motivates them.
- Maximum 1-2 visualizations per lesson."####,
                allowed_types = allowed_types,
                domain_list = domain_list
            ),
        );
    }

    builder = builder.rules(&[
        "Use GitHub-flavored Markdown".to_string(),
        "Include code snippets with language identifiers when relevant".to_string(),
        "Be precise and factual".to_string(),
        "Tone should be supportive and clear".to_string(),
        "If information is uncertain, state assumptions".to_string(),
    ]);

    (build_explain_system_prompt(), builder.build_user())
}
