//! The `storage` service: the Rust twin of `StorageIntegration` (`@almadar/integrations` `integrations/storage`).
//! With `STORAGE_ACCESS_KEY_ID` and `STORAGE_SECRET_ACCESS_KEY` it talks to an S3-compatible service through the
//! `Transport` (SigV4-signed, the SDK's retry policy); without them it is the in-memory development backend.
//! Actions: `upload`, `uploadMany`, `download`, `list`, `delete`, `getSignedUrl`.

mod s3;
mod sigv4;
mod xml;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use md5::{Digest as _, Md5};
use regex::Regex;
use serde_json::{json, Map, Value};

use self::s3::{parse_listing, Endpoint, S3};
use self::sigv4::{sha256_hex, Signer};
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::{locale_compare, opt_f64, opt_str, str_param, unknown_action, Rand, SystemRand};
use crate::jsnumber::js_number;
use crate::json::number;
use crate::jswire::{base64_decode_lenient, base64_encode};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::Transport;
use crate::wallclock::{system_clock, Clock};

pub const SERVICE: &str = "storage";

const PRODUCTION_REFUSAL: &str = "Storage credentials missing in production (STORAGE_ACCESS_KEY_ID / STORAGE_SECRET_ACCESS_KEY) \u{2014} refusing the in-memory fallback. See SECRETS.md.";

struct Stored {
    content: String,
    content_type: String,
    size: usize,
    metadata: Value,
    last_modified: i64,
}

enum Backend {
    Memory(Mutex<HashMap<String, Stored>>),
    S3(S3),
}

pub struct StorageProvider {
    backend: Backend,
    default_bucket: String,
    public_url_base: String,
    endpoint: String,
    region: String,
    clock: Clock,
    data_url: Regex,
}

struct FilePayload {
    name: String,
    size: f64,
    kind: String,
    content: Option<String>,
}

struct Upload {
    key: String,
    bytes: Vec<u8>,
    content_type: String,
    public: bool,
}

/// Storage is always configured: the credentials only choose the backend.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    match StorageProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

fn file_payload(value: Option<&Value>) -> Option<FilePayload> {
    let record = value?.as_object()?;
    let name = record.get("name")?.as_str()?.to_string();
    Some(FilePayload {
        name,
        size: record.get("size").and_then(Value::as_f64).unwrap_or(0.0),
        kind: record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string(),
        content: record
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// `decodeURIComponent` to bytes.
fn decode_uri_component(text: &str) -> Result<Vec<u8>, IntegrationError> {
    let malformed = || IntegrationError::plain("URI malformed");
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        if raw[at] != b'%' {
            out.push(raw[at]);
            at += 1;
            continue;
        }
        let hex = raw.get(at + 1..at + 3).ok_or_else(malformed)?;
        let byte = u8::from_str_radix(std::str::from_utf8(hex).map_err(|_| malformed())?, 16)
            .map_err(|_| malformed())?;
        out.push(byte);
        at += 3;
    }
    if raw.contains(&b'%') && std::str::from_utf8(&out).is_err() {
        return Err(malformed());
    }
    Ok(out)
}

/// `Array.prototype.slice(start, end)` over a length, with JS's relative and clamped indices.
fn slice_range(len: usize, start: f64, end: f64) -> (usize, usize) {
    let relative = |index: f64| {
        let index = if index.is_nan() { 0.0 } else { index.trunc() };
        if index < 0.0 {
            (len as f64 + index).max(0.0)
        } else {
            index.min(len as f64)
        }
    };
    let (from, to) = (relative(start) as usize, relative(end) as usize);
    (from, to.max(from))
}

/// `Number.parseInt(text, 10)`; `None` stands for `NaN`.
fn parse_int(text: &str) -> Option<f64> {
    let trimmed = text.trim_start();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let run: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if run.is_empty() {
        None
    } else {
        run.parse::<f64>().ok().map(|n| sign * n)
    }
}

impl StorageProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let get = |name: &str| creds.get(name).unwrap_or("").to_string();
        let region = creds
            .get("STORAGE_REGION")
            .unwrap_or("us-east-1")
            .to_string();
        let (access, secret) = (
            get("STORAGE_ACCESS_KEY_ID"),
            get("STORAGE_SECRET_ACCESS_KEY"),
        );
        let endpoint = get("STORAGE_ENDPOINT");
        let backend = if !access.is_empty() && !secret.is_empty() {
            let endpoint = if endpoint.is_empty() {
                None
            } else {
                Some(Endpoint::parse(&endpoint)?)
            };
            Backend::S3(S3::new(
                Signer {
                    access_key_id: access,
                    secret_access_key: secret,
                    region: region.clone(),
                },
                endpoint,
                transport,
                Arc::new(std::thread::sleep),
                Arc::new(SystemRand::new()),
            ))
        } else {
            if creds.get("NODE_ENV") == Some("production") {
                return Err(IntegrationError::plain(PRODUCTION_REFUSAL));
            }
            Backend::Memory(Mutex::new(HashMap::new()))
        };
        Ok(Self {
            backend,
            default_bucket: get("STORAGE_BUCKET"),
            public_url_base: get("STORAGE_PUBLIC_URL_BASE"),
            endpoint,
            region,
            clock: system_clock(),
            data_url: Regex::new(r"(?s)^data:([^;,]+)?(;base64)?,(.*)$").expect("data url pattern"),
        })
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Replace the wait between retries and the jitter source (tests; hosts with their own clock). In-memory
    /// storage has neither.
    pub fn with_retry(mut self, sleeper: s3::Sleeper, rand: Arc<dyn Rand>) -> Self {
        if let Backend::S3(client) = &mut self.backend {
            client.set_retry(sleeper, rand);
        }
        self
    }

    fn bucket_of(&self, params: &Map<String, Value>) -> String {
        match opt_str(params, "bucket") {
            Some(bucket) if !bucket.is_empty() => bucket.to_string(),
            _ => self.default_bucket.clone(),
        }
    }

    fn decode_content(&self, content: &str) -> Result<(Vec<u8>, Option<String>), IntegrationError> {
        let Some(found) = self.data_url.captures(content) else {
            return Ok((content.as_bytes().to_vec(), None));
        };
        let mime = found
            .get(1)
            .map(|m| m.as_str().to_string())
            .filter(|m| !m.is_empty());
        let body = found.get(3).map_or("", |m| m.as_str());
        let bytes = if found.get(2).is_some() {
            base64_decode_lenient(body)
        } else {
            decode_uri_component(body)?
        };
        Ok((bytes, mime))
    }

    fn resolve_upload(&self, params: &Map<String, Value>) -> Result<Upload, IntegrationError> {
        let public = opt_str(params, "acl") == Some("public");
        if let Some(file) = file_payload(params.get("file")) {
            let max_size = opt_f64(params, "maxSize").unwrap_or(0.0);
            if max_size > 0.0 && file.size > max_size {
                return Err(IntegrationError::plain(format!(
                    "Upload rejected: {} is {} bytes (max {})",
                    file.name,
                    js_number(file.size),
                    js_number(max_size)
                )));
            }
            let content = file.content.as_deref().filter(|c| !c.is_empty()).ok_or_else(|| {
                IntegrationError::plain(format!(
                    "Upload rejected: file payload for '{}' carries no content \u{2014} the uploader must include the base64 data URL",
                    file.name
                ))
            })?;
            let (bytes, content_type) = self.decode_content(content)?;
            let mut safe = String::new();
            for c in file.name.chars() {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    safe.push(c);
                } else {
                    safe.extend(std::iter::repeat_n('_', c.len_utf16()));
                }
            }
            return Ok(Upload {
                key: format!("{}-{safe}", (self.clock)()),
                bytes,
                content_type: content_type.unwrap_or(file.kind),
                public,
            });
        }
        let key = opt_str(params, "key").filter(|k| !k.is_empty());
        let content = params.get("content").filter(|c| !c.is_null());
        let (Some(key), Some(content)) = (key, content) else {
            return Err(IntegrationError::plain(
                "upload requires either `file` (with content) or the `key` + `content` pair",
            ));
        };
        let raw = match content {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let (bytes, mime) = self.decode_content(&raw)?;
        let content_type = opt_str(params, "contentType")
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .or(mime)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(Upload {
            key: key.to_string(),
            bytes,
            content_type,
            public,
        })
    }

    fn public_url(&self, bucket: &str, key: &str) -> String {
        if !self.public_url_base.is_empty() {
            return format!(
                "{}/{key}",
                self.public_url_base
                    .strip_suffix('/')
                    .unwrap_or(&self.public_url_base)
            );
        }
        if !self.endpoint.is_empty() {
            return format!(
                "{}/{bucket}/{key}",
                self.endpoint.strip_suffix('/').unwrap_or(&self.endpoint)
            );
        }
        format!("https://{bucket}.s3.{}.amazonaws.com/{key}", self.region)
    }

    fn upload(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let bucket = self.bucket_of(params);
        let Upload {
            key,
            bytes,
            content_type,
            public,
        } = self.resolve_upload(params)?;
        let etag = format!("\"{}\"", hex(&Md5::digest(&bytes)));
        let now = (self.clock)();
        let url = match &self.backend {
            Backend::S3(client) => {
                let mut headers = vec![("content-type".to_string(), content_type.clone())];
                if public {
                    headers.push(("x-amz-acl".to_string(), "public-read".to_string()));
                }
                client.send(
                    "PUT",
                    &bucket,
                    Some(&key),
                    &[("x-id".to_string(), "PutObject".to_string())],
                    &headers,
                    &bytes,
                    now,
                )?;
                if public {
                    self.public_url(&bucket, &key)
                } else {
                    client.presign(&bucket, &key, "get", "3600", now)?
                }
            }
            Backend::Memory(objects) => {
                let encoded = base64_encode(&bytes);
                objects.lock().expect("storage state").insert(
                    format!("{bucket}/{key}"),
                    Stored {
                        content: encoded.clone(),
                        content_type: content_type.clone(),
                        size: bytes.len(),
                        metadata: params.get("metadata").cloned().unwrap_or_else(|| json!({})),
                        last_modified: now,
                    },
                );
                format!("data:{content_type};base64,{encoded}")
            }
        };
        Ok(
            json!({ "key": key, "bucket": bucket, "size": bytes.len(), "etag": etag, "id": key, "url": url }),
        )
    }

    fn upload_many(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let files: Vec<FilePayload> = params
            .get("files")
            .and_then(Value::as_array)
            .map(|all| all.iter().filter_map(|f| file_payload(Some(f))).collect())
            .unwrap_or_default();
        if files.is_empty() {
            return Err(IntegrationError::plain(
                "uploadMany requires a non-empty `files` array (UploadDropZone batch shape)",
            ));
        }
        let mut items = Vec::new();
        for file in files {
            let mut one = Map::new();
            if let Some(bucket) = params.get("bucket") {
                one.insert("bucket".into(), bucket.clone());
            }
            let mut payload = Map::new();
            payload.insert("name".into(), json!(file.name));
            payload.insert("size".into(), number(file.size));
            payload.insert("type".into(), json!(file.kind));
            if let Some(content) = &file.content {
                payload.insert("content".into(), json!(content));
            }
            one.insert("file".into(), Value::Object(payload));
            for passthrough in ["acl", "maxSize"] {
                if let Some(value) = params.get(passthrough) {
                    one.insert(passthrough.into(), value.clone());
                }
            }
            let done = self.upload(&one)?;
            items.push(json!({
                "id": done["id"],
                "key": done["key"],
                "url": done["url"],
                "name": file.name,
                "sizeBytes": number(file.size),
                "mimeType": file.kind,
            }));
        }
        let count = items.len();
        Ok(json!({ "items": items, "count": count }))
    }

    fn download(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let bucket = self.bucket_of(params);
        let key = str_param(params, "key")?;
        match &self.backend {
            Backend::S3(client) => {
                let reply = client.send(
                    "GET",
                    &bucket,
                    Some(key),
                    &[("x-id".to_string(), "GetObject".to_string())],
                    &[],
                    &[],
                    (self.clock)(),
                )?;
                let content_type = reply
                    .header("content-type")
                    .map_or_else(|| "application/octet-stream".to_string(), str::to_string);
                Ok(json!({
                    "content": base64_encode(&reply.body),
                    "contentType": content_type,
                    "size": reply.body.len(),
                    "metadata": {},
                }))
            }
            Backend::Memory(objects) => {
                let objects = objects.lock().expect("storage state");
                let found = objects.get(&format!("{bucket}/{key}")).ok_or_else(|| {
                    IntegrationError::plain(format!("Object not found: {bucket}/{key}"))
                })?;
                Ok(json!({
                    "content": found.content,
                    "contentType": found.content_type,
                    "size": found.size,
                    "metadata": found.metadata,
                }))
            }
        }
    }

    fn list(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let bucket = self.bucket_of(params);
        let prefix = opt_str(params, "prefix").unwrap_or("");
        let max_keys = opt_f64(params, "maxKeys").unwrap_or(1000.0);
        let token = opt_str(params, "continuationToken");
        match &self.backend {
            Backend::S3(client) => {
                let mut query = vec![
                    ("list-type".to_string(), "2".to_string()),
                    ("max-keys".to_string(), js_number(max_keys)),
                ];
                if !prefix.is_empty() {
                    query.push(("prefix".to_string(), prefix.to_string()));
                }
                if let Some(token) = token.filter(|t| !t.is_empty()) {
                    query.push(("continuation-token".to_string(), token.to_string()));
                }
                let reply = client.send("GET", &bucket, None, &query, &[], &[], (self.clock)())?;
                let listing = parse_listing(&reply.text())?;
                let keys: Vec<Value> = listing
                    .keys
                    .iter()
                    .map(|e| json!({ "key": e.key, "size": number(e.size), "lastModified": e.last_modified }))
                    .collect();
                let mut out = Map::new();
                out.insert("keys".into(), Value::Array(keys));
                out.insert("truncated".into(), json!(listing.truncated));
                if let (true, Some(next)) = (listing.truncated, listing.next_token) {
                    out.insert("nextToken".into(), json!(next));
                }
                Ok(Value::Object(out))
            }
            Backend::Memory(objects) => {
                let objects = objects.lock().expect("storage state");
                let full_prefix = format!("{bucket}/{prefix}");
                let mut results: Vec<(String, &Stored)> = objects
                    .iter()
                    .filter(|(composite, _)| composite.starts_with(&full_prefix))
                    .map(|(composite, stored)| (composite[bucket.len() + 1..].to_string(), stored))
                    .collect();
                results.sort_by(|a, b| locale_compare(&a.0, &b.0));
                let offset = token
                    .and_then(parse_int)
                    .filter(|n| *n != 0.0)
                    .unwrap_or(0.0);
                let (from, to) = slice_range(results.len(), offset, offset + max_keys);
                let keys: Vec<Value> = results[from..to]
                    .iter()
                    .map(|(key, stored)| json!({ "key": key, "size": stored.size, "lastModified": stored.last_modified }))
                    .collect();
                let truncated = offset + max_keys < results.len() as f64;
                let mut out = Map::new();
                out.insert("keys".into(), Value::Array(keys));
                out.insert("truncated".into(), json!(truncated));
                if truncated {
                    out.insert("nextToken".into(), json!(js_number(offset + max_keys)));
                }
                Ok(Value::Object(out))
            }
        }
    }

    fn delete(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let bucket = self.bucket_of(params);
        let key = str_param(params, "key")?;
        match &self.backend {
            Backend::S3(client) => {
                client.send(
                    "DELETE",
                    &bucket,
                    Some(key),
                    &[("x-id".to_string(), "DeleteObject".to_string())],
                    &[],
                    &[],
                    (self.clock)(),
                )?;
                Ok(json!({ "deleted": true }))
            }
            Backend::Memory(objects) => {
                let existed = objects
                    .lock()
                    .expect("storage state")
                    .remove(&format!("{bucket}/{key}"))
                    .is_some();
                Ok(json!({ "deleted": existed }))
            }
        }
    }

    fn get_signed_url(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let bucket = self.bucket_of(params);
        let key = str_param(params, "key")?;
        let expires_in = opt_f64(params, "expiresIn").unwrap_or(3600.0);
        let operation = opt_str(params, "operation").unwrap_or("get");
        let now = (self.clock)();
        let expires_at = now as f64 + expires_in * 1000.0;
        let url = match &self.backend {
            Backend::S3(client) => {
                client.presign(&bucket, key, operation, &js_number(expires_in), now)?
            }
            Backend::Memory(_) => {
                let token =
                    sha256_hex(format!("{bucket}/{key}/{}", js_number(expires_at)).as_bytes());
                format!(
                    "https://storage.mock.local/{bucket}/{key}?X-Amz-Algorithm=MOCK-HMAC-SHA256&X-Amz-Expires={}&X-Amz-SignedHeaders=host&X-Amz-Signature={}&operation={operation}",
                    js_number(expires_in),
                    &token[..16]
                )
            }
        };
        Ok(json!({ "url": url, "expiresAt": number(expires_at) }))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Provider for StorageProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "upload" => self.upload(params),
            "uploadMany" => self.upload_many(params),
            "download" => self.download(params),
            "list" => self.list(params),
            "delete" => self.delete(params),
            "getSignedUrl" => self.get_signed_url(params),
            other => Err(unknown_action("storage", other)),
        }
    }
}
