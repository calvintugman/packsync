//! PackSync's hosted storage: end-to-end encryption over the PackSync cloud service.
//!
//! * Sign in with a Microsoft / Minecraft account ([`msauth`]) to receive an account, a per-device
//!   session token, and the account's server-wrapped encryption key. Every device signed in to the
//!   same Microsoft account receives the same account ID and key automatically.
//! * Everything is encrypted on the computer before upload ([`crypto`]); the service stores bytes it
//!   cannot read.
//! * Anonymous accounts ([`create_account`]) and pairing ([`pairing`]) remain available when enabled
//!   on a self-hosted service.
//!
//! The service is a Cloudflare Worker in `services/hosted`; [`mock`] is an in-process stand-in for tests.

pub mod client;
pub mod crypto;
#[cfg(feature = "mock")]
pub mod mock;
pub mod msauth;
pub mod pairing;

use std::sync::Arc;

use base64::Engine as _;
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::storage::{Storage, StorageError, StorageResult};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub use client::{AccountCredentials, HostedStats, HostedStorage};
pub use crypto::{EncryptedStorage, MasterKey};

/// The service built into this program (set when building a release: `PACKSYNC_HOSTED_URL`), unless
/// the environment variable of the same name overrides it at run time.
pub fn default_endpoint() -> Option<String> {
    std::env::var("PACKSYNC_HOSTED_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| option_env!("PACKSYNC_HOSTED_URL").map(str::to_string))
}

pub(crate) fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Everything a computer needs to use an account: where, who, and the encryption key.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub account: AccountCredentials,
    pub key: MasterKey,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    e: String,
    a: String,
    t: String,
    k: String,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.e.zeroize();
        self.a.zeroize();
        self.t.zeroize();
        self.k.zeroize();
    }
}

const RECOVERY_PREFIX: &str = "packsync1-";

impl Credentials {
    /// The secret part as one string for the credential store (token and key).
    pub fn secret(&self) -> Secret {
        Secret::new(format!("{}.{}", self.account.token.expose(), b64().encode(self.key.as_bytes())))
    }

    /// Rebuild from the settings (endpoint, account) and [`Credentials::secret`].
    pub fn from_secret(endpoint: &str, account: &str, secret: &Secret) -> StorageResult<Credentials> {
        let bad = || {
            StorageError::Other("the saved PackSync service credentials are damaged; pair this computer again".into())
        };
        let (token, key) = secret.expose().split_once('.').ok_or_else(bad)?;
        let key: [u8; 32] = b64().decode(key).ok().and_then(|k| k.try_into().ok()).ok_or_else(bad)?;
        Ok(Credentials {
            account: AccountCredentials {
                endpoint: endpoint.to_string(),
                account: account.to_string(),
                token: Secret::new(token.to_string()),
            },
            key: MasterKey::from_bytes(key),
        })
    }

    /// A text that restores access on any computer without pairing. Whoever has it can read and
    /// delete the data: keep it like a password (a password manager, or printed).
    pub fn recovery_key(&self) -> Secret {
        let s = Stored {
            e: self.account.endpoint.clone(),
            a: self.account.account.clone(),
            t: self.account.token.expose().to_string(),
            k: b64().encode(self.key.as_bytes()),
        };
        Secret::new(format!("{RECOVERY_PREFIX}{}", b64().encode(serde_json::to_vec(&s).expect("serialisable"))))
    }

    pub fn from_recovery_key(text: &str) -> StorageResult<Credentials> {
        let bad = || StorageError::Other("that is not a PackSync recovery key (it starts with packsync1-)".into());
        let body: String = text.split_whitespace().collect();
        let raw = body.strip_prefix(RECOVERY_PREFIX).ok_or_else(bad)?;
        let s: Stored = b64().decode(raw).ok().and_then(|j| serde_json::from_slice(&j).ok()).ok_or_else(bad)?;
        let endpoint = client::check_endpoint(&s.e)?;
        let key: [u8; 32] = b64().decode(&s.k).ok().and_then(|k| k.try_into().ok()).ok_or_else(bad)?;
        Ok(Credentials {
            account: AccountCredentials { endpoint, account: s.a.clone(), token: Secret::new(s.t.clone()) },
            key: MasterKey::from_bytes(key),
        })
    }
}

/// Create an account on the service at `endpoint`, with a fresh encryption key.
pub fn create_account(endpoint: &str, ctx: &OpCtx) -> StorageResult<Credentials> {
    let account = client::Http::new(endpoint)?.register(ctx)?;
    Ok(Credentials { account, key: MasterKey::generate() })
}

/// The account's storage: the service client wrapped in end-to-end encryption.
pub fn open_storage(creds: &Credentials) -> StorageResult<(Arc<dyn Storage>, Arc<HostedStorage>)> {
    let hosted = Arc::new(HostedStorage::new(creds.account.clone())?);
    let enc: Arc<dyn Storage> = Arc::new(EncryptedStorage::new(hosted.clone(), &creds.key));
    Ok((enc, hosted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_keys_and_stored_secrets_round_trip() {
        let c = Credentials {
            account: AccountCredentials {
                endpoint: "https://packsync.example.workers.dev".into(),
                account: "abcdefghijklmnopqrstuvwxyz".into(),
                token: Secret::new("T".repeat(43)),
            },
            key: MasterKey::from_bytes([9; 32]),
        };
        let r = c.recovery_key();
        assert!(r.expose().starts_with("packsync1-"));
        let back = Credentials::from_recovery_key(&format!("  {}\n", r.expose())).unwrap();
        assert_eq!(back.account.account, c.account.account);
        assert_eq!(back.account.endpoint, c.account.endpoint);
        assert_eq!(back.account.token.expose(), c.account.token.expose());
        assert_eq!(back.key.as_bytes(), c.key.as_bytes());
        let s = Credentials::from_secret(&c.account.endpoint, &c.account.account, &c.secret()).unwrap();
        assert_eq!(s.key.as_bytes(), c.key.as_bytes());
        assert!(Credentials::from_recovery_key("packsync1-nonsense").is_err());
        assert!(!format!("{c:?}").contains("TTTT"), "credentials never print secrets");

        // Recovery keys pointing at cleartext non-loopback http:// or non-http schemes are refused.
        let bad_payload = serde_json::json!({
            "e": "http://evil.example.com",
            "a": "abcdefghijklmnopqrstuvwxyz",
            "t": "T".repeat(43),
            "k": b64().encode([9; 32]),
        });
        let bad_key = format!("packsync1-{}", b64().encode(bad_payload.to_string().as_bytes()));
        assert!(Credentials::from_recovery_key(&bad_key).is_err());
    }
}
