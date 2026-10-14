//! AWS Signature Version 4 for S3: header-signed requests and query-presigned URLs, the two ways the
//! `@aws-sdk/client-s3` and `@aws-sdk/s3-request-presigner` calls of the TS provider are signed.

use sha2::{Digest, Sha256};

use crate::hmacsha256::hmac_sha256;
use crate::wallclock::iso8601;

pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// RFC 3986 percent-encoding: only `A-Za-z0-9-_.~` stay literal (what the AWS SDKs send), `/` too when asked.
pub fn rfc3986(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && byte == b'/')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` of an instant.
pub fn amz_date(now_ms: i64) -> (String, String) {
    let iso = iso8601(now_ms);
    let date = iso[..10].replace('-', "");
    let time = format!("{}{}{}", &iso[11..13], &iso[14..16], &iso[17..19]);
    (date.clone(), format!("{date}T{time}Z"))
}

/// What a request is addressed to: method, the `host` header value, the encoded path and the (unencoded) query.
pub struct Target<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
}

pub struct Signer {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

/// `name=value` pairs sorted by encoded name then value, as the canonical query string.
fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (rfc3986(k, false), rfc3986(v, false)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

impl Signer {
    fn scope(&self, date: &str) -> String {
        format!("{date}/{}/s3/aws4_request", self.region)
    }

    fn signature(&self, date: &str, string_to_sign: &str) -> String {
        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_access_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()))
    }

    fn string_to_sign(&self, stamp: &str, date: &str, canonical_request: &str) -> String {
        format!(
            "{ALGORITHM}\n{stamp}\n{}\n{}",
            self.scope(date),
            sha256_hex(canonical_request.as_bytes())
        )
    }

    /// The headers that authorize a request: `x-amz-date`, `x-amz-content-sha256` and `authorization`. `headers`
    /// are the other headers to sign (lowercase names); `host` is the value the HTTP client will send.
    pub fn sign_request(
        &self,
        target: &Target,
        headers: &[(String, String)],
        payload: &[u8],
        now_ms: i64,
    ) -> Vec<(String, String)> {
        let Target {
            method,
            host,
            path,
            query,
        } = target;
        let (date, stamp) = amz_date(now_ms);
        let payload_hash = sha256_hex(payload);
        let mut signed: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        signed.push(("host".into(), host.to_string()));
        signed.push(("x-amz-content-sha256".into(), payload_hash.clone()));
        signed.push(("x-amz-date".into(), stamp.clone()));
        signed.sort();
        let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed_names = signed
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_request = format!(
            "{method}\n{path}\n{}\n{canonical_headers}\n{signed_names}\n{payload_hash}",
            canonical_query(query)
        );
        let signature = self.signature(
            &date,
            &self.string_to_sign(&stamp, &date, &canonical_request),
        );
        vec![
            ("x-amz-date".into(), stamp),
            ("x-amz-content-sha256".into(), payload_hash),
            (
                "authorization".into(),
                format!(
                    "{ALGORITHM} Credential={}/{}, SignedHeaders={signed_names}, Signature={signature}",
                    self.access_key_id,
                    self.scope(&date)
                ),
            ),
        ]
    }

    /// The query string of a presigned URL (everything after `?`): `query` plus the `X-Amz-*` parameters and the
    /// signature, sorted. `content_sha256` is added as `X-Amz-Content-Sha256` when given (the SDK presigner does).
    pub fn presign_query(
        &self,
        target: &Target,
        expires_in: &str,
        content_sha256: Option<&str>,
        now_ms: i64,
    ) -> String {
        let Target {
            method,
            host,
            path,
            query,
        } = target;
        let (date, stamp) = amz_date(now_ms);
        let mut pairs: Vec<(String, String)> = query.to_vec();
        pairs.push(("X-Amz-Algorithm".into(), ALGORITHM.into()));
        if let Some(hash) = content_sha256 {
            pairs.push(("X-Amz-Content-Sha256".into(), hash.into()));
        }
        pairs.push((
            "X-Amz-Credential".into(),
            format!("{}/{}", self.access_key_id, self.scope(&date)),
        ));
        pairs.push(("X-Amz-Date".into(), stamp.clone()));
        pairs.push(("X-Amz-Expires".into(), expires_in.into()));
        pairs.push(("X-Amz-SignedHeaders".into(), "host".into()));
        let canonical_request = format!(
            "{method}\n{path}\n{}\nhost:{host}\n\nhost\n{}",
            canonical_query(&pairs),
            content_sha256.unwrap_or(UNSIGNED_PAYLOAD)
        );
        let signature = self.signature(
            &date,
            &self.string_to_sign(&stamp, &date, &canonical_request),
        );
        pairs.push(("X-Amz-Signature".into(), signature));
        canonical_query(&pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EXAMPLE_TIME: i64 = 1_369_353_600_000; // 2013-05-24T00:00:00Z

    fn signer() -> Signer {
        Signer {
            access_key_id: ACCESS.into(),
            secret_access_key: SECRET.into(),
            region: "us-east-1".into(),
        }
    }

    #[test]
    fn encodes_like_the_aws_sdks() {
        assert_eq!(
            rfc3986("a b/c+d!*'()~_-.", true),
            "a%20b/c%2Bd%21%2A%27%28%29~_-."
        );
        assert_eq!(rfc3986("a/b", false), "a%2Fb");
        assert_eq!(rfc3986("é", false), "%C3%A9");
    }

    #[test]
    fn dates_are_basic_iso() {
        assert_eq!(
            amz_date(EXAMPLE_TIME),
            ("20130524".into(), "20130524T000000Z".into())
        );
    }

    /// The "GET Object" example of the S3 SigV4 documentation (header authorization with a Range header).
    #[test]
    fn signs_the_documented_get_object_example() {
        let headers = signer().sign_request(
            &Target {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
            },
            &[("range".into(), "bytes=0-9".into())],
            b"",
            EXAMPLE_TIME,
        );
        let auth = &headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .expect("authorization")
            .1;
        assert_eq!(
            auth.replace(", ", ","),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// The "PUT Object" example of the same documentation (a body, a storage-class header).
    #[test]
    fn signs_the_documented_put_object_example() {
        let headers = signer().sign_request(
            &Target {
                method: "PUT",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test%24file.text",
                query: &[],
            },
            &[
                ("date".into(), "Fri, 24 May 2013 00:00:00 GMT".into()),
                ("x-amz-storage-class".into(), "REDUCED_REDUNDANCY".into()),
            ],
            b"Welcome to Amazon S3.",
            EXAMPLE_TIME,
        );
        let auth = &headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .expect("authorization")
            .1;
        assert!(
            auth.ends_with(
                "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
            ),
            "{auth}"
        );
    }

    /// The "Authenticating Requests: Using Query Parameters" example (no content-sha256 parameter).
    #[test]
    fn presigns_the_documented_example() {
        let query = signer().presign_query(
            &Target {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                query: &[],
            },
            "86400",
            None,
            EXAMPLE_TIME,
        );
        assert!(
            query.contains(
                "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            ),
            "{query}"
        );
    }

    #[test]
    fn control_a_different_secret_changes_the_signature() {
        let mut other = signer();
        other.secret_access_key.push('x');
        let target = Target {
            method: "GET",
            host: "h",
            path: "/k",
            query: &[],
        };
        let a = signer().presign_query(&target, "60", None, EXAMPLE_TIME);
        let b = other.presign_query(&target, "60", None, EXAMPLE_TIME);
        assert_ne!(a, b);
    }
}
