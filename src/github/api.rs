//! The GitHub REST client: the twin of `github-api.ts`.

use base64::Engine;
use serde_json::{json, Map, Value};

use super::{GithubProvider, Repo};
use crate::error::{IntegrationError, SERVICE_ERROR, VALIDATION_ERROR};
use crate::jswire::fetch_failure;
use crate::transport::HttpRequest;
use crate::uricomponent::{encode_uri_component, form_urlencode};

const API: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
/// What `fetch` sends as the content type of a string body when the caller names none.
const STRING_BODY_TYPE: &str = "text/plain;charset=UTF-8";

const RATE_LIMIT_ERROR: &str = "RATE_LIMIT_ERROR";
const AUTH_ERROR: &str = "AUTH_ERROR";
const NETWORK_ERROR: &str = "NETWORK_ERROR";

/// `githubFetch`: a request to `endpoint` with the bearer token; the parsed JSON body of a 2xx reply.
fn fetch(
    provider: &GithubProvider,
    endpoint: &str,
    post: Option<String>,
) -> Result<Value, IntegrationError> {
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", provider.token),
        ),
        (
            "Accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        ("X-GitHub-Api-Version".to_string(), API_VERSION.to_string()),
    ];
    let method = if post.is_some() { "POST" } else { "GET" };
    if post.is_some() {
        headers.push(("Content-Type".to_string(), STRING_BODY_TYPE.to_string()));
    }
    let request = HttpRequest {
        method: method.to_string(),
        url: format!("{API}{endpoint}"),
        headers,
        body: post.unwrap_or_default().into_bytes(),
        timeout: None,
    };
    let network = |detail: String| {
        IntegrationError::coded(
            NETWORK_ERROR,
            format!("GitHub API request failed: {detail}"),
        )
    };
    let response = provider
        .transport
        .send(&request)
        .map_err(|e| network(fetch_failure(e).message))?;

    if !response.is_success() {
        let status_text = response.status_text();
        let error: Value = serde_json::from_str(&response.text())
            .unwrap_or_else(|_| json!({ "message": status_text }));
        let detail = match error.get("message") {
            Some(Value::String(m)) if !m.is_empty() => m.clone(),
            _ => status_text.to_string(),
        };
        return Err(match response.status {
            429 => IntegrationError::coded(RATE_LIMIT_ERROR, "GitHub API rate limit exceeded"),
            401 | 403 => IntegrationError::coded(
                AUTH_ERROR,
                format!("GitHub API authentication failed: {detail}"),
            ),
            _ => IntegrationError::coded(
                SERVICE_ERROR,
                format!("GitHub API request failed: {detail}"),
            ),
        });
    }
    serde_json::from_str(&response.text()).map_err(|e| network(e.to_string()))
}

/// A JS number in a template string: whole values print without a fraction.
pub(super) fn js_number(value: &Value) -> String {
    match value.as_f64() {
        Some(n) if n.fract() == 0.0 && n.abs() < 1e21 => format!("{}", n as i64),
        Some(n) => n.to_string(),
        None => String::new(),
    }
}

/// `Array.prototype.join` element text.
fn js_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(_) => js_number(value),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// JS truthiness of an optional param, as `if (options?.path)` reads it.
fn truthy(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    })
}

/// `JSON.stringify` of an object whose undefined members drop out.
fn body_of(entries: &[(&str, Option<Value>)]) -> String {
    let mut map = Map::new();
    for (key, value) in entries {
        if let Some(v) = value {
            map.insert((*key).to_string(), v.clone());
        }
    }
    Value::Object(map).to_string()
}

fn param(params: &Map<String, Value>, key: &str) -> Option<Value> {
    params.get(key).cloned()
}

fn field(data: &Value, key: &str) -> Option<Value> {
    data.get(key).cloned()
}

/// `new URLSearchParams(...).toString()`.
fn query_string(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn api_path(repo: &Repo, tail: &str) -> String {
    format!("/repos/{}/{}{tail}", repo.owner, repo.name)
}

fn per_call_repo(params: &Map<String, Value>) -> Repo {
    let text = |key: &str| params.get(key).map(js_text).unwrap_or_default();
    Repo {
        owner: text("owner"),
        name: text("repo"),
    }
}

pub fn create_pr(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = provider.configured_repo()?;
    let body = body_of(&[
        ("title", param(params, "title")),
        ("body", param(params, "body")),
        ("base", param(params, "baseBranch")),
        ("head", param(params, "headBranch")),
        (
            "draft",
            Some(param(params, "draft").unwrap_or(Value::Bool(false))),
        ),
    ]);
    fetch(provider, &api_path(&repo, "/pulls"), Some(body))
}

pub fn get_pr_comments(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = provider.configured_repo()?;
    let number = params.get("prNumber").map(js_number).unwrap_or_default();
    let comments = fetch(
        provider,
        &api_path(&repo, &format!("/pulls/{number}/comments")),
        None,
    )?;
    Ok(json!({ "comments": comments }))
}

pub fn list_issues(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = provider.configured_repo()?;
    let state = params
        .get("state")
        .map_or_else(|| "open".to_string(), js_text);
    let per_page = params
        .get("per_page")
        .and_then(Value::as_f64)
        .unwrap_or(30.0);
    let mut query = vec![
        ("state", state),
        ("per_page", js_number(&json!(per_page.min(100.0)))),
    ];
    if let Some(Value::Array(labels)) = params.get("labels") {
        if !labels.is_empty() {
            let joined: Vec<String> = labels.iter().map(js_text).collect();
            query.push(("labels", joined.join(",")));
        }
    }
    let issues = fetch(
        provider,
        &api_path(&repo, &format!("/issues?{}", query_string(&query))),
        None,
    )?;
    Ok(json!({ "issues": issues }))
}

pub fn get_issue(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = provider.configured_repo()?;
    let number = params.get("issueNumber").map(js_number).unwrap_or_default();
    let issue = fetch(
        provider,
        &api_path(&repo, &format!("/issues/{number}")),
        None,
    )?;
    let comments = fetch(
        provider,
        &api_path(&repo, &format!("/issues/{number}/comments")),
        None,
    )?;
    Ok(json!({ "issue": issue, "comments": comments }))
}

/// `Buffer.from(content, 'base64').toString('utf-8')`: whitespace and stray characters are skipped, padding ends
/// the data, and invalid UTF-8 becomes U+FFFD.
pub(super) fn decode_base64_lenient(content: &str) -> String {
    let mut symbols: Vec<u8> = Vec::new();
    for byte in content.bytes() {
        match byte {
            b'=' => break,
            b'-' => symbols.push(b'+'),
            b'_' => symbols.push(b'/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => symbols.push(byte),
            _ => {}
        }
    }
    if symbols.len() % 4 == 1 {
        symbols.pop();
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let bytes = engine.decode(&symbols).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn get_file(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = per_call_repo(params);
    let path = params.get("path").map(js_text).unwrap_or_default();
    let query = match truthy(params.get("ref")) {
        Some(r) => format!("?ref={}", encode_uri_component(&js_text(r))),
        None => String::new(),
    };
    let data = fetch(
        provider,
        &api_path(&repo, &format!("/contents/{path}{query}")),
        None,
    )?;
    let encoding = data.get("encoding").and_then(Value::as_str);
    let content = match (encoding, data.get("content")) {
        (Some("base64"), Some(Value::String(c))) => Some(Value::String(decode_base64_lenient(c))),
        (_, other) => other.cloned(),
    };
    let mut out = Map::new();
    for (key, value) in [
        ("path", field(&data, "path")),
        ("content", content),
        ("sha", field(&data, "sha")),
        ("size", field(&data, "size")),
    ] {
        if let Some(v) = value {
            out.insert(key.to_string(), v);
        }
    }
    Ok(Value::Object(out))
}

pub fn list_commits(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = per_call_repo(params);
    let mut query: Vec<(&str, String)> = Vec::new();
    for (key, source) in [("path", "path"), ("sha", "ref"), ("per_page", "per_page")] {
        if let Some(value) = truthy(params.get(source)) {
            query.push((key, js_text(value)));
        }
    }
    let tail = if query.is_empty() {
        "/commits".to_string()
    } else {
        format!("/commits?{}", query_string(&query))
    };
    let data = fetch(provider, &api_path(&repo, &tail), None)?;
    let items = data.as_array().cloned().unwrap_or_default();
    let commits: Vec<Value> = items
        .iter()
        .map(|item| {
            let author = item.pointer("/commit/author");
            let mut out = Map::new();
            for (key, value) in [
                ("sha", field(item, "sha")),
                ("message", item.pointer("/commit/message").cloned()),
                ("author", author.and_then(|a| a.get("name")).cloned()),
                ("date", author.and_then(|a| a.get("date")).cloned()),
            ] {
                if let Some(v) = value {
                    out.insert(key.to_string(), v);
                }
            }
            Value::Object(out)
        })
        .collect();
    Ok(json!({ "commits": commits }))
}

pub fn create_issue(
    provider: &GithubProvider,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let repo = per_call_repo(params);
    let body = body_of(&[
        ("title", param(params, "title")),
        ("body", param(params, "body")),
        ("labels", param(params, "labels")),
    ]);
    let data = fetch(provider, &api_path(&repo, "/issues"), Some(body))?;
    let mut out = Map::new();
    for key in ["number", "title", "url"] {
        if let Some(v) = field(&data, key) {
            out.insert(key.to_string(), v);
        }
    }
    Ok(Value::Object(out))
}

/// `parseRepoUrl`: the owner and repository named by an HTTPS clone URL.
pub fn parse_repo_url(repo_url: &str) -> Result<(String, String), IntegrationError> {
    let failed = || {
        IntegrationError::coded(
            VALIDATION_ERROR,
            format!("Failed to parse repository URL: {repo_url}"),
        )
    };
    let url = url::Url::parse(repo_url).map_err(|_| failed())?;
    let parts: Vec<&str> = url.path().split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err(failed());
    }
    let repo = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
    Ok((parts[0].to_string(), repo.to_string()))
}
