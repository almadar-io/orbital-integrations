//! In-memory implementations of every port: the store the unit tests and the action ports' tests run against.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde_json::{Map, Value};

use super::engine::pure::{GraphNode, NodeBasedKnowledgeGraph};
use super::store::*;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Graphs, learner records, social records and icon cache in memory; vectors and models answer empty/refuse.
#[derive(Default)]
pub struct MemoryStore {
    clock: Mutex<f64>,
    graphs: Mutex<BTreeMap<(String, String), NodeBasedKnowledgeGraph>>,
    preferences: Mutex<HashMap<String, Map<String, Value>>>,
    progress: Mutex<HashMap<(String, String, String, String), GraphNode>>,
    enrolled: Mutex<HashMap<String, Vec<String>>>,
    members: Mutex<HashMap<String, HashMap<String, f64>>>,
    tokens: Mutex<HashMap<String, PeerTokenRecord>>,
    connections: Mutex<Vec<StoredConnection>>,
    messages: Mutex<HashMap<String, Vec<StoredMessageRow>>>,
    personas: Mutex<HashMap<String, StoredPersona>>,
    icons: Mutex<HashMap<String, IconCacheEntry>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose `now_ms` is `start`, advancing 1 ms per read, so edits stamp predictable times.
    pub fn at(start: f64) -> Self {
        Self {
            clock: Mutex::new(start),
            ..Self::default()
        }
    }
}

impl GraphStore for MemoryStore {
    fn now_ms(&self) -> f64 {
        let mut clock = lock(&self.clock);
        if *clock == 0.0 {
            return std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_millis() as f64);
        }
        let now = *clock;
        *clock += 1.0;
        now
    }

    fn get_graph(&self, uid: &str, graph_id: &str) -> StoreResult<NodeBasedKnowledgeGraph> {
        lock(&self.graphs)
            .get(&(uid.to_string(), graph_id.to_string()))
            .cloned()
            .ok_or_else(|| KnowledgeError::other(format!("Graph {graph_id} not found")))
    }

    fn save_graph(
        &self,
        uid: &str,
        graph: &NodeBasedKnowledgeGraph,
        expected_version: Option<f64>,
    ) -> StoreResult<NodeBasedKnowledgeGraph> {
        let now = self.now_ms();
        let mut graphs = lock(&self.graphs);
        let key = (uid.to_string(), graph.id.clone());
        let mut saved = graph.clone();
        match graphs.get(&key) {
            None => {
                if let Some(v) = expected_version {
                    return Err(KnowledgeError::other(format!(
                        "Graph {} not found (expected version {})",
                        graph.id,
                        crate::jsnumber::js_number(v)
                    )));
                }
                saved.version = Some(1.0);
            }
            Some(current) => {
                let current_version = current.version.unwrap_or(0.0);
                if expected_version.is_some_and(|v| current_version > v) {
                    return Err(KnowledgeError::conflict(format!(
                        "Graph {} changed since version {}",
                        graph.id,
                        crate::jsnumber::js_number(expected_version.unwrap_or(0.0))
                    )));
                }
                saved.version = Some(current_version + 1.0);
            }
        }
        saved.updated_at = now;
        graphs.insert(key, saved.clone());
        Ok(saved)
    }

    fn delete_graph(&self, uid: &str, graph_id: &str) -> StoreResult<()> {
        lock(&self.graphs)
            .remove(&(uid.to_string(), graph_id.to_string()))
            .map(|_| ())
            .ok_or_else(|| KnowledgeError::other(format!("Graph {graph_id} not found")))
    }

    fn list_graph_ids(&self, uid: &str) -> StoreResult<Vec<String>> {
        Ok(lock(&self.graphs)
            .keys()
            .filter(|(owner, _)| owner == uid)
            .map(|(_, id)| id.clone())
            .collect())
    }
}

impl StudentStore for MemoryStore {
    fn get_preferences(&self, uid: &str) -> StoreResult<Option<Map<String, Value>>> {
        Ok(lock(&self.preferences).get(uid).cloned())
    }

    fn set_preferences(
        &self,
        uid: &str,
        patch: &Map<String, Value>,
    ) -> StoreResult<Map<String, Value>> {
        let mut all = lock(&self.preferences);
        let prefs = all.entry(uid.to_string()).or_default();
        for (k, v) in patch {
            prefs.insert(k.clone(), v.clone());
        }
        Ok(prefs.clone())
    }

    fn get_progress_for_concept(
        &self,
        owner_uid: &str,
        graph_id: &str,
        student_uid: &str,
        concept_id: &str,
    ) -> StoreResult<Option<Map<String, Value>>> {
        Ok(lock(&self.progress)
            .get(&(
                owner_uid.into(),
                graph_id.into(),
                student_uid.into(),
                concept_id.into(),
            ))
            .map(|n| n.properties.clone()))
    }

    fn upsert_progress(
        &self,
        owner_uid: &str,
        graph_id: &str,
        student_uid: &str,
        concept_id: &str,
        data: &Map<String, Value>,
    ) -> StoreResult<GraphNode> {
        let now = self.now_ms();
        let key = (
            owner_uid.into(),
            graph_id.into(),
            student_uid.into(),
            concept_id.into(),
        );
        let mut all = lock(&self.progress);
        let node = all.entry(key).or_insert_with(|| GraphNode {
            id: format!("progress-{student_uid}-{concept_id}"),
            node_type: "Progress".to_string(),
            properties: Map::new(),
            created_at: crate::json::number(now).as_number().cloned(),
            updated_at: None,
        });
        for (k, v) in data {
            node.properties.insert(k.clone(), v.clone());
        }
        node.updated_at = crate::json::number(now).as_number().cloned();
        Ok(node.clone())
    }

    fn remove_enrolled_graph(
        &self,
        _owner_uid: &str,
        student_uid: &str,
        graph_id: &str,
    ) -> StoreResult<()> {
        if let Some(list) = lock(&self.enrolled).get_mut(student_uid) {
            list.retain(|g| g != graph_id);
        }
        Ok(())
    }
}

impl VectorIndex for MemoryStore {
    fn upsert_nodes(&self, _graph_id: &str, _nodes: &[GraphNode], _uid: &str) -> StoreResult<()> {
        Ok(())
    }
    fn delete_graph(&self, _graph_id: &str) -> StoreResult<()> {
        Ok(())
    }
    fn search_by_text_cross(
        &self,
        _graph_ids: &[String],
        _query: &str,
        _k: usize,
        _node_types: Option<&[String]>,
    ) -> StoreResult<Vec<VectorHit>> {
        Ok(Vec::new())
    }
    fn get_canonical_concepts_by_owner(&self, _uid: &str) -> StoreResult<CanonicalConceptSet> {
        Ok(CanonicalConceptSet::default())
    }
}

impl ModelPort for MemoryStore {
    fn stream(&self, _request: &Value) -> StoreResult<ModelStream> {
        Err(KnowledgeError::service("MemoryStore has no model"))
    }
    fn call(&self, _request: &Value) -> StoreResult<Value> {
        Err(KnowledgeError::service("MemoryStore has no model"))
    }
    fn generate_structured(&self, _request: &Value) -> StoreResult<Value> {
        Err(KnowledgeError::service("MemoryStore has no model"))
    }
}

impl SocialStore for MemoryStore {
    fn touch_member(&self, node_key: &str, uid: &str, at: f64) -> StoreResult<()> {
        lock(&self.members)
            .entry(node_key.into())
            .or_default()
            .insert(uid.into(), at);
        Ok(())
    }
    fn list_members_since(&self, node_key: &str, cutoff: f64) -> StoreResult<Vec<NodeMember>> {
        let mut out: Vec<NodeMember> = lock(&self.members)
            .get(node_key)
            .map(|m| {
                m.iter()
                    .filter(|(_, at)| **at >= cutoff)
                    .map(|(uid, at)| NodeMember {
                        uid: uid.clone(),
                        last_seen_on_node: *at,
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        Ok(out)
    }
    fn put_peer_token(&self, token: &str, record: PeerTokenRecord) -> StoreResult<()> {
        lock(&self.tokens).insert(token.into(), record);
        Ok(())
    }
    fn get_peer_token(&self, token: &str) -> StoreResult<Option<PeerTokenRecord>> {
        Ok(lock(&self.tokens).get(token).cloned())
    }
    fn delete_peer_token(&self, token: &str) -> StoreResult<()> {
        lock(&self.tokens).remove(token);
        Ok(())
    }
    fn create_connection(&self, connection: StoredConnection) -> StoreResult<String> {
        let mut all = lock(&self.connections);
        all.push(connection);
        Ok(format!("conn-{}", all.len()))
    }
    fn get_connection(&self, id: &str) -> StoreResult<Option<StoredConnection>> {
        let index = id
            .strip_prefix("conn-")
            .and_then(|n| n.parse::<usize>().ok());
        Ok(index.and_then(|n| lock(&self.connections).get(n.wrapping_sub(1)).cloned()))
    }
    fn update_connection(&self, id: &str, patch: ConnectionPatch) -> StoreResult<()> {
        let index = id
            .strip_prefix("conn-")
            .and_then(|n| n.parse::<usize>().ok());
        let mut all = lock(&self.connections);
        let Some(c) = index.and_then(|n| all.get_mut(n.wrapping_sub(1))) else {
            return Err(KnowledgeError::not_found(format!(
                "Connection {id} not found"
            )));
        };
        if let Some(v) = patch.active_badge_id {
            c.active_badge_id = v;
        }
        if let Some(v) = patch.status {
            c.status = v;
        }
        if let Some(v) = patch.archived_at {
            c.archived_at = Some(v);
        }
        if let Some(v) = patch.last_activity_at {
            c.last_activity_at = v;
        }
        Ok(())
    }
    fn add_message(&self, connection_id: &str, message: StoredMessage) -> StoreResult<String> {
        let mut all = lock(&self.messages);
        let rows = all.entry(connection_id.into()).or_default();
        let id = format!("msg-{}", rows.len() + 1);
        rows.push(StoredMessageRow {
            id: id.clone(),
            message,
        });
        Ok(id)
    }
    fn list_messages(&self, connection_id: &str) -> StoreResult<Vec<StoredMessageRow>> {
        Ok(lock(&self.messages)
            .get(connection_id)
            .cloned()
            .unwrap_or_default())
    }
    fn set_message_moderation(
        &self,
        connection_id: &str,
        message_id: &str,
        moderation: MessageModeration,
    ) -> StoreResult<()> {
        let mut all = lock(&self.messages);
        let row = all
            .get_mut(connection_id)
            .and_then(|rows| rows.iter_mut().find(|r| r.id == message_id))
            .ok_or_else(|| KnowledgeError::not_found(format!("Message {message_id} not found")))?;
        row.message.moderation = Some(moderation);
        Ok(())
    }
    fn get_persona(&self, key: &str) -> StoreResult<Option<StoredPersona>> {
        Ok(lock(&self.personas).get(key).cloned())
    }
    fn put_persona(&self, key: &str, persona: StoredPersona) -> StoreResult<()> {
        lock(&self.personas).insert(key.into(), persona);
        Ok(())
    }
}

impl IconStore for MemoryStore {
    fn get(&self, key: &str) -> StoreResult<Option<IconCacheEntry>> {
        Ok(lock(&self.icons).get(key).cloned())
    }
    fn put(&self, key: &str, entry: IconCacheEntry) -> StoreResult<()> {
        lock(&self.icons).insert(key.into(), entry);
        Ok(())
    }
}

impl MemoryStore {
    /// The deps bundle with this store behind every port.
    pub fn deps(self: &std::sync::Arc<Self>) -> KnowledgeDeps {
        KnowledgeDeps {
            graphs: self.clone(),
            students: self.clone(),
            vectors: self.clone(),
            social: self.clone(),
            icons: self.clone(),
            models: self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::knowledge::engine::pure::Relationship;

    fn concept(id: &str) -> GraphNode {
        serde_json::from_value(json!({ "id": id, "type": "Concept", "properties": { "name": id } }))
            .unwrap()
    }

    fn edge(id: &str, from: &str, to: &str) -> Relationship {
        serde_json::from_value(json!({ "id": id, "source": from, "target": to, "type": "hasChild", "direction": "forward" })).unwrap()
    }

    fn seeded() -> MemoryStore {
        let store = MemoryStore::at(1_000.0);
        store
            .save_graph("u", &NodeBasedKnowledgeGraph::new("g", 1.0), None)
            .unwrap();
        store
    }

    #[test]
    fn save_creates_at_version_one_then_bumps() {
        let store = seeded();
        let g = store.get_graph("u", "g").unwrap();
        assert_eq!(g.version, Some(1.0));
        let saved = store.save_graph("u", &g, g.version).unwrap();
        assert_eq!(saved.version, Some(2.0));
    }

    #[test]
    fn a_stale_expected_version_conflicts_and_an_expected_version_needs_an_existing_graph() {
        let store = seeded();
        let g = store.get_graph("u", "g").unwrap();
        store.save_graph("u", &g, g.version).unwrap();
        let err = store.save_graph("u", &g, g.version).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        let err = store
            .save_graph("u", &NodeBasedKnowledgeGraph::new("new", 1.0), Some(1.0))
            .unwrap_err();
        assert_eq!(err.message, "Graph new not found (expected version 1)");
    }

    #[test]
    fn a_missing_graph_is_a_plain_error() {
        let err = seeded().get_graph("u", "nope").unwrap_err();
        assert_eq!(
            (err.kind, err.message.as_str()),
            (ErrorKind::Other, "Graph nope not found")
        );
        assert!(!seeded().graph_exists("u", "nope"));
        assert!(seeded().graph_exists("u", "g"));
    }

    #[test]
    fn owners_do_not_see_each_others_graphs() {
        let store = seeded();
        store
            .save_graph("v", &NodeBasedKnowledgeGraph::new("h", 1.0), None)
            .unwrap();
        assert_eq!(store.list_graph_ids("u").unwrap(), ["g"]);
        assert_eq!(store.list_graph_ids("v").unwrap(), ["h"]);
        assert!(store.get_graph("v", "g").is_err());
    }

    #[test]
    fn node_edits_load_edit_and_save_at_the_loaded_version() {
        let store = seeded();
        store.create_node("u", "g", concept("a")).unwrap();
        store.create_node("u", "g", concept("b")).unwrap();
        store
            .create_relationship("u", "g", edge("r1", "a", "b"))
            .unwrap();
        assert_eq!(store.get_graph("u", "g").unwrap().version, Some(4.0));
        let again = store
            .create_relationship("u", "g", edge("r2", "a", "b"))
            .unwrap();
        assert_eq!(again.id, "r1");
        assert_eq!(store.get_graph("u", "g").unwrap().version, Some(4.0));
        assert!(store.create_node("u", "g", concept("a")).is_err());
        store.delete_node("u", "g", "a").unwrap();
        let g = store.get_graph("u", "g").unwrap();
        assert!(g.relationships.is_empty() && g.nodes.len() == 1);
        assert!(store.get_node("u", "g", "a").unwrap().is_none());
    }

    #[test]
    fn relationships_filter_by_direction() {
        let store = seeded();
        for id in ["a", "b", "c"] {
            store.create_node("u", "g", concept(id)).unwrap();
        }
        store
            .create_relationship("u", "g", edge("r1", "a", "b"))
            .unwrap();
        store
            .create_relationship("u", "g", edge("r2", "b", "c"))
            .unwrap();
        let ids = |d| {
            store
                .get_relationships("u", "g", Some("b"), d)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Direction::Outgoing), ["r2"]);
        assert_eq!(ids(Direction::Incoming), ["r1"]);
        assert_eq!(ids(Direction::Both), ["r1", "r2"]);
        assert_eq!(
            store
                .get_relationships("u", "g", None, Direction::Both)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn move_graph_relocates_and_stamps_the_author() {
        let store = seeded();
        let moved = store.move_graph("u", "pub", "g", Some("u")).unwrap();
        assert_eq!(moved.author_uid.as_deref(), Some("u"));
        assert!(store.get_graph("u", "g").is_err());
        assert!(store.get_graph("pub", "g").is_ok());
        let err = store.move_graph("pub", "pub", "g", None).unwrap_err();
        assert!(err.message.contains("identical"));
    }

    #[test]
    fn preferences_merge_and_progress_upserts() {
        let store = MemoryStore::at(5.0);
        assert!(store.get_preferences("u").unwrap().is_none());
        store
            .set_preferences("u", json!({ "a": 1 }).as_object().unwrap())
            .unwrap();
        let merged = store
            .set_preferences("u", json!({ "b": 2 }).as_object().unwrap())
            .unwrap();
        assert_eq!(Value::Object(merged), json!({ "a": 1, "b": 2 }));
        assert!(store
            .get_progress_for_concept("u", "g", "u", "c")
            .unwrap()
            .is_none());
        store
            .upsert_progress(
                "u",
                "g",
                "u",
                "c",
                json!({ "masteryLevel": 1 }).as_object().unwrap(),
            )
            .unwrap();
        let node = store
            .upsert_progress(
                "u",
                "g",
                "u",
                "c",
                json!({ "lastStudied": 9 }).as_object().unwrap(),
            )
            .unwrap();
        assert_eq!(
            Value::Object(node.properties),
            json!({ "masteryLevel": 1, "lastStudied": 9 })
        );
    }
}
