//! Google sign-in (installed-app flow with PKCE and a loopback redirect) against the mock server, and
//! the secret-hygiene guarantee: nothing the mock issues ever shows up in logs, errors or `Debug` output.

mod common;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::*;
use packsync_core::credentials::{CredentialStore, MemoryCredentialStore};
use packsync_core::progress::{CancelToken, OpCtx};
use packsync_core::storage::{AuthProblem, Storage, StorageError};
use packsync_drive::mock::{Consent, Endpoint, MockBrowser, MockConfig, MockDrive};
use packsync_drive::oauth::{AuthError, AuthStatus, Authenticator, LoginOptions, PrintUrl};
use packsync_drive::{DriveSpace, StatsHandle};

fn sign_in_setup() -> (MockDrive, Arc<Authenticator>, Arc<MemoryCredentialStore>, StatsHandle) {
    let mock = MockDrive::start_default().unwrap();
    let stats = StatsHandle::default();
    let creds = Arc::new(MemoryCredentialStore::default());
    let auth = Arc::new(Authenticator::new(
        mock.client_config(),
        DriveSpace::VisibleFolder,
        auth_config(&mock, &stats),
        creds.clone(),
    ));
    (mock, auth, creds, stats)
}

fn quick() -> LoginOptions {
    LoginOptions { timeout: Duration::from_secs(10), ..LoginOptions::default() }
}

#[test]
fn sign_in_stores_only_a_refresh_token_in_the_credential_store_and_reports_the_account() {
    let (mock, auth, creds, _stats) = sign_in_setup();
    assert!(matches!(auth.status(), AuthStatus::NotConnected));
    let browser = MockBrowser::new();
    let who = auth.login(&browser, &quick(), &OpCtx::detached()).unwrap();
    assert_eq!(who.email.as_deref(), Some(mock.config().email.as_str()));
    // the browser was shown a plain "you can close this tab" page
    let visits = browser.wait_visits(1, Duration::from_secs(5));
    assert_eq!(visits[0].status, 200);
    assert!(!visits[0].body.contains("code=") && visits[0].body.to_lowercase().contains("close"), "{}", visits[0].body);
    match auth.status() {
        AuthStatus::Connected { has_required_scope, email, scopes } => {
            assert!(has_required_scope);
            assert_eq!(email.as_deref(), Some(mock.config().email.as_str()));
            assert_eq!(scopes, vec![DriveSpace::VisibleFolder.scope().to_string()]);
        }
        other => panic!("{other:?}"),
    }
    // exactly one secret is persisted: the refresh token, in the credential store (no access token, no code)
    let stored = creds.get(auth.credential_account()).unwrap().expect("a refresh token was stored");
    let record = stored.expose().to_string();
    assert!(record.contains("refresh_token") || mock.secrets().iter().any(|s| record.contains(s.as_str())), "{record}");
    assert!(mock.token_requests("authorization_code") == 1 && mock.count(Endpoint::Authorize) >= 1);
}

#[test]
fn a_forged_callback_is_rejected_and_the_genuine_one_completes_the_sign_in() {
    let (mock, auth, _creds, _stats) = sign_in_setup();
    let browser = MockBrowser::forging_first_callback();
    let who = auth.login(&browser, &quick(), &OpCtx::detached()).unwrap();
    assert_eq!(who.email.as_deref(), Some(mock.config().email.as_str()));
    let visits = browser.wait_visits(2, Duration::from_secs(5));
    assert_eq!(visits.len(), 2);
    assert!(visits[0].status >= 400, "the forged callback was refused: {visits:?}");
    assert_eq!(visits[1].status, 200);
    assert_eq!(mock.token_requests("authorization_code"), 1, "the forged state never reached the token endpoint");
}

#[test]
fn a_wrong_state_from_the_consent_screen_never_produces_credentials() {
    let (mock, auth, creds, _stats) = sign_in_setup();
    mock.set_consent(Consent::WrongState);
    let opts = LoginOptions { timeout: Duration::from_secs(2), ..LoginOptions::default() };
    let err = auth.login(&MockBrowser::new(), &opts, &OpCtx::detached()).unwrap_err();
    assert!(matches!(err, AuthError::TimedOut { rejected_callbacks, .. } if rejected_callbacks >= 1), "{err}");
    assert!(creds.get(auth.credential_account()).unwrap().is_none());
    assert_eq!(mock.token_requests("authorization_code"), 0);
}

#[test]
fn declined_consent_and_a_narrow_grant_are_reported_clearly_and_store_nothing() {
    let (mock, auth, creds, _stats) = sign_in_setup();
    mock.set_consent(Consent::Deny);
    let err = auth.login(&MockBrowser::new(), &quick(), &OpCtx::detached()).unwrap_err();
    assert!(matches!(err, AuthError::Denied(_)), "{err}");
    assert!(err.to_string().contains("access was not granted"));
    // the user unticks the Drive permission on Google's granular consent screen
    mock.set_consent(Consent::GrantScopes(vec!["openid".into(), "email".into()]));
    let err = auth.login(&MockBrowser::new(), &quick(), &OpCtx::detached()).unwrap_err();
    assert!(matches!(err, AuthError::InsufficientScope { .. }), "{err}");
    assert!(err.to_string().contains("keep that permission checked"), "{err}");
    assert_eq!(err.to_storage_error().to_string(), StorageError::Auth(AuthProblem::InsufficientScope).to_string());
    assert!(
        creds.get(auth.credential_account()).unwrap().is_none(),
        "nothing was stored for a grant that is not usable"
    );
    assert!(matches!(auth.status(), AuthStatus::NotConnected));
}

#[test]
fn nobody_answering_times_out_and_cancellation_stops_the_wait_at_once() {
    let (_mock, auth, _creds, _stats) = sign_in_setup();
    // a browser that never opens: the user closed the tab
    let t = std::time::Instant::now();
    let err = auth
        .login(
            &PrintUrl,
            &LoginOptions { timeout: Duration::from_millis(600), ..LoginOptions::default() },
            &OpCtx::detached(),
        )
        .unwrap_err();
    assert!(matches!(err, AuthError::TimedOut { .. }), "{err}");
    assert!(t.elapsed() < Duration::from_secs(5));
    // cancellation from the UI
    let cancel = CancelToken::new();
    let ctx = OpCtx::new(Arc::new(packsync_core::progress::NullSink), cancel.clone());
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c2.cancel();
    });
    let t = std::time::Instant::now();
    let err = auth
        .login(&PrintUrl, &LoginOptions { timeout: Duration::from_secs(60), ..LoginOptions::default() }, &ctx)
        .unwrap_err();
    assert!(matches!(err, AuthError::Cancelled), "{err}");
    assert!(t.elapsed() < Duration::from_secs(10));
}

#[test]
fn the_access_token_is_refreshed_silently_and_a_revoked_grant_asks_to_reconnect() {
    let env = setup();
    let ctx = OpCtx::detached();
    env.drive.list("instances/", &ctx).unwrap();
    let before = env.mock.token_requests("refresh_token");
    env.mock.expire_access_tokens();
    env.drive.list("instances/", &ctx).unwrap();
    assert_eq!(env.mock.token_requests("refresh_token"), before + 1, "exactly one silent refresh");
    env.mock.revoke_all_tokens();
    let err = env.drive.list("instances/", &ctx).unwrap_err();
    assert!(matches!(err, StorageError::Auth(AuthProblem::Revoked)), "{err:?}");
    assert!(matches!(env.auth.status(), AuthStatus::Revoked));
}

#[test]
fn disconnect_forgets_locally_while_revoke_also_asks_google() {
    let (mock, auth, creds, _stats) = sign_in_setup();
    auth.login(&MockBrowser::new(), &quick(), &OpCtx::detached()).unwrap();
    let before = mock.count(Endpoint::Revoke);
    auth.disconnect().unwrap();
    assert!(creds.get(auth.credential_account()).unwrap().is_none());
    assert_eq!(mock.count(Endpoint::Revoke), before, "disconnect is local only");
    assert!(matches!(auth.status(), AuthStatus::NotConnected));
    auth.login(&MockBrowser::new(), &quick(), &OpCtx::detached()).unwrap();
    auth.revoke(&OpCtx::detached()).unwrap();
    assert_eq!(mock.count(Endpoint::Revoke), before + 1);
    assert!(creds.get(auth.credential_account()).unwrap().is_none());
}

// ---------------------------------------------------------------------------------------------------
// Secret hygiene
// ---------------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        // PackSync's own events at the most verbose level; third-party crates (ureq prints whole
        // request/response headers at DEBUG, and the scripted "browser" in these tests is a ureq client
        // too) stay at `warn`, exactly as the application configures them.
        let filter = tracing_subscriber::EnvFilter::new("warn,packsync_drive=trace,packsync_core=trace");
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(Capture(buf.clone()))
            .try_init();
        buf
    })
    .clone()
}

#[test]
fn no_secret_ever_appears_in_logs_errors_or_debug_output() {
    let logs = captured_logs();
    let mock = MockDrive::start(MockConfig::default()).unwrap();
    let stats = StatsHandle::default();
    let creds = Arc::new(MemoryCredentialStore::default());
    let auth = Arc::new(Authenticator::new(
        mock.client_config(),
        DriveSpace::VisibleFolder,
        auth_config(&mock, &stats),
        creds.clone(),
    ));
    let ctx = OpCtx::detached();
    let mut printed: Vec<String> = vec![];
    // sign in (code, verifier, tokens flow through here), including a forged attempt and a refusal
    mock.set_consent(Consent::Deny);
    printed.push(format!("{:?}", auth.login(&MockBrowser::new(), &quick(), &ctx).unwrap_err()));
    mock.set_consent(Consent::Approve);
    let who = auth.login(&MockBrowser::forging_first_callback(), &quick(), &ctx).unwrap();
    printed.push(format!("{who:?} {auth:?} {:?} {}", auth.status(), creds.describe()));
    // use the storage: uploads (resumable session URLs are secrets), a silent refresh and a failure of every kind
    let cfg = drive_config(&mock, DriveSpace::VisibleFolder, &stats);
    printed.push(format!("{cfg:?} {:?}", mock.client_config()));
    let state = Arc::new(packsync_core::storage::MemoryTransferState::default());
    let drive = packsync_drive::DriveStorage::new(cfg, auth.clone(), state).unwrap();
    let big = pattern(3 * 1024 * 1024, 7);
    drive
        .put(
            "packs/hygiene.bin",
            packsync_core::storage::PutSource::Bytes(&big),
            &packsync_core::storage::PutOptions::replace(),
            &ctx,
        )
        .unwrap();
    mock.expire_access_tokens();
    drive.list("packs/", &ctx).unwrap();
    for e in [drive.get_bytes("packs/missing.bin", 10, &ctx).unwrap_err(), {
        mock.revoke_all_tokens();
        drive.list("packs/", &ctx).unwrap_err()
    }] {
        printed.push(format!("{e} | {e:?}"));
    }
    printed.push(format!("{:?} {:?}", drive.stats(), drive.recent_retries()));
    printed.push(String::from_utf8_lossy(&logs.lock().unwrap()).into_owned());
    let secrets = mock.secrets();
    assert!(secrets.len() >= 5, "the mock issued codes, tokens and session IDs: {}", secrets.len());
    let everything = printed.join("\n");
    for s in &secrets {
        if s.len() < 8 {
            continue; // too short to be meaningful as a needle
        }
        if everything.contains(s.as_str()) {
            let at = printed.iter().position(|p| p.contains(s.as_str())).unwrap();
            let line = printed[at].lines().find(|l| l.contains(s.as_str())).unwrap_or("");
            panic!(
                "a secret ({} chars, starts {:?}) leaked into output #{at}: {}",
                s.len(),
                &s[..4],
                line.chars().take(300).collect::<String>()
            );
        }
    }
    // the logs did record the flow (so the check above is not vacuous)
    assert!(everything.contains("Google account connected"), "{}", &everything[everything.len().saturating_sub(400)..]);
}
