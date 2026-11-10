//! Command-line grammar. Everything a user can do in the desktop app can be done here, with stable
//! `--json` output and documented exit codes (`docs/cli.md`).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "packsync",
    version,
    about = "Continue your Minecraft modpack and worlds on any computer, through your own Google Drive",
    long_about = "PackSync keeps a Prism Launcher instance (mods, configs, worlds) in sync across your computers \
using a folder in your own Google Drive. Nothing leaves your Google account; there is no PackSync server.\n\n\
Exit codes: 0 ok, 1 failure, 2 usage, 3 not found, 10 offline, 11 authentication, 12 conflict, 13 busy, \
14 game running, 15 upload pending, 16 corrupt cloud data, 17 no space, 18 cancelled, 19 safety guard / crash \
pending, 20 Drive full, 30 launch failed."
)]
pub struct Cli {
    /// Print one JSON document on stdout instead of text (`{"ok":true,"result":..}` or `{"ok":false,"error":..}`).
    #[arg(long, global = true)]
    pub json: bool,
    /// Do not print progress or hints on stderr.
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,
    /// Write more detail to the log file (`<state>/logs/packsync.log`; secrets are always redacted).
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,
    /// Answer yes to confirmation questions (required when stdin is not a terminal).
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,
    /// Directory for settings (default: the platform config directory). Env: PACKSYNC_CONFIG_DIR.
    #[arg(long, global = true, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// Directory for the database and staging area (default: the platform state directory). Env: PACKSYNC_STATE_DIR.
    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show the sync state of one or all instances.
    Status(StatusArgs),
    /// List Prism instances found on this computer and what PackSync does with them.
    Instances,
    /// Show what would be synced for a folder, without changing anything.
    Preview(PreviewArgs),
    /// Start syncing an instance (shows the preview first).
    Enable(EnableArgs),
    /// Stop syncing an instance on this computer (cloud data and local files stay as they are).
    Disable(InstanceArg),
    /// Show local changes that are not in a snapshot yet.
    Changes(InstanceArg),
    /// Save the current state of an instance as a local snapshot.
    Snapshot(SnapshotArgs),
    /// Upload saved snapshots to the cloud.
    Upload(UploadArgs),
    /// Snapshot, then bring this computer and the cloud in step (the "Sync now" button).
    Sync(SyncArgs),
    /// Compare with the cloud: download newer versions, upload local ones, report conflicts.
    Reconcile(ReconcileArgs),
    /// List snapshots of an instance.
    History(HistoryArgs),
    /// Check local files (and with --remote the cloud data) against the last snapshot.
    Verify(VerifyArgs),
    /// Restore a snapshot over an instance (the previous state is kept as a recovery point).
    Restore(RestoreArgs),
    /// Bring an instance from the cloud to this computer as a new Prism instance.
    RestoreNew(RestoreNewArgs),
    /// List instances stored in the cloud.
    Cloud,
    /// List recovery points (what restores replaced) of an instance.
    Recovery(InstanceArg),
    /// Show the two versions of a diverged instance.
    Conflicts(InstanceArg),
    /// Resolve a diverged instance. Both versions are preserved in history.
    Resolve(ResolveArgs),
    /// Decide what to do with a save from a session that did not end cleanly.
    #[command(subcommand)]
    Crash(CrashCmd),
    /// Explain or preview the sync rules.
    #[command(subcommand)]
    Rules(RulesCmd),
    /// Install, inspect or remove the optional Prism launcher hooks.
    #[command(subcommand)]
    Hooks(HooksCmd),
    /// Entry points that Prism calls (pre-launch / post-exit). Not meant to be run by hand.
    #[command(subcommand, hide = true)]
    Hook(HookCmd),
    /// Sync, start the game through Prism, wait for it, save and upload.
    Launch(LaunchArgs),
    /// Connect storage, sign in, or disconnect.
    #[command(subcommand)]
    Storage(StorageCmd),
    /// Sign in to or out of Google (shortcut for `storage`).
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Show or change settings.
    #[command(subcommand)]
    Settings(SettingsCmd),
    /// Name this computer.
    Device(DeviceArgs),
    /// Check the installation and print diagnostics (secrets are never included).
    Doctor(DoctorArgs),
    /// Plan or run cloud cleanup (manual only; PackSync never deletes cloud data on its own).
    #[command(subcommand)]
    Gc(GcCmd),
    /// Local HTTP API for UI development and automated UI tests (127.0.0.1 only).
    #[command(hide = true)]
    DevBridge(BridgeArgs),
}

#[derive(Args, Debug)]
pub struct InstanceArg {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Afterwards, show where the time and the requests went: each step with its duration, HTTP
    /// requests and bytes, the files that cost the most, and the manifest size.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Limit to one instance.
    pub instance: Option<String>,
    /// Also ask the cloud (otherwise only local facts are shown).
    #[arg(long)]
    pub online: bool,
}

#[derive(Args, Debug)]
pub struct PreviewArgs {
    /// Instance folder (the one containing instance.cfg).
    pub path: PathBuf,
    #[arg(long, default_value = "prism")]
    pub launcher: String,
    /// Extra rules (TOML) appended to the defaults.
    #[arg(long, value_name = "FILE")]
    pub rules_file: Option<PathBuf>,
    /// Read every file to estimate how much would be uploaded.
    #[arg(long)]
    pub estimate_upload: bool,
}

#[derive(Args, Debug)]
pub struct EnableArgs {
    /// Instance folder (the one containing instance.cfg).
    pub path: PathBuf,
    #[arg(long, default_value = "prism")]
    pub launcher: String,
    /// Extra rules (TOML) appended to the defaults.
    #[arg(long, value_name = "FILE")]
    pub rules_file: Option<PathBuf>,
    /// Create the first snapshot but do not upload it yet.
    #[arg(long)]
    pub no_upload: bool,
}

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Create a snapshot even if nothing changed.
    #[arg(long)]
    pub force: bool,
    /// A note stored with the snapshot.
    #[arg(long)]
    pub note: Option<String>,
    /// Allow a snapshot that deletes many files (after you verified that is intended).
    #[arg(long)]
    pub confirm_mass_delete: bool,
}

#[derive(Args, Debug)]
pub struct UploadArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Retry now even if a backoff is pending.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ReconcileArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Report a newer cloud version instead of applying it.
    #[arg(long)]
    pub no_fast_forward: bool,
    /// Do not upload local snapshots.
    #[arg(long)]
    pub no_upload: bool,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Do not contact the cloud.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Also check that the cloud has everything the snapshot needs.
    #[arg(long)]
    pub remote: bool,
    /// With --remote: download and hash every object (slow, uses data).
    #[arg(long, requires = "remote")]
    pub deep: bool,
    /// Snapshot to check in the cloud (default: this computer's base).
    #[arg(long)]
    pub snapshot: Option<String>,
    /// With --remote: upload again, from this computer's files, any data the cloud is missing (for this
    /// snapshot and for versions not uploaded yet). Never changes the instance's files or its history.
    #[arg(long, requires = "remote")]
    pub repair: bool,
    /// With --repair: if this computer no longer has some of that data either, drop the versions that only
    /// exist here and can never be uploaded (the files are not touched; `packsync sync` then saves them).
    #[arg(long, requires = "repair")]
    pub discard_unrepairable: bool,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Snapshot ID (or unique prefix) from `packsync history`.
    pub snapshot: String,
    /// Show what would change and stop.
    #[arg(long)]
    pub preview: bool,
    /// Skip files that cannot be written on this platform instead of refusing.
    #[arg(long)]
    pub skip_incompatible: bool,
}

#[derive(Args, Debug)]
pub struct RestoreNewArgs {
    /// Cloud instance (ID, prefix or name); optional when the cloud has exactly one.
    pub instance: Option<String>,
    /// Snapshot to restore (default: the cloud's single head).
    #[arg(long)]
    pub snapshot: Option<String>,
    /// Prism data directory to restore into (default: the first one found).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Folder name for the new instance.
    #[arg(long)]
    pub name: Option<String>,
    /// Java executable to use on this computer.
    #[arg(long)]
    pub java: Option<String>,
    /// Minimum Java heap for this computer (MB).
    #[arg(long)]
    pub min_memory_mb: Option<u32>,
    /// Maximum Java heap for this computer (MB).
    #[arg(long)]
    pub max_memory_mb: Option<u32>,
    /// Skip files that cannot be written on this platform instead of refusing.
    #[arg(long)]
    pub skip_incompatible: bool,
}

#[derive(Args, Debug)]
pub struct ResolveArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// keep-local | keep-cloud | split
    pub choice: String,
    /// keep-cloud with several cloud versions: which one (snapshot ID or prefix).
    #[arg(long)]
    pub head: Option<String>,
    /// split: name of the new, separate instance.
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum CrashCmd {
    /// Show the pending crash save of an instance.
    Show(InstanceArg),
    /// Trust this computer's files: make the crash save a normal snapshot.
    Promote(InstanceArg),
    /// Go back to the last good snapshot; the crash data is kept as a recovery point and in history.
    Discard(InstanceArg),
}

#[derive(Subcommand, Debug)]
pub enum RulesCmd {
    /// Print the effective rules of an instance.
    Show(InstanceArg),
    /// Say why a path is (not) synced.
    Explain {
        instance: String,
        /// Path relative to the instance folder, e.g. `.minecraft/saves/World/region/r.0.0.mca`.
        path: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum HooksCmd {
    /// Show whether the hooks are installed.
    Status(InstanceArg),
    /// Install the hooks (shows exactly what changes first). Prism must be closed.
    Install {
        instance: String,
        /// Path of the `packsync` executable the hooks should call.
        #[arg(long)]
        cli: Option<PathBuf>,
        /// Only show the plan.
        #[arg(long)]
        plan: bool,
    },
    /// Remove the hooks and restore the commands that were there before.
    Uninstall {
        instance: String,
        #[arg(long)]
        plan: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HookCmd {
    PreLaunch(HookArgs),
    PostExit(HookArgs),
}

#[derive(Args, Debug)]
pub struct HookArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    #[arg(long)]
    pub instance: String,
    #[arg(long)]
    pub marker: Option<String>,
    /// The user's own command that was chained after PackSync's (after `--`).
    #[arg(last = true)]
    pub chained: Vec<String>,
}

#[derive(Args, Debug)]
pub struct LaunchArgs {
    /// Instance: its ID (or unique prefix), its name, or the path of its folder.
    pub instance: String,
    /// Start Prism in offline mode with this player name.
    #[arg(long)]
    pub offline_player: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
    /// Do not contact the cloud before starting.
    #[arg(long)]
    pub skip_sync: bool,
    /// Do not upload after the game ends.
    #[arg(long)]
    pub no_upload: bool,
}

#[derive(Subcommand, Debug)]
pub enum StorageCmd {
    /// Show the current connection.
    Status,
    /// Use a local folder as the "cloud" (testing, or a folder another tool mirrors).
    UseLocal { dir: PathBuf },
    /// Use Google Drive (then run `packsync auth login`).
    UseGoogle {
        /// visible_folder (default) or app_data.
        #[arg(long, default_value = "visible_folder")]
        space: String,
    },
    /// Use Cloudflare R2. Asks for the secret access key (hidden) and keeps it in the OS credential store.
    UseR2 {
        /// Cloudflare account ID (dashboard → R2 → Account details).
        #[arg(long)]
        account_id: String,
        /// The bucket (create it first in the dashboard).
        #[arg(long)]
        bucket: String,
        /// The R2 API token's access key ID.
        #[arg(long)]
        access_key_id: String,
        /// Folder inside the bucket that belongs to PackSync.
        #[arg(long, default_value = "packsync")]
        prefix: String,
        /// Read the secret access key from standard input instead of asking (scripts).
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Check that the connection works.
    Test,
    /// Forget the connection (cloud data is not touched).
    Disconnect {
        /// Also revoke PackSync's access at Google.
        #[arg(long)]
        revoke: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Sign in with Google in your browser.
    Login {
        /// Print the address instead of opening a browser (for SSH sessions).
        #[arg(long)]
        no_browser: bool,
    },
    Status,
    Logout {
        #[arg(long)]
        revoke: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SettingsCmd {
    Show,
    /// Change one setting: `packsync settings set hook_offline_policy block`.
    Set {
        key: String,
        value: String,
    },
}

#[derive(Args, Debug)]
pub struct DeviceArgs {
    /// New name for this computer (omit to show it).
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Also contact the cloud.
    #[arg(long)]
    pub online: bool,
    /// Write the report to a file (secrets are redacted).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum GcCmd {
    /// Show what a cleanup would do. Changes nothing.
    Plan(GcOpts),
    /// Carry out a cleanup: give the plan id that `gc plan` printed. The plan is recomputed first and
    /// refused if the cloud changed since you looked at it.
    Run {
        /// The plan id printed by `gc plan` (with the same options).
        plan: String,
        #[command(flatten)]
        opts: GcOpts,
    },
}

#[derive(Args, Debug, Clone)]
pub struct GcOpts {
    /// Also release the content of old snapshots (retention). Without this only objects that nothing
    /// needs are removed.
    #[arg(long)]
    pub retain: bool,
    /// With --retain: keep the content of the newest N snapshots.
    #[arg(long, default_value_t = 20)]
    pub keep_snapshots: usize,
    /// With --retain: and of every snapshot younger than this many days.
    #[arg(long, default_value_t = 30)]
    pub keep_days: u32,
    /// With --retain: keep crash saves (and the snapshot before them) this many days.
    #[arg(long, default_value_t = 90)]
    pub keep_crash_days: u32,
    /// With --retain: limit it to one instance (ID or name).
    #[arg(long)]
    pub instance: Option<String>,
    /// Never touch objects younger than this (at least 24): they may belong to an upload in progress.
    #[arg(long, default_value_t = 48)]
    pub grace_hours: u32,
    /// Do not merge small index objects.
    #[arg(long)]
    pub no_consolidate: bool,
}

#[derive(Args, Debug)]
pub struct BridgeArgs {
    /// 0 picks a free port.
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    /// Serve the built UI from this folder (same origin as the API).
    #[arg(long, value_name = "DIR")]
    pub ui_dir: Option<PathBuf>,
    /// Let a UI dev server on this origin call the API (otherwise use the dev server's proxy).
    #[arg(long, value_name = "ORIGIN")]
    pub allow_origin: Option<String>,
}
