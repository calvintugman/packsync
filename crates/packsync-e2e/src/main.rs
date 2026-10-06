//! `packsync-e2e`: the world the browser end-to-end tests of the desktop UI run in.
//!
//! It builds two "computers" — the **desktop** (the one the UI controls) and a **laptop** — each with its
//! own PackSync state, its own Prism data folder full of generated instances and a fake `prismlauncher`
//! that runs a fake game. A shared folder plays the cloud. The desktop's application layer is served by
//! the development bridge together with the built UI, and a second, control port lets a test do what a
//! person would do elsewhere: edit files, play on the laptop, change what the next game does.
//!
//! Nothing here touches a real Prism, a real Minecraft or the network. It is test support, not part of
//! the shipped product.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use packsync_app::bridge::{self, BridgeConfig};
use packsync_app::{App, AppOptions, AppPaths, RestoreCloudRequest, StorageWrapper};
use packsync_core::credentials::MemoryCredentialStore;
use packsync_core::progress::OpCtx;
use packsync_core::retry::{RecordingSleeper, Sleeper};
use packsync_core::storage::{AuthProblem, Storage, StorageError};
use packsync_core::storage_fault::{Fault, FaultPlan, FaultRule, FaultyStorage, OpKind, When};
use packsync_core::testkit::fixtures::FixtureSpec;
use packsync_prism::PrismLauncher;
use packsync_prism::testkit::{FakeGame, FakePrism, make_instance};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "packsync-e2e", about = "A disposable world for the desktop UI's end-to-end tests")]
struct Args {
    /// The world's folder (created if needed; the caller deletes it).
    #[arg(long)]
    dir: PathBuf,
    /// The built UI (`apps/desktop/dist`). Required unless `--prepare-only`.
    #[arg(long, required_unless_present = "prepare_only")]
    ui_dir: Option<PathBuf>,
    /// Only lay the world out on disk and print where everything is, then exit. Point the desktop app
    /// (or the CLI) at it with PACKSYNC_CONFIG_DIR / PACKSYNC_STATE_DIR.
    #[arg(long)]
    prepare_only: bool,
    /// Start syncing (and upload) this desktop instance before anything else. Repeatable.
    #[arg(long, value_name = "NAME")]
    enable: Vec<String>,
    /// After enabling, change a file of this instance and save a snapshot without uploading it, so it
    /// shows as "saved here, not uploaded yet". Repeatable.
    #[arg(long, value_name = "NAME")]
    leave_unuploaded: Vec<String>,
    /// `packsync` CLI binary that launcher hooks will call.
    #[arg(long)]
    cli: Option<PathBuf>,
    /// Bridge port (0 = any).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Control port (0 = any).
    #[arg(long, default_value_t = 0)]
    control_port: u16,
    /// Start with the shared cloud already connected on the desktop (skips the setup wizard).
    #[arg(long)]
    connected: bool,
}

struct Device {
    app: Arc<App>,
    data_dir: PathBuf,
    fake: FakePrism,
}

struct World {
    cloud: PathBuf,
    desktop: Device,
    laptop: Device,
    /// What the desktop's cloud does to it: nothing, until a test says otherwise.
    faults: Arc<FaultPlan>,
}

fn spec(mods: usize) -> FixtureSpec {
    let mut s = FixtureSpec::tiny();
    s.mods = mods;
    s.mod_size = (30_000, 90_000);
    s.regions_per_world = 2;
    s
}

fn device(base: &Path, name: &str, cli: Option<&Path>, faults: Option<Arc<FaultPlan>>) -> Device {
    let data_dir = base.join("prism");
    std::fs::create_dir_all(data_dir.join("instances")).expect("create the Prism data folder");
    let fake = FakePrism::install(&base.join("bin"), &data_dir);
    let prism = PrismLauncher::new().with_data_dirs(vec![data_dir.clone()]).with_executable(fake.exe.clone());
    let storage_wrapper: Option<StorageWrapper> = faults.map(|plan| {
        let wrap: StorageWrapper =
            Arc::new(move |s: Arc<dyn Storage>| FaultyStorage::new(s, plan.clone()) as Arc<dyn Storage>);
        wrap
    });
    // with faults in play, retries back off without really waiting: a simulated outage is reported at once
    let sleeper = storage_wrapper.as_ref().map(|_| Arc::new(RecordingSleeper::default()) as Arc<dyn Sleeper>);
    let app = App::open(
        AppPaths::under(&base.join("app")),
        AppOptions {
            credentials: Some(Arc::new(MemoryCredentialStore::default())),
            prism: Some(prism),
            sleeper,
            storage_wrapper,
            ..Default::default()
        },
    )
    .expect("open the application");
    let cli = cli.map(Path::to_path_buf);
    let (exe, dirs) = (fake.exe.clone(), vec![data_dir.clone()]);
    app.update_settings(|s| {
        s.device_name = Some(name.to_string());
        s.cli_path = cli;
        // persisted, so a separate process (the real desktop app) finds this Prism and not the user's own
        s.prism_executable = Some(exe);
        s.prism_data_dirs = dirs;
    })
    .expect("save settings");
    Device { app, data_dir, fake }
}

fn str_arg<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key).and_then(Value::as_str).ok_or_else(|| format!("{key} is required"))
}

impl World {
    fn device(&self, which: &str) -> Result<&Device, String> {
        match which {
            "desktop" => Ok(&self.desktop),
            "laptop" => Ok(&self.laptop),
            other => Err(format!("unknown device {other:?}")),
        }
    }

    fn instance_root(&self, dev: &Device, name: &str) -> Result<PathBuf, String> {
        dev.app
            .engine()
            .instances()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|r| r.name == name)
            .map(|r| r.root)
            .or_else(|| Some(dev.data_dir.join("instances").join(name)).filter(|p| p.is_dir()))
            .ok_or_else(|| format!("no instance named {name:?}"))
    }

    fn control(&self, cmd: &Value) -> Result<Value, String> {
        let ctx = OpCtx::detached();
        match str_arg(cmd, "cmd")? {
            "info" => Ok(json!({
                "cloud": self.cloud,
                "desktop_prism": self.desktop.data_dir,
                "laptop_prism": self.laptop.data_dir,
            })),
            // what the next game does: {device, secs, exit, crash_report, write: [path, payload]}
            "fake_game" => {
                let dev = self.device(cmd.get("device").and_then(Value::as_str).unwrap_or("desktop"))?;
                let write = cmd
                    .get("write")
                    .and_then(Value::as_array)
                    .and_then(|a| Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_str()?.to_string())));
                dev.fake.set(&FakeGame {
                    secs: cmd.get("secs").and_then(Value::as_f64).unwrap_or(0.6) as f32,
                    exit: cmd.get("exit").and_then(Value::as_i64).unwrap_or(0) as i32,
                    write,
                    crash_report: cmd.get("crash_report").and_then(Value::as_bool).unwrap_or(false),
                    ..FakeGame::default()
                });
                Ok(json!({}))
            }
            // write a file in an instance's game folder: {device, instance, path, content}
            "edit" => {
                let dev = self.device(str_arg(cmd, "device")?)?;
                let root = self.instance_root(dev, str_arg(cmd, "instance")?)?;
                let game = if root.join("minecraft").is_dir() { "minecraft" } else { ".minecraft" };
                let path = root.join(game).join(str_arg(cmd, "path")?);
                std::fs::create_dir_all(path.parent().ok_or("bad path")?).map_err(|e| e.to_string())?;
                std::fs::write(&path, str_arg(cmd, "content")?).map_err(|e| e.to_string())?;
                Ok(json!({ "written": path }))
            }
            // what the desktop's cloud does: {mode: "ok" | "offline" | "quota" | "auth" | "rate_limited" | "slow", ms?}
            "cloud_mode" => {
                let mode = str_arg(cmd, "mode")?;
                self.faults.clear();
                self.faults.set_offline(false);
                match mode {
                    "ok" => {}
                    "offline" => self.faults.set_offline(true),
                    "quota" => {
                        self.faults.add(FaultRule::new(
                            OpKind::Put,
                            When::Always,
                            Fault::Error(StorageError::QuotaExceeded("your Google Drive is full".into())),
                        ));
                    }
                    "auth" => {
                        self.faults.add(FaultRule::new(
                            OpKind::Any,
                            When::Always,
                            Fault::Error(StorageError::Auth(AuthProblem::Revoked)),
                        ));
                    }
                    "rate_limited" => {
                        self.faults.add(FaultRule::new(
                            OpKind::Put,
                            When::Always,
                            Fault::Error(StorageError::RateLimited { retry_after: None }),
                        ));
                    }
                    "slow" => {
                        let ms = cmd.get("ms").and_then(Value::as_u64).unwrap_or(1500);
                        self.faults.add(FaultRule::new(
                            OpKind::Put,
                            When::Always,
                            Fault::Latency(std::time::Duration::from_millis(ms)),
                        ));
                    }
                    other => return Err(format!("unknown cloud mode {other:?}")),
                }
                Ok(json!({ "mode": mode }))
            }
            "laptop_connect" => {
                self.laptop.app.use_local_storage(&self.cloud).map_err(|e| e.to_string())?;
                Ok(json!({}))
            }
            // bring an instance from the cloud to the laptop: {instance (name)}
            "laptop_restore" => {
                self.laptop.app.use_local_storage(&self.cloud).map_err(|e| e.to_string())?;
                let name = str_arg(cmd, "instance")?;
                let cloud = self.laptop.app.engine().cloud_instances(&ctx).map_err(|e| e.to_string())?;
                let c = cloud.iter().find(|c| c.name == name).ok_or_else(|| format!("{name:?} is not in the cloud"))?;
                let r = self
                    .laptop
                    .app
                    .restore_cloud_instance(RestoreCloudRequest { instance: Some(c.id), ..Default::default() }, &ctx)
                    .map_err(|e| e.to_string())?;
                Ok(json!({ "root": r.root, "instance": r.instance }))
            }
            // a brand-new instance on the laptop, synced and uploaded: {instance (name)}
            "laptop_new_instance" => {
                self.laptop.app.use_local_storage(&self.cloud).map_err(|e| e.to_string())?;
                let name = str_arg(cmd, "instance")?;
                let root = make_instance(&self.laptop.data_dir, name, &spec(3));
                let out =
                    self.laptop.app.enable_sync("prism", &root, String::new(), &ctx).map_err(|e| e.to_string())?;
                self.laptop.app.engine().upload_pending(out.instance, false, &ctx).map_err(|e| e.to_string())?;
                Ok(json!({ "instance": out.instance, "root": root }))
            }
            // snapshot, then reconcile (upload / fast-forward / conflict) on a device: {device, instance}
            "sync" => {
                let dev = self.device(str_arg(cmd, "device")?)?;
                let name = str_arg(cmd, "instance")?;
                let id = dev
                    .app
                    .engine()
                    .instances()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|r| r.name == name)
                    .map(|r| r.id)
                    .ok_or_else(|| format!("no instance named {name:?}"))?;
                let (_, rec) = dev.app.engine().sync_now(id, &ctx).map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(rec).map_err(|e| e.to_string())?)
            }
            "cloud" => {
                let cloud = self.desktop.app.engine().cloud_instances(&ctx).map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(cloud).map_err(|e| e.to_string())?)
            }
            other => Err(format!("unknown control command {other:?}")),
        }
    }
}

fn main() {
    let args = Args::parse();
    std::fs::create_dir_all(&args.dir).expect("create the world folder");
    let dir = dunce_canonical(&args.dir);
    let cloud = dir.join("cloud");
    std::fs::create_dir_all(&cloud).expect("create the cloud folder");

    let faults = FaultPlan::new(7);
    let desktop = device(&dir.join("desktop"), "Desktop", args.cli.as_deref(), Some(faults.clone()));
    let laptop = device(&dir.join("laptop"), "Laptop", args.cli.as_deref(), None);
    make_instance(&desktop.data_dir, "Skyblock Pack", &spec(4));
    make_instance(&desktop.data_dir, "Vanilla Plus", &spec(2));
    if args.connected {
        desktop.app.use_local_storage(&cloud).expect("connect the desktop to the cloud");
    }
    let world = Arc::new(World { cloud: cloud.clone(), desktop, laptop, faults });

    let ctx = OpCtx::detached();
    for name in &args.enable {
        let root = world.desktop.data_dir.join("instances").join(name);
        let out = world.desktop.app.enable_sync("prism", &root, String::new(), &ctx).expect("enable the instance");
        world.desktop.app.engine().upload_pending(out.instance, false, &ctx).expect("upload the first snapshot");
    }
    for name in &args.leave_unuploaded {
        let root = world.desktop.data_dir.join("instances").join(name);
        let id = world
            .desktop
            .app
            .engine()
            .find_instance_by_root(&root)
            .expect("look up the instance")
            .map(|r| r.id)
            .expect("--leave-unuploaded needs --enable for the same instance");
        std::fs::write(root.join(".minecraft/config/evening-session.toml"), "played = true\n").expect("edit a file");
        world.desktop.app.engine().snapshot(id, Default::default(), &ctx).expect("snapshot");
    }

    if args.prepare_only {
        println!(
            "{}",
            json!({
                "dir": dir,
                "cloud": cloud,
                "config_dir": dir.join("desktop/app/config"),
                "state_dir": dir.join("desktop/app/state"),
                "desktop_prism": world.desktop.data_dir,
            })
        );
        return;
    }
    let ui_dir = args.ui_dir.clone().expect("--ui-dir is required");

    let b = bridge::start(
        world.desktop.app.clone(),
        BridgeConfig { port: args.port, ui_dir: Some(ui_dir), allow_origin: None },
    )
    .expect("start the bridge");

    let control = tiny_http::Server::http(("127.0.0.1", args.control_port)).expect("start the control server");
    let control_port = control.server_addr().to_ip().expect("control address").port();
    println!(
        "{}",
        json!({
            "port": b.addr.port(),
            "token": b.token,
            "url": format!("http://127.0.0.1:{}/#token={}", b.addr.port(), b.token),
            "control": format!("http://127.0.0.1:{control_port}/control"),
            "cloud": cloud,
            "dir": dir,
            "desktop_prism": world.desktop.data_dir,
        })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    for mut req in control.incoming_requests() {
        let w = world.clone();
        std::thread::spawn(move || {
            let mut body = String::new();
            let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
            let doc = match serde_json::from_str::<Value>(&body) {
                Ok(cmd) => match w.control(&cmd) {
                    Ok(r) => json!({ "ok": true, "result": r }),
                    Err(e) => json!({ "ok": false, "error": e }),
                },
                Err(e) => json!({ "ok": false, "error": format!("not JSON: {e}") }),
            };
            let mut resp = tiny_http::Response::from_string(doc.to_string());
            resp.add_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("header"),
            );
            let _ = req.respond(resp);
        });
    }
}

fn dunce_canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}
