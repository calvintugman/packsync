//! Launcher integration contract. Prism Launcher is the first implementation
//! (`packsync-prism`); the core never mentions a concrete launcher.
//!
//! Responsibilities of a launcher adapter:
//! * discover instances and describe a manually selected instance folder;
//! * say which part of the instance directory is the *game root* and contribute launcher-specific
//!   rules and mixed-file policies (e.g. Prism's `instance.cfg`: portable keys vs local settings);
//! * register a restored instance with the launcher *safely* (never expose a half-restored
//!   instance to the launcher), adapting local settings (Java path, memory);
//! * launch the game (non-blocking) and manage opt-in launcher hooks (preview → apply → remove).
//!
//! Everything else (snapshots, uploads, locking, process tracking) stays in the core.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::InstanceId;
use crate::mixed::MixedPolicy;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderInfo {
    /// `forge`, `neoforge`, `fabric`, `quilt`, `liteloader`, ...
    pub kind: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentInfo {
    /// Launcher component uid (`net.minecraft`, `net.minecraftforge`, `org.lwjgl3`, ...).
    pub uid: String,
    pub version: Option<String>,
}

/// The exact Minecraft/loader identity of a pack as the launcher records it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackIdentity {
    pub minecraft_version: Option<String>,
    pub loader: Option<LoaderInfo>,
    pub components: Vec<ComponentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldInfo {
    /// Folder name under `saves/`.
    pub folder: String,
    /// `LevelName` from `level.dat` when readable.
    pub name: Option<String>,
    pub last_played_ms: Option<i64>,
}

/// How a launcher refers to one of its instances. Opaque to the core; round-trips through JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherRef {
    pub launcher: String,
    /// Launcher installation's data directory (Prism: the directory holding `instances/`).
    pub data_dir: Option<PathBuf>,
    /// The launcher's own instance ID (Prism: the folder name).
    pub instance_id: Option<String>,
    /// Launcher executable if known.
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredInstance {
    pub launcher_ref: LauncherRef,
    /// Instance root (the directory that is synced; for Prism, the folder with `instance.cfg`).
    pub root: PathBuf,
    pub name: String,
    /// Game directory relative to `root` (`.minecraft`, `minecraft`, or `""` for a plain game dir).
    pub game_root: String,
    pub identity: PackIdentity,
    pub worlds: Vec<WorldInfo>,
}

/// Machine-specific settings the user may need to adjust after a restore.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSettings {
    pub java_path: Option<String>,
    pub min_memory_mb: Option<u32>,
    pub max_memory_mb: Option<u32>,
    pub jvm_args: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RegisterRequest {
    /// Fully restored and validated instance directory, in a location the launcher ignores
    /// (hidden `.packsync-restore-*` folder next to the final location).
    pub staged_root: PathBuf,
    /// Which launcher installation to register into (`None` → the adapter's first discovered one).
    pub target: Option<LauncherRef>,
    /// Display name for the instance (from the snapshot).
    pub display_name: String,
    /// Preferred final folder name; the adapter makes it unique and filesystem-safe.
    pub preferred_folder: String,
    /// Local overrides to apply (Java path, memory). `None` fields keep the launcher defaults.
    pub local: LocalSettings,
    /// PackSync identity of the instance (recorded so hooks can be installed later).
    pub instance_id: InstanceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredInstance {
    pub launcher_ref: LauncherRef,
    /// Final instance root. The staged directory has been atomically renamed here.
    pub root: PathBuf,
    pub game_root: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub launcher_ref: LauncherRef,
    pub root: PathBuf,
    /// Offline player name (`prismlauncher --offline`), when the user chose offline mode.
    pub offline_name: Option<String>,
    pub profile: Option<String>,
    /// Extra environment for the launcher process (ignored when an already-running launcher is
    /// asked to launch — documented limitation).
    pub env: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct LaunchHandle {
    /// The spawned launcher process. `None` when the launcher forwarded the request to an
    /// already-running instance (the child exits immediately; the game cannot be waited on via it).
    pub child: Option<std::process::Child>,
    pub launcher_pid: Option<u32>,
    pub forwarded_to_running_launcher: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    /// Absolute path of the CLI executable that implements `hook pre-launch|post-exit`.
    pub executable: PathBuf,
    /// PackSync instance identity passed to the hooks (a UUID: no quoting problems).
    pub instance_id: InstanceId,
}

/// One change a hook installation or removal would make (shown to the user before applying).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookChange {
    pub file: PathBuf,
    pub key: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPlan {
    pub root: PathBuf,
    pub changes: Vec<HookChange>,
    /// Existing user commands that will be chained (kept running) rather than replaced.
    pub preserved_commands: Vec<String>,
    /// Problems that make applying unsafe right now (launcher running, ...).
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookState {
    pub installed: bool,
    /// Whether the installed commands still match what PackSync would write.
    pub intact: bool,
    pub pre_launch: Option<String>,
    pub post_exit: Option<String>,
}

/// Launcher adapter. Blocking; implementations must be thread-safe.
pub trait Launcher: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Instances found on this machine (all launcher installations the adapter can locate).
    fn discover(&self) -> Result<Vec<DiscoveredInstance>>;

    /// Validate and describe a user-selected instance folder (manual selection).
    fn describe(&self, root: &Path) -> Result<DiscoveredInstance>;

    /// Launcher-specific rules (same TOML schema as user rules), merged between the built-in
    /// defaults and user overrides. Paths use the `$GAME` placeholder for the game root.
    fn rules_fragment(&self) -> &'static str;

    /// Launcher-owned files that mix portable and machine-local data.
    fn mixed_policies(&self) -> Vec<MixedPolicy>;

    /// Current machine-specific settings of an instance (for the "configure Java/RAM" step).
    fn local_settings(&self, root: &Path) -> Result<LocalSettings>;

    /// Register a restored instance with the launcher (see [`RegisterRequest`]).
    fn register_restored(&self, req: &RegisterRequest) -> Result<RegisteredInstance>;

    /// Start the launcher for the instance. Non-blocking.
    fn launch(&self, req: &LaunchRequest) -> Result<LaunchHandle>;

    fn is_launcher_running(&self, launcher_ref: &LauncherRef) -> bool;

    /// Extra command-line fragments that identify the instance's game process (Prism:
    /// `-Djava.library.path=<root>/natives`). Used in addition to generic cwd/cmdline matching.
    fn game_process_patterns(&self, root: &Path) -> Vec<String>;

    // --- opt-in hooks -------------------------------------------------------------------
    fn hook_state(&self, root: &Path) -> Result<HookState>;
    /// Compute (but do not write) what installing the hooks would change.
    fn plan_install_hooks(&self, root: &Path, spec: &HookSpec) -> Result<HookPlan>;
    /// Apply a previously computed plan. Must re-validate against the current file contents.
    fn apply_hook_plan(&self, plan: &HookPlan) -> Result<()>;
    fn plan_remove_hooks(&self, root: &Path) -> Result<HookPlan>;
}
