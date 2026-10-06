//! Diagnostic logging to a file in the state directory, safe by construction:
//!
//! * every line is passed through the redactor before it is written (tokens, client secrets, upload
//!   session IDs, `Authorization` values);
//! * third-party network and credential crates are pinned to `warn` whatever the user asks for —
//!   `ureq` prints whole request and response headers at `debug`, which would put `Authorization:
//!   Bearer …` and resumable-upload session URLs into a log people paste into bug reports;
//! * the file is rotated at start-up so it cannot grow without bound.
//!
//! `PACKSYNC_LOG` takes `tracing` filter directives (`packsync_core=debug`); directives naming the
//! pinned crates are ignored with a warning.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use packsync_core::redact::redact;
use parking_lot::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Crates whose logs can contain credentials: never more verbose than `warn`.
pub const PINNED: &[&str] = &["ureq", "ureq_proto", "rustls", "hyper", "h2", "http", "tiny_http", "keyring", "zbus", "oauth2", "reqwest"];

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const KEEP_OLD: usize = 3;

/// The filter for a user-supplied directive string (or the default).
pub fn build_filter(user: Option<&str>, verbose: bool) -> (EnvFilter, Vec<String>) {
    let mut ignored = vec![];
    let mut dirs: Vec<String> = vec!["warn".into()];
    let base = if verbose { "debug" } else { "info" };
    for krate in ["packsync_core", "packsync_drive", "packsync_prism", "packsync_app", "packsync"] {
        dirs.push(format!("{krate}={base}"));
    }
    if let Some(u) = user {
        for d in u.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            let target = d.split('=').next().unwrap_or("").trim();
            // `target[span]{field}=level` forms: look at the crate name only
            let name = target.split(['[', '{']).next().unwrap_or("").split("::").next().unwrap_or("");
            if PINNED.contains(&name) {
                ignored.push(d.to_string());
            } else {
                dirs.push(d.to_string());
            }
        }
    }
    // pinned crates last so that nothing above can raise them
    for p in PINNED {
        dirs.push(format!("{p}=warn"));
    }
    (EnvFilter::new(dirs.join(",")), ignored)
}

/// Rotate `path` (`.1`, `.2`, `.3`) when it is larger than the cap.
pub fn rotate(path: &Path) {
    let too_big = std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    if !too_big {
        return;
    }
    let numbered = |n: usize| PathBuf::from(format!("{}.{n}", path.display()));
    let _ = std::fs::remove_file(numbered(KEEP_OLD));
    for n in (1..KEEP_OLD).rev() {
        let _ = std::fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

/// Writes complete lines, redacted, to the log file.
#[derive(Clone)]
pub struct RedactingFile {
    inner: Arc<Mutex<(File, Vec<u8>)>>,
}

impl RedactingFile {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        rotate(path);
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { inner: Arc::new(Mutex::new((f, Vec::new()))) })
    }
}

impl Write for RedactingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut g = self.inner.lock();
        g.1.extend_from_slice(buf);
        // only whole lines are redacted and written, so a secret split across two writes is still one line
        while let Some(nl) = g.1.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = g.1.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line);
            let clean = redact(&text);
            g.0.write_all(clean.as_bytes())?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut g = self.inner.lock();
        if !g.1.is_empty() {
            let rest = std::mem::take(&mut g.1);
            let clean = redact(&String::from_utf8_lossy(&rest)).into_owned();
            g.0.write_all(clean.as_bytes())?;
        }
        g.0.flush()
    }
}

impl<'a> MakeWriter<'a> for RedactingFile {
    type Writer = RedactingFile;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Start logging to `<logs_dir>/packsync.log`. Returns the log path, or `None` when a subscriber was
/// already installed or the file cannot be opened (logging is never worth failing a command for).
pub fn init(logs_dir: &Path, verbose: bool) -> Option<PathBuf> {
    let path = logs_dir.join("packsync.log");
    let writer = RedactingFile::open(&path).ok()?;
    let user = std::env::var(packsync_core::brand::env_var("LOG")).ok();
    let (filter, ignored) = build_filter(user.as_deref(), verbose);
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(writer).try_init().ok()?;
    for d in ignored {
        tracing::warn!(directive = %d, "ignoring a log directive for a crate that can log credentials (pinned to warn)");
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_directives_cannot_raise_the_crates_that_log_credentials() {
        let (f, ignored) = build_filter(Some("ureq=trace, packsync_core=trace ,rustls::client=debug,my_crate=debug"), false);
        let text = f.to_string();
        assert_eq!(ignored, vec!["ureq=trace".to_string(), "rustls::client=debug".to_string()]);
        // the pinned crates appear after everything the user could add, at warn
        assert!(text.contains("ureq=warn") && !text.contains("ureq=trace") && !text.contains("rustls::client"), "{text}");
        assert!(text.contains("packsync_core=trace") && text.contains("my_crate=debug"), "{text}");
    }

    #[test]
    fn redaction_applies_per_line_even_when_the_writer_splits_it() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("l/packsync.log");
        let mut w = RedactingFile::open(&path).unwrap();
        w.write_all(b"2026 INFO sent Authorization: Bear").unwrap();
        w.write_all(b"er ya29.a0AfH6SMBsecretsecretsecret and refresh_token=1//0gSECRETSECRET done\n").unwrap();
        w.write_all(b"plain line\nunterminated").unwrap();
        w.flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secretsecret") && !text.contains("SECRETSECRET"), "{text}");
        assert!(text.contains("plain line") && text.contains("unterminated"), "{text}");
    }

    #[test]
    fn big_logs_are_rotated_and_old_ones_are_capped() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("packsync.log");
        for round in 0..6 {
            std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
            rotate(&path);
            assert!(!path.exists() || std::fs::metadata(&path).unwrap().len() <= MAX_LOG_BYTES, "round {round}");
        }
        let names: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| n == "packsync.log" || n.ends_with(".1") || n.ends_with(".2") || n.ends_with(".3")), "{names:?}");
        assert!(names.len() <= KEEP_OLD + 1, "{names:?}");
    }
}
