//! Local services plugged into `ClientHub` by the desktop app (the Bot engine in `crates/bots`).
//!
//! An extension answers its own frontend commands (`bots.*`, `tasks.*`, `connectors.*`), receives the client MCP calls
//! hosts relay to this client (`clientMcp.call`), and sees every host notification and status change. ClientHub
//! introduces itself to every host with the extension's identity (`client.hello`), so hosts know where to send calls.

use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Who this client is, sent to every host with `client.hello`
#[derive(Debug, Clone)]
pub struct Identity {
    pub client_id: String,
    /// desktop / mobile / web
    pub kind: String,
    pub name: String,
}

pub trait Extension: Send + Sync + 'static {
    fn identity(&self) -> Identity;
    /// Whether `cmd` (a frontend command) is answered by this extension
    fn handles(&self, cmd: &str) -> bool;
    fn call<'a>(&'a self, cmd: &'a str, args: Value) -> BoxFut<'a, Result<Value>>;
    /// A tool call relayed by host `host` (`clientMcp.call` params: sessionId, server, tool, arguments). The result
    /// is an MCP CallToolResult or any value, which reaches the model as text
    fn client_mcp<'a>(&'a self, host: &'a str, params: Value) -> BoxFut<'a, Result<Value>>;
    /// A notification from host `host`, before it is forwarded to the frontend
    fn observe(&self, host: &str, method: &str, params: &Value);
    /// A host's connection status changed (the same value the frontend gets as `host.status`)
    fn host_status(&self, host: &str, status: &Value);
}
