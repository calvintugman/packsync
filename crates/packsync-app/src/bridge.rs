//! A local HTTP bridge to the application layer, for developing the UI in a browser and for the
//! automated UI tests. It is **not** part of the shipped desktop app (which talks to the same
//! [`rpc`] table through Tauri commands) and is only reachable through the hidden
//! `packsync dev-bridge` command.
//!
//! It is bound to `127.0.0.1`, requires a random token on every API request (a web page you happen to
//! have open cannot call it), and checks the `Host` header (DNS rebinding). Static UI files can be
//! served from one directory so that a browser test uses a single origin.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use packsync_core::error::{Error, Result};
use serde_json::{Value, json};

use crate::app::App;
use crate::events::{AppEvent, ErrorView};
use crate::rpc;

pub struct BridgeConfig {
    /// 0 = pick a free port.
    pub port: u16,
    pub ui_dir: Option<PathBuf>,
    /// Allow this origin to call the API from another port (Vite dev server without its proxy).
    pub allow_origin: Option<String>,
}

pub struct Bridge {
    pub addr: SocketAddr,
    pub token: String,
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

fn token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn start(app: Arc<App>, cfg: BridgeConfig) -> Result<Bridge> {
    let server = tiny_http::Server::http(SocketAddr::from(([127, 0, 0, 1], cfg.port)))
        .map_err(|e| Error::other(format!("cannot start the bridge: {e}")))?;
    let addr = server.server_addr().to_ip().ok_or_else(|| Error::other("the bridge has no IP address"))?;
    let tok = token();
    let bridge = Bridge { addr, token: tok.clone() };
    let cfg = Arc::new(cfg);
    std::thread::Builder::new()
        .name("bridge".into())
        .spawn(move || {
            for req in server.incoming_requests() {
                let (app, tok, cfg) = (app.clone(), tok.clone(), cfg.clone());
                std::thread::spawn(move || handle(req, &app, &tok, &cfg, addr.port()));
            }
        })
        .map_err(|e| Error::io("starting the bridge thread", e))?;
    Ok(bridge)
}

fn json_response(status: u16, body: &Value, cors: Option<&str>) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut r = tiny_http::Response::from_string(body.to_string()).with_status_code(status);
    r.add_header(header("Content-Type", "application/json; charset=utf-8"));
    r.add_header(header("Cache-Control", "no-store"));
    r.add_header(header("X-Content-Type-Options", "nosniff"));
    if let Some(o) = cors {
        r.add_header(header("Access-Control-Allow-Origin", o));
        r.add_header(header("Access-Control-Allow-Headers", "content-type, x-packsync-token"));
        r.add_header(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"));
    }
    r
}

fn handle(mut req: tiny_http::Request, app: &Arc<App>, tok: &str, cfg: &BridgeConfig, port: u16) {
    let host_ok = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| {
            let v = h.value.as_str();
            v == format!("127.0.0.1:{port}") || v == format!("localhost:{port}")
        })
        .unwrap_or(false);
    let cors = cfg.allow_origin.as_deref();
    if !host_ok {
        let _ = req.respond(json_response(403, &json!({ "ok": false, "error": "unexpected Host header" }), None));
        return;
    }
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    if method == tiny_http::Method::Options && path.starts_with("/api/") {
        let _ = req.respond(json_response(204, &Value::Null, cors).with_status_code(204));
        return;
    }
    if !path.starts_with("/api/") {
        serve_static(req, cfg, &path);
        return;
    }
    let supplied =
        req.headers().iter().find(|h| h.field.equiv("X-PackSync-Token")).map(|h| h.value.as_str().to_string());
    // EventSource cannot set headers: allow the token as a query parameter for the events stream only
    let query_token = url.split_once("?token=").map(|(_, t)| t.to_string());
    let given = supplied.or(if path == "/api/events" { query_token } else { None }).unwrap_or_default();
    if !ct_eq(&given, tok) {
        let _ = req.respond(json_response(401, &json!({ "ok": false, "error": "missing or wrong token" }), cors));
        return;
    }
    match (method, path.as_str()) {
        (tiny_http::Method::Get, "/api/methods") => {
            let list: Vec<Value> =
                rpc::METHODS.iter().map(|m| json!({ "name": m.name, "long": m.long, "doc": m.doc })).collect();
            let _ = req.respond(json_response(200, &json!({ "ok": true, "result": list }), cors));
        }
        (tiny_http::Method::Get, "/api/events") => stream_events(req, app.bus().subscribe(), cors),
        (tiny_http::Method::Post, "/api/call") | (tiny_http::Method::Post, "/api/job") => {
            let mut body = Vec::new();
            if req.as_reader().take(1 << 20).read_to_end(&mut body).is_err() {
                let _ = req.respond(json_response(400, &json!({ "ok": false, "error": "unreadable body" }), cors));
                return;
            }
            let parsed: std::result::Result<Value, _> = serde_json::from_slice(&body);
            let Ok(v) = parsed else {
                let _ = req.respond(json_response(400, &json!({ "ok": false, "error": "body is not JSON" }), cors));
                return;
            };
            let Some(method_name) = v.get("method").and_then(|m| m.as_str()).map(str::to_string) else {
                let _ = req.respond(json_response(400, &json!({ "ok": false, "error": "method is required" }), cors));
                return;
            };
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let result = if path == "/api/job" {
                rpc::start_job(app, &method_name, params).map(|id| json!({ "job": id }))
            } else {
                rpc::call(app, &method_name, params, &app.ctx())
            };
            let doc = match result {
                Ok(r) => json!({ "ok": true, "result": r }),
                Err(e) => json!({ "ok": false, "error": ErrorView::from(&e) }),
            };
            let _ = req.respond(json_response(200, &doc, cors));
        }
        _ => {
            let _ = req.respond(json_response(404, &json!({ "ok": false, "error": "no such endpoint" }), cors));
        }
    }
}

/// Server-sent events: one `data:` line per [`AppEvent`], a comment every 15 s to keep the connection
/// open. The response is written by hand on the raw connection because `tiny_http` buffers streamed
/// bodies and an event stream has to flush after every event.
fn stream_events(req: tiny_http::Request, rx: Receiver<AppEvent>, cors: Option<&str>) {
    let mut w = req.into_writer();
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
    );
    if let Some(o) = cors {
        head.push_str(&format!("Access-Control-Allow-Origin: {o}\r\n"));
    }
    head.push_str("\r\n: connected\n\n");
    if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
        return;
    }
    loop {
        let chunk = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(ev) => format!("data: {}\n\n", serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into())),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => ": keep-alive\n\n".to_string(),
            Err(_) => return,
        };
        // a closed browser tab shows up as a failed write: the subscription is dropped with this thread
        if w.write_all(chunk.as_bytes()).and_then(|_| w.flush()).is_err() {
            return;
        }
    }
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn serve_static(req: tiny_http::Request, cfg: &BridgeConfig, path: &str) {
    let Some(dir) = &cfg.ui_dir else {
        let _ =
            req.respond(json_response(404, &json!({ "ok": false, "error": "no UI is served by this bridge" }), None));
        return;
    };
    let rel = Path::new(path.trim_start_matches('/'));
    // only plain components: no `..`, no absolute paths, no drive prefixes
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        let _ = req.respond(json_response(400, &json!({ "ok": false, "error": "bad path" }), None));
        return;
    }
    let mut file = dir.join(rel);
    if rel.as_os_str().is_empty() || file.is_dir() {
        file = dir.join(rel).join("index.html");
    }
    if !file.is_file() && rel.extension().is_none() {
        file = dir.join("index.html"); // single-page app routes
    }
    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut r = tiny_http::Response::from_data(bytes);
            r.add_header(header("Content-Type", content_type(&file)));
            r.add_header(header("Cache-Control", "no-store"));
            r.add_header(header("X-Content-Type-Options", "nosniff"));
            r.add_header(header(
                "Content-Security-Policy",
                "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src 'self'",
            ));
            let _ = req.respond(r);
        }
        Err(_) => {
            let _ = req.respond(json_response(404, &json!({ "ok": false, "error": "not found" }), None));
        }
    }
}
