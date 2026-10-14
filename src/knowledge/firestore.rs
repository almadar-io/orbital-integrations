//! The Firestore REST store behind the `knowledge` ports: the twin of the TS `GraphLoader`,
//! `StudentProfileService`, `StudentDataService`, `FirestoreSocialStore` and `FirestoreIconStore`.
//!
//! Collections and field names are the TS layout: `users/{uid}/knowledgeGraphs/{graphId}` (one document per
//! graph), `users/{uid}/profile/student`, `node-activity/{key}/members/{uid}`, `peer-tokens/{token}`,
//! `connections/{id}[/messages/{id}]`, `concept-personas/{key}`, `concept-icons/{key}`.
//!
//! The wire requests are the ones the Firebase Admin SDK sends in REST mode (`FIRESTORE_PREFER_REST`): `batchGet`
//! for a read, `commit` for a write, `runQuery` for a query, and a transaction that begins lazily inside its
//! first read (`newTransaction`), so a recorded exchange replays against this store request for request.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use super::engine::pure::{GraphNode, NodeBasedKnowledgeGraph, OrderedMap, Relationship, Stored};
use super::store::*;
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::json::number;
use crate::transport::{HttpRequest, Transport};
use crate::uricomponent::encode_uri_component;
use crate::wallclock::{system_clock, Clock};

const GRAPHS: &str = "knowledgeGraphs";
const GRAPH_TTL_MS: f64 = 5.0 * 60.0 * 1000.0;
const DEFAULT_PROJECT: &str = "demo-almadar";
const PRODUCTION_BASE: &str = "https://firestore.googleapis.com";
const OP_GREATER_THAN_OR_EQUAL: u8 = 4;
const ASCENDING: u8 = 1;

/// How requests authenticate: the emulator accepts any owner token; a project needs a bearer token the host mints.
#[derive(Clone)]
pub enum FirestoreAuth {
    Emulator,
    Token(Arc<dyn Fn() -> Result<String, String> + Send + Sync>),
}

#[derive(Clone)]
pub struct FirestoreConfig {
    pub base: String,
    pub project_id: String,
    pub database_id: String,
    pub auth: FirestoreAuth,
}

impl FirestoreConfig {
    pub fn emulator(host: &str, project_id: &str) -> Self {
        Self {
            base: format!("http://{host}"),
            project_id: project_id.to_string(),
            database_id: "(default)".to_string(),
            auth: FirestoreAuth::Emulator,
        }
    }

    /// The target `@almadar/db/firebase` picks from the environment: `FIRESTORE_EMULATOR_HOST` (the emulator,
    /// project `FIREBASE_PROJECT_ID` or `demo-almadar`), else `FIREBASE_PROJECT_ID` authenticated by a service
    /// account (`FIREBASE_SERVICE_ACCOUNT_PATH`, or `FIREBASE_CLIENT_EMAIL` + `FIREBASE_PRIVATE_KEY`), else a bearer
    /// token the host mints (`FIRESTORE_ACCESS_TOKEN`). Application-default credentials are not ported.
    pub fn from_credentials(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let database_id = creds
            .get("FIREBASE_DATABASE_ID")
            .or_else(|| creds.get("FB_DB_ID"))
            .unwrap_or("(default)")
            .to_string();
        if let Some(host) = creds.get("FIRESTORE_EMULATOR_HOST") {
            let project = creds.get("FIREBASE_PROJECT_ID").unwrap_or(DEFAULT_PROJECT);
            let mut config = Self::emulator(host, project);
            config.database_id = database_id;
            return Ok(config);
        }
        let Some(project) = creds.get("FIREBASE_PROJECT_ID") else {
            return Err(IntegrationError::plain(
                "@almadar/db/firebase: Cannot initialize Firebase — no credentials found. Set FIREBASE_PROJECT_ID, or FIRESTORE_EMULATOR_HOST.",
            ));
        };
        let auth = Self::production_auth(creds, transport)?;
        Ok(Self {
            base: PRODUCTION_BASE.to_string(),
            project_id: project.to_string(),
            database_id,
            auth,
        })
    }

    fn production_auth(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<FirestoreAuth, IntegrationError> {
        #[cfg(feature = "google")]
        {
            let key = match (creds.get("FIREBASE_SERVICE_ACCOUNT_PATH"), creds.get("FIREBASE_CLIENT_EMAIL"), creds.get("FIREBASE_PRIVATE_KEY")) {
                (Some(path), _, _) => Some(
                    std::fs::read_to_string(path)
                        .map_err(|e| IntegrationError::plain(format!("ENOENT: no such file or directory, open '{path}' ({e})")))?,
                ),
                (None, Some(email), Some(private_key)) => {
                    Some(json!({ "client_email": email, "private_key": private_key.replace("\\n", "\n") }).to_string())
                }
                _ => None,
            };
            if let Some(key) = key {
                use crate::google::{Gax, JwtSource, TokenSource};
                let clock: crate::jswire::Clock = crate::jswire::system_clock();
                let source = JwtSource::from_key(
                    &key,
                    "https://www.googleapis.com/auth/datastore",
                    None,
                    clock,
                )?;
                let gax = Gax {
                    transport,
                    sleeper: Arc::new(std::thread::sleep),
                };
                return Ok(FirestoreAuth::Token(Arc::new(move || {
                    source.access_token(&gax).map_err(|e| e.message)
                })));
            }
        }
        #[cfg(not(feature = "google"))]
        let _ = transport;
        let token = creds.get("FIRESTORE_ACCESS_TOKEN").map(str::to_string);
        Ok(FirestoreAuth::Token(Arc::new(move || {
            token.clone().ok_or_else(|| "Could not load the default credentials: set FIREBASE_SERVICE_ACCOUNT_PATH, or FIRESTORE_ACCESS_TOKEN".to_string())
        })))
    }
}

// ---- the Firestore value wire format ----

fn is_whole(f: f64) -> bool {
    f.is_finite() && f.fract() == 0.0 && f.abs() < 9_223_372_036_854_775_000.0
}

/// A JSON value as a Firestore `Value`.
pub fn to_wire(value: &Value) -> Value {
    match value {
        Value::Null => json!({ "nullValue": null }),
        Value::Bool(b) => json!({ "booleanValue": b }),
        Value::Number(n) => match n.as_f64() {
            Some(f) if is_whole(f) => json!({ "integerValue": (f as i64).to_string() }),
            Some(f) => json!({ "doubleValue": f }),
            None => json!({ "nullValue": null }),
        },
        Value::String(s) => json!({ "stringValue": s }),
        Value::Array(items) if items.is_empty() => json!({ "arrayValue": {} }),
        Value::Array(items) => {
            json!({ "arrayValue": { "values": items.iter().map(to_wire).collect::<Vec<_>>() } })
        }
        Value::Object(map) => json!({ "mapValue": { "fields": fields_to_wire(map) } }),
    }
}

pub fn fields_to_wire(map: &Map<String, Value>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), to_wire(v)))
            .collect::<Map<String, Value>>(),
    )
}

/// A Firestore `Value` as JSON.
pub fn from_wire(value: &Value) -> Value {
    let Some(obj) = value.as_object() else {
        return Value::Null;
    };
    if let Some(s) = obj.get("stringValue").and_then(Value::as_str) {
        return Value::String(s.to_string());
    }
    if let Some(i) = obj.get("integerValue") {
        let parsed = i
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| i.as_f64());
        return parsed.map_or(Value::Null, number);
    }
    if let Some(d) = obj.get("doubleValue").and_then(Value::as_f64) {
        return number(d);
    }
    if let Some(b) = obj.get("booleanValue").and_then(Value::as_bool) {
        return Value::Bool(b);
    }
    if obj.contains_key("nullValue") {
        return Value::Null;
    }
    if let Some(arr) = obj.get("arrayValue") {
        let values = arr.get("values").and_then(Value::as_array);
        return Value::Array(values.map_or_else(Vec::new, |v| v.iter().map(from_wire).collect()));
    }
    if let Some(map) = obj.get("mapValue") {
        let fields = map.get("fields").and_then(Value::as_object);
        return Value::Object(fields.map_or_else(Map::new, fields_from_wire));
    }
    for key in ["timestampValue", "referenceValue"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            return Value::String(s.to_string());
        }
    }
    Value::Null
}

pub fn fields_from_wire(fields: &Map<String, Value>) -> Map<String, Value> {
    fields
        .iter()
        .map(|(k, v)| (k.clone(), from_wire(v)))
        .collect()
}

// ---- the REST client ----

type IdGen = Arc<dyn Fn() -> String + Send + Sync>;

const AUTO_ID_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A Firestore auto-id: 20 characters from `[A-Za-z0-9]`.
pub fn random_auto_id() -> String {
    let state = RandomState::new();
    let mut id = String::with_capacity(20);
    let mut round = 0u64;
    while id.len() < 20 {
        let mut hasher = state.build_hasher();
        hasher.write_u64(round);
        round += 1;
        for byte in hasher.finish().to_le_bytes() {
            if id.len() < 20 && byte <= 247 {
                id.push(AUTO_ID_CHARS[usize::from(byte) % 62] as char);
            }
        }
    }
    id
}

fn plain(message: impl Into<String>) -> KnowledgeError {
    KnowledgeError::other(message)
}

struct Client {
    config: FirestoreConfig,
    transport: Arc<dyn Transport>,
}

struct BatchGet {
    docs: Vec<Option<Map<String, Value>>>,
    transaction: Option<String>,
}

impl Client {
    fn doc_name(&self, path: &str) -> String {
        format!(
            "projects/{}/databases/{}/documents/{path}",
            self.config.project_id, self.config.database_id
        )
    }

    fn bearer(&self) -> StoreResult<String> {
        match &self.config.auth {
            FirestoreAuth::Emulator => Ok("owner".to_string()),
            FirestoreAuth::Token(token) => token().map_err(plain),
        }
    }

    fn post(&self, suffix: &str, body: &Value) -> StoreResult<Value> {
        let url = format!(
            "{}/v1/projects/{}/databases/{}/documents{suffix}?$alt=json%3Benum-encoding=int",
            self.config.base, self.config.project_id, self.config.database_id
        );
        let request = HttpRequest {
            method: "POST".to_string(),
            url,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "*/*".to_string()),
                (
                    "authorization".to_string(),
                    format!("Bearer {}", self.bearer()?),
                ),
            ],
            body: body.to_string().into_bytes(),
            timeout: None,
        };
        let response = self
            .transport
            .send(&request)
            .map_err(|e| plain(format!("{e:?}")))?;
        if !response.is_success() {
            return Err(plain(response.text().into_owned()));
        }
        serde_json::from_str(&response.text())
            .map_err(|e| plain(format!("Firestore answered an unreadable reply: {e}")))
    }

    fn batch_get(&self, paths: &[&str], new_transaction: bool) -> StoreResult<BatchGet> {
        let mut body =
            json!({ "documents": paths.iter().map(|p| self.doc_name(p)).collect::<Vec<_>>() });
        if new_transaction {
            body["newTransaction"] = json!({ "readWrite": {} });
        }
        let reply = self.post(":batchGet", &body)?;
        let elements = reply.as_array().cloned().unwrap_or_default();
        let mut transaction = None;
        let mut docs: Vec<Option<Map<String, Value>>> = vec![None; paths.len()];
        for element in &elements {
            if let Some(tx) = element.get("transaction").and_then(Value::as_str) {
                transaction = Some(tx.to_string());
            }
            if let Some(found) = element.get("found") {
                let name = found
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let fields = found
                    .get("fields")
                    .and_then(Value::as_object)
                    .map_or_else(Map::new, fields_from_wire);
                if let Some(at) = paths.iter().position(|p| name == self.doc_name(p)) {
                    docs[at] = Some(fields);
                }
            }
        }
        Ok(BatchGet { docs, transaction })
    }

    fn get(&self, path: &str) -> StoreResult<Option<Map<String, Value>>> {
        Ok(self.batch_get(&[path], false)?.docs.remove(0))
    }

    fn commit(&self, writes: Vec<Value>, transaction: Option<&str>) -> StoreResult<()> {
        let mut body = json!({ "writes": writes });
        if let Some(tx) = transaction {
            body["transaction"] = Value::String(tx.to_string());
        }
        self.post(":commit", &body).map(|_| ())
    }

    fn set_write(&self, path: &str, data: &Map<String, Value>) -> Value {
        json!({ "update": { "fields": fields_to_wire(data), "name": self.doc_name(path) } })
    }

    fn masked_write(&self, path: &str, fields: &Map<String, Value>, must_exist: bool) -> Value {
        let mut paths: Vec<&String> = fields.keys().collect();
        paths.sort();
        let mut write = json!({
            "update": { "fields": fields_to_wire(fields), "name": self.doc_name(path) },
            "updateMask": { "fieldPaths": paths },
        });
        if must_exist {
            write["currentDocument"] = json!({ "exists": true });
        }
        write
    }

    fn set(&self, path: &str, data: &Map<String, Value>) -> StoreResult<()> {
        self.commit(vec![self.set_write(path, data)], None)
    }

    /// `set(data, { merge: true })`.
    fn set_merge(&self, path: &str, data: &Map<String, Value>) -> StoreResult<()> {
        self.commit(vec![self.masked_write(path, data, false)], None)
    }

    /// `update(fields)`: fails when the document does not exist.
    fn update(&self, path: &str, fields: &Map<String, Value>) -> StoreResult<()> {
        self.commit(vec![self.masked_write(path, fields, true)], None)
    }

    fn create(&self, path: &str, data: &Map<String, Value>) -> StoreResult<()> {
        let mut write = self.set_write(path, data);
        write["currentDocument"] = json!({ "exists": false });
        self.commit(vec![write], None)
    }

    fn delete(&self, path: &str) -> StoreResult<()> {
        self.commit(vec![json!({ "delete": self.doc_name(path) })], None)
    }

    /// The documents of `collection` under `parent` (`""` for a top-level collection) that match `structured`.
    fn query(
        &self,
        parent: &str,
        collection: &str,
        structured: Value,
    ) -> StoreResult<Vec<(String, Map<String, Value>)>> {
        let mut query = json!({ "from": [{ "collectionId": collection }] });
        if let (Some(target), Some(extra)) = (query.as_object_mut(), structured.as_object()) {
            for (k, v) in extra {
                target.insert(k.clone(), v.clone());
            }
        }
        let suffix = if parent.is_empty() {
            ":runQuery".to_string()
        } else {
            let encoded: Vec<String> = parent.split('/').map(encode_uri_component).collect();
            format!("/{}:runQuery", encoded.join("/"))
        };
        let reply = self.post(&suffix, &json!({ "structuredQuery": query }))?;
        let mut rows = Vec::new();
        for element in reply.as_array().cloned().unwrap_or_default() {
            let Some(document) = element.get("document") else {
                continue;
            };
            let id = document
                .get("name")
                .and_then(Value::as_str)
                .and_then(|n| n.rsplit('/').next())
                .unwrap_or_default()
                .to_string();
            let fields = document
                .get("fields")
                .and_then(Value::as_object)
                .map_or_else(Map::new, fields_from_wire);
            rows.push((id, fields));
        }
        Ok(rows)
    }
}

// ---- document shape checks (the TS zod schemas) ----

fn obj(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn is_str(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_string)
}

fn graph_from_doc(doc: &Map<String, Value>) -> Option<NodeBasedKnowledgeGraph> {
    let well_formed = is_str(doc.get("id"))
        && doc.get("nodes").is_some_and(Value::is_object)
        && doc.get("nodeTypes").is_some_and(Value::is_object)
        && doc.get("relationships").is_some_and(Value::is_array);
    if !well_formed {
        return None;
    }
    serde_json::from_value(Value::Object(doc.clone())).ok()
}

/// A relationship written as an object literal rather than through `createRelationship`: TS leaves `strength` and
/// `metadata` out of its stored form instead of storing them as null.
pub fn literal_relationship(
    source: &str,
    target: &str,
    rel_type: &str,
    now: f64,
    metadata: Option<Map<String, Value>>,
) -> Relationship {
    Relationship {
        id: relationship_id(source, target, rel_type),
        source: source.to_string(),
        target: target.to_string(),
        rel_type: rel_type.to_string(),
        direction: Some("forward".to_string()),
        strength: Stored::Absent,
        metadata: metadata.map_or(Stored::Absent, Stored::Value),
        created_at: match number(now) {
            Value::Number(n) => Some(n),
            _ => None,
        },
        source_graph_id: None,
        target_graph_id: None,
    }
}

/// A graph as the document it is stored as.
fn graph_to_doc(graph: &NodeBasedKnowledgeGraph) -> Map<String, Value> {
    match serde_json::to_value(graph) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

const MERGED_INDEX_TYPES: [&str; 10] = [
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
];

fn max_stamp(
    a: Option<&serde_json::Number>,
    b: Option<&serde_json::Number>,
) -> Option<serde_json::Number> {
    let x = a.and_then(serde_json::Number::as_f64).unwrap_or(0.0);
    let y = b.and_then(serde_json::Number::as_f64).unwrap_or(0.0);
    match number(x.max(y)) {
        Value::Number(n) => Some(n),
        _ => None,
    }
}

/// `mergeGraphMutations`: the incoming graph laid over the stored one when the stored version moved on.
pub fn merge_graph_mutations(
    current: &NodeBasedKnowledgeGraph,
    incoming: &NodeBasedKnowledgeGraph,
    now: f64,
) -> NodeBasedKnowledgeGraph {
    let mut nodes: OrderedMap<GraphNode> = OrderedMap::new();
    for (id, node) in current.nodes.iter() {
        nodes.insert(id.clone(), node.clone());
    }
    for (id, node) in incoming.nodes.iter() {
        let merged = match nodes.get(id) {
            Some(existing) => {
                let mut properties = existing.properties.clone();
                for (k, v) in &node.properties {
                    properties.insert(k.clone(), v.clone());
                }
                GraphNode {
                    id: node.id.clone(),
                    node_type: node.node_type.clone(),
                    properties,
                    created_at: node
                        .created_at
                        .clone()
                        .or_else(|| existing.created_at.clone()),
                    updated_at: max_stamp(existing.updated_at.as_ref(), node.updated_at.as_ref()),
                }
            }
            None => node.clone(),
        };
        nodes.insert(id.clone(), merged);
    }

    let mut relationships: Vec<Relationship> = Vec::new();
    for rel in current
        .relationships
        .iter()
        .chain(incoming.relationships.iter())
    {
        match relationships.iter().position(|r| r.id == rel.id) {
            Some(at) => relationships[at] = rel.clone(),
            None => relationships.push(rel.clone()),
        }
    }

    let mut node_types: OrderedMap<Vec<String>> = OrderedMap::new();
    for kind in MERGED_INDEX_TYPES {
        node_types.insert(kind.to_string(), Vec::new());
    }
    for (id, node) in nodes.iter() {
        if let Some(list) = node_types.get_mut(&node.node_type) {
            if !list.contains(id) {
                list.push(id.clone());
            }
        }
    }

    let mut extra = current.extra.clone();
    for (k, v) in &incoming.extra {
        extra.insert(k.clone(), v.clone());
    }
    NodeBasedKnowledgeGraph {
        id: incoming.id.clone(),
        seed_concept_id: incoming
            .seed_concept_id
            .clone()
            .or_else(|| current.seed_concept_id.clone()),
        created_at: incoming.created_at,
        updated_at: current.updated_at.max(incoming.updated_at).max(now),
        version: Some(current.version.unwrap_or(0.0) + 1.0),
        author_uid: incoming
            .author_uid
            .clone()
            .or_else(|| current.author_uid.clone()),
        nodes,
        node_types,
        relationships,
        extra,
    }
}

// ---- the store ----

struct Cached {
    graph: NodeBasedKnowledgeGraph,
    expires_at: f64,
}

/// Graphs, the learner's profile, peer/connection records and the icon cache in Firestore.
pub struct FirestoreStore {
    client: Client,
    clock: Clock,
    ids: IdGen,
    cache: Mutex<HashMap<(String, String), Cached>>,
    vectors: Option<Arc<dyn VectorIndex>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn without_nulls_of<T: serde::Serialize>(value: &T) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

impl FirestoreStore {
    pub fn new(config: FirestoreConfig, transport: Arc<dyn Transport>) -> Self {
        Self {
            client: Client { config, transport },
            clock: system_clock(),
            ids: Arc::new(random_auto_id),
            cache: Mutex::new(HashMap::new()),
            vectors: None,
        }
    }

    /// Replace the clock (`Date.now()`), so a replay runs at the instant it was recorded.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Keep a vector index in step with node edits, as the TS `NodeMutationService` does (best effort, never fails an edit).
    pub fn with_vectors(mut self, vectors: Arc<dyn VectorIndex>) -> Self {
        self.vectors = Some(vectors);
        self
    }

    fn sync_to_vectors(&self, uid: &str, graph: &NodeBasedKnowledgeGraph, node: &GraphNode) {
        let Some(vectors) = &self.vectors else { return };
        const CONTENT: [&str; 5] = [
            "Concept",
            "Lesson",
            "PracticeExercise",
            "LearningGoal",
            "Milestone",
        ];
        if CONTENT.contains(&node.node_type.as_str()) {
            let _ = vectors.upsert_nodes(&graph.id, std::slice::from_ref(node), uid);
        }
        if node.node_type == "Concept" {
            self.sync_goal_coverage(uid, graph);
        }
    }

    /// A concept came or went: the goal's coverage embedding lists the path's concepts, so it is refreshed.
    fn sync_goal_coverage(&self, uid: &str, graph: &NodeBasedKnowledgeGraph) {
        let Some(vectors) = &self.vectors else { return };
        let goals: Vec<GraphNode> = graph
            .ids_of("LearningGoal")
            .iter()
            .filter_map(|id| graph.nodes.get(id))
            .cloned()
            .collect();
        if !goals.is_empty() {
            let _ = vectors.upsert_nodes(&graph.id, &goals, uid);
        }
    }

    /// Replace the auto-id source `add()` draws from.
    pub fn with_id_generator(mut self, ids: IdGen) -> Self {
        self.ids = ids;
        self
    }

    /// The deps bundle with this store behind the graph, learner, social and icon ports.
    pub fn deps(
        self: &Arc<Self>,
        vectors: Arc<dyn VectorIndex>,
        models: Arc<dyn ModelPort>,
    ) -> KnowledgeDeps {
        KnowledgeDeps {
            graphs: self.clone(),
            students: self.clone(),
            vectors,
            social: self.clone(),
            icons: self.clone(),
            models,
        }
    }

    fn now(&self) -> f64 {
        (self.clock)() as f64
    }

    fn graph_path(uid: &str, graph_id: &str) -> String {
        format!("users/{uid}/{GRAPHS}/{graph_id}")
    }

    fn profile_path(uid: &str) -> String {
        format!("users/{uid}/profile/student")
    }

    fn cache_get(&self, uid: &str, graph_id: &str) -> Option<NodeBasedKnowledgeGraph> {
        let mut cache = lock(&self.cache);
        let key = (uid.to_string(), graph_id.to_string());
        match cache.get(&key) {
            Some(entry) if self.now() <= entry.expires_at => Some(entry.graph.clone()),
            Some(_) => {
                cache.remove(&key);
                None
            }
            None => None,
        }
    }

    fn cache_set(&self, uid: &str, graph: &NodeBasedKnowledgeGraph) {
        lock(&self.cache).insert(
            (uid.to_string(), graph.id.clone()),
            Cached {
                graph: graph.clone(),
                expires_at: self.now() + GRAPH_TTL_MS,
            },
        );
    }

    fn cache_drop(&self, uid: &str, graph_id: &str) {
        lock(&self.cache).remove(&(uid.to_string(), graph_id.to_string()));
    }

    /// `StudentDataService.loadGraph`: the owner's graph, or the refusal that names the owner.
    fn content_graph(
        &self,
        owner_uid: &str,
        graph_id: &str,
    ) -> StoreResult<NodeBasedKnowledgeGraph> {
        if !self.graph_exists(owner_uid, graph_id) {
            return Err(plain(format!(
                "Content graph {graph_id} not found for owner {owner_uid}; cannot store student data in it"
            )));
        }
        self.get_graph(owner_uid, graph_id)
    }

    // ---- the student profile (users/{uid}/profile/student) ----

    fn default_profile(&self, uid: &str, now: f64) -> Map<String, Value> {
        let mut profile = Map::new();
        profile.insert("uid".into(), json!(uid));
        profile.insert("preferences".into(), json!({ "interests": [] }));
        profile.insert("achievements".into(), json!([]));
        profile.insert("enrolledGraphs".into(), json!([]));
        profile.insert("createdAt".into(), number(now));
        profile.insert("updatedAt".into(), number(now));
        profile
    }

    fn coerce_profile(&self, uid: &str, data: &Map<String, Value>) -> Map<String, Value> {
        let now = self.now();
        let optional_string = |k: &str| data.get(k).is_none_or(|v| v.is_null() || v.is_string());
        let optional_number = |k: &str| data.get(k).is_none_or(|v| v.is_null() || v.is_number());
        let shaped = optional_string("displayName")
            && optional_string("email")
            && optional_string("phone")
            && optional_number("createdAt")
            && optional_number("updatedAt")
            && data
                .get("preferences")
                .is_none_or(|v| v.is_null() || v.is_object())
            && data
                .get("achievements")
                .is_none_or(|v| v.is_null() || v.is_array())
            && data
                .get("enrolledGraphs")
                .is_none_or(|v| v.is_null() || v.is_array());
        if !shaped {
            return self.default_profile(uid, now);
        }
        let or_null = |k: &str| match data.get(k) {
            Some(v) if !v.is_null() => v.clone(),
            _ => Value::Null,
        };
        let or_else = |k: &str, fallback: Value| match data.get(k) {
            Some(v) if !v.is_null() => v.clone(),
            _ => fallback,
        };
        let mut profile = Map::new();
        profile.insert("uid".into(), json!(uid));
        profile.insert("displayName".into(), or_null("displayName"));
        profile.insert("email".into(), or_null("email"));
        profile.insert("phone".into(), or_null("phone"));
        profile.insert(
            "preferences".into(),
            or_else("preferences", json!({ "interests": [] })),
        );
        profile.insert("achievements".into(), or_else("achievements", json!([])));
        profile.insert(
            "enrolledGraphs".into(),
            or_else("enrolledGraphs", json!([])),
        );
        profile.insert("createdAt".into(), or_else("createdAt", number(now)));
        profile.insert("updatedAt".into(), or_else("updatedAt", number(now)));
        profile
    }

    fn load_profile(&self, uid: &str) -> StoreResult<Option<Map<String, Value>>> {
        Ok(self
            .client
            .get(&Self::profile_path(uid))?
            .map(|data| self.coerce_profile(uid, &data)))
    }

    fn save_profile(
        &self,
        uid: &str,
        mut profile: Map<String, Value>,
    ) -> StoreResult<Map<String, Value>> {
        profile.insert("uid".into(), json!(uid));
        profile.insert("updatedAt".into(), number(self.now()));
        self.client.set(&Self::profile_path(uid), &profile)?;
        Ok(profile)
    }

    fn add_enrolled_graph(
        &self,
        student_uid: &str,
        owner_uid: &str,
        graph_id: &str,
    ) -> StoreResult<()> {
        let mut current = match self.load_profile(student_uid)? {
            Some(p) => p,
            None => self.default_profile(student_uid, self.now()),
        };
        let enrolled = current
            .get("enrolledGraphs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let already = enrolled.iter().any(|g| {
            g.get("ownerUid").and_then(Value::as_str) == Some(owner_uid)
                && g.get("graphId").and_then(Value::as_str) == Some(graph_id)
        });
        if already {
            return Ok(());
        }
        let mut next = enrolled;
        next.push(json!({ "ownerUid": owner_uid, "graphId": graph_id }));
        current.insert("enrolledGraphs".into(), Value::Array(next));
        self.save_profile(student_uid, current)?;
        Ok(())
    }

    fn ensure_student(
        &self,
        owner_uid: &str,
        graph_id: &str,
        student_uid: &str,
    ) -> StoreResult<String> {
        let graph = self.content_graph(owner_uid, graph_id)?;
        let id = format!("student-{student_uid}");
        if !graph.nodes.contains_key(&id) {
            let now = self.now();
            let properties = json!({
                "id": id, "userId": student_uid, "name": "", "email": "", "phone": null,
                "createdAt": number(now), "updatedAt": number(now),
            });
            self.create_node(
                owner_uid,
                graph_id,
                node_of(&id, "Student", properties, now),
            )?;
        }
        Ok(id)
    }
}

fn node_of(id: &str, node_type: &str, properties: Value, now: f64) -> GraphNode {
    GraphNode {
        id: id.to_string(),
        node_type: node_type.to_string(),
        properties: properties.as_object().cloned().unwrap_or_default(),
        created_at: number(now).as_number().cloned(),
        updated_at: number(now).as_number().cloned(),
    }
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `generateRelationshipId`.
pub fn relationship_id(source: &str, target: &str, rel_type: &str) -> String {
    format!("rel-{}-{rel_type}-{}", sanitize(source), sanitize(target))
}

/// `createRelationship(source, target, type, 'forward')`.
pub fn forward_relationship(source: &str, target: &str, rel_type: &str, now: f64) -> Relationship {
    Relationship {
        strength: Stored::Null,
        metadata: Stored::Null,
        ..literal_relationship(source, target, rel_type, now, None)
    }
}

impl GraphStore for FirestoreStore {
    fn now_ms(&self) -> f64 {
        self.now()
    }

    fn get_graph(&self, uid: &str, graph_id: &str) -> StoreResult<NodeBasedKnowledgeGraph> {
        if let Some(graph) = self.cache_get(uid, graph_id) {
            return Ok(graph);
        }
        let doc = self.client.get(&Self::graph_path(uid, graph_id))?;
        let graph = doc
            .as_ref()
            .and_then(graph_from_doc)
            .ok_or_else(|| plain(format!("Graph {graph_id} not found")))?;
        self.cache_set(uid, &graph);
        Ok(graph)
    }

    fn save_graph(
        &self,
        uid: &str,
        graph: &NodeBasedKnowledgeGraph,
        expected_version: Option<f64>,
    ) -> StoreResult<NodeBasedKnowledgeGraph> {
        let path = Self::graph_path(uid, &graph.id);
        let read = self.client.batch_get(&[&path], true)?;
        let transaction = read
            .transaction
            .ok_or_else(|| plain("Transaction ID was missing from server response"))?;
        let current = read
            .docs
            .first()
            .and_then(Option::as_ref)
            .and_then(graph_from_doc);
        let now = self.now();
        let to_save = match current {
            None => {
                if let Some(v) = expected_version {
                    return Err(plain(format!(
                        "Graph {} not found (expected version {})",
                        graph.id,
                        crate::jsnumber::js_number(v)
                    )));
                }
                let mut fresh = graph.clone();
                fresh.version = Some(1.0);
                fresh.updated_at = now;
                fresh
            }
            Some(stored) => {
                let stored_version = stored.version.unwrap_or(0.0);
                if expected_version.is_some_and(|v| stored_version > v) {
                    merge_graph_mutations(&stored, graph, now)
                } else {
                    let mut next = graph.clone();
                    next.version = Some(stored_version + 1.0);
                    next.updated_at = now;
                    next
                }
            }
        };
        let doc = graph_to_doc(&to_save);
        self.client
            .commit(vec![self.client.set_write(&path, &doc)], Some(&transaction))?;
        self.cache_set(uid, &to_save);
        Ok(to_save)
    }

    fn create_node(&self, uid: &str, graph_id: &str, node: GraphNode) -> StoreResult<GraphNode> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        let created = super::engine::mutations::create_node(&mut graph, node, self.now_ms())
            .map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        self.sync_to_vectors(uid, &graph, &created);
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
            super::engine::mutations::update_node(&mut graph, node_id, updates, self.now_ms())
                .map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        self.sync_to_vectors(uid, &graph, &updated);
        Ok(updated)
    }

    fn delete_node(&self, uid: &str, graph_id: &str, node_id: &str) -> StoreResult<()> {
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        let removed = super::engine::mutations::delete_node(&mut graph, node_id, self.now_ms())
            .map_err(plain)?;
        self.save_graph(uid, &graph, version)?;
        if removed.node_type == "Concept" {
            self.sync_goal_coverage(uid, &graph);
        }
        Ok(())
    }

    fn create_relationship(
        &self,
        uid: &str,
        graph_id: &str,
        relationship: Relationship,
    ) -> StoreResult<Relationship> {
        let relationship = Relationship {
            strength: relationship.strength.or_null(),
            metadata: relationship.metadata.or_null(),
            ..relationship
        };
        let mut graph = self.get_graph(uid, graph_id)?;
        let version = graph.version;
        let (rel, changed) =
            super::engine::mutations::create_relationship(&mut graph, relationship, self.now_ms())
                .map_err(plain)?;
        if changed {
            self.save_graph(uid, &graph, version)?;
        }
        Ok(rel)
    }

    fn delete_graph(&self, uid: &str, graph_id: &str) -> StoreResult<()> {
        let path = Self::graph_path(uid, graph_id);
        if self.client.get(&path)?.is_none() {
            return Err(plain(format!("Graph {graph_id} not found")));
        }
        self.client.delete(&path)?;
        self.cache_drop(uid, graph_id);
        Ok(())
    }

    fn list_graph_ids(&self, uid: &str) -> StoreResult<Vec<String>> {
        let rows = self.client.query(
            &format!("users/{uid}"),
            GRAPHS,
            json!({ "select": { "fields": [{ "fieldPath": "id" }] } }),
        )?;
        Ok(rows.into_iter().map(|(id, _)| id).collect())
    }
}

impl StudentStore for FirestoreStore {
    fn get_preferences(&self, uid: &str) -> StoreResult<Option<Map<String, Value>>> {
        Ok(self
            .load_profile(uid)?
            .and_then(|p| p.get("preferences").and_then(obj).cloned()))
    }

    fn set_preferences(
        &self,
        uid: &str,
        patch: &Map<String, Value>,
    ) -> StoreResult<Map<String, Value>> {
        let mut current = match self.load_profile(uid)? {
            Some(p) => p,
            None => self.default_profile(uid, self.now()),
        };
        let mut preferences = current
            .get("preferences")
            .and_then(obj)
            .cloned()
            .unwrap_or_default();
        for (k, v) in patch {
            preferences.insert(k.clone(), v.clone());
        }
        current.insert("preferences".into(), Value::Object(preferences));
        let saved = self.save_profile(uid, current)?;
        Ok(saved
            .get("preferences")
            .and_then(obj)
            .cloned()
            .unwrap_or_default())
    }

    fn get_progress_for_concept(
        &self,
        owner_uid: &str,
        graph_id: &str,
        student_uid: &str,
        concept_id: &str,
    ) -> StoreResult<Option<Map<String, Value>>> {
        let graph = self.content_graph(owner_uid, graph_id)?;
        let id = format!("progress-{student_uid}-{concept_id}");
        Ok(graph
            .nodes
            .get(&id)
            .filter(|n| n.node_type == "Progress")
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
        let student_node = self.ensure_student(owner_uid, graph_id, student_uid)?;
        self.add_enrolled_graph(student_uid, owner_uid, graph_id)?;
        let graph = self.get_graph(owner_uid, graph_id)?;
        let id = format!("progress-{student_uid}-{concept_id}");
        let now = self.now();

        if graph
            .nodes
            .get(&id)
            .is_some_and(|n| n.node_type == "Progress")
        {
            let mut properties = data.clone();
            properties.insert("conceptId".into(), json!(concept_id));
            properties.insert("sourceGraphId".into(), json!(graph_id));
            properties.insert("updatedAt".into(), number(now));
            let mut updates = Map::new();
            updates.insert("properties".into(), Value::Object(properties));
            self.update_node(owner_uid, graph_id, &id, &updates)?;
            let updated = self.get_graph(owner_uid, graph_id)?;
            return updated
                .nodes
                .get(&id)
                .filter(|n| n.node_type == "Progress")
                .cloned()
                .ok_or_else(|| plain(format!("Progress {id} vanished after update")));
        }

        let pick = |k: &str| data.get(k).cloned().unwrap_or(Value::Null);
        let properties = json!({
            "id": id,
            "masteryLevel": data.get("masteryLevel").cloned().unwrap_or(json!(0)),
            "bloomLevelsCompleted": data.get("bloomLevelsCompleted").cloned().unwrap_or(json!([])),
            "activationResponse": pick("activationResponse"),
            "reflectionNotes": pick("reflectionNotes"),
            "bloomAnswered": pick("bloomAnswered"),
            "lastStudied": data.get("lastStudied").cloned().unwrap_or_else(|| number(now)),
            "conceptId": concept_id,
            "graphId": pick("graphId"),
            "courseId": pick("courseId"),
            "lessonId": pick("lessonId"),
            "sourceGraphId": graph_id,
            "createdAt": data.get("createdAt").cloned().unwrap_or_else(|| number(now)),
            "updatedAt": data.get("updatedAt").cloned().unwrap_or_else(|| number(now)),
        });
        let node = node_of(&id, "Progress", properties, now);
        self.create_node(owner_uid, graph_id, node.clone())?;
        self.create_relationship(
            owner_uid,
            graph_id,
            forward_relationship(&student_node, &id, "hasProgress", now),
        )?;
        Ok(node)
    }

    fn remove_enrolled_graph(
        &self,
        owner_uid: &str,
        student_uid: &str,
        graph_id: &str,
    ) -> StoreResult<()> {
        let Some(mut current) = self.load_profile(student_uid)? else {
            return Ok(());
        };
        let enrolled = current
            .get("enrolledGraphs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let kept: Vec<Value> = enrolled
            .iter()
            .filter(|g| {
                !(g.get("ownerUid").and_then(Value::as_str) == Some(owner_uid)
                    && g.get("graphId").and_then(Value::as_str) == Some(graph_id))
            })
            .cloned()
            .collect();
        if kept.len() == enrolled.len() {
            return Ok(());
        }
        current.insert("enrolledGraphs".into(), Value::Array(kept));
        self.save_profile(student_uid, current)?;
        Ok(())
    }
}

// ---- social ----

fn members_collection(node_key: &str) -> (String, &'static str) {
    (
        format!("node-activity/{}", encode_uri_component(node_key)),
        "members",
    )
}

fn typed<T: serde::de::DeserializeOwned>(fields: Map<String, Value>) -> Option<T> {
    serde_json::from_value(Value::Object(fields)).ok()
}

impl SocialStore for FirestoreStore {
    fn touch_member(&self, node_key: &str, uid: &str, at: f64) -> StoreResult<()> {
        let (parent, collection) = members_collection(node_key);
        let mut data = Map::new();
        data.insert("lastSeenOnNode".into(), number(at));
        self.client
            .set_merge(&format!("{parent}/{collection}/{uid}"), &data)
    }

    fn list_members_since(&self, node_key: &str, cutoff: f64) -> StoreResult<Vec<NodeMember>> {
        let (parent, collection) = members_collection(node_key);
        let rows = self.client.query(
            &parent,
            collection,
            json!({ "where": { "fieldFilter": {
                "field": { "fieldPath": "lastSeenOnNode" },
                "op": OP_GREATER_THAN_OR_EQUAL,
                "value": to_wire(&number(cutoff)),
            } } }),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid, fields)| {
                let seen = fields.get("lastSeenOnNode").and_then(Value::as_f64)?;
                Some(NodeMember {
                    uid,
                    last_seen_on_node: seen,
                })
            })
            .collect())
    }

    fn put_peer_token(&self, token: &str, record: PeerTokenRecord) -> StoreResult<()> {
        let mut data = without_nulls_of(&record);
        data.insert("createdAt".into(), number(self.now()));
        self.client.set(&format!("peer-tokens/{token}"), &data)
    }

    fn get_peer_token(&self, token: &str) -> StoreResult<Option<PeerTokenRecord>> {
        Ok(self
            .client
            .get(&format!("peer-tokens/{token}"))?
            .and_then(typed))
    }

    fn delete_peer_token(&self, token: &str) -> StoreResult<()> {
        self.client.delete(&format!("peer-tokens/{token}"))
    }

    fn create_connection(&self, connection: StoredConnection) -> StoreResult<String> {
        let id = (self.ids)();
        self.client
            .create(&format!("connections/{id}"), &without_nulls_of(&connection))?;
        Ok(id)
    }

    fn get_connection(&self, id: &str) -> StoreResult<Option<StoredConnection>> {
        Ok(self
            .client
            .get(&format!("connections/{id}"))?
            .and_then(typed))
    }

    fn update_connection(&self, id: &str, patch: ConnectionPatch) -> StoreResult<()> {
        let mut fields = Map::new();
        if let Some(v) = patch.active_badge_id {
            fields.insert("activeBadgeId".into(), json!(v));
        }
        if let Some(v) = patch.status {
            fields.insert("status".into(), json!(v));
        }
        if let Some(v) = patch.archived_at {
            fields.insert("archivedAt".into(), number(v));
        }
        if let Some(v) = patch.last_activity_at {
            fields.insert("lastActivityAt".into(), number(v));
        }
        self.client.update(&format!("connections/{id}"), &fields)
    }

    fn add_message(&self, connection_id: &str, message: StoredMessage) -> StoreResult<String> {
        let id = (self.ids)();
        self.client.create(
            &format!("connections/{connection_id}/messages/{id}"),
            &without_nulls_of(&message),
        )?;
        Ok(id)
    }

    fn list_messages(&self, connection_id: &str) -> StoreResult<Vec<StoredMessageRow>> {
        let rows = self.client.query(
            &format!("connections/{connection_id}"),
            "messages",
            json!({ "orderBy": [{ "field": { "fieldPath": "createdAt" }, "direction": ASCENDING }] }),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, fields)| {
                typed::<StoredMessage>(fields).map(|message| StoredMessageRow { id, message })
            })
            .collect())
    }

    fn set_message_moderation(
        &self,
        connection_id: &str,
        message_id: &str,
        moderation: MessageModeration,
    ) -> StoreResult<()> {
        let mut fields = Map::new();
        fields.insert(
            "moderation".into(),
            Value::Object(without_nulls_of(&moderation)),
        );
        self.client.update(
            &format!("connections/{connection_id}/messages/{message_id}"),
            &fields,
        )
    }

    fn get_persona(&self, key: &str) -> StoreResult<Option<StoredPersona>> {
        Ok(self
            .client
            .get(&format!("concept-personas/{key}"))?
            .and_then(typed))
    }

    fn put_persona(&self, key: &str, persona: StoredPersona) -> StoreResult<()> {
        self.client.set(
            &format!("concept-personas/{key}"),
            &without_nulls_of(&persona),
        )
    }
}

// ---- icons ----

impl IconStore for FirestoreStore {
    fn get(&self, key: &str) -> StoreResult<Option<IconCacheEntry>> {
        let Some(fields) = self.client.get(&format!("concept-icons/{key}"))? else {
            return Ok(None);
        };
        let label = fields.get("label").and_then(Value::as_str);
        let fetched_at = fields.get("fetchedAt").and_then(Value::as_f64);
        let icon = fields.get("icon");
        let tone = fields.get("tone");
        let (Some(label), Some(fetched_at)) = (label, fetched_at) else {
            return Ok(None);
        };
        let icon_ok = icon.is_some_and(|v| v.is_null() || v.is_string());
        let tone_ok = tone.is_some_and(|v| {
            v.is_null() || matches!(v.as_str(), Some("themed" | "dark" | "light"))
        });
        if !icon_ok || !tone_ok {
            return Ok(None);
        }
        match icon.and_then(Value::as_str) {
            None => Ok(Some(IconCacheEntry {
                label: label.to_string(),
                icon: None,
                fetched_at,
            })),
            Some(icon) => Ok(tone.and_then(Value::as_str).map(|tone| IconCacheEntry {
                label: label.to_string(),
                icon: Some(ResolvedIcon {
                    icon: icon.to_string(),
                    tone: tone.to_string(),
                }),
                fetched_at,
            })),
        }
    }

    fn put(&self, key: &str, entry: IconCacheEntry) -> StoreResult<()> {
        let mut data = Map::new();
        data.insert("label".into(), json!(entry.label));
        data.insert(
            "icon".into(),
            entry.icon.as_ref().map_or(Value::Null, |i| json!(i.icon)),
        );
        data.insert(
            "tone".into(),
            entry.icon.as_ref().map_or(Value::Null, |i| json!(i.tone)),
        );
        data.insert("fetchedAt".into(), number(entry.fetched_at));
        self.client.set(&format!("concept-icons/{key}"), &data)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::transport::{HttpResponse, TransportError};

    struct Script {
        replies: Mutex<VecDeque<(u16, String)>>,
        sent: Mutex<Vec<HttpRequest>>,
    }

    impl Script {
        fn new(replies: &[(u16, Value)]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|(s, b)| (*s, b.to_string())).collect()),
                sent: Mutex::new(Vec::new()),
            })
        }
    }

    impl Transport for Script {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.sent.lock().unwrap().push(request.clone());
            let (status, body) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::Failed("script exhausted".into()))?;
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: body.into_bytes(),
            })
        }
    }

    fn doc_reply(path: &str, data: Value) -> Value {
        let fields = data.as_object().map(fields_to_wire).unwrap_or(Value::Null);
        json!([{ "found": { "name": format!("projects/demo/databases/(default)/documents/{path}"), "fields": fields }, "readTime": "t" }])
    }

    fn graph_doc(version: u32, node_name: &str) -> Value {
        json!({
            "id": "g", "createdAt": 1, "updatedAt": 1, "version": version,
            "nodes": { "a": { "id": "a", "type": "Concept", "properties": { "name": node_name }, "updatedAt": 5 } },
            "nodeTypes": { "Concept": ["a"] }, "relationships": []
        })
    }

    fn store(script: Arc<Script>, clock: Clock) -> FirestoreStore {
        FirestoreStore::new(FirestoreConfig::emulator("localhost:8080", "demo"), script)
            .with_clock(clock)
    }

    #[test]
    fn values_survive_the_wire_round_trip() {
        let value =
            json!({ "a": [1, 2.5, "x", null, true, {}, []], "b": { "c": 1700000000000i64 } });
        assert_eq!(from_wire(&to_wire(&value)), value);
    }

    #[test]
    fn whole_numbers_are_integer_values_and_fractions_doubles() {
        assert_eq!(to_wire(&json!(3.0)), json!({ "integerValue": "3" }));
        assert_eq!(to_wire(&json!(0.25)), json!({ "doubleValue": 0.25 }));
        assert_eq!(to_wire(&json!([])), json!({ "arrayValue": {} }));
        assert_eq!(to_wire(&json!({})), json!({ "mapValue": { "fields": {} } }));
    }

    #[test]
    fn an_auto_id_is_twenty_alphanumerics() {
        let id = random_auto_id();
        assert_eq!(id.len(), 20);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(id, random_auto_id());
    }

    #[test]
    fn a_graph_is_read_once_until_its_cache_entry_expires() {
        let script = Script::new(&[
            (
                200,
                doc_reply("users/u/knowledgeGraphs/g", graph_doc(1, "A")),
            ),
            (
                200,
                doc_reply("users/u/knowledgeGraphs/g", graph_doc(2, "B")),
            ),
        ]);
        let now = Arc::new(Mutex::new(1_000i64));
        let clock_now = now.clone();
        let store = store(script.clone(), Arc::new(move || *clock_now.lock().unwrap()));
        assert_eq!(store.get_graph("u", "g").unwrap().version, Some(1.0));
        assert_eq!(store.get_graph("u", "g").unwrap().version, Some(1.0));
        assert_eq!(script.sent.lock().unwrap().len(), 1);
        *now.lock().unwrap() += 5 * 60 * 1000 + 1;
        assert_eq!(store.get_graph("u", "g").unwrap().version, Some(2.0));
        assert_eq!(script.sent.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_missing_graph_is_the_plain_not_found_error() {
        let script = Script::new(&[(
            200,
            json!([{ "missing": "projects/p/databases/(default)/documents/users/u/knowledgeGraphs/g", "readTime": "t" }]),
        )]);
        let err = store(script, Arc::new(|| 1))
            .get_graph("u", "g")
            .unwrap_err();
        assert_eq!(err.message, "Graph g not found");
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[test]
    fn a_save_against_a_newer_stored_version_merges_instead_of_overwriting() {
        let mut stored = doc_reply("users/u/knowledgeGraphs/g", graph_doc(5, "stored"));
        stored[0]["transaction"] = json!("dHg=");
        let script = Script::new(&[
            (200, stored),
            (200, json!({ "writeResults": [], "commitTime": "t" })),
        ]);
        let store = store(script.clone(), Arc::new(|| 9_000));
        let mut incoming: NodeBasedKnowledgeGraph =
            serde_json::from_value(graph_doc(3, "edited")).unwrap();
        incoming.nodes.get_mut("a").unwrap().updated_at = Some(serde_json::Number::from(7));
        let saved = store.save_graph("u", &incoming, Some(3.0)).unwrap();
        assert_eq!(saved.version, Some(6.0));
        assert_eq!(
            saved.nodes.get("a").unwrap().properties["name"],
            json!("edited")
        );
        assert_eq!(
            saved
                .nodes
                .get("a")
                .unwrap()
                .updated_at
                .as_ref()
                .and_then(serde_json::Number::as_f64),
            Some(7.0)
        );
        let sent = script.sent.lock().unwrap();
        let commit: Value = serde_json::from_str(&sent[1].text()).unwrap();
        assert_eq!(commit["transaction"], json!("dHg="));
        assert_eq!(
            commit["writes"][0]["update"]["fields"]["version"],
            json!({ "integerValue": "6" })
        );
    }

    #[test]
    fn control_a_save_at_the_stored_version_bumps_it_by_one() {
        let mut stored = doc_reply("users/u/knowledgeGraphs/g", graph_doc(3, "stored"));
        stored[0]["transaction"] = json!("dHg=");
        let script = Script::new(&[
            (200, stored),
            (200, json!({ "writeResults": [], "commitTime": "t" })),
        ]);
        let store = store(script, Arc::new(|| 9_000));
        let incoming: NodeBasedKnowledgeGraph =
            serde_json::from_value(graph_doc(3, "edited")).unwrap();
        let saved = store.save_graph("u", &incoming, Some(3.0)).unwrap();
        assert_eq!(saved.version, Some(4.0));
        assert_eq!(saved.updated_at, 9000.0);
    }

    #[test]
    fn a_query_encodes_each_parent_segment_and_an_http_failure_is_the_raw_body() {
        let script = Script::new(&[
            (200, json!([{ "readTime": "t" }])),
            (403, json!({ "error": { "message": "denied" } })),
        ]);
        let store = store(script.clone(), Arc::new(|| 1));
        assert!(store
            .list_members_since("concept:light", 5.0)
            .unwrap()
            .is_empty());
        let url = script.sent.lock().unwrap()[0].url.clone();
        assert!(
            url.contains("/documents/node-activity/concept%253Alight:runQuery?"),
            "{url}"
        );
        let err = store.get_persona("k").unwrap_err();
        assert_eq!(err.message, "{\"error\":{\"message\":\"denied\"}}");
    }

    #[test]
    fn credentials_choose_the_emulator_or_demand_a_project() {
        let env = |pairs: &[(&str, &str)]| {
            Credentials::from_env(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };
        let transport: Arc<dyn Transport> = Script::new(&[]);
        let emulator = FirestoreConfig::from_credentials(
            &env(&[("FIRESTORE_EMULATOR_HOST", "h:1")]),
            transport.clone(),
        )
        .unwrap();
        assert_eq!(emulator.base, "http://h:1");
        assert_eq!(emulator.project_id, "demo-almadar");
        let err = FirestoreConfig::from_credentials(&env(&[]), transport.clone())
            .err()
            .expect("no credentials");
        assert!(err.message.contains("no credentials found"));
        let project = FirestoreConfig::from_credentials(
            &env(&[
                ("FIREBASE_PROJECT_ID", "p"),
                ("FIRESTORE_ACCESS_TOKEN", "tok"),
            ]),
            transport,
        )
        .unwrap();
        assert_eq!(project.base, PRODUCTION_BASE);
        assert!(matches!(&project.auth, FirestoreAuth::Token(t) if t().as_deref() == Ok("tok")));
    }
}
