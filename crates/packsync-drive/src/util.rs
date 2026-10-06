//! Small helpers shared by the adapter, the OAuth flow and the mock server: percent/form
//! encoding, Drive query literals, randomness, RFC 3339 parsing, `Retry-After` parsing and
//! validation of untrusted identifiers and URLs.

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode everything except RFC 3986 unreserved characters (`A-Z a-z 0-9 - . _ ~`).
/// Safe for query components, form bodies and path segments alike.
pub(crate) fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX_UPPER[(b >> 4) as usize] as char);
            out.push(HEX_UPPER[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode `%XX` escapes (and `+` as a space when `plus_is_space`). Invalid escapes are kept
/// literally; invalid UTF-8 is replaced (never panics on hostile input).
pub(crate) fn pct_decode(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(if plus_is_space && b == b'+' { b' ' } else { b });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `k1=v1&k2=v2` with both sides percent-encoded (valid as a query string and as an
/// `application/x-www-form-urlencoded` body).
pub(crate) fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&pct_encode(k));
        out.push('=');
        out.push_str(&pct_encode(v));
    }
    out
}

/// Parse a query string / form body. Pairs without `=` get an empty value.
pub(crate) fn form_decode(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (pct_decode(k, true), pct_decode(v, true)),
            None => (pct_decode(p, true), String::new()),
        })
        .collect()
}

/// Append `pairs` to `base` as a query string.
pub(crate) fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return base.to_string();
    }
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}{}", form_encode(pairs))
}

/// A Drive `q` string literal: single-quoted, with `\` and `'` escaped by a backslash
/// (<https://developers.google.com/workspace/drive/api/guides/search-files>).
pub(crate) fn q_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Drive file IDs (including generated ones and the `root` / `appDataFolder` aliases) only use
/// URL-safe characters. Anything else coming from the server is rejected before it can reach a
/// URL path or a query.
pub(crate) fn is_valid_drive_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Lowercase hex with an exact length (checksums reported by the server).
pub(crate) fn is_hex_len(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `n` bytes from the operating system's CSPRNG, base64url-encoded without padding.
pub(crate) fn random_urlsafe(n_bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; n_bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("the system random number generator failed: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(&buf))
}

/// PKCE S256 challenge: `BASE64URL(SHA256(ASCII(verifier)))` (RFC 7636 §4.2).
pub(crate) fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// RFC 3339 timestamp → milliseconds since the Unix epoch.
pub(crate) fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Milliseconds since the epoch → `2026-10-06T12:00:00.123Z` (the format Drive returns).
#[cfg_attr(not(feature = "mock"), allow(dead_code))]
pub(crate) fn format_rfc3339_ms(ms: i64) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&fmt).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

/// `Retry-After` as delta-seconds or an IMF-fixdate (RFC 9110 §10.2.3).
pub(crate) fn parse_retry_after(v: &str, now: SystemTime) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let fmt = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    );
    let at = time::PrimitiveDateTime::parse(v, &fmt).ok()?.assume_utc();
    let at: SystemTime = at.into();
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Where URLs handed to us by the server (resumable session URIs, download redirects) may point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UrlRules {
    /// Host patterns: `example.com` matches exactly, `.example.com` matches any subdomain.
    pub hosts: Vec<String>,
    /// Allow plain `http` (tests against the local mock only).
    pub allow_http: bool,
}

impl UrlRules {
    /// Returns `true` when `url` is absolute, uses an allowed scheme and host, and carries no
    /// user-info component.
    pub fn allows(&self, url: &str) -> bool {
        let Ok(uri) = url.parse::<ureq::http::Uri>() else { return false };
        let scheme_ok = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => self.allow_http,
            _ => false,
        };
        if !scheme_ok {
            return false;
        }
        let Some(auth) = uri.authority() else { return false };
        if auth.as_str().contains('@') {
            return false;
        }
        let host = auth.host().to_ascii_lowercase();
        self.hosts.iter().any(|pat| {
            let pat = pat.to_ascii_lowercase();
            match pat.strip_prefix('.') {
                Some(suffix) => host.ends_with(&format!(".{suffix}")),
                None => host == pat,
            }
        })
    }
}

/// Host of an absolute URL (used to derive [`UrlRules`] for custom endpoints).
pub(crate) fn url_host(url: &str) -> Option<String> {
    let uri = url.parse::<ureq::http::Uri>().ok()?;
    Some(uri.host()?.to_ascii_lowercase())
}

/// Truncate `s` to at most `max` bytes on a UTF-8 character boundary.
pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Current time in milliseconds since the Unix epoch.
#[cfg_attr(not(feature = "mock"), allow(dead_code))]
pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_roundtrip() {
        let s = "a b&c=d/é'\"\\%+~._-";
        let e = pct_encode(s);
        assert!(e.bytes().all(|b| b.is_ascii_alphanumeric() || b"%-._~".contains(&b)), "{e}");
        assert_eq!(pct_decode(&e, false), s);
        assert_eq!(pct_decode("a+b%20c", true), "a b c");
        assert_eq!(pct_decode("%zz%4", false), "%zz%4", "invalid escapes are kept");
        assert_eq!(pct_decode("%", true), "%");
        assert_eq!(pct_decode("%4", true), "%4");
        assert_eq!(pct_decode("%41", true), "A");
    }

    #[test]
    fn form_roundtrip() {
        let body = form_encode(&[("code", "4/0A&b=c"), ("redirect_uri", "http://127.0.0.1:5/")]);
        let back = form_decode(&body);
        assert_eq!(back[0], ("code".into(), "4/0A&b=c".into()));
        assert_eq!(back[1], ("redirect_uri".into(), "http://127.0.0.1:5/".into()));
        assert_eq!(with_query("https://x/y", &[("a", "1")]), "https://x/y?a=1");
        assert_eq!(with_query("https://x/y?z=0", &[("a", "1")]), "https://x/y?z=0&a=1");
    }

    #[test]
    fn query_literals_escape_quotes_and_backslashes() {
        assert_eq!(q_string("plain"), "'plain'");
        assert_eq!(q_string("quinn's paper\\essay"), r"'quinn\'s paper\\essay'");
        assert_eq!(q_string("ünï ' \\'"), r"'ünï \' \\\''");
        assert_eq!(q_string(""), "''");
    }

    #[test]
    fn id_validation() {
        assert!(is_valid_drive_id("1AbC-_xyz"));
        assert!(is_valid_drive_id("appDataFolder"));
        for bad in ["", "a/b", "a b", "a'b", "../x", "a%2F", &"x".repeat(257), "ü"] {
            assert!(!is_valid_drive_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        assert_eq!(pkce_s256("dBjftJeZ4CVP-mJ92K9pD7cK-xNjSyxSxLc-5cTE0sc"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn random_tokens_are_urlsafe_and_distinct() {
        let a = random_urlsafe(32).unwrap();
        let b = random_urlsafe(32).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }

    #[test]
    fn rfc3339() {
        let ms = parse_rfc3339_ms("2026-10-06T12:00:00.123Z").unwrap();
        assert_eq!(format_rfc3339_ms(ms), "2026-10-06T12:00:00.123Z");
        assert_eq!(parse_rfc3339_ms("2026-10-06T12:00:00Z").unwrap() + 123, ms);
        assert!(parse_rfc3339_ms("yesterday").is_none());
    }

    #[test]
    fn retry_after_forms() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(parse_retry_after("7", now), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        // 1994-11-06 08:49:37 UTC = 784111777
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:50:37 GMT", now), Some(Duration::from_secs(60)));
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:40:37 GMT", now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
    }

    #[test]
    fn url_rules() {
        let g = UrlRules { hosts: vec![".googleapis.com".into()], allow_http: false };
        assert!(g.allows("https://www.googleapis.com/upload/drive/v3/files?upload_id=x"));
        assert!(!g.allows("https://googleapis.com.evil.example/x"));
        assert!(!g.allows("https://evil.example/www.googleapis.com"));
        assert!(!g.allows("http://www.googleapis.com/x"), "plain http is refused");
        assert!(!g.allows("https://user:pw@www.googleapis.com/x"));
        assert!(!g.allows("/relative"));
        assert!(!g.allows("javascript:alert(1)"));
        let local = UrlRules { hosts: vec!["127.0.0.1".into()], allow_http: true };
        assert!(local.allows("http://127.0.0.1:4567/upload?upload_id=1"));
        assert!(!local.allows("http://127.0.0.2:4567/"));
        assert_eq!(url_host("http://127.0.0.1:99/x").as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn utf8_truncation() {
        assert_eq!(truncate_utf8("abc", 5), "abc");
        assert_eq!(truncate_utf8("aé", 2), "a");
        assert_eq!(truncate_utf8("éé", 3), "é");
    }
}
