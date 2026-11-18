//! AWS Signature Version 4, enough for S3 objects and SES v2 SendEmail.
//! Hand-rolled because the two calls Substrate makes do not justify the SDK.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub struct Creds {
    pub key_id: String,
    pub secret: String,
    pub region: String,
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// Percent-encode per SigV4 (RFC 3986 unreserved kept), `/` kept when `path`.
pub fn uri_encode(s: &str, path: bool) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) || (path && c == '/') {
            o.push(c);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// Headers to add to a request: `authorization`, `x-amz-date`,
/// `x-amz-content-sha256`. `host` must be sent as given.
pub fn sign(
    c: &Creds,
    service: &str,
    method: &str,
    host: &str,
    path: &str,
    query: &[(String, String)],
    extra_headers: &[(String, String)],
    body: &[u8],
    at: DateTime<Utc>,
) -> Vec<(String, String)> {
    let amz_date = at.format("%Y%m%dT%H%M%SZ").to_string();
    let date = at.format("%Y%m%d").to_string();
    let payload_hash = hex::encode(Sha256::digest(body));

    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), host.to_string()),
        ("x-amz-content-sha256".into(), payload_hash.clone()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    for (k, v) in extra_headers {
        headers.push((k.to_lowercase(), v.trim().to_string()));
    }
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical = format!(
        "{method}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        uri_encode(path, true)
    );
    let scope = format!("{date}/{}/{service}/aws4_request", c.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{}", c.secret).as_bytes(), &date);
    let k = hmac(&k, &c.region);
    let k = hmac(&k, service);
    let k = hmac(&k, "aws4_request");
    let sig = hex::encode(hmac(&k, &to_sign));
    vec![
        (
            "authorization".into(),
            format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={sig}", c.key_id),
        ),
        ("x-amz-date".into(), amz_date),
        ("x-amz-content-sha256".into(), payload_hash),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // AWS's published example (GET /test.txt, examplebucket, us-east-1,
    // 2013-05-24) from the S3 SigV4 docs.
    #[test]
    fn matches_aws_example() {
        let c = Creds {
            key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
        };
        let at = Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();
        let h = sign(&c, "s3", "GET", "examplebucket.s3.amazonaws.com", "/test.txt", &[],
            &[("Range".into(), "bytes=0-9".into())], b"", at);
        assert_eq!(
            h[0].1,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }
}
