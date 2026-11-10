//! The end-to-end test plugin. A mock server serves `GET /events?after=<n>` → `[{seq, key, thread, sender,
//! addressed, text}]`, `POST /reply`, `POST /notify`, `GET /context?thread=` and `POST /write`. The tools exercise
//! the sandbox: `exec` must be refused, `fetch_denied` must not reach a host outside `permissions.net`, and `spin`
//! must be cut off by the time limit (the instance is then restarted).

use serde::Deserialize;
use serde_json::{json, Value};
use yi_plugin::{emit, host, http, tool, ContextMessage, ContextPage, Event, Plugin, SendResult, Sender, ToolDef};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    server: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    poll_ms: Option<u64>,
}

fn config() -> Result<Config, String> {
    yi_plugin::config()
}

fn url(c: &Config, path: &str) -> String {
    format!("{}{path}", c.server.trim_end_matches('/'))
}

fn post(path: &str, body: Value) -> Result<Value, String> {
    let c = config()?;
    let mut r = http::post(url(&c, path)).json(body);
    if let Some(t) = c.token.as_deref().filter(|t| !t.is_empty()) {
        r = r.bearer(t);
    }
    r.send()?.expect_json()
}

fn poll() -> Result<(), String> {
    let c = config()?;
    let after: u64 = yi_plugin::kv_get("after").and_then(|s| s.parse().ok()).unwrap_or(0);
    let list: Vec<Value> = http::get(url(&c, &format!("/events?after={after}"))).send()?.expect_json()?;
    let mut last = after;
    for e in list {
        last = last.max(e["seq"].as_u64().unwrap_or(0));
        let sender = e["sender"].as_str().unwrap_or("");
        emit(
            Event::new(e["key"].as_str().unwrap_or(""), e["thread"].as_str().unwrap_or(""), e["text"].as_str().unwrap_or(""))
                .sender(sender, Some(sender.to_string()), false)
                .addressed(e["addressed"].as_bool().unwrap_or(true))
                .at(yi_plugin::now()),
        );
    }
    if last != after {
        yi_plugin::kv_set("after", &last.to_string());
    }
    Ok(())
}

fn sent(v: Value) -> SendResult {
    if v["gone"] == true {
        SendResult::Gone
    } else {
        SendResult::Sent
    }
}

struct Testkit;

impl Plugin for Testkit {
    fn start() -> Result<(), String> {
        let c = config()?;
        host::set_timer(c.poll_ms.unwrap_or(1000).max(100), true);
        Ok(())
    }

    fn on_timer(_id: u32) {
        match poll() {
            Ok(()) => host::health("ok", ""),
            Err(e) => host::health("error", &e),
        }
    }

    fn reply(thread: &str, reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult, String> {
        post("/reply", json!({"thread": thread, "replyTo": reply_to, "key": key, "text": text})).map(sent)
    }

    fn notify(text: &str, key: &str) -> Result<SendResult, String> {
        post("/notify", json!({"key": key, "text": text})).map(sent)
    }

    fn read_context(thread: &str, _cursor: Option<&str>) -> Result<ContextPage, String> {
        let c = config()?;
        let list: Vec<Value> = http::get(url(&c, &format!("/context?thread={thread}"))).send()?.expect_json()?;
        Ok(ContextPage {
            messages: list
                .iter()
                .map(|m| ContextMessage {
                    id: m["id"].as_str().unwrap_or("").into(),
                    sender: Sender { id: m["sender"].as_str().unwrap_or("").into(), name: None, bot: false },
                    at: 0,
                    text: m["text"].as_str().unwrap_or("").into(),
                    url: None,
                })
                .collect(),
            next: None,
        })
    }

    fn tools() -> Vec<ToolDef> {
        let text = json!({"type": "object", "properties": {"text": {"type": "string"}}});
        vec![
            tool("echo", "Return the input text", text.clone(), true),
            tool("write", "Store the text on the mock server", text, false),
            tool("exec", "Try to run a command (the sandbox must refuse)", json!({"type": "object"}), true),
            tool("fetch_denied", "Try to reach a host outside the allowed list (must fail)", json!({"type": "object"}), true),
            tool("spin", "Loop forever (must be cut off by the time limit)", json!({"type": "object"}), true),
        ]
    }

    fn call_tool(name: &str, input: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
        match name {
            "echo" => Ok(v["text"].as_str().unwrap_or("").to_string()),
            "write" => post("/write", json!({"text": v["text"]})).map(|r| r.to_string()),
            "exec" => match host::exec("whoami", &[], None) {
                Ok(out) => Err(format!("exec was allowed: {out}")),
                Err(e) => Ok(format!("refused: {e}")),
            },
            "fetch_denied" => match http::get("https://example.com/").send() {
                Ok(r) => Err(format!("fetch was allowed: HTTP {}", r.status)),
                Err(e) => Ok(format!("refused: {e}")),
            },
            "spin" => {
                let mut n: u64 = 0;
                loop {
                    n = std::hint::black_box(n.wrapping_add(1));
                }
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }
}

yi_plugin::export!(Testkit);
