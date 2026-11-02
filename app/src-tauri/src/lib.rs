//! Tauri 外壳：前端经 `hub_call` 调 ClientHub，ClientHub 的事件以 `hub-event` 推给前端。
//! 浏览器模式（yi-web）走的是同一套 ClientHub.call，两边行为一致。

mod clipfiles;

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

/// 弹系统的"另存为"对话框，把 base64 内容写到选中的位置；用户取消返回 false
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

/// 默认窗口 1320×800；屏幕放不下（比如 1366×768 的笔记本）时缩到工作区的 92% 并居中
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

/// 手机上没有本机 yi-agentd，只能连 SSH 主机或经中继连电脑
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
        // 记住窗口大小和位置，下次按上次的打开
        .plugin(tauri_plugin_window_state::Builder::default().build());
    builder
        // 浏览器标签预览主机上的网页：ahfs://localhost/<令牌>/<相对路径>（Windows / Android 上是 http://ahfs.localhost/…）
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
            // 打包进安装包的各平台 yi-agentd，给 SSH 主机自动部署用；手机上不带
            let yi_agentd_dir = if LOCAL_SUPPORTED {
                app.path().resource_dir().ok().map(|d| d.join("yi-agentd-dist"))
            } else {
                None
            };
            let handle = app.handle().clone();
            let sink = Arc::new(move |v: Value| {
                let _ = handle.emit("hub-event", v);
            });
            // ClientHub::new 会为自动连接的主机起后台任务，需要在 tokio 运行时里调用
            let hub = tauri::async_runtime::block_on(async move {
                ClientHub::new(data_dir, yi_agentd_dir, LOCAL_SUPPORTED, sink)
            });
            app.manage(Hub(hub));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![hub_call, save_file, clipfiles::clipboard_files])
        .run(tauri::generate_context!())
        .expect("启动 Yi Console 失败");
}
