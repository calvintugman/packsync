//! The `packsync` command-line tool. See `docs/cli.md`.

mod args;
mod commands;
mod render;
mod resolve;
mod ui;

use clap::Parser;

fn main() {
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
    std::process::exit(code);
}
