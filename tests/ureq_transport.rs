#![cfg(feature = "http")]
//! The native transport against a one-shot local HTTP server.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use orbital_integrations::{HttpRequest, Transport, TransportError, UreqTransport};

fn serve_once(
    status_line: &'static str,
    extra_header: &'static str,
    body: &'static str,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/v1/echo", listener.local_addr().expect("addr"));
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut received = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).expect("read");
            received.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&received).to_string();
            if n == 0 || text.ends_with(r#"{"a":1}"#) {
                break;
            }
        }
        let request = String::from_utf8_lossy(&received).to_string();
        let reply = format!("HTTP/1.1 {status_line}\r\n{extra_header}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        stream.write_all(reply.as_bytes()).expect("write");
        request
    });
    (url, handle)
}

fn request(url: String) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("authorization".into(), "Bearer test-key".into()),
        ],
        body: r#"{"a":1}"#.into(),
        timeout: Some(Duration::from_secs(5)),
    }
}

#[test]
fn sends_method_headers_and_body_and_reads_a_success() {
    let (url, server) = serve_once("200 OK", "", r#"{"ok":true}"#);
    let response = UreqTransport::new().send(&request(url)).expect("response");
    assert_eq!(
        (response.status, response.text().as_ref()),
        (200, r#"{"ok":true}"#)
    );
    let seen = server.join().expect("server").to_lowercase();
    assert!(seen.starts_with("post /v1/echo"), "{seen}");
    assert!(seen.contains("authorization: bearer test-key"));
    assert!(seen.ends_with(r#"{"a":1}"#));
}

#[test]
fn a_non_2xx_status_is_a_response_with_its_headers_and_body() {
    let (url, server) = serve_once(
        "429 Too Many Requests",
        "retry-after: 7\r\n",
        r#"{"error":"slow down"}"#,
    );
    let response = UreqTransport::new().send(&request(url)).expect("response");
    server.join().expect("server");
    assert_eq!(response.status, 429);
    assert_eq!(response.header("Retry-After"), Some("7"));
    assert_eq!(response.text(), r#"{"error":"slow down"}"#);
}

#[test]
fn an_unreachable_host_is_a_transport_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    drop(listener);
    assert!(matches!(
        UreqTransport::new().send(&request(url)),
        Err(TransportError::Failed(_))
    ));
}
