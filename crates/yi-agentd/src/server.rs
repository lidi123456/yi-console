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
    let mut g = TOOLS.lock().unwrap();
    if refresh || g.is_none() {
        *g = Some(crate::tools::detect(&hub.settings.read().unwrap()));
    }
    g.clone().unwrap_or_default()
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
        let tx = out_tx.clone();
        let local = peer == "local";
        tokio::spawn(async move {
            let id = req.id;
            // Host credentials can only be written by the desktop client on this machine, not by devices coming in through the relay or SSH
            let r = if req.method == methods::PROXY_SET_HOSTS && !local {
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
            let info = hub.info(&sid)?;
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
            let hub2 = hub.clone();
            tokio::task::spawn_blocking(move || hub2.send(&sid, &text, images, queue)).await??;
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
            tokio::task::spawn_blocking(move || crate::gitops::log(&cwd, skip, limit)).await??
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
                let models = crate::probe::models(tool, &settings).await.unwrap_or_default();
                hub.remember_models(tool, &models);
                serde_json::to_value(models)?
            } else {
                serde_json::to_value(cached)?
            }
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
            let settings = hub.settings.read().unwrap().clone();
            crate::probe::quota(hub, tool, &settings, p["refresh"].as_bool().unwrap_or(false)).await?
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
            serde_json::to_value(crate::history::list(tool, &settings).await)?
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
        SETTINGS_GET => serde_json::to_value(hub.settings.read().unwrap().clone())?,
        SETTINGS_SET => {
            let new: DaemonSettings = serde_json::from_value(p)?;
            crate::settings::save(&new)?;
            *hub.settings.write().unwrap() = new.clone();
            *TOOLS.lock().unwrap() = None;
            serde_json::to_value(new)?
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
