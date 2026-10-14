//! The HTTP seam: providers build a request and read a response; the transport moves the bytes.

use std::borrow::Cow;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// The bytes on the wire; empty for no body.
    pub body: Vec<u8>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The body as UTF-8 text, each invalid sequence becoming U+FFFD.
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// A request whose body is `text`, as its UTF-8 bytes.
    pub fn with_text_body(mut self, text: &str) -> Self {
        self.body = text.as_bytes().to_vec();
        self
    }
}

impl HttpResponse {
    /// The body as fetch's `res.text()` reads it: UTF-8, each invalid sequence becoming U+FFFD.
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn status_text(&self) -> &'static str {
        reason_phrase(self.status)
    }
}

/// The request never produced a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    TimedOut,
    Failed(String),
}

pub trait Transport: Send + Sync {
    /// A non-2xx status is a response, not an error.
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The standard reason phrase (`Response.statusText` in fetch) for a status code.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

#[cfg(feature = "http")]
pub use native::UreqTransport;

#[cfg(feature = "http")]
mod native {
    use super::{HttpRequest, HttpResponse, Transport, TransportError};
    use std::io::Read;

    /// Blocking HTTP(S) over `ureq` + rustls.
    pub struct UreqTransport {
        agent: ureq::Agent,
    }

    impl UreqTransport {
        pub fn new() -> Self {
            Self {
                agent: ureq::AgentBuilder::new().build(),
            }
        }
    }

    impl Default for UreqTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    fn collect_headers(response: &ureq::Response) -> Vec<(String, String)> {
        response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                response
                    .header(&name)
                    .map(|v| (name.clone(), v.to_string()))
            })
            .collect()
    }

    fn read_response(response: ureq::Response) -> Result<HttpResponse, TransportError> {
        let status = response.status();
        let headers = collect_headers(&response);
        let mut body = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|e| TransportError::Failed(e.to_string()))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    impl Transport for UreqTransport {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            let mut call = self.agent.request(&request.method, &request.url);
            if let Some(timeout) = request.timeout {
                call = call.timeout(timeout);
            }
            for (name, value) in &request.headers {
                call = call.set(name, value);
            }
            let outcome = call.send_bytes(&request.body);
            let response = match outcome {
                Ok(response) => response,
                Err(ureq::Error::Status(_, response)) => response,
                Err(ureq::Error::Transport(t)) => {
                    let text = t.to_string();
                    return Err(if text.to_ascii_lowercase().contains("timed out") {
                        TransportError::TimedOut
                    } else {
                        TransportError::Failed(text)
                    });
                }
            };
            read_response(response)
        }
    }
}
