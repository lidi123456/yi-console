//! Yi Console Bots, run by the desktop app: always-on orchestrating sessions ("Bots") that take events from trigger
//! plugins and spawn task sessions on any connected host. See docs/internals/bots.md.
//!
//! Plugged into `ClientHub` as an `Extension`: frontend commands `bots.*` / `tasks.*` / `plugins.*`, the Bot tools
//! relayed by hosts (`clientMcp.call`), and host notifications to follow the Bot and task sessions.

pub mod config;
pub mod engine;
pub mod plugins;
pub mod prompt;
pub mod store;
pub mod tools;

use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use yi_client::ext::{BoxFut, Extension, Identity};
use yi_client::ClientHub;

pub struct Bots {
    pub engine: Arc<engine::Engine>,
}

/// This client's id, kept in the app data directory so hosts recognize it across restarts
fn client_id(data_dir: &Path) -> String {
    let path = data_dir.join("client-id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::write(&path, &id);
    id
}

/// Open the Bots database, plug the engine into the hub and start the enabled Bots. `bundled_plugins`: the WASM
/// plugins shipped with the app (one folder per plugin), if any. Call inside the tokio runtime
pub fn start(hub: &Arc<ClientHub>, data_dir: &Path, device_name: &str, bundled_plugins: Option<&Path>) -> Result<Arc<Bots>> {
    let store = store::Store::open(&data_dir.join("bots.db"))?;
    let identity = Identity { client_id: client_id(data_dir), kind: "desktop".into(), name: device_name.into() };
    let engine = engine::Engine::new(hub, store, identity, Some(&data_dir.join("plugins")), bundled_plugins);
    let bots = Arc::new(Bots { engine: engine.clone() });
    hub.set_extension(bots.clone());
    engine.start();
    Ok(bots)
}

impl Bots {
    /// Whether any Bot is enabled (the app then keeps running in the tray when its window closes)
    pub fn any_enabled(&self) -> bool {
        self.engine.store.bots().map(|b| b.iter().any(|b| b.config.enabled)).unwrap_or(false)
    }
}

impl Extension for Bots {
    fn identity(&self) -> Identity {
        self.engine.identity()
    }

    fn handles(&self, cmd: &str) -> bool {
        cmd.starts_with("bots.") || cmd.starts_with("tasks.") || cmd.starts_with("plugins.")
    }

    fn call<'a>(&'a self, cmd: &'a str, args: Value) -> BoxFut<'a, Result<Value>> {
        Box::pin(self.engine.command(cmd, args))
    }

    fn client_mcp<'a>(&'a self, host: &'a str, params: Value) -> BoxFut<'a, Result<Value>> {
        Box::pin(self.engine.client_mcp(host, params))
    }

    fn observe(&self, host: &str, method: &str, params: &Value) {
        self.engine.observe(host, method, params);
    }

    fn host_status(&self, host: &str, status: &Value) {
        self.engine.host_status(host, status);
    }
}
