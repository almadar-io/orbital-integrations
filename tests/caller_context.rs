//! The call carries its caller (`IntegrationCallContext`): `ServiceHandler::call` hands the dispatch's `@user`
//! to `Provider::call`, and the providers TS gives `context.principal` / `context.role` read it from there.
//! Mock mode follows `unmockedServices`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use orbital_core::effects::{ServiceCaller, ServiceHandler};
use orbital_core::Value;
use orbital_integrations::credential_admin::{self, Catalog, CredentialsProvider, Hosts};
use orbital_integrations::oauth::{ConnectionRow, OAuthConnectionStore, OauthProvider};
use orbital_integrations::workspace::{self, LocalWorkspaceHost, WorkspaceHost};
use orbital_integrations::{
    CallContext, Credentials, IntegrationError, Integrations, MockResponder, Provider,
};
use serde_json::{json, Map};

fn caller(principal: &str, role: Option<&str>) -> ServiceCaller {
    ServiceCaller {
        principal: principal.into(),
        role: role.map(str::to_string),
    }
}

fn args(value: serde_json::Value) -> Value {
    orbital_integrations::json::from_json(&value)
}

fn call(
    integrations: &Integrations,
    caller: Option<&ServiceCaller>,
    service: &str,
    method: &str,
    value: serde_json::Value,
) -> Result<serde_json::Value, String> {
    ServiceHandler::call(integrations, service, method, &args(value), None, caller)
        .map(|out| orbital_integrations::json::to_json(&out))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("caller-ctx-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_provider_receives_the_callers_context_through_the_service_handler() {
    struct Echo;
    impl Provider for Echo {
        fn call(
            &self,
            _action: &str,
            _params: &Map<String, serde_json::Value>,
            ctx: &CallContext,
        ) -> Result<serde_json::Value, IntegrationError> {
            Ok(json!({ "principal": ctx.principal(), "role": ctx.role() }))
        }
    }
    let mut integrations = Integrations::new(&Credentials::default()).expect("mode");
    integrations.register("arxiv", Arc::new(Echo));
    let params = json!({ "query": "x" });
    assert_eq!(
        call(
            &integrations,
            Some(&caller("u1", Some("owner"))),
            "arxiv",
            "search",
            params.clone()
        ),
        Ok(json!({ "principal": "u1", "role": "owner" }))
    );
    assert_eq!(
        call(
            &integrations,
            Some(&caller("u2", None)),
            "arxiv",
            "search",
            params.clone()
        ),
        Ok(json!({ "principal": "u2", "role": null }))
    );
    assert_eq!(
        call(&integrations, None, "arxiv", "search", params),
        Ok(json!({ "principal": null, "role": null })),
        "control: no caller is an anonymous context"
    );
}

#[test]
fn workspace_serves_each_caller_their_own_files() {
    let dir = TempDir::new("workspace");
    let host: Arc<dyn WorkspaceHost> = Arc::new(LocalWorkspaceHost::new(&dir.0));
    let mut integrations = Integrations::new(&Credentials::default()).expect("mode");
    workspace::register(&mut integrations, Some(host));
    let (u1, u2) = (caller("u1", None), caller("u2", None));
    let write = json!({ "workspace": "app-1", "path": "a.txt", "content": "mine" });
    call(&integrations, Some(&u1), "workspace", "writeFile", write).expect("u1 writes");
    let read = json!({ "workspace": "app-1", "path": "a.txt" });
    let seen = |who: &ServiceCaller| {
        call(
            &integrations,
            Some(who),
            "workspace",
            "readFile",
            read.clone(),
        )
        .expect("read")
    };
    assert_eq!(seen(&u1)["content"], "mine");
    assert_eq!(seen(&u2)["exists"], false, "u2 never sees u1's file");
    assert_eq!(seen(&u2)["content"], "");
}

#[test]
fn workspace_refuses_a_call_with_no_signed_in_caller() {
    let dir = TempDir::new("workspace-anon");
    let host: Arc<dyn WorkspaceHost> = Arc::new(LocalWorkspaceHost::new(&dir.0));
    let mut integrations = Integrations::new(&Credentials::default()).expect("mode");
    workspace::register(&mut integrations, Some(host));
    let read = json!({ "workspace": "app-1", "path": "a.txt" });
    let refused = "A workspace needs a signed-in caller";
    assert_eq!(
        call(&integrations, None, "workspace", "readFile", read.clone()),
        Err(refused.to_string())
    );
    assert_eq!(
        call(
            &integrations,
            Some(&caller("", None)),
            "workspace",
            "readFile",
            read
        ),
        Err(refused.to_string()),
        "an empty principal is no principal"
    );
}

fn credentials(roles: Option<&str>) -> Integrations {
    let creds = match roles {
        Some(roles) => Credentials::from_env(
            [(
                "ALMADAR_CREDENTIAL_ADMIN_ROLES".to_string(),
                roles.to_string(),
            )]
            .into_iter()
            .collect::<HashMap<_, _>>(),
        ),
        None => Credentials::default(),
    };
    let provider = CredentialsProvider::new(
        &creds,
        Catalog::from_registry().expect("catalog"),
        Hosts::default(),
    );
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(credential_admin::SERVICE, Arc::new(provider));
    integrations
}

#[test]
fn credentials_gate_follows_the_calls_role() {
    let integrations = credentials(None);
    let test = json!({ "service": "arxiv" });
    for role in ["owner", "admin"] {
        assert!(
            call(
                &integrations,
                Some(&caller("u1", Some(role))),
                "credentials",
                "test",
                test.clone()
            )
            .is_ok(),
            "{role} may run a gated action"
        );
    }
    let forbidden = "forbidden: \"test\" requires one of roles: admin, owner";
    for who in [
        Some(caller("u1", Some("viewer"))),
        Some(caller("u1", None)),
        None,
    ] {
        assert_eq!(
            call(
                &integrations,
                who.as_ref(),
                "credentials",
                "test",
                test.clone()
            ),
            Err(forbidden.to_string()),
            "{who:?}"
        );
    }
    assert!(
        call(&integrations, None, "credentials", "list", json!({})).is_ok(),
        "control: list is not role gated"
    );
}

#[test]
fn credentials_admin_roles_come_from_the_configuration() {
    let integrations = credentials(Some("editor"));
    let test = json!({ "service": "arxiv" });
    assert!(call(
        &integrations,
        Some(&caller("u1", Some("editor"))),
        "credentials",
        "test",
        test.clone()
    )
    .is_ok());
    assert!(call(
        &integrations,
        Some(&caller("u1", Some("owner"))),
        "credentials",
        "test",
        test
    )
    .is_err());
}

#[test]
fn credentials_gate_runs_before_contract_validation() {
    let integrations = credentials(None);
    let invalid = json!({});
    let refused = call(&integrations, None, "credentials", "set", invalid.clone()).unwrap_err();
    assert_eq!(
        refused, "forbidden: \"set\" requires one of roles: admin, owner",
        "a refused caller never learns how its params were wrong"
    );
    let owner = caller("u1", Some("owner"));
    let validated = call(&integrations, Some(&owner), "credentials", "set", invalid).unwrap_err();
    assert!(
        validated.starts_with("Validation failed for credentials.set"),
        "control: an admitted caller reaches validation, got {validated}"
    );
}

#[derive(Default)]
struct Connections(Mutex<HashMap<(String, &'static str), Vec<String>>>);

impl OAuthConnectionStore for Connections {
    fn save(
        &self,
        principal: &str,
        provider: orbital_integrations::oauth::OAuthProviderKind,
        grant: &Map<String, serde_json::Value>,
    ) -> Result<ConnectionRow, IntegrationError> {
        let scope: Vec<String> = grant["scope"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.0
            .lock()
            .expect("store")
            .insert((principal.to_string(), provider.name()), scope.clone());
        Ok(row(provider, Some(scope), principal))
    }

    fn status(
        &self,
        principal: &str,
        provider: orbital_integrations::oauth::OAuthProviderKind,
    ) -> Result<ConnectionRow, IntegrationError> {
        let scope = self
            .0
            .lock()
            .expect("store")
            .get(&(principal.to_string(), provider.name()))
            .cloned();
        Ok(row(provider, scope, principal))
    }

    fn remove(
        &self,
        principal: &str,
        provider: orbital_integrations::oauth::OAuthProviderKind,
    ) -> Result<ConnectionRow, IntegrationError> {
        self.0
            .lock()
            .expect("store")
            .remove(&(principal.to_string(), provider.name()));
        Ok(row(provider, None, principal))
    }
}

fn row(
    provider: orbital_integrations::oauth::OAuthProviderKind,
    scope: Option<Vec<String>>,
    principal: &str,
) -> ConnectionRow {
    ConnectionRow {
        provider,
        connected: scope.is_some(),
        account: if scope.is_some() {
            principal.to_string()
        } else {
            String::new()
        },
        avatar_url: String::new(),
        scope: scope.unwrap_or_default(),
        connected_at: String::new(),
    }
}

fn oauth(store: Option<Arc<dyn OAuthConnectionStore>>) -> Integrations {
    let creds = Credentials::from_env(
        [("OAUTH_MODE".to_string(), "mock".to_string())]
            .into_iter()
            .collect::<HashMap<_, _>>(),
    );
    let transport: Arc<dyn orbital_integrations::Transport> = Arc::new(NoNetwork);
    let mut provider = OauthProvider::new(&creds, transport);
    if let Some(store) = store {
        provider = provider.with_connection_store(store);
    }
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register("oauth", Arc::new(provider));
    integrations
}

struct NoNetwork;

impl orbital_integrations::Transport for NoNetwork {
    fn send(
        &self,
        _request: &orbital_integrations::HttpRequest,
    ) -> Result<orbital_integrations::HttpResponse, orbital_integrations::TransportError> {
        Err(orbital_integrations::TransportError::Failed(
            "no network in this test".into(),
        ))
    }
}

fn authorize(integrations: &Integrations, subject: &str) -> String {
    let started = call(
        integrations,
        None,
        "oauth",
        "authorize",
        json!({ "provider": "github", "scopes": ["repo"], "redirectUri": "https://x/cb", "subject": subject }),
    )
    .expect("authorize");
    started["state"].as_str().expect("state").to_string()
}

#[test]
fn oauth_connections_are_keyed_by_the_calls_principal() {
    let integrations = oauth(Some(Arc::new(Connections::default())));
    let (u1, u2) = (caller("u1", None), caller("u2", None));
    let state = authorize(&integrations, "u1");
    let connected = call(
        &integrations,
        Some(&u1),
        "oauth",
        "connect",
        json!({ "provider": "github", "code": "c", "state": state }),
    )
    .expect("connect");
    assert_eq!(connected["connected"], true);
    assert_eq!(connected["account"], "u1");
    let provider = json!({ "provider": "github" });
    let status = |who: &ServiceCaller| {
        call(
            &integrations,
            Some(who),
            "oauth",
            "connection",
            provider.clone(),
        )
        .expect("status")
    };
    assert_eq!(status(&u1)["connected"], true);
    assert_eq!(
        status(&u2)["connected"],
        false,
        "u2 has no connection of its own"
    );
    let gone = call(
        &integrations,
        Some(&u2),
        "oauth",
        "disconnect",
        provider.clone(),
    )
    .expect("u2 disconnect");
    assert_eq!(gone["connected"], false);
    assert_eq!(
        status(&u1)["connected"],
        true,
        "u2's disconnect left u1's connection"
    );
    call(
        &integrations,
        Some(&u1),
        "oauth",
        "disconnect",
        provider.clone(),
    )
    .expect("u1 disconnect");
    assert_eq!(status(&u1)["connected"], false);
}

#[test]
fn oauth_connect_for_another_subject_is_refused_by_the_grant_check() {
    let integrations = oauth(Some(Arc::new(Connections::default())));
    let state = authorize(&integrations, "u1");
    let err = call(
        &integrations,
        Some(&caller("u2", None)),
        "oauth",
        "connect",
        json!({ "provider": "github", "code": "c", "state": state }),
    )
    .unwrap_err();
    assert!(
        err.contains("not the one that started the authorization"),
        "{err}"
    );
}

#[test]
fn oauth_connection_actions_need_a_caller_and_a_store_in_ts_order() {
    let with_store = oauth(Some(Arc::new(Connections::default())));
    let provider = json!({ "provider": "github" });
    for action in ["connection", "disconnect"] {
        assert_eq!(
            call(&with_store, None, "oauth", action, provider.clone()),
            Err("A provider connection needs a signed-in caller".to_string()),
            "{action}"
        );
        assert_eq!(
            call(
                &with_store,
                Some(&caller("", None)),
                "oauth",
                action,
                provider.clone()
            ),
            Err("A provider connection needs a signed-in caller".to_string()),
            "{action}: an empty principal is no principal"
        );
    }
    let connect = json!({ "provider": "github", "code": "c", "state": "s" });
    assert_eq!(
        call(&with_store, None, "oauth", "connect", connect.clone()),
        Err("A provider connection needs a signed-in caller".to_string())
    );
    let without_store = oauth(None);
    let u1 = caller("u1", None);
    for action in ["connection", "disconnect"] {
        assert_eq!(
            call(&without_store, Some(&u1), "oauth", action, provider.clone()),
            Err("No OAuth connection store is installed on this host".to_string()),
            "{action}"
        );
        assert_eq!(
            call(&without_store, None, "oauth", action, provider.clone()),
            Err("No OAuth connection store is installed on this host".to_string()),
            "{action}: TS looks the store up before it reads the caller"
        );
    }
    assert_eq!(
        call(
            &without_store,
            Some(&u1),
            "oauth",
            "connect",
            connect.clone()
        ),
        Err("No OAuth connection store is installed on this host".to_string())
    );
    assert_eq!(
        call(&without_store, None, "oauth", "connect", connect),
        Err("A provider connection needs a signed-in caller".to_string()),
        "connect reads the caller first"
    );
}

struct Fixed;

impl MockResponder for Fixed {
    fn respond(
        &self,
        service: &str,
        action: &str,
        _params: &Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, IntegrationError> {
        Ok(json!({ "mocked": format!("{service}.{action}") }))
    }
}

fn mock_mode() -> Integrations {
    let creds = Credentials::from_env(
        [("ALMADAR_INTEGRATIONS_MODE".to_string(), "mock".to_string())]
            .into_iter()
            .collect::<HashMap<_, _>>(),
    );
    Integrations::new(&creds)
        .expect("mode")
        .with_mock_responder(Arc::new(Fixed))
}

#[test]
fn mock_mode_answers_a_mock_service_from_the_responder_and_runs_workspace_for_real() {
    let dir = TempDir::new("mock-workspace");
    let host: Arc<dyn WorkspaceHost> = Arc::new(LocalWorkspaceHost::new(&dir.0));
    let mut integrations = mock_mode();
    workspace::register(&mut integrations, Some(host));
    integrations.register("arxiv", Arc::new(PanicsIfReached));
    let u1 = caller("u1", None);
    assert_eq!(
        call(
            &integrations,
            Some(&u1),
            "arxiv",
            "search",
            json!({ "query": "x" })
        ),
        Ok(json!({ "mocked": "arxiv.search" }))
    );
    let history = call(
        &integrations,
        Some(&u1),
        "workspace",
        "tree",
        json!({ "workspace": "app-1" }),
    )
    .expect("the real workspace answers");
    assert!(
        history["nodes"].is_array() && history.get("mocked").is_none(),
        "the real workspace answered, not the responder"
    );
    assert_eq!(
        call(
            &integrations,
            None,
            "workspace",
            "tree",
            json!({ "workspace": "app-1" })
        ),
        Err("A workspace needs a signed-in caller".to_string()),
        "the real provider ran, so its caller check ran"
    );
}

#[test]
fn mock_mode_keeps_the_credentials_control_plane_live() {
    let mut integrations = mock_mode();
    let provider = CredentialsProvider::new(
        &Credentials::default(),
        Catalog::from_registry().expect("catalog"),
        Hosts::default(),
    );
    integrations.register(credential_admin::SERVICE, Arc::new(provider));
    let listed = call(&integrations, None, "credentials", "list", json!({})).expect("list");
    assert!(
        listed["entries"].is_array() && listed.get("mocked").is_none(),
        "the real credentials list answered, not the responder"
    );
}

#[test]
fn control_in_live_mode_a_mock_service_is_not_answered_by_the_responder() {
    let mut integrations = Integrations::new(&Credentials::default())
        .expect("mode")
        .with_mock_responder(Arc::new(Fixed));
    integrations.register("arxiv", Arc::new(Reached));
    assert_eq!(
        call(
            &integrations,
            None,
            "arxiv",
            "search",
            json!({ "query": "x" })
        ),
        Ok(json!({ "reached": true }))
    );
}

struct PanicsIfReached;

impl Provider for PanicsIfReached {
    fn call(
        &self,
        _action: &str,
        _params: &Map<String, serde_json::Value>,
        _ctx: &CallContext,
    ) -> Result<serde_json::Value, IntegrationError> {
        panic!("mock mode must answer this service from the responder")
    }
}

struct Reached;

impl Provider for Reached {
    fn call(
        &self,
        _action: &str,
        _params: &Map<String, serde_json::Value>,
        _ctx: &CallContext,
    ) -> Result<serde_json::Value, IntegrationError> {
        Ok(json!({ "reached": true }))
    }
}
