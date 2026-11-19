# Writing Bot connectors

中文：[编写 Bot 连接器](connectors.md)

Bot connectors bring outside events to Bots (a chat message, a GitHub mention), send the Bot's replies back, and can give the Bot tools. A connector is a WebAssembly component that runs in a sandbox inside the desktop app; it is installed from a `.yiconnector` package on the **Connectors** page. The GitHub and Feishu connectors ship with the desktop app and update with it; they need no install.

The first-party connectors in [`connectors/`](../connectors) are complete examples: `github` (polling a REST API), `feishu` (a WebSocket long connection) and `testkit` (the end-to-end test connector).

## What a connector can do

A connector declares capabilities in its manifest and implements the matching methods:

| Capability | Methods | Meaning |
| --- | --- | --- |
| `trigger` | `start`, `on_timer`, `on_ws` → `emit` | Brings events to Bots |
| `reply` | `reply` | Answers in the conversation an event came from |
| `context` | `read_context` | Lets the Bot read earlier messages of a conversation |
| `notify` | `notify` | Can be picked as a Bot's notification channel |
| `tools` | `tools`, `call_tool` | Tools the Bot can call (`connector_tools` / `connector_call`) |

A connector can have several configurations, for example several schedules, several Feishu bots or several GitHub accounts, each running on its own (the developer sections call each one an *instance*). Bots pick which ones to use in their settings.

## Getting started

Requirements: Rust with the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`).

Add a crate to the `connectors/` workspace, or make your own crate depending on the SDK:

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
yi-connector = { path = "../sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

```rust
use yi_connector::{emit, host, Event, Connector, SendResult};

struct Hello;

impl Connector for Hello {
    fn start() -> Result<(), String> {
        host::set_timer(60_000, true); // every minute
        Ok(())
    }

    fn on_timer(_id: u32) {
        emit(Event::new("tick-1", "hello:1", "Good morning").addressed(true));
    }

    fn reply(thread: &str, _reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult, String> {
        yi_connector::once(key, || {
            yi_connector::log(&format!("reply to {thread}: {text}"));
            Ok(SendResult::Sent)
        })
    }
}

yi_connector::export!(Hello);
```

Build and pack:

```bash
cd connectors
cargo build --release --target wasm32-wasip2 -p my-connector
cargo run -p yi-connector-pack -- my-connector      # writes dist/<id>/ and dist/<id>-<version>.yiconnector
```

Then **Connectors → Install connector** in the desktop app: choose the `.yiconnector`, or type the path of the `dist/<id>` directory while developing.

## Let the AI write it

Tell a Bot in its session which service it needs to reach. The Bot doesn't write code itself; it starts a task for it. The task calls `connector_kit` to get a cargo workspace (the SDK, the WIT, an example connector and this guide) on its host, writes the connector there and builds it for `wasm32-wasip2`. When the task is done, the Bot checks the network and command permissions the connector asks for and submits the package or connector folder with `propose_connector`. The app then opens the permission review on the connectors page and you decide whether to install it; neither the Bot nor the task can install anything itself.

## connector.json

```json
{
  "id": "hello",
  "version": "0.1.0",
  "api": "0.1",
  "runtime": "wasm",
  "main": "connector.wasm",
  "name": { "zh": "示例", "en": "Hello" },
  "description": { "zh": "…", "en": "…" },
  "capabilities": ["trigger", "reply"],
  "permissions": { "net": ["api.example.com", "*.example.net"], "exec": [] },
  "config": {
    "type": "object",
    "required": ["token"],
    "properties": {
      "token": { "type": "string", "title": "令牌", "x-secret": true, "x-en": { "title": "Token" } }
    }
  }
}
```

- `id`: lower-case letters, digits and `-`, at most 40 characters. Built-in ids (`cron`, `session-events`, `app-notify`, `app`, `builtin`) are reserved.
- `api`: the interface version, currently `0.1`.
- `permissions.net`: host names the connector may reach over HTTP(S) and WebSocket. `*.x` matches subdomains of `x` (not `x` itself). Nothing else is reachable.
- `permissions.exec`: commands the connector may run on the desktop machine. Commands run outside the sandbox, and the install review highlights them; avoid them unless the connector truly needs a local program.
- `config`: a JSON schema for an instance's settings, rendered as a form. `title` / `description` are Chinese, `x-en` holds the English ones. `x-secret: true` fields are never shown again after saving. Enums can have labels: `"x-labels": {"a": "甲"}` with `"x-en": {"x-labels": {"a": "A"}}`. A string array is edited one item per line.
- Error and health messages (`Err(..)`, `host::health`) are in English: they reach the Bot's AI as tool results and are shown in the app as they are, without translation.
- `prompt.md` next to the manifest (optional): a short English note that tells the Bot how to read this source's events (thread format, when to reply). It goes into the Bot's system prompt.

On install the app shows the permissions (and on updates, which ones are new); nothing is written before the owner confirms.

## The host interface

The full interface is [`connectors/wit/yi-connector.wit`](../connectors/wit/yi-connector.wit). The SDK wraps it:

| SDK | |
| --- | --- |
| `yi_connector::config::<T>()` | The instance configuration (secrets included) |
| `emit(Event::new(key, thread, text)...)` | Send an event. `key` is unique per event (repeats are ignored), `thread` groups a conversation; `.sender(id, name, bot)`, `.addressed(true)` when it is addressed to the Bot (an @, an assignment), `.url`, `.at`, `.meta` |
| `http::get(url).bearer(t).send()?` | HTTP through the host; `.json(v)`, `.header`, `.body`; `Response { status, headers, body }`, `.expect_json()` |
| `host::ws_open(url, headers_json)` | Opens a WebSocket; events arrive in `on_ws(id, kind, data)` with kind `open` / `text` / `binary` / `close` / `error` |
| `host::set_timer(ms, repeat)` / `clear_timer` | Timers; they fire `on_timer(id)` |
| `kv_get` / `kv_set` / `kv_del` | A small persistent key-value store per instance (cursors, caches) |
| `once(key, send)` | Run a send at most once per key |
| `host::health(state, detail)` | Show `ok` / `error` with a reason on the Connectors page |
| `log` / `warn` | Logging |
| `host::exec(cmd, args, stdin)` | Run a command listed in `permissions.exec` |

### Rules

- Every call into the connector must finish within 5 seconds of its own computing time (waiting on HTTP doesn't count). Memory is capped at 64 MB. A connector that goes over, or panics, is restarted with a growing delay and shown as unhealthy.
- `reply` / `notify` get an idempotency key and may be retried after a failure: send at most once per key (`once` does this). Return `SendResult::Gone` when the conversation no longer accepts messages, so the app stops retrying.
- Mark events that come from bot accounts with `.sender(id, name, true)`: they never start tasks.
- Tools: return them from `tools()` with `yi_connector::tool(name, description, input_schema, read_only)`. Mark tools that change nothing as read-only; a Bot that is not in automatic mode can only run read-only tools.
- Unit-test plain logic natively (`cargo test`): on non-wasm targets host functions are stubs, so keep parsing and formatting in pure functions as the first-party connectors do.
