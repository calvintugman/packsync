//! Small formatting helpers for human output.

use packsync_core::engine::SyncPhase;

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// "3 min ago", "2 days ago" for a time in milliseconds since the epoch.
pub fn ago(ms: i64) -> String {
    let now = packsync_core::db::now_ms();
    let secs = ((now - ms) / 1000).max(0);
    match secs {
        0..=44 => "just now".into(),
        45..=3_599 => format!("{} min ago", (secs + 30) / 60),
        3_600..=86_399 => format!("{} h ago", (secs + 1800) / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// `2026-10-06T12:34:56Z` → `2026-10-06 12:34`.
pub fn when(rfc3339: &str) -> String {
    let t = rfc3339.replace('T', " ");
    t.chars().take(16).collect()
}

pub fn phase_label(p: SyncPhase) -> &'static str {
    match p {
        SyncPhase::NotEnabled => "sync is not enabled",
        SyncPhase::LocalSnapshotComplete => "saved on this computer — not uploaded yet",
        SyncPhase::UploadPending => "upload pending — will be retried (not synced yet)",
        SyncPhase::CloudPublished => "uploaded and verified — not yet visible to other computers",
        SyncPhase::ReadyOnOtherDevices => "up to date — ready on your other computers",
        SyncPhase::CloudAhead => "the cloud has a newer version",
        SyncPhase::Conflict => "two versions need your decision (packsync conflicts)",
        SyncPhase::CrashPending => "the last session ended abnormally — decide what to keep (packsync crash show)",
        SyncPhase::Restoring => "a restore was interrupted — run any command to recover",
        SyncPhase::Unavailable => "the instance folder is not available",
    }
}

pub fn short(id: &impl std::fmt::Display) -> String {
    id.to_string().chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500_000), "1.5 MB");
        assert_eq!(bytes(2_000_000_000), "2.0 GB");
        assert_eq!(when("2026-10-06T12:34:56Z"), "2026-10-06 12:34");
        assert_eq!(ago(packsync_core::db::now_ms()), "just now");
        assert_eq!(ago(packsync_core::db::now_ms() - 5 * 60_000), "5 min ago");
        assert_eq!(ago(packsync_core::db::now_ms() - 3 * 86_400_000), "3 days ago");
    }
}
