//! Error mapping and backoff (RecordingSleeper: nothing really sleeps).

mod common;

use std::time::Duration;

use common::{digest, pattern, read_all};
use packsync_core::progress::OpCtx;
use packsync_core::retry::RetryPolicy;
use packsync_core::storage::{AuthProblem, PutOptions, PutSource, Storage, StorageError};
use packsync_drive::mock::{Endpoint, Fault, FaultRule, MockConfig, When};
use packsync_drive::oauth::{AccessTokenSource, AuthStatus};
use packsync_drive::{DriveEndpoints, DriveSpace, HttpTimeouts};

fn ctx() -> OpCtx {
    OpCtx::detached()
}

/// Create one object so folders exist and an access token is cached.
fn warm(env: &common::Env) {
    env.drive.put("warm/w.bin", PutSource::Bytes(b"w"), &PutOptions::create_only(None), &ctx()).unwrap();
    env.mock.clear_log();
}

#[test]
fn rate_limits_back_off_with_retry_after_as_a_floor() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::too_many_requests(Some(3))).max(2));
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Nth(3), Fault::rate_limit_403(Some(5))));
    assert!(env.drive.list("warm/", &ctx()).unwrap().len() == 1);
    let sleeps = env.sleeps();
    assert_eq!(sleeps.len(), 3, "{sleeps:?}");
    assert!(sleeps[0] >= Duration::from_secs(3) && sleeps[1] >= Duration::from_secs(3), "{sleeps:?}");
    assert!(sleeps[2] >= Duration::from_secs(5), "{sleeps:?}");
    assert!(sleeps.iter().all(|d| *d <= Duration::from_secs(32)), "bounded by the policy cap");
    assert_eq!(env.drive.stats().retries, 3);
}

#[test]
fn server_errors_are_retried_a_bounded_number_of_times() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::server_error(503)));
    let r = env.drive.list("warm/", &ctx());
    assert!(matches!(r, Err(StorageError::Transient(_))), "{r:?}");
    assert_eq!(env.mock.count(Endpoint::FilesList), RetryPolicy::network().max_attempts as usize);
    assert_eq!(env.sleeps().len(), RetryPolicy::network().max_attempts as usize - 1);
    assert!(env.sleeps().iter().all(|d| *d <= Duration::from_secs(32)));
    // Transient 5xx followed by success.
    env.mock.clear_faults();
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::server_error(500)).max(2));
    assert_eq!(env.drive.list("warm/", &ctx()).unwrap().len(), 1);
}

#[test]
fn long_retry_after_is_returned_to_the_caller_instead_of_sleeping() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::too_many_requests(Some(3600))));
    match env.drive.list("warm/", &ctx()) {
        Err(StorageError::RateLimited { retry_after }) => assert_eq!(retry_after, Some(Duration::from_secs(3600))),
        other => panic!("{other:?}"),
    }
    assert!(env.sleeps().is_empty());
}

#[test]
fn storage_quota_exceeded_is_not_retried() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::UploadMultipart, When::Always, Fault::storage_quota_exceeded()));
    let data = pattern(2000, 1);
    let r = env.drive.put("warm/q.bin", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx());
    assert!(matches!(r, Err(StorageError::QuotaExceeded(_))), "{r:?}");
    assert_eq!(env.mock.count(Endpoint::UploadMultipart), 1);
    assert!(env.sleeps().is_empty());
    // The real limit is enforced by the mock too.
    let small = common::setup_with(
        MockConfig { quota_limit: Some(1000), ..MockConfig::default() },
        DriveSpace::VisibleFolder,
        |_| {},
    );
    let r = small.drive.put("a/b.bin", PutSource::Bytes(&data), &PutOptions::create_only(None), &ctx());
    assert!(matches!(r, Err(StorageError::QuotaExceeded(_))), "{r:?}");
    let q = small.drive.quota(&ctx()).unwrap().unwrap();
    assert_eq!(q.total, Some(1000));
}

#[test]
fn permission_errors_map_to_permission() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesGetMedia, When::Always, Fault::app_not_authorized()));
    let r = env.drive.get("warm/w.bin", None, &ctx()).map(|_| ());
    assert!(matches!(r, Err(StorageError::Permission(_))), "{r:?}");
    assert!(env.sleeps().is_empty());
}

#[test]
fn a_401_refreshes_the_token_once_and_succeeds() {
    let env = common::setup();
    warm(&env);
    let refreshes = env.mock.token_requests("refresh_token");
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Nth(1), Fault::unauthorized()));
    assert_eq!(env.drive.list("warm/", &ctx()).unwrap().len(), 1);
    assert_eq!(env.mock.token_requests("refresh_token") - refreshes, 1, "exactly one refresh");
    assert!(env.sleeps().is_empty(), "a 401 is not a backoff case");
    // Expired access tokens are refreshed the same way.
    env.mock.expire_access_tokens();
    assert_eq!(env.drive.list("warm/", &ctx()).unwrap().len(), 1);
    assert_eq!(env.mock.token_requests("refresh_token") - refreshes, 2);
}

#[test]
fn concurrent_401s_share_one_refresh() {
    let env = common::setup();
    warm(&env);
    let refreshes = env.mock.token_requests("refresh_token");
    env.mock.expire_access_tokens();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| env.drive.list("warm/", &ctx()).unwrap());
        }
    });
    let n = env.mock.token_requests("refresh_token") - refreshes;
    assert!(n <= 2, "single-flight refresh, got {n}");
}

#[test]
fn revoked_refresh_token_is_revoked_and_nothing_is_lost() {
    let env = common::setup();
    let data = pattern(3000, 2);
    env.drive
        .put("keep/k.bin", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx())
        .unwrap();
    env.mock.revoke_all_tokens();
    let r = env.drive.stat("keep/k.bin", &ctx());
    assert!(matches!(r, Err(StorageError::Auth(AuthProblem::Revoked))), "{r:?}");
    assert!(env.sleeps().is_empty(), "not retried");
    assert_eq!(env.auth.status(), AuthStatus::Revoked);
    // A second call does not hammer the token endpoint.
    let before = env.mock.token_requests("refresh_token");
    assert!(env.drive.stat("keep/k.bin", &ctx()).is_err());
    assert_eq!(env.mock.token_requests("refresh_token"), before);
    // Nothing in the cloud was touched; reconnecting restores access.
    assert!(env.mock.objects().iter().any(|o| o.name == "k.bin"));
    let rt = env.mock.issue_refresh_token(&[DriveSpace::VisibleFolder.scope()]);
    env.auth
        .store_refresh_token(
            packsync_core::redact::Secret::new(rt),
            vec![DriveSpace::VisibleFolder.scope().into()],
            None,
        )
        .unwrap();
    assert!(matches!(env.auth.status(), AuthStatus::Connected { .. }));
    assert_eq!(read_all(env.drive.get("keep/k.bin", None, &ctx()).unwrap()).unwrap(), data);
}

#[test]
fn insufficient_scope_is_reported() {
    let env = common::setup();
    // A drive.file grant used for the appDataFolder space.
    let appdata = env.new_instance(|c| c.space = DriveSpace::AppData);
    let r = appdata.list("x/", &ctx());
    assert!(matches!(r, Err(StorageError::Auth(AuthProblem::InsufficientScope))), "{r:?}");
}

#[test]
fn dropped_connections_are_retried_then_reported_offline() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::DropConnection));
    let r = env.drive.list("warm/", &ctx());
    assert!(matches!(r, Err(StorageError::Offline(_))), "{r:?}");
    assert_eq!(env.sleeps().len(), 5);
    // Nothing listens on port 1: connection refused is Offline as well.
    let mut cfg = env.cfg.clone();
    cfg.endpoints = DriveEndpoints::custom("http://127.0.0.1:1");
    cfg.retry = RetryPolicy { max_attempts: 2, ..RetryPolicy::network() };
    let dead =
        packsync_drive::DriveStorage::with_sleeper(cfg, env.auth.clone(), env.state.clone(), env.sleeper.clone())
            .unwrap();
    assert!(matches!(dead.list("a/", &ctx()), Err(StorageError::Offline(_))));
}

#[test]
fn truncated_downloads_resume_from_the_received_offset() {
    let env = common::setup();
    let data = pattern(300_000, 3);
    env.drive.put("d/t.bin", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx()).unwrap();
    env.mock.clear_log();
    env.mock.add_fault(FaultRule::new(Endpoint::FilesGetMedia, When::Nth(1), Fault::TruncateResponse { keep: 70_000 }));
    let got = read_all(env.drive.get("d/t.bin", None, &ctx()).unwrap()).unwrap();
    assert_eq!(got, data);
    assert_eq!(env.mock.count(Endpoint::FilesGetMedia), 2, "one resumed range request");
    // A range read is resumed as well and still returns exactly the requested bytes.
    env.mock.add_fault(FaultRule::new(Endpoint::FilesGetMedia, When::Nth(3), Fault::TruncateResponse { keep: 10 }));
    let part = read_all(env.drive.get("d/t.bin", Some((1000, 50_000)), &ctx()).unwrap()).unwrap();
    assert_eq!(part, data[1000..51_000]);
}

#[test]
fn whole_object_reads_verify_the_md5() {
    let env = common::setup();
    let data = pattern(5000, 4);
    let info = env
        .drive
        .put("d/c.bin", PutSource::Bytes(&data), &PutOptions::create_only(Some(digest(&data))), &ctx())
        .unwrap();
    // Server-side corruption the server itself does not notice: the stored bytes change but the
    // reported checksum does not.
    env.mock.corrupt_stored_bytes(info.provider_ref.as_deref().unwrap());
    let err = read_all(env.drive.get("d/c.bin", None, &ctx()).unwrap()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
    // Range reads cannot be checked against the MD5 but still return exactly the requested bytes.
    assert_eq!(read_all(env.drive.get("d/c.bin", Some((10, 100)), &ctx()).unwrap()).unwrap(), data[10..110]);
}

#[test]
fn slow_responses_within_the_timeout_succeed_and_beyond_it_are_offline() {
    let env = common::setup_with(MockConfig::default(), DriveSpace::VisibleFolder, |c| {
        c.timeouts = HttpTimeouts { response: Duration::from_millis(400), ..HttpTimeouts::for_tests() };
        c.retry = RetryPolicy { max_attempts: 2, ..RetryPolicy::network() };
    });
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Nth(1), Fault::Delay(Duration::from_millis(50))));
    assert_eq!(env.drive.list("warm/", &ctx()).unwrap().len(), 1);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::Delay(Duration::from_millis(900))));
    let r = env.drive.list("warm/", &ctx());
    assert!(matches!(r, Err(StorageError::Offline(ref m)) if m.contains("timed out")), "{r:?}");
}

#[test]
fn session_urls_must_stay_on_the_expected_host() {
    let env = common::setup();
    env.mock.add_fault(FaultRule::new(
        Endpoint::ResumableInit,
        When::Always,
        Fault::BadLocation("http://evil.example/upload?upload_id=steal".into()),
    ));
    let data = pattern(6 * 1024 * 1024, 5);
    let r = env.drive.put("p/x.pack", PutSource::Bytes(&data), &PutOptions::create_only(None), &ctx());
    assert!(matches!(r, Err(StorageError::Other(ref m)) if m.contains("session address")), "{r:?}");
    assert_eq!(env.mock.count(Endpoint::ResumableChunk), 0);
}

#[test]
fn cancellation_interrupts_backoff() {
    let env = common::setup();
    warm(&env);
    env.mock.add_fault(FaultRule::new(Endpoint::FilesList, When::Always, Fault::server_error(503)));
    let c = ctx();
    c.cancel.cancel();
    assert!(matches!(env.drive.list("warm/", &c), Err(StorageError::Cancelled)));
    assert!(matches!(env.auth.access_token(&c), Err(StorageError::Cancelled)));
}
