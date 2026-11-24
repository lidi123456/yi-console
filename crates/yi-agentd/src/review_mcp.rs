//! Strictly read-only code review MCP gateway (`yi_review`).
//!
//! Provides pre-authorized read-only diff, scope, file-read, and repo-grep tools to AI
//! review side chats across all 7 structured providers without triggering CLI permission prompts.
//!
//! Security boundary:
//! - Strictly read-only (`review_scope`, `review_diff`, `review_read_file`, `review_grep`).
//! - Local IPC only (`review.mcp`), bound to the first proxy PID + parent AI tool (`proxy_access`).
//! - Paths are strictly repo-relative; absolute paths, `..`, and symlinks outside the repo root are rejected.
//! - Every git command runs via `gitops` (`GIT_OPTIONAL_LOCKS=0`).

use crate::computer_use::{process, proxy_access, ProxyAccess};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use yi_proto::{ReviewMeta, SessionInfo};

pub const SERVER: &str = "yi_review";
pub const TOOLS: &[&str] = &["review_scope", "review_diff", "review_read_file", "review_grep"];

const DEFAULT_DIFF_CHARS: usize = 24_000;
const MAX_DIFF_CHARS: usize = 48_000;
const MAX_READ_CHARS: usize = 24_000;
const DEFAULT_READ_LINES: usize = 300;
const MAX_READ_LINES: usize = 600;
const DEFAULT_GREP_MATCHES: usize = 50;
const MAX_GREP_MATCHES: usize = 150;
const MAX_GREP_CHARS: usize = 24_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

const GUIDE: &str = "The yi_review tools provide strictly read-only, pre-authorized access to the review scope, \
unified diffs, repository files (with 1-based line numbers), and code search. \
Prefer yi_review tools instead of shell commands whenever possible; never attempt to modify files or git state.";

pub fn tool_names() -> impl Iterator<Item = &'static str> {
    TOOLS.iter().copied()
}

struct ReviewGrant {
    ticket: String,
    proxy: Option<(u32, Option<u32>)>,
}

pub struct ReviewMcp {
    tickets_dir: PathBuf,
    grants: Mutex<HashMap<String, ReviewGrant>>,
}

impl Default for ReviewMcp {
    fn default() -> Self {
        Self::new(crate::paths::base_dir().join("review-mcp-tickets"))
    }
}

impl ReviewMcp {
    pub fn new(tickets_dir: PathBuf) -> Self {
        Self {
            tickets_dir,
            grants: Mutex::new(HashMap::new()),
        }
    }

    fn ticket_path(&self, sid: &str) -> PathBuf {
        self.tickets_dir.join(format!("{sid}.json"))
    }

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
        grants.insert(sid.to_string(), ReviewGrant { ticket, proxy: None });
        Ok(())
    }

    pub fn release_proxy(&self, sid: &str) {
        if let Some(g) = self.grants.lock().unwrap().get_mut(sid) {
            g.proxy = None;
        }
    }

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

    pub fn authorizes(&self, sid: &str, name: &str) -> bool {
        let Some(tool) = name.strip_prefix("mcp__yi_review__") else {
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
        let pid = pid.ok_or_else(|| anyhow!("无法确认代码审阅代理进程"))?;
        {
            let mut grants = self.grants.lock().unwrap();
            let g = grants
                .get_mut(sid)
                .ok_or_else(|| anyhow!("当前会话未启用只读审阅工具"))?;
            if g.ticket != ticket {
                bail!("代码审阅代理凭据已失效，请重新启动审阅");
            }
            let parent = process::parent(pid);
            match proxy_access(g.proxy, pid, parent, process::alive) {
                ProxyAccess::Deny => {
                    bail!("yi_review 只接受 AI 工具自己启动的代理，请直接调用 yi_review 工具")
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
                .ok_or_else(|| anyhow!("当前会话未启用只读审阅工具"))?;
            if g.ticket != ticket {
                bail!("代码审阅代理凭据已失效，请重新启动审阅");
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
            m => bail!("不支持的 yi_review MCP 请求：{m}"),
        }
    }

    fn tool_call(&self, hub: &crate::hub::Hub, caller_sid: &str, params: &Value) -> Result<Value> {
        let tool = params["name"]
            .as_str()
            .ok_or_else(|| anyhow!("缺少工具名称"))?;
        if !tool_names().any(|t| t == tool) {
            bail!("未知的代码审阅工具：{tool}");
        }
        let mut args = params["arguments"].as_object().cloned().unwrap_or_default();
        args.remove("__tool_use_purpose");
        args.remove("toolSummary");
        let args = Value::Object(args);

        let Ok(caller) = hub.info(caller_sid) else {
            return Ok(tool_error("当前会话不存在"));
        };
        Ok(call_with_session(&caller, tool, &args))
    }
}

pub(crate) fn call_with_session(caller: &SessionInfo, tool: &str, args: &Value) -> Value {
    match tool {
        "review_scope" => call_scope(caller),
        "review_diff" => call_diff(caller, args),
        "review_read_file" => call_read_file(caller, args),
        "review_grep" => call_grep(caller, args),
        _ => tool_error(&format!("未知的代码审阅工具：{tool}")),
    }
}

fn norm_rel(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    s.trim_end_matches('/').to_string()
}

fn validate_rel_path(raw: &str) -> Result<String> {
    let clean = norm_rel(raw);
    if clean.is_empty() || clean.contains('\0') {
        bail!("文件路径不能为空");
    }
    if clean.starts_with('-') || Path::new(&clean).is_absolute() || Path::new(raw.trim()).is_absolute() {
        bail!("只允许仓库相对路径: {raw}");
    }
    if clean.split('/').any(|c| c == ".." || c == ".") {
        bail!("路径不能包含 .. 或 .: {raw}");
    }
    crate::gitops::check_paths(&[clean.clone()])?;
    Ok(clean)
}

fn repo_root_for(cwd: &str) -> Result<PathBuf> {
    let dir = crate::paths::expand(cwd);
    if !dir.exists() {
        bail!("工作目录不存在: {}", dir.display());
    }
    crate::gitops::repo_root(&dir).ok_or_else(|| anyhow!("当前目录不是 Git 仓库"))
}

fn resolve_inside_repo(root: &Path, rel: &str) -> Result<PathBuf> {
    let clean = validate_rel_path(rel)?;
    let can_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let joined = root.join(&clean);
    let can_target = std::fs::canonicalize(&joined).map_err(|_| anyhow!("文件不存在: {clean}"))?;
    if !can_target.starts_with(&can_root) {
        bail!("路径超出仓库根目录范围: {clean}");
    }
    Ok(can_target)
}

fn scope_params_from_meta(meta: Option<&ReviewMeta>, with_diff: bool, paths: Option<&[String]>) -> Value {
    let mut obj = serde_json::Map::new();
    let mode = meta
        .map(|m| m.scope.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("uncommitted");
    obj.insert("mode".into(), json!(mode));
    obj.insert("diff".into(), json!(with_diff));
    if let Some(m) = meta {
        if let Some(f) = &m.uncommitted_filter {
            obj.insert("uncommittedFilter".into(), json!(f));
        }
        if let Some(b) = &m.base {
            obj.insert("base".into(), json!(b));
        }
        if let Some(t) = &m.target {
            obj.insert("target".into(), json!(t));
        }
        if let Some(inc) = m.include_uncommitted {
            obj.insert("includeUncommitted".into(), json!(inc));
        }
        if !m.commits.is_empty() {
            obj.insert("commits".into(), json!(m.commits));
        }
        if let Some(r) = &m.range_expr {
            obj.insert("range".into(), json!(r));
        }
    }
    if let Some(p) = paths {
        obj.insert("paths".into(), json!(p));
    }
    Value::Object(obj)
}

fn filter_scope_files(files: &[Value], meta: Option<&ReviewMeta>) -> Vec<Value> {
    let excluded: HashSet<&str> = meta
        .map(|m| m.excluded_files.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let included: HashSet<&str> = meta
        .map(|m| m.included_files.iter().map(String::as_str).collect())
        .unwrap_or_default();
    files
        .iter()
        .filter(|f| {
            let Some(p) = f["path"].as_str() else { return false };
            if excluded.contains(p) {
                return false;
            }
            if !included.is_empty() && !included.contains(p) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn call_scope(caller: &SessionInfo) -> Value {
    let meta = caller.review.as_ref();
    let params = scope_params_from_meta(meta, false, None);
    let res = match crate::gitops::review_scope(&caller.cwd, &params) {
        Ok(r) => r,
        Err(e) => return tool_error(&format!("获取审阅范围失败: {e}")),
    };
    if let Some(err) = res["error"].as_str() {
        return tool_error(err);
    }
    let files = filter_scope_files(res["files"].as_array().map(Vec::as_slice).unwrap_or(&[]), meta);
    let label = meta
        .and_then(|m| m.scope_label.as_deref())
        .unwrap_or_else(|| res["mode"].as_str().unwrap_or("uncommitted"));

    let mut out = format!(
        "Review scope: {label} (mode={}, branch={}, files={})\n",
        res["mode"].as_str().unwrap_or("uncommitted"),
        res["branch"].as_str().unwrap_or(""),
        files.len()
    );
    if let (Some(b), Some(e)) = (res["baseRev"].as_str(), res["endRev"].as_str()) {
        out.push_str(&format!("Revisions: {b}..{e}\n"));
    }
    if let Some(m) = meta {
        if !m.excluded_files.is_empty() {
            out.push_str(&format!("Excluded files count: {} (do not review excluded files)\n", m.excluded_files.len()));
        }
    }
    out.push('\n');
    let mut shown = 0usize;
    for f in &files {
        let path = f["path"].as_str().unwrap_or("");
        let display_path = match f["oldPath"].as_str() {
            Some(old) if !old.is_empty() => format!("{old} -> {path}"),
            _ => path.to_string(),
        };
        let st = f["status"].as_str().unwrap_or("M");
        let add = f["added"].as_u64();
        let del = f["deleted"].as_u64();
        let bin = f["binary"].as_bool().unwrap_or(false);
        let stat = if bin {
            "[binary]".to_string()
        } else {
            match (add, del) {
                (Some(a), Some(d)) => format!("+{a} -{d}"),
                (Some(a), None) => format!("+{a}"),
                (None, Some(d)) => format!("-{d}"),
                _ => String::new(),
            }
        };
        let line = if stat.is_empty() {
            format!("- [{st}] {display_path}\n")
        } else {
            format!("- [{st}] {display_path} ({stat})\n")
        };
        if out.chars().count() + line.chars().count() > MAX_READ_CHARS - 120 {
            out.push_str(&format!("\n... ({} more files omitted)\n", files.len() - shown));
            break;
        }
        out.push_str(&line);
        shown += 1;
    }

    tool_ok(
        &out,
        json!({
            "scopeLabel": label,
            "mode": res["mode"],
            "branch": res["branch"],
            "baseRev": res["baseRev"],
            "endRev": res["endRev"],
            "files": files,
        }),
    )
}

fn call_diff(caller: &SessionInfo, args: &Value) -> Value {
    let meta = caller.review.as_ref();
    let req_path = match args["path"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match validate_rel_path(raw) {
            Ok(p) => Some(p),
            Err(e) => return tool_error(&e.to_string()),
        },
        None => None,
    };

    if let (Some(p), Some(m)) = (req_path.as_deref(), meta) {
        if m.excluded_files.iter().any(|x| x == p) {
            return tool_error(&format!("文件 {p} 已被用户从本次审阅范围中排除"));
        }
        if !m.included_files.is_empty() && !m.included_files.iter().any(|x| x == p) {
            return tool_error(&format!("文件 {p} 不在本次审阅范围内"));
        }
    }

    let only_paths: Option<Vec<String>> = match req_path.as_ref() {
        Some(p) => Some(vec![p.clone()]),
        None => meta
            .filter(|m| !m.included_files.is_empty() && m.included_files.len() <= 200)
            .map(|m| m.included_files.clone()),
    };

    let params = scope_params_from_meta(meta, true, only_paths.as_deref());
    let res = match crate::gitops::review_scope(&caller.cwd, &params) {
        Ok(r) => r,
        Err(e) => return tool_error(&format!("读取代码 diff 失败: {e}")),
    };
    if let Some(err) = res["error"].as_str() {
        return tool_error(err);
    }

    let files = filter_scope_files(res["files"].as_array().map(Vec::as_slice).unwrap_or(&[]), meta);
    if files.is_empty() {
        return tool_ok("No diff changes found for the requested scope/path.", json!({ "diff": "", "truncated": false }));
    }

    let mut combined = String::new();
    for f in &files {
        let p = f["path"].as_str().unwrap_or("");
        let display_path = match f["oldPath"].as_str() {
            Some(old) if !old.is_empty() => format!("{old} -> {p}"),
            _ => p.to_string(),
        };
        let st = f["status"].as_str().unwrap_or("M");
        let bin = f["binary"].as_bool().unwrap_or(false);
        let diff = f["diff"].as_str().unwrap_or("");
        if !combined.is_empty() {
            combined.push('\n');
        }
        if bin {
            combined.push_str(&format!("=== [{st}] {display_path} (binary file) ===\n"));
        } else if diff.trim().is_empty() {
            combined.push_str(&format!("=== [{st}] {display_path} (diff omitted: >200KB or empty; use review_read_file to inspect lines) ===\n"));
        } else {
            combined.push_str(&format!("=== [{st}] {display_path} ===\n{diff}"));
            if !diff.ends_with('\n') {
                combined.push('\n');
            }
        }
    }

    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let max_chars = args["maxChars"]
        .as_u64()
        .map(|n| (n as usize).clamp(400, MAX_DIFF_CHARS))
        .unwrap_or(DEFAULT_DIFF_CHARS);
    let total_chars = combined.chars().count();
    let (chunk, truncated) = slice_chars(&combined, offset, max_chars);
    let next_offset = truncated.then(|| offset + chunk.chars().count());

    let mut text = chunk.clone();
    if let Some(next) = next_offset {
        let hint_arg = req_path
            .as_deref()
            .map(|p| format!(r#"{{"path":"{p}","offset":{next}}}"#))
            .unwrap_or_else(|| format!(r#"{{"offset":{next}}}"#));
        text.push_str(&format!(
            "\n[truncated at offset={offset}, totalChars={total_chars}; call review_diff({hint_arg}) for more, or pass path for a single file]"
        ));
    }

    tool_ok(
        &text,
        json!({
            "offset": offset,
            "totalChars": total_chars,
            "truncated": truncated,
            "nextOffset": next_offset,
            "filesCount": files.len(),
        }),
    )
}

fn call_read_file(caller: &SessionInfo, args: &Value) -> Value {
    let raw_path = args["path"].as_str().unwrap_or("");
    let clean = match validate_rel_path(raw_path) {
        Ok(p) => p,
        Err(e) => return tool_error(&e.to_string()),
    };
    let root = match repo_root_for(&caller.cwd) {
        Ok(r) => r,
        Err(e) => return tool_error(&e.to_string()),
    };

    let rev = args["rev"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let bytes = if let Some(rev) = rev {
        if !crate::gitops::is_safe_ref_token(rev) {
            return tool_error(&format!("无效的 Git 引用: {rev}"));
        }
        let spec = format!("{rev}:{clean}");
        match crate::gitops::git(&root, &["show", &spec]) {
            Ok(out) if out.status.success() => out.stdout,
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return tool_error(&format!("无法从 {rev} 读取 {clean}: {err}"));
            }
            Err(e) => return tool_error(&e.to_string()),
        }
    } else {
        let full = match resolve_inside_repo(&root, &clean) {
            Ok(p) => p,
            Err(e) => return tool_error(&e.to_string()),
        };
        let Ok(meta) = std::fs::metadata(&full) else {
            return tool_error(&format!("文件不存在: {clean}"));
        };
        if meta.is_dir() {
            return tool_error(&format!("{clean} 是目录，不是文件"));
        }
        if meta.len() > MAX_FILE_BYTES {
            return tool_error(&format!("{clean} 超过 2MB 大小限制"));
        }
        match std::fs::read(&full) {
            Ok(b) => b,
            Err(e) => return tool_error(&format!("读取失败: {e}")),
        }
    };

    if bytes.iter().take(8000).any(|&b| b == 0) {
        return tool_error(&format!("{clean} 是二进制文件，无法以文本形式读取"));
    }

    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let start_line = args["startLine"]
        .as_u64()
        .map(|n| (n as usize).max(1))
        .unwrap_or(1);
    let end_line = args["endLine"]
        .as_u64()
        .map(|n| (n as usize).max(start_line))
        .unwrap_or_else(|| start_line.saturating_add(DEFAULT_READ_LINES - 1))
        .min(start_line.saturating_add(MAX_READ_LINES - 1));

    if total_lines == 0 {
        return tool_ok(
            &format!("{clean} (empty file)"),
            json!({ "path": clean, "totalLines": 0, "startLine": 1, "endLine": 0, "truncated": false }),
        );
    }
    if start_line > total_lines {
        return tool_ok(
            &format!("{clean} has {total_lines} lines (requested startLine={start_line} is past EOF)."),
            json!({ "path": clean, "totalLines": total_lines, "startLine": start_line, "endLine": total_lines, "truncated": false }),
        );
    }

    let actual_end = end_line.min(total_lines);
    let mut out = format!(
        "{clean}{} (lines {start_line}-{actual_end} of {total_lines})\n",
        rev.map(|r| format!(" @{r}")).unwrap_or_default()
    );
    let mut rendered_end = start_line - 1;
    let mut hit_char_cap = false;
    for ln in start_line..=actual_end {
        let row = format!("{ln:>4}: {}\n", lines[ln - 1]);
        if ln > start_line && out.chars().count() + row.chars().count() > MAX_READ_CHARS - 160 {
            hit_char_cap = true;
            break;
        }
        out.push_str(&row);
        rendered_end = ln;
    }
    let has_more = rendered_end < total_lines;
    if has_more {
        let next = rendered_end + 1;
        out.push_str(&format!(
            "\n[pass startLine={next} to read lines {next}..{total_lines}{}]",
            if hit_char_cap { " (output size limit reached)" } else { "" }
        ));
    }

    tool_ok(
        &out,
        json!({
            "path": clean,
            "rev": rev,
            "totalLines": total_lines,
            "startLine": start_line,
            "endLine": rendered_end,
            "truncated": has_more,
        }),
    )
}

fn call_grep(caller: &SessionInfo, args: &Value) -> Value {
    let pattern = args["pattern"].as_str().unwrap_or("");
    if pattern.is_empty() {
        return tool_error("pattern 不能为空");
    }
    let root = match repo_root_for(&caller.cwd) {
        Ok(r) => r,
        Err(e) => return tool_error(&e.to_string()),
    };

    let sub_path = match args["path"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match validate_rel_path(raw) {
            Ok(p) => {
                if let Err(e) = resolve_inside_repo(&root, &p) {
                    return tool_error(&e.to_string());
                }
                Some(p)
            }
            Err(e) => return tool_error(&e.to_string()),
        },
        None => None,
    };

    // Collect tracked + non-ignored untracked files using GIT_OPTIONAL_LOCKS=0
    let ls = match crate::gitops::git(&root, &["ls-files", "-co", "--exclude-standard", "-z"]) {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return tool_error(&format!("列出仓库文件失败: {err}"));
        }
        Err(e) => return tool_error(&e.to_string()),
    };

    let can_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let mut paths: Vec<String> = ls
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
        .filter(|rel| {
            if rel.starts_with(".git/") || rel == ".git" {
                return false;
            }
            match sub_path.as_deref() {
                Some(prefix) => rel == prefix || rel.starts_with(&format!("{prefix}/")),
                None => true,
            }
        })
        .collect();
    paths.retain(|rel| {
        let full = root.join(rel);
        std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_file())
            && std::fs::canonicalize(&full).is_ok_and(|c| c.starts_with(&can_root))
    });
    paths.sort();

    let max_matches = args["maxMatches"]
        .as_u64()
        .map(|n| (n as usize).clamp(1, MAX_GREP_MATCHES))
        .unwrap_or(DEFAULT_GREP_MATCHES);
    let opts = crate::fsapi::GrepOpts {
        case_sensitive: args["caseSensitive"].as_bool().unwrap_or(false),
        whole_word: false,
        regex: args["regex"].as_bool().unwrap_or(false),
        include: args["glob"].as_str().unwrap_or("").to_string(),
        exclude: String::new(),
        paths: Some(paths),
        replace: None,
        preserve_case: false,
    };

    let res = match crate::fsapi::grep(&root.to_string_lossy(), pattern, &opts, max_matches) {
        Ok(v) => v,
        Err(e) => return tool_error(&e.to_string()),
    };

    let hits = res["hits"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let truncated = res["truncated"].as_bool().unwrap_or(false);
    if hits.is_empty() {
        return tool_ok("No matches found.", json!({ "hits": [], "truncated": false }));
    }

    let mut out = format!("Found {} match line(s) across {} file(s):\n\n", hits.len(), res["files"].as_u64().unwrap_or(0));
    for h in hits {
        let p = h["path"].as_str().unwrap_or("");
        let line = h["line"].as_u64().unwrap_or(0);
        let text = h["text"].as_str().unwrap_or("");
        let row = format!("{p}:{line}: {text}\n");
        if out.chars().count() + row.chars().count() > MAX_GREP_CHARS - 120 {
            out.push_str("\n[output truncated; narrow path or glob]");
            break;
        }
        out.push_str(&row);
    }
    if truncated {
        out.push_str("\n[match limit reached; narrow path or glob]");
    }

    tool_ok(&out, res)
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
        "review-mcp".into(),
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
            "name": "review_scope",
            "description": "List the files and metadata in the active code review scope (respecting user exclusions). Strictly read-only.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "review_diff",
            "description": "Read the unified git diff for the active review scope or a single repo-relative file. Supports offset pagination when large. Strictly read-only.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional repo-relative path to inspect a single file's diff"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Character offset for pagination"},
                    "maxChars": {"type": "integer", "minimum": 400, "maximum": 48000, "description": "Max characters per call (default: 24000)"}
                }
            }
        }),
        json!({
            "name": "review_read_file",
            "description": "Read lines from any file inside the repository with 1-based line numbers (working tree by default, or at a git revision). Strictly read-only.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "description": "Repo-relative file path"},
                    "startLine": {"type": "integer", "minimum": 1, "description": "1-based start line (default: 1)"},
                    "endLine": {"type": "integer", "minimum": 1, "description": "1-based end line (default: startLine + 299)"},
                    "rev": {"type": "string", "description": "Optional git commit/ref to read historical content from"}
                }
            }
        }),
        json!({
            "name": "review_grep",
            "description": "Search text or regex across repository files (respecting .gitignore). Strictly read-only.",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": {"type": "string", "description": "Literal text or regex pattern to search for"},
                    "path": {"type": "string", "description": "Optional repo-relative directory or file to narrow search"},
                    "glob": {"type": "string", "description": "Optional comma-separated file globs (e.g. '*.rs,app/src/**/*.ts')"},
                    "regex": {"type": "boolean", "description": "Treat pattern as regular expression (default: false)"},
                    "caseSensitive": {"type": "boolean", "description": "Case-sensitive match (default: false)"},
                    "maxMatches": {"type": "integer", "minimum": 1, "maximum": 150, "description": "Max matching lines (default: 50)"}
                }
            }
        }),
    ]
}

/// Entry point for `yi-agentd review-mcp --session <sid> --ticket-file <path>`.
pub fn proxy_main(args: &[String]) -> Result<()> {
    let arg = |key: &str| {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let sid = arg("--session").ok_or_else(|| anyhow!("缺少会话标识"))?;
    let path = arg("--ticket-file").ok_or_else(|| anyhow!("缺少代码审阅连接凭据"))?;
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let ticket = value["ticket"]
        .as_str()
        .ok_or_else(|| anyhow!("代码审阅连接凭据无效"))?
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
            yi_proto::methods::REVIEW_MCP,
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

/// True when `extra_args` carries the injected `yi_review` MCP server (Claude / Codex).
pub fn has_in_args(extra_args: &[String]) -> bool {
    extra_args.iter().any(|a| a.contains(SERVER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use yi_proto::{SessionMode, SessionStatus, ToolKind, Usage};

    fn test_session(cwd: &Path, review: Option<ReviewMeta>) -> SessionInfo {
        SessionInfo {
            id: "rev-1".into(),
            tool: ToolKind::Codex,
            mode: SessionMode::Structured,
            cwd: cwd.to_string_lossy().into_owned(),
            title: "Review".into(),
            status: SessionStatus::Running,
            external_id: None,
            created_at: 1,
            updated_at: 1,
            last_process_error_at: None,
            last_seq: 0,
            pending_permissions: 0,
            usage: Usage::default(),
            model: None,
            command: None,
            context_percent: None,
            permission_mode: Some("plan".into()),
            effort: None,
            model_config_options: vec![],
            model_config_pending: false,
            pinned: false,
            queued: vec![],
            queue_paused: None,
            restart_interrupted: false,
            background_tasks: vec![],
            parent_id: Some("p-1".into()),
            side_chat: true,
            synced_users: None,
            auto_resume: None,
            limit_hits: 0,
            computer_use: false,
            browser_use: false,
            pending: None,
            turn: None,
            bot: None,
            client_mcp: vec![],
            append_system_prompt: None,
            account_id: None,
            account_label: None,
            checkpoints: vec![],
            review,
            worktree: None,
        }
    }

    #[test]
    fn scope_diff_read_grep_and_containment() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(repo)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(repo.join("src/skip.lock"), "lock v1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);

        std::fs::write(repo.join("src/a.rs"), "fn alpha() {\n    println!(\"changed\");\n}\nfn beta() {}\n").unwrap();
        std::fs::write(repo.join("src/skip.lock"), "lock v2\n").unwrap();
        std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();

        let sess = test_session(
            repo,
            Some(ReviewMeta {
                scope: "uncommitted".into(),
                included_files: vec!["src/a.rs".into()],
                excluded_files: vec!["src/skip.lock".into()],
                scope_label: Some("未提交".into()),
                ..Default::default()
            }),
        );

        let sc = call_with_session(&sess, "review_scope", &json!({}));
        assert_eq!(sc["isError"], false);
        let sc_text = sc["content"][0]["text"].as_str().unwrap();
        assert!(sc_text.contains("src/a.rs") && !sc_text.contains("src/skip.lock"), "{sc_text}");

        let df = call_with_session(&sess, "review_diff", &json!({ "maxChars": 400 }));
        assert_eq!(df["isError"], false);
        assert!(df["content"][0]["text"].as_str().unwrap().contains("changed"));

        let excl = call_with_session(&sess, "review_diff", &json!({ "path": "src/skip.lock" }));
        assert_eq!(excl["isError"], true);

        let rd = call_with_session(&sess, "review_read_file", &json!({ "path": "src/a.rs", "startLine": 2, "endLine": 3 }));
        assert_eq!(rd["isError"], false);
        assert!(rd["content"][0]["text"].as_str().unwrap().contains("   2:     println!"));

        let rd_head = call_with_session(&sess, "review_read_file", &json!({ "path": "src/a.rs", "rev": "HEAD" }));
        assert_eq!(rd_head["isError"], false);
        assert!(rd_head["content"][0]["text"].as_str().unwrap().contains("fn alpha() {}"));

        let trav = call_with_session(&sess, "review_read_file", &json!({ "path": "../etc/passwd" }));
        assert_eq!(trav["isError"], true);

        let bin = call_with_session(&sess, "review_read_file", &json!({ "path": "bin.dat" }));
        assert_eq!(bin["isError"], true);

        let gr = call_with_session(&sess, "review_grep", &json!({ "pattern": "beta" }));
        assert_eq!(gr["isError"], false);
        assert!(gr["content"][0]["text"].as_str().unwrap().contains("src/a.rs:4:"));

        let rmcp = ReviewMcp::new(dir.path().join("tickets"));
        rmcp.ensure_session("s1").unwrap();
        assert!(rmcp.authorizes("s1", "mcp__yi_review__review_diff"));
        assert!(!rmcp.authorizes("s1", "mcp__yi_review__unknown"));
        assert!(!rmcp.authorizes("s2", "mcp__yi_review__review_diff"));
    }

    #[cfg(unix)]
    #[test]
    fn review_grep_skips_symlinks_pointing_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside-secret.txt");
        std::fs::write(&outside, "SECRET_MARKER_XYZ\n").unwrap();

        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("ok.rs"), "const SEARCH_TOKEN: &str = \"SECRET_MARKER_XYZ\";\n").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("leak.txt")).unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);

        let sess = test_session(&repo, None);
        let gr = call_with_session(&sess, "review_grep", &json!({ "pattern": "SECRET_MARKER_XYZ" }));
        assert_eq!(gr["isError"], false);
        let text = gr["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ok.rs:1:"), "{text}");
        assert!(!text.contains("leak.txt"), "symlink target must not be grepped: {text}");
    }
}
