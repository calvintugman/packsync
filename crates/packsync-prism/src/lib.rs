//! Prism Launcher adapter for PackSync. See `docs/hooks.md` and `docs/testing.md`.
//!
//! * [`PrismLauncher`] implements [`packsync_core::launcher::Launcher`]: discovery, validation of
//!   a manually selected folder, restore registration (atomic rename into the instances folder),
//!   launching, and opt-in hook management.
//! * [`qtini`] is a port of Qt's `QSettings` INI reader/writer: Prism writes `instance.cfg` with it,
//!   so edits must follow its escaping rules exactly to round-trip through the real launcher.
//! * [`cmdline`] ports `QProcess::splitCommand`, which Prism uses to run hook commands (no shell).

pub mod cmdline;
pub mod discovery;
pub mod env;
pub mod files;
pub mod hooks;
pub mod instance;
mod launcher;
pub mod process;
pub mod qtini;
pub mod rules;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use launcher::{Exe, KIND, LaunchCommand, PrismLauncher};
