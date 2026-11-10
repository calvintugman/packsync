//! AWS Signature Version 4 (header-based), as S3, Cloudflare R2 and MinIO accept it.
//!
//! Reference: <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>.
//! The unit tests check this implementation against AWS's own published examples.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// `x-amz-content-sha256` value for a payload that is not hashed into the signature (allowed over
/// TLS; integrity is checked with `Content-MD5` instead).
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// SHA-256 of the empty string, the payload hash of a request without a body.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// RFC 3986 percent-encoding of everything except the unreserved characters (and `/` if asked).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The canonical query string: every name and value encoded, sorted by name (then value).
pub fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut enc: Vec<(String, String)> =
        pairs.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    enc.sort();
    enc.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// The canonical request and the `SignedHeaders` list. `headers` are (name, value) pairs; names are
/// lower-cased and values trimmed with inner runs of spaces collapsed, as the specification requires.
pub fn canonical_request(
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> (String, String) {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    (format!("{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}"), signed)
}

fn hmac(key: &[u8], data: &str) -> [u8; 32] {
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().into()
}

/// The derived signing key for one day, region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    hmac(&k_service, "aws4_request")
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Everything needed to sign one request.
pub struct SignInput<'a> {
    pub access_key_id: &'a str,
    pub secret: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`, the value of the `x-amz-date` header.
    pub amz_date: &'a str,
    pub method: &'a str,
    pub canonical_uri: &'a str,
    pub query: &'a [(String, String)],
    /// Every header to sign, including `host` and `x-amz-date`.
    pub headers: &'a [(String, String)],
    pub payload_hash: &'a str,
}

/// The signature (hex) of a request.
pub fn signature(i: &SignInput<'_>) -> String {
    let (creq, _) = canonical_request(i.method, i.canonical_uri, &canonical_query(i.query), i.headers, i.payload_hash);
    let date = &i.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", i.region, i.service);
    let sts = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", i.amz_date, sha256_hex(creq.as_bytes()));
    hex::encode(hmac(&signing_key(i.secret, date, i.region, i.service), &sts))
}

/// The `Authorization` header value.
pub fn authorization(i: &SignInput<'_>) -> String {
    let (_, signed) = canonical_request(i.method, i.canonical_uri, "", i.headers, i.payload_hash);
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}/{}/{}/aws4_request, SignedHeaders={signed}, Signature={}",
        i.access_key_id,
        &i.amz_date[..8],
        i.region,
        i.service,
        signature(i)
    )
}

/// `x-amz-date` for now.
pub fn amz_date_now() -> String {
    let f = time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
    time::OffsetDateTime::now_utc().format(&f).expect("formatting a UTC time cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// AWS documentation: "Examples of how to derive a signing key for Signature Version 4".
    #[test]
    fn signing_key_matches_the_aws_example() {
        let k = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20150830", "us-east-1", "iam");
        assert_eq!(hex::encode(k), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
    }

    /// AWS SigV4 test suite, `get-vanilla`.
    #[test]
    fn get_vanilla_from_the_aws_test_suite() {
        let headers = h(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")]);
        let i = SignInput {
            access_key_id: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
            method: "GET",
            canonical_uri: "/",
            query: &[],
            headers: &headers,
            payload_hash: EMPTY_SHA256,
        };
        assert_eq!(signature(&i), "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        assert_eq!(
            authorization(&i),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    const S3_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    /// Amazon S3 documentation, "Signature Calculations for the Authorization Header", example
    /// GET Object (with a Range header).
    #[test]
    fn s3_get_object_example() {
        let headers = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let i = SignInput {
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret: S3_SECRET,
            region: "us-east-1",
            service: "s3",
            amz_date: "20130524T000000Z",
            method: "GET",
            canonical_uri: "/test.txt",
            query: &[],
            headers: &headers,
            payload_hash: EMPTY_SHA256,
        };
        assert_eq!(signature(&i), "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }

    /// Same documentation, example PUT Object (a `$` in the key is percent-encoded).
    #[test]
    fn s3_put_object_example() {
        let body_hash = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(body_hash, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let headers = h(&[
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", &body_hash),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ]);
        let i = SignInput {
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret: S3_SECRET,
            region: "us-east-1",
            service: "s3",
            amz_date: "20130524T000000Z",
            method: "PUT",
            canonical_uri: &uri_encode("/test$file.text", true),
            query: &[],
            headers: &headers,
            payload_hash: &body_hash,
        };
        assert_eq!(i.canonical_uri, "/test%24file.text");
        assert_eq!(signature(&i), "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
    }

    /// Same documentation, example GET Bucket (List Objects) with a query string.
    #[test]
    fn s3_list_objects_example() {
        let headers = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let query = [("max-keys".to_string(), "2".to_string()), ("prefix".to_string(), "J".to_string())];
        let i = SignInput {
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret: S3_SECRET,
            region: "us-east-1",
            service: "s3",
            amz_date: "20130524T000000Z",
            method: "GET",
            canonical_uri: "/",
            query: &query,
            headers: &headers,
            payload_hash: EMPTY_SHA256,
        };
        assert_eq!(signature(&i), "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    #[test]
    fn encoding_and_query_ordering() {
        assert_eq!(uri_encode("a b/c+d=~", false), "a%20b%2Fc%2Bd%3D~");
        assert_eq!(uri_encode("packs/x.pack", true), "packs/x.pack");
        let q = canonical_query(&[
            ("prefix".into(), "a/b".into()),
            ("list-type".into(), "2".into()),
            ("delimiter".into(), "/".into()),
        ]);
        assert_eq!(q, "delimiter=%2F&list-type=2&prefix=a%2Fb");
    }
}
