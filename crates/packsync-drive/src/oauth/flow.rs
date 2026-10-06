//! Interactive sign-in: system browser + loopback listener on `127.0.0.1:<ephemeral>`, PKCE S256,
//! `state` verification, code exchange. The authorization code never appears in logs, errors or
//! the pages served to the browser.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use packsync_core::progress::OpCtx;
use packsync_core::redact::Secret;
use packsync_core::storage::StorageError;
use sha2::{Digest, Sha256};

use super::{AccountInfo, AuthError, Authenticator, StoredGrant, TokenFailure};
use crate::util::{form_decode, now_ms, pkce_s256, random_urlsafe, with_query};

/// Shows the authorization URL to the user.
pub trait UrlOpener: Send + Sync {
    /// Open or display `url`. The URL contains no secrets (only the PKCE challenge and a one-time
    /// `state`), so it may be printed.
    fn open(&self, url: &str) -> Result<(), String>;
}

/// Opens the system's default browser (the only browser Google allows for installed apps; no
/// embedded web views).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBrowser {
    /// When no browser can be launched, print the URL to stderr instead of failing.
    pub print_fallback: bool,
}

impl UrlOpener for SystemBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        match webbrowser::open(url) {
            Ok(()) => Ok(()),
            Err(_) if self.print_fallback => PrintUrl.open(url),
            Err(e) => Err(format!("no browser could be started ({e})")),
        }
    }
}

/// Prints the URL to stderr (headless machines, `--no-browser` in the CLI).
#[derive(Debug, Clone, Copy, Default)]
pub struct PrintUrl;

impl UrlOpener for PrintUrl {
    fn open(&self, url: &str) -> Result<(), String> {
        eprintln!("\nOpen this address in a browser on this computer to connect your Google account:\n\n  {url}\n");
        Ok(())
    }
}

/// Adapts a closure (tests, GUI shells that open the URL themselves).
pub struct FnOpener<F>(pub F);

impl<F: Fn(&str) -> Result<(), String> + Send + Sync> UrlOpener for FnOpener<F> {
    fn open(&self, url: &str) -> Result<(), String> {
        (self.0)(url)
    }
}

/// Options of [`Authenticator::login`].
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// Give up when no callback arrives within this time (the user may have closed the tab).
    pub timeout: Duration,
    /// Pre-fill the account chooser with this e-mail address.
    pub login_hint: Option<String>,
    /// Loopback address to listen on (`127.0.0.1` by default; `::1` is also allowed by Google).
    pub listen_ip: IpAddr,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(300), login_hint: None, listen_ip: IpAddr::V4(Ipv4Addr::LOCALHOST) }
    }
}

const SCRIPT: &str = "history.replaceState(null,'','/');";

fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title}</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <style>body{{font-family:system-ui,sans-serif;max-width:34rem;margin:4rem auto;padding:0 1rem;line-height:1.5;color:#222;background:#fff}}\
         @media (prefers-color-scheme:dark){{body{{color:#eee;background:#1b1b1b}}}}</style>\
         <script>{SCRIPT}</script></head><body><h1>{title}</h1><p>{text}</p></body></html>"
    )
}

fn respond(req: tiny_http::Request, status: u16, title: &str, text: &str) {
    let script_hash = STANDARD.encode(Sha256::digest(SCRIPT.as_bytes()));
    let csp = format!("default-src 'none'; style-src 'unsafe-inline'; script-src 'sha256-{script_hash}'");
    let mut resp = tiny_http::Response::from_string(page(title, text)).with_status_code(status);
    for (k, v) in [
        ("Content-Type", "text/html; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        ("X-Content-Type-Options", "nosniff"),
        ("Content-Security-Policy", csp.as_str()),
    ] {
        if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    // The browser may already be gone; nothing useful can be done about a failed write.
    let _ = req.respond(resp);
}

/// Keep only `[a-z0-9_]` of an OAuth error code (never reflect raw input).
fn sanitize_code(s: &str) -> String {
    let c: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect();
    if c.is_empty() { "unknown_error".into() } else { c }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(super) fn run_login(
    auth: &Authenticator,
    opener: &dyn UrlOpener,
    opts: &LoginOptions,
    ctx: &OpCtx,
) -> Result<AccountInfo, AuthError> {
    if ctx.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    // Fail before bothering the user when the credentials could not be kept anyway.
    auth.creds.get(&auth.cred_account)?;
    if !opts.listen_ip.is_loopback() {
        return Err(AuthError::Listener("the redirect listener must use a loopback address".into()));
    }

    let server = tiny_http::Server::http(SocketAddr::new(opts.listen_ip, 0))
        .map_err(|e| AuthError::Listener(e.to_string()))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or_else(|| AuthError::Listener("the listener has no TCP address".into()))?;
    let host = match opts.listen_ip {
        IpAddr::V4(ip) => format!("{ip}:{port}"),
        IpAddr::V6(ip) => format!("[{ip}]:{port}"),
    };
    let redirect_uri = format!("http://{host}");

    let verifier = random_urlsafe(48).map_err(AuthError::Listener)?; // 64 chars of [A-Za-z0-9-_]
    let challenge = pkce_s256(&verifier);
    let state = random_urlsafe(24).map_err(AuthError::Listener)?;

    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", auth.client.client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", auth.scope.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ];
    if let Some(h) = &opts.login_hint {
        params.push(("login_hint", h.as_str()));
    }
    let auth_url = with_query(&auth.client.endpoints.auth_url, &params);
    opener.open(&auth_url).map_err(AuthError::Browser)?;
    tracing::info!(target: "packsync_drive::oauth", port, "waiting for the browser to complete Google sign-in");

    let started = Instant::now();
    let deadline = started + opts.timeout;
    let mut rejected = 0u32;
    loop {
        if ctx.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(AuthError::TimedOut { waited: now - started, rejected_callbacks: rejected });
        }
        let req = match server.recv_timeout((deadline - now).min(Duration::from_millis(100))) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => return Err(AuthError::Listener(e.to_string())),
        };
        let host_ok = req.headers().iter().any(|h| h.field.equiv("Host") && h.value.as_str() == host);
        if *req.method() != tiny_http::Method::Get || !host_ok {
            respond(req, 400, "Unexpected request", "This address is only used to finish signing in to PackSync.");
            continue;
        }
        let (path, query) = req.url().split_once('?').unwrap_or((req.url(), ""));
        if path != "/" {
            respond(req, 404, "Not found", "This address is only used to finish signing in to PackSync.");
            continue;
        }
        let params = form_decode(query);
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if !get("state").is_some_and(|s| constant_time_eq(s, &state)) {
            rejected += 1;
            tracing::warn!(target: "packsync_drive::oauth", "rejected a sign-in callback whose state does not match this attempt");
            respond(
                req,
                400,
                "Sign-in link not valid",
                "This answer does not belong to the sign-in that PackSync is waiting for. \
                 Return to PackSync and start connecting again.",
            );
            continue;
        }
        if let Some(err) = get("error") {
            let code = sanitize_code(err);
            respond(
                req,
                200,
                "Access not granted",
                "PackSync was not given access to Google Drive. You can close this tab and try again from PackSync.",
            );
            return Err(AuthError::Denied(code));
        }
        let Some(code) = get("code").filter(|c| !c.is_empty() && c.len() <= 4096).map(Secret::new) else {
            respond(req, 400, "Sign-in incomplete", "Google did not send an authorization code. Please try again.");
            continue;
        };

        let result = finish(auth, &code, &redirect_uri, &verifier, ctx);
        match &result {
            Ok(_) => respond(
                req,
                200,
                "PackSync is connected",
                "Your Google Drive is connected. You can close this tab and return to PackSync.",
            ),
            Err(_) => respond(
                req,
                200,
                "Connection failed",
                "Signing in could not be completed. Return to PackSync for details and try again.",
            ),
        }
        return result;
    }
}

fn finish(
    auth: &Authenticator,
    code: &Secret,
    redirect_uri: &str,
    verifier: &str,
    ctx: &OpCtx,
) -> Result<AccountInfo, AuthError> {
    let grant = auth
        .token_request(
            &[
                ("grant_type", "authorization_code"),
                ("code", code.expose()),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
            ],
            ctx,
        )
        .map_err(|f| match f {
            TokenFailure::InvalidGrant(desc) => {
                AuthError::Exchange(format!("Google rejected the authorization code ({desc})"))
            }
            TokenFailure::Other(StorageError::Cancelled) => AuthError::Cancelled,
            TokenFailure::Other(e) => AuthError::Storage(e),
        })?;
    if let Some(granted) = &grant.scopes
        && !granted.iter().any(|s| s == &auth.scope)
    {
        return Err(AuthError::InsufficientScope { needed: auth.scope.clone(), granted: granted.clone() });
    }
    let refresh = grant.refresh_token.ok_or(AuthError::NoRefreshToken)?;
    let account = match auth.fetch_account(&grant.access_token, ctx) {
        Ok(a) => a,
        Err(StorageError::Cancelled) => return Err(AuthError::Cancelled),
        Err(e) => {
            tracing::warn!(target: "packsync_drive::oauth", error = %e, "signed in, but the account details could not be read");
            AccountInfo::default()
        }
    };
    let stored = StoredGrant {
        v: 1,
        refresh_token: refresh.expose().to_string(),
        scopes: grant.scopes.unwrap_or_else(|| vec![auth.scope.clone()]),
        email: account.email.clone(),
        display_name: account.display_name.clone(),
        connected_ms: now_ms(),
    };
    auth.persist_grant(stored, Some((grant.access_token, grant.expires_in)))?;
    tracing::info!(target: "packsync_drive::oauth", "Google account connected");
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(sanitize_code("access_denied"), "access_denied");
        assert_eq!(sanitize_code("<script>"), "script");
        assert_eq!(sanitize_code("!!"), "unknown_error");
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        let p = page("T", "x");
        assert!(p.contains(SCRIPT));
    }
}
