//! Read-only related-session MCP gateway (`yi_session`).
//!
//! Lets a side chat inspect its parent conversation (omitted history, full item text, or live
//! streaming progress) and lets a parent session read its side chats on demand.
//!
//! Security boundary:
//! - Strictly read-only (`session_read`, `session_list_related`).
//! - Local IPC only (`session.mcp`), bound to the first proxy PID + parent AI tool (`proxy_access`).
//! - Scoped to the caller's session family (the parent session and its side chats); unrelated
//!   sessions on the host are always refused.

use crate::computer_use::{process, proxy_access, ProxyAccess};
use crate::hub::strip_handoff_prefix;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use yi_proto::{Event, EventBody, SessionInfo, SessionMode};

pub const SERVER: &str = "yi_session";
pub const TOOLS: &[&str] = &["session_read", "session_list_related"];

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
const DEFAULT_MAX_CHARS_PER_ITEM: usize = 4_000;
const MAX_CHARS_PER_ITEM: usize = 12_000;
const MAX_TOTAL_RESPONSE_CHARS: usize = 24_000;
const MAX_LIVE_TEXT_CHARS: usize = 6_000;

const GUIDE: &str = "The yi_session tools provide strictly read-only access to the current conversation family \
(a parent session and its hidden side chats). \
Use `session_read` to inspect the parent conversation's live progress, recover omitted history, \
or read the full conversation of a referenced side chat. \
Do not modify or continue the other session's work unless explicitly asked.";

pub fn tool_names() -> impl Iterator<Item = &'static str> {
    TOOLS.iter().copied()
}

struct SessionGrant {
    ticket: String,
    proxy: Option<(u32, Option<u32>)>,
}

pub struct SessionMcp {
    tickets_dir: PathBuf,
    grants: Mutex<HashMap<String, SessionGrant>>,
}

impl Default for SessionMcp {
    fn default() -> Self {
        Self::new(crate::paths::base_dir().join("session-mcp-tickets"))
    }
}

impl SessionMcp {
    pub fn new(tickets_dir: PathBuf) -> Self {
        Self {
            tickets_dir,
            grants: Mutex::new(HashMap::new()),
        }
    }

    fn ticket_path(&self, sid: &str) -> PathBuf {
        self.tickets_dir.join(format!("{sid}.json"))
    }

    /// Ensure a structured AI session has a ticket file and grant so it can talk to `session.mcp`.
    pub fn ensure_session(&self, sid: &str) -> Result<()> {
        if !crate::hub::is_safe_ticket_sid(sid) {
            bail!("会话 ID 无效");
        }
        let path = self.ticket_path(sid);
        let mut grants = self.grants.lock().unwrap();
        if let Some(g) = grants.get(sid) {
            if path.is_file() {
                return Ok(());
            }
            write_ticket_file(&path, &g.ticket)?;
            return Ok(());
        }
        let ticket = uuid::Uuid::new_v4().to_string();
        write_ticket_file(&path, &ticket)?;
        grants.insert(sid.to_string(), SessionGrant { ticket, proxy: None });
        Ok(())
    }

    /// Release the bound proxy PID before launching a new tool process for this session.
    pub fn release_proxy(&self, sid: &str) {
        if let Some(g) = self.grants.lock().unwrap().get_mut(sid) {
            g.proxy = None;
        }
    }

    /// Clean up session grant and ticket file when a session is deleted.
    pub fn remove_session(&self, sid: &str) {
        if !crate::hub::is_safe_ticket_sid(sid) {
            return;
        }
        if self.grants.lock().unwrap().remove(sid).is_some() {
            let _ = std::fs::remove_file(self.ticket_path(sid));
        }
    }

    pub fn mcp_config(&self, sid: &str) -> Option<Value> {
        self.grants.lock().unwrap().get(sid)?;
        Some(json!({
            "command": std::env::current_exe().ok()?.to_string_lossy(),
            "args": proxy_args(std::env::var_os("YI_CONSOLE_HOME"), sid, &self.ticket_path(sid)),
        }))
    }

    /// True if `name` is a known `mcp__yi_session__<tool>` and this session has an active grant.
    pub fn authorizes(&self, sid: &str, name: &str) -> bool {
        let Some(tool) = name.strip_prefix("mcp__yi_session__") else {
            return false;
        };
        tool_names().any(|t| t == tool) && self.grants.lock().unwrap().contains_key(sid)
    }

    pub async fn proxy_call(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        ticket: &str,
        packet: Value,
        pid: Option<u32>,
    ) -> Result<Value> {
        let pid = pid.ok_or_else(|| anyhow!("无法确认会话只读代理进程"))?;
        {
            let mut grants = self.grants.lock().unwrap();
            let g = grants
                .get_mut(sid)
                .ok_or_else(|| anyhow!("当前会话未启用会话读取工具"))?;
            if g.ticket != ticket {
                bail!("会话读取代理凭据已失效，请重新打开会话");
            }
            let parent = process::parent(pid);
            match proxy_access(g.proxy, pid, parent, process::alive) {
                ProxyAccess::Deny => {
                    bail!("yi_session 只接受 AI 工具自己启动的代理，请直接调用 yi_session 工具")
                }
                ProxyAccess::Bind => g.proxy = Some((pid, parent)),
                ProxyAccess::Allow => {}
            }
        }
        self.mcp(hub, sid, ticket, packet)
    }

    pub fn mcp(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        ticket: &str,
        packet: Value,
    ) -> Result<Value> {
        {
            let grants = self.grants.lock().unwrap();
            let g = grants
                .get(sid)
                .ok_or_else(|| anyhow!("当前会话未启用会话读取工具"))?;
            if g.ticket != ticket {
                bail!("会话读取代理凭据已失效，请重新打开会话");
            }
        }
        let params = &packet["params"];
        match packet["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "serverInfo": {"name": SERVER, "version": "1"},
                "capabilities": {"tools": {}},
                "instructions": GUIDE,
            })),
            "tools/list" => Ok(json!({"tools": tool_schemas()})),
            "ping" => Ok(json!({})),
            "resources/list" => Ok(json!({"resources": []})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            "tools/call" => self.tool_call(hub, sid, params),
            m => bail!("不支持的 yi_session MCP 请求：{m}"),
        }
    }

    fn tool_call(&self, hub: &crate::hub::Hub, caller_sid: &str, params: &Value) -> Result<Value> {
        let tool = params["name"]
            .as_str()
            .ok_or_else(|| anyhow!("缺少工具名称"))?;
        if !tool_names().any(|t| t == tool) {
            bail!("未知的会话读取工具：{tool}");
        }
        let mut args = params["arguments"].as_object().cloned().unwrap_or_default();
        args.remove("__tool_use_purpose");
        args.remove("toolSummary");
        let args = Value::Object(args);

        match tool {
            "session_read" => Ok(call_read(hub, caller_sid, &args)),
            "session_list_related" => Ok(call_list_related(hub, caller_sid)),
            _ => bail!("未知的会话读取工具：{tool}"),
        }
    }
}

fn family_root(info: &SessionInfo) -> &str {
    info.parent_id.as_deref().unwrap_or(&info.id)
}

fn call_list_related(hub: &crate::hub::Hub, caller_sid: &str) -> Value {
    let Ok(caller) = hub.info(caller_sid) else {
        return tool_error("当前会话不存在");
    };
    let root = family_root(&caller).to_string();
    let mut rows: Vec<Value> = hub
        .list()
        .into_iter()
        .filter(|s| {
            s.mode == SessionMode::Structured
                && (s.id == root || (s.side_chat && s.parent_id.as_deref() == Some(root.as_str())))
        })
        .map(|s| {
            json!({
                "sessionId": s.id,
                "role": if s.id == root { "parent" } else { "side_chat" },
                "isCurrent": s.id == caller_sid,
                "title": s.title,
                "tool": s.tool.as_str(),
                "status": s.status,
                "lastSeq": s.last_seq,
                "updatedAt": s.updated_at,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let a_parent = a["role"] == "parent";
        let b_parent = b["role"] == "parent";
        b_parent
            .cmp(&a_parent)
            .then_with(|| b["updatedAt"].as_i64().cmp(&a["updatedAt"].as_i64()))
    });

    let mut text = String::from("Related sessions in this conversation family:\n");
    for r in &rows {
        let role = r["role"].as_str().unwrap_or("side_chat");
        let sid = r["sessionId"].as_str().unwrap_or("");
        let title = r["title"].as_str().unwrap_or("").trim();
        let title = if title.is_empty() { "(untitled)" } else { title };
        let tool = r["tool"].as_str().unwrap_or("");
        let status = r["status"].as_str().unwrap_or("");
        let cur = if r["isCurrent"].as_bool().unwrap_or(false) {
            " [current]"
        } else {
            ""
        };
        text.push_str(&format!(
            "- [{role}]{cur} id={sid} tool={tool} status={status} lastSeq={} title=\"{title}\"\n",
            r["lastSeq"].as_i64().unwrap_or(0)
        ));
    }
    tool_ok(&text, json!({"sessions": rows}))
}

fn call_read(hub: &crate::hub::Hub, caller_sid: &str, args: &Value) -> Value {
    let Ok(caller) = hub.info(caller_sid) else {
        return tool_error("当前会话不存在");
    };
    let requested_id = args["sessionId"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let target_id = match requested_id {
        Some(id) => id.to_string(),
        None if caller.side_chat => caller
            .parent_id
            .clone()
            .unwrap_or_else(|| caller.id.clone()),
        None => caller.id.clone(),
    };
    let Ok(target) = hub.info(&target_id) else {
        return tool_error(&format!("找不到会话：{target_id}"));
    };
    if family_root(&caller) != family_root(&target) {
        return tool_error("无权读取无关会话：yi_session 只能读取当前会话及其父子侧聊");
    }
    if target.id != caller.id && target.mode != SessionMode::Structured {
        return tool_error("不支持读取终端子会话");
    }

    let view = match args["view"].as_str().unwrap_or("messages") {
        "activity" => "activity",
        _ => "messages",
    };
    let limit = args["limit"]
        .as_u64()
        .map(|n| (n as usize).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);
    let max_chars = args["maxCharsPerItem"]
        .as_u64()
        .map(|n| (n as usize).clamp(200, MAX_CHARS_PER_ITEM))
        .unwrap_or(DEFAULT_MAX_CHARS_PER_ITEM);

    let live_text_raw = hub.live_text_snapshot(&target.id);
    let (live_text, live_truncated) = slice_chars(&live_text_raw, 0, MAX_LIVE_TEXT_CHARS);
    let live_activity = target
        .turn
        .as_ref()
        .filter(|t| t.ended_at.is_none())
        .and_then(|t| t.activity.as_ref())
        .map(|a| json!({"name": a.name, "title": a.title, "at": a.at}));

    let session_meta = json!({
        "sessionId": target.id,
        "title": target.title,
        "tool": target.tool.as_str(),
        "status": target.status,
        "sideChat": target.side_chat,
        "lastSeq": target.last_seq,
        "liveActivity": live_activity,
        "liveText": if live_text.is_empty() { Value::Null } else { json!(live_text) },
        "liveTextTruncated": live_truncated,
    });

    // Mode 1: read a single event by exact seq with optional textOffset pagination
    if let Some(exact_seq) = args["seq"].as_i64() {
        let offset = args["textOffset"].as_u64().unwrap_or(0) as usize;
        let evs = hub
            .store
            .events_since(&target.id, exact_seq.saturating_sub(1), 1)
            .unwrap_or_default();
        let Some(ev) = evs.into_iter().find(|e| e.seq == exact_seq) else {
            return tool_error(&format!("会话 {target_id} 中不存在序号为 {exact_seq} 的事件"));
        };
        let Some((kind, full)) = format_event_full(&ev) else {
            return tool_error(&format!("序号 {exact_seq} 的事件不包含可读文本内容"));
        };
        let total_chars = full.chars().count();
        let (chunk, truncated) = slice_chars(&full, offset, max_chars);
        let next_offset = if truncated {
            Some(offset + chunk.chars().count())
        } else {
            None
        };
        let item = json!({
            "seq": ev.seq,
            "ts": ev.ts,
            "kind": kind,
            "text": chunk,
            "totalChars": total_chars,
            "truncated": truncated,
            "nextTextOffset": next_offset,
        });
        let text = render_single_item(&target, &item);
        return tool_ok(
            &text,
            json!({
                "session": session_meta,
                "items": [item],
                "earlierSeq": Value::Null,
                "nextSeq": Value::Null,
                "hasMore": truncated,
            }),
        );
    }

    let before_seq = args["beforeSeq"].as_i64();
    let after_seq = args["afterSeq"].as_i64();
    let newest_first = before_seq.is_some() || after_seq.is_none();

    // Over-fetch and scan bounded batches so runs of non-visible events (tool calls, status, etc.) never stall pagination
    let scan_limit = (limit * 6).clamp(80, 400);
    const MAX_SCAN_BATCHES: usize = 20;
    let mut cur_before = before_seq;
    let mut cur_after = after_seq.unwrap_or(0);
    let mut raw_min_seq: Option<i64> = None;
    let mut raw_max_seq: Option<i64> = None;
    let mut db_more = false;
    let mut hit_budget_or_limit = false;
    let mut selected: Vec<Value> = Vec::new();
    let header_reserve = 900 + live_text.chars().count().min(MAX_LIVE_TEXT_CHARS);
    let mut remaining = MAX_TOTAL_RESPONSE_CHARS.saturating_sub(header_reserve);

    for _ in 0..MAX_SCAN_BATCHES {
        let (raw_events, batch_db_more) = if newest_first {
            hub.store
                .events_tail(&target.id, cur_before, scan_limit)
                .unwrap_or_default()
        } else {
            let mut evs = hub
                .store
                .events_since(&target.id, cur_after, scan_limit + 1)
                .unwrap_or_default();
            let more = evs.len() > scan_limit;
            evs.truncate(scan_limit);
            (evs, more)
        };
        if raw_events.is_empty() {
            db_more = false;
            break;
        }
        if newest_first {
            cur_before = raw_events.first().map(|e| e.seq);
            raw_min_seq = cur_before;
            if raw_max_seq.is_none() {
                raw_max_seq = raw_events.last().map(|e| e.seq);
            }
        } else {
            if raw_min_seq.is_none() {
                raw_min_seq = raw_events.first().map(|e| e.seq);
            }
            if let Some(last) = raw_events.last() {
                cur_after = last.seq;
                raw_max_seq = Some(last.seq);
            }
        }
        db_more = batch_db_more;

        let filtered: Vec<(Event, &'static str, String)> = raw_events
            .into_iter()
            .filter_map(|ev| {
                let (kind, full) = format_event_for_view(&ev, view)?;
                Some((ev, kind, full))
            })
            .collect();

        let iter: Box<dyn Iterator<Item = &(Event, &'static str, String)>> = if newest_first {
            Box::new(filtered.iter().rev())
        } else {
            Box::new(filtered.iter())
        };
        for (ev, kind, full) in iter {
            if selected.len() >= limit {
                hit_budget_or_limit = true;
                break;
            }
            let (chunk, truncated) = slice_chars(full, 0, max_chars);
            let cost = chunk.chars().count() + 48;
            if !selected.is_empty() && cost > remaining {
                hit_budget_or_limit = true;
                break;
            }
            remaining = remaining.saturating_sub(cost);
            let next_offset = truncated.then(|| chunk.chars().count());
            selected.push(json!({
                "seq": ev.seq,
                "ts": ev.ts,
                "kind": kind,
                "text": chunk,
                "truncated": truncated,
                "nextTextOffset": next_offset,
            }));
        }
        if hit_budget_or_limit || !db_more {
            break;
        }
    }
    if newest_first {
        selected.reverse();
    }

    let more_remaining = db_more || hit_budget_or_limit;
    let first_seq = selected.first().and_then(|v| v["seq"].as_i64());
    let last_seq = selected.last().and_then(|v| v["seq"].as_i64());
    let earlier_seq = if newest_first && more_remaining {
        if hit_budget_or_limit {
            first_seq.or(raw_min_seq)
        } else {
            raw_min_seq.or(first_seq)
        }
    } else {
        None
    };
    let next_seq = if newest_first {
        last_seq.or(raw_max_seq)
    } else if more_remaining {
        if hit_budget_or_limit {
            last_seq.or(raw_max_seq)
        } else {
            raw_max_seq.or(last_seq)
        }
    } else {
        last_seq.or(raw_max_seq)
    };
    let has_more = if newest_first {
        earlier_seq.is_some()
    } else {
        more_remaining && next_seq.is_some()
    };

    let text = render_page(
        &target,
        view,
        &selected,
        live_activity.as_ref(),
        &live_text,
        live_truncated,
        earlier_seq,
        next_seq,
        has_more,
    );

    tool_ok(
        &text,
        json!({
            "session": session_meta,
            "items": selected,
            "earlierSeq": earlier_seq,
            "nextSeq": next_seq,
            "hasMore": has_more,
        }),
    )
}

fn format_event_for_view(ev: &Event, view: &str) -> Option<(&'static str, String)> {
    let (kind, text) = format_event_full(ev)?;
    let visible = match view {
        "activity" => true,
        _ => matches!(kind, "user_message" | "assistant_text" | "plan" | "error"),
    };
    visible.then_some((kind, text))
}

fn format_event_full(ev: &Event) -> Option<(&'static str, String)> {
    match &ev.body {
        EventBody::UserMessage { text, .. } => {
            let clean = strip_handoff_prefix(text).trim();
            if clean.is_empty() {
                None
            } else {
                Some(("user_message", clean.to_string()))
            }
        }
        EventBody::AssistantText { text, .. } => {
            let clean = text.trim();
            if clean.is_empty() {
                None
            } else {
                Some(("assistant_text", clean.to_string()))
            }
        }
        EventBody::Plan { entries, .. } => {
            if entries.is_empty() {
                return None;
            }
            let lines: Vec<String> = entries
                .iter()
                .map(|e| format!("[{}] {}", e.status, e.content))
                .collect();
            Some(("plan", lines.join("\n")))
        }
        EventBody::ToolCall { name, title, .. } => {
            Some(("tool_call", format!("{name}: {}", title.trim())))
        }
        EventBody::ToolResult {
            output, is_error, ..
        } => {
            let clean = output.trim();
            if clean.is_empty() && !is_error {
                None
            } else if *is_error {
                Some(("tool_result", format!("[error] {clean}")))
            } else {
                Some(("tool_result", clean.to_string()))
            }
        }
        EventBody::Error { message } => {
            let clean = message.trim();
            if clean.is_empty() {
                None
            } else {
                Some(("error", clean.to_string()))
            }
        }
        EventBody::Compact { phase, .. } => Some(("compact", format!("compaction {phase}"))),
        EventBody::Handoff {
            session_id,
            tool,
            to,
        } => Some((
            "handoff",
            if *to {
                format!("handed off to {} ({session_id})", tool.as_str())
            } else {
                format!("continued from {} ({session_id})", tool.as_str())
            },
        )),
        _ => None,
    }
}

fn slice_chars(s: &str, offset: usize, max_chars: usize) -> (String, bool) {
    let mut iter = s.chars();
    for _ in 0..offset {
        if iter.next().is_none() {
            return (String::new(), false);
        }
    }
    let mut out = String::new();
    for _ in 0..max_chars {
        match iter.next() {
            Some(c) => out.push(c),
            None => return (out, false),
        }
    }
    let truncated = iter.next().is_some();
    (out, truncated)
}

fn render_single_item(target: &SessionInfo, item: &Value) -> String {
    let title = if target.title.trim().is_empty() {
        "(untitled)"
    } else {
        target.title.trim()
    };
    let mut out = format!(
        "Session {} (\"{}\", status={:?}) — seq={} ({})\n\n{}",
        target.id,
        title,
        target.status,
        item["seq"].as_i64().unwrap_or(0),
        item["kind"].as_str().unwrap_or(""),
        item["text"].as_str().unwrap_or("")
    );
    if let Some(next) = item["nextTextOffset"].as_u64() {
        out.push_str(&format!(
            "\n\n[truncated; pass seq={} and textOffset={} to read more]",
            item["seq"].as_i64().unwrap_or(0),
            next
        ));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_page(
    target: &SessionInfo,
    view: &str,
    items: &[Value],
    live_activity: Option<&Value>,
    live_text: &str,
    live_truncated: bool,
    earlier_seq: Option<i64>,
    next_seq: Option<i64>,
    has_more: bool,
) -> String {
    let title = if target.title.trim().is_empty() {
        "(untitled)"
    } else {
        target.title.trim()
    };
    let mut out = format!(
        "Session: {} (\"{}\", tool={}, status={:?}, view={})\n",
        target.id,
        title,
        target.tool.as_str(),
        target.status,
        view
    );
    if let Some(act) = live_activity {
        out.push_str(&format!(
            "Live activity: {} — {}\n",
            act["name"].as_str().unwrap_or(""),
            act["title"].as_str().unwrap_or("")
        ));
    }
    if items.is_empty() {
        out.push_str("\n(No recorded items in this range.)\n");
    } else {
        out.push('\n');
        for item in items {
            let seq = item["seq"].as_i64().unwrap_or(0);
            let kind = item["kind"].as_str().unwrap_or("");
            let text = item["text"].as_str().unwrap_or("");
            out.push_str(&format!("[seq={seq} {kind}]\n{text}\n"));
            if let Some(next) = item["nextTextOffset"].as_u64() {
                out.push_str(&format!(
                    "(truncated; call session_read with seq={seq}, textOffset={next} for remainder)\n"
                ));
            }
            out.push('\n');
        }
    }
    if !live_text.is_empty() {
        out.push_str("[in-progress assistant output (streaming)]\n");
        out.push_str(live_text);
        if live_truncated {
            out.push_str("\n(live output truncated)");
        }
        out.push('\n');
    }
    if let Some(es) = earlier_seq {
        out.push_str(&format!(
            "\nPagination: pass beforeSeq={es} to read older items."
        ));
    } else if has_more {
        if let Some(ns) = next_seq {
            out.push_str(&format!(
                "\nPagination: pass afterSeq={ns} to read newer items."
            ));
        }
    }
    out
}

fn write_ticket_file(path: &Path, ticket: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec(&json!({ "ticket": ticket }))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(&data)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, &data)?;
    }
    Ok(())
}

fn proxy_args(home: Option<std::ffi::OsString>, sid: &str, ticket_path: &Path) -> Vec<String> {
    let mut args = vec![];
    if let Some(h) = home.filter(|v| !v.is_empty()) {
        args.extend(["--home".into(), h.to_string_lossy().into_owned()]);
    }
    args.extend([
        "session-mcp".into(),
        "--session".into(),
        sid.into(),
        "--ticket-file".into(),
        ticket_path.to_string_lossy().into_owned(),
    ]);
    args
}

fn tool_ok(text: &str, structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": false,
    })
}

fn tool_error(msg: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": msg}],
        "isError": true,
    })
}

fn tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "session_read",
            "description": "Read the status, live streaming output, and recorded conversation timeline of a related session in this conversation family (the parent session or one of its side chats). Omit `sessionId` inside a side chat to read the parent session. Strictly read-only.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": {"type": "string", "description": "Target session id (parent or sibling/child side chat). Omit inside a side chat to read the parent session."},
                    "view": {"type": "string", "enum": ["messages", "activity"], "description": "messages (default: user/assistant/plan/error) or activity (includes tool calls, results, compaction, handoffs)"},
                    "beforeSeq": {"type": "integer", "description": "Read older items with seq < beforeSeq (newest-first pagination)"},
                    "afterSeq": {"type": "integer", "description": "Read newer items with seq > afterSeq (oldest-first; pass 0 to start from the beginning)"},
                    "seq": {"type": "integer", "description": "Read a single item by exact seq (use with textOffset for long items)"},
                    "textOffset": {"type": "integer", "minimum": 0, "description": "Character offset when reading a single long item by seq"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Max items to return (default: 20)"},
                    "maxCharsPerItem": {"type": "integer", "minimum": 200, "maximum": 12000, "description": "Max characters per item before truncation (default: 4000)"}
                }
            }
        }),
        json!({
            "name": "session_list_related",
            "description": "List the parent session and all side chats in the current conversation family with their ids, titles, tools, and live statuses. Strictly read-only.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

/// Entry point for `yi-agentd session-mcp --session <sid> --ticket-file <path>`.
pub fn proxy_main(args: &[String]) -> Result<()> {
    let arg = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let sid = arg("--session").ok_or_else(|| anyhow!("缺少会话标识"))?;
    let path = arg("--ticket-file").ok_or_else(|| anyhow!("缺少会话读取连接凭据"))?;
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let ticket = value["ticket"]
        .as_str()
        .ok_or_else(|| anyhow!("会话读取连接凭据无效"))?
        .to_string();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > 1024 * 1024 {
            bail!("MCP 请求过大");
        }
        let Ok(packet) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if packet.get("id").is_none() {
            continue;
        }
        let id = packet["id"].clone();
        // Never spawn a fresh daemon from here: a new daemon would not have this session's grant
        let result = crate::daemon_request(
            yi_proto::methods::SESSION_MCP,
            json!({"sessionId": sid, "ticket": ticket, "packet": packet}),
            false,
            30,
        );
        let response = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) if packet["method"] == "tools/call" => {
                json!({"jsonrpc": "2.0", "id": id, "result": tool_error(&e.to_string())})
            }
            Err(e) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": e.to_string()}})
            }
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

/// True when `extra_args` carries the injected `yi_session` MCP server (Claude / Codex).
pub fn has_in_args(extra_args: &[String]) -> bool {
    extra_args.iter().any(|a| a.contains(SERVER))
}
