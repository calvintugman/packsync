//! Output, progress and confirmations. Machine-readable output goes to stdout only; everything meant
//! for a human who is watching (progress, hints, warnings) goes to stderr.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use packsync_core::error::{Error, Result};
use packsync_core::progress::{CancelToken, EventKind, Level, OpCtx, Phase, ProgressEvent, ProgressSink};
use parking_lot::Mutex;
use serde::Serialize;

static CANCEL: OnceLock<CancelToken> = OnceLock::new();

/// Ctrl-C asks the running operation to stop at its next safe point; a second Ctrl-C ends the process
/// (every operation is built to survive that: see docs/consistency.md).
pub fn cancel_token() -> CancelToken {
    CANCEL
        .get_or_init(|| {
            let token = CancelToken::new();
            let t = token.clone();
            let _ = ctrlc::set_handler(move || {
                if t.is_cancelled() {
                    eprintln!("\nstopping immediately; run `packsync status` to check the state");
                    std::process::exit(130);
                }
                eprintln!("\nstopping at the next safe point (press Ctrl-C again to stop immediately)");
                t.cancel();
            });
            token
        })
        .clone()
}

#[derive(Clone)]
pub struct Out {
    pub json: bool,
    pub quiet: bool,
    pub yes: bool,
}

impl Out {
    /// Print a result: JSON document (`{"ok":true,"result":..}`) or the human rendering.
    pub fn emit<T: Serialize>(
        &self,
        value: &T,
        human: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
    ) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        let r = if self.json {
            let doc = serde_json::json!({ "ok": true, "result": value });
            serde_json::to_writer_pretty(&mut stdout, &doc).map_err(std::io::Error::from).and_then(|_| writeln!(stdout))
        } else {
            human(&mut stdout)
        };
        r.map_err(|e| Error::io("writing output", e))
    }

    /// A line for the person watching (stderr), unless `--quiet`.
    pub fn note(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            eprintln!("{}", msg.as_ref());
        }
    }

    pub fn warn(&self, msg: impl AsRef<str>) {
        if !self.quiet || self.json {
            eprintln!("warning: {}", msg.as_ref());
        }
    }

    /// Ask before doing something that changes or discards data.
    pub fn confirm(&self, question: &str) -> Result<bool> {
        if self.yes {
            return Ok(true);
        }
        if !std::io::stdin().is_terminal() || self.json {
            return Err(Error::invalid(
                "confirmation",
                format!("{question} — re-run with --yes to confirm (no terminal to ask on)"),
            ));
        }
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|e| Error::io("reading the answer", e))?;
        Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    pub fn ctx(&self) -> OpCtx {
        let sink: Arc<dyn ProgressSink> =
            if self.quiet { Arc::new(packsync_core::progress::NullSink) } else { Arc::new(StderrSink::new()) };
        OpCtx::new(sink, cancel_token())
    }
}

/// Phase changes and warnings as lines; byte/item progress as one updating line on a terminal.
pub struct StderrSink {
    tty: bool,
    last: Mutex<Instant>,
    drawn: Mutex<bool>,
}

impl StderrSink {
    pub fn new() -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
            drawn: Mutex::new(false),
        }
    }
    fn clear(&self) {
        let mut d = self.drawn.lock();
        if *d {
            eprint!("\r\x1b[2K");
            *d = false;
        }
    }
}

impl Default for StderrSink {
    fn default() -> Self {
        Self::new()
    }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Discovery => "looking at the cloud",
        Phase::Scan => "scanning",
        Phase::Snapshot => "saving snapshot",
        Phase::Upload => "uploading",
        Phase::Publish => "publishing",
        Phase::Reconcile => "comparing with the cloud",
        Phase::StageDownload => "downloading",
        Phase::Validate => "verifying",
        Phase::Apply => "applying",
        Phase::Recover => "recovering",
        Phase::Verify => "verifying",
        Phase::Cleanup => "cleaning up",
        Phase::Conflict => "conflict",
        Phase::Launch => "launch",
        Phase::Idle => "idle",
    }
}

impl ProgressSink for StderrSink {
    fn emit(&self, ev: ProgressEvent) {
        match ev.kind {
            EventKind::Phase => {
                self.clear();
                eprintln!("→ {}", ev.message);
            }
            EventKind::Message { level } => {
                self.clear();
                match level {
                    Level::Info => eprintln!("  {}", ev.message),
                    Level::Warn => eprintln!("warning: {}", ev.message),
                    Level::Error => eprintln!("error: {}", ev.message),
                }
            }
            EventKind::Progress => {
                if !self.tty {
                    return;
                }
                let mut last = self.last.lock();
                if last.elapsed() < Duration::from_millis(150) {
                    return;
                }
                *last = Instant::now();
                let mut line = format!("  {}", phase_name(ev.phase));
                if ev.items_total > 0 {
                    line.push_str(&format!(" {}/{}", ev.items_done, ev.items_total));
                }
                if ev.bytes_total > 0 {
                    line.push_str(&format!(
                        "  {} / {}",
                        crate::render::bytes(ev.bytes_done),
                        crate::render::bytes(ev.bytes_total)
                    ));
                }
                if !ev.message.is_empty() {
                    let m: String = ev.message.chars().take(50).collect();
                    line.push_str(&format!("  {m}"));
                }
                eprint!("\r\x1b[2K{line}");
                *self.drawn.lock() = true;
                let _ = std::io::stderr().flush();
            }
        }
    }
}
