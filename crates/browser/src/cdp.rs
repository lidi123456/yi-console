//! A minimal CDP client over one browser-level WebSocket, using flattened target sessions (`sessionId` on every
//! message), as agent-browser's `cdp/client.rs` and Puppeteer do.

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// How long one CDP command may take. Navigation waits are done by the caller on events, not here
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

/// Waiting commands: id -> (session it went to, reply)
type Pending = Arc<Mutex<HashMap<u64, (Option<String>, oneshot::Sender<Result<Value, String>>)>>>;
/// Sessions that went away recently (bounded), so commands to them fail at once instead of timing out
const DETACHED_MEMORY: usize = 256;

#[derive(Clone)]
pub struct Cdp {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicU64>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    detached: Arc<Mutex<std::collections::VecDeque<String>>>,
}

/// JSON text with unpaired UTF-16 surrogate escapes (`\ud800` without its pair) replaced by U+FFFD. Pages can put
/// such strings in the DOM, titles or console messages; serde_json rejects them, which would drop the whole message
/// (and leave its command waiting until the timeout), as agent-browser's `sanitize_lone_surrogates` notes
pub fn sanitize_surrogates(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("\\u") && !text.contains("\\U") {
        return std::borrow::Cow::Borrowed(text);
    }
    let b = text.as_bytes();
    let hex = |i: usize| -> Option<u16> {
        let s = text.get(i..i + 4)?;
        u16::from_str_radix(s, 16).ok()
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            // An escaped backslash: skip both
            if b.get(i + 1) == Some(&b'\\') {
                i += 2;
                continue;
            }
            if matches!(b.get(i + 1), Some(b'u' | b'U')) {
                if let Some(u) = hex(i + 2) {
                    let lone = match u {
                        0xD800..=0xDBFF => !(b.get(i + 6) == Some(&b'\\') && matches!(b.get(i + 7), Some(b'u' | b'U')) && hex(i + 8).is_some_and(|l| (0xDC00..=0xDFFF).contains(&l))),
                        0xDC00..=0xDFFF => true,
                        _ => false,
                    };
                    if lone {
                        out.push_str(&text[last..i]);
                        out.push_str("\\ufffd");
                        i += 6;
                        last = i;
                        continue;
                    }
                    // A valid pair: keep both escapes
                    i += if (0xD800..=0xDBFF).contains(&u) { 12 } else { 6 };
                    continue;
                }
            }
        }
        i += 1;
    }
    if last == 0 {
        return std::borrow::Cow::Borrowed(text);
    }
    out.push_str(&text[last..]);
    std::borrow::Cow::Owned(out)
}

/// The `"id":N` of a message that didn't parse, so its command can fail at once
fn leading_id(text: &str) -> Option<u64> {
    let at = text.find("\"id\":")? + 5;
    let digits: String = text[at..].trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

impl Cdp {
    /// Connect to `ws://127.0.0.1:<port><path>`. Every event goes to `on_event`, called on the reader task in
    /// arrival order; it must not block.
    pub async fn connect(url: &str, on_event: impl Fn(Event) + Send + 'static) -> Result<Cdp> {
        // Full-page screenshots can be large; Chrome may send them in one frame
        let config = WebSocketConfig::default().max_message_size(Some(256 << 20)).max_frame_size(Some(256 << 20));
        let (ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), true).await?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::default();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let (p, c) = (pending.clone(), closed.clone());
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let text = match msg {
                    Ok(Message::Text(t)) => t.to_string(),
                    Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let v = match serde_json::from_str::<Value>(&sanitize_surrogates(&text)) {
                    Ok(v) => v,
                    Err(e) => {
                        if let Some((_, reply)) = leading_id(&text).and_then(|id| p.lock().unwrap().remove(&id)) {
                            let _ = reply.send(Err(format!("浏览器返回的数据无法解析：{e}")));
                        }
                        continue;
                    }
                };
                if let Some(id) = v["id"].as_u64() {
                    if let Some((_, reply)) = p.lock().unwrap().remove(&id) {
                        let r = match v.get("error") {
                            Some(e) => Err(e["message"].as_str().unwrap_or("CDP error").to_string()),
                            None => Ok(v["result"].clone()),
                        };
                        let _ = reply.send(r);
                    }
                } else if let Some(method) = v["method"].as_str() {
                    on_event(Event {
                        method: method.to_string(),
                        params: v["params"].clone(),
                        session: v["sessionId"].as_str().map(String::from),
                    });
                }
            }
            c.store(true, Ordering::SeqCst);
            // Fail everything still waiting
            p.lock().unwrap().clear();
            on_event(Event { method: "Yi.disconnected".into(), params: Value::Null, session: None });
        });
        Ok(Cdp { tx, pending, next: Arc::new(AtomicU64::new(1)), closed, detached: Arc::default() })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send a command without waiting longer than `COMMAND_TIMEOUT`
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_within(session, method, params, COMMAND_TIMEOUT).await
    }

    pub async fn call_within(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let (id, rx) = self.send_id(session, method, params)?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{method}: {e}"),
            Ok(Err(_)) => bail!("浏览器连接已断开"),
            Err(_) => {
                // Nobody waits for it any more
                self.pending.lock().unwrap().remove(&id);
                bail!("浏览器响应超时（{method}）")
            }
        }
    }

    /// A session went away: its waiting commands fail now, and later ones fail when sent
    pub fn session_detached(&self, session: &str) {
        {
            let mut d = self.detached.lock().unwrap();
            d.push_back(session.to_string());
            while d.len() > DETACHED_MEMORY {
                d.pop_front();
            }
        }
        let mut p = self.pending.lock().unwrap();
        let ids: Vec<u64> = p.iter().filter(|(_, (s, _))| s.as_deref() == Some(session)).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some((_, reply)) = p.remove(&id) {
                let _ = reply.send(Err("页面或框架已关闭".into()));
            }
        }
    }

    /// Commands still waiting for an answer (tests)
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Send a command and return the receiver for its answer, for callers that race it against events (a dialog
    /// opened by a click blocks `Input.dispatchMouseEvent` until the dialog is handled)
    pub fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<oneshot::Receiver<Result<Value, String>>> {
        self.send_id(session, method, params).map(|(_, rx)| rx)
    }

    fn send_id(&self, session: Option<&str>, method: &str, params: Value) -> Result<(u64, oneshot::Receiver<Result<Value, String>>)> {
        if self.is_closed() {
            bail!("浏览器连接已断开");
        }
        if let Some(s) = session {
            if self.detached.lock().unwrap().iter().any(|d| d == s) {
                bail!("{method}: 页面或框架已关闭");
            }
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, (session.map(String::from), tx));
        if self.tx.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("浏览器连接已断开"));
        }
        Ok((id, rx))
    }

    /// Fire and forget (dialog auto-accept from the event handler)
    pub fn notify(&self, session: Option<&str>, method: &str, params: Value) {
        if let Ok(rx) = self.send(session, method, params) {
            drop(rx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_surrogates_are_replaced() {
        let ok = r#"{"a":"\ud83d\ude00 x"}"#;
        assert_eq!(sanitize_surrogates(ok), ok);
        let lone = r#"{"id":7,"result":{"t":"a\ud800b","u":"\udc00","v":"\\ud800"}}"#;
        let fixed = sanitize_surrogates(lone);
        let v: Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["result"]["t"], "a\u{FFFD}b");
        assert_eq!(v["result"]["u"], "\u{FFFD}");
        // An escaped backslash before "ud800" is plain text
        assert_eq!(v["result"]["v"], "\\ud800");
        assert!(serde_json::from_str::<Value>(lone).is_err());
        assert_eq!(leading_id(lone), Some(7));
        assert_eq!(leading_id(r#"{"method":"x"}"#), None);
    }
}
