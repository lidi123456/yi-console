//! SDK for Yi Console connectors (WASM components built for `wasm32-wasip2`).
//!
//! A connector implements [`Connector`] (every method has a default, so only the declared capabilities need code) and
//! exports it with `yi_connector::export!(MyConnector);`. The host interface is in [`host`]; the helpers below wrap the
//! JSON parts of it.
//!
//! ```ignore
//! struct Echo;
//! impl yi_connector::Plugin for Echo {
//!     fn start() -> Result<(), String> {
//!         yi_connector::host::set_timer(60_000, true);
//!         Ok(())
//!     }
//!     fn on_timer(_id: u32) {
//!         yi_connector::emit(yi_connector::Event::new("k1", "echo:1", "hello"));
//!     }
//! }
//! yi_connector::export!(Echo);
//! ```
//!
//! On non-wasm targets the host functions are stubs that panic, so plain logic can still be unit tested natively.

wit_bindgen::generate!({
    path: "../wit",
    world: "yi-connector",
    pub_export_macro: true,
    default_bindings_module: "yi_connector",
    export_macro_name: "export_bindings",
});

pub use exports::yi::connector::connector::Guest;
pub use yi::connector::host;
pub use yi::connector::types::{ContextMessage, ContextPage, SendResult, Sender, ToolDef, TriggerEvent};

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What a connector implements. Unimplemented capabilities answer with an error
pub trait Connector {
    fn start() -> Result<(), String> {
        Ok(())
    }
    fn stop() {}
    fn on_timer(_id: u32) {}
    /// kind: open / text / binary / close / error
    fn on_ws(_id: u32, _kind: &str, _data: Vec<u8>) {}
    fn reply(_thread: &str, _reply_to: Option<&str>, _key: &str, _text: &str) -> Result<SendResult, String> {
        Err("this connector can't reply".into())
    }
    fn notify(_text: &str, _key: &str) -> Result<SendResult, String> {
        Err("this connector can't notify".into())
    }
    fn read_context(_thread: &str, _cursor: Option<&str>) -> Result<ContextPage, String> {
        Ok(ContextPage { messages: vec![], next: None })
    }
    fn tools() -> Vec<ToolDef> {
        vec![]
    }
    fn call_tool(name: &str, _input: &str) -> Result<String, String> {
        Err(format!("unknown tool {name}"))
    }
}

/// Bridges a [`Connector`] to the generated `Guest` trait; used by [`export!`]
#[doc(hidden)]
pub struct Adapter<P>(std::marker::PhantomData<P>);

impl<P: Connector> Guest for Adapter<P> {
    fn start() -> Result<(), String> {
        P::start()
    }
    fn stop() {
        P::stop()
    }
    fn on_timer(id: u32) {
        P::on_timer(id)
    }
    fn on_ws(id: u32, kind: String, data: Vec<u8>) {
        P::on_ws(id, &kind, data)
    }
    fn reply(thread: String, reply_to: Option<String>, key: String, text: String) -> Result<SendResult, String> {
        P::reply(&thread, reply_to.as_deref(), &key, &text)
    }
    fn notify(text: String, key: String) -> Result<SendResult, String> {
        P::notify(&text, &key)
    }
    fn read_context(thread: String, cursor: Option<String>) -> Result<ContextPage, String> {
        P::read_context(&thread, cursor.as_deref())
    }
    fn tools() -> Vec<ToolDef> {
        P::tools()
    }
    fn call_tool(name: String, input: String) -> Result<String, String> {
        P::call_tool(&name, &input)
    }
}

/// Export a [`Connector`] as the component's `yi:connector/connector` interface
#[macro_export]
macro_rules! export {
    ($t:ty) => {
        type __YiConnectorAdapter = $crate::Adapter<$t>;
        $crate::export_bindings!(__YiConnectorAdapter with_types_in $crate);
    };
}

/// The instance configuration
pub fn config<T: DeserializeOwned>() -> Result<T, String> {
    serde_json::from_str(&host::config()).map_err(|e| format!("bad config: {e}"))
}

pub fn now() -> u64 {
    host::now()
}

pub fn log(message: &str) {
    host::log("info", message);
}

pub fn warn(message: &str) {
    host::log("warn", message);
}

pub fn kv_get(key: &str) -> Option<String> {
    host::kv_get(key)
}

pub fn kv_set(key: &str, value: &str) {
    host::kv_set(key, Some(value));
}

pub fn kv_del(key: &str) {
    host::kv_set(key, None);
}

/// Idempotent sends: run `send` once per key, remembering keys that went out
pub fn once(key: &str, send: impl FnOnce() -> Result<SendResult, String>) -> Result<SendResult, String> {
    let k = format!("sent:{key}");
    if kv_get(&k).is_some() {
        return Ok(SendResult::Sent);
    }
    let r = send()?;
    if r == SendResult::Sent {
        kv_set(&k, &now().to_string());
    }
    Ok(r)
}

/// A trigger event under construction
pub struct Event(pub TriggerEvent);

impl Event {
    pub fn new(key: impl Into<String>, thread: impl Into<String>, text: impl Into<String>) -> Event {
        Event(TriggerEvent {
            key: key.into(),
            thread: thread.into(),
            sender: Sender { id: String::new(), name: None, bot: false },
            addressed: false,
            text: text.into(),
            url: None,
            at: 0,
            meta: "null".into(),
        })
    }
    pub fn sender(mut self, id: impl Into<String>, name: Option<String>, bot: bool) -> Event {
        self.0.sender = Sender { id: id.into(), name, bot };
        self
    }
    pub fn addressed(mut self, yes: bool) -> Event {
        self.0.addressed = yes;
        self
    }
    pub fn url(mut self, url: Option<String>) -> Event {
        self.0.url = url;
        self
    }
    pub fn at(mut self, ms: u64) -> Event {
        self.0.at = ms;
        self
    }
    pub fn meta(mut self, meta: Value) -> Event {
        self.0.meta = meta.to_string();
        self
    }
}

pub fn emit(e: Event) {
    host::emit(&e.0);
}

/// A tool definition; `read_only` tools may run while the Bot only observes
pub fn tool(name: &str, description: &str, input_schema: Value, read_only: bool) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), input_schema: input_schema.to_string(), read_only }
}

pub mod http {
    //! HTTP through the host (`http-fetch`): only hosts in the manifest's `permissions.net` are reachable
    use super::*;

    pub struct Request {
        method: String,
        url: String,
        headers: BTreeMap<String, String>,
        body: Option<String>,
        json: Option<Value>,
    }

    pub fn request(method: &str, url: impl Into<String>) -> Request {
        Request { method: method.into(), url: url.into(), headers: BTreeMap::new(), body: None, json: None }
    }
    pub fn get(url: impl Into<String>) -> Request {
        request("GET", url)
    }
    pub fn post(url: impl Into<String>) -> Request {
        request("POST", url)
    }

    impl Request {
        pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
            self.headers.insert(name.into(), value.into());
            self
        }
        pub fn bearer(self, token: &str) -> Request {
            self.header("authorization", format!("Bearer {token}"))
        }
        pub fn body(mut self, body: impl Into<String>) -> Request {
            self.body = Some(body.into());
            self
        }
        pub fn json(mut self, v: Value) -> Request {
            self.json = Some(v);
            self
        }
        pub fn send(self) -> Result<Response, String> {
            let mut r = json!({"method": self.method, "url": self.url, "headers": self.headers});
            if let Some(b) = self.body {
                r["body"] = json!(b);
            }
            if let Some(j) = self.json {
                r["json"] = j;
            }
            let out = host::http_fetch(&r.to_string())?;
            let v: Value = serde_json::from_str(&out).map_err(|e| format!("bad response: {e}"))?;
            Ok(Response {
                status: v["status"].as_u64().unwrap_or(0) as u16,
                headers: v["headers"]
                    .as_object()
                    .map(|h| h.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap_or("").to_string())).collect())
                    .unwrap_or_default(),
                body: v["body"].as_str().unwrap_or("").to_string(),
            })
        }
    }

    pub struct Response {
        pub status: u16,
        /// Lower-case names
        pub headers: BTreeMap<String, String>,
        pub body: String,
    }

    impl Response {
        pub fn ok(&self) -> bool {
            (200..300).contains(&self.status)
        }
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
        }
        pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
            serde_json::from_str(&self.body).map_err(|e| format!("bad JSON from the server: {e}"))
        }
        /// The body as JSON, or an error carrying the status and the start of the body
        pub fn expect_json<T: DeserializeOwned>(self) -> Result<T, String> {
            if !self.ok() {
                return Err(format!("HTTP {}: {}", self.status, cut(&self.body, 300)));
            }
            self.json()
        }
    }
}

/// At most `max` characters, with an ellipsis when cut
pub fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}
