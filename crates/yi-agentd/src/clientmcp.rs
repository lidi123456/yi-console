//! Client MCP bridge: MCP servers whose tools are implemented by a connected client (the desktop app's Bots).
//!
//! The AI tool starts `yi-agentd client-mcp --session <sid> --server <name> --ticket-file <f>` as a stdio MCP server.
//! That proxy forwards each request to the daemon over local IPC (`clientMcp.proxy`). The daemon answers `initialize`
//! and `tools/list` itself from the session's `ClientMcp` entry, and relays `tools/call` to the client that registered
//! with `client.hello` under the entry's `clientId` (`clientMcp.call` notification, answered by `clientMcp.result`).
//! While that client is offline, calls get the entry's offline text or fail.
//!
//! The ticket file guards the proxy like `yi_computer_use`: the ticket is bound to the first proxy process (and the AI
//! tool that started it), so the model's own shell can't open a second MCP client with it.

use crate::computer_use::{process, proxy_access, ProxyAccess};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use yi_proto::{methods, ClientMcp, Notification};

/// How long a tool call may wait for the client. Bot tools may create sessions and worktrees on other hosts
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

struct ClientConn {
    conn: u64,
    tx: mpsc::UnboundedSender<String>,
    kind: String,
    name: String,
    /// Came in over local IPC (`bridge --local` or the app itself), not SSH or the relay
    local: bool,
}

struct Ticket {
    ticket: String,
    proxy: Option<(u32, Option<u32>)>,
}

type Reply = oneshot::Sender<Result<Value, String>>;

#[derive(Default)]
pub struct Clients {
    conns: Mutex<HashMap<String, ClientConn>>,
    calls: Mutex<HashMap<String, (u64, Reply)>>,
    /// (session id, server name) -> ticket
    tickets: Mutex<HashMap<(String, String), Ticket>>,
}

/// Whether process `pid` was started (directly or further down) by this daemon, i.e. it belongs to an AI tool or
/// something the tool ran. Such processes may not register as a client or remote-control the desktop app, so the
/// model can't reach the Bot engine around its tools
pub fn spawned_by_daemon(pid: u32) -> bool {
    descends_from(pid, std::process::id(), process::parent)
}

fn descends_from(pid: u32, ancestor: u32, parent: impl Fn(u32) -> Option<u32>) -> bool {
    let mut cur = pid;
    for _ in 0..64 {
        match parent(cur) {
            Some(p) if p == ancestor => return true,
            Some(p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Checks a session's client MCP list against the host's own MCP servers
pub fn validate(list: &[ClientMcp], settings: &yi_proto::DaemonSettings) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for m in list {
        if !valid_name(&m.name) {
            bail!("MCP 名称只能包含字母、数字、下划线和短横线，长度为 1–64");
        }
        if m.name == crate::mcp::DESKTOP_SERVER
            || m.name == crate::mcp::BROWSER_SERVER
            || m.name == crate::mcp::MEMORY_SERVER
            || settings.mcp_servers.iter().any(|s| s.name == m.name)
        {
            bail!("MCP 名称和这台主机上已有的 MCP 重名：{}", m.name);
        }
        if !seen.insert(&m.name) {
            bail!("MCP 名称重复：{}", m.name);
        }
        if m.client_id.is_empty() {
            bail!("缺少客户端标识：{}", m.name);
        }
    }
    Ok(())
}

impl Clients {
    /// `client.hello` on connection `conn`. Returns whether the set of desktop apps on this machine changed
    pub fn hello(&self, conn: u64, tx: mpsc::UnboundedSender<String>, local: bool, p: &Value) -> Result<bool> {
        let id = p["clientId"].as_str().filter(|s| !s.is_empty() && s.len() <= 128).ok_or_else(|| anyhow!("缺少参数 clientId"))?;
        let kind = p["kind"].as_str().unwrap_or("desktop").to_string();
        let name = p["name"].as_str().unwrap_or("").chars().take(128).collect();
        let desktop = local && kind == "desktop";
        self.conns.lock().unwrap().insert(id.to_string(), ClientConn { conn, tx, kind, name, local });
        Ok(desktop)
    }

    /// A connection closed: forget the clients on it and fail their calls. Returns whether a desktop app on this
    /// machine went away
    pub fn disconnected(&self, conn: u64) -> bool {
        let mut desktop_gone = false;
        self.conns.lock().unwrap().retain(|_, c| {
            let keep = c.conn != conn;
            if !keep && c.local && c.kind == "desktop" {
                desktop_gone = true;
            }
            keep
        });
        let gone: Vec<Reply> = {
            let mut calls = self.calls.lock().unwrap();
            let ids: Vec<String> = calls.iter().filter(|(_, (c, _))| *c == conn).map(|(k, _)| k.clone()).collect();
            ids.into_iter().filter_map(|k| calls.remove(&k)).map(|(_, r)| r).collect()
        };
        for r in gone {
            let _ = r.send(Err("客户端已断开".into()));
        }
        desktop_gone
    }

    /// `clientMcp.result` / `client.result` from a client. Only the connection the call went to may answer it
    pub fn result(&self, conn: u64, p: &Value) -> Result<Value> {
        let id = p["callId"].as_str().ok_or_else(|| anyhow!("缺少参数 callId"))?;
        let mut calls = self.calls.lock().unwrap();
        match calls.get(id) {
            Some((c, _)) if *c == conn => {}
            _ => bail!("调用不存在或已超时"),
        }
        let (_, reply) = calls.remove(id).unwrap();
        let r = match p["error"].as_str() {
            Some(e) => Err(e.to_string()),
            None => Ok(p["result"].clone()),
        };
        let _ = reply.send(r);
        Ok(Value::Null)
    }

    #[allow(dead_code)]
    pub fn online(&self, client_id: &str) -> bool {
        self.conns.lock().unwrap().get(client_id).is_some_and(|c| !c.tx.is_closed())
    }

    /// `clients.list`: desktop apps registered over local IPC (only these can be remote-controlled)
    pub fn desktops(&self) -> Value {
        let conns = self.conns.lock().unwrap();
        let mut list: Vec<Value> = conns
            .iter()
            .filter(|(_, c)| c.local && c.kind == "desktop" && !c.tx.is_closed())
            .map(|(id, c)| json!({"clientId": id, "name": c.name}))
            .collect();
        list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!(list)
    }

    /// `client.call`: relay a remote-control command to a desktop app on this machine
    pub async fn remote_call(&self, p: &Value) -> Result<Value> {
        let client = p["clientId"].as_str().ok_or_else(|| anyhow!("缺少参数 clientId"))?;
        let cmd = p["cmd"].as_str().ok_or_else(|| anyhow!("缺少参数 cmd"))?;
        if !["bots.", "tasks.", "connectors."].iter().any(|x| cmd.starts_with(x)) {
            bail!("不能遥控这个命令：{cmd}");
        }
        let target = {
            let conns = self.conns.lock().unwrap();
            conns.get(client).is_some_and(|c| c.local && c.kind == "desktop")
        };
        if !target {
            bail!("桌面端不在线");
        }
        let args = if p["args"].is_null() { json!({}) } else { p["args"].clone() };
        match self.call(client, methods::EV_CLIENT_CALL, json!({"cmd": cmd, "args": args})).await {
            Ok(v) => Ok(v),
            Err(Some(e)) => Err(anyhow!(e)),
            Err(None) => bail!("桌面端不在线"),
        }
    }

    /// `client.event` from connection `conn`: only a desktop app registered on this machine may push events.
    /// Returns the notification params for every connected client
    pub fn event(&self, conn: u64, p: &Value) -> Result<Value> {
        let conns = self.conns.lock().unwrap();
        let (id, _) = conns
            .iter()
            .find(|(_, c)| c.conn == conn && c.local && c.kind == "desktop")
            .ok_or_else(|| anyhow!("只有本机登记的桌面端可以推送事件"))?;
        Ok(json!({"clientId": id, "event": p["event"].clone()}))
    }

    /// Send a call (`method`: clientMcp.call / client.call) to a client and wait for its answer. Err(None): the
    /// client is not connected
    async fn call(&self, client_id: &str, method: &str, params: Value) -> std::result::Result<Value, Option<String>> {
        let call_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let conns = self.conns.lock().unwrap();
            let c = conns.get(client_id).filter(|c| !c.tx.is_closed()).ok_or(None)?;
            let mut params = params;
            params["callId"] = json!(call_id);
            let n = Notification { method: method.into(), params };
            self.calls.lock().unwrap().insert(call_id.clone(), (c.conn, tx));
            if c.tx.send(serde_json::to_string(&n).unwrap()).is_err() {
                self.calls.lock().unwrap().remove(&call_id);
                return Err(None);
            }
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(Some(e)),
            Ok(Err(_)) => Err(Some("客户端已断开".into())),
            Err(_) => {
                self.calls.lock().unwrap().remove(&call_id);
                Err(Some("客户端处理超时".into()))
            }
        }
    }

    fn ticket_path(sid: &str, server: &str) -> PathBuf {
        crate::paths::base_dir().join("client-mcp").join(format!("{sid}-{server}.json"))
    }

    /// The MCP server entry for a launch: writes a ticket for (session, server) unless one is already issued
    pub fn server_config(&self, sid: &str, server: &str) -> Result<yi_proto::McpServerConfig> {
        let path = Self::ticket_path(sid, server);
        {
            let mut tickets = self.tickets.lock().unwrap();
            let key = (sid.to_string(), server.to_string());
            let fresh = !tickets.contains_key(&key) || !path.exists();
            if fresh {
                let t = uuid::Uuid::new_v4().to_string();
                std::fs::create_dir_all(path.parent().unwrap())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o700))?;
                }
                std::fs::write(&path, serde_json::to_vec(&json!({"ticket": t}))?)?;
                tickets.insert(key, Ticket { ticket: t, proxy: None });
            }
        }
        let exe = std::env::current_exe()?.to_string_lossy().into_owned();
        Ok(serde_json::from_value(json!({
            "name": server,
            "command": exe,
            "args": proxy_args(std::env::var_os("YI_CONSOLE_HOME"), sid, server, &path),
        }))?)
    }

    /// A new tool process starts its own proxies
    pub fn release_proxies(&self, sid: &str) {
        for ((s, _), t) in self.tickets.lock().unwrap().iter_mut() {
            if s == sid {
                t.proxy = None;
            }
        }
    }

    /// The session is gone: drop its tickets
    pub fn forget(&self, sid: &str) {
        let mut tickets = self.tickets.lock().unwrap();
        let keys: Vec<_> = tickets.keys().filter(|(s, _)| s == sid).cloned().collect();
        for k in keys {
            let _ = std::fs::remove_file(Self::ticket_path(&k.0, &k.1));
            tickets.remove(&k);
        }
    }

    fn check_ticket(&self, sid: &str, server: &str, ticket: &str, pid: Option<u32>) -> Result<()> {
        let pid = pid.ok_or_else(|| anyhow!("无法确认 MCP 代理进程"))?;
        let mut tickets = self.tickets.lock().unwrap();
        let t = tickets
            .get_mut(&(sid.to_string(), server.to_string()))
            .filter(|t| t.ticket == ticket)
            .ok_or_else(|| anyhow!("MCP 凭据已失效"))?;
        let parent = process::parent(pid);
        match proxy_access(t.proxy, pid, parent, process::alive) {
            ProxyAccess::Deny => bail!("{server} 只接受 AI 工具自己启动的 MCP 代理，请直接调用它的工具"),
            ProxyAccess::Bind => t.proxy = Some((pid, parent)),
            ProxyAccess::Allow => {}
        }
        Ok(())
    }

    /// One MCP request from a proxy process
    pub async fn proxy(&self, hub: &crate::hub::Hub, p: &Value, pid: Option<u32>) -> Result<Value> {
        let sid = p["sessionId"].as_str().ok_or_else(|| anyhow!("缺少参数 sessionId"))?;
        let server = p["server"].as_str().ok_or_else(|| anyhow!("缺少参数 server"))?;
        let ticket = p["ticket"].as_str().ok_or_else(|| anyhow!("缺少参数 ticket"))?;
        self.check_ticket(sid, server, ticket, pid)?;
        let info = hub.info(sid)?;
        let entry = info
            .client_mcp
            .iter()
            .find(|m| m.name == server)
            .cloned()
            .ok_or_else(|| anyhow!("这个会话没有挂载 {server}"))?;
        let packet = &p["packet"];
        Ok(match packet["method"].as_str().unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": packet["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": server, "version": yi_proto::DAEMON_VERSION},
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": entry.tools}),
            "tools/call" => {
                let tool = packet["params"]["name"].as_str().unwrap_or("").to_string();
                if !entry.tools.iter().any(|t| t["name"] == tool.as_str()) {
                    bail!("未知工具：{tool}");
                }
                let params = json!({
                    "sessionId": sid,
                    "server": server,
                    "tool": tool,
                    "arguments": packet["params"]["arguments"].clone(),
                });
                match self.call(&entry.client_id, methods::EV_CLIENT_MCP_CALL, params).await {
                    Ok(v) => call_result(v),
                    Err(Some(e)) => tool_error(&e),
                    Err(None) => match entry.offline.get(&tool) {
                        Some(text) => json!({"content": [{"type": "text", "text": text}]}),
                        None => tool_error("提供这个工具的客户端（桌面 App）不在线，请稍后再试"),
                    },
                }
            }
            "resources/list" => json!({"resources": []}),
            "prompts/list" => json!({"prompts": []}),
            m => bail!("不支持的 MCP 方法：{m}"),
        })
    }
}

/// A client may answer with a full CallToolResult or with any value (sent to the model as JSON text)
fn call_result(v: Value) -> Value {
    if v["content"].is_array() {
        return v;
    }
    let text = match &v {
        Value::String(s) => s.clone(),
        Value::Null => "ok".into(),
        other => other.to_string(),
    };
    json!({"content": [{"type": "text", "text": text}]})
}

fn tool_error(msg: &str) -> Value {
    json!({"isError": true, "content": [{"type": "text", "text": msg}]})
}

/// Same rule as the computer-use proxy: pass --home only when the daemon itself runs with YI_CONSOLE_HOME
fn proxy_args(home: Option<std::ffi::OsString>, sid: &str, server: &str, ticket: &std::path::Path) -> Vec<String> {
    let mut args = vec![];
    if let Some(home) = home.filter(|v| !v.is_empty()) {
        args.extend(["--home".to_string(), home.to_string_lossy().into_owned()]);
    }
    args.extend(["client-mcp", "--session", sid, "--server", server, "--ticket-file"].map(String::from));
    args.push(ticket.to_string_lossy().into_owned());
    args
}

/// `yi-agentd client-mcp`: the stdio MCP server the AI tool starts
pub fn proxy_main(args: &[String]) -> Result<()> {
    let arg = |key: &str| args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned();
    let sid = arg("--session").ok_or_else(|| anyhow!("缺少会话标识"))?;
    let server = arg("--server").ok_or_else(|| anyhow!("缺少 MCP 名称"))?;
    let path = arg("--ticket-file").ok_or_else(|| anyhow!("缺少 MCP 凭据"))?;
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let ticket = value["ticket"].as_str().ok_or_else(|| anyhow!("MCP 凭据无效"))?.to_string();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > 1024 * 1024 {
            bail!("MCP 请求过大");
        }
        let Ok(packet) = serde_json::from_str::<Value>(&line) else { continue };
        if packet.get("id").is_none() {
            continue;
        }
        let id = packet["id"].clone();
        let result = crate::daemon_request(
            methods::CLIENT_MCP_PROXY,
            json!({"sessionId": sid, "server": server, "ticket": ticket, "packet": packet}),
            false,
            CALL_TIMEOUT.as_secs() + 30,
        );
        let response = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) if packet["method"] == "tools/call" => {
                json!({"jsonrpc": "2.0", "id": id, "result": tool_error(&e.to_string())})
            }
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": e.to_string()}}),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> ClientMcp {
        serde_json::from_value(json!({"name": "yi_bot", "clientId": "c1", "tools": [{"name": "spawn_task"}]})).unwrap()
    }

    #[test]
    fn names_must_not_clash_with_host_servers() {
        let mut settings = yi_proto::DaemonSettings::default();
        validate(&[entry()], &settings).unwrap();
        settings.mcp_servers = vec![serde_json::from_value(json!({"name": "yi_bot", "command": "x"})).unwrap()];
        assert!(validate(&[entry()], &settings).is_err());
        let mut bad = entry();
        bad.name = "a b".into();
        assert!(validate(&[bad], &yi_proto::DaemonSettings::default()).is_err());
        assert!(validate(&[entry(), entry()], &yi_proto::DaemonSettings::default()).is_err());
    }

    #[tokio::test]
    async fn calls_reach_the_registered_client_and_only_it_may_answer() {
        let clients = std::sync::Arc::new(Clients::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        clients.hello(7, tx, true, &json!({"clientId": "c1", "kind": "desktop"})).unwrap();
        assert!(clients.online("c1"));
        let c2 = clients.clone();
        let task = tokio::spawn(async move { c2.call("c1", methods::EV_CLIENT_MCP_CALL, json!({"tool": "x"})).await });
        let line = rx.recv().await.unwrap();
        let n: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(n["method"], methods::EV_CLIENT_MCP_CALL);
        let call_id = n["params"]["callId"].as_str().unwrap().to_string();
        // Another connection can't answer it
        assert!(clients.result(8, &json!({"callId": call_id, "result": 1})).is_err());
        clients.result(7, &json!({"callId": call_id, "result": {"ok": true}})).unwrap();
        assert_eq!(task.await.unwrap().unwrap(), json!({"ok": true}));
        // Offline clients are reported as such
        assert_eq!(clients.call("nobody", methods::EV_CLIENT_MCP_CALL, json!({})).await, Err(None));
        // Disconnecting fails what is still waiting
        let c3 = clients.clone();
        let task = tokio::spawn(async move { c3.call("c1", methods::EV_CLIENT_MCP_CALL, json!({})).await });
        let _ = rx.recv().await.unwrap();
        clients.disconnected(7);
        assert!(matches!(task.await.unwrap(), Err(Some(_))));
        assert!(!clients.online("c1"));
    }

    #[tokio::test]
    async fn remote_control_reaches_only_local_desktops() {
        let clients = std::sync::Arc::new(Clients::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(clients.hello(1, tx, true, &json!({"clientId": "desk", "kind": "desktop", "name": "PC"})).unwrap());
        // A desktop app that registered over SSH (for the client MCP bridge) is not controllable here
        let (tx2, _rx2) = mpsc::unbounded_channel();
        assert!(!clients.hello(2, tx2, false, &json!({"clientId": "far", "kind": "desktop"})).unwrap());
        assert_eq!(clients.desktops(), json!([{"clientId": "desk", "name": "PC"}]));
        assert!(clients.remote_call(&json!({"clientId": "far", "cmd": "bots.list"})).await.is_err());
        assert!(clients.remote_call(&json!({"clientId": "desk", "cmd": "session.delete"})).await.is_err());

        let c2 = clients.clone();
        let task = tokio::spawn(async move { c2.remote_call(&json!({"clientId": "desk", "cmd": "bots.list"})).await });
        let n: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(n["method"], methods::EV_CLIENT_CALL);
        assert_eq!(n["params"]["cmd"], "bots.list");
        assert_eq!(n["params"]["args"], json!({}));
        clients.result(1, &json!({"callId": n["params"]["callId"], "error": "不行"})).unwrap();
        assert_eq!(task.await.unwrap().unwrap_err().to_string(), "不行");

        // Only the registered desktop pushes events
        assert_eq!(clients.event(1, &json!({"event": {"type": "bots.changed"}})).unwrap()["clientId"], "desk");
        assert!(clients.event(2, &json!({"event": {}})).is_err());
        assert!(clients.disconnected(1));
        assert!(!clients.disconnected(2));
        assert_eq!(clients.desktops(), json!([]));
    }

    #[test]
    fn plain_values_become_text_results() {
        assert_eq!(call_result(json!("hi"))["content"][0]["text"], "hi");
        assert_eq!(call_result(json!({"content": [], "isError": true}))["isError"], true);
        assert_eq!(call_result(json!({"a": 1}))["content"][0]["text"], "{\"a\":1}");
    }

    #[test]
    fn proxy_args_follow_daemon_home() {
        let t = std::path::Path::new("t.json");
        assert_eq!(proxy_args(None, "s", "yi_bot", t)[0], "client-mcp");
        assert_eq!(&proxy_args(Some("/x".into()), "s", "yi_bot", t)[..3], ["--home", "/x", "client-mcp"]);
    }

    #[test]
    fn ancestry() {
        let tree = |p: u32| match p {
            30 => Some(20),
            20 => Some(10),
            10 => Some(1),
            _ => None,
        };
        assert!(descends_from(30, 10, tree));
        assert!(descends_from(20, 10, tree));
        assert!(!descends_from(10, 10, tree));
        assert!(!descends_from(30, 99, tree));
        // A loop in stale process data ends
        assert!(!descends_from(5, 99, |p| Some(if p == 5 { 6 } else { 5 })));
    }
}
