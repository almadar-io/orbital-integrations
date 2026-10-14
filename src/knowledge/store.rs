//! The ports the `knowledge` actions run against: the Rust twin of `KnowledgeIntegrationDeps` plus the
//! services behind `KnowledgeGraphAccessLayer`. Every method is synchronous (calls run to completion on the
//! caller's thread, like the rest of this crate). A host implements these over Firestore/Chroma; tests use
//! `MemoryStore`. The shape is shared with the action ports and stays stable.
//!
//! Errors: a store raises `KnowledgeError`. TS plain `Error`s (a missing graph, a missing node) are
//! `ErrorKind::Other`; the action layer's own refusals use the other kinds.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::engine::mutations;
use super::engine::pure::{GraphNode, NodeBasedKnowledgeGraph, Relationship};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    Validation,
    Conflict,
    Service,
    Cancelled,
    /// A plain `Error` (no code), as `BaseIntegration.handleError` reports it.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeError {
    pub kind: ErrorKind,
    pub message: String,
}

impl KnowledgeError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }
    pub fn service(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Service, message)
    }
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "The call was cancelled")
    }
}

impl fmt::Display for KnowledgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for KnowledgeError {}

pub type StoreResult<T> = Result<T, KnowledgeError>;

fn plain(message: String) -> KnowledgeError {
    KnowledgeError::other(message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
    Both,
}

/// Graph persistence (`GraphLoader`) and the node/relationship edits layered on it. Implement the
/// six required methods; the edits default to load -> pure edit (`engine::mutations`) -> save at the loaded version.
pub trait GraphStore: Send + Sync {
    /// Milliseconds since the epoch; hosts and tests override it for determinism.
    fn now_ms(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }

    /// `Graph {id} not found` (`ErrorKind::Other`) when absent.
    fn get_graph(&self, uid: &str, graph_id: &str) -> StoreResult<NodeBasedKnowledgeGraph>;
    /// Create or version-checked update; returns the saved graph (version bumped, `updatedAt` stamped).
    fn save_graph(
        &self,
        uid: &str,
        graph: &NodeBasedKnowledgeGraph,
        expected_version: Option<f64>,
    ) -> StoreResult<NodeBasedKnowledgeGraph>;
    fn delete_graph(&self, uid: &str, graph_id: &str) -> StoreResult<()>;
    fn list_graph_ids(&self, uid: &str) -> StoreResult<Vec<String>>;

    fn graph_exists(&self, uid: &str, graph_id: &str) -> bool {
        self.get_graph(uid, graph_id).is_ok()
    }

    /// Save to the destination first, then delete the source (`GraphLoader.moveGraph`).
    fn move_graph(
        &self,
        from_uid: &str,
        to_uid: &str,
        graph_id: &str,
        author_uid: Option<&str>,
    ) -> StoreResult<NodeBasedKnowledgeGraph> {
        if from_uid == to_uid {
            return Err(plain(format!(
                "moveGraph: source and destination owner are identical ({from_uid})"
            )));
        }
        let mut graph = self.get_graph(from_uid, graph_id)?;
        if let Some(author) = author_uid {
            graph.author_uid = Some(author.to_string());
        }
        let saved = self.save_graph(to_uid, &graph, None)?;
        self.delete_graph(from_uid, graph_id)?;
        Ok(saved)
    }

    fn get_node(&self, uid: &str, graph_id: &str, node_id: &str) -> StoreResult<Option<GraphNode>> {
        Ok(self.get_graph(uid, graph_id)?.nodes.get(node_id).cloned())
    }

    fn create_node(&self, uid: &str, graph_id: &str, node: GraphNode) -> StoreResult<GraphNode> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        let created = mutations::create_node(&mut graph, node, self.now_ms()).map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        Ok(created)
    }

    fn update_node(
        &self,
        uid: &str,
        graph_id: &str,
        node_id: &str,
        updates: &Map<String, Value>,
    ) -> StoreResult<GraphNode> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        let updated =
            mutations::update_node(&mut graph, node_id, updates, self.now_ms()).map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        Ok(updated)
    }

    fn delete_node(&self, uid: &str, graph_id: &str, node_id: &str) -> StoreResult<()> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        mutations::delete_node(&mut graph, node_id, self.now_ms()).map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        Ok(())
    }

    /// The edges touching `node_id` (all edges when `None`), by `direction`.
    fn get_relationships(
        &self,
        uid: &str,
        graph_id: &str,
        node_id: Option<&str>,
        direction: Direction,
    ) -> StoreResult<Vec<Relationship>> {
        let graph = self.get_graph(uid, graph_id)?;
        Ok(graph
            .relationships
            .into_iter()
            .filter(|r| match node_id {
                None => true,
                Some(id) => match direction {
                    Direction::Outgoing => r.source == id,
                    Direction::Incoming => r.target == id,
                    Direction::Both => r.source == id || r.target == id,
                },
            })
            .collect())
    }

    fn create_relationship(
        &self,
        uid: &str,
        graph_id: &str,
        relationship: Relationship,
    ) -> StoreResult<Relationship> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        let (rel, changed) =
            mutations::create_relationship(&mut graph, relationship, self.now_ms())
                .map_err(plain)?;
        if changed {
            self.save_graph(uid, &graph, version)?;
        }
        Ok(rel)
    }

    fn delete_relationship(&self, uid: &str, graph_id: &str, rel_id: &str) -> StoreResult<()> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        mutations::delete_relationship(&mut graph, rel_id, self.now_ms()).map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        Ok(())
    }
}

/// The learner's profile preferences and per-concept progress (`StudentProfileService` / `StudentDataService`).
/// Records are JSON objects in their TS field names.
pub trait StudentStore: Send + Sync {
    fn get_preferences(&self, uid: &str) -> StoreResult<Option<Map<String, Value>>>;
    /// Merge `patch` into the preferences and return the result.
    fn set_preferences(
        &self,
        uid: &str,
        patch: &Map<String, Value>,
    ) -> StoreResult<Map<String, Value>>;
    fn get_progress_for_concept(
        &self,
        owner_uid: &str,
        graph_id: &str,
        student_uid: &str,
        concept_id: &str,
    ) -> StoreResult<Option<Map<String, Value>>>;
    /// Create or merge the learner's progress node for a concept; returns the node.
    fn upsert_progress(
        &self,
        owner_uid: &str,
        graph_id: &str,
        student_uid: &str,
        concept_id: &str,
        data: &Map<String, Value>,
    ) -> StoreResult<GraphNode>;
    /// Drop `graph_id` from the learner's enrolled-graph index (`removeEnrolledGraph`).
    fn remove_enrolled_graph(
        &self,
        owner_uid: &str,
        student_uid: &str,
        graph_id: &str,
    ) -> StoreResult<()>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorHit {
    #[serde(rename = "graphId")]
    pub graph_id: String,
    #[serde(rename = "nodeId")]
    pub node_id: String,
    pub score: f64,
    #[serde(rename = "nodeType")]
    pub node_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        rename = "canonicalName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub canonical_name: Option<String>,
    pub text: String,
}

/// Distinct canonical concepts a user owns (`CanonicalConceptSet`): names only, no node text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalConceptSet {
    #[serde(rename = "canonicalNames")]
    pub canonical_names: Vec<String>,
    pub count: usize,
}

/// Semantic search and index upkeep (`KnowledgeVectorService`, Chroma). Best effort: index writes may be no-ops.
pub trait VectorIndex: Send + Sync {
    fn upsert_nodes(&self, graph_id: &str, nodes: &[GraphNode], uid: &str) -> StoreResult<()>;
    fn delete_graph(&self, graph_id: &str) -> StoreResult<()>;
    /// Search across `graph_ids` (the caller passes only graphs the learner may see); empty ids give no hits.
    fn search_by_text_cross(
        &self,
        graph_ids: &[String],
        query: &str,
        k: usize,
        node_types: Option<&[String]>,
    ) -> StoreResult<Vec<VectorHit>>;
    fn get_canonical_concepts_by_owner(&self, uid: &str) -> StoreResult<CanonicalConceptSet>;
}

/// One chunk of a streamed model reply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelChunk {
    pub content: String,
    pub done: bool,
}

pub struct ModelStream {
    pub model: String,
    pub chunks: Box<dyn Iterator<Item = StoreResult<ModelChunk>> + Send>,
}

/// The model calls the actions make (`engine/server/llm.ts`): streaming chat, one-shot chat and structured
/// output. Requests and replies are JSON in the TS `LLMRequest` / schema shapes, so the action ports own them.
pub trait ModelPort: Send + Sync {
    fn stream(&self, request: &Value) -> StoreResult<ModelStream>;
    fn call(&self, request: &Value) -> StoreResult<Value>;
    fn generate_structured(&self, request: &Value) -> StoreResult<Value>;
}

// ---- social ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeMember {
    pub uid: String,
    #[serde(rename = "lastSeenOnNode")]
    pub last_seen_on_node: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerTokenRecord {
    #[serde(rename = "peerUid")]
    pub peer_uid: String,
    #[serde(rename = "issuedTo")]
    pub issued_to: String,
    #[serde(rename = "nodeKey")]
    pub node_key: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredBadge {
    pub id: String,
    pub label: String,
    /// `subtopic` | `friction`
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionOrigin {
    /// `concept` | `path`
    pub kind: String,
    #[serde(rename = "canonicalId")]
    pub canonical_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredConnection {
    #[serde(rename = "callerUid")]
    pub caller_uid: String,
    #[serde(rename = "peerUid")]
    pub peer_uid: String,
    pub participants: Vec<String>,
    pub origin: ConnectionOrigin,
    pub badges: Vec<StoredBadge>,
    #[serde(rename = "activeBadgeId")]
    pub active_badge_id: String,
    /// `active` | `archived`
    pub status: String,
    #[serde(rename = "createdAt")]
    pub created_at: f64,
    #[serde(rename = "lastActivityAt")]
    pub last_activity_at: f64,
    #[serde(
        rename = "archivedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub archived_at: Option<f64>,
}

/// The patchable connection fields; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionPatch {
    pub active_badge_id: Option<String>,
    pub status: Option<String>,
    pub archived_at: Option<f64>,
    pub last_activity_at: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageModeration {
    pub score: f64,
    pub flagged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    #[serde(rename = "senderUid")]
    pub sender_uid: String,
    pub content: String,
    #[serde(rename = "createdAt")]
    pub created_at: f64,
    #[serde(rename = "activeBadgeId")]
    pub active_badge_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderation: Option<MessageModeration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMessageRow {
    pub id: String,
    #[serde(flatten)]
    pub message: StoredMessage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredPersona {
    pub name: String,
    pub description: String,
    #[serde(rename = "portraitUrl")]
    pub portrait_url: String,
    pub greeting: String,
    pub bio: String,
    #[serde(rename = "cachedAt")]
    pub cached_at: f64,
}

/// Peer activity, peer tokens, connections and concept personas (`SocialStore`).
pub trait SocialStore: Send + Sync {
    fn touch_member(&self, node_key: &str, uid: &str, at: f64) -> StoreResult<()>;
    fn list_members_since(&self, node_key: &str, cutoff: f64) -> StoreResult<Vec<NodeMember>>;
    fn put_peer_token(&self, token: &str, record: PeerTokenRecord) -> StoreResult<()>;
    fn get_peer_token(&self, token: &str) -> StoreResult<Option<PeerTokenRecord>>;
    fn delete_peer_token(&self, token: &str) -> StoreResult<()>;
    fn create_connection(&self, connection: StoredConnection) -> StoreResult<String>;
    fn get_connection(&self, id: &str) -> StoreResult<Option<StoredConnection>>;
    fn update_connection(&self, id: &str, patch: ConnectionPatch) -> StoreResult<()>;
    fn add_message(&self, connection_id: &str, message: StoredMessage) -> StoreResult<String>;
    fn list_messages(&self, connection_id: &str) -> StoreResult<Vec<StoredMessageRow>>;
    fn set_message_moderation(
        &self,
        connection_id: &str,
        message_id: &str,
        moderation: MessageModeration,
    ) -> StoreResult<()>;
    fn get_persona(&self, key: &str) -> StoreResult<Option<StoredPersona>>;
    fn put_persona(&self, key: &str, persona: StoredPersona) -> StoreResult<()>;
}

// ---- icons ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedIcon {
    pub icon: String,
    /// `themed` | `dark` | `light`
    pub tone: String,
}

/// One cached lookup; `icon: None` is a cached miss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IconCacheEntry {
    pub label: String,
    pub icon: Option<ResolvedIcon>,
    #[serde(rename = "fetchedAt")]
    pub fetched_at: f64,
}

pub trait IconStore: Send + Sync {
    fn get(&self, key: &str) -> StoreResult<Option<IconCacheEntry>>;
    fn put(&self, key: &str, entry: IconCacheEntry) -> StoreResult<()>;
}

/// Everything an action reads or writes through; the host supplies it once.
#[derive(Clone)]
pub struct KnowledgeDeps {
    pub graphs: Arc<dyn GraphStore>,
    pub students: Arc<dyn StudentStore>,
    pub vectors: Arc<dyn VectorIndex>,
    pub social: Arc<dyn SocialStore>,
    pub icons: Arc<dyn IconStore>,
    pub models: Arc<dyn ModelPort>,
}
