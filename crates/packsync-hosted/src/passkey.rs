//! Getting back into the PackSync cloud with a passkey, when no paired computer is left.
//!
//! The service's `/passkey` page runs WebAuthn with the PRF extension in the system browser: the passkey
//! (Windows Hello, iCloud Keychain, Google Password Manager, a phone, a security key) produces a 32-byte
//! secret that only it can produce again. The page hands that secret to this program through a one-off
//! address on this computer (`http://127.0.0.1:<port>/`), never to the service. Here it seals the
//! account's credentials and encryption key; the service stores only the sealed blob, under the passkey's
//! ID. To recover, the page asks for the passkey again, this program fetches the blob by that ID and
//! opens it. The service never has to check a passkey signature: without the secret, the blob is noise.

use std::time::{Duration, Instant};

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use packsync_core::progress::OpCtx;
use packsync_core::storage::{StorageError, StorageResult};
use serde::Deserialize;
use zeroize::Zeroize;

use crate::client::{HostedStorage, Http, check_endpoint, classify};
use crate::{Credentials, b64};

const PURPOSE: &str = "packsync 2026 passkey recovery blob";
const WAIT: Duration = Duration::from_secs(5 * 60);

/// What the browser page reported.
struct Answer {
    credential_id: String,
    secret: [u8; 32],
}

impl Drop for Answer {
    fn drop(&mut self) {
        self.credential_id.zeroize();
        self.secret.zeroize();
    }
}

fn valid_credential_id(id: &str) -> bool {
    (16..=1400).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sealer(secret: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&blake3::derive_key(PURPOSE, secret).into())
}

/// The account's credentials sealed under a passkey's secret, bound to that passkey's ID.
fn seal(creds: &Credentials, credential_id: &str, secret: &[u8; 32]) -> String {
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut nonce).expect("random nonce");
    let plain = creds.recovery_key();
    let ct = sealer(secret)
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain.expose().as_bytes(), aad: credential_id.as_bytes() })
        .expect("in-memory encryption");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    b64().encode(out)
}

fn open(blob: &str, credential_id: &str, secret: &[u8; 32]) -> StorageResult<Credentials> {
    let bad =
        || StorageError::Other("this passkey does not open the stored key (it may belong to an older setup)".into());
    let raw = b64().decode(blob.trim()).map_err(|_| bad())?;
    if raw.len() < 24 {
        return Err(bad());
    }
    let (nonce, ct) = raw.split_at(24);
    let plain = sealer(secret)
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: credential_id.as_bytes() })
        .map_err(|_| bad())?;
    Credentials::from_recovery_key(&String::from_utf8(plain).map_err(|_| bad())?)
}

fn random_state() -> String {
    let mut b = [0u8; 24];
    getrandom::getrandom(&mut b).expect("random");
    b64().encode(b)
}

fn query_escape(v: &str) -> String {
    let mut s = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~') {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Open the page in the browser (through `open_url`) and wait for its answer on a one-off local address.
fn ceremony(
    endpoint: &str,
    mode: &str,
    account: Option<&str>,
    label: &str,
    open_url: &dyn Fn(&str) -> std::io::Result<()>,
    ctx: &OpCtx,
) -> StorageResult<Answer> {
    let endpoint = check_endpoint(endpoint)?;
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| StorageError::Other(format!("could not listen for the browser's answer: {e}")))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let state = random_state();
    let cb = format!("http://127.0.0.1:{port}/passkey");
    let mut url =
        format!("{endpoint}/passkey#mode={mode}&cb={}&state={state}&label={}", query_escape(&cb), query_escape(label));
    if let Some(a) = account {
        url.push_str(&format!("&user={}", query_escape(a)));
    }
    open_url(&url).map_err(|e| StorageError::Other(format!("could not open the browser: {e}")))?;

    #[derive(Deserialize)]
    struct Reply {
        state: String,
        #[serde(default)]
        credential_id: Option<String>,
        #[serde(default)]
        prf: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }
    let cors = |r: tiny_http::Response<std::io::Cursor<Vec<u8>>>| {
        [
            ("Access-Control-Allow-Origin", endpoint.as_str()),
            ("Access-Control-Allow-Methods", "POST"),
            ("Access-Control-Allow-Headers", "content-type"),
            // Chrome asks before a public page may talk to this computer
            ("Access-Control-Allow-Private-Network", "true"),
        ]
        .into_iter()
        .fold(r, |r, (k, v)| {
            r.with_header(tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header"))
        })
    };
    let deadline = Instant::now() + WAIT;
    loop {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if Instant::now() > deadline {
            return Err(StorageError::Other("no passkey was used within 5 minutes; try again".into()));
        }
        let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(250)) else { continue };
        if req.url() != "/passkey" {
            let _ = req.respond(tiny_http::Response::from_string("not found").with_status_code(404));
            continue;
        }
        if req.method() == &tiny_http::Method::Options {
            let _ = req.respond(cors(tiny_http::Response::from_data(Vec::new()).with_status_code(204)));
            continue;
        }
        if req.method() != &tiny_http::Method::Post {
            let _ = req.respond(cors(tiny_http::Response::from_data(b"{}".to_vec()).with_status_code(405)));
            continue;
        }
        let is_json = req.headers().iter().any(|h| {
            h.field.equiv("Content-Type")
                && h.value.as_str().split(';').next().is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
        });
        let origin_ok = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Origin"))
            .is_none_or(|h| h.value.as_str().trim_end_matches('/').eq_ignore_ascii_case(&endpoint));
        if !is_json || !origin_ok {
            let _ = req.respond(cors(tiny_http::Response::from_data(b"{}".to_vec()).with_status_code(403)));
            continue;
        }
        let mut body = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::Read::take(req.as_reader(), 16 * 1024), &mut body);
        let reply: Option<Reply> = serde_json::from_str(&body).ok();
        let Some(reply) = reply.filter(|r| ct_eq(r.state.as_bytes(), state.as_bytes())) else {
            // not from the page this program opened: ignore it
            let _ = req.respond(cors(tiny_http::Response::from_data(b"{}".to_vec()).with_status_code(400)));
            continue;
        };
        let _ = req.respond(cors(tiny_http::Response::from_data(b"{\"ok\":true}".to_vec())));
        if reply.error.as_deref() == Some("unsupported") {
            return Err(StorageError::Other(
                "this passkey cannot protect encrypted data (it lacks the PRF feature). Try another passkey manager \
                 (Google Password Manager, iCloud Keychain, Bitwarden, 1Password, a recent Windows Hello), or keep a recovery key"
                    .into(),
            ));
        }
        let (Some(id), Some(prf)) = (reply.credential_id, reply.prf) else {
            return Err(StorageError::Other("the browser's answer was incomplete; try again".into()));
        };
        if !valid_credential_id(&id) {
            return Err(StorageError::Other("the passkey returned an invalid credential ID".into()));
        }
        let secret: [u8; 32] = b64()
            .decode(prf.trim())
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| StorageError::Other("the passkey's answer was not valid".into()))?;
        return Ok(Answer { credential_id: id, secret });
    }
}

/// Protect the account with a new passkey: create it in the browser and store the sealed blob.
pub fn add_passkey(
    hosted: &HostedStorage,
    creds: &Credentials,
    label: &str,
    open_url: &dyn Fn(&str) -> std::io::Result<()>,
    ctx: &OpCtx,
) -> StorageResult<()> {
    let ans = ceremony(&creds.account.endpoint, "create", Some(&creds.account.account), label, open_url, ctx)?;
    let blob = seal(creds, &ans.credential_id, &ans.secret);
    let body = serde_json::json!({ "blob": blob, "label": label }).to_string();
    let r = hosted.http().call(
        ctx,
        "saving the passkey",
        "PUT",
        &format!("/v1/passkeys/{}", ans.credential_id),
        &[hosted.auth(), ("content-type", "application/json".into())],
        Some(body.as_bytes()),
        64 * 1024,
    )?;
    match r.status {
        200 | 201 => Ok(()),
        _ => Err(classify(&r, "saving the passkey")),
    }
}

/// Get back in with a passkey: use it in the browser, fetch the sealed blob, open it here.
pub fn recover_with_passkey(
    endpoint: &str,
    open_url: &dyn Fn(&str) -> std::io::Result<()>,
    ctx: &OpCtx,
) -> StorageResult<Credentials> {
    let endpoint = check_endpoint(endpoint)?;
    let http = Http::new(&endpoint)?;
    let ans = ceremony(&endpoint, "get", None, "", open_url, ctx)?;
    let r = http.call(
        ctx,
        "fetching the passkey's key",
        "GET",
        &format!("/v1/passkeys/{}", ans.credential_id),
        &[],
        None,
        64 * 1024,
    )?;
    if r.status == 404 {
        return Err(StorageError::Other("this passkey is not registered with PackSync (or was removed)".into()));
    }
    if r.status != 200 {
        return Err(classify(&r, "fetching the passkey's key"));
    }
    #[derive(Deserialize)]
    struct B {
        blob: String,
    }
    let b: B = serde_json::from_slice(&r.body).map_err(|e| StorageError::Other(e.to_string()))?;
    let mut creds = open(&b.blob, &ans.credential_id, &ans.secret)?;
    // the service the blob names may be older than the one this program uses: keep the one in use
    creds.account.endpoint = endpoint;
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountCredentials, MasterKey};
    use packsync_core::redact::Secret;

    #[test]
    fn a_blob_opens_only_with_its_passkeys_secret_and_id() {
        let c = Credentials {
            account: AccountCredentials {
                endpoint: "https://packsync.example".into(),
                account: "abcdefghijklmnopqrstuvwxyz".into(),
                token: Secret::new("t".repeat(43)),
            },
            key: MasterKey::from_bytes([3; 32]),
        };
        let blob = seal(&c, "cred-1", &[7; 32]);
        let back = open(&blob, "cred-1", &[7; 32]).unwrap();
        assert_eq!(back.key.as_bytes(), c.key.as_bytes());
        assert_eq!(back.account.account, c.account.account);
        assert!(open(&blob, "cred-1", &[8; 32]).is_err(), "another passkey's secret");
        assert!(open(&blob, "cred-2", &[7; 32]).is_err(), "moved to another passkey's ID");
    }

    #[test]
    fn recover_rejects_bad_endpoint_before_opening_browser() {
        let called = std::sync::atomic::AtomicBool::new(false);
        let err = recover_with_passkey(
            "javascript:alert(1)",
            &|_| {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            &OpCtx::detached(),
        )
        .unwrap_err();
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(err.to_string().contains("https://"));
        assert!(
            recover_with_passkey("http://evil.example.com", &|_| panic!("must not open"), &OpCtx::detached()).is_err()
        );
        assert!(!valid_credential_id("../accounts"));
        assert!(valid_credential_id("cred-AAAAAAAAAAAAAAAA"));
    }
}
