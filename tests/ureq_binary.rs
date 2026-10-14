#![cfg(feature = "http")]
//! The native transport carries a binary body (web-push ciphertext) byte for byte, with the caller's own
//! `Content-Length`, and a binary response body arrives unchanged, against a one-shot local HTTP server.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use orbital_integrations::{HttpRequest, Transport, UreqTransport};

const PAYLOAD: [u8; 8] = [0x00, 0xff, 0xfe, 0x80, 0x0a, 0x0d, 0x7f, 0xc3];

fn serve_once() -> (String, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/push", listener.local_addr().expect("addr"));
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut received = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).expect("read");
            received.extend_from_slice(&chunk[..n]);
            if n == 0 || received.ends_with(&PAYLOAD) {
                break;
            }
        }
        let reply = "HTTP/1.1 201 Created\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        stream.write_all(reply.as_bytes()).expect("write");
        received
    });
    (url, handle)
}

fn request(url: String) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url,
        headers: vec![
            ("Content-Type".into(), "application/octet-stream".into()),
            ("Content-Length".into(), PAYLOAD.len().to_string()),
            ("TTL".into(), "60".into()),
        ],
        body: PAYLOAD.to_vec(),
        timeout: Some(Duration::from_secs(5)),
    }
}

#[test]
fn a_binary_body_arrives_unchanged_with_a_single_content_length() {
    let (url, server) = serve_once();
    let response = UreqTransport::new().send(&request(url)).expect("response");
    assert_eq!(response.status, 201);
    let seen = server.join().expect("server");
    assert!(seen.ends_with(&PAYLOAD), "body bytes");
    let head = String::from_utf8_lossy(&seen[..seen.len() - PAYLOAD.len()]).to_lowercase();
    assert!(head.starts_with("post /push"), "{head}");
    assert_eq!(head.matches("content-length").count(), 1, "{head}");
    assert!(head.contains("ttl: 60"), "{head}");
}

#[test]
fn a_binary_response_body_arrives_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/object", listener.local_addr().expect("addr"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut chunk = [0u8; 1024];
        let _ = stream.read(&mut chunk).expect("read");
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            PAYLOAD.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(&PAYLOAD).expect("body");
    });
    let mut get = request(url);
    get.method = "GET".into();
    get.body = Vec::new();
    let response = UreqTransport::new().send(&get).expect("response");
    server.join().expect("server");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, PAYLOAD.to_vec());
}
