//! Tauri shell: the frontend calls ClientHub through `hub_call`, and ClientHub events are pushed to the frontend as `hub-event`.
//! Browser mode (yi-web) goes through the same ClientHub.call, so both behave the same.

mod clipfiles;
#[cfg(desktop)]
mod updater;

/// No tauri-plugin-updater on mobile: Android updates itself in MainActivity.kt (YiConsoleAndroid.update*), these only keep the handler list the same
#[cfg(mobile)]
mod updater {
    #[tauri::command]
    pub async fn update_check() -> Result<serde_json::Value, String> {
        Err("手机上不支持".into())
    }
    #[tauri::command]
    pub async fn update_install() -> Result<(), String> {
        Err("手机上不支持".into())
    }
}

use std::sync::Arc;

use yi_client::ClientHub;
use serde_json::Value;
use tauri::{Emitter, Manager, State};

struct Hub(Arc<ClientHub>);

#[tauri::command]
async fn hub_call(hub: State<'_, Hub>, cmd: String, args: Value) -> Result<Value, String> {
    let hub = hub.0.clone();
    hub.call(&cmd, args).await.map_err(|e| format!("{e:#}"))
}

/// Show the system "Save as" dialog and write the base64 content to the chosen location; returns false if the user cancels
#[tauri::command]
async fn save_file(app: tauri::AppHandle, name: String, data: String) -> Result<bool, String> {
    #[cfg(desktop)]
    {
        use base64::Engine;
        use tauri_plugin_dialog::DialogExt;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("数据有误: {e}"))?;
        let mut dlg = app.dialog().file().set_file_name(&name);
        if let Some(ext) = std::path::Path::new(&name).extension().and_then(|e| e.to_str()) {
            dlg = dlg.add_filter(ext.to_uppercase(), &[ext]);
        }
        let Some(path) = dlg.blocking_save_file() else {
            return Ok(false);
        };
        let path = path.into_path().map_err(|e| format!("{e}"))?;
        std::fs::write(&path, bytes).map_err(|e| format!("保存失败: {e}"))?;
        Ok(true)
    }
    #[cfg(mobile)]
    {
        let _ = (app, name, data);
        Err("手机上暂不支持另存为".into())
    }
}

/// Default window is 1320×800; if the screen can't fit it (e.g. a 1366×768 laptop), shrink to 92% of the work area and center
#[cfg(desktop)]
fn fit_to_screen(w: &tauri::WebviewWindow) {
    let (Ok(Some(m)), Ok(size)) = (w.current_monitor(), w.outer_size()) else { return };
    let area = m.work_area().size;
    let max_w = area.width * 92 / 100;
    let max_h = area.height * 92 / 100;
    if size.width > max_w || size.height > max_h {
        let _ = w.set_size(tauri::PhysicalSize::new(size.width.min(max_w), size.height.min(max_h)));
        let _ = w.center();
    }
}

/// There's no local yi-agentd on mobile; it can only connect to SSH hosts or to computers via a relay
const LOCAL_SUPPORTED: bool = cfg!(not(any(target_os = "android", target_os = "ios")));

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init());
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_opener::init());
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        // Remember window size and position and reopen with them next time
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updater::Pending::default());
    builder
        // Browser tabs previewing web pages on the host: ahfs://localhost/<token>/<relative path> (http://ahfs.localhost/… on Windows / Android)
        .register_asynchronous_uri_scheme_protocol("ahfs", |ctx, req, responder| {
            let hub = ctx.app_handle().state::<Hub>().0.clone();
            let path = req.uri().path().to_string();
            tauri::async_runtime::spawn(async move {
                let (code, ctype, body) = match hub.serve_preview(&path).await {
                    Ok((b, m)) => (200, m, b),
                    Err(e) => (404, "text/plain; charset=utf-8", format!("{e:#}").into_bytes()),
                };
                let resp = tauri::http::Response::builder()
                    .status(code)
                    .header("Content-Type", ctype)
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Cache-Control", "no-cache")
                    .body(body)
                    .unwrap();
                responder.respond(resp);
            });
        })
        .setup(|app| {
            #[cfg(desktop)]
            if let Some(w) = app.get_webview_window("main") {
                fit_to_screen(&w);
            }
            let data_dir = app.path().app_data_dir()?;
            // yi-agentd builds for every platform bundled in the installer, for automatic deployment to SSH hosts; not shipped on mobile
            let yi_agentd_dir = if LOCAL_SUPPORTED {
                app.path().resource_dir().ok().map(|d| d.join("yi-agentd-dist"))
            } else {
                None
            };
            let handle = app.handle().clone();
            let sink = Arc::new(move |v: Value| {
                let _ = handle.emit("hub-event", v);
            });
            // ClientHub::new spawns background tasks for auto-connecting hosts, so it must be called inside the tokio runtime
            let hub = tauri::async_runtime::block_on(async move {
                ClientHub::new(data_dir, yi_agentd_dir, LOCAL_SUPPORTED, sink)
            });
            app.manage(Hub(hub));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            hub_call,
            save_file,
            clipfiles::clipboard_files,
            updater::update_check,
            updater::update_install
        ])
        .run(tauri::generate_context!())
        .expect("启动 Yi Console 失败");
}
