//! `packsync-bench`: reproducible benchmarks. Always run with `--release`; see `docs/performance.md`.

mod hw;
mod lab;
mod report;
mod scenarios;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use lab::Backend;
use scenarios::{RunOpts, Size};

#[derive(Parser)]
#[command(name = "packsync-bench", about = "Benchmarks of the PackSync engine on generated instances")]
struct Cli {
    /// Working directory for fixtures, the fake cloud and engine state. Use a real disk, not tmpfs.
    #[arg(long, global = true, default_value = "target/bench")]
    dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print what the numbers would be measured on.
    Hardware,
    /// Initial sync, no-change, small edit, region edit, mod replacement, restore — with bytes, requests, memory.
    Run {
        /// tiny (≈2 MB, smoke), small (≈50 MB), medium (≈0.75 GB) or large (≈4 GB).
        #[arg(long, default_value = "small")]
        size: String,
        /// local (a directory) or mock-drive (the in-process Drive mock: counts real Drive HTTP requests).
        #[arg(long, default_value = "local")]
        storage: String,
        /// Region-file block size in bytes (default: the engine's default).
        #[arg(long)]
        region_block: Option<u32>,
        #[arg(long)]
        upload_concurrency: Option<usize>,
        /// Skip comparing restored trees with the source (faster for the large fixture).
        #[arg(long)]
        no_verify: bool,
        /// Write results here (JSON) and next to it (Markdown).
        #[arg(long, default_value = "target/bench/results.json")]
        out: PathBuf,
        /// Keep the generated files afterwards.
        #[arg(long)]
        keep: bool,
    },
    /// Compare chunking strategies for region files on identical data and edits.
    Chunking {
        /// Use the larger region set (≈430 MB) instead of ≈90 MB.
        #[arg(long)]
        big: bool,
        #[arg(long, default_value = "target/bench/chunking.json")]
        out: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.dir).expect("cannot create the work directory");
    let hw = hw::detect(&cli.dir);
    match cli.cmd {
        Cmd::Hardware => println!("{}", serde_json::to_string_pretty(&hw).unwrap()),
        Cmd::Run { size, storage, region_block, upload_concurrency, no_verify, out, keep } => {
            let size = Size::parse(&size).unwrap_or_else(|e| fail(&e));
            let backend = Backend::parse(&storage).unwrap_or_else(|e| fail(&e));
            if cfg!(debug_assertions) {
                eprintln!(
                    "warning: this is a debug build; use `cargo run -p packsync-bench --release` for meaningful numbers"
                );
            }
            let suite = scenarios::run_suite(&RunOpts {
                size,
                backend,
                work: cli.dir.clone(),
                region_block,
                upload_concurrency,
                verify: !no_verify,
                keep,
            });
            let doc = serde_json::json!({ "hardware": hw, "suite": suite });
            if let Some(d) = out.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            std::fs::write(&out, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
            let md = format!("{}\n{}", report::hardware_markdown(&hw), report::suite_markdown(&hw, &suite));
            std::fs::write(out.with_extension("md"), &md).unwrap();
            println!("\n{md}");
            if !keep {
                let _ = std::fs::remove_dir_all(cli.dir.join("instance"));
                let _ = std::fs::remove_dir_all(cli.dir.join("lab"));
            }
        }
        Cmd::Chunking { big, out } => {
            let res = scenarios::chunking_experiment(&cli.dir, big);
            let doc = serde_json::json!({ "hardware": hw, "chunking": res });
            if let Some(d) = out.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            std::fs::write(&out, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
            let md = format!("{}\n{}", report::hardware_markdown(&hw), report::chunking_markdown(&res));
            std::fs::write(out.with_extension("md"), &md).unwrap();
            println!("\n{md}");
            for d in ["chunk-instance", "chunk-lab", "chunk-restored"] {
                let _ = std::fs::remove_dir_all(cli.dir.join(d));
            }
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
