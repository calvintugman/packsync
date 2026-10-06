//! Secret redaction for logs, diagnostics and error messages, plus a `Secret` wrapper whose
//! `Debug`/`Display` never reveal the value.

use std::borrow::Cow;
use std::fmt;

/// Wraps a secret string. Never prints its content; call [`Secret::expose`] deliberately.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

const REDACTED: &str = "<redacted>";

/// Query-string / form / header keys whose values are secrets.
const SECRET_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "code",
    "code_verifier",
    "upload_id",
    "uploadid",
    "token",
    "authorization",
    "api_key",
    "key",
    "password",
    "secret",
];

/// Bare token prefixes that identify secrets even without a key (Google access `ya29.`, refresh `1//`).
const TOKEN_PREFIXES: &[&str] = &["ya29.", "1//0", "GOCSPX-"];

/// Redact secrets from free-form text. Handles `key=value` / `key: value` / `"key":"value"`
/// pairs (including URL query strings, so resumable session URIs lose their `upload_id`) and
/// bare Google token prefixes. Intentionally conservative: when in doubt, redact.
pub fn redact(input: &str) -> Cow<'_, str> {
    if !needs_redaction(input) {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Bare token prefixes.
        if let Some(p) = TOKEN_PREFIXES.iter().find(|p| input[i..].starts_with(**p)) {
            let mut j = i + p.len();
            while j < bytes.len() && is_token_char(bytes[j]) {
                j += 1;
            }
            out.push_str(REDACTED);
            i = j;
            continue;
        }
        // key=value / key: value / "key":"value"
        if let Some((klen, key)) = key_at(input, i) {
            let mut j = i + klen;
            // optional closing quote of the key
            if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                j += 1;
            }
            let sep_start = j;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'=' || bytes[j] == b':') {
                j += 1;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                let quote = if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                    j += 1;
                    Some(bytes[j - 1])
                } else {
                    None
                };
                let vstart = j;
                // For an `Authorization: Bearer xxx` style header, swallow the scheme word too.
                if key.eq_ignore_ascii_case("authorization") {
                    while j < bytes.len()
                        && bytes[j] != b'\n'
                        && bytes[j] != b'\r'
                        && quote.is_none_or(|q| bytes[j] != q)
                    {
                        j += 1;
                    }
                } else {
                    while j < bytes.len() {
                        let c = bytes[j];
                        let stop = match quote {
                            Some(q) => c == q,
                            None => {
                                c.is_ascii_whitespace()
                                    || matches!(c, b'&' | b',' | b';' | b')' | b'"' | b'\'' | b'}' | b'>')
                            }
                        };
                        if stop {
                            break;
                        }
                        j += 1;
                    }
                }
                if j > vstart {
                    out.push_str(&input[i..vstart]);
                    out.push_str(REDACTED);
                    i = j;
                    continue;
                }
            }
            // not a secret pair after all: emit the key text and carry on after it
            out.push_str(&input[i..sep_start]);
            i = sep_start;
            continue;
        }
        // copy one UTF-8 char
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    Cow::Owned(out)
}

fn needs_redaction(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    TOKEN_PREFIXES.iter().any(|p| s.contains(p)) || SECRET_KEYS.iter().any(|k| l.contains(k))
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b'+' | b'=')
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// If a secret key name starts at byte `i` (at a word boundary), return its length and name.
fn key_at(s: &str, i: usize) -> Option<(usize, &str)> {
    if i > 0 {
        let prev = s.as_bytes()[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let rest = &s[i..];
    // longest match first so `refresh_token` wins over `token`
    let mut best: Option<&str> = None;
    for k in SECRET_KEYS {
        if rest.len() >= k.len() && rest.is_char_boundary(k.len()) && rest[..k.len()].eq_ignore_ascii_case(k) {
            // must be followed by a non-identifier char
            let next = rest.as_bytes().get(k.len()).copied();
            if next.is_none_or(|n| !(n.is_ascii_alphanumeric() || n == b'_')) && best.is_none_or(|b| k.len() > b.len())
            {
                best = Some(&rest[..k.len()]);
            }
        }
    }
    best.map(|k| (k.len(), k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_session_uris_and_tokens() {
        let s = "PUT https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&upload_id=AEnB2Uo123_abc-XYZ failed";
        let r = redact(s);
        assert!(!r.contains("AEnB2Uo123"), "{r}");
        assert!(r.contains("uploadType=resumable"), "non-secret params stay: {r}");

        // (assembled at run time: a source file with a realistic access-token shape trips secret scanners)
        let leaky = format!("Authorization: Bearer {}{}\nnext line", "ya29", ".a0AfH6SMBx-secret_value");
        let r = redact(&leaky);
        assert!(!r.contains("ya29"), "{r}");
        assert!(r.contains("next line"));

        let r = redact(r#"{"refresh_token":"1//0gABCDEF","expires_in":3599}"#);
        assert!(!r.contains("1//0g"), "{r}");
        assert!(r.contains("expires_in"));

        let r = redact("client_secret=GOCSPX-abcdef123 code_verifier=zzzz&state=ok");
        assert!(!r.contains("GOCSPX"));
        assert!(!r.contains("zzzz"));
        assert!(r.contains("state=ok"));
    }

    #[test]
    fn leaves_innocent_text_alone() {
        let s = "uploaded 3 packs (12 MiB) to instance 0f0e";
        assert!(matches!(redact(s), Cow::Borrowed(_)));
        let s = "the keyboard is broken, token bucket is empty";
        // "token" without a separator+value is not a pair
        assert_eq!(redact(s), s);
    }

    #[test]
    fn secret_wrapper_never_prints() {
        let s = Secret::new("hunter2");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!format!("{s}").contains("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn handles_unicode_without_panicking() {
        let s = "ünï cödé=ok ключ=значение token=abc";
        let r = redact(s);
        assert!(r.contains("ünï"));
        assert!(!r.contains("abc"));
    }
}
