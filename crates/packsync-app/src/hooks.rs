//! Installing and removing the opt-in launcher hooks. Hooks are never installed implicitly: the UI
//! and CLI first show the plan (exact before/after values, existing commands that will be chained)
//! and only then apply it.

use std::path::{Path, PathBuf};

use packsync_core::brand::CLI_NAME;
use packsync_core::error::{Error, Result};
use packsync_core::ids::InstanceId;
use packsync_core::launcher::{HookPlan, HookSpec, HookState};

use crate::app::App;

fn exe_name() -> String {
    if cfg!(windows) { format!("{CLI_NAME}.exe") } else { CLI_NAME.to_string() }
}

impl App {
    /// The command-line executable that launcher hooks should call: the configured path, this
    /// executable when it *is* the CLI, a `packsync` next to it, or one on `PATH`.
    pub fn resolve_cli_path(&self) -> Result<PathBuf> {
        if let Some(p) = self.settings().cli_path {
            return if p.is_file() {
                Ok(p)
            } else {
                Err(Error::Config(format!("the configured PackSync command-line tool {} does not exist", p.display())))
            };
        }
        let name = exe_name();
        if let Ok(me) = std::env::current_exe() {
            if me.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&name)) {
                return Ok(me);
            }
            if let Some(sibling) = me.parent().map(|d| d.join(&name)).filter(|p| p.is_file()) {
                return Ok(sibling);
            }
        }
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let p = dir.join(&name);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
        Err(Error::Config(format!(
            "the PackSync command-line tool ({name}) was not found. Install it next to the app, put it on PATH, or set its location in Settings → Launcher hooks"
        )))
    }

    fn hook_target(
        &self,
        id: InstanceId,
    ) -> Result<(packsync_core::db::InstanceRow, std::sync::Arc<dyn packsync_core::launcher::Launcher>)> {
        let inst = self.engine().instance(id)?;
        let launcher = self.engine().launcher(&inst.launcher);
        Ok((inst, launcher))
    }

    pub fn hooks_state(&self, id: InstanceId) -> Result<HookState> {
        let (inst, l) = self.hook_target(id)?;
        l.hook_state(&inst.root)
    }

    /// What installing the hooks would change. Writes nothing.
    pub fn hooks_plan_install(&self, id: InstanceId, cli: &Path) -> Result<HookPlan> {
        let (inst, l) = self.hook_target(id)?;
        l.plan_install_hooks(&inst.root, &HookSpec { executable: cli.to_path_buf(), instance_id: id })
    }

    /// What removing the hooks would change (restoring the commands that were there before).
    pub fn hooks_plan_remove(&self, id: InstanceId) -> Result<HookPlan> {
        let (inst, l) = self.hook_target(id)?;
        l.plan_remove_hooks(&inst.root)
    }

    /// Apply a plan returned by one of the two functions above (re-validated against the file).
    pub fn hooks_apply(&self, id: InstanceId, plan: &HookPlan) -> Result<()> {
        let (inst, l) = self.hook_target(id)?;
        if dunce::canonicalize(&plan.root).ok() != dunce::canonicalize(&inst.root).ok() {
            return Err(Error::invalid("hook plan", "the plan is for a different folder than this instance"));
        }
        l.apply_hook_plan(plan)?;
        self.engine().db().event(
            Some(id),
            "info",
            "hooks_changed",
            &format!("{} change(s) applied to the launcher configuration", plan.changes.len()),
        );
        self.bus().emit(crate::events::AppEvent::InstanceChanged { instance: id });
        Ok(())
    }
}
