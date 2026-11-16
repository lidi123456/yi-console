//! Tray mode: while a Bot is enabled, closing the main window hides it and the app keeps running in the system tray,
//! so Bots keep receiving triggers. Without enabled Bots (or with `closeToTray` off) closing quits as before.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

pub struct BotsState(pub Option<Arc<yi_bots::Bots>>);

/// "Quit" from the tray: let the close go through
static QUITTING: AtomicBool = AtomicBool::new(false);
static TRAY: AtomicBool = AtomicBool::new(false);

pub fn setup(_app: &mut tauri::App) -> tauri::Result<()> {
    Ok(())
}

pub fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The tray icon is created the first time the window hides into it
fn ensure_tray(app: &AppHandle) -> tauri::Result<()> {
    if TRAY.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let open = MenuItem::with_id(app, "open", "Yi Console", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 / Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut b = TrayIconBuilder::with_id("main").tooltip("Yi Console").menu(&menu).on_menu_event(|app, e| {
        match e.id.as_ref() {
            "open" => show(app),
            "quit" => {
                QUITTING.store(true, Ordering::SeqCst);
                app.exit(0);
            }
            _ => {}
        }
    });
    if let Some(icon) = app.default_window_icon() {
        b = b.icon(icon.clone());
    }
    b.build(app)?;
    Ok(())
}

pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    let tauri::WindowEvent::CloseRequested { api, .. } = event else { return };
    if QUITTING.load(Ordering::SeqCst) || window.label() != "main" {
        return;
    }
    let app = window.app_handle();
    let bots = app.try_state::<BotsState>().and_then(|s| s.0.clone());
    let tray = app.try_state::<crate::Hub>().is_some_and(|h| h.0.close_to_tray());
    if tray && bots.is_some_and(|b| b.any_enabled()) && ensure_tray(app).is_ok() {
        api.prevent_close();
        let _ = window.hide();
    }
}
