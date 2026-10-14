//! The Iconify API as the icon lookup needs it (`iconSources.ts`), over the `iconify` provider.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::IconSources;
use crate::provider::{CallContext, Provider};

pub struct IconifyIconSources {
    iconify: Arc<dyn Provider>,
}

impl IconifyIconSources {
    pub fn new(iconify: Arc<dyn Provider>) -> Self {
        Self { iconify }
    }

    fn call(&self, action: &str, params: Value) -> Option<Value> {
        let map: Map<String, Value> = params.as_object().cloned().unwrap_or_default();
        self.iconify
            .call(action, &map, &CallContext::anonymous())
            .ok()
    }
}

impl IconSources for IconifyIconSources {
    fn svg_exists(&self, path: &str) -> bool {
        self.call("svgExists", json!({ "path": path }))
            .and_then(|r| r.get("exists").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    fn search(&self, query: &str) -> Option<String> {
        let result = self.call("search", json!({ "query": query, "limit": 1 }))?;
        result
            .get("icons")?
            .as_array()?
            .first()?
            .as_str()
            .map(str::to_string)
    }

    fn icon_body(&self, icon_id: &str) -> Option<String> {
        let result = self.call("getIconBody", json!({ "iconId": icon_id }))?;
        result.get("body")?.as_str().map(str::to_string)
    }
}
