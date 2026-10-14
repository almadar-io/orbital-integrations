//! The import target for the knowledge graph (`importSink.ts`): the entity and relationship vocabulary an import
//! can write, a sink that writes rows into one of the learner's graphs, and the fresh graph an import starts in.

use serde_json::{json, Map, Value};

use super::common::make_node;
use super::IdSource;
use crate::json::number;
use crate::knowledge::engine::pure::NodeBasedKnowledgeGraph;
use crate::knowledge::firestore::forward_relationship;
use crate::knowledge::store::{KnowledgeDeps, KnowledgeError, StoreResult};

/// The relationships an import may create, as `(type, from, to)`.
pub const MIGRATION_RELATIONSHIPS: [(&str, &str, &str); 5] = [
    ("containsConcept", "LearningGoal", "Concept"),
    ("containsConcept", "Concept", "Concept"),
    ("hasLesson", "Concept", "Lesson"),
    ("hasFlashCard", "Concept", "FlashCard"),
    ("hasLearningGoal", "Graph", "LearningGoal"),
];

const IMPORT_RELATIONSHIP_TYPES: [&str; 4] = [
    "containsConcept",
    "hasLesson",
    "hasFlashCard",
    "hasLearningGoal",
];

struct CreatedRow {
    id: String,
    entity: String,
}

pub struct CommitResult {
    pub committed: usize,
    pub failed: Vec<(String, String)>,
}

fn string_field(fields: &Map<String, Value>, key: &str) -> Option<String> {
    fields.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `ImportSink` over the graph store: `create_entity_row` writes a node; unlinked top-level concepts attach to the graph on commit.
pub struct KnowledgeImportSink<'a> {
    deps: &'a KnowledgeDeps,
    uuid: &'a IdSource,
    uid: &'a str,
    graph_id: &'a str,
    created: Vec<CreatedRow>,
    linked_targets: Vec<String>,
}

impl<'a> KnowledgeImportSink<'a> {
    pub fn new(
        deps: &'a KnowledgeDeps,
        uuid: &'a IdSource,
        uid: &'a str,
        graph_id: &'a str,
    ) -> Self {
        Self {
            deps,
            uuid,
            uid,
            graph_id,
            created: Vec::new(),
            linked_targets: Vec::new(),
        }
    }

    fn now(&self) -> f64 {
        self.deps.graphs.now_ms()
    }

    pub fn create_entity_row(
        &mut self,
        entity: &str,
        fields: &Map<String, Value>,
    ) -> StoreResult<String> {
        match entity {
            "Concept" => self.create_concept(fields),
            "Lesson" => self.create_lesson(fields),
            "FlashCard" => self.create_flash_card(fields),
            "LearningGoal" => self.create_learning_goal(fields),
            other => Err(KnowledgeError::other(format!(
                "entity '{other}' is not declared in the knowledge migration target"
            ))),
        }
    }

    pub fn create_relationship(
        &mut self,
        rel_type: &str,
        from_id: &str,
        to_id: &str,
    ) -> StoreResult<()> {
        if !IMPORT_RELATIONSHIP_TYPES.contains(&rel_type) {
            return Err(KnowledgeError::other(format!(
                "relationship type '{rel_type}' is not declared in the knowledge migration target"
            )));
        }
        let relationship = forward_relationship(from_id, to_id, rel_type, self.now());
        self.deps
            .graphs
            .create_relationship(self.uid, self.graph_id, relationship)?;
        self.linked_targets.push(to_id.to_string());
        Ok(())
    }

    pub fn commit(&mut self) -> StoreResult<CommitResult> {
        let mut failed = Vec::new();
        let unlinked: Vec<String> = self
            .created
            .iter()
            .filter(|r| r.entity == "Concept" && !self.linked_targets.contains(&r.id))
            .map(|r| r.id.clone())
            .collect();
        for id in unlinked {
            if let Err(e) =
                self.create_relationship("containsConcept", self.graph_id.to_string().as_str(), &id)
            {
                failed.push((id, e.message));
            }
        }
        let mut committed = 0;
        for row in &self.created {
            match self
                .deps
                .graphs
                .get_node(self.uid, self.graph_id, &row.id)?
            {
                Some(_) => committed += 1,
                None => failed.push((row.id.clone(), "node missing after write".to_string())),
            }
        }
        Ok(CommitResult { committed, failed })
    }

    fn put(&mut self, id: &str, node_type: &str, properties: Value) -> StoreResult<()> {
        let node = make_node(id, node_type, properties, self.now());
        self.deps
            .graphs
            .create_node(self.uid, self.graph_id, node)?;
        self.created.push(CreatedRow {
            id: id.to_string(),
            entity: node_type.to_string(),
        });
        Ok(())
    }

    fn create_concept(&mut self, fields: &Map<String, Value>) -> StoreResult<String> {
        let id = (self.uuid)();
        self.put(
            &id,
            "Concept",
            json!({
                "id": id,
                "name": string_field(fields, "name").unwrap_or_else(|| "Untitled concept".to_string()),
                "description": string_field(fields, "description").unwrap_or_default(),
                "isAutoGenerated": true,
            }),
        )?;
        if let Some(lesson) = string_field(fields, "lesson").filter(|l| !l.trim().is_empty()) {
            let lesson_id = format!("lesson-{id}");
            self.put(
                &lesson_id,
                "Lesson",
                json!({ "id": lesson_id, "content": lesson, "generatedAt": number(self.now()) }),
            )?;
            self.create_relationship("hasLesson", &id, &lesson_id)?;
        }
        Ok(id)
    }

    fn create_lesson(&mut self, fields: &Map<String, Value>) -> StoreResult<String> {
        let id = format!("lesson-{}", (self.uuid)());
        self.put(
            &id,
            "Lesson",
            json!({ "id": id, "content": string_field(fields, "content").unwrap_or_default(), "generatedAt": number(self.now()) }),
        )?;
        Ok(id)
    }

    fn create_flash_card(&mut self, fields: &Map<String, Value>) -> StoreResult<String> {
        let id = format!("flash-{}", (self.uuid)());
        self.put(
            &id,
            "FlashCard",
            json!({
                "id": id,
                "front": string_field(fields, "front").unwrap_or_default(),
                "back": string_field(fields, "back").unwrap_or_default(),
            }),
        )?;
        Ok(id)
    }

    fn create_learning_goal(&mut self, fields: &Map<String, Value>) -> StoreResult<String> {
        let id = (self.uuid)();
        let now = self.now();
        self.put(
            &id,
            "LearningGoal",
            json!({
                "id": id,
                "name": string_field(fields, "name").unwrap_or_else(|| "Imported goal".to_string()),
                "description": string_field(fields, "description").unwrap_or_default(),
                "type": "custom",
                "createdAt": number(now),
                "updatedAt": number(now),
            }),
        )?;
        self.create_relationship("hasLearningGoal", self.graph_id, &id)?;
        Ok(id)
    }
}

pub const NODE_TYPE_INDEX: [&str; 24] = [
    "Graph",
    "Concept",
    "Layer",
    "LearningGoal",
    "Milestone",
    "PracticeExercise",
    "Lesson",
    "ConceptMetadata",
    "GraphMetadata",
    "FlashCard",
    "Story",
    "Course",
    "Assessment",
    "Translation",
    "LanguageConfig",
    "Student",
    "ScheduleSlot",
    "Progress",
    "Enrollment",
    "AssessmentSubmission",
    "Achievement",
    "StudentPreferences",
    "CourseCategory",
    "CourseTemplate",
];

/// A new graph holding one seed concept, ready to receive an import.
pub fn create_import_graph(
    deps: &KnowledgeDeps,
    uuid: &IdSource,
    uid: &str,
    name: &str,
) -> StoreResult<String> {
    let graph_id = uuid();
    let seed_id = uuid();
    let now = deps.graphs.now_ms();
    let mut node_types = Map::new();
    for kind in NODE_TYPE_INDEX {
        node_types.insert(kind.to_string(), json!([]));
    }
    node_types.insert("Graph".into(), json!([graph_id]));
    node_types.insert("Concept".into(), json!([seed_id]));
    let mut nodes = Map::new();
    nodes.insert(
        graph_id.clone(),
        serde_json::to_value(make_node(
            &graph_id,
            "Graph",
            json!({ "id": graph_id, "seedConceptId": seed_id, "createdAt": number(now), "updatedAt": number(now) }),
            now,
        ))
        .unwrap_or(Value::Null),
    );
    nodes.insert(
        seed_id.clone(),
        serde_json::to_value(make_node(
            &seed_id,
            "Concept",
            json!({ "id": seed_id, "name": name, "description": format!("Imported from {name}"), "isSeed": true }),
            now,
        ))
        .unwrap_or(Value::Null),
    );
    let graph: NodeBasedKnowledgeGraph = serde_json::from_value(json!({
        "id": graph_id,
        "seedConceptId": seed_id,
        "createdAt": number(now),
        "updatedAt": number(now),
        "version": 1,
        "name": name,
        "nodes": nodes,
        "nodeTypes": node_types,
        "relationships": [
            forward_relationship(&graph_id, &seed_id, "containsConcept", now),
            forward_relationship(&graph_id, &seed_id, "hasSeedConcept", now),
        ],
    }))
    .map_err(|e| KnowledgeError::other(format!("the import graph is invalid: {e}")))?;
    deps.graphs.save_graph(uid, &graph, None)?;
    Ok(graph_id)
}
