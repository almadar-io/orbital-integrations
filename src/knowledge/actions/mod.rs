//! The node `knowledge` actions: the Rust twin of `integrations/knowledge/node/actions`. Each module ports one TS
//! file and registers its actions into the `ActionTable` the `KnowledgeProvider` dispatches on. What lives
//! outside the process (the clock, entropy, Iconify, Wikipedia, the model) reaches an action through `Env`.

pub mod batch;
pub mod common;
pub mod concept_stream;
pub mod courses;
pub mod create;
pub mod enrich;
pub mod enrichment;
pub mod expansion;
pub mod explain_prompt;
pub mod feed;
pub mod feed_ranking;
pub mod goal;
pub mod icon_logic;
pub mod icon_sources;
pub mod import;
pub mod import_graph;
pub mod import_items;
pub mod import_sink;
pub mod lesson;
pub mod mappers;
pub mod migration;
pub mod misc;
pub mod model;
pub mod model_options;
pub mod params;
pub mod paths;
pub mod persona_logic;
pub mod prompt_builder;
pub mod prompts;
pub mod queries;
pub mod social;
pub mod social_logic;
pub mod social_sources;
pub mod vector_text;

use std::sync::Arc;

use serde_json::{Map, Value};

use super::store::{KnowledgeDeps, StoreResult};
use super::{ActionTable, KnowledgeAction, KnowledgeCall};

pub type IdSource = Arc<dyn Fn() -> String + Send + Sync>;

/// The Iconify lookups the icon action needs (`IconSources`).
pub trait IconSources: Send + Sync {
    fn svg_exists(&self, path: &str) -> bool;
    fn search(&self, query: &str) -> Option<String>;
    fn icon_body(&self, icon_id: &str) -> Option<String>;
}

/// Text embeddings (`embedBatch`): `None` when no embedding client is configured or the batch fails.
pub trait Embedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Option<Vec<Vec<f64>>>;
}

/// No embedding client: every feature that needs vectors degrades the way the TS twin does without a key.
pub struct NoEmbedder;

impl Embedder for NoEmbedder {
    fn embed_batch(&self, _texts: &[String]) -> Option<Vec<Vec<f64>>> {
        None
    }
}

/// Everything an action reads from outside the stores: entropy and the external lookups.
#[derive(Clone)]
pub struct Env {
    /// `randomUUID()`
    pub uuid: IdSource,
    /// `randomBytes(18).toString('base64url')`
    pub token: IdSource,
    pub icons: Arc<dyn IconSources>,
    pub embedder: Arc<dyn Embedder>,
    pub feed: Arc<dyn feed::FeedSources>,
    pub social: Arc<dyn social_sources::SocialSources>,
    /// The visualization domains a lesson may point at, read by the host from the installed learning organisms.
    pub visualizations: Vec<explain_prompt::VisualizationDomain>,
}

struct Run<F>(F);

impl<F> KnowledgeAction for Run<F>
where
    F: Fn(&KnowledgeCall<'_>, &Map<String, Value>, &KnowledgeDeps) -> StoreResult<Value>
        + Send
        + Sync,
{
    fn run(
        &self,
        call: &KnowledgeCall<'_>,
        params: &Map<String, Value>,
        deps: &KnowledgeDeps,
    ) -> StoreResult<Value> {
        (self.0)(call, params, deps)
    }
}

/// An action from a closure.
pub fn action<F>(f: F) -> Arc<dyn KnowledgeAction>
where
    F: Fn(&KnowledgeCall<'_>, &Map<String, Value>, &KnowledgeDeps) -> StoreResult<Value>
        + Send
        + Sync
        + 'static,
{
    Arc::new(Run(f))
}

/// Every action this port carries (`knowledgeActions`).
pub fn action_table(env: &Env) -> ActionTable {
    let mut table = ActionTable::new();
    misc::register(&mut table, env);
    courses::register(&mut table);
    lesson::register(&mut table, env);
    paths::register(&mut table, env);
    feed::register(&mut table, env);
    social::register(&mut table, env);
    import::register(&mut table, env);
    import_items::register(&mut table, env);
    create::register(&mut table, env);
    enrich::register(&mut table, env);
    table
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    if getrandom::getrandom(&mut bytes).is_err() {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    bytes
}

/// `randomUUID()`: a version-4 UUID.
pub fn random_uuid() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `randomBytes(18).toString('base64url')`.
pub fn random_peer_token() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(random_bytes::<18>())
}

/// The node leg wired for a host: Firestore for the learner's documents, the OpenAI-compatible chat API for the
/// model, Iconify and Wikipedia for lookups. One `NodeHost` serves any number of learners: each call's
/// `context.principal` is the learner. The vector index (Chroma) is the host's to supply.
pub struct NodeHost {
    deps: KnowledgeDeps,
    table: ActionTable,
}

impl NodeHost {
    pub fn from_credentials(
        creds: &crate::credentials::Credentials,
        transport: Arc<dyn crate::transport::Transport>,
        vectors: Arc<dyn super::store::VectorIndex>,
        visualizations: Vec<explain_prompt::VisualizationDomain>,
    ) -> Result<Self, crate::error::IntegrationError> {
        let config = super::firestore::FirestoreConfig::from_credentials(creds, transport.clone())?;
        let store = Arc::new(
            super::firestore::FirestoreStore::new(config, transport.clone())
                .with_vectors(vectors.clone()),
        );
        let models: Arc<dyn super::store::ModelPort> =
            Arc::new(model::OpenAiCompatModels::new(creds, transport.clone()));
        let deps = store.deps(vectors, models.clone());
        let wikimedia: Arc<dyn crate::provider::Provider> = Arc::new(
            crate::wikimedia::WikimediaProvider::new(creds, transport.clone()),
        );
        let iconify: Arc<dyn crate::provider::Provider> = Arc::new(
            crate::iconify::IconifyProvider::new(creds, transport.clone()),
        );
        let env = Env {
            uuid: Arc::new(random_uuid),
            token: Arc::new(random_peer_token),
            icons: Arc::new(icon_sources::IconifyIconSources::new(iconify)),
            embedder: Arc::new(model::HttpEmbedder::new(creds, transport)),
            feed: Arc::new(feed::ModelFeedSources {
                models: models.clone(),
            }),
            social: Arc::new(social_sources::ModelSocialSources { models, wikimedia }),
            visualizations,
        };
        Ok(Self {
            table: action_table(&env),
            deps,
        })
    }

    /// The provider that answers every learner's calls.
    pub fn provider(&self) -> super::KnowledgeProvider {
        super::KnowledgeProvider::new(self.deps.clone(), self.table.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT_ACTIONS: [&str; 42] = [
        "listPaths",
        "pathMap",
        "deletePath",
        "conceptsByLayer",
        "expand",
        "layerPractice",
        "conceptDetail",
        "explain",
        "saveLesson",
        "answerQuestion",
        "addNote",
        "deleteNote",
        "deleteQuestion",
        "recordLessonProgress",
        "markLearned",
        "createGoalWithGraph",
        "importPreview",
        "importCommit",
        "importItems",
        "peers",
        "touchNode",
        "openConnection",
        "connection",
        "setActiveBadge",
        "sendMessage",
        "conceptPersona",
        "conceptIcon",
        "getPreferences",
        "setPreferences",
        "feed",
        "cardContent",
        "exploreContent",
        "feedSignal",
        "addExplore",
        "listCourses",
        "courseDetail",
        "createCourse",
        "updateCourse",
        "deleteCourse",
        "coursePackage",
        "enrichPreview",
        "enrichApply",
    ];

    #[test]
    fn every_node_action_of_the_contract_has_an_implementation() {
        let host = NodeHost::from_credentials(
            &crate::credentials::Credentials::from_env(
                [(
                    "FIRESTORE_EMULATOR_HOST".to_string(),
                    "localhost:8080".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            Arc::new(Unreachable),
            Arc::new(super::super::memory::MemoryStore::new()),
            Vec::new(),
        )
        .expect("an emulator host is a complete Firestore target");
        let mut names = host.table.names();
        names.sort_unstable();
        let mut expected: Vec<&str> = CONTRACT_ACTIONS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[test]
    fn a_uuid_is_version_4_and_a_token_is_24_url_safe_characters() {
        let id = random_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert_ne!(id, random_uuid());
        let token = random_peer_token();
        assert_eq!(token.len(), 24);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    struct Unreachable;

    impl crate::transport::Transport for Unreachable {
        fn send(
            &self,
            _request: &crate::transport::HttpRequest,
        ) -> Result<crate::transport::HttpResponse, crate::transport::TransportError> {
            Err(crate::transport::TransportError::Failed(
                "no network in this test".to_string(),
            ))
        }
    }
}
