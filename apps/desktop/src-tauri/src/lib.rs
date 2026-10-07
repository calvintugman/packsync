//! The PackSync desktop shell.
//!
//! This crate holds no sync logic and no UI logic. It opens the same application layer the command-line
//! tool uses (`packsync-app`), exposes its JSON method table to the web UI as two Tauri commands, and
//! forwards the application's events to the window:
//!
//! * `call(method, params)` runs a short method and returns its JSON result;
//! * `start_job(method, params)` starts a long method in the background and returns the job id — progress
//!   and the final result arrive as events;
//! * every [`AppEvent`](packsync_app::AppEvent) is emitted to the window as `packsync://event`.
//!
//! Errors cross the boundary as [`ErrorView`] (code, message, advice), never as Rust debug output.

use std::sync::Arc;

use packsync_app::{App, AppOptions, AppPaths, ErrorView, logging, rpc};
use serde_json::Value;
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// The event name the UI listens to (`TAURI_EVENT` in `src/api/transport.ts`).
pub const EVENT: &str = "packsync://event";

struct AppState(Arc<App>);

fn internal(message: String) -> ErrorView {
    ErrorView { code: "internal".into(), message, advice: Some("Please report this, with Settings → Run checks.".into()), exit_code: 1 }
}

/// Run a short application method. The engine blocks, so it runs on a thread that may block.
#[tauri::command]
async fn call(state: State<'_, AppState>, method: String, params: Value) -> Result<Value, ErrorView> {
    let app = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || rpc::call(&app, &method, params, &app.ctx()))
        .await
        .map_err(|e| internal(format!("the operation stopped unexpectedly: {e}")))?
        .map_err(|e| ErrorView::from(&e))
}

/// Start a long application method as a background job; returns its id.
#[tauri::command]
async fn start_job(state: State<'_, AppState>, method: String, params: Value) -> Result<String, ErrorView> {
    let app = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || rpc::start_job(&app, &method, params))
        .await
        .map_err(|e| internal(format!("the operation stopped unexpectedly: {e}")))?
        .map(|id| id.to_string())
        .map_err(|e| ErrorView::from(&e))
}

fn fatal(app: &tauri::AppHandle, message: &str) -> ! {
    app.dialog().message(message).title("PackSync can't start").kind(MessageDialogKind::Error).blocking_show();
    std::process::exit(1)
}

pub fn run() {
    tauri::Builder::default()
        // a second launch focuses the window that is already open instead of opening a second engine
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|tauri_app| {
            let paths = AppPaths::discover();
            let _log = logging::init(&paths.logs_dir, false);
            let app = match App::open(paths, AppOptions::default()) {
                Ok(a) => a,
                Err(e) => fatal(tauri_app.handle(), &format!("{e}\n\nYour files were not touched.")),
            };
            // events from the application layer → the window
            let rx = app.bus().subscribe();
            let handle = tauri_app.handle().clone();
            std::thread::Builder::new()
                .name("events".into())
                .spawn(move || {
                    for ev in rx {
                        let _ = handle.emit(EVENT, &ev);
                    }
                })?;
            app.start_upload_retries(std::time::Duration::from_secs(60));
            tauri_app.manage(AppState(app));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![call, start_job])
        .run(tauri::generate_context!())
        .expect("error while running the PackSync desktop app");
}
