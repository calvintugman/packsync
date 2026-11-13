//! `PrismLauncher`: the [`Launcher`] implementation for Prism Launcher.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use packsync_core::error::{Error, Result};
use packsync_core::launcher::*;
use packsync_core::mixed::MixedPolicy;

use crate::discovery::{INSTANCE_CFG, instances_dir, list_instance_dirs};
use crate::env::{FLATPAK_APP_ID, Os, PrismEnv, same_dir};
use crate::hooks;
use crate::instance::{CfgView, game_root_name, summarize_leniently, validate_instance};
use crate::process::{ProcessSource, SystemProcesses, processes_for};
use crate::qtini::{KeyEdit, apply_edits};
use crate::rules::{RULES_FRAGMENT, instance_cfg_policy};

pub const KIND: &str = "prism";

pub struct PrismLauncher {
    env: PrismEnv,
    data_dirs: Option<Vec<PathBuf>>,
    executable: Option<PathBuf>,
    procs: Arc<dyn ProcessSource>,
}

impl Default for PrismLauncher {
    fn default() -> Self {
        Self::new()
    }
}

/// How to start Prism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exe {
    Native(PathBuf),
    Flatpak,
}

/// A fully built launch command (no shell involved anywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl PrismLauncher {
    pub fn new() -> Self {
        Self { env: PrismEnv::from_process(), data_dirs: None, executable: None, procs: Arc::new(SystemProcesses) }
    }

    /// Use exactly these data directories instead of discovering them (tests, `--dir` style setups).
    pub fn with_data_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.data_dirs = Some(dirs);
        self
    }
    pub fn with_executable(mut self, exe: PathBuf) -> Self {
        self.executable = Some(exe);
        self
    }
    pub fn with_env(mut self, env: PrismEnv) -> Self {
        self.env = env;
        self
    }
    pub fn with_process_source(mut self, p: Arc<dyn ProcessSource>) -> Self {
        self.procs = p;
        self
    }

    /// Existing data directories, most specific first.
    pub fn data_dirs(&self) -> Vec<PathBuf> {
        let cands = match &self.data_dirs {
            Some(d) => d.clone(),
            None => self.env.data_dir_candidates(self.executable.as_deref()),
        };
        let mut out: Vec<PathBuf> = vec![];
        for c in cands {
            if c.is_dir() && !out.iter().any(|o| same_dir(o, &c)) {
                out.push(c);
            }
        }
        out
    }

    pub fn find_exe(&self) -> Option<Exe> {
        if let Some(e) = &self.executable {
            return Some(Exe::Native(e.clone()));
        }
        let names: &[&str] = match self.env.os {
            Os::Windows => &["prismlauncher.exe", "PrismLauncher.exe"],
            _ => &["prismlauncher", "PrismLauncher"],
        };
        for d in &self.env.path_dirs {
            for n in names {
                let p = d.join(n);
                if p.is_file() {
                    return Some(Exe::Native(p));
                }
            }
        }
        match self.env.os {
            Os::Windows => {
                if let Some(l) = &self.env.localappdata {
                    let p = l.join("Programs").join("PrismLauncher").join("prismlauncher.exe");
                    if p.is_file() {
                        return Some(Exe::Native(p));
                    }
                }
            }
            Os::MacOs => {
                let p = PathBuf::from("/Applications/Prism Launcher.app/Contents/MacOS/prismlauncher");
                if p.is_file() {
                    return Some(Exe::Native(p));
                }
            }
            Os::Linux => {
                if self.env.flatpak_data_dir().is_some_and(|d| d.is_dir())
                    && self.env.path_dirs.iter().any(|d| d.join("flatpak").is_file())
                {
                    return Some(Exe::Flatpak);
                }
            }
        }
        None
    }

    fn data_dir_for_instance_root(&self, root: &Path) -> Option<PathBuf> {
        let parent = root.parent()?;
        self.data_dirs().into_iter().find(|d| same_dir(&instances_dir(d), parent))
    }

    fn describe_inner(&self, root: &Path, strict: bool) -> Result<DiscoveredInstance> {
        let folder = root.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let sum = if strict { validate_instance(root)? } else { summarize_leniently(root, &folder) };
        let game_dir = root.join(sum.game_root);
        Ok(DiscoveredInstance {
            launcher_ref: LauncherRef {
                launcher: KIND.into(),
                data_dir: self.data_dir_for_instance_root(root),
                instance_id: Some(folder),
                executable: match self.find_exe() {
                    Some(Exe::Native(p)) => Some(p),
                    _ => None,
                },
            },
            root: root.to_path_buf(),
            name: sum.name,
            game_root: sum.game_root.to_string(),
            identity: sum.identity,
            worlds: packsync_core::worlds::list_worlds(&game_dir),
        })
    }

    /// The command to start `req` (separated from spawning so it can be tested exactly).
    pub fn launch_command(&self, req: &LaunchRequest) -> Result<LaunchCommand> {
        let id = req
            .launcher_ref
            .instance_id
            .clone()
            .or_else(|| req.root.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()))
            .ok_or_else(|| Error::invalid("launcher reference", "no Prism instance ID (folder name)"))?;
        let exe = self.find_exe().ok_or_else(|| {
            Error::NotFound(
                "Prism Launcher was not found. Install it, or set its location in PackSync's settings".into(),
            )
        })?;
        let mut args: Vec<String> = vec![];
        let program = match &exe {
            Exe::Native(p) => {
                if let Some(d) = &req.launcher_ref.data_dir {
                    args.push("--dir".into());
                    args.push(d.to_string_lossy().into_owned());
                }
                p.clone()
            }
            Exe::Flatpak => {
                args.push("run".into());
                args.push(FLATPAK_APP_ID.into());
                PathBuf::from("flatpak")
            }
        };
        args.push("--launch".into());
        args.push(id);
        if let Some(n) = &req.offline_name {
            args.push("--offline".into());
            args.push(n.clone());
        }
        if let Some(p) = &req.profile {
            args.push("--profile".into());
            args.push(p.clone());
        }
        Ok(LaunchCommand { program, args, env: req.env.clone() })
    }
}

fn safe_folder_name(name: &str) -> String {
    let mut s: String =
        name.chars()
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
    s = s.trim().trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    if s.chars().count() > 100 {
        s = s.chars().take(100).collect::<String>().trim_end().to_string();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if s.is_empty() {
        "instance".into()
    } else if reserved {
        format!("_{s}")
    } else {
        s
    }
}

fn unique_folder(dir: &Path, preferred: &str) -> String {
    let base = safe_folder_name(preferred);
    let taken: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
        .unwrap_or_default();
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name.to_lowercase()) {
        name = format!("{base} ({n})");
        n += 1;
    }
    name
}

/// `mmc-pack.json` without what Prism derives and refreshes itself: every field whose name starts with
/// `cached` (`cachedName`, `cachedVersion`, `cachedRequires`, `cachedVolatile`, …), and formatting.
fn components_without_caches(bytes: &[u8]) -> Option<serde_json::Value> {
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.retain(|k, _| !k.starts_with("cached"));
                m.values_mut().for_each(strip);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    strip(&mut v);
    Some(v)
}

impl Launcher for PrismLauncher {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn self_maintained_change(&self, path: &str, base: &[u8], local: &[u8]) -> bool {
        path == crate::discovery::PACK_JSON
            && matches!(
                (components_without_caches(base), components_without_caches(local)),
                (Some(a), Some(b)) if a == b
            )
    }

    fn discover(&self) -> Result<Vec<DiscoveredInstance>> {
        let mut out = vec![];
        for dd in self.data_dirs() {
            for e in list_instance_dirs(&instances_dir(&dd)) {
                let mut d = self.describe_inner(&e.path, false)?;
                d.launcher_ref.data_dir = Some(dd.clone());
                d.launcher_ref.instance_id = Some(e.id);
                out.push(d);
            }
        }
        Ok(out)
    }

    fn describe(&self, root: &Path) -> Result<DiscoveredInstance> {
        self.describe_inner(root, true)
    }

    fn rules_fragment(&self) -> &'static str {
        RULES_FRAGMENT
    }

    fn mixed_policies(&self) -> Vec<MixedPolicy> {
        vec![instance_cfg_policy()]
    }

    fn local_settings(&self, root: &Path) -> Result<LocalSettings> {
        let bytes = crate::discovery::read_instance_cfg(root)?;
        Ok(CfgView::parse(&bytes)?.local_settings())
    }

    fn register_restored(&self, req: &RegisterRequest) -> Result<RegisteredInstance> {
        let data_dir = req
            .target
            .as_ref()
            .and_then(|t| t.data_dir.clone())
            .or_else(|| self.data_dirs().into_iter().next())
            .ok_or_else(|| {
                Error::Config(
                    "Prism Launcher's data folder was not found. Start Prism once, or choose where to restore".into(),
                )
            })?;
        let inst_dir = instances_dir(&data_dir);
        std::fs::create_dir_all(&inst_dir).map_err(|e| Error::io(format!("creating {}", inst_dir.display()), e))?;
        let parent = req
            .staged_root
            .parent()
            .ok_or_else(|| Error::invalid("restore location", "the staged folder has no parent"))?;
        if !same_dir(parent, &inst_dir) {
            return Err(Error::Unsupported(format!(
                "the restored instance is staged in {}, but Prism's instances folder is {}; it must be staged inside it so that it can be moved into place atomically (never copied)",
                parent.display(),
                inst_dir.display()
            )));
        }
        if !req.staged_root.join("mmc-pack.json").is_file() {
            return Err(Error::Corrupt(
                "the restored instance has no mmc-pack.json; the snapshot is incomplete".into(),
            ));
        }
        // 1. make instance.cfg valid for this machine
        let cfg_file = req.staged_root.join(INSTANCE_CFG);
        let cur = std::fs::read(&cfg_file).unwrap_or_default();
        let view_has_type =
            CfgView::parse(&cur).ok().and_then(|v| v.string("InstanceType")).is_some_and(|s| !s.is_empty());
        let mut edits = vec![KeyEdit::Set { key: "name".into(), value: req.display_name.clone() }];
        if !view_has_type {
            edits.push(KeyEdit::Set { key: "InstanceType".into(), value: "OneSix".into() });
        }
        let mut notes = vec![];
        if let Some(j) = req.local.java_path.as_ref().filter(|j| !j.is_empty()) {
            edits.push(KeyEdit::Set { key: "OverrideJavaLocation".into(), value: "true".into() });
            edits.push(KeyEdit::Set { key: "JavaPath".into(), value: j.clone() });
        } else {
            notes.push("Java: Prism will use its global Java setting for this instance. Set a Java path in the instance's settings if the pack needs a specific version.".into());
        }
        if req.local.min_memory_mb.is_some() || req.local.max_memory_mb.is_some() {
            edits.push(KeyEdit::Set { key: "OverrideMemory".into(), value: "true".into() });
            if let Some(m) = req.local.min_memory_mb {
                edits.push(KeyEdit::Set { key: "MinMemAlloc".into(), value: m.to_string() });
            }
            if let Some(m) = req.local.max_memory_mb {
                edits.push(KeyEdit::Set { key: "MaxMemAlloc".into(), value: m.to_string() });
            }
        } else {
            notes.push("Memory: this computer's default allocation applies. Large modpacks often need 6-12 GB; adjust it in the instance's settings.".into());
        }
        if let Some(a) = req.local.jvm_args.as_ref().filter(|a| !a.is_empty()) {
            edits.push(KeyEdit::Set { key: "OverrideJavaArgs".into(), value: "true".into() });
            edits.push(KeyEdit::Set { key: "JvmArgs".into(), value: a.clone() });
        }
        let new_cfg = apply_edits(&cur, &edits)?;
        packsync_core::fsutil::write_atomic(&cfg_file, &new_cfg)
            .map_err(|e| Error::io("writing the restored instance.cfg", e))?;
        // 2. choose a name nobody uses and move the folder into place with one atomic rename
        let folder = unique_folder(&inst_dir, &req.preferred_folder);
        let dest = inst_dir.join(&folder);
        if dest.exists() {
            return Err(Error::Busy(format!("{} appeared while restoring; try again", dest.display())));
        }
        std::fs::rename(&req.staged_root, &dest).map_err(|e| {
            if e.raw_os_error() == Some(18) {
                Error::Unsupported("the staging folder and Prism's instances folder are on different file systems; PackSync will not copy a half-finished instance into place".into())
            } else {
                Error::io(format!("moving the restored instance to {}", dest.display()), e)
            }
        })?;
        if self.is_launcher_running(&LauncherRef {
            launcher: KIND.into(),
            data_dir: Some(data_dir.clone()),
            instance_id: None,
            executable: None,
        }) {
            notes.push("Prism is running: the new instance appears after Prism refreshes its list (restart it if it does not).".into());
        }
        Ok(RegisteredInstance {
            launcher_ref: LauncherRef {
                launcher: KIND.into(),
                data_dir: Some(data_dir),
                instance_id: Some(folder),
                executable: None,
            },
            game_root: game_root_name(&dest).to_string(),
            root: dest,
            notes,
        })
    }

    fn launch(&self, req: &LaunchRequest) -> Result<LaunchHandle> {
        let lc = self.launch_command(req)?;
        let forwarded = self.is_launcher_running(&req.launcher_ref);
        let mut cmd = Command::new(&lc.program);
        cmd.args(&lc.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for (k, v) in &lc.env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().map_err(|e| Error::io(format!("starting {}", lc.program.display()), e))?;
        let pid = child.id();
        Ok(LaunchHandle { child: Some(child), launcher_pid: Some(pid), forwarded_to_running_launcher: forwarded })
    }

    fn is_launcher_running(&self, r: &LauncherRef) -> bool {
        !processes_for(&*self.procs, &self.env, r.data_dir.as_deref()).is_empty()
    }

    fn game_process_patterns(&self, root: &Path) -> Vec<String> {
        // Only the instance-specific natives path: the generic `org.prismlauncher.EntryPoint` main class
        // appears on *every* Prism game, so using it as a match would make a game running from one
        // instance block syncing of all the others.
        vec![format!("-Djava.library.path={}", root.join("natives").display())]
    }

    fn hook_state(&self, root: &Path) -> Result<HookState> {
        hooks::state(root)
    }

    fn plan_install_hooks(&self, root: &Path, spec: &HookSpec) -> Result<HookPlan> {
        let dd = self.data_dir_for_instance_root(root);
        let running = dd
            .as_ref()
            .map(|d| {
                self.is_launcher_running(&LauncherRef {
                    launcher: KIND.into(),
                    data_dir: Some(d.clone()),
                    instance_id: None,
                    executable: None,
                })
            })
            .unwrap_or_else(|| {
                // unknown data dir: be conservative if any Prism is running at all
                !processes_for(&*self.procs, &self.env, None).is_empty()
            });
        let global = dd.as_deref().map(hooks::read_global_commands).unwrap_or_default();
        hooks::plan_install(root, spec, running, &global)
    }

    fn apply_hook_plan(&self, plan: &HookPlan) -> Result<()> {
        let running =
            !processes_for(&*self.procs, &self.env, self.data_dir_for_instance_root(&plan.root).as_deref()).is_empty();
        hooks::apply(plan, running)
    }

    fn plan_remove_hooks(&self, root: &Path) -> Result<HookPlan> {
        let dd = self.data_dir_for_instance_root(root);
        let running = !processes_for(&*self.procs, &self.env, dd.as_deref()).is_empty();
        hooks::plan_remove(root, running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcInfo;
    use packsync_core::ids::InstanceId;

    struct NoProcs;
    impl ProcessSource for NoProcs {
        fn prism_processes(&self) -> Vec<ProcInfo> {
            vec![]
        }
    }
    struct OneProc(PathBuf);
    impl ProcessSource for OneProc {
        fn prism_processes(&self) -> Vec<ProcInfo> {
            vec![ProcInfo {
                pid: 42,
                name: "prismlauncher".into(),
                cmd: vec!["prismlauncher".into(), "--dir".into(), self.0.to_string_lossy().into_owned()],
                ..Default::default()
            }]
        }
    }

    fn data_dir(tmp: &Path) -> PathBuf {
        let dd = tmp.join("Prism Data ünï");
        std::fs::create_dir_all(dd.join("instances")).unwrap();
        dd
    }

    fn make_instance(dd: &Path, folder: &str, game: &str) -> PathBuf {
        let r = dd.join("instances").join(folder);
        std::fs::create_dir_all(r.join(game).join("saves/World One")).unwrap();
        std::fs::write(r.join("instance.cfg"), "[General]\nInstanceType=OneSix\nname=My Pack\n").unwrap();
        std::fs::write(
            r.join("mmc-pack.json"),
            r#"{"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.minecraftforge","version":"47.4.0"}],"formatVersion":1}"#,
        )
        .unwrap();
        r
    }

    fn launcher(dd: &Path) -> PrismLauncher {
        PrismLauncher::new()
            .with_data_dirs(vec![dd.to_path_buf()])
            .with_process_source(Arc::new(NoProcs))
            .with_executable(PathBuf::from("/usr/bin/prismlauncher"))
    }

    #[test]
    fn discovers_and_describes_instances_with_spaces_unicode_and_both_game_folders() {
        let t = tempfile::tempdir().unwrap();
        let dd = data_dir(t.path());
        make_instance(&dd, "GregTech Easy(1)", ".minecraft");
        make_instance(&dd, "ünï cödé pack", "minecraft");
        std::fs::create_dir_all(dd.join("instances/.packsync-restore-abc")).unwrap();
        let l = launcher(&dd);
        let found = l.discover().unwrap();
        assert_eq!(found.len(), 2);
        let g = found.iter().find(|d| d.launcher_ref.instance_id.as_deref() == Some("GregTech Easy(1)")).unwrap();
        assert_eq!(g.game_root, ".minecraft");
        assert_eq!(g.identity.minecraft_version.as_deref(), Some("1.20.1"));
        assert_eq!(g.identity.loader.as_ref().unwrap().kind, "forge");
        assert_eq!(g.worlds.len(), 0, "no level.dat in the fixture");
        let d = l.describe(&g.root).unwrap();
        assert_eq!(d.launcher_ref.data_dir.as_deref(), Some(dd.as_path()));
        assert_eq!(d.name, "My Pack");
        // selecting the game folder gives an instruction, not a vague error
        let e = l.describe(&g.root.join(".minecraft")).unwrap_err().to_string();
        assert!(e.contains("Select the instance folder"), "{e}");
    }

    #[test]
    fn register_restored_adapts_settings_never_overwrites_and_renames_atomically() {
        let t = tempfile::tempdir().unwrap();
        let dd = data_dir(t.path());
        let existing = make_instance(&dd, "My Pack", ".minecraft");
        std::fs::write(existing.join("keep.txt"), b"existing instance").unwrap();
        let staged = dd.join("instances/.packsync-restore-1234");
        std::fs::create_dir_all(staged.join(".minecraft")).unwrap();
        std::fs::write(staged.join("instance.cfg"), "[General]\nInstanceType=OneSix\nManagedPack=true\n").unwrap();
        std::fs::write(staged.join("mmc-pack.json"), b"{}").unwrap();
        let l = launcher(&dd);
        let local = LocalSettings {
            java_path: Some("/opt/java 21/bin/java".into()),
            min_memory_mb: Some(1024),
            max_memory_mb: Some(8192),
            jvm_args: None,
        };
        let reg = l
            .register_restored(&RegisterRequest {
                staged_root: staged.clone(),
                target: None,
                display_name: "My Pack".into(),
                preferred_folder: "My Pack".into(),
                local,
                instance_id: InstanceId::new(),
            })
            .unwrap();
        assert_eq!(reg.root.file_name().unwrap(), "My Pack (2)", "an existing instance is never overwritten");
        assert!(!staged.exists() && reg.root.join("mmc-pack.json").exists());
        assert_eq!(std::fs::read(existing.join("keep.txt")).unwrap(), b"existing instance");
        let cfg = std::fs::read_to_string(reg.root.join("instance.cfg")).unwrap();
        for want in [
            "JavaPath=/opt/java 21/bin/java",
            "OverrideJavaLocation=true",
            "MaxMemAlloc=8192",
            "OverrideMemory=true",
            "name=My Pack",
            "ManagedPack=true",
        ] {
            assert!(cfg.contains(want), "{want} missing:\n{cfg}");
        }
        assert_eq!(l.local_settings(&reg.root).unwrap().max_memory_mb, Some(8192));
        // the staging folder must be inside the instances folder
        let elsewhere = t.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("mmc-pack.json"), b"{}").unwrap();
        let e = l
            .register_restored(&RegisterRequest {
                staged_root: elsewhere,
                target: None,
                display_name: "x".into(),
                preferred_folder: "x".into(),
                local: Default::default(),
                instance_id: InstanceId::new(),
            })
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e}");
    }

    #[test]
    fn folder_names_are_made_safe() {
        for (input, want) in [
            ("a/b\\c:d", "a_b_c_d"),
            ("  ..hidden. ", "hidden"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("", "instance"),
            ("ünï 日本語 🎮", "ünï 日本語 🎮"),
        ] {
            assert_eq!(safe_folder_name(input), want, "{input:?}");
        }
        assert!(safe_folder_name(&"x".repeat(300)).chars().count() <= 100);
    }

    #[test]
    fn launch_command_is_an_argument_vector_with_dir_offline_and_profile() {
        let t = tempfile::tempdir().unwrap();
        let dd = data_dir(t.path());
        let l = launcher(&dd);
        let req = LaunchRequest {
            launcher_ref: LauncherRef {
                launcher: KIND.into(),
                data_dir: Some(dd.clone()),
                instance_id: Some("My Pack ünï".into()),
                executable: None,
            },
            root: dd.join("instances/My Pack ünï"),
            offline_name: Some("Steve".into()),
            profile: Some("work".into()),
            env: vec![("PACKSYNC_SESSION".into(), "t1".into())],
        };
        let c = l.launch_command(&req).unwrap();
        assert_eq!(c.program, PathBuf::from("/usr/bin/prismlauncher"));
        assert_eq!(
            c.args,
            ["--dir", dd.to_str().unwrap(), "--launch", "My Pack ünï", "--offline", "Steve", "--profile", "work"]
                .map(String::from)
                .to_vec(),
            "paths with spaces/Unicode are single arguments; no shell, no quoting"
        );
        assert_eq!(c.env, vec![("PACKSYNC_SESSION".to_string(), "t1".to_string())]);
        // a running Prism with the same --dir is detected (the launch will be forwarded to it)
        let running = PrismLauncher::new()
            .with_data_dirs(vec![dd.clone()])
            .with_process_source(Arc::new(OneProc(dd.clone())))
            .with_executable("/usr/bin/prismlauncher".into());
        assert!(running.is_launcher_running(&req.launcher_ref));
        assert!(!launcher(&dd).is_launcher_running(&req.launcher_ref));
        // …and one that uses another data dir is not
        let other = PrismLauncher::new()
            .with_data_dirs(vec![dd.clone()])
            .with_process_source(Arc::new(OneProc(t.path().join("elsewhere"))))
            .with_executable("/usr/bin/prismlauncher".into());
        assert!(!other.is_launcher_running(&req.launcher_ref));
    }

    #[test]
    fn hooks_through_the_trait_are_blocked_while_prism_runs() {
        let t = tempfile::tempdir().unwrap();
        let dd = data_dir(t.path());
        let root = make_instance(&dd, "pack", ".minecraft");
        let spec = HookSpec {
            executable: PathBuf::from(if cfg!(windows) { r"C:\opt\packsync" } else { "/opt/packsync" }),
            instance_id: InstanceId::new(),
            extra_args: vec![],
        };
        let running =
            PrismLauncher::new().with_data_dirs(vec![dd.clone()]).with_process_source(Arc::new(OneProc(dd.clone())));
        let plan = running.plan_install_hooks(&root, &spec).unwrap();
        assert!(!plan.blockers.is_empty());
        assert!(running.apply_hook_plan(&plan).is_err());
        let idle = launcher(&dd);
        let plan = idle.plan_install_hooks(&root, &spec).unwrap();
        idle.apply_hook_plan(&plan).unwrap();
        assert!(idle.hook_state(&root).unwrap().intact);
        idle.apply_hook_plan(&idle.plan_remove_hooks(&root).unwrap()).unwrap();
        assert!(!idle.hook_state(&root).unwrap().installed);
        let pats = idle.game_process_patterns(&root);
        assert_eq!(pats.len(), 1, "no generic pattern that would match other instances' games: {pats:?}");
        assert!(pats[0].ends_with("/natives") || pats[0].ends_with("\\natives"), "{pats:?}");
    }
}
