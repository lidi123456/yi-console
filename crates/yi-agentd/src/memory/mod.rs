//! Host personal long-term memory (`yi_memory`) MCP gateway and RPC helpers.
//!
//! The AI tool starts `yi-agentd memory-mcp --session <sid> --ticket-file <f>`, which relays
//! MCP JSON-RPC over local IPC (`memory.mcp`, local peers only). The ticket binds to the first
//! proxy process and its parent AI tool (`proxy_access`), matching `yi_browser` and `client-mcp`.
//!
//! All 7 structured AI tools preauthorize `yi_memory` at the CLI/ACP layer while this gateway
//! enforces every safety and governance rule uniformly:
//! - `plan` mode, Bot sessions (`info.bot.is_some()`), and `writePolicy == "readonly"` are strictly read-only.
//! - Secret detection rejects credentials/private keys before writing.
//! - `writePolicy == "direct"` saves with a `.history/` undo snapshot and emits `EventBody::Memory`.
//! - `writePolicy == "confirm"` queues a pending proposal and emits `EventBody::Memory` for user confirmation.

pub mod store;

use crate::computer_use::{process, proxy_access, ProxyAccess};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use store::{detect_secret, MemoryStore, SaveInput};
use yi_proto::{DaemonSettings, EventBody, MemoryMode, MemoryScope, PendingMemory};

pub const SERVER: &str = "yi_memory";
pub const READ_TOOLS: &[&str] = &["memory_list", "memory_read", "memory_search"];
pub const WRITE_TOOLS: &[&str] = &["memory_save", "memory_delete"];

/// Every memory tool exposed by the gateway (used by Kiro `--trust-tools` and ACP `gateway_tool`).
pub fn tool_names() -> impl Iterator<Item = &'static str> {
    READ_TOOLS.iter().chain(WRITE_TOOLS.iter()).copied()
}

const GUIDE: &str = "The yi_memory tools access personal environment memories stored on this host outside the git repository, \
shared across all AI tools in Yi Console. \
Memories are scoped to `global` (this host) and `project` (current repository; project overrides global on conflict). \
Use `memory_read` to load full details for topics listed in <yi-memory>, or `memory_search` to find relevant notes. \
When the user asks you to remember durable environment details or preferences, check existing topics first and update by id with `memory_save`. \
Never store passwords, API keys, tokens, or private keys. Never start your own MCP client or read Yi Console ticket files.";

struct SessionGrant {
    ticket: String,
    proxy: Option<(u32, Option<u32>)>,
    cwd: PathBuf,
    readonly_session: bool,
}

pub struct Memories {
    pub store: MemoryStore,
    grants: Mutex<HashMap<String, SessionGrant>>,
}

impl Default for Memories {
    fn default() -> Self {
        Self::new(MemoryStore::default())
    }
}

impl Memories {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            grants: Mutex::new(HashMap::new()),
        }
    }

    fn ticket_path(&self, sid: &str) -> PathBuf {
        self.store
            .root_dir()
            .join("tickets")
            .join(format!("{sid}.json"))
    }

    /// Ensure a session has a ticket file and grant so its AI process can talk to `memory.mcp`.
    pub fn ensure_session(&self, sid: &str, cwd: &Path, readonly_session: bool) -> Result<()> {
        if !crate::hub::is_safe_ticket_sid(sid) {
            bail!("会话 ID 无效");
        }
        let path = self.ticket_path(sid);
        let mut grants = self.grants.lock().unwrap();
        if let Some(g) = grants.get_mut(sid) {
            g.cwd = cwd.to_path_buf();
            g.readonly_session = readonly_session;
            if path.is_file() {
                return Ok(());
            }
            // Ticket file disappeared on disk; recreate it with the same secret
            write_ticket_file(&path, &g.ticket)?;
            return Ok(());
        }
        let ticket = uuid::Uuid::new_v4().to_string();
        write_ticket_file(&path, &ticket)?;
        grants.insert(
            sid.to_string(),
            SessionGrant {
                ticket,
                proxy: None,
                cwd: cwd.to_path_buf(),
                readonly_session,
            },
        );
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

    /// True if `name` is a known `mcp__yi_memory__<tool>` and this session has an active grant.
    /// Actual write/plan/bot enforcement happens inside the gateway.
    pub fn authorizes(&self, sid: &str, name: &str) -> bool {
        let Some(tool) = name.strip_prefix("mcp__yi_memory__") else {
            return false;
        };
        tool_names().any(|t| t == tool) && self.grants.lock().unwrap().contains_key(sid)
    }

    /// Build the L0 progressive-disclosure snapshot for a session launch.
    pub fn build_l0_prompt(
        &self,
        cwd: &Path,
        settings: &DaemonSettings,
        readonly_session: bool,
    ) -> Option<String> {
        let preview = self
            .store
            .build_l0_preview(Some(cwd), None, &settings.memory, readonly_session);
        if preview.prompt.is_empty() {
            None
        } else {
            Some(preview.prompt)
        }
    }

    pub async fn proxy_call(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        ticket: &str,
        packet: Value,
        pid: Option<u32>,
    ) -> Result<Value> {
        let pid = pid.ok_or_else(|| anyhow!("无法确认记忆代理进程"))?;
        {
            let mut grants = self.grants.lock().unwrap();
            let g = grants
                .get_mut(sid)
                .ok_or_else(|| anyhow!("当前会话未启用个人记忆"))?;
            if g.ticket != ticket {
                bail!("记忆代理凭据已失效，请重新打开会话");
            }
            let parent = process::parent(pid);
            match proxy_access(g.proxy, pid, parent, process::alive) {
                ProxyAccess::Deny => {
                    bail!("yi_memory 只接受 AI 工具自己启动的代理，请直接调用 yi_memory 工具")
                }
                ProxyAccess::Bind => g.proxy = Some((pid, parent)),
                ProxyAccess::Allow => {}
            }
        }
        self.mcp(hub, sid, ticket, packet)
    }

    pub fn mcp(&self, hub: &crate::hub::Hub, sid: &str, ticket: &str, packet: Value) -> Result<Value> {
        let (cwd, readonly_session) = {
            let grants = self.grants.lock().unwrap();
            let g = grants
                .get(sid)
                .ok_or_else(|| anyhow!("当前会话未启用个人记忆"))?;
            if g.ticket != ticket {
                bail!("记忆代理凭据已失效，请重新打开会话");
            }
            (g.cwd.clone(), g.readonly_session)
        };
        let settings = hub.settings.read().unwrap().clone();
        let params = &packet["params"];
        match packet["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "serverInfo": {"name": SERVER, "version": "1"},
                "capabilities": {"tools": {}},
                "instructions": GUIDE,
            })),
            "tools/list" => {
                let allow_write =
                    !readonly_session && settings.memory.normalized_policy() != "readonly";
                Ok(json!({"tools": tool_schemas(allow_write)}))
            }
            "ping" => Ok(json!({})),
            "resources/list" => Ok(json!({"resources": []})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            "tools/call" => self.tool_call(hub, sid, &cwd, readonly_session, &settings, params),
            m => bail!("不支持的 yi_memory MCP 请求：{m}"),
        }
    }

    fn tool_call(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        cwd: &Path,
        readonly_session: bool,
        settings: &DaemonSettings,
        params: &Value,
    ) -> Result<Value> {
        if !settings.memory.enabled {
            return Ok(tool_error("这台主机已关闭个人长期记忆功能"));
        }
        let tool = params["name"]
            .as_str()
            .ok_or_else(|| anyhow!("缺少工具名称"))?;
        if !tool_names().any(|t| t == tool) {
            bail!("未知的记忆工具：{tool}");
        }
        let mut args = params["arguments"].as_object().cloned().unwrap_or_default();
        // Strip display-only fields added by Kiro / Antigravity
        args.remove("__tool_use_purpose");
        args.remove("toolSummary");
        let args = Value::Object(args);
        if serde_json::to_vec(&args)?.len() > 128 * 1024 {
            return Ok(tool_error("记忆工具参数过大"));
        }

        match tool {
            "memory_list" => Ok(self.call_list(cwd, &args)),
            "memory_read" => Ok(self.call_read(cwd, &args)),
            "memory_search" => Ok(self.call_search(cwd, &args)),
            "memory_save" => Ok(self.call_save(hub, sid, cwd, readonly_session, settings, &args)),
            "memory_delete" => Ok(self.call_delete(hub, sid, cwd, readonly_session, settings, &args)),
            _ => bail!("未知的记忆工具：{tool}"),
        }
    }

    fn call_list(&self, cwd: &Path, args: &Value) -> Value {
        let scope_arg = args["scope"].as_str().unwrap_or("all");
        let snap = self.store.list(Some(cwd), None);
        let mut out = String::new();
        if scope_arg == "all" || scope_arg == "global" {
            out.push_str("## Global memories\n");
            if snap.global.is_empty() {
                out.push_str("(none)\n");
            } else {
                for m in &snap.global {
                    out.push_str(&format!(
                        "- `{}` [{}] {}: {}\n",
                        m.id,
                        m.mode.as_str(),
                        m.title,
                        m.description
                    ));
                }
            }
        }
        if scope_arg == "all" || scope_arg == "project" {
            let pname = snap
                .resolved_project
                .as_ref()
                .map(|p| p.display_name.as_str())
                .unwrap_or("(no project detected)");
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("## Project memories ({pname})\n"));
            if snap.project.is_empty() {
                out.push_str("(none)\n");
            } else {
                for m in &snap.project {
                    out.push_str(&format!(
                        "- `{}` [{}] {}: {}\n",
                        m.id,
                        m.mode.as_str(),
                        m.title,
                        m.description
                    ));
                }
            }
        }
        tool_ok(
            out.trim_end(),
            json!({
                "project": snap.resolved_project,
                "global": if scope_arg == "project" { vec![] } else { snap.global },
                "projectMemories": if scope_arg == "global" { vec![] } else { snap.project },
            }),
        )
    }

    fn call_read(&self, cwd: &Path, args: &Value) -> Value {
        let mut requests: Vec<(Option<MemoryScope>, String)> = vec![];
        if let Some(arr) = args["items"].as_array() {
            for it in arr.iter().take(10) {
                if let Some(id) = it["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    let scope = parse_scope_opt(it["scope"].as_str());
                    requests.push((scope, id.to_string()));
                }
            }
        }
        if let Some(arr) = args["ids"].as_array() {
            let scope = parse_scope_opt(args["scope"].as_str());
            for v in arr.iter().take(10) {
                if let Some(id) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    requests.push((scope, id.to_string()));
                }
            }
        }
        if requests.is_empty() {
            if let Some(id) = args["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                requests.push((parse_scope_opt(args["scope"].as_str()), id.to_string()));
            }
        }
        if requests.is_empty() {
            return tool_error("请提供要读取的记忆 id（或 items 数组）");
        }

        let resolved_project = self.store.resolve_project(cwd);
        let pkey = resolved_project.as_ref().map(|p| p.key.as_str());
        let mut found = vec![];
        let mut missing = vec![];
        let mut text = String::new();

        for (scope_opt, id) in requests {
            let entry = match scope_opt {
                Some(MemoryScope::Global) => self.store.get(MemoryScope::Global, None, None, &id).ok(),
                Some(MemoryScope::Project) => pkey
                    .and_then(|k| self.store.get(MemoryScope::Project, Some(k), None, &id).ok()),
                // When scope is omitted, project overrides global
                None => pkey
                    .and_then(|k| self.store.get(MemoryScope::Project, Some(k), None, &id).ok())
                    .or_else(|| self.store.get(MemoryScope::Global, None, None, &id).ok()),
            };
            match entry {
                Some(e) => {
                    if !text.is_empty() {
                        text.push_str("\n\n---\n\n");
                    }
                    text.push_str(&format!(
                        "# {} (`{}:{}`)\n",
                        e.meta.title,
                        e.meta.scope.as_str(),
                        e.meta.id
                    ));
                    if !e.meta.description.is_empty() {
                        text.push_str(&format!("> {}\n", e.meta.description));
                    }
                    text.push('\n');
                    text.push_str(&e.content);
                    found.push(e);
                }
                None => missing.push(id),
            }
        }

        if found.is_empty() {
            return tool_error(&format!(
                "未找到记忆：{}。请先调用 memory_list 或 memory_search 确认标识。",
                missing.join(", ")
            ));
        }
        if !missing.is_empty() {
            text.push_str(&format!("\n\n(Not found: {})", missing.join(", ")));
        }
        tool_ok(&text, json!({"entries": found, "missing": missing}))
    }

    fn call_search(&self, cwd: &Path, args: &Value) -> Value {
        let Some(query) = args["query"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return tool_error("请提供搜索关键词 query");
        };
        let scope = parse_scope_opt(args["scope"].as_str());
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 30) as usize;
        let hits = self.store.search(query, scope, None, Some(cwd), limit);
        if hits.is_empty() {
            return tool_ok("No matching memories found.", json!({"hits": []}));
        }
        let mut text = format!("Found {} matching memories:\n", hits.len());
        for h in &hits {
            text.push_str(&format!(
                "- `{}:{}` ({}) — {}\n  {}\n",
                h.scope.as_str(),
                h.id,
                h.title,
                h.description,
                h.snippet
            ));
        }
        tool_ok(text.trim_end(), json!({"hits": hits}))
    }

    fn guard_write(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        readonly_session: bool,
        settings: &DaemonSettings,
    ) -> Result<&'static str, Value> {
        if readonly_session {
            return Err(tool_error(
                "Bot 会话中的主机个人记忆为只读；请使用 Bot 自身的记忆工具，不要写入全局/项目个人环境记忆",
            ));
        }
        if let Ok(info) = hub.info(sid) {
            if info.bot.is_some() {
                return Err(tool_error("Bot 会话中的主机个人记忆为只读"));
            }
            if info.permission_mode.as_deref() == Some("plan") {
                return Err(tool_error("当前是只读规划模式，不能写入或删除长期记忆"));
            }
        }
        let policy = settings.memory.normalized_policy();
        if policy == "readonly" {
            return Err(tool_error(
                "这台主机已将个人长期记忆设为只读模式，AI 不能修改记忆",
            ));
        }
        Ok(policy)
    }

    fn call_save(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        cwd: &Path,
        readonly_session: bool,
        settings: &DaemonSettings,
        args: &Value,
    ) -> Value {
        let policy = match self.guard_write(hub, sid, readonly_session, settings) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Some(id) = args["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return tool_error("缺少记忆标识 id（仅支持字母、数字、下划线、短横线）");
        };
        let Some(content) = args["content"].as_str().filter(|s| !s.trim().is_empty()) else {
            return tool_error("缺少记忆正文 content");
        };
        let title = args["title"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let description = args["description"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mode = match args["mode"].as_str() {
            Some("always") => Some(MemoryMode::Always),
            Some("on-demand") | None => Some(MemoryMode::OnDemand),
            Some(other) => {
                return tool_error(&format!(
                    "无效的 mode：{other}（只能是 on-demand 或 always）"
                ))
            }
        };
        let tags = args["tags"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>()
        });

        // Secret filter on AI writes
        for text in [
            Some(content),
            title.as_deref(),
            description.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(kind) = detect_secret(text) {
                return tool_error(&format!(
                    "记忆内容疑似包含敏感凭据（{kind}），已拒绝保存。请不要在记忆中写入密码、API Key、Token 或私钥，只记录它们存放的位置即可。"
                ));
            }
        }

        let resolved_project = self.store.resolve_project(cwd);
        let scope = match args["scope"].as_str() {
            Some("global") => MemoryScope::Global,
            Some("project") => MemoryScope::Project,
            None => {
                if resolved_project.is_some() {
                    MemoryScope::Project
                } else {
                    MemoryScope::Global
                }
            }
            Some(other) => {
                return tool_error(&format!(
                    "无效的 scope：{other}（只能是 global 或 project）"
                ))
            }
        };
        if scope == MemoryScope::Project && resolved_project.is_none() {
            return tool_error("当前会话目录不是项目目录，请传 scope: \"global\" 保存为全局记忆");
        }

        if policy == "confirm" {
            if let Err(e) = store::validate_id(id) {
                return tool_error(&e.to_string());
            }
            if content.len() > store::MAX_CONTENT_BYTES {
                return tool_error(&format!(
                    "记忆正文超过上限（最多 {} KB）",
                    store::MAX_CONTENT_BYTES / 1024
                ));
            }
            let title_str = title.unwrap_or_else(|| id.to_string());
            let desc_str = description.unwrap_or_default();
            let proj = if scope == MemoryScope::Project {
                resolved_project
            } else {
                None
            };
            let pending = match self.store.enqueue_pending(PendingMemory {
                pending_id: String::new(),
                action: "save".into(),
                scope,
                project_key: proj.as_ref().map(|p| p.key.clone()),
                project_name: proj.as_ref().map(|p| p.display_name.clone()),
                id: id.to_string(),
                title: title_str.clone(),
                description: desc_str.clone(),
                mode: mode.unwrap_or(MemoryMode::OnDemand),
                tags: tags.unwrap_or_default(),
                content: content.to_string(),
                session_id: Some(sid.to_string()),
                created_at: 0,
            }) {
                Ok(p) => p,
                Err(e) => return tool_error(&e.to_string()),
            };
            hub.desktop_event(
                sid,
                EventBody::Memory {
                    action: "pending_save".into(),
                    scope,
                    project_key: pending.project_key.clone(),
                    project_name: pending.project_name.clone(),
                    id: pending.id.clone(),
                    title: pending.title.clone(),
                    description: pending.description.clone(),
                    mode: pending.mode,
                    undo_id: None,
                    pending_id: Some(pending.pending_id.clone()),
                },
            );
            hub.memory_changed();
            return tool_ok(
                &format!(
                    "Submitted memory proposal `{}:{}` for user confirmation in Yi Console (pendingId: {}). It will take effect once confirmed.",
                    scope.as_str(),
                    pending.id,
                    pending.pending_id
                ),
                json!({"status": "pending", "pendingId": pending.pending_id, "scope": scope, "id": pending.id}),
            );
        }

        // Direct save with undo backup
        let outcome = match self.store.save(SaveInput {
            scope,
            project_key: None,
            cwd: Some(cwd.to_path_buf()),
            id: id.to_string(),
            prev_id: None,
            title,
            description,
            mode,
            tags,
            content: content.to_string(),
        }) {
            Ok(o) => o,
            Err(e) => return tool_error(&e.to_string()),
        };
        hub.desktop_event(
            sid,
            EventBody::Memory {
                action: "saved".into(),
                scope: outcome.entry.meta.scope,
                project_key: outcome.entry.meta.project_key.clone(),
                project_name: outcome.project_name.clone(),
                id: outcome.entry.meta.id.clone(),
                title: outcome.entry.meta.title.clone(),
                description: outcome.entry.meta.description.clone(),
                mode: outcome.entry.meta.mode,
                undo_id: Some(outcome.undo_id.clone()),
                pending_id: None,
            },
        );
        hub.memory_changed();
        tool_ok(
            &format!(
                "Saved {} memory `{}` (mode: {}). It is readable immediately via `memory_read` and will appear in future sessions' <yi-memory> index.",
                outcome.entry.meta.scope.as_str(),
                outcome.entry.meta.id,
                outcome.entry.meta.mode.as_str()
            ),
            json!({"status": "saved", "entry": outcome.entry.meta, "undoId": outcome.undo_id}),
        )
    }

    fn call_delete(
        &self,
        hub: &crate::hub::Hub,
        sid: &str,
        cwd: &Path,
        readonly_session: bool,
        settings: &DaemonSettings,
        args: &Value,
    ) -> Value {
        let policy = match self.guard_write(hub, sid, readonly_session, settings) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Some(id) = args["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return tool_error("缺少要删除的记忆标识 id");
        };
        let Some(scope) = parse_scope_opt(args["scope"].as_str()) else {
            return tool_error("删除记忆必须明确指定 scope（global 或 project）");
        };
        let resolved_project = if scope == MemoryScope::Project {
            self.store.resolve_project(cwd)
        } else {
            None
        };
        if scope == MemoryScope::Project && resolved_project.is_none() {
            return tool_error("当前会话目录不是项目目录");
        }
        let existing = match self.store.get(
            scope,
            resolved_project.as_ref().map(|p| p.key.as_str()),
            None,
            id,
        ) {
            Ok(e) => e,
            Err(e) => return tool_error(&e.to_string()),
        };

        if policy == "confirm" {
            let pending = match self.store.enqueue_pending(PendingMemory {
                pending_id: String::new(),
                action: "delete".into(),
                scope,
                project_key: existing.meta.project_key.clone(),
                project_name: resolved_project.as_ref().map(|p| p.display_name.clone()),
                id: existing.meta.id.clone(),
                title: existing.meta.title.clone(),
                description: existing.meta.description.clone(),
                mode: existing.meta.mode,
                tags: existing.meta.tags.clone(),
                content: String::new(),
                session_id: Some(sid.to_string()),
                created_at: 0,
            }) {
                Ok(p) => p,
                Err(e) => return tool_error(&e.to_string()),
            };
            hub.desktop_event(
                sid,
                EventBody::Memory {
                    action: "pending_delete".into(),
                    scope,
                    project_key: pending.project_key.clone(),
                    project_name: pending.project_name.clone(),
                    id: pending.id.clone(),
                    title: pending.title.clone(),
                    description: pending.description.clone(),
                    mode: pending.mode,
                    undo_id: None,
                    pending_id: Some(pending.pending_id.clone()),
                },
            );
            hub.memory_changed();
            return tool_ok(
                &format!(
                    "Submitted request to delete {} memory `{}` for user confirmation (pendingId: {}).",
                    scope.as_str(),
                    pending.id,
                    pending.pending_id
                ),
                json!({"status": "pending", "pendingId": pending.pending_id, "scope": scope, "id": pending.id}),
            );
        }

        let outcome = match self.store.delete(
            scope,
            resolved_project.as_ref().map(|p| p.key.as_str()),
            None,
            id,
        ) {
            Ok(o) => o,
            Err(e) => return tool_error(&e.to_string()),
        };
        hub.desktop_event(
            sid,
            EventBody::Memory {
                action: "deleted".into(),
                scope: outcome.deleted.scope,
                project_key: outcome.deleted.project_key.clone(),
                project_name: outcome.project_name.clone(),
                id: outcome.deleted.id.clone(),
                title: outcome.deleted.title.clone(),
                description: outcome.deleted.description.clone(),
                mode: outcome.deleted.mode,
                undo_id: Some(outcome.undo_id.clone()),
                pending_id: None,
            },
        );
        hub.memory_changed();
        tool_ok(
            &format!(
                "Deleted {} memory `{}` (the user can undo this from the timeline).",
                outcome.deleted.scope.as_str(),
                outcome.deleted.id
            ),
            json!({"status": "deleted", "scope": outcome.deleted.scope, "id": outcome.deleted.id, "undoId": outcome.undo_id}),
        )
    }
}

fn parse_scope_opt(s: Option<&str>) -> Option<MemoryScope> {
    match s.map(str::trim) {
        Some("global") => Some(MemoryScope::Global),
        Some("project") => Some(MemoryScope::Project),
        _ => None,
    }
}

fn write_ticket_file(path: &Path, ticket: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::write(path, serde_json::to_vec(&json!({"ticket": ticket}))?)?;
    crate::util::restrict_file(path);
    Ok(())
}

fn proxy_args(home: Option<std::ffi::OsString>, sid: &str, ticket_path: &Path) -> Vec<String> {
    let mut args = vec![];
    if let Some(h) = home.filter(|v| !v.is_empty()) {
        args.extend(["--home".into(), h.to_string_lossy().into_owned()]);
    }
    args.extend([
        "memory-mcp".into(),
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

fn tool_schemas(allow_write: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "memory_list",
            "description": "List available personal environment memories (titles, descriptions, modes) for this host and current project.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["all", "global", "project"], "description": "Filter by scope (default: all)"}
                }
            }
        }),
        json!({
            "name": "memory_read",
            "description": "Read the full Markdown content of one or more personal memories by id. Pass `id` (and optional `scope`) or `items: [{scope, id}]` for batch reads.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Memory id to read"},
                    "scope": {"type": "string", "enum": ["global", "project"], "description": "Optional scope (defaults to project first, then global)"},
                    "items": {
                        "type": "array",
                        "description": "Batch read multiple memories at once",
                        "items": {
                            "type": "object",
                            "properties": {
                                "scope": {"type": "string", "enum": ["global", "project"]},
                                "id": {"type": "string"}
                            },
                            "required": ["id"]
                        }
                    }
                }
            }
        }),
        json!({
            "name": "memory_search",
            "description": "Search global and current-project personal memories by keyword (matches id, title, tags, description, and body).",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query (Chinese or English)"},
                    "scope": {"type": "string", "enum": ["global", "project"], "description": "Optional scope filter"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Max results (default: 10)"}
                },
                "required": ["query"]
            }
        }),
    ];
    if allow_write {
        tools.push(json!({
            "name": "memory_save",
            "description": "Create or update a personal environment memory outside the git repo. Check existing topics first and reuse the same id when updating. Use scope='project' for repo-specific paths/scripts/ops and scope='global' for host-wide environment or personal preferences. Never store passwords, API keys, tokens, or private keys.",
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["global", "project"], "description": "global (host-wide) or project (current repository; default when inside a project)"},
                    "id": {"type": "string", "description": "Short kebab-case ASCII identifier (1-64 chars, e.g. local-build, relay-ops)"},
                    "title": {"type": "string", "description": "Short human-readable title"},
                    "description": {"type": "string", "description": "One-line trigger description explaining WHEN to read this memory"},
                    "mode": {"type": "string", "enum": ["on-demand", "always"], "description": "on-demand (default: only index shown at startup) or always (short hard guardrail <= 300 chars always shown at startup)"},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional short search tags"},
                    "content": {"type": "string", "description": "Markdown body of the memory"}
                },
                "required": ["id", "content"]
            }
        }));
        tools.push(json!({
            "name": "memory_delete",
            "description": "Delete an obsolete or incorrect personal memory (soft-deleted with undo history).",
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {"type": "string", "enum": ["global", "project"], "description": "Scope of the memory to delete"},
                    "id": {"type": "string", "description": "Memory id to delete"}
                },
                "required": ["scope", "id"]
            }
        }));
    }
    tools
}

/// Entry point for `yi-agentd memory-mcp --session <sid> --ticket-file <path>`.
pub fn proxy_main(args: &[String]) -> Result<()> {
    let arg = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let sid = arg("--session").ok_or_else(|| anyhow!("缺少会话标识"))?;
    let path = arg("--ticket-file").ok_or_else(|| anyhow!("缺少记忆连接凭据"))?;
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let ticket = value["ticket"]
        .as_str()
        .ok_or_else(|| anyhow!("记忆连接凭据无效"))?
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
            yi_proto::methods::MEMORY_MCP,
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

/// True when `extra_args` carries the injected `yi_memory` MCP server (Claude / Codex).
pub fn has_in_args(extra_args: &[String]) -> bool {
    extra_args.iter().any(|a| a.contains(SERVER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn mcp_gateway_enforces_direct_confirm_readonly_bot_and_secret_filter() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("my-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mem_dir = tmp.path().join("mem");

        let hub = crate::hub::Hub::new(
            crate::store::Store::open_memory().unwrap(),
            DaemonSettings::default(),
        );
        // Point hub.memories at our temp store
        let mem = Memories::new(MemoryStore::new(Some(mem_dir)));
        mem.ensure_session("s1", &repo, false).unwrap();
        let ticket = mem.grants.lock().unwrap().get("s1").unwrap().ticket.clone();

        // 1) Direct save succeeds
        let res = mem
            .mcp(
                &hub,
                "s1",
                &ticket,
                json!({
                    "method": "tools/call",
                    "params": {
                        "name": "memory_save",
                        "arguments": {
                            "scope": "project",
                            "id": "local-build",
                            "title": "Local Build",
                            "description": "Build on Windows",
                            "content": "Always run cargo test on ssh win."
                        }
                    }
                }),
            )
            .unwrap();
        assert_eq!(res["isError"], false);
        assert_eq!(res["structuredContent"]["status"], "saved");

        // 2) Secret is rejected
        let bad = mem
            .mcp(
                &hub,
                "s1",
                &ticket,
                json!({
                    "method": "tools/call",
                    "params": {
                        "name": "memory_save",
                        "arguments": {
                            "scope": "global",
                            "id": "leak",
                            "content": "token: ghp_1234567890abcdefghij1234567890"
                        }
                    }
                }),
            )
            .unwrap();
        assert_eq!(bad["isError"], true);

        // 3) Read works (batch and single)
        let read = mem
            .mcp(
                &hub,
                "s1",
                &ticket,
                json!({
                    "method": "tools/call",
                    "params": {
                        "name": "memory_read",
                        "arguments": { "id": "local-build" }
                    }
                }),
            )
            .unwrap();
        assert_eq!(read["isError"], false);
        assert!(read["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Always run cargo test on ssh win."));

        // 4) Bot session is strictly read-only and hides write tools in tools/list
        mem.ensure_session("bot1", &repo, true).unwrap();
        let bot_ticket = mem.grants.lock().unwrap().get("bot1").unwrap().ticket.clone();
        let list = mem
            .mcp(&hub, "bot1", &bot_ticket, json!({"method": "tools/list", "params": {}}))
            .unwrap();
        let names: Vec<&str> = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"memory_read"));
        assert!(!names.contains(&"memory_save"));
        let bot_write = mem
            .mcp(
                &hub,
                "bot1",
                &bot_ticket,
                json!({
                    "method": "tools/call",
                    "params": {
                        "name": "memory_save",
                        "arguments": { "scope": "global", "id": "pwn", "content": "injected" }
                    }
                }),
            )
            .unwrap();
        assert_eq!(bot_write["isError"], true);

        // 5) Confirm mode queues pending proposal instead of activating immediately
        hub.settings.write().unwrap().memory.write_policy = "confirm".into();
        let prop = mem
            .mcp(
                &hub,
                "s1",
                &ticket,
                json!({
                    "method": "tools/call",
                    "params": {
                        "name": "memory_save",
                        "arguments": {
                            "scope": "global",
                            "id": "pref-zh",
                            "title": "Chinese",
                            "content": "Always reply in Chinese."
                        }
                    }
                }),
            )
            .unwrap();
        assert_eq!(prop["isError"], false);
        assert_eq!(prop["structuredContent"]["status"], "pending");
        assert!(mem.store.get(MemoryScope::Global, None, None, "pref-zh").is_err());
        assert_eq!(mem.store.list_pending().len(), 1);

        // 6) Readonly mode refuses writes
        hub.settings.write().unwrap().memory.write_policy = "readonly".into();
        let ro = mem
            .mcp(
                &hub,
                "s1",
                &ticket,
                json!({
                    "method": "tools/call",
                    "params": {
                        "name": "memory_save",
                        "arguments": { "scope": "global", "id": "x", "content": "y" }
                    }
                }),
            )
            .unwrap();
        assert_eq!(ro["isError"], true);
    }
}
