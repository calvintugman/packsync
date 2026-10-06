//! Opt-in launcher hooks: preview → apply → remove, additive and reversible.
//!
//! What gets written (instance level, in `instance.cfg`):
//!
//! ```text
//! OverrideCommands=true
//! PreLaunchCommand="<packsync>" hook pre-launch --instance <uuid> --marker packsync-hook [-- <original>]
//! PostExitCommand="<packsync>" hook post-exit  --instance <uuid> --marker packsync-hook [-- <original>]
//! ```
//!
//! * **Existing user commands are chained, never replaced**: the original command line is appended
//!   after `--` verbatim (Prism tokenises the whole string, so the CLI receives the original as plain
//!   argv after `--` and runs it itself).
//! * Prism only honours instance commands when `OverrideCommands` is on, and then uses the instance's
//!   values for *all three* of pre-launch / wrapper / post-exit. If that switch was off, the launcher
//!   wide commands would silently stop applying, so they are **copied into the instance** (wrapper) or
//!   **chained** (pre/post) and shown in the plan.
//! * The file is edited line-by-line (every other byte untouched), written atomically, and the
//!   original raw lines are saved in a receipt so removal restores them byte for byte.
//! * Prism rewrites `instance.cfg` from memory: while it runs, installation is blocked.
//! * Applying re-checks that the file still looks as it did when the plan was made.

use std::path::{Path, PathBuf};

use packsync_core::brand::{HOOK_MARKER, LOCAL_MARKER_DIR};
use packsync_core::error::{Error, Result};
use packsync_core::launcher::{HookChange, HookPlan, HookSpec, HookState};
use serde::{Deserialize, Serialize};

use crate::cmdline::{chain_suffix, quote_arg, tokenize};
use crate::discovery::{INSTANCE_CFG, read_instance_cfg};
use crate::instance::CfgView;
use crate::qtini::{IniDoc, KeyEdit, apply_edits};

pub const KEY_PRE: &str = "PreLaunchCommand";
pub const KEY_POST: &str = "PostExitCommand";
pub const KEY_WRAPPER: &str = "WrapperCommand";
pub const KEY_OVERRIDE: &str = "OverrideCommands";
const KEY_OVERRIDE_LEGACY: &str = "OverrideLaunchCmd";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Pre,
    Post,
}

impl HookKind {
    fn word(&self) -> &'static str {
        match self {
            HookKind::Pre => "pre-launch",
            HookKind::Post => "post-exit",
        }
    }
}

/// A command line that PackSync wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OurHook {
    pub exe: String,
    pub kind: HookKind,
    pub instance: String,
    /// The user's original command, verbatim.
    pub chained: Option<String>,
}

/// Recognise a command line written by [`build_command`].
pub fn parse_ours(cmd: &str) -> Option<OurHook> {
    let toks: Vec<String> = tokenize(cmd).into_iter().map(|t| t.text).collect();
    let marker_at = toks.iter().position(|t| t == "--marker")?;
    if toks.get(marker_at + 1).map(|s| s.as_str()) != Some(HOOK_MARKER)
        || toks.get(1).map(|s| s.as_str()) != Some("hook")
    {
        return None;
    }
    let kind = match toks.get(2)?.as_str() {
        "pre-launch" => HookKind::Pre,
        "post-exit" => HookKind::Post,
        _ => return None,
    };
    let inst_at = toks.iter().position(|t| t == "--instance")?;
    Some(OurHook {
        exe: toks[0].clone(),
        kind,
        instance: toks.get(inst_at + 1)?.clone(),
        chained: chain_suffix(cmd).map(|s| s.to_string()),
    })
}

pub fn build_command(spec: &HookSpec, kind: HookKind, chained: Option<&str>) -> Result<String> {
    let exe = spec
        .executable
        .to_str()
        .ok_or_else(|| Error::invalid("hook command", "the PackSync executable path is not valid UTF-8"))?;
    if !spec.executable.is_absolute() {
        return Err(Error::invalid("hook command", "the PackSync executable path must be absolute"));
    }
    let mut s =
        format!("{} hook {} --instance {} --marker {HOOK_MARKER}", quote_arg(exe)?, kind.word(), spec.instance_id);
    for a in &spec.extra_args {
        s.push(' ');
        s.push_str(&quote_arg(a)?);
    }
    if let Some(c) = chained.map(|c| c.trim()).filter(|c| !c.is_empty()) {
        s.push_str(" -- ");
        s.push_str(c);
    }
    Ok(s)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SavedKey {
    /// The raw line as found (`None` = the key was absent).
    raw: Option<String>,
    logical: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    instance: String,
    saved: std::collections::BTreeMap<String, SavedKey>,
}

fn receipt_path(root: &Path) -> PathBuf {
    root.join(LOCAL_MARKER_DIR).join("hooks.json")
}

fn cfg_path(root: &Path) -> PathBuf {
    root.join(INSTANCE_CFG)
}

/// The commands Prism uses for launches that are *not* instance-level overridden.
#[derive(Debug, Default, Clone)]
pub struct GlobalCommands {
    pub pre: Option<String>,
    pub wrapper: Option<String>,
    pub post: Option<String>,
}

/// Read only the three command keys from a launcher-wide `prismlauncher.cfg`; everything else in
/// that file (which may hold API keys) is dropped unread.
pub fn read_global_commands(data_dir: &Path) -> GlobalCommands {
    let Ok(Some(bytes)) = crate::files::read_bounded_opt(&data_dir.join(crate::discovery::LAUNCHER_CFG), 1024 * 1024)
    else {
        return GlobalCommands::default();
    };
    let Ok(view) = CfgView::parse(&bytes) else { return GlobalCommands::default() };
    let get = |k: &str| view.string(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    GlobalCommands { pre: get(KEY_PRE), wrapper: get(KEY_WRAPPER), post: get(KEY_POST) }
}

fn override_on(view: &CfgView<'_>) -> bool {
    view.flag(KEY_OVERRIDE) || view.flag(KEY_OVERRIDE_LEGACY)
}

fn val(view: &CfgView<'_>, key: &str) -> Option<String> {
    view.string(key).filter(|s| !s.is_empty())
}

pub fn state(root: &Path) -> Result<HookState> {
    let bytes = read_instance_cfg(root)?;
    let view = CfgView::parse(&bytes)?;
    let pre = val(&view, KEY_PRE);
    let post = val(&view, KEY_POST);
    let (p, q) = (pre.as_deref().and_then(parse_ours), post.as_deref().and_then(parse_ours));
    let installed =
        p.as_ref().is_some_and(|h| h.kind == HookKind::Pre) && q.as_ref().is_some_and(|h| h.kind == HookKind::Post);
    let intact = installed
        && override_on(&view)
        && p.as_ref().zip(q.as_ref()).is_some_and(|(a, b)| a.instance == b.instance && a.exe == b.exe);
    Ok(HookState { installed, intact, pre_launch: pre, post_exit: post })
}

fn change(file: &Path, key: &str, before: Option<String>, after: Option<String>) -> Option<HookChange> {
    if before == after {
        return None;
    }
    Some(HookChange { file: file.to_path_buf(), key: key.to_string(), before, after })
}

/// Compute what installing the hooks would change. Nothing is written.
pub fn plan_install(root: &Path, spec: &HookSpec, launcher_running: bool, global: &GlobalCommands) -> Result<HookPlan> {
    let bytes = read_instance_cfg(root)?;
    let view = CfgView::parse(&bytes)?;
    let file = cfg_path(root);
    let mut blockers = vec![];
    let mut preserved = vec![];
    if launcher_running {
        blockers.push("Prism Launcher is running. Close it first: it rewrites instance.cfg from memory and would undo or corrupt the change.".to_string());
    }
    let override_before = override_on(&view);
    let (cur_pre, cur_post, cur_wrap) = (val(&view, KEY_PRE), val(&view, KEY_POST), val(&view, KEY_WRAPPER));

    // What to chain: the user's own instance commands; if the instance did not override commands, the
    // launcher-wide ones that would otherwise stop applying.
    let chain_for = |kind: HookKind| -> Option<String> {
        let (cur, glob) = match kind {
            HookKind::Pre => (&cur_pre, &global.pre),
            HookKind::Post => (&cur_post, &global.post),
        };
        let base = if override_before { cur.clone() } else { glob.clone() };
        match base.as_deref().and_then(parse_ours) {
            Some(o) => o.chained, // re-installing: keep the existing chain, never nest our own hook
            None => base,
        }
    };
    let (chain_pre, chain_post) = (chain_for(HookKind::Pre), chain_for(HookKind::Post));
    if let Some(c) = &chain_pre {
        preserved.push(format!("pre-launch: {c}"));
    }
    if let Some(c) = &chain_post {
        preserved.push(format!("post-exit: {c}"));
    }
    // Wrapper: Prism would use the instance's (empty) wrapper once the override is on.
    let wrapper_after = if override_before { cur_wrap.clone() } else { global.wrapper.clone().or(cur_wrap.clone()) };
    if !override_before && global.wrapper.is_some() {
        preserved.push(format!(
            "wrapper command (copied from Prism's global settings so it keeps applying): {}",
            global.wrapper.as_deref().unwrap_or("")
        ));
    }
    let new_pre = build_command(spec, HookKind::Pre, chain_pre.as_deref());
    let new_post = build_command(spec, HookKind::Post, chain_post.as_deref());
    let (new_pre, new_post) = match (new_pre, new_post) {
        (Ok(a), Ok(b)) => (Some(a), Some(b)),
        (a, b) => {
            for e in [a.err(), b.err()].into_iter().flatten() {
                blockers.push(e.to_string());
            }
            (None, None)
        }
    };
    let mut changes = vec![];
    changes.extend(
        change(&file, KEY_OVERRIDE, view.string(KEY_OVERRIDE), Some("true".into())).filter(|_| !override_before),
    );
    changes.extend(change(&file, KEY_PRE, cur_pre.clone(), new_pre));
    changes.extend(change(&file, KEY_POST, cur_post.clone(), new_post));
    if wrapper_after != cur_wrap {
        changes.extend(change(&file, KEY_WRAPPER, cur_wrap, wrapper_after));
    }
    Ok(HookPlan { root: root.to_path_buf(), changes, preserved_commands: preserved, blockers })
}

/// Compute what removing the hooks would change (restoring what was there before). Nothing is written.
pub fn plan_remove(root: &Path, launcher_running: bool) -> Result<HookPlan> {
    let bytes = read_instance_cfg(root)?;
    let view = CfgView::parse(&bytes)?;
    let file = cfg_path(root);
    let mut blockers = vec![];
    if launcher_running {
        blockers.push("Prism Launcher is running. Close it first: it rewrites instance.cfg from memory.".to_string());
    }
    let receipt: Option<Receipt> = std::fs::read(receipt_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let mut changes = vec![];
    let mut preserved = vec![];
    for (key, kind) in [(KEY_PRE, HookKind::Pre), (KEY_POST, HookKind::Post)] {
        let cur = val(&view, key);
        let Some(ours) = cur.as_deref().and_then(parse_ours).filter(|h| h.kind == kind) else { continue };
        let after = match &receipt {
            Some(r) => r.saved.get(key).and_then(|s| s.logical.clone()),
            None => ours.chained.clone(),
        };
        if let Some(a) = &after {
            preserved.push(format!("{}: {a}", kind.word()));
        }
        changes.extend(change(&file, key, cur, after));
    }
    if let Some(r) = &receipt {
        // restore the override switch and any copied wrapper to what they were
        for key in [KEY_OVERRIDE, KEY_WRAPPER] {
            if let Some(saved) = r.saved.get(key) {
                changes.extend(change(&file, key, view.string(key), saved.logical.clone()));
            }
        }
    }
    Ok(HookPlan { root: root.to_path_buf(), changes, preserved_commands: preserved, blockers })
}

/// Apply a plan made by [`plan_install`] / [`plan_remove`]. Re-validates the current file first.
pub fn apply(plan: &HookPlan, launcher_running: bool) -> Result<()> {
    if launcher_running {
        return Err(Error::Busy("Prism Launcher is running; close it before changing hooks".into()));
    }
    if !plan.blockers.is_empty() {
        return Err(Error::Busy(plan.blockers.join("; ")));
    }
    if plan.changes.is_empty() {
        return Ok(());
    }
    let root = &plan.root;
    let bytes = read_instance_cfg(root)?;
    let view = CfgView::parse(&bytes)?;
    // the file must still look exactly as it did when the plan was made
    for c in &plan.changes {
        let now = view.string(&c.key).filter(|s| !(s.is_empty() && c.before.is_none()));
        if now != c.before {
            return Err(Error::Conflict(format!(
                "instance.cfg changed since the plan was made ({} is now {:?}); review the new plan",
                c.key, now
            )));
        }
    }
    let installing = plan.changes.iter().any(|c| c.after.as_deref().and_then(parse_ours).is_some());
    let doc = IniDoc::parse(&bytes)?;
    let raw_of = |key: &str| -> Option<String> {
        doc.find(key)
            .into_iter()
            .find(|e| e.section.is_empty())
            .map(|e| String::from_utf8_lossy(doc.raw_line(e)).trim_end_matches(['\r', '\n']).to_string())
    };
    let old_receipt: Option<Receipt> =
        std::fs::read(receipt_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let mut edits = vec![];
    let mut receipt = Receipt { version: 1, instance: String::new(), saved: Default::default() };
    for c in &plan.changes {
        if installing {
            receipt.saved.insert(c.key.clone(), SavedKey { raw: raw_of(&c.key), logical: c.before.clone() });
            if let Some(ours) = c.after.as_deref().and_then(parse_ours) {
                receipt.instance = ours.instance;
            }
        }
        match (&c.after, installing) {
            (None, _) => edits.push(KeyEdit::Remove { key: c.key.clone() }),
            // restoring: use the original raw line from the receipt when it matches the plan
            (Some(v), false) => match old_receipt
                .as_ref()
                .and_then(|r| r.saved.get(&c.key))
                .filter(|s| s.logical.as_deref() == Some(v.as_str()))
                .and_then(|s| s.raw.clone())
            {
                Some(raw) => edits.push(KeyEdit::SetRaw { key: c.key.clone(), line: raw }),
                None => edits.push(KeyEdit::Set { key: c.key.clone(), value: v.clone() }),
            },
            (Some(v), true) => edits.push(KeyEdit::Set { key: c.key.clone(), value: v.clone() }),
        }
    }
    let new_bytes = apply_edits(&bytes, &edits)?;
    // The edited file must read back to exactly the planned values (checked with the Qt-faithful reader).
    let check = CfgView::parse(&new_bytes)?;
    for c in &plan.changes {
        let got = check.string(&c.key).filter(|s| !s.is_empty());
        if got != c.after.clone().filter(|s| !s.is_empty()) {
            return Err(Error::Invalid {
                what: "instance.cfg edit",
                detail: format!("{} would not read back as planned; nothing was written", c.key),
            });
        }
    }
    // backup, then atomic write
    let backup_dir = root.join(LOCAL_MARKER_DIR).join("hooks-backup");
    std::fs::create_dir_all(&backup_dir).map_err(|e| Error::io("creating the hooks backup folder", e))?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    std::fs::write(backup_dir.join(format!("instance.cfg.{stamp}")), &bytes)
        .map_err(|e| Error::io("backing up instance.cfg", e))?;
    if installing {
        packsync_core::fsutil::write_atomic(&receipt_path(root), &serde_json::to_vec_pretty(&receipt)?)
            .map_err(|e| Error::io("saving the hook receipt", e))?;
    }
    packsync_core::fsutil::write_atomic(&cfg_path(root), &new_bytes)
        .map_err(|e| Error::io("writing instance.cfg", e))?;
    if !installing {
        let _ = std::fs::remove_file(receipt_path(root));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use packsync_core::ids::InstanceId;

    fn spec(exe: &str) -> HookSpec {
        HookSpec { executable: PathBuf::from(exe), instance_id: InstanceId::new(), extra_args: vec![] }
    }

    fn instance(cfg: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("ünï inst");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("instance.cfg"), cfg).unwrap();
        (d, root)
    }

    const PLAIN: &str = "[General]\nConfigVersion=1.3\nInstanceType=OneSix\nname=Pack\nnotes=hi\n\n[UI]\nmods_Page\\Columns=\"AAAA/wAAAAEA=\"\n";

    #[test]
    fn install_is_previewable_idempotent_and_reversible_byte_for_byte() {
        let (_d, root) = instance(PLAIN);
        let s = spec("/opt/My PackSync/packsync");
        let plan = plan_install(&root, &s, false, &GlobalCommands::default()).unwrap();
        assert!(plan.blockers.is_empty());
        let keys: Vec<_> = plan.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["OverrideCommands", "PreLaunchCommand", "PostExitCommand"]);
        // nothing written by planning
        assert_eq!(std::fs::read_to_string(root.join("instance.cfg")).unwrap(), PLAIN);
        apply(&plan, false).unwrap();
        let after = std::fs::read_to_string(root.join("instance.cfg")).unwrap();
        assert!(
            after.contains("hook pre-launch") && after.contains("[UI]\nmods_Page\\Columns=\"AAAA/wAAAAEA=\""),
            "other lines untouched:\n{after}"
        );
        let st = state(&root).unwrap();
        assert!(st.installed && st.intact, "{st:?}");
        // the stored command reads back as the exact argv we intended, spaces and Unicode included
        let pre = st.pre_launch.unwrap();
        let argv = crate::cmdline::split_command(&pre);
        assert_eq!(&argv[..4], ["/opt/My PackSync/packsync", "hook", "pre-launch", "--instance"]);
        // idempotent: planning again changes nothing
        let again = plan_install(&root, &s, false, &GlobalCommands::default()).unwrap();
        assert!(again.changes.is_empty(), "{again:?}");
        // removal restores the original bytes exactly
        let rm = plan_remove(&root, false).unwrap();
        apply(&rm, false).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("instance.cfg")).unwrap(), PLAIN);
        assert!(!state(&root).unwrap().installed);
    }

    #[test]
    fn existing_user_commands_are_chained_and_restored_exactly() {
        // exactly how Prism's writer (Qt) stores a value that contains quotes: wrapped, with \" escapes
        let cfg = "[General]\nInstanceType=OneSix\nOverrideCommands=true\nPostExitCommand=\"\\\"/usr/bin/notify send\\\" \\\"game over\\\"\"\nPreLaunchCommand=/home/u/my pre.sh --fast\nname=X\n";
        let (_d, root) = instance(cfg);
        let s = spec("/opt/packsync");
        let plan = plan_install(&root, &s, false, &GlobalCommands::default()).unwrap();
        assert_eq!(plan.preserved_commands.len(), 2, "{plan:?}");
        apply(&plan, false).unwrap();
        let st = state(&root).unwrap();
        let (pre, post) = (st.pre_launch.unwrap(), st.post_exit.unwrap());
        assert_eq!(parse_ours(&pre).unwrap().chained.as_deref(), Some("/home/u/my pre.sh --fast"));
        assert_eq!(parse_ours(&post).unwrap().chained.as_deref(), Some("\"/usr/bin/notify send\" \"game over\""));
        // installing twice never nests our hook inside itself
        let again = plan_install(&root, &s, false, &GlobalCommands::default()).unwrap();
        assert!(again.changes.is_empty());
        let rm = plan_remove(&root, false).unwrap();
        apply(&rm, false).unwrap();
        let restored = std::fs::read_to_string(root.join("instance.cfg")).unwrap();
        assert_eq!(restored, cfg, "user commands come back byte for byte");
    }

    #[test]
    fn global_commands_keep_applying_when_the_instance_starts_overriding() {
        let (_d, root) = instance(PLAIN);
        let g = GlobalCommands {
            pre: Some("/usr/bin/echo before".into()),
            wrapper: Some("gamemoderun".into()),
            post: None,
        };
        let plan = plan_install(&root, &spec("/opt/packsync"), false, &g).unwrap();
        assert!(
            plan.changes.iter().any(|c| c.key == "WrapperCommand" && c.after.as_deref() == Some("gamemoderun")),
            "{plan:?}"
        );
        apply(&plan, false).unwrap();
        let cfg = std::fs::read_to_string(root.join("instance.cfg")).unwrap();
        assert!(cfg.contains("WrapperCommand=gamemoderun") && cfg.contains("-- /usr/bin/echo before"), "{cfg}");
        apply(&plan_remove(&root, false).unwrap(), false).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("instance.cfg")).unwrap(), PLAIN);
    }

    #[test]
    fn blockers_and_stale_plans_are_refused() {
        let (_d, root) = instance(PLAIN);
        let s = spec("/opt/packsync");
        // Prism running → blocker, and apply refuses even with a hand-made plan
        let running = plan_install(&root, &s, true, &GlobalCommands::default()).unwrap();
        assert!(!running.blockers.is_empty());
        assert!(apply(&running, false).is_err());
        assert!(apply(&plan_install(&root, &s, false, &GlobalCommands::default()).unwrap(), true).is_err());
        // an executable path that cannot be quoted safely is a blocker, not a mangled command
        for bad in ["/opt/pack\"sync/packsync", "/opt/$HOME/packsync", "relative/packsync"] {
            let p = plan_install(&root, &spec(bad), false, &GlobalCommands::default()).unwrap();
            assert!(!p.blockers.is_empty(), "{bad}");
        }
        // the file changed between plan and apply
        let plan = plan_install(&root, &s, false, &GlobalCommands::default()).unwrap();
        std::fs::write(
            root.join("instance.cfg"),
            PLAIN.replace("name=Pack\n", "name=Pack\nPreLaunchCommand=/bin/true\n"),
        )
        .unwrap();
        let e = apply(&plan, false).unwrap_err();
        assert!(matches!(e, Error::Conflict(_)), "{e}");
    }

    #[test]
    fn windows_style_paths_survive_prisms_ini_escaping_and_tokenizer() {
        let (_d, root) = instance(PLAIN);
        let mut s = spec("/placeholder");
        s.executable = PathBuf::from(r"C:\Program Files\PackSync\packsync.exe");
        // `is_absolute` is false for this on Linux, so build the command directly the way Windows would
        let cmd = format!(
            "{} hook pre-launch --instance {} --marker {HOOK_MARKER}",
            quote_arg(&s.executable.to_string_lossy()).unwrap(),
            s.instance_id
        );
        let edited = apply_edits(
            &std::fs::read(root.join("instance.cfg")).unwrap(),
            &[KeyEdit::Set { key: KEY_PRE.into(), value: cmd.clone() }],
        )
        .unwrap();
        std::fs::write(root.join("instance.cfg"), &edited).unwrap();
        let st = state(&root).unwrap();
        assert_eq!(st.pre_launch.as_deref(), Some(cmd.as_str()), "the INI value round-trips");
        assert_eq!(crate::cmdline::split_command(&cmd)[0], r"C:\Program Files\PackSync\packsync.exe");
    }
}
