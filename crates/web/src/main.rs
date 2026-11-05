//! yi-web: use Yi Console from a browser.
//!
//!   yi-web [--listen 127.0.0.1:3001] [--dist app/dist] [--data <data dir>] [--yi-agentd-dir <dir>] [--token <token>]
//!
//! Static files are served directly; `/ws?token=` is the channel between the frontend and ClientHub:
//!   frontend -> {"id":1,"cmd":"hosts.list","args":{}}
//!   backend -> {"id":1,"result":...} / {"id":1,"error":"..."} / {"event":{...}}
//! Only listening on 127.0.0.1 is recommended; the token keeps other web pages on the same machine from connecting.

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use yi_client::ClientHub;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

struct Args {
    listen: String,
    dist: PathBuf,
    data: PathBuf,
    yi_agentd_dir: Option<PathBuf>,
    token: String,
}

fn args() -> Result<Args> {
    let mut a = Args {
        listen: "127.0.0.1:3001".into(),
        dist: PathBuf::from("app/dist"),
        data: dirs::home_dir().unwrap_or_default().join(".yi-console-web"),
        yi_agentd_dir: None,
        token: yi_link::random_id(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| anyhow!("{k} 缺少取值"));
        match k.as_str() {
            "--listen" => a.listen = v()?,
            "--dist" => a.dist = PathBuf::from(v()?),
            "--data" => a.data = PathBuf::from(v()?),
            "--yi-agentd-dir" => a.yi_agentd_dir = Some(PathBuf::from(v()?)),
            "--token" => a.token = v()?,
            _ => return Err(anyhow!("未知参数 {k}")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = args()?;
    let (tx, _) = broadcast::channel::<Value>(4096);
    let tx2 = tx.clone();
    let hub = ClientHub::new(
        a.data.clone(),
        a.yi_agentd_dir.clone(),
        true,
        Arc::new(move |v| {
            let _ = tx2.send(v);
        }),
    );
    let listener = TcpListener::bind(&a.listen).await?;
    println!("Yi Console Web: http://{}/?token={}", a.listen, a.token);
    let dist = Arc::new(a.dist);
    let token = Arc::new(a.token);
    loop {
        let (s, _) = listener.accept().await?;
        let (hub, tx, dist, token) = (hub.clone(), tx.clone(), dist.clone(), token.clone());
        tokio::spawn(async move {
            let _ = handle(s, hub, tx, &dist, &token).await;
        });
    }
}

async fn handle(
    s: TcpStream,
    hub: Arc<ClientHub>,
    tx: broadcast::Sender<Value>,
    dist: &Path,
    token: &str,
) -> Result<()> {
    let mut peek = [0u8; 4096];
    let n = s.peek(&mut peek).await?;
    let head = String::from_utf8_lossy(&peek[..n]).to_string();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    if path.starts_with("/ws") {
        let ok = path
            .split_once('?')
            .map(|(_, q)| q.split('&').any(|kv| kv == format!("token={token}")))
            .unwrap_or(false);
        if !ok {
            return http(s, 403, "text/plain", b"bad token".to_vec()).await;
        }
        return ws(s, hub, tx).await;
    }
    // Browser tabs previewing web pages on the host (the token is the credential, see yi_client::preview)
    if let Some(rest) = path.strip_prefix("/fsv/") {
        let mut s = s;
        let mut buf = vec![0u8; 8192];
        let _ = s.read(&mut buf).await?;
        let (code, ctype, body) = match hub.serve_preview(rest).await {
            Ok((b, m)) => (200, m, b),
            Err(e) => (404, "text/plain; charset=utf-8", format!("{e:#}").into_bytes()),
        };
        let head = format!(
            "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
            if code == 200 { "OK" } else { "Not Found" },
            body.len()
        );
        s.write_all(head.as_bytes()).await?;
        s.write_all(&body).await?;
        s.flush().await?;
        return Ok(());
    }
    serve_file(s, dist, &path).await
}

async fn ws(s: TcpStream, hub: Arc<ClientHub>, tx: broadcast::Sender<Value>) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(s).await?;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut events = tx.subscribe();
    let o2 = out_tx.clone();
    let fwd = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(v) => {
                    if o2.send(json!({ "event": v }).to_string()).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(Message::Text(m.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(t) = msg else { continue };
        let Ok(req) = serde_json::from_str::<Value>(&t) else {
            continue;
        };
        let (hub, out) = (hub.clone(), out_tx.clone());
        tokio::spawn(async move {
            let id = req["id"].clone();
            let cmd = req["cmd"].as_str().unwrap_or("").to_string();
            let resp = match hub.call(&cmd, req["args"].clone()).await {
                Ok(r) => json!({ "id": id, "result": r }),
                Err(e) => json!({ "id": id, "error": format!("{e:#}") }),
            };
            let _ = out.send(resp.to_string());
        });
    }
    fwd.abort();
    writer.abort();
    Ok(())
}

fn mime(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "webmanifest" => "application/json",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// The app's Content-Security-Policy, the same one Tauri sends (`app.security.csp` in tauri.conf.json), plus
/// `frame-ancestors`, which only a header can carry: other sites must not frame the page that holds the token.
/// Sent on the app's HTML only; host files under /fsv/ are previewed in a sandboxed frame and keep their own rules.
fn csp() -> &'static str {
    static CSP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CSP.get_or_init(|| {
        let conf: Value = serde_json::from_str(include_str!("../../../app/src-tauri/tauri.conf.json")).expect("tauri.conf.json");
        let map = conf["app"]["security"]["csp"].as_object().expect("app.security.csp is a directive map");
        let mut parts: Vec<String> = map
            .iter()
            .map(|(k, v)| match v {
                Value::Array(a) => format!("{k} {}", a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
                v => format!("{k} {}", v.as_str().unwrap_or_default()),
            })
            .collect();
        parts.push("frame-ancestors 'none'".into());
        parts.join("; ")
    })
}

async fn serve_file(mut s: TcpStream, dist: &Path, path: &str) -> Result<()> {
    // Consume the request headers
    let mut buf = vec![0u8; 8192];
    let _ = s.read(&mut buf).await?;
    let clean = path.split('?').next().unwrap_or("/");
    let rel = clean.trim_start_matches('/');
    if rel.split('/').any(|seg| seg == "..") {
        return http(s, 400, "text/plain", b"bad path".to_vec()).await;
    }
    let mut file = dist.join(if rel.is_empty() { "index.html" } else { rel });
    if !file.is_file() {
        // The frontend is a single-page app; unknown paths all return index.html
        file = dist.join("index.html");
    }
    match tokio::fs::read(&file).await {
        Ok(body) => {
            let ctype = mime(&file);
            let extra = if ctype.starts_with("text/html") { format!("Content-Security-Policy: {}\r\n", csp()) } else { String::new() };
            respond(s, 200, ctype, &extra, body).await
        }
        Err(_) => http(s, 404, "text/plain", b"not found (build the frontend first)".to_vec()).await,
    }
}

async fn http(s: TcpStream, code: u16, ctype: &str, body: Vec<u8>) -> Result<()> {
    respond(s, code, ctype, "", body).await
}

/// `extra`: additional header lines, each ending in \r\n
async fn respond(mut s: TcpStream, code: u16, ctype: &str, extra: &str, body: Vec<u8>) -> Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(&body).await?;
    s.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn csp_comes_from_tauri_conf() {
        let p = super::csp();
        assert!(p.starts_with("default-src 'self'") || p.contains("default-src 'self'"));
        assert!(p.contains("script-src 'self'"));
        assert!(p.ends_with("frame-ancestors 'none'"));
        assert!(!p.contains("unsafe-eval"));
    }
}
