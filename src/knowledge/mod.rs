//! The `knowledge` service: the Rust twin of `integrations/knowledge`.
//!
//! - `engine/` is the pure graph engine (graph model, mutations, validator, coercion): input -> output, no I/O.
//! - `store.rs` holds the ports the I/O actions run against (graphs, learner, vectors, models, social, icons).
//! - `KnowledgeProvider` is the node leg (`KnowledgeIntegration`): the signed-in learner's own graphs. It
//!   dispatches `status`/`connect` itself and every other action to the `ActionTable` the action ports fill.
//! - `web::WebProvider` is the browser leg (`createKnowledgeService`): `currentTopics` over KFlow's HTTP routes.

pub mod actions;
pub mod engine;
pub mod firestore;
pub mod memory;
pub mod store;
pub mod web;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{IntegrationError, SERVICE_ERROR, VALIDATION_ERROR};
use crate::provider::{CallContext, Integrations, Provider};

pub use self::store::{ErrorKind, KnowledgeDeps, KnowledgeError, StoreResult};

pub const SERVICE: &str = "knowledge";
pub const AUTH_ERROR: &str = "AUTH_ERROR";
pub const CANCELLED: &str = "CANCELLED";

/// A live progress message from a streaming action (`KnowledgeProgress`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeProgress {
    /// `start` | `delta` | `step`
    pub phase: String,
    pub text: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl KnowledgeProgress {
    pub fn new(phase: &str, text: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            phase: phase.to_string(),
            text: text.into(),
            model: model.into(),
            field: None,
        }
    }
}

pub type ProgressSink = Arc<dyn Fn(&KnowledgeProgress) + Send + Sync>;

/// Set by the host when the program cancels the call; an action stops streaming and never writes after it.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One call, scoped to the signed-in learner (`KnowledgeCall`).
pub struct KnowledgeCall<'a> {
    pub uid: &'a str,
    progress: &'a ProgressSink,
    cancel: &'a CancelToken,
}

impl KnowledgeCall<'_> {
    pub fn progress(&self, message: KnowledgeProgress) {
        (self.progress)(&message);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Fails with `CANCELLED` once the caller has cancelled; every write is preceded by this.
    pub fn throw_if_cancelled(&self) -> StoreResult<()> {
        if self.cancelled() {
            Err(KnowledgeError::cancelled())
        } else {
            Ok(())
        }
    }

    /// Relay a model stream (`start`, then one `delta` per non-empty chunk) and return the full text.
    pub fn relay_model_stream(&self, streamed: store::ModelStream) -> StoreResult<String> {
        self.progress(KnowledgeProgress::new("start", "", &streamed.model));
        let mut content = String::new();
        for chunk in streamed.chunks {
            self.throw_if_cancelled()?;
            let chunk = chunk?;
            if !chunk.content.is_empty() {
                content.push_str(&chunk.content);
                self.progress(KnowledgeProgress::new(
                    "delta",
                    chunk.content,
                    &streamed.model,
                ));
            }
            if chunk.done {
                break;
            }
        }
        self.throw_if_cancelled()?;
        Ok(content)
    }
}

/// One node action: its param validation and implementation (`KnowledgeAction`). Params arrive already
/// checked against the installed contract; an action's own refusals are `KnowledgeError`s.
pub trait KnowledgeAction: Send + Sync {
    fn run(
        &self,
        call: &KnowledgeCall<'_>,
        params: &Map<String, Value>,
        deps: &KnowledgeDeps,
    ) -> StoreResult<Value>;
}

/// The action name -> implementation table (`knowledgeActions`). The action ports register theirs.
#[derive(Default, Clone)]
pub struct ActionTable {
    actions: HashMap<String, Arc<dyn KnowledgeAction>>,
}

impl ActionTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: &str, action: Arc<dyn KnowledgeAction>) -> &mut Self {
        self.actions.insert(name.to_string(), action);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn KnowledgeAction>> {
        self.actions.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.actions.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

/// The node leg: every action is scoped to the call's `context.principal`; a call without one is refused.
pub struct KnowledgeProvider {
    deps: KnowledgeDeps,
    actions: ActionTable,
    progress: ProgressSink,
    cancel: CancelToken,
}

impl KnowledgeProvider {
    pub fn new(deps: KnowledgeDeps, actions: ActionTable) -> Self {
        Self {
            deps,
            actions,
            progress: Arc::new(|_| {}),
            cancel: CancelToken::default(),
        }
    }

    /// Deliver progress messages to the call site's `onMessage`.
    pub fn with_progress(mut self, sink: ProgressSink) -> Self {
        self.progress = sink;
        self
    }

    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }
}

fn connection(uid: Option<&str>) -> Value {
    json!({ "connected": uid.is_some(), "account": uid.unwrap_or_default() })
}

/// How the TS integration reports an action failure.
pub fn into_integration_error(error: KnowledgeError) -> IntegrationError {
    match error.kind {
        ErrorKind::Validation => IntegrationError::coded(VALIDATION_ERROR, error.message),
        ErrorKind::NotFound | ErrorKind::Conflict | ErrorKind::Service => {
            IntegrationError::coded(SERVICE_ERROR, error.message)
        }
        ErrorKind::Cancelled => IntegrationError::coded(CANCELLED, error.message),
        ErrorKind::Other => IntegrationError::plain(error.message),
    }
}

impl Provider for KnowledgeProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        let uid = ctx.principal();
        if action == "status" || action == "connect" {
            return Ok(connection(uid));
        }
        let Some(uid) = uid else {
            return Err(IntegrationError::coded(
                AUTH_ERROR,
                format!("knowledge.{action} needs a signed-in learner"),
            ));
        };
        let Some(entry) = self.actions.get(action) else {
            return Err(IntegrationError::validation(format!(
                "knowledge has no node action '{action}'"
            )));
        };
        let call = KnowledgeCall {
            uid,
            progress: &self.progress,
            cancel: &self.cancel,
        };
        entry
            .run(&call, params, &self.deps)
            .map_err(into_integration_error)
    }
}

/// Register the node leg. Without a provider (no stores on this host) the service stays unconfigured.
pub fn register(integrations: &mut Integrations, provider: Option<KnowledgeProvider>) {
    match provider {
        Some(p) => integrations.register(SERVICE, Arc::new(p)),
        None => integrations.register_unconfigured(SERVICE),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::memory::MemoryStore;
    use super::store::{ModelChunk, ModelStream};
    use super::*;
    use crate::credentials::Credentials;

    struct Echo;
    impl KnowledgeAction for Echo {
        fn run(
            &self,
            call: &KnowledgeCall<'_>,
            params: &Map<String, Value>,
            _deps: &KnowledgeDeps,
        ) -> StoreResult<Value> {
            Ok(json!({ "uid": call.uid, "params": params }))
        }
    }

    struct Fails(ErrorKind);
    impl KnowledgeAction for Fails {
        fn run(
            &self,
            _call: &KnowledgeCall<'_>,
            _params: &Map<String, Value>,
            _deps: &KnowledgeDeps,
        ) -> StoreResult<Value> {
            Err(KnowledgeError::new(self.0, "refused"))
        }
    }

    struct Streams;
    impl KnowledgeAction for Streams {
        fn run(
            &self,
            call: &KnowledgeCall<'_>,
            _params: &Map<String, Value>,
            _deps: &KnowledgeDeps,
        ) -> StoreResult<Value> {
            let chunks = vec![
                Ok(ModelChunk {
                    content: "he".into(),
                    done: false,
                }),
                Ok(ModelChunk {
                    content: String::new(),
                    done: false,
                }),
                Ok(ModelChunk {
                    content: "llo".into(),
                    done: true,
                }),
                Ok(ModelChunk {
                    content: "never".into(),
                    done: false,
                }),
            ];
            let text = call.relay_model_stream(ModelStream {
                model: "m".into(),
                chunks: Box::new(chunks.into_iter()),
            })?;
            Ok(json!(text))
        }
    }

    fn provider() -> KnowledgeProvider {
        let store = Arc::new(MemoryStore::new());
        let mut table = ActionTable::new();
        table
            .insert("echo", Arc::new(Echo))
            .insert("missing", Arc::new(Fails(ErrorKind::NotFound)))
            .insert("conflict", Arc::new(Fails(ErrorKind::Conflict)))
            .insert("invalid", Arc::new(Fails(ErrorKind::Validation)))
            .insert("service", Arc::new(Fails(ErrorKind::Service)))
            .insert("plain", Arc::new(Fails(ErrorKind::Other)))
            .insert("stop", Arc::new(Fails(ErrorKind::Cancelled)))
            .insert("stream", Arc::new(Streams));
        KnowledgeProvider::new(store.deps(), table)
    }

    fn learner(uid: Option<&str>) -> CallContext {
        uid.map_or_else(CallContext::anonymous, |uid| {
            CallContext::signed_in(uid, None)
        })
    }

    fn call(
        p: &KnowledgeProvider,
        uid: Option<&str>,
        action: &str,
    ) -> Result<Value, IntegrationError> {
        p.call(action, &Map::new(), &learner(uid))
    }

    #[test]
    fn status_and_connect_report_the_signed_in_learner_without_a_table_entry() {
        for action in ["status", "connect"] {
            assert_eq!(
                call(&provider(), Some("u1"), action).unwrap(),
                json!({ "connected": true, "account": "u1" })
            );
            assert_eq!(
                call(&provider(), None, action).unwrap(),
                json!({ "connected": false, "account": "" })
            );
        }
    }

    #[test]
    fn an_action_without_a_learner_is_an_auth_error() {
        let err = call(&provider(), None, "echo").unwrap_err();
        assert_eq!(err.code.as_deref(), Some(AUTH_ERROR));
        assert_eq!(err.message, "knowledge.echo needs a signed-in learner");
    }

    #[test]
    fn control_the_same_action_runs_for_a_learner_and_receives_the_uid() {
        let mut params = Map::new();
        params.insert("graphId".into(), json!("g1"));
        let out = provider()
            .call("echo", &params, &learner(Some("u1")))
            .unwrap();
        assert_eq!(out, json!({ "uid": "u1", "params": { "graphId": "g1" } }));
    }

    #[test]
    fn two_learners_on_one_provider_each_get_their_own_uid() {
        let p = provider();
        let mut params = Map::new();
        params.insert("graphId".into(), json!("g1"));
        for uid in ["u1", "u2"] {
            let out = p.call("echo", &params, &learner(Some(uid))).unwrap();
            assert_eq!(out["uid"], uid);
            assert_eq!(
                call(&p, Some(uid), "status").unwrap(),
                json!({ "connected": true, "account": uid })
            );
        }
        assert_eq!(
            call(&p, None, "status").unwrap(),
            json!({ "connected": false, "account": "" }),
            "control: no caller is not connected"
        );
    }

    #[test]
    fn an_unknown_action_is_a_validation_error() {
        let err = call(&provider(), Some("u1"), "dropDatabase").unwrap_err();
        assert_eq!(err.code.as_deref(), Some(VALIDATION_ERROR));
        assert_eq!(err.message, "knowledge has no node action 'dropDatabase'");
    }

    #[test]
    fn the_auth_check_comes_before_the_unknown_action_check() {
        let err = call(&provider(), None, "dropDatabase").unwrap_err();
        assert_eq!(err.code.as_deref(), Some(AUTH_ERROR));
    }

    #[test]
    fn action_failures_map_to_the_codes_the_ts_integration_reports() {
        let p = provider();
        for (action, code) in [
            ("missing", Some(SERVICE_ERROR)),
            ("conflict", Some(SERVICE_ERROR)),
            ("service", Some(SERVICE_ERROR)),
            ("invalid", Some(VALIDATION_ERROR)),
            ("stop", Some(CANCELLED)),
            ("plain", None),
        ] {
            let err = call(&p, Some("u1"), action).unwrap_err();
            assert_eq!(err.code.as_deref(), code, "{action}");
            assert_eq!(err.message, "refused", "{action}");
        }
    }

    #[test]
    fn a_streamed_model_reply_is_relayed_as_start_and_deltas_until_done() {
        let seen = Arc::new(Mutex::new(Vec::<KnowledgeProgress>::new()));
        let sink = seen.clone();
        let p = provider().with_progress(Arc::new(move |m| sink.lock().unwrap().push(m.clone())));
        assert_eq!(call(&p, Some("u1"), "stream").unwrap(), json!("hello"));
        let phases: Vec<(String, String)> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|m| (m.phase.clone(), m.text.clone()))
            .collect();
        assert_eq!(
            phases,
            [("start", ""), ("delta", "he"), ("delta", "llo")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn a_cancelled_call_stops_streaming_with_cancelled() {
        let token = CancelToken::default();
        token.cancel();
        let p = provider().with_cancel(token);
        let err = call(&p, Some("u1"), "stream").unwrap_err();
        assert_eq!(err.code.as_deref(), Some(CANCELLED));
    }

    #[test]
    fn registered_through_integrations_the_contract_checks_run_first() {
        let creds = Credentials::default();
        let mut integrations = Integrations::new(&creds).unwrap();
        register(&mut integrations, Some(provider()));
        let err = integrations
            .call_json(
                SERVICE,
                "dropDatabase",
                &Map::new(),
                &CallContext::anonymous(),
            )
            .unwrap_err();
        assert_eq!(err.code.as_deref(), Some(VALIDATION_ERROR));
        assert!(
            err.message
                .starts_with("Validation failed for knowledge.dropDatabase"),
            "{}",
            err.message
        );
        assert_eq!(
            integrations
                .call_json(SERVICE, "status", &Map::new(), &learner(Some("u1")))
                .unwrap(),
            json!({ "connected": true, "account": "u1" })
        );
    }

    #[test]
    fn without_stores_the_service_is_unconfigured() {
        let mut integrations = Integrations::new(&Credentials::default()).unwrap();
        register(&mut integrations, None);
        let err = integrations
            .call_json(SERVICE, "status", &Map::new(), &CallContext::anonymous())
            .unwrap_err();
        assert_eq!(err.code.as_deref(), Some(crate::error::NOT_CONFIGURED));
    }
}
