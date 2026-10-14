//! The isomorphic graph model (twin of `engine/pure.ts` + the envelope types in `engine/schema`). Node
//! properties stay a JSON map: per-type shapes are the action ports' concern, the envelope is what the engine reads.
//!
//! Object key order is data here: the validator reports violations in `Object.entries` order, so `nodes` and
//! `nodeTypes` keep their wire order (`OrderedMap`). JS orders integer-like keys first; ids are never integers.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};

/// A JSON object whose keys keep their insertion order.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderedMap<T> {
    entries: Vec<(String, T)>,
}

impl<T> Default for OrderedMap<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> OrderedMap<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&T> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut T> {
        self.entries
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Replace in place (keeping the key's position) or append.
    pub fn insert(&mut self, key: String, value: T) -> Option<T> {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => Some(std::mem::replace(slot, value)),
            None => {
                self.entries.push((key, value));
                None
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<T> {
        let at = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(at).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &T)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|(_, v)| v)
    }

    /// `get_mut`, inserting `make()` first when the key is absent.
    pub fn get_or_insert_with(&mut self, key: &str, make: impl FnOnce() -> T) -> &mut T {
        if !self.contains_key(key) {
            self.entries.push((key.to_string(), make()));
        }
        match self.get_mut(key) {
            Some(v) => v,
            None => unreachable!("the key was inserted above"),
        }
    }
}

impl<T: Serialize> Serialize for OrderedMap<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (k, v) in &self.entries {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OrderedMap<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = OrderedMap<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut out = OrderedMap::new();
                while let Some((k, v)) = access.next_entry::<String, T>()? {
                    out.insert(k, v);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(V(PhantomData))
    }
}

/// Whole numbers stay integral on the wire, as JS prints them.
fn ser_whole<S: Serializer>(n: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    crate::json::number(*n).serialize(serializer)
}

fn ser_whole_opt<S: Serializer>(n: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match n {
        Some(n) => ser_whole(n, serializer),
        None => serializer.serialize_none(),
    }
}

/// One graph node: the envelope the engine reads, with the per-type `properties` as a JSON map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub properties: Map<String, Value>,
    #[serde(rename = "createdAt", default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Number>,
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Number>,
}

/// A stored key that may be missing, present as `null`, or set: TS writes all three, and a document must
/// round-trip as it was written.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Stored<T> {
    #[default]
    Absent,
    Null,
    Value(T),
}

impl<T> Stored<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Stored::Absent)
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Stored::Value(v) => Some(v),
            Stored::Absent | Stored::Null => None,
        }
    }

    /// TS `removeUndefined`: an unset key is written as `null`.
    pub fn or_null(self) -> Self {
        match self {
            Stored::Absent => Stored::Null,
            kept => kept,
        }
    }
}

impl<T: Serialize> Serialize for Stored<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Stored::Value(v) => v.serialize(serializer),
            Stored::Absent | Stored::Null => serializer.serialize_none(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Stored<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(v) => Stored::Value(v),
            None => Stored::Null,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(rename = "type")]
    pub rel_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Stored::is_absent")]
    pub strength: Stored<Number>,
    #[serde(default, skip_serializing_if = "Stored::is_absent")]
    pub metadata: Stored<Map<String, Value>>,
    #[serde(rename = "createdAt", default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Number>,
    #[serde(
        rename = "sourceGraphId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_graph_id: Option<String>,
    #[serde(
        rename = "targetGraphId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub target_graph_id: Option<String>,
}

/// A whole graph. Fields the engine does not read (`model`, `difficulty`, `focus`, ...) ride along in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeBasedKnowledgeGraph {
    pub id: String,
    #[serde(
        rename = "seedConceptId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub seed_concept_id: Option<String>,
    #[serde(rename = "createdAt", default, serialize_with = "ser_whole")]
    pub created_at: f64,
    #[serde(rename = "updatedAt", default, serialize_with = "ser_whole")]
    pub updated_at: f64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_whole_opt"
    )]
    pub version: Option<f64>,
    #[serde(rename = "authorUid", default, skip_serializing_if = "Option::is_none")]
    pub author_uid: Option<String>,
    #[serde(default)]
    pub nodes: OrderedMap<GraphNode>,
    #[serde(rename = "nodeTypes", default)]
    pub node_types: OrderedMap<Vec<String>>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl NodeBasedKnowledgeGraph {
    pub fn new(id: impl Into<String>, now: f64) -> Self {
        Self {
            id: id.into(),
            seed_concept_id: None,
            created_at: now,
            updated_at: now,
            version: None,
            author_uid: None,
            nodes: OrderedMap::new(),
            node_types: OrderedMap::new(),
            relationships: Vec::new(),
            extra: Map::new(),
        }
    }

    /// The ids indexed under `node_type`, empty when the index has no such list.
    pub fn ids_of(&self, node_type: &str) -> &[String] {
        self.node_types.get(node_type).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ordered_map_keeps_wire_order_through_a_round_trip() {
        let text = r#"{"z":1,"a":2,"m":3}"#;
        let map: OrderedMap<i32> = serde_json::from_str(text).unwrap();
        assert_eq!(map.keys().cloned().collect::<Vec<_>>(), ["z", "a", "m"]);
        assert_eq!(serde_json::to_string(&map).unwrap(), text);
    }

    #[test]
    fn insert_replaces_in_place_and_remove_closes_the_gap() {
        let mut map = OrderedMap::new();
        map.insert("a".into(), 1);
        map.insert("b".into(), 2);
        assert_eq!(map.insert("a".into(), 9), Some(1));
        assert_eq!(map.keys().cloned().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(map.remove("a"), Some(9));
        assert_eq!(map.remove("a"), None);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn a_graph_keeps_unread_fields_and_omits_absent_ones() {
        let wire = json!({
            "id": "g", "createdAt": 1, "updatedAt": 2, "model": "m", "focus": "f",
            "nodes": { "c": { "id": "c", "type": "Concept", "properties": { "name": "N" } } },
            "nodeTypes": { "Concept": ["c"] },
            "relationships": []
        });
        let graph: NodeBasedKnowledgeGraph = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(graph.extra["model"], json!("m"));
        assert_eq!(graph.ids_of("Concept"), ["c"]);
        assert!(graph.ids_of("Layer").is_empty());
        let back = serde_json::to_value(&graph).unwrap();
        assert!(back.get("version").is_none());
        assert_eq!(back["focus"], json!("f"));
    }
}

#[cfg(test)]
mod stored_tests {
    use super::*;
    use serde_json::json;

    fn rel(extra: Value) -> Value {
        let mut base = json!({ "id": "r", "source": "a", "target": "b", "type": "hasChild" });
        if let (Some(map), Value::Object(more)) = (base.as_object_mut(), extra) {
            map.extend(more);
        }
        base
    }

    fn round_trip(doc: Value) -> Value {
        let parsed: Relationship = serde_json::from_value(doc).unwrap();
        serde_json::to_value(parsed).unwrap()
    }

    #[test]
    fn an_absent_key_stays_absent() {
        let doc = rel(json!({}));
        assert_eq!(round_trip(doc.clone()), doc);
    }

    #[test]
    fn a_null_key_stays_null() {
        let doc = rel(json!({ "strength": null, "metadata": null }));
        assert_eq!(round_trip(doc.clone()), doc);
    }

    #[test]
    fn a_set_key_keeps_its_value() {
        let doc = rel(json!({ "strength": 0.5, "metadata": { "k": 1 } }));
        let parsed: Relationship = serde_json::from_value(doc.clone()).unwrap();
        assert_eq!(parsed.strength.value().and_then(Number::as_f64), Some(0.5));
        assert_eq!(serde_json::to_value(parsed).unwrap(), doc);
    }

    #[test]
    fn or_null_writes_an_unset_key_as_null_and_keeps_the_rest() {
        assert_eq!(Stored::<Number>::Absent.or_null(), Stored::Null);
        assert_eq!(Stored::Value(1).or_null(), Stored::Value(1));
        assert_eq!(Stored::<Number>::Null.or_null(), Stored::Null);
    }
}
