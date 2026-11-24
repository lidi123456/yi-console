//! Resolves real local file system paths for files dropped onto the main Windows WebView2 window.
//! The window keeps `dragDropEnabled: false` so HTML5 `draggable` items (file tree rows, queue items, accounts)
//! continue working; on drop, the frontend sends the `File` objects via
//! `chrome.webview.postMessageWithAdditionalObjects({ __yiDropPaths: id }, files)` and receives their disk paths
//! over the `"drop-paths"` Tauri event.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Url, WebviewWindow};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2File, ICoreWebView2WebMessageReceivedEventArgs2,
};
use webview2_com::{take_pwstr, WebMessageReceivedEventHandler};
use windows::core::{Interface, PWSTR};

#[derive(Deserialize)]
struct DropReq {
    #[serde(rename = "__yiDropPaths")]
    id: String,
}

#[derive(Clone, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DroppedEntry {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DropPathsEvent {
    id: String,
    entries: Vec<Option<DroppedEntry>>,
}

/// Parse `{ "__yiDropPaths": "<id>" }` from WebView2's JSON message representation.
/// Non-matching or plain string IPC messages return `None` so Tauri's own IPC handler runs unaffected.
fn parse_drop_req(raw_json: &str) -> Option<String> {
    let req: DropReq = serde_json::from_str(raw_json).ok()?;
    (!req.id.is_empty()).then_some(req.id)
}

/// Strip the Windows verbatim prefix (`\\?\C:\...`) when followed by an ASCII drive letter.
fn strip_verbatim(p: &str) -> String {
    let bytes = p.as_bytes();
    if bytes.len() >= 6
        && bytes.starts_with(br"\\?\")
        && bytes[4].is_ascii_alphabetic()
        && bytes[5] == b':'
    {
        p[4..].to_string()
    } else {
        p.to_string()
    }
}

fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

pub fn setup(app: &AppHandle, w: &WebviewWindow) {
    let app = app.clone();
    let _ = w.with_webview(move |pw| unsafe {
        let Ok(core) = pw.controller().CoreWebView2() else {
            return;
        };
        let mut token: i64 = 0;
        let handler = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let json = {
                let mut raw = PWSTR::null();
                if args.WebMessageAsJson(&mut raw).is_err() {
                    return Ok(());
                }
                take_pwstr(raw)
            };
            let Some(id) = parse_drop_req(&json) else {
                return Ok(());
            };
            let src = {
                let mut raw = PWSTR::null();
                if args.Source(&mut raw).is_err() {
                    return Ok(());
                }
                take_pwstr(raw)
            };
            let expected_origin = app
                .get_webview("main")
                .and_then(|v| v.url().ok())
                .map(|u| origin_of(&u));
            let source_origin = Url::parse(&src).ok().map(|u| origin_of(&u));
            if expected_origin.is_none() || source_origin != expected_origin {
                return Ok(());
            }

            let mut entries = Vec::new();
            if let Ok(args2) = args.cast::<ICoreWebView2WebMessageReceivedEventArgs2>() {
                if let Ok(col) = args2.AdditionalObjects() {
                    let mut count = 0u32;
                    if col.Count(&mut count).is_ok() {
                        entries.reserve(count as usize);
                        for i in 0..count {
                            let entry = (|| -> Option<DroppedEntry> {
                                let item = col.GetValueAtIndex(i).ok()?;
                                let file = item.cast::<ICoreWebView2File>().ok()?;
                                let mut raw = PWSTR::null();
                                file.Path(&mut raw).ok()?;
                                let path = strip_verbatim(&take_pwstr(raw));
                                let meta = std::fs::metadata(&path).ok()?;
                                Some(DroppedEntry {
                                    is_dir: meta.is_dir(),
                                    path,
                                })
                            })();
                            entries.push(entry);
                        }
                    }
                }
            }

            let _ = app.emit_to("main", "drop-paths", DropPathsEvent { id, entries });
            Ok(())
        }));
        let _ = core.add_WebMessageReceived(&handler, &mut token);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_drop_request_only() {
        assert_eq!(parse_drop_req(r#"{"__yiDropPaths":"req-1"}"#), Some("req-1".into()));
        assert_eq!(parse_drop_req(r#"{"__yiDropPaths":""}"#), None);
        assert_eq!(parse_drop_req(r#""plain-ipc-string""#), None);
        assert_eq!(parse_drop_req(r#"{"cmd":"hub_call"}"#), None);
        assert_eq!(parse_drop_req("not-json"), None);
    }

    #[test]
    fn strips_verbatim_drive_prefix() {
        assert_eq!(strip_verbatim(r"\\?\D:\work\a.txt"), r"D:\work\a.txt");
        assert_eq!(strip_verbatim(r"\\?\c:\a.txt"), r"c:\a.txt");
        assert_eq!(strip_verbatim(r"\\?\UNC\server\share\a.txt"), r"\\?\UNC\server\share\a.txt");
        assert_eq!(strip_verbatim(r"D:\work\a.txt"), r"D:\work\a.txt");
    }
}
