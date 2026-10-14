//! Rust providers answering the kernel's `call-service` requests, each held equal to its `@almadar/integrations` twin by recorded-fixture parity tests.

pub mod accounting;
pub mod analytics;
pub mod apikeys;
pub mod arxiv;
pub mod banking;
#[cfg(feature = "google")]
pub mod calendar;
pub mod contract;
pub mod credential_admin;
pub mod credentials;
pub mod database;
pub mod docker;
#[cfg(feature = "google")]
pub mod drive;
pub mod email;
pub mod error;
pub mod esign;
pub mod formwire;
pub mod github;
#[cfg(feature = "google")]
pub mod google;
pub mod hmacsha256;
pub mod iconify;
pub mod infra;
pub mod jsnumber;
pub mod json;
pub mod jswire;
pub mod knowledge;
pub mod llm;
pub mod meta_ads;
pub mod ml;
pub mod mock;
pub mod oauth;
pub mod otel;
pub mod provider;
pub mod push;
pub mod queue;
pub mod redis;
pub mod storage;
pub mod stripe;
pub mod transport;
pub mod twilio;
pub mod uricomponent;
pub mod wallclock;
pub mod webhook;
pub mod wikimedia;
pub mod workspace;
pub mod youtube;

pub use credentials::Credentials;
pub use error::IntegrationError;
pub use mock::{CoreMockResponder, MockResponder, Mode};
pub use provider::{CallContext, Integrations, Provider, UNMOCKED_SERVICES};
#[cfg(feature = "http")]
pub use transport::UreqTransport;
pub use transport::{HttpRequest, HttpResponse, Transport, TransportError};
