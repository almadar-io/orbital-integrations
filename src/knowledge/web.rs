//! The browser leg of `knowledge` (twin of `createKnowledgeService`, `knowledge/service.ts`): `status`,
//! `connect` and `currentTopics`, composed from KFlow's jump-back-in, goals and recent-activity routes with
//! the learner's own sign-in. The sign-in is a port; the HTTP goes through `Transport`.

use std::cmp::Ordering;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::error::{IntegrationError, SERVICE_ERROR};
use crate::json::number;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};
use crate::wallclock::{system_clock, Clock};

pub const NETWORK_ERROR: &str = "NETWORK_ERROR";
pub const AUTH_ERROR: &str = "AUTH_ERROR";
const DAY_MS: f64 = 86_400_000.0;
const DEFAULT_LIMIT: f64 = 5.0;
const DEFAULT_SINCE_DAYS: f64 = 30.0;

/// Who is connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub connected: bool,
    pub account: String,
}

/// The learner's sign-in (`KnowledgeAuthPort`).
pub trait KnowledgeAuth: Send + Sync {
    fn status(&self) -> Result<Connection, IntegrationError>;
    fn connect(&self) -> Result<Connection, IntegrationError>;
    /// A current ID token; raises `AUTH_ERROR` when no learner is connected.
    fn id_token(&self) -> Result<String, IntegrationError>;
}

pub struct WebProvider {
    base_url: String,
    auth: Arc<dyn KnowledgeAuth>,
    transport: Arc<dyn Transport>,
    clock: Clock,
}

impl WebProvider {
    pub fn new(
        base_url: impl Into<String>,
        auth: Arc<dyn KnowledgeAuth>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            auth,
            transport,
            clock: system_clock(),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

/// Register the browser leg as the `knowledge` service.
pub fn register(integrations: &mut Integrations, provider: WebProvider) {
    integrations.register(super::SERVICE, Arc::new(provider));
}

/// `new URL(path, baseUrl)` for an absolute path: the base's origin followed by the path.
fn resolve(base_url: &str, path: &str) -> Option<String> {
    let (scheme, rest) = base_url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    {
        return None;
    }
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return None;
    }
    Some(format!("{}://{host}{path}", scheme.to_ascii_lowercase()))
}

fn field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key)
}

fn string_of(object: &Map<String, Value>, key: &str) -> Option<String> {
    field(object, key)?.as_str().map(str::to_string)
}

fn number_of(object: &Map<String, Value>, key: &str) -> Option<f64> {
    field(object, key)?.as_f64()
}

struct PathItem {
    title: String,
    last_accessed_at: f64,
    done: f64,
}

struct Goal {
    title: String,
    target: String,
    updated_at: f64,
}

struct Activity {
    kind: String,
    name: String,
    timestamp: f64,
}

fn rows<'a>(body: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    body.as_object()?.get(key)?.as_array()
}

fn parse_paths(body: &Value) -> Option<Vec<PathItem>> {
    rows(body, "items")?
        .iter()
        .map(|row| {
            let row = row.as_object()?;
            let done = match field(row, "progress") {
                None => 0.0,
                Some(p) => number_of(p.as_object()?, "progressPercentage")?,
            };
            Some(PathItem {
                title: string_of(row, "title")?,
                last_accessed_at: number_of(row, "lastAccessedAt")?,
                done,
            })
        })
        .collect()
}

fn parse_goals(body: &Value) -> Option<Vec<Goal>> {
    rows(body, "goals")?
        .iter()
        .map(|row| {
            let row = row.as_object()?;
            Some(Goal {
                title: string_of(row, "title")?,
                target: string_of(row, "target")?,
                updated_at: number_of(row, "updatedAt")?,
            })
        })
        .collect()
}

fn parse_activity(body: &Value) -> Option<Vec<Activity>> {
    rows(body, "activity")?
        .iter()
        .map(|row| {
            let row = row.as_object()?;
            Some(Activity {
                kind: string_of(row, "type")?,
                name: string_of(row, "resourceName")?,
                timestamp: number_of(row, "timestamp")?,
            })
        })
        .collect()
}

struct Topic {
    label: String,
    keywords: Vec<String>,
    weight: f64,
}

/// JS `Array.prototype.slice(0, end)` length for a numeric `end`.
fn slice_len(len: usize, end: f64) -> usize {
    let len_f = len as f64;
    let end = if end.is_nan() { 0.0 } else { end.trunc() };
    let end = if end < 0.0 {
        (len_f + end).max(0.0)
    } else {
        end.min(len_f)
    };
    end as usize
}

/// A close stand-in for ICU `localeCompare` on the labels KFlow stores: letters ignore case first (lowercase
/// sorts before uppercase on a tie), digits sort before letters, punctuation before digits.
fn locale_compare(a: &str, b: &str) -> Ordering {
    fn class(c: char) -> u8 {
        if c.is_whitespace() {
            0
        } else if c.is_alphabetic() {
            3
        } else if c.is_numeric() {
            2
        } else {
            1
        }
    }
    let primary = |s: &str| -> Vec<(u8, String)> {
        s.chars()
            .map(|c| (class(c), c.to_lowercase().collect::<String>()))
            .collect()
    };
    primary(a).cmp(&primary(b)).then_with(|| {
        let tertiary = |s: &str| -> Vec<bool> { s.chars().map(char::is_uppercase).collect() };
        tertiary(a).cmp(&tertiary(b))
    })
}

impl WebProvider {
    fn get(&self, path: &str, token: &str) -> Result<Value, IntegrationError> {
        let unreachable =
            || IntegrationError::coded(NETWORK_ERROR, format!("KFlow {path} is unreachable"));
        let url = resolve(&self.base_url, path).ok_or_else(unreachable)?;
        let request = HttpRequest {
            method: "GET".to_string(),
            url,
            headers: vec![
                ("authorization".to_string(), format!("Bearer {token}")),
                ("accept".to_string(), "*/*".to_string()),
            ],
            body: Vec::new(),
            timeout: None,
        };
        let response = self.transport.send(&request).map_err(|_| unreachable())?;
        if response.status == 401 || response.status == 403 {
            return Err(IntegrationError::coded(
                AUTH_ERROR,
                "KFlow rejected the sign-in",
            ));
        }
        if !response.is_success() {
            return Err(IntegrationError::coded(
                SERVICE_ERROR,
                format!("KFlow {path} answered {}", response.status),
            ));
        }
        serde_json::from_str(&response.text()).map_err(|e| {
            if response.text().trim().is_empty() {
                IntegrationError::plain("Unexpected end of JSON input")
            } else {
                IntegrationError::plain(format!("Unexpected token in JSON: {e}"))
            }
        })
    }

    fn shape_error(path: &str) -> IntegrationError {
        IntegrationError::coded(
            SERVICE_ERROR,
            format!("KFlow {path} answered an unexpected shape"),
        )
    }

    fn num_param(params: &Map<String, Value>, name: &str) -> Result<Option<f64>, IntegrationError> {
        match params.get(name) {
            None => Ok(None),
            Some(v) => v.as_f64().map(Some).ok_or_else(|| {
                IntegrationError::validation(format!("Param '{name}' must be a number"))
            }),
        }
    }

    fn current_topics(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let limit = Self::num_param(params, "limit")?.unwrap_or(DEFAULT_LIMIT);
        let since_days = Self::num_param(params, "sinceDays")?.unwrap_or(DEFAULT_SINCE_DAYS);
        let token = self.auth.id_token()?;

        const PATHS: &str = "/api/user/jump-back-in";
        const GOALS: &str = "/api/learning/goals";
        const ACTIVITY: &str = "/api/user/statistics/recent-activity?limit=50";
        let paths =
            parse_paths(&self.get(PATHS, &token)?).ok_or_else(|| Self::shape_error(PATHS))?;
        let goals =
            parse_goals(&self.get(GOALS, &token)?).ok_or_else(|| Self::shape_error(GOALS))?;
        let activity = parse_activity(&self.get(ACTIVITY, &token)?)
            .ok_or_else(|| Self::shape_error(ACTIVITY))?;

        let now = (self.clock)() as f64;
        let recency = |at: f64| -> f64 {
            let v = 1.0 - (now - at) / DAY_MS / since_days;
            if v.is_nan() {
                v
            } else {
                v.max(0.0)
            }
        };
        let mut topics: Vec<Topic> = Vec::new();
        let mut add = |label: &str, keywords: Vec<String>, weight: f64| {
            if weight <= 0.0 {
                return;
            }
            match topics.iter_mut().find(|t| t.label == label) {
                None => topics.push(Topic {
                    label: label.to_string(),
                    keywords,
                    weight,
                }),
                Some(prev) => {
                    for k in keywords {
                        if !prev.keywords.contains(&k) {
                            prev.keywords.push(k);
                        }
                    }
                    prev.weight = if prev.weight.is_nan() || weight.is_nan() {
                        f64::NAN
                    } else {
                        prev.weight.max(weight)
                    };
                }
            }
        };

        for p in &paths {
            if p.done >= 100.0 {
                continue;
            }
            add(
                &p.title,
                Vec::new(),
                recency(p.last_accessed_at) * (1.0 - p.done / 100.0),
            );
        }
        for g in &goals {
            let keywords = if g.target.is_empty() {
                Vec::new()
            } else {
                vec![g.target.clone()]
            };
            add(&g.title, keywords, recency(g.updated_at));
        }
        for a in &activity {
            if a.kind == "concept_studied" {
                add(&a.name, Vec::new(), recency(a.timestamp));
            }
        }

        topics.sort_by(|a, b| {
            let diff = b.weight - a.weight;
            if diff.is_nan() || diff == 0.0 {
                locale_compare(&a.label, &b.label)
            } else if diff < 0.0 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        });
        let keep = slice_len(topics.len(), limit);
        let out: Vec<Value> = topics
            .into_iter()
            .take(keep)
            .map(|t| {
                json!({
                    "label": t.label,
                    "keywords": t.keywords,
                    "weight": number(t.weight),
                })
            })
            .collect();
        Ok(json!({ "topics": out }))
    }
}

fn connection_json(c: Connection) -> Value {
    json!({ "connected": c.connected, "account": c.account })
}

impl Provider for WebProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "status" => self.auth.status().map(connection_json),
            "connect" => self.auth.connect().map(connection_json),
            "currentTopics" => self.current_topics(params),
            _ => Err(IntegrationError::validation(format!(
                "knowledge has no action '{action}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_keeps_the_origin_and_replaces_the_base_path() {
        assert_eq!(
            resolve("https://kflow.test/app/", "/api/x?limit=50").as_deref(),
            Some("https://kflow.test/api/x?limit=50")
        );
        assert_eq!(
            resolve("http://127.0.0.1:8080", "/a").as_deref(),
            Some("http://127.0.0.1:8080/a")
        );
        assert_eq!(resolve("not a url", "/a"), None);
        assert_eq!(resolve("https://", "/a"), None);
    }

    #[test]
    fn slice_follows_js_semantics() {
        assert_eq!(slice_len(3, 5.0), 3);
        assert_eq!(slice_len(3, 2.9), 2);
        assert_eq!(slice_len(3, -1.0), 2);
        assert_eq!(slice_len(3, -9.0), 0);
        assert_eq!(slice_len(3, 0.0), 0);
    }

    #[test]
    fn locale_compare_ignores_case_before_breaking_ties_lowercase_first() {
        assert_eq!(locale_compare("apple", "Banana"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("A", "a"), Ordering::Greater);
        assert_eq!(locale_compare("1", "a"), Ordering::Less);
        assert_eq!(locale_compare("x", "x"), Ordering::Equal);
    }

    #[test]
    fn shape_checks_reject_what_zod_rejects() {
        assert!(parse_paths(&json!({"items": [{"title": "t", "lastAccessedAt": 1}]})).is_some());
        assert!(parse_paths(
            &json!({"items": [{"title": "t", "lastAccessedAt": 1, "progress": null}]})
        )
        .is_none());
        assert!(parse_paths(&json!({"items": [{"title": 3, "lastAccessedAt": 1}]})).is_none());
        assert!(parse_paths(&json!({})).is_none());
        assert!(
            parse_goals(&json!({"goals": [{"title": "t", "target": "", "updatedAt": 1}]}))
                .is_some()
        );
        assert!(parse_goals(&json!({"goals": [{"title": "t", "updatedAt": 1}]})).is_none());
        assert!(parse_activity(&json!({"activity": []})).is_some());
        assert!(
            parse_activity(&json!({"activity": [{"type": "x", "resourceName": "n"}]})).is_none()
        );
    }
}
