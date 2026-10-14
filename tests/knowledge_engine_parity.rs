//! Twin parity for the pure `knowledge` engine: every input -> output pair `__tests__/knowledge-engine-fixtures.test.ts`
//! recorded from the TS engine (`fixtures/knowledge-engine/*.json`) must come out the same from the Rust engine.

use std::path::PathBuf;

use orbital_integrations::knowledge::engine::mutations;
use orbital_integrations::knowledge::engine::pure::{
    GraphNode, NodeBasedKnowledgeGraph, Relationship,
};
use orbital_integrations::knowledge::engine::{validate_graph, Violation};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct Kind {
    kind: String,
    case: String,
}

#[derive(Deserialize)]
struct ValidateFixture {
    input: NodeBasedKnowledgeGraph,
    output: Vec<Violation>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum Op {
    CreateNode {
        node: GraphNode,
    },
    UpdateNode {
        #[serde(rename = "nodeId")]
        node_id: String,
        updates: Map<String, Value>,
    },
    DeleteNode {
        #[serde(rename = "nodeId")]
        node_id: String,
    },
    DeleteNodes {
        #[serde(rename = "nodeIds")]
        node_ids: Vec<String>,
    },
    CreateRelationship {
        relationship: Relationship,
    },
    UpdateRelationship {
        #[serde(rename = "relId")]
        rel_id: String,
        updates: Map<String, Value>,
    },
    DeleteRelationship {
        #[serde(rename = "relId")]
        rel_id: String,
    },
    DeleteRelationshipsBetween {
        #[serde(rename = "sourceId")]
        source_id: String,
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(rename = "type")]
        rel_type: Option<String>,
    },
    DeleteRelationshipsByType {
        #[serde(rename = "type")]
        rel_type: String,
    },
    DeleteRelationships {
        #[serde(rename = "relIds")]
        rel_ids: Vec<String>,
    },
}

#[derive(Deserialize)]
struct MutateInput {
    graph: NodeBasedKnowledgeGraph,
    op: Op,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MutateOutput {
    Ok { result: Value, saved: Value },
    Err { message: String },
}

#[derive(Deserialize)]
struct MutateFixture {
    now: f64,
    input: MutateInput,
    output: MutateOutput,
}

fn texts() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/knowledge-engine");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("fixtures/knowledge-engine is missing ({e}); record it with ALMADAR_RECORD_FIXTURES"))
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("read fixture");
            let kind: Kind =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (format!("{}:{}", kind.kind, kind.case), text)
        })
        .collect()
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("serialize")
}

/// Apply `op` and report (result, saved?, graph): `saved` mirrors whether the TS service persisted the graph.
fn apply(graph: &mut NodeBasedKnowledgeGraph, op: &Op, now: f64) -> Result<(Value, bool), String> {
    Ok(match op {
        Op::CreateNode { node } => (
            to_value(&mutations::create_node(graph, node.clone(), now)?),
            true,
        ),
        Op::UpdateNode { node_id, updates } => (
            to_value(&mutations::update_node(graph, node_id, updates, now)?),
            true,
        ),
        Op::DeleteNode { node_id } => {
            mutations::delete_node(graph, node_id, now)?;
            (Value::Null, true)
        }
        Op::DeleteNodes { node_ids } => {
            mutations::delete_nodes(graph, node_ids, now);
            (Value::Null, true)
        }
        Op::CreateRelationship { relationship } => {
            let (rel, changed) = mutations::create_relationship(graph, relationship.clone(), now)?;
            (to_value(&rel), changed)
        }
        Op::UpdateRelationship { rel_id, updates } => (
            to_value(&mutations::update_relationship(
                graph, rel_id, updates, now,
            )?),
            true,
        ),
        Op::DeleteRelationship { rel_id } => {
            mutations::delete_relationship(graph, rel_id, now)?;
            (Value::Null, true)
        }
        Op::DeleteRelationshipsBetween {
            source_id,
            target_id,
            rel_type,
        } => {
            let n = mutations::delete_relationships_between(
                graph,
                source_id,
                target_id,
                rel_type.as_deref(),
                now,
            );
            (Value::from(n), n > 0)
        }
        Op::DeleteRelationshipsByType { rel_type } => {
            let n = mutations::delete_relationships_by_type(graph, rel_type, now);
            (Value::from(n), n > 0)
        }
        Op::DeleteRelationships { rel_ids } => {
            mutations::delete_relationships(graph, rel_ids, now);
            (Value::Null, true)
        }
    })
}

fn check_validate(case: &str, text: &str) {
    let fixture: ValidateFixture =
        serde_json::from_str(text).unwrap_or_else(|e| panic!("{case}: {e}"));
    assert_eq!(validate_graph(&fixture.input), fixture.output, "{case}");
}

fn check_mutate(case: &str, text: &str) {
    let fixture: MutateFixture =
        serde_json::from_str(text).unwrap_or_else(|e| panic!("{case}: {e}"));
    let mut graph = fixture.input.graph;
    let outcome = apply(&mut graph, &fixture.input.op, fixture.now);
    match (fixture.output, outcome) {
        (MutateOutput::Ok { result, saved }, Ok((actual, was_saved))) => {
            assert_eq!(actual, result, "{case}: result");
            let after = if was_saved {
                to_value(&graph)
            } else {
                Value::Null
            };
            assert_eq!(after, saved, "{case}: saved graph");
        }
        (MutateOutput::Ok { .. }, Err(e)) => panic!("{case}: expected success, got {e}"),
        (MutateOutput::Err { message }, Err(e)) => {
            assert!(e.contains(&message), "{case}: {e} vs {message}")
        }
        (MutateOutput::Err { message }, Ok(_)) => panic!("{case}: expected failure `{message}`"),
    }
}

#[test]
fn every_recorded_case_replays_to_the_same_output() {
    let all = texts();
    let validates = all
        .iter()
        .filter(|(c, _)| c.starts_with("validate:"))
        .count();
    let mutates = all.iter().filter(|(c, _)| c.starts_with("mutate:")).count();
    assert!(
        validates >= 20 && mutates >= 20,
        "expected the recorded engine cases, found {validates} validate / {mutates} mutate"
    );
    for (case, text) in &all {
        if case.starts_with("validate:") {
            check_validate(case, text);
        } else {
            check_mutate(case, text);
        }
    }
}

#[test]
fn control_a_drifted_violation_is_caught() {
    let (case, text) = texts()
        .into_iter()
        .find(|(c, _)| c == "validate:validate-seed-missing")
        .expect("seed-missing fixture");
    let drifted = text.replace("KG_INV_01_SEED_MISSING", "KG_INV_01_SEED_ELSEWHERE");
    assert!(std::panic::catch_unwind(|| check_validate(&case, &drifted)).is_err());
}

#[test]
fn control_a_drifted_mutation_result_is_caught() {
    let (case, text) = texts()
        .into_iter()
        .find(|(c, _)| c == "mutate:mutate-create-node-new-type")
        .expect("create-node fixture");
    let drifted = text.replacen("\"layerNumber\": 1", "\"layerNumber\": 2", 1);
    assert_ne!(drifted, text);
    assert!(std::panic::catch_unwind(|| check_mutate(&case, &drifted)).is_err());
}

#[test]
fn the_validator_reports_in_wire_order() {
    let (_, text) = texts()
        .into_iter()
        .find(|(c, _)| c == "validate:validate-violations-keep-node-order")
        .expect("order fixture");
    let fixture: ValidateFixture = serde_json::from_str(&text).expect("fixture");
    let nodes: Vec<&str> = fixture
        .output
        .iter()
        .filter_map(|v| v.node_id.as_deref())
        .collect();
    assert_eq!(
        nodes,
        ["z-last", "a-first", "m-mid", "z-last", "z-last", "a-first"]
    );
    let mut sorted = nodes.clone();
    sorted.sort_unstable();
    assert_ne!(
        nodes, sorted,
        "control: wire order is not alphabetical order"
    );
}
