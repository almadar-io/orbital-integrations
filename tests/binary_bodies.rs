//! Binary bodies cross the `Transport` seam as bytes: an S3 object, a Drive file and a DocuSign document that are
//! not UTF-8 reach the caller exactly as the TS providers return them (base64 of the received bytes), and an
//! uploaded binary object is sent unchanged.

use orbital_integrations::CallContext;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use orbital_integrations::{
    drive, esign, storage, Credentials, HttpRequest, HttpResponse, Integrations, Transport,
    TransportError,
};
use serde_json::{json, Map, Value};

const BINARY: [u8; 12] = [
    0x25, 0x50, 0x44, 0x46, 0x00, 0xff, 0xfe, 0x80, 0xc3, 0x28, 0x0d, 0x0a,
];

struct Routed {
    sent: Mutex<Vec<HttpRequest>>,
    reply: Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>,
}

impl Routed {
    fn new(reply: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            reply: Box::new(reply),
        })
    }
}

impl Transport for Routed {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.sent.lock().expect("sent").push(request.clone());
        Ok((self.reply)(request))
    }
}

fn response(status: u16, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body,
    }
}

fn creds(pairs: &[(&str, &str)]) -> Credentials {
    Credentials::from_env(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

fn params(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("params must be an object, got {other}"),
    }
}

fn s3_creds() -> Credentials {
    creds(&[
        ("STORAGE_ACCESS_KEY_ID", "test-access-key-id"),
        ("STORAGE_SECRET_ACCESS_KEY", "test-key"),
        ("STORAGE_BUCKET", "media"),
        ("STORAGE_ENDPOINT", "http://s3.test:9000"),
    ])
}

#[test]
fn a_binary_s3_object_round_trips_through_storage_byte_for_byte() {
    let creds = s3_creds();
    let stored: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let backend = stored.clone();
    let transport = Routed::new(move |request| {
        let mut object = backend.lock().expect("object");
        if request.method == "PUT" {
            *object = request.body.clone();
            HttpResponse {
                status: 200,
                headers: vec![("etag".into(), "\"1\"".into())],
                body: vec![],
            }
        } else {
            response(200, object.clone())
        }
    });
    let mut integrations = Integrations::new(&creds).expect("mode");
    storage::register(&mut integrations, &creds, transport.clone());

    let content = format!(
        "data:application/octet-stream;base64,{}",
        STANDARD.encode(BINARY)
    );
    let uploaded = integrations
        .call_json(
            "storage",
            "upload",
            &params(json!({ "bucket": "media", "key": "bin/a.pdf", "content": content })),
            &CallContext::anonymous(),
        )
        .expect("upload");
    assert_eq!(uploaded["size"], json!(BINARY.len()));
    assert_eq!(*stored.lock().expect("object"), BINARY.to_vec(), "PUT body");

    let downloaded = integrations
        .call_json(
            "storage",
            "download",
            &params(json!({ "bucket": "media", "key": "bin/a.pdf" })),
            &CallContext::anonymous(),
        )
        .expect("download");
    assert_eq!(downloaded["content"], json!(STANDARD.encode(BINARY)));
    assert_eq!(downloaded["size"], json!(BINARY.len()));
}

#[test]
fn control_a_text_s3_object_still_round_trips() {
    let creds = s3_creds();
    let transport = Routed::new(|request| {
        if request.method == "PUT" {
            response(200, vec![])
        } else {
            response(200, b"hello".to_vec())
        }
    });
    let mut integrations = Integrations::new(&creds).expect("mode");
    storage::register(&mut integrations, &creds, transport.clone());
    integrations
        .call_json(
            "storage",
            "upload",
            &params(json!({ "bucket": "media", "key": "t.txt", "content": "hello" })),
            &CallContext::anonymous(),
        )
        .expect("upload");
    assert_eq!(transport.sent.lock().expect("sent")[0].body, b"hello");
    let downloaded = integrations
        .call_json(
            "storage",
            "download",
            &params(json!({ "bucket": "media", "key": "t.txt" })),
            &CallContext::anonymous(),
        )
        .expect("download");
    assert_eq!(downloaded["content"], json!("aGVsbG8="));
    assert_eq!(downloaded["size"], json!(5));
}

#[test]
fn a_binary_drive_file_reaches_the_result_as_base64_of_its_bytes() {
    let creds = creds(&[
        ("GOOGLE_DRIVE_REFRESH_TOKEN", "test-refresh-token"),
        ("OAUTH_CLIENT_ID", "test-client-id"),
        ("OAUTH_CLIENT_SECRET", "test-client-secret"),
    ]);
    let transport = Routed::new(|request| {
        if request.url.contains("oauth2.googleapis.com") {
            response(
                200,
                br#"{"access_token":"ya29.t","expires_in":3599,"token_type":"Bearer"}"#.to_vec(),
            )
        } else if request.url.ends_with("alt=media") {
            response(200, BINARY.to_vec())
        } else {
            response(
                200,
                br#"{"id":"f1","name":"n.pdf","mimeType":"application/pdf"}"#.to_vec(),
            )
        }
    });
    let mut integrations = Integrations::new(&creds).expect("mode");
    drive::register(&mut integrations, &creds, transport);
    let file = integrations
        .call_json(
            "drive",
            "getFile",
            &params(json!({ "fileId": "f1" })),
            &CallContext::anonymous(),
        )
        .expect("getFile");
    assert_eq!(file["content"], json!(STANDARD.encode(BINARY)));
    assert_eq!(file["size"], json!(BINARY.len()));
}

#[test]
fn a_binary_drive_upload_is_sent_unchanged_inside_the_multipart_body() {
    let creds = creds(&[
        ("GOOGLE_DRIVE_REFRESH_TOKEN", "test-refresh-token"),
        ("OAUTH_CLIENT_ID", "test-client-id"),
        ("OAUTH_CLIENT_SECRET", "test-client-secret"),
    ]);
    let transport = Routed::new(|request| {
        if request.url.contains("oauth2.googleapis.com") {
            response(
                200,
                br#"{"access_token":"ya29.t","expires_in":3599,"token_type":"Bearer"}"#.to_vec(),
            )
        } else {
            response(200, br#"{"id":"f1","name":"b.bin"}"#.to_vec())
        }
    });
    let mut integrations = Integrations::new(&creds).expect("mode");
    drive::register(&mut integrations, &creds, transport.clone());
    integrations
        .call_json(
            "drive",
            "uploadFile",
            &params(json!({
                "name": "b.bin",
                "content": format!("data:application/octet-stream;base64,{}", STANDARD.encode(BINARY)),
            })), &CallContext::anonymous())
        .expect("uploadFile");
    let sent = transport.sent.lock().expect("sent");
    let upload = sent.last().expect("upload request");
    assert!(
        upload.body.windows(BINARY.len()).any(|w| w == BINARY),
        "the binary content must appear verbatim in the multipart body"
    );
}

#[test]
fn a_binary_docusign_document_reaches_the_result_as_base64_of_its_bytes() {
    let creds = creds(&[
        ("DOCUSIGN_BASE_URL", "http://docusign.test/restapi"),
        ("DOCUSIGN_ACCESS_TOKEN", "test-token"),
    ]);
    let transport = Routed::new(|_| response(200, BINARY.to_vec()));
    let mut integrations = Integrations::new(&creds).expect("mode");
    esign::register(&mut integrations, &creds, transport);
    let document = integrations
        .call_json(
            "esign",
            "downloadDocument",
            &params(json!({ "envelopeId": "env-1" })),
            &CallContext::anonymous(),
        )
        .expect("downloadDocument");
    assert_eq!(document["content"], json!(STANDARD.encode(BINARY)));
    assert_eq!(document["documentName"], json!("envelope-env-1.pdf"));
}
