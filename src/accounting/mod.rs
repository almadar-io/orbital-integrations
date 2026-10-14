//! The `accounting` service: the Rust twin of `AccountingIntegration` (generic CSV export, no network).
//! Actions: `exportInvoices`, `exportJournal`.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jsnumber::js_string;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::Transport;
use crate::wallclock::{system_clock, utc_date, Clock};

pub const SERVICE: &str = "accounting";

const INVOICE_COLUMNS: [&str; 10] = [
    "id", "number", "customer", "issuedAt", "dueAt", "currency", "net", "tax", "gross", "status",
];
const JOURNAL_COLUMNS: [&str; 6] = [
    "date",
    "account",
    "description",
    "debit",
    "credit",
    "reference",
];

pub struct AccountingProvider {
    clock: Clock,
}

/// `accounting` needs no credentials: the manager configures it unconditionally.
pub fn register(
    integrations: &mut Integrations,
    _creds: &Credentials,
    _transport: Arc<dyn Transport>,
) {
    integrations.register(SERVICE, Arc::new(AccountingProvider::new()));
}

impl Default for AccountingProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountingProvider {
    pub fn new() -> Self {
        Self {
            clock: system_clock(),
        }
    }

    /// Replace the clock that stamps the export filename (tests; hosts with their own clock).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn export_rows(
        &self,
        rows: Option<&Value>,
        columns: &[&str],
        kind: &str,
    ) -> Result<Value, IntegrationError> {
        let Some(Value::Array(rows)) = rows else {
            return Err(IntegrationError::plain(format!(
                "{kind} export requires an array of rows"
            )));
        };
        let mut lines = vec![columns.join(",")];
        for row in rows {
            let Value::Object(record) = row else {
                return Err(IntegrationError::plain(format!(
                    "{kind} export: every row must be an object"
                )));
            };
            let cells: Vec<String> = columns
                .iter()
                .map(|column| csv_cell(record.get(*column)))
                .collect();
            lines.push(cells.join(","));
        }
        Ok(json!({
            "content": format!("{}\r\n", lines.join("\r\n")),
            "filename": format!("{kind}-export-{}.csv", utc_date((self.clock)())),
            "count": rows.len(),
        }))
    }
}

impl Provider for AccountingProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "exportInvoices" => {
                self.export_rows(params.get("invoices"), &INVOICE_COLUMNS, "invoices")
            }
            "exportJournal" => self.export_rows(params.get("entries"), &JOURNAL_COLUMNS, "journal"),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

fn csv_cell(value: Option<&Value>) -> String {
    let raw = match value {
        None | Some(Value::Null) => return String::new(),
        Some(v) => js_string(v),
    };
    if raw.contains(['"', ',', '\r', '\n']) {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_a_cell_with_a_comma_is_quoted_and_a_plain_one_is_not() {
        assert_eq!(csv_cell(Some(&json!("a,b"))), "\"a,b\"");
        assert_eq!(csv_cell(Some(&json!("ab"))), "ab");
        assert_eq!(csv_cell(Some(&json!(["a", "b", null]))), "\"a,b,\"");
    }
}
