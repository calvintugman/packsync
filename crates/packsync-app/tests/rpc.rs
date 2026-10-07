//! The JSON API exactly as the desktop UI uses it, and the dev bridge in front of it.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use packsync_app::{App, JobState, bridge, rpc};
use packsync_core::progress::OpCtx;
use serde_json::{Value, json};

fn call(a: &Arc<App>, method: &str, params: Value) -> Value {
    rpc::call(a, method, params, &OpCtx::detached()).unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn job(a: &Arc<App>, method: &str, params: Value) -> Value {
    let id = rpc::start_job(a, method, params).unwrap();
    let j = a.jobs().wait(id, Duration::from_secs(60)).expect("job exists");
    assert_eq!(j.state, JobState::Succeeded, "{method}: {:?}", j.error);
    j.result.unwrap()
}

#[test]
fn the_ui_journey_through_json_only() {
    let w = World::new();
    let a = w.device("desktop");
    let app = &a.app;
    let info = call(app, "app.info", Value::Null);
    assert_eq!(info["device"]["name"], "desktop");
    assert_eq!(info["storage"]["kind"], "local");

    // settings: typos are refused, valid patches apply and persist
    let err = rpc::call(app, "settings.update", json!({ "patch": { "upload_concurency": 3 } }), &OpCtx::detached())
        .unwrap_err();
    assert_eq!(err.code().as_str(), "invalid");
    let up =
        call(app, "settings.update", json!({ "patch": { "upload_concurrency": 3, "hook_offline_policy": "block" } }));
    assert_eq!(up["settings"]["upload_concurrency"], 3);
    assert_eq!(call(app, "settings.get", Value::Null)["hook_offline_policy"], "block");
    call(app, "settings.update", json!({ "patch": { "hook_offline_policy": "play_local" } }));

    // discover → preview → enable (a job) → status → history
    let root = packsync_prism::testkit::make_instance(&a.data_dir, "Pack One", &spec());
    let found = call(app, "launcher.discover", Value::Null);
    assert_eq!(found[0]["name"], "Pack One");
    assert_eq!(found[0]["sync_enabled"], false);
    let pv = job(app, "instances.preview", json!({ "root": root, "launcher": "prism" }));
    assert!(pv["included_files"].as_u64().unwrap() > 10);
    let en = job(app, "instances.enable", json!({ "path": root }));
    let id = en["outcome"]["instance"].as_str().unwrap().to_string();
    assert_eq!(en["upload"]["remaining"], 0);
    let list = call(app, "instances.list", json!({ "online": true }));
    assert_eq!(list[0]["id"], id.as_str());
    assert!(
        matches!(list[0]["phase"].as_str(), Some("ready_on_other_devices" | "cloud_published")),
        "{}",
        list[0]["phase"]
    );
    let hist = call(app, "instance.history", json!({ "instance": id, "online": true }));
    assert_eq!(hist.as_array().unwrap().len(), 1);
    assert_eq!(
        call(app, "rules.explain", json!({ "instance": id, "path": ".minecraft/logs/latest.log" }))["synced"],
        false
    );
    assert_eq!(
        call(app, "rules.explain", json!({ "instance": id, "path": ".minecraft/mods/mod-000-1.0.0.jar" }))["synced"],
        true
    );

    // play (a job), then a second computer restores it
    // the UI cannot set timings: the default 500 ms polling needs a game that lives for a moment, like a real one
    a.play(packsync_prism::testkit::FakeGame { secs: 2.0, ..playing("+json") });
    let l = job(app, "launch.start", json!({ "instance": id, "options": {} }));
    assert_eq!(l["end"]["outcome"], "clean", "{l}");
    let b = w.device("laptop");
    let cloud = job(&b.app, "cloud.instances", Value::Null);
    assert_eq!(cloud[0]["id"], id.as_str());
    let r = job(&b.app, "restore.new", json!({}));
    assert!(r["root"].as_str().unwrap().contains("Pack One"));
    // hooks: the plan can be previewed and the apply payload is the plan itself
    let cli = if cfg!(windows) { r"C:\Program Files\PackSync\packsync.exe" } else { "/opt/packsync/packsync" };
    let plan = call(&b.app, "hooks.plan_install", json!({ "instance": id, "cli": cli }));
    assert_eq!(plan["blockers"].as_array().unwrap().len(), 0);
    call(&b.app, "hooks.apply", json!({ "instance": id, "plan": plan }));
    assert_eq!(call(&b.app, "hooks.state", json!({ "instance": id }))["installed"], true);
    // cleanup is previewable through the same surface
    let gc = job(
        app,
        "gc.plan",
        json!({ "grace_hours": 48, "retention": null, "instance": null, "consolidate_indexes": true }),
    );
    assert!(gc["delete"].as_array().unwrap().is_empty());
    // jobs are listed, errors are structured
    assert!(call(app, "jobs.list", Value::Null).as_array().unwrap().len() >= 3);
    let e = rpc::call(
        app,
        "instance.status",
        json!({ "instance": "00000000-0000-0000-0000-000000000000" }),
        &OpCtx::detached(),
    )
    .unwrap_err();
    assert_eq!(e.code().as_str(), "not_found");
}

#[test]
fn every_declared_method_is_dispatched() {
    let w = World::new();
    let a = w.device("desktop");
    let mut names = std::collections::HashSet::new();
    for m in rpc::METHODS {
        assert!(names.insert(m.name), "duplicate method {}", m.name);
        // with nonsense parameters a real method says "invalid request" (or answers), never "unknown method"
        let r = rpc::call(&a.app, m.name, json!({ "nonsense": true }), &OpCtx::detached());
        if let Err(e) = r {
            assert!(!e.to_string().contains("unknown method"), "{} is declared but not dispatched", m.name);
        }
    }
    assert!(rpc::call(&a.app, "no.such.method", Value::Null, &OpCtx::detached()).is_err());
    assert!(rpc::start_job(&a.app, "no.such.method", Value::Null).is_err());
}

fn raw(port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[test]
fn the_bridge_requires_a_token_checks_the_host_and_serves_the_api() {
    let w = World::new();
    let a = w.device("desktop");
    let ui = w.dir.path().join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(ui.join("index.html"), "<!doctype html><title>ui</title>").unwrap();
    std::fs::write(w.dir.path().join("secret.txt"), "private").unwrap();
    let b =
        bridge::start(a.app.clone(), bridge::BridgeConfig { port: 0, ui_dir: Some(ui), allow_origin: None }).unwrap();
    let base = format!("http://127.0.0.1:{}", b.addr.port());
    // no token → 401; wrong token → 401
    let r = ureq::post(&format!("{base}/api/call"))
        .config()
        .http_status_as_error(false)
        .build()
        .send_json(json!({ "method": "app.info" }))
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = ureq::post(&format!("{base}/api/call"))
        .header("X-PackSync-Token", "nope")
        .config()
        .http_status_as_error(false)
        .build()
        .send_json(json!({ "method": "app.info" }))
        .unwrap();
    assert_eq!(r.status(), 401);
    // a request that claims another Host (DNS rebinding) is refused even with the token
    let req = format!(
        "POST /api/call HTTP/1.1\r\nHost: evil.example\r\nX-PackSync-Token: {}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
        b.token
    );
    assert!(raw(b.addr.port(), &req).starts_with("HTTP/1.1 403"));
    // the API works with the token; errors are in-band and structured
    let mut ok: Value = ureq::post(&format!("{base}/api/call"))
        .header("X-PackSync-Token", &b.token)
        .send_json(json!({ "method": "app.info" }))
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["result"]["device"]["name"], "desktop");
    ok = ureq::post(&format!("{base}/api/call"))
        .header("X-PackSync-Token", &b.token)
        .send_json(
            json!({ "method": "instance.status", "params": { "instance": "00000000-0000-0000-0000-000000000000" } }),
        )
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!((ok["ok"].clone(), ok["error"]["code"].clone()), (json!(false), json!("not_found")));
    // static files: the UI loads, traversal does not
    let page = ureq::get(&format!("{base}/")).call().unwrap().body_mut().read_to_string().unwrap();
    assert!(page.contains("<title>ui</title>"));
    let r = raw(
        b.addr.port(),
        &format!("GET /../secret.txt HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", b.addr.port()),
    );
    assert!(!r.contains("private"), "{r}");
    let r = raw(
        b.addr.port(),
        &format!("GET /%2e%2e/secret.txt HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", b.addr.port()),
    );
    assert!(!r.contains("private"), "{r}");
}

#[test]
fn jobs_started_through_the_bridge_report_progress_on_the_event_stream() {
    let w = World::new();
    let a = w.device("desktop");
    let (_id, _root) = {
        let r = a.new_synced_instance("Pack One");
        (r.0, r.1)
    };
    let b = bridge::start(a.app.clone(), bridge::BridgeConfig { port: 0, ui_dir: None, allow_origin: None }).unwrap();
    // subscribe first, then start the job
    let mut s = TcpStream::connect(("127.0.0.1", b.addr.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "GET /api/events?token={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: text/event-stream\r\n\r\n",
        b.token,
        b.addr.port()
    )
    .unwrap();
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.starts_with(": connected") {
            break;
        }
        assert!(!line.is_empty(), "stream closed early");
    }
    let started: Value = ureq::post(&format!("http://127.0.0.1:{}/api/job", b.addr.port()))
        .header("X-PackSync-Token", &b.token)
        .send_json(json!({ "method": "instance.sync", "params": { "instance": _id } }))
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(started["ok"], true, "{started}");
    let job_id = started["result"]["job"].as_str().unwrap().to_string();
    let mut saw_progress_or_phase = false;
    let mut finished = None;
    for _ in 0..200 {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data: ") else { continue };
        let ev: Value = serde_json::from_str(data.trim()).unwrap();
        match ev["type"].as_str() {
            Some("progress") => saw_progress_or_phase = true,
            Some("job") if ev["id"] == job_id.as_str() && ev["state"] != "running" => {
                finished = Some(ev);
                break;
            }
            _ => {}
        }
    }
    let fin = finished.expect("the job finished");
    assert_eq!(fin["state"], "succeeded", "{fin}");
    assert!(saw_progress_or_phase, "progress events were streamed");
}
