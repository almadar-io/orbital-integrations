//! What the social actions need from outside the process (`socialSources.ts`): model calls and Wikipedia.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::model::schema as z;
use super::persona_logic::{persona_user_prompt, PERSONA_SYSTEM_PROMPT};
use crate::knowledge::store::{KnowledgeError, ModelPort, StoreResult};
use crate::provider::{CallContext, Provider};

#[derive(Debug, Clone)]
pub struct PersonaDraft {
    pub name: String,
    pub description: String,
    pub greeting: String,
}

#[derive(Debug, Clone, Default)]
pub struct WikiPage {
    pub description: Option<String>,
    pub portrait_url: Option<String>,
    pub extract: Option<String>,
}

fn trimmed_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Everything the social actions need from outside the process: model calls and Wikipedia.
pub trait SocialSources: Send + Sync {
    fn badge_labels(&self, canonical_id: &str, kind: &str) -> StoreResult<Vec<String>>;
    fn relevance(&self, content: &str, badge_label: &str) -> StoreResult<(f64, Option<String>)>;
    fn persona_draft(
        &self,
        concept_label: &str,
        correction: Option<&str>,
        context: Option<&str>,
    ) -> StoreResult<PersonaDraft>;
    fn wiki_page(&self, title: &str) -> WikiPage;
}

/// The production sources: structured model calls through the `ModelPort`, Wikipedia through the `wikimedia` provider.
pub struct ModelSocialSources {
    pub models: Arc<dyn ModelPort>,
    pub wikimedia: Arc<dyn Provider>,
}

fn structured(
    models: &dyn ModelPort,
    system: &str,
    user: &str,
    temperature: f64,
    shape: Value,
) -> StoreResult<Value> {
    models.generate_structured(&json!({
        "systemPrompt": system,
        "userPrompt": user,
        "provider": "deepseek",
        "temperature": temperature,
        "schema": shape,
    }))
}

impl SocialSources for ModelSocialSources {
    fn badge_labels(&self, canonical_id: &str, kind: &str) -> StoreResult<Vec<String>> {
        let out = structured(
            self.models.as_ref(),
            "You pick discussion sub-topics for a knowledge peer-connection. Return JSON {\"labels\":[...]} with 3-5 short strings \
(max 6 words each): the most useful sub-topics two learners might discuss for the given topic. No prose, no numbering.",
            &format!("Topic ({kind}): {canonical_id}"),
            0.4,
            z::object(vec![("labels", z::array(z::string(true, true), Some(1), Some(5)))], &[]),
        )?;
        let labels: Option<Vec<String>> = out
            .get("labels")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().map(|l| trimmed_string(Some(l))).collect());
        match labels {
            Some(l) if (1..=5).contains(&l.len()) => Ok(l),
            _ => Err(KnowledgeError::other(
                "The model's badge labels are invalid",
            )),
        }
    }

    fn relevance(&self, content: &str, badge_label: &str) -> StoreResult<(f64, Option<String>)> {
        let out = structured(
            self.models.as_ref(),
            "You moderate a knowledge discussion scoped to one topic. Reply ONLY with compact JSON: \
{\"score\":<0.0-1.0>,\"reason\":<short>}. \"score\" is how on-topic the message is to the topic. \
Greeting/social-only/off-topic/spam means a low score.",
            &format!("Topic: {badge_label}\nMessage: {content}"),
            0.0,
            z::object(vec![("score", z::number()), ("reason", z::string(false, false))], &["reason"]),
        )?;
        let score = out
            .get("score")
            .and_then(Value::as_f64)
            .ok_or_else(|| KnowledgeError::other("The model's score is invalid"))?;
        Ok((
            score,
            out.get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        ))
    }

    fn persona_draft(
        &self,
        concept_label: &str,
        correction: Option<&str>,
        context: Option<&str>,
    ) -> StoreResult<PersonaDraft> {
        let out = structured(
            self.models.as_ref(),
            PERSONA_SYSTEM_PROMPT,
            &persona_user_prompt(concept_label, correction, context),
            if correction.is_none() { 0.3 } else { 0.2 },
            z::object(
                vec![
                    ("name", z::string(true, true)),
                    ("description", z::string(true, true)),
                    ("greeting", z::string(true, true)),
                ],
                &[],
            ),
        )?;
        match (
            trimmed_string(out.get("name")),
            trimmed_string(out.get("description")),
            trimmed_string(out.get("greeting")),
        ) {
            (Some(name), Some(description), Some(greeting)) => Ok(PersonaDraft {
                name,
                description,
                greeting,
            }),
            _ => Err(KnowledgeError::other("The model's persona is invalid")),
        }
    }

    fn wiki_page(&self, title: &str) -> WikiPage {
        let params: Map<String, Value> = Map::from_iter([("title".to_string(), json!(title))]);
        let Ok(page) = self
            .wikimedia
            .call("getPage", &params, &CallContext::anonymous())
        else {
            return WikiPage::default();
        };
        let text = |k: &str| page.get(k).and_then(Value::as_str).map(str::to_string);
        WikiPage {
            description: text("description"),
            portrait_url: text("portraitUrl"),
            extract: text("extract"),
        }
    }
}
