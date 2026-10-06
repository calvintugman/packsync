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
    let code = match commands::run(cli) {
        Ok(code) => code,
        Err(e) => commands::report_error(&e, json),
    };
    std::process::exit(code);
}
