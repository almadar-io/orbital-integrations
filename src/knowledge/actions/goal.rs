//! A path's learning goal and the choices read from it (`graphHelpers.ts`, `getMilestonesForGoal`).

use serde_json::{Map, Value};

use super::common::str_prop;
use crate::knowledge::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};

#[derive(Debug, Clone)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub completed: bool,
}

#[derive(Debug, Clone)]
pub struct LearningGoal {
    pub title: String,
    pub description: String,
    pub goal_type: String,
    pub target: String,
    pub estimated_time: Option<f64>,
    pub milestones: Option<Vec<Milestone>>,
    pub custom_metadata: Option<Map<String, Value>>,
    pub assessed_level: Option<String>,
}

fn truthy_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn milestones_for_goal<'a>(
    graph: &'a NodeBasedKnowledgeGraph,
    goal_id: &str,
) -> Vec<&'a GraphNode> {
    let mut ids: Vec<&str> = Vec::new();
    for r in &graph.relationships {
        if r.source == goal_id && r.rel_type == "hasMilestone" && !ids.contains(&r.target.as_str())
        {
            ids.push(&r.target);
        }
    }
    let mut nodes: Vec<&GraphNode> = ids
        .iter()
        .filter_map(|id| graph.nodes.get(id))
        .filter(|n| n.node_type == "Milestone")
        .collect();
    nodes.sort_by(|a, b| {
        let sa = a
            .properties
            .get("sequence")
            .and_then(Value::as_f64)
            .unwrap_or(f64::INFINITY);
        let sb = b
            .properties
            .get("sequence")
            .and_then(Value::as_f64)
            .unwrap_or(f64::INFINITY);
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    });
    nodes
}

/// `inferLearningGoalFromGraph`: the first goal of the graph, with its milestones.
pub fn infer_learning_goal(graph: &NodeBasedKnowledgeGraph) -> Option<LearningGoal> {
    let goal = graph
        .ids_of("LearningGoal")
        .first()
        .and_then(|id| graph.nodes.get(id))?;
    if goal.node_type != "LearningGoal" {
        return None;
    }
    let milestones: Vec<Milestone> = milestones_for_goal(graph, &goal.id)
        .into_iter()
        .map(|n| Milestone {
            id: truthy_string(n.properties.get("id")).unwrap_or_else(|| n.id.clone()),
            title: str_prop(n, "name"),
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
    Some(LearningGoal {
        title: str_prop(goal, "name"),
        description: str_prop(goal, "description"),
        goal_type: truthy_string(goal.properties.get("type"))
            .unwrap_or_else(|| "skill_mastery".to_string()),
        target: str_prop(goal, "target"),
        estimated_time: goal.properties.get("estimatedTime").and_then(Value::as_f64),
        milestones: (!milestones.is_empty()).then_some(milestones),
        custom_metadata: goal
            .properties
            .get("customMetadata")
            .and_then(Value::as_object)
            .cloned(),
        assessed_level: truthy_string(goal.properties.get("assessedLevel")),
    })
}

/// A path's difficulty: the learner's chosen level, else a valid drafted level, else the graph's, else intermediate.
pub fn resolve_difficulty(goal: Option<&LearningGoal>, graph: &NodeBasedKnowledgeGraph) -> String {
    if let Some(level) = goal.and_then(|g| g.assessed_level.clone()) {
        return level;
    }
    let drafted = goal
        .and_then(|g| g.custom_metadata.as_ref())
        .and_then(|m| m.get("difficulty"))
        .and_then(Value::as_str);
    if let Some(level) = drafted.filter(|d| matches!(*d, "beginner" | "intermediate" | "advanced"))
    {
        return level.to_string();
    }
    graph
        .extra
        .get("difficulty")
        .and_then(Value::as_str)
        .map_or_else(|| "intermediate".to_string(), str::to_string)
}

pub fn infer_difficulty(graph: &NodeBasedKnowledgeGraph) -> String {
    resolve_difficulty(infer_learning_goal(graph).as_ref(), graph)
}

/// The focus a prompt carries: the goal's declared focus, else its description, else the graph's.
pub fn focus_of(goal: Option<&LearningGoal>, graph: &NodeBasedKnowledgeGraph) -> Option<String> {
    let declared = goal
        .and_then(|g| g.custom_metadata.as_ref())
        .and_then(|m| m.get("focus"))
        .and_then(Value::as_str);
    declared
        .map(str::to_string)
        .or_else(|| goal.map(|g| g.description.clone()))
        .or_else(|| {
            graph
                .extra
                .get("focus")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub fn infer_focus(graph: &NodeBasedKnowledgeGraph) -> Option<String> {
    focus_of(infer_learning_goal(graph).as_ref(), graph)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn graph(extra: Value) -> NodeBasedKnowledgeGraph {
        let mut g = json!({
            "id": "g", "createdAt": 1, "updatedAt": 1,
            "nodes": {
                "goal": { "id": "goal", "type": "LearningGoal", "properties": { "name": "Plants", "description": "How plants live", "customMetadata": { "difficulty": "advanced" } } },
                "m2": { "id": "m2", "type": "Milestone", "properties": { "name": "Second", "sequence": 2 } },
                "m1": { "id": "m1", "type": "Milestone", "properties": { "name": "First", "sequence": 1, "completed": true } }
            },
            "nodeTypes": { "LearningGoal": ["goal"] },
            "relationships": [
                { "id": "a", "source": "goal", "target": "m2", "type": "hasMilestone" },
                { "id": "b", "source": "goal", "target": "m1", "type": "hasMilestone" }
            ]
        });
        if let (Some(base), Some(add)) = (g.as_object_mut(), extra.as_object()) {
            base.extend(add.clone());
        }
        serde_json::from_value(g).unwrap()
    }

    #[test]
    fn milestones_come_back_in_sequence_order() {
        let goal = infer_learning_goal(&graph(json!({}))).unwrap();
        let titles: Vec<_> = goal
            .milestones
            .unwrap()
            .iter()
            .map(|m| m.title.clone())
            .collect();
        assert_eq!(titles, ["First", "Second"]);
        assert_eq!(goal.goal_type, "skill_mastery");
    }

    #[test]
    fn difficulty_prefers_the_chosen_level_then_the_drafted_one_then_the_graphs() {
        let g = graph(json!({ "difficulty": "beginner" }));
        assert_eq!(infer_difficulty(&g), "advanced");
        let mut with_chosen = infer_learning_goal(&g).unwrap();
        with_chosen.assessed_level = Some("beginner".into());
        assert_eq!(resolve_difficulty(Some(&with_chosen), &g), "beginner");
        assert_eq!(resolve_difficulty(None, &g), "beginner");
        assert_eq!(resolve_difficulty(None, &graph(json!({}))), "intermediate");
    }

    #[test]
    fn focus_is_the_goal_description_when_the_goal_declares_none() {
        assert_eq!(
            infer_focus(&graph(json!({ "focus": "ignored" }))).as_deref(),
            Some("How plants live")
        );
    }
}
