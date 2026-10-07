//! Test support: a stand-in for Prism Launcher **and** for the game it starts, as one small program, so
//! PackSync's launch tracking can be tested where shell scripts cannot run (Windows). Never shipped.
//!
//! `packsync_prism::testkit::FakePrism` copies this program twice: as the launcher (any name) and as
//! `java` (the "game process"). It behaves like the shell-script fake used on Unix:
//!
//! * launcher: `--dir D --launch ID [--offline N] [--profile P]`; reads `D/fake-game.conf` (`KEY=VALUE`,
//!   values in shell single quotes) and starts `java -Djava.library.path=<instance>/natives
//!   org.prismlauncher.EntryPoint` in the instance's game folder, then exits with the game's status — or, when
//!   `FAKE_FORWARD=1`, starts it in the background and exits 0 at once (Prism forwarding to a running Prism);
//! * game: sleeps `FAKE_SECS`, appends `FAKE_PAYLOAD` to `FAKE_WRITE` (relative to the game folder), leaves a
//!   crash report when `FAKE_CRASH_REPORT=1`, exits with `FAKE_EXIT`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::time::Duration;

fn main() {
    let me = std::env::current_exe().unwrap_or_else(|_| exit(90));
    let stem = me.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if stem == "java" || stem == "javaw" { game() } else { launcher(&me) }
}

fn var(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

fn game() -> ! {
    let secs: f32 = var("FAKE_SECS").parse().unwrap_or(1.0);
    std::thread::sleep(Duration::from_secs_f32(secs.max(0.0)));
    let target = var("FAKE_WRITE");
    if !target.is_empty() {
        let p = PathBuf::from(&target);
        if let Some(d) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let payload = std::env::var("FAKE_PAYLOAD").unwrap_or_else(|_| "x".into());
            let _ = f.write_all(payload.as_bytes());
        }
    }
    if var("FAKE_CRASH_REPORT") == "1" {
        let _ = std::fs::create_dir_all("crash-reports");
        let _ = std::fs::write("crash-reports/crash-fake-client.txt", "boom\n");
    }
    exit(var("FAKE_EXIT").parse().unwrap_or(0))
}

/// `KEY=VALUE` lines; a value in shell single quotes is unquoted (`'\''` is a quote).
fn read_conf(path: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
                v[1..v.len() - 1].replace("'\\''", "'")
            } else {
                v.to_string()
            };
            (k.trim().to_string(), v)
        })
        .collect()
}

fn launcher(me: &Path) -> ! {
    let (mut dir, mut id) = (None::<PathBuf>, None::<String>);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            "--launch" => id = args.next(),
            "--offline" | "--profile" => {
                args.next();
            }
            _ => {}
        }
    }
    let Some(dir) = dir else { exit(2) };
    let conf = read_conf(&dir.join("fake-game.conf"));
    let get = |k: &str| conf.get(k).cloned().unwrap_or_default();
    let root = dir.join("instances").join(id.unwrap_or_default());
    let game_dir = if root.join("minecraft").is_dir() { root.join("minecraft") } else { root.join(".minecraft") };
    if !game_dir.is_dir() {
        exit(3);
    }
    if get("FAKE_FAIL_EARLY") == "1" {
        exit(1);
    }
    let java = me.with_file_name(format!("java{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = Command::new(java);
    cmd.arg(format!("-Djava.library.path={}", root.join("natives").display()))
        .arg("org.prismlauncher.EntryPoint")
        .current_dir(&game_dir);
    for k in ["FAKE_SECS", "FAKE_EXIT", "FAKE_WRITE", "FAKE_PAYLOAD", "FAKE_CRASH_REPORT"] {
        cmd.env(k, get(k));
    }
    if get("FAKE_FORWARD") == "1" {
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let _ = cmd.spawn();
        exit(0);
    }
    match cmd.status() {
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(_) => exit(4),
    }
}
