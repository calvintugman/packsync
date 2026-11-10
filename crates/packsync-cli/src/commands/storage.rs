use std::io::{IsTerminal, Write};

use packsync_app::StorageSetting;
use packsync_core::error::{Error, Result};
use packsync_drive::oauth::{PrintUrl, SystemBrowser, UrlOpener};

use super::Cx;
use crate::args::{AuthCmd, DeviceArgs, SettingsCmd, StorageCmd};
use crate::render::bytes;

fn print_status(w: &mut dyn Write, s: &packsync_app::provider::StorageStatus) -> std::io::Result<()> {
    writeln!(w, "{}{}", s.detail, if s.connected { "  — connected" } else { "  — not connected" })?;
    if s.needs_reconnect {
        writeln!(w, "  sign in again: `packsync auth login`")?;
    }
    if let Some(p) = &s.problem {
        writeln!(w, "  problem: {p}")?;
    }
    if let Some(h) = &s.setup_hint {
        writeln!(w, "  {h}")?;
    }
    Ok(())
}

pub fn storage(cx: &Cx, c: StorageCmd) -> Result<()> {
    let ctx = cx.out.ctx();
    match c {
        StorageCmd::Status => {
            let s = cx.app.storage_status();
            cx.out.emit(&s, |w| print_status(w, &s))
        }
        StorageCmd::UseLocal { dir } => {
            cx.app.use_local_storage(&dir)?;
            let s = cx.app.storage_status();
            cx.out.emit(&s, |w| print_status(w, &s))
        }
        StorageCmd::UseGoogle { space } => {
            let s = cx.app.use_google(&space)?;
            cx.out.emit(&s, |w| print_status(w, &s))?;
            match &s.problem {
                Some(p) if !s.oauth_client_configured => {
                    Err(Error::Config(format!("{p}. {}", s.setup_hint.clone().unwrap_or_default())))
                }
                _ => Ok(()),
            }
        }
        StorageCmd::UseR2 { account_id, bucket, access_key_id, prefix, secret_stdin } => {
            let secret = if secret_stdin || !std::io::stdin().is_terminal() {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map_err(|e| Error::io("reading the secret access key", e))?;
                line.trim().to_string()
            } else {
                rpassword::prompt_password("R2 secret access key (not shown): ")
                    .map_err(|e| Error::io("reading the secret access key", e))?
                    .trim()
                    .to_string()
            };
            let secret = packsync_core::redact::Secret::new(secret);
            let s = cx.app.use_r2(&account_id, &bucket, &prefix, &access_key_id, &secret, &ctx)?;
            cx.out.emit(&s, |w| {
                print_status(w, &s)?;
                writeln!(w, "The connection works; the secret access key is in the OS credential store.")
            })
        }
        StorageCmd::Test => {
            let r = cx.app.test_storage(&ctx)?;
            cx.out.emit(&r, |w| {
                writeln!(w, "The connection works.")?;
                writeln!(w, "  storage:   {}", r["provider"].as_str().unwrap_or("?"))?;
                writeln!(w, "  instances: {} in the cloud", r["instances_in_cloud"])?;
                writeln!(w, "  listing:   {} ms", r["list_ms"])?;
                match (r["quota"]["used"].as_u64(), r["quota"]["total"].as_u64()) {
                    (Some(u), Some(t)) => writeln!(w, "  space:     {} used of {}", bytes(u), bytes(t)),
                    (Some(u), None) => writeln!(w, "  space:     {} used", bytes(u)),
                    _ => writeln!(w, "  space:     not reported by this storage"),
                }
            })
        }
        StorageCmd::Disconnect { revoke } => {
            if !cx
                .out
                .confirm("Disconnect the storage? Cloud data is not touched; syncing stops until you connect again.")?
            {
                return Err(Error::Cancelled);
            }
            cx.app.disconnect_storage(revoke, &ctx)?;
            cx.out.emit(&cx.app.storage_status(), |w| writeln!(w, "Disconnected."))
        }
    }
}

pub fn auth(cx: &Cx, c: AuthCmd) -> Result<()> {
    let ctx = cx.out.ctx();
    match c {
        AuthCmd::Login { no_browser } => {
            match cx.app.settings().storage {
                StorageSetting::Google { .. } => {}
                StorageSetting::None => {
                    cx.app.use_google("visible_folder")?;
                }
                StorageSetting::Local { .. } | StorageSetting::R2 { .. } => {
                    return Err(Error::Config(
                        "Google Drive is not the selected storage; run `packsync storage use-google` first".into(),
                    ));
                }
            }
            let opener: Box<dyn UrlOpener> =
                if no_browser { Box::new(PrintUrl) } else { Box::new(SystemBrowser { print_fallback: true }) };
            cx.out.note(
                "Opening your browser to sign in with Google. PackSync only asks for access to the files it creates.",
            );
            let info = cx.app.google_login(&*opener, &ctx)?;
            cx.out.emit(&serde_json::json!({ "account": info }), |w| {
                writeln!(w, "Signed in as {}.", info.email.as_deref().unwrap_or("(unknown)"))
            })
        }
        AuthCmd::Status => {
            let s = cx.app.storage_status();
            cx.out.emit(&s, |w| print_status(w, &s))
        }
        AuthCmd::Logout { revoke } => storage(cx, StorageCmd::Disconnect { revoke }),
    }
}

pub fn settings(cx: &Cx, c: SettingsCmd) -> Result<()> {
    match c {
        SettingsCmd::Show => {
            let s = cx.app.settings();
            cx.out.emit(&s, |w| writeln!(w, "{}", serde_json::to_string_pretty(&s).unwrap_or_default()))
        }
        SettingsCmd::Set { key, value } => {
            let bad = |what: &str| Error::invalid("setting", format!("{key}: {what}"));
            let up = match key.as_str() {
                "device_name" => cx.app.update_settings(|s| s.device_name = Some(value.clone())),
                "hook_offline_policy" => {
                    let p = match value.as_str() {
                        "play_local" => packsync_app::OfflinePolicy::PlayLocal,
                        "block" => packsync_app::OfflinePolicy::Block,
                        _ => return Err(bad("use play_local or block")),
                    };
                    cx.app.update_settings(|s| s.hook_offline_policy = p)
                }
                "hook_block_on_problem" => {
                    let v: bool = value.parse().map_err(|_| bad("use true or false"))?;
                    cx.app.update_settings(|s| s.hook_block_on_problem = v)
                }
                "upload_concurrency" => {
                    let v: usize = value.parse().map_err(|_| bad("use a number"))?;
                    cx.app.update_settings(|s| s.upload_concurrency = v)
                }
                "recovery_point_days" => {
                    let v: u32 = value.parse().map_err(|_| bad("use a number of days"))?;
                    cx.app.update_settings(|s| s.recovery_point_days = v)
                }
                "lease_ttl_secs" => {
                    let v: u32 = value.parse().map_err(|_| bad("use a number of seconds"))?;
                    cx.app.update_settings(|s| s.lease_ttl_secs = v)
                }
                "cli_path" => cx.app.update_settings(|s| s.cli_path = Some(value.clone().into())),
                "prism_executable" => cx.app.update_settings(|s| s.prism_executable = Some(value.clone().into())),
                "prism_data_dir" => cx.app.update_settings(|s| s.prism_data_dirs = vec![value.clone().into()]),
                "google_client_file" => cx.app.update_settings(|s| s.google_client_file = Some(value.clone().into())),
                _ => return Err(bad("unknown setting (see `packsync settings show`)")),
            }?;
            if let Some(p) = &up.storage_problem {
                cx.out.warn(p);
            }
            cx.out.emit(&up.settings, |w| writeln!(w, "{key} updated."))
        }
    }
}

pub fn device(cx: &Cx, a: DeviceArgs) -> Result<()> {
    if let Some(n) = a.name {
        cx.app.update_settings(|s| s.device_name = Some(n.clone()))?;
    }
    let (id, name) = cx.app.engine().device();
    cx.out.emit(&serde_json::json!({ "id": id, "name": name }), |w| {
        writeln!(w, "This computer is called {name:?} ({id}).")
    })
}
