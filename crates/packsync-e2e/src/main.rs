//! The browser-test world (see `world.rs`). It fakes Prism Launcher with shell scripts, so it runs on
//! Unix only; the Windows build of the workspace compiles everything else.

#[cfg(unix)]
mod world;

#[cfg(unix)]
fn main() {
    world::main()
}

#[cfg(not(unix))]
fn main() {
    eprintln!("packsync-e2e needs a Unix system: its fake Prism Launcher is a shell script");
    std::process::exit(2);
}
