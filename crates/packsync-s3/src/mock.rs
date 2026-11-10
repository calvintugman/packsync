//! An in-process S3 server for tests and benchmarks: the subset PackSync uses (PUT with
//! `Content-MD5` and `If-None-Match`/`If-Match`, GET with ranges, HEAD, DELETE, ListObjectsV2 with
//! delimiter and pagination), path-style, one bucket. It **verifies every request's SigV4
//! signature** from the request as received (so encoding mistakes on the client side are caught), and
//! can inject throttling (`503 SlowDown`) for retry tests.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use base64::Engine as _;
use md5::{Digest as _, Md5};
use parking_lot::Mutex;

use crate::sigv4;

pub const ACCESS_KEY: &str = "MOCKACCESSKEY";
pub const SECRET: &str = "mock/secret/key+with/slashes";
pub const REGION: &str = "auto";

/// A response: status, headers, body.
type Answer = (u16, Vec<(String, String)>, Vec<u8>);

#[derive(Clone)]
struct Object {
    data: Vec<u8>,
    etag: String,
    meta: BTreeMap<String, String>,
    modified: std::time::SystemTime,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Object>,
}

pub struct MockS3 {
    url: String,
    bucket: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    pub requests: Arc<AtomicU64>,
    fail_next: Arc<AtomicU64>,
    server: Arc<tiny_http::Server>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Largest page a listing returns (S3 allows 1000); small values exercise pagination.
    page_cap: usize,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn error(status: u16, code: &str, message: &str) -> Answer {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{}</Message></Error>",
        xml_escape(message)
    );
    (status, vec![("Content-Type".into(), "application/xml".into())], body.into_bytes())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len() + 1
            && let Some(v) = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

impl MockS3 {
    pub fn start(bucket: &str) -> MockS3 {
        Self::start_with(bucket, 1000)
    }

    pub fn start_with(bucket: &str, page_cap: usize) -> MockS3 {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind the mock S3 server"));
        let port = server.server_addr().to_ip().expect("ip listener").port();
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicU64::new(0));
        let fail_next = Arc::new(AtomicU64::new(0));
        let mut workers = vec![];
        for _ in 0..12 {
            let (server, state, stop, requests, fail_next, bucket) =
                (server.clone(), state.clone(), stop.clone(), requests.clone(), fail_next.clone(), bucket.to_string());
            workers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok(Some(mut req)) = server.recv_timeout(std::time::Duration::from_millis(100)) else {
                        continue;
                    };
                    requests.fetch_add(1, Ordering::SeqCst);
                    let mut body = Vec::new();
                    let _ = req.as_reader().read_to_end(&mut body);
                    let (status, headers, out) = if fail_next.load(Ordering::SeqCst) > 0 {
                        fail_next.fetch_sub(1, Ordering::SeqCst);
                        error(503, "SlowDown", "Please reduce your request rate.")
                    } else {
                        handle(&req, &body, &bucket, &state, page_cap)
                    };
                    // For HEAD, tiny_http sends the headers and no body. It switches to chunked transfer for
                    // bodies over 32 KiB, which has no Content-Length — S3 always sends one (HEAD reports the
                    // object's size with it), so chunking is turned off.
                    let mut resp =
                        tiny_http::Response::from_data(out).with_status_code(status).with_chunked_threshold(usize::MAX);
                    for (k, v) in headers {
                        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                            resp.add_header(h);
                        }
                    }
                    let _ = req.respond(resp);
                }
            }));
        }
        MockS3 {
            url: format!("http://127.0.0.1:{port}"),
            bucket: bucket.into(),
            state,
            stop,
            requests,
            fail_next,
            server,
            workers,
            page_cap,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Answer the next `n` requests with `503 SlowDown`.
    pub fn fail_next(&self, n: u64) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    /// Every stored key (full key, with the prefix).
    pub fn keys(&self) -> Vec<String> {
        self.state.lock().objects.keys().cloned().collect()
    }

    pub fn page_cap(&self) -> usize {
        self.page_cap
    }

    /// A configuration for an [`crate::S3Storage`] talking to this server.
    pub fn config(&self, prefix: &str) -> crate::S3Config {
        let mut c = crate::S3Config::r2("mock", &self.bucket, prefix, ACCESS_KEY);
        c.endpoint = self.url.clone();
        c.region = REGION.into();
        c.label = "mock-s3".into();
        c.retry = packsync_core::retry::RetryPolicy {
            max_attempts: 4,
            base: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
        };
        c
    }
}

impl Drop for MockS3 {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn header<'a>(req: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

/// Check the SigV4 `Authorization` header against the request as received.
fn verify_signature(
    req: &tiny_http::Request,
    path: &str,
    query: &str,
) -> Result<(), Answer> {
    let auth = header(req, "authorization").ok_or_else(|| error(403, "AccessDenied", "no Authorization header"))?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or_else(|| error(400, "AuthorizationHeaderMalformed", "not SigV4"))?;
    let mut cred = None;
    let mut signed = None;
    let mut sig = None;
    for part in rest.split(", ") {
        if let Some(v) = part.strip_prefix("Credential=") {
            cred = Some(v);
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed = Some(v);
        } else if let Some(v) = part.strip_prefix("Signature=") {
            sig = Some(v);
        }
    }
    let (cred, signed, sig) = match (cred, signed, sig) {
        (Some(c), Some(s), Some(g)) => (c, s, g),
        _ => return Err(error(400, "AuthorizationHeaderMalformed", "incomplete Authorization header")),
    };
    let parts: Vec<&str> = cred.split('/').collect();
    if parts.len() != 5 || parts[0] != ACCESS_KEY {
        return Err(error(403, "InvalidAccessKeyId", "unknown access key"));
    }
    if parts[2] != REGION || parts[3] != "s3" || parts[4] != "aws4_request" {
        return Err(error(400, "AuthorizationHeaderMalformed", "wrong scope"));
    }
    if !signed.split(';').any(|h| h == "host") || !signed.split(';').any(|h| h == "x-amz-date") {
        return Err(error(400, "AuthorizationHeaderMalformed", "host and x-amz-date must be signed"));
    }
    let amz_date = header(req, "x-amz-date").ok_or_else(|| error(400, "AccessDenied", "no x-amz-date"))?;
    if !amz_date.starts_with(parts[1]) {
        return Err(error(403, "SignatureDoesNotMatch", "the credential date does not match x-amz-date"));
    }
    let mut headers = vec![];
    for name in signed.split(';') {
        let v = header(req, name).ok_or_else(|| error(403, "SignatureDoesNotMatch", "a signed header is missing"))?;
        headers.push((name.to_string(), v.to_string()));
    }
    let payload =
        header(req, "x-amz-content-sha256").ok_or_else(|| error(400, "InvalidRequest", "no x-amz-content-sha256"))?;
    let q = query_pairs(query);
    let expected = sigv4::signature(&sigv4::SignInput {
        access_key_id: ACCESS_KEY,
        secret: SECRET,
        region: REGION,
        service: "s3",
        amz_date,
        method: req.method().as_str(),
        canonical_uri: path,
        query: &q,
        headers: &headers,
        payload_hash: payload,
    });
    if expected != sig {
        return Err(error(
            403,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        ));
    }
    Ok(())
}

fn http_date(t: std::time::SystemTime) -> String {
    httpdate::fmt_http_date(t)
}

fn iso(t: std::time::SystemTime) -> String {
    let d = time::OffsetDateTime::from(t);
    d.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

fn handle(
    req: &tiny_http::Request,
    body: &[u8],
    bucket: &str,
    state: &Mutex<State>,
    page_cap: usize,
) -> Answer {
    let url = req.url().to_string();
    let (raw_path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    if let Err(e) = verify_signature(req, raw_path, query) {
        return e;
    }
    // path-style: /bucket[/key]; keys PackSync writes need no decoding beyond %XX
    let path = percent_decode(raw_path);
    let mut segs = path.trim_start_matches('/').splitn(2, '/');
    let b = segs.next().unwrap_or("");
    let key = segs.next().unwrap_or("").to_string();
    if b != bucket {
        return error(404, "NoSuchBucket", "The specified bucket does not exist");
    }
    let method = req.method().as_str().to_string();
    match (method.as_str(), key.is_empty()) {
        ("GET", true) => list(query, state, page_cap),
        ("PUT", false) => {
            let md5 = Md5::digest(body);
            if let Some(cm) = header(req, "content-md5")
                && base64::engine::general_purpose::STANDARD.encode(md5) != cm
            {
                return error(400, "BadDigest", "The Content-MD5 you specified did not match what we received.");
            }
            let etag = hex::encode(md5);
            let mut st = state.lock();
            let current = st.objects.get(&key);
            if header(req, "if-none-match") == Some("*") && current.is_some() {
                return error(
                    412,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                );
            }
            if let Some(m) = header(req, "if-match")
                && current.is_none_or(|o| format!("\"{}\"", o.etag) != m.trim())
            {
                return error(
                    412,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                );
            }
            let meta = req
                .headers()
                .iter()
                .filter_map(|h| {
                    let n = h.field.as_str().as_str().to_ascii_lowercase();
                    n.strip_prefix("x-amz-meta-").map(|k| (k.to_string(), h.value.as_str().to_string()))
                })
                .collect();
            st.objects.insert(
                key,
                Object { data: body.to_vec(), etag: etag.clone(), meta, modified: std::time::SystemTime::now() },
            );
            (200, vec![("ETag".into(), format!("\"{etag}\""))], vec![])
        }
        ("GET", false) | ("HEAD", false) => {
            let st = state.lock();
            let Some(o) = st.objects.get(&key) else {
                return if method == "HEAD" {
                    (404, vec![], vec![])
                } else {
                    error(404, "NoSuchKey", "The specified key does not exist.")
                };
            };
            let mut hs = vec![
                ("ETag".to_string(), format!("\"{}\"", o.etag)),
                ("Last-Modified".to_string(), http_date(o.modified)),
            ];
            for (k, v) in &o.meta {
                hs.push((format!("x-amz-meta-{k}"), v.clone()));
            }
            if method == "HEAD" {
                return (200, hs, o.data.clone());
            }
            if let Some(r) = header(req, "range") {
                let spec = r.trim().strip_prefix("bytes=").unwrap_or("");
                let (a, b) = spec.split_once('-').unwrap_or(("", ""));
                let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) else {
                    return error(400, "InvalidRange", "bad range");
                };
                if a >= o.data.len() || b < a {
                    return error(416, "InvalidRange", "The requested range is not satisfiable");
                }
                let b = b.min(o.data.len() - 1);
                return (206, hs, o.data[a..=b].to_vec());
            }
            (200, hs, o.data.clone())
        }
        ("DELETE", false) => {
            state.lock().objects.remove(&key);
            (204, vec![], vec![])
        }
        _ => error(405, "MethodNotAllowed", "not supported by the mock"),
    }
}

fn list(query: &str, state: &Mutex<State>, page_cap: usize) -> Answer {
    let q: BTreeMap<String, String> = query_pairs(query).into_iter().collect();
    if q.get("list-type").map(String::as_str) != Some("2") {
        return error(400, "InvalidArgument", "only ListObjectsV2 is supported");
    }
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let delim = q.get("delimiter").cloned();
    let max = q.get("max-keys").and_then(|m| m.parse::<usize>().ok()).unwrap_or(1000).min(page_cap).max(1);
    let after = q
        .get("continuation-token")
        .and_then(|t| base64::engine::general_purpose::STANDARD.decode(t).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let st = state.lock();
    // entries in key order: objects, or one common prefix per "sub-directory"
    let mut entries: Vec<(String, Option<&Object>)> = vec![];
    let mut seen_prefixes = std::collections::BTreeSet::new();
    for (k, o) in st.objects.range(prefix.clone()..) {
        if !k.starts_with(&prefix) {
            break;
        }
        let rest = &k[prefix.len()..];
        match delim.as_deref().and_then(|d| rest.find(d).map(|i| (d, i))) {
            Some((d, i)) => {
                let p = format!("{prefix}{}{d}", &rest[..i]);
                if seen_prefixes.insert(p.clone()) {
                    entries.push((p, None));
                }
            }
            None => entries.push((k.clone(), Some(o))),
        }
    }
    let start = after.as_ref().map(|a| entries.iter().position(|(k, _)| k > a).unwrap_or(entries.len())).unwrap_or(0);
    let page = &entries[start..(start + max).min(entries.len())];
    let truncated = start + max < entries.len();
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    x.push_str(&format!(
        "<Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
        xml_escape(&prefix),
        page.len()
    ));
    for (k, o) in page {
        match o {
            Some(o) => x.push_str(&format!(
                "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                xml_escape(k),
                iso(o.modified),
                o.etag,
                o.data.len()
            )),
            None => x.push_str(&format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", xml_escape(k))),
        }
    }
    if truncated && let Some((last, _)) = page.last() {
        x.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            base64::engine::general_purpose::STANDARD.encode(last.as_bytes())
        ));
    }
    x.push_str("</ListBucketResult>");
    (200, vec![("Content-Type".into(), "application/xml".into())], x.into_bytes())
}
