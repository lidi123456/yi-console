//! Desktop self-update (tauri-plugin-updater): `latest.json` from the latest GitHub release (endpoint and public key in tauri.conf.json).
//! The frontend (app/src/update.ts) calls `update_check`, then `update_install`, which downloads with `update-progress` events,
//! installs and restarts. On Windows the NSIS installer (passive mode) takes over: the app exits and the installer relaunches it.
//! The local yi-agentd runs from ~/.yi-console/bin, not the install directory, so the installer can replace the bundled copy;
//! after the restart ClientHub sees the version mismatch and upgrades the daemon as usual.

use serde_json::{json, Value};
use tauri::async_runtime::Mutex;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The update found by the last check, installed by `update_install`; the lock also keeps two installs from running at once
#[derive(Default)]
pub struct Pending(Mutex<Option<Update>>);

/// Linux installs only update in place as an AppImage (the updater overwrites $APPIMAGE); .deb / .rpm installs get a download link instead
fn installable() -> bool {
    !cfg!(target_os = "linux") || std::env::var_os("APPIMAGE").is_some()
}

/// `{current, installable, update: null | {version, notes, date}}`
#[tauri::command]
pub async fn update_check(app: AppHandle, pending: State<'_, Pending>) -> Result<Value, String> {
    let current = app.package_info().version.to_string();
    let found = app
        .updater()
        .map_err(|e| format!("{e}"))?
        .check()
        .await
        .map_err(|e| format!("检查更新失败: {e}"))?;
    let info = found.as_ref().map(|u| {
        json!({
            "version": u.version,
            "notes": u.body,
            "date": u.date.map(|d| d.unix_timestamp() * 1000),
        })
    });
    let mut slot = pending.0.try_lock().map_err(|_| "正在安装更新".to_string())?;
    *slot = found;
    Ok(json!({ "current": current, "installable": installable(), "update": info }))
}

/// Download (progress as `update-progress` {got, total}), verify the signature, install and restart. Only returns on failure
#[tauri::command]
pub async fn update_install(app: AppHandle, pending: State<'_, Pending>) -> Result<(), String> {
    if !installable() {
        return Err("这种安装方式不支持自动更新，请到发布页下载新版本".into());
    }
    let slot = pending.0.try_lock().map_err(|_| "正在安装更新".to_string())?;
    let Some(update) = slot.as_ref() else {
        return Err("没有可安装的更新，请先检查更新".into());
    };
    let h = app.clone();
    let mut got: u64 = 0;
    let mut last: u64 = 0;
    update
        .download_and_install(
            move |chunk, total| {
                got += chunk as u64;
                // About every 256 KB, plus the last chunk
                if got - last >= 256 * 1024 || total.is_some_and(|t| got >= t) {
                    last = got;
                    let _ = h.emit("update-progress", json!({ "got": got, "total": total }));
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("安装更新失败: {e}"))?;
    app.restart();
}
