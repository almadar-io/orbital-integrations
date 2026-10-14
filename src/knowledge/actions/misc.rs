//! Icons and model preferences (`misc.ts`).

use serde_json::{json, Map, Value};

use super::icon_logic::{classify_tone, devicon_slug, icon_cache_key};
use super::model_options::{is_model_key, DEFAULT_EXPAND_MODEL, DEFAULT_EXPLAIN_MODEL, MODEL_KEYS};
use super::params::{self, refine_failed, validate};
use super::{action, Env, IconSources};
use crate::knowledge::store::{IconCacheEntry, KnowledgeError, ResolvedIcon, StoreResult};
use crate::knowledge::ActionTable;

fn resolve_icon(sources: &dyn IconSources, term: &str) -> Option<ResolvedIcon> {
    let slug = devicon_slug(term);
    let mut icon_id = None;
    if !slug.is_empty() && sources.svg_exists(&format!("devicon/{slug}.svg")) {
        icon_id = Some(format!("devicon:{slug}"));
    }
    let icon_id = match icon_id {
        Some(id) => id,
        None => sources.search(term)?,
    };
    let body = sources.icon_body(&icon_id)?;
    Some(ResolvedIcon {
        tone: classify_tone(&body).to_string(),
        icon: icon_id,
    })
}

fn model_issue(params: &Map<String, Value>, key: &str) -> Option<String> {
    match params.get(key) {
        None => None,
        Some(Value::String(s)) if is_model_key(s) => None,
        Some(Value::String(_)) => Some(format!(
            "{key}: unknown model; expected one of {}",
            MODEL_KEYS.join(", ")
        )),
        Some(other) => Some(format!(
            "{key}: Expected string, received {}",
            match other {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::Array(_) => "array",
                _ => "object",
            }
        )),
    }
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let icons = env.icons.clone();
    table.insert(
        "conceptIcon",
        action(move |_call, p, deps| -> StoreResult<Value> {
            validate(p, &[params::trimmed("label")])?;
            let term = params::str_of(p, "label").trim().to_string();
            let key = icon_cache_key(&term);
            let cached = deps.icons.get(&key)?;
            let resolved = match &cached {
                Some(entry) => entry.icon.clone(),
                None => resolve_icon(icons.as_ref(), &term),
            };
            if cached.is_none() {
                deps.icons.put(
                    &key,
                    IconCacheEntry {
                        label: term,
                        icon: resolved.clone(),
                        fetched_at: deps.graphs.now_ms(),
                    },
                )?;
            }
            Ok(match resolved {
                Some(r) => json!({ "icon": r.icon, "tone": r.tone }),
                None => json!({ "icon": "", "tone": "none" }),
            })
        }),
    );

    table.insert(
        "getPreferences",
        action(|call, _p, deps| {
            let prefs = deps.students.get_preferences(call.uid)?;
            let pick = |key: &str, default: &str| -> String {
                prefs
                    .as_ref()
                    .and_then(|p| p.get(key))
                    .and_then(Value::as_str)
                    .filter(|m| is_model_key(m))
                    .unwrap_or(default)
                    .to_string()
            };
            Ok(json!({
                "expandModel": pick("expandModel", DEFAULT_EXPAND_MODEL),
                "explainModel": pick("explainModel", DEFAULT_EXPLAIN_MODEL),
            }))
        }),
    );

    table.insert(
        "setPreferences",
        action(|call, p, deps| {
            let issues: Vec<String> = ["expandModel", "explainModel"]
                .iter()
                .filter_map(|k| model_issue(p, k))
                .collect();
            if !issues.is_empty() {
                return Err(KnowledgeError::validation(issues.join("; ")));
            }
            if !p.contains_key("expandModel") && !p.contains_key("explainModel") {
                return Err(refine_failed("expandModel or explainModel required"));
            }
            let mut patch = Map::new();
            for key in ["expandModel", "explainModel"] {
                if let Some(v) = p.get(key) {
                    patch.insert(key.to_string(), v.clone());
                }
            }
            deps.students.set_preferences(call.uid, &patch)?;
            Ok(json!({ "saved": true }))
        }),
    );
}
