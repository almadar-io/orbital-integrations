//! The pure knowledge-graph engine: no I/O, no clocks. Twin of `integrations/knowledge/engine/{pure,mutations,validator,coerce,interactiveOrbitalTypes}.ts`.
//! The async server half of the TS engine (access layer, vectors, LLM operations) belongs to the I/O actions.

pub mod coerce;
pub mod interactive_orbital_types;
pub mod mutations;
pub mod pure;
pub mod validator;

pub use interactive_orbital_types::INTERACTIVE_ORBITAL_TYPES;
pub use mutations::{GraphMutation, MutationBatch, MutationContext, MutationError};
pub use pure::{GraphNode, NodeBasedKnowledgeGraph, OrderedMap, Relationship};
pub use validator::{validate_graph, Severity, Violation};
