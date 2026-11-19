//! Feishu / Lark plugin. Messages arrive over the long connection (the same WebSocket protocol as the official SDKs:
//! protobuf frames, ping / pong control frames, every data frame acknowledged), so nothing has to listen on a port.
//! Replies, reminders and history go through the Open API with a tenant access token.

use serde::Deserialize;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use yi_connector::{cut, emit, http, host, ContextMessage, ContextPage, Event, Connector, SendResult, Sender};

pub mod frame;
use frame::Frame;

const MAX_TEXT: usize = 4000;

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct Config {
    app_id: String,
    app_secret: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    notify_chat: Option<String>,
}

impl Config {
    fn base(&self) -> &'static str {
        if self.domain.as_deref() == Some("lark") {
            "https://open.larksuite.com"
        } else {
            "https://open.feishu.cn"
        }
    }
}

fn config() -> Result<Config, String> {
    let c: Config = yi_connector::config()?;
    if c.app_id.trim().is_empty() || c.app_secret.trim().is_empty() {
        return Err("App ID or App Secret is missing".into());
    }
    Ok(c)
}

#[derive(Default)]
struct State {
    ws: Option<u32>,
    service: i32,
    ping_every: u64,
    ping_timer: Option<u32>,
    reconnect_timer: Option<u32>,
    reconnect_delay: u64,
    token: Option<(String, u64)>,
    bot: Option<String>,
    /// Multi-part payloads by message id
    parts: HashMap<String, Vec<Option<Vec<u8>>>>,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    ST.with(|s| f(&mut s.borrow_mut()))
}

/// The tenant access token, refreshed five minutes before it expires
fn token(c: &Config) -> Result<String, String> {
    let now = yi_connector::now();
    if let Some((t, until)) = st(|s| s.token.clone()) {
        if now < until {
            return Ok(t);
        }
    }
    let v: Value = http::post(format!("{}/open-apis/auth/v3/tenant_access_token/internal", c.base()))
        .json(json!({"app_id": c.app_id, "app_secret": c.app_secret}))
        .send()?
        .expect_json()?;
    if v["code"].as_i64() != Some(0) {
        return Err(format!("Getting tenant_access_token failed: {}", v["msg"].as_str().unwrap_or("")));
    }
    let t = v["tenant_access_token"].as_str().unwrap_or("").to_string();
    let expire = v["expire"].as_u64().unwrap_or(7200);
    st(|s| s.token = Some((t.clone(), now + expire.saturating_sub(300) * 1000)));
    Ok(t)
}

fn open_api(c: &Config, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let mut r = http::request(method, format!("{}{path}", c.base())).bearer(&token(c)?);
    if let Some(b) = body {
        r = r.json(b);
    }
    let resp = r.send()?;
    let v: Value = resp.json().map_err(|_| format!("HTTP {}: {}", resp.status, cut(&resp.body, 300)))?;
    Ok(v)
}

/// The bot's own open_id: messages that @ it are addressed
fn bot_id(c: &Config) -> Option<String> {
    if let Some(b) = st(|s| s.bot.clone()) {
        return Some(b);
    }
    let v = open_api(c, "GET", "/open-apis/bot/v3/info", None).ok()?;
    let id = v["bot"]["open_id"].as_str()?.to_string();
    st(|s| s.bot = Some(id.clone()));
    Some(id)
}

fn connect() -> Result<(), String> {
    let c = config()?;
    let v: Value = http::post(format!("{}/callback/ws/endpoint", c.base()))
        .header("locale", "zh")
        .json(json!({"AppID": c.app_id, "AppSecret": c.app_secret}))
        .send()?
        .expect_json()?;
    if v["code"].as_i64() != Some(0) {
        return Err(format!("Getting the long connection address failed: {}", v["msg"].as_str().unwrap_or("")));
    }
    let url = v["data"]["URL"].as_str().ok_or("Feishu returned no long connection address")?.to_string();
    let service = query(&url, "service_id").and_then(|s| s.parse().ok()).unwrap_or(0);
    let ping = v["data"]["ClientConfig"]["PingInterval"].as_u64().unwrap_or(120);
    let id = host::ws_open(&url, "{}")?;
    st(|s| {
        s.ws = Some(id);
        s.service = service;
        s.ping_every = ping;
    });
    Ok(())
}

fn query(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn reconnect_later() {
    let delay = st(|s| {
        s.ws = None;
        if let Some(t) = s.ping_timer.take() {
            host::clear_timer(t);
        }
        s.reconnect_delay = (s.reconnect_delay * 2).clamp(5, 120);
        s.reconnect_delay
    });
    let t = host::set_timer(delay * 1000, false);
    st(|s| s.reconnect_timer = Some(t));
}

fn ping() {
    let (ws, service) = st(|s| (s.ws, s.service));
    if let Some(id) = ws {
        let f = Frame { method: frame::CONTROL, service, headers: vec![("type".into(), "ping".into())], ..Default::default() };
        let _ = host::ws_send(id, &f.encode(), true);
    }
}

/// Plain text of a message body (text or rich text), with mention keys replaced by names
pub fn message_text(kind: &str, content: &str, mentions: &[Value]) -> String {
    let v: Value = serde_json::from_str(content).unwrap_or(Value::Null);
    let mut text = match kind {
        "text" => v["text"].as_str().unwrap_or("").to_string(),
        "post" => {
            let body = if v["content"].is_array() { &v } else { v.as_object().and_then(|o| o.values().next()).unwrap_or(&Value::Null) };
            let mut out = body["title"].as_str().map(|t| format!("{t}\n")).unwrap_or_default();
            for line in body["content"].as_array().into_iter().flatten() {
                for el in line.as_array().into_iter().flatten() {
                    match el["tag"].as_str() {
                        Some("text") | Some("a") => out.push_str(el["text"].as_str().unwrap_or("")),
                        Some("at") => out.push_str(&format!("@{}", el["user_name"].as_str().unwrap_or(el["user_id"].as_str().unwrap_or("")))),
                        _ => {}
                    }
                }
                out.push('\n');
            }
            out.trim_end().to_string()
        }
        "image" => "[image]".into(),
        "file" => format!("[file] {}", v["file_name"].as_str().unwrap_or("")),
        other => format!("[{other}]"),
    };
    for m in mentions {
        if let (Some(k), Some(n)) = (m["key"].as_str(), m["name"].as_str()) {
            text = text.replace(k, &format!("@{n}"));
        }
    }
    text
}

/// An `im.message.receive_v1` event as a Bot event (None: not for us)
pub fn to_event(ev: &Value, bot: Option<&str>, listen_all: bool) -> Option<Event> {
    let msg = &ev["message"];
    let chat = msg["chat_id"].as_str()?;
    let id = msg["message_id"].as_str()?;
    let p2p = msg["chat_type"] == "p2p";
    let mentions = msg["mentions"].as_array().cloned().unwrap_or_default();
    let at_bot = bot.is_some_and(|b| mentions.iter().any(|m| m["id"]["open_id"] == b));
    let addressed = p2p || at_bot;
    if !addressed && !listen_all {
        return None;
    }
    let root = msg["root_id"].as_str().filter(|r| !r.is_empty()).unwrap_or(id);
    let thread = if p2p { format!("feishu:{chat}") } else { format!("feishu:{chat}:{root}") };
    let sender = &ev["sender"];
    let sender_id = sender["sender_id"]["open_id"].as_str().unwrap_or("").to_string();
    let text = message_text(msg["message_type"].as_str().unwrap_or("text"), msg["content"].as_str().unwrap_or(""), &mentions);
    let at = msg["create_time"].as_str().and_then(|t| t.parse().ok()).unwrap_or(0);
    Some(
        Event::new(id, thread, cut(&text, MAX_TEXT))
            .sender(sender_id, None, sender["sender_type"] != "user")
            .addressed(addressed)
            .at(at)
            .meta(json!({"chatId": chat, "chatType": msg["chat_type"], "messageId": id})),
    )
}

fn on_data(f: &Frame) {
    let get = |k: &str| f.header(k).map(str::to_string);
    let sum: usize = get("sum").and_then(|s| s.parse().ok()).unwrap_or(1);
    let seq: usize = get("seq").and_then(|s| s.parse().ok()).unwrap_or(0);
    let mid = get("message_id").unwrap_or_default();
    let payload = if sum <= 1 {
        Some(f.payload.clone())
    } else {
        st(|s| {
            let parts = s.parts.entry(mid.clone()).or_insert_with(|| vec![None; sum]);
            if seq < parts.len() {
                parts[seq] = Some(f.payload.clone());
            }
            if parts.iter().all(Option::is_some) {
                let all = s.parts.remove(&mid).unwrap_or_default();
                Some(all.into_iter().flatten().flatten().collect())
            } else {
                None
            }
        })
    };
    // Every data frame is acknowledged, or the server sends it again
    let started = yi_connector::now();
    if let Some(p) = payload {
        if get("type").as_deref() == Some("event") {
            if let Ok(v) = serde_json::from_slice::<Value>(&p) {
                if v["header"]["event_type"] == "im.message.receive_v1" {
                    if let Ok(c) = config() {
                        let bot = bot_id(&c);
                        if let Some(e) = to_event(&v["event"], bot.as_deref(), c.listen.as_deref() == Some("all")) {
                            emit(e);
                        }
                    }
                }
            }
        }
    }
    let mut ack = f.clone();
    ack.headers.push(("biz_rt".into(), (yi_connector::now() - started).to_string()));
    ack.payload = br#"{"code":200}"#.to_vec();
    if let Some(id) = st(|s| s.ws) {
        let _ = host::ws_send(id, &ack.encode(), true);
    }
}

fn send(c: &Config, thread: &str, reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult, String> {
    let content = json!({"text": text}).to_string();
    // uuid dedups on Feishu's side for an hour; at most 50 characters
    let uuid = frame::short_hash(key);
    let v = match reply_to.filter(|r| r.starts_with("om_")) {
        Some(mid) => open_api(c, "POST", &format!("/open-apis/im/v1/messages/{mid}/reply"), Some(json!({"msg_type": "text", "content": content, "uuid": uuid})))?,
        None => {
            let chat = thread.strip_prefix("feishu:").and_then(|t| t.split(':').next()).filter(|c| !c.is_empty()).ok_or_else(|| format!("Can't reply in this conversation: {thread}"))?;
            open_api(
                c,
                "POST",
                "/open-apis/im/v1/messages?receive_id_type=chat_id",
                Some(json!({"receive_id": chat, "msg_type": "text", "content": content, "uuid": uuid})),
            )?
        }
    };
    match v["code"].as_i64() {
        Some(0) => Ok(SendResult::Sent),
        // The message was recalled, the chat is gone, or the bot is no longer in it
        Some(230011) | Some(230002) | Some(232009) | Some(230006) => Ok(SendResult::Gone),
        _ => Err(format!("Feishu returned {}: {}", v["code"], v["msg"].as_str().unwrap_or(""))),
    }
}

struct Feishu;

impl Connector for Feishu {
    fn start() -> Result<(), String> {
        let c = config()?;
        token(&c)?;
        st(|s| s.reconnect_delay = 0);
        connect()
    }

    fn stop() {
        if let Some(id) = st(|s| s.ws.take()) {
            host::ws_close(id);
        }
    }

    fn on_timer(id: u32) {
        let (ping_t, re_t) = st(|s| (s.ping_timer, s.reconnect_timer));
        if Some(id) == ping_t {
            ping();
        } else if Some(id) == re_t {
            st(|s| s.reconnect_timer = None);
            if let Err(e) = connect() {
                host::health("error", &e);
                reconnect_later();
            }
        }
    }

    fn on_ws(id: u32, kind: &str, data: Vec<u8>) {
        if st(|s| s.ws) != Some(id) {
            return;
        }
        match kind {
            "open" => {
                host::health("ok", "");
                let every = st(|s| {
                    s.reconnect_delay = 0;
                    s.ping_every.max(10)
                });
                let t = host::set_timer(every * 1000, true);
                st(|s| s.ping_timer = Some(t));
            }
            "binary" => {
                let Ok(f) = Frame::decode(&data) else { return };
                if f.method == frame::DATA {
                    on_data(&f);
                } else if f.header("type") == Some("pong") {
                    // The pong may carry a new client config
                    if let Ok(v) = serde_json::from_slice::<Value>(&f.payload) {
                        if let Some(p) = v["PingInterval"].as_u64() {
                            st(|s| s.ping_every = p);
                        }
                    }
                }
            }
            "close" | "error" => {
                host::health("error", &format!("Long connection closed: {}", String::from_utf8_lossy(&data)));
                reconnect_later();
            }
            _ => {}
        }
    }

    fn reply(thread: &str, reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult, String> {
        let c = config()?;
        yi_connector::once(key, || send(&c, thread, reply_to, key, text))
    }

    fn notify(text: &str, key: &str) -> Result<SendResult, String> {
        let c = config()?;
        let chat = c.notify_chat.clone().filter(|c| !c.trim().is_empty()).ok_or("No chat is configured for notifications")?;
        yi_connector::once(key, || send(&c, &format!("feishu:{}", chat.trim()), None, key, text))
    }

    fn read_context(thread: &str, cursor: Option<&str>) -> Result<ContextPage, String> {
        let c = config()?;
        let chat = thread.strip_prefix("feishu:").and_then(|t| t.split(':').next()).ok_or("No conversation to reply in")?;
        let mut path = format!("/open-apis/im/v1/messages?container_id_type=chat&container_id={chat}&sort_type=ByCreateTimeDesc&page_size=20");
        if let Some(cur) = cursor {
            path.push_str(&format!("&page_token={cur}"));
        }
        let v = open_api(&c, "GET", &path, None)?;
        if v["code"].as_i64() != Some(0) {
            return Err(format!("Feishu returned {}: {}", v["code"], v["msg"].as_str().unwrap_or("")));
        }
        let messages = v["data"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| ContextMessage {
                id: m["message_id"].as_str().unwrap_or("").into(),
                sender: Sender { id: m["sender"]["id"].as_str().unwrap_or("").into(), name: None, bot: m["sender"]["sender_type"] == "app" },
                at: m["create_time"].as_str().and_then(|t| t.parse().ok()).unwrap_or(0),
                text: cut(&message_text(m["msg_type"].as_str().unwrap_or("text"), m["body"]["content"].as_str().unwrap_or(""), m["mentions"].as_array().map(Vec::as_slice).unwrap_or(&[])), MAX_TEXT),
                url: None,
            })
            .collect();
        let next = (v["data"]["has_more"] == true).then(|| v["data"]["page_token"].as_str().unwrap_or("").to_string()).filter(|t| !t.is_empty());
        Ok(ContextPage { messages, next })
    }
}

yi_connector::export!(Feishu);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_become_events() {
        let ev = json!({
            "sender": {"sender_id": {"open_id": "ou_ann"}, "sender_type": "user"},
            "message": {
                "message_id": "om_2", "root_id": "om_1", "chat_id": "oc_x", "chat_type": "group", "message_type": "text",
                "create_time": "1700000000000",
                "content": "{\"text\":\"@_user_1 fix the build\"}",
                "mentions": [{"key": "@_user_1", "id": {"open_id": "ou_bot"}, "name": "Yi"}],
            }
        });
        let e = to_event(&ev, Some("ou_bot"), false).unwrap().0;
        assert!(e.addressed);
        assert_eq!(e.thread, "feishu:oc_x:om_1");
        assert_eq!(e.text, "@Yi fix the build");
        assert_eq!(e.key, "om_2");
        assert_eq!(e.at, 1700000000000);
        // Not @ the bot in a group: dropped unless listening to everything
        assert!(to_event(&ev, Some("ou_other"), false).is_none());
        assert!(!to_event(&ev, Some("ou_other"), true).unwrap().0.addressed);
        // Private chats are always addressed
        let mut p = ev.clone();
        p["message"]["chat_type"] = json!("p2p");
        let e = to_event(&p, None, false).unwrap().0;
        assert!(e.addressed);
        assert_eq!(e.thread, "feishu:oc_x");
    }

    #[test]
    fn rich_text() {
        let c = json!({"zh_cn": {"title": "T", "content": [[{"tag": "text", "text": "hi "}, {"tag": "at", "user_name": "Bob"}]]}}).to_string();
        assert_eq!(message_text("post", &c, &[]), "T\nhi @Bob");
        assert_eq!(query("wss://x/ws?device_id=1&service_id=33", "service_id").as_deref(), Some("33"));
    }
}
