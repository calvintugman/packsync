//! Smoke test against the **real Prism Launcher** (Linux only, ignored by default).
//!
//! Run it with `scripts/prism-smoke.sh`. It never touches your real Prism: everything happens in a
//! disposable data directory, with a fake offline-capable account, **no network at all** (the launcher
//! runs in an empty network namespace and reads Minecraft's metadata from a local file server), and a
//! wrapper command that stands in for Java so that Minecraft itself never starts. Only the read-only
//! caches (`meta/`, `libraries/`, `assets/`, `java/`) are copied from an existing Prism data directory
//! (reflink when the file system supports it); `accounts.json`, `*.cfg` and instances are never read.
//!
//! What it establishes about Prism's behaviour is listed in `docs/dev/prism-verification.md`.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_packsync");

struct Sandbox {
    root: PathBuf,
    data: PathBuf,
    bin: PathBuf,
}

fn run(cmd: &mut Command) -> Output {
    cmd.stdin(Stdio::null()).output().unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"))
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn seed_dir() -> PathBuf {
    std::env::var_os("PACKSYNC_PRISM_SEED").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/PrismLauncher")
    })
}

/// A Minecraft version and Java runtime that exist in the seed's caches.
fn pick_version(seed: &Path) -> Option<(&'static str, &'static str)> {
    for (mc, java) in [("1.20.1", "java-runtime-gamma"), ("1.21.1", "java-runtime-delta")] {
        if seed.join("meta/net.minecraft").join(format!("{mc}.json")).exists()
            && seed.join("libraries/com/mojang/minecraft").join(mc).exists()
            && seed.join("java").join(java).join("bin/java").exists()
        {
            return Some((mc, java));
        }
    }
    None
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Qt writes a value with quotes as `\"` pairs; wrap a command line the way instance.cfg stores it.
fn cfg_value(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

impl Sandbox {
    fn build(root: &Path, seed: &Path, mc: &str, java: &str) -> Sandbox {
        let _ = std::fs::remove_dir_all(root);
        let data = root.join("data");
        let bin = root.join("bin");
        std::fs::create_dir_all(data.join("instances")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for d in ["meta", "metacache", "libraries", "assets", "java"] {
            let st = run(Command::new("cp").args(["-r", "--reflink=auto"]).arg(seed.join(d)).arg(data.join(d)));
            assert!(st.status.success(), "copying {d}: {}", String::from_utf8_lossy(&st.stderr));
        }
        // the metadata cache must look fresh, or Prism insists on asking the network
        let mc_path = data.join("metacache");
        let mut cache: Value = serde_json::from_slice(&std::fs::read(&mc_path).unwrap()).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis().to_string();
        for e in cache["entries"].as_array_mut().unwrap() {
            if matches!(e["base"].as_str(), Some("meta" | "libraries" | "asset_indexes" | "java" | "general")) {
                e["max_age"] = "315360000".into();
                e["current_age"] = "0".into();
                e["last_changed_timestamp"] = now.clone().into();
            }
        }
        std::fs::write(&mc_path, serde_json::to_vec(&cache).unwrap()).unwrap();
        // a local "meta.prismlauncher.org": the cached metadata, served from inside the sandbox's namespace
        std::fs::create_dir_all(root.join("serve")).unwrap();
        std::os::unix::fs::symlink(data.join("meta"), root.join("serve/v1")).unwrap();
        let host = run(&mut Command::new("hostname")).stdout;
        // Prism's own settings: no first-run wizard, no downloads, metadata from the local server
        write(
            &data.join("prismlauncher.cfg"),
            &format!(
                "[General]\nApplicationTheme=system\nConfigVersion=1.3\nIconTheme=pe_colored\nIgnoreJavaWizard=true\nLanguage=en_US\nLastHostname={}\nMetaURLOverride=http://127.0.0.1:8099/v1/\nUserAskedAboutAutomaticJavaDownload=true\nAutomaticJavaDownload=false\nAutomaticJavaSwitch=false\nJavaPath={}\nWrapperCommand=\"{}\"\n",
                String::from_utf8_lossy(&host).trim(),
                data.join("java").join(java).join("bin/java").display(),
                cfg_value(&format!("\"{}\"", bin.join("wrapper.sh").display()))
            ),
        );
        write(
            &data.join("qtlogging.ini"),
            "[Rules]\n*.debug=true\nlauncher.task=true\nlauncher.task.net.download.debug=false\nlauncher.task.net.metacache=false\nqt.*.debug=false\n",
        );
        // Prism's setup wizard insists on one account that owns Minecraft; a fake one with far-future
        // tokens is accepted. With no network, refreshing it fails as "offline", which allows `--offline NAME`.
        write(
            &data.join("accounts.json"),
            r#"{"accounts":[{"type":"MSA","active":true,"entitlement":{"canPlayMinecraft":true,"ownsMinecraft":true},"profile":{"id":"7e1c0a0f6b3d3f6a9e5c7d1a2b3c4d5e","name":"Steve","skin":{"id":"","url":"","variant":""},"capes":[]},"msa-client-id":"c36a9fb6-4f2a-41ff-90bd-ae7cc92031eb","msa":{"token":"x","refresh_token":"x","iat":1700000000,"exp":4102444800},"utoken":{"token":"x","iat":1700000000,"exp":4102444800},"xrp-main":{"token":"x","iat":1700000000,"exp":4102444800},"xrp-mc":{"token":"x","iat":1700000000,"exp":4102444800},"ygg":{"token":"x","iat":1700000000,"exp":4102444800}}],"formatVersion":3}"#,
        );
        // the "game": started by the wrapper exactly where Prism would start Java, with Prism's real arguments
        executable(
            &bin.join("java"),
            r#"#!/bin/sh
D="$(cd "$(dirname "$0")/.." && pwd)"
echo "started pid=$$ cwd=$(pwd) args_have_natives=$(echo "$*" | grep -c 'java.library.path=')" >> "$D/game.log"
[ -f "$D/game-sleep" ] && sleep "$(cat "$D/game-sleep")"
[ -f "$D/game-write" ] && printf '%s' "$(cat "$D/game-write")" >> "config/example.toml"
if [ -f "$D/game-crash-report" ]; then mkdir -p crash-reports; echo boom > crash-reports/crash-smoke-client.txt; fi
[ -f "$D/game-exit" ] && exit "$(cat "$D/game-exit")"
exit 0
"#,
        );
        executable(
            &bin.join("wrapper.sh"),
            r#"#!/bin/sh
D="$(cd "$(dirname "$0")/.." && pwd)"
echo "wrapper ppid=$PPID INST_ID=$INST_ID" >> "$D/wrapper.log"
shift
exec "$D/bin/java" "$@"
"#,
        );
        executable(
            &bin.join("hook.sh"),
            r#"#!/bin/sh
# a user's own hook command: records how Prism started it
D="$(cd "$(dirname "$0")/.." && pwd)"
echo "$1: argc=$# argv=[$*] cwd=$(pwd) INST_ID=$INST_ID INST_NAME=$INST_NAME INST_DIR=$INST_DIR INST_MC_DIR=$INST_MC_DIR ppid=$PPID" >> "$D/hooks.log"
[ -f "$D/hook-$1-exit" ] && exit "$(cat "$D/hook-$1-exit")"
exit 0
"#,
        );
        // Prism in an empty network namespace, with the local metadata server next to it
        executable(
            &root.join("run-prism.sh"),
            r#"#!/bin/sh
S="$(cd "$(dirname "$0")" && pwd)"
T="$1"; shift
exec unshare --user --map-root-user --net sh -c '
  ip link set lo up
  python3 -m http.server 8099 --bind 127.0.0.1 --directory "$0/serve" >/dev/null 2>&1 &
  SRV=$!
  QT_QPA_PLATFORM=offscreen timeout "$1" prismlauncher --dir "$0/data" "${@:2}"
  RC=$?
  kill $SRV 2>/dev/null
  exit $RC
' "$S" "$T" "$@"
"#,
        );
        // what PackSync is told is "the Prism executable": it runs the sandboxed one
        executable(
            &bin.join("prism"),
            &format!("#!/bin/sh\nexec \"{}\" 600 \"$@\"\n", root.join("run-prism.sh").display()),
        );
        let _ = (mc, java);
        Sandbox { root: root.to_path_buf(), data, bin }
    }

    fn java_path(&self, java: &str) -> PathBuf {
        self.data.join("java").join(java).join("bin/java")
    }

    /// Create an instance folder Prism will accept (a space and Unicode in the name on purpose).
    fn instance(&self, folder: &str, mc: &str, java: &str, extra_cfg: &str) -> PathBuf {
        let root = self.data.join("instances").join(folder);
        std::fs::create_dir_all(root.join(".minecraft/config")).unwrap();
        std::fs::create_dir_all(root.join(".minecraft/mods")).unwrap();
        write(&root.join(".minecraft/config/example.toml"), "a = 1\n");
        let jar: Vec<u8> = (0..40_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        std::fs::write(root.join(".minecraft/mods/example-mod-1.0.jar"), jar).unwrap();
        write(
            &root.join("mmc-pack.json"),
            &format!(
                r#"{{"components":[{{"uid":"net.minecraft","version":"{mc}","important":true}}],"formatVersion":1}}"#
            ),
        );
        write(
            &root.join("instance.cfg"),
            &format!(
                "[General]\nConfigVersion=1.3\nInstanceType=OneSix\nname={folder}\nOverrideJavaLocation=true\nJavaPath={}\nOverrideCommands=true\nWrapperCommand=\"{}\"\n{extra_cfg}",
                self.java_path(java).display(),
                cfg_value(&format!("\"{}\"", self.bin.join("wrapper.sh").display())),
            ),
        );
        root
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn set(&self, flag: &str, content: &str) {
        std::fs::write(self.file(flag), content).unwrap();
    }

    fn unset(&self, flags: &[&str]) {
        for f in flags {
            let _ = std::fs::remove_file(self.file(f));
        }
    }

    fn clear_logs(&self) {
        self.unset(&["hooks.log", "wrapper.log", "game.log"]);
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.file(name)).unwrap_or_default()
    }

    /// Run Prism to completion (or until `timeout`), returning how it ended and how long it took.
    fn prism(&self, timeout: u64, args: &[&str]) -> (Option<i32>, Duration, String) {
        let t = Instant::now();
        let o = run(Command::new(self.file("run-prism.sh")).arg(timeout.to_string()).args(args));
        (
            o.status.code(),
            t.elapsed(),
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
        )
    }

    fn spawn_prism(&self, timeout: u64, args: &[&str]) -> std::process::Child {
        Command::new(self.file("run-prism.sh"))
            .arg(timeout.to_string())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}

/// One PackSync "computer" (its own config and state) using the sandbox's Prism.
struct Pk {
    config: PathBuf,
    state: PathBuf,
}

impl Pk {
    fn new(sb: &Sandbox, name: &str, cloud: &Path) -> Pk {
        let pk = Pk { config: sb.root.join(format!("{name}-config")), state: sb.root.join(format!("{name}-state")) };
        for a in [
            vec!["settings", "set", "device_name", name],
            vec!["settings", "set", "prism_data_dir", sb.data.to_str().unwrap()],
            vec!["settings", "set", "prism_executable", sb.bin.join("prism").to_str().unwrap()],
            vec!["storage", "use-local", cloud.to_str().unwrap()],
        ] {
            let r = pk.run(&a);
            assert_eq!(r.0, 0, "{a:?}: {}", r.2);
        }
        pk
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.args(["--config-dir", self.config.to_str().unwrap(), "--state-dir", self.state.to_str().unwrap()]);
        c.env("PACKSYNC_LAUNCH_POLL_MS", "100");
        c
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let o = run(self.cmd().args(args));
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut a = vec!["--json", "--yes"];
        a.extend_from_slice(args);
        let (code, out, err) = self.run(&a);
        (code, serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: not JSON ({e}): {out}\n{err}")))
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (code, v) = self.json(args);
        assert_eq!(code, 0, "{args:?}: {v}");
        v["result"].clone()
    }
}

struct Report(Vec<(String, bool, String)>);

impl Report {
    fn check(&mut self, what: &str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        println!(
            "{} {what}{}",
            if ok { "PASS" } else { "FAIL" },
            if detail.is_empty() { String::new() } else { format!("  — {detail}") }
        );
        self.0.push((what.to_string(), ok, detail));
    }
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

#[test]
#[ignore = "needs a real Prism Launcher and a seed Prism data directory: run scripts/prism-smoke.sh"]
fn real_prism_launcher_smoke() {
    let seed = seed_dir();
    for tool in ["prismlauncher", "unshare", "ip", "python3", "timeout"] {
        assert!(have(tool), "prerequisite missing: {tool}");
    }
    let (mc, java) = pick_version(&seed).unwrap_or_else(|| {
        panic!(
            "{} has no cached Minecraft 1.20.1 / 1.21.1 with its Java runtime (launch one instance in Prism once)",
            seed.display()
        )
    });
    // Prism reports canonical paths (no `..`), and the checks compare against them, so the sandbox path must be
    // canonical too — which needs the directory to exist before it is canonicalised (a fresh checkout has none).
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/prism-smoke");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let sb = Sandbox::build(&root, &seed, mc, java);
    let mut rep = Report(vec![]);
    let version =
        String::from_utf8_lossy(&run(Command::new("prismlauncher").arg("--version")).stdout).trim().to_string();
    println!("Prism: {version}; Minecraft {mc}; sandbox {}", sb.root.display());

    // ---- 1. the hook semantics Prism documents nowhere ------------------------------------------------
    let hook = sb.bin.join("hook.sh");
    let h = hook.display();
    let inst = sb.instance(
        "Smoke ünï 1",
        mc,
        java,
        &format!(
            "PreLaunchCommand=\"{}\"\nPostExitCommand=\"{}\"\n",
            cfg_value(&format!("\"{h}\" pre \"$INST_NAME\" \"two words\"")),
            cfg_value(&format!("\"{h}\" post \"$INST_ID\""))
        ),
    );
    let launch = |id: &str| sb.prism(60, &["--launch", id, "--offline", "Steve"]);

    sb.clear_logs();
    let (code, secs, _out) = launch("Smoke ünï 1");
    let (hooks, wrapper) = (sb.log("hooks.log"), sb.log("wrapper.log"));
    rep.check(
        "clean exit: Prism exits 0 and runs pre-launch, wrapper/game and post-exit in that order",
        code == Some(0) && hooks.starts_with("pre:") && hooks.contains("\npost:") && wrapper.lines().count() == 1,
        format!(
            "exit {code:?} in {secs:.1?}; hooks=[{}]",
            hooks.lines().map(|l| l.split(':').next().unwrap()).collect::<Vec<_>>().join(",")
        ),
    );
    let pre_line = hooks.lines().next().unwrap_or("");
    rep.check(
        "quoting: a path with spaces and Unicode is one program; $INST_NAME with spaces stays one argument",
        pre_line.contains("argc=3") && pre_line.contains("argv=[pre Smoke ünï 1 two words]"),
        pre_line.chars().take(90).collect::<String>(),
    );
    rep.check(
        "environment: INST_ID, INST_NAME, INST_DIR, INST_MC_DIR are set; working directory is the game folder",
        pre_line.contains("INST_ID=Smoke ünï 1")
            && pre_line.contains(&format!("INST_DIR={}", inst.display()))
            && pre_line.contains(&format!("INST_MC_DIR={}/.minecraft", inst.display()))
            && pre_line.contains(&format!("cwd={}/.minecraft", inst.display())),
        pre_line.chars().skip(60).collect::<String>(),
    );
    let game = sb.log("game.log");
    rep.check(
        "the java arguments carry -Djava.library.path=<instance>/natives (what PackSync matches the game by)",
        game.contains("args_have_natives=1"),
        game.trim().to_string(),
    );

    sb.clear_logs();
    sb.set("hook-pre-exit", "1\n");
    let (code, _, out) = sb.prism(8, &["--launch", "Smoke ünï 1", "--offline", "Steve"]);
    sb.unset(&["hook-pre-exit"]);
    rep.check(
        "a non-zero pre-launch command stops the launch (no game) and Prism stays open on the error",
        sb.log("game.log").is_empty() && out.contains("Pre-Launch command failed with code 1") && code == Some(124),
        format!("exit {code:?}"),
    );
    for (n, label) in [("1", "1"), ("7", "7")] {
        sb.clear_logs();
        sb.set("game-exit", n);
        let (code, _, out) = sb.prism(8, &["--launch", "Smoke ünï 1", "--offline", "Steve"]);
        sb.unset(&["game-exit"]);
        rep.check(
            &format!("game exit status {label}: post-exit is NOT run (\"Game crashed.\")"),
            !sb.log("hooks.log").contains("post:")
                && sb.log("hooks.log").contains("pre:")
                && out.contains("Game crashed"),
            format!("Prism exit {code:?}"),
        );
    }
    sb.clear_logs();
    sb.set("hook-post-exit", "1\n");
    let (_, _, out) = sb.prism(8, &["--launch", "Smoke ünï 1", "--offline", "Steve"]);
    sb.unset(&["hook-post-exit"]);
    rep.check(
        "a failing post-exit command is reported by Prism as a failed launch",
        out.contains("Post-Launch command failed with code 1"),
        "",
    );

    // ---- 2. a second --launch while Prism is running is forwarded -------------------------------------
    let _inst2 = sb.instance("Smoke ünï 2", mc, java, "");
    sb.clear_logs();
    sb.set("game-sleep", "6");
    let mut first = sb.spawn_prism(60, &["--launch", "Smoke ünï 1", "--offline", "Steve"]);
    wait_until("the first game to start", 30, || sb.log("game.log").lines().count() == 1);
    let prism_pids: Vec<u32> = String::from_utf8_lossy(
        &run(Command::new("pgrep").args(["-f", &format!("^prismlauncher --dir {}", sb.data.display())])).stdout,
    )
    .split_whitespace()
    .filter_map(|p| p.parse().ok())
    .collect();
    let (code, secs, _) = sb.prism(30, &["--launch", "Smoke ünï 2", "--offline", "Alex"]);
    wait_until("the forwarded game to start", 30, || sb.log("game.log").lines().count() == 2);
    let wl = sb.log("wrapper.log");
    let ppids: Vec<String> =
        wl.lines().filter_map(|l| l.split("ppid=").nth(1)?.split_whitespace().next().map(str::to_string)).collect();
    rep.check(
        "a second launch while Prism runs is forwarded: it exits 0 at once and the running Prism starts the game",
        code == Some(0)
            && secs < Duration::from_secs(5)
            && ppids.len() == 2
            && ppids[0] == ppids[1]
            && prism_pids.iter().any(|p| p.to_string() == ppids[0]),
        format!("exit {code:?} after {secs:.2?}; wrapper parents {ppids:?}; prism pids {prism_pids:?}"),
    );
    let _ = first.wait();
    sb.unset(&["game-sleep"]);

    // ---- 3. PackSync with the real launcher ------------------------------------------------------------
    let cloud = sb.file("cloud");
    std::fs::create_dir_all(&cloud).unwrap();
    let pk = Pk::new(&sb, "desktop", &cloud);
    let found = pk.ok(&["instances"]);
    rep.check(
        "discovery lists the real Prism instances (names, game folder, Minecraft version)",
        found
            .as_array()
            .map(|a| {
                a.len() == 2
                    && a.iter().any(|d| {
                        d["name"] == "Smoke ünï 1"
                            && d["game_root"] == ".minecraft"
                            && d["identity"]["minecraft_version"] == mc
                    })
            })
            .unwrap_or(false),
        "",
    );
    let en = pk.ok(&["enable", inst.to_str().unwrap()]);
    let id = en["outcome"]["instance"].as_str().unwrap().to_string();
    let cfg_before = std::fs::read_to_string(inst.join("instance.cfg")).unwrap();

    // while Prism runs, editing instance.cfg is refused
    sb.clear_logs();
    sb.set("game-sleep", "5");
    let mut running = sb.spawn_prism(60, &["--launch", "Smoke ünï 2", "--offline", "Steve"]);
    wait_until("Prism to run", 30, || sb.log("game.log").lines().count() >= 1);
    let (_, plan) = pk.json(&["hooks", "install", &id, "--cli", BIN, "--plan"]);
    let blockers = plan["result"]["blockers"].as_array().map(|a| a.len()).unwrap_or(0);
    rep.check(
        "hook installation is refused while the real Prism runs (its --dir is recognised)",
        blockers == 1,
        format!("blockers: {}", plan["result"]["blockers"]),
    );
    let _ = running.wait();
    sb.unset(&["game-sleep"]);
    sb.clear_logs();

    // install → play through Prism → hooks sync → uninstall
    pk.ok(&["hooks", "install", &id, "--cli", BIN]);
    sb.set("game-write", "+through-prism\n");
    let (code, _, _) = launch("Smoke ünï 1");
    sb.unset(&["game-write"]);
    let hooks = sb.log("hooks.log");
    rep.check(
        "PackSync's hooks run under Prism and chain the user's own commands (both ran, with the right arguments)",
        code == Some(0)
            && hooks.contains("pre: argc=3 argv=[pre Smoke ünï 1 two words]")
            && hooks.contains("post: argc=2"),
        format!("exit {code:?}"),
    );
    wait_until("the background upload after post-exit", 30, || {
        let s = pk.ok(&["status", &id, "--online"]);
        matches!(s["instances"][0]["phase"].as_str(), Some("ready_on_other_devices" | "cloud_published"))
            && pk.ok(&["history", &id]).as_array().map(|a| a.len()).unwrap_or(0) == 2
    });
    rep.check(
        "the post-exit hook saved the session and uploaded it in the background (2 snapshots, up to date)",
        true,
        "",
    );
    // a second computer receives it
    let laptop = Pk::new(&sb, "laptop", &cloud);
    let restored = laptop.ok(&["restore-new", "--name", "Smoke ünï restored"]);
    let rroot = PathBuf::from(restored["root"].as_str().unwrap());
    let got = std::fs::read_to_string(rroot.join(".minecraft/config/example.toml")).unwrap_or_default();
    rep.check(
        "another computer restores the progress made through Prism into a new instance",
        got.ends_with("+through-prism"),
        format!("{got:?}"),
    );
    sb.clear_logs();
    let (code, _, _) = launch("Smoke ünï restored");
    rep.check(
        "Prism launches the restored instance (the instance PackSync registered is valid for the real launcher)",
        code == Some(0) && sb.log("game.log").lines().count() == 1,
        format!("exit {code:?}"),
    );

    // a crashed session: no post-exit, so the next launch is refused until the user decides
    sb.clear_logs();
    sb.set("game-exit", "1");
    sb.set("game-write", "+half-written\n");
    sb.set("game-crash-report", "1");
    let (_, _, _) = sb.prism(8, &["--launch", "Smoke ünï 1", "--offline", "Steve"]);
    sb.unset(&["game-exit", "game-write", "game-crash-report"]);
    sb.clear_logs();
    let (_, _, out) = sb.prism(8, &["--launch", "Smoke ünï 1", "--offline", "Steve"]);
    rep.check(
        "after a crash (no post-exit) the next launch is refused by PackSync's pre-launch hook with a clear reason",
        sb.log("game.log").is_empty() && out.contains("Pre-Launch command failed with code 19"),
        "exit status 19 = a crash waiting for a decision (the reason is printed by the hook and shown by `packsync crash show`)",
    );
    let status = pk.ok(&["status", &id]);
    rep.check(
        "PackSync shows the crash waiting for a decision",
        status["instances"][0]["phase"] == "crash_pending",
        format!("{}", status["instances"][0]["phase"]),
    );
    pk.ok(&["crash", "promote", &id]);
    sb.clear_logs();
    let (code, _, _) = launch("Smoke ünï 1");
    rep.check(
        "after the decision the launch goes through again",
        code == Some(0) && sb.log("game.log").lines().count() == 1,
        format!("exit {code:?}"),
    );

    pk.ok(&["hooks", "uninstall", &id]);
    let cfg_after = std::fs::read_to_string(inst.join("instance.cfg")).unwrap();
    let keys = |t: &str| {
        t.lines()
            .filter(|l| {
                ["OverrideCommands=", "PreLaunchCommand=", "PostExitCommand=", "WrapperCommand="]
                    .iter()
                    .any(|k| l.starts_with(k))
            })
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    rep.check("uninstalling the hooks restores the user's original commands exactly (Prism had rewritten the file in between)", {
        let b = keys(&cfg_before);
        let a = keys(&cfg_after);
        // Prism re-serialises the file after launches; compare what the user typed, key by key, after normalising
        let norm = |v: &Vec<String>| v.iter().map(|l| l.replace('\\', "")).collect::<std::collections::BTreeSet<_>>();
        norm(&a) == norm(&b)
    }, "");

    // PackSync starts the game itself through the real launcher: the game is found by the process table
    sb.clear_logs();
    sb.set("game-sleep", "3");
    sb.set("game-write", "+started-by-packsync\n");
    let (code, out) = {
        let (c, o, e) = pk.run(&["--json", "--yes", "launch", &id, "--offline-player", "Steve"]);
        (c, serde_json::from_str::<Value>(&o).unwrap_or_else(|_| serde_json::json!({ "raw": o, "err": e })))
    };
    sb.unset(&["game-sleep", "game-write"]);
    let end = &out["result"]["end"];
    rep.check(
        "`packsync launch` through the real Prism finds the game process, sees Prism's exit status 0 and saves + uploads",
        code == 0 && out["result"]["game_started"] == true && out["result"]["launcher_exit_code"] == 0 && end["outcome"] == "clean" && end["uploaded"] == true,
        format!("exit {code}: {}", serde_json::to_string(&out["result"]).unwrap_or_default().chars().take(300).collect::<String>()),
    );

    // report
    let failed: Vec<_> = rep.0.iter().filter(|(_, ok, _)| !ok).collect();
    let mut md = format!(
        "# Real Prism Launcher smoke test\n\nPrism `{version}`, Minecraft {mc}, run on {}\n\n",
        std::env::consts::OS
    );
    for (w, ok, d) in &rep.0 {
        md.push_str(&format!(
            "- {} {w}{}\n",
            if *ok { "PASS" } else { "FAIL" },
            if d.is_empty() { String::new() } else { format!(" — `{d}`") }
        ));
    }
    let _ = std::fs::write(sb.file("report.md"), md);
    let _ = &sb.bin;
    assert!(
        failed.is_empty(),
        "{} check(s) failed: {:?}",
        failed.len(),
        failed.iter().map(|(w, _, _)| w).collect::<Vec<_>>()
    );
}
