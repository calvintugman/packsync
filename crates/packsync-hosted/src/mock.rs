//! An in-process server with the hosted service's API (`services/hosted/src/index.ts`), for tests and
//! local demos: accounts with quotas, objects with conditional writes and ranges, pairing mailboxes.
//! It follows the Worker's rules; the Worker itself is checked against the same client with
//! `PACKSYNC_TEST_HOSTED_URL` (see `tests/service.rs`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use md5::{Digest as _, Md5};
use parking_lot::Mutex;
use sha2::Sha256;

type Answer = (u16, Vec<(String, String)>, Vec<u8>);

struct Object {
    data: Vec<u8>,
    etag: String,
    uploaded_ms: i64,
}

struct Account {
    token: String,
    used: u64,
    quota: u64,
}

#[derive(Default)]
struct Pairing {
    account: String,
    created: Option<std::time::Instant>,
    slots: HashMap<char, String>,
}

#[derive(Default)]
struct State {
    accounts: HashMap<String, Account>,
    /// `<account>/<key>` → object
    objects: BTreeMap<String, Object>,
    pairings: HashMap<String, Pairing>,
    next_nameplate: u32,
    /// credential ID → (account, sealed blob)
    passkeys: HashMap<String, (String, String)>,
}

pub struct MockService {
    url: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    pub requests: Arc<AtomicU64>,
    quota: Arc<AtomicU64>,
    server: Arc<tiny_http::Server>,
    workers: Vec<std::thread::JoinHandle<()>>,
    page_cap: usize,
}

fn json(status: u16, v: serde_json::Value) -> Answer {
    (status, vec![("Content-Type".into(), "application/json".into())], v.to_string().into_bytes())
}

fn err(status: u16, code: &str, message: &str) -> Answer {
    json(status, serde_json::json!({ "error": code, "message": message }))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn random_id(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("random");
    let abc = b"abcdefghijklmnopqrstuvwxyz234567";
    b.iter().map(|x| abc[(x % 32) as usize] as char).collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
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

impl MockService {
    pub fn start() -> MockService {
        Self::start_with(1000)
    }

    /// `page_cap`: largest listing page (the Worker returns up to 1000); small values test paging.
    pub fn start_with(page_cap: usize) -> MockService {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind the mock service"));
        let port = server.server_addr().to_ip().expect("ip listener").port();
        let mut m = MockService {
            url: format!("http://127.0.0.1:{port}"),
            state: Arc::new(Mutex::new(State { next_nameplate: 1000, ..Default::default() })),
            stop: Arc::new(AtomicBool::new(false)),
            requests: Arc::new(AtomicU64::new(0)),
            quota: Arc::new(AtomicU64::new(20 << 30)),
            server,
            workers: vec![],
            page_cap,
        };
        for _ in 0..12 {
            let (server, state, stop, requests, quota, cap) =
                (m.server.clone(), m.state.clone(), m.stop.clone(), m.requests.clone(), m.quota.clone(), m.page_cap);
            m.workers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok(Some(mut req)) = server.recv_timeout(std::time::Duration::from_millis(100)) else {
                        continue;
                    };
                    requests.fetch_add(1, Ordering::SeqCst);
                    let mut body = Vec::new();
                    let _ = std::io::Read::read_to_end(req.as_reader(), &mut body);
                    let headers: HashMap<String, String> = req
                        .headers()
                        .iter()
                        .map(|h| (h.field.as_str().as_str().to_ascii_lowercase(), h.value.as_str().to_string()))
                        .collect();
                    let method = req.method().as_str().to_string();
                    let (status, hs, out) = handle(&state, &quota, cap, &method, req.url(), &headers, body);
                    // tiny_http leaves the body out of HEAD answers but keeps its Content-Length
                    let mut resp = tiny_http::Response::from_data(out).with_status_code(status);
                    for (k, v) in hs {
                        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                            resp.add_header(h);
                        }
                    }
                    let _ = req.respond(resp);
                }
            }));
        }
        m
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Space each account may use from now on (new and existing accounts).
    pub fn set_quota(&self, bytes: u64) {
        self.quota.store(bytes, Ordering::SeqCst);
        for a in self.state.lock().accounts.values_mut() {
            a.quota = bytes;
        }
    }

    /// Stored (encrypted) bytes of an object, for tests that look at what the service sees.
    pub fn raw_object(&self, account: &str, key: &str) -> Option<Vec<u8>> {
        self.state.lock().objects.get(&format!("{account}/{key}")).map(|o| o.data.clone())
    }

    /// Every stored key, `<account>/<key>`.
    pub fn keys(&self) -> Vec<String> {
        self.state.lock().objects.keys().cloned().collect()
    }
}

impl Drop for MockService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn valid_key(key: &str, dir: bool) -> bool {
    if key.is_empty() || key.len() > 512 || dir != key.ends_with('/') {
        return false;
    }
    let k = if dir { &key[..key.len() - 1] } else { key };
    k.split('/').all(|p| {
        !p.is_empty() && p != "." && p != ".." && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    })
}

fn authenticate(st: &State, headers: &HashMap<String, String>) -> Result<String, Answer> {
    let h = headers.get("authorization").map(String::as_str).unwrap_or("");
    let Some((id, token)) = h.strip_prefix("Bearer ").and_then(|v| v.split_once('.')) else {
        return Err(err(401, "unauthorized", "missing or malformed credentials"));
    };
    match st.accounts.get(id) {
        Some(a) if a.token == token => Ok(id.to_string()),
        _ => Err(err(401, "unauthorized", "unknown account or wrong token")),
    }
}

fn obj_headers(o: &Object) -> Vec<(String, String)> {
    vec![
        ("ETag".into(), format!("\"{}\"", o.etag)),
        ("X-Uploaded-Ms".into(), o.uploaded_ms.to_string()),
        ("X-Object-Size".into(), o.data.len().to_string()),
        ("Content-Type".into(), "application/octet-stream".into()),
    ]
}

fn handle(
    state: &Mutex<State>,
    quota: &AtomicU64,
    page_cap: usize,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: Vec<u8>,
) -> Answer {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let q: HashMap<String, String> = query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect();
    let mut st = state.lock();
    match (method, path) {
        ("POST", "/v1/accounts") => {
            let id = random_id(26);
            let mut t = [0u8; 32];
            getrandom::getrandom(&mut t).expect("random");
            let token = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, t);
            let q = quota.load(Ordering::SeqCst);
            st.accounts.insert(id.clone(), Account { token: token.clone(), used: 0, quota: q });
            json(201, serde_json::json!({ "account": id, "token": token, "quota": q }))
        }
        ("DELETE", "/v1/accounts") => {
            let id = match authenticate(&st, headers) {
                Ok(id) => id,
                Err(a) => return a,
            };
            let prefix = format!("{id}/");
            st.objects.retain(|k, _| !k.starts_with(&prefix));
            st.accounts.remove(&id);
            json(200, serde_json::json!({ "ok": true }))
        }
        ("GET", "/v1/usage") => match authenticate(&st, headers) {
            Ok(id) => {
                let a = &st.accounts[&id];
                json(200, serde_json::json!({ "used": a.used, "quota": a.quota }))
            }
            Err(a) => a,
        },
        ("GET", "/v1/objects") => {
            let id = match authenticate(&st, headers) {
                Ok(id) => id,
                Err(a) => return a,
            };
            let prefix = q.get("prefix").cloned().unwrap_or_default();
            if !prefix.is_empty() && !valid_key(&prefix, true) {
                return err(400, "bad_key", "the prefix must be a directory key");
            }
            let root = format!("{id}/");
            let full = format!("{root}{prefix}");
            // all entries in order (objects and directories), then a page from the cursor
            let mut entries: Vec<(String, Option<&Object>)> = vec![];
            let mut seen_dirs = std::collections::BTreeSet::new();
            for (k, o) in st.objects.range(full.clone()..) {
                let Some(rest) = k.strip_prefix(&full) else { break };
                match rest.find('/') {
                    Some(i) => {
                        let d = format!("{prefix}{}/", &rest[..i]);
                        if seen_dirs.insert(d.clone()) {
                            entries.push((d, None));
                        }
                    }
                    None => entries.push((k[root.len()..].to_string(), Some(o))),
                }
            }
            let start: usize = q.get("cursor").and_then(|c| c.parse().ok()).unwrap_or(0);
            let page: Vec<_> = entries.iter().skip(start).take(page_cap).collect();
            let more = start + page.len() < entries.len();
            let objects: Vec<_> = page
                .iter()
                .filter_map(|(k, o)| o.map(|o| serde_json::json!({"key": k, "size": o.data.len(), "etag": o.etag, "uploaded_ms": o.uploaded_ms})))
                .collect();
            let prefixes: Vec<_> = page.iter().filter(|(_, o)| o.is_none()).map(|(k, _)| k.clone()).collect();
            json(
                200,
                serde_json::json!({
                    "objects": objects, "prefixes": prefixes,
                    "cursor": if more { Some((start + page.len()).to_string()) } else { None }
                }),
            )
        }
        (_, p) if p.starts_with("/v1/objects/") => {
            let key = percent_decode(&p["/v1/objects/".len()..]);
            if !valid_key(&key, false) {
                return err(400, "bad_key", "invalid object key");
            }
            let id = match authenticate(&st, headers) {
                Ok(id) => id,
                Err(a) => return a,
            };
            let full = format!("{id}/{key}");
            match method {
                "HEAD" | "GET" => {
                    let Some(o) = st.objects.get(&full) else { return err(404, "not_found", "no such object") };
                    let mut hs = obj_headers(o);
                    if method == "GET"
                        && let Some(r) = headers.get("range")
                    {
                        let Some((a, b)) = r.strip_prefix("bytes=").and_then(|r| r.split_once('-')) else {
                            return err(400, "bad_range", "bad range");
                        };
                        let (a, b): (usize, usize) = match (a.parse(), b.parse()) {
                            (Ok(a), Ok(b)) if b >= a => (a, b),
                            _ => return err(400, "bad_range", "bad range"),
                        };
                        if a >= o.data.len() {
                            return err(416, "bad_range", "range starts after the end");
                        }
                        let end = (b + 1).min(o.data.len());
                        hs.push(("Content-Range".into(), format!("bytes {a}-{}/{}", end - 1, o.data.len())));
                        return (206, hs, o.data[a..end].to_vec());
                    }
                    (200, hs, o.data.clone())
                }
                "PUT" => {
                    let sha = headers.get("x-content-sha256").cloned().unwrap_or_default();
                    if hex::encode(Sha256::digest(&body)) != sha {
                        return err(400, "bad_digest", "the upload was damaged in transit");
                    }
                    if body.len() as u64 > crate::client::MAX_PUT {
                        return err(413, "too_large", "too large");
                    }
                    let existing = st.objects.get(&full);
                    if headers.get("if-none-match").map(String::as_str) == Some("*") && existing.is_some() {
                        return err(412, "precondition_failed", "already exists");
                    }
                    if let Some(m) = headers.get("if-match")
                        && existing.map(|o| format!("\"{}\"", o.etag)).as_deref() != Some(m.trim())
                    {
                        return err(412, "precondition_failed", "changed meanwhile");
                    }
                    let delta = body.len() as i64 - existing.map(|o| o.data.len() as i64).unwrap_or(0);
                    let a = st.accounts.get_mut(&id).expect("authenticated");
                    if delta > 0 && a.used + delta as u64 > a.quota {
                        let (u, q) = (a.used, a.quota);
                        return err(507, "quota", &format!("this account's space is full ({u} of {q} bytes used)"));
                    }
                    a.used = (a.used as i64 + delta).max(0) as u64;
                    let o = Object { etag: hex::encode(Md5::digest(&body)), uploaded_ms: now_ms(), data: body };
                    let hs = obj_headers(&o);
                    let reply =
                        serde_json::json!({ "etag": o.etag, "size": o.data.len(), "uploaded_ms": o.uploaded_ms });
                    st.objects.insert(full, o);
                    let (s, mut h, b) = json(200, reply);
                    h.extend(hs.into_iter().filter(|(k, _)| k != "Content-Type"));
                    (s, h, b)
                }
                "DELETE" => {
                    if let Some(o) = st.objects.remove(&full) {
                        let a = st.accounts.get_mut(&id).expect("authenticated");
                        a.used = a.used.saturating_sub(o.data.len() as u64);
                    }
                    json(200, serde_json::json!({ "ok": true }))
                }
                _ => err(405, "method", "method not allowed"),
            }
        }
        (_, p) if p.starts_with("/v1/passkeys/") => {
            let cid = p["/v1/passkeys/".len()..].to_string();
            match method {
                "GET" => match st.passkeys.get(&cid) {
                    Some((_, blob)) => json(200, serde_json::json!({ "blob": blob })),
                    None => err(404, "not_found", "this passkey is not registered with PackSync"),
                },
                "PUT" => {
                    let id = match authenticate(&st, headers) {
                        Ok(id) => id,
                        Err(a) => return a,
                    };
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let Some(blob) = v["blob"].as_str() else { return err(400, "bad_blob", "not a sealed blob") };
                    if st.passkeys.get(&cid).is_some_and(|(a, _)| *a != id) {
                        return err(409, "taken", "another account's passkey");
                    }
                    st.passkeys.insert(cid, (id, blob.to_string()));
                    json(201, serde_json::json!({ "ok": true }))
                }
                _ => err(405, "method", "method not allowed"),
            }
        }
        ("POST", "/v1/pairings") => {
            let id = match authenticate(&st, headers) {
                Ok(id) => id,
                Err(a) => return a,
            };
            st.next_nameplate = if st.next_nameplate >= 9999 { 1000 } else { st.next_nameplate + 1 };
            let n = st.next_nameplate.to_string();
            st.pairings.insert(
                n.clone(),
                Pairing { account: id, created: Some(std::time::Instant::now()), slots: HashMap::new() },
            );
            json(201, serde_json::json!({ "nameplate": n, "expires_ms": now_ms() + 600_000 }))
        }
        (_, p) if p.starts_with("/v1/pairings/") => {
            let rest = &p["/v1/pairings/".len()..];
            let (n, slot) = match rest.split_once('/') {
                Some((n, s)) => (n.to_string(), s.chars().next()),
                None => (rest.to_string(), None),
            };
            let live = st
                .pairings
                .get(&n)
                .is_some_and(|pr| pr.created.is_some_and(|c| c.elapsed() < std::time::Duration::from_secs(600)));
            match (method, slot) {
                ("DELETE", None) => {
                    let id = match authenticate(&st, headers) {
                        Ok(id) => id,
                        Err(a) => return a,
                    };
                    if st.pairings.get(&n).is_some_and(|pr| pr.account == id) {
                        st.pairings.remove(&n);
                    }
                    json(200, serde_json::json!({ "ok": true }))
                }
                (_, Some(s)) if "abcd".contains(s) => {
                    if !live {
                        return err(404, "expired", "this pairing code has expired");
                    }
                    if method == "GET" {
                        return match st.pairings[&n].slots.get(&s) {
                            Some(v) => (200, vec![], v.clone().into_bytes()),
                            None => json(202, serde_json::json!({ "pending": true })),
                        };
                    }
                    if "ac".contains(s) {
                        match authenticate(&st, headers) {
                            Ok(id) if id == st.pairings[&n].account => {}
                            Ok(_) => return err(403, "forbidden", "another account's pairing"),
                            Err(a) => return a,
                        }
                    }
                    let text = String::from_utf8_lossy(&body).into_owned();
                    if text.is_empty()
                        || text.len() > 4096
                        || !text.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    {
                        return err(400, "bad_message", "pairing messages are short base64url text");
                    }
                    let pr = st.pairings.get_mut(&n).expect("live");
                    if pr.slots.contains_key(&s) {
                        return err(409, "taken", "this pairing code was already used");
                    }
                    pr.slots.insert(s, text);
                    json(201, serde_json::json!({ "ok": true }))
                }
                _ => err(404, "not_found", "no such endpoint"),
            }
        }
        _ => err(404, "not_found", "no such endpoint"),
    }
}
