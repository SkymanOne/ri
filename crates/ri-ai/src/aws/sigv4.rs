//! AWS Signature Version 4 for JSON requests, as `@smithy/signature-v4`
//! signs them for services other than S3.

use aws_lc_rs::hmac;
use sha2::{Digest, Sha256};

use super::Credentials;

/// Headers the signer never signs, as Smithy's `ALWAYS_UNSIGNABLE_HEADERS`.
const UNSIGNABLE: &[&str] = &[
    "authorization",
    "cache-control",
    "connection",
    "expect",
    "from",
    "keep-alive",
    "max-forwards",
    "pragma",
    "referer",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "x-amzn-trace-id",
];

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

/// RFC 3986 escaping of everything outside the unreserved set, as Smithy's
/// `escapeUri` (`encodeURIComponent` plus `!'()*`).
pub fn escape(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The canonical form of an already-encoded path: each segment escaped again.
fn canonical_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let mut out = format!(
        "/{}",
        segments
            .iter()
            .map(|segment| escape(segment))
            .collect::<Vec<_>>()
            .join("/")
    );
    if path.ends_with('/') && out.len() > 1 {
        out.push('/');
    }
    out
}

/// What to sign with.
pub struct Scope<'a> {
    /// Signing region.
    pub region: &'a str,
    /// Signing service name, such as `bedrock`.
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

/// Signs a request: adds `x-amz-date`, `x-amz-content-sha256` and the
/// session token to `headers`, then returns the `authorization` value.
/// `path` is the request path as sent; `query` is the canonical query
/// string, empty for none.
pub fn sign(
    method: &str,
    path: &str,
    query: &str,
    headers: &mut Vec<(String, String)>,
    body: &[u8],
    credentials: &Credentials,
    scope: &Scope<'_>,
) -> String {
    let payload_hash = sha256_hex(body);
    let mut set = |name: &str, value: String| {
        headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        headers.push((name.to_owned(), value));
    };
    set("x-amz-date", scope.amz_date.to_owned());
    set("x-amz-content-sha256", payload_hash.clone());
    if let Some(token) = &credentials.session_token {
        set("x-amz-security-token", token.clone());
    }
    let mut canonical: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            (name.to_lowercase(), value)
        })
        .filter(|(name, _)| !UNSIGNABLE.contains(&name.as_str()))
        .collect();
    canonical.sort();
    let signed_headers = canonical
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let header_block: String = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let request = format!(
        "{method}\n{}\n{query}\n{header_block}\n{signed_headers}\n{payload_hash}",
        canonical_path(path)
    );
    let date = &scope.amz_date[..8];
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{credential_scope}\n{}",
        scope.amz_date,
        sha256_hex(request.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    for part in [scope.region, scope.service, "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    )
}

/// `YYYYMMDDTHHMMSSZ` for Unix time in milliseconds.
pub fn amz_date(now_ms: u64) -> String {
    let seconds = now_ms / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let rest = seconds % 86_400;
    // Civil date from days, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_sdk_signature() {
        // A request pi's Bedrock provider made, with fake credentials.
        let body = r#"{"messages":[{"role":"user","content":[{"text":"hi"},{"cachePoint":{"type":"default"}}]}],"inferenceConfig":{"maxTokens":64000}}"#;
        let mut headers: Vec<(String, String)> = [
            ("content-type", "application/json"),
            ("content-length", "128"),
            ("x-amz-user-agent", "aws-sdk-js/3.1126.0"),
            ("user-agent", "aws-sdk-js/3.1126.0 ua/2.1"),
            ("host", "127.0.0.1:46595"),
            (
                "amz-sdk-invocation-id",
                "7226f89e-2566-48f7-a706-83de04cf2e08",
            ),
            ("amz-sdk-request", "attempt=1; max=3"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let credentials = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            expiration_ms: None,
        };
        let authorization = sign(
            "POST",
            "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream",
            "",
            &mut headers,
            body.as_bytes(),
            &credentials,
            &Scope {
                region: "us-east-1",
                service: "bedrock",
                amz_date: "20261004T190829Z",
            },
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261004/us-east-1/bedrock/aws4_request, SignedHeaders=amz-sdk-invocation-id;amz-sdk-request;content-length;content-type;host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=d8bfe77b50c1d282905f3dd383c9c21223013443acb57692c7782fd152662aa1"
        );
        assert!(headers.contains(&(
            "x-amz-content-sha256".into(),
            "5f46bb63fdce94cc025c273a1e4cbbffb21d8fc0c69bb5f44c03fa7e322acd8d".into()
        )));
    }

    #[test]
    fn formats_dates_and_paths() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_791_140_909_000), "20261004T190829Z");
        assert_eq!(
            canonical_path("/model/arn%3Aaws%3Abedrock/converse-stream"),
            "/model/arn%253Aaws%253Abedrock/converse-stream"
        );
        assert_eq!(escape("a b!*"), "a%20b%21%2A");
    }
}
