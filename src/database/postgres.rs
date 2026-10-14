//! The native Postgres connector for the `database` service, over the synchronous `postgres` client.
//!
//! Parameters go on the wire as text so the server infers their types, as `node-postgres` sends them; result
//! columns arrive in the binary format and are decoded here into the JSON values `node-postgres` would produce
//! (`int8`/`numeric` as strings, timestamps as ISO strings, `json`/`jsonb` parsed, arrays as arrays).

use std::error::Error;
use std::sync::{Arc, Mutex};

use bytes::BytesMut;
use postgres::config::SslMode;
use postgres::types::{to_sql_checked, FromSql, IsNull, Kind, ToSql, Type};
use postgres::{Client, Config, NoTls};
use serde_json::{json, Map, Value};

use super::{Connection, Connector, DriverError, QueryResult};
use crate::jsnumber::js_number;
use crate::json::number;
use crate::wallclock::iso8601;

/// Opens one lazily connected, mutex-serialized `postgres::Client` per connection string.
pub struct PostgresConnector;

struct PostgresConnection {
    config: Config,
    client: Mutex<Option<Client>>,
}

#[derive(Debug)]
struct TextParam(Option<String>);

impl ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match &self.0 {
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> postgres::types::Format {
        postgres::types::Format::Text
    }

    to_sql_checked!();
}

/// The text `node-postgres` sends for a bind value (`prepareValue`).
fn bind_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(js_number(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => Some(array_literal(items)),
        Value::Object(_) => Some(value.to_string()),
    }
}

fn array_literal(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| match item {
            Value::Null => "NULL".to_string(),
            Value::Array(inner) => array_literal(inner),
            other => {
                let text = bind_text(other).unwrap_or_default();
                format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

struct Raw(Vec<u8>);

impl<'a> FromSql<'a> for Raw {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        Ok(Raw(raw.to_vec()))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

fn be<const N: usize>(raw: &[u8]) -> Result<[u8; N], String> {
    raw.get(..N)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| format!("binary value shorter than {N} bytes"))
}

fn utf8(raw: &[u8]) -> Result<String, String> {
    String::from_utf8(raw.to_vec()).map_err(|e| e.to_string())
}

/// `numeric` in the binary format as the decimal text `node-postgres` returns.
pub fn decode_numeric(raw: &[u8]) -> Result<String, String> {
    let ndigits = i16::from_be_bytes(be(raw)?);
    let weight = i16::from_be_bytes(be(raw.get(2..).unwrap_or_default())?);
    let sign = u16::from_be_bytes(be(raw.get(4..).unwrap_or_default())?);
    let dscale = usize::from(u16::from_be_bytes(be(raw.get(6..).unwrap_or_default())?));
    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        _ => {}
    }
    let digits: Vec<i16> = (0..usize::try_from(ndigits).map_err(|e| e.to_string())?)
        .map(|i| {
            raw.get(8 + i * 2..)
                .ok_or_else(|| "truncated numeric".to_string())
                .and_then(|s| be::<2>(s).map(i16::from_be_bytes))
        })
        .collect::<Result<_, _>>()?;
    let mut integer = String::new();
    for place in 0..=i32::from(weight).max(0) {
        let digit = digits
            .get(usize::try_from(place).unwrap_or(0))
            .copied()
            .unwrap_or(0);
        if integer.is_empty() {
            integer.push_str(&digit.to_string());
        } else {
            integer.push_str(&format!("{digit:04}"));
        }
    }
    if weight < 0 {
        integer = "0".into();
    }
    let mut fraction = String::new();
    let first_fraction = i32::from(weight) + 1;
    let mut place = first_fraction;
    while fraction.len() < dscale {
        let digit = if place < 0 {
            0
        } else {
            digits
                .get(usize::try_from(place).unwrap_or(0))
                .copied()
                .unwrap_or(0)
        };
        fraction.push_str(&format!("{digit:04}"));
        place += 1;
    }
    fraction.truncate(dscale);
    let negative = sign == 0x4000;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&integer);
    if dscale > 0 {
        out.push('.');
        out.push_str(&fraction);
    }
    Ok(out)
}

const POSTGRES_EPOCH_UNIX_MS: i64 = 946_684_800_000;

fn timestamp_value(micros: i64, with_zone: bool) -> Value {
    if micros == i64::MAX || micros == i64::MIN {
        return Value::Null;
    }
    let _ = with_zone;
    json!(iso8601(POSTGRES_EPOCH_UNIX_MS + micros.div_euclid(1000)))
}

fn uuid_text(raw: &[u8]) -> Result<String, String> {
    if raw.len() != 16 {
        return Err("uuid is not 16 bytes".into());
    }
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn time_text(micros: i64) -> String {
    let (h, m, s, us) = (
        micros / 3_600_000_000,
        micros / 60_000_000 % 60,
        micros / 1_000_000 % 60,
        micros % 1_000_000,
    );
    if us == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{:06}", us)
            .trim_end_matches('0')
            .to_string()
    }
}

fn decode_scalar(ty: &Type, raw: &[u8]) -> Result<Value, String> {
    Ok(match *ty {
        Type::BOOL => json!(raw.first().copied().unwrap_or(0) != 0),
        Type::INT2 => json!(i16::from_be_bytes(be(raw)?)),
        Type::INT4 => json!(i32::from_be_bytes(be(raw)?)),
        Type::OID => json!(u32::from_be_bytes(be(raw)?)),
        Type::INT8 => json!(i64::from_be_bytes(be(raw)?).to_string()),
        Type::FLOAT4 => {
            let value = f32::from_be_bytes(be(raw)?);
            number(value.to_string().parse::<f64>().unwrap_or(f64::NAN))
        }
        Type::FLOAT8 => number(f64::from_be_bytes(be(raw)?)),
        Type::NUMERIC => json!(decode_numeric(raw)?),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => json!(utf8(raw)?),
        Type::CHAR => json!(char::from(raw.first().copied().unwrap_or(0)).to_string()),
        Type::UUID => json!(uuid_text(raw)?),
        Type::JSON => serde_json::from_slice(raw).map_err(|e| e.to_string())?,
        Type::JSONB => {
            serde_json::from_slice(raw.get(1..).unwrap_or_default()).map_err(|e| e.to_string())?
        }
        Type::TIMESTAMP => timestamp_value(i64::from_be_bytes(be(raw)?), false),
        Type::TIMESTAMPTZ => timestamp_value(i64::from_be_bytes(be(raw)?), true),
        Type::DATE => {
            let days = i64::from(i32::from_be_bytes(be(raw)?));
            json!(iso8601(POSTGRES_EPOCH_UNIX_MS + days * 86_400_000))
        }
        Type::TIME => json!(time_text(i64::from_be_bytes(be(raw)?))),
        Type::BYTEA => json!({ "type": "Buffer", "data": raw }),
        ref other => {
            return Err(format!(
                "unsupported column type: {} ({})",
                other.name(),
                other.oid()
            ))
        }
    })
}

fn decode_array(elem: &Type, raw: &[u8]) -> Result<Value, String> {
    let ndim = usize::try_from(i32::from_be_bytes(be(raw)?)).map_err(|e| e.to_string())?;
    if ndim == 0 {
        return Ok(Value::Array(Vec::new()));
    }
    let mut at = 12;
    let mut dims = Vec::new();
    for _ in 0..ndim {
        let len = usize::try_from(i32::from_be_bytes(be(raw.get(at..).unwrap_or_default())?))
            .map_err(|e| e.to_string())?;
        dims.push(len);
        at += 8;
    }
    let total: usize = dims.iter().product();
    let mut items = Vec::with_capacity(total);
    for _ in 0..total {
        let len = i32::from_be_bytes(be(raw.get(at..).unwrap_or_default())?);
        at += 4;
        if len < 0 {
            items.push(Value::Null);
        } else {
            let len = len as usize;
            let slice = raw
                .get(at..at + len)
                .ok_or_else(|| "truncated array".to_string())?;
            items.push(decode_scalar(elem, slice)?);
            at += len;
        }
    }
    fn nest(items: &mut std::vec::IntoIter<Value>, dims: &[usize]) -> Value {
        match dims.split_first() {
            None => items.next().unwrap_or(Value::Null),
            Some((len, rest)) => Value::Array((0..*len).map(|_| nest(items, rest)).collect()),
        }
    }
    Ok(nest(&mut items.into_iter(), &dims))
}

fn decode(ty: &Type, raw: Option<&[u8]>) -> Result<Value, String> {
    let Some(raw) = raw else {
        return Ok(Value::Null);
    };
    match ty.kind() {
        Kind::Array(elem) => decode_array(elem, raw),
        Kind::Enum(_) => Ok(Value::String(utf8(raw)?)),
        Kind::Domain(inner) => decode(inner, Some(raw)),
        _ => decode_scalar(ty, raw),
    }
}

fn driver_error(error: &postgres::Error) -> DriverError {
    match error.as_db_error() {
        Some(db) => DriverError {
            message: db.message().to_string(),
            code: Some(db.code().code().to_string()),
        },
        None => DriverError::plain(
            error
                .source()
                .map_or_else(|| error.to_string(), ToString::to_string),
        ),
    }
}

impl Connection for PostgresConnection {
    fn query(&self, sql: &str, params: &[Value]) -> Result<QueryResult, DriverError> {
        let mut slot = self.client.lock().expect("postgres client");
        if slot.is_none() {
            let client = self.config.connect(NoTls).map_err(|e| driver_error(&e))?;
            *slot = Some(client);
        }
        let client = slot.as_mut().expect("connected client");
        let texts: Vec<TextParam> = params.iter().map(|p| TextParam(bind_text(p))).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            texts.iter().map(|t| t as &(dyn ToSql + Sync)).collect();
        let rows = match client.query(sql, &refs) {
            Ok(rows) => rows,
            Err(error) => {
                if client.is_closed() {
                    *slot = None;
                }
                return Err(driver_error(&error));
            }
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut map = Map::new();
            for (index, column) in row.columns().iter().enumerate() {
                let raw: Option<Raw> = row
                    .try_get(index)
                    .map_err(|e| DriverError::plain(e.to_string()))?;
                let value = decode(column.type_(), raw.as_ref().map(|r| r.0.as_slice()))
                    .map_err(DriverError::plain)?;
                map.insert(column.name().to_string(), value);
            }
            out.push(map);
        }
        let count = out.len() as u64;
        Ok(QueryResult {
            rows: out,
            row_count: Some(count),
        })
    }
}

impl Connector for PostgresConnector {
    fn connect(
        &self,
        connection_string: &str,
        statement_timeout_ms: f64,
    ) -> Result<Arc<dyn Connection>, DriverError> {
        let mut config: Config = connection_string
            .parse()
            .map_err(|e: postgres::Error| DriverError::plain(e.to_string()))?;
        if matches!(config.get_ssl_mode(), SslMode::Require) {
            return Err(DriverError::plain(
                "sslmode=require needs TLS, which the Rust Postgres connector does not support yet",
            ));
        }
        config.options(&format!(
            "-c statement_timeout={}",
            statement_timeout_ms.floor() as u64
        ));
        Ok(Arc::new(PostgresConnection {
            config,
            client: Mutex::new(None),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(digits: &[i16], weight: i16, sign: u16, dscale: u16) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend((digits.len() as i16).to_be_bytes());
        raw.extend(weight.to_be_bytes());
        raw.extend(sign.to_be_bytes());
        raw.extend(dscale.to_be_bytes());
        for d in digits {
            raw.extend(d.to_be_bytes());
        }
        raw
    }

    #[test]
    fn numeric_decodes_to_the_text_postgres_prints() {
        assert_eq!(
            decode_numeric(&numeric(&[1234, 5600], 0, 0, 2)).unwrap(),
            "1234.56"
        );
        assert_eq!(
            decode_numeric(&numeric(&[12, 3400, 5678], 1, 0, 4)).unwrap(),
            "123400.5678"
        );
        assert_eq!(
            decode_numeric(&numeric(&[1, 0, 0], 2, 0, 0)).unwrap(),
            "100000000"
        );
        assert_eq!(
            decode_numeric(&numeric(&[5000], -1, 0x4000, 4)).unwrap(),
            "-0.5000"
        );
        assert_eq!(decode_numeric(&numeric(&[50], -1, 0, 4)).unwrap(), "0.0050");
        assert_eq!(
            decode_numeric(&numeric(&[50], -2, 0, 8)).unwrap(),
            "0.00000050"
        );
        assert_eq!(decode_numeric(&numeric(&[], 0, 0, 0)).unwrap(), "0");
        assert_eq!(decode_numeric(&numeric(&[], 0, 0xC000, 0)).unwrap(), "NaN");
        assert_eq!(decode_numeric(&numeric(&[7], 0, 0, 3)).unwrap(), "7.000");
    }

    #[test]
    fn scalars_decode_like_node_postgres() {
        assert_eq!(
            decode_scalar(&Type::INT4, &7i32.to_be_bytes()).unwrap(),
            json!(7)
        );
        assert_eq!(
            decode_scalar(&Type::INT8, &9_007_199_254_740_993i64.to_be_bytes()).unwrap(),
            json!("9007199254740993")
        );
        assert_eq!(
            decode_scalar(&Type::FLOAT4, &1.1f32.to_be_bytes()).unwrap(),
            json!(1.1)
        );
        assert_eq!(decode_scalar(&Type::BOOL, &[1]).unwrap(), json!(true));
        assert_eq!(
            decode_scalar(&Type::TEXT, "é".as_bytes()).unwrap(),
            json!("é")
        );
        assert_eq!(
            decode_scalar(&Type::TIMESTAMPTZ, &0i64.to_be_bytes()).unwrap(),
            json!("2000-01-01T00:00:00.000Z")
        );
        assert_eq!(
            decode_scalar(&Type::DATE, &1i32.to_be_bytes()).unwrap(),
            json!("2000-01-02T00:00:00.000Z")
        );
        assert_eq!(
            decode_scalar(&Type::TIME, &3_723_000_000i64.to_be_bytes()).unwrap(),
            json!("01:02:03")
        );
        assert_eq!(
            decode_scalar(&Type::BYTEA, &[1, 2]).unwrap(),
            json!({ "type": "Buffer", "data": [1, 2] })
        );
        assert_eq!(
            decode_scalar(
                &Type::UUID,
                &[
                    0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a,
                    0xbc, 0xde, 0xf0
                ]
            )
            .unwrap(),
            json!("12345678-9abc-def0-1234-56789abcdef0")
        );
        assert_eq!(
            decode_scalar(&Type::JSONB, b"\x01{\"a\":[1,2]}").unwrap(),
            json!({ "a": [1, 2] })
        );
        assert!(decode_scalar(&Type::INTERVAL, &[0; 16]).is_err());
    }

    #[test]
    fn arrays_nest_by_dimension_and_keep_nulls() {
        let mut raw = Vec::new();
        raw.extend(1i32.to_be_bytes());
        raw.extend(1i32.to_be_bytes());
        raw.extend(23u32.to_be_bytes());
        raw.extend(3i32.to_be_bytes());
        raw.extend(1i32.to_be_bytes());
        for v in [Some(1i32), None, Some(3)] {
            match v {
                Some(n) => {
                    raw.extend(4i32.to_be_bytes());
                    raw.extend(n.to_be_bytes());
                }
                None => raw.extend((-1i32).to_be_bytes()),
            }
        }
        assert_eq!(
            decode_array(&Type::INT4, &raw).unwrap(),
            json!([1, null, 3])
        );
    }

    #[test]
    fn binds_follow_node_postgres_text_rules() {
        assert_eq!(bind_text(&json!(null)), None);
        assert_eq!(bind_text(&json!(true)).as_deref(), Some("true"));
        assert_eq!(bind_text(&json!(10.5)).as_deref(), Some("10.5"));
        assert_eq!(bind_text(&json!(7)).as_deref(), Some("7"));
        assert_eq!(
            bind_text(&json!(["a\"b", null, 3])).as_deref(),
            Some("{\"a\\\"b\",NULL,\"3\"}")
        );
        assert_eq!(bind_text(&json!({"a": 1})).as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn tls_required_connection_strings_are_refused_loudly() {
        let error = PostgresConnector
            .connect("postgres://u:p@h/db?sslmode=require", 1000.0)
            .err()
            .expect("refused");
        assert!(error.message.contains("TLS"));
    }
}
