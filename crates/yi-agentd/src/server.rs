//! A single client connection: read JSON-RPC requests line by line and dispatch them, while forwarding the hub's broadcasts to this connection.

use crate::adapters::AdapterCmd;
use crate::hub::{CreateParams, Hub};
use anyhow::{anyhow, Result};
use base64::Engine;
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

static TOOLS: Mutex<Option<Vec<ToolInfo>>> = Mutex::new(None);

pub(crate) fn tools(hub: &Hub, refresh: bool) -> Vec<ToolInfo> {
    let mut list = {
        let mut g = TOOLS.lock().unwrap();
        if refresh || g.is_none() {
            *g = Some(crate::tools::detect(&hub.settings.read().unwrap()));
        }
        g.clone().unwrap_or_default()
    };
    for t in &mut list {
        let (cnt, active_id, active_label) = hub.accounts.summary(t.kind);
        t.account_count = cnt;
        t.active_account_id = active_id;
        t.active_account_label = active_label;
    }
    list
}

/// A local peer process started by this daemon (an AI tool or its children). Unknown pids count as such
fn from_tool(peer_pid: Option<u32>) -> bool {
    peer_pid.map_or(true, crate::clientmcp::spawned_by_daemon)
}

fn clients_changed(hub: &Hub) {
    let _ = hub.bus.send(Arc::new(crate::hub::Outbound { session_id: None, method: methods::EV_CLIENTS_CHANGED, params: json!({}) }));
}

pub async fn serve_conn<R, W>(hub: Arc<Hub>, r: R, mut w: W, peer: &str)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(r);
    let mut first = String::new();
    if reader.read_line(&mut first).await.unwrap_or(0) == 0 {
        crate::info!("client disconnected ({peer})");
        return;
    }
    // `yi-agentd bridge` coming in over SSH first sends a marker line: this connection on local IPC is actually remote
    let mut peer = peer;
    if peer == "local" && first.trim() == yi_proto::BRIDGE_REMOTE_MARK {
        peer = "ssh";
        first.clear();
        if reader.read_line(&mut first).await.unwrap_or(0) == 0 {
            return;
        }
    }
    crate::info!("client connected ({peer})");
    // If the first message is proxy.connect, the whole connection is handed to the target host (see proxy.rs)
    if let Ok(req) = serde_json::from_str::<Request>(&first) {
        if req.method == methods::PROXY_CONNECT {
            let target = req.params["hostId"].as_str().unwrap_or("").to_string();
            crate::proxy::serve(reader, w, req.id, &target).await;
            return;
        }
        if req.method == methods::PORT_CONNECT {
            let port = req.params["port"].as_u64().unwrap_or(0).min(65535) as u16;
            crate::info!("port.connect {port} ({peer})");
            crate::ports::serve(reader, w, req.id, port).await;
            return;
        }
    }
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let subs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // Live views of AI browsers on this connection; dropping one stops its frames
    let views: Arc<Mutex<std::collections::HashMap<String, crate::browser::View>>> = Arc::default();
    // Identifies this connection to the client MCP bridge (`client.hello`)
    static NEXT_CONN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let conn_id = NEXT_CONN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let writer = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            // Batch where possible: don't flush while more is queued
            if out_rx.is_empty() && w.flush().await.is_err() {
                break;
            }
        }
    });

    let mut bus = hub.bus.subscribe();
    let subs2 = subs.clone();
    let tx2 = out_tx.clone();
    let forward = tokio::spawn(async move {
        loop {
            match bus.recv().await {
                Ok(ob) => {
                    if let Some(sid) = &ob.session_id {
                        if !subs2.lock().unwrap().contains(sid) {
                            continue;
                        }
                    }
                    let n = Notification {
                        method: ob.method.to_string(),
                        params: ob.params.clone(),
                    };
                    if tx2.send(serde_json::to_string(&n).unwrap()).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The client was too slow and dropped messages: have it resubscribe from lastSeq
                    let n = Notification {
                        method: "resync".into(),
                        params: json!({}),
                    };
                    let _ = tx2.send(serde_json::to_string(&n).unwrap());
                }
                Err(_) => break,
            }
        }
    });

    let mut lines = reader.lines();
    let mut pending = Some(first);
    loop {
        let line = match pending.take() {
            Some(l) => l,
            None => match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                crate::warn!("bad request: {e}");
                continue;
            }
        };
        let hub = hub.clone();
        let subs = subs.clone();
        let views = views.clone();
        let tx = out_tx.clone();
        let local = peer == "local";
        let peer_pid = crate::transport::PEER_PID.try_with(|pid| *pid).ok().flatten();
        tokio::spawn(async move {
            let id = req.id;
            // Host credentials can only be written by the desktop client on this machine, not by devices coming in through the relay or SSH
            let r = if req.method == methods::COMPUTER_USE_MCP && !local {
                Err(anyhow!("桌面代理只接受本机连接"))
            } else if req.method == methods::COMPUTER_USE_MCP {
                let p = &req.params;
                match (s(p, "sessionId"), s(p, "ticket")) {
                    (Ok(sid), Ok(ticket)) => hub.desktop.proxy_call(&hub, sid, ticket, p["packet"].clone(), peer_pid).await,
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            } else if req.method == methods::BROWSER_USE_MCP && !local {
                Err(anyhow!("浏览器代理只接受本机连接"))
            } else if req.method == methods::BROWSER_USE_MCP {
                let p = &req.params;
                match (s(p, "sessionId"), s(p, "ticket")) {
                    (Ok(sid), Ok(ticket)) => hub.browsers.proxy_call(&hub, sid, ticket, p["packet"].clone(), peer_pid).await,
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            } else if req.method == methods::MEMORY_MCP && !local {
                Err(anyhow!("记忆代理只接受本机连接"))
            } else if req.method == methods::MEMORY_MCP {
                let p = &req.params;
                match (s(p, "sessionId"), s(p, "ticket")) {
                    (Ok(sid), Ok(ticket)) => hub.memories.proxy_call(&hub, sid, ticket, p["packet"].clone(), peer_pid).await,
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            } else if req.method == methods::SESSION_MCP && !local {
                Err(anyhow!("会话读取代理只接受本机连接"))
            } else if req.method == methods::SESSION_MCP {
                let p = &req.params;
                match (s(p, "sessionId"), s(p, "ticket")) {
                    (Ok(sid), Ok(ticket)) => hub.session_mcp.proxy_call(&hub, sid, ticket, p["packet"].clone(), peer_pid).await,
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            } else if req.method == methods::REVIEW_MCP && !local {
                Err(anyhow!("代码审阅代理只接受本机连接"))
            } else if req.method == methods::REVIEW_MCP {
                let p = &req.params;
                match (s(p, "sessionId"), s(p, "ticket")) {
                    (Ok(sid), Ok(ticket)) => hub.review_mcp.proxy_call(&hub, sid, ticket, p["packet"].clone(), peer_pid).await,
                    (Err(e), _) | (_, Err(e)) => Err(e),
                }
            } else if local
                && matches!(
                    req.method.as_str(),
                    methods::MEMORY_SAVE
                        | methods::MEMORY_DELETE
                        | methods::MEMORY_MOVE
                        | methods::MEMORY_UNDO
                        | methods::MEMORY_RESOLVE_PENDING
                        | methods::MEMORY_IMPORT
                        | methods::MEMORY_REBIND_PROJECT
                )
                && from_tool(peer_pid)
            {
                // AI tools and their subprocesses must go through the `yi_memory` MCP gateway, never direct admin RPCs
                Err(anyhow!("AI 工具启动的进程不能直接调用 {}", req.method))
            } else if req.method == methods::BROWSER_VIEW_ATTACH {
                // Frames go only to this connection, never through the hub's broadcast
                match s(&req.params, "sessionId") {
                    Ok(sid) => {
                        let opts = crate::browser::ViewOpts::new(local, &req.params);
                        hub.browsers.attach(hub.clone(), sid, opts, tx.clone()).map(|view| {
                            views.lock().unwrap().insert(sid.to_string(), view);
                            json!({"maxFps": opts.max_fps, "maxWidth": opts.max_width, "quality": opts.quality})
                        })
                    }
                    Err(e) => Err(e),
                }
            } else if req.method == methods::BROWSER_VIEW_DETACH {
                s(&req.params, "sessionId").map(|sid| {
                    views.lock().unwrap().remove(sid);
                    Value::Null
                })
            } else if req.method == methods::BROWSER_VIEW_ACK {
                s(&req.params, "sessionId").map(|sid| {
                    if let Some(v) = views.lock().unwrap().get(sid) {
                        v.ack(req.params["seq"].as_u64().unwrap_or(0));
                    }
                    Value::Null
                })
            } else if req.method == methods::CLIENT_MCP_PROXY && !local {
                Err(anyhow!("MCP 代理只接受本机连接"))
            } else if req.method == methods::CLIENT_MCP_PROXY {
                hub.clients.proxy(&hub, &req.params, peer_pid).await
            } else if local && (req.method == methods::CLIENT_CALL && from_tool(peer_pid) || req.method == methods::CLIENT_HELLO && peer_pid.is_some_and(crate::clientmcp::spawned_by_daemon)) {
                // The AI tools (and anything they run) are children of this daemon: they must not pose as the
                // desktop app or drive its Bot engine
                Err(anyhow!("AI 工具启动的进程不能调用 {}", req.method))
            } else if req.method == methods::CLIENT_HELLO {
                let r = hub.clients.hello(conn_id, tx.clone(), local, &req.params);
                if matches!(r, Ok(true)) {
                    clients_changed(&hub);
                }
                r.map(|_| Value::Null)
            } else if req.method == methods::CLIENT_MCP_RESULT || req.method == methods::CLIENT_RESULT {
                hub.clients.result(conn_id, &req.params)
            } else if req.method == methods::CLIENTS_LIST {
                Ok(hub.clients.desktops())
            } else if req.method == methods::CLIENT_CALL {
                hub.clients.remote_call(&req.params).await
            } else if req.method == methods::CLIENT_EVENT {
                hub.clients.event(conn_id, &req.params).map(|params| {
                    let _ = hub.bus.send(Arc::new(crate::hub::Outbound { session_id: None, method: methods::EV_CLIENT_EVENT, params }));
                    Value::Null
                })
            } else if req.method == methods::PROXY_SET_HOSTS && !local {
                Err(anyhow!("只能由本机客户端同步主机"))
            } else if req.method == methods::FS_OPEN && !local {
                // Remote devices can't launch programs on this computer
                Err(anyhow!("只能在本机打开文件"))
            } else if req.method == methods::FS_OPEN {
                let p = &req.params;
                match p["path"].as_str() {
                    Some(path) => crate::fsapi::open_native(path, p["reveal"].as_bool().unwrap_or(false)),
                    None => Err(anyhow!("缺少参数 path")),
                }
            } else {
                dispatch(&hub, &subs, &req.method, req.params).await
            };
            let resp = match r {
                Ok(result) => Response {
                    id,
                    result: Some(result),
                    error: None,
                },
                Err(e) => Response {
                    id,
                    result: None,
                    error: Some(RpcError {
                        code: -32000,
                        message: e.to_string(),
                    }),
                },
            };
            let _ = tx.send(serde_json::to_string(&resp).unwrap());
        });
    }
    forward.abort();
    views.lock().unwrap().clear();
    if hub.clients.disconnected(conn_id) {
        clients_changed(&hub);
    }
    drop(out_tx);
    let _ = writer.await;
    crate::info!("client disconnected ({peer})");
}

fn s<'a>(p: &'a Value, k: &str) -> Result<&'a str> {
    p[k].as_str().ok_or_else(|| anyhow!("缺少参数 {k}"))
}

pub async fn dispatch(
    hub: &Arc<Hub>,
    subs: &Mutex<HashSet<String>>,
    method: &str,
    p: Value,
) -> Result<Value> {
    use methods::*;
    Ok(match method {
        HELLO => {
            let hub2 = hub.clone();
            let tools = tokio::task::spawn_blocking(move || tools(&hub2, false)).await?;
            serde_json::to_value(HelloResult {
                protocol: PROTOCOL_VERSION,
                daemon_version: DAEMON_VERSION.into(),
                hostname: gethostname::gethostname().to_string_lossy().to_string(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                home: crate::paths::home().display().to_string(),
                tools,
                build_time: build_time(),
            })?
        }
        TOOLS_DETECT => {
            let hub2 = hub.clone();
            // Rebuild PATH first so tools installed after the daemon started are found
            serde_json::to_value(
                tokio::task::spawn_blocking(move || {
                    crate::tools::fix_path();
                    tools(&hub2, true)
                })
                .await?,
            )?
        }
        COMPUTER_USE_STATUS => hub.desktop.status_checked(p["sessionId"].as_str()).await,
        COMPUTER_USE_TARGETS => serde_json::to_value(hub.desktop.targets().await?)?,
        SESSION_COMPUTER_USE => {
            hub.computer_use(s(&p, "sessionId")?, s(&p, "action")?).await?
        }
        // Handled in serve_conn, which knows the peer and its process id
        COMPUTER_USE_MCP => anyhow::bail!("桌面代理只接受本机连接"),
        BROWSER_USE_STATUS => hub.browser_status(p["sessionId"].as_str()),
        SESSION_BROWSER_USE => hub.browser_use(s(&p, "sessionId")?, s(&p, "action")?, &p).await?,
        BROWSER_VIEW_INPUT => {
            let events = p["events"].as_array().cloned().unwrap_or_default();
            hub.browsers.input(s(&p, "sessionId")?, &events).await?;
            Value::Null
        }
        BROWSER_USE_MCP => anyhow::bail!("浏览器代理只接受本机连接"),
        MEMORY_MCP => anyhow::bail!("记忆代理只接受本机连接"),
        MEMORY_LIST => {
            let h = hub.clone();
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let pkey = p["projectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let settings = hub.settings.read().unwrap().memory.clone();
            let snap = tokio::task::spawn_blocking(move || h.memories.store.list(cwd.as_deref(), pkey.as_deref())).await?;
            let mut val = serde_json::to_value(snap)?;
            val["settings"] = serde_json::to_value(settings)?;
            val
        }
        MEMORY_GET => {
            let h = hub.clone();
            let scope: MemoryScope = serde_json::from_value(p["scope"].clone()).map_err(|_| anyhow!("scope 无效（应为 global 或 project）"))?;
            let pkey = p["projectKey"].as_str().map(str::to_string);
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let id = s(&p, "id")?.to_string();
            let entry = tokio::task::spawn_blocking(move || h.memories.store.get(scope, pkey.as_deref(), cwd.as_deref(), &id)).await??;
            serde_json::to_value(entry)?
        }
        MEMORY_SAVE => {
            let h = hub.clone();
            let scope: MemoryScope = serde_json::from_value(p["scope"].clone()).map_err(|_| anyhow!("scope 无效（应为 global 或 project）"))?;
            let project_key = p["projectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let id = s(&p, "id")?.trim().to_string();
            let prev_id = p["prevId"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let title = p["title"].as_str().map(str::to_string);
            let description = p["description"].as_str().map(str::to_string);
            let mode: Option<MemoryMode> = if p["mode"].is_null() {
                None
            } else {
                Some(serde_json::from_value(p["mode"].clone()).map_err(|_| anyhow!("mode 无效（应为 on-demand 或 always）"))?)
            };
            let tags: Option<Vec<String>> = p["tags"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
            let content = p["content"].as_str().ok_or_else(|| anyhow!("缺少参数 content"))?.to_string();
            let out = tokio::task::spawn_blocking(move || {
                h.memories.store.save(crate::memory::store::SaveInput {
                    scope,
                    project_key,
                    cwd,
                    id,
                    prev_id,
                    title,
                    description,
                    mode,
                    tags,
                    content,
                })
            })
            .await??;
            hub.memory_changed();
            json!({ "entry": out.entry, "undoId": out.undo_id })
        }
        MEMORY_DELETE => {
            let h = hub.clone();
            let scope: MemoryScope = serde_json::from_value(p["scope"].clone()).map_err(|_| anyhow!("scope 无效（应为 global 或 project）"))?;
            let pkey = p["projectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let id = s(&p, "id")?.trim().to_string();
            let out = tokio::task::spawn_blocking(move || h.memories.store.delete(scope, pkey.as_deref(), cwd.as_deref(), &id)).await??;
            hub.memory_changed();
            json!({ "undoId": out.undo_id })
        }
        MEMORY_MOVE => {
            let h = hub.clone();
            let from_scope: MemoryScope = serde_json::from_value(p["fromScope"].clone()).map_err(|_| anyhow!("fromScope 无效"))?;
            let from_key = p["fromProjectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let id = s(&p, "id")?.trim().to_string();
            let to_scope: MemoryScope = serde_json::from_value(p["toScope"].clone()).map_err(|_| anyhow!("toScope 无效"))?;
            let to_key = p["toProjectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let new_id = p["newId"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let out = tokio::task::spawn_blocking(move || {
                h.memories.store.move_entry(
                    from_scope,
                    from_key.as_deref(),
                    &id,
                    to_scope,
                    to_key.as_deref(),
                    cwd.as_deref(),
                    new_id.as_deref(),
                )
            })
            .await??;
            hub.memory_changed();
            json!({ "entry": out.entry, "undoId": out.undo_id })
        }
        MEMORY_UNDO => {
            let h = hub.clone();
            let undo_id = s(&p, "undoId")?.trim().to_string();
            let out = tokio::task::spawn_blocking(move || h.memories.store.undo(&undo_id)).await??;
            hub.memory_changed();
            json!({ "ok": true, "action": out.action, "scope": out.scope, "projectKey": out.project_key, "id": out.id })
        }
        MEMORY_RESOLVE_PENDING => {
            let h = hub.clone();
            let pending_id = s(&p, "pendingId")?.trim().to_string();
            let approve = p["approve"].as_bool().unwrap_or(false);
            let scope_override: Option<MemoryScope> = if p["scope"].is_null() {
                None
            } else {
                Some(serde_json::from_value(p["scope"].clone()).map_err(|_| anyhow!("scope 无效"))?)
            };
            let project_key_override = p["projectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let id_override = p["id"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let title_override = p["title"].as_str().map(str::to_string);
            let description_override = p["description"].as_str().map(str::to_string);
            let mode_override: Option<MemoryMode> = if p["mode"].is_null() {
                None
            } else {
                Some(serde_json::from_value(p["mode"].clone()).map_err(|_| anyhow!("mode 无效"))?)
            };
            let tags_override: Option<Vec<String>> = p["tags"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
            let content_override = p["content"].as_str().map(str::to_string);
            let entry = tokio::task::spawn_blocking(move || {
                h.memories.store.resolve_pending(
                    &pending_id,
                    approve,
                    scope_override,
                    project_key_override,
                    id_override,
                    title_override,
                    description_override,
                    mode_override,
                    tags_override,
                    content_override,
                )
            })
            .await??;
            hub.memory_changed();
            json!({ "ok": true, "entry": entry })
        }
        MEMORY_PREVIEW => {
            let h = hub.clone();
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let pkey = p["projectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
            let settings = hub.settings.read().unwrap().memory.clone();
            let preview = tokio::task::spawn_blocking(move || {
                h.memories
                    .store
                    .build_l0_preview(cwd.as_deref(), pkey.as_deref(), &settings, false)
            })
            .await?;
            serde_json::to_value(preview)?
        }
        MEMORY_SCAN_IMPORT => {
            let h = hub.clone();
            let cwd = p["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
            let (candidates, project) = tokio::task::spawn_blocking(move || h.memories.store.scan_import(cwd.as_deref())).await?;
            json!({ "candidates": candidates, "project": project })
        }
        MEMORY_IMPORT => {
            let h = hub.clone();
            let items = p["items"]
                .as_array()
                .cloned()
                .ok_or_else(|| anyhow!("缺少参数 items"))?;
            let (imported, skipped) = tokio::task::spawn_blocking(move || -> Result<(usize, usize)> {
                let mut imported = 0usize;
                let mut skipped = 0usize;
                for it in items {
                    let scope: MemoryScope = serde_json::from_value(it["scope"].clone()).unwrap_or(MemoryScope::Project);
                    let project_key = it["projectKey"].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
                    let cwd = it["cwd"].as_str().filter(|x| !x.trim().is_empty()).map(std::path::PathBuf::from);
                    let id = it["id"].as_str().unwrap_or("").trim().to_string();
                    let overwrite = it["overwrite"].as_bool().unwrap_or(false);
                    if id.is_empty() {
                        skipped += 1;
                        continue;
                    }
                    if !overwrite && h.memories.store.get(scope, project_key.as_deref(), cwd.as_deref(), &id).is_ok() {
                        skipped += 1;
                        continue;
                    }
                    let title = it["title"].as_str().map(str::to_string);
                    let description = it["description"].as_str().map(str::to_string);
                    let mode: Option<MemoryMode> = if it["mode"].is_null() {
                        None
                    } else {
                        serde_json::from_value(it["mode"].clone()).ok()
                    };
                    let tags: Option<Vec<String>> = it["tags"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
                    let content = it["content"].as_str().unwrap_or("").to_string();
                    if content.trim().is_empty() {
                        skipped += 1;
                        continue;
                    }
                    h.memories.store.save(crate::memory::store::SaveInput {
                        scope,
                        project_key,
                        cwd,
                        id,
                        prev_id: None,
                        title,
                        description,
                        mode,
                        tags,
                        content,
                    })?;
                    imported += 1;
                }
                Ok((imported, skipped))
            })
            .await??;
            if imported > 0 {
                hub.memory_changed();
            }
            json!({ "imported": imported, "skipped": skipped })
        }
        MEMORY_REBIND_PROJECT => {
            let h = hub.clone();
            let project_key = s(&p, "projectKey")?.trim().to_string();
            let cwd = std::path::PathBuf::from(s(&p, "cwd")?);
            let info = tokio::task::spawn_blocking(move || h.memories.store.rebind_project(&project_key, &cwd)).await??;
            hub.memory_changed();
            serde_json::to_value(info)?
        }
        SESSION_LIST => serde_json::to_value(hub.list())?,
        SESSION_CREATE => {
            let params: CreateParams = serde_json::from_value(p)?;
            let hub2 = hub.clone();
            let info = tokio::task::spawn_blocking(move || hub2.create(params)).await??;
            serde_json::to_value(info)?
        }
        SESSION_SUBSCRIBE => {
            let sid = s(&p, "sessionId")?.to_string();
            let since = p["sinceSeq"].as_i64().unwrap_or(0);
            // Register the subscription before fetching the backlog; events in between may be duplicated, and clients dedupe by seq
            subs.lock().unwrap().insert(sid.clone());
            let info = hub.subscription_info(&sid)?;
            // On first open, only the latest tail events may be requested; older ones are paged in with session.history
            let tail = p["tail"].as_u64().filter(|_| since == 0);
            let mut r = match tail {
                Some(n) => {
                    let (events, more) = hub.backlog_tail(&sid, None, n as usize)?;
                    json!({ "info": info, "events": events, "hasMore": more })
                }
                None => json!({ "info": info, "events": hub.backlog(&sid, since)? }),
            };
            if info.mode == SessionMode::Terminal {
                let snap = hub.term_snapshot(&sid)?;
                r["terminal"] = json!(base64::engine::general_purpose::STANDARD.encode(snap));
            }
            r
        }
        SESSION_STATUS_LINE => {
            let sid = s(&p, "sessionId")?;
            crate::statusline::query(hub, sid, p["git"].as_bool().unwrap_or(true)).await?
        }
        SESSION_HISTORY => {
            let sid = s(&p, "sessionId")?;
            let before = p["beforeSeq"].as_i64();
            let limit = p["limit"].as_u64().unwrap_or(500) as usize;
            let (events, more) = hub.backlog_tail(sid, before, limit)?;
            json!({ "events": events, "hasMore": more })
        }
        SESSION_USER_MESSAGES => {
            let sid = s(&p, "sessionId")?;
            let items = hub.user_messages(sid)?;
            json!({ "items": items })
        }
        SESSION_UNSUBSCRIBE => {
            subs.lock().unwrap().remove(s(&p, "sessionId")?);
            Value::Null
        }
        SESSION_SEND => {
            let sid = s(&p, "sessionId")?.to_string();
            let text = p["text"].as_str().unwrap_or("").to_string();
            let images: Vec<crate::hub::IncomingImage> =
                serde_json::from_value(p["images"].clone()).unwrap_or_default();
            if text.trim().is_empty() && images.is_empty() {
                return Err(anyhow!("消息不能为空"));
            }
            let queue = p["queue"].as_bool().unwrap_or(false);
            let origin = p["origin"].as_str().filter(|o| !o.is_empty()).map(|o| o.chars().take(32).collect::<String>());
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.send(&sid, &text, images, queue, origin)).await??;
            Value::Null
        }
        SESSION_QUEUE => {
            let sid = s(&p, "sessionId")?.to_string();
            let hub2 = hub.clone();
            // resume / sendNow may start the tool process, like session.send
            tokio::task::spawn_blocking(move || hub2.queue_action(&sid, &p)).await??
        }
        SESSION_INTERRUPTED => {
            let sid = s(&p, "sessionId")?.to_string();
            let action = s(&p, "action")?.to_string();
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.interrupted_action(&sid, &action)).await??;
            Value::Null
        }
        SESSION_SET_PERMISSION_MODE => {
            hub.set_permission_mode(s(&p, "sessionId")?, s(&p, "mode")?)?;
            Value::Null
        }
        SESSION_COMMANDS => serde_json::to_value(hub.session_commands(s(&p, "sessionId")?)?)?,
        SESSION_SET_MODE => {
            let mode: SessionMode = serde_json::from_value(p["mode"].clone()).map_err(|_| anyhow!("未知模式"))?;
            hub.set_mode(s(&p, "sessionId")?, mode).await?;
            Value::Null
        }
        SESSION_AUTO_RESUME => {
            hub.auto_resume_action(s(&p, "sessionId")?, s(&p, "action")?)?;
            Value::Null
        }
        SETTINGS_UI_LANG => {
            hub.set_ui_lang(s(&p, "lang")?)?;
            Value::Null
        }
        SESSION_PIN => {
            hub.pin(s(&p, "sessionId")?, p["pinned"].as_bool().unwrap_or(true))?;
            Value::Null
        }
        SEARCH_MESSAGES => {
            let q = s(&p, "query")?.trim().to_string();
            if q.is_empty() {
                return Ok(json!([]));
            }
            let limit = p["limit"].as_u64().unwrap_or(50).min(200) as usize;
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.search(&q, limit)).await??
        }
        GIT_CHANGES => {
            let cwd = s(&p, "cwd")?.to_string();
            let with_diff = p["diff"].as_bool().unwrap_or(true);
            let only: Option<Vec<String>> = p["paths"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
            tokio::task::spawn_blocking(move || crate::gitops::changes_with(&cwd, with_diff, only.as_deref())).await??
        }
        GIT_STATUS => {
            let cwd = s(&p, "cwd")?;
            let val = crate::statusline::git_info(cwd).await.unwrap_or(Value::Null);
            if let Some(branch) = val["branch"].as_str() {
                hub.sync_worktree_branch_for_cwd(cwd, branch);
            }
            val
        }
        GIT_WORKTREE_ADD => {
            let cwd = s(&p, "cwd")?.to_string();
            let name = s(&p, "name")?.to_string();
            let branch = p["branch"].as_str().map(str::to_string);
            let base = p["base"].as_str().map(str::to_string);
            let root = crate::gitops::worktree_root(&hub.settings.read().unwrap());
            tokio::task::spawn_blocking(move || crate::gitops::worktree_add(&root, &cwd, &name, branch.as_deref(), base.as_deref())).await??
        }
        GIT_WORKTREE_REMOVE => {
            let path = s(&p, "path")?.to_string();
            let force = p["force"].as_bool().unwrap_or(false);
            let delete_branch = p["deleteBranch"].as_bool().unwrap_or(false);
            let root = crate::gitops::worktree_root(&hub.settings.read().unwrap());
            tokio::task::spawn_blocking(move || crate::gitops::worktree_remove(&root, &path, force, delete_branch)).await??;
            Value::Null
        }
        SESSION_ENTER_WORKTREE => {
            let sid = s(&p, "sessionId")?.to_string();
            let name = p["name"].as_str().map(str::to_string);
            let branch = p["branch"].as_str().map(str::to_string);
            let base = p["base"].as_str().map(str::to_string);
            let hub2 = hub.clone();
            let info = tokio::task::spawn_blocking(move || {
                hub2.enter_worktree(&sid, name.as_deref(), branch.as_deref(), base.as_deref())
            })
            .await??;
            serde_json::to_value(info)?
        }
        SESSION_RENAME_WORKTREE => {
            let sid = s(&p, "sessionId")?.to_string();
            let branch = s(&p, "branch")?.to_string();
            let hub2 = hub.clone();
            let info = tokio::task::spawn_blocking(move || hub2.rename_worktree_branch(&sid, &branch)).await??;
            serde_json::to_value(info)?
        }
        SESSION_SET_CLIENT_MCP => {
            let list: Vec<ClientMcp> = serde_json::from_value(p["clientMcp"].clone()).map_err(|e| anyhow!("clientMcp 无效: {e}"))?;
            serde_json::to_value(hub.set_client_mcp(s(&p, "sessionId")?, list)?)?
        }
        SESSION_SET_SYSTEM_PROMPT => {
            serde_json::to_value(hub.set_system_prompt(s(&p, "sessionId")?, p["prompt"].as_str().map(str::to_string))?)?
        }
        SESSION_SET_BOT => {
            let bot: Option<BotLink> = serde_json::from_value(p["bot"].clone()).map_err(|e| anyhow!("bot 无效: {e}"))?;
            serde_json::to_value(hub.set_bot(s(&p, "sessionId")?, bot)?)?
        }
        // Handled in serve_conn, which knows the connection
        CLIENT_HELLO | CLIENT_MCP_RESULT | CLIENT_MCP_PROXY | CLIENTS_LIST | CLIENT_CALL | CLIENT_RESULT | CLIENT_EVENT => anyhow::bail!("只能在客户端连接上调用"),
        GIT_DISCARD => {
            let cwd = s(&p, "cwd")?.to_string();
            let paths: Vec<String> = serde_json::from_value(p["paths"].clone())?;
            tokio::task::spawn_blocking(move || crate::gitops::discard(&cwd, &paths)).await??;
            Value::Null
        }
        GIT_COMMIT => {
            let cwd = s(&p, "cwd")?.to_string();
            let paths: Vec<String> = serde_json::from_value(p["paths"].clone())?;
            let msg = s(&p, "message")?.to_string();
            tokio::task::spawn_blocking(move || crate::gitops::commit(&cwd, &paths, &msg)).await??
        }
        GIT_LOG => {
            let cwd = s(&p, "cwd")?.to_string();
            let skip = p["skip"].as_u64().unwrap_or(0) as usize;
            let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, 500) as usize;
            let query = p["query"].as_str().map(str::to_string);
            tokio::task::spawn_blocking(move || crate::gitops::log_with(&cwd, skip, limit, query.as_deref())).await??
        }
        GIT_SHOW => {
            let cwd = s(&p, "cwd")?.to_string();
            let hash = s(&p, "hash")?.to_string();
            let only: Option<Vec<String>> = p["paths"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
            tokio::task::spawn_blocking(move || crate::gitops::show(&cwd, &hash, only.as_deref())).await??
        }
        GIT_BLOB => {
            let cwd = s(&p, "cwd")?.to_string();
            let path = s(&p, "path")?.to_string();
            let rev = s(&p, "rev")?.to_string();
            tokio::task::spawn_blocking(move || crate::gitops::blob(&cwd, &path, &rev)).await??
        }
        GIT_WORKTREES => {
            let cwd = s(&p, "cwd")?.to_string();
            let val = tokio::task::spawn_blocking(move || crate::gitops::worktrees(&cwd)).await??;
            if let Some(items) = val["worktrees"].as_array() {
                for item in items {
                    if item["detached"].as_bool() != Some(true) {
                        if let (Some(wt_path), Some(wt_branch)) =
                            (item["path"].as_str(), item["branch"].as_str())
                        {
                            hub.sync_worktree_branch_for_cwd(wt_path, wt_branch);
                        }
                    }
                }
            }
            val
        }
        GIT_BRANCHES => {
            let cwd = s(&p, "cwd")?.to_string();
            tokio::task::spawn_blocking(move || crate::gitops::branches(&cwd)).await??
        }
        GIT_CHECKOUT => {
            let cwd = s(&p, "cwd")?.to_string();
            let cwd_sync = cwd.clone();
            let branch = s(&p, "branch")?.to_string();
            let create = p["create"].as_bool().unwrap_or(false);
            let base = p["base"].as_str().map(str::to_string);
            let val = tokio::task::spawn_blocking(move || crate::gitops::checkout(&cwd, &branch, create, base.as_deref())).await??;
            crate::statusline::invalidate_all();
            if let Some(live) = val["branch"].as_str() {
                hub.sync_worktree_branch_for_cwd(&cwd_sync, live);
            }
            val
        }
        GIT_RENAME_BRANCH => {
            let cwd = s(&p, "cwd")?.to_string();
            let cwd_sync = cwd.clone();
            let old_branch = p["oldBranch"].as_str().map(str::to_string);
            let new_branch = s(&p, "newBranch")?.to_string();
            let val = tokio::task::spawn_blocking(move || {
                crate::gitops::rename_branch(&cwd, old_branch.as_deref(), &new_branch, false)
            })
            .await??;
            crate::statusline::invalidate_all();
            if let Some(live) = val["branch"].as_str() {
                hub.sync_worktree_branch_for_cwd(&cwd_sync, live);
            }
            val
        }
        GIT_PUSH => {
            let cwd = s(&p, "cwd")?.to_string();
            let remote = p["remote"].as_str().map(str::to_string);
            let branch = p["branch"].as_str().map(str::to_string);
            let set_upstream = p["setUpstream"].as_bool().unwrap_or(true);
            tokio::task::spawn_blocking(move || crate::gitops::push(&cwd, remote.as_deref(), branch.as_deref(), set_upstream)).await??
        }
        GIT_BRANCH_DIFF => {
            let cwd = s(&p, "cwd")?.to_string();
            let base = p["base"].as_str().map(str::to_string);
            let with_diff = p["diff"].as_bool().unwrap_or(true);
            let only: Option<Vec<String>> = p["paths"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
            tokio::task::spawn_blocking(move || crate::gitops::branch_diff(&cwd, base.as_deref(), with_diff, only.as_deref())).await??
        }
        GIT_REF_DIFF => {
            let cwd = s(&p, "cwd")?.to_string();
            let base_ref = s(&p, "baseRef")?.to_string();
            let end_ref = s(&p, "endRef")?.to_string();
            let with_diff = p["diff"].as_bool().unwrap_or(true);
            let only: Option<Vec<String>> = p["paths"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
            tokio::task::spawn_blocking(move || crate::gitops::ref_diff(&cwd, &base_ref, &end_ref, with_diff, only.as_deref())).await??
        }
        GIT_REVIEW_SCOPE => {
            let cwd = s(&p, "cwd")?.to_string();
            let params = p.clone();
            tokio::task::spawn_blocking(move || crate::gitops::review_scope(&cwd, &params)).await??
        }
        FS_SEARCH => {
            let root = s(&p, "root")?.to_string();
            let q = p["query"].as_str().unwrap_or("").to_string();
            let limit = p["limit"].as_u64().unwrap_or(30).min(200) as usize;
            tokio::task::spawn_blocking(move || crate::fsapi::search(&root, &q, limit)).await??
        }
        SESSION_IMAGE => {
            let (sid, iid) = (s(&p, "sessionId")?.to_string(), s(&p, "imageId")?.to_string());
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.image(&sid, &iid)).await??
        }
        SESSION_ATTACH_FILE => {
            let sid = s(&p, "sessionId")?.to_string();
            hub.info(&sid)?;
            let (name, data) = (s(&p, "name")?.to_string(), s(&p, "data")?.to_string());
            tokio::task::spawn_blocking(move || crate::hub::attach_file(&sid, &name, &data)).await??
        }
        SESSION_ATTACH_APPEND => {
            let sid = s(&p, "sessionId")?.to_string();
            hub.info(&sid)?;
            let (path, data) = (s(&p, "path")?.to_string(), s(&p, "data")?.to_string());
            tokio::task::spawn_blocking(move || crate::hub::attach_append(&sid, &path, &data)).await??
        }
        SESSION_LOG_FILE => {
            let sid = s(&p, "sessionId")?.to_string();
            // Terminal mode: make sure the recorded tool session is still the one this process is on
            hub.guess_terminal_session(&sid).await;
            let info = hub.info(&sid)?;
            let terminal = info.mode == SessionMode::Terminal;
            // Whether there is anything to hand off, so the client doesn't have to fetch events to find out.
            // Messages typed into a terminal aren't in our event log, so terminal mode asks the tool's own log
            let logged = if terminal { 0 } else { hub.store.count_user_messages(&info.id)? };
            let (path, has_user) = match info.external_id {
                Some(ext) => {
                    let tool = info.tool;
                    tokio::task::spawn_blocking(move || {
                        let path = crate::history::session_file(tool, &ext);
                        let has = match &path {
                            Some(f) if terminal => crate::history::has_user(tool, &ext, f),
                            _ => logged > 0,
                        };
                        (path, has)
                    })
                    .await?
                }
                None => (None, logged > 0),
            };
            json!({ "path": path.map(|p| p.display().to_string()), "hasUser": has_user })
        }
        SESSION_MODELS => hub.session_models(s(&p, "sessionId")?)?,
        SESSION_SET_MODEL => {
            hub.set_model(s(&p, "sessionId")?, p["model"].as_str().unwrap_or(""))?;
            Value::Null
        }
        PROXY_SET_HOSTS => {
            let hosts: Vec<yi_client::hosts::HostConfig> = serde_json::from_value(p["hosts"].clone())?;
            crate::proxy::set_hosts(hosts, p["knownHosts"].as_str().unwrap_or(""))?;
            Value::Null
        }
        PROXY_LIST => crate::proxy::list(),
        SESSION_CLEAR => {
            hub.clear(s(&p, "sessionId")?).await?;
            Value::Null
        }
        SESSION_COMPACT => {
            let sid = s(&p, "sessionId")?.to_string();
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.compact(&sid)).await??;
            Value::Null
        }
        SESSION_STOP_TASK => {
            hub.stop_task(s(&p, "sessionId")?, s(&p, "taskId")?)?;
            Value::Null
        }
        SESSION_SET_MODEL_CONFIG => {
            hub.set_model_config(s(&p, "sessionId")?, s(&p, "configId")?, s(&p, "value")?)?;
            Value::Null
        }
        SESSION_SET_EFFORT => {
            hub.set_effort(s(&p, "sessionId")?, p["effort"].as_str().unwrap_or(""))?;
            Value::Null
        }
        TOOLS_MODELS => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let cached = hub.cached_models(tool);
            if cached.is_empty() {
                // This tool hasn't been used yet: start a temporary process to ask
                let settings = hub.settings.read().unwrap().clone();
                let models = crate::probe::models(hub, tool, &settings).await.unwrap_or_default();
                hub.remember_models(tool, &models);
                serde_json::to_value(models)?
            } else {
                serde_json::to_value(cached)?
            }
        }
        TOOLS_MODEL_CONFIG => {
            let model = s(&p, "model")?;
            if model.is_empty() { return Err(anyhow!("缺少模型")); }
            let account_id = p["accountId"].as_str().filter(|s| !s.is_empty());
            serde_json::to_value(hub.model_config_for(model, p["cwd"].as_str().unwrap_or(""), account_id).await?)?
        }
        TOOLS_LATEST => {
            let hub2 = hub.clone();
            let refresh = p["refresh"].as_bool().unwrap_or(false);
            tokio::task::spawn_blocking(move || crate::toolupdate::latest(&hub2, refresh)).await?
        }
        TOOLS_UPDATE => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            crate::toolupdate::start(hub, tool).await?
        }
        TOOLS_QUOTA => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let account_id = p["accountId"].as_str().filter(|s| !s.is_empty());
            let settings = hub.settings.read().unwrap().clone();
            crate::probe::quota(hub, tool, &settings, account_id, p["refresh"].as_bool().unwrap_or(false)).await?
        }
        TOOLS_QUOTAS => {
            let tool_filter = match p["tool"].as_str().filter(|s| !s.is_empty()) {
                Some(name) => Some(ToolKind::parse(name).ok_or_else(|| anyhow!("未知工具"))?),
                None => None,
            };
            let refresh = p["refresh"].as_bool().unwrap_or(false);
            let refresh_inactive = p["refreshInactive"].as_bool().unwrap_or(false);
            crate::probe::quotas(hub, tool_filter, refresh, refresh_inactive).await?
        }
        TOOLS_ACCOUNTS_LIST => {
            let tool_filter = match p["tool"].as_str().filter(|s| !s.is_empty()) {
                Some(name) => Some(ToolKind::parse(name).ok_or_else(|| anyhow!("未知工具"))?),
                None => None,
            };
            let check_native = p["checkNative"].as_bool().unwrap_or(true);
            let hub2 = hub.clone();
            let list = tokio::task::spawn_blocking(move || {
                let in_use = hub2.account_in_use_counts();
                let base_dir = hub2.accounts.base_dir().to_path_buf();
                hub2.accounts.list(
                    tool_filter,
                    &in_use,
                    &|t, aid| crate::probe::cached_quota(t, aid, &base_dir),
                    check_native,
                )
            })
            .await?;
            let active_logins = hub.logins.active_jobs(tool_filter);
            json!({ "tools": list, "logins": active_logins })
        }
        TOOLS_ACCOUNTS_CAPTURE => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let label = p["label"].as_str().map(str::to_string);
            let source = p["source"].as_str().map(str::to_string);
            let hub2 = hub.clone();
            let view = tokio::task::spawn_blocking(move || {
                let mut v = hub2
                    .accounts
                    .capture_with_source(tool, label.as_deref(), source.as_deref())?;
                if v.tool == ToolKind::Cursor && v.auth_kind == AccountAuthKind::ApiKey {
                    if let Some(stored) = hub2.accounts.get_stored(ToolKind::Cursor, &v.id) {
                        if let Some(cfg) = &stored.api_config {
                            if let Ok(Some((ident, raw_q))) = crate::probe::verify_cursor_api_key(
                                &cfg.api_key,
                                Some(cfg.base_url.as_str()).filter(|s| !s.is_empty()),
                            ) {
                                let auto_lbl = if label.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none() {
                                    ident.email.as_ref().map(|e| format!("{e} (API Key)"))
                                } else {
                                    None
                                };
                                if let Ok(updated) = hub2.accounts.save(crate::accounts::SaveAccountRequest {
                                    tool: ToolKind::Cursor,
                                    id: Some(v.id.clone()),
                                    label: auto_lbl,
                                    verified_identity: Some(ident),
                                    ..Default::default()
                                }) {
                                    v = updated;
                                }
                                crate::probe::cache_verified_cursor_quota(
                                    hub2.accounts.base_dir(),
                                    &v,
                                    raw_q,
                                );
                            }
                        }
                    }
                }
                hub2.publish_accounts_updated(tool, v.active.then(|| v.id.clone()));
                Ok::<_, anyhow::Error>(v)
            })
            .await??;
            serde_json::to_value(view)?
        }
        TOOLS_ACCOUNTS_SAVE => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let auth_kind = p
                .get("authKind")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value::<AccountAuthKind>(v.clone()))
                .transpose()?;
            let api_config = p
                .get("apiConfig")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value::<ApiKeyConfig>(v.clone()))
                .transpose()?;
            let req = crate::accounts::SaveAccountRequest {
                tool,
                id: p["id"].as_str().map(str::to_string),
                label: p["label"].as_str().map(str::to_string),
                auth_kind,
                allow_auto_failover: p["allowAutoFailover"].as_bool(),
                priority: p["priority"].as_i64().map(|n| n as i32),
                api_config,
                verified_identity: None,
                activate: p["activate"].as_bool().unwrap_or(false),
            };
            let clear_cache = req.api_config.is_some();
            let hub2 = hub.clone();
            let view = tokio::task::spawn_blocking(move || {
                let mut req = req;
                let mut verified_quota = None;
                if tool == ToolKind::Cursor
                    && req.auth_kind.unwrap_or(AccountAuthKind::ApiKey) == AccountAuthKind::ApiKey
                {
                    if let Some(cfg) = &req.api_config {
                        let trimmed = cfg.api_key.trim();
                        if !trimmed.is_empty() && trimmed != "********" {
                            if let Some((ident, raw_q)) = crate::probe::verify_cursor_api_key(
                                trimmed,
                                Some(cfg.base_url.as_str()).filter(|s| !s.trim().is_empty()),
                            )? {
                                req.verified_identity = Some(ident);
                                verified_quota = Some(raw_q);
                            }
                        }
                    }
                }
                let v = hub2.accounts.save(req)?;
                if clear_cache {
                    crate::probe::clear_account_cache(tool, &v.id);
                }
                if let Some(raw_q) = verified_quota {
                    crate::probe::cache_verified_cursor_quota(hub2.accounts.base_dir(), &v, raw_q);
                }
                hub2.on_account_updated(tool, &v);
                hub2.publish_accounts_updated(tool, v.active.then(|| v.id.clone()));
                Ok::<_, anyhow::Error>(v)
            })
            .await??;
            serde_json::to_value(view)?
        }
        TOOLS_ACCOUNTS_CONVERT => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            if tool != ToolKind::Codex {
                return Err(anyhow!("只有 Codex 账号可以转换"));
            }
            let account_id = s(&p, "accountId")?.to_string();
            let to = serde_json::from_value::<AccountAuthKind>(p["to"].clone()).map_err(|_| anyhow!("to 必须是 apiKey 或 oauth"))?;
            let hub2 = hub.clone();
            let view = tokio::task::spawn_blocking(move || {
                let v = hub2.accounts.convert_codex(&account_id, to)?;
                crate::probe::clear_account_cache(tool, &v.id);
                hub2.on_account_updated(tool, &v);
                hub2.publish_accounts_updated(tool, v.active.then(|| v.id.clone()));
                Ok::<_, anyhow::Error>(v)
            })
            .await??;
            serde_json::to_value(view)?
        }
        TOOLS_ACCOUNTS_DELETE => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let account_id = s(&p, "accountId")?.to_string();
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || {
                hub2.accounts.delete(tool, &account_id)?;
                crate::probe::clear_account_cache(tool, &account_id);
                hub2.on_account_deleted(tool, &account_id);
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            Value::Null
        }
        TOOLS_ACCOUNTS_ACTIVATE => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let account_id = s(&p, "accountId")?.to_string();
            let hub2 = hub.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let out = hub2.accounts.activate(tool, &account_id)?;
                if out.host_switched {
                    hub2.on_host_account_activated(tool, &account_id, Some("host_active"));
                } else {
                    hub2.publish_accounts_updated(tool, Some(account_id));
                }
                Ok::<_, anyhow::Error>(out)
            })
            .await??;
            json!({ "account": outcome.account, "hostSwitched": outcome.host_switched })
        }
        TOOLS_ACCOUNTS_REORDER => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let ids: Vec<String> = serde_json::from_value(p["ids"].clone()).map_err(|_| anyhow!("缺少参数 ids"))?;
            hub.accounts.reorder(tool, &ids)?;
            hub.publish_accounts_updated(tool, None);
            Value::Null
        }
        TOOLS_ACCOUNTS_TEST => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let mut cfg: ApiKeyConfig = serde_json::from_value(p["apiConfig"].clone()).map_err(|_| anyhow!("缺少参数 apiConfig"))?;
            if let Some(id) = p["accountId"].as_str() {
                if let Some(saved) = hub.accounts.get_stored(tool, id).and_then(|a| a.api_config) {
                    if cfg.api_key == "********" {
                        cfg.api_key = saved.api_key;
                    }
                    if cfg.auth_token == "********" {
                        cfg.auth_token = saved.auth_token;
                    }
                }
            }
            tokio::task::spawn_blocking(move || match crate::probe::test_api_key(tool, &cfg) {
                Ok(models) => json!({ "ok": true, "models": models }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            })
            .await?
        }
        TOOLS_ACCOUNTS_SET_POLICY => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let auto_failover = p["autoFailover"].as_bool().ok_or_else(|| anyhow!("缺少参数 autoFailover"))?;
            hub.accounts.set_policy(tool, auto_failover)?;
            hub.publish_accounts_updated(tool, None);
            Value::Null
        }
        TOOLS_ACCOUNTS_LOGIN_START => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let req = crate::account_login::StartLoginRequest {
                tool,
                mode: p["mode"].as_str().map(str::to_string),
                account_id: p["accountId"].as_str().map(str::to_string),
                label: p["label"].as_str().map(str::to_string),
            };
            let settings = hub.settings.read().unwrap().clone();
            let emit_hub = hub.clone();
            let emit: crate::account_login::LoginEmitFn = Arc::new(move |upd| {
                emit_hub.publish(None, methods::EV_ACCOUNT_LOGIN, serde_json::to_value(&upd).unwrap_or(Value::Null));
            });
            let changed_hub = hub.clone();
            let on_changed: crate::account_login::AccountsChangedFn = Arc::new(move |t, aid, host_switched| {
                if host_switched {
                    if let Some(id) = aid {
                        changed_hub.on_host_account_activated(t, &id, Some("host_active"));
                        return;
                    }
                }
                changed_hub.publish_accounts_updated(t, aid);
            });
            let upd = hub.logins.start(req, &settings, emit, on_changed)?;
            serde_json::to_value(upd)?
        }
        TOOLS_ACCOUNTS_LOGIN_INPUT => {
            let job_id = s(&p, "jobId")?;
            let input = p["input"].as_str().unwrap_or("");
            let emit_hub = hub.clone();
            let emit: crate::account_login::LoginEmitFn = Arc::new(move |upd| {
                emit_hub.publish(None, methods::EV_ACCOUNT_LOGIN, serde_json::to_value(&upd).unwrap_or(Value::Null));
            });
            let upd = hub.logins.send_input(job_id, input, emit)?;
            serde_json::to_value(upd)?
        }
        TOOLS_ACCOUNTS_LOGIN_CANCEL => {
            let job_id = s(&p, "jobId")?;
            let emit_hub = hub.clone();
            let emit: crate::account_login::LoginEmitFn = Arc::new(move |upd| {
                emit_hub.publish(None, methods::EV_ACCOUNT_LOGIN, serde_json::to_value(&upd).unwrap_or(Value::Null));
            });
            let upd = hub.logins.cancel(job_id, emit)?;
            serde_json::to_value(upd)?
        }
        TOOLS_ACCOUNTS_EXPORT => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let account_id = s(&p, "accountId")?.to_string();
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.accounts.export_bundle(tool, &account_id)).await??
        }
        TOOLS_ACCOUNTS_REVEAL => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let account_id = s(&p, "accountId")?.to_string();
            let cfg = hub.accounts.reveal_api_config(tool, &account_id)?;
            serde_json::to_value(cfg)?
        }
        TOOLS_ACCOUNTS_IMPORT => {
            let bundle = if p.get("bundle").is_some() { p["bundle"].clone() } else { p.clone() };
            let hub2 = hub.clone();
            let view = tokio::task::spawn_blocking(move || {
                let v = hub2.accounts.import_bundle(bundle)?;
                hub2.publish_accounts_updated(v.tool, v.active.then(|| v.id.clone()));
                Ok::<_, anyhow::Error>(v)
            })
            .await??;
            serde_json::to_value(view)?
        }
        SESSION_SWITCH_ACCOUNT => {
            let sid = s(&p, "sessionId")?.to_string();
            let account_id = s(&p, "accountId")?.to_string();
            let auto_continue = p["autoContinue"].as_bool().unwrap_or(false);
            let info = hub.switch_session_account_async(&sid, &account_id, auto_continue).await?;
            serde_json::to_value(info)?
        }
        "session.resume" => {
            let sid = s(&p, "sessionId")?.to_string();
            let cols = p["cols"].as_u64().unwrap_or(120) as u16;
            let rows = p["rows"].as_u64().unwrap_or(32) as u16;
            hub.guess_terminal_session(&sid).await;
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.resume(&sid, cols, rows)).await??;
            Value::Null
        }
        SESSION_INTERRUPT => {
            hub.interrupt(s(&p, "sessionId")?)?;
            Value::Null
        }
        SESSION_PERMISSION => {
            hub.command(
                s(&p, "sessionId")?,
                AdapterCmd::Permission {
                    request_id: s(&p, "requestId")?.to_string(),
                    option_id: s(&p, "optionId")?.to_string(),
                    answers: serde_json::from_value(p["answers"].clone()).unwrap_or_default(),
                },
            )?;
            Value::Null
        }
        SESSION_KILL => {
            hub.kill(s(&p, "sessionId")?)?;
            Value::Null
        }
        SESSION_DELETE => {
            hub.delete(s(&p, "sessionId")?)?;
            Value::Null
        }
        SESSION_RENAME => {
            hub.rename(s(&p, "sessionId")?, s(&p, "title")?)?;
            Value::Null
        }
        PTY_INPUT => {
            let data = base64::engine::general_purpose::STANDARD.decode(s(&p, "data")?)?;
            hub.command(s(&p, "sessionId")?, AdapterCmd::PtyInput(data))?;
            Value::Null
        }
        PTY_SCROLL => {
            let sid = s(&p, "sessionId")?.to_string();
            hub.info(&sid)?;
            let (lines, cancel) = (p["lines"].as_i64().unwrap_or(0), p["cancel"].as_bool().unwrap_or(false));
            let name = crate::tmux::session_name(&sid);
            match tokio::task::spawn_blocking(move || crate::tmux::scroll(&name, lines, cancel)).await? {
                Some((position, history)) => json!({ "inTmux": true, "position": position, "history": history }),
                None => json!({ "inTmux": false }),
            }
        }
        PTY_RESIZE => {
            hub.command(
                s(&p, "sessionId")?,
                AdapterCmd::PtyResize {
                    cols: p["cols"].as_u64().unwrap_or(80) as u16,
                    rows: p["rows"].as_u64().unwrap_or(24) as u16,
                },
            )?;
            Value::Null
        }
        HISTORY_LIST => {
            let tool = p["tool"].as_str().and_then(ToolKind::parse);
            let settings = hub.settings.read().unwrap().clone();
            serde_json::to_value(crate::history::list(tool, &settings, Some(&hub.accounts)).await)?
        }
        HISTORY_READ => {
            let tool = ToolKind::parse(s(&p, "tool")?).ok_or_else(|| anyhow!("未知工具"))?;
            let id = s(&p, "id")?.to_string();
            let evs = tokio::task::spawn_blocking(move || crate::history::read(tool, &id)).await??;
            let events: Vec<Event> = evs
                .into_iter()
                .enumerate()
                .map(|(i, body)| Event {
                    seq: i as i64 + 1,
                    ts: 0,
                    body,
                })
                .collect();
            serde_json::to_value(events)?
        }
        FS_LIST => crate::fsapi::list(
            p["path"].as_str().unwrap_or("~"),
            p["showHidden"].as_bool().unwrap_or(false),
        )?,
        FS_READ if p["image"].as_bool() == Some(true) => {
            let path = s(&p, "path")?.to_string();
            let (strict, probe) = (p["strict"].as_bool() == Some(true), p["probe"].as_bool() == Some(true));
            tokio::task::spawn_blocking(move || crate::fsapi::read_image(&path, strict, probe)).await??
        }
        FS_READ if p["raw"].as_bool() == Some(true) => {
            let path = s(&p, "path")?.to_string();
            let root = p["root"].as_str().map(str::to_string);
            tokio::task::spawn_blocking(move || crate::fsapi::read_raw(&path, root.as_deref())).await??
        }
        PORTS_LIST => tokio::task::spawn_blocking(crate::ports::list).await??,
        FS_GREP => {
            let root = s(&p, "root")?.to_string();
            let q = p["query"].as_str().unwrap_or("").to_string();
            let o = grep_opts(&p);
            let limit = p["limit"].as_u64().unwrap_or(500).min(20000) as usize;
            tokio::task::spawn_blocking(move || crate::fsapi::grep(&root, &q, &o, limit)).await??
        }
        FS_REPLACE => {
            let root = s(&p, "root")?.to_string();
            let q = p["query"].as_str().unwrap_or("").to_string();
            let o = grep_opts(&p);
            let dry = p["dryRun"].as_bool().unwrap_or(false);
            let files: Vec<crate::fsapi::ReplaceFile> = p["files"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|f| crate::fsapi::ReplaceFile {
                            path: f["path"].as_str().unwrap_or("").to_string(),
                            stat: f["stat"].as_array().and_then(|v| Some((v.first()?.as_i64()?, v.get(1)?.as_u64()?))),
                            only: f["matches"].as_array().map(|m| {
                                m.iter()
                                    .filter_map(|x| Some((x.get(0)?.as_u64()? as usize, x.get(1)?.as_u64()? as usize)))
                                    .collect()
                            }),
                            content: f["content"].as_str().map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || crate::fsapi::replace(&root, &q, &o, &files, dry)).await??
        }
        FS_UPLOAD => {
            let path = s(&p, "path")?.to_string();
            let data = s(&p, "data")?.to_string();
            let ow = p["overwrite"].as_bool().unwrap_or(false);
            tokio::task::spawn_blocking(move || crate::fsapi::upload(&path, &data, ow)).await??
        }
        FS_READ => crate::fsapi::read(s(&p, "path")?, p["tail"].as_bool().unwrap_or(false))?,
        FS_WRITE => crate::fsapi::write(
            s(&p, "path")?,
            p["content"].as_str().unwrap_or(""),
            p["expectModified"].as_i64(),
            p["expectSize"].as_u64(),
            p["createOnly"].as_bool().unwrap_or(false),
        )?,
        FS_MKDIR => crate::fsapi::mkdir(s(&p, "path")?)?,
        FS_RENAME => crate::fsapi::rename(s(&p, "from")?, s(&p, "to")?)?,
        FS_DELETE => crate::fsapi::delete(s(&p, "path")?)?,
        FS_ROOTS => tokio::task::spawn_blocking(crate::fsapi::roots).await?,
        STATS_USAGE => {
            // The first time requires scanning Claude / Codex logs, so do it on a blocking thread
            let h = hub.clone();
            let (days, tz) = (p["days"].as_i64().unwrap_or(30), p["tzOffset"].as_i64().unwrap_or(0));
            tokio::task::spawn_blocking(move || crate::stats::usage(&h.store, days, tz)).await??
        }
        SETTINGS_GET => serde_json::to_value(crate::mcp::redact(hub.settings.read().unwrap().clone()))?,
        SETTINGS_SET => {
            let old = hub.settings.read().unwrap().clone();
            let mut new: DaemonSettings = serde_json::from_value(p.clone())?;
            if p.get("mcpServers").is_none() { new.mcp_servers = old.mcp_servers.clone(); }
            if p.get("memory").is_none() { new.memory = old.memory.clone(); }
            new.memory.write_policy = new.memory.normalized_policy().into();
            crate::mcp::keep_secrets(&mut new, &old)?;
            crate::mcp::validate(&new)?;
            crate::settings::save(&new)?;
            *hub.settings.write().unwrap() = new.clone();
            hub.mcp_settings_changed(&old, &new);
            hub.browsers.settings_changed(&old, &new);
            if old.memory != new.memory {
                hub.memory_changed();
            }
            *TOOLS.lock().unwrap() = None;
            serde_json::to_value(crate::mcp::redact(new))?
        }
        "relay.pairing" => crate::relay::pairing_info()?,
        "relay.configure" => crate::relay::configure(
            p["url"].as_str().unwrap_or(""),
            p["fp"].as_str(),
            p["invite"].as_str(),
            p["enabled"].as_bool().unwrap_or(true),
            p["link"].as_str(),
        )?,
        "relay.pairStart" => crate::relay::pair_start()?,
        "relay.clients" => crate::relay::clients()?,
        "relay.revoke" => crate::relay::revoke(s(&p, "id")?)?,
        "relay.renameClient" => crate::relay::rename_client(s(&p, "id")?, s(&p, "name")?)?,
        "relay.reset" => crate::relay::reset()?,
        "relay.invite" => crate::relay::invite().await?,
        DAEMON_SHUTDOWN => {
            hub.shutdown.notify_waiters();
            Value::Null
        }
        _ => return Err(anyhow!("未知方法: {method}")),
    })
}

/// Search options shared by fs.grep and fs.replace
fn grep_opts(p: &Value) -> crate::fsapi::GrepOpts {
    crate::fsapi::GrepOpts {
        case_sensitive: p["caseSensitive"].as_bool().unwrap_or(false),
        whole_word: p["wholeWord"].as_bool().unwrap_or(false),
        regex: p["regex"].as_bool().unwrap_or(false),
        include: p["include"].as_str().unwrap_or("").to_string(),
        exclude: p["exclude"].as_str().unwrap_or("").to_string(),
        paths: p["paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()),
        replace: p["replace"].as_str().map(str::to_string),
        preserve_case: p["preserveCase"].as_bool().unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn desktop_proxy_rejects_remote_peers_and_forwarded_local_connections() {
        for (method, error, peer, marker) in [
            (methods::COMPUTER_USE_MCP, "桌面代理只接受本机连接", "ssh", ""),
            (methods::COMPUTER_USE_MCP, "桌面代理只接受本机连接", "relay", ""),
            (methods::COMPUTER_USE_MCP, "桌面代理只接受本机连接", "local", yi_proto::BRIDGE_REMOTE_MARK),
            (methods::BROWSER_USE_MCP, "浏览器代理只接受本机连接", "ssh", ""),
            (methods::BROWSER_USE_MCP, "浏览器代理只接受本机连接", "relay", ""),
            (methods::BROWSER_USE_MCP, "浏览器代理只接受本机连接", "local", yi_proto::BRIDGE_REMOTE_MARK),
            (methods::MEMORY_MCP, "记忆代理只接受本机连接", "ssh", ""),
            (methods::MEMORY_MCP, "记忆代理只接受本机连接", "relay", ""),
            (methods::MEMORY_MCP, "记忆代理只接受本机连接", "local", yi_proto::BRIDGE_REMOTE_MARK),
        ] {
            let hub = Hub::new(crate::store::Store::open_memory().unwrap(), DaemonSettings::default());
            let (mut client, server) = tokio::io::duplex(4096);
            let (r, w) = tokio::io::split(server);
            let task = tokio::spawn(serve_conn(hub, r, w, peer));
            let request = json!({"id":1,"method":method,"params":{}});
            let packet = format!("{}{}{}\n", marker, if marker.is_empty() { "" } else { "\n" }, request);
            client.write_all(packet.as_bytes()).await.unwrap();
            let mut reader = BufReader::new(client);
            let mut response = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(2), reader.read_line(&mut response)).await.unwrap().unwrap();
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], 1);
            assert_eq!(response["error"]["message"], error);
            drop(reader);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn accounts_reveal_returns_unredacted_api_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut hub = Hub::new(crate::store::Store::open_memory().unwrap(), DaemonSettings::default());
        Arc::get_mut(&mut hub).unwrap().accounts = Arc::new(crate::accounts::AccountManager::with_dirs(
            dir.path().join("yi"),
            dir.path().join("home"),
        ));
        let view = hub
            .accounts
            .save(crate::accounts::SaveAccountRequest {
                tool: ToolKind::Claude,
                label: Some("Reveal Test".into()),
                auth_kind: Some(AccountAuthKind::ApiKey),
                api_config: Some(ApiKeyConfig {
                    api_key: "sk-ant-plain-123".into(),
                    auth_token: "tok-plain-456".into(),
                    extra_env: [("CUSTOM_SECRET".into(), "sec-789".into())].into_iter().collect(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.api_config.as_ref().unwrap().api_key, "********");

        let (mut client, server) = tokio::io::duplex(4096);
        let (r, w) = tokio::io::split(server);
        let task = tokio::spawn(serve_conn(hub, r, w, "local"));
        let request = json!({
            "id": 1,
            "method": methods::TOOLS_ACCOUNTS_REVEAL,
            "params": { "tool": "claude", "accountId": view.id }
        });
        client.write_all(format!("{request}\n").as_bytes()).await.unwrap();
        let mut reader = BufReader::new(client);
        let mut response = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), reader.read_line(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["apiKey"], "sk-ant-plain-123");
        assert_eq!(response["result"]["authToken"], "tok-plain-456");
        assert_eq!(response["result"]["extraEnv"]["CUSTOM_SECRET"], "sec-789");
        drop(reader);
        task.await.unwrap();
    }
}
