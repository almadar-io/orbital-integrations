//! The `drive` service: the Rust twin of `DriveIntegration` (Google Drive). Reads (`listFiles`, `getFile`,
//! `shareFile`) use the service-account client when configured, else the user's; writes (`uploadFile`,
//! `createFolder`) prefer the user's refresh-token client, because Google gives service accounts no storage
//! quota on personal accounts. File content crosses this boundary as base64 (a data URL is accepted on upload).

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::formwire::{encode_component, encode_path_segment, number_text, Pairs};
use crate::google::{Body, GoogleClient, JwtSource, RefreshSource, Sleeper};
use crate::jswire::{base64_decode_lenient, base64_encode, js_number, system_clock, Clock};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::Transport;

pub const SERVICE: &str = "drive";
const ROOT: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_ROOT: &str = "https://www.googleapis.com/upload/drive/v3";
const SCOPE: &str = "https://www.googleapis.com/auth/drive";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const NOT_CONFIGURED: &str = "Drive not configured — set GOOGLE_DRIVE_SA_KEY (reads) and/or GOOGLE_DRIVE_REFRESH_TOKEN + OAUTH_CLIENT_ID/SECRET (writes)";

pub struct DriveProvider {
    service_account: Option<GoogleClient>,
    user: Option<GoogleClient>,
    default_folder_id: Option<String>,
}

/// The credentials that configure `drive`: one member of the declared `auth` group is enough, and a
/// constructor error names what is missing.
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get("GOOGLE_DRIVE_SA_KEY").is_some() || creds.get("GOOGLE_DRIVE_REFRESH_TOKEN").is_some()
}

pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    if !is_configured(creds) {
        integrations.register_unconfigured(SERVICE);
        return;
    }
    match DriveProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl DriveProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        Self::with_runtime(
            creds,
            transport,
            system_clock(),
            Arc::new(std::thread::sleep),
        )
    }

    /// Build with an injected clock (token timestamps and expiry) and sleeper (retry waits).
    pub fn with_runtime(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
        clock: Clock,
        sleeper: Sleeper,
    ) -> Result<Self, IntegrationError> {
        let service_account = match creds.get("GOOGLE_DRIVE_SA_KEY") {
            Some(raw) => {
                let source = JwtSource::from_key(
                    raw,
                    SCOPE,
                    creds.get("GOOGLE_DRIVE_SUBJECT"),
                    clock.clone(),
                )?;
                Some(GoogleClient::new(
                    Arc::new(source),
                    transport.clone(),
                    sleeper.clone(),
                ))
            }
            None => None,
        };
        let user = match (
            creds.get("GOOGLE_DRIVE_REFRESH_TOKEN"),
            creds.get("OAUTH_CLIENT_ID"),
            creds.get("OAUTH_CLIENT_SECRET"),
        ) {
            (Some(refresh), Some(id), Some(secret)) => {
                let source = RefreshSource::new(id, secret, refresh, clock);
                Some(GoogleClient::new(Arc::new(source), transport, sleeper))
            }
            _ => None,
        };
        if service_account.is_none() && user.is_none() {
            return Err(IntegrationError::plain(NOT_CONFIGURED));
        }
        Ok(Self {
            service_account,
            user,
            default_folder_id: creds.get("GOOGLE_DRIVE_FOLDER_ID").map(str::to_string),
        })
    }

    fn read_client(&self) -> Result<&GoogleClient, IntegrationError> {
        self.service_account
            .as_ref()
            .or(self.user.as_ref())
            .ok_or_else(|| IntegrationError::plain("Drive not configured"))
    }

    fn write_client(&self) -> Result<&GoogleClient, IntegrationError> {
        self.user
            .as_ref()
            .or(self.service_account.as_ref())
            .ok_or_else(|| IntegrationError::plain("Drive not configured"))
    }

    fn list_files(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut clauses = vec!["trashed = false".to_string()];
        if let Some(folder) = text(params, "folderId").filter(|f| !f.is_empty()) {
            clauses.push(format!("'{}' in parents", folder.replace('\'', "\\'")));
        }
        if let Some(query) = text(params, "query").filter(|q| !q.is_empty()) {
            clauses.push(query.to_string());
        }
        let page_size = match js_number(params.get("maxResults")) {
            n if n == 0.0 || n.is_nan() => 100.0,
            n => n,
        };
        let mut query = Pairs::default();
        query.push("q", &clauses.join(" and "));
        query.push("pageSize", &number_text(page_size));
        query.push(
            "fields",
            "files(id, name, mimeType, size, modifiedTime, webViewLink)",
        );
        let reply = self.read_client()?.request(
            "GET",
            &format!("{ROOT}/files?{}", query.encode()),
            Body::None,
        )?;
        let files: Vec<Value> = reply
            .data
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .map(|file| {
                        json!({
                            "id": or_empty(file, "id"),
                            "name": or_empty(file, "name"),
                            "mimeType": or_empty(file, "mimeType"),
                            "size": crate::json::number(js_number(file.get("size"))),
                            "modifiedTime": or_empty(file, "modifiedTime"),
                            "webViewLink": or_empty(file, "webViewLink"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "files": files }))
    }

    fn get_file(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let file_id = text(params, "fileId").unwrap_or_default();
        let id = encode_path_segment(file_id);
        let client = self.read_client()?;
        let mut meta_query = Pairs::default();
        meta_query.push("fields", "id, name, mimeType, size");
        let meta = client.request(
            "GET",
            &format!("{ROOT}/files/{id}?{}", meta_query.encode()),
            Body::None,
        )?;
        let content = self.read_client()?.request(
            "GET",
            &format!("{ROOT}/files/{id}?alt=media"),
            Body::None,
        )?;
        let bytes = content.body.as_slice();
        Ok(json!({
            "id": meta.data.get("id").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(file_id)),
            "name": or_empty(&meta.data, "name"),
            "mimeType": meta.data.get("mimeType").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("application/octet-stream")),
            "content": base64_encode(bytes),
            "size": bytes.len(),
        }))
    }

    fn upload_file(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let name = text(params, "name").unwrap_or_default();
        let (bytes, content_type) = decode_content(text(params, "content").unwrap_or_default())?;
        let parent = text(params, "folderId")
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .or_else(|| self.default_folder_id.clone());
        let mut metadata = Map::new();
        metadata.insert("name".into(), json!(name));
        if let Some(parent) = parent {
            metadata.insert("parents".into(), json!([parent]));
        }
        let mime = text(params, "mimeType")
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .or(content_type)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let boundary = boundary();
        let mut body = format!(
            "--{boundary}\r\ncontent-type: application/json\r\n\r\n{}\r\n--{boundary}\r\ncontent-type: {mime}\r\n\r\n",
            Value::Object(metadata),
        )
        .into_bytes();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
        let mut query = Pairs::default();
        query.push("fields", "id, name, webViewLink");
        let url = format!(
            "{UPLOAD_ROOT}/files?{}&uploadType=multipart",
            query.encode()
        );
        let reply = self.write_client()?.request(
            "POST",
            &url,
            Body::Raw {
                content_type: format!("multipart/related; boundary={boundary}"),
                body,
            },
        )?;
        Ok(json!({
            "id": or_empty(&reply.data, "id"),
            "name": reply.data.get("name").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(name)),
            "webViewLink": or_empty(&reply.data, "webViewLink"),
        }))
    }

    fn create_folder(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let name = text(params, "name").unwrap_or_default();
        let parent = text(params, "parentId")
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .or_else(|| self.default_folder_id.clone());
        let mut body = Map::new();
        body.insert("name".into(), json!(name));
        body.insert("mimeType".into(), json!(FOLDER_MIME));
        if let Some(parent) = parent {
            body.insert("parents".into(), json!([parent]));
        }
        let mut query = Pairs::default();
        query.push("fields", "id, name");
        let reply = self.write_client()?.request(
            "POST",
            &format!("{ROOT}/files?{}", query.encode()),
            Body::Json(Value::Object(body)),
        )?;
        Ok(json!({
            "id": or_empty(&reply.data, "id"),
            "name": reply.data.get("name").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(name)),
        }))
    }

    fn share_file(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let file_id = encode_path_segment(text(params, "fileId").unwrap_or_default());
        let role = text(params, "role")
            .filter(|r| !r.is_empty())
            .unwrap_or("reader");
        let body = json!({
            "type": "user",
            "role": role,
            "emailAddress": params.get("email").cloned().unwrap_or(Value::Null),
        });
        let url = format!(
            "{ROOT}/files/{file_id}/permissions?fields={}",
            encode_component("id")
        );
        let reply = self
            .read_client()?
            .request("POST", &url, Body::Json(body))?;
        Ok(json!({ "shared": true, "permissionId": or_empty(&reply.data, "id") }))
    }
}

fn text<'a>(params: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str)
}

/// `value[field] ?? ''`.
fn or_empty(value: &Value, field: &str) -> Value {
    value
        .get(field)
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!(""))
}

fn boundary() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes = [0x2f; 16];
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// A data URL (`data:[mime][;base64],body`) decodes to its bytes and mime type; any other text is the
/// content itself. A percent-encoded (non-base64) body that does not decode is `URI malformed`, as in JS.
fn decode_content(content: &str) -> Result<(Vec<u8>, Option<String>), IntegrationError> {
    let plain = || Ok((content.as_bytes().to_vec(), None));
    let Some(rest) = content.strip_prefix("data:") else {
        return plain();
    };
    let mime_end = rest.find([';', ',']).unwrap_or(rest.len());
    let (mime, mut rest) = rest.split_at(mime_end);
    let is_base64 = match rest.strip_prefix(";base64") {
        Some(after) => {
            rest = after;
            true
        }
        None => false,
    };
    let Some(body) = rest.strip_prefix(',') else {
        return plain();
    };
    let mime = (!mime.is_empty()).then(|| mime.to_string());
    if is_base64 {
        return Ok((base64_decode_lenient(body), mime));
    }
    Ok((percent_decode(body)?, mime))
}

fn percent_decode(text: &str) -> Result<Vec<u8>, IntegrationError> {
    let malformed = || IntegrationError::plain("URI malformed");
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3).ok_or_else(malformed)?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| malformed())?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map(String::into_bytes)
        .map_err(|_| malformed())
}

impl Provider for DriveProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "listFiles" => self.list_files(params),
            "getFile" => self.get_file(params),
            "uploadFile" => self.upload_file(params),
            "createFolder" => self.create_folder(params),
            "shareFile" => self.share_file(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_decode_to_bytes_and_mime() {
        assert_eq!(
            decode_content("data:text/plain;base64,aGk=").unwrap(),
            (b"hi".to_vec(), Some("text/plain".into()))
        );
        assert_eq!(
            decode_content("data:,a%20b").unwrap(),
            (b"a b".to_vec(), None)
        );
        assert_eq!(
            decode_content("plain text").unwrap(),
            (b"plain text".to_vec(), None)
        );
    }

    #[test]
    fn a_data_url_with_other_parameters_is_plain_content() {
        let text = "data:text/plain;charset=utf-8,hi";
        assert_eq!(
            decode_content(text).unwrap(),
            (text.as_bytes().to_vec(), None)
        );
    }

    #[test]
    fn a_broken_percent_escape_is_a_malformed_uri() {
        assert_eq!(
            decode_content("data:text/plain,100%").unwrap_err().message,
            "URI malformed"
        );
        assert_eq!(
            decode_content("data:text/plain,%zz").unwrap_err().message,
            "URI malformed"
        );
        assert_eq!(
            decode_content("data:text/plain,%C3").unwrap_err().message,
            "URI malformed"
        );
        assert_eq!(
            decode_content("data:text/plain,%C3%A9").unwrap().0,
            "é".as_bytes()
        );
    }

    #[test]
    fn boundaries_are_uuid_shaped_and_fresh() {
        let (a, b) = (boundary(), boundary());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }
}
