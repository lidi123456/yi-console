//! Connector extension points. A connector provides any of: triggers (events), replies into the source conversation,
//! conversation context, notify channels, tools and prompt notes. Bots reference connector *instances* (a connector plus a
//! configuration).
//!
//! The traits mirror the `yi:connector` WIT world in the proposal: `ConnectorHost` is what the app gives an instance
//! (`host` interface), `ConnectorInstance` is what a connector implements (`connector` interface), and `ConnectorRuntime` loads
//! connectors of one kind. Only the built-in runtime exists now; WASM (wasmtime) and later a shared Node host plug in
//! as further runtimes without changing the engine.

pub mod builtin;
pub mod cron;
pub mod package;
pub mod wasm;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// The `yi:connector` interface version this app implements
pub const API_VERSION: &str = "0.1";
/// Interface versions installed connectors may declare (older ones stay supported)
pub const SUPPORTED_APIS: &[&str] = &["0.1"];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Text {
    #[serde(default)]
    pub zh: String,
    #[serde(default)]
    pub en: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Permissions {
    /// Outbound hosts the connector may reach
    #[serde(default)]
    pub net: Vec<String>,
    /// Commands it may run on the desktop machine
    #[serde(default)]
    pub exec: Vec<String>,
}

/// `connector.json`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub api: String,
    /// builtin / wasm (node reserved)
    pub runtime: String,
    /// The component file inside the package (default connector.wasm)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    pub name: Text,
    #[serde(default)]
    pub description: Text,
    /// trigger / reply / context / notify / tools
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub permissions: Permissions,
    /// JSON Schema of an instance's configuration; `x-secret` marks secrets
    #[serde(default)]
    pub config: Value,
    /// How the Bot should treat this source's events (joins the generated prompt layer)
    #[serde(default)]
    pub prompt: Option<String>,
    /// Built-in connectors with a fixed single instance (the app notification) can't be instantiated by hand
    #[serde(default)]
    pub singleton: bool,
    /// Shipped with the app (loaded from its resources): updated with the app, can't be uninstalled or replaced.
    /// Set by the loader, never read from connector.json
    #[serde(default, skip_deserializing)]
    pub bundled: bool,
}

impl Manifest {
    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Sender {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// A bot account: never starts a task by itself
    #[serde(default)]
    pub bot: bool,
}

/// One input from a connector instance
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TriggerEvent {
    /// Source-side id, for deduplication
    pub key: String,
    /// The conversation it belongs to (an issue, a chat thread); follow-ups go to the same task
    pub thread: String,
    pub sender: Sender,
    /// Explicitly addressed to the Bot: @, assignment, command, schedule
    pub addressed: bool,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    /// ms
    pub at: i64,
    #[serde(default)]
    pub meta: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SendResult {
    Sent,
    /// The conversation no longer accepts replies
    Gone,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextMessage {
    pub id: String,
    pub sender: Sender,
    pub at: i64,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextPage {
    pub messages: Vec<ContextMessage>,
    #[serde(default)]
    pub next: Option<String>,
}

/// What the app gives a connector instance
pub trait ConnectorHost: Send + Sync {
    fn instance_id(&self) -> &str;
    /// The instance configuration, secrets included
    fn config(&self) -> Value;
    fn emit(&self, event: TriggerEvent);
    fn kv_get(&self, key: &str) -> Option<String>;
    fn kv_set(&self, key: &str, value: Option<&str>);
    fn log(&self, level: &str, message: &str);
    /// ok / starting / error
    fn health(&self, state: &str, detail: &str);
}

/// What a connector implements; every capability is optional
pub trait ConnectorInstance: Send + Sync {
    fn stop(&self);
    /// `key` is an idempotency key: a retry must not send twice
    fn reply(&self, _thread: &str, _reply_to: Option<&str>, _key: &str, _text: &str) -> Result<SendResult> {
        bail!("这个连接器不能回复")
    }
    fn notify(&self, _text: &str, _key: &str) -> Result<SendResult> {
        bail!("这个连接器不能发提醒")
    }
    fn read_context(&self, _thread: &str, _cursor: Option<&str>) -> Result<ContextPage> {
        Ok(ContextPage::default())
    }
    fn tools(&self) -> Vec<Value> {
        vec![]
    }
    fn call_tool(&self, _name: &str, _input: &Value) -> Result<String> {
        bail!("这个连接器没有工具")
    }
    /// Built-in connectors only: notifications from connected hosts (session-events)
    fn on_host_event(&self, _host: &str, _method: &str, _params: &Value) {}
}

/// Loads and starts connectors of one kind
pub trait ConnectorRuntime: Send + Sync {
    /// builtin / wasm / node
    fn kind(&self) -> &'static str;
    fn manifests(&self) -> Vec<Manifest>;
    fn start(&self, connector_id: &str, host: Arc<dyn ConnectorHost>) -> Result<Arc<dyn ConnectorInstance>>;
}

/// Replace secret values (`x-secret` in the schema) before a configuration leaves the engine
pub const MASK: &str = "********";

pub fn redact(schema: &Value, config: &Value) -> Value {
    let mut out = config.clone();
    if let (Some(props), Some(obj)) = (schema["properties"].as_object(), out.as_object_mut()) {
        for (k, p) in props {
            if p["x-secret"] == true && obj.get(k).is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
                obj.insert(k.clone(), Value::String(MASK.into()));
            }
        }
    }
    out
}

/// A save that comes back with `********` keeps the stored value
pub fn keep_secrets(schema: &Value, config: &mut Value, old: &Value) -> Result<()> {
    if let (Some(props), Some(obj)) = (schema["properties"].as_object(), config.as_object_mut()) {
        for (k, p) in props {
            if p["x-secret"] == true && obj.get(k).and_then(|v| v.as_str()) == Some(MASK) {
                match old.get(k) {
                    Some(v) => {
                        obj.insert(k.clone(), v.clone());
                    }
                    None => bail!("密钥占位符没有原值，请重新填写：{k}"),
                }
            }
        }
    }
    Ok(())
}

/// Minimal check of a configuration against the manifest schema: required fields and simple types
/// Check a configuration against the plugin's schema; `required`: also insist on the required settings
pub fn validate_config(schema: &Value, config: &Value, required: bool) -> Result<()> {
    let Some(obj) = config.as_object() else { bail!("连接器配置必须是对象") };
    for r in schema["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).filter(|_| required) {
        if obj.get(r).map_or(true, |v| v.is_null() || v.as_str() == Some("")) {
            bail!("缺少配置项：{r}");
        }
    }
    if let Some(props) = schema["properties"].as_object() {
        for (k, v) in obj {
            let Some(p) = props.get(k) else { continue };
            let ok = match p["type"].as_str() {
                Some("string") => v.is_string(),
                Some("array") => v.is_array(),
                Some("boolean") => v.is_boolean(),
                Some("integer") | Some("number") => v.is_number(),
                _ => true,
            };
            if !ok && !v.is_null() {
                bail!("配置项类型不对：{k}");
            }
            if let Some(values) = p["enum"].as_array() {
                if !v.is_null() && !values.contains(v) {
                    bail!("配置项取值不对：{k}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_masked_and_kept() {
        let schema = json!({"properties": {"token": {"type": "string", "x-secret": true}, "chat": {"type": "string"}}, "required": ["token"]});
        let stored = json!({"token": "abc", "chat": "c1"});
        let shown = redact(&schema, &stored);
        assert_eq!(shown["token"], MASK);
        assert_eq!(shown["chat"], "c1");
        let mut back = shown.clone();
        keep_secrets(&schema, &mut back, &stored).unwrap();
        assert_eq!(back, stored);
        assert!(keep_secrets(&schema, &mut shown.clone(), &json!({})).is_err());
        validate_config(&schema, &stored, true).unwrap();
        assert!(validate_config(&schema, &json!({"chat": "x"}), true).is_err());
        validate_config(&schema, &json!({"chat": "x"}), false).unwrap();
        assert!(validate_config(&schema, &json!({"token": 1}), true).is_err());
    }
}
