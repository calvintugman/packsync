//! Application layer shared by the `packsync` CLI, the desktop shell and the dev bridge.
//!
//! * [`App`] owns the sync engine, settings, the storage connection and background jobs.
//! * [`launch`] orchestrates "sync → play → save → upload" and the launcher-hook entry points.
//! * [`rpc`] is the JSON method table the desktop UI calls.
//!
//! No front-end logic lives here (no printing, no windows), so everything is testable headless.

pub mod app;
pub mod bridge;
pub mod diagnostics;
pub mod events;
pub mod hooks;
pub mod jobs;
pub mod launch;
pub mod paths;
pub mod provider;
pub mod rpc;
pub mod settings;

pub use app::{App, AppOptions, DiscoveredView, RestoreCloudRequest, SettingsUpdate};
pub use events::{AppEvent, ErrorView, EventBus, JobInfo, JobState};
pub use launch::{HookOutcome, LaunchOptions, LaunchOutcome, LaunchTiming};
pub use paths::AppPaths;
pub use settings::{OfflinePolicy, Settings, StorageSetting};
