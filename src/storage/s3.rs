//! The S3 backend over the `Transport` seam: request addressing, SigV4 signing, the SDK's retry policy, and the
//! error and listing replies. The twin of the `S3Client` calls `StorageIntegration` makes.

use std::sync::Arc;
use std::time::Duration;

use url::Url;

use super::sigv4::{rfc3986, Signer, Target, UNSIGNED_PAYLOAD};
use super::xml;
use crate::error::IntegrationError;
use crate::infra::{parse_iso_ms, Rand};
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};

/// Total attempts the SDK makes (`maxAttempts` 3, standard retry mode).
const MAX_ATTEMPTS: u32 = 3;
const MAX_BACKOFF_MS: f64 = 20_000.0;
const BASE_BACKOFF_MS: f64 = 100.0;
const THROTTLE_BACKOFF_MS: f64 = 500.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const THROTTLING_CODES: [&str; 14] = [
    "BandwidthLimitExceeded",
    "EC2ThrottledException",
    "LimitExceededException",
    "PriorRequestNotComplete",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
    "RequestThrottled",
    "RequestThrottledException",
    "SlowDown",
    "ThrottledException",
    "Throttling",
    "ThrottlingException",
    "TooManyRequestsException",
    "TransactionInProgressException",
];
const TRANSIENT_CODES: [&str; 3] = ["TimeoutError", "RequestTimeout", "RequestTimeoutException"];
const TRANSIENT_STATUSES: [u16; 4] = [500, 502, 503, 504];

pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

/// Where requests go when `STORAGE_ENDPOINT` names an S3-compatible service.
#[derive(Clone)]
pub struct Endpoint {
    origin: String,
    host: String,
    base_path: String,
}

impl Endpoint {
    pub fn parse(endpoint: &str) -> Result<Self, IntegrationError> {
        let url = Url::parse(endpoint)
            .map_err(|_| IntegrationError::plain(format!("Invalid URL: {endpoint}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| IntegrationError::plain(format!("Invalid URL: {endpoint}")))?;
        let host_header = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        Ok(Self {
            origin: format!("{}://{host_header}", url.scheme()),
            host: host_header,
            base_path: url.path().trim_end_matches('/').to_string(),
        })
    }
}

struct Location {
    origin: String,
    host: String,
    path: String,
}

pub struct S3 {
    signer: Signer,
    endpoint: Option<Endpoint>,
    transport: Arc<dyn Transport>,
    sleeper: Sleeper,
    rand: Arc<dyn Rand>,
}

fn dns_compatible(bucket: &str) -> bool {
    let bytes = bucket.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

fn label_error(label: &str) -> IntegrationError {
    IntegrationError::plain(format!("No value provided for input HTTP label: {label}."))
}

/// What the SDK deserializes from a failed reply: the error's message.
fn failure_message(response: &HttpResponse) -> (Option<String>, String) {
    let text = response.text();
    let body = text.trim();
    if body.is_empty() {
        return (None, "UnknownError".into());
    }
    match xml::parse(body) {
        None => (
            None,
            "@aws-sdk XML parse error: unexpected content.\n  Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object.".into(),
        ),
        Some(root) => {
            let (code, message) = if root.name == "Error" {
                (
                    root.text_of("Code").map(str::to_string),
                    root.text_of("Message").or_else(|| root.text_of("message")),
                )
            } else {
                (None, None)
            };
            (code, message.unwrap_or("UnknownError").to_string())
        }
    }
}

fn retryable(response: &HttpResponse, code: Option<&str>) -> (bool, bool) {
    let throttled = response.status == 429 || code.is_some_and(|c| THROTTLING_CODES.contains(&c));
    let transient = TRANSIENT_STATUSES.contains(&response.status)
        || code.is_some_and(|c| TRANSIENT_CODES.contains(&c));
    (throttled || transient, throttled)
}

impl S3 {
    pub fn new(
        signer: Signer,
        endpoint: Option<Endpoint>,
        transport: Arc<dyn Transport>,
        sleeper: Sleeper,
        rand: Arc<dyn Rand>,
    ) -> Self {
        Self {
            signer,
            endpoint,
            transport,
            sleeper,
            rand,
        }
    }

    pub fn set_retry(&mut self, sleeper: Sleeper, rand: Arc<dyn Rand>) {
        self.sleeper = sleeper;
        self.rand = rand;
    }

    fn location(&self, bucket: &str, key: Option<&str>) -> Result<Location, IntegrationError> {
        if bucket.is_empty() {
            return Err(label_error("Bucket"));
        }
        let key_path = match key {
            Some(key) => key
                .split('/')
                .map(|segment| rfc3986(segment, false))
                .collect::<Vec<_>>()
                .join("/"),
            None => String::new(),
        };
        let tail = if key.is_some() {
            format!("/{key_path}")
        } else {
            "/".to_string()
        };
        match &self.endpoint {
            Some(endpoint) => Ok(Location {
                origin: endpoint.origin.clone(),
                host: endpoint.host.clone(),
                path: format!(
                    "{}/{}{}",
                    endpoint.base_path,
                    rfc3986(bucket, false),
                    if key.is_some() { tail } else { "/".into() }
                ),
            }),
            None if dns_compatible(bucket) => {
                let host = format!("{bucket}.s3.{}.amazonaws.com", self.signer.region);
                Ok(Location {
                    origin: format!("https://{host}"),
                    host,
                    path: tail,
                })
            }
            None => {
                let host = format!("s3.{}.amazonaws.com", self.signer.region);
                Ok(Location {
                    origin: format!("https://{host}"),
                    host,
                    path: format!(
                        "/{}{}",
                        rfc3986(bucket, false),
                        if key.is_some() { tail } else { "/".into() }
                    ),
                })
            }
        }
    }

    /// A presigned URL for `operation` (`get` or `put`) on an object, signed for `now_ms`.
    pub fn presign(
        &self,
        bucket: &str,
        key: &str,
        operation: &str,
        expires_in: &str,
        now_ms: i64,
    ) -> Result<String, IntegrationError> {
        let at = self.location(bucket, Some(key))?;
        let extra: Vec<(String, String)> = if operation == "put" {
            vec![
                ("x-amz-checksum-crc32".into(), "AAAAAA==".into()),
                ("x-amz-sdk-checksum-algorithm".into(), "CRC32".into()),
                ("x-id".into(), "PutObject".into()),
            ]
        } else {
            vec![
                ("x-amz-checksum-mode".into(), "ENABLED".into()),
                ("x-id".into(), "GetObject".into()),
            ]
        };
        let method = if operation == "put" { "PUT" } else { "GET" };
        let query = self.signer.presign_query(
            &Target {
                method,
                host: &at.host,
                path: &at.path,
                query: &extra,
            },
            expires_in,
            Some(UNSIGNED_PAYLOAD),
            now_ms,
        );
        Ok(format!("{}{}?{query}", at.origin, at.path))
    }

    /// One signed request with the SDK's retry policy. Returns the successful reply.
    #[allow(clippy::too_many_arguments)]
    pub fn send(
        &self,
        method: &str,
        bucket: &str,
        key: Option<&str>,
        query: &[(String, String)],
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<HttpResponse, IntegrationError> {
        let at = self.location(bucket, key)?;
        let mut sorted = query.to_vec();
        sorted.sort();
        let query_string = sorted
            .iter()
            .map(|(k, v)| format!("{}={}", rfc3986(k, false), rfc3986(v, false)))
            .collect::<Vec<_>>()
            .join("&");
        let url = if query_string.is_empty() {
            format!("{}{}", at.origin, at.path)
        } else {
            format!("{}{}?{query_string}", at.origin, at.path)
        };
        let mut last = IntegrationError::plain("UnknownError");
        for attempt in 1..=MAX_ATTEMPTS {
            let mut wire = headers.to_vec();
            wire.extend(self.signer.sign_request(
                &Target {
                    method,
                    host: &at.host,
                    path: &at.path,
                    query: &sorted,
                },
                headers,
                body,
                now_ms,
            ));
            let request = HttpRequest {
                method: method.to_string(),
                url: url.clone(),
                headers: wire,
                body: body.to_vec(),
                timeout: Some(REQUEST_TIMEOUT),
            };
            let (retry, throttled) = match self.transport.send(&request) {
                Ok(response) if response.is_success() => return Ok(response),
                Ok(response) => {
                    let (code, message) = failure_message(&response);
                    last = IntegrationError::plain(message);
                    retryable(&response, code.as_deref())
                }
                Err(TransportError::TimedOut) => {
                    last = IntegrationError::plain("Connection timed out");
                    (true, false)
                }
                Err(TransportError::Failed(detail)) => {
                    last = IntegrationError::plain(detail);
                    (true, false)
                }
            };
            if !retry || attempt == MAX_ATTEMPTS {
                break;
            }
            let base = if throttled {
                THROTTLE_BACKOFF_MS
            } else {
                BASE_BACKOFF_MS
            };
            let delay = (self.rand.unit() * f64::from(1u32 << attempt) * base)
                .min(MAX_BACKOFF_MS)
                .floor();
            (self.sleeper)(Duration::from_millis(delay as u64));
        }
        Err(last)
    }
}

/// One `<Contents>` entry of a `ListObjectsV2` reply.
pub struct Listed {
    pub key: String,
    pub size: f64,
    pub last_modified: i64,
}

pub struct Listing {
    pub keys: Vec<Listed>,
    pub truncated: bool,
    pub next_token: Option<String>,
}

pub fn parse_listing(body: &str) -> Result<Listing, IntegrationError> {
    let root = xml::parse(body).ok_or_else(|| {
        IntegrationError::plain("@aws-sdk XML parse error: unexpected content.\n  Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object.")
    })?;
    let keys = root
        .children_named("Contents")
        .map(|entry| Listed {
            key: entry.text_of("Key").unwrap_or_default().to_string(),
            size: entry
                .text_of("Size")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0.0),
            last_modified: entry
                .text_of("LastModified")
                .and_then(|d| parse_iso_ms(d.trim()))
                .unwrap_or(0),
        })
        .collect();
    Ok(Listing {
        keys,
        truncated: root
            .text_of("IsTruncated")
            .is_some_and(|t| t.trim() == "true"),
        next_token: root.text_of("NextContinuationToken").map(str::to_string),
    })
}
