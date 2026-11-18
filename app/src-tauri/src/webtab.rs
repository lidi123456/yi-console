//! Web tabs of the file panel as native child webviews of the main window (Windows only for now).
//! An iframe can't show sites that forbid embedding and can't follow navigation in cross-origin pages;
//! a child webview is a real top-level page. The frontend keeps a placeholder element in the panel and
//! sends its rectangle (logical px) here; it hides the webview while a menu or dialog covers it, because
//! a native view always paints above the main webview.
//!
//! Whatever of the app lies on top of the view is cut out of the view's window region (`set_holes`), so the app's
//! element shows through and the rest of the page stays live; the frontend swaps the view for a capture
//! (`webtab_capture`) when something covers most of it.
//!
//! Safety: pages here are remote, so Tauri's ACL already rejects their IPC calls (`Webview::on_message`
//! checks every non-local origin). What we must prevent is the child landing on a local origin, where IPC
//! would be allowed: the app itself, its custom protocols (ahfs, ipc, asset) or anything that isn't http(s).

use serde::{Deserialize, Serialize};
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Rect, Url, WebviewBuilder, WebviewUrl};

const PREFIX: &str = "webtab-";

#[derive(Deserialize)]
pub struct Bounds {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Ev {
    label: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

fn emit(app: &AppHandle, label: &str, kind: &'static str, url: Option<String>, title: Option<String>) {
    let _ = app.emit_to("main", "webtab", Ev { label: label.to_string(), kind, url, title });
}

fn check_label(l: &str) -> Result<(), String> {
    let id = l.strip_prefix(PREFIX).ok_or("标签页名称无效")?;
    if id.is_empty() || id.len() > 40 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("标签页名称无效".into());
    }
    Ok(())
}

/// Origin of the app's own pages (tauri://localhost, http://tauri.localhost or the dev server)
fn app_origin(app: &AppHandle) -> Option<String> {
    app.get_webview("main").and_then(|w| w.url().ok()).map(|u| u.origin().ascii_serialization())
}

/// Hosts Tauri uses for custom protocols on Windows (`http://<scheme>.localhost`)
const INTERNAL_HOSTS: [&str; 4] = ["tauri.localhost", "ipc.localhost", "ahfs.localhost", "asset.localhost"];

fn allowed(app_origin: Option<&str>, u: &Url) -> bool {
    if u.scheme() == "about" {
        return u.as_str() == "about:blank";
    }
    if !matches!(u.scheme(), "http" | "https") {
        return false;
    }
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    if INTERNAL_HOSTS.contains(&host.as_str()) {
        return false;
    }
    app_origin != Some(u.origin().ascii_serialization().as_str())
}

fn parse(app: &AppHandle, url: &str) -> Result<Url, String> {
    let u = Url::parse(url).map_err(|_| "地址无效".to_string())?;
    if !matches!(u.scheme(), "http" | "https") || !allowed(app_origin(app).as_deref(), &u) {
        return Err("这个地址不能在这里打开".into());
    }
    Ok(u)
}

fn rect(b: &Bounds) -> Rect {
    Rect {
        position: LogicalPosition::new(b.x, b.y).into(),
        size: LogicalSize::new(b.w.max(1.0), b.h.max(1.0)).into(),
    }
}

/// Create the child webview (async: building a webview from a sync command deadlocks on Windows)
#[tauri::command]
pub async fn webtab_open(app: AppHandle, label: String, url: String, bounds: Bounds) -> Result<(), String> {
    check_label(&label)?;
    let u = parse(&app, &url)?;
    if let Some(w) = app.get_webview(&label) {
        w.navigate(u).map_err(|e| e.to_string())?;
        return w.set_bounds(rect(&bounds)).map_err(|e| e.to_string());
    }
    let window = app.get_window("main").ok_or("找不到主窗口")?;
    let origin = app_origin(&app);
    let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app.clone());
    let (l1, l2, l3, l4) = (label.clone(), label.clone(), label.clone(), label.clone());
    let builder = WebviewBuilder::new(&label, WebviewUrl::External(u))
        .on_navigation(move |u| {
            let ok = allowed(origin.as_deref(), u);
            if !ok {
                emit(&a1, &l1, "blocked", Some(u.to_string()), None);
            }
            ok
        })
        // Links with target=_blank and window.open become new tabs in the panel
        .on_new_window(move |u, _| {
            emit(&a2, &l2, "newWindow", Some(u.to_string()), None);
            NewWindowResponse::Deny
        })
        .on_page_load(move |_, p| {
            let kind = match p.event() {
                PageLoadEvent::Started => "loading",
                PageLoadEvent::Finished => "loaded",
            };
            emit(&a3, &l3, kind, Some(p.url().to_string()), None);
        })
        // Also the only signal for SPA route changes (pushState fires no page load)
        .on_document_title_changed(move |w, t| emit(&a4, &l4, "title", w.url().ok().map(|u| u.to_string()), Some(t)));
    window
        .add_child(builder, LogicalPosition::new(bounds.x, bounds.y), LogicalSize::new(bounds.w.max(1.0), bounds.h.max(1.0)))
        .map_err(|e| format!("打开网页失败: {e}"))?;
    Ok(())
}

/// Move / resize, show or hide, and cut `holes` (logical px, relative to the view) out of it
#[tauri::command]
pub async fn webtab_bounds(app: AppHandle, label: String, bounds: Bounds, visible: bool, holes: Option<Vec<Bounds>>) -> Result<(), String> {
    check_label(&label)?;
    let Some(w) = app.get_webview(&label) else { return Ok(()) };
    if visible {
        w.set_bounds(rect(&bounds)).map_err(|e| e.to_string())?;
        let scale = w.window().scale_factor().unwrap_or(1.0);
        set_holes(&w, &bounds, holes.unwrap_or_default(), scale);
        w.show().map_err(|e| e.to_string())
    } else {
        w.hide().map_err(|e| e.to_string())
    }
}

/// The view's window region: everything but the holes (no region at all without holes)
fn set_holes(w: &tauri::Webview, b: &Bounds, holes: Vec<Bounds>, scale: f64) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, RGN_DIFF};
    let px = move |v: f64| (v * scale).round() as i32;
    let (bw, bh) = (px(b.w), px(b.h));
    let _ = w.with_webview(move |pw| unsafe {
        let mut hwnd = HWND::default();
        if pw.controller().ParentWindow(&mut hwnd).is_err() || hwnd.is_invalid() {
            return;
        }
        if holes.is_empty() {
            SetWindowRgn(hwnd, None, true);
            return;
        }
        let rgn = CreateRectRgn(0, 0, bw, bh);
        for h in &holes {
            let cut = CreateRectRgn(px(h.x).max(0), px(h.y).max(0), px(h.x + h.w).min(bw), px(h.y + h.h).min(bh));
            CombineRgn(Some(rgn), Some(rgn), Some(cut), RGN_DIFF);
            let _ = DeleteObject(cut.into());
        }
        // The system owns the region from here on
        SetWindowRgn(hwnd, Some(rgn), true);
    });
}

/// JPEG of the page as a data URL, shown in place of the view while a modal covers it
#[tauri::command]
pub async fn webtab_capture(app: AppHandle, label: String) -> Result<String, String> {
    use base64::Engine;
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_JPEG;
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};

    check_label(&label)?;
    let w = app.get_webview(&label).ok_or("网页已关闭")?;
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<u8>, String>>();
    w.with_webview(move |pw| unsafe {
        let start = || -> windows::core::Result<()> {
            let core = pw.controller().CoreWebView2()?;
            let stream: IStream = CreateStreamOnHGlobal(Default::default(), true)?;
            let s2 = stream.clone();
            let handler = CapturePreviewCompletedHandler::create(Box::new(move |r| {
                let read = || -> windows::core::Result<Vec<u8>> {
                    r?;
                    let mut st = STATSTG::default();
                    s2.Stat(&mut st, STATFLAG_NONAME)?;
                    s2.Seek(0, STREAM_SEEK_SET, None)?;
                    let mut buf = vec![0u8; st.cbSize as usize];
                    let mut got = 0u32;
                    s2.Read(buf.as_mut_ptr() as _, buf.len() as u32, Some(&mut got)).ok()?;
                    buf.truncate(got as usize);
                    Ok(buf)
                };
                let _ = tx.send(read().map_err(|e| e.to_string()));
                Ok(())
            }));
            core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_JPEG, &stream, &handler)
        };
        if let Err(e) = start() {
            eprintln!("webtab capture: {e}");
        }
    })
    .map_err(|e| e.to_string())?;
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(3), rx)
        .await
        .map_err(|_| "截图超时".to_string())?
        .map_err(|_| "截图失败".to_string())??;
    Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// back / forward / reload / go (with url) / focus
#[tauri::command]
pub async fn webtab_nav(app: AppHandle, label: String, action: String, url: Option<String>) -> Result<(), String> {
    check_label(&label)?;
    let w = app.get_webview(&label).ok_or("网页已关闭")?;
    let r = match action.as_str() {
        "back" => w.eval("history.back()"),
        "forward" => w.eval("history.forward()"),
        "reload" => w.reload(),
        "focus" => w.set_focus(),
        "go" => w.navigate(parse(&app, url.as_deref().unwrap_or(""))?),
        _ => return Err("未知操作".into()),
    };
    r.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn webtab_close(app: AppHandle, label: String) -> Result<(), String> {
    check_label(&label)?;
    if let Some(w) = app.get_webview(&label) {
        w.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Close every web tab webview: the frontend calls this when it (re)loads, so tabs of a previous page don't linger
#[tauri::command]
pub async fn webtab_reset(app: AppHandle) {
    for (label, w) in app.webviews() {
        if label.starts_with(PREFIX) {
            let _ = w.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_local_origins() {
        let app = Some("http://tauri.localhost");
        let ok = |s: &str| allowed(app, &Url::parse(s).unwrap());
        assert!(ok("https://example.com/x"));
        assert!(ok("http://localhost:3000/"));
        assert!(ok("about:blank"));
        assert!(!ok("http://tauri.localhost/index.html"));
        assert!(!ok("http://ahfs.localhost/tok/a.html"));
        assert!(!ok("https://IPC.localhost/x"));
        assert!(!ok("file:///C:/x.html"));
        assert!(!ok("javascript:alert(1)"));
        assert!(!ok("about:srcdoc"));
        assert!(!allowed(Some("http://localhost:1420"), &Url::parse("http://localhost:1420/").unwrap()));
        assert!(check_label("webtab-a1").is_ok());
        assert!(check_label("main").is_err());
        assert!(check_label("webtab-").is_err());
        assert!(check_label("webtab-a/b").is_err());
    }
}
