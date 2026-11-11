//! Bringing another computer into an account with a short code, like `4821-K7QM-X2PD`.
//!
//! The first four digits name a mailbox on the service (the *nameplate*); the rest is a secret that
//! never leaves the two computers. Both run SPAKE2, a password-authenticated key exchange, through the
//! mailbox: they end up with the same strong key exactly when both used the same code, and the service
//! (or anyone watching) learns nothing it could use to guess the code offline. The computer that has
//! the account then sends the account's credentials and encryption key, sealed with that key. A
//! mailbox accepts one joiner only, so a guessed code gets exactly one try (40 bits of secret), and
//! expires after 10 minutes.
//!
//! Mailbox slots: `a` the owner's SPAKE2 message, `b` the joiner's, `c` the sealed credentials, `d` the
//! joiner's sealed confirmation.

use std::time::{Duration, Instant};

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::storage::{StorageError, StorageResult};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::client::{AccountCredentials, HostedStorage, Http, classify};
use crate::crypto::MasterKey;
use crate::{Credentials, b64};

/// Crockford's base32 (no I, L, O, U): easy to read aloud and to type.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const IDENTITY: &[u8] = b"packsync pairing v1";
const POLL: Duration = Duration::from_millis(1000);
const LIFETIME: Duration = Duration::from_secs(10 * 60);

/// A pairing code: nameplate (4 digits) and secret (8 characters, 40 bits).
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode {
    pub nameplate: String,
    secret: String,
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PairingCode({}-<secret>)", self.nameplate)
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}-{}", self.nameplate, &self.secret[..4], &self.secret[4..])
    }
}

impl PairingCode {
    fn new(nameplate: String) -> PairingCode {
        let mut b = [0u8; 8];
        getrandom::getrandom(&mut b).expect("the operating system's random number generator");
        let secret = b.iter().map(|x| ALPHABET[(x % 32) as usize] as char).collect();
        PairingCode { nameplate, secret }
    }

    /// Accepts what a person types: any case, spaces or dashes anywhere, `O` for zero, `I`/`L` for one.
    pub fn parse(text: &str) -> Option<PairingCode> {
        let clean: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            })
            .collect();
        if clean.len() != 12 || !clean.is_ascii() {
            return None;
        }
        let (n, s) = clean.split_at(4);
        if !n.bytes().all(|b| b.is_ascii_digit()) || !s.bytes().all(|b| ALPHABET.contains(&b)) {
            return None;
        }
        Some(PairingCode { nameplate: n.to_string(), secret: s.to_string() })
    }

    fn password(&self) -> Vec<u8> {
        format!("{}{}", self.nameplate, self.secret).into_bytes()
    }
}

#[derive(Serialize, Deserialize)]
struct Offer {
    v: u32,
    endpoint: String,
    account: String,
    token: String,
    key: String,
}

fn subkey(shared: &[u8], purpose: &str) -> XChaCha20Poly1305 {
    let k = blake3::derive_key(purpose, shared);
    XChaCha20Poly1305::new(&k.into())
}

fn seal(shared: &[u8], purpose: &str, msg: &[u8]) -> String {
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut nonce).expect("random nonce");
    let ct = subkey(shared, purpose)
        .encrypt(XNonce::from_slice(&nonce), Payload { msg, aad: purpose.as_bytes() })
        .expect("in-memory encryption");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    b64().encode(out)
}

fn open(shared: &[u8], purpose: &str, text: &str) -> Option<Vec<u8>> {
    let raw = b64().decode(text.trim()).ok()?;
    if raw.len() < 24 {
        return None;
    }
    let (nonce, ct) = raw.split_at(24);
    subkey(shared, purpose).decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: purpose.as_bytes() }).ok()
}

fn slot_path(nameplate: &str, slot: char) -> String {
    format!("/v1/pairings/{nameplate}/{slot}")
}

fn write_slot(
    http: &Http,
    auth: Option<(&'static str, String)>,
    n: &str,
    slot: char,
    body: &str,
    ctx: &OpCtx,
) -> StorageResult<()> {
    let headers: Vec<(&str, String)> = auth.into_iter().chain([("content-type", "text/plain".to_string())]).collect();
    let r = http.call(ctx, "pairing", "PUT", &slot_path(n, slot), &headers, Some(body.as_bytes()), 64 * 1024)?;
    match r.status {
        201 | 200 => Ok(()),
        409 => Err(StorageError::Other(
            "this pairing code was already used by another computer; make a new code and try again".into(),
        )),
        404 => Err(StorageError::Other("this pairing code has expired; make a new one".into())),
        _ => Err(classify(&r, "pairing")),
    }
}

/// Wait for a slot to be written; `None` when `deadline` passes.
fn read_slot(http: &Http, n: &str, slot: char, deadline: Instant, ctx: &OpCtx) -> StorageResult<Option<String>> {
    loop {
        if ctx.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let r = http.call(ctx, "pairing", "GET", &slot_path(n, slot), &[], None, 64 * 1024)?;
        match r.status {
            200 => return Ok(Some(String::from_utf8_lossy(&r.body).into_owned())),
            202 | 204 => {}
            404 => {
                return Err(StorageError::Other(
                    "this pairing code has expired or is not known; check it, or make a new one".into(),
                ));
            }
            _ => return Err(classify(&r, "pairing")),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

/// The computer that has the account: a code is shown, then [`PairingOffer::wait`] runs until another
/// computer joined with it (or it expired).
pub struct PairingOffer {
    storage: std::sync::Arc<HostedStorage>,
    creds: Credentials,
    code: PairingCode,
    state: Option<Spake2<Ed25519Group>>,
    expires: Instant,
}

impl PairingOffer {
    pub fn start(
        storage: std::sync::Arc<HostedStorage>,
        creds: &Credentials,
        ctx: &OpCtx,
    ) -> StorageResult<PairingOffer> {
        #[derive(Deserialize)]
        struct Created {
            nameplate: String,
        }
        let http = storage.http();
        let r = http.call(ctx, "making a pairing code", "POST", "/v1/pairings", &[storage.auth()], None, 64 * 1024)?;
        if r.status != 201 {
            return Err(classify(&r, "making a pairing code"));
        }
        let c: Created = serde_json::from_slice(&r.body).map_err(|e| StorageError::Other(e.to_string()))?;
        let code = PairingCode::new(c.nameplate);
        let (state, msg) =
            Spake2::<Ed25519Group>::start_symmetric(&Password::new(code.password()), &Identity::new(IDENTITY));
        write_slot(http, Some(storage.auth()), &code.nameplate, 'a', &b64().encode(msg), ctx)?;
        Ok(PairingOffer { storage, creds: creds.clone(), code, state: Some(state), expires: Instant::now() + LIFETIME })
    }

    /// The code to show (type it on the other computer).
    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    /// Wait until the other computer joined. Errors when the code expired, was used with a wrong
    /// secret, or was taken by someone else.
    pub fn wait(mut self, ctx: &OpCtx) -> StorageResult<()> {
        let http = self.storage.http();
        let n = self.code.nameplate.clone();
        let result = (|| {
            let Some(b) = read_slot(http, &n, 'b', self.expires, ctx)? else {
                return Err(StorageError::Other("nobody joined with the code in 10 minutes; make a new one".into()));
            };
            let theirs =
                b64().decode(b.trim()).map_err(|_| StorageError::Other("the other computer sent garbage".into()))?;
            let shared = self
                .state
                .take()
                .expect("wait runs once")
                .finish(&theirs)
                .map_err(|_| StorageError::Other("the other computer's message was not valid".into()))?;
            let offer = Offer {
                v: 1,
                endpoint: self.creds.account.endpoint.clone(),
                account: self.creds.account.account.clone(),
                token: self.creds.account.token.expose().to_string(),
                key: b64().encode(self.creds.key.as_bytes()),
            };
            let body = serde_json::to_vec(&offer).expect("serialisable");
            write_slot(http, Some(self.storage.auth()), &n, 'c', &seal(&shared, "packsync pairing offer", &body), ctx)?;
            let Some(d) = read_slot(http, &n, 'd', self.expires, ctx)? else {
                return Err(StorageError::Other("the other computer did not finish pairing in time".into()));
            };
            match open(&shared, "packsync pairing joined", &d) {
                Some(m) if m == b"joined" => Ok(()),
                _ => Err(StorageError::Other(
                    "the code typed on the other computer was wrong; nothing was shared. Make a new code and try again"
                        .into(),
                )),
            }
        })();
        let _ =
            http.call(ctx, "pairing", "DELETE", &format!("/v1/pairings/{n}"), &[self.storage.auth()], None, 64 * 1024);
        result
    }
}

/// The joining computer: exchange the code for the account's credentials and key.
pub fn join(endpoint: &str, code: &str, ctx: &OpCtx) -> StorageResult<Credentials> {
    let code = PairingCode::parse(code).ok_or_else(|| {
        StorageError::Other("that is not a pairing code; it looks like 4821-K7QM-X2PD (12 digits and letters)".into())
    })?;
    let http = Http::new(endpoint)?;
    let n = code.nameplate.clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    let Some(a) = read_slot(&http, &n, 'a', deadline, ctx)? else {
        return Err(StorageError::Other("the other computer is not ready; check the code and try again".into()));
    };
    let theirs = b64().decode(a.trim()).map_err(|_| StorageError::Other("the other computer sent garbage".into()))?;
    let (state, msg) =
        Spake2::<Ed25519Group>::start_symmetric(&Password::new(code.password()), &Identity::new(IDENTITY));
    write_slot(&http, None, &n, 'b', &b64().encode(msg), ctx)?;
    let shared =
        state.finish(&theirs).map_err(|_| StorageError::Other("the other computer's message was not valid".into()))?;
    let Some(c) = read_slot(&http, &n, 'c', Instant::now() + LIFETIME, ctx)? else {
        return Err(StorageError::Other("the other computer did not answer in time".into()));
    };
    let Some(plain) = open(&shared, "packsync pairing offer", &c) else {
        // tell the other side at once instead of letting it wait for the expiry
        let _ = write_slot(&http, None, &n, 'd', &b64().encode(b"wrong code"), ctx);
        return Err(StorageError::Other(
            "the code does not match the one shown on the other computer; check it and try again with a new code"
                .into(),
        ));
    };
    let offer: Offer =
        serde_json::from_slice(&plain).map_err(|e| StorageError::Other(format!("unexpected pairing offer: {e}")))?;
    let key: [u8; 32] = b64()
        .decode(&offer.key)
        .ok()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| StorageError::Other("the pairing offer carried no valid key".into()))?;
    write_slot(&http, None, &n, 'd', &seal(&shared, "packsync pairing joined", b"joined"), ctx)?;
    Ok(Credentials {
        account: AccountCredentials {
            endpoint: offer.endpoint,
            account: offer.account,
            token: Secret::new(offer.token),
        },
        key: MasterKey::from_bytes(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_print_and_parse_forgivingly() {
        let c = PairingCode::new("4821".into());
        let shown = c.to_string();
        assert_eq!(shown.len(), 14);
        assert_eq!(PairingCode::parse(&shown), Some(c.clone()));
        assert_eq!(PairingCode::parse(&shown.to_lowercase().replace('-', " ")), Some(c));
        let typed = PairingCode::parse("4821 k7qm x2pd").unwrap();
        assert_eq!(typed.to_string(), "4821-K7QM-X2PD");
        assert_eq!(PairingCode::parse("4821-K7QM-X2PO").unwrap().to_string(), "4821-K7QM-X2P0", "O is zero");
        assert!(PairingCode::parse("4821-K7QM").is_none());
        assert!(PairingCode::parse("48A1-K7QM-X2PD").is_none());
        assert!(PairingCode::parse("4821-K7QM-X2PU").is_none(), "U is not in the alphabet");
    }
}
