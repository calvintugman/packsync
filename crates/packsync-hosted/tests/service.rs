//! The hosted storage against the in-process service, and — when `PACKSYNC_TEST_HOSTED_URL` names a
//! running Worker (`npm run dev` in `services/hosted`) — against the real service code.

use std::sync::Arc;

use packsync_core::progress::OpCtx;
use packsync_core::storage::*;
use packsync_core::testkit::conformance::{ConformanceOptions, assert_conformance};
use packsync_hosted::client::HostedStorage;
use packsync_hosted::mock::MockService;
use packsync_hosted::msauth::start_device_login;
use packsync_hosted::pairing::{PairingOffer, join};
use packsync_hosted::{Credentials, EncryptedStorage, MasterKey, create_account, open_storage};

fn opts() -> ConformanceOptions {
    ConformanceOptions {
        list_count: 60,
        large_bytes: Some(12 * 1024 * 1024),
        writers: 8,
        list_wait: std::time::Duration::ZERO,
    }
}

fn real_service() -> Option<String> {
    std::env::var("PACKSYNC_TEST_HOSTED_URL").ok().filter(|s| !s.is_empty())
}

#[test]
fn the_service_client_conforms() {
    let m = MockService::start_with(7); // small pages: paging is exercised
    let c = create_account(m.url(), &OpCtx::detached()).unwrap();
    assert_conformance(&HostedStorage::new(c.account).unwrap(), opts());
}

#[test]
fn encrypted_storage_conforms_over_the_service_and_over_a_folder() {
    let m = MockService::start();
    let c = create_account(m.url(), &OpCtx::detached()).unwrap();
    let (st, _) = open_storage(&c).unwrap();
    assert_conformance(&*st, opts());

    let dir = tempfile::tempdir().unwrap();
    let local: Arc<dyn Storage> = Arc::new(packsync_core::storage_local::LocalDirStorage::new(dir.path()).unwrap());
    assert_conformance(&EncryptedStorage::new(local, &MasterKey::generate()), opts());
}

#[test]
fn the_service_sees_only_ciphertext_and_another_key_reads_nothing() {
    let m = MockService::start();
    let ctx = OpCtx::detached();
    let c = create_account(m.url(), &ctx).unwrap();
    let (st, _) = open_storage(&c).unwrap();
    let secret = b"level.dat of my world: diamonds at 120 64 -300 ".repeat(5000);
    st.put("packs/p1.pack", PutSource::Bytes(&secret), &PutOptions::create_only(None), &ctx).unwrap();
    let raw = m.raw_object(&c.account.account, "packs/p1.pack").unwrap();
    assert!(raw.len() > secret.len());
    assert!(!raw.windows(20).any(|w| w == &secret[..20]), "plaintext is never stored");
    // ranges inside, across and at the end of segments
    for (s, l) in [(0u64, 10u64), (65_530, 20), (100_000, 135_000), (secret.len() as u64 - 7, 7)] {
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut st.get("packs/p1.pack", Some((s, l)), &ctx).unwrap(), &mut got).unwrap();
        assert_eq!(got, &secret[s as usize..(s + l) as usize], "range {s}+{l}");
    }
    // the same account with the wrong key: nothing readable
    let wrong = Credentials { account: c.account.clone(), key: MasterKey::generate() };
    let (other, _) = open_storage(&wrong).unwrap();
    assert!(matches!(other.get_bytes("packs/p1.pack", 1 << 30, &ctx), Err(StorageError::Corrupt(_))));
}

#[test]
fn a_full_account_reports_quota_exceeded_and_usage_is_counted() {
    let m = MockService::start();
    m.set_quota(200_000);
    let ctx = OpCtx::detached();
    let c = create_account(m.url(), &ctx).unwrap();
    let (st, hosted) = open_storage(&c).unwrap();
    st.put("a/1", PutSource::Bytes(&[1u8; 150_000]), &PutOptions::create_only(None), &ctx).unwrap();
    let used = hosted.usage(&ctx).unwrap().used;
    assert!(used > 150_000 && used < 151_000, "{used}");
    match st.put("a/2", PutSource::Bytes(&[2u8; 100_000]), &PutOptions::create_only(None), &ctx) {
        Err(StorageError::QuotaExceeded(m)) => assert!(m.contains("full"), "{m}"),
        other => panic!("{other:?}"),
    }
    st.delete("a/1", DeleteMode::Permanent, &ctx).unwrap();
    assert_eq!(hosted.usage(&ctx).unwrap().used, 0);
}

fn pair(endpoint: &str) {
    let ctx = OpCtx::detached();
    let owner = create_account(endpoint, &ctx).unwrap();
    let (st, hosted) = open_storage(&owner).unwrap();
    st.put("instances/x/HEAD.json", PutSource::Bytes(b"{\"v\":1}"), &PutOptions::if_version(None), &ctx).unwrap();

    let offer = PairingOffer::start(hosted.clone(), &owner, &ctx).unwrap();
    let code = offer.code().to_string();
    let waiter = std::thread::spawn(move || offer.wait(&OpCtx::detached()));
    let joined = join(endpoint, &code.to_lowercase(), &ctx).unwrap();
    waiter.join().unwrap().unwrap();
    assert_eq!(joined.account.account, owner.account.account);
    assert_eq!(joined.key.as_bytes(), owner.key.as_bytes());
    let (st2, _) = open_storage(&joined).unwrap();
    assert_eq!(st2.get_bytes("instances/x/HEAD.json", 1000, &ctx).unwrap(), b"{\"v\":1}");
    // a used code cannot be used again
    let again = join(endpoint, &code, &ctx).unwrap_err().to_string();
    assert!(again.contains("expired") || again.contains("already used"), "{again}");
    hosted.delete_account(&ctx).unwrap();
}

fn wrong_code(endpoint: &str) {
    let ctx = OpCtx::detached();
    let owner = create_account(endpoint, &ctx).unwrap();
    let (_, hosted) = open_storage(&owner).unwrap();
    let offer = PairingOffer::start(hosted.clone(), &owner, &ctx).unwrap();
    let code = offer.code().to_string();
    let waiter = std::thread::spawn(move || offer.wait(&OpCtx::detached()));
    // right mailbox, wrong secret: both sides fail and nothing is shared
    let last = code.chars().last().unwrap();
    let typo = format!("{}{}", &code[..code.len() - 1], if last == '0' { '1' } else { '0' });
    let e = join(endpoint, &typo, &ctx).unwrap_err().to_string();
    assert!(e.contains("does not match"), "{e}");
    let w = waiter.join().unwrap().unwrap_err().to_string();
    assert!(w.contains("wrong"), "{w}");
    // and the mailbox is spent: the right code no longer works either
    assert!(join(endpoint, &code, &ctx).is_err());
    hosted.delete_account(&ctx).unwrap();
}

#[test]
fn pairing_shares_the_account_and_a_wrong_code_shares_nothing() {
    let m = MockService::start();
    pair(m.url());
    let before = m.pairing_offer_writes.load(std::sync::atomic::Ordering::SeqCst);
    wrong_code(m.url());
    assert_eq!(
        m.pairing_offer_writes.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "key confirmation in slot b must reject a wrong code before slot c is ever written"
    );
}

#[test]
fn microsoft_login_shares_account_across_pcs_isolates_users_and_cleans_up_on_delete() {
    let m = MockService::start();
    let ctx = OpCtx::detached();

    // User 1 signs in on PC #1.
    m.set_mock_microsoft_user("ms-user-alpha", "AlphaCrafter");
    let pending1 = start_device_login(m.url(), &ctx).unwrap();
    assert!(!pending1.prompt().user_code.is_empty());
    assert_eq!(pending1.prompt().privacy_url, format!("{}/privacy", m.url()));
    let pc1 = pending1.wait(&ctx).unwrap();
    assert!(pc1.created);
    assert_eq!(pc1.display_name.as_deref(), Some("AlphaCrafter"));
    let (st1, hosted1) = open_storage(&pc1.credentials).unwrap();
    st1.put("instances/x/HEAD.json", PutSource::Bytes(b"{\"v\":10}"), &PutOptions::if_version(None), &ctx).unwrap();

    // User 1 signs in on PC #2: receives the same account ID and master_key, with a distinct session token.
    let pending2 = start_device_login(m.url(), &ctx).unwrap();
    let pc2 = pending2.wait(&ctx).unwrap();
    assert!(!pc2.created);
    assert_eq!(pc2.credentials.account.account, pc1.credentials.account.account);
    assert_eq!(pc2.credentials.key.as_bytes(), pc1.credentials.key.as_bytes());
    assert_ne!(pc2.credentials.account.token.expose(), pc1.credentials.account.token.expose());
    let (st2, hosted2) = open_storage(&pc2.credentials).unwrap();
    assert_eq!(st2.get_bytes("instances/x/HEAD.json", 1000, &ctx).unwrap(), b"{\"v\":10}");

    // User 2 signs in: gets an isolated account and distinct master_key, and cannot read User 1's objects.
    m.set_mock_microsoft_user("ms-user-beta", "BetaBuilder");
    let user2 = start_device_login(m.url(), &ctx).unwrap().wait(&ctx).unwrap();
    assert!(user2.created);
    assert_eq!(user2.display_name.as_deref(), Some("BetaBuilder"));
    assert_ne!(user2.credentials.account.account, pc1.credentials.account.account);
    assert_ne!(user2.credentials.key.as_bytes(), pc1.credentials.key.as_bytes());
    let (st_u2, hosted_u2) = open_storage(&user2.credentials).unwrap();
    assert!(st_u2.list("instances/", &ctx).unwrap().is_empty());
    assert!(st_u2.get_bytes("instances/x/HEAD.json", 1000, &ctx).is_err());

    // User 1 deletes their account from PC #1: all sessions (PC #1 and PC #2) stop working immediately,
    // while User 2's account remains unaffected.
    hosted1.delete_account(&ctx).unwrap();
    assert!(matches!(hosted1.usage(&ctx), Err(StorageError::Auth(_))));
    assert!(matches!(hosted2.usage(&ctx), Err(StorageError::Auth(_))));
    assert!(st2.get_bytes("instances/x/HEAD.json", 1000, &ctx).is_err());
    assert_eq!(hosted_u2.usage(&ctx).unwrap().used, 0);

    // Re-signing in as User 1 creates a brand new account and master_key.
    m.set_mock_microsoft_user("ms-user-alpha", "AlphaCrafter");
    let fresh = start_device_login(m.url(), &ctx).unwrap().wait(&ctx).unwrap();
    assert!(fresh.created);
    assert_ne!(fresh.credentials.account.account, pc1.credentials.account.account);
    assert_ne!(fresh.credentials.key.as_bytes(), pc1.credentials.key.as_bytes());
}

#[test]
fn mock_privacy_terms_and_auth_config_errors_behave_as_expected() {
    let m = MockService::start();
    let ctx = OpCtx::detached();
    for path in ["/privacy", "/terms"] {
        let mut resp = ureq::get(&format!("{}{path}", m.url())).call().unwrap();
        assert_eq!(resp.status(), 200);
        let html = resp.body_mut().read_to_string().unwrap();
        assert!(html.contains("PackSync"), "{path}: {html}");
    }

    m.set_mock_auth_configured(false);
    let err = start_device_login(m.url(), &ctx).unwrap_err();
    assert!(err.to_string().contains("not configured"), "{err}");

    m.set_mock_auth_configured(true);
    m.set_mock_token_error(Some("access_denied"));
    let pending = start_device_login(m.url(), &ctx).unwrap();
    let err = pending.wait(&ctx).unwrap_err();
    assert!(matches!(err, StorageError::Auth(_)), "{err:?}");
}

/// The same checks against the Worker (`services/hosted`, `npm run dev`).
#[test]
fn against_the_real_service_when_running() {
    let Some(url) = real_service() else {
        eprintln!("PACKSYNC_TEST_HOSTED_URL not set: skipped");
        return;
    };
    let c = create_account(&url, &OpCtx::detached()).unwrap();
    assert_conformance(&HostedStorage::new(c.account.clone()).unwrap(), opts());
    let (st, hosted) = open_storage(&c).unwrap();
    assert_conformance(&*st, opts());
    hosted.delete_account(&OpCtx::detached()).unwrap();
    pair(&url);
    wrong_code(&url);
}
