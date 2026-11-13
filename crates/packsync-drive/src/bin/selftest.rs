//! `packsync-drive-selftest`: runs PackSync's storage conformance suite against **real Google Drive**.
//!
//! This is the only code in the repository that talks to a live Google account on its own, and it is
//! deliberately hard to run by accident:
//!
//! * it is compiled only with `--features selftest`;
//! * it refuses to do anything without `--i-understand-this-uses-my-drive`;
//! * everything it creates lives under one dedicated folder (`PackSync-Test`) in the account you sign
//!   in with, is tagged `ps=1`, and is removed again at the end (exactly the objects this run created);
//! * its refresh token is stored in the OS credential store under its own service name
//!   (`packsync-selftest`), never in a file, and `logout` removes it.
//!
//! See `docs/testing.md` for the full procedure, including the two-computer check that decides
//! whether the default `drive.file` scope works across devices.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use packsync_core::credentials::CredentialStore;
use packsync_core::progress::OpCtx;
use packsync_core::storage::{MemoryTransferState, PutOptions, PutSource, Storage, StorageError};
use packsync_core::testkit::conformance::{ConformanceOptions, run_all};
use packsync_drive::oauth::{AuthConfig, AuthStatus, Authenticator, LoginOptions, PrintUrl, SystemBrowser, UrlOpener};
use packsync_drive::{DriveConfig, DriveSpace, DriveStorage, KeyringCredentialStore, OAuthClientConfig};

const ROOT: &str = "PackSync-Test";
const SERVICE: &str = "packsync-selftest";
const MARKER_KEY: &str = "selftest/cross-device-marker.txt";

#[derive(Parser)]
#[command(
    name = "packsync-drive-selftest",
    version,
    about = "Run PackSync's storage conformance suite against your real Google Drive (in a dedicated 'PackSync-Test' folder)"
)]
struct Args {
    /// Required. Confirms that you know this signs in to Google and creates (then deletes) files in your Drive.
    #[arg(long = "i-understand-this-uses-my-drive")]
    ack: bool,
    /// Google's downloaded OAuth client JSON (default: PACKSYNC_GOOGLE_CLIENT_ID/SECRET from the environment).
    #[arg(long, value_name = "FILE")]
    client_file: Option<PathBuf>,
    /// `visible_folder` (scope drive.file, default) or `app_data` (scope drive.appdata).
    #[arg(long, default_value = "visible_folder")]
    space: String,
    /// Print the sign-in address instead of opening a browser.
    #[arg(long)]
    no_browser: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the conformance suite (the default). Cleans up after itself unless --keep.
    Run {
        /// Leave the PackSync-Test folder and its contents in place.
        #[arg(long)]
        keep: bool,
        /// Skip the 12 MiB resumable-upload test (saves bandwidth).
        #[arg(long)]
        no_large: bool,
        /// Seconds to keep polling `list` for eventually consistent listings.
        #[arg(long, default_value_t = 20)]
        list_wait_secs: u64,
    },
    /// Two-computer check, step 1: create a marker object and keep it. Run on computer A.
    CrossDeviceWrite,
    /// Two-computer check, step 2: on computer B (same Google account, same OAuth client) — can it see and
    /// read what computer A created? This decides whether `drive.file` works across devices.
    CrossDeviceRead,
    /// Delete the two-computer check's marker and folders.
    CrossDeviceCleanup,
    /// Forget the stored sign-in (and optionally revoke it at Google).
    Logout {
        #[arg(long)]
        revoke: bool,
    },
}

fn space_of(s: &str) -> Result<DriveSpace, String> {
    match s {
        "visible_folder" => Ok(DriveSpace::VisibleFolder),
        "app_data" => Ok(DriveSpace::AppData),
        other => Err(format!("unknown space {other:?}: use visible_folder or app_data")),
    }
}

fn main() {
    let args = Args::parse();
    if !args.ack {
        eprintln!(
            "This tool signs in to Google and creates files in a folder called '{ROOT}' in your Drive, runs a few \
             minutes of upload/download tests, and deletes what it created.\n\
             It is meant for developers validating PackSync against the real service.\n\n\
             Re-run with --i-understand-this-uses-my-drive to proceed. See docs/testing.md."
        );
        std::process::exit(2);
    }
    match run(args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: Args) -> Result<i32, String> {
    let space = space_of(&args.space)?;
    let ctx = OpCtx::detached();
    let client = OAuthClientConfig::discover(args.client_file.as_deref()).map_err(|e| e.to_string())?;
    let creds: Arc<dyn CredentialStore> = Arc::new(KeyringCredentialStore::with_service(SERVICE));
    let auth = Arc::new(Authenticator::new(client, space, AuthConfig::default(), creds));

    if let Some(Cmd::Logout { revoke }) = &args.command {
        if *revoke {
            auth.revoke(&ctx).map_err(|e| e.to_string())?;
        }
        auth.disconnect().map_err(|e| e.to_string())?;
        println!(
            "Signed out of the self-test account{}.",
            if *revoke { " and revoked PackSync-Test's access at Google" } else { "" }
        );
        return Ok(0);
    }

    // sign in when needed
    match auth.status() {
        AuthStatus::Connected { has_required_scope: true, .. } => {}
        AuthStatus::CredentialStoreUnavailable(m) => return Err(m),
        _ => {
            println!("Signing in to Google (scope: {})…", auth.required_scope());
            let opener: Box<dyn UrlOpener> =
                if args.no_browser { Box::new(PrintUrl) } else { Box::new(SystemBrowser { print_fallback: true }) };
            let who = auth.login(&*opener, &LoginOptions::default(), &ctx).map_err(|e| e.to_string())?;
            println!("Signed in as {}.", who.email.as_deref().unwrap_or("(unknown)"));
        }
    }
    let account = auth.account_info(&ctx).map_err(|e| e.to_string())?.email.unwrap_or_default();

    let mut cfg = DriveConfig::new(account.clone(), space);
    cfg.root_folder_name = ROOT.to_string();
    let cmd = args.command.unwrap_or(Cmd::Run { keep: false, no_large: false, list_wait_secs: 20 });
    // the cross-device steps must leave their objects behind
    cfg.track_created = matches!(cmd, Cmd::Run { keep: false, .. } | Cmd::CrossDeviceCleanup);
    let drive =
        DriveStorage::new(cfg, auth.clone(), Arc::new(MemoryTransferState::default())).map_err(|e| e.to_string())?;

    match cmd {
        Cmd::Run { keep, no_large, list_wait_secs } => {
            println!("Running the conformance suite against {} (folder '{ROOT}')…", drive.provider_id());
            let opts = ConformanceOptions {
                list_count: 60,
                large_bytes: if no_large { None } else { Some(12 * 1024 * 1024) },
                writers: 6,
                list_wait: Duration::from_secs(list_wait_secs),
            };
            let results = run_all(&drive, opts);
            let mut failed = 0;
            for (name, r) in &results {
                match r {
                    Ok(()) => println!("PASS  {name}"),
                    Err(e) => {
                        failed += 1;
                        println!("FAIL  {name}\n      {e}");
                    }
                }
            }
            let stats = drive.stats();
            println!("\nHTTP requests during the run: {stats:?}");
            if let Ok(Some(q)) = drive.quota(&ctx) {
                println!(
                    "Drive usage: {} bytes of {}",
                    q.used,
                    q.total.map(|t| t.to_string()).unwrap_or_else(|| "unlimited".into())
                );
            }
            if keep {
                println!("--keep: the '{ROOT}' folder was left in place.");
            } else {
                match drive.delete_created(&ctx) {
                    Ok(n) => println!("Cleaned up: {n} object(s) created by this run were deleted."),
                    Err(e) => {
                        failed += 1;
                        println!("Cleanup problem: {e}\nDelete the '{ROOT}' folder in your Drive manually.");
                    }
                }
            }
            println!(
                "\n{} passed, {failed} failed.",
                results.len() - results.iter().filter(|(_, r)| r.is_err()).count()
            );
            Ok(if failed == 0 { 0 } else { 1 })
        }
        Cmd::CrossDeviceWrite => {
            let token = format!(
                "{:x}",
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
            );
            let body = format!("packsync cross-device marker {token}\n");
            let info = drive
                .put(
                    MARKER_KEY,
                    PutSource::Bytes(body.as_bytes()),
                    &PutOptions::replace().with_tag("ps", "1").with_tag("kind", "selftest"),
                    &ctx,
                )
                .map_err(|e| e.to_string())?;
            println!(
                "Created {MARKER_KEY} ({} bytes, Drive file {}).",
                info.size,
                info.provider_ref.as_deref().unwrap_or("?")
            );
            println!(
                "Now, on ANOTHER computer with the same Google account and the same OAuth client, run:\n  packsync-drive-selftest --i-understand-this-uses-my-drive --space {} cross-device-read",
                args.space
            );
            println!("Expected content: {body:?}");
            Ok(0)
        }
        Cmd::CrossDeviceRead => {
            let items = drive.list("selftest/", &ctx).map_err(|e| e.to_string())?;
            println!("Objects visible under selftest/: {}", items.len());
            match drive.get_bytes(MARKER_KEY, 4096, &ctx) {
                Ok(bytes) => {
                    println!(
                        "PASS  this computer can read the object computer A created: {:?}",
                        String::from_utf8_lossy(&bytes)
                    );
                    println!(
                        "      → with scope {}, files created by the app are visible across devices (same client ID).",
                        auth.required_scope()
                    );
                    Ok(0)
                }
                Err(StorageError::NotFound(_)) => {
                    println!("FAIL  the marker is not visible from this computer.");
                    println!(
                        "      With the default `drive.file` scope this means files created on one computer are NOT visible to the"
                    );
                    println!(
                        "      same OAuth client on another. Re-run both steps with `--space app_data` (scope drive.appdata); if that"
                    );
                    println!("      passes, use the app-data space for real use (set storage space to app_data).");
                    Ok(1)
                }
                Err(e) => Err(e.to_string()),
            }
        }
        Cmd::CrossDeviceCleanup => {
            drive.delete(MARKER_KEY, packsync_core::storage::DeleteMode::Permanent, &ctx).map_err(|e| e.to_string())?;
            println!("Deleted {MARKER_KEY}. The empty '{ROOT}' folder can be removed from Drive by hand.");
            Ok(0)
        }
        Cmd::Logout { .. } => unreachable!("handled above"),
    }
}
