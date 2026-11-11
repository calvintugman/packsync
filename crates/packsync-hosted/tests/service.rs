//! The hosted storage against the in-process service, and — when `PACKSYNC_TEST_HOSTED_URL` names a
//! running Worker (`npm run dev` in `services/hosted`) — against the real service code.

use std::sync::Arc;

use packsync_core::progress::OpCtx;
use packsync_core::storage::*;
use packsync_core::testkit::conformance::{ConformanceOptions, assert_conformance};
use packsync_hosted::client::HostedStorage;
use packsync_hosted::mock::MockService;
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
    wrong_code(m.url());
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
