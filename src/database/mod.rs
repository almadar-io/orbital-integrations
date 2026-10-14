//! The `database` service: the Rust twin of `DatabaseIntegration` (`@almadar/integrations` `integrations/database`).
//! One action, `query`: a single read-only SELECT (see [`guard`]) against a connection named by `connectionRef`,
//! an env-var name resolved through the credential store and then the environment. The Postgres client sits
//! behind the [`Connector`] port; `postgres` (a default feature) is the native connector.

mod guard;
#[cfg(feature = "postgres")]
pub mod postgres;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

pub use self::guard::{assert_read_only_select, GuardResult};
use crate::credentials::Credentials;
use crate::error::{IntegrationError, SERVICE_ERROR, VALIDATION_ERROR};
use crate::infra::{str_param, unknown_action};
use crate::provider::{CallContext, Integrations, Provider};

pub const SERVICE: &str = "database";
const DEFAULT_STATEMENT_TIMEOUT_MS: f64 = 10_000.0;
/// Postgres SQLSTATE raised when `statement_timeout` cancels a query.
const PG_QUERY_CANCELED: &str = "57014";
const AUTH_ERROR: &str = "AUTH_ERROR";
const TIMEOUT_ERROR: &str = "TIMEOUT_ERROR";

/// A failure from the database client: its message and, when it carries one, its SQLSTATE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverError {
    pub message: String,
    pub code: Option<String>,
}

impl DriverError {
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub rows: Vec<Map<String, Value>>,
    /// The command's row count; absent where the driver reports none (`pg` gives `null` for some commands).
    pub row_count: Option<u64>,
}

/// An open pool for one connection string.
pub trait Connection: Send + Sync {
    fn query(&self, sql: &str, params: &[Value]) -> Result<QueryResult, DriverError>;
}

/// Opens connections: the seam where the Postgres client plugs in.
pub trait Connector: Send + Sync {
    fn connect(
        &self,
        connection_string: &str,
        statement_timeout_ms: f64,
    ) -> Result<Arc<dyn Connection>, DriverError>;
}

pub struct DatabaseProvider {
    creds: Credentials,
    connector: Arc<dyn Connector>,
    statement_timeout_ms: f64,
    connections: Mutex<HashMap<String, Arc<dyn Connection>>>,
}

/// `Number(text)` for the cases an env value takes: decimal, exponent, `0x`/`0o`/`0b`, surrounding white space.
fn js_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    match trimmed {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        other
            if other
                .chars()
                .any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') =>
        {
            f64::NAN
        }
        other => other.parse().unwrap_or(f64::NAN),
    }
}

/// Register `database`: always configured (it declares no credentials); a bad `DATABASE_STATEMENT_TIMEOUT_MS`
/// fails every call with the TS constructor's error.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    connector: Arc<dyn Connector>,
) {
    match DatabaseProvider::new(creds, connector) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl DatabaseProvider {
    pub fn new(
        creds: &Credentials,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, IntegrationError> {
        let statement_timeout_ms = match creds.get("DATABASE_STATEMENT_TIMEOUT_MS") {
            Some(raw) => {
                let parsed = js_number(raw);
                if !parsed.is_finite() || parsed <= 0.0 {
                    return Err(IntegrationError::plain(format!(
                        "Invalid DATABASE_STATEMENT_TIMEOUT_MS: {raw}"
                    )));
                }
                parsed
            }
            None => DEFAULT_STATEMENT_TIMEOUT_MS,
        };
        Ok(Self {
            creds: creds.clone(),
            connector,
            statement_timeout_ms,
            connections: Mutex::new(HashMap::new()),
        })
    }

    fn connection_for(&self, reference: &str) -> Result<Arc<dyn Connection>, IntegrationError> {
        let connection_string = self.creds.get(reference).ok_or_else(|| {
            IntegrationError::coded(
                AUTH_ERROR,
                format!("Connection reference \"{reference}\" is not set in the credential store or environment"),
            )
        })?;
        let mut cache = self.connections.lock().expect("connections");
        if let Some(open) = cache.get(connection_string) {
            return Ok(open.clone());
        }
        let open = self
            .connector
            .connect(connection_string, self.statement_timeout_ms)
            .map_err(|e| IntegrationError::plain(e.message))?;
        cache.insert(connection_string.to_string(), open.clone());
        Ok(open)
    }

    fn run_query(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let sql = str_param(params, "sql")?;
        let guard = assert_read_only_select(sql);
        if !guard.ok {
            return Err(IntegrationError::coded(
                VALIDATION_ERROR,
                format!(
                    "Read-only violation: {}",
                    guard.reason.as_deref().unwrap_or("not a SELECT statement")
                ),
            ));
        }
        let connection = self.connection_for(str_param(params, "connectionRef")?)?;
        // `[...params]`: an array spreads, a string spreads into its characters, anything else is not iterable.
        let binds: Vec<Value> = match params.get("params") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(Value::String(text)) => {
                text.chars().map(|c| Value::String(c.to_string())).collect()
            }
            Some(_) => {
                return Err(IntegrationError::coded(
                    SERVICE_ERROR,
                    "params is not iterable",
                ))
            }
        };
        match connection.query(sql, &binds) {
            Ok(result) => {
                let count = result.row_count.unwrap_or(result.rows.len() as u64);
                Ok(json!({ "rows": result.rows, "rowCount": count }))
            }
            Err(error) if error.code.as_deref() == Some(PG_QUERY_CANCELED) => Err(
                IntegrationError::coded(TIMEOUT_ERROR, "Statement timeout exceeded"),
            ),
            Err(error) => Err(IntegrationError::coded(SERVICE_ERROR, error.message)),
        }
    }
}

impl Provider for DatabaseProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "query" => self.run_query(params),
            other => Err(unknown_action("database", other)),
        }
    }
}
