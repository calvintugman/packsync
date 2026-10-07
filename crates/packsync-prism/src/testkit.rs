//! Test fixtures: Prism-shaped instances and a fake `prismlauncher` executable that runs a fake game
//! (a process named `java` that sleeps, edits a file, and exits with a chosen status). No Minecraft,
//! no network, no real launcher: enough to exercise PackSync's launch tracking end to end.
//!
//! The fake understands the same command line as Prism (`--dir D --launch ID [--offline N]
//! [--profile P]`) and, like Prism, exits with the game's status, or forwards the request and exits
//! at once when `forward` is set.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use packsync_core::testkit::fixtures::{FixtureSpec, generate};

/// Create `<data_dir>/instances/<folder>` with a generated game directory, `instance.cfg` (including
/// machine-local keys that must never be synced) and `mmc-pack.json` (Minecraft 1.20.1 + Forge).
pub fn make_instance(data_dir: &Path, folder: &str, spec: &FixtureSpec) -> PathBuf {
    let root = data_dir.join("instances").join(folder);
    std::fs::create_dir_all(&root).unwrap();
    let mut spec = spec.clone();
    spec.dot_minecraft = true;
    generate(&root, &spec);
    std::fs::write(
        root.join("instance.cfg"),
        format!(
            "[General]\nConfigVersion=1.3\nInstanceType=OneSix\nname={folder}\nnotes=fixture pack\nOverrideJavaLocation=true\nJavaPath=/opt/java-17/bin/java\nOverrideMemory=true\nMinMemAlloc=512\nMaxMemAlloc=4096\nlastLaunchTime=1700000000\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("mmc-pack.json"),
        r#"{"components":[{"uid":"net.minecraft","version":"1.20.1","important":true},{"uid":"net.minecraftforge","version":"47.4.0"}],"formatVersion":1}"#,
    )
    .unwrap();
    root
}

/// What the fake game does on its next start.
#[derive(Debug, Clone)]
pub struct FakeGame {
    /// How long it runs.
    pub secs: f32,
    /// Its exit status (Prism exits 0 on success and 1 when the game crashed).
    pub exit: i32,
    /// Append `payload` to this file (relative to the game directory) while running: "playing".
    pub write: Option<(String, String)>,
    /// Leave a crash report behind.
    pub crash_report: bool,
    /// Behave like Prism forwarding to an already-running Prism: start the game in the background
    /// and exit immediately with status 0.
    pub forward: bool,
    /// Fail before any game starts (e.g. no Java).
    pub fail_early: bool,
}

impl Default for FakeGame {
    fn default() -> Self {
        Self { secs: 0.6, exit: 0, write: None, crash_report: false, forward: false, fail_early: false }
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub struct FakePrism {
    /// The fake `prismlauncher` executable.
    pub exe: PathBuf,
    /// The fake `java` (the "game process").
    pub java: PathBuf,
    data_dir: PathBuf,
}

impl FakePrism {
    /// Write the scripts into `bin_dir`; the per-launch behaviour is read from
    /// `<data_dir>/fake-game.conf` (see [`FakePrism::set`]).
    #[cfg(unix)]
    pub fn install(bin_dir: &Path, data_dir: &Path) -> FakePrism {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(bin_dir).unwrap();
        let java = bin_dir.join("java");
        std::fs::write(
            &java,
            "#!/bin/sh\nsleep \"${FAKE_SECS:-1}\"\nif [ -n \"$FAKE_WRITE\" ]; then printf '%s' \"${FAKE_PAYLOAD:-x}\" >> \"$FAKE_WRITE\"; fi\nif [ \"$FAKE_CRASH_REPORT\" = 1 ]; then mkdir -p crash-reports; echo boom > crash-reports/crash-fake-client.txt; fi\nexit \"${FAKE_EXIT:-0}\"\n",
        )
        .unwrap();
        let exe = bin_dir.join("prismlauncher");
        std::fs::write(
            &exe,
            format!(
                r#"#!/bin/sh
DIR=""; ID=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dir) DIR="$2"; shift 2;;
    --launch) ID="$2"; shift 2;;
    --offline|--profile) shift 2;;
    *) shift;;
  esac
done
[ -n "$DIR" ] || exit 2
. "$DIR/fake-game.conf"
export FAKE_SECS FAKE_EXIT FAKE_WRITE FAKE_PAYLOAD FAKE_CRASH_REPORT
ROOT="$DIR/instances/$ID"
if [ -d "$ROOT/minecraft" ]; then GAME="$ROOT/minecraft"; else GAME="$ROOT/.minecraft"; fi
[ -d "$GAME" ] || exit 3
[ "$FAKE_FAIL_EARLY" = 1 ] && exit 1
run_game() {{
  cd "$GAME" || exit 3
  {java} "-Djava.library.path=$ROOT/natives" org.prismlauncher.EntryPoint
}}
if [ "$FAKE_FORWARD" = 1 ]; then
  ( run_game >/dev/null 2>&1 & )
  exit 0
fi
run_game
exit $?
"#,
                java = sh_quote(&java.to_string_lossy())
            ),
        )
        .unwrap();
        for p in [&java, &exe] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let f = FakePrism { exe, java, data_dir: data_dir.to_path_buf() };
        f.set(&FakeGame::default());
        f
    }

    /// Windows (and anything without `/bin/sh`): the same behaviour from one small program,
    /// `packsync-fake-prism` (crate `packsync-fake-prism`), copied in as the launcher and as `java.exe`. It
    /// is found next to the test program's folder (`target/<profile>/`) or at `PACKSYNC_FAKE_PRISM`; build
    /// the workspace (`cargo test --workspace`) so it exists.
    #[cfg(not(unix))]
    pub fn install(bin_dir: &Path, data_dir: &Path) -> FakePrism {
        let prog = std::env::var_os("PACKSYNC_FAKE_PRISM").map(PathBuf::from).unwrap_or_else(|| {
            let exe = std::env::current_exe().expect("the test program's path");
            let name = format!("packsync-fake-prism{}", std::env::consts::EXE_SUFFIX);
            exe.ancestors().skip(1).take(3).map(|d| d.join(&name)).find(|p| p.is_file()).unwrap_or_else(|| {
                panic!(
                    "{name} was not found near {}: run `cargo test --workspace` (or build -p packsync-fake-prism)",
                    exe.display()
                )
            })
        });
        std::fs::create_dir_all(bin_dir).unwrap();
        let exe = bin_dir.join(format!("prismlauncher{}", std::env::consts::EXE_SUFFIX));
        let java = bin_dir.join(format!("java{}", std::env::consts::EXE_SUFFIX));
        for p in [&exe, &java] {
            std::fs::copy(&prog, p).unwrap();
        }
        let f = FakePrism { exe, java, data_dir: data_dir.to_path_buf() };
        f.set(&FakeGame::default());
        f
    }

    /// Choose what the next launch does.
    pub fn set(&self, g: &FakeGame) {
        let mut c = String::new();
        let b = |v: bool| if v { 1 } else { 0 };
        let _ = writeln!(c, "FAKE_SECS={}", g.secs);
        let _ = writeln!(c, "FAKE_EXIT={}", g.exit);
        let (w, p) = g.write.clone().unwrap_or_default();
        let _ = writeln!(c, "FAKE_WRITE={}", sh_quote(&w));
        let _ = writeln!(c, "FAKE_PAYLOAD={}", sh_quote(&p));
        let _ = writeln!(c, "FAKE_CRASH_REPORT={}", b(g.crash_report));
        let _ = writeln!(c, "FAKE_FORWARD={}", b(g.forward));
        let _ = writeln!(c, "FAKE_FAIL_EARLY={}", b(g.fail_early));
        std::fs::create_dir_all(&self.data_dir).unwrap();
        std::fs::write(self.data_dir.join("fake-game.conf"), c).unwrap();
    }
}
