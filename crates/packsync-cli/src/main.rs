//! The `packsync` command-line tool. See `docs/cli.md`.

mod args;
mod ask;
mod commands;
mod render;
mod resolve;
mod ui;

use clap::Parser;

/// Measurements for `docs/benchmarks.md`: when `PACKSYNC_TRANSFER_STATS` names a file, one JSON line is
/// appended to it per run with the wall-clock time, the exit code and what really went over HTTP to
/// Google Drive (requests by kind, body bytes up and down, retries), labelled with
/// `PACKSYNC_TRANSFER_LABEL`. Never written to stdout, so `--json` output is unchanged; contains no
/// names, paths or secrets.
fn record_transfer_stats(started: std::time::Instant, code: i32) {
    let Some(path) = std::env::var_os("PACKSYNC_TRANSFER_STATS") else { return };
    let line = serde_json::json!({
        "label": std::env::var("PACKSYNC_TRANSFER_LABEL").ok(),
        "exit": code,
        "wall_ms": started.elapsed().as_millis() as u64,
        "drive": packsync_app::provider::drive_request_stats(),
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn main() {
    let started = std::time::Instant::now();
    let cli = args::Cli::parse();
    let json = cli.json;
    // diagnostics go to a redacted log file, never to stdout (that is the machine-readable channel)
    let _log = packsync_app::logging::init(&commands::paths_from(&cli).logs_dir, cli.verbose);
    let code = match commands::run(cli) {
        Ok(code) => {
            if let Some(result) = ui::take_pending_result() {
                ui::print_json(&serde_json::json!({ "ok": true, "result": result }));
            }
            code
        }
        Err(e) => commands::report_error(&e, json),
    };
    record_transfer_stats(started, code);
    std::process::exit(code);
}
