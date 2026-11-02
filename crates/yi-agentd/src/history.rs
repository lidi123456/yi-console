//! 导入各工具自己保存的历史会话。
//! - Claude Code：~/.claude/projects/*/<id>.jsonl，可离线读取
//! - Codex：~/.codex/sessions/**/rollout-*.jsonl，可离线读取
//! - Cursor：ACP session/list（失败时退回扫描 ~/.cursor/chats），内容需 resume 后由 session/load 回放
//! - Kiro：`kiro-cli chat --list-sessions --all-cwds -f json`，同样 resume 后回放

use crate::adapters::{claude, codex};
use crate::util::{parse_iso, truncate};
use anyhow::{anyhow, Result};
use yi_proto::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

type CacheKey = (PathBuf, i64, u64);
static CACHE: Mutex<Option<HashMap<CacheKey, Option<HistoryEntry>>>> = Mutex::new(None);

fn file_key(p: &Path) -> Option<CacheKey> {
    let m = std::fs::metadata(p).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((p.to_path_buf(), mtime, m.len()))
}

fn cached(p: &Path, f: impl FnOnce(&Path, i64) -> Option<HistoryEntry>) -> Option<HistoryEntry> {
    let key = file_key(p)?;
    if let Some(v) = CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return v.clone();
    }
    let v = f(p, key.1);
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, v.clone());
    v
}

pub async fn list(tool: Option<ToolKind>, settings: &DaemonSettings) -> Vec<HistoryEntry> {
    let mut out = vec![];
    let want = |t: ToolKind| tool.map(|x| x == t).unwrap_or(true);
    if want(ToolKind::Claude) {
        out.extend(tokio::task::spawn_blocking(list_claude).await.unwrap_or_default());
    }
    if want(ToolKind::Codex) {
        out.extend(tokio::task::spawn_blocking(list_codex).await.unwrap_or_default());
    }
    if want(ToolKind::Qoder) && crate::tools::resolve(ToolKind::Qoder, settings).is_some() {
        out.extend(tokio::task::spawn_blocking(list_qoder).await.unwrap_or_default());
    }
    if want(ToolKind::Codebuddy) && crate::tools::resolve(ToolKind::Codebuddy, settings).is_some() {
        out.extend(tokio::task::spawn_blocking(list_codebuddy).await.unwrap_or_default());
    }
    if want(ToolKind::Kiro) {
        if let Some(p) = crate::tools::resolve(ToolKind::Kiro, settings) {
            out.extend(list_kiro(&p).await.unwrap_or_default());
        }
    }
    if want(ToolKind::Cursor) {
        if let Some(p) = crate::tools::resolve(ToolKind::Cursor, settings) {
            match list_cursor_acp(&p).await {
                Ok(v) if !v.is_empty() => out.extend(v),
                _ => out.extend(list_cursor_dirs()),
            }
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

pub fn read(tool: ToolKind, id: &str) -> Result<Vec<EventBody>> {
    match tool {
        ToolKind::Claude => {
            let p = find_claude_file(id).ok_or_else(|| anyhow!("找不到 Claude 会话 {id}"))?;
            read_claude(&p)
        }
        ToolKind::Codex => {
            let p = find_codex_file(id).ok_or_else(|| anyhow!("找不到 Codex 会话 {id}"))?;
            read_codex(&p)
        }
        ToolKind::Qoder => {
            let p = find_qoder_file(id).ok_or_else(|| anyhow!("找不到 Qoder 会话 {id}"))?;
            read_claude(&p)
        }
        _ => Err(anyhow!("该工具的历史需要“继续会话”后才能查看")),
    }
}

// ---------------- Claude ----------------

pub(crate) fn claude_root() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::paths::home().join(".claude"));
    base.join("projects")
}

fn claude_files() -> Vec<PathBuf> {
    jsonl_files(&claude_root())
}

/// `<root>/<目录>/*.jsonl`
fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    let Ok(dirs) = std::fs::read_dir(root) else {
        return v;
    };
    for d in dirs.flatten() {
        if let Ok(files) = std::fs::read_dir(d.path()) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    v.push(p);
                }
            }
        }
    }
    v
}

/// Claude 会话记录在不在（终端里开了没说话就退出时不会生成）
pub fn claude_session_exists(id: &str) -> bool {
    find_claude_file(id).is_some()
}

/// 终端里开了工具但没说话就退出时，工具不会留下会话记录，这时不能 --resume，要用同一个 id 新开。
/// Claude、Qoder、CodeBuddy 都把会话存成 `<配置目录>/projects/<目录>/<id>.jsonl`
pub fn tool_session_exists(tool: ToolKind, id: &str) -> bool {
    let dir = match tool {
        ToolKind::Claude => return claude_session_exists(id),
        ToolKind::Qoder => ".qoder",
        ToolKind::Codebuddy => ".codebuddy",
        _ => return true,
    };
    let root = projects_root(dir);
    let file = format!("{id}.jsonl");
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|d| d.path().join(&file).is_file())
}

fn projects_root(dir: &str) -> PathBuf {
    crate::paths::home().join(dir).join("projects")
}

pub(crate) fn qoder_root() -> PathBuf {
    projects_root(".qoder")
}

pub(crate) fn codebuddy_root() -> PathBuf {
    projects_root(".codebuddy")
}

/// Qoder 的会话文件和 Claude 的格式一样，直接用 Claude 的解析
pub(crate) fn find_qoder_file(id: &str) -> Option<PathBuf> {
    jsonl_files(&qoder_root())
        .into_iter()
        .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(id))
}

fn list_qoder() -> Vec<HistoryEntry> {
    jsonl_files(&qoder_root())
        .into_iter()
        .filter_map(|p| cached(&p, scan_claude))
        .map(|mut e| {
            e.tool = ToolKind::Qoder;
            e
        })
        .collect()
}

fn list_codebuddy() -> Vec<HistoryEntry> {
    jsonl_files(&codebuddy_root())
        .into_iter()
        .filter_map(|p| cached(&p, scan_codebuddy))
        .collect()
}

/// CodeBuddy 的会话文件：每行 {type:"message", role, content:[{type:"input_text"|"output_text", text}], sessionId, cwd, timestamp(毫秒)}，
/// 另有 {type:"ai-title", aiTitle}。只列出来，内容要「继续会话」后经 ACP 回放
fn scan_codebuddy(p: &Path, mtime: i64) -> Option<HistoryEntry> {
    let f = std::fs::File::open(p).ok()?;
    let mut id = p.file_stem()?.to_str()?.to_string();
    let mut cwd = String::new();
    let mut first_user: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut count = 0u64;
    let mut last_ts = 0i64;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("\"type\":\"message\"") && !line.contains("\"type\":\"ai-title\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["type"] == "ai-title" {
            ai_title = v["aiTitle"].as_str().map(str::to_string);
            continue;
        }
        if cwd.is_empty() {
            cwd = v["cwd"].as_str().unwrap_or("").to_string();
        }
        if let Some(s) = v["sessionId"].as_str() {
            id = s.to_string();
        }
        if let Some(ts) = v["timestamp"].as_i64() {
            last_ts = ts;
        }
        let text: String = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        match v["role"].as_str() {
            // 命令回显、系统提醒都包在 <xxx> 里，不算用户消息
            Some("user") if !v["providerData"]["skipRun"].as_bool().unwrap_or(false) && !text.trim_start().starts_with('<') => {
                if text.trim().is_empty() {
                    continue;
                }
                count += 1;
                first_user.get_or_insert(text);
            }
            Some("assistant") if !text.trim().is_empty() => count += 1,
            _ => {}
        }
    }
    if first_user.is_none() {
        return None;
    }
    Some(HistoryEntry {
        tool: ToolKind::Codebuddy,
        id,
        cwd,
        title: truncate(&ai_title.or(first_user).unwrap_or_default(), 100),
        updated_at: if last_ts > 0 { last_ts } else { mtime },
        message_count: Some(count),
        readable: false,
    })
}

/// 换个工具继续用：工具自己记的会话文件（新工具直接读），没有可读文件的工具（Cursor）返回 None
pub fn session_file(tool: ToolKind, id: &str) -> Option<PathBuf> {
    // id 来自工具自己，拼路径前还是挡一下路径分隔符和 ..
    if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") {
        return None;
    }
    let by_stem = |root: PathBuf| {
        jsonl_files(&root)
            .into_iter()
            .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(id))
    };
    match tool {
        ToolKind::Claude => find_claude_file(id),
        ToolKind::Codex => find_codex_file(id),
        ToolKind::Qoder => find_qoder_file(id),
        ToolKind::Codebuddy => by_stem(codebuddy_root()),
        // Kiro 的对话（ACP）和终端界面都记在 ~/.kiro/sessions/cli/<id>.jsonl
        ToolKind::Kiro => {
            let p = crate::paths::home().join(".kiro").join("sessions").join("cli").join(format!("{id}.jsonl"));
            p.is_file().then_some(p)
        }
        _ => None,
    }
}

/// 终端模式下第一次启动就用 --session-id 指定会话 id 的工具
pub fn pins_session_id(tool: ToolKind) -> bool {
    matches!(tool, ToolKind::Claude | ToolKind::Qoder | ToolKind::Codebuddy)
}

fn find_claude_file(id: &str) -> Option<PathBuf> {
    claude_files()
        .into_iter()
        .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(id))
}

fn list_claude() -> Vec<HistoryEntry> {
    claude_files()
        .into_iter()
        .filter_map(|p| cached(&p, scan_claude))
        .collect()
}

/// 用户真正输入的文本（排除命令回显、系统注入的 <xxx> 块）
fn claude_user_text(v: &Value) -> Option<String> {
    if v["isMeta"].as_bool().unwrap_or(false) {
        return None;
    }
    let c = &v["message"]["content"];
    let text = if let Some(s) = c.as_str() {
        s.to_string()
    } else {
        c.as_array()?
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let t = text.trim();
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    Some(t.to_string())
}

fn scan_claude(p: &Path, mtime: i64) -> Option<HistoryEntry> {
    let f = std::fs::File::open(p).ok()?;
    let id = p.file_stem()?.to_str()?.to_string();
    let mut cwd = String::new();
    let mut first_user: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut count = 0u64;
    let mut last_ts = 0i64;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        // 只解析可能有用的行，大文件里 tool 输出很多
        let is_msg = line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"");
        let is_title = line.contains("\"type\":\"ai-title\"");
        if !is_msg && !is_title {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("ai-title") => ai_title = v["aiTitle"].as_str().map(str::to_string),
            Some("user") | Some("assistant") => {
                if v["isSidechain"].as_bool().unwrap_or(false) {
                    continue;
                }
                if cwd.is_empty() {
                    cwd = v["cwd"].as_str().unwrap_or("").to_string();
                }
                if let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) {
                    last_ts = ts;
                }
                if v["type"] == "user" {
                    if let Some(t) = claude_user_text(&v) {
                        count += 1;
                        if first_user.is_none() {
                            first_user = Some(t);
                        }
                    }
                } else {
                    count += 1;
                }
            }
            _ => {}
        }
    }
    if count == 0 {
        return None;
    }
    Some(HistoryEntry {
        tool: ToolKind::Claude,
        id,
        cwd,
        title: truncate(
            &ai_title.or(first_user).unwrap_or_else(|| "(无标题)".into()),
            100,
        ),
        updated_at: if last_ts > 0 { last_ts } else { mtime },
        message_count: Some(count),
        readable: true,
    })
}

fn read_claude(p: &Path) -> Result<Vec<EventBody>> {
    let f = std::fs::File::open(p)?;
    let mut out = vec![];
    let mut n: HashMap<String, u32> = HashMap::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["isSidechain"].as_bool().unwrap_or(false) {
            continue;
        }
        match v["type"].as_str() {
            Some("user") => {
                if let Some(t) = claude_user_text(&v) {
                    out.push(EventBody::UserMessage { text: t, images: vec![] });
                } else {
                    for ev in claude::user_content_events(&v["message"]["content"]) {
                        if matches!(ev, EventBody::ToolResult { .. }) {
                            out.push(ev);
                        }
                    }
                }
            }
            Some("assistant") => {
                let mid = v["message"]["id"]
                    .as_str()
                    .or(v["uuid"].as_str())
                    .unwrap_or("")
                    .to_string();
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    let c = n.entry(mid.clone()).or_insert(0);
                    *c += 1;
                    out.extend(claude::blocks_to_events(b, &format!("{mid}:{c}")));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------- Codex ----------------

pub(crate) fn codex_root() -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::paths::home().join(".codex"))
        .join("sessions")
}

pub(crate) fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_jsonl(&p, out, depth + 1);
        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

fn codex_files() -> Vec<PathBuf> {
    let mut v = vec![];
    walk_jsonl(&codex_root(), &mut v, 0);
    v
}

fn find_codex_file(id: &str) -> Option<PathBuf> {
    codex_files().into_iter().find(|p| {
        p.file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.contains(id))
            .unwrap_or(false)
    })
}

fn list_codex() -> Vec<HistoryEntry> {
    codex_files()
        .into_iter()
        .filter_map(|p| cached(&p, scan_codex))
        .collect()
}

/// 新版 rollout 用 event_msg/item_completed，旧版是 event_msg/user_message
fn codex_user_text(p: &Value) -> Option<String> {
    let t = match p["type"].as_str()? {
        "user_message" => p["message"].as_str()?.to_string(),
        "item_completed" if p["item"]["type"] == "UserMessage" => p["item"]["content"]
            .as_array()?
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let t = t.trim();
    (!t.is_empty() && !t.starts_with('<')).then(|| t.to_string())
}

fn scan_codex(p: &Path, mtime: i64) -> Option<HistoryEntry> {
    let f = std::fs::File::open(p).ok()?;
    let mut id = String::new();
    let mut cwd = String::new();
    let mut title: Option<String> = None;
    let mut count = 0u64;
    let mut last_ts = 0;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("session_meta") && !line.contains("\"event_msg\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(ts) = v["timestamp"].as_str().and_then(parse_iso) {
            last_ts = ts;
        }
        let pl = &v["payload"];
        match v["type"].as_str() {
            Some("session_meta") => {
                id = pl["id"].as_str().unwrap_or("").to_string();
                cwd = pl["cwd"].as_str().unwrap_or("").to_string();
            }
            Some("event_msg") => {
                if let Some(t) = codex_user_text(pl) {
                    count += 1;
                    title.get_or_insert(t);
                } else if pl["type"] == "agent_message"
                    || (pl["type"] == "item_completed" && pl["item"]["type"] == "AgentMessage")
                {
                    count += 1;
                }
            }
            _ => {}
        }
    }
    if id.is_empty() || count == 0 {
        return None;
    }
    Some(HistoryEntry {
        tool: ToolKind::Codex,
        id,
        cwd,
        title: truncate(&title.unwrap_or_else(|| "(无标题)".into()), 100),
        updated_at: if last_ts > 0 { last_ts } else { mtime },
        message_count: Some(count),
        readable: true,
    })
}

/// rollout 里 item_completed 的 item 是 PascalCase type + snake_case 字段，
/// 转成 app-server ThreadItem 的形状后复用 codex::item_events
fn rollout_item_to_thread_item(item: &Value) -> Value {
    let t = item["type"].as_str().unwrap_or("");
    let mut out = json!({ "id": item["id"] });
    let camel = |s: &str| -> String {
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    };
    out["type"] = json!(camel(t));
    match t {
        "AgentMessage" => {
            let text = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            out["text"] = json!(text);
        }
        "CommandExecution" => {
            out["command"] = item["command"].clone();
            out["cwd"] = item["cwd"].clone();
            out["status"] = json!(item["status"].as_str().map(|s| s.to_lowercase()).unwrap_or("completed".into()));
            out["aggregatedOutput"] = item["aggregated_output"].clone();
            out["exitCode"] = item["exit_code"].clone();
        }
        "FileChange" => {
            let changes: Vec<Value> = match &item["changes"] {
                Value::Array(a) => a.clone(),
                Value::Object(m) => m
                    .iter()
                    .map(|(path, c)| {
                        json!({"path": path, "diff": c["unified_diff"].as_str().or(c["diff"].as_str())})
                    })
                    .collect(),
                _ => vec![],
            };
            out["changes"] = json!(changes);
            out["status"] = json!("completed");
        }
        "Reasoning" => {
            out["summary"] = item["summary_text"].clone();
            out["content"] = item["raw_content"].clone();
        }
        _ => {}
    }
    out
}

fn read_codex(p: &Path) -> Result<Vec<EventBody>> {
    let f = std::fs::File::open(p)?;
    let mut out = vec![];
    let mut legacy = vec![];
    let mut saw_items = false;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["type"] != "event_msg" {
            continue;
        }
        let pl = &v["payload"];
        match pl["type"].as_str().unwrap_or("") {
            "item_completed" => {
                saw_items = true;
                let item = &pl["item"];
                if item["type"] == "UserMessage" {
                    if let Some(t) = codex_user_text(pl) {
                        out.push(EventBody::UserMessage { text: t, images: vec![] });
                    }
                    continue;
                }
                out.extend(codex::item_events(&rollout_item_to_thread_item(item), false));
            }
            "user_message" => {
                if let Some(t) = codex_user_text(pl) {
                    legacy.push(EventBody::UserMessage { text: t, images: vec![] });
                }
            }
            "agent_message" => {
                if let Some(t) = pl["message"].as_str() {
                    legacy.push(EventBody::AssistantText {
                        id: format!("legacy-{}", legacy.len()),
                        text: t.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(if saw_items { out } else { legacy })
}

// ---------------- Kiro ----------------

async fn list_kiro(program: &Path) -> Result<Vec<HistoryEntry>> {
    let mut cmd: tokio::process::Command = crate::adapters::std_command(
        program,
        &[
            "chat".into(),
            "--list-sessions".into(),
            "--all-cwds".into(),
            "-f".into(),
            "json".into(),
        ],
    )
    .into();
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await??;
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let mut res = vec![];
    for group in v.as_array().into_iter().flatten() {
        let cwd = group["cwd"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches(r"\\?\")
            .to_string();
        for s in group["sessions"].as_array().into_iter().flatten() {
            res.push(HistoryEntry {
                tool: ToolKind::Kiro,
                id: s["sessionId"].as_str().unwrap_or("").to_string(),
                cwd: cwd.clone(),
                title: truncate(s["title"].as_str().unwrap_or("(无标题)"), 100),
                updated_at: s["updatedAt"].as_str().and_then(parse_iso).unwrap_or(0),
                message_count: s["messageCount"].as_u64(),
                readable: false,
            });
        }
    }
    Ok(res)
}

// ---------------- Cursor ----------------

/// 起一个短命的 `cursor-agent acp` 调 session/list
async fn list_cursor_acp(program: &Path) -> Result<Vec<HistoryEntry>> {
    let home = crate::paths::home();
    let mut p = crate::adapters::JsonProc::spawn(program, &["acp".into()], &home)?;
    let fut = async {
        p.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,
            "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}))
            .await?;
        let mut entries = vec![];
        let mut next_id = 10;
        while let Some(line) = p.stdout.next_line().await? {
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            match v["id"].as_u64() {
                Some(1) => {
                    if v["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_null() {
                        break;
                    }
                    if let Some(m) = v["result"]["authMethods"][0]["id"].as_str() {
                        p.send(&json!({"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":m}}))
                            .await?;
                    } else {
                        p.send(&json!({"jsonrpc":"2.0","id":next_id,"method":"session/list","params":{}}))
                            .await?;
                    }
                }
                Some(2) => {
                    if v.get("error").is_some() {
                        break;
                    }
                    p.send(&json!({"jsonrpc":"2.0","id":next_id,"method":"session/list","params":{}}))
                        .await?;
                }
                Some(id) if id == next_id => {
                    let r = &v["result"];
                    for s in r["sessions"].as_array().into_iter().flatten() {
                        entries.push(HistoryEntry {
                            tool: ToolKind::Cursor,
                            id: s["sessionId"].as_str().unwrap_or("").to_string(),
                            cwd: s["cwd"].as_str().unwrap_or("").to_string(),
                            title: truncate(s["title"].as_str().unwrap_or("(无标题)"), 100),
                            updated_at: s["updatedAt"].as_str().and_then(parse_iso).unwrap_or(0),
                            message_count: None,
                            readable: false,
                        });
                    }
                    let cursor = r["nextCursor"].as_str().map(str::to_string);
                    if cursor.is_none() || entries.len() > 500 {
                        break;
                    }
                    next_id += 1;
                    p.send(&json!({"jsonrpc":"2.0","id":next_id,"method":"session/list",
                        "params":{"cursor":cursor}}))
                        .await?;
                }
                _ => {}
            }
        }
        anyhow::Ok(entries)
    };
    let res = tokio::time::timeout(Duration::from_secs(20), fut).await;
    let _ = p.child.start_kill();
    Ok(res??)
}

/// Cursor 的对话模式（ACP）和终端界面各存一份会话：ACP 在 `~/.cursor/acp-sessions/<id>/`，
/// 终端界面在 `~/.cursor/chats/<md5(cwd)>/<id>/`，都是同样格式的 store.db。
/// 切换模式时（两边的进程都已结束）把当前这份拷到另一边，工具才接得上前面聊的。
/// 另一边的那份比这边新（比如在别处接着聊过）就不覆盖
pub fn cursor_handoff(id: &str, cwd: &str, to_terminal: bool) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("Cursor 会话 id 无效: {id}");
    }
    let root = crate::paths::home().join(".cursor");
    let acp = root.join("acp-sessions").join(id);
    let chats = root.join("chats");
    let chat = std::fs::read_dir(&chats)
        .ok()
        .and_then(|ws| ws.flatten().map(|w| w.path().join(id)).find(|p| p.is_dir()))
        .unwrap_or_else(|| chats.join(format!("{:x}", md5::compute(cwd.as_bytes()))).join(id));
    let (src, dst) = if to_terminal { (acp, chat) } else { (chat, acp) };
    if !src.join("store.db").is_file() {
        return Ok(());
    }
    let newest = |d: &Path| {
        ["store.db", "store.db-wal"]
            .iter()
            .filter_map(|f| std::fs::metadata(d.join(f)).and_then(|m| m.modified()).ok())
            .max()
    };
    if let (Some(a), Some(b)) = (newest(&src), newest(&dst)) {
        if b > a {
            return Ok(());
        }
    }
    std::fs::create_dir_all(&dst)?;
    for f in ["store.db", "store.db-wal", "store.db-shm"] {
        let _ = std::fs::remove_file(dst.join(f));
    }
    // shm 是 sqlite 自己的索引，打开时会重建
    for f in ["store.db", "store.db-wal"] {
        if src.join(f).is_file() {
            std::fs::copy(src.join(f), dst.join(f))?;
        }
    }
    if !dst.join("meta.json").is_file() {
        let title = std::fs::read(src.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["title"].as_str().map(str::to_string))
            .unwrap_or_default();
        let now = crate::util::now_ms();
        let meta = if to_terminal {
            json!({"schemaVersion":1,"createdAtMs":now,"hasConversation":true,"title":title,"updatedAtMs":now,"cwd":cwd})
        } else {
            json!({"schemaVersion":1,"cwd":cwd,"title":title})
        };
        std::fs::write(dst.join("meta.json"), serde_json::to_vec(&meta)?)?;
    }
    Ok(())
}

/// 退路：~/.cursor/chats/<workspace-hash>/<chatId>/ 目录
fn list_cursor_dirs() -> Vec<HistoryEntry> {
    let mut v = vec![];
    let root = crate::paths::home().join(".cursor").join("chats");
    let Ok(ws) = std::fs::read_dir(root) else {
        return v;
    };
    for w in ws.flatten() {
        let Ok(chats) = std::fs::read_dir(w.path()) else {
            continue;
        };
        for c in chats.flatten() {
            if !c.path().is_dir() {
                continue;
            }
            let mtime = c
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            v.push(HistoryEntry {
                tool: ToolKind::Cursor,
                id: c.file_name().to_string_lossy().to_string(),
                cwd: String::new(),
                title: "(Cursor 会话)".into(),
                updated_at: mtime,
                message_count: None,
                readable: false,
            });
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn session_file_rejects_paths() {
        for id in ["", "../x", "a/b", "a\\b", ".."] {
            assert!(session_file(ToolKind::Kiro, id).is_none(), "{id}");
        }
        assert!(session_file(ToolKind::Cursor, "abc").is_none());
    }

    #[test]
    fn claude_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"user","isMeta":true,"message":{{"role":"user","content":"<x>"}},"cwd":"/w","timestamp":"2026-01-01T00:00:00Z"}}"#).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"role":"user","content":"修个 bug"}},"cwd":"/w","timestamp":"2026-01-01T00:00:01Z"}}"#).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"text","text":"好"}},{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"ls"}}}}]}},"cwd":"/w","timestamp":"2026-01-01T00:00:02Z"}}"#).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"a.txt"}}]}},"cwd":"/w","timestamp":"2026-01-01T00:00:03Z"}}"#).unwrap();
        writeln!(f, r#"{{"type":"ai-title","aiTitle":"修复问题"}}"#).unwrap();
        drop(f);
        let e = scan_claude(&p, 0).unwrap();
        assert_eq!(e.title, "修复问题");
        assert_eq!(e.cwd, "/w");
        let evs = read_claude(&p).unwrap();
        assert!(matches!(&evs[0], EventBody::UserMessage { text, .. } if text == "修个 bug"));
        assert!(matches!(&evs[1], EventBody::AssistantText { .. }));
        assert!(matches!(&evs[2], EventBody::ToolCall { name, .. } if name == "Bash"));
        assert!(matches!(&evs[3], EventBody::ToolResult { output, .. } if output == "a.txt"));
    }
}

#[cfg(test)]
mod codebuddy_tests {
    #[test]
    fn scan() {
        let d = std::env::temp_dir().join(format!("ah-cb-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("s1.jsonl");
        std::fs::write(&p, concat!(
            r#"{"id":"a","timestamp":1,"type":"message","role":"user","content":[{"type":"input_text","text":"<command-name>/clear</command-name>"}],"providerData":{"skipRun":true},"sessionId":"s1","cwd":"d:\\x"}"#, "\n",
            r#"{"id":"b","timestamp":5,"type":"message","role":"user","content":[{"type":"input_text","text":"帮我写个命令"}],"sessionId":"s1","cwd":"d:\\x"}"#, "\n",
            r#"{"id":"c","timestamp":9,"type":"message","role":"assistant","content":[{"type":"output_text","text":"好的"}],"sessionId":"s1","cwd":"d:\\x"}"#, "\n",
        )).unwrap();
        let e = super::scan_codebuddy(&p, 0).unwrap();
        assert_eq!((e.id.as_str(), e.cwd.as_str(), e.title.as_str(), e.message_count, e.updated_at), ("s1", "d:\\x", "帮我写个命令", Some(2), 9));
        let _ = std::fs::remove_dir_all(&d);
    }
}
