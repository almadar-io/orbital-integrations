//! How the Rawi feed picks and orders its cards (`feedRanking.ts`): pure, deterministic for a cursor.

use std::collections::{HashMap, HashSet};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

use super::common::{locale_compare, num_order};
use crate::json::number;

const HOUR: f64 = 3_600_000.0;
const DAY: f64 = 24.0 * HOUR;

pub const SEEN_COOLDOWN_MS: f64 = 12.0 * HOUR;
pub const DEFAULT_FEED_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct FeedMix {
    pub review: usize,
    pub frontier: usize,
    pub explore: usize,
}

pub const DEFAULT_FEED_MIX: FeedMix = FeedMix {
    review: 3,
    frontier: 4,
    explore: 1,
};

/// One concept of the learner's, with everything ranking reads.
#[derive(Debug, Clone)]
pub struct FeedConcept {
    pub graph_id: String,
    pub path_title: String,
    pub concept_id: String,
    pub route_key: String,
    pub name: String,
    pub layer: f64,
    pub sequence: f64,
    pub mastery: f64,
    pub last_studied: Option<f64>,
    pub prerequisites: Vec<String>,
    pub neighbours: Vec<String>,
    pub liked: bool,
    pub hidden: bool,
    pub last_seen: Option<f64>,
    pub content: Option<Value>,
}

/// A concept adjacent to a path, proposed but not in the graph.
#[derive(Debug, Clone)]
pub struct ExploreCandidate {
    pub graph_id: String,
    pub path_title: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct FeedCursor {
    pub seen: Vec<String>,
    pub as_of: f64,
}

pub struct FeedPage {
    pub cards: Vec<Value>,
    pub next_cursor: Option<String>,
}

pub fn owned_card_id(graph_id: &str, concept_id: &str) -> String {
    format!("{graph_id}:{concept_id}")
}

pub fn explore_card_id(graph_id: &str, name: &str) -> String {
    format!("explore:{graph_id}:{name}")
}

pub fn encode_cursor(cursor: &FeedCursor) -> String {
    let seen = Value::Array(
        cursor
            .seen
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    let text = format!("{{\"seen\":{seen},\"asOf\":{}}}", number(cursor.as_of));
    URL_SAFE_NO_PAD.encode(text.as_bytes())
}

/// The cursor a previous page returned; `Ok(None)` when the text is not one, `Err` where JS throws reading a field of `null`.
pub fn decode_cursor(text: &str) -> Result<Option<FeedCursor>, String> {
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')) else {
        return Ok(None);
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)) else {
        return Ok(None);
    };
    if parsed.is_null() {
        return Err("Cannot read properties of null (reading 'seen')".to_string());
    }
    let Some(seen) = parsed.get("seen").and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(seen) = seen
        .iter()
        .map(|s| s.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let Some(as_of) = parsed.get("asOf").and_then(Value::as_f64) else {
        return Ok(None);
    };
    Ok(Some(FeedCursor { seen, as_of }))
}

fn interval_of(mastery: f64) -> Option<f64> {
    if mastery == 1.0 {
        Some(DAY)
    } else if mastery == 2.0 {
        Some(3.0 * DAY)
    } else if mastery == 3.0 {
        Some(10.0 * DAY)
    } else {
        None
    }
}

/// How far past due a concept is (ms); `None` when it is not due.
pub fn overdue_by(c: &FeedConcept, now: f64) -> Option<f64> {
    let interval = interval_of(c.mastery)?;
    let late = now - c.last_studied? - interval;
    (late >= 0.0).then_some(late)
}

/// Unlearned, and every prerequisite in its path is at mastery 2 or more.
pub fn is_frontier(c: &FeedConcept, by_key: &HashMap<String, &FeedConcept>) -> bool {
    c.mastery == 0.0
        && c.prerequisites.iter().all(|p| {
            by_key
                .get(&owned_card_id(&c.graph_id, p))
                .map_or(0.0, |x| x.mastery)
                >= 2.0
        })
}

fn to_card(c: &FeedConcept, source: &str) -> Value {
    json!({
        "id": owned_card_id(&c.graph_id, &c.concept_id),
        "source": source,
        "graphId": c.graph_id,
        "pathTitle": c.path_title,
        "conceptId": c.concept_id,
        "routeKey": c.route_key,
        "name": c.name,
        "masteryLevel": number(c.mastery),
        "liked": c.liked,
        "content": c.content.clone().unwrap_or(Value::Null),
    })
}

fn explore_to_card(e: &ExploreCandidate) -> Value {
    json!({
        "id": explore_card_id(&e.graph_id, &e.name),
        "source": "explore",
        "graphId": e.graph_id,
        "pathTitle": e.path_title,
        "conceptId": "",
        "routeKey": "",
        "name": e.name,
        "masteryLevel": 0,
        "liked": false,
        "content": null,
    })
}

fn boosted_first(
    items: Vec<&FeedConcept>,
    boosted: impl Fn(&FeedConcept) -> bool,
) -> Vec<&FeedConcept> {
    let mut out: Vec<&FeedConcept> = items.iter().copied().filter(|c| boosted(c)).collect();
    out.extend(items.iter().copied().filter(|c| !boosted(c)));
    out
}

/// One page of the feed: review (most overdue first), frontier (by level, then order) and explore buckets, a
/// liked concept lifting its neighbours to the front of their bucket, interleaved by `mix`.
pub fn rank_feed(
    concepts: &[FeedConcept],
    explore: &[ExploreCandidate],
    now: f64,
    limit: usize,
    mix: FeedMix,
    cursor: Option<&FeedCursor>,
) -> FeedPage {
    let as_of = cursor.map_or(now, |c| c.as_of);
    let seen: HashSet<&str> = cursor
        .map(|c| c.seen.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let by_key: HashMap<String, &FeedConcept> = concepts
        .iter()
        .map(|c| (owned_card_id(&c.graph_id, &c.concept_id), c))
        .collect();
    let lifted_keys: HashSet<String> = concepts
        .iter()
        .filter(|c| c.liked)
        .flat_map(|c| c.neighbours.iter().map(|n| owned_card_id(&c.graph_id, n)))
        .collect();
    let lifted = |c: &FeedConcept| lifted_keys.contains(&owned_card_id(&c.graph_id, &c.concept_id));
    let available = |c: &FeedConcept| {
        !c.hidden && !seen.contains(owned_card_id(&c.graph_id, &c.concept_id).as_str())
    };
    let cooling_down = |c: &FeedConcept| c.last_seen.is_some_and(|s| as_of - s < SEEN_COOLDOWN_MS);

    let mut due: Vec<&FeedConcept> = concepts
        .iter()
        .filter(|c| available(c) && overdue_by(c, as_of).is_some())
        .collect();
    due.sort_by(|a, b| {
        num_order(
            overdue_by(b, as_of).unwrap_or(0.0),
            overdue_by(a, as_of).unwrap_or(0.0),
        )
    });
    let review: Vec<Value> = boosted_first(due, lifted)
        .into_iter()
        .map(|c| to_card(c, "review"))
        .collect();

    let mut ready: Vec<&FeedConcept> = concepts
        .iter()
        .filter(|c| available(c) && !cooling_down(c) && is_frontier(c, &by_key))
        .collect();
    ready.sort_by(|a, b| {
        num_order(a.layer, b.layer)
            .then_with(|| num_order(a.sequence, b.sequence))
            .then_with(|| locale_compare(&a.name, &b.name))
    });
    let frontier: Vec<Value> = boosted_first(ready, lifted)
        .into_iter()
        .map(|c| to_card(c, "frontier"))
        .collect();

    let explore_cards: Vec<Value> = explore
        .iter()
        .map(explore_to_card)
        .filter(|card| !seen.contains(card.get("id").and_then(Value::as_str).unwrap_or_default()))
        .collect();

    let buckets = [
        (review, mix.review),
        (frontier, mix.frontier),
        (explore_cards, mix.explore),
    ];
    let mut cards: Vec<Value> = Vec::new();
    let mut cursors = [0usize; 3];
    while cards.len() < limit {
        let mut took = false;
        for (i, (bucket, weight)) in buckets.iter().enumerate() {
            for _ in 0..*weight {
                if cards.len() >= limit {
                    break;
                }
                let Some(card) = bucket.get(cursors[i]) else {
                    break;
                };
                cards.push(card.clone());
                cursors[i] += 1;
                took = true;
            }
        }
        if !took {
            break;
        }
    }
    let remaining = buckets
        .iter()
        .enumerate()
        .any(|(i, (bucket, _))| cursors[i] < bucket.len());
    let next_cursor = remaining.then(|| {
        let mut all: Vec<String> = cursor.map(|c| c.seen.clone()).unwrap_or_default();
        all.extend(
            cards
                .iter()
                .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string)),
        );
        encode_cursor(&FeedCursor { seen: all, as_of })
    });
    FeedPage { cards, next_cursor }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concept(id: &str, mastery: f64, last: Option<f64>) -> FeedConcept {
        FeedConcept {
            graph_id: "g".into(),
            path_title: "P".into(),
            concept_id: id.into(),
            route_key: id.into(),
            name: id.into(),
            layer: 1.0,
            sequence: 1.0,
            mastery,
            last_studied: last,
            prerequisites: Vec::new(),
            neighbours: Vec::new(),
            liked: false,
            hidden: false,
            last_seen: None,
            content: None,
        }
    }

    #[test]
    fn a_concept_is_due_once_its_interval_has_passed() {
        let c = concept("a", 1.0, Some(0.0));
        assert_eq!(overdue_by(&c, DAY - 1.0), None);
        assert_eq!(overdue_by(&c, DAY + 5.0), Some(5.0));
        assert_eq!(overdue_by(&concept("b", 0.0, Some(0.0)), 1e12), None);
    }

    #[test]
    fn the_cursor_round_trips_and_a_stranger_is_refused() {
        let cursor = FeedCursor {
            seen: vec!["g:a".into()],
            as_of: 5.0,
        };
        let back = decode_cursor(&encode_cursor(&cursor)).unwrap().unwrap();
        assert_eq!(back.seen, cursor.seen);
        assert_eq!(back.as_of, 5.0);
        assert!(decode_cursor("nonsense").unwrap().is_none());
    }

    #[test]
    fn pages_interleave_the_buckets_and_leave_a_cursor_while_cards_remain() {
        let concepts = vec![
            concept("a", 0.0, None),
            concept("b", 0.0, None),
            concept("c", 0.0, None),
        ];
        let page = rank_feed(
            &concepts,
            &[],
            0.0,
            2,
            FeedMix {
                review: 0,
                frontier: 1,
                explore: 0,
            },
            None,
        );
        assert_eq!(page.cards.len(), 2);
        let cursor = decode_cursor(&page.next_cursor.unwrap()).unwrap().unwrap();
        let rest = rank_feed(&concepts, &[], 0.0, 2, DEFAULT_FEED_MIX, Some(&cursor));
        assert_eq!(rest.cards.len(), 1);
        assert!(rest.next_cursor.is_none());
    }
}
